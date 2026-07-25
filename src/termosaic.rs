//! Termosaic semantic-document integration.
//!
//! This module re-exports the matching Termosaic crates and provides
//! [`SurfaceRenderer`], the retained path from a prepared semantic document to
//! a TurtleTap surface's frame.

use crate::{Frame, Rect};

pub use ::termosaic::*;
pub use termosaic_ratatui::{
    NativeStyleConverter, StyleConverter, to_color, to_line, to_line_with, to_style, to_style_with,
    to_text, to_text_with,
};

/// Retained Termosaic renderer for frame-driven TurtleTap surfaces.
///
/// Prepare a [`Doc`] when surface content changes, retain this renderer with
/// the surface, and call [`SurfaceRenderer::render`] each frame. Termosaic
/// reuses its layout and styled-output storage after warm-up and paints
/// directly into Ratatui's frame buffer.
pub struct SurfaceRenderer<C = NativeStyleConverter> {
    inner: termosaic_ratatui::RatatuiRenderer<C>,
}

impl Default for SurfaceRenderer<NativeStyleConverter> {
    fn default() -> Self {
        Self::new()
    }
}

impl SurfaceRenderer<NativeStyleConverter> {
    /// Creates a renderer with Termosaic's native Ratatui style conversion.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: termosaic_ratatui::RatatuiRenderer::new(),
        }
    }
}

impl<C: StyleConverter> SurfaceRenderer<C> {
    /// Creates a renderer with application-owned color and style policy.
    #[must_use]
    pub fn with_converter(converter: C) -> Self {
        Self {
            inner: termosaic_ratatui::RatatuiRenderer::with_converter(converter),
        }
    }

    /// Lays out and paints a prepared document in a surface's content area.
    ///
    /// The document is re-laid out at the current area width, so terminal
    /// resizes do not require surface-owned width caches. Empty areas are a
    /// no-op.
    pub fn render(
        &mut self,
        frame: &mut Frame<'_>,
        area: Rect,
        document: &PreparedDoc,
        theme: &Theme,
        options: HumanLayoutOptions,
    ) -> Result<(), HumanLayoutError> {
        self.inner
            .render_prepared_doc(document, theme, options, area, frame.buffer_mut())
    }

    /// Releases retained Termosaic rendering capacity.
    pub fn release_capacity(&mut self) {
        self.inner.release_capacity();
    }
}
