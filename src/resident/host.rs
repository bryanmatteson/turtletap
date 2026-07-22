use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use super::{
    ApplicationError, AttachmentMode, Authorization, ClientEnvelope, ClientHello, ClientInstanceId,
    ClientRequest, ConnectionId, ControlResult, DriverChange, Durability, EffectCancellation,
    EffectContext, EffectDelivery, EffectId, EffectRequest, EventSequence, FileJournal,
    JournalRecord, LeaderCapabilities, LeaderCore, LeaderInstanceId, LeaderLock, ProtocolRejection,
    RequestId, ResidentApplication, ResidentSession, ServerHandshake, ServerHello, ServerMessage,
    SessionControlSnapshot, SessionId, SessionSelector, SessionSummary, SessionTransition,
    ShutdownReason, VersionRange, WireError, encode_frame,
    runtime::{Clock, FrameWriter as _, Listener as _, Spawner, Transport},
};

const HOST_STORAGE_VERSION: u32 = 2;

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
    /// Default deadline for an effect without an explicit timeout.
    pub effect_timeout: Option<Duration>,
    /// Maximum effects executing concurrently across all sessions.
    pub max_concurrent_effects: usize,
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
            effect_timeout: Some(Duration::from_secs(30 * 60)),
            max_concurrent_effects: 32,
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
        let (incoming_tx, incoming_rx): (
            IncomingSender<A::EffectOutput>,
            IncomingReceiver<A::EffectOutput>,
        ) = async_channel::bounded(inbound_capacity);
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
        let tick_sender = incoming_tx.clone();
        let tick_rate = self.config.tick_rate.max(Duration::from_millis(1));
        self.runtime.clone().spawn(async move {
            loop {
                tick_runtime.sleep(tick_rate).await;
                if tick_sender.send(Incoming::Tick(tick_rate)).await.is_err() {
                    return;
                }
            }
        });

        let mut state = HostState::load(
            self.application,
            self.runtime,
            self.config,
            leader,
            incoming_tx,
            incoming_rx,
        )?;
        state.dispatch_recovered_effects()?;
        state.run().await
    }
}

enum Incoming<O> {
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
    EffectCompleted {
        session: SessionId,
        effect: EffectId,
        output: Result<O, ApplicationError>,
    },
    EffectTimedOut {
        session: SessionId,
        effect: EffectId,
    },
    Tick(Duration),
    ListenerFailed(io::Error),
}

type IncomingSender<O> = async_channel::Sender<Incoming<O>>;
type IncomingReceiver<O> = async_channel::Receiver<Incoming<O>>;

struct HostedSession<A: ResidentApplication> {
    session: A::Session,
    events: VecDeque<(EventSequence, A::Event)>,
    pending_effects: VecDeque<PendingEffect<A::Effect>>,
    active_effect: Option<ActiveEffect>,
    log_sequence: EventSequence,
}

#[derive(Clone)]
struct PendingEffect<F> {
    id: EffectId,
    delivery: EffectDelivery,
    attempts: u32,
    effect: F,
    timeout: Option<Duration>,
}

struct ActiveEffect {
    id: EffectId,
    cancellation: EffectCancellation,
}

#[derive(Clone, Deserialize, Serialize)]
struct StoredEffect {
    id: EffectId,
    delivery: EffectDelivery,
    attempts: u32,
    effect: serde_json::Value,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Clone, Deserialize, Serialize)]
