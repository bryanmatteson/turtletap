//! Render a Termosaic semantic document inside a TurtleTap surface.

use std::borrow::Cow;

use turtletap::{
    Frame, Rect, Shell, ShellConfig, Surface, SurfaceAction, SurfaceEvent,
    termosaic::{Doc, HumanLayoutOptions, SurfaceRenderer, TextLayout, Theme, TokenStyle, tokens},
};

struct ReportSurface {
    document: turtletap::termosaic::PreparedDoc,
    theme: Theme,
    renderer: SurfaceRenderer,
}

impl ReportSurface {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let document = Doc::concat([
            Doc::token_text(tokens::TEXT_HEADING, "Build report"),
            Doc::hard_line(),
            Doc::from_text(
                "The semantic report wraps to the available surface width.",
                TextLayout::Prose,
            ),
        ])
        .prepare()?;
        let theme = Theme::builder("report")
            .rule(tokens::TEXT_HEADING, TokenStyle::emphasized())?
            .build()?;
        Ok(Self {
            document,
            theme,
            renderer: SurfaceRenderer::new(),
        })
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
                HumanLayoutOptions::fast(),
            )
            .expect("prepared report should render");
    }

    fn handle(&mut self, _event: SurfaceEvent) -> SurfaceAction {
        SurfaceAction::Ignored
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut shell = Shell::new(ShellConfig::new("Termosaic"));
    shell.add_surface(ReportSurface::new()?);
    let _ = shell.attach()?;
    Ok(())
}
