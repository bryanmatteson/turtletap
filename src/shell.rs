use std::{
    io,
    time::{Duration, Instant},
};

use crossterm::event::{
    self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};

use crate::{
    InputPolicy, Surface, SurfaceAction, SurfaceEvent, Theme, render, terminal::TerminalSession,
};

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

/// One key chord reserved by the shell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    /// The non-modifier key.
    pub code: KeyCode,
    /// Required modifier keys.
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    /// Creates a key binding.
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    fn matches(self, key: KeyEvent) -> bool {
        self.code == key.code && self.modifiers == key.modifiers
    }

    /// Returns the compact label used by help and shell chrome.
    #[must_use]
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push("Super".to_owned());
        }
        parts.push(match self.code {
            KeyCode::Backspace => "Backspace".to_owned(),
            KeyCode::Enter => "Enter".to_owned(),
            KeyCode::Left => "Left".to_owned(),
            KeyCode::Right => "Right".to_owned(),
            KeyCode::Up => "Up".to_owned(),
            KeyCode::Down => "Down".to_owned(),
            KeyCode::Home => "Home".to_owned(),
            KeyCode::End => "End".to_owned(),
            KeyCode::PageUp => "PageUp".to_owned(),
            KeyCode::PageDown => "PageDown".to_owned(),
            KeyCode::Tab => "Tab".to_owned(),
            KeyCode::BackTab => "BackTab".to_owned(),
            KeyCode::Delete => "Delete".to_owned(),
            KeyCode::Insert => "Insert".to_owned(),
            KeyCode::F(number) => format!("F{number}"),
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char(character) => character.to_string().to_uppercase(),
            KeyCode::Esc => "Esc".to_owned(),
            _ => "Key".to_owned(),
        });
        parts.join("-")
    }
}

/// Configurable global navigation bindings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShellBindings {
    /// Chords that arm the shell leader.
    pub leaders: Vec<KeyBinding>,
    /// Chords that open the command palette.
    pub palette: Vec<KeyBinding>,
    /// Chords that focus the next screen.
    pub next_screen: Vec<KeyBinding>,
    /// Chords that focus the previous screen.
    pub previous_screen: Vec<KeyBinding>,
    /// Modifiers which, with digits 1 through 9, jump to a screen.
    pub jump_modifiers: Vec<KeyModifiers>,
}

