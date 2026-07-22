use ratatui::style::{Color, Modifier, Style};

/// Semantic styles used by Turtle's shell chrome.
#[derive(Clone, Debug)]
pub struct Theme {
    /// Unselected shell chrome.
    pub chrome: Style,
    /// Secondary explanatory text.
    pub muted: Style,
    /// Active tab and selection.
    pub selected: Style,
    /// Informational accent.
    pub accent: Style,
    /// Working state.
    pub working: Style,
    /// Attention state.
    pub attention: Style,
    /// Failure state.
    pub failed: Style,
    /// Completion state.
    pub complete: Style,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            chrome: Style::default().fg(Color::White),
            muted: Style::default().fg(Color::DarkGray),
            selected: Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
            accent: Style::default().fg(Color::Cyan),
            working: Style::default().fg(Color::Blue),
            attention: Style::default().fg(Color::Yellow),
            failed: Style::default().fg(Color::Red),
            complete: Style::default().fg(Color::Green),
        }
    }
}
