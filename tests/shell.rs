//! Behavioral tests for TurtleTap's host-independent shell state machine.

use std::{
    borrow::Cow,
    sync::{Arc, Mutex},
};

use crossterm::event::{MouseButton, MouseEventKind};
use turtletap::{
    Chrome, Event, Frame, InputPolicy, KeyBinding, KeyCode, KeyEvent, KeyModifiers, MouseEvent,
    Rect, Shell, ShellBindings, ShellConfig, ShellSignal, Surface, SurfaceAction, SurfaceEvent,
    SurfaceId,
};

struct Probe {
    title: &'static str,
    key: Option<&'static str>,
    policy: InputPolicy,
    escape_opens_action_bar: bool,
    events: Arc<Mutex<Vec<SurfaceEvent>>>,
}

impl Probe {
    fn new(title: &'static str, policy: InputPolicy) -> (Self, Arc<Mutex<Vec<SurfaceEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                title,
                key: None,
                policy,
                escape_opens_action_bar: false,
                events: Arc::clone(&events),
            },
            events,
        )
    }

    fn with_empty_prompt_escape(mut self) -> Self {
        self.escape_opens_action_bar = true;
        self
    }

    fn with_key(mut self, key: &'static str) -> Self {
        self.key = Some(key);
        self
    }
}

