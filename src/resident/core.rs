use std::collections::{HashMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::{
    AttachmentMode, ClientInstanceId, ConnectionId, DriverLease, EventSequence, LeaseEpoch,
    RequestId, SessionId, SessionSummary,
};

const DEFAULT_DEDUPLICATION_WINDOW: usize = 4_096;

/// Restorable control-plane state for one session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionControlSnapshot {
    /// Stable session identity.
    pub id: SessionId,
    /// Unique display name.
    pub name: String,
    /// Latest committed event sequence.
    pub sequence: EventSequence,
    /// Current lease epoch. The owner is intentionally not restored after restart.
    pub lease_epoch: LeaseEpoch,
    /// Recently committed requests used for reconnect deduplication.
    pub committed: Vec<(RequestId, EventSequence)>,
}

#[derive(Debug)]
struct ClientState {
    instance: ClientInstanceId,
    sessions: HashSet<SessionId>,
}

#[derive(Debug)]
struct SessionState {
    id: SessionId,
    name: String,
    subscribers: HashSet<ConnectionId>,
    driver: Option<DriverLease>,
    lease_epoch: LeaseEpoch,
    sequence: EventSequence,
    committed: HashMap<RequestId, EventSequence>,
    commit_order: VecDeque<RequestId>,
}

impl SessionState {
    fn summary(&self) -> SessionSummary {
        SessionSummary {
            id: self.id,
            name: self.name.clone(),
            driver: self.driver,
            viewers: self.subscribers.len(),
            sequence: self.sequence,
        }
    }

    fn snapshot(&self) -> SessionControlSnapshot {
        SessionControlSnapshot {
            id: self.id,
            name: self.name.clone(),
            sequence: self.sequence,
            lease_epoch: self.lease_epoch,
            committed: self
                .commit_order
                .iter()
                .filter_map(|request| {
                    self.committed
                        .get(request)
                        .copied()
                        .map(|sequence| (*request, sequence))
                })
                .collect(),
        }
    }
}

/// Leader-core failures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CoreError {
    /// Unknown transport connection.
    UnknownConnection,
    /// Unknown session.
    UnknownSession,
    /// A session already uses the requested name.
    DuplicateName,
    /// A session name is empty after trimming.
    InvalidName,
    /// The client is not subscribed to the session.
    NotAttached,
    /// The client is not the current driver.
    NotDriver,
    /// The command carries an obsolete fencing token.
    StaleLease {
        /// Current lease epoch.
        current: LeaseEpoch,
    },
    /// Request identity belongs to another client.
    ForeignRequest,
}

impl std::fmt::Display for CoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownConnection => formatter.write_str("unknown resident connection"),
            Self::UnknownSession => formatter.write_str("unknown resident session"),
            Self::DuplicateName => formatter.write_str("a session already has that name"),
            Self::InvalidName => formatter.write_str("session name cannot be empty"),
            Self::NotAttached => formatter.write_str("client is not attached to this session"),
            Self::NotDriver => formatter.write_str("client does not hold the driver lease"),
            Self::StaleLease { current } => {
                write!(
                    formatter,
                    "stale driver lease; current epoch is {}",
                    current.0
                )
            }
            Self::ForeignRequest => {
                formatter.write_str("request identity belongs to another client")
            }
        }
    }
}

impl std::error::Error for CoreError {}

/// Attachment conflicts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttachError {
    /// Generic control-plane failure.
    Core(CoreError),
    /// Another client currently owns the driver lease.
    DriverBusy(DriverLease),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Core(error) => error.fmt(formatter),
            Self::DriverBusy(lease) => write!(
                formatter,
                "session is driven by client {} at epoch {}",
                lease.owner, lease.epoch.0
            ),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<CoreError> for AttachError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

/// Result of attaching to a session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachOutcome {
    /// Current session summary.
    pub session: SessionSummary,
    /// Lease granted to a driving client.
    pub lease: Option<LeaseEpoch>,
    /// Driver transition to broadcast, if ownership changed.
    pub driver_change: Option<DriverChange>,
}

/// A driver transition and its recipients.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverChange {
    /// Changed session.
    pub session: SessionId,
    /// New driver, if any.
    pub lease: Option<DriverLease>,
    /// Current subscribers to notify.
    pub subscribers: Vec<ConnectionId>,
}

/// Whether a validated domain request should execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Authorization {
    /// Execute and eventually commit this request.
    Apply,
    /// Return the original committed sequence without executing again.
    Duplicate(EventSequence),
}

