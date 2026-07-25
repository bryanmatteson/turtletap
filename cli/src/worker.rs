//! Authenticated, framed, bounded per-session command worker.

use std::{
    collections::{HashSet, VecDeque},
    env,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{
        ffi::OsStringExt as _,
        fs::{OpenOptionsExt as _, PermissionsExt as _},
        net::{UnixListener, UnixStream},
        process::{CommandExt as _, ExitStatusExt as _},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    str::FromStr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use turtletap::resident::{Durability, EffectWake, SessionId, ShutdownReason};

use crate::command::{RunningCommand, running_from_worker};

const CONNECT_ATTEMPTS: usize = 200;
const CONNECT_DELAY: Duration = Duration::from_millis(5);
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 64 * 1024 * 1024;
const MAX_OUTPUT_CHUNK_BYTES: usize = 64 * 1024;
const MAX_SUBSCRIBERS: usize = 8;
const OUTPUT_QUEUE_CAPACITY: usize = 64;
const SUBSCRIBER_QUEUE_CAPACITY: usize = 64;
const MAX_WORKERS: usize = 32;
const AUTH_FILE: &str = "auth-v1";
const OWNER_FILE: &str = "owner-v1.json";
const STATE_FILE: &str = "state-v2.json";
const SPOOL_FILE: &str = "spool-v2.frames";

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WorkerRequest {
    Hello {
        token: String,
    },
    Run {
        command_id: u64,
        command: String,
        cwd: PathBuf,
        after: u64,
    },
    Interrupt,
    Prepare,
    Shutdown,
    Release,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WorkerEvent {
    Ready,
    Output {
        sequence: u64,
        stderr: bool,
        text: String,
    },
    OutputGap {
        requested_after: u64,
        first_available: u64,
    },
    Completed {
        sequence: u64,
        code: i32,
    },
    Error {
        message: String,
    },
    Stopped,
}

#[derive(Clone)]
pub(crate) struct WorkerManager {
    state_root: PathBuf,
    durability: Durability,
    stopping: Arc<Mutex<HashSet<SessionId>>>,
}

impl WorkerManager {
    pub(crate) fn new(state_root: PathBuf, durability: Durability) -> Self {
        Self {
            state_root,
            durability,
            stopping: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub(crate) fn execute(
        &self,
        session: SessionId,
        command_id: u64,
        command: &str,
        cwd: &Path,
        after: u64,
        wake: Option<EffectWake>,
    ) -> io::Result<RunningCommand> {
        let stopping = self
            .stopping
            .lock()
            .map_err(|_| io::Error::other("worker manager lock poisoned"))?;
        if stopping.contains(&session) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session worker is stopping",
            ));
        }
        if command.len() > MAX_COMMAND_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command exceeds the persistent worker size limit",
            ));
        }
        let mut output = self.connect(session)?;
        write_frame(
            &mut output,
            &WorkerRequest::Run {
                command_id,
                command: command.to_owned(),
                cwd: cwd.to_owned(),
                after,
            },
        )?;
        let control = self.connect(session)?;
        let running = running_from_worker(output, control, wake);
        drop(stopping);
        running
    }

    pub(crate) fn prepare(&self, session: SessionId) -> io::Result<()> {
        let stopping = self
            .stopping
            .lock()
            .map_err(|_| io::Error::other("worker manager lock poisoned"))?;
        if stopping.contains(&session) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session worker is stopping",
            ));
        }
        let mut stream = self.connect(session)?;
        write_frame(&mut stream, &WorkerRequest::Prepare)?;
        let result = expect_ready(&mut stream);
        drop(stopping);
        result
    }

    pub(crate) fn stop_session(&self, session: SessionId) -> io::Result<()> {
        let mut stopping = self
            .stopping
            .lock()
            .map_err(|_| io::Error::other("worker manager lock poisoned"))?;
        stopping.insert(session);
        let directory = self.worker_directory(session);
        if !directory.exists() {
            return Ok(());
        }
        let socket = worker_socket(session);
        if let Ok(mut stream) = self.connect_existing(session) {
            write_frame(&mut stream, &WorkerRequest::Shutdown)?;
            let _ = read_frame::<WorkerEvent>(&mut stream);
        }

        let owner = read_json::<Owner>(&directory.join(OWNER_FILE)).ok();
        if !wait_until(Duration::from_secs(3), || {
            owner
                .as_ref()
                .is_none_or(|owner| !identity_matches(owner.pid, &owner.started))
        }) && let Some(owner) = owner.as_ref()
        {
            signal_verified_process(owner.pid, &owner.started, "-TERM")?;
            if !wait_until(Duration::from_secs(1), || {
                !identity_matches(owner.pid, &owner.started)
            }) {
                signal_verified_process(owner.pid, &owner.started, "-KILL")?;
            }
        }

        if let Ok(stored) = read_json::<StoredState>(&directory.join(STATE_FILE))
            && let Some(command) = stored.command
            && matches!(
                command.status,
                StoredStatus::Dispatching | StoredStatus::Running
            )
            && let (Some(group), Some(started)) = (command.process_group, command.process_started)
        {
            terminate_verified_group(group, &started)?;
        }

        if owner
            .as_ref()
            .is_some_and(|owner| identity_matches(owner.pid, &owner.started))
        {
            return Err(io::Error::other(
                "worker cleanup could not prove the worker exited",
            ));
        }
        let _ = fs::remove_file(socket);
        match fs::remove_dir_all(directory) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn stop_all(&self, reason: ShutdownReason) -> io::Result<()> {
        let entries = match fs::read_dir(&self.state_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        let mut first_error = None;
        for entry in entries {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(session) = SessionId::from_str(&name) else {
                continue;
            };
            if !entry.path().join("worker").exists() {
                continue;
            }
            let result = match reason {
                ShutdownReason::Manual | ShutdownReason::Failure => self.stop_session(session),
                ShutdownReason::Upgrade => self.release(session),
            };
            if let Err(error) = result
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn release(&self, session: SessionId) -> io::Result<()> {
        match self.connect_existing(session) {
            Ok(mut stream) => {
                write_frame(&mut stream, &WorkerRequest::Release)?;
                expect_ready(&mut stream)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn connect(&self, session: SessionId) -> io::Result<UnixStream> {
        ensure_private_directory(&self.worker_directory(session))?;
        let token = ensure_auth_token(&self.worker_directory(session), self.durability)?;
        let socket = worker_socket(session);
        match connect_authenticated(&socket, &token) {
            Ok(stream) => Ok(stream),
            Err(_) => {
                self.spawn(session, &socket)?;
                connect_retry(&socket, &token)
            }
        }
    }

    fn connect_existing(&self, session: SessionId) -> io::Result<UnixStream> {
        let directory = self.worker_directory(session);
        let token = fs::read_to_string(directory.join(AUTH_FILE))?;
        connect_authenticated(&worker_socket(session), token.trim())
    }

    fn spawn(&self, session: SessionId, socket: &Path) -> io::Result<()> {
        if live_worker_count()? >= MAX_WORKERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "worker_capacity_exhausted: all persistent worker slots are busy",
            ));
        }
        let state = self.worker_directory(session);
        ensure_private_directory(&state)?;
        let mut worker = Command::new(env::current_exe()?)
            .arg("__shell-worker")
            .arg(session.to_string())
            .arg(socket)
            .arg(state)
            .env(
                "TURTLETAP_WORKER_DURABILITY",
                match self.durability {
                    Durability::Flush => "flush",
                    Durability::Fsync => "fsync",
                },
            )
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        thread::spawn(move || {
            let _ = worker.wait();
        });
        Ok(())
    }

    fn worker_directory(&self, session: SessionId) -> PathBuf {
        self.state_root.join(session.to_string()).join("worker")
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Owner {
    pid: u32,
    started: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct StoredState {
    high_watermark: u64,
    command: Option<StoredCommand>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct StoredCommand {
    id: u64,
    command: String,
    cwd: PathBuf,
    status: StoredStatus,
    process_group: Option<u32>,
    process_started: Option<String>,
    sequence: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredStatus {
    Dispatching,
    Running,
    Completed { code: i32 },
}

struct RuntimeState {
    stored: StoredState,
    events: VecDeque<WorkerEvent>,
    output_bytes: usize,
    subscribers: Vec<mpsc::SyncSender<WorkerEvent>>,
    completion_override: Option<i32>,
    directory: PathBuf,
    durability: Durability,
    spool: Option<File>,
}

impl RuntimeState {
    fn load(directory: PathBuf, durability: Durability) -> io::Result<Self> {
        let stored = match read_json(&directory.join(STATE_FILE)) {
            Ok(stored) => stored,
            Err(error) if error.kind() == io::ErrorKind::NotFound => StoredState::default(),
            Err(error) => return Err(error),
        };
        let events = load_spool(&directory.join(SPOOL_FILE))?;
        let output_bytes = events.iter().map(event_spool_bytes).sum();
        Ok(Self {
            stored,
            events,
            output_bytes,
            subscribers: Vec::new(),
            completion_override: None,
            directory,
            durability,
            spool: None,
        })
    }

    fn persist(&self) -> io::Result<()> {
        write_json_atomic(
            &self.directory.join(STATE_FILE),
            &self.stored,
            self.durability,
        )
    }

    fn clear_spool(&mut self) -> io::Result<()> {
        self.events.clear();
        self.output_bytes = 0;
        let durability = self.durability;
        let file = self.spool()?;
        file.set_len(0)?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        durable_file(file, durability)
    }

    fn append(&mut self, event: WorkerEvent) -> io::Result<()> {
        let durability = self.durability;
        let spool = self.spool()?;
        write_frame(spool, &event)?;
        durable_file(spool, durability)?;
        self.output_bytes = self.output_bytes.saturating_add(event_spool_bytes(&event));
        self.events.push_back(event.clone());
        self.enforce_output_limit(MAX_OUTPUT_BYTES);
        let spool_path = self.directory.join(SPOOL_FILE);
        if fs::metadata(&spool_path).is_ok_and(|metadata| metadata.len() > MAX_OUTPUT_BYTES as u64)
        {
            self.spool = None;
            rewrite_spool(&spool_path, &self.events, self.durability)?;
            self.spool = Some(open_spool(&spool_path)?);
        }
        self.subscribers
            .retain(|subscriber| subscriber.try_send(event.clone()).is_ok());
        Ok(())
    }

    fn spool(&mut self) -> io::Result<&mut File> {
        if self.spool.is_none() {
            self.spool = Some(open_spool(&self.directory.join(SPOOL_FILE))?);
        }
        self.spool
            .as_mut()
            .ok_or_else(|| io::Error::other("worker spool did not open"))
    }

    fn enforce_output_limit(&mut self, maximum: usize) {
        while self.output_bytes > maximum {
            let Some(index) = self
                .events
                .iter()
                .position(|candidate| event_spool_bytes(candidate) > 0)
            else {
                break;
            };
            let removed = self
                .events
                .remove(index)
                .expect("the selected spool event exists");
            self.output_bytes = self
                .output_bytes
                .saturating_sub(event_spool_bytes(&removed));
        }
    }

    fn next_sequence(&mut self) -> u64 {
        let Some(command) = self.stored.command.as_mut() else {
            return 1;
        };
        command.sequence = command.sequence.saturating_add(1);
        command.sequence
    }

    fn finish(&mut self, code: i32) -> io::Result<()> {
        self.completion_override = None;
        let sequence = self.next_sequence();
        if let Some(command) = self.stored.command.as_mut() {
            command.status = StoredStatus::Completed { code };
            command.process_group = None;
            command.process_started = None;
        }
        self.append(WorkerEvent::Completed { sequence, code })?;
        self.persist()
    }
}

pub(crate) fn run(socket: PathBuf, state: PathBuf) -> io::Result<()> {
    ensure_private_directory(&state)?;
    let durability = configured_durability()?;
    let token = fs::read_to_string(state.join(AUTH_FILE))?.trim().to_owned();
    if token.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "worker authentication token is empty",
        ));
    }
    if socket.exists() {
        if UnixStream::connect(&socket).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "worker is running",
            ));
        }
        fs::remove_file(&socket)?;
    }

    let mut runtime = RuntimeState::load(state.clone(), durability)?;
    recover_incomplete_command(&mut runtime)?;
    let shared = Arc::new(Mutex::new(runtime));
    let shutting_down = Arc::new(AtomicBool::new(false));
    let last_activity = Arc::new(Mutex::new(Instant::now()));
    let login_environment = Arc::new(capture_login_environment()?);

    let owner = Owner {
        pid: std::process::id(),
        started: format!("start:{}", process_identity(std::process::id())?),
    };
    write_json_atomic(&state.join(OWNER_FILE), &owner, durability)?;
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;

    let result = loop {
        if shutting_down.load(Ordering::Acquire) {
            break Ok(());
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if let Ok(mut activity) = last_activity.lock() {
                    *activity = Instant::now();
                }
                let shared = Arc::clone(&shared);
                let shutting_down = Arc::clone(&shutting_down);
                let token = token.clone();
                let last_activity = Arc::clone(&last_activity);
                let login_environment = Arc::clone(&login_environment);
                thread::spawn(move || {
                    let _ = serve_connection(
                        stream,
                        &token,
                        shared,
                        shutting_down,
                        last_activity,
                        login_environment,
                    );
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if !socket.exists() {
                    break Ok(());
                }
                let idle = last_activity
                    .lock()
                    .is_ok_and(|activity| activity.elapsed() >= IDLE_TIMEOUT);
                let active = shared.lock().is_ok_and(|runtime| {
                    runtime.stored.command.as_ref().is_some_and(|command| {
                        matches!(
                            command.status,
                            StoredStatus::Dispatching | StoredStatus::Running
                        )
                    })
                });
                if idle && !active {
                    break Ok(());
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => break Err(error),
        }
    };
    let _ = fs::remove_file(&socket);
    let _ = fs::remove_file(state.join(OWNER_FILE));
    result
}

fn serve_connection(
    mut stream: UnixStream,
    token: &str,
    shared: Arc<Mutex<RuntimeState>>,
    shutting_down: Arc<AtomicBool>,
    last_activity: Arc<Mutex<Instant>>,
    login_environment: Arc<Vec<(OsString, OsString)>>,
) -> io::Result<()> {
    // Accepted Unix sockets inherit the listener's nonblocking flag on some
    // platforms. Each connection has a dedicated thread, so use blocking
    // framed reads after acceptance.
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    match read_frame::<WorkerRequest>(&mut stream)? {
        WorkerRequest::Hello { token: supplied } if supplied == token => {
            write_frame(&mut stream, &WorkerEvent::Ready)?;
        }
        WorkerRequest::Hello { .. } => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "worker authentication failed",
            ));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "worker handshake required",
            ));
        }
    }
    stream.set_read_timeout(None)?;
    if let Ok(mut activity) = last_activity.lock() {
        *activity = Instant::now();
    }
    match read_frame::<WorkerRequest>(&mut stream)? {
        WorkerRequest::Prepare | WorkerRequest::Release => {
            write_frame(&mut stream, &WorkerEvent::Ready)
        }
        WorkerRequest::Interrupt => {
            interrupt_active(&shared)?;
            write_frame(&mut stream, &WorkerEvent::Ready)
        }
        WorkerRequest::Shutdown => {
            terminate_active(&shared)?;
            shutting_down.store(true, Ordering::Release);
            write_frame(&mut stream, &WorkerEvent::Stopped)
        }
        WorkerRequest::Run {
            command_id,
            command,
            cwd,
            after,
        } => serve_run(
            stream,
            shared,
            command_id,
            command,
            cwd,
            after,
            login_environment,
        ),
        WorkerRequest::Hello { .. } => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker handshake was already completed",
        )),
    }
}