impl Default for ShellBindings {
    fn default() -> Self {
        Self {
            leaders: vec![
                KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL),
                KeyBinding::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
            ],
            palette: vec![KeyBinding::new(KeyCode::Char('p'), KeyModifiers::CONTROL)],
            next_screen: vec![
                KeyBinding::new(KeyCode::Right, KeyModifiers::ALT),
                KeyBinding::new(KeyCode::PageDown, KeyModifiers::CONTROL),
            ],
            previous_screen: vec![
                KeyBinding::new(KeyCode::Left, KeyModifiers::ALT),
                KeyBinding::new(KeyCode::PageUp, KeyModifiers::CONTROL),
            ],
            jump_modifiers: vec![KeyModifiers::ALT],
        }
    }
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
            .map_or_else(|| "palette".to_owned(), |binding| binding.label())
    }

    pub(crate) fn primary_next_label(&self) -> String {
        self.next_screen
            .first()
            .map_or_else(|| "next".to_owned(), |binding| binding.label())
    }

    pub(crate) fn primary_previous_label(&self) -> String {
        self.previous_screen
            .first()
            .map_or_else(|| "previous".to_owned(), |binding| binding.label())
    }

    pub(crate) fn primary_jump_label(&self) -> String {
        self.jump_modifiers.first().map_or_else(
            || "1…9".to_owned(),
            |modifiers| {
                KeyBinding::new(KeyCode::Char('1'), *modifiers)
                    .label()
                    .replace('1', "1…9")
            },
        )
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PaletteAction {
    SelectSurface(usize),
    NextSurface,
    PreviousSurface,
    CloseSurface,
    Detach,
    Help,
}

pub(crate) struct PaletteItem {
    pub(crate) label: String,
    pub(crate) detail: String,
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
    pub(crate) notice: Option<String>,
    pub(crate) tab_hits: Vec<(Rect, usize)>,
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
            notice: None,
            tab_hits: Vec::new(),
        }
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

    /// Number of currently open surfaces.
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

    /// Attaches this shell to the current terminal until detach or final close.
    ///
    /// The terminal is restored on success, error, and panic unwinding. Because the
    /// shell is borrowed, a host may call `attach` again without rebuilding surfaces.
    pub fn attach(&mut self) -> io::Result<ExitReason> {
        if self.entries.is_empty() {
            return Ok(ExitReason::NoSurfaces);
        }

        let mut session = TerminalSession::enter(self.config.mouse_capture)?;
        let mut previous_tick = Instant::now();
        let tick_rate = self.config.tick_rate.max(Duration::from_millis(1));
        self.dirty = true;

        loop {
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
                // Show lifecycle-driven changes before waiting for more input.
                continue;
            }

            let timeout = tick_rate.saturating_sub(elapsed);
            let signal = if event::poll(timeout)? {
                self.handle_event(event::read()?)
            } else {
                let elapsed = previous_tick.elapsed();
                previous_tick = Instant::now();
                self.dispatch_to_all(SurfaceEvent::Tick(elapsed))
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

        if self.leader_armed {
            self.leader_armed = false;
            return self.handle_leader_key(key);
        }

        if ShellBindings::matches(&self.config.bindings.leaders, key) {
            self.leader_armed = true;
            self.notice =
                Some("Leader: 1-9 jump · n/p switch · d detach · s commands · ? help".to_string());
            self.dirty = true;
            return ShellSignal::Continue;
        }

        if ShellBindings::matches(&self.config.bindings.palette, key) {
            self.open_palette();
            return ShellSignal::Continue;
        }

        // Direct screen navigation is intentionally available even when the
        // active surface captures ordinary input.
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

        let policy = self
            .active
            .map(|index| self.entries[index].surface.input_policy())
            .unwrap_or_default();

        if policy == InputPolicy::Shell {
            if self.config.direct_detach && is_ctrl(key, 'd') {
                return ShellSignal::Exit(ExitReason::Detached);
            }
            match key.code {
                KeyCode::Tab if key.modifiers.is_empty() => {
                    self.select_relative(1);
                    return ShellSignal::Continue;
                }
                KeyCode::BackTab => {
                    self.select_relative(-1);
                    return ShellSignal::Continue;
                }
                KeyCode::Char('?') if key.modifiers.is_empty() => {
                    self.overlay = Some(Overlay::Help);
                    self.dirty = true;
                    return ShellSignal::Continue;
                }
                _ => {}
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
        match key.code {
            KeyCode::Char('d') => ShellSignal::Exit(ExitReason::Detached),
            KeyCode::Char('s') => {
                self.open_palette();
                ShellSignal::Continue
            }
            KeyCode::Char('n') | KeyCode::Tab => {
                self.select_relative(1);
                ShellSignal::Continue
            }
            KeyCode::Char('p') | KeyCode::BackTab => {
                self.select_relative(-1);
                ShellSignal::Continue
            }
            KeyCode::Char(digit @ '1'..='9') => {
                self.select_numbered(digit);
                ShellSignal::Continue
            }
            KeyCode::Char('x') => self.close_active(),
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.overlay = Some(Overlay::Help);
                self.dirty = true;
                ShellSignal::Continue
            }
            KeyCode::Esc => ShellSignal::Continue,
            _ => {
                let leader = self.config.bindings.primary_leader_label();
                self.notice = Some(format!("Unknown {leader} chord; press {leader} ? for help"));
                self.dirty = true;
                ShellSignal::Continue
            }
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> ShellSignal {
        let previous_overlay = self.overlay.clone();
        let mut signal = ShellSignal::Continue;
        match self.overlay.clone() {
            Some(Overlay::Help) => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
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
                            .map(|item| item.action)
                        {
                            signal = self.execute_palette_action(action);
                        }
                    }
                    KeyCode::Backspace => {
                        query.pop();
                        self.overlay = Some(Overlay::Palette { query, selected: 0 });
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        self.overlay = Some(Overlay::Palette {
                            query: String::new(),
                            selected: 0,
                        });
                    }
                    KeyCode::Char(digit @ '1'..='9')
                        if query.is_empty() && key.modifiers.is_empty() =>
                    {
                        let index = usize::from(digit as u8 - b'1');
                        if index < self.entries.len() {
                            signal =
                                self.execute_palette_action(PaletteAction::SelectSurface(index));
                        } else {
                            query.push(digit);
                            self.overlay = Some(Overlay::Palette { query, selected: 0 });
                        }
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
                .tab_hits
                .iter()
                .find(|(area, _)| contains(*area, mouse.column, mouse.row))
                .copied()
        {
            self.select_index(index);
            return ShellSignal::Continue;
        }
        self.dispatch_to_active(SurfaceEvent::Mouse(mouse))
    }

    fn dispatch_to_active(&mut self, event: SurfaceEvent) -> ShellSignal {
        let Some(index) = self.active else {
            return ShellSignal::Exit(ExitReason::NoSurfaces);
        };
        let action = self.entries[index].surface.handle(event);
        match action {
            SurfaceAction::Ignored => ShellSignal::Continue,
            SurfaceAction::Consumed => {
                self.dirty = true;
                ShellSignal::Continue
            }
            SurfaceAction::Close => self.close_active(),
            SurfaceAction::Detach => ShellSignal::Exit(ExitReason::Detached),
            SurfaceAction::Open(surface) => {
                self.add_boxed_surface(surface);
                ShellSignal::Continue
            }
        }
    }

    fn dispatch_to_all(&mut self, event: SurfaceEvent) -> ShellSignal {
        let actions: Vec<(usize, SurfaceAction)> = self
            .entries
            .iter_mut()
            .enumerate()
            .map(|(index, entry)| (index, entry.surface.handle(event.clone())))
            .collect();

        let mut close_indices = Vec::new();
        let mut open_surfaces = Vec::new();
        let mut detach = false;
        let mut redraw = false;

        for (index, action) in actions {
            match action {
                SurfaceAction::Ignored => {}
                SurfaceAction::Consumed => redraw = true,
                SurfaceAction::Close => close_indices.push(index),
                SurfaceAction::Detach => detach = true,
                SurfaceAction::Open(surface) => open_surfaces.push(surface),
            }
        }

        // Removing from the end keeps every original action index valid.
        for index in close_indices.into_iter().rev() {
            let _ = self.remove_index(index);
        }
        for surface in open_surfaces {
            self.add_boxed_surface(surface);
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

    fn open_palette(&mut self) {
        let selected = self.active.unwrap_or(0);
        self.overlay = Some(Overlay::Palette {
            query: String::new(),
            selected,
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
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                let status = entry.surface.status();
                PaletteItem {
                    label: format!("Switch to {}", entry.surface.title()),
                    detail: format!("surface · {}", status.label()),
                    status: Some(status),
                    action: PaletteAction::SelectSurface(index),
                }
            })
            .collect::<Vec<_>>();
        items.extend([
            PaletteItem {
                label: "Next surface".to_owned(),
                detail: format!("shell · {}", self.config.bindings.primary_next_label()),
                status: None,
                action: PaletteAction::NextSurface,
            },
            PaletteItem {
                label: "Previous surface".to_owned(),
                detail: format!("shell · {}", self.config.bindings.primary_previous_label()),
                status: None,
                action: PaletteAction::PreviousSurface,
            },
            PaletteItem {
                label: "Close active surface".to_owned(),
                detail: format!("shell · {} x", self.config.bindings.primary_leader_label()),
                status: None,
                action: PaletteAction::CloseSurface,
            },
            PaletteItem {
                label: "Detach".to_owned(),
                detail: format!("shell · {} d", self.config.bindings.primary_leader_label()),
                status: None,
                action: PaletteAction::Detach,
            },
            PaletteItem {
                label: "Show keyboard help".to_owned(),
                detail: format!("shell · {} ?", self.config.bindings.primary_leader_label()),
                status: None,
                action: PaletteAction::Help,
            },
        ]);
        items
            .into_iter()
            .filter(|item| palette_matches(item, query))
            .collect()
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

fn is_ctrl(key: KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers.contains(KeyModifiers::CONTROL)
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

fn palette_matches(item: &PaletteItem, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let haystack = format!("{} {}", item.label, item.detail).to_lowercase();
    query
        .split_whitespace()
        .all(|needle| fuzzy_subsequence(needle, &haystack))
}

fn fuzzy_subsequence(needle: &str, haystack: &str) -> bool {
    let mut characters = haystack.chars();
    needle
        .chars()
        .all(|needle| characters.by_ref().any(|candidate| candidate == needle))
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
    }

    impl Surface for TickSurface {
        fn title(&self) -> Cow<'_, str> {
            "tick".into()
        }

        fn render(&mut self, _frame: &mut Frame<'_>, _area: Rect) {}

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
        shell.add_surface(TickSurface { redraw: false });
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
        shell.add_surface(TickSurface { redraw: true });
        shell.dirty = false;

        assert_eq!(
            shell.dispatch_to_all(SurfaceEvent::Tick(Duration::from_millis(100))),
            ShellSignal::Continue
        );
        assert!(shell.dirty);
    }
}
