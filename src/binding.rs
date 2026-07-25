//! Keybinding identifiers, portable encoding, and validation.

use std::{error::Error, fmt, str::FromStr};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// One key chord reserved by the shell or a host action.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyBinding {
    /// The non-modifier key.
    pub code: KeyCode,
    /// Required modifier keys.
    pub modifiers: KeyModifiers,
}

impl KeyBinding {
    /// Creates a key binding.
    #[must_use]
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Returns whether this binding matches a terminal key event exactly.
    #[must_use]
    pub fn matches(self, key: KeyEvent) -> bool {
        self.code == key.code && self.modifiers == key.modifiers
    }

    /// Returns the compact, platform-native label used by help and shell chrome.
    #[must_use]
    pub fn label(self) -> String {
        let mut parts = Vec::new();
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("Ctrl".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            parts.push("Alt".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("Shift".to_owned());
        }
        if self.modifiers.contains(KeyModifiers::SUPER) {
            parts.push(
                if cfg!(target_os = "macos") {
                    "Cmd"
                } else {
                    "Super"
                }
                .to_owned(),
            );
        }
        parts.push(match self.code {
            KeyCode::Backspace => "Backspace".to_owned(),
            KeyCode::Enter => "Enter".to_owned(),
            KeyCode::Left => "Left".to_owned(),
            KeyCode::Right => "Right".to_owned(),
            KeyCode::Up => "Up".to_owned(),
            KeyCode::Down => "Down".to_owned(),
            KeyCode::Home => "Home".to_owned(),
            KeyCode::End => "End".to_owned(),
            KeyCode::PageUp => "PageUp".to_owned(),
            KeyCode::PageDown => "PageDown".to_owned(),
            KeyCode::Tab => "Tab".to_owned(),
            KeyCode::BackTab => "BackTab".to_owned(),
            KeyCode::Delete => "Delete".to_owned(),
            KeyCode::Insert => "Insert".to_owned(),
            KeyCode::F(number) => format!("F{number}"),
            KeyCode::Char(' ') => "Space".to_owned(),
            KeyCode::Char(character) => character.to_string().to_uppercase(),
            KeyCode::Esc => "Esc".to_owned(),
            _ => "Key".to_owned(),
        });
        parts.join("-")
    }
}

/// A semantic interaction context in which bindings may collide.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum BindingContext {
    /// Shortcuts considered before surface-specific input.
    Global,
    /// Shortcuts active on shell-managed surfaces.
    Shell,
    /// Keys pressed after the shell leader.
    Leader,
    /// Accelerators active while the action bar is open.
    Action,
    /// Shortcuts active in an interactive command session.
    Session,
    /// Shortcuts active on the session dashboard.
    Dashboard,
}

impl BindingContext {
    /// Returns the stable configuration group, or `None` for global bindings.
    #[must_use]
    pub const fn config_scope(self) -> Option<&'static str> {
        match self {
            Self::Global => None,
            Self::Shell => Some("shell"),
            Self::Leader => Some("leader"),
            Self::Action => Some("action"),
            Self::Session => Some("session"),
            Self::Dashboard => Some("dashboard"),
        }
    }

    /// Returns the user-facing context name.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Global => "Global",
            Self::Shell => "Shell",
            Self::Leader => "Leader",
            Self::Action => "Action bar",
            Self::Session => "Session",
            Self::Dashboard => "Dashboard",
        }
    }

    /// Returns whether unmodified text and arrow keys must remain available.
    #[must_use]
    pub const fn accepts_text(self) -> bool {
        matches!(self, Self::Global | Self::Action | Self::Session)
    }
}

/// The value type stored by a binding identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindingKind {
    /// One or more complete key chords.
    Keys,
    /// Modifiers combined with digits 1 through 9.
    Modifiers,
}