struct SequencedEvent<E> {
    sequence: EventSequence,
    event: E,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum HostLogEvent<E> {
    Transition {
        request: Option<RequestId>,
        control_sequence: EventSequence,
        events: Vec<SequencedEvent<E>>,
        effects: Vec<StoredEffect>,
        completed_effect: Option<EffectId>,
    },
    EffectAttempted {
        effect: EffectId,
        attempt: u32,
    },
}

#[derive(Deserialize, Serialize)]
struct StoredSession {
    #[serde(default)]
    host_version: u32,
    #[serde(default)]
    application_version: u32,
    control: SessionControlSnapshot,
    state: serde_json::Value,
    #[serde(default)]
    log_sequence: EventSequence,
    #[serde(default)]
    pending_effects: Vec<StoredEffect>,
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

struct HostState<A: ResidentApplication, R: Clock + Spawner> {
    application: A,
    runtime: R,
    config: ResidentHostConfig,
    leader: LeaderInstanceId,
    core: LeaderCore,
    sessions: HashMap<SessionId, HostedSession<A>>,
    clients: HashMap<ConnectionId, async_channel::Sender<ServerMessage>>,
    incoming_tx: IncomingSender<A::EffectOutput>,
    incoming: IncomingReceiver<A::EffectOutput>,
    ready_effects: VecDeque<SessionId>,
    ready_effect_set: HashSet<SessionId>,
}

impl<A, R> HostState<A, R>
where
    A: ResidentApplication,
    R: Clock + Spawner,
{
    fn load(
        application: A,
        runtime: R,
        config: ResidentHostConfig,
        leader: LeaderInstanceId,
        incoming_tx: IncomingSender<A::EffectOutput>,
        incoming: IncomingReceiver<A::EffectOutput>,
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
            let checkpoint =
                FileJournal::<A::Event>::read_checkpoint::<StoredSession>(&checkpoint_path);
            let (
                id,
                mut session,
                checkpoint_sequence,
                checkpoint_log_sequence,
                host_version,
                mut pending_effects,
            ) = match checkpoint {
                Ok(Some(stored)) => {
                    require_host_version(stored.host_version, &checkpoint_path)?;
                    let host_version = stored.host_version;
                    let state = application
                        .migrate(stored.application_version, stored.state)
                        .map_err(io::Error::other)?;
                    let session = application.restore(state).map_err(io::Error::other)?;
                    let id = stored.control.id;
                    let checkpoint_sequence = stored.control.sequence;
                    let pending_effects = stored
                        .pending_effects
                        .into_iter()
                        .map(decode_effect)
                        .collect::<io::Result<VecDeque<_>>>()?;
                    core.restore_session(stored.control)
                        .map_err(io::Error::other)?;
                    (
                        id,
                        session,
                        checkpoint_sequence,
                        stored.log_sequence,
                        host_version,
                        pending_effects,
                    )
                }
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
                    (
                        manifest.id,
                        session,
                        EventSequence(0),
                        EventSequence(0),
                        manifest.host_version,
                        VecDeque::new(),
                    )
                }
            };
            let mut events = VecDeque::new();
            let mut log_sequence = checkpoint_log_sequence;
            if host_version >= 2 {
                let journal = FileJournal::new(path.join("journal-v2.log"), config.durability);
                for record in journal.load().map_err(io::Error::other)? {
                    log_sequence = log_sequence.max(record.sequence);
                    let replay = record.sequence > checkpoint_log_sequence;
                    match record.event {
                        HostLogEvent::Transition {
                            request,
                            control_sequence,
                            events: transition_events,
                            effects,
                            completed_effect,
                        } => {
                            let has_events = !transition_events.is_empty();
                            for (index, event) in transition_events.into_iter().enumerate() {
                                if replay {
                                    session.replay(&event.event).map_err(io::Error::other)?;
                                    core.replay(
                                        id,
                                        event.sequence,
                                        (index == 0).then_some(request).flatten(),
                                    )
                                    .map_err(io::Error::other)?;
                                }
                                events.push_back((event.sequence, event.event));
                            }
                            if replay {
                                if !has_events && let Some(request) = request {
                                    core.replay(id, control_sequence, Some(request))
                                        .map_err(io::Error::other)?;
                                }
                                if let Some(completed) = completed_effect {
                                    pending_effects.retain(|effect| effect.id != completed);
                                }
                                pending_effects.extend(
                                    effects
                                        .into_iter()
                                        .map(decode_effect)
                                        .collect::<io::Result<Vec<_>>>()?,
                                );
                            }
                        }
                        HostLogEvent::EffectAttempted { effect, attempt } if replay => {
                            if let Some(pending) = pending_effects
                                .iter_mut()
                                .find(|pending| pending.id == effect)
                            {
                                pending.attempts = pending.attempts.max(attempt);
                            }
                        }
                        HostLogEvent::EffectAttempted { .. } => {}
                    }
                    while events.len() > config.event_history {
                        events.pop_front();
                    }
                }
            } else {
                let journal = FileJournal::new(path.join("journal.log"), config.durability);
                for record in journal.load().map_err(io::Error::other)? {
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
            }
            sessions.insert(
                id,
                HostedSession {
                    session,
                    events,
                    pending_effects,
                    active_effect: None,
                    log_sequence,
                },
            );
        }

        let legacy_root = config.state_dir.join("checkpoint.json");
        let mut imported_legacy_root = false;
        if let Ok(Some(legacy)) =
            FileJournal::<A::Event>::read_checkpoint::<LegacyRootCheckpoint>(&legacy_root)
        {
            imported_legacy_root = true;
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
                        pending_effects: VecDeque::new(),
                        active_effect: None,
                        log_sequence: EventSequence(0),
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
                    pending_effects: VecDeque::new(),
                    active_effect: None,
                    log_sequence: EventSequence(0),
                },
            );
        }

