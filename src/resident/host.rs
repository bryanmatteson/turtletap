use std::{
    collections::{HashMap, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    ApplicationError, AttachmentMode, Authorization, ClientEnvelope, ClientHello, ClientInstanceId,
    ClientRequest, ConnectionId, ControlResult, DriverChange, Durability, EventSequence,
    FileJournal, JournalRecord, LeaderCapabilities, LeaderCore, LeaderInstanceId, LeaderLock,
    ProtocolRejection, RequestId, ResidentApplication, ResidentSession, ServerHandshake,
    ServerHello, ServerMessage, SessionControlSnapshot, SessionId, SessionSelector, SessionSummary,
    SessionTransition, ShutdownReason, VersionRange, WireError, encode_frame,
    runtime::{Clock, FrameWriter as _, Listener as _, Spawner, Transport},
};

const HOST_STORAGE_VERSION: u32 = 1;

/// Configuration shared by every resident application host.
#[derive(Clone, Debug)]
pub struct ResidentHostConfig {
    /// Local transport endpoint.
    pub endpoint: PathBuf,
    /// Durable session directory.
    pub state_dir: PathBuf,
    /// Host binary version exposed during registration.
    pub binary_version: String,
    /// Periodic application poll interval.
    pub tick_rate: Duration,
    /// Per-client outbound event capacity.
    pub outbound_capacity: usize,
    /// Number of recent events retained for cursor replay.
    pub event_history: usize,
    /// Journal flush policy.
    pub durability: Durability,
    /// Session created when no durable sessions exist.
    pub initial_session: Option<String>,
}

impl ResidentHostConfig {
    /// Creates production-oriented defaults for an endpoint and state directory.
    #[must_use]
    pub fn new(
        endpoint: impl Into<PathBuf>,
        state_dir: impl Into<PathBuf>,
        binary_version: impl Into<String>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            state_dir: state_dir.into(),
            binary_version: binary_version.into(),
            tick_rate: Duration::from_millis(20),
            outbound_capacity: 256,
            event_history: 1_024,
            durability: Durability::Flush,
            initial_session: None,
        }
    }

    /// Creates a named session when storage is empty.
    #[must_use]
    pub fn with_initial_session(mut self, name: impl Into<String>) -> Self {
        self.initial_session = Some(name.into());
        self
    }
}

/// Reusable resident server orchestration for one application.
pub struct ResidentHost<A, R, T>
where
    A: ResidentApplication,
    R: Clock + Spawner,
    T: Transport,
{
    application: A,
    runtime: R,
    transport: T,
    config: ResidentHostConfig,
}

impl<A, R, T> ResidentHost<A, R, T>
where
    A: ResidentApplication,
    R: Clock + Spawner,
    T: Transport,
{
    /// Creates a resident application host.
    #[must_use]
    pub fn new(application: A, runtime: R, transport: T, config: ResidentHostConfig) -> Self {
        Self {
            application,
            runtime,
            transport,
            config,
        }
    }

    /// Binds the endpoint, acquires lifetime leadership, and serves until shutdown.
    pub async fn serve(self) -> io::Result<()> {
        prepare_private_directory(&self.config.state_dir)?;
        let mut listener = self.transport.bind(&self.config.endpoint).await?;
        set_private_socket_permissions(&self.config.endpoint)?;

        let mut leader_lock = LeaderLock::for_socket(&self.config.endpoint);
        leader_lock.acquire().map_err(io::Error::other)?;
        leader_lock
            .assume_leadership(std::process::id())
            .map_err(io::Error::other)?;

        let leader = LeaderInstanceId::new();
        let inbound_capacity = self.config.outbound_capacity.saturating_mul(4).max(64);
        let (incoming_tx, incoming_rx) = async_channel::bounded(inbound_capacity);
        let next_connection = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1));

        let accept_runtime = self.runtime.clone();
        let connection_runtime = self.runtime.clone();
        let accept_sender = incoming_tx.clone();
        let accept_counter = std::sync::Arc::clone(&next_connection);
        let binary_version = self.config.binary_version.clone();
        let outbound_capacity = self.config.outbound_capacity;
        accept_runtime.clone().spawn(async move {
            loop {
                let connection = match listener.accept().await {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = accept_sender.send(Incoming::ListenerFailed(error)).await;
                        return;
                    }
                };
                let id =
                    ConnectionId(accept_counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
                let sender = accept_sender.clone();
                let runtime = connection_runtime.clone();
                let version = binary_version.clone();
                runtime.clone().spawn(serve_connection(
                    runtime,
                    connection,
                    id,
                    leader,
                    version,
                    outbound_capacity,
                    sender,
                ));
            }
        });

        let tick_runtime = self.runtime.clone();
        let tick_sender = incoming_tx;
        let tick_rate = self.config.tick_rate.max(Duration::from_millis(1));
        self.runtime.clone().spawn(async move {
            loop {
                tick_runtime.sleep(tick_rate).await;
                if tick_sender.send(Incoming::Tick(tick_rate)).await.is_err() {
                    return;
                }
            }
        });

        let mut state = HostState::load(self.application, self.config, leader, incoming_rx)?;
        state.run().await
    }
}