fn serve_run(
    mut stream: UnixStream,
    shared: Arc<Mutex<RuntimeState>>,
    command_id: u64,
    command: String,
    cwd: PathBuf,
    after: u64,
    login_environment: Arc<Vec<(OsString, OsString)>>,
) -> io::Result<()> {
    let (replay, receiver, completed, start) = {
        let mut runtime = shared
            .lock()
            .map_err(|_| io::Error::other("worker state lock poisoned"))?;
        let mut start = false;
        match runtime.stored.command.as_ref() {
            Some(existing)
                if existing.id == command_id
                    && (existing.command != command || existing.cwd != cwd) =>
            {
                return write_terminal_error(
                    &mut stream,
                    "command identity was reused with a different payload",
                );
            }
            Some(existing)
                if existing.id != command_id
                    && matches!(
                        existing.status,
                        StoredStatus::Dispatching | StoredStatus::Running
                    ) =>
            {
                return write_terminal_error(&mut stream, "worker is already running a command");
            }
            Some(existing)
                if existing.id != command_id && command_id <= runtime.stored.high_watermark =>
            {
                return write_terminal_error(&mut stream, "command identity has been retired");
            }
            Some(existing) if existing.id == command_id => {}
            _ => {
                runtime.stored.high_watermark = runtime.stored.high_watermark.max(command_id);
                runtime.stored.command = Some(StoredCommand {
                    id: command_id,
                    command: command.clone(),
                    cwd: cwd.clone(),
                    status: StoredStatus::Dispatching,
                    process_group: None,
                    process_started: None,
                    sequence: 0,
                });
                runtime.clear_spool()?;
                runtime.persist()?;
                start = true;
            }
        }
        let first_available = runtime.events.iter().find_map(event_sequence);
        let mut replay = Vec::new();
        if let Some(first_available) = first_available
            && after.saturating_add(1) < first_available
        {
            replay.push(WorkerEvent::OutputGap {
                requested_after: after,
                first_available,
            });
        }
        replay.extend(
            runtime
                .events
                .iter()
                .filter(|event| event_sequence(event).is_none_or(|sequence| sequence > after))
                .cloned(),
        );
        let completed = runtime
            .stored
            .command
            .as_ref()
            .is_some_and(|command| matches!(command.status, StoredStatus::Completed { .. }));
        let receiver = if completed {
            None
        } else {
            let (sender, receiver) = mpsc::sync_channel(SUBSCRIBER_QUEUE_CAPACITY);
            if runtime.subscribers.len() >= MAX_SUBSCRIBERS {
                runtime.subscribers.remove(0);
            }
            runtime.subscribers.push(sender);
            Some(receiver)
        };
        (replay, receiver, completed, start)
    };

    if start {
        spawn_active_command(
            Arc::clone(&shared),
            command,
            cwd,
            login_environment.as_ref(),
        )?;
    }
    for event in replay {
        write_frame(&mut stream, &event)?;
    }
    if completed {
        return Ok(());
    }
    let Some(receiver) = receiver else {
        return Ok(());
    };
    while let Ok(event) = receiver.recv() {
        write_frame(&mut stream, &event)?;
        if matches!(event, WorkerEvent::Completed { .. }) {
            break;
        }
    }
    Ok(())
}

