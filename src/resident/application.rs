use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use serde::{Serialize, de::DeserializeOwned};

use super::{EffectId, SessionId};

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

/// Delivery guarantee applied to one durable effect.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectDelivery {
    /// Retry an unfinished effect after leader recovery with the same effect identity.
    #[default]
    AtLeastOnce,
    /// Never retry after execution may have started; report an unknown outcome instead.
    AtMostOnce,
}

/// One effect requested by a session transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectRequest<F> {
    /// Delivery guarantee used during leader recovery.
    pub delivery: EffectDelivery,
    /// Application-owned effect payload.
    pub effect: F,
    /// Optional deadline overriding the host default.
    pub timeout: Option<Duration>,
}

impl<F> EffectRequest<F> {
    /// Creates an idempotent effect that is redriven until it completes.
    #[must_use]
    pub const fn at_least_once(effect: F) -> Self {
        Self {
            delivery: EffectDelivery::AtLeastOnce,
            effect,
            timeout: None,
        }
    }

    /// Creates a non-idempotent effect that is never executed twice.
    #[must_use]
    pub const fn at_most_once(effect: F) -> Self {
        Self {
            delivery: EffectDelivery::AtMostOnce,
            effect,
            timeout: None,
        }
    }

    /// Overrides the host's default deadline for this effect.
    #[must_use]
    pub const fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

/// Cooperative cancellation signal shared with one effect execution attempt.
#[derive(Clone)]
pub struct EffectCancellation {
    state: Arc<CancellationState>,
}

struct CancellationState {
    cancelled: AtomicBool,
    waker: Mutex<Option<Waker>>,
}

impl EffectCancellation {
    pub(crate) fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
                waker: Mutex::new(None),
            }),
        }
    }

    /// Returns whether cancellation has been requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Resolves when cancellation is requested.
    pub fn cancelled(&self) -> impl Future<Output = ()> + Send + 'static {
        Cancelled {
            state: Arc::clone(&self.state),
        }
    }

    pub(crate) fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel)
            && let Some(waker) = self.state.waker.lock().expect("waker lock poisoned").take()
        {
            waker.wake();
        }
    }
}

struct Cancelled {
    state: Arc<CancellationState>,
}

impl Future for Cancelled {
    type Output = ();

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        if self.state.cancelled.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        let mut waker = self.state.waker.lock().expect("waker lock poisoned");
        if self.state.cancelled.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            waker.clone_from(&Some(context.waker().clone()));
            Poll::Pending
        }
    }
}

/// Stable context supplied to every effect execution attempt.
#[derive(Clone)]
pub struct EffectContext {
    /// Session that requested the effect.
    pub session: SessionId,
    /// Stable identity retained across at-least-once retries.
    pub effect: EffectId,
    /// One-based execution attempt number.
    pub attempt: u32,
    /// Cooperative cancellation signal for timeout and shutdown handling.
    pub cancellation: EffectCancellation,
}

/// Durable application events and follow-on effects produced by one transition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionTransition<E, F> {
    /// Events to journal, apply to subscribers, and include in replay.
    pub events: Vec<E>,
    /// Application effects to run only after preceding events are durable.
    pub effects: Vec<EffectRequest<F>>,
}

impl<E, F> SessionTransition<E, F> {
    /// Creates a transition from durable events and follow-on effects.
    #[must_use]
    pub fn new(events: Vec<E>, effects: Vec<F>) -> Self {
        Self {
            events,
            effects: effects
                .into_iter()
                .map(EffectRequest::at_least_once)
                .collect(),
        }
    }

    /// Creates a transition with explicit effect delivery guarantees.
    #[must_use]
    pub fn with_effects(events: Vec<E>, effects: Vec<EffectRequest<F>>) -> Self {
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
    type Effect: Clone + DeserializeOwned + Serialize + Send + 'static;
    /// Result produced by asynchronous effect execution.
    type EffectOutput: Send + 'static;
    /// Per-session state machine.
    type Session: ResidentSession<
            Command = Self::Command,
            Event = Self::Event,
            Snapshot = Self::Snapshot,
            State = Self::State,
            Effect = Self::Effect,
            EffectOutput = Self::EffectOutput,
        >;

    /// Version of the application's checkpoint state representation.
    const STORAGE_VERSION: u32;

    /// Creates a new named session.
    fn create(&self, name: &str) -> Result<Self::Session, ApplicationError>;

    /// Restores a session from the current checkpoint representation.
    fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError>;

    /// Executes external work without borrowing the resident actor or session state.
    fn execute(
        &self,
        context: EffectContext,
        effect: Self::Effect,
    ) -> impl Future<Output = Result<Self::EffectOutput, ApplicationError>> + Send;

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
    /// Asynchronous effect result type.
    type EffectOutput;

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

    /// Reduces one asynchronous effect outcome after the host receives it.
    fn effect_completed(
        &mut self,
        effect: EffectId,
        output: Result<Self::EffectOutput, ApplicationError>,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError>;

    /// Applies a durable event found after the latest checkpoint during recovery.
    fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError>;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::EffectCancellation;

    #[tokio::test]
    async fn cancellation_wakes_a_waiting_effect() {
        let cancellation = EffectCancellation::new();
        let waiting = cancellation.cancelled();
        cancellation.cancel();
        tokio::time::timeout(Duration::from_millis(100), waiting)
            .await
            .expect("cancellation should wake its waiter");
        assert!(cancellation.is_cancelled());
    }
}
