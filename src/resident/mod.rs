//! Runtime-neutral building blocks for resident, reconnectable surfaces.
//!
//! The resident core owns connection identity, session subscriptions, driver
//! leases, request deduplication, and event sequencing. Transport, timers,
//! persistence, and domain work are effects supplied by adapters.

mod application;
#[cfg(all(unix, feature = "tokio"))]
pub mod blocking;
mod client;
mod core;
mod election;
mod framing;
mod host;
mod journal;
mod protocol;
pub mod runtime;
#[cfg(all(unix, feature = "tokio"))]
pub mod supervisor;

pub use application::{
    ApplicationError, EffectCancellation, EffectContext, EffectDelivery, EffectRequest, EffectWake,
    ResidentApplication, ResidentSession, SessionTransition,
};
pub use client::{Attachment, ClientError, ExchangeId, ExchangeItem, ResidentClient};
pub use core::{
    AttachError, AttachOutcome, Authorization, CoreError, DriverChange, LeaderCore,
    SessionControlSnapshot,
};
pub use election::{LeaderLock, LockError};
pub use framing::{FrameDecoder, FrameError, MAX_FRAME_SIZE, encode_frame};
pub use host::{ResidentHost, ResidentHostConfig};
pub use journal::{Durability, FileJournal, JournalError, JournalRecord};
pub use protocol::{
    AttachmentMode, ClientCapabilities, ClientEnvelope, ClientHello, ClientInstanceId,
    ClientRequest, ConnectionId, ControlResult, DriverLease, EffectId, EventSequence,
    LeaderCapabilities, LeaderInstanceId, LeaseEpoch, PROTOCOL_VERSION, ProtocolRejection,
    ProtocolVersion, RequestId, ServerHandshake, ServerHello, ServerMessage, SessionId,
    SessionSelector, SessionSummary, ShutdownReason, VersionRange, WireError, replacement_is_newer,
};