fn spawn_active_command(
    shared: Arc<Mutex<RuntimeState>>,
    command: String,
    cwd: PathBuf,
    login_environment: &[(OsString, OsString)],
) -> io::Result<()> {
    let executable = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let (gate, durability) = {
        let runtime = shared
            .lock()
            .map_err(|_| io::Error::other("worker state lock poisoned"))?;
        let command_id = runtime
            .stored
            .command
            .as_ref()
            .map(|command| command.id)
            .ok_or_else(|| io::Error::other("worker command state is missing"))?;
        (
            runtime
                .directory
                .join(format!("dispatch-{command_id}.gate")),
            runtime.durability,
        )
    };
    let _ = fs::remove_file(&gate);
    let process_token = random_token(16)?;
    const GATED_EXEC: &str = "i=0; while [ ! -f \"$1\" ]; do i=$((i+1)); [ \"$i\" -ge 5000 ] && exit 125; sleep 0.001; done; \"$2\" -c \"$3\" \"$4\"; status=$?; exit \"$status\"";
    let mut child = match Command::new("/bin/sh")
        .arg("-c")
        .arg(GATED_EXEC)
        .arg("turtletap-dispatch")
        .arg(&gate)
        .arg(executable)
        .arg(&command)
        .arg(&process_token)
        .current_dir(&cwd)
        .env_clear()
        .envs(login_environment.iter().cloned())
        .env("PWD", &cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let mut runtime = shared
                .lock()
                .map_err(|_| io::Error::other("worker state lock poisoned"))?;
            runtime.append(WorkerEvent::Error {
                message: format!("could not start command: {error}"),
            })?;
            runtime.finish(126)?;
            return Ok(());
        }
    };
    let group = child.id();
    let started = Some(format!("argv:{process_token}"));
    let persist_error = {
        let mut runtime = shared
            .lock()
            .map_err(|_| io::Error::other("worker state lock poisoned"))?;
        if let Some(active) = runtime.stored.command.as_mut() {
            active.status = StoredStatus::Running;
            active.process_group = Some(group);
            active.process_started = started;
        }
        runtime.persist().err()
    };
    if let Some(error) = persist_error {
        let _ = terminate_child_group(&mut child);
        let mut runtime = shared
            .lock()
            .map_err(|_| io::Error::other("worker state lock poisoned"))?;
        runtime.append(WorkerEvent::Error {
            message: format!("could not persist command dispatch: {error}"),
        })?;
        runtime.finish(126)?;
        return Ok(());
    }
    let temporary_gate = gate.with_extension("gate.tmp");
    let _ = fs::remove_file(&temporary_gate);
    let gate_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary_gate);
    let gate_file = match gate_file {
        Ok(file) => file,
        Err(error) => {
            let _ = terminate_child_group(&mut child);
            let mut runtime = shared
                .lock()
                .map_err(|_| io::Error::other("worker state lock poisoned"))?;
            runtime.append(WorkerEvent::Error {
                message: format!("could not release durable command dispatch: {error}"),
            })?;
            runtime.finish(126)?;
            return Ok(());
        }
    };
    let release_result =
        durable_file(&gate_file, durability).and_then(|()| fs::rename(&temporary_gate, &gate));
    if let Err(error) = release_result {
        let _ = terminate_child_group(&mut child);
        let _ = fs::remove_file(&temporary_gate);
        let mut runtime = shared
            .lock()
            .map_err(|_| io::Error::other("worker state lock poisoned"))?;
        runtime.append(WorkerEvent::Error {
            message: format!("could not release durable command dispatch: {error}"),
        })?;
        runtime.finish(126)?;
        return Ok(());
    }

    let (sender, receiver) = mpsc::sync_channel(OUTPUT_QUEUE_CAPACITY);
    if let Some(stdout) = child.stdout.take() {
        spawn_output_reader(stdout, false, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_output_reader(stderr, true, sender.clone());
    }
    drop(sender);
    thread::spawn(move || monitor_command(shared, child, receiver, gate));
    Ok(())
}

