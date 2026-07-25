use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[cfg(feature = "async-shell")]
use crossterm::event::EventStream;
use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
#[cfg(feature = "async-shell")]
use futures_util::{StreamExt as _, future::OptionFuture};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};

use crate::{
    InputPolicy, Surface, SurfaceAction, SurfaceEvent, Theme,
    binding::{KeyBinding, ShellBindings},
    render,
    terminal::TerminalSession,
};

const PULSE_FRAME_RATE: Duration = Duration::from_millis(250);
const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_millis(4);
const PULSE_FRAMES: [&str; 8] = ["·", "·", "•", "●", "•", "·", "·", "·"];
static ATTACHED: AtomicBool = AtomicBool::new(false);

struct AttachLease;

impl AttachLease {
    fn acquire() -> io::Result<Self> {
        ATTACHED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "already_attached: another TurtleTap shell owns terminal input",
                )
            })
    }
}

impl Drop for AttachLease {
    fn drop(&mut self) {
        ATTACHED.store(false, Ordering::Release);
    }
}

/// Stable identity assigned to a surface by its shell.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SurfaceId(u64);

impl SurfaceId {
    /// Returns the shell-local numeric identity.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Why an attached shell returned control to its host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitReason {
    /// The user detached; surfaces remain owned by the shell.
    Detached,
    /// The final surface was closed.
    NoSurfaces,
}

/// Result of feeding one event to a shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellSignal {
    /// Keep the shell attached.
    Continue,
    /// Return control to the host.
    Exit(ExitReason),
}

impl Default for ShellBindings {
    fn default() -> Self {
        Self {
            leaders: vec![KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL)],
            palette: vec![
                KeyBinding::new(KeyCode::Char('`'), KeyModifiers::CONTROL),
                // Some terminals encode Ctrl-` as the same NUL byte as
                // Ctrl-Space, so accept both representations.
                KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
            ],
            redraw: vec![plain(KeyCode::F(5))],
            next_screen: Vec::new(),
            previous_screen: Vec::new(),
            jump_modifiers: Vec::new(),
            shell_detach: vec![ctrl('d')],
            shell_next_screen: vec![plain(KeyCode::Tab)],
            shell_previous_screen: vec![plain(KeyCode::BackTab)],
            shell_help: vec![
                plain(KeyCode::Char('?')),
                plain(KeyCode::F(1)),
                alt(KeyCode::Char('h')),
            ],
            leader_palette: vec![plain(KeyCode::Char('s'))],
            leader_next_screen: vec![
                plain(KeyCode::Char('n')),
                plain(KeyCode::Tab),
                plain(KeyCode::Right),
            ],
            leader_previous_screen: vec![
                plain(KeyCode::Char('p')),
                plain(KeyCode::BackTab),
                plain(KeyCode::Left),
            ],
            leader_scroll_up: vec![plain(KeyCode::Char('k')), plain(KeyCode::Up)],
            leader_scroll_down: vec![plain(KeyCode::Char('j')), plain(KeyCode::Down)],
            leader_close: vec![plain(KeyCode::Char('x'))],
            leader_detach: vec![plain(KeyCode::Char('d'))],
            leader_help: vec![plain(KeyCode::Char('?')), plain(KeyCode::Char('h'))],
            leader_jump_modifiers: vec![KeyModifiers::empty()],
            action_next_screen: vec![alt(KeyCode::Right)],
            action_previous_screen: vec![alt(KeyCode::Left)],
            action_scroll_up: vec![alt(KeyCode::Up)],
            action_scroll_down: vec![alt(KeyCode::Down)],
            action_close: vec![alt(KeyCode::Char('x'))],
            action_detach: vec![alt(KeyCode::Char('d'))],
            action_help: vec![alt(KeyCode::Char('?'))],
            action_clear_query: vec![ctrl('u')],
            action_jump_modifiers: vec![KeyModifiers::ALT],
            session_release_driver: vec![plain(KeyCode::F(2))],
            session_take_driver: vec![plain(KeyCode::F(3))],
            session_clear: vec![
                ctrl('l'),
                KeyBinding::new(KeyCode::Char('k'), KeyModifiers::SUPER),
            ],
            session_interrupt: vec![ctrl('c')],
            session_detach: vec![ctrl('d')],
            session_delete_to_start: vec![
                ctrl('u'),
                KeyBinding::new(KeyCode::Backspace, KeyModifiers::SUPER),
            ],
            session_word_left: vec![alt(KeyCode::Char('b')), alt(KeyCode::Left)],
            session_word_right: vec![alt(KeyCode::Char('f')), alt(KeyCode::Right)],
            session_line_start: vec![
                ctrl('a'),
                KeyBinding::new(KeyCode::Left, KeyModifiers::SUPER),
            ],
            session_line_end: vec![
                ctrl('e'),
                KeyBinding::new(KeyCode::Right, KeyModifiers::SUPER),
            ],
            session_delete_word: vec![ctrl('w'), alt(KeyCode::Backspace)],
            session_complete: vec![plain(KeyCode::Tab)],
            session_scroll_up: vec![plain(KeyCode::PageUp)],
            session_scroll_down: vec![plain(KeyCode::PageDown)],
            session_scroll_top: vec![KeyBinding::new(KeyCode::Home, KeyModifiers::CONTROL)],
            session_scroll_bottom: vec![KeyBinding::new(KeyCode::End, KeyModifiers::CONTROL)],
            dashboard_up: vec![plain(KeyCode::Char('k'))],
            dashboard_down: vec![plain(KeyCode::Char('j'))],
            dashboard_view: vec![plain(KeyCode::Char('v'))],
            dashboard_take: vec![plain(KeyCode::Char('t'))],
            dashboard_search: vec![plain(KeyCode::Char('/'))],
            dashboard_new: vec![plain(KeyCode::Char('n'))],
            dashboard_rename: vec![plain(KeyCode::Char('r'))],
            dashboard_delete: vec![plain(KeyCode::Char('x'))],
            dashboard_stop: vec![plain(KeyCode::Char('!'))],
            dashboard_keybindings: vec![plain(KeyCode::Char('b'))],
            dashboard_close: vec![plain(KeyCode::Char('q'))],
        }
    }
}

const fn plain(code: KeyCode) -> KeyBinding {
    KeyBinding::new(code, KeyModifiers::empty())
}

const fn ctrl(character: char) -> KeyBinding {
    KeyBinding::new(KeyCode::Char(character), KeyModifiers::CONTROL)
}