macro_rules! binding_catalog {
    (
        keys {
            $(($key_variant:ident, $key_field:ident, $flat_id:literal, $config_name:literal, $label:literal, $context:ident)),+ $(,)?
        }
        modifiers {
            $(($modifier_variant:ident, $modifier_field:ident, $modifier_flat_id:literal, $modifier_config_name:literal, $modifier_label:literal, $modifier_context:ident)),+ $(,)?
        }
    ) => {
        /// Configurable action bindings for shell and host interaction contexts.
        #[derive(Clone, Debug, Eq, PartialEq)]
        pub struct ShellBindings {
            $(
                #[doc = concat!("Complete key chords for `", $flat_id, "` (", $label, ").")]
                pub $key_field: Vec<KeyBinding>,
            )+
            $(
                #[doc = concat!("Digit-jump modifiers for `", $modifier_flat_id, "` (", $modifier_label, ").")]
                pub $modifier_field: Vec<KeyModifiers>,
            )+
        }

        /// Stable identifier for one configurable TurtleTap action.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum BindingId {
            $(
                #[doc = concat!("The `", $flat_id, "` binding.")]
                $key_variant,
            )+
            $(
                #[doc = concat!("The `", $modifier_flat_id, "` modifier binding.")]
                $modifier_variant,
            )+
        }

        impl BindingId {
            /// Every binding in stable display order.
            pub const ALL: &'static [Self] = &[
                $(Self::$key_variant,)+
                $(Self::$modifier_variant,)+
            ];

            /// Every complete-key binding in stable display order.
            pub const KEY_BINDINGS: &'static [Self] = &[
                $(Self::$key_variant,)+
            ];

            /// Returns whether this identifier stores complete keys or modifiers.
            #[must_use]
            pub const fn kind(self) -> BindingKind {
                match self {
                    $(Self::$key_variant => BindingKind::Keys,)+
                    $(Self::$modifier_variant => BindingKind::Modifiers,)+
                }
            }

            /// Returns the interaction context for this binding.
            #[must_use]
            pub const fn context(self) -> BindingContext {
                match self {
                    $(Self::$key_variant => BindingContext::$context,)+
                    $(Self::$modifier_variant => BindingContext::$modifier_context,)+
                }
            }

            /// Returns the stable legacy flat identifier.
            #[must_use]
            pub const fn flat_id(self) -> &'static str {
                match self {
                    $(Self::$key_variant => $flat_id,)+
                    $(Self::$modifier_variant => $modifier_flat_id,)+
                }
            }

            /// Returns the name used inside this binding's configuration group.
            #[must_use]
            pub const fn config_name(self) -> &'static str {
                match self {
                    $(Self::$key_variant => $config_name,)+
                    $(Self::$modifier_variant => $modifier_config_name,)+
                }
            }

            /// Returns the stable dotted configuration path.
            #[must_use]
            pub fn config_path(self) -> String {
                self.context().config_scope().map_or_else(
                    || format!("bindings.{}", self.config_name()),
                    |scope| format!("bindings.{scope}.{}", self.config_name()),
                )
            }

            /// Returns the user-facing action label.
            #[must_use]
            pub const fn label(self) -> &'static str {
                match self {
                    $(Self::$key_variant => $label,)+
                    $(Self::$modifier_variant => $modifier_label,)+
                }
            }
        }

        impl ShellBindings {
            /// Returns the complete key chords for `id`, or `None` for a modifier binding.
            #[must_use]
            pub fn keys(&self, id: BindingId) -> Option<&[KeyBinding]> {
                match id {
                    $(BindingId::$key_variant => Some(&self.$key_field),)+
                    $(BindingId::$modifier_variant => None,)+
                }
            }

            /// Replaces the complete key chords for `id`.
            ///
            /// Returns an error when `id` denotes a modifier binding.
            pub fn set_keys(
                &mut self,
                id: BindingId,
                keys: Vec<KeyBinding>,
            ) -> Result<(), BindingTypeError> {
                match id {
                    $(BindingId::$key_variant => {
                        self.$key_field = keys;
                        Ok(())
                    },)+
                    $(BindingId::$modifier_variant => Err(BindingTypeError {
                        id,
                        expected: BindingKind::Keys,
                    }),)+
                }
            }

            /// Returns the digit-jump modifiers for `id`, or `None` for a key binding.
            #[must_use]
            pub fn modifiers(&self, id: BindingId) -> Option<&[KeyModifiers]> {
                match id {
                    $(BindingId::$key_variant => None,)+
                    $(BindingId::$modifier_variant => Some(&self.$modifier_field),)+
                }
            }

            /// Replaces the digit-jump modifiers for `id`.
            ///
            /// Returns an error when `id` denotes a complete-key binding.
            pub fn set_modifiers(
                &mut self,
                id: BindingId,
                modifiers: Vec<KeyModifiers>,
            ) -> Result<(), BindingTypeError> {
                match id {
                    $(BindingId::$key_variant => Err(BindingTypeError {
                        id,
                        expected: BindingKind::Modifiers,
                    }),)+
                    $(BindingId::$modifier_variant => {
                        self.$modifier_field = modifiers;
                        Ok(())
                    },)+
                }
            }
        }
    };
}

