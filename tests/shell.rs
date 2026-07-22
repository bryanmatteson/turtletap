//! Behavioral tests for TurtleTap's host-independent shell state machine.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use crossterm::event::{MouseButton, MouseEventKind};
use turtletap::{
    Event, Frame, InputPolicy, KeyCode, KeyEvent, KeyModifiers, MouseEvent, Rect, Shell,
    ShellConfig, ShellSignal, Surface, SurfaceAction, SurfaceEvent,
};

struct Probe {
    title: &'static str,
    policy: InputPolicy,
    events: Arc<Mutex<Vec<SurfaceEvent>>>,
}

impl Probe {
    fn new(title: &'static str, policy: InputPolicy) -> (Self, Arc<Mutex<Vec<SurfaceEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                title,
                policy,
                events: Arc::clone(&events),
            },
            events,
        )
    }
}

impl Surface for Probe {
    fn title(&self) -> Cow<'_, str> {
        self.title.into()
    }

    fn input_policy(&self) -> InputPolicy {
        self.policy
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(format!("{} body", self.title), area);
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        self.events
            .lock()
            .expect("probe lock should be healthy")
            .push(event);
        SurfaceAction::Consumed
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

fn click(column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::empty(),
    })
}

#[test]
fn ctrl_d_detaches_a_shell_managed_surface() {
    let (surface, events) = Probe::new("agent", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(surface);

    let signal = shell.handle_event(key(KeyCode::Char('d'), KeyModifiers::CONTROL));

    assert_eq!(signal, ShellSignal::Exit(turtletap::ExitReason::Detached));
    assert!(
        events
            .lock()
            .expect("probe lock should be healthy")
            .is_empty()
    );
    assert_eq!(shell.len(), 1);
}

#[test]
fn ctrl_d_is_forwarded_to_a_captured_surface() {
    let (surface, events) = Probe::new("terminal", InputPolicy::Captured);
    let mut shell = Shell::new(ShellConfig::new("Talos"));
    shell.add_surface(surface);

    let signal = shell.handle_event(key(KeyCode::Char('d'), KeyModifiers::CONTROL));

    assert_eq!(signal, ShellSignal::Continue);
    let events = events.lock().expect("probe lock should be healthy");
    assert!(matches!(events.as_slice(), [SurfaceEvent::Key(_)]));
}

#[test]
fn leader_d_detaches_from_a_captured_surface() {
    let (surface, events) = Probe::new("terminal", InputPolicy::Captured);
    let mut shell = Shell::new(ShellConfig::new("Talos"));
    shell.add_surface(surface);

    assert_eq!(
        shell.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        ShellSignal::Continue
    );
    assert_eq!(
        shell.handle_event(key(KeyCode::Char('d'), KeyModifiers::empty())),
        ShellSignal::Exit(turtletap::ExitReason::Detached)
    );
    assert!(
        events
            .lock()
            .expect("probe lock should be healthy")
            .is_empty()
    );
}

#[test]
fn switcher_numbers_select_the_visible_item() {
    let (first, _) = Probe::new("first", InputPolicy::Shell);
    let (second, _) = Probe::new("second", InputPolicy::Shell);
    let (third, _) = Probe::new("third", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    let first_id = shell.add_surface(first);
    shell.add_surface(second);
    shell.add_surface(third);

    shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('1'), KeyModifiers::empty()));

    assert_eq!(shell.active_id(), Some(first_id));
}

#[test]
fn rendered_chrome_follows_the_active_surface() {
    let (agent, _) = Probe::new("agent", InputPolicy::Shell);
    let (terminal, _) = Probe::new("terminal", InputPolicy::Captured);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(agent);
    shell.add_surface(terminal);

    let rendered = shell
        .render_to_string(72, 12)
        .expect("off-screen render should succeed");

    assert!(rendered.contains("Koda"));
    assert!(rendered.contains("agent"));
    assert!(rendered.contains("terminal body"));
    assert!(rendered.contains("Ctrl-G"));
    assert!(rendered.contains("detach"));
}

#[test]
fn resize_is_broadcast_to_inactive_surfaces() {
    let (first, first_events) = Probe::new("first", InputPolicy::Shell);
    let (second, second_events) = Probe::new("second", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(first);
    shell.add_surface(second);

    let signal = shell.handle_event(Event::Resize(120, 40));

    assert_eq!(signal, ShellSignal::Continue);
    assert!(matches!(
        first_events
            .lock()
            .expect("probe lock should be healthy")
            .as_slice(),
        [SurfaceEvent::Resize {
            columns: 120,
            rows: 40
        }]
    ));
    assert!(matches!(
        second_events
            .lock()
            .expect("probe lock should be healthy")
            .as_slice(),
        [SurfaceEvent::Resize {
            columns: 120,
            rows: 40
        }]
    ));
}

#[test]
fn modal_overlay_consumes_mouse_input() {
    let (first, _) = Probe::new("first", InputPolicy::Shell);
    let (second, second_events) = Probe::new("second", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(first);
    let second_id = shell.add_surface(second);
    shell
        .render_to_string(72, 12)
        .expect("off-screen render should succeed");

    shell.handle_event(key(KeyCode::Char('?'), KeyModifiers::empty()));
    let signal = shell.handle_event(click(8, 0));

    assert_eq!(signal, ShellSignal::Continue);
    assert_eq!(shell.active_id(), Some(second_id));
    assert!(
        second_events
            .lock()
            .expect("probe lock should be healthy")
            .is_empty()
    );
}

#[test]
fn tab_mouse_hitboxes_use_display_cell_width() {
    let (first, _) = Probe::new("first", InputPolicy::Shell);
    let (second, _) = Probe::new("second", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("界"));
    let first_id = shell.add_surface(first);
    let second_id = shell.add_surface(second);
    shell
        .render_to_string(72, 12)
        .expect("off-screen render should succeed");

    // `界` occupies two cells. Column 5 is still shell-title spacing; the
    // first tab begins at column 6.
    shell.handle_event(click(5, 0));
    assert_eq!(shell.active_id(), Some(second_id));

    shell.handle_event(click(6, 0));
    assert_eq!(shell.active_id(), Some(first_id));
}
