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
    pub mouse_capture: bool,
    /// Semantic shell styles.
    pub theme: Theme,
}

impl ShellConfig {
    /// Creates a configuration with terminal-friendly defaults.
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            tick_rate: Duration::from_millis(100),
            direct_detach: true,
            mouse_capture: true,
            theme: Theme::default(),
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
}

struct Entry {
    id: SurfaceId,
    surface: Box<dyn Surface>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Overlay {
    Switcher { selected: usize },
    Help,
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
            Event::Paste(text) => self.dispatch_to_active(SurfaceEvent::Paste(text)),
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

        if is_ctrl(key, 'g') {
            self.leader_armed = true;
            self.notice = Some("Ctrl-G: d detach · s surfaces · ? help".to_string());
            self.dirty = true;
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
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.open_switcher();
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
                self.open_switcher();
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
            KeyCode::Char('x') => self.close_active(),
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.overlay = Some(Overlay::Help);
                self.dirty = true;
                ShellSignal::Continue
            }
            KeyCode::Esc => ShellSignal::Continue,
            _ => {
                self.notice = Some("Unknown Ctrl-G chord; press Ctrl-G ? for help".to_string());
                self.dirty = true;
                ShellSignal::Continue
            }
        }
    }

    fn handle_overlay_key(&mut self, key: KeyEvent) -> ShellSignal {
        let previous_overlay = self.overlay;
        match self.overlay {
            Some(Overlay::Help) => {
                if matches!(
                    key.code,
                    KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')
                ) {
                    self.overlay = None;
                }
            }
            Some(Overlay::Switcher { selected }) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.overlay = None,
                KeyCode::Down | KeyCode::Char('j') | KeyCode::Tab => {
                    let next = wrap_index(selected, 1, self.entries.len());
                    self.overlay = Some(Overlay::Switcher { selected: next });
                }
                KeyCode::Up | KeyCode::Char('k') | KeyCode::BackTab => {
                    let next = wrap_index(selected, -1, self.entries.len());
                    self.overlay = Some(Overlay::Switcher { selected: next });
                }
                KeyCode::Enter => {
                    self.overlay = None;
                    self.select_index(selected);
                }
                KeyCode::Char(digit @ '1'..='9') => {
                    let index = usize::from(digit as u8 - b'1');
                    if index < self.entries.len() {
                        self.overlay = None;
                        self.select_index(index);
                    }
                }
                _ => {}
            },
            None => {}
        }
        if self.overlay != previous_overlay {
            self.dirty = true;
        }
        ShellSignal::Continue
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

    fn open_switcher(&mut self) {
        let selected = self.active.unwrap_or(0);
        self.overlay = Some(Overlay::Switcher { selected });
        self.dirty = true;
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use ratatui::{Frame, layout::Rect};

    use super::*;

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