binding_catalog!(
    keys {
        (Leaders, leaders, "leaders", "leaders", "Leader", Global),
        (Palette, palette, "palette", "palette", "Action bar", Global),
        (Redraw, redraw, "redraw", "redraw", "Redraw", Global),
        (NextScreen, next_screen, "next-screen", "next-screen", "Next screen", Global),
        (PreviousScreen, previous_screen, "previous-screen", "previous-screen", "Previous screen", Global),
        (ShellDetach, shell_detach, "shell-detach", "detach", "Detach", Shell),
        (ShellNextScreen, shell_next_screen, "shell-next-screen", "next-screen", "Next screen", Shell),
        (ShellPreviousScreen, shell_previous_screen, "shell-previous-screen", "previous-screen", "Previous screen", Shell),
        (ShellHelp, shell_help, "shell-help", "help", "Help", Shell),
        (LeaderPalette, leader_palette, "leader-palette", "palette", "Action bar", Leader),
        (LeaderNextScreen, leader_next_screen, "leader-next-screen", "next-screen", "Next screen", Leader),
        (LeaderPreviousScreen, leader_previous_screen, "leader-previous-screen", "previous-screen", "Previous screen", Leader),
        (LeaderScrollUp, leader_scroll_up, "leader-scroll-up", "scroll-up", "Scroll up", Leader),
        (LeaderScrollDown, leader_scroll_down, "leader-scroll-down", "scroll-down", "Scroll down", Leader),
        (LeaderClose, leader_close, "leader-close", "close", "Close", Leader),
        (LeaderDetach, leader_detach, "leader-detach", "detach", "Detach", Leader),
        (LeaderHelp, leader_help, "leader-help", "help", "Help", Leader),
        (ActionNextScreen, action_next_screen, "action-next-screen", "next-screen", "Next screen", Action),
        (ActionPreviousScreen, action_previous_screen, "action-previous-screen", "previous-screen", "Previous screen", Action),
        (ActionScrollUp, action_scroll_up, "action-scroll-up", "scroll-up", "Scroll up", Action),
        (ActionScrollDown, action_scroll_down, "action-scroll-down", "scroll-down", "Scroll down", Action),
        (ActionClose, action_close, "action-close", "close", "Close", Action),
        (ActionDetach, action_detach, "action-detach", "detach", "Detach", Action),
        (ActionHelp, action_help, "action-help", "help", "Help", Action),
        (ActionClearQuery, action_clear_query, "action-clear-query", "clear-query", "Clear query", Action),
        (SessionReleaseDriver, session_release_driver, "session-release-driver", "release-driver", "Release driver", Session),
        (SessionTakeDriver, session_take_driver, "session-take-driver", "take-driver", "Take driver", Session),
        (SessionClear, session_clear, "session-clear", "clear", "Clear", Session),
        (SessionInterrupt, session_interrupt, "session-interrupt", "interrupt", "Interrupt", Session),
        (SessionDetach, session_detach, "session-detach", "detach", "Detach", Session),
        (SessionDeleteToStart, session_delete_to_start, "session-delete-to-start", "delete-to-start", "Delete to start", Session),
        (SessionWordLeft, session_word_left, "session-word-left", "word-left", "Word left", Session),
        (SessionWordRight, session_word_right, "session-word-right", "word-right", "Word right", Session),
        (SessionLineStart, session_line_start, "session-line-start", "line-start", "Line start", Session),
        (SessionLineEnd, session_line_end, "session-line-end", "line-end", "Line end", Session),
        (SessionDeleteWord, session_delete_word, "session-delete-word", "delete-word", "Delete word", Session),
        (SessionComplete, session_complete, "session-complete", "complete", "Complete", Session),
        (SessionScrollUp, session_scroll_up, "session-scroll-up", "scroll-up", "Scroll up", Session),
        (SessionScrollDown, session_scroll_down, "session-scroll-down", "scroll-down", "Scroll down", Session),
        (SessionScrollTop, session_scroll_top, "session-scroll-top", "scroll-top", "Scroll to top", Session),
        (SessionScrollBottom, session_scroll_bottom, "session-scroll-bottom", "scroll-bottom", "Scroll to bottom", Session),
        (DashboardUp, dashboard_up, "dashboard-up", "up", "Move up", Dashboard),
        (DashboardDown, dashboard_down, "dashboard-down", "down", "Move down", Dashboard),
        (DashboardView, dashboard_view, "dashboard-view", "view", "View session", Dashboard),
        (DashboardTake, dashboard_take, "dashboard-take", "take", "Take session", Dashboard),
        (DashboardSearch, dashboard_search, "dashboard-search", "search", "Search", Dashboard),
        (DashboardNew, dashboard_new, "dashboard-new", "new", "New session", Dashboard),
        (DashboardRename, dashboard_rename, "dashboard-rename", "rename", "Rename session", Dashboard),
        (DashboardDelete, dashboard_delete, "dashboard-delete", "delete", "Delete session", Dashboard),
        (DashboardStop, dashboard_stop, "dashboard-stop", "stop", "Stop resident", Dashboard),
        (DashboardKeybindings, dashboard_keybindings, "dashboard-keybindings", "keybindings", "Edit keybindings", Dashboard),
        (DashboardClose, dashboard_close, "dashboard-close", "close", "Close", Dashboard)
    }
    modifiers {
        (JumpModifiers, jump_modifiers, "jump-modifiers", "jump-modifiers", "Jump to screen", Global),
        (LeaderJumpModifiers, leader_jump_modifiers, "leader-jump-modifiers", "jump-modifiers", "Jump to screen", Leader),
        (ActionJumpModifiers, action_jump_modifiers, "action-jump-modifiers", "jump-modifiers", "Jump to screen", Action)
    }
);