fn spawn_output_reader(
    mut reader: impl Read + Send + 'static,
    stderr: bool,
    sender: mpsc::SyncSender<(bool, String)>,
) {
    thread::spawn(move || {
        let mut buffer = vec![0_u8; MAX_OUTPUT_CHUNK_BYTES];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    if sender.send((stderr, text)).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send((true, format!("output read error: {error}")));
                    break;
                }
            }
        }
    });
}

fn monitor_command(
    shared: Arc<Mutex<RuntimeState>>,
    mut child: Child,
    receiver: mpsc::Receiver<(bool, String)>,
    gate: PathBuf,
) {
    let mut status = None;
    let mut output_closed = false;
    loop {
        if !output_closed {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok((stderr, text)) => {
                    if record_output(&shared, stderr, &text).is_err() {
                        let _ = terminate_child_group(&mut child);
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => output_closed = true,
            }
        }
        if status.is_none() {
            status = child.try_wait().ok().flatten();
        }
        if status.is_some() && output_closed {
            break;
        }
        if output_closed {
            thread::sleep(Duration::from_millis(5));
        }
    }
    let status = status.or_else(|| child.wait().ok());
    let code = status.map_or(125, |status| {
        status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
    });
    if let Ok(mut runtime) = shared.lock() {
        let code = runtime.completion_override.take().unwrap_or(code);
        let _ = runtime.finish(code);
    }
    let _ = fs::remove_file(gate);
}

fn record_output(shared: &Arc<Mutex<RuntimeState>>, stderr: bool, text: &str) -> io::Result<()> {
    for part in text.split_inclusive('\n') {
        let clean = safe_terminal_text(part.trim_end_matches('\n'));
        if clean.is_empty() {
            continue;
        }
        for chunk in utf8_chunks(&clean, MAX_OUTPUT_CHUNK_BYTES) {
            let mut runtime = shared
                .lock()
                .map_err(|_| io::Error::other("worker state lock poisoned"))?;
            let sequence = runtime.next_sequence();
            runtime.append(WorkerEvent::Output {
                sequence,
                stderr,
                text: chunk.to_owned(),
            })?;
        }
    }
    Ok(())
}

fn recover_incomplete_command(runtime: &mut RuntimeState) -> io::Result<()> {
    let Some(command) = runtime.stored.command.clone() else {
        return Ok(());
    };
    if !matches!(
        command.status,
        StoredStatus::Dispatching | StoredStatus::Running
    ) {
        return Ok(());
    }
    if let (Some(group), Some(started)) =
        (command.process_group, command.process_started.as_deref())
    {
        terminate_verified_group(group, started)?;
    }
    runtime.append(WorkerEvent::Error {
        message: "worker restarted after dispatch; command was not re-executed and its outcome is unknown"
            .to_owned(),
    })?;
    runtime.finish(125)
}

fn interrupt_active(shared: &Arc<Mutex<RuntimeState>>) -> io::Result<()> {
    let identity = {
        let mut runtime = shared
            .lock()
            .map_err(|_| io::Error::other("worker state lock poisoned"))?;
        let identity = runtime.stored.command.as_ref().and_then(|command| {
            if matches!(
                command.status,
                StoredStatus::Dispatching | StoredStatus::Running
            ) {
                command.process_group.zip(command.process_started.clone())
            } else {
                None
            }
        });
        if identity.is_some() {
            runtime.completion_override = Some(130);
        }
        identity
    };
    if let Some((group, started)) = identity
        && let Err(error) = interrupt_verified_group(group, &started)
    {
        if let Ok(mut runtime) = shared.lock() {
            runtime.completion_override = None;
        }
        return Err(error);
    }
    Ok(())
}

fn terminate_active(shared: &Arc<Mutex<RuntimeState>>) -> io::Result<()> {
    let identity = active_identity(shared)?;
    if let Some((group, started)) = identity {
        terminate_verified_group(group, &started)?;
    }
    Ok(())
}

fn active_identity(shared: &Arc<Mutex<RuntimeState>>) -> io::Result<Option<(u32, String)>> {
    let runtime = shared
        .lock()
        .map_err(|_| io::Error::other("worker state lock poisoned"))?;
    Ok(runtime.stored.command.as_ref().and_then(|command| {
        if matches!(
            command.status,
            StoredStatus::Dispatching | StoredStatus::Running
        ) {
            command.process_group.zip(command.process_started.clone())
        } else {
            None
        }
    }))
}

fn terminate_verified_group(group: u32, started: &str) -> io::Result<()> {
    if !identity_matches(group, started) {
        return Ok(());
    }
    verify_group_leader(group)?;
    signal_group(group, "-TERM")?;
    if !wait_until(Duration::from_secs(1), || !group_has_live_processes(group)) {
        signal_group(group, "-KILL")?;
        if !wait_until(Duration::from_secs(1), || !group_has_live_processes(group)) {
            return Err(io::Error::other(
                "process group remained alive after SIGKILL",
            ));
        }
    }
    Ok(())
}

fn interrupt_verified_group(group: u32, started: &str) -> io::Result<()> {
    if !identity_matches(group, started) {
        return Ok(());
    }
    verify_group_leader(group)?;
    signal_group(group, "-INT")?;
    if group_has_live_processes(group) {
        signal_group(group, "-TERM")?;
    }
    if !wait_until(Duration::from_secs(1), || !group_has_live_processes(group)) {
        signal_group(group, "-KILL")?;
        if !wait_until(Duration::from_secs(1), || !group_has_live_processes(group)) {
            return Err(io::Error::other(
                "process group remained alive after interrupt escalation",
            ));
        }
    }
    Ok(())
}

fn verify_group_leader(group: u32) -> io::Result<()> {
    let observed_group = process_group(group)?;
    if observed_group != group {
        return Err(io::Error::other(
            "process is no longer the recorded process-group leader",
        ));
    }
    Ok(())
}

fn signal_group(group: u32, signal: &str) -> io::Result<()> {
    let status = Command::new("kill")
        .arg(signal)
        .arg("--")
        .arg(format!("-{group}"))
        .status()?;
    if status.success() || !group_has_live_processes(group) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not send {signal} to process group {group}"
        )))
    }
}