const fn alt(code: KeyCode) -> KeyBinding {
    KeyBinding::new(code, KeyModifiers::ALT)
}

impl ShellBindings {
    fn matches(bindings: &[KeyBinding], key: KeyEvent) -> bool {
        bindings.iter().any(|binding| binding.matches(key))
    }

    pub(crate) fn primary_leader_label(&self) -> String {
        self.leaders
            .first()
            .map_or_else(|| "leader".to_owned(), |binding| binding.label())
    }

    pub(crate) fn primary_palette_label(&self) -> String {
        self.palette
            .first()
            .map_or_else(|| "disabled".to_owned(), |binding| binding.label())
    }

    pub(crate) fn primary_redraw_label(&self) -> String {
        self.redraw
            .first()
            .map_or_else(|| "redraw".to_owned(), |binding| binding.label())
    }

    pub(crate) fn primary_next_label(&self) -> String {
        self.next_screen.first().map_or_else(
            || self.primary_action_label(&self.action_next_screen),
            |binding| binding.label(),
        )
    }

    pub(crate) fn primary_previous_label(&self) -> String {
        self.previous_screen.first().map_or_else(
            || self.primary_action_label(&self.action_previous_screen),
            |binding| binding.label(),
        )
    }

    pub(crate) fn primary_jump_label(&self) -> String {
        self.jump_modifiers.first().map_or_else(
            || {
                self.action_jump_modifiers.first().map_or_else(
                    || "disabled".to_owned(),
                    |modifiers| {
                        format!(
                            "{} {}",
                            self.primary_palette_label(),
                            digit_range_label(*modifiers)
                        )
                    },
                )
            },
            |modifiers| digit_range_label(*modifiers),
        )
    }

    pub(crate) fn primary_leader_chord_label(&self, suffixes: &[KeyBinding]) -> String {
        match (self.leaders.first(), suffixes.first()) {
            (Some(leader), Some(suffix)) => format!("{} {}", leader.label(), suffix.label()),
            _ => "disabled".to_owned(),
        }
    }

    fn primary_action_label(&self, actions: &[KeyBinding]) -> String {
        match (self.palette.first(), actions.first()) {
            (Some(palette), Some(action)) => format!("{} {}", palette.label(), action.label()),
            _ => "disabled".to_owned(),
        }
    }
}

fn digit_range_label(modifiers: KeyModifiers) -> String {
    KeyBinding::new(KeyCode::Char('1'), modifiers)
        .label()
        .replace('1', "1…9")
}

/// How the shell presents its list of surfaces.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Chrome {
    /// A horizontal tab strip above the active surface.
    ///
    /// Suited to a handful of long-lived surfaces; a growing list outgrows the
    /// single row it has to share.
    #[default]
    Tabs,
    /// A persistent vertical list beside the active surface (master-detail).
    ///
    /// On a terminal too narrow for titles this narrows to a marker-only rail
    /// rather than reverting to tabs, so the list never changes edges and
    /// ambient status survives.
    Rail {
        /// Columns for the full list.
        width: u16,
        /// Columns when only status markers fit.
        narrow: u16,
        /// Columns the detail pane must retain for the full list to be shown.
        min_content: u16,
    },
}

impl Chrome {
    /// A master-detail rail with terminal-friendly defaults.
    #[must_use]
    pub const fn rail() -> Self {
        Self::Rail {
            width: 24,
            narrow: 5,
            min_content: 48,
        }
    }

    /// Columns this chrome claims at `total` width, or `None` for tabs.
    pub(crate) const fn rail_width(self, total: u16) -> Option<u16> {
        match self {
            Self::Tabs => None,
            Self::Rail {
                width,
                narrow,
                min_content,
            } => {
                let wide = width.saturating_add(8);
                if total >= 120 && total.saturating_sub(wide) >= min_content {
                    Some(wide)
                } else if total.saturating_sub(width) >= min_content {
                    Some(width)
                } else {
                    Some(narrow)
                }
            }
        }
    }
}

/// Configuration for shell chrome and terminal behavior.
#[derive(Clone, Debug)]
pub struct ShellConfig {
    /// Host product name shown at the start of the tab bar.
    pub title: String,
    /// Periodic update frequency while no terminal input arrives.
    pub tick_rate: Duration,
    /// Whether direct `Ctrl-D` detaches shell-managed surfaces.
    pub direct_detach: bool,
    /// Whether to capture terminal mouse events while attached.
    ///
    /// Disabled by default so drag gestures remain available for native
    /// terminal text selection. Enable it only when a surface needs mouse
    /// events; users can typically hold their terminal's override modifier
    /// while dragging when capture is active.
    pub mouse_capture: bool,
    /// Semantic shell styles.
    pub theme: Theme,
    /// Global navigation key bindings.
    pub bindings: ShellBindings,
    /// How the surface list is presented.
    pub chrome: Chrome,
}

impl ShellConfig {
    /// Creates a configuration with terminal-friendly defaults.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            tick_rate: Duration::from_millis(100),
            direct_detach: true,
            mouse_capture: false,
            theme: Theme::default(),
            bindings: ShellBindings::default(),
            chrome: Chrome::default(),
        }
    }

    /// Replaces the periodic update rate.
    #[must_use]
    pub const fn with_tick_rate(mut self, tick_rate: Duration) -> Self {
        self.tick_rate = tick_rate;
        self
    }

    /// Enables or disables terminal mouse capture.
    #[must_use]
    pub const fn with_mouse_capture(mut self, enabled: bool) -> Self {
        self.mouse_capture = enabled;
        self
    }

    /// Enables or disables direct `Ctrl-D` detachment.
    #[must_use]
    pub const fn with_direct_detach(mut self, enabled: bool) -> Self {
        self.direct_detach = enabled;
        self
    }

    /// Replaces the shell theme.
    #[must_use]
    pub fn with_theme(mut self, theme: Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Replaces the shell's global navigation bindings.
    #[must_use]
    pub fn with_bindings(mut self, bindings: ShellBindings) -> Self {
        self.bindings = bindings;
        self
    }

    /// Chooses how the surface list is presented.
    #[must_use]
    pub const fn with_chrome(mut self, chrome: Chrome) -> Self {
        self.chrome = chrome;
        self
    }
}

