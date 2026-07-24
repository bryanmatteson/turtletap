#![doc = include_str!("../README.md")]

mod render;
#[cfg(feature = "resident")]
pub mod resident;
mod shell;
mod surface;
mod terminal;
mod theme;

pub use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
pub use ratatui::{Frame, layout::Rect};
pub use shell::{
    Chrome, ExitReason, KeyBinding, Shell, ShellBindings, ShellConfig, ShellSignal, SurfaceId,
};
pub use surface::{InputPolicy, Shortcut, Surface, SurfaceAction, SurfaceEvent, SurfaceStatus};
pub use theme::Theme;

/// Ratatui types commonly needed by surface implementations.
pub mod tui {
    pub use ratatui::{buffer, layout, style, text, widgets};
}