fn signal_verified_process(pid: u32, started: &str, signal: &str) -> io::Result<()> {
    if !identity_matches(pid, started) {
        return Ok(());
    }
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()?;
    if status.success() || !identity_matches(pid, started) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "could not send {signal} to process {pid}"
        )))
    }
}

fn terminate_child_group(child: &mut Child) -> io::Result<()> {
    let group = child.id();
    let status = Command::new("kill")
        .arg("-KILL")
        .arg("--")
        .arg(format!("-{group}"))
        .status()?;
    let _ = child.wait();
    if status.success() || !group_has_live_processes(group) {
        if wait_until(Duration::from_secs(1), || !group_has_live_processes(group)) {
            Ok(())
        } else {
            Err(io::Error::other(
                "command process group remained alive after SIGKILL",
            ))
        }
    } else {
        Err(io::Error::other(
            "could not terminate command process group",
        ))
    }
}

fn process_identity(pid: u32) -> io::Result<String> {
    let output = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()?;
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !identity.is_empty() {
        Ok(identity)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} is not running"),
        ))
    }
}

fn process_group(pid: u32) -> io::Result<u32> {
    let output = Command::new("ps")
        .args(["-o", "pgid=", "-p", &pid.to_string()])
        .output()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn group_has_live_processes(group: u32) -> bool {
    let Ok(output) = Command::new("ps").args(["-axo", "pgid=,stat="]).output() else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields
            .next()
            .and_then(|value| value.parse::<u32>().ok())
            .zip(fields.next())
            .is_some_and(|(observed, status)| observed == group && !status.starts_with('Z'))
    })
}