struct Entry {
    id: SurfaceId,
    surface: Box<dyn Surface>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Overlay {
    Palette { query: String, selected: usize },
    Help,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum PaletteAction {
    RunSurfaceCommand(std::borrow::Cow<'static, str>),
    SelectSurface(usize),
    NextSurface,
    PreviousSurface,
    ScrollUp,
    ScrollDown,
    CloseSurface,
    Detach,
    Help,
}

pub(crate) struct PaletteItem {
    pub(crate) label: String,
    pub(crate) detail: String,
    pub(crate) shortcut: Option<String>,
    pub(crate) status: Option<crate::SurfaceStatus>,
    pub(crate) action: PaletteAction,
}

/// A reusable terminal shell containing heterogeneous surfaces.
pub struct Shell {
    pub(crate) config: ShellConfig,
    entries: Vec<Entry>,
    active: Option<usize>,
    next_id: u64,
    pub(crate) overlay: Option<Overlay>,
    leader_armed: bool,
    dirty: bool,
    pulse_elapsed: Duration,
    pulse_frame: usize,
    pulse_enabled: bool,
    clear_requested: bool,
    frame_interval: Duration,
    background_cursor: usize,
    pub(crate) notice: Option<String>,
    pub(crate) chrome_hits: Vec<(Rect, usize)>,
}

impl Shell {
    /// Creates an empty shell.
    #[must_use]
    pub fn new(config: ShellConfig) -> Self {
        Self {
            config,
            entries: Vec::new(),
            active: None,
            next_id: 1,
            overlay: None,
            leader_armed: false,
            dirty: true,
            pulse_elapsed: Duration::ZERO,
            pulse_frame: 0,
            pulse_enabled: true,
            clear_requested: false,
            frame_interval: DEFAULT_FRAME_INTERVAL,
            background_cursor: 0,
            notice: None,
            chrome_hits: Vec::new(),
        }
    }

    /// Replaces the maximum interval used to coalesce dirty frames during
    /// asynchronous attachment.
    ///
    /// A zero duration disables coalescing.
    #[must_use]
    pub const fn with_frame_interval(mut self, frame_interval: Duration) -> Self {
        self.frame_interval = frame_interval;
        self
    }

    /// Enables or disables the ambient title pulse.
    #[must_use]
    pub const fn with_pulse_enabled(mut self, enabled: bool) -> Self {
        self.pulse_enabled = enabled;
        self
    }

    /// Adds a surface and focuses it.
    pub fn add_surface(&mut self, surface: impl Surface + 'static) -> SurfaceId {
        self.add_boxed_surface(Box::new(surface))
    }

    fn add_boxed_surface(&mut self, surface: Box<dyn Surface>) -> SurfaceId {
        if let Some(active) = self.active {
            self.entries[active].surface.blur();
        }

        let id = SurfaceId(self.next_id);
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(Entry { id, surface });
        self.active = Some(self.entries.len() - 1);
        self.entries
            .last_mut()
            .expect("surface was just inserted")
            .surface
            .focus();
        self.dirty = true;
        id
    }

    /// Number of open surfaces.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns true when the shell has no surfaces.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Identity of the active surface, if any.
    #[must_use]
    pub fn active_id(&self) -> Option<SurfaceId> {
        self.active.map(|index| self.entries[index].id)
    }

    /// Identities of open surfaces in display order.
    #[must_use]
    pub fn surface_ids(&self) -> Vec<SurfaceId> {
        self.entries.iter().map(|entry| entry.id).collect()
    }

    /// Focuses a surface by identity.
    pub fn select(&mut self, id: SurfaceId) -> bool {
        let Some(index) = self.entries.iter().position(|entry| entry.id == id) else {
            return false;
        };
        self.select_index(index);
        true
    }

    fn tick_rate(&self) -> Duration {
        self.entries
            .iter()
            .filter_map(|entry| entry.surface.poll_interval())
            .fold(self.config.tick_rate, std::cmp::min)
            .max(Duration::from_millis(1))
    }

    /// Attaches this shell to the current terminal until detach or final close.
    ///
    /// The terminal is restored on success, error, and panic unwinding. Because the
    /// shell is borrowed, a host may call `attach` again without rebuilding surfaces.
    pub fn attach(&mut self) -> io::Result<ExitReason> {
        if self.entries.is_empty() {
            return Ok(ExitReason::NoSurfaces);
        }

        let _lease = AttachLease::acquire()?;
        let mut session = TerminalSession::enter(self.config.mouse_capture)?;
        let mut previous_tick = Instant::now();
        self.dirty = true;

        loop {
            let tick_rate = self.tick_rate();
            if self.clear_requested {
                session.clear_frame()?;
                self.clear_requested = false;
                self.dirty = true;
            }
            if self.dirty {
                session.terminal().draw(|frame| render::draw(frame, self))?;
                self.dirty = false;
            }

            let elapsed = previous_tick.elapsed();
            if elapsed >= tick_rate {
                previous_tick = Instant::now();
                if let ShellSignal::Exit(reason) = self.dispatch_to_all(SurfaceEvent::Tick(elapsed))
                {
                    return Ok(reason);
                }
                if let ShellSignal::Exit(reason) = self.poll_background_sync() {
                    return Ok(reason);
                }
                // Show lifecycle-driven changes before waiting for more input.
                continue;
            }

            let timeout = tick_rate.saturating_sub(elapsed);
            let signal = if event::poll(timeout)? {
                self.handle_event(event::read()?)
            } else {
                let elapsed = previous_tick.elapsed();
                previous_tick = Instant::now();
                let tick = self.dispatch_to_all(SurfaceEvent::Tick(elapsed));
                if matches!(tick, ShellSignal::Continue) {
                    self.poll_background_sync()
                } else {
                    tick
                }
            };

            if let ShellSignal::Exit(reason) = signal {
                return Ok(reason);
            }
        }
    }

    /// Attaches this shell using event-driven terminal, background, redraw, and
    /// timer multiplexing.
    ///
    /// The caller must poll this future inside a Tokio runtime. Dropping the
    /// future restores the terminal before releasing process-global input
    /// ownership.
    #[cfg(feature = "async-shell")]
    pub async fn attach_async(&mut self) -> io::Result<ExitReason> {
        if self.entries.is_empty() {
            return Ok(ExitReason::NoSurfaces);
        }

        let _lease = AttachLease::acquire()?;
        let mut session = TerminalSession::enter(self.config.mouse_capture)?;
        let mut input = EventStream::new();
        let mut previous_tick = Instant::now();
        let mut last_draw = None;
        self.dirty = true;

        loop {
            let now = Instant::now();
            let tick_rate = self.tick_rate();
            let elapsed = now.saturating_duration_since(previous_tick);
            if elapsed >= tick_rate {
                previous_tick = now;
                if let ShellSignal::Exit(reason) = self.dispatch_to_all(SurfaceEvent::Tick(elapsed))
                {
                    return Ok(reason);
                }
            }

            let draw_due = self.dirty
                && (self.frame_interval.is_zero()
                    || last_draw.is_none_or(|drawn: Instant| {
                        now.saturating_duration_since(drawn) >= self.frame_interval
                    }));
            if self.clear_requested && draw_due {
                session.clear_frame()?;
                self.clear_requested = false;
                self.dirty = true;
            }
            if draw_due {
                session.terminal().draw(|frame| render::draw(frame, self))?;
                self.dirty = false;
                last_draw = Some(Instant::now());
                continue;
            }

            let tick_deadline = previous_tick + tick_rate;
            let redraw_deadline = self
                .dirty
                .then(|| last_draw.map_or(now, |drawn| drawn + self.frame_interval));
            let redraw = OptionFuture::from(redraw_deadline.map(|deadline| {
                tokio::time::sleep_until(tokio::time::Instant::from_std(deadline))
            }));
            tokio::pin!(redraw);

            enum AsyncEvent {
                Terminal(Option<io::Result<Event>>),
                Background(usize, SurfaceAction),
                Tick,
                Redraw,
            }

            let next = {
                let background =
                    std::future::poll_fn(|context| self.poll_background_round_robin(context));
                tokio::select! {
                    terminal = input.next() => AsyncEvent::Terminal(terminal),
                    (index, action) = background => AsyncEvent::Background(index, action),
                    _ = tokio::time::sleep_until(
                        tokio::time::Instant::from_std(tick_deadline)
                    ) => AsyncEvent::Tick,
                    _ = &mut redraw, if redraw_deadline.is_some() => AsyncEvent::Redraw,
                }
            };

            let signal = match next {
                AsyncEvent::Terminal(Some(Ok(event))) => self.handle_event(event),
                AsyncEvent::Terminal(Some(Err(error))) => return Err(error),
                AsyncEvent::Terminal(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "terminal event stream closed",
                    ));
                }
                AsyncEvent::Background(index, action) => self.apply_surface_action(index, action),
                AsyncEvent::Tick | AsyncEvent::Redraw => ShellSignal::Continue,
            };
            if let ShellSignal::Exit(reason) = signal {
                return Ok(reason);
            }
        }
    }

    /// Feeds a Crossterm event into the shell state machine.
    pub fn handle_event(&mut self, event: Event) -> ShellSignal {
        match event {
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                self.handle_key(key)
            }
            Event::Key(_) => ShellSignal::Continue,
            Event::Mouse(mouse) => self.handle_mouse(mouse),
            Event::Paste(text) => {
                if matches!(self.overlay, Some(Overlay::Palette { .. })) {
                    self.append_palette_query(&text);
                    ShellSignal::Continue
                } else {
                    self.dispatch_to_active(SurfaceEvent::Paste(text))
                }
            }
            Event::Resize(columns, rows) => {
                self.dirty = true;
                self.dispatch_to_all(SurfaceEvent::Resize { columns, rows })
            }
            Event::FocusGained => {
                self.dirty = true;
                ShellSignal::Continue
            }
            Event::FocusLost => ShellSignal::Continue,
        }
    }

