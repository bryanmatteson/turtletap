//! Interactive keybinding capture and review.

use std::{
    borrow::Cow,
    io::{self, IsTerminal as _},
};

use turtletap::{
    BindingId, Chrome, Frame, InputPolicy, KeyBinding, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, Rect, Shell, ShellConfig, Shortcut, Surface, SurfaceAction, SurfaceCommand,
    SurfaceEvent, Theme,
    tui::{
        layout::{Constraint, Direction, Layout},
        style::{Modifier, Style},
        text::{Line, Span},
        widgets::{Block, Borders, Paragraph},
    },
};

const REMAP_BINDING: &str = "keybindings.remap";
const SAVE_BINDING: &str = "keybindings.save";
const CANCEL_REVIEW: &str = "keybindings.cancel";

use crate::{commands::InteractiveOptions, settings};

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

    fn action(&self) -> BindingId {
        BindingId::KEY_BINDINGS[self.selected]
    }

    fn move_selection(&mut self, delta: isize) {
        let last = BindingId::KEY_BINDINGS.len().saturating_sub(1);
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
        let binding = match KeyBinding::from_event(key) {
            Ok(binding) => binding,
            Err(error) => {
                self.notice = Some((
                    true,
                    format!("This terminal key cannot be stored as a TurtleTap binding: {error}"),
                ));
                return SurfaceAction::Consumed;
            }
        };
        let mut proposed = self.config.bindings.clone();
        if let Err(error) = proposed.set_keys(self.action(), vec![binding]) {
            self.notice = Some((true, error.to_string()));
            return SurfaceAction::Consumed;
        }
        match proposed.validate() {
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

    fn commands(&self) -> Vec<SurfaceCommand> {
        match self.mode {
            EditorMode::Browse => vec![
                SurfaceCommand::new(REMAP_BINDING, "Remap selected binding")
                    .with_description("Keybinding editor")
                    .with_shortcut("Enter"),
            ],
            EditorMode::Capture => Vec::new(),
            EditorMode::Review(_) => vec![
                SurfaceCommand::new(SAVE_BINDING, "Save binding")
                    .with_description("Keybinding editor")
                    .with_shortcut("Enter"),
                SurfaceCommand::new(CANCEL_REVIEW, "Cancel change")
                    .with_description("Keybinding editor"),
            ],
        }
    }

    fn execute_command(&mut self, id: &str) -> SurfaceAction {
        match (id, self.mode) {
            (REMAP_BINDING, EditorMode::Browse) => {
                self.mode = EditorMode::Capture;
                self.notice = None;
                SurfaceAction::Consumed
            }
            (SAVE_BINDING, EditorMode::Review(binding)) => self.save(binding),
            (CANCEL_REVIEW, EditorMode::Review(_)) => {
                self.mode = EditorMode::Browse;
                self.notice = None;
                SurfaceAction::Consumed
            }
            _ => SurfaceAction::Ignored,
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
        for (index, action) in BindingId::KEY_BINDINGS
            .iter()
            .copied()
            .enumerate()
            .skip(self.offset)
            .take(height)
        {
            let group = if action.context().label() != last_group {
                last_group = action.context().label();
                action.context().label()
            } else {
                ""
            };
            let keys = self
                .config
                .bindings
                .keys(action)
                .unwrap_or_default()
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
        let current = self
            .config
            .bindings
            .keys(action)
            .unwrap_or_default()
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
            Line::from(format!(
                "Action:  {} / {}",
                action.context().label(),
                action.label()
            )),
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
    fn action_bar_commands_follow_keybinding_editor_mode() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        assert_eq!(editor.commands()[0].id, REMAP_BINDING);

        assert!(matches!(
            editor.execute_command(REMAP_BINDING),
            SurfaceAction::Consumed
        ));
        assert_eq!(editor.mode, EditorMode::Capture);
        assert!(editor.commands().is_empty());
    }

    #[test]
    fn conflicts_remain_in_capture_with_an_explanation() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        editor.mode = EditorMode::Capture;

        let action = editor.handle(key(KeyCode::Char('`'), KeyModifiers::CONTROL));

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

    #[test]
    fn typing_context_rejects_unmodified_characters_and_arrows() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        editor.selected = BindingId::KEY_BINDINGS
            .iter()
            .position(|action| *action == BindingId::SessionInterrupt)
            .expect("session interrupt should be editable");
        editor.mode = EditorMode::Capture;

        let _ = editor.handle(key(KeyCode::Char('z'), KeyModifiers::NONE));
        assert_eq!(editor.mode, EditorMode::Capture);
        assert!(
            editor.notice.as_ref().is_some_and(
                |(failed, message)| *failed && message.contains("context accepts text")
            )
        );

        let _ = editor.handle(key(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(editor.mode, EditorMode::Capture);
    }

    #[test]
    fn dashboard_context_still_accepts_single_key_shortcuts() {
        let mut editor = KeybindingEditor::new(ShellConfig::new("test"), false);
        editor.selected = BindingId::KEY_BINDINGS
            .iter()
            .position(|action| *action == BindingId::DashboardView)
            .expect("dashboard view should be editable");
        editor.mode = EditorMode::Capture;

        let _ = editor.handle(key(KeyCode::Char('z'), KeyModifiers::NONE));

        assert_eq!(
            editor.mode,
            EditorMode::Review(KeyBinding::new(KeyCode::Char('z'), KeyModifiers::NONE))
        );
    }
}