fn identity_matches(pid: u32, expected: &str) -> bool {
    if process_is_zombie(pid) {
        return false;
    }
    if let Some(token) = expected.strip_prefix("argv:") {
        return process_command(pid).is_ok_and(|command| command.contains(token));
    }
    let expected = expected.strip_prefix("start:").unwrap_or(expected);
    process_identity(pid).is_ok_and(|observed| observed == expected)
}

fn process_command(pid: u32) -> io::Result<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()?;
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && !command.is_empty() {
        Ok(command)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} is not running"),
        ))
    }
}

fn process_is_zombie(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .is_ok_and(|output| {
            String::from_utf8_lossy(&output.stdout)
                .trim()
                .starts_with('Z')
        })
}

fn wait_until(timeout: Duration, predicate: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    predicate()
}

fn configured_durability() -> io::Result<Durability> {
    let configured = env::var("TURTLETAP_WORKER_DURABILITY");
    match configured.as_deref() {
        Ok("fsync") => Ok(Durability::Fsync),
        Ok("flush") | Err(env::VarError::NotPresent) => Ok(Durability::Flush),
        Ok(value) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown worker durability {value:?}"),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            error.to_string(),
        )),
    }
}

fn capture_login_environment() -> io::Result<Vec<(OsString, OsString)>> {
    let executable = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let output = Command::new(executable)
        .arg("-l")
        .arg("-c")
        .arg("/usr/bin/env -0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Ok(env::vars_os().collect());
    };
    if !output.status.success() {
        return Ok(env::vars_os().collect());
    }
    let mut environment = Vec::new();
    for entry in output.stdout.split(|byte| *byte == 0) {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        environment.push((
            OsString::from_vec(entry[..separator].to_vec()),
            OsString::from_vec(entry[separator + 1..].to_vec()),
        ));
    }
    if environment.is_empty() {
        Ok(env::vars_os().collect())
    } else {
        Ok(environment)
    }
}

