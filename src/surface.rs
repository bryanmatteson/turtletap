use std::{
    borrow::Cow,
    task::{Context, Poll},
    time::Duration,
};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::{Frame, layout::Rect};

use crate::ShellConfig;

/// Whether the shell may interpret convenient single-key navigation.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum InputPolicy {
    /// TurtleTap handles its direct shortcuts before forwarding unrecognized input.
    #[default]
    Shell,
    /// Turtle forwards ordinary input while reserving screen navigation and the
    /// configured leader chords.
    Captured,
    /// Turtle forwards every key directly to the surface.
    ///
    /// This is intended for short-lived key capture flows. The surface is
    /// responsible for providing and handling its own cancel key while this
    /// policy is active.
    Exclusive,
}

/// A surface's current user-visible state.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SurfaceStatus {
    /// Ready for interaction.
    #[default]
    Ready,
    /// Work is in progress.
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

/// An executable command exposed by the active surface in the action bar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SurfaceCommand {
    /// Stable surface-local identifier passed to [`Surface::execute_command`].
    pub id: Cow<'static, str>,
    /// Short imperative label shown in the action bar.
    pub label: Cow<'static, str>,
    /// Concise context shown after the label.
    pub description: Cow<'static, str>,
    /// Optional direct shortcut shown before the label.
    pub shortcut: Option<Cow<'static, str>>,
}

impl SurfaceCommand {
    /// Creates an executable surface command.
    pub fn new(id: impl Into<Cow<'static, str>>, label: impl Into<Cow<'static, str>>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            description: Cow::Borrowed("Current surface"),
            shortcut: None,
        }
    }

    /// Sets the context displayed after the command label.
    #[must_use]
    pub fn with_description(mut self, description: impl Into<Cow<'static, str>>) -> Self {
        self.description = description.into();
        self
    }

    /// Sets the direct shortcut displayed before the command label.
    #[must_use]
    pub fn with_shortcut(mut self, shortcut: impl Into<Cow<'static, str>>) -> Self {
        self.shortcut = Some(shortcut.into());
        self
    }
}

/// Input and lifecycle events delivered to a surface.
#[derive(Clone, Debug)]
pub enum SurfaceEvent {
    /// A keyboard event not consumed by shell navigation.
    Key(KeyEvent),
    /// Scroll the active surface upward by one viewport.
    ScrollPageUp,
    /// Scroll the active surface downward by one viewport.
    ScrollPageDown,
    /// Bracketed-paste content.
    Paste(String),
    /// A mouse event inside the shell.
    Mouse(MouseEvent),
    /// A periodic opportunity for every open surface to drain channels or
    /// advance animation, including surfaces without focus.
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
    /// Focus an already-open surface with the matching stable key.
    FocusKey(Cow<'static, str>),
    /// Replace shell presentation settings and notify every surface.
    Reconfigure(Box<ShellConfig>),
}

impl SurfaceAction {
    /// Opens and focuses a new surface.
    pub fn open(surface: impl Surface + 'static) -> Self {
        Self::Open(Box::new(surface))
    }

    /// Focuses an already-open surface by its stable key.
    pub fn focus_key(key: impl Into<Cow<'static, str>>) -> Self {
        Self::FocusKey(key.into())
    }
}

/// One independently navigable item hosted by a [`crate::Shell`].
///
/// Implementations may represent agent sessions, PTYs, forms, approval prompts,
/// log streams, or any other terminal-native interaction. The trait is object-safe
/// so a shell can host heterogeneous surfaces together.
pub trait Surface: Send {
    /// The short title shown in tabs and the action bar.
    fn title(&self) -> Cow<'_, str>;

    /// Stable shell-local lookup key used by another surface to focus this one.
    ///
    /// Most surfaces do not need a key. Products that expose a master surface
    /// alongside dynamically named detail surfaces can use an immutable domain
    /// identity here so renames do not break navigation.
    fn key(&self) -> Option<Cow<'_, str>> {
        None
    }

    /// Current state shown beside the title.
    fn status(&self) -> SurfaceStatus {
        SurfaceStatus::Ready
    }

    /// A short annotation shown after the title — an unread count, a role, an
    /// elapsed time.
    ///
    /// Chrome renders this in its own column rather than as part of
    /// [`Surface::title`], so titles stay truncatable and badges stay aligned.
    fn badge(&self) -> Option<Cow<'_, str>> {
        None
    }

    /// A richer annotation used when a vertical rail has extra width.
    ///
    /// The default preserves [`Surface::badge`], so existing implementations
    /// require no changes.
    fn wide_badge(&self) -> Option<Cow<'_, str>> {
        self.badge()
    }

    /// Determines whether ordinary shell shortcuts may intercept input.
    fn input_policy(&self) -> InputPolicy {
        InputPolicy::Shell
    }

    /// Applies settings reloaded by the host while the shell remains attached.
    fn reconfigure(&mut self, _config: &ShellConfig) {}

    /// Whether an unmodified Escape opens the action bar in the current state.
    ///
    /// Input surfaces can use this for an empty-prompt escape hatch while
    /// preserving Escape as ordinary input whenever they have text or an
    /// interactive terminal state that needs it.
    fn opens_action_bar_on_escape(&self) -> bool {
        false
    }

    /// Requests a shorter interval between background ticks while this surface
    /// has latency-sensitive work in progress.
    ///
    /// The shell uses the shortest requested interval, capped by its configured
    /// idle tick rate. Returning `None` keeps the configured rate.
    fn poll_interval(&self) -> Option<Duration> {
        None
    }

    /// Draws this surface inside the content area owned by TurtleTap.
    fn render(&mut self, frame: &mut Frame<'_>, area: Rect);

    /// Handles an input or lifecycle event.
    fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction;

    /// Polls application-owned background readiness.
    ///
    /// Implementations must register `context.waker()` before returning
    /// [`Poll::Pending`]. A ready result must represent bounded forward
    /// progress; continuously returning `Ready(Ignored)` would spin an async
    /// attach loop.
    ///
    /// The asynchronous shell polls this when the registered source wakes. The
    /// synchronous shell polls it on ordinary ticks as a compatibility
    /// fallback.
    fn poll_background(&mut self, context: &mut Context<'_>) -> Poll<SurfaceAction> {
        let _ = context;
        Poll::Pending
    }

    /// Returns surface-specific shortcuts for contextual help.
    fn shortcuts(&self) -> Vec<Shortcut> {
        Vec::new()
    }

    /// Returns commands the action bar can execute for the current surface state.
    fn commands(&self) -> Vec<SurfaceCommand> {
        Vec::new()
    }

    /// Executes a command previously returned by [`Surface::commands`].
    fn execute_command(&mut self, _id: &str) -> SurfaceAction {
        SurfaceAction::Ignored
    }

    /// Called after this surface becomes active.
    fn focus(&mut self) {}

    /// Called before this surface stops being active.
    fn blur(&mut self) {}
}
