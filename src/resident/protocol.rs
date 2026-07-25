use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The current resident wire protocol.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

macro_rules! uuid_id {
    ($name:ident) => {
        #[doc = concat!("A stable ", stringify!($name), ".")]
        #[derive(
            Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            /// Generates a new random identity.
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = uuid::Error;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value).map(Self)
            }
        }
    };
}

uuid_id!(ClientInstanceId);
uuid_id!(EffectId);
uuid_id!(LeaderInstanceId);
uuid_id!(SessionId);

/// A leader-local transport connection identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ConnectionId(pub u64);

/// A monotonically increasing request number scoped to one client instance.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct RequestId {
    /// Stable client identity, retained across reconnects.
    pub client: ClientInstanceId,
    /// Client-local sequence number.
    pub sequence: u64,
}

/// A monotonically increasing event number scoped to one session.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct EventSequence(pub u64);

/// A fencing token for one session's current driver.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct LeaseEpoch(pub u64);

/// A resident protocol version.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

/// Inclusive protocol versions accepted by a client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VersionRange {
    /// Oldest accepted protocol.
    pub minimum: ProtocolVersion,
    /// Newest accepted protocol.
    pub maximum: ProtocolVersion,
}

impl VersionRange {
    /// A range accepting only the current protocol.
    #[must_use]
    pub const fn current() -> Self {
        Self {
            minimum: PROTOCOL_VERSION,
            maximum: PROTOCOL_VERSION,
        }
    }

    /// Negotiates the newest mutually supported version.
    #[must_use]
    pub fn negotiate(self, peer: Self) -> Option<ProtocolVersion> {
        let minimum = self.minimum.max(peer.minimum);
        let maximum = self.maximum.min(peer.maximum);
        (minimum <= maximum).then_some(maximum)
    }
}

/// Features understood by a resident client.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct ClientCapabilities {
    /// Client can consume incremental session events.
    pub incremental_events: bool,
    /// Client can reconnect using an event cursor.
    pub resumable: bool,
    /// Client understands fenced driver leases.
    pub driver_leases: bool,
}

/// Features offered by a resident leader.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default)]
pub struct LeaderCapabilities {
    /// Leader supports multiple named sessions.
    pub named_sessions: bool,
    /// Leader persists session state.
    pub durable_sessions: bool,
    /// Leader supports multiple viewers and one driver.
    pub shared_sessions: bool,
}

/// The first message sent by a client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClientHello {
    /// Supported protocol versions.
    pub protocol: VersionRange,
    /// Client package version.
    pub binary_version: String,
    /// Stable identity retained during reconnects.
    pub client_instance: ClientInstanceId,
    /// Human-readable client name.
    pub client_name: String,
    /// Optional client features.
    #[serde(default)]
    pub capabilities: ClientCapabilities,
}

/// The first response sent by a compatible leader.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServerHello {
    /// Negotiated protocol version.
    pub protocol: ProtocolVersion,
    /// Leader package version.
    pub binary_version: String,
    /// Identity that changes after every leader restart.
    pub leader_instance: LeaderInstanceId,
    /// Optional leader features.
    #[serde(default)]
    pub capabilities: LeaderCapabilities,
}

/// The leader's registration response, including an explicit incompatibility reply.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum ServerHandshake {
    /// The client and leader selected a shared protocol.
    Accepted(ServerHello),
    /// The leader could not select a shared protocol.
    Rejected(ProtocolRejection),
}

/// A structured handshake rejection that can be shown without guessing why EOF occurred.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolRejection {
    /// Marker that prevents this response from being mistaken for [`ServerHello`].
    pub rejected: bool,
    /// Protocol versions accepted by the leader.
    pub supported: VersionRange,
    /// Leader package version.
    pub binary_version: String,
    /// Human-readable incompatibility detail.
    pub message: String,
}

/// How a client attaches to a session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentMode {
    /// Receive events and request the driver lease.
    Drive,
    /// Receive events without mutation authority.
    View,
}

/// A session selected by stable ID or display name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSelector {
    /// Select by stable identity.
    Id(SessionId),
    /// Select by unique display name.
    Name(String),
}

/// The current driver lease.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DriverLease {
    /// Client allowed to mutate the session.
    pub owner: ClientInstanceId,
    /// Fencing token required by every mutation.
    pub epoch: LeaseEpoch,
}

/// A compact session listing.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSummary {
    /// Stable session identity.
    pub id: SessionId,
    /// Unique display name.
    pub name: String,
    /// Current driver, if any.
    pub driver: Option<DriverLease>,
    /// Number of attached clients, including the driver.
    #[serde(rename = "viewers", alias = "attached_clients")]
    pub attached_clients: usize,
    /// Latest committed event.
    pub sequence: EventSequence,
    /// Milliseconds since the Unix epoch when the latest event was committed.
    ///
    /// `None` before the session commits anything, and on payloads from hosts
    /// predating this field.
    #[serde(default)]
    pub last_event_at: Option<u64>,
    /// Application-supplied summary payload for list and overview rendering.
    ///
    /// The resident layer never interprets this; it carries whatever
    /// [`crate::resident::ResidentSession::digest`] returned.
    #[serde(default)]
    pub digest: Option<serde_json::Value>,
}

/// A request envelope with an idempotency key.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClientEnvelope {
    /// Stable request identity.
    pub request: RequestId,
    /// Requested control or domain operation.
    pub message: ClientRequest,
}