/// Deterministic control plane for a single resident leader.
#[derive(Debug)]
pub struct LeaderCore {
    clients: HashMap<ConnectionId, ClientState>,
    sessions: HashMap<SessionId, SessionState>,
    names: HashMap<String, SessionId>,
    deduplication_window: usize,
}

impl Default for LeaderCore {
    fn default() -> Self {
        Self::new()
    }
}

impl LeaderCore {
    /// Creates an empty leader core.
    #[must_use]
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
            sessions: HashMap::new(),
            names: HashMap::new(),
            deduplication_window: DEFAULT_DEDUPLICATION_WINDOW,
        }
    }

    /// Overrides the per-session committed-request retention window.
    #[must_use]
    pub fn with_deduplication_window(mut self, window: usize) -> Self {
        self.deduplication_window = window.max(1);
        self
    }

    /// Registers a new transport connection.
    pub fn connect(&mut self, connection: ConnectionId, instance: ClientInstanceId) {
        self.clients.insert(
            connection,
            ClientState {
                instance,
                sessions: HashSet::new(),
            },
        );
    }

    /// Removes a connection and releases its subscriptions and driver leases.
    pub fn disconnect(&mut self, connection: ConnectionId) -> Vec<DriverChange> {
        let Some(client) = self.clients.remove(&connection) else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for session_id in client.sessions {
            let Some(session) = self.sessions.get_mut(&session_id) else {
                continue;
            };
            session.subscribers.remove(&connection);
            let same_client_remains = self.clients.values().any(|other| {
                other.instance == client.instance && other.sessions.contains(&session_id)
            });
            if !same_client_remains
                && session
                    .driver
                    .is_some_and(|lease| lease.owner == client.instance)
            {
                session.driver = None;
                changes.push(driver_change(session));
            }
        }
        changes
    }

    /// Creates a new empty session control record.
    pub fn create_session(
        &mut self,
        id: SessionId,
        name: impl Into<String>,
    ) -> Result<SessionSummary, CoreError> {
        let name = normalize_name(name.into());
        if name.is_empty() {
            return Err(CoreError::InvalidName);
        }
        if self.names.contains_key(&name) {
            return Err(CoreError::DuplicateName);
        }
        let session = SessionState {
            id,
            name: name.clone(),
            subscribers: HashSet::new(),
            driver: None,
            lease_epoch: LeaseEpoch(0),
            sequence: EventSequence(0),
            committed: HashMap::new(),
            commit_order: VecDeque::new(),
        };
        let summary = session.summary();
        self.names.insert(name, id);
        self.sessions.insert(id, session);
        Ok(summary)
    }

    /// Restores session control state. Driver ownership is deliberately cleared.
    pub fn restore_session(&mut self, snapshot: SessionControlSnapshot) -> Result<(), CoreError> {
        let name = normalize_name(snapshot.name);
        if self.names.contains_key(&name) {
            return Err(CoreError::DuplicateName);
        }
        let committed: HashMap<_, _> = snapshot.committed.iter().copied().collect();
        let commit_order = snapshot
            .committed
            .iter()
            .map(|(request, _)| *request)
            .collect();
        self.names.insert(name.clone(), snapshot.id);
        self.sessions.insert(
            snapshot.id,
            SessionState {
                id: snapshot.id,
                name,
                subscribers: HashSet::new(),
                driver: None,
                lease_epoch: LeaseEpoch(snapshot.lease_epoch.0.saturating_add(1)),
                sequence: snapshot.sequence,
                committed,
                commit_order,
            },
        );
        Ok(())
    }

    /// Lists sessions in stable name order.
    #[must_use]
    pub fn sessions(&self) -> Vec<SessionSummary> {
        let mut sessions: Vec<_> = self.sessions.values().map(SessionState::summary).collect();
        sessions.sort_by(|left, right| left.name.cmp(&right.name));
        sessions
    }

    /// Resolves a display name.
    #[must_use]
    pub fn session_named(&self, name: &str) -> Option<SessionId> {
        self.names.get(&normalize_name(name.to_owned())).copied()
    }

    /// Renames a session while preserving its stable identity and state.
    pub fn rename_session(
        &mut self,
        session_id: SessionId,
        name: impl Into<String>,
    ) -> Result<SessionSummary, CoreError> {
        let name = normalize_name(name.into());
        if name.is_empty() {
            return Err(CoreError::InvalidName);
        }
        if self
            .names
            .get(&name)
            .is_some_and(|existing| *existing != session_id)
        {
            return Err(CoreError::DuplicateName);
        }
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        self.names.remove(&session.name);
        session.name.clone_from(&name);
        self.names.insert(name, session_id);
        Ok(session.summary())
    }

    /// Attaches a connection as a viewer or driver.
    pub fn attach(
        &mut self,
        connection: ConnectionId,
        session_id: SessionId,
        mode: AttachmentMode,
        force: bool,
    ) -> Result<AttachOutcome, AttachError> {
        let client_instance = self
            .clients
            .get(&connection)
            .ok_or(CoreError::UnknownConnection)?;
        let client_instance = client_instance.instance;
        let existing_driver = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::UnknownSession)?
            .driver;
        if mode == AttachmentMode::Drive
            && let Some(current) = existing_driver
            && current.owner != client_instance
            && !force
        {
            return Err(AttachError::DriverBusy(current));
        }
        let client = self
            .clients
            .get_mut(&connection)
            .ok_or(CoreError::UnknownConnection)?;
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        client.sessions.insert(session_id);
        session.subscribers.insert(connection);

        let mut changed_driver = None;
        let lease = if mode == AttachmentMode::Drive {
            match session.driver {
                Some(current) if current.owner == client.instance => Some(current.epoch),
                Some(current) if !force => return Err(AttachError::DriverBusy(current)),
                _ => {
                    session.lease_epoch = LeaseEpoch(session.lease_epoch.0.saturating_add(1));
                    let granted = DriverLease {
                        owner: client.instance,
                        epoch: session.lease_epoch,
                    };
                    session.driver = Some(granted);
                    changed_driver = Some(driver_change(session));
                    Some(granted.epoch)
                }
            }
        } else {
            None
        };
        Ok(AttachOutcome {
            session: session.summary(),
            lease,
            driver_change: changed_driver,
        })
    }

    /// Detaches a connection and releases its driver lease.
    pub fn detach(
        &mut self,
        connection: ConnectionId,
        session_id: SessionId,
    ) -> Result<Option<DriverChange>, CoreError> {
        let client = self
            .clients
            .get_mut(&connection)
            .ok_or(CoreError::UnknownConnection)?;
        if !client.sessions.remove(&session_id) {
            return Err(CoreError::NotAttached);
        }
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        session.subscribers.remove(&connection);
        if session
            .driver
            .is_some_and(|lease| lease.owner == client.instance)
        {
            session.driver = None;
            return Ok(Some(driver_change(session)));
        }
        Ok(None)
    }

    /// Releases a driver's mutation authority without detaching its viewer.
    pub fn release_driver(
        &mut self,
        connection: ConnectionId,
        session_id: SessionId,
    ) -> Result<Option<DriverChange>, CoreError> {
        let client = self
            .clients
            .get(&connection)
            .ok_or(CoreError::UnknownConnection)?;
        if !client.sessions.contains(&session_id) {
            return Err(CoreError::NotAttached);
        }
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        if session
            .driver
            .is_some_and(|lease| lease.owner == client.instance)
        {
            session.driver = None;
            return Ok(Some(driver_change(session)));
        }
        Err(CoreError::NotDriver)
    }

    /// Removes session control state and all subscriptions.
    pub fn remove_session(&mut self, session_id: SessionId) -> Result<(), CoreError> {
        let session = self
            .sessions
            .remove(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        self.names.remove(&session.name);
        for client in self.clients.values_mut() {
            client.sessions.remove(&session_id);
        }
        Ok(())
    }

    /// Validates a domain mutation and detects reconnect retries.
    pub fn authorize(
        &self,
        connection: ConnectionId,
        session_id: SessionId,
        lease: LeaseEpoch,
        request: RequestId,
    ) -> Result<Authorization, CoreError> {
        let client = self
            .clients
            .get(&connection)
            .ok_or(CoreError::UnknownConnection)?;
        if request.client != client.instance {
            return Err(CoreError::ForeignRequest);
        }
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        if !client.sessions.contains(&session_id) {
            return Err(CoreError::NotAttached);
        }
        if let Some(sequence) = session.committed.get(&request) {
            return Ok(Authorization::Duplicate(*sequence));
        }
        let driver = session.driver.ok_or(CoreError::NotDriver)?;
        if driver.owner != client.instance {
            return Err(CoreError::NotDriver);
        }
        if driver.epoch != lease {
            return Err(CoreError::StaleLease {
                current: driver.epoch,
            });
        }
        Ok(Authorization::Apply)
    }

    /// Commits an authorized request and allocates its event sequence.
    pub fn commit(
        &mut self,
        session_id: SessionId,
        request: RequestId,
    ) -> Result<EventSequence, CoreError> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        if let Some(sequence) = session.committed.get(&request) {
            return Ok(*sequence);
        }
        session.sequence = EventSequence(session.sequence.0.saturating_add(1));
        session.committed.insert(request, session.sequence);
        session.commit_order.push_back(request);
        while session.commit_order.len() > self.deduplication_window {
            if let Some(expired) = session.commit_order.pop_front() {
                session.committed.remove(&expired);
            }
        }
        Ok(session.sequence)
    }

    /// Allocates a sequence for an unsolicited domain event such as process output.
    pub fn publish(&mut self, session_id: SessionId) -> Result<EventSequence, CoreError> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        session.sequence = EventSequence(session.sequence.0.saturating_add(1));
        Ok(session.sequence)
    }

    /// Restores a journal record newer than the latest checkpoint.
    pub fn replay(
        &mut self,
        session_id: SessionId,
        sequence: EventSequence,
        request: Option<RequestId>,
    ) -> Result<(), CoreError> {
        let session = self
            .sessions
            .get_mut(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        if sequence <= session.sequence {
            return Ok(());
        }
        session.sequence = sequence;
        if let Some(request) = request {
            session.committed.insert(request, sequence);
            session.commit_order.push_back(request);
            while session.commit_order.len() > self.deduplication_window {
                if let Some(expired) = session.commit_order.pop_front() {
                    session.committed.remove(&expired);
                }
            }
        }
        Ok(())
    }

    /// Connections subscribed to a session.
    pub fn subscribers(&self, session_id: SessionId) -> Result<Vec<ConnectionId>, CoreError> {
        let session = self
            .sessions
            .get(&session_id)
            .ok_or(CoreError::UnknownSession)?;
        Ok(session.subscribers.iter().copied().collect())
    }

    /// Serializable control state for persistence.
    pub fn snapshot(&self, session_id: SessionId) -> Result<SessionControlSnapshot, CoreError> {
        self.sessions
            .get(&session_id)
            .map(SessionState::snapshot)
            .ok_or(CoreError::UnknownSession)
    }
}