    /// Renders the shell to plain text without taking over a real terminal.
    pub fn render_to_string(&mut self, columns: u16, rows: u16) -> io::Result<String> {
        let backend = TestBackend::new(columns, rows);
        let mut terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(infallible) => match infallible {},
        };
        match terminal.draw(|frame| render::draw(frame, self)) {
            Ok(_) => {}
            Err(infallible) => match infallible {},
        }
        let buffer = terminal.backend().buffer();
        let mut output = String::new();

        for y in 0..rows {
            let mut line = String::new();
            for x in 0..columns {
                line.push_str(buffer[(x, y)].symbol());
            }
            output.push_str(line.trim_end());
            if y + 1 < rows {
                output.push('\n');
            }
        }

        Ok(output)
    }

    fn handle_key(&mut self, key: KeyEvent) -> ShellSignal {
        if self.overlay.is_some() {
            return self.handle_overlay_key(key);
        }

        let policy = self
            .active
            .map(|index| self.entries[index].surface.input_policy())
            .unwrap_or_default();
        if policy == InputPolicy::Exclusive {
            if self.notice.take().is_some() {
                self.dirty = true;
            }
            return self.dispatch_to_active(SurfaceEvent::Key(key));
        }

        if self.leader_armed {
            self.leader_armed = false;
            return self.handle_leader_key(key);
        }

        if key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && self
                .active
                .is_some_and(|index| self.entries[index].surface.opens_action_bar_on_escape())
        {
            self.open_palette();
            return ShellSignal::Continue;
        }

        if ShellBindings::matches(&self.config.bindings.leaders, key) {
            self.leader_armed = true;
            self.notice = Some(format!(
                "{} armed · press a configured action key · Esc cancel",
                self.config.bindings.primary_leader_label()
            ));
            self.dirty = true;
            return ShellSignal::Continue;
        }

        if ShellBindings::matches(&self.config.bindings.palette, key) {
            self.open_palette();
            return ShellSignal::Continue;
        }

        if ShellBindings::matches(&self.config.bindings.redraw, key) {
            self.clear_requested = true;
            self.dirty = true;
            return ShellSignal::Continue;
        }

        // Direct screen navigation remains active while the surface captures
        // ordinary input.
        if ShellBindings::matches(&self.config.bindings.previous_screen, key) {
            self.select_relative(-1);
            return ShellSignal::Continue;
        }
        if ShellBindings::matches(&self.config.bindings.next_screen, key) {
            self.select_relative(1);
            return ShellSignal::Continue;
        }
        if let KeyCode::Char(digit @ '1'..='9') = key.code
            && self.config.bindings.jump_modifiers.contains(&key.modifiers)
        {
            self.select_numbered(digit);
            return ShellSignal::Continue;
        }

        if policy == InputPolicy::Shell {
            if self.config.direct_detach
                && ShellBindings::matches(&self.config.bindings.shell_detach, key)
            {
                return ShellSignal::Exit(ExitReason::Detached);
            }
            if ShellBindings::matches(&self.config.bindings.shell_next_screen, key) {
                self.select_relative(1);
                return ShellSignal::Continue;
            }
            if ShellBindings::matches(&self.config.bindings.shell_previous_screen, key) {
                self.select_relative(-1);
                return ShellSignal::Continue;
            }
            if ShellBindings::matches(&self.config.bindings.shell_help, key) {
                self.overlay = Some(Overlay::Help);
                self.dirty = true;
                return ShellSignal::Continue;
            }
        }

        if self.notice.take().is_some() {
            self.dirty = true;
        }
        self.dispatch_to_active(SurfaceEvent::Key(key))
    }

    fn handle_leader_key(&mut self, key: KeyEvent) -> ShellSignal {
        if self.notice.take().is_some() {
            self.dirty = true;
        }
        let bindings = self.config.bindings.clone();
        if ShellBindings::matches(&bindings.leader_detach, key) {
            ShellSignal::Exit(ExitReason::Detached)
        } else if ShellBindings::matches(&bindings.leader_palette, key) {
            self.open_palette();
            ShellSignal::Continue
        } else if ShellBindings::matches(&bindings.leader_next_screen, key) {
            self.select_relative(1);
            ShellSignal::Continue
        } else if ShellBindings::matches(&bindings.leader_previous_screen, key) {
            self.select_relative(-1);
            ShellSignal::Continue
        } else if ShellBindings::matches(&bindings.leader_scroll_up, key) {
            self.dispatch_to_active(SurfaceEvent::ScrollPageUp)
        } else if ShellBindings::matches(&bindings.leader_scroll_down, key) {
            self.dispatch_to_active(SurfaceEvent::ScrollPageDown)
        } else if let KeyCode::Char(digit @ '1'..='9') = key.code
            && bindings.leader_jump_modifiers.contains(&key.modifiers)
        {
            self.select_numbered(digit);
            ShellSignal::Continue
        } else if ShellBindings::matches(&bindings.leader_close, key) {
            self.close_active()
        } else if ShellBindings::matches(&bindings.leader_help, key) {
            self.overlay = Some(Overlay::Help);
            self.dirty = true;
            ShellSignal::Continue
        } else if key.code == KeyCode::Esc && key.modifiers.is_empty() {
            ShellSignal::Continue
        } else {
            let leader = self.config.bindings.primary_leader_label();
            let help = self
                .config
                .bindings
                .leader_help
                .first()
                .map_or_else(|| "help".to_owned(), |binding| binding.label());
            self.notice = Some(format!(
                "Unknown {leader} chord; press {leader} {help} for help"
            ));
            self.dirty = true;
            ShellSignal::Continue
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> ShellSignal {
        let previous_overlay = self.overlay.clone();
        let mut signal = ShellSignal::Continue;
        match self.overlay.clone() {
            Some(Overlay::Help) => {
                if key.code == KeyCode::Esc && key.modifiers.is_empty() {
                    self.overlay = None;
                }
            }
            Some(Overlay::Palette {
                mut query,
                mut selected,
            }) => {
                let item_count = self.palette_items(&query).len();
                match key.code {
                    KeyCode::Esc => self.overlay = None,
                    _ if ShellBindings::matches(&self.config.bindings.action_next_screen, key) => {
                        signal = self.execute_palette_action(PaletteAction::NextSurface);
                    }
                    _ if ShellBindings::matches(
                        &self.config.bindings.action_previous_screen,
                        key,
                    ) =>
                    {
                        signal = self.execute_palette_action(PaletteAction::PreviousSurface);
                    }
                    _ if ShellBindings::matches(&self.config.bindings.action_scroll_up, key) => {
                        signal = self.execute_palette_action(PaletteAction::ScrollUp);
                    }
                    _ if ShellBindings::matches(&self.config.bindings.action_scroll_down, key) => {
                        signal = self.execute_palette_action(PaletteAction::ScrollDown);
                    }
                    KeyCode::Char(digit @ '1'..='9')
                        if self
                            .config
                            .bindings
                            .action_jump_modifiers
                            .contains(&key.modifiers) =>
                    {
                        let index = usize::from(digit as u8 - b'1');
                        if index < self.entries.len() {
                            signal =
                                self.execute_palette_action(PaletteAction::SelectSurface(index));
                        }
                    }
                    _ if ShellBindings::matches(&self.config.bindings.action_detach, key) => {
                        signal = self.execute_palette_action(PaletteAction::Detach);
                    }
                    _ if ShellBindings::matches(&self.config.bindings.action_close, key) => {
                        signal = self.execute_palette_action(PaletteAction::CloseSurface);
                    }
                    _ if ShellBindings::matches(&self.config.bindings.action_help, key) => {
                        signal = self.execute_palette_action(PaletteAction::Help);
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        selected = wrap_index(selected, 1, item_count);
                        self.overlay = Some(Overlay::Palette { query, selected });
                    }
                    KeyCode::Up | KeyCode::BackTab => {
                        selected = wrap_index(selected, -1, item_count);
                        self.overlay = Some(Overlay::Palette { query, selected });
                    }
                    KeyCode::Enter => {
                        if let Some(action) = self
                            .palette_items(&query)
                            .get(selected)
                            .map(|item| item.action.clone())
                        {
                            signal = self.execute_palette_action(action);
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.overlay = Some(Overlay::Palette { query, selected: 0 });
                    }
                    _ if ShellBindings::matches(&self.config.bindings.action_clear_query, key) => {
                        self.overlay = Some(Overlay::Palette {
                            query: String::new(),
                            selected: 0,
                        });
                    }
                    KeyCode::Char(character)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        query.push(character);
                        self.overlay = Some(Overlay::Palette { query, selected: 0 });
                    }
                    _ => {}
                }
            }
            None => {}
        }
        if self.overlay != previous_overlay {
            self.dirty = true;
        }
        signal
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) -> ShellSignal {
        // Overlays are modal. Until an overlay owns mouse hit regions, consume
        // mouse input instead of leaking it to tabs or hidden surface content.
        if self.overlay.is_some() {
            return ShellSignal::Continue;
        }

        if matches!(mouse.kind, MouseEventKind::Down(_))
            && let Some((_, index)) = self
                .chrome_hits
                .iter()
                .find(|(area, _)| contains(*area, mouse.column, mouse.row))
                .copied()
        {
            self.select_index(index);
            return ShellSignal::Continue;
        }
        self.dispatch_to_active(SurfaceEvent::Mouse(mouse))
    }

    fn poll_background_sync(&mut self) -> ShellSignal {
        let mut context = Context::from_waker(std::task::Waker::noop());
        match self.poll_background_round_robin(&mut context) {
            Poll::Ready((index, action)) => self.apply_surface_action(index, action),
            Poll::Pending => ShellSignal::Continue,
        }
    }

    fn poll_background_round_robin(
        &mut self,
        context: &mut Context<'_>,
    ) -> Poll<(usize, SurfaceAction)> {
        let len = self.entries.len();
        if len == 0 {
            return Poll::Pending;
        }
        let start = self.background_cursor % len;
        for offset in 0..len {
            let index = (start + offset) % len;
            if let Poll::Ready(action) = self.entries[index].surface.poll_background(context) {
                self.background_cursor = (index + 1) % len;
                return Poll::Ready((index, action));
            }
        }
        Poll::Pending
    }

    fn apply_surface_action(&mut self, index: usize, action: SurfaceAction) -> ShellSignal {
        if index >= self.entries.len() {
            return ShellSignal::Continue;
        }
        match action {
            SurfaceAction::Ignored => ShellSignal::Continue,
            SurfaceAction::Consumed => {
                self.dirty = true;
                ShellSignal::Continue
            }
            SurfaceAction::Close => {
                let _ = self.remove_index(index);
                if self.entries.is_empty() {
                    ShellSignal::Exit(ExitReason::NoSurfaces)
                } else {
                    ShellSignal::Continue
                }
            }
            SurfaceAction::Detach => ShellSignal::Exit(ExitReason::Detached),
            SurfaceAction::Open(surface) => {
                if let Some(key) = surface.key()
                    && self
                        .entries
                        .iter()
                        .position(|entry| entry.surface.key().as_deref() == Some(key.as_ref()))
                        .is_some_and(|existing| {
                            self.select_index(existing);
                            true
                        })
                {
                    return ShellSignal::Continue;
                }
                self.add_boxed_surface(surface);
                ShellSignal::Continue
            }
            SurfaceAction::FocusKey(key) => {
                self.select_key(&key);
                ShellSignal::Continue
            }
            SurfaceAction::Reconfigure(config) => {
                self.config = *config;
                for entry in &mut self.entries {
                    entry.surface.reconfigure(&self.config);
                }
                self.dirty = true;
                ShellSignal::Continue
            }
        }
    }

    fn dispatch_to_active(&mut self, event: SurfaceEvent) -> ShellSignal {
        let Some(index) = self.active else {
            return ShellSignal::Exit(ExitReason::NoSurfaces);
        };
        let action = self.entries[index].surface.handle(event);
        self.apply_surface_action(index, action)
    }

    fn dispatch_to_all(&mut self, event: SurfaceEvent) -> ShellSignal {
        let animation_changed = match &event {
            SurfaceEvent::Tick(elapsed) => self.advance_pulse(*elapsed),
            _ => false,
        };
        let actions: Vec<(usize, SurfaceAction)> = self
            .entries
            .iter_mut()
            .enumerate()
            .map(|(index, entry)| (index, entry.surface.handle(event.clone())))
            .collect();

        let mut close_indices = Vec::new();
        let mut open_surfaces = Vec::new();
        let mut focus_keys = Vec::new();
        let mut reconfigure = None;
        let mut detach = false;
        let mut redraw = animation_changed;

        for (index, action) in actions {
            match action {
                SurfaceAction::Ignored => {}
                SurfaceAction::Consumed => redraw = true,
                SurfaceAction::Close => close_indices.push(index),
                SurfaceAction::Detach => detach = true,
                SurfaceAction::Open(surface) => open_surfaces.push(surface),
                SurfaceAction::FocusKey(key) => focus_keys.push(key),
                SurfaceAction::Reconfigure(config) => reconfigure = Some(config),
            }
        }

        // Removing from the end keeps every original action index valid.
        for index in close_indices.into_iter().rev() {
            let _ = self.remove_index(index);
        }
        for surface in open_surfaces {
            self.add_boxed_surface(surface);
        }
        for key in focus_keys {
            self.select_key(&key);
        }
        if let Some(config) = reconfigure {
            self.config = *config;
            for entry in &mut self.entries {
                entry.surface.reconfigure(&self.config);
            }
            redraw = true;
        }
        self.dirty |= redraw;

        if detach {
            ShellSignal::Exit(ExitReason::Detached)
        } else if self.entries.is_empty() {
            ShellSignal::Exit(ExitReason::NoSurfaces)
        } else {
            ShellSignal::Continue
        }
    }

    fn advance_pulse(&mut self, elapsed: Duration) -> bool {
        if !self.pulse_enabled {
            return false;
        }
        let previous_marker = self.pulse_marker();
        self.pulse_elapsed = self.pulse_elapsed.saturating_add(elapsed);
        while self.pulse_elapsed >= PULSE_FRAME_RATE {
            self.pulse_elapsed = self.pulse_elapsed.saturating_sub(PULSE_FRAME_RATE);
            self.pulse_frame = (self.pulse_frame + 1) % PULSE_FRAMES.len();
        }
        self.pulse_marker() != previous_marker
    }

    pub(crate) fn pulse_marker(&self) -> &'static str {
        if self.pulse_enabled {
            PULSE_FRAMES[self.pulse_frame]
        } else {
            ""
        }
    }

    fn open_palette(&mut self) {
        self.overlay = Some(Overlay::Palette {
            query: String::new(),
            selected: 0,
        });
        self.dirty = true;
    }

    fn append_palette_query(&mut self, text: &str) {
        let Some(Overlay::Palette { query, selected }) = &mut self.overlay else {
            return;
        };
        for character in text.chars() {
            if character.is_whitespace() {
                query.push(' ');
            } else if !character.is_control() {
                query.push(character);
            }
        }
        *selected = 0;
        self.dirty = true;
    }

    fn execute_palette_action(&mut self, action: PaletteAction) -> ShellSignal {
        self.overlay = None;
        match action {
            PaletteAction::RunSurfaceCommand(id) => {
                let Some(index) = self.active else {
                    return ShellSignal::Exit(ExitReason::NoSurfaces);
                };
                let action = self.entries[index].surface.execute_command(&id);
                self.apply_surface_action(index, action)
            }
            PaletteAction::SelectSurface(index) => {
                self.select_index(index);
                ShellSignal::Continue
            }
            PaletteAction::NextSurface => {
                self.select_relative(1);
                ShellSignal::Continue
            }
            PaletteAction::PreviousSurface => {
                self.select_relative(-1);
                ShellSignal::Continue
            }
            PaletteAction::ScrollUp => self.dispatch_to_active(SurfaceEvent::ScrollPageUp),
            PaletteAction::ScrollDown => self.dispatch_to_active(SurfaceEvent::ScrollPageDown),
            PaletteAction::CloseSurface => self.close_active(),
            PaletteAction::Detach => ShellSignal::Exit(ExitReason::Detached),
            PaletteAction::Help => {
                self.overlay = Some(Overlay::Help);
                ShellSignal::Continue
            }
        }
    }

    pub(crate) fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = self
            .active_surface()
            .into_iter()
            .flat_map(|surface| surface.commands())
            .map(|command| PaletteItem {
                label: command.label.into_owned(),
                detail: command.description.into_owned(),
                shortcut: command.shortcut.map(std::borrow::Cow::into_owned),
                status: None,
                action: PaletteAction::RunSurfaceCommand(command.id),
            })
            .collect::<Vec<_>>();
        items.extend(self.entries.iter().enumerate().map(|(index, entry)| {
            let status = entry.surface.status();
            PaletteItem {
                label: format!("Switch to {}", entry.surface.title()),
                detail: if self.active == Some(index) {
                    format!("Surface · current · {}", status.label())
                } else {
                    format!("Surface · {}", status.label())
                },
                shortcut: None,
                status: Some(status),
                action: PaletteAction::SelectSurface(index),
            }
        }));
        items.extend([
            PaletteItem {
                label: "Next surface".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_next_screen),
                status: None,
                action: PaletteAction::NextSurface,
            },
            PaletteItem {
                label: "Previous surface".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_previous_screen),
                status: None,
                action: PaletteAction::PreviousSurface,
            },
            PaletteItem {
                label: "Scroll active surface up".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_scroll_up),
                status: None,
                action: PaletteAction::ScrollUp,
            },
            PaletteItem {
                label: "Scroll active surface down".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_scroll_down),
                status: None,
                action: PaletteAction::ScrollDown,
            },
            PaletteItem {
                label: "Close active surface".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_close),
                status: None,
                action: PaletteAction::CloseSurface,
            },
            PaletteItem {
                label: "Detach".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_detach),
                status: None,
                action: PaletteAction::Detach,
            },
            PaletteItem {
                label: "Show keyboard help".to_owned(),
                detail: "Shell".to_owned(),
                shortcut: primary_binding_label(&self.config.bindings.action_help),
                status: None,
                action: PaletteAction::Help,
            },
        ]);
        let mut matched = items
            .into_iter()
            .enumerate()
            .filter_map(|(position, item)| {
                palette_match_score(&item, query).map(|score| (score, position, item))
            })
            .collect::<Vec<_>>();
        if !query.trim().is_empty() {
            matched.sort_by_key(|(score, position, _)| (*score, *position));
        }
        matched.into_iter().map(|(_, _, item)| item).collect()
    }

    fn close_active(&mut self) -> ShellSignal {
        let Some(index) = self.active else {
            return ShellSignal::Exit(ExitReason::NoSurfaces);
        };
        self.remove_index(index)
    }

    fn remove_index(&mut self, index: usize) -> ShellSignal {
        if index >= self.entries.len() {
            return if self.entries.is_empty() {
                ShellSignal::Exit(ExitReason::NoSurfaces)
            } else {
                ShellSignal::Continue
            };
        }

        let previous_active = self.active;
        if previous_active == Some(index) {
            self.entries[index].surface.blur();
        }
        self.entries.remove(index);
        self.dirty = true;
        if self.entries.is_empty() {
            self.active = None;
            return ShellSignal::Exit(ExitReason::NoSurfaces);
        }

        match previous_active {
            Some(active) if active == index => {
                let next = index.min(self.entries.len() - 1);
                self.active = Some(next);
                self.entries[next].surface.focus();
            }
            Some(active) if index < active => self.active = Some(active - 1),
            Some(active) => self.active = Some(active),
            None => self.active = None,
        }
        ShellSignal::Continue
    }

    fn select_relative(&mut self, delta: isize) {
        if let Some(active) = self.active {
            self.select_index(wrap_index(active, delta, self.entries.len()));
        }
    }

    fn select_numbered(&mut self, digit: char) {
        let index = usize::from(digit as u8 - b'1');
        if index < self.entries.len() {
            self.select_index(index);
        } else {
            self.notice = Some(format!(
                "Screen {digit} is not open · {} screen{} available",
                self.entries.len(),
                if self.entries.len() == 1 { "" } else { "s" }
            ));
            self.dirty = true;
        }
    }

    fn select_index(&mut self, index: usize) {
        if self.entries.is_empty() || index >= self.entries.len() || self.active == Some(index) {
            return;
        }
        if let Some(active) = self.active {
            self.entries[active].surface.blur();
        }
        self.active = Some(index);
        self.entries[index].surface.focus();
        self.dirty = true;
    }

    fn select_key(&mut self, key: &str) {
        let Some(index) = self.entries.iter().position(|entry| {
            entry
                .surface
                .key()
                .as_deref()
                .is_some_and(|candidate| candidate == key)
        }) else {
            self.notice = Some(format!("Surface '{key}' is not open"));
            self.dirty = true;
            return;
        };
        self.select_index(index);
    }

    pub(crate) fn entries(&self) -> impl Iterator<Item = (SurfaceId, &dyn Surface)> {
        self.entries
            .iter()
            .map(|entry| (entry.id, entry.surface.as_ref()))
    }

    pub(crate) fn active_index(&self) -> Option<usize> {
        self.active
    }

    pub(crate) fn active_position(&self) -> Option<(usize, usize)> {
        self.active.map(|index| (index + 1, self.entries.len()))
    }

    pub(crate) fn active_surface_mut(&mut self) -> Option<&mut (dyn Surface + '_)> {
        let index = self.active?;
        Some(self.entries[index].surface.as_mut())
    }

    pub(crate) fn active_surface(&self) -> Option<&dyn Surface> {
        self.active
            .map(|index| self.entries[index].surface.as_ref())
    }
}

