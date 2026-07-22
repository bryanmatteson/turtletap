use std::{fmt, time::Duration};

use serde::{Serialize, de::DeserializeOwned};

/// Application-domain failure returned to a resident client.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationError {
    code: String,
    message: String,
}

impl ApplicationError {
    /// Creates an application failure with a stable code and user-facing message.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Machine-readable error code.
    #[must_use]
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Human-readable detail.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for ApplicationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ApplicationError {}

/// Durable application events and follow-on effects produced by one transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTransition<E, F> {
    /// Events to journal, apply to subscribers, and include in replay.
    pub events: Vec<E>,
    /// Application effects to run only after preceding events are durable.
    pub effects: Vec<F>,
}

impl<E, F> SessionTransition<E, F> {
    /// Creates a transition from durable events and follow-on effects.
    #[must_use]
    pub fn new(events: Vec<E>, effects: Vec<F>) -> Self {
        Self { events, effects }
    }

    /// Creates an event-only transition.
    #[must_use]
    pub fn events(events: impl IntoIterator<Item = E>) -> Self {
        Self {
            events: events.into_iter().collect(),
            effects: Vec::new(),
        }
    }

    /// Creates a transition that has no durable or visible change.
    #[must_use]
    pub const fn idle() -> Self {
        Self {
            events: Vec::new(),
            effects: Vec::new(),
        }
    }
}

/// Product-specific behavior hosted by a [`super::ResidentHost`].
pub trait ResidentApplication: Clone + Send + Sync + 'static {
    /// Client command decoded from the generic wire envelope.
    type Command: DeserializeOwned + Send + 'static;
    /// Durable, replayable session event.
    type Event: Clone + DeserializeOwned + Serialize + Send + Sync + 'static;
    /// Complete client-facing state used for attach and resynchronization.
    type Snapshot: Clone + DeserializeOwned + Serialize + Send + Sync + 'static;
    /// Complete application state stored in checkpoints.
    type State: Clone + DeserializeOwned + Serialize + Send + Sync + 'static;
    /// Follow-on operation run after its preceding events are durable.
    type Effect: Send + 'static;
    /// Per-session state machine.
    type Session: ResidentSession<
            Command = Self::Command,
            Event = Self::Event,
            Snapshot = Self::Snapshot,
            State = Self::State,
            Effect = Self::Effect,
        >;

    /// Version of the application's checkpoint state representation.
    const STORAGE_VERSION: u32;

    /// Creates a new named session.
    fn create(&self, name: &str) -> Result<Self::Session, ApplicationError>;

    /// Restores a session from the current checkpoint representation.
    fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError>;

    /// Migrates an older serialized checkpoint into the current representation.
    fn migrate(
        &self,
        stored_version: u32,
        state: serde_json::Value,
    ) -> Result<Self::State, ApplicationError> {
        if stored_version != Self::STORAGE_VERSION {
            return Err(ApplicationError::new(
                "unsupported_storage_version",
                format!(
                    "stored application state is version {stored_version}; this binary requires {}",
                    Self::STORAGE_VERSION
                ),
            ));
        }
        serde_json::from_value(state).map_err(|error| {
            ApplicationError::new("invalid_checkpoint", format!("invalid checkpoint: {error}"))
        })
    }
}

/// One product session managed by the resident actor.
pub trait ResidentSession: Send + 'static {
    /// Client command type.
    type Command;
    /// Durable event type.
    type Event;
    /// Client snapshot type.
    type Snapshot;
    /// Checkpoint state type.
    type State;
    /// Follow-on effect type.
    type Effect;

    /// Returns current client-facing state.
    fn snapshot(&self) -> Self::Snapshot;

    /// Returns current checkpoint state.
    fn state(&self) -> Self::State;

    /// Handles an authorized client command.
    fn command(
        &mut self,
        command: Self::Command,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError>;

    /// Gives an active or background session an opportunity to advance.
    fn poll(
        &mut self,
        _elapsed: Duration,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        Ok(SessionTransition::idle())
    }

    /// Runs a follow-on effect after preceding events are durable.
    fn effect(
        &mut self,
        effect: Self::Effect,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError>;

    /// Applies a durable event found after the latest checkpoint during recovery.
    fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError>;
}