enum Incoming {
    Connected {
        connection: ConnectionId,
        client: ClientInstanceId,
        outbound: async_channel::Sender<ServerMessage>,
    },
    Message {
        connection: ConnectionId,
        envelope: ClientEnvelope,
    },
    Disconnected(ConnectionId),
    Tick(Duration),
    ListenerFailed(io::Error),
}

struct HostedSession<A: ResidentApplication> {
    session: A::Session,
    events: VecDeque<(EventSequence, A::Event)>,
}

#[derive(Deserialize, Serialize)]
struct StoredSession {
    #[serde(default)]
    host_version: u32,
    #[serde(default)]
    application_version: u32,
    control: SessionControlSnapshot,
    state: serde_json::Value,
}

#[derive(Deserialize, Serialize)]
struct StoredManifest {
    host_version: u32,
    application_version: u32,
    id: SessionId,
    name: String,
}

#[derive(Deserialize)]
struct LegacyStoredSession {
    control: SessionControlSnapshot,
    state: serde_json::Value,
}

#[derive(Deserialize)]
struct LegacyRootCheckpoint {
    sessions: Vec<LegacyStoredSession>,
}

struct HostState<A: ResidentApplication> {
    application: A,
    config: ResidentHostConfig,
    leader: LeaderInstanceId,
    core: LeaderCore,
    sessions: HashMap<SessionId, HostedSession<A>>,
    clients: HashMap<ConnectionId, async_channel::Sender<ServerMessage>>,
    incoming: async_channel::Receiver<Incoming>,
}

impl<A: ResidentApplication> HostState<A> {
    fn load(
        application: A,
        config: ResidentHostConfig,
        leader: LeaderInstanceId,
        incoming: async_channel::Receiver<Incoming>,
    ) -> io::Result<Self> {
        let mut core = LeaderCore::new();
        let mut sessions = HashMap::new();
        for entry in fs::read_dir(&config.state_dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() || path.is_symlink() {
                continue;
            }
            prepare_private_directory(&path)?;
            let checkpoint_path = path.join("checkpoint.json");
            let journal = FileJournal::new(path.join("journal.log"), config.durability);
            let records = journal.load().map_err(io::Error::other)?;
            let checkpoint =
                FileJournal::<A::Event>::read_checkpoint::<StoredSession>(&checkpoint_path);
            let (id, mut session, checkpoint_sequence) = match checkpoint {
                Ok(Some(stored)) => {
                    require_host_version(stored.host_version, &checkpoint_path)?;
                    let state = application
                        .migrate(stored.application_version, stored.state)
                        .map_err(io::Error::other)?;
                    let session = application.restore(state).map_err(io::Error::other)?;
                    let id = stored.control.id;
                    let checkpoint_sequence = stored.control.sequence;
                    core.restore_session(stored.control)
                        .map_err(io::Error::other)?;
                    (id, session, checkpoint_sequence)
                }
                Ok(None) if records.is_empty() => continue,
                Ok(None) | Err(_) => {
                    let manifest_path = path.join("manifest.json");
                    let manifest =
                        FileJournal::<A::Event>::read_checkpoint::<StoredManifest>(&manifest_path)
                            .map_err(io::Error::other)?
                            .ok_or_else(|| {
                                io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    format!(
                                        "cannot recover {} without a valid manifest",
                                        checkpoint_path.display()
                                    ),
                                )
                            })?;
                    require_host_version(manifest.host_version, &manifest_path)?;
                    if manifest.application_version != A::STORAGE_VERSION {
                        return Err(io::Error::new(
                            io::ErrorKind::Unsupported,
                            format!(
                                "cannot replay application journal version {}; this binary requires {}",
                                manifest.application_version,
                                A::STORAGE_VERSION
                            ),
                        ));
                    }
                    let session = application
                        .create(&manifest.name)
                        .map_err(io::Error::other)?;
                    core.create_session(manifest.id, manifest.name)
                        .map_err(io::Error::other)?;
                    (manifest.id, session, EventSequence(0))
                }
            };
            let mut events = VecDeque::new();
            for record in records {
                if record.sequence > checkpoint_sequence {
                    session.replay(&record.event).map_err(io::Error::other)?;
                    core.replay(id, record.sequence, record.request)
                        .map_err(io::Error::other)?;
                }
                events.push_back((record.sequence, record.event));
                while events.len() > config.event_history {
                    events.pop_front();
                }
            }
            sessions.insert(id, HostedSession { session, events });
        }