impl Surface for Probe {
    fn title(&self) -> Cow<'_, str> {
        self.title.into()
    }

    fn key(&self) -> Option<Cow<'_, str>> {
        self.key.map(Into::into)
    }

    fn input_policy(&self) -> InputPolicy {
        self.policy
    }

    fn opens_action_bar_on_escape(&self) -> bool {
        self.escape_opens_action_bar
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

struct OpenKeyedProbe;

impl Surface for OpenKeyedProbe {
    fn title(&self) -> Cow<'_, str> {
        "opener".into()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget("opener", area);
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        if matches!(event, SurfaceEvent::Key(_)) {
            let (surface, _) = Probe::new("replacement", InputPolicy::Shell);
            SurfaceAction::open(surface.with_key("shared"))
        } else {
            SurfaceAction::Ignored
        }
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
fn opening_a_surface_with_an_existing_key_focuses_without_duplicating_it() {
    let (existing, _) = Probe::new("existing", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Talos"));
    let existing_id = shell.add_surface(existing.with_key("shared"));
    shell.add_surface(OpenKeyedProbe);

    assert_eq!(
        shell.handle_event(key(KeyCode::Enter, KeyModifiers::NONE)),
        ShellSignal::Continue
    );
    assert_eq!(shell.len(), 2);
    assert_eq!(shell.active_id(), Some(existing_id));
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
fn ctrl_backtick_opens_the_action_bar_over_a_captured_surface() {
    let (surface, events) = Probe::new("terminal", InputPolicy::Captured);
    let mut shell = Shell::new(ShellConfig::new("Talos"));
    shell.add_surface(surface);

    let signal = shell.handle_event(key(KeyCode::Char('`'), KeyModifiers::CONTROL));
    let rendered = shell
        .render_to_string(72, 12)
        .expect("action bar should render");

    assert_eq!(signal, ShellSignal::Continue);
    assert!(rendered.contains("Action bar"));
    assert!(
        events
            .lock()
            .expect("probe lock should be healthy")
            .is_empty()
    );
}

#[test]
fn exclusive_surface_receives_shell_shortcuts_verbatim() {
    let (surface, events) = Probe::new("capture", InputPolicy::Exclusive);
    let mut shell = Shell::new(ShellConfig::new("Talos"));
    shell.add_surface(surface);

    assert_eq!(
        shell.handle_event(key(KeyCode::Char('`'), KeyModifiers::CONTROL)),
        ShellSignal::Continue
    );
    assert_eq!(
        shell.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        ShellSignal::Continue
    );

    let events = events.lock().expect("probe lock should be healthy");
    assert!(matches!(
        events.as_slice(),
        [SurfaceEvent::Key(first), SurfaceEvent::Key(second)]
            if first.code == KeyCode::Char('`')
                && first.modifiers == KeyModifiers::CONTROL
                && second.code == KeyCode::Char('g')
                && second.modifiers == KeyModifiers::CONTROL
    ));
}

#[test]
fn escape_opens_the_action_bar_only_when_the_surface_reports_an_empty_prompt() {
    let (empty, empty_events) = Probe::new("empty", InputPolicy::Captured);
    let mut empty_shell = Shell::new(ShellConfig::new("Talos"));
    empty_shell.add_surface(empty.with_empty_prompt_escape());

    assert_eq!(
        empty_shell.handle_event(key(KeyCode::Esc, KeyModifiers::empty())),
        ShellSignal::Continue
    );
    assert!(
        empty_shell
            .render_to_string(72, 12)
            .expect("action bar should render")
            .contains("Action bar")
    );
    assert!(
        empty_events
            .lock()
            .expect("probe lock should be healthy")
            .is_empty()
    );

    let (editing, editing_events) = Probe::new("editing", InputPolicy::Captured);
    let mut editing_shell = Shell::new(ShellConfig::new("Talos"));
    editing_shell.add_surface(editing);

    assert_eq!(
        editing_shell.handle_event(key(KeyCode::Esc, KeyModifiers::empty())),
        ShellSignal::Continue
    );
    assert!(matches!(
        editing_events
            .lock()
            .expect("probe lock should be healthy")
            .as_slice(),
        [SurfaceEvent::Key(key)] if key.code == KeyCode::Esc
    ));
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
fn direct_screen_bindings_switch_and_jump_across_captured_surfaces() {
    let (first, _) = Probe::new("first", InputPolicy::Captured);
    let (second, _) = Probe::new("second", InputPolicy::Captured);
    let (third, third_events) = Probe::new("third", InputPolicy::Captured);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    let first_id = shell.add_surface(first);
    let second_id = shell.add_surface(second);
    let third_id = shell.add_surface(third);

    shell.handle_event(key(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(shell.active_id(), Some(third_id));
    assert!(matches!(
        third_events
            .lock()
            .expect("probe lock should be healthy")
            .last(),
        Some(SurfaceEvent::Key(key))
            if key.code == KeyCode::Left && key.modifiers == KeyModifiers::ALT
    ));

    shell.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Left, KeyModifiers::empty()));
    assert_eq!(shell.active_id(), Some(second_id));

    shell.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('1'), KeyModifiers::empty()));
    assert_eq!(shell.active_id(), Some(first_id));
}

#[test]
fn leader_vertical_arrows_dispatch_scrollback_events() {
    let (surface, events) = Probe::new("command", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(surface);

    for (arrow, expected) in [
        (KeyCode::Up, SurfaceEvent::ScrollPageUp),
        (KeyCode::Down, SurfaceEvent::ScrollPageDown),
    ] {
        shell.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        shell.handle_event(key(arrow, KeyModifiers::empty()));
        assert!(matches!(
            events.lock().expect("probe lock should be healthy").last(),
            Some(event)
                if std::mem::discriminant(event) == std::mem::discriminant(&expected)
        ));
    }
}

#[test]
fn host_can_replace_the_default_screen_bindings() {
    let (first, _) = Probe::new("first", InputPolicy::Captured);
    let (second, _) = Probe::new("second", InputPolicy::Captured);
    let bindings = ShellBindings {
        next_screen: vec![KeyBinding::new(KeyCode::Char('j'), KeyModifiers::CONTROL)],
        previous_screen: Vec::new(),
        ..ShellBindings::default()
    };
    let mut shell = Shell::new(ShellConfig::new("Koda").with_bindings(bindings));
    let first_id = shell.add_surface(first);
    shell.add_surface(second);

    shell.handle_event(key(KeyCode::Right, KeyModifiers::ALT));
    assert_ne!(shell.active_id(), Some(first_id));

    shell.handle_event(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
    assert_eq!(shell.active_id(), Some(first_id));
}

#[test]
fn leader_and_action_bar_actions_use_the_configured_bindings() {
    let (first, _) = Probe::new("first", InputPolicy::Captured);
    let (second, _) = Probe::new("second", InputPolicy::Captured);
    let bindings = ShellBindings {
        palette: vec![KeyBinding::new(KeyCode::Char('o'), KeyModifiers::CONTROL)],
        leader_next_screen: vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::empty())],
        action_next_screen: vec![KeyBinding::new(KeyCode::Char('n'), KeyModifiers::ALT)],
        ..ShellBindings::default()
    };
    let mut shell = Shell::new(ShellConfig::new("Koda").with_bindings(bindings));
    let first_id = shell.add_surface(first);
    let second_id = shell.add_surface(second);

    shell.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('z'), KeyModifiers::empty()));
    assert_eq!(shell.active_id(), Some(first_id));

    shell.handle_event(key(KeyCode::Char('o'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('n'), KeyModifiers::ALT));
    assert_eq!(shell.active_id(), Some(second_id));
}

#[test]
fn contextual_help_renders_the_configured_primary_bindings() {
    let (surface, _) = Probe::new("terminal", InputPolicy::Captured);
    let bindings = ShellBindings {
        leaders: vec![KeyBinding::new(KeyCode::Char('a'), KeyModifiers::CONTROL)],
        ..ShellBindings::default()
    };
    let mut shell = Shell::new(ShellConfig::new("Koda").with_bindings(bindings));
    shell.add_surface(surface);

    shell.handle_event(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('?'), KeyModifiers::empty()));
    let rendered = shell.render_to_string(80, 18).expect("help should render");

    assert!(rendered.contains("Ctrl-A D"));
    assert!(!rendered.contains("Ctrl-G D"));
}

#[test]
fn palette_numbers_select_the_visible_surface() {
    let (first, _) = Probe::new("first", InputPolicy::Shell);
    let (second, _) = Probe::new("second", InputPolicy::Shell);
    let (third, _) = Probe::new("third", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    let first_id = shell.add_surface(first);
    shell.add_surface(second);
    shell.add_surface(third);

    shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));

    assert_eq!(shell.active_id(), Some(first_id));
}