fn driver_change(session: &SessionState) -> DriverChange {
    DriverChange {
        session: session.id,
        lease: session.driver,
        subscribers: session.subscribers.iter().copied().collect(),
    }
}

fn normalize_name(name: String) -> String {
    name.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forced_takeover_fences_the_previous_driver() {
        let mut core = LeaderCore::new();
        let session = SessionId::new();
        core.create_session(session, "build").expect("create");
        let first = ConnectionId(1);
        let second = ConnectionId(2);
        let first_client = ClientInstanceId::new();
        let second_client = ClientInstanceId::new();
        core.connect(first, first_client);
        core.connect(second, second_client);
        let old = core
            .attach(first, session, AttachmentMode::Drive, false)
            .expect("first attach")
            .lease
            .expect("lease");
        let new = core
            .attach(second, session, AttachmentMode::Drive, true)
            .expect("takeover")
            .lease
            .expect("lease");
        assert!(new > old);
        let request = RequestId {
            client: first_client,
            sequence: 1,
        };
        assert_eq!(
            core.authorize(first, session, old, request),
            Err(CoreError::NotDriver)
        );
    }

    #[test]
    fn a_retried_request_is_not_applied_twice() {
        let mut core = LeaderCore::new();
        let session = SessionId::new();
        core.create_session(session, "build").expect("create");
        let connection = ConnectionId(1);
        let client = ClientInstanceId::new();
        core.connect(connection, client);
        let lease = core
            .attach(connection, session, AttachmentMode::Drive, false)
            .expect("attach")
            .lease
            .expect("lease");
        let request = RequestId {
            client,
            sequence: 4,
        };
        assert_eq!(
            core.authorize(connection, session, lease, request),
            Ok(Authorization::Apply)
        );
        let committed = core.commit(session, request).expect("commit");
        assert_eq!(
            core.authorize(connection, session, lease, request),
            Ok(Authorization::Duplicate(committed))
        );
    }
}