        let legacy_root = config.state_dir.join("checkpoint.json");
        if let Ok(Some(legacy)) =
            FileJournal::<A::Event>::read_checkpoint::<LegacyRootCheckpoint>(&legacy_root)
        {
            for stored in legacy.sessions {
                let id = stored.control.id;
                if sessions.contains_key(&id) {
                    continue;
                }
                let state = application
                    .migrate(0, stored.state)
                    .map_err(io::Error::other)?;
                let session = application.restore(state).map_err(io::Error::other)?;
                core.restore_session(stored.control)
                    .map_err(io::Error::other)?;
                sessions.insert(
                    id,
                    HostedSession {
                        session,
                        events: VecDeque::new(),
                    },
                );
            }
        }

        if sessions.is_empty()
            && let Some(name) = config.initial_session.as_deref()
        {
            let id = SessionId::new();
            let session = application.create(name).map_err(io::Error::other)?;
            core.create_session(id, name).map_err(io::Error::other)?;
            sessions.insert(
                id,
                HostedSession {
                    session,
                    events: VecDeque::new(),
                },
            );
        }

        let state = Self {
            application,
            config,
            leader,
            core,
            sessions,
            clients: HashMap::new(),
            incoming,
        };
        for id in state.sessions.keys().copied() {
            state.persist_manifest(id)?;
            state.persist_checkpoint(id)?;
        }
        Ok(state)
    }

    async fn run(&mut self) -> io::Result<()> {
        while let Ok(incoming) = self.incoming.recv().await {
            match incoming {
                Incoming::Connected {
                    connection,
                    client,
                    outbound,
                } => {
                    self.core.connect(connection, client);
                    self.clients.insert(connection, outbound);
                }
                Incoming::Disconnected(connection) => self.disconnect(connection),
                Incoming::Message {
                    connection,
                    envelope,
                } => {
                    if let Some(reason) = self.handle_request(connection, envelope)? {
                        self.shutdown(reason).await;
                        return Ok(());
                    }
                }
                Incoming::Tick(elapsed) => self.poll(elapsed)?,
                Incoming::ListenerFailed(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "resident host mailbox closed",
        ))
    }

    fn handle_request(
        &mut self,
        connection: ConnectionId,
        envelope: ClientEnvelope,
    ) -> io::Result<Option<ShutdownReason>> {
        let request = envelope.request;
        let mut shutdown = None;
        let result = (|| -> Result<ControlResult, WireError> {
            match envelope.message {
                ClientRequest::Status => Ok(ControlResult::Status {
                    pid: std::process::id(),
                    leader: self.leader,
                    sessions: self.core.sessions(),
                }),
                ClientRequest::ListSessions => Ok(ControlResult::Sessions {
                    sessions: self.core.sessions(),
                }),
                ClientRequest::CreateSession { name } => self.create_session(name),
                ClientRequest::RenameSession { session, name } => {
                    let summary = self
                        .core
                        .rename_session(session, name)
                        .map_err(wire_state)?;
                    self.persist_checkpoint(session).map_err(wire_io)?;
                    Ok(ControlResult::Renamed { session: summary })
                }
                ClientRequest::Attach {
                    session,
                    mode,
                    after,
                    force,
                } => self.attach(connection, session, mode, after, force),
                ClientRequest::Command {
                    session,
                    lease,
                    command,
                } => self.command(connection, request, session, lease, command),
                ClientRequest::Detach { session } => {
                    let change = self.core.detach(connection, session).map_err(wire_state)?;
                    if let Some(change) = change {
                        self.broadcast_driver(change);
                    }
                    Ok(ControlResult::Detached { session })
                }
                ClientRequest::AcquireDriver { session, force } => {
                    let outcome = self
                        .core
                        .attach(connection, session, AttachmentMode::Drive, force)
                        .map_err(wire_state)?;
                    if let Some(change) = outcome.driver_change {
                        self.broadcast_driver(change);
                    }
                    Ok(ControlResult::Driver {
                        session,
                        lease: outcome.session.driver,
                    })
                }
                ClientRequest::ReleaseDriver { session } => {
                    if let Some(change) = self
                        .core
                        .release_driver(connection, session)
                        .map_err(wire_state)?
                    {
                        self.broadcast_driver(change);
                    }
                    Ok(ControlResult::Driver {
                        session,
                        lease: None,
                    })
                }
                ClientRequest::StopSession { session } => {
                    self.stop_session(session).map_err(wire_io)?;
                    Ok(ControlResult::Stopping)
                }
                ClientRequest::Ping => Ok(ControlResult::Pong),
                ClientRequest::StopLeader => {
                    shutdown = Some(ShutdownReason::Manual);
                    Ok(ControlResult::Stopping)
                }
                ClientRequest::ReplaceLeader { binary_version } => {
                    if replacement_is_newer(&self.config.binary_version, &binary_version) {
                        shutdown = Some(ShutdownReason::Upgrade);
                        Ok(ControlResult::Stopping)
                    } else {
                        Err(WireError {
                            code: "replacement_not_newer".to_owned(),
                            message: format!(
                                "leader {} will not be replaced by {binary_version}",
                                self.config.binary_version
                            ),
                        })
                    }
                }
            }
        })();
        self.send(connection, ServerMessage::Response { request, result });
        Ok(shutdown)
    }

    fn create_session(&mut self, name: String) -> Result<ControlResult, WireError> {
        let id = SessionId::new();
        let session = self.application.create(&name).map_err(wire_application)?;
        let summary = self.core.create_session(id, name).map_err(wire_state)?;
        self.sessions.insert(
            id,
            HostedSession {
                session,
                events: VecDeque::new(),
            },
        );
        self.persist_checkpoint(id).map_err(wire_io)?;
        Ok(ControlResult::Created { session: summary })
    }

    fn attach(
        &mut self,
        connection: ConnectionId,
        selector: SessionSelector,
        mode: AttachmentMode,
        after: Option<EventSequence>,
        force: bool,
    ) -> Result<ControlResult, WireError> {
        let id = self.resolve(selector)?;
        let outcome = self
            .core
            .attach(connection, id, mode, force)
            .map_err(wire_state)?;
        if let Some(change) = outcome.driver_change {
            self.broadcast_driver(change);
        }
        self.replay_or_snapshot(connection, id, after)?;
        Ok(ControlResult::Attached {
            session: outcome.session,
            lease: outcome.lease,
        })
    }

    fn command(
        &mut self,
        connection: ConnectionId,
        request: RequestId,
        session: SessionId,
        lease: super::LeaseEpoch,
        value: serde_json::Value,
    ) -> Result<ControlResult, WireError> {
        match self
            .core
            .authorize(connection, session, lease, request)
            .map_err(wire_state)?
        {
            Authorization::Duplicate(sequence) => {
                return Ok(ControlResult::Accepted {
                    session,
                    sequence,
                    duplicate: true,
                });
            }
            Authorization::Apply => {}
        }
        let command: A::Command = serde_json::from_value(value).map_err(|error| WireError {
            code: "invalid_command".to_owned(),
            message: error.to_string(),
        })?;
        let transition = self
            .sessions
            .get_mut(&session)
            .ok_or_else(unknown_session)?
            .session
            .command(command)
            .map_err(wire_application)?;
        let sequence = self
            .apply_transition(session, Some(request), transition)
            .map_err(wire_io)?;
        Ok(ControlResult::Accepted {
            session,
            sequence,
            duplicate: false,
        })
    }

    fn apply_transition(
        &mut self,
        session: SessionId,
        request: Option<RequestId>,
        transition: SessionTransition<A::Event, A::Effect>,
    ) -> io::Result<EventSequence> {
        enum Work<E, F> {
            Transition(Option<RequestId>, SessionTransition<E, F>),
            Effect(F),
        }

        let mut work = VecDeque::from([Work::Transition(request, transition)]);
        let mut request_sequence = None;
        let mut last_sequence = None;

        while let Some(item) = work.pop_front() {
            match item {
                Work::Transition(transition_request, transition) => {
                    let has_events = !transition.events.is_empty();
                    let has_effects = !transition.effects.is_empty();
                    let mut emitted = Vec::new();
                    for (index, event) in transition.events.into_iter().enumerate() {
                        let event_request = (index == 0).then_some(transition_request).flatten();
                        let sequence = match event_request {
                            Some(request) => self.core.commit(session, request),
                            None => self.core.publish(session),
                        }
                        .map_err(io::Error::other)?;
                        if event_request.is_some() {
                            request_sequence.get_or_insert(sequence);
                        }
                        last_sequence = Some(sequence);
                        self.journal(session)
                            .append(&JournalRecord {
                                sequence,
                                request: event_request,
                                event: event.clone(),
                            })
                            .map_err(io::Error::other)?;
                        emitted.push((sequence, event));
                    }

                    if !has_events && let Some(request) = transition_request {
                        let sequence = self
                            .core
                            .commit(session, request)
                            .map_err(io::Error::other)?;
                        request_sequence = Some(sequence);
                        last_sequence = Some(sequence);
                    }

                    // The checkpoint is the durability barrier before effects.
                    if has_events || has_effects || transition_request.is_some() {
                        self.persist_checkpoint(session)?;
                    }
                    for (sequence, event) in emitted {
                        self.record_and_broadcast(session, sequence, event)?;
                    }
                    for effect in transition.effects.into_iter().rev() {
                        work.push_front(Work::Effect(effect));
                    }
                }
                Work::Effect(effect) => {
                    let transition = self
                        .sessions
                        .get_mut(&session)
                        .ok_or_else(|| io::Error::other("unknown resident session"))?
                        .session
                        .effect(effect)
                        .map_err(io::Error::other)?;
                    work.push_front(Work::Transition(None, transition));
                }
            }
        }
        request_sequence.or(last_sequence).ok_or_else(|| {
            io::Error::other("transition did not commit a request or publish an event")
        })
    }

    fn poll(&mut self, elapsed: Duration) -> io::Result<()> {
        let sessions: Vec<_> = self.sessions.keys().copied().collect();
        for id in sessions {
            let transition = self
                .sessions
                .get_mut(&id)
                .expect("session id came from map")
                .session
                .poll(elapsed)
                .map_err(io::Error::other)?;
            if !transition.events.is_empty() || !transition.effects.is_empty() {
                let _ = self.apply_transition(id, None, transition)?;
            }
        }
        Ok(())
    }

    fn record_and_broadcast(
        &mut self,
        session: SessionId,
        sequence: EventSequence,
        event: A::Event,
    ) -> io::Result<()> {
        let hosted = self
            .sessions
            .get_mut(&session)
            .ok_or_else(|| io::Error::other("unknown resident session"))?;
        hosted.events.push_back((sequence, event.clone()));
        while hosted.events.len() > self.config.event_history {
            hosted.events.pop_front();
        }
        let event = serde_json::to_value(event).map_err(io::Error::other)?;
        let subscribers = self.core.subscribers(session).map_err(io::Error::other)?;
        for connection in subscribers {
            self.send(
                connection,
                ServerMessage::Event {
                    session,
                    sequence,
                    event: event.clone(),
                },
            );
        }
        Ok(())
    }

    fn replay_or_snapshot(
        &mut self,
        connection: ConnectionId,
        session: SessionId,
        after: Option<EventSequence>,
    ) -> Result<(), WireError> {
        let hosted = self.sessions.get(&session).ok_or_else(unknown_session)?;
        let summary = self.summary(session).ok_or_else(unknown_session)?;
        let replay = after.and_then(|cursor| {
            let oldest = hosted.events.front().map(|(sequence, _)| *sequence)?;
            (cursor.0.saturating_add(1) >= oldest.0).then(|| {
                hosted
                    .events
                    .iter()
                    .filter(|(sequence, _)| *sequence > cursor)
                    .cloned()
                    .collect::<Vec<_>>()
            })
        });
        if let Some(events) = replay {
            for (sequence, event) in events {
                self.send(
                    connection,
                    ServerMessage::Event {
                        session,
                        sequence,
                        event: serde_json::to_value(event).map_err(wire_io)?,
                    },
                );
            }
        } else {
            let state = serde_json::to_value(hosted.session.snapshot()).map_err(wire_io)?;
            self.send(
                connection,
                ServerMessage::Snapshot {
                    session,
                    sequence: summary.sequence,
                    state,
                },
            );
        }
        Ok(())
    }

    fn persist_checkpoint(&self, session: SessionId) -> io::Result<()> {
        let hosted = self
            .sessions
            .get(&session)
            .ok_or_else(|| io::Error::other("unknown resident session"))?;
        let directory = self.session_directory(session);
        prepare_private_directory(&directory)?;
        self.persist_manifest(session)?;
        let stored = StoredSession {
            host_version: HOST_STORAGE_VERSION,
            application_version: A::STORAGE_VERSION,
            control: self.core.snapshot(session).map_err(io::Error::other)?,
            state: serde_json::to_value(hosted.session.state()).map_err(io::Error::other)?,
        };
        FileJournal::<A::Event>::write_checkpoint(&directory.join("checkpoint.json"), &stored)
            .map_err(io::Error::other)
    }

    fn persist_manifest(&self, session: SessionId) -> io::Result<()> {
        let control = self.core.snapshot(session).map_err(io::Error::other)?;
        let directory = self.session_directory(session);
        prepare_private_directory(&directory)?;
        FileJournal::<A::Event>::write_checkpoint(
            &directory.join("manifest.json"),
            &StoredManifest {
                host_version: HOST_STORAGE_VERSION,
                application_version: A::STORAGE_VERSION,
                id: control.id,
                name: control.name,
            },
        )
        .map_err(io::Error::other)
    }

    fn journal(&self, session: SessionId) -> FileJournal<A::Event> {
        FileJournal::new(
            self.session_directory(session).join("journal.log"),
            self.config.durability,
        )
    }

    fn stop_session(&mut self, session: SessionId) -> io::Result<()> {
        self.sessions
            .remove(&session)
            .ok_or_else(|| io::Error::other("unknown resident session"))?;
        self.core
            .remove_session(session)
            .map_err(io::Error::other)?;
        let directory = self.session_directory(session);
        match fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn resolve(&self, selector: SessionSelector) -> Result<SessionId, WireError> {
        match selector {
            SessionSelector::Id(id) if self.sessions.contains_key(&id) => Ok(id),
            SessionSelector::Name(name) => {
                self.core.session_named(&name).ok_or_else(unknown_session)
            }
            SessionSelector::Id(_) => Err(unknown_session()),
        }
    }

    fn summary(&self, id: SessionId) -> Option<SessionSummary> {
        self.core
            .sessions()
            .into_iter()
            .find(|summary| summary.id == id)
    }

    fn session_directory(&self, session: SessionId) -> PathBuf {
        self.config.state_dir.join(session.to_string())
    }

    fn send(&mut self, connection: ConnectionId, message: ServerMessage) {
        let Some(client) = self.clients.get(&connection) else {
            return;
        };
        if client.try_send(message).is_err() {
            self.disconnect(connection);
        }
    }

    fn disconnect(&mut self, connection: ConnectionId) {
        self.clients.remove(&connection);
        for change in self.core.disconnect(connection) {
            self.broadcast_driver(change);
        }
    }

    fn broadcast_driver(&mut self, change: DriverChange) {
        for connection in change.subscribers {
            self.send(
                connection,
                ServerMessage::DriverChanged {
                    session: change.session,
                    lease: change.lease,
                },
            );
        }
    }

    async fn shutdown(&self, reason: ShutdownReason) {
        let clients: Vec<_> = self.clients.values().cloned().collect();
        for client in &clients {
            let _ = client.send(ServerMessage::ShuttingDown { reason }).await;
        }
        for client in &clients {
            let _ = client.send(ServerMessage::Shutdown { reason }).await;
        }
    }
}

async fn serve_connection<R, C>(
    runtime: R,
    connection: C,
    id: ConnectionId,
    leader: LeaderInstanceId,
    binary_version: String,
    outbound_capacity: usize,
    incoming: async_channel::Sender<Incoming>,
) where
    R: Spawner,
    C: super::runtime::Connection,
{
    let (mut reader, mut writer) = connection.split();
    let hello: ClientHello = match receive_json(&mut reader).await {
        Ok(hello) => hello,
        Err(_) => return,
    };
    let Some(protocol) = VersionRange::current().negotiate(hello.protocol) else {
        let rejection = ServerHandshake::Rejected(ProtocolRejection {
            rejected: true,
            supported: VersionRange::current(),
            binary_version,
            message: format!(
                "resident protocol {}..={} is incompatible with client protocol {}..={}",
                VersionRange::current().minimum.0,
                VersionRange::current().maximum.0,
                hello.protocol.minimum.0,
                hello.protocol.maximum.0
            ),
        });
        if let Ok(frame) = encode_frame(&rejection) {
            let _ = writer.send(frame).await;
        }
        return;
    };
    let response = ServerHello {
        protocol,
        binary_version,
        leader_instance: leader,
        capabilities: LeaderCapabilities {
            named_sessions: true,
            durable_sessions: true,
            shared_sessions: true,
        },
    };
    if writer
        .send(match encode_frame(&ServerHandshake::Accepted(response)) {
            Ok(frame) => frame,
            Err(_) => return,
        })
        .await
        .is_err()
    {
        return;
    }

    let (outbound_tx, outbound_rx) = async_channel::bounded(outbound_capacity.max(1));
    if incoming
        .send(Incoming::Connected {
            connection: id,
            client: hello.client_instance,
            outbound: outbound_tx,
        })
        .await
        .is_err()
    {
        return;
    }

    let disconnected = incoming.clone();
    runtime.spawn(async move {
        while let Ok(message) = outbound_rx.recv().await {
            let frame = match encode_frame(&message) {
                Ok(frame) => frame,
                Err(_) => break,
            };
            if writer.send(frame).await.is_err() {
                break;
            }
        }
        let _ = disconnected.send(Incoming::Disconnected(id)).await;
    });

    while let Ok(envelope) = receive_json::<ClientEnvelope, _>(&mut reader).await {
        if incoming
            .send(Incoming::Message {
                connection: id,
                envelope,
            })
            .await
            .is_err()
        {
            return;
        }
    }
    let _ = incoming.send(Incoming::Disconnected(id)).await;
}

async fn receive_json<T, R>(reader: &mut R) -> io::Result<T>
where
    T: DeserializeOwned,
    R: super::runtime::FrameReader,
{
    let payload = reader.receive().await?;
    serde_json::from_slice(&payload).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid resident JSON: {error}"),
        )
    })
}