/// An identifier was used with the wrong binding value type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BindingTypeError {
    id: BindingId,
    expected: BindingKind,
}

impl fmt::Display for BindingTypeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} stores {:?}, not {:?}",
            self.id.config_path(),
            self.id.kind(),
            self.expected
        )
    }
}

impl Error for BindingTypeError {}

/// Why a key chord or modifier string cannot be represented.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyBindingError {
    /// The input did not contain a key.
    MissingKey,
    /// A key name is not supported by the portable configuration format.
    UnsupportedKey(String),
    /// A modifier name is not supported by the portable configuration format.
    UnsupportedModifier(String),
    /// A modifier occurred more than once.
    RepeatedModifier(String),
    /// An empty modifier occurred in a chord.
    EmptyModifier,
    /// A modifier-only value did not contain any modifiers.
    MissingModifier,
    /// A terminal event included modifier flags the format cannot preserve.
    UnsupportedModifierFlags(KeyModifiers),
}

impl fmt::Display for KeyBindingError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingKey => formatter.write_str("key chord has no key"),
            Self::UnsupportedKey(key) => write!(formatter, "'{key}' is not a supported key name"),
            Self::UnsupportedModifier(modifier) => {
                write!(formatter, "'{modifier}' is not a supported modifier")
            }
            Self::RepeatedModifier(modifier) => {
                write!(formatter, "modifier '{modifier}' appears more than once")
            }
            Self::EmptyModifier => formatter.write_str("key chord contains an empty modifier"),
            Self::MissingModifier => {
                formatter.write_str("modifier binding must contain at least one modifier")
            }
            Self::UnsupportedModifierFlags(modifiers) => {
                write!(
                    formatter,
                    "terminal modifiers {modifiers:?} cannot be stored"
                )
            }
        }
    }
}

impl Error for KeyBindingError {}

impl FromStr for KeyBinding {
    type Err = KeyBindingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase();
        let mut parts = normalized.split('-').collect::<Vec<_>>();
        let key = parts
            .pop()
            .filter(|key| !key.is_empty())
            .ok_or(KeyBindingError::MissingKey)?;
        let modifiers = parse_modifier_parts(&parts)?;
        let code = parse_key_code(key)?;
        KeyBinding::new(code, modifiers).canonical()
    }
}

impl KeyBinding {
    /// Converts a terminal event into a portable, normalized binding.
    pub fn from_event(event: KeyEvent) -> Result<Self, KeyBindingError> {
        Self::new(event.code, event.modifiers).canonical()
    }

