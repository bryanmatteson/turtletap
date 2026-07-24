//! Interactive keybinding capture and review.

use std::{
    borrow::Cow,
    io::{self, IsTerminal as _},
};

use turtletap::{
    Chrome, Frame, InputPolicy, KeyBinding, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, Rect,
    Shell, ShellBindings, ShellConfig, Shortcut, Surface, SurfaceAction, SurfaceEvent, Theme,
    tui::{
        layout::{Constraint, Direction, Layout},
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Paragraph},
    },
};

use crate::{commands::InteractiveOptions, settings};

macro_rules! binding_actions {
    ($(($field:ident, $id:literal, $label:literal, $group:literal)),+ $(,)?) => {
        #[allow(non_camel_case_types)]
        #[derive(Clone, Copy, Debug, Eq, PartialEq)]
        pub(crate) enum BindingAction {
            $($field),+
        }

        impl BindingAction {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$field),+];

            pub(crate) const fn id(self) -> &'static str {
                match self {
                    $(Self::$field => $id),+
                }
            }

            const fn label(self) -> &'static str {
                match self {
                    $(Self::$field => $label),+
                }
            }

            const fn group(self) -> &'static str {
                match self {
                    $(Self::$field => $group),+
                }
            }

            fn bindings(self, bindings: &ShellBindings) -> &[KeyBinding] {
                match self {
                    $(Self::$field => &bindings.$field),+
                }
            }

            pub(crate) fn replace(self, bindings: &mut ShellBindings, binding: KeyBinding) {
                match self {
                    $(Self::$field => bindings.$field = vec![binding]),+
                }
            }
        }
    };
}

