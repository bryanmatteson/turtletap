#![doc = include_str!("../README.md")]

mod binding;
mod render;
#[cfg(feature = "resident")]
pub mod resident;
mod runtime;
mod shell;
mod surface;
mod terminal;
#[cfg(feature = "termosaic")]
pub mod termosaic;
#[cfg(feature = "termosaic")]
pub use ::termosaic::laidout as layout;
mod theme;

pub use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
pub use ratatui::{Frame, layout::Rect};
pub use runtime::{
    RuntimeAction, RuntimeEvent, TerminalApplication, TerminalConfig, TerminalRuntime,
};
pub use shell::{Chrome, ExitReason, Shell, ShellConfig, ShellSignal, SurfaceId};
pub use surface::{
    InputPolicy, Shortcut, Surface, SurfaceAction, SurfaceCommand, SurfaceEvent, SurfaceStatus,
};
pub use theme::Theme;

/// Ratatui types commonly needed by surface implementations.
pub mod tui {
    pub use ratatui::{buffer, layout, style, text, widgets};
}
pub use binding::{
    BindingContext, BindingId, BindingKind, BindingTypeError, BindingValidationError, KeyBinding,
    KeyBindingError, ShellBindings, key_modifiers_config_label, parse_key_modifiers,
};