    /// Validates and normalizes this binding for portable configuration.
    pub fn canonical(self) -> Result<Self, KeyBindingError> {
        let supported =
            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::SUPER;
        let unsupported = self.modifiers.difference(supported);
        if !unsupported.is_empty() {
            return Err(KeyBindingError::UnsupportedModifierFlags(unsupported));
        }
        let code = match self.code {
            KeyCode::Char(character) => KeyCode::Char(character.to_ascii_lowercase()),
            KeyCode::Backspace
            | KeyCode::Enter
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Tab
            | KeyCode::BackTab
            | KeyCode::Delete
            | KeyCode::Insert
            | KeyCode::Esc
            | KeyCode::F(1..=24) => self.code,
            unsupported => {
                return Err(KeyBindingError::UnsupportedKey(format!("{unsupported:?}")));
            }
        };
        Ok(Self::new(code, self.modifiers))
    }

    /// Returns the stable, platform-independent configuration representation.
    pub fn config_label(self) -> Result<String, KeyBindingError> {
        let binding = self.canonical()?;
        let mut parts = Vec::new();
        if binding.modifiers.contains(KeyModifiers::CONTROL) {
            parts.push("ctrl".to_owned());
        }
        if binding.modifiers.contains(KeyModifiers::ALT) {
            parts.push("alt".to_owned());
        }
        if binding.modifiers.contains(KeyModifiers::SHIFT) {
            parts.push("shift".to_owned());
        }
        if binding.modifiers.contains(KeyModifiers::SUPER) {
            parts.push("super".to_owned());
        }
        parts.push(key_code_config_label(binding.code)?);
        Ok(parts.join("-"))
    }
}

/// Parses one modifier-only configuration value.
pub fn parse_key_modifiers(value: &str) -> Result<KeyModifiers, KeyBindingError> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized == "none" {
        return Ok(KeyModifiers::empty());
    }
    let parts = normalized.split(['-', '+']).collect::<Vec<_>>();
    let modifiers = parse_modifier_parts(&parts)?;
    if modifiers.is_empty() {
        return Err(KeyBindingError::MissingModifier);
    }
    Ok(modifiers)
}

/// Returns the stable, platform-independent representation of modifier flags.
pub fn key_modifiers_config_label(modifiers: KeyModifiers) -> Result<String, KeyBindingError> {
    if modifiers.is_empty() {
        return Ok("none".to_owned());
    }
    KeyBinding::new(KeyCode::Char('1'), modifiers)
        .config_label()
        .map(|label| label.trim_end_matches("-1").to_owned())
}

fn parse_modifier_parts(parts: &[&str]) -> Result<KeyModifiers, KeyBindingError> {
    let mut modifiers = KeyModifiers::empty();
    for part in parts {
        let modifier = match *part {
            "ctrl" | "control" => KeyModifiers::CONTROL,
            "alt" | "option" | "opt" => KeyModifiers::ALT,
            "shift" => KeyModifiers::SHIFT,
            "super" | "cmd" | "command" => KeyModifiers::SUPER,
            "" => return Err(KeyBindingError::EmptyModifier),
            unknown => return Err(KeyBindingError::UnsupportedModifier(unknown.to_owned())),
        };
        if modifiers.contains(modifier) {
            return Err(KeyBindingError::RepeatedModifier((*part).to_owned()));
        }
        modifiers.insert(modifier);
    }
    Ok(modifiers)
}

fn parse_key_code(key: &str) -> Result<KeyCode, KeyBindingError> {
    let code = match key {
        "backspace" => KeyCode::Backspace,
        "enter" | "return" => KeyCode::Enter,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdown" => KeyCode::PageDown,
        "tab" => KeyCode::Tab,
        "backtab" => KeyCode::BackTab,
        "delete" | "del" => KeyCode::Delete,
        "insert" | "ins" => KeyCode::Insert,
        "escape" | "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        key if key.chars().count() == 1 => {
            KeyCode::Char(key.chars().next().ok_or(KeyBindingError::MissingKey)?)
        }
        key if key.starts_with('f') => {
            let number = key[1..]
                .parse::<u8>()
                .ok()
                .filter(|number| (1..=24).contains(number))
                .ok_or_else(|| KeyBindingError::UnsupportedKey(key.to_owned()))?;
            KeyCode::F(number)
        }
        _ => return Err(KeyBindingError::UnsupportedKey(key.to_owned())),
    };
    Ok(code)
}

