//! Durable multi-client resident sessions for the standalone command shell.

#[cfg(not(unix))]
use std::{io, path::PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OutputFormat {
    Human,
    Json,
}

#[cfg(unix)]
mod unix {
    use std::{
        collections::{VecDeque, hash_map::DefaultHasher},
        env, fs,
        hash::{Hash, Hasher},
        io::{self, Read, Write},
        os::unix::{ffi::OsStrExt, fs::PermissionsExt, net::UnixStream, process::CommandExt},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
        thread,
        time::{Duration, Instant},
    };

    use crossterm::event::MouseEventKind;
    use serde::{Deserialize, Serialize, de::DeserializeOwned};
    use turtletap::{
        Frame, InputPolicy, KeyCode, KeyModifiers, Rect, Shell, Shortcut, Surface, SurfaceAction,
        SurfaceEvent, SurfaceStatus,
        resident::{
            ApplicationError, AttachmentMode, ClientCapabilities, ClientEnvelope, ClientHello,
            ClientInstanceId, ClientRequest, ControlResult, Durability, EffectContext, EffectId,
            EffectRequest, EventSequence, LeaderLock, LeaseEpoch, MAX_FRAME_SIZE, PROTOCOL_VERSION,
            RequestId, ResidentApplication, ResidentHost, ResidentHostConfig, ResidentSession,
            ServerHandshake, ServerMessage, SessionId, SessionSelector, SessionSummary,
            SessionTransition, ShutdownReason, VersionRange, WireError, encode_frame,
            runtime::tokio::{TokioRuntime, TokioUnixTransport},
        },
        tui::{
            layout::{Constraint, Direction, Layout, Position},
            style::{Color, Modifier, Style},
            text::{Line, Span},
            widgets::Paragraph,
        },
    };

    use super::super::{
        CommandSurface, PersistedCommandSurface, RunningCommand, Scrollback, TranscriptEntry,
        TranscriptKind, char_slice_width, spawn_command, split_command,
    };
    use super::OutputFormat;

    const START_TIMEOUT: Duration = Duration::from_secs(5);
    const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
    const SERVER_TICK: Duration = Duration::from_millis(20);
    const MAX_SOCKET_PATH_BYTES: usize = 100;
    const OUTBOUND_CAPACITY: usize = 256;
    const EVENT_HISTORY: usize = 1_024;
    const DEFAULT_SESSION: &str = "default";

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    pub(crate) struct SessionSnapshot {
        revision: u64,
        transcript: Vec<TranscriptEntry>,
        history: Vec<String>,
        commands: Vec<String>,
        cwd_label: String,
        running: bool,
        last_failed: bool,
        queued: usize,
    }

    impl SessionSnapshot {
        fn from_session(session: &CommandSurface) -> Self {
            Self {
                revision: session.revision,
                transcript: session.transcript.clone(),
                history: session.history.clone(),
                commands: session.commands.keys().cloned().collect(),
                cwd_label: session.prompt_label(),
                running: session.running.is_some(),
                last_failed: session.last_failed,
                queued: session.pending.len(),
            }
        }
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub(crate) enum ShellCommand {
        Submit { line: String },
        Interrupt,
        Clear,
    }

    #[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub(crate) enum ShellEvent {
        Appended {
            revision: u64,
            entries: Vec<TranscriptEntry>,
            running: bool,
            last_failed: bool,
            queued: usize,
            cwd_label: String,
            history: Vec<String>,
            commands: Vec<String>,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            state: Option<PersistedCommandSurface>,
        },
        Reset {
            snapshot: SessionSnapshot,
            #[serde(default, skip_serializing_if = "Option::is_none")]
            state: Option<PersistedCommandSurface>,
        },
    }

    impl ShellEvent {
        fn between(previous: &SessionSnapshot, current: &SessionSnapshot) -> Self {
            let prefix_matches = current.transcript.starts_with(&previous.transcript);
            if prefix_matches {
                Self::Appended {
                    revision: current.revision,
                    entries: current.transcript[previous.transcript.len()..].to_vec(),
                    running: current.running,
                    last_failed: current.last_failed,
                    queued: current.queued,
                    cwd_label: current.cwd_label.clone(),
                    history: current.history.clone(),
                    commands: current.commands.clone(),
                    state: None,
                }
            } else {
                Self::Reset {
                    snapshot: current.clone(),
                    state: None,
                }
            }
        }

        fn from_surface(previous: &SessionSnapshot, surface: &CommandSurface) -> Self {
            let current = SessionSnapshot::from_session(surface);
            let state = surface.persisted();
            match Self::between(previous, &current) {
                Self::Appended {
                    revision,
                    entries,
                    running,
                    last_failed,
                    queued,
                    cwd_label,
                    history,
                    commands,
                    ..
                } => Self::Appended {
                    revision,
                    entries,
                    running,
                    last_failed,
                    queued,
                    cwd_label,
                    history,
                    commands,
                    state: Some(state),
                },
                Self::Reset { snapshot, .. } => Self::Reset {
                    snapshot,
                    state: Some(state),
                },
            }
        }

        fn persisted_state(&self) -> Option<&PersistedCommandSurface> {
            match self {
                Self::Appended { state, .. } | Self::Reset { state, .. } => state.as_ref(),
            }
        }

        fn appended_count(&self, previous: &SessionSnapshot) -> usize {
            match self {
                Self::Appended { entries, .. } => entries.len(),
                Self::Reset { snapshot, .. } => {
                    transcript_append_count(&previous.transcript, &snapshot.transcript)
                }
            }
        }

        fn apply(&self, snapshot: &mut SessionSnapshot) {
            match self {
                Self::Appended {
                    revision,
                    entries,
                    running,
                    last_failed,
                    queued,
                    cwd_label,
                    history,
                    commands,
                    ..
                } => {
                    snapshot.revision = *revision;
                    snapshot.transcript.extend(entries.iter().cloned());
                    let excess = snapshot
                        .transcript
                        .len()
                        .saturating_sub(super::super::MAX_TRANSCRIPT_LINES);
                    if excess > 0 {
                        snapshot.transcript.drain(..excess);
                    }
                    snapshot.running = *running;
                    snapshot.last_failed = *last_failed;
                    snapshot.queued = *queued;
                    snapshot.cwd_label.clone_from(cwd_label);
                    snapshot.history.clone_from(history);
                    snapshot.commands.clone_from(commands);
                }
                Self::Reset {
                    snapshot: current, ..
                } => snapshot.clone_from(current),
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    pub(crate) struct ShellApplication;

    #[derive(Clone, Debug, Deserialize, Serialize)]
    #[serde(tag = "type", rename_all = "snake_case")]
    pub(crate) enum ShellEffect {
        Run { command: String, cwd: PathBuf },
    }

    impl ResidentApplication for ShellApplication {
        type Command = ShellCommand;
        type Event = ShellEvent;
        type Snapshot = SessionSnapshot;
        type State = PersistedCommandSurface;
        type Effect = ShellEffect;
        type EffectOutput = RunningCommand;
        type Session = CommandSurface;

        const STORAGE_VERSION: u32 = 1;

        fn create(&self, _name: &str) -> Result<Self::Session, ApplicationError> {
            CommandSurface::new().map_err(shell_io)
        }

        fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError> {
            Ok(CommandSurface::restore(state))
        }

        async fn execute(
            &self,
            _context: EffectContext,
            effect: Self::Effect,
        ) -> Result<Self::EffectOutput, ApplicationError> {
            match effect {
                ShellEffect::Run { command, cwd } => {
                    spawn_command(&command, &cwd).map_err(shell_io)
                }
            }
        }

        fn migrate(
            &self,
            stored_version: u32,
            state: serde_json::Value,
        ) -> Result<Self::State, ApplicationError> {
            if !matches!(stored_version, 0 | Self::STORAGE_VERSION) {
                return Err(ApplicationError::new(
                    "unsupported_storage_version",
                    format!(
                        "stored shell state is version {stored_version}; this binary requires {}",
                        Self::STORAGE_VERSION
                    ),
                ));
            }
            serde_json::from_value(state)
                .map_err(|error| ApplicationError::new("invalid_checkpoint", error.to_string()))
        }
    }

    impl ResidentSession for CommandSurface {
        type Command = ShellCommand;
        type Event = ShellEvent;
        type Snapshot = SessionSnapshot;
        type State = PersistedCommandSurface;
        type Effect = ShellEffect;
        type EffectOutput = RunningCommand;

        fn snapshot(&self) -> Self::Snapshot {
            SessionSnapshot::from_session(self)
        }

        fn state(&self) -> Self::State {
            self.persisted()
        }

        fn command(
            &mut self,
            command: Self::Command,
        ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
            let previous = SessionSnapshot::from_session(self);
            let effects = match command {
                ShellCommand::Submit { line } => {
                    let _ = self.accept_line(line);
                    resident_effects(self)
                }
                ShellCommand::Interrupt => {
                    let _ = self.interrupt();
                    Vec::new()
                }
                ShellCommand::Clear => {
                    self.transcript.clear();
                    self.scrollback.follow();
                    self.touch();
                    Vec::new()
                }
            };
            Ok(shell_transition(previous, self, effects))
        }

        fn poll(
            &mut self,
            _elapsed: Duration,
        ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
            let previous = SessionSnapshot::from_session(self);
            let _ = self.poll_command_deferred();
            let effects = resident_effects(self);
            Ok(shell_transition(previous, self, effects))
        }

        fn effect_completed(
            &mut self,
            _effect: EffectId,
            output: Result<Self::EffectOutput, ApplicationError>,
        ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
            let previous = SessionSnapshot::from_session(self);
            if self.started_command.is_some() {
                let _ = self.pending.pop_front();
            }
            match output {
                Ok(running) => {
                    self.running = Some(running);
                    self.last_failed = false;
                }
                Err(error)
                    if error.code() == "effect_outcome_unknown"
                        && self.started_command.is_none() => {}
                Err(error) => {
                    self.started_command = None;
                    self.last_failed = true;
                    self.push(
                        TranscriptKind::Error,
                        format!("Could not start command: {error}"),
                    );
                }
            }
            let effects = resident_effects(self);
            Ok(shell_transition(previous, self, effects))
        }

        fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError> {
            let state = event.persisted_state().ok_or_else(|| {
                ApplicationError::new(
                    "legacy_journal_event",
                    "journal event does not contain replayable application state",
                )
            })?;
            *self = CommandSurface::restore(state.clone());
            Ok(())
        }
    }

    fn shell_transition(
        previous: SessionSnapshot,
        surface: &CommandSurface,
        effects: Vec<EffectRequest<ShellEffect>>,
    ) -> SessionTransition<ShellEvent, ShellEffect> {
        let current = SessionSnapshot::from_session(surface);
        let events = (current != previous)
            .then(|| ShellEvent::from_surface(&previous, surface))
            .into_iter()
            .collect();
        SessionTransition::with_effects(events, effects)
    }

    fn resident_effects(surface: &mut CommandSurface) -> Vec<EffectRequest<ShellEffect>> {
        while surface.running.is_none() && surface.started_command.is_none() {
            let Some(line) = surface.pending.front().cloned() else {
                break;
            };
            surface.started_command = Some(line.clone());
            surface.touch();
            if surface.run_builtin(&line).is_some() {
                let _ = surface.pending.pop_front();
                surface.started_command = None;
                surface.touch();
                continue;
            }
            return vec![EffectRequest::at_most_once(ShellEffect::Run {
                command: surface.expand_command(&line),
                cwd: surface.cwd.clone(),
            })];
        }
        Vec::new()
    }

    fn shell_io(error: io::Error) -> ApplicationError {
        ApplicationError::new("shell_io", error.to_string())
    }

    fn transcript_append_count(previous: &[TranscriptEntry], current: &[TranscriptEntry]) -> usize {
        if current.starts_with(previous) {
            return current.len().saturating_sub(previous.len());
        }
        if current.is_empty() {
            return 0;
        }
        for removed in 1..previous.len() {
            let overlap = previous.len() - removed;
            if overlap <= current.len() && previous[removed..] == current[..overlap] {
                return current.len() - overlap;
            }
        }
        current.len()
    }

    fn read_sync<T: DeserializeOwned>(stream: &mut UnixStream) -> io::Result<T> {
        let mut length = [0; 4];
        stream.read_exact(&mut length)?;
        let size = u32::from_be_bytes(length) as usize;
        if size > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("resident frame is too large: {size} bytes"),
            ));
        }
        let mut payload = vec![0; size];
        stream.read_exact(&mut payload)?;
        serde_json::from_slice(&payload).map_err(protocol_error)
    }

    fn write_sync<T: Serialize>(stream: &mut UnixStream, value: &T) -> io::Result<()> {
        let frame = encode_frame(value).map_err(frame_error)?;
        stream.write_all(&frame)?;
        stream.flush()
    }

    fn protocol_error(error: serde_json::Error) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }

    fn frame_error(error: impl std::fmt::Display) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error.to_string())
    }

    struct Attachment {
        selector: SessionSelector,
        session: SessionId,
        mode: AttachmentMode,
        force: bool,
        lease: Option<LeaseEpoch>,
        cursor: EventSequence,
    }

    struct SessionClient {
        path: PathBuf,
        instance: ClientInstanceId,
        next_request: u64,
        writer: UnixStream,
        incoming: Receiver<io::Result<ServerMessage>>,
        leader_version: String,
        pending: VecDeque<ServerMessage>,
        attachment: Option<Attachment>,
    }

    impl SessionClient {
        fn connect(path: &Path) -> io::Result<Self> {
            let instance = ClientInstanceId::new();
            let (writer, incoming, leader_version) = connect_stream(path, instance)?;
            Ok(Self {
                path: path.to_owned(),
                instance,
                next_request: 1,
                writer,
                incoming,
                leader_version,
                pending: VecDeque::new(),
                attachment: None,
            })
        }

        fn request(&mut self, message: ClientRequest) -> io::Result<ControlResult> {
            let request = self.next_id();
            let mut envelope = ClientEnvelope { request, message };
            for attempt in 0..=3 {
                match self.request_once(&envelope) {
                    Ok(result) => return result.map_err(wire_error),
                    Err(error) if attempt < 3 && is_connection_error(&error) => {
                        self.reconnect()?;
                        if let ClientRequest::Command { lease, .. } = &mut envelope.message {
                            *lease = self
                                .attachment
                                .as_ref()
                                .and_then(|attachment| attachment.lease)
                                .ok_or_else(|| {
                                    io::Error::new(
                                        io::ErrorKind::PermissionDenied,
                                        "the reconnected client did not regain the driver lease",
                                    )
                                })?;
                        }
                    }
                    Err(error) => return Err(error),
                }
            }
            Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "resident reconnect attempts were exhausted",
            ))
        }

        fn request_once(
            &mut self,
            envelope: &ClientEnvelope,
        ) -> io::Result<Result<ControlResult, WireError>> {
            write_sync(&mut self.writer, envelope)?;
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.incoming.recv_timeout(remaining) {
                    Ok(Ok(ServerMessage::Response { request, result }))
                        if request == envelope.request =>
                    {
                        return Ok(result);
                    }
                    Ok(Ok(message)) => self.pending.push_back(message),
                    Ok(Err(error)) => return Err(error),
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "resident request timed out",
                        ));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "resident connection closed",
                        ));
                    }
                }
            }
        }

        fn attach(
            &mut self,
            selector: SessionSelector,
            mode: AttachmentMode,
            force: bool,
        ) -> io::Result<(SessionSummary, Option<LeaseEpoch>, SessionSnapshot)> {
            let result = self.request(ClientRequest::Attach {
                session: selector.clone(),
                mode,
                after: None,
                force,
            })?;
            let ControlResult::Attached { session, lease } = result else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resident returned the wrong attach response",
                ));
            };
            let snapshot = self.wait_for_snapshot(session.id)?;
            self.attachment = Some(Attachment {
                selector,
                session: session.id,
                mode,
                force,
                lease,
                cursor: session.sequence,
            });
            Ok((session, lease, snapshot))
        }

        fn wait_for_snapshot(&mut self, session: SessionId) -> io::Result<SessionSnapshot> {
            let deadline = Instant::now() + REQUEST_TIMEOUT;
            loop {
                if let Some(index) = self.pending.iter().position(|message| {
                    matches!(message, ServerMessage::Snapshot { session: id, .. } if *id == session)
                }) {
                    let Some(ServerMessage::Snapshot { state, .. }) = self.pending.remove(index)
                    else {
                        continue;
                    };
                    return serde_json::from_value(state).map_err(protocol_error);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                match self.incoming.recv_timeout(remaining) {
                    Ok(Ok(ServerMessage::Snapshot {
                        session: id, state, ..
                    })) if id == session => {
                        return serde_json::from_value(state).map_err(protocol_error);
                    }
                    Ok(Ok(message)) => self.pending.push_back(message),
                    Ok(Err(error)) => return Err(error),
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "resident attach snapshot timed out",
                        ));
                    }
                    Err(RecvTimeoutError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "resident connection closed",
                        ));
                    }
                }
            }
        }

        fn reconnect(&mut self) -> io::Result<()> {
            ensure_started(&self.path)?;
            let (writer, incoming, leader_version) = connect_stream(&self.path, self.instance)?;
            self.writer = writer;
            self.incoming = incoming;
            self.leader_version = leader_version;
            let Some(previous) = self.attachment.take() else {
                return Ok(());
            };
            let request = self.next_id();
            let envelope = ClientEnvelope {
                request,
                message: ClientRequest::Attach {
                    session: previous.selector.clone(),
                    mode: previous.mode,
                    after: Some(previous.cursor),
                    force: previous.force,
                },
            };
            let result = self.request_once(&envelope)?.map_err(wire_error)?;
            let ControlResult::Attached { session, lease } = result else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resident returned the wrong reconnect response",
                ));
            };
            self.attachment = Some(Attachment {
                selector: previous.selector,
                session: session.id,
                mode: previous.mode,
                force: previous.force,
                lease,
                cursor: previous.cursor,
            });
            Ok(())
        }

        fn drain(&mut self) -> io::Result<Vec<ServerMessage>> {
            let mut messages: Vec<_> = self.pending.drain(..).collect();
            loop {
                match self.incoming.try_recv() {
                    Ok(Ok(message)) => messages.push(message),
                    Ok(Err(error)) => return Err(error),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "resident connection closed",
                        ));
                    }
                }
            }
            Ok(messages)
        }

        fn lease(&self) -> io::Result<LeaseEpoch> {
            self.attachment
                .as_ref()
                .and_then(|attachment| attachment.lease)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "this terminal is viewing the session; use 'turtletap take' to drive it",
                    )
                })
        }

        fn session(&self) -> io::Result<SessionId> {
            self.attachment
                .as_ref()
                .map(|attachment| attachment.session)
                .ok_or_else(|| io::Error::other("client is not attached"))
        }

        fn set_cursor(&mut self, sequence: EventSequence) {
            if let Some(attachment) = self.attachment.as_mut() {
                attachment.cursor = attachment.cursor.max(sequence);
            }
        }

        fn next_id(&mut self) -> RequestId {
            let request = RequestId {
                client: self.instance,
                sequence: self.next_request,
            };
            self.next_request = self.next_request.saturating_add(1);
            request
        }
    }

    fn connect_stream(
        path: &Path,
        instance: ClientInstanceId,
    ) -> io::Result<(UnixStream, Receiver<io::Result<ServerMessage>>, String)> {
        validate_socket_path(path)?;
        let mut writer = UnixStream::connect(path)?;
        writer.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        writer.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        write_sync(
            &mut writer,
            &ClientHello {
                protocol: VersionRange::current(),
                binary_version: env!("CARGO_PKG_VERSION").to_owned(),
                client_instance: instance,
                client_name: "turtletap-cli".to_owned(),
                capabilities: ClientCapabilities {
                    incremental_events: true,
                    resumable: true,
                    driver_leases: true,
                },
            },
        )?;
        let handshake: ServerHandshake = read_sync(&mut writer)?;
        let hello = match handshake {
            ServerHandshake::Accepted(hello) => hello,
            ServerHandshake::Rejected(rejection) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    rejection.message,
                ));
            }
        };
        if hello.protocol != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "resident protocol {} is incompatible with client protocol {}",
                    hello.protocol.0, PROTOCOL_VERSION.0
                ),
            ));
        }
        let mut reader = writer.try_clone()?;
        reader.set_read_timeout(None)?;
        let (sender, incoming) = mpsc::channel();
        thread::spawn(move || {
            loop {
                match read_sync(&mut reader) {
                    Ok(message) => {
                        if sender.send(Ok(message)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        return;
                    }
                }
            }
        });
        Ok((writer, incoming, hello.binary_version))
    }

    fn wire_error(error: WireError) -> io::Error {
        io::Error::other(format!("{}: {}", error.code, error.message))
    }

    fn is_connection_error(error: &io::Error) -> bool {
        matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected
                | io::ErrorKind::TimedOut
                | io::ErrorKind::UnexpectedEof
        )
    }

    #[derive(Clone, Copy)]
    enum DashboardMode {
        Browse,
        Search,
        Create,
        Rename(SessionId),
        ConfirmDelete(SessionId),
        ConfirmStopLeader,
    }

    struct SessionDashboard {
        path: PathBuf,
        client: SessionClient,
        sessions: Vec<SessionSummary>,
        selected: usize,
        query: String,
        mode: DashboardMode,
        notice: Option<String>,
        refresh_elapsed: Duration,
    }

    impl SessionDashboard {
        fn connect(path: &Path) -> io::Result<Self> {
            let client = SessionClient::connect(path)?;
            let mut dashboard = Self {
                path: path.to_owned(),
                client,
                sessions: Vec::new(),
                selected: 0,
                query: String::new(),
                mode: DashboardMode::Browse,
                notice: None,
                refresh_elapsed: Duration::ZERO,
            };
            let _ = dashboard.refresh()?;
            Ok(dashboard)
        }

        fn refresh(&mut self) -> io::Result<bool> {
            self.refresh_elapsed = Duration::ZERO;
            let result = self.client.request(ClientRequest::ListSessions)?;
            let ControlResult::Sessions { sessions } = result else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "resident returned the wrong list response",
                ));
            };
            let previous_selected = self.selected;
            let mut changed = self.sessions != sessions;
            self.sessions = sessions;
            self.selected = previous_selected.min(self.filtered_indices().len().saturating_sub(1));
            changed |= self.selected != previous_selected;
            if self
                .notice
                .as_deref()
                .is_some_and(|notice| notice.starts_with("Disconnected · retrying:"))
            {
                self.notice = Some("Reconnected to the resident.".to_owned());
                changed = true;
            }
            Ok(changed)
        }

        fn filtered_indices(&self) -> Vec<usize> {
            let query = self.query.to_lowercase();
            self.sessions
                .iter()
                .enumerate()
                .filter_map(|(index, session)| {
                    (query.is_empty() || session.name.to_lowercase().contains(&query))
                        .then_some(index)
                })
                .collect()
        }

        fn selected_session(&self) -> Option<SessionSummary> {
            let indices = self.filtered_indices();
            indices
                .get(self.selected)
                .and_then(|index| self.sessions.get(*index))
                .cloned()
        }

        fn move_selection(&mut self, delta: isize) {
            let length = self.filtered_indices().len();
            if length == 0 {
                self.selected = 0;
                return;
            }
            self.selected = if delta.is_negative() {
                self.selected.saturating_sub(delta.unsigned_abs())
            } else {
                (self.selected + delta as usize).min(length - 1)
            };
        }

        fn open_selected(&mut self, mode: AttachmentMode, force: bool) -> SurfaceAction {
            let Some(session) = self.selected_session() else {
                self.notice = Some("No matching session to open.".to_owned());
                return SurfaceAction::Consumed;
            };
            let client = match SessionClient::connect(&self.path) {
                Ok(client) => client,
                Err(error) => return self.fail(error),
            };
            match RemoteSurface::attach(client, SessionSelector::Id(session.id), mode, force) {
                Ok(surface) => SurfaceAction::open(surface),
                Err(error) => self.fail(error),
            }
        }

        fn submit_text(&mut self) -> SurfaceAction {
            let value = self.query.trim().to_owned();
            if value.is_empty() {
                self.notice = Some("A session name cannot be empty.".to_owned());
                return SurfaceAction::Consumed;
            }
            let mode = std::mem::replace(&mut self.mode, DashboardMode::Browse);
            let result = match mode {
                DashboardMode::Create => self
                    .client
                    .request(ClientRequest::CreateSession {
                        name: value.clone(),
                    })
                    .and_then(|result| match result {
                        ControlResult::Created { .. } => Ok(()),
                        _ => Err(io::Error::other(
                            "resident returned the wrong create response",
                        )),
                    }),
                DashboardMode::Rename(session) => self
                    .client
                    .request(ClientRequest::RenameSession {
                        session,
                        name: value.clone(),
                    })
                    .and_then(|result| match result {
                        ControlResult::Renamed { .. } => Ok(()),
                        _ => Err(io::Error::other(
                            "resident returned the wrong rename response",
                        )),
                    }),
                _ => Ok(()),
            };
            match result.and_then(|()| self.refresh()) {
                Ok(_) => {
                    self.query.clear();
                    self.notice = Some(format!("Saved session '{value}'."));
                    SurfaceAction::Consumed
                }
                Err(error) => self.fail(error),
            }
        }

        fn confirm_delete(&mut self, session: SessionId) -> SurfaceAction {
            match self
                .client
                .request(ClientRequest::StopSession { session })
                .and_then(|result| match result {
                    ControlResult::Stopping => Ok(()),
                    _ => Err(io::Error::other(
                        "resident returned the wrong stop response",
                    )),
                })
                .and_then(|()| self.refresh())
            {
                Ok(_) => {
                    self.mode = DashboardMode::Browse;
                    self.notice = Some("Session stopped and its durable state deleted.".to_owned());
                    SurfaceAction::Consumed
                }
                Err(error) => self.fail(error),
            }
        }

        fn fail(&mut self, error: impl std::fmt::Display) -> SurfaceAction {
            let message = error.to_string();
            if self.notice.as_deref() == Some(&message) {
                SurfaceAction::Ignored
            } else {
                self.notice = Some(message);
                SurfaceAction::Consumed
            }
        }

        fn render_input<'a>(&'a self) -> Option<Line<'a>> {
            let label = match self.mode {
                DashboardMode::Search => "filter / ",
                DashboardMode::Create => "new session / ",
                DashboardMode::Rename(_) => "rename / ",
                _ => return None,
            };
            Some(Line::from(vec![
                Span::styled(label, Style::default().fg(Color::Cyan)),
                Span::raw(self.query.as_str()),
                Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            ]))
        }
    }

    impl Surface for SessionDashboard {
        fn title(&self) -> std::borrow::Cow<'_, str> {
            "sessions".into()
        }

        fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
            let driven = self
                .sessions
                .iter()
                .filter(|session| session.driver.is_some())
                .count();
            let attached: usize = self.sessions.iter().map(|session| session.viewers).sum();
            let mut lines = vec![
                Line::styled(
                    "Resident sessions",
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Line::styled(
                    format!(
                        "{} session{} · {driven} driven · {attached} attached",
                        self.sessions.len(),
                        if self.sessions.len() == 1 { "" } else { "s" }
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
                Line::styled(
                    "Enter open · v view · t take over · n new · r rename · x delete · / search",
                    Style::default().fg(Color::DarkGray),
                ),
                Line::raw(""),
            ];
            let filtered = self.filtered_indices();
            if filtered.is_empty() {
                lines.push(Line::styled(
                    if self.sessions.is_empty() {
                        "  No sessions yet. Press n to create one."
                    } else {
                        "  No sessions match this filter."
                    },
                    Style::default().fg(Color::Yellow),
                ));
            }
            for (visible, index) in filtered.into_iter().enumerate() {
                let session = &self.sessions[index];
                let selected = visible == self.selected;
                let marker = if selected { "→" } else { " " };
                let (activity, role, activity_style) = if session.driver.is_some() {
                    ("●", "DRIVEN", Style::default().fg(Color::Green))
                } else {
                    ("○", "IDLE", Style::default().fg(Color::DarkGray))
                };
                let name_style = if selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{marker} {activity} "), activity_style),
                    Span::styled(format!("{:<24}", session.name), name_style),
                    Span::styled(
                        format!(
                            " [{role:<6}]  {} attached  seq {}",
                            session.viewers, session.sequence.0
                        ),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]));
            }
            lines.push(Line::raw(""));
            match self.mode {
                DashboardMode::ConfirmDelete(_) | DashboardMode::ConfirmStopLeader => {
                    lines.push(Line::styled(
                        if matches!(self.mode, DashboardMode::ConfirmStopLeader) {
                            "Stop the resident leader? Sessions remain durable. [y/N]"
                        } else {
                            "Delete this session and its durable state? [y/N]"
                        },
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    ))
                }
                _ => {
                    if let Some(input) = self.render_input() {
                        lines.push(input);
                    } else if let Some(notice) = &self.notice {
                        lines.push(Line::styled(notice, Style::default().fg(Color::Yellow)));
                    }
                }
            }
            frame.render_widget(Paragraph::new(lines), area);
        }

        fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
            match event {
                SurfaceEvent::Tick(elapsed) => {
                    self.refresh_elapsed += elapsed;
                    if self.refresh_elapsed >= Duration::from_secs(1) {
                        return match self.refresh() {
                            Ok(true) => SurfaceAction::Consumed,
                            Ok(false) => SurfaceAction::Ignored,
                            Err(error) => self.fail(format!("Disconnected · retrying: {error}")),
                        };
                    }
                    SurfaceAction::Ignored
                }
                SurfaceEvent::Key(key) => match self.mode {
                    DashboardMode::Browse => match key.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            self.move_selection(-1);
                            SurfaceAction::Consumed
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            self.move_selection(1);
                            SurfaceAction::Consumed
                        }
                        KeyCode::Enter => {
                            let mode = if self
                                .selected_session()
                                .is_some_and(|session| session.driver.is_some())
                            {
                                AttachmentMode::View
                            } else {
                                AttachmentMode::Drive
                            };
                            self.open_selected(mode, false)
                        }
                        KeyCode::Char('v') => self.open_selected(AttachmentMode::View, false),
                        KeyCode::Char('t') => self.open_selected(AttachmentMode::Drive, true),
                        KeyCode::Char('/') => {
                            self.mode = DashboardMode::Search;
                            self.query.clear();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char('n') => {
                            self.mode = DashboardMode::Create;
                            self.query.clear();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char('r') => {
                            if let Some(session) = self.selected_session() {
                                self.query.clone_from(&session.name);
                                self.mode = DashboardMode::Rename(session.id);
                            }
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char('x') => {
                            if let Some(session) = self.selected_session() {
                                self.mode = DashboardMode::ConfirmDelete(session.id);
                            }
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char('!') => {
                            self.mode = DashboardMode::ConfirmStopLeader;
                            SurfaceAction::Consumed
                        }
                        KeyCode::Char('q') => SurfaceAction::Close,
                        _ => SurfaceAction::Ignored,
                    },
                    DashboardMode::Search | DashboardMode::Create | DashboardMode::Rename(_) => {
                        match key.code {
                            KeyCode::Esc => {
                                self.mode = DashboardMode::Browse;
                                self.query.clear();
                                SurfaceAction::Consumed
                            }
                            KeyCode::Enter if matches!(self.mode, DashboardMode::Search) => {
                                self.mode = DashboardMode::Browse;
                                SurfaceAction::Consumed
                            }
                            KeyCode::Enter => self.submit_text(),
                            KeyCode::Backspace => {
                                self.query.pop();
                                self.selected = 0;
                                SurfaceAction::Consumed
                            }
                            KeyCode::Char(character) => {
                                self.query.push(character);
                                self.selected = 0;
                                SurfaceAction::Consumed
                            }
                            _ => SurfaceAction::Ignored,
                        }
                    }
                    DashboardMode::ConfirmDelete(session) => match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => self.confirm_delete(session),
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            self.mode = DashboardMode::Browse;
                            self.notice = Some("Delete cancelled.".to_owned());
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Ignored,
                    },
                    DashboardMode::ConfirmStopLeader => match key.code {
                        KeyCode::Char('y') | KeyCode::Char('Y') => {
                            match self.client.request(ClientRequest::StopLeader) {
                                Ok(ControlResult::Stopping) => SurfaceAction::Detach,
                                Ok(_) => self.fail("resident returned the wrong stop response"),
                                Err(error) => self.fail(error),
                            }
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                            self.mode = DashboardMode::Browse;
                            self.notice = Some("Leader stop cancelled.".to_owned());
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Ignored,
                    },
                },
                SurfaceEvent::Paste(text)
                    if matches!(
                        self.mode,
                        DashboardMode::Search | DashboardMode::Create | DashboardMode::Rename(_)
                    ) =>
                {
                    self.query.push_str(&text.replace(['\r', '\n'], " "));
                    self.selected = 0;
                    SurfaceAction::Consumed
                }
                SurfaceEvent::Paste(_) | SurfaceEvent::Mouse(_) | SurfaceEvent::Resize { .. } => {
                    SurfaceAction::Ignored
                }
            }
        }

        fn shortcuts(&self) -> Vec<Shortcut> {
            vec![
                Shortcut::new("Enter", "Open selected session"),
                Shortcut::new("/", "Search sessions"),
                Shortcut::new("n / r / x", "New, rename, or delete"),
                Shortcut::new("v / t", "View or take driver"),
                Shortcut::new("!", "Stop resident leader"),
            ]
        }
    }

    #[derive(Default)]
    struct ScreenActivity {
        focused: bool,
        unread_lines: usize,
    }

    impl ScreenActivity {
        fn appended(&mut self, lines: usize) {
            if !self.focused {
                self.unread_lines = self.unread_lines.saturating_add(lines);
            }
        }

        fn focus(&mut self) {
            self.focused = true;
            self.unread_lines = 0;
        }

        fn blur(&mut self) {
            self.focused = false;
        }
    }

    struct RemoteSurface {
        client: SessionClient,
        name: String,
        mode: AttachmentMode,
        snapshot: SessionSnapshot,
        input: Vec<char>,
        cursor: usize,
        history_cursor: Option<usize>,
        history_draft: String,
        connection_error: Option<String>,
        planned_shutdown: Option<ShutdownReason>,
        banner: Option<String>,
        scrollback: Scrollback,
        activity: ScreenActivity,
    }

    impl RemoteSurface {
        fn attach(
            mut client: SessionClient,
            selector: SessionSelector,
            mode: AttachmentMode,
            force: bool,
        ) -> io::Result<Self> {
            let (session, _lease, snapshot) = client.attach(selector, mode, force)?;
            Ok(Self {
                client,
                name: session.name,
                mode,
                snapshot,
                input: Vec::new(),
                cursor: 0,
                history_cursor: None,
                history_draft: String::new(),
                connection_error: None,
                planned_shutdown: None,
                banner: None,
                scrollback: Scrollback::default(),
                activity: ScreenActivity::default(),
            })
        }

        fn take_driver(&mut self) -> SurfaceAction {
            let session = match self.client.session() {
                Ok(session) => session,
                Err(error) => return self.fail(error),
            };
            match self.client.request(ClientRequest::AcquireDriver {
                session,
                force: true,
            }) {
                Ok(ControlResult::Driver { lease, .. }) => {
                    if let Some(attachment) = self.client.attachment.as_mut() {
                        attachment.lease = lease.map(|lease| lease.epoch);
                        attachment.mode = AttachmentMode::Drive;
                    }
                    self.mode = AttachmentMode::Drive;
                    self.banner = Some("Driver acquired · input is live.".to_owned());
                    SurfaceAction::Consumed
                }
                Ok(_) => self.fail("resident returned the wrong driver response"),
                Err(error) => self.fail(error),
            }
        }

        fn release_driver(&mut self) -> SurfaceAction {
            let session = match self.client.session() {
                Ok(session) => session,
                Err(error) => return self.fail(error),
            };
            match self
                .client
                .request(ClientRequest::ReleaseDriver { session })
            {
                Ok(ControlResult::Driver { .. }) => {
                    if let Some(attachment) = self.client.attachment.as_mut() {
                        attachment.lease = None;
                        attachment.mode = AttachmentMode::View;
                    }
                    self.mode = AttachmentMode::View;
                    self.banner = Some("Driver released · viewing only.".to_owned());
                    SurfaceAction::Consumed
                }
                Ok(_) => self.fail("resident returned the wrong driver response"),
                Err(error) => self.fail(error),
            }
        }

        fn command(&mut self, command: ShellCommand) -> SurfaceAction {
            let session = match self.client.session() {
                Ok(session) => session,
                Err(error) => return self.fail(error),
            };
            let lease = match self.client.lease() {
                Ok(lease) => lease,
                Err(error) => return self.fail(error),
            };
            let command = match serde_json::to_value(command) {
                Ok(command) => command,
                Err(error) => return self.fail(error),
            };
            match self.client.request(ClientRequest::Command {
                session,
                lease,
                command,
            }) {
                Ok(ControlResult::Accepted { sequence, .. }) => {
                    self.client.set_cursor(sequence);
                    self.connection_error = None;
                    self.drain_updates();
                    SurfaceAction::Consumed
                }
                Ok(_) => self.fail("resident returned the wrong command response"),
                Err(error) => self.fail(error),
            }
        }

        fn drain_updates(&mut self) -> SurfaceAction {
            let messages = match self.client.drain() {
                Ok(messages) => messages,
                Err(error) if is_connection_error(&error) => {
                    if self.planned_shutdown.is_some() {
                        return SurfaceAction::Detach;
                    }
                    self.banner = Some("Disconnected · reconnecting…".to_owned());
                    if let Err(reconnect) = self.client.reconnect() {
                        return self.fail(format!(
                            "Connection to the resident session was lost: {reconnect}"
                        ));
                    }
                    self.connection_error = None;
                    self.banner = Some("Reconnected · session state resumed.".to_owned());
                    match self.client.drain() {
                        Ok(messages) => messages,
                        Err(error) => return self.fail(error),
                    }
                }
                Err(error) => return self.fail(error),
            };
            let mut changed = false;
            for message in messages {
                match message {
                    ServerMessage::Snapshot {
                        session,
                        sequence,
                        state,
                    } if self.client.session().ok() == Some(session) => {
                        match serde_json::from_value::<SessionSnapshot>(state) {
                            Ok(snapshot) => {
                                let appended = transcript_append_count(
                                    &self.snapshot.transcript,
                                    &snapshot.transcript,
                                );
                                self.snapshot = snapshot;
                                self.scrollback
                                    .appended(appended, self.snapshot.transcript.len());
                                self.activity.appended(appended);
                                self.client.set_cursor(sequence);
                                changed = true;
                            }
                            Err(error) => return self.fail(error),
                        }
                    }
                    ServerMessage::Event {
                        session,
                        sequence,
                        event,
                    } if self.client.session().ok() == Some(session) => {
                        match serde_json::from_value::<ShellEvent>(event) {
                            Ok(event) => {
                                let appended = event.appended_count(&self.snapshot);
                                event.apply(&mut self.snapshot);
                                self.scrollback
                                    .appended(appended, self.snapshot.transcript.len());
                                self.activity.appended(appended);
                                self.client.set_cursor(sequence);
                                changed = true;
                            }
                            Err(error) => return self.fail(error),
                        }
                    }
                    ServerMessage::DriverChanged { lease, .. } => {
                        if let Some(attachment) = self.client.attachment.as_mut() {
                            attachment.lease = lease
                                .filter(|lease| lease.owner == self.client.instance)
                                .map(|lease| lease.epoch);
                        }
                        changed = true;
                    }
                    ServerMessage::ResyncRequired { .. } => {
                        if let Err(error) = self.client.reconnect() {
                            return self.fail(error);
                        }
                        changed = true;
                    }
                    ServerMessage::ShuttingDown { reason } => {
                        self.planned_shutdown = Some(reason);
                        self.banner = Some(format!("Leader {reason:?} · reconnecting…"));
                        self.snapshot.transcript.push(TranscriptEntry {
                            kind: TranscriptKind::System,
                            text: format!("Resident is shutting down: {reason:?}"),
                        });
                        self.activity.appended(1);
                        changed = true;
                    }
                    ServerMessage::Shutdown { reason } => {
                        if reason == ShutdownReason::Upgrade {
                            self.planned_shutdown = None;
                            if let Err(error) = self.client.reconnect() {
                                return self.fail(error);
                            }
                            self.banner = Some("Reconnected to the replacement leader.".to_owned());
                            changed = true;
                        } else {
                            return SurfaceAction::Detach;
                        }
                    }
                    ServerMessage::Response { .. } => {}
                    ServerMessage::Snapshot { .. } | ServerMessage::Event { .. } => {}
                }
            }
            if changed {
                SurfaceAction::Consumed
            } else {
                SurfaceAction::Ignored
            }
        }

        fn fail(&mut self, error: impl std::fmt::Display) -> SurfaceAction {
            let message = error.to_string();
            if self.connection_error.as_deref() == Some(&message) {
                return SurfaceAction::Ignored;
            }
            self.snapshot.transcript.push(TranscriptEntry {
                kind: TranscriptKind::Error,
                text: message.clone(),
            });
            self.snapshot.last_failed = true;
            self.connection_error = Some(message);
            self.activity.appended(1);
            SurfaceAction::Consumed
        }

        fn submit(&mut self) -> SurfaceAction {
            if self.mode == AttachmentMode::View {
                return self.fail("this terminal is viewing the session; use 'turtletap take'");
            }
            let line: String = self.input.iter().collect();
            let line = line.trim().to_owned();
            if line.is_empty() {
                return SurfaceAction::Ignored;
            }
            if is_detach_command(&line) {
                let session = match self.client.session() {
                    Ok(session) => session,
                    Err(error) => return self.fail(error),
                };
                let _ = self.client.request(ClientRequest::Detach { session });
                return SurfaceAction::Detach;
            }
            self.clear_input();
            self.command(ShellCommand::Submit { line })
        }

        fn clear_input(&mut self) {
            self.input.clear();
            self.cursor = 0;
            self.history_cursor = None;
            self.history_draft.clear();
        }

        fn history_previous(&mut self) {
            if self.snapshot.history.is_empty() {
                return;
            }
            let index = match self.history_cursor {
                Some(index) => index.saturating_sub(1),
                None => {
                    self.history_draft = self.input.iter().collect();
                    self.snapshot.history.len() - 1
                }
            };
            self.history_cursor = Some(index);
            self.input = self.snapshot.history[index].chars().collect();
            self.cursor = self.input.len();
        }

        fn history_next(&mut self) {
            let Some(index) = self.history_cursor else {
                return;
            };
            if index + 1 < self.snapshot.history.len() {
                let next = index + 1;
                self.history_cursor = Some(next);
                self.input = self.snapshot.history[next].chars().collect();
            } else {
                self.history_cursor = None;
                self.input = self.history_draft.chars().collect();
                self.history_draft.clear();
            }
            self.cursor = self.input.len();
        }

        fn complete(&mut self) {
            if self.cursor != self.input.len() {
                return;
            }
            let input: String = self.input.iter().collect();
            if input.chars().any(char::is_whitespace) {
                return;
            }
            let mut candidates: Vec<_> = [
                ":add",
                ":commands",
                ":remove",
                ":cd",
                ":history",
                ":clear",
                ":help",
                ":quit",
            ]
            .into_iter()
            .map(str::to_owned)
            .chain(self.snapshot.commands.iter().cloned())
            .filter(|candidate| candidate.starts_with(&input))
            .collect();
            candidates.sort();
            candidates.dedup();
            if let [candidate] = candidates.as_slice() {
                self.input = candidate.chars().collect();
                self.cursor = self.input.len();
            }
        }

        fn render_prompt(&self, frame: &mut Frame<'_>, area: Rect) {
            if area.width == 0 || area.height == 0 {
                return;
            }
            let role = if self.client.lease().is_ok() {
                ""
            } else {
                " [view]"
            };
            let label = format!("{}{} ❯ ", self.snapshot.cwd_label, role);
            let label_width = Line::from(label.as_str()).width();
            let available = usize::from(area.width).saturating_sub(label_width).max(1);
            let mut start = 0;
            while start < self.cursor
                && char_slice_width(&self.input[start..self.cursor]) >= available
            {
                start += 1;
            }
            let mut end = start;
            while end < self.input.len() && char_slice_width(&self.input[start..=end]) <= available
            {
                end += 1;
            }
            let visible: String = self.input[start..end].iter().collect();
            let before_cursor = char_slice_width(&self.input[start..self.cursor]);
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled(label, Style::default().fg(Color::Cyan)),
                    Span::raw(visible),
                ])),
                area,
            );
            let x = area
                .x
                .saturating_add((label_width + before_cursor).min(usize::from(u16::MAX)) as u16)
                .min(area.right().saturating_sub(1));
            frame.set_cursor_position(Position::new(x, area.y));
        }
    }

    impl Surface for RemoteSurface {
        fn title(&self) -> std::borrow::Cow<'_, str> {
            let role = if self.connection_error.is_some() {
                "RECONNECT"
            } else if self.client.lease().is_ok() {
                "DRIVE"
            } else {
                "VIEW"
            };
            if self.activity.unread_lines == 0 {
                format!("{} [{role}]", self.name).into()
            } else {
                format!("{} [{role}] +{}", self.name, self.activity.unread_lines).into()
            }
        }

        fn status(&self) -> SurfaceStatus {
            if self.connection_error.is_some() || self.snapshot.last_failed {
                SurfaceStatus::Failed
            } else if self.activity.unread_lines > 0 && !self.snapshot.running {
                SurfaceStatus::Attention
            } else if self.snapshot.running {
                SurfaceStatus::Working
            } else {
                SurfaceStatus::Ready
            }
        }

        fn input_policy(&self) -> InputPolicy {
            InputPolicy::Captured
        }

        fn render(&mut self, frame: &mut Frame<'_>, area: Rect) {
            let constraints = if self.banner.is_some() {
                vec![
                    Constraint::Length(1),
                    Constraint::Min(0),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ]
            } else {
                vec![
                    Constraint::Min(0),
                    Constraint::Length(1),
                    Constraint::Length(1),
                ]
            };
            let sections = Layout::default()
                .direction(Direction::Vertical)
                .constraints(constraints)
                .split(area);
            let (transcript_area, scroll_area, prompt_area) = if let Some(banner) = &self.banner {
                frame.render_widget(
                    Paragraph::new(Line::styled(
                        banner.as_str(),
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    )),
                    sections[0],
                );
                (sections[1], sections[2], sections[3])
            } else {
                (sections[0], sections[1], sections[2])
            };
            let visible_lines = usize::from(transcript_area.height);
            let (start, end) = self
                .scrollback
                .window(self.snapshot.transcript.len(), visible_lines);
            let lines: Vec<Line<'_>> = self.snapshot.transcript[start..end]
                .iter()
                .map(|entry| {
                    let style = match entry.kind {
                        TranscriptKind::System => Style::default().fg(Color::DarkGray),
                        TranscriptKind::Command => Style::default().fg(Color::Cyan),
                        TranscriptKind::Stdout => Style::default(),
                        TranscriptKind::Stderr => Style::default().fg(Color::Red),
                        TranscriptKind::Error => Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::BOLD),
                    };
                    Line::styled(entry.text.as_str(), style)
                })
                .collect();
            frame.render_widget(Paragraph::new(lines), transcript_area);
            frame.render_widget(Paragraph::new(self.scrollback.status_line()), scroll_area);
            self.render_prompt(frame, prompt_area);
        }

        fn handle(&mut self, event: SurfaceEvent) -> SurfaceAction {
            match event {
                SurfaceEvent::Tick(_) => self.drain_updates(),
                SurfaceEvent::Paste(text) if self.mode == AttachmentMode::Drive => {
                    let normalized = text.replace(['\r', '\n'], " ");
                    let inserted: Vec<_> = normalized.chars().collect();
                    let count = inserted.len();
                    self.input.splice(self.cursor..self.cursor, inserted);
                    self.cursor += count;
                    SurfaceAction::Consumed
                }
                SurfaceEvent::Paste(_) => SurfaceAction::Ignored,
                SurfaceEvent::Key(key) => {
                    if key.code == KeyCode::F(2) {
                        return self.release_driver();
                    }
                    if key.code == KeyCode::F(3) {
                        return self.take_driver();
                    }
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        return match key.code {
                            KeyCode::Char('c') if self.snapshot.running => {
                                self.command(ShellCommand::Interrupt)
                            }
                            KeyCode::Char('c') => {
                                self.clear_input();
                                SurfaceAction::Consumed
                            }
                            KeyCode::Char('d') if self.input.is_empty() => SurfaceAction::Detach,
                            KeyCode::Char('d') => {
                                if self.cursor < self.input.len() {
                                    self.input.remove(self.cursor);
                                }
                                SurfaceAction::Consumed
                            }
                            KeyCode::Char('l') => self.command(ShellCommand::Clear),
                            KeyCode::Home => {
                                self.scrollback.top(self.snapshot.transcript.len());
                                SurfaceAction::Consumed
                            }
                            KeyCode::End => {
                                self.scrollback.follow();
                                SurfaceAction::Consumed
                            }
                            _ => SurfaceAction::Ignored,
                        };
                    }
                    match key.code {
                        KeyCode::PageUp => {
                            let page = self.scrollback.page_size();
                            self.scrollback
                                .scroll_up(self.snapshot.transcript.len(), page);
                            SurfaceAction::Consumed
                        }
                        KeyCode::PageDown => {
                            let page = self.scrollback.page_size();
                            self.scrollback.scroll_down(page);
                            SurfaceAction::Consumed
                        }
                        KeyCode::Enter => self.submit(),
                        KeyCode::Char(character) if self.mode == AttachmentMode::Drive => {
                            self.input.insert(self.cursor, character);
                            self.cursor += 1;
                            SurfaceAction::Consumed
                        }
                        KeyCode::Backspace => {
                            if self.cursor > 0 {
                                self.cursor -= 1;
                                self.input.remove(self.cursor);
                            }
                            SurfaceAction::Consumed
                        }
                        KeyCode::Delete => {
                            if self.cursor < self.input.len() {
                                self.input.remove(self.cursor);
                            }
                            SurfaceAction::Consumed
                        }
                        KeyCode::Left => {
                            self.cursor = self.cursor.saturating_sub(1);
                            SurfaceAction::Consumed
                        }
                        KeyCode::Right => {
                            self.cursor = (self.cursor + 1).min(self.input.len());
                            SurfaceAction::Consumed
                        }
                        KeyCode::Home => {
                            self.cursor = 0;
                            SurfaceAction::Consumed
                        }
                        KeyCode::End => {
                            self.cursor = self.input.len();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Up => {
                            self.history_previous();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Down => {
                            self.history_next();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Tab => {
                            self.complete();
                            SurfaceAction::Consumed
                        }
                        KeyCode::Esc => {
                            self.clear_input();
                            SurfaceAction::Consumed
                        }
                        _ => SurfaceAction::Ignored,
                    }
                }
                SurfaceEvent::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => {
                        self.scrollback.scroll_up(self.snapshot.transcript.len(), 3);
                        SurfaceAction::Consumed
                    }
                    MouseEventKind::ScrollDown => {
                        self.scrollback.scroll_down(3);
                        SurfaceAction::Consumed
                    }
                    _ => SurfaceAction::Ignored,
                },
                SurfaceEvent::Resize { .. } => SurfaceAction::Ignored,
            }
        }

        fn shortcuts(&self) -> Vec<Shortcut> {
            vec![
                Shortcut::new("Enter", "Run command"),
                Shortcut::new("↑ / ↓", "Command history"),
                Shortcut::new("Tab", "Complete added command"),
                Shortcut::new("PgUp / PgDn", "Scroll transcript history"),
                Shortcut::new("Ctrl-Home / Ctrl-End", "Oldest output or live tail"),
                Shortcut::new("Ctrl-C", "Interrupt command or clear input"),
                Shortcut::new("Ctrl-D", "Detach; the session keeps running"),
                Shortcut::new("F2 / F3", "Release or take driver"),
            ]
        }

        fn focus(&mut self) {
            self.activity.focus();
        }

        fn blur(&mut self) {
            self.activity.blur();
        }
    }

    pub(crate) fn open() -> io::Result<()> {
        let path = socket_path();
        ensure_started(&path)?;
        let dashboard = SessionDashboard::connect(&path)?;
        let mut shell = Shell::new(crate::settings::shell_config("TurtleTap")?);
        shell.add_surface(dashboard);
        let _reason = shell.attach()?;
        Ok(())
    }

    pub(crate) fn attach() -> io::Result<()> {
        attach_named(DEFAULT_SESSION)
    }

    pub(crate) fn attach_named(name: &str) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        attach_path(&path, name, AttachmentMode::Drive, false)
    }

    pub(crate) fn view(name: &str) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        attach_path(&path, name, AttachmentMode::View, false)
    }

    pub(crate) fn take(name: &str) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        attach_path(&path, name, AttachmentMode::Drive, true)
    }

    pub(crate) fn create(name: &str) -> io::Result<()> {
        let path = socket_path();
        ensure_started(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let result = client.request(ClientRequest::CreateSession {
            name: name.to_owned(),
        })?;
        let ControlResult::Created { session } = result else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong create response",
            ));
        };
        attach_path(&path, &session.name, AttachmentMode::Drive, false)
    }

    pub(crate) fn rename(old: &str, new: &str) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let sessions = match client.request(ClientRequest::ListSessions)? {
            ControlResult::Sessions { sessions } => sessions,
            _ => {
                return Err(io::Error::other(
                    "resident returned the wrong list response",
                ));
            }
        };
        let session = sessions
            .into_iter()
            .find(|session| session.name == old)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown resident session"))?;
        match client.request(ClientRequest::RenameSession {
            session: session.id,
            name: new.to_owned(),
        })? {
            ControlResult::Renamed { session } => {
                println!("Renamed '{old}' to '{}'.", session.name);
                Ok(())
            }
            _ => Err(io::Error::other(
                "resident returned the wrong rename response",
            )),
        }
    }

    pub(crate) fn list(format: OutputFormat) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let result = client.request(ClientRequest::ListSessions)?;
        let ControlResult::Sessions { sessions } = result else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong list response",
            ));
        };
        if format == OutputFormat::Json {
            return print_json(&sessions);
        }
        for session in sessions {
            let role = if session.driver.is_some() {
                "driven"
            } else {
                "idle"
            };
            println!(
                "{:<20} {}  {} attached",
                session.name, role, session.viewers
            );
        }
        Ok(())
    }

    pub(crate) fn start() -> io::Result<()> {
        let path = socket_path();
        let started = ensure_started(&path)?;
        println!(
            "TurtleTap resident {}.",
            if started {
                "started"
            } else {
                "is already running"
            }
        );
        Ok(())
    }

    pub(crate) fn status(format: OutputFormat) -> io::Result<()> {
        let path = socket_path();
        if !probe_server(&path)? {
            if format == OutputFormat::Json {
                return print_json(&serde_json::json!({
                    "resident": "stopped",
                    "pid": null,
                    "leader": null,
                    "sessions": [],
                }));
            }
            println!("Resident: stopped");
            return Ok(());
        }
        let mut client = SessionClient::connect(&path)?;
        let result = client.request(ClientRequest::Status)?;
        let ControlResult::Status {
            pid,
            leader,
            sessions,
        } = result
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong status response",
            ));
        };
        if format == OutputFormat::Json {
            return print_json(&serde_json::json!({
                "resident": "running",
                "pid": pid,
                "leader": leader,
                "sessions": sessions,
            }));
        }
        println!("Resident: running");
        println!("PID: {pid}");
        println!("Leader: {leader}");
        println!("Sessions: {}", sessions.len());
        for session in sessions {
            println!(
                "  {} — {} viewer{}, {}",
                session.name,
                session.viewers,
                if session.viewers == 1 { "" } else { "s" },
                if session.driver.is_some() {
                    "driven"
                } else {
                    "idle"
                }
            );
        }
        Ok(())
    }

    fn print_json(value: &impl Serialize) -> io::Result<()> {
        println!("{}", serde_json::to_string(value).map_err(protocol_error)?);
        Ok(())
    }

    pub(crate) fn stop() -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let _ = client.request(ClientRequest::StopLeader)?;
        let deadline = Instant::now() + START_TIMEOUT;
        while path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "resident acknowledged shutdown but did not release its socket",
            ));
        }
        println!("TurtleTap resident stopped; durable sessions were preserved.");
        Ok(())
    }

    pub(crate) fn stop_session(name: &str) -> io::Result<()> {
        let path = socket_path();
        require_running(&path)?;
        let mut client = SessionClient::connect(&path)?;
        let sessions = match client.request(ClientRequest::ListSessions)? {
            ControlResult::Sessions { sessions } => sessions,
            _ => {
                return Err(io::Error::other(
                    "resident returned the wrong list response",
                ));
            }
        };
        let session = sessions
            .into_iter()
            .find(|session| session.name == name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "unknown resident session"))?;
        let _ = client.request(ClientRequest::StopSession {
            session: session.id,
        })?;
        println!("Stopped session '{name}'.");
        Ok(())
    }

    pub(crate) fn serve(path: PathBuf) -> io::Result<()> {
        validate_socket_path(&path)?;
        let state_dir = state_dir(&path);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        let mut config = ResidentHostConfig::new(&path, state_dir, env!("CARGO_PKG_VERSION"))
            .with_initial_session(DEFAULT_SESSION);
        config.tick_rate = SERVER_TICK;
        config.outbound_capacity = OUTBOUND_CAPACITY;
        config.event_history = EVENT_HISTORY;
        config.durability = Durability::Flush;
        runtime.block_on(
            ResidentHost::new(ShellApplication, TokioRuntime, TokioUnixTransport, config).serve(),
        )
    }

    fn attach_path(path: &Path, name: &str, mode: AttachmentMode, force: bool) -> io::Result<()> {
        let dashboard = SessionDashboard::connect(path)?;
        let client = SessionClient::connect(path)?;
        let surface =
            RemoteSurface::attach(client, SessionSelector::Name(name.to_owned()), mode, force)?;
        let mut shell = Shell::new(crate::settings::shell_config("TurtleTap")?);
        shell.add_surface(dashboard);
        shell.add_surface(surface);
        let _reason = shell.attach()?;
        Ok(())
    }

    fn require_running(path: &Path) -> io::Result<()> {
        if probe_server(path)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "no TurtleTap resident is running; run 'turtletap start' first",
            ))
        }
    }

    fn ensure_started(path: &Path) -> io::Result<bool> {
        if use_or_replace_running_leader(path)? {
            return Ok(false);
        }
        prepare_parent(path)?;
        let deadline = Instant::now() + START_TIMEOUT;
        let mut lock = LeaderLock::for_socket(path);
        loop {
            if lock.try_acquire().map_err(io::Error::other)? {
                break;
            }
            if probe_server(path)? {
                return Ok(false);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for another client to start the resident",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
        if probe_server(path)? {
            lock.release_for_handoff().map_err(io::Error::other)?;
            return Ok(false);
        }
        lock.cleanup_stale_socket().map_err(io::Error::other)?;

        let executable = env::current_exe()?;
        let mut child = Command::new(executable)
            .arg("__serve")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()?;
        loop {
            if socket_connectable(path) {
                lock.release_for_handoff().map_err(io::Error::other)?;
                break;
            }
            if let Some(status) = child.try_wait()? {
                let mut detail = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    let _ = stderr.read_to_string(&mut detail);
                }
                return Err(io::Error::other(format!(
                    "resident exited during startup ({status}): {}",
                    detail.trim()
                )));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "resident did not bind its socket within five seconds",
                ));
            }
            thread::sleep(Duration::from_millis(20));
        }
        while Instant::now() < deadline {
            if probe_server(path)? {
                return Ok(true);
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "resident did not become ready within five seconds",
        ))
    }

    fn use_or_replace_running_leader(path: &Path) -> io::Result<bool> {
        let mut client = match SessionClient::connect(path) {
            Ok(client) => client,
            Err(error)
                if is_connection_error(&error) || error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let requested = env!("CARGO_PKG_VERSION");
        if !version_is_newer(requested, &client.leader_version) {
            return Ok(true);
        }
        let _ = client.request(ClientRequest::ReplaceLeader {
            binary_version: requested.to_owned(),
        })?;
        drop(client);
        let deadline = Instant::now() + START_TIMEOUT;
        while socket_connectable(path) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        if socket_connectable(path) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "older resident accepted replacement but did not release its socket",
            ));
        }
        Ok(false)
    }

    fn version_is_newer(requested: &str, current: &str) -> bool {
        match (
            semver::Version::parse(requested),
            semver::Version::parse(current),
        ) {
            (Ok(requested), Ok(current)) => requested > current,
            _ => requested > current,
        }
    }

    fn probe_server(path: &Path) -> io::Result<bool> {
        let mut client = match SessionClient::connect(path) {
            Ok(client) => client,
            Err(error)
                if is_connection_error(&error) || error.kind() == io::ErrorKind::NotFound =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        match client.request(ClientRequest::Ping) {
            Ok(ControlResult::Pong) => Ok(true),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident returned the wrong ping response",
            )),
            Err(error) if is_connection_error(&error) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn socket_connectable(path: &Path) -> bool {
        UnixStream::connect(path).is_ok()
    }

    fn socket_path() -> PathBuf {
        if let Some(path) = env::var_os("TURTLETAP_SOCKET").filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
        let mut hasher = DefaultHasher::new();
        env::var_os("HOME").hash(&mut hasher);
        env::temp_dir()
            .join(format!("turtletap-{:016x}", hasher.finish()))
            .join("resident.sock")
    }

    fn state_dir(socket: &Path) -> PathBuf {
        if let Some(path) = env::var_os("TURTLETAP_STATE_DIR").filter(|path| !path.is_empty()) {
            return PathBuf::from(path);
        }
        socket.with_extension("state")
    }

    fn prepare_parent(path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "socket path has no parent")
        })?;
        match fs::symlink_metadata(parent) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("refusing symlinked socket parent: {}", parent.display()),
                ));
            }
            Ok(metadata) if metadata.is_dir() => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("socket parent is not a directory: {}", parent.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        fs::create_dir_all(parent)?;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
    }

    fn validate_socket_path(path: &Path) -> io::Result<()> {
        let length = path.as_os_str().as_bytes().len();
        if length > MAX_SOCKET_PATH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "socket path is {length} bytes; use TURTLETAP_SOCKET with a path no longer than {MAX_SOCKET_PATH_BYTES} bytes"
                ),
            ));
        }
        Ok(())
    }

    fn is_detach_command(line: &str) -> bool {
        matches!(split_command(line).0, ":quit" | ":detach" | "exit")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn incremental_event_roundtrips() {
            let mut before = SessionSnapshot {
                revision: 1,
                transcript: vec![],
                history: vec![],
                commands: vec![],
                cwd_label: "~".to_owned(),
                running: false,
                last_failed: false,
                queued: 0,
            };
            let mut after = before.clone();
            after.revision = 2;
            after.transcript.push(TranscriptEntry {
                kind: TranscriptKind::Stdout,
                text: "hello".to_owned(),
            });
            let event = ShellEvent::between(&before, &after);
            event.apply(&mut before);
            assert_eq!(before, after);
        }

        #[test]
        fn transcript_append_count_handles_a_pruned_front() {
            let entry = |text: &str| TranscriptEntry {
                kind: TranscriptKind::Stdout,
                text: text.to_owned(),
            };
            let previous = vec![entry("a"), entry("b"), entry("c"), entry("d")];
            let current = vec![entry("c"), entry("d"), entry("e"), entry("f")];

            assert_eq!(transcript_append_count(&previous, &current), 2);
            assert_eq!(transcript_append_count(&previous, &[]), 0);
            assert_eq!(transcript_append_count(&previous, &[entry("new")]), 1);
        }

        #[test]
        fn screen_activity_counts_only_while_unfocused() {
            let mut activity = ScreenActivity::default();

            activity.focus();
            activity.appended(2);
            assert_eq!(activity.unread_lines, 0);

            activity.blur();
            activity.appended(3);
            assert_eq!(activity.unread_lines, 3);

            activity.focus();
            assert_eq!(activity.unread_lines, 0);
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::{
    attach, attach_named, create, list, open, rename, serve, start, status, stop, stop_session,
    take, view,
};

#[cfg(not(unix))]
pub(crate) fn open() -> io::Result<()> {
    let mut shell = turtletap::Shell::new(crate::settings::shell_config("TurtleTap")?);
    shell.add_surface(super::CommandSurface::new()?);
    let _reason = shell.attach()?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn attach() -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn attach_named(_name: &str) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn view(_name: &str) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn take(_name: &str) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn create(_name: &str) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn rename(_old: &str, _new: &str) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn list(_format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn start() -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn status(_format: OutputFormat) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn stop() -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn stop_session(_name: &str) -> io::Result<()> {
    unsupported()
}
#[cfg(not(unix))]
pub(crate) fn serve(_path: PathBuf) -> io::Result<()> {
    unsupported()
}

#[cfg(not(unix))]
fn unsupported() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "resident sessions currently require a Unix platform",
    ))
}