#[test]
fn palette_filters_surfaces_and_opens_the_match() {
    let (first, _) = Probe::new("first agent", InputPolicy::Shell);
    let (second, _) = Probe::new("second terminal", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    let first_id = shell.add_surface(first);
    shell.add_surface(second);

    shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    for character in "first".chars() {
        shell.handle_event(key(KeyCode::Char(character), KeyModifiers::empty()));
    }
    let rendered = shell
        .render_to_string(72, 12)
        .expect("palette should render");
    assert!(rendered.contains("Action bar"));
    assert!(rendered.contains("Switch to first agent"));
    assert!(!rendered.contains("Switch to second terminal"));

    shell.handle_event(key(KeyCode::Enter, KeyModifiers::empty()));
    assert_eq!(shell.active_id(), Some(first_id));
}

#[test]
fn palette_runs_shell_actions() {
    let (surface, _) = Probe::new("agent", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(surface);

    shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    for character in "dtch".chars() {
        shell.handle_event(key(KeyCode::Char(character), KeyModifiers::empty()));
    }

    assert_eq!(
        shell.handle_event(key(KeyCode::Enter, KeyModifiers::empty())),
        ShellSignal::Exit(turtletap::ExitReason::Detached)
    );
}

#[test]
fn action_bar_alt_shortcuts_switch_jump_and_scroll() {
    let (first, first_events) = Probe::new("first", InputPolicy::Shell);
    let (second, _) = Probe::new("second", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    let first_id = shell.add_surface(first);
    let second_id = shell.add_surface(second);

    shell.handle_event(key(KeyCode::Char('`'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Left, KeyModifiers::ALT));
    assert_eq!(shell.active_id(), Some(first_id));

    shell.handle_event(key(KeyCode::Char('`'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Right, KeyModifiers::ALT));
    assert_eq!(shell.active_id(), Some(second_id));

    shell.handle_event(key(KeyCode::Char('`'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Char('1'), KeyModifiers::ALT));
    assert_eq!(shell.active_id(), Some(first_id));

    shell.handle_event(key(KeyCode::Char('`'), KeyModifiers::CONTROL));
    shell.handle_event(key(KeyCode::Up, KeyModifiers::ALT));
    assert!(matches!(
        first_events
            .lock()
            .expect("probe lock should be healthy")
            .last(),
        Some(SurfaceEvent::ScrollPageUp)
    ));
}

#[test]
fn palette_accepts_pasted_queries_and_reports_no_matches() {
    let (surface, events) = Probe::new("agent", InputPolicy::Shell);
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(surface);

    shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    shell.handle_event(Event::Paste("nothing matches this".to_owned()));
    let rendered = shell
        .render_to_string(72, 12)
        .expect("palette should render");

    assert!(rendered.contains("No matching commands"));
    assert!(
        events
            .lock()
            .expect("probe lock should be healthy")
            .is_empty()
    );
}

#[test]
fn action_bar_query_clear_uses_the_configured_binding() {
    let (surface, _) = Probe::new("agent", InputPolicy::Shell);
    let bindings = ShellBindings {
        action_clear_query: vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::ALT)],
        ..ShellBindings::default()
    };
    let mut shell = Shell::new(ShellConfig::new("Koda").with_bindings(bindings));
    shell.add_surface(surface);

    shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    shell.handle_event(Event::Paste("missing".to_owned()));
    shell.handle_event(key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert!(
        shell
            .render_to_string(72, 12)
            .expect("palette should render")
            .contains("missing")
    );

    shell.handle_event(key(KeyCode::Char('z'), KeyModifiers::ALT));
    assert!(
        !shell
            .render_to_string(72, 12)
            .expect("palette should render")
            .contains("missing")
    );
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
    assert!(rendered.contains("Ctrl-` Alt-Left/Ctrl-` Alt-Right"));
    assert!(rendered.contains("screen 2/2"));
}

#[test]
fn overflowing_tabs_keep_the_active_screen_visible() {
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    for title in ["one-long", "two-long", "three-long", "four-long"] {
        let (surface, _) = Probe::new(title, InputPolicy::Captured);
        shell.add_surface(surface);
    }

    let rendered = shell
        .render_to_string(34, 8)
        .expect("narrow shell should render");
    let tab_row = rendered.lines().next().expect("shell has a tab row");

    assert!(tab_row.contains("4:○ four-long"), "{tab_row:?}");
    assert!(tab_row.contains('‹'), "{tab_row:?}");
    assert!(!tab_row.contains("one-long"), "{tab_row:?}");
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

/// A surface that reports a badge, used to prove chrome renders it in its own
/// column rather than requiring implementors to bake it into the title.
struct Badged {
    title: &'static str,
    badge: &'static str,
}

impl Surface for Badged {
    fn key(&self) -> Option<Cow<'_, str>> {
        Some(self.title.into())
    }

    fn title(&self) -> Cow<'_, str> {
        self.title.into()
    }

    fn badge(&self) -> Option<Cow<'_, str>> {
        Some(self.badge.into())
    }

    fn wide_badge(&self) -> Option<Cow<'_, str>> {
        Some(format!("{} 3m", self.badge).into())
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget(format!("{} body", self.title), area);
    }

    fn handle(&mut self, _event: SurfaceEvent) -> SurfaceAction {
        SurfaceAction::Consumed
    }
}

struct FocusOnHandle(&'static str);

impl Surface for FocusOnHandle {
    fn title(&self) -> Cow<'_, str> {
        "requester".into()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        frame.render_widget("requester body", area);
    }

    fn handle(&mut self, _event: SurfaceEvent) -> SurfaceAction {
        SurfaceAction::focus_key(self.0)
    }
}

fn railed(surfaces: &[&'static str]) -> Shell {
    let mut shell = Shell::new(ShellConfig::new("Koda").with_chrome(Chrome::rail()));
    for title in surfaces {
        shell.add_surface(Badged { title, badge: "+7" });
    }
    shell
}

#[test]
fn a_surface_can_focus_an_existing_surface_by_stable_key() {
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    let target = shell.add_surface(Badged {
        title: "session:target",
        badge: "+7",
    });
    shell.add_surface(FocusOnHandle("session:target"));

    shell.handle_event(key(KeyCode::Enter, KeyModifiers::empty()));

    assert_eq!(shell.active_id(), Some(target));
    assert_eq!(shell.len(), 2);
}

#[test]
fn rail_lists_every_surface_with_its_badge() {
    let mut shell = railed(&["build", "tests", "deploy"]);

    let frame = shell
        .render_to_string(90, 12)
        .expect("rail should render headlessly");

    for title in ["build", "tests", "deploy"] {
        assert!(frame.contains(title), "{title} missing from rail: {frame}");
    }
    assert!(frame.contains("+7"), "badge missing from rail: {frame}");
}

#[test]
fn tabs_append_the_structured_badge() {
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(Badged {
        title: "build",
        badge: "+7",
    });

    let frame = shell.render_to_string(90, 8).expect("tabs should render");
    let first = frame.lines().next().unwrap_or_default();

    assert!(first.contains("build +7"), "tab badge missing: {frame}");
}

#[test]
fn wide_rail_uses_the_richer_badge() {
    let mut shell = railed(&["build"]);

    let frame = shell
        .render_to_string(120, 8)
        .expect("wide rail should render");

    assert!(frame.contains("+7 3m"), "wide badge missing: {frame}");
}

#[test]
fn clicking_a_rail_row_selects_that_surface() {
    let mut shell = railed(&["build", "tests", "deploy"]);
    let ids = shell.surface_ids();
    // Draw once so the chrome publishes its hit rectangles.
    let _ = shell.render_to_string(90, 12).expect("render");

    // Row 0 of the rail sits on terminal row 1, under the title row.
    shell.handle_event(click(2, 2));

    assert_eq!(
        shell.active_id(),
        Some(ids[1]),
        "clicking the second rail row should select the second surface"
    );
}

#[test]
fn clicking_a_scrolled_rail_row_selects_the_visible_surface() {
    let titles = [
        "s00", "s01", "s02", "s03", "s04", "s05", "s06", "s07", "s08", "s09", "s10", "s11",
    ];
    let mut shell = railed(&titles);
    let ids = shell.surface_ids();
    let frame = shell.render_to_string(90, 7).expect("scrolled rail");
    let first_row = frame.lines().nth(1).unwrap_or_default();
    let visible = titles
        .iter()
        .position(|title| first_row.contains(title))
        .expect("first rail row should contain a surface title");

    shell.handle_event(click(2, 1));

    assert_eq!(
        shell.active_id(),
        Some(ids[visible]),
        "first visible row was {visible}, but its hitbox selected another surface:\n{frame}"
    );
}

/// The rail's width tiers switch at width + min_content, which is 72 for the
/// defaults. Below it the list narrows to markers; it never becomes a tab strip,
/// because moving the list from the left edge to the top row on a resize would
/// invalidate both pointer targets and spatial memory.
#[test]
fn rail_narrows_to_markers_at_its_breakpoint_rather_than_reverting_to_tabs() {
    let mut shell = railed(&["build", "tests"]);

    let full = shell.render_to_string(72, 10).expect("render at 72");
    let narrow = shell.render_to_string(71, 10).expect("render at 71");

    assert!(
        full.contains("build"),
        "72 columns should show titles: {full}"
    );
    assert!(
        !narrow.contains("build"),
        "71 columns should drop titles rather than truncate them: {narrow}"
    );
    assert!(
        narrow.contains('○'),
        "narrow rail must still carry status markers: {narrow}"
    );
    // A tab strip would have put the surface list on the same row as the host
    // title; the rail keeps it on the left at every width.
    let first = narrow.lines().next().unwrap_or_default();
    assert!(
        !first.contains('○'),
        "narrow rail must not fall back to a tab strip: {narrow}"
    );
}

/// The rail is a view of the shell's existing selection state, not a second
/// selection model, so every navigation binding must move it identically.
#[test]
fn keyboard_selection_behaves_identically_in_both_chrome_modes() {
    let titles = ["build", "tests", "deploy"];
    let mut rail = railed(&titles);
    let mut tabs = Shell::new(ShellConfig::new("Koda"));
    for title in titles {
        tabs.add_surface(Badged { title, badge: "+7" });
    }

    for shell in [&mut rail, &mut tabs] {
        let first = shell.surface_ids()[0];
        shell.select(first);
    }
    for code in [KeyCode::Right, KeyCode::Left] {
        rail.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        tabs.handle_event(key(KeyCode::Char('g'), KeyModifiers::CONTROL));
        rail.handle_event(key(code, KeyModifiers::empty()));
        tabs.handle_event(key(code, KeyModifiers::empty()));
        assert_eq!(
            rail.active_id().map(SurfaceId::get),
            tabs.active_id().map(SurfaceId::get),
            "chrome mode must not change which surface a binding selects"
        );
    }

    for shell in [&mut rail, &mut tabs] {
        shell.handle_event(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        shell.handle_event(key(KeyCode::Char('1'), KeyModifiers::empty()));
    }
    assert_eq!(
        rail.active_id().map(SurfaceId::get),
        tabs.active_id().map(SurfaceId::get),
        "palette selection must move both chrome modes identically"
    );
    assert_eq!(
        rail.active_id(),
        Some(rail.surface_ids()[0]),
        "palette number 1 should select the first surface"
    );
}

#[test]
fn tabs_remain_the_default_so_existing_hosts_are_untouched() {
    let mut shell = Shell::new(ShellConfig::new("Koda"));
    shell.add_surface(Badged {
        title: "build",
        badge: "+7",
    });

    let frame = shell.render_to_string(90, 8).expect("render");

    // The tab strip puts the host title and the surface on the same first row.
    let first = frame.lines().next().unwrap_or_default();
    assert!(
        first.contains("Koda") && first.contains("build"),
        "default chrome should still be a tab strip: {frame}"
    );
}