binding_actions!(
    (leaders, "leaders", "Leader", "Global"),
    (palette, "palette", "Action bar", "Global"),
    (redraw, "redraw", "Redraw", "Global"),
    (next_screen, "next-screen", "Next screen", "Global"),
    (
        previous_screen,
        "previous-screen",
        "Previous screen",
        "Global"
    ),
    (shell_detach, "shell-detach", "Detach", "Shell"),
    (
        shell_next_screen,
        "shell-next-screen",
        "Next screen",
        "Shell"
    ),
    (
        shell_previous_screen,
        "shell-previous-screen",
        "Previous screen",
        "Shell"
    ),
    (shell_help, "shell-help", "Help", "Shell"),
    (leader_palette, "leader-palette", "Action bar", "Leader"),
    (
        leader_next_screen,
        "leader-next-screen",
        "Next screen",
        "Leader"
    ),
    (
        leader_previous_screen,
        "leader-previous-screen",
        "Previous screen",
        "Leader"
    ),
    (leader_scroll_up, "leader-scroll-up", "Scroll up", "Leader"),
    (
        leader_scroll_down,
        "leader-scroll-down",
        "Scroll down",
        "Leader"
    ),
    (leader_close, "leader-close", "Close", "Leader"),
    (leader_detach, "leader-detach", "Detach", "Leader"),
    (leader_help, "leader-help", "Help", "Leader"),
    (
        action_next_screen,
        "action-next-screen",
        "Next screen",
        "Action bar"
    ),
    (
        action_previous_screen,
        "action-previous-screen",
        "Previous screen",
        "Action bar"
    ),
    (
        action_scroll_up,
        "action-scroll-up",
        "Scroll up",
        "Action bar"
    ),
    (
        action_scroll_down,
        "action-scroll-down",
        "Scroll down",
        "Action bar"
    ),
    (action_close, "action-close", "Close", "Action bar"),
    (action_detach, "action-detach", "Detach", "Action bar"),
    (action_help, "action-help", "Help", "Action bar"),
    (
        action_clear_query,
        "action-clear-query",
        "Clear query",
        "Action bar"
    ),
    (
        session_release_driver,
        "session-release-driver",
        "Release driver",
        "Session"
    ),
    (
        session_take_driver,
        "session-take-driver",
        "Take driver",
        "Session"
    ),
    (session_clear, "session-clear", "Clear", "Session"),
    (
        session_interrupt,
        "session-interrupt",
        "Interrupt",
        "Session"
    ),
    (session_detach, "session-detach", "Detach", "Session"),
    (
        session_delete_to_start,
        "session-delete-to-start",
        "Delete to start",
        "Session"
    ),
    (
        session_word_left,
        "session-word-left",
        "Word left",
        "Session"
    ),
    (
        session_word_right,
        "session-word-right",
        "Word right",
        "Session"
    ),
    (
        session_line_start,
        "session-line-start",
        "Line start",
        "Session"
    ),
    (session_line_end, "session-line-end", "Line end", "Session"),
    (
        session_delete_word,
        "session-delete-word",
        "Delete word",
        "Session"
    ),
    (session_complete, "session-complete", "Complete", "Session"),
    (
        session_scroll_up,
        "session-scroll-up",
        "Scroll up",
        "Session"
    ),
    (
        session_scroll_down,
        "session-scroll-down",
        "Scroll down",
        "Session"
    ),
    (
        session_scroll_top,
        "session-scroll-top",
        "Scroll to top",
        "Session"
    ),
    (
        session_scroll_bottom,
        "session-scroll-bottom",
        "Scroll to bottom",
        "Session"
    ),
    (dashboard_up, "dashboard-up", "Move up", "Dashboard"),
    (dashboard_down, "dashboard-down", "Move down", "Dashboard"),
    (
        dashboard_view,
        "dashboard-view",
        "View session",
        "Dashboard"
    ),
    (
        dashboard_take,
        "dashboard-take",
        "Take session",
        "Dashboard"
    ),
    (dashboard_search, "dashboard-search", "Search", "Dashboard"),
    (dashboard_new, "dashboard-new", "New session", "Dashboard"),
    (
        dashboard_rename,
        "dashboard-rename",
        "Rename session",
        "Dashboard"
    ),
    (
        dashboard_delete,
        "dashboard-delete",
        "Delete session",
        "Dashboard"
    ),
    (
        dashboard_stop,
        "dashboard-stop",
        "Stop resident",
        "Dashboard"
    ),
    (
        dashboard_keybindings,
        "dashboard-keybindings",
        "Edit keybindings",
        "Dashboard"
    ),
    (dashboard_close, "dashboard-close", "Close", "Dashboard"),
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EditorMode {
    Browse,
    Capture,
    Review(KeyBinding),
}

pub(crate) struct KeybindingEditor {
    selected: usize,
    offset: usize,
    mode: EditorMode,
    config: ShellConfig,
    theme: Theme,
    no_color: bool,
    standalone: bool,
    notice: Option<(bool, String)>,
}

impl KeybindingEditor {
    pub(crate) fn new(config: ShellConfig, no_color: bool) -> Self {
        Self {
            selected: 0,
            offset: 0,
            mode: EditorMode::Browse,
            theme: config.theme.clone(),
            config,
            no_color,
            standalone: false,
            notice: None,
        }
    }

    fn standalone(config: ShellConfig, no_color: bool) -> Self {
        Self {
            standalone: true,
            ..Self::new(config, no_color)
        }
    }

    fn action(&self) -> BindingAction {
        BindingAction::ALL[self.selected]
    }

    fn move_selection(&mut self, delta: isize) {
        let last = BindingAction::ALL.len().saturating_sub(1);
        self.selected = if delta.is_negative() {
            self.selected.saturating_sub(delta.unsigned_abs())
        } else {
            (self.selected + delta as usize).min(last)
        };
        self.notice = None;
    }

    fn capture(&mut self, key: KeyEvent) -> SurfaceAction {
        if key.kind != KeyEventKind::Press {
            return SurfaceAction::Ignored;
        }
        let binding = match settings::canonical_binding(KeyBinding::new(key.code, key.modifiers)) {
            Ok(binding) => binding,
            Err(error) => {
                self.notice = Some((true, error.to_string()));
                return SurfaceAction::Consumed;
            }
        };
        let mut proposed = self.config.bindings.clone();
        self.action().replace(&mut proposed, binding);
        match settings::validate_binding_set(&proposed) {
            Ok(()) => {
                self.mode = EditorMode::Review(binding);
                self.notice = None;
            }
            Err(error) => {
                self.notice = Some((true, error.to_string()));
            }
        }
        SurfaceAction::Consumed
    }

    fn save(&mut self, binding: KeyBinding) -> SurfaceAction {
        match settings::save_binding(self.action(), binding) {
            Ok(mut config) => {
                if self.standalone {
                    config.chrome = Chrome::Tabs;
                }
                if self.no_color {
                    config.theme = config.theme.without_color();
                }
                self.config = config;
                self.theme = self.config.theme.clone();
                self.mode = EditorMode::Browse;
                self.notice = Some((
                    false,
                    format!("Saved {} as {}.", self.action().label(), binding.label()),
                ));
                SurfaceAction::Reconfigure(Box::new(self.config.clone()))
            }
            Err(error) => {
                self.notice = Some((true, format!("Could not save: {error}")));
                SurfaceAction::Consumed
            }
        }
    }
}

impl Surface for KeybindingEditor {
    fn title(&self) -> Cow<'_, str> {
        "keybindings".into()
    }

    fn key(&self) -> Option<Cow<'_, str>> {
        Some("turtletap:keybindings".into())
    }

    fn input_policy(&self) -> InputPolicy {
        if self.mode == EditorMode::Capture {
            InputPolicy::Exclusive
        } else {
            InputPolicy::Captured
        }
    }

    fn shortcuts(&self) -> Vec<Shortcut> {
        match self.mode {
            EditorMode::Browse => vec![
                Shortcut::new("↑/↓", "select"),
                Shortcut::new("Enter", "remap"),
                Shortcut::new("Esc/q", "close"),
            ],
            EditorMode::Capture => vec![Shortcut::new("any key", "capture for review")],
            EditorMode::Review(_) => vec![
                Shortcut::new("Enter", "save"),
                Shortcut::new("Esc", "cancel"),
            ],
        }
    }

    fn reconfigure(&mut self, config: &ShellConfig) {
        self.config = config.clone();
        self.theme = config.theme.clone();
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
            ])
            .split(area);
        let heading = match self.mode {
            EditorMode::Browse => "Choose an action, then press Enter to remap it.",
            EditorMode::Capture => "Press the new key combination · nothing is saved yet",
            EditorMode::Review(binding) => {
                return self.render_review(frame, area, binding);
            }
        };
        frame.render_widget(
            Paragraph::new(heading).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Keybinding editor "),
            ),
            rows[0],
        );

        let height = rows[1].height.saturating_sub(2) as usize;
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + height.max(1) {
            self.offset = self.selected + 1 - height.max(1);
        }
        let mut lines = Vec::new();
        let mut last_group = "";
        for (index, action) in BindingAction::ALL
            .iter()
            .copied()
            .enumerate()
            .skip(self.offset)
            .take(height)
        {
            let group = if action.group() != last_group {
                last_group = action.group();
                action.group()
            } else {
                ""
            };
            let keys = action
                .bindings(&self.config.bindings)
                .iter()
                .map(|binding| binding.label())
                .collect::<Vec<_>>()
                .join(", ");
            let marker = if index == self.selected { "›" } else { " " };
            let style = if index == self.selected {
                self.theme.selected
            } else {
                Style::default()
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} {group:<12}"), style),
                Span::styled(format!("{:<24}", action.label()), style),
                Span::styled(keys, style.add_modifier(Modifier::BOLD)),
            ]));
        }
        frame.render_widget(
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
            rows[1],
        );
        let (failed, notice) = self.notice.as_ref().map_or(
            (false, "Changes are previewed before they are written."),
            |(failed, text)| (*failed, text.as_str()),
        );
        let style = if failed {
            self.theme.failed
        } else {
            self.theme.muted
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(notice, style)))
                .block(Block::default().borders(Borders::ALL)),
            rows[2],
        );
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        let SurfaceEvent::Key(key) = event else {
            return SurfaceAction::Ignored;
        };
        if self.mode == EditorMode::Capture {
            return self.capture(key);
        }
        if key.kind != KeyEventKind::Press {
            return SurfaceAction::Ignored;
        }
        match self.mode {
            EditorMode::Browse => match (key.code, key.modifiers) {
                (KeyCode::Up | KeyCode::Char('k'), KeyModifiers::NONE) => {
                    self.move_selection(-1);
                    SurfaceAction::Consumed
                }
                (KeyCode::Down | KeyCode::Char('j'), KeyModifiers::NONE) => {
                    self.move_selection(1);
                    SurfaceAction::Consumed
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    self.mode = EditorMode::Capture;
                    self.notice = None;
                    SurfaceAction::Consumed
                }
                (KeyCode::Esc | KeyCode::Char('q'), KeyModifiers::NONE) => SurfaceAction::Close,
                _ => SurfaceAction::Ignored,
            },
            EditorMode::Review(binding) => match (key.code, key.modifiers) {
                (KeyCode::Enter, KeyModifiers::NONE) => self.save(binding),
                (KeyCode::Esc, KeyModifiers::NONE) => {
                    self.mode = EditorMode::Browse;
                    self.notice = Some((false, "Change discarded.".to_owned()));
                    SurfaceAction::Consumed
                }
                _ => SurfaceAction::Ignored,
            },
            EditorMode::Capture => unreachable!("capture handled above"),
        }
    }
}

