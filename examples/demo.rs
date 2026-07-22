//! Interactive demonstration of shell-managed and captured surfaces.

use std::borrow::Cow;

use turtletap::{
    Frame, InputPolicy, KeyCode, Rect, Shell, ShellConfig, Shortcut, Surface, SurfaceAction,
    SurfaceEvent, SurfaceStatus,
    tui::{
        style::{Modifier, Style},
        text::Line,
        widgets::{Paragraph, Wrap},
    },
};

struct DemoSurface {
    title: &'static str,
    captured: bool,
    lines: Vec<String>,
    working: bool,
}

impl DemoSurface {
    fn new(title: &'static str, captured: bool, lines: &[&str]) -> Self {
        Self {
            title,
            captured,
            lines: lines.iter().map(ToString::to_string).collect(),
            working: true,
        }
    }
}

impl Surface for DemoSurface {
    fn title(&self) -> Cow<'_, str> {
        self.title.into()
    }

    fn status(&self) -> SurfaceStatus {
        if self.working {
            SurfaceStatus::Working
        } else {
            SurfaceStatus::Complete
        }
    }

    fn input_policy(&self) -> InputPolicy {
        if self.captured {
            InputPolicy::Captured
        } else {
            InputPolicy::Shell
        }
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        let mut lines = vec![
            Line::styled(
                if self.captured {
                    "Captured input surface"
                } else {
                    "Shell-managed surface"
                },
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Line::raw(""),
        ];
        lines.extend(self.lines.iter().cloned().map(Line::raw));
        lines.push(Line::raw(""));
        lines.push(Line::raw("Press Enter to toggle completion."));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
    }

    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
        if let SurfaceEvent::Key(key) = event {
            match key.code {
                KeyCode::Enter => {
                    self.working = !self.working;
                    return SurfaceAction::Consumed;
                }
                KeyCode::Char(character) if self.captured => {
                    self.lines.push(format!("received: {character:?}"));
                    return SurfaceAction::Consumed;
                }
                _ => {}
            }
        }
        SurfaceAction::Ignored
    }

    fn shortcuts(&self) -> Vec<Shortcut> {
        vec![Shortcut::new("Enter", "Toggle working/completed state")]
    }
}

fn main() -> std::io::Result<()> {
    let mut shell = Shell::new(ShellConfig::new("TurtleTap demo"));
    shell.add_surface(DemoSurface::new(
        "planner",
        false,
        &["An agent, questionnaire, log, or approval can live here."],
    ));
    shell.add_surface(DemoSurface::new(
        "terminal",
        true,
        &[
            "This surface captures ordinary keys.",
            "Ctrl-D is delivered here; Ctrl-G d detaches.",
        ],
    ));

    let reason = shell.attach()?;
    println!(
        "TurtleTap returned control: {reason:?} ({} surfaces remain)",
        shell.len()
    );
    Ok(())
}