fn prepare_private_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "refusing symlinked resident state directory: {}",
                path.display()
            ),
        )),
        Ok(metadata) if metadata.is_dir() => set_directory_permissions(path),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("resident state path is not a directory: {}", path.display()),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
            set_directory_permissions(path)
        }
        Err(error) => Err(error),
    }
}

fn require_host_version(version: u32, path: &Path) -> io::Result<()> {
    if matches!(version, 0 | HOST_STORAGE_VERSION) {
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        format!(
            "resident storage {} uses host version {version}; this binary requires {HOST_STORAGE_VERSION}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
fn set_directory_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn set_directory_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private_socket_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn unknown_session() -> WireError {
    WireError {
        code: "unknown_session".to_owned(),
        message: "unknown resident session".to_owned(),
    }
}

fn wire_state(error: impl std::fmt::Display) -> WireError {
    WireError {
        code: "resident_state".to_owned(),
        message: error.to_string(),
    }
}

fn wire_application(error: ApplicationError) -> WireError {
    WireError {
        code: error.code().to_owned(),
        message: error.message().to_owned(),
    }
}

fn wire_io(error: impl std::fmt::Display) -> WireError {
    WireError {
        code: "resident_io".to_owned(),
        message: error.to_string(),
    }
}

fn replacement_is_newer(current: &str, requested: &str) -> bool {
    match (
        semver::Version::parse(current),
        semver::Version::parse(requested),
    ) {
        (Ok(current), Ok(requested)) => requested > current,
        _ => requested > current,
    }
}
