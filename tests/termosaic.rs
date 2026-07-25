//! Termosaic-to-TurtleTap surface integration.

#![cfg(feature = "termosaic")]

use std::borrow::Cow;

use ratatui::{Terminal, backend::TestBackend, style::Color as RatatuiColor};
use turtletap::{
    Frame, Rect, Shell, ShellConfig, Surface, SurfaceAction, SurfaceEvent,
    termosaic::{
        AnsiColor, Color, Doc, HumanLayoutOptions, SurfaceRenderer, TextLayout, Theme, TokenStyle,
        tokens,
    },
};

struct ReportSurface {
    document: turtletap::termosaic::PreparedDoc,
    theme: Theme,
    renderer: SurfaceRenderer,
}

impl ReportSurface {
    fn new() -> Self {
        let document = Doc::concat([
            Doc::token_text(tokens::TEXT_HEADING, "Build report"),
            Doc::hard_line(),
            Doc::from_text("alpha beta gamma delta epsilon zeta", TextLayout::Prose),
        ])
        .prepare()
        .expect("report document should prepare");
        let theme = Theme::builder("integration")
            .rule(
                tokens::TEXT_HEADING,
                TokenStyle::bold(Color::Ansi(AnsiColor::Cyan)),
            )
            .expect("heading rule should be unique")
            .build()
            .expect("theme should build");
        Self {
            document,
            theme,
            renderer: SurfaceRenderer::new(),
        }
    }
}

impl Surface for ReportSurface {
    fn title(&self) -> Cow<'_, str> {
        "report".into()
    }

    fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.renderer
            .render(
                frame,
                area,
                &self.document,
                &self.theme,
                HumanLayoutOptions::exact(),
            )
            .expect("semantic document should render");
    }

    fn handle(&mut self, _event: SurfaceEvent) -> SurfaceAction {
        SurfaceAction::Ignored
    }
}

#[test]
fn semantic_document_renders_through_a_turtletap_surface() {
    let mut shell = Shell::new(ShellConfig::new("Termosaic"));
    shell.add_surface(ReportSurface::new());

    let output = shell
        .render_to_string(40, 8)
        .expect("off-screen rendering should succeed");

    assert!(output.contains("Build report"), "{output}");
    assert!(
        output.contains("alpha beta gamma delta epsilon zeta"),
        "{output}"
    );
}

#[test]
fn semantic_document_reflows_when_the_surface_width_changes() {
    let mut shell = Shell::new(ShellConfig::new("Termosaic"));
    shell.add_surface(ReportSurface::new());

    let wide = shell
        .render_to_string(40, 10)
        .expect("wide rendering should succeed");
    let narrow = shell
        .render_to_string(18, 10)
        .expect("narrow rendering should succeed");

    assert!(
        wide.contains("alpha beta gamma delta epsilon zeta"),
        "{wide}"
    );
    assert!(narrow.contains("alpha beta"), "{narrow}");
    assert!(!narrow.contains("alpha beta gamma delta"), "{narrow}");
}

#[test]
fn semantic_style_is_painted_into_the_turtletap_frame_buffer() {
    let document = Doc::token_text(tokens::STATUS_SUCCESS, "ready")
        .prepare()
        .expect("status document should prepare");
    let theme = Theme::builder("integration")
        .rule(
            tokens::STATUS_SUCCESS,
            TokenStyle::bold(Color::Ansi(AnsiColor::Green)),
        )
        .expect("status rule should be unique")
        .build()
        .expect("theme should build");
    let mut renderer = SurfaceRenderer::new();
    let backend = TestBackend::new(12, 1);
    let mut terminal = Terminal::new(backend).expect("test terminal should initialize");

    terminal
        .draw(|frame| {
            renderer
                .render(
                    frame,
                    frame.area(),
                    &document,
                    &theme,
                    HumanLayoutOptions::fast(),
                )
                .expect("semantic status should render");
        })
        .expect("test frame should draw");

    let ready = &terminal.backend().buffer()[(0, 0)];
    assert_eq!(ready.symbol(), "r");
    assert_eq!(ready.fg, RatatuiColor::Green);
    assert!(ready.modifier.contains(ratatui::style::Modifier::BOLD));
}