fn primary_binding_label(bindings: &[KeyBinding]) -> Option<String> {
    bindings.first().map(|binding| binding.label())
}

fn wrap_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    current.wrapping_add_signed(delta) % len
}

fn contains(area: Rect, x: u16, y: u16) -> bool {
    x >= area.x
        && x < area.x.saturating_add(area.width)
        && y >= area.y
        && y < area.y.saturating_add(area.height)
}

fn palette_match_score(item: &PaletteItem, query: &str) -> Option<usize> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    let label = item.label.to_lowercase();
    let detail = item.detail.to_lowercase();
    let haystack = format!("{label} {detail}");
    query
        .split_whitespace()
        .map(|needle| fuzzy_score(needle, &label, &haystack))
        .try_fold(0, |total, score| score.map(|score| total + score))
}

fn fuzzy_score(needle: &str, label: &str, haystack: &str) -> Option<usize> {
    if label == needle {
        return Some(0);
    }
    if label
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| word == needle)
    {
        return Some(10);
    }
    if label
        .split(|character: char| !character.is_alphanumeric())
        .any(|word| word.starts_with(needle))
    {
        return Some(20);
    }
    if let Some(position) = label.find(needle) {
        return Some(30 + position);
    }
    if let Some(position) = haystack.find(needle) {
        return Some(50 + position);
    }

    let mut cursor = 0;
    let mut first = None;
    let mut last = 0;
    for character in needle.chars() {
        let offset = haystack[cursor..].find(character)?;
        let position = cursor + offset;
        first.get_or_insert(position);
        last = position;
        cursor = position + character.len_utf8();
    }
    Some(100 + last.saturating_sub(first.unwrap_or(0)))
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use ratatui::{Frame, layout::Rect};

    use super::*;

    #[test]
    fn native_terminal_selection_is_enabled_by_default() {
        assert!(!ShellConfig::new("test").mouse_capture);
    }

    struct TickSurface {
        redraw: bool,
        poll_interval: Option<Duration>,
    }

    impl Surface for TickSurface {
        fn title(&self) -> Cow<'_, str> {
            "tick".into()
        }

        fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {}

        fn poll_interval(&self) -> Option<Duration> {
            self.poll_interval
        }

        fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
            if matches!(event, SurfaceEvent::Tick(_)) && self.redraw {
                SurfaceAction::Consumed
            } else {
                SurfaceAction::Ignored
            }
        }
    }

    #[test]
    fn ignored_ticks_do_not_request_a_redraw() {
        let mut shell = Shell::new(ShellConfig::new("test"));
        shell.add_surface(TickSurface {
            redraw: false,
            poll_interval: None,
        });
        shell.dirty = false;

        assert_eq!(
            shell.dispatch_to_all(SurfaceEvent::Tick(Duration::from_millis(100))),
            ShellSignal::Continue
        );
        assert!(!shell.dirty);
    }

    #[test]
    fn consumed_ticks_request_a_redraw() {
        let mut shell = Shell::new(ShellConfig::new("test"));
        shell.add_surface(TickSurface {
            redraw: true,
            poll_interval: None,
        });
        shell.dirty = false;

        assert_eq!(
            shell.dispatch_to_all(SurfaceEvent::Tick(Duration::from_millis(100))),
            ShellSignal::Continue
        );
        assert!(shell.dirty);
    }

    #[test]
    fn heartbeat_advances_at_a_low_rate_and_requests_a_redraw() {
        let mut shell = Shell::new(ShellConfig::new("test"));
        shell.add_surface(TickSurface {
            redraw: false,
            poll_interval: None,
        });
        shell.dirty = false;
        let initial = shell.pulse_marker();

        shell.dispatch_to_all(SurfaceEvent::Tick(Duration::from_millis(499)));
        assert!(!shell.dirty);
        assert_eq!(shell.pulse_marker(), initial);

        shell.dispatch_to_all(SurfaceEvent::Tick(Duration::from_millis(1)));
        assert!(shell.dirty);
        assert_eq!(shell.pulse_frame, 2);
    }

    #[test]
    fn reduced_motion_disables_the_ambient_pulse() {
        let mut shell = Shell::new(ShellConfig::new("test")).with_pulse_enabled(false);
        shell.add_surface(TickSurface {
            redraw: false,
            poll_interval: None,
        });
        shell.dirty = false;

        shell.dispatch_to_all(SurfaceEvent::Tick(Duration::from_secs(2)));

        assert!(!shell.dirty);
        assert_eq!(shell.pulse_frame, 0);
        assert_eq!(shell.pulse_marker(), "");
    }

    #[test]
    fn active_surface_can_request_a_faster_tick_rate() {
        let mut shell =
            Shell::new(ShellConfig::new("test").with_tick_rate(Duration::from_millis(100)));
        shell.add_surface(TickSurface {
            redraw: false,
            poll_interval: Some(Duration::from_millis(20)),
        });

        assert_eq!(shell.tick_rate(), Duration::from_millis(20));
    }

    #[test]
    fn surface_cannot_slow_the_configured_tick_rate() {
        let mut shell =
            Shell::new(ShellConfig::new("test").with_tick_rate(Duration::from_millis(10)));
        shell.add_surface(TickSurface {
            redraw: false,
            poll_interval: Some(Duration::from_millis(20)),
        });

        assert_eq!(shell.tick_rate(), Duration::from_millis(10));
    }

    #[test]
    fn redraw_binding_requests_an_explicit_terminal_clear() {
        let mut shell = Shell::new(ShellConfig::new("test"));
        shell.add_surface(TickSurface {
            redraw: false,
            poll_interval: None,
        });
        shell.dirty = false;

        assert_eq!(
            shell.handle_event(Event::Key(
                KeyEvent::new(KeyCode::F(5), KeyModifiers::NONE,)
            )),
            ShellSignal::Continue
        );
        assert!(shell.clear_requested);
        assert!(shell.dirty);
    }

    #[test]
    fn palette_search_prefers_exact_labels_to_subsequence_matches() {
        let exact = PaletteItem {
            label: "Detach".to_owned(),
            detail: "Shell".to_owned(),
            shortcut: None,
            status: None,
            action: PaletteAction::Detach,
        };
        let subsequence = PaletteItem {
            label: "Detach terminal".to_owned(),
            detail: "Dashboard".to_owned(),
            shortcut: None,
            status: None,
            action: PaletteAction::Detach,
        };

        assert!(
            palette_match_score(&exact, "detach") < palette_match_score(&subsequence, "detach")
        );
    }
}