impl KeybindingEditor {
    fn render_review(&self, frame: &mut Frame<'_>, area: Rect, binding: KeyBinding) {
        let action = self.action();
        let current = action
            .bindings(&self.config.bindings)
            .iter()
            .map(|binding| binding.label())
            .collect::<Vec<_>>()
            .join(", ");
        let content = vec![
            Line::from(Span::styled(
                "Review change",
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from(format!("Action:  {} / {}", action.group(), action.label())),
            Line::from(format!("Current: {current}")),
            Line::from(format!("New:     {}", binding.label())),
            Line::from(""),
            Line::from("Enter saves to the active config · Esc discards"),
        ];
        frame.render_widget(
            Paragraph::new(content).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Keybinding editor "),
            ),
            area,
        );
    }
}

pub(crate) fn open(options: InteractiveOptions) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the keybinding editor requires an interactive terminal",
        ));
    }
    let mut config = settings::shell_config("TurtleTap keybindings")?;
    config.chrome = Chrome::Tabs;
    if options.no_color {
        config.theme = config.theme.without_color();
    }
    let mut shell = Shell::new(config.clone()).with_pulse_enabled(!options.reduced_motion);
    shell.add_surface(KeybindingEditor::standalone(config, options.no_color));
    let _reason = shell.attach()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> SurfaceEvent {
        SurfaceEvent::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn captured_shell_shortcut_advances_to_review() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        editor.mode = EditorMode::Capture;

        let action = editor.handle(key(KeyCode::F(12), KeyModifiers::NONE));

        assert!(matches!(action, SurfaceAction::Consumed));
        assert_eq!(
            editor.mode,
            EditorMode::Review(KeyBinding::new(KeyCode::F(12), KeyModifiers::NONE))
        );
    }

    #[test]
    fn conflicts_remain_in_capture_with_an_explanation() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        editor.mode = EditorMode::Capture;

        let action = editor.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));

        assert!(matches!(action, SurfaceAction::Consumed));
        assert_eq!(editor.mode, EditorMode::Capture);
        assert!(
            editor
                .notice
                .as_ref()
                .is_some_and(|(failed, message)| *failed && message.contains("assigned to both"))
        );
    }

    #[test]
    fn escape_can_itself_be_captured_then_discarded() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        editor.mode = EditorMode::Capture;

        let _ = editor.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(
            editor.mode,
            EditorMode::Review(KeyBinding::new(KeyCode::Esc, KeyModifiers::NONE))
        );
        let _ = editor.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(editor.mode, EditorMode::Browse);
    }
}