fn ensure_private_directory(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn ensure_auth_token(directory: &Path, durability: Durability) -> io::Result<String> {
    let path = directory.join(AUTH_FILE);
    match fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => return Ok(token.trim().to_owned()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "worker authentication token is empty",
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    let token = random_token(32)?;
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(0o600);
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.write_all(b"\n")?;
            durable_file(&file, durability)?;
            Ok(token)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::read_to_string(path)?;
            Ok(existing.trim().to_owned())
        }
        Err(error) => Err(error),
    }
}

fn random_token(bytes: usize) -> io::Result<String> {
    let mut random = vec![0_u8; bytes];
    File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>())
}

fn connect_authenticated(socket: &Path, token: &str) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    stream.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    write_frame(
        &mut stream,
        &WorkerRequest::Hello {
            token: token.to_owned(),
        },
    )?;
    expect_ready(&mut stream)?;
    stream.set_read_timeout(None)?;
    stream.set_write_timeout(None)?;
    Ok(stream)
}

fn connect_retry(socket: &Path, token: &str) -> io::Result<UnixStream> {
    let mut last = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match connect_authenticated(socket, token) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
        thread::sleep(CONNECT_DELAY);
    }
    Err(last.unwrap_or_else(|| io::Error::other("persistent worker did not start")))
}

fn expect_ready(stream: &mut UnixStream) -> io::Result<()> {
    match read_frame::<WorkerEvent>(stream)? {
        WorkerEvent::Ready => Ok(()),
        event => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("worker returned an unexpected handshake event: {event:?}"),
        )),
    }
}

fn worker_socket(session: SessionId) -> PathBuf {
    PathBuf::from("/tmp").join(format!("turtletap-worker-{session}.sock"))
}

fn live_worker_count() -> io::Result<usize> {
    Ok(fs::read_dir("/tmp")?
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            entry.file_name().to_str().is_some_and(|name| {
                name.starts_with("turtletap-worker-")
                    && name.ends_with(".sock")
                    && UnixStream::connect(path).is_ok()
            })
        })
        .count())
}

fn write_terminal_error(stream: &mut UnixStream, message: &str) -> io::Result<()> {
    write_frame(
        stream,
        &WorkerEvent::Error {
            message: message.to_owned(),
        },
    )?;
    write_frame(
        stream,
        &WorkerEvent::Completed {
            sequence: 0,
            code: 125,
        },
    )
}

fn event_sequence(event: &WorkerEvent) -> Option<u64> {
    match event {
        WorkerEvent::Output { sequence, .. } | WorkerEvent::Completed { sequence, .. } => {
            Some(*sequence)
        }
        _ => None,
    }
}

fn event_spool_bytes(event: &WorkerEvent) -> usize {
    serde_json::to_vec(event)
        .map_or(MAX_FRAME_BYTES, |bytes| bytes.len())
        .saturating_add(4)
}

fn utf8_chunks(value: &str, maximum: usize) -> impl Iterator<Item = &str> {
    let mut offset = 0;
    std::iter::from_fn(move || {
        if offset >= value.len() {
            return None;
        }
        let mut end = (offset + maximum).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &value[offset..end];
        offset = end;
        Some(chunk)
    })
}

fn safe_terminal_text(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            if matches!(characters.peek(), Some('[')) {
                let _ = characters.next();
                for next in characters.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if character == '\t' || !character.is_control() {
            output.push(character);
        }
    }
    output
}

pub(crate) fn write_frame(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "worker frame exceeds the 8 MiB limit",
        ));
    }
    writer.write_all(&(bytes.len() as u32).to_be_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()
}