        let state = Self {
            application,
            runtime,
            config,
            leader,
            core,
            sessions,
            clients: HashMap::new(),
            incoming_tx,
            incoming,
            ready_effects: VecDeque::new(),
            ready_effect_set: HashSet::new(),
        };
        for id in state.sessions.keys().copied() {
            state.persist_manifest(id)?;
            state.persist_checkpoint(id)?;
        }
        if imported_legacy_root {
            let archive = state
                .config
                .state_dir
                .join(format!("checkpoint.legacy-v0-{}.json", state.leader));
            fs::rename(&legacy_root, archive)?;
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
                Incoming::EffectCompleted {
                    session,
                    effect,
                    output,
                } => self.complete_effect(session, effect, output)?,
                Incoming::EffectTimedOut { session, effect } => {
                    self.timeout_effect(session, effect)?;
                }
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
                pending_effects: VecDeque::new(),
                active_effect: None,
                log_sequence: EventSequence(0),
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
            .apply_transition(session, Some(request), None, transition)
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
        completed_effect: Option<EffectId>,
        transition: SessionTransition<A::Event, A::Effect>,
    ) -> io::Result<EventSequence> {
        let mut request_sequence = None;
        let mut last_sequence = None;
        let mut emitted = Vec::new();
        for (index, event) in transition.events.into_iter().enumerate() {
            let event_request = (index == 0).then_some(request).flatten();
            let sequence = match event_request {
                Some(request) => self.core.commit(session, request),
                None => self.core.publish(session),
            }
            .map_err(io::Error::other)?;
            if event_request.is_some() {
                request_sequence = Some(sequence);
            }
            last_sequence = Some(sequence);
            emitted.push(SequencedEvent { sequence, event });
        }
        if emitted.is_empty()
            && let Some(request) = request
        {
            let sequence = self
                .core
                .commit(session, request)
                .map_err(io::Error::other)?;
            request_sequence = Some(sequence);
            last_sequence = Some(sequence);
        }

        let pending = transition
            .effects
            .into_iter()
            .map(|request| pending_effect(request, self.config.effect_timeout))
            .collect::<Vec<_>>();
        let stored_effects = pending
            .iter()
            .map(encode_effect)
            .collect::<io::Result<Vec<_>>>()?;
        let control_sequence = self
            .summary(session)
            .ok_or_else(|| io::Error::other("unknown resident session"))?
            .sequence;
        self.append_log(
            session,
            HostLogEvent::Transition {
                request,
                control_sequence,
                events: emitted.clone(),
                effects: stored_effects,
                completed_effect,
            },
        )?;
        {
            let hosted = self
                .sessions
                .get_mut(&session)
                .ok_or_else(|| io::Error::other("unknown resident session"))?;
            if let Some(completed) = completed_effect {
                hosted
                    .pending_effects
                    .retain(|effect| effect.id != completed);
                hosted.active_effect = None;
            }
            hosted.pending_effects.extend(pending);
        }
        self.persist_checkpoint(session)?;
        for event in emitted {
            self.record_and_broadcast(session, event.sequence, event.event)?;
        }
        self.schedule_effect(session);
        self.dispatch_effects()?;
        Ok(request_sequence
            .or(last_sequence)
            .unwrap_or(control_sequence))
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
                let _ = self.apply_transition(id, None, None, transition)?;
            }
        }
        Ok(())
    }

    fn complete_effect(
        &mut self,
        session: SessionId,
        effect: EffectId,
        output: Result<A::EffectOutput, ApplicationError>,
    ) -> io::Result<()> {
        let hosted = match self.sessions.get_mut(&session) {
            Some(hosted) => hosted,
            None => return Ok(()),
        };
        if hosted.active_effect.as_ref().map(|active| active.id) != Some(effect)
            || hosted.pending_effects.front().map(|pending| pending.id) != Some(effect)
        {
            return Ok(());
        }
        let transition = hosted
            .session
            .effect_completed(effect, output)
            .map_err(io::Error::other)?;
        let _ = self.apply_transition(session, None, Some(effect), transition)?;
        Ok(())
    }

    fn dispatch_recovered_effects(&mut self) -> io::Result<()> {
        let sessions = self.sessions.keys().copied().collect::<Vec<_>>();
        for session in sessions {
            self.schedule_effect(session);
        }
        self.dispatch_effects()
    }

    fn schedule_effect(&mut self, session: SessionId) {
        let ready = self.sessions.get(&session).is_some_and(|hosted| {
            hosted.active_effect.is_none() && !hosted.pending_effects.is_empty()
        });
        if ready && self.ready_effect_set.insert(session) {
            self.ready_effects.push_back(session);
        }
    }

    fn dispatch_effects(&mut self) -> io::Result<()> {
        let limit = self.config.max_concurrent_effects.max(1);
        while self
            .sessions
            .values()
            .filter(|hosted| hosted.active_effect.is_some())
            .count()
            < limit
        {
            let Some(session) = self.ready_effects.pop_front() else {
                break;
            };
            self.ready_effect_set.remove(&session);
            self.dispatch_one_effect(session)?;
        }
        Ok(())
    }

    fn dispatch_one_effect(&mut self, session: SessionId) -> io::Result<()> {
        let Some(pending) = self.sessions.get(&session).and_then(|hosted| {
            (hosted.active_effect.is_none())
                .then(|| hosted.pending_effects.front().cloned())
                .flatten()
        }) else {
            return Ok(());
        };
        if pending.delivery == EffectDelivery::AtMostOnce && pending.attempts > 0 {
            self.sessions
                .get_mut(&session)
                .expect("session exists")
                .active_effect = Some(ActiveEffect {
                id: pending.id,
                cancellation: EffectCancellation::new(),
            });
            let sender = self.incoming_tx.clone();
            self.runtime.spawn(async move {
                let _ = sender
                    .send(Incoming::EffectCompleted {
                        session,
                        effect: pending.id,
                        output: Err(ApplicationError::new(
                            "effect_outcome_unknown",
                            "leader stopped after an at-most-once effect may have started",
                        )),
                    })
                    .await;
            });
            return Ok(());
        }

        let attempt = pending.attempts.saturating_add(1);
        if let Some(effect) = self
            .sessions
            .get_mut(&session)
            .and_then(|hosted| hosted.pending_effects.front_mut())
        {
            effect.attempts = attempt;
        }
        self.append_log(
            session,
            HostLogEvent::EffectAttempted {
                effect: pending.id,
                attempt,
            },
        )?;
        self.persist_checkpoint(session)?;
        let cancellation = EffectCancellation::new();
        self.sessions
            .get_mut(&session)
            .expect("session exists")
            .active_effect = Some(ActiveEffect {
            id: pending.id,
            cancellation: cancellation.clone(),
        });
        let application = self.application.clone();
        let sender = self.incoming_tx.clone();
        let effect_id = pending.id;
        let effect_cancellation = cancellation.clone();
        self.runtime.spawn(async move {
            let output = application
                .execute(
                    EffectContext {
                        session,
                        effect: effect_id,
                        attempt,
                        cancellation: effect_cancellation,
                    },
                    pending.effect,
                )
                .await;
            let _ = sender
                .send(Incoming::EffectCompleted {
                    session,
                    effect: effect_id,
                    output,
                })
                .await;
        });
        if let Some(timeout) = pending.timeout {
            let timer = self.runtime.clone();
            let sender = self.incoming_tx.clone();
            self.runtime.spawn(async move {
                timer.sleep(timeout).await;
                let _ = sender
                    .send(Incoming::EffectTimedOut {
                        session,
                        effect: effect_id,
                    })
                    .await;
            });
        }
        Ok(())
    }

    fn timeout_effect(&mut self, session: SessionId, effect: EffectId) -> io::Result<()> {
        let Some(active) = self
            .sessions
            .get(&session)
            .and_then(|hosted| hosted.active_effect.as_ref())
            .filter(|active| active.id == effect)
        else {
            return Ok(());
        };
        active.cancellation.cancel();
        self.complete_effect(
            session,
            effect,
            Err(ApplicationError::new(
                "effect_timed_out",
                "effect exceeded its execution deadline",
            )),
        )
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
            log_sequence: hosted.log_sequence,
            pending_effects: hosted
                .pending_effects
                .iter()
                .map(encode_effect)
                .collect::<io::Result<Vec<_>>>()?,
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

    fn journal(&self, session: SessionId) -> FileJournal<HostLogEvent<A::Event>> {
        FileJournal::new(
            self.session_directory(session).join("journal-v2.log"),
            self.config.durability,
        )
    }

    fn append_log(&mut self, session: SessionId, event: HostLogEvent<A::Event>) -> io::Result<()> {
        let sequence = self
            .sessions
            .get(&session)
            .ok_or_else(|| io::Error::other("unknown resident session"))?
            .log_sequence
            .0
            .checked_add(1)
            .map(EventSequence)
            .ok_or_else(|| io::Error::other("session log sequence exhausted"))?;
        self.journal(session)
            .append(&JournalRecord {
                sequence,
                request: None,
                event,
            })
            .map_err(io::Error::other)?;
        self.sessions
            .get_mut(&session)
            .expect("session exists")
            .log_sequence = sequence;
        Ok(())
    }

    fn stop_session(&mut self, session: SessionId) -> io::Result<()> {
        let hosted = self
            .sessions
            .remove(&session)
            .ok_or_else(|| io::Error::other("unknown resident session"))?;
        if let Some(active) = hosted.active_effect {
            active.cancellation.cancel();
        }
        self.ready_effect_set.remove(&session);
        self.ready_effects.retain(|ready| *ready != session);
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

    async fn shutdown(&mut self, reason: ShutdownReason) {
        for hosted in self.sessions.values() {
            if let Some(active) = &hosted.active_effect {
                active.cancellation.cancel();
            }
        }
        let clients: Vec<_> = self.clients.values().cloned().collect();
        for client in &clients {
            let _ = client.send(ServerMessage::ShuttingDown { reason }).await;
        }
        for client in &clients {
            let _ = client.send(ServerMessage::Shutdown { reason }).await;
        }
    }
}

async fn serve_connection<R, C, O>(
    runtime: R,
    connection: C,
    id: ConnectionId,
    leader: LeaderInstanceId,
    binary_version: String,
    outbound_capacity: usize,
    incoming: async_channel::Sender<Incoming<O>>,
) where
    R: Spawner,
    C: super::runtime::Connection,
    O: Send + 'static,
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

fn pending_effect<F>(
    request: EffectRequest<F>,
    default_timeout: Option<Duration>,
) -> PendingEffect<F> {
    PendingEffect {
        id: EffectId::new(),
        delivery: request.delivery,
        attempts: 0,
        effect: request.effect,
        timeout: request.timeout.or(default_timeout),
    }
}

fn encode_effect<F: Serialize>(pending: &PendingEffect<F>) -> io::Result<StoredEffect> {
    Ok(StoredEffect {
        id: pending.id,
        delivery: pending.delivery,
        attempts: pending.attempts,
        effect: serde_json::to_value(&pending.effect).map_err(io::Error::other)?,
        timeout_ms: pending
            .timeout
            .map(|timeout| u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX)),
    })
}