fn key_code_config_label(code: KeyCode) -> Result<String, KeyBindingError> {
    let label = match code {
        KeyCode::Backspace => "backspace".to_owned(),
        KeyCode::Enter => "enter".to_owned(),
        KeyCode::Left => "left".to_owned(),
        KeyCode::Right => "right".to_owned(),
        KeyCode::Up => "up".to_owned(),
        KeyCode::Down => "down".to_owned(),
        KeyCode::Home => "home".to_owned(),
        KeyCode::End => "end".to_owned(),
        KeyCode::PageUp => "pageup".to_owned(),
        KeyCode::PageDown => "pagedown".to_owned(),
        KeyCode::Tab => "tab".to_owned(),
        KeyCode::BackTab => "backtab".to_owned(),
        KeyCode::Delete => "delete".to_owned(),
        KeyCode::Insert => "insert".to_owned(),
        KeyCode::Esc => "esc".to_owned(),
        KeyCode::F(number @ 1..=24) => format!("f{number}"),
        KeyCode::Char(' ') => "space".to_owned(),
        KeyCode::Char(character) => character.to_ascii_lowercase().to_string(),
        unsupported => {
            return Err(KeyBindingError::UnsupportedKey(format!("{unsupported:?}")));
        }
    };
    Ok(label)
}

/// Why a complete binding set is unsafe or ambiguous.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BindingValidationError {
    /// A binding contains a key that cannot be stored portably.
    InvalidKey {
        /// The affected action.
        id: BindingId,
        /// The underlying key error.
        source: KeyBindingError,
    },
    /// A modifier binding contains flags that cannot be stored portably.
    InvalidModifiers {
        /// The affected action.
        id: BindingId,
        /// The underlying modifier error.
        source: KeyBindingError,
    },
    /// One action contains the same key more than once.
    DuplicateKey {
        /// The affected action.
        id: BindingId,
        /// The repeated key.
        binding: KeyBinding,
    },
    /// Two actions in the same context use the same key.
    ConflictingKey {
        /// The first action.
        first: BindingId,
        /// The second action.
        second: BindingId,
        /// The conflicting key.
        binding: KeyBinding,
    },
    /// One modifier binding contains the same modifier set more than once.
    DuplicateModifiers {
        /// The affected action.
        id: BindingId,
        /// The repeated modifiers.
        modifiers: KeyModifiers,
    },
    /// A digit-jump chord collides with another action.
    ModifierConflict {
        /// The complete-key action.
        key_id: BindingId,
        /// The modifier action.
        modifier_id: BindingId,
        /// The conflicting generated key.
        binding: KeyBinding,
    },
    /// An unmodified text or arrow key would intercept input.
    UnsafeTextKey {
        /// The affected action.
        id: BindingId,
        /// The unsafe key.
        binding: KeyBinding,
    },
    /// An unmodified digit-jump group would intercept text.
    UnsafeTextModifiers {
        /// The affected action.
        id: BindingId,
    },
}

impl fmt::Display for BindingValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidKey { id, source } => {
                write!(formatter, "{} is invalid: {source}", id.config_path())
            }
            Self::InvalidModifiers { id, source } => {
                write!(formatter, "{} is invalid: {source}", id.config_path())
            }
            Self::DuplicateKey { id, binding } => write!(
                formatter,
                "{} contains {} more than once",
                id.config_path(),
                binding.label()
            ),
            Self::ConflictingKey {
                first,
                second,
                binding,
            } => write!(
                formatter,
                "{} is assigned to both {} and {}",
                binding.label(),
                first.config_path(),
                second.config_path()
            ),
            Self::DuplicateModifiers { id, modifiers } => write!(
                formatter,
                "{} contains {} more than once",
                id.config_path(),
                key_modifiers_config_label(*modifiers).unwrap_or_else(|_| format!("{modifiers:?}"))
            ),
            Self::ModifierConflict {
                key_id,
                modifier_id,
                binding,
            } => write!(
                formatter,
                "{} is assigned to both {} and {}",
                binding.label(),
                key_id.config_path(),
                modifier_id.config_path()
            ),
            Self::UnsafeTextKey { id, binding } => write!(
                formatter,
                "{} cannot use unmodified {} because this context accepts text; add Ctrl, Alt, Shift, or Cmd",
                id.config_path(),
                binding.label()
            ),
            Self::UnsafeTextModifiers { id } => write!(
                formatter,
                "{} cannot use 'none' because this context accepts text; add Ctrl, Alt, Shift, or Cmd",
                id.config_path()
            ),
        }
    }
}

