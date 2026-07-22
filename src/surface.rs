use std::{borrow::Cow, time::Duration};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};

/// Whether the shell may interpret convenient single-key navigation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputPolicy {
    /// TurtleTap handles its direct shortcuts before forwarding unrecognized input.
    #[default]
    Shell,
    /// Turtle forwards ordinary input while reserving screen navigation and the
    /// configured leader chords.
    Captured,
}

/// A surface's current user-visible state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceStatus {
    /// Ready for interaction.
    #[default]
    Ready,
    /// Work is currently progressing.
    Working,
    /// The surface needs the user's attention.
    Attention,
    /// The surface encountered a failure.
    Failed,
    /// The surface completed its work.
    Complete,
}

impl SurfaceStatus {
    /// A compact, color-independent marker for this state.
    #[must_use]
    pub const fn marker(self) -> &'static str {
        match self {
            Self::Ready => "○",
            Self::Working => "●",
            Self::Attention => "!",
            Self::Failed => "×",
            Self::Complete => "✓",
        }
    }

    /// A screen-reader-friendly label for this state.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Working => "working",
            Self::Attention => "attention",
            Self::Failed => "failed",
            Self::Complete => "complete",
        }
    }
}

/// A shortcut shown in contextual help.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Shortcut {
    /// The human-readable key chord.
    pub key: Cow<'static, str>,
    /// The action the key performs.
    pub description: Cow<'static, str>,
}

impl Shortcut {
    /// Creates a contextual shortcut.
    pub fn new(
        key: impl Into<Cow<'static, str>>,
        description: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            key: key.into(),
            description: description.into(),
        }
    }
}

/// Input and lifecycle events delivered to a surface.
#[derive(Clone, Debug)]
pub enum SurfaceEvent {
    /// A keyboard event not consumed by shell navigation.
    Key(KeyEvent),
    /// Bracketed-paste content.
    Paste(String),
    /// A mouse event inside the shell.
    Mouse(MouseEvent),
    /// A periodic opportunity for every open surface to drain channels or
    /// advance animation, including surfaces that are not currently focused.
    Tick(Duration),
    /// The terminal changed size. TurtleTap broadcasts this to every open
    /// surface so background PTYs can update their dimensions immediately.
    Resize {
        /// New terminal width in cells.
        columns: u16,
        /// New terminal height in cells.
        rows: u16,
    },
}

/// A request from a surface back to its shell.
pub enum SurfaceAction {
    /// The event did not change visible state, so no redraw is needed.
    Ignored,
    /// The event changed visible state and requests a redraw.
    Consumed,
    /// Close this surface without terminating unrelated surfaces.
    Close,
    /// Detach the shell and restore the host terminal.
    Detach,
    /// Add and focus another surface.
    Open(Box<dyn Surface>),
}

impl SurfaceAction {
    /// Opens and focuses a new surface.
    pub fn open(surface: impl Surface + 'static) -> Self {
        Self::Open(Box::new(surface))
    }
}

/// One independently navigable item hosted by a [`crate::Shell`].
///
/// Implementations may represent agent sessions, PTYs, forms, approval prompts,
/// log streams, or any other terminal-native interaction. The trait is object-safe
/// so a shell can host heterogeneous surfaces together.
pub trait Surface: Send {
    /// The short title shown in tabs and the command palette.
    fn title(&self) -> Cow<'_, str>;

    /// Current state shown beside the title.
    fn status(&self) -> SurfaceStatus {
        SurfaceStatus::Ready
    }

    /// Determines whether ordinary shell shortcuts may intercept input.
    fn input_policy(&self) -> InputPolicy {
        InputPolicy::Shell
    }

    /// Draws this surface inside the content area owned by TurtleTap.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);

    /// Handles an input or lifecycle event.
    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction;

    /// Returns surface-specific shortcuts for contextual help.
    fn shortcuts(&self) -> Vec<Shortcut> {
        Vec::new()
    }

    /// Called after this surface becomes active.
    fn focus(&mut self) {}

    /// Called before this surface stops being active.
    fn blur(&mut self) {}
}