fn decode_effect<F: DeserializeOwned>(stored: StoredEffect) -> io::Result<PendingEffect<F>> {
    Ok(PendingEffect {
        id: stored.id,
        delivery: stored.delivery,
        attempts: stored.attempts,
        effect: serde_json::from_value(stored.effect).map_err(io::Error::other)?,
        timeout: stored.timeout_ms.map(Duration::from_millis),
    })
}

fn require_host_version(version: u32, path: &Path) -> io::Result<()> {
    if matches!(version, 0 | 1 | HOST_STORAGE_VERSION) {
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

#[cfg(test)]
mod tests {
    use std::{
        future,
        sync::{Arc, Mutex},
    };

    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::resident::runtime::tokio::TokioRuntime;

    #[derive(Clone)]
    struct EffectApplication {
        complete: bool,
        attempts: Arc<Mutex<Vec<EffectContext>>>,
    }

    #[derive(Clone, Debug, Deserialize, Serialize)]
    struct TestEffect;

    #[derive(Clone, Debug, Default, Deserialize, Serialize)]
    struct TestState {
        completed: Option<EffectId>,
        attempt: u32,
        error: Option<String>,
    }

    struct TestSession(TestState);

    impl ResidentApplication for EffectApplication {
        type Command = ();
        type Event = TestState;
        type Snapshot = TestState;
        type State = TestState;
        type Effect = TestEffect;
        type EffectOutput = EffectContext;
        type Session = TestSession;

        const STORAGE_VERSION: u32 = 1;

        fn create(&self, _name: &str) -> Result<Self::Session, ApplicationError> {
            Ok(TestSession(TestState::default()))
        }

        fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError> {
            Ok(TestSession(state))
        }

        async fn execute(
            &self,
            context: EffectContext,
            _effect: Self::Effect,
        ) -> Result<Self::EffectOutput, ApplicationError> {
            self.attempts
                .lock()
                .expect("attempt log should be healthy")
                .push(context.clone());
            if !self.complete {
                future::pending::<()>().await;
            }
            Ok(context)
        }
    }

    impl ResidentSession for TestSession {
        type Command = ();
        type Event = TestState;
        type Snapshot = TestState;
        type State = TestState;
        type Effect = TestEffect;
        type EffectOutput = EffectContext;

        fn snapshot(&self) -> Self::Snapshot {
            self.0.clone()
        }

        fn state(&self) -> Self::State {
            self.0.clone()
        }

        fn command(
            &mut self,
            _command: Self::Command,
        ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
            Ok(SessionTransition::new(Vec::new(), vec![TestEffect]))
        }

        fn effect_completed(
            &mut self,
            _effect: EffectId,
            output: Result<Self::EffectOutput, ApplicationError>,
        ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
            match output {
                Ok(context) => {
                    self.0.completed = Some(context.effect);
                    self.0.attempt = context.attempt;
                }
                Err(error) => {
                    self.0.completed = Some(_effect);
                    self.0.error = Some(error.code().to_owned());
                }
            }
            Ok(SessionTransition::events([self.0.clone()]))
        }

        fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError> {
            self.0.clone_from(event);
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unfinished_effect_is_redriven_without_blocking_the_actor() {
        let directory = std::env::temp_dir().join(format!(
            "turtletap-effect-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let endpoint = directory.join("resident.sock");
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let config =
            ResidentHostConfig::new(&endpoint, &directory, "test").with_initial_session("effect");
        let (sender, receiver) = async_channel::bounded(16);
        let mut first = HostState::load(
            EffectApplication {
                complete: false,
                attempts: Arc::clone(&attempts),
            },
            TokioRuntime,
            config.clone(),
            LeaderInstanceId::new(),
            sender,
            receiver,
        )
        .expect("first host should load");
        let session = *first.sessions.keys().next().expect("initial session");
        let transition = first
            .sessions
            .get_mut(&session)
            .expect("initial session")
            .session
            .command(())
            .expect("command should reduce");
        let _ = first
            .apply_transition(session, None, None, transition)
            .expect("effect should be durably queued");
        let effect = first
            .sessions
            .get(&session)
            .and_then(|hosted| hosted.pending_effects.front())
            .map(|pending| pending.id)
            .expect("pending effect");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !attempts
                    .lock()
                    .expect("attempt log should be healthy")
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first effect attempt should start");
        assert_eq!(
            attempts.lock().expect("attempt log should be healthy")[0].attempt,
            1
        );
        drop(first);

        let (sender, receiver) = async_channel::bounded(16);
        let mut recovered = HostState::load(
            EffectApplication {
                complete: true,
                attempts: Arc::clone(&attempts),
            },
            TokioRuntime,
            config,
            LeaderInstanceId::new(),
            sender,
            receiver,
        )
        .expect("recovered host should load");
        recovered
            .dispatch_recovered_effects()
            .expect("recovered effect should dispatch");
        let incoming = tokio::time::timeout(Duration::from_secs(1), recovered.incoming.recv())
            .await
            .expect("effect should complete")
            .expect("host mailbox should remain open");
        let Incoming::EffectCompleted {
            session,
            effect: completed,
            output,
        } = incoming
        else {
            panic!("expected effect completion");
        };
        recovered
            .complete_effect(session, completed, output)
            .expect("completion should reduce");
        let state = recovered
            .sessions
            .get(&session)
            .expect("recovered session")
            .session
            .snapshot();
        assert_eq!(state.completed, Some(effect));
        assert_eq!(state.attempt, 2);
        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn effect_timeout_cancels_and_reduces_an_error() {
        let directory = std::env::temp_dir().join(format!(
            "turtletap-timeout-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let mut config =
            ResidentHostConfig::new(directory.join("resident.sock"), &directory, "test")
                .with_initial_session("effect");
        config.effect_timeout = Some(Duration::from_millis(10));
        let (sender, receiver) = async_channel::bounded(16);
        let mut state = HostState::load(
            EffectApplication {
                complete: false,
                attempts,
            },
            TokioRuntime,
            config,
            LeaderInstanceId::new(),
            sender,
            receiver,
        )
        .expect("host should load");
        let session = *state.sessions.keys().next().expect("initial session");
        let transition = state
            .sessions
            .get_mut(&session)
            .expect("initial session")
            .session
            .command(())
            .expect("command should reduce");
        let _ = state
            .apply_transition(session, None, None, transition)
            .expect("effect should queue");
        let cancellation = state
            .sessions
            .get(&session)
            .and_then(|hosted| hosted.active_effect.as_ref())
            .map(|active| active.cancellation.clone())
            .expect("active effect cancellation");
        let incoming = tokio::time::timeout(Duration::from_secs(1), state.incoming.recv())
            .await
            .expect("timeout should fire")
            .expect("mailbox should remain open");
        let Incoming::EffectTimedOut { session, effect } = incoming else {
            panic!("expected effect timeout");
        };
        state
            .timeout_effect(session, effect)
            .expect("timeout should reduce");
        assert!(cancellation.is_cancelled());
        let hosted = state.sessions.get(&session).expect("session remains");
        assert!(hosted.active_effect.is_none());
        assert!(hosted.pending_effects.is_empty());
        assert_eq!(
            hosted.session.snapshot().error.as_deref(),
            Some("effect_timed_out")
        );
        fs::remove_dir_all(directory).expect("test directory should be removed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn global_effect_limit_advances_waiting_sessions() {
        let directory = std::env::temp_dir().join(format!(
            "turtletap-effect-limit-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&directory).expect("test directory should be created");
        let attempts = Arc::new(Mutex::new(Vec::new()));
        let mut config =
            ResidentHostConfig::new(directory.join("resident.sock"), &directory, "test")
                .with_initial_session("first");
        config.effect_timeout = None;
        config.max_concurrent_effects = 1;
        let (sender, receiver) = async_channel::bounded(16);
        let mut state = HostState::load(
            EffectApplication {
                complete: false,
                attempts: Arc::clone(&attempts),
            },
            TokioRuntime,
            config,
            LeaderInstanceId::new(),
            sender,
            receiver,
        )
        .expect("host should load");
        let first = state.core.session_named("first").expect("first session");
        state
            .create_session("second".to_owned())
            .expect("second session should be created");
        let second = state.core.session_named("second").expect("second session");

        for session in [first, second] {
            let transition = state
                .sessions
                .get_mut(&session)
                .expect("session should exist")
                .session
                .command(())
                .expect("command should reduce");
            let _ = state
                .apply_transition(session, None, None, transition)
                .expect("effect should queue");
        }
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if attempts
                    .lock()
                    .expect("attempt log should be healthy")
                    .len()
                    == 1
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("first effect should start");
        assert!(
            state
                .sessions
                .get(&first)
                .expect("first session")
                .active_effect
                .is_some()
        );
        assert!(
            state
                .sessions
                .get(&second)
                .expect("second session")
                .active_effect
                .is_none()
        );

        let first_context = attempts.lock().expect("attempt log should be healthy")[0].clone();
        first_context.cancellation.cancel();
        state
            .complete_effect(first, first_context.effect, Ok(first_context.clone()))
            .expect("first effect should complete");
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if attempts
                    .lock()
                    .expect("attempt log should be healthy")
                    .len()
                    == 2
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("waiting effect should start");
        assert!(
            state
                .sessions
                .get(&first)
                .expect("first session")
                .active_effect
                .is_none()
        );
        let second_active = state
            .sessions
            .get(&second)
            .expect("second session")
            .active_effect
            .as_ref()
            .expect("second effect should be active");
        second_active.cancellation.cancel();
        fs::remove_dir_all(directory).expect("test directory should be removed");
    }
}