pub(crate) fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker frame exceeds the 8 MiB limit",
        ));
    }
    let mut bytes = vec![0_u8; length];
    reader.read_exact(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn open_spool(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn load_spool(path: &Path) -> io::Result<VecDeque<WorkerEvent>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(VecDeque::new()),
        Err(error) => return Err(error),
    };
    let mut events = VecDeque::new();
    loop {
        match read_frame(&mut file) {
            Ok(event) => events.push_back(event),
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(error),
        }
    }
    Ok(events)
}

fn rewrite_spool(
    path: &Path,
    events: &VecDeque<WorkerEvent>,
    durability: Durability,
) -> io::Result<()> {
    let temporary = path.with_extension("frames.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    for event in events {
        write_frame(&mut file, event)?;
    }
    durable_file(&file, durability)?;
    fs::rename(temporary, path)
}

fn write_json_atomic(
    path: &Path,
    value: &impl Serialize,
    durability: Durability,
) -> io::Result<()> {
    let temporary = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    durable_file(&file, durability)?;
    fs::rename(temporary, path)
}

fn durable_file(mut file: &File, durability: Durability) -> io::Result<()> {
    file.flush()?;
    if durability == Durability::Fsync {
        file.sync_data()?;
    }
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd as _;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system time should follow the Unix epoch")
                .as_nanos();
            let path =
                env::temp_dir().join(format!("turtletap-worker-{}-{nonce}", std::process::id()));
            fs::create_dir(&path).expect("worker test directory should be created");
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn framing_rejects_oversized_payload_before_allocation() {
        let prefix = ((MAX_FRAME_BYTES as u32) + 1).to_be_bytes();
        let mut bytes = prefix.as_slice();
        let error = read_frame::<WorkerEvent>(&mut bytes).expect_err("oversized frame must fail");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn framing_roundtrips_embedded_newlines_and_control_like_text() {
        let event = WorkerEvent::Output {
            sequence: 7,
            stderr: false,
            text: "\u{1e}TT_DONE_spoof\n{\"type\":\"completed\"}".to_owned(),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &event).expect("frame should serialize");
        let decoded: WorkerEvent =
            read_frame(&mut bytes.as_slice()).expect("frame should deserialize");
        assert!(matches!(
            decoded,
            WorkerEvent::Output {
                sequence: 7,
                text,
                ..
            } if text.contains("TT_DONE_spoof")
        ));
    }

    #[test]
    fn output_chunks_respect_the_utf8_byte_limit() {
        let text = "🦀".repeat(MAX_OUTPUT_CHUNK_BYTES);
        let chunks = utf8_chunks(&text, MAX_OUTPUT_CHUNK_BYTES).collect::<Vec<_>>();
        assert!(chunks.len() > 1);
        assert!(
            chunks
                .iter()
                .all(|chunk| chunk.len() <= MAX_OUTPUT_CHUNK_BYTES)
        );
        assert_eq!(chunks.concat(), text);
    }

    #[test]
    fn spool_writer_is_reused_and_recovery_reads_every_complete_frame() {
        let directory = TestDirectory::new();
        let mut runtime =
            RuntimeState::load(directory.0.clone(), Durability::Flush).expect("spool should open");
        runtime
            .append(WorkerEvent::Output {
                sequence: 1,
                stderr: false,
                text: "first".to_owned(),
            })
            .expect("first frame should append");
        let first_descriptor = runtime
            .spool
            .as_ref()
            .expect("append should retain the spool")
            .as_raw_fd();
        runtime
            .append(WorkerEvent::Output {
                sequence: 2,
                stderr: true,
                text: "second".to_owned(),
            })
            .expect("second frame should append");
        assert_eq!(
            runtime
                .spool
                .as_ref()
                .expect("spool should remain open")
                .as_raw_fd(),
            first_descriptor
        );
        drop(runtime);

        let recovered = load_spool(&directory.0.join(SPOOL_FILE))
            .expect("complete spool frames should recover");
        assert_eq!(recovered.len(), 2);
        assert!(matches!(
            recovered.back(),
            Some(WorkerEvent::Output {
                sequence: 2,
                stderr: true,
                text,
            }) if text == "second"
        ));
    }

    #[test]
    fn spool_retention_drops_oldest_output_to_stay_bounded() {
        let output = |sequence, text: &str| WorkerEvent::Output {
            sequence,
            stderr: false,
            text: text.to_owned(),
        };
        let events = VecDeque::from([output(1, "old1"), output(2, "old2"), output(3, "new3")]);
        let retained_limit = events.iter().skip(1).map(event_spool_bytes).sum();
        let mut runtime = RuntimeState {
            stored: StoredState::default(),
            output_bytes: events.iter().map(event_spool_bytes).sum(),
            events,
            subscribers: Vec::new(),
            completion_override: None,
            directory: PathBuf::new(),
            durability: Durability::Flush,
            spool: None,
        };

        runtime.enforce_output_limit(retained_limit);

        assert_eq!(runtime.output_bytes, retained_limit);
        assert_eq!(runtime.events.len(), 2);
        assert!(matches!(
            runtime.events.front(),
            Some(WorkerEvent::Output { sequence: 2, .. })
        ));
    }
}