impl Error for BindingValidationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidKey { source, .. } | Self::InvalidModifiers { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl ShellBindings {
    /// Validates portability, context-local conflicts, and text-input safety.
    pub fn validate(&self) -> Result<(), BindingValidationError> {
        for id in BindingId::KEY_BINDINGS {
            let keys = self.keys(*id).unwrap_or_default();
            for binding in keys {
                binding
                    .canonical()
                    .map_err(|source| BindingValidationError::InvalidKey { id: *id, source })?;
                if id.context().accepts_text()
                    && binding.modifiers.is_empty()
                    && matches!(
                        binding.code,
                        KeyCode::Char(_)
                            | KeyCode::Left
                            | KeyCode::Right
                            | KeyCode::Up
                            | KeyCode::Down
                    )
                {
                    return Err(BindingValidationError::UnsafeTextKey {
                        id: *id,
                        binding: *binding,
                    });
                }
            }
            if let Some(binding) = duplicate(keys) {
                return Err(BindingValidationError::DuplicateKey { id: *id, binding });
            }
        }

        for context in [
            BindingContext::Global,
            BindingContext::Shell,
            BindingContext::Leader,
            BindingContext::Action,
            BindingContext::Session,
            BindingContext::Dashboard,
        ] {
            let ids = BindingId::KEY_BINDINGS
                .iter()
                .copied()
                .filter(|id| id.context() == context)
                .collect::<Vec<_>>();
            for (index, first) in ids.iter().enumerate() {
                let first_keys = self.keys(*first).unwrap_or_default();
                for second in &ids[index + 1..] {
                    let second_keys = self.keys(*second).unwrap_or_default();
                    if let Some(binding) = first_keys
                        .iter()
                        .find(|binding| second_keys.contains(binding))
                    {
                        return Err(BindingValidationError::ConflictingKey {
                            first: *first,
                            second: *second,
                            binding: *binding,
                        });
                    }
                }
            }
        }

        for modifier_id in [
            BindingId::JumpModifiers,
            BindingId::LeaderJumpModifiers,
            BindingId::ActionJumpModifiers,
        ] {
            let modifiers = self.modifiers(modifier_id).unwrap_or_default();
            for modifiers in modifiers {
                key_modifiers_config_label(*modifiers).map_err(|source| {
                    BindingValidationError::InvalidModifiers {
                        id: modifier_id,
                        source,
                    }
                })?;
            }
            if modifier_id.context().accepts_text() && modifiers.contains(&KeyModifiers::empty()) {
                return Err(BindingValidationError::UnsafeTextModifiers { id: modifier_id });
            }
            if let Some(modifiers) = duplicate(modifiers) {
                return Err(BindingValidationError::DuplicateModifiers {
                    id: modifier_id,
                    modifiers,
                });
            }
            for key_id in BindingId::KEY_BINDINGS
                .iter()
                .copied()
                .filter(|id| id.context() == modifier_id.context())
            {
                let keys = self.keys(key_id).unwrap_or_default();
                for modifiers in modifiers {
                    for digit in '1'..='9' {
                        let binding = KeyBinding::new(KeyCode::Char(digit), *modifiers);
                        if keys.contains(&binding) {
                            return Err(BindingValidationError::ModifierConflict {
                                key_id,
                                modifier_id,
                                binding,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

fn duplicate<T: Copy + PartialEq>(values: &[T]) -> Option<T> {
    values
        .iter()
        .enumerate()
        .find_map(|(index, value)| values[index + 1..].contains(value).then_some(*value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn portable_labels_roundtrip_without_platform_aliases() {
        let binding = KeyBinding::new(KeyCode::Backspace, KeyModifiers::SUPER);
        assert_eq!(
            binding.config_label().expect("binding should encode"),
            "super-backspace"
        );
        assert_eq!(
            "cmd-backspace"
                .parse::<KeyBinding>()
                .expect("alias should parse"),
            binding
        );
    }

    #[test]
    fn terminal_friendly_aliases_parse_to_portable_bindings() {
        assert_eq!(
            "Option-Right"
                .parse::<KeyBinding>()
                .expect("Option alias should parse"),
            KeyBinding::new(KeyCode::Right, KeyModifiers::ALT)
        );
        assert_eq!(
            "Ctrl-PgDown"
                .parse::<KeyBinding>()
                .expect("page alias should parse"),
            KeyBinding::new(KeyCode::PageDown, KeyModifiers::CONTROL)
        );
        assert_eq!(
            "ctrl-space"
                .parse::<KeyBinding>()
                .expect("named space should parse"),
            KeyBinding::new(KeyCode::Char(' '), KeyModifiers::CONTROL)
        );
        assert_eq!(
            parse_key_modifiers("none").expect("none should parse"),
            KeyModifiers::empty()
        );
    }

    #[test]
    fn terminal_only_modifier_flags_are_not_portable() {
        let binding = KeyBinding::new(KeyCode::Char('h'), KeyModifiers::HYPER);

        assert!(matches!(
            binding.canonical(),
            Err(KeyBindingError::UnsupportedModifierFlags(modifiers))
                if modifiers == KeyModifiers::HYPER
        ));
    }

    #[test]
    fn catalog_reads_and_replaces_library_bindings() {
        let mut bindings = ShellBindings::default();
        let replacement = KeyBinding::new(KeyCode::Char('x'), KeyModifiers::CONTROL);
        bindings
            .set_keys(BindingId::SessionInterrupt, vec![replacement])
            .expect("key action should accept keys");

        assert_eq!(
            bindings.keys(BindingId::SessionInterrupt),
            Some([replacement].as_slice())
        );
        assert_eq!(
            BindingId::SessionInterrupt.config_path(),
            "bindings.session.interrupt"
        );

        bindings
            .set_modifiers(BindingId::ActionJumpModifiers, vec![KeyModifiers::CONTROL])
            .expect("modifier action should accept modifier sets");
        assert_eq!(
            bindings.modifiers(BindingId::ActionJumpModifiers),
            Some([KeyModifiers::CONTROL].as_slice())
        );
    }

    #[test]
    fn catalog_identifiers_and_paths_are_complete_and_unique() {
        assert_eq!(BindingId::KEY_BINDINGS.len(), 52);
        assert_eq!(BindingId::ALL.len(), 55);
        assert_eq!(
            BindingId::ALL
                .iter()
                .map(|id| id.flat_id())
                .collect::<HashSet<_>>()
                .len(),
            BindingId::ALL.len()
        );
        assert_eq!(
            BindingId::ALL
                .iter()
                .map(|id| id.config_path())
                .collect::<HashSet<_>>()
                .len(),
            BindingId::ALL.len()
        );
    }

    #[test]
    fn validation_rejects_plain_keys_in_text_contexts() {
        let mut bindings = ShellBindings::default();
        bindings
            .set_keys(
                BindingId::SessionInterrupt,
                vec![KeyBinding::new(KeyCode::Char('x'), KeyModifiers::empty())],
            )
            .expect("session interrupt stores complete keys");

        assert!(matches!(
            bindings.validate(),
            Err(BindingValidationError::UnsafeTextKey {
                id: BindingId::SessionInterrupt,
                ..
            })
        ));
    }

    #[test]
    fn validation_allows_plain_keys_in_dashboard_context() {
        let mut bindings = ShellBindings::default();
        bindings
            .set_keys(
                BindingId::DashboardView,
                vec![KeyBinding::new(KeyCode::Char('z'), KeyModifiers::empty())],
            )
            .expect("dashboard view stores complete keys");
        bindings.validate().expect("dashboard keys may be plain");
    }

    #[test]
    fn validation_rejects_terminal_only_modifier_groups() {
        let mut bindings = ShellBindings::default();
        bindings
            .set_modifiers(BindingId::ActionJumpModifiers, vec![KeyModifiers::HYPER])
            .expect("action jump stores modifier sets");

        assert!(matches!(
            bindings.validate(),
            Err(BindingValidationError::InvalidModifiers {
                id: BindingId::ActionJumpModifiers,
                ..
            })
        ));
    }
}