/// Requests understood by the generic resident leader.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientRequest {
    /// Inspect the leader and its sessions.
    Status,
    /// List resident sessions.
    ListSessions,
    /// Create a named session.
    CreateSession {
        /// Unique display name.
        name: String,
    },
    /// Rename one durable session.
    RenameSession {
        /// Session to rename.
        session: SessionId,
        /// New unique display name.
        name: String,
    },
    /// Attach to a session.
    Attach {
        /// Session to attach.
        session: SessionSelector,
        /// Requested authority.
        mode: AttachmentMode,
        /// Last event already observed by the client.
        after: Option<EventSequence>,
        /// Whether an existing driver may be displaced.
        #[serde(default)]
        force: bool,
    },
    /// Submit a domain command.
    Command {
        /// Target session.
        session: SessionId,
        /// Current driver fencing token.
        lease: LeaseEpoch,
        /// Application-defined command.
        command: serde_json::Value,
    },
    /// Release this client's attachment.
    Detach {
        /// Session to detach.
        session: SessionId,
    },
    /// Acquire or take over the driver lease.
    AcquireDriver {
        /// Target session.
        session: SessionId,
        /// Whether an existing driver may be displaced.
        force: bool,
    },
    /// Release the driver lease while remaining attached.
    ReleaseDriver {
        /// Target session.
        session: SessionId,
    },
    /// Stop and remove one session.
    StopSession {
        /// Session to stop and remove.
        session: SessionId,
    },
    /// Liveness probe.
    Ping,
    /// Gracefully stop the leader.
    StopLeader,
    /// Replace an older compatible leader with the requesting binary version.
    ReplaceLeader {
        /// Version of the replacement binary waiting to take leadership.
        binary_version: String,
    },
}

/// Successful request results.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlResult {
    /// Leader status.
    Status {
        /// Leader process ID.
        pid: u32,
        /// Current leader instance.
        leader: LeaderInstanceId,
        /// Resident sessions.
        sessions: Vec<SessionSummary>,
    },
    /// Session listing.
    Sessions {
        /// Resident sessions.
        sessions: Vec<SessionSummary>,
    },
    /// A newly created session.
    Created {
        /// Created session.
        session: SessionSummary,
    },
    /// A renamed session.
    Renamed {
        /// Updated session summary.
        session: SessionSummary,
    },
    /// Successful attachment.
    Attached {
        /// Attached session.
        session: SessionSummary,
        /// Granted fencing token for a driving attachment.
        lease: Option<LeaseEpoch>,
    },
    /// A domain command was durably accepted.
    Accepted {
        /// Target session.
        session: SessionId,
        /// Committed event sequence.
        sequence: EventSequence,
        /// Whether this was a replay of an already committed request.
        duplicate: bool,
    },
    /// The client detached.
    Detached {
        /// Detached session.
        session: SessionId,
    },
    /// The driver lease changed.
    Driver {
        /// Changed session.
        session: SessionId,
        /// Current driver.
        lease: Option<DriverLease>,
    },
    /// A session or leader stop was accepted.
    Stopping,
    /// Liveness response.
    Pong,
}

/// A stable wire error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WireError {
    /// Machine-readable error code.
    pub code: String,
    /// Human-readable detail.
    pub message: String,
}

impl fmt::Display for WireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

/// Why a leader is exiting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownReason {
    /// Explicit administrative request.
    Manual,
    /// Replacement by a compatible newer binary.
    Upgrade,
    /// Internal unrecoverable error.
    Failure,
}

/// Messages emitted after the handshake.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    /// Response to one request.
    Response {
        /// Request being answered.
        request: RequestId,
        /// Successful result or stable wire error.
        result: Result<ControlResult, WireError>,
    },
    /// Complete state used for first attach or resynchronization.
    Snapshot {
        /// Session represented by the state.
        session: SessionId,
        /// Latest event included in the state.
        sequence: EventSequence,
        /// Application-defined state.
        state: serde_json::Value,
    },
    /// One incremental domain event.
    Event {
        /// Session producing the event.
        session: SessionId,
        /// Monotonic session event sequence.
        sequence: EventSequence,
        /// Application-defined event.
        event: serde_json::Value,
    },
    /// Driver ownership changed.
    DriverChanged {
        /// Changed session.
        session: SessionId,
        /// Current driver.
        lease: Option<DriverLease>,
    },
    /// The client's cursor was compacted or its outbound queue overflowed.
    ResyncRequired {
        /// Session requiring a fresh snapshot.
        session: SessionId,
        /// Latest event available at the leader.
        latest: EventSequence,
    },
    /// Advance notice of graceful shutdown.
    ShuttingDown {
        /// Shutdown reason.
        reason: ShutdownReason,
    },
    /// Final graceful-shutdown message.
    Shutdown {
        /// Completed shutdown reason.
        reason: ShutdownReason,
    },
}

/// Returns whether `requested` displaces a leader running `current`.
///
/// Versions that do not parse as semver fall back to string ordering, so a
/// product using its own versioning scheme still gets a total order rather
/// than silently refusing every replacement.
#[must_use]
pub fn replacement_is_newer(current: &str, requested: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(requested),
    ) {
        (Ok(current), Ok(requested)) => requested > current,
        _ => requested > current,
    }
}
