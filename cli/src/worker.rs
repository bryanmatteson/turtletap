//! Persistent per-session login-shell worker.

use std::{
    collections::HashMap,
    env,
    fs::{self, File},
    io::{self, Read, Write},
    os::unix::{
        fs::PermissionsExt as _,
        net::{UnixListener, UnixStream},
        process::CommandExt as _,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use turtletap::resident::{EffectContext, EffectWake, SessionId};

use crate::command::{RunningCommand, running_from_worker};

const CONNECT_ATTEMPTS: usize = 100;
const MAX_COMMAND_BYTES: usize = 1024 * 1024;
const MAX_WORKERS: usize = 32;
const IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WorkerRequest {
    Run {
        command_id: String,
        command: String,
        cwd: PathBuf,
    },
    Interrupt,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WorkerEvent {
    Output {
        sequence: u64,
        stderr: bool,
        text: String,
    },
    Completed {
        sequence: u64,
        code: i32,
    },
    Error {
        message: String,
    },
}

#[derive(Clone)]
pub(crate) struct WorkerManager {
    state_root: PathBuf,
}

impl WorkerManager {
    pub(crate) fn new(state_root: PathBuf) -> Self {
        Self { state_root }
    }

    pub(crate) fn execute(
        &self,
        context: &EffectContext,
        command: &str,
        cwd: &Path,
        wake: Option<EffectWake>,
    ) -> io::Result<RunningCommand> {
        if command.len() > MAX_COMMAND_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "command exceeds the persistent worker size limit",
            ));
        }
        let socket = worker_socket(context.session);
        let mut stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(_) => {
                self.spawn(context.session, &socket)?;
                connect_retry(&socket)?
            }
        };
        write_message(
            &mut stream,
            &WorkerRequest::Run {
                command_id: context.effect.to_string(),
                command: command.to_owned(),
                cwd: cwd.to_owned(),
            },
        )?;
        running_from_worker(stream, wake)
    }

    fn spawn(&self, session: SessionId, socket: &Path) -> io::Result<()> {
        if live_worker_count()? >= MAX_WORKERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "worker_capacity_exhausted: all persistent worker slots are busy",
            ));
        }
        let state = self.state_root.join(session.to_string());
        fs::create_dir_all(&state)?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700))?;
        Command::new(env::current_exe()?)
            .arg("__shell-worker")
            .arg(session.to_string())
            .arg(socket)
            .arg(state)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()?;
        Ok(())
    }
}

fn live_worker_count() -> io::Result<usize> {
    let mut live = 0;
    for entry in fs::read_dir("/tmp")? {
        let path = entry?.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("turtletap-worker-") && name.ends_with(".sock"))
            && UnixStream::connect(path).is_ok()
        {
            live += 1;
        }
    }
    Ok(live)
}

fn worker_socket(session: SessionId) -> PathBuf {
    // macOS commonly supplies a long per-user TMPDIR that leaves too little
    // room under sockaddr_un::sun_path. Session IDs make this short /tmp name
    // collision-resistant while keeping it below every supported SUN_LEN.
    PathBuf::from("/tmp").join(format!("turtletap-worker-{session}.sock"))
}

fn connect_retry(path: &Path) -> io::Result<UnixStream> {
    let mut last = None;
    for _ in 0..CONNECT_ATTEMPTS {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
        thread::sleep(Duration::from_millis(2));
    }
    Err(last.unwrap_or_else(|| io::Error::other("persistent worker did not start")))
}

pub(crate) fn run(socket: PathBuf, state: PathBuf) -> io::Result<()> {
    if socket.exists() {
        match UnixStream::connect(&socket) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "worker is running",
                ));
            }
            Err(_) => fs::remove_file(&socket)?,
        }
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let result = worker_loop(listener, &state);
    let _ = fs::remove_file(&socket);
    result
}

struct ShellProcess {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<ShellOutput>,
    dialect: ShellDialect,
}

#[derive(Clone, Copy)]
enum ShellDialect {
    Posix,
    Fish,
}

struct ShellOutput {
    stderr: bool,
    text: String,
}

fn worker_loop(listener: UnixListener, state: &Path) -> io::Result<()> {
    let mut shell = start_shell()?;
    let mut completed: HashMap<String, Vec<WorkerEvent>> = HashMap::new();
    fs::create_dir_all(state)?;
    listener.set_nonblocking(true)?;
    let mut last_activity = Instant::now();
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if last_activity.elapsed() >= IDLE_TIMEOUT {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(1));
                continue;
            }
            Err(error) => return Err(error),
        };
        let first = read_request_line(&mut stream)?;
        let Ok(request) = serde_json::from_str::<WorkerRequest>(&first) else {
            // Capacity probes and clients that disconnect before completing a
            // request must not take down the session's persistent shell.
            continue;
        };
        let WorkerRequest::Run {
            command_id,
            command,
            cwd,
        } = request
        else {
            continue;
        };
        last_activity = Instant::now();
        let spool = state.join(format!("{}.json", safe_id(&command_id)));
        let dispatched = state.join(format!("{}.dispatched", safe_id(&command_id)));
        if !completed.contains_key(&command_id)
            && let Ok(bytes) = fs::read(&spool)
            && let Ok(events) = serde_json::from_slice(&bytes)
        {
            completed.insert(command_id.clone(), events);
        }
        if let Some(events) = completed.get(&command_id) {
            for event in events {
                write_message(&mut stream, event)?;
            }
            continue;
        }
        if dispatched.exists() {
            let events = vec![
                WorkerEvent::Error {
                    message: "worker stopped after dispatch; command was not re-executed"
                        .to_owned(),
                },
                WorkerEvent::Completed {
                    sequence: 1,
                    code: 125,
                },
            ];
            for event in &events {
                write_message(&mut stream, event)?;
            }
            completed.insert(command_id, events);
            continue;
        }
        let dispatch = File::create(&dispatched)?;
        dispatch.sync_all()?;
        let events = execute_one(&mut shell, stream, &command_id, &command, &cwd)?;
        if let Ok(serialized) = serde_json::to_vec(&events) {
            let _ = fs::write(spool, serialized);
        }
        completed.insert(command_id, events);
    }
}

fn read_request_line(stream: &mut UnixStream) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() <= MAX_COMMAND_BYTES {
        match stream.read(&mut byte)? {
            0 => break,
            1 if byte[0] == b'\n' => break,
            1 => bytes.push(byte[0]),
            _ => unreachable!(),
        }
    }
    if bytes.len() > MAX_COMMAND_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "worker request exceeds its size limit",
        ));
    }
    String::from_utf8(bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn start_shell() -> io::Result<ShellProcess> {
    let executable = env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let dialect = Path::new(&executable)
        .file_stem()
        .and_then(|name| name.to_str())
        .map_or(ShellDialect::Posix, |name| {
            if name == "fish" {
                ShellDialect::Fish
            } else {
                ShellDialect::Posix
            }
        });
    let runner = match dialect {
        ShellDialect::Fish => "while read -l __tt_line; eval $__tt_line; end",
        ShellDialect::Posix => "while IFS= read -r __tt_line; do eval \"$__tt_line\"; done",
    };
    let mut child = Command::new(executable)
        .arg("-l")
        .arg("-c")
        .arg(runner)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()?;
    let input = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("persistent shell stdin unavailable"))?;
    let (sender, output) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        spawn_shell_reader(stdout, false, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        spawn_shell_reader(stderr, true, sender);
    }
    let mut shell = ShellProcess {
        child,
        input,
        output,
        dialect,
    };
    initialize_shell(&mut shell)?;
    Ok(shell)
}

fn initialize_shell(shell: &mut ShellProcess) -> io::Result<()> {
    const READY: &str = "\u{1e}TT_WORKER_READY";
    let bootstrap = match shell.dialect {
        ShellDialect::Posix => "printf '\\036TT_WORKER_READY\\n'\n",
        ShellDialect::Fish => "printf '\\036TT_WORKER_READY\\n'\n",
    };
    shell.input.write_all(bootstrap.as_bytes())?;
    shell.input.flush()?;
    let mut observed = Vec::new();
    loop {
        let output = shell
            .output
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("persistent shell bootstrap timed out after {observed:?}"),
                )
            })?;
        if !output.stderr && output.text.contains(READY) {
            return Ok(());
        }
        observed.push((output.stderr, output.text));
    }
}

fn spawn_shell_reader(
    mut reader: impl io::Read + Send + 'static,
    stderr: bool,
    sender: mpsc::Sender<ShellOutput>,
) {
    thread::spawn(move || {
        let mut buffer = [0_u8; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    if sender.send(ShellOutput { stderr, text }).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(ShellOutput {
                        stderr,
                        text: format!("output read error: {error}"),
                    });
                    break;
                }
            }
        }
    });
}

fn execute_one(
    shell: &mut ShellProcess,
    mut stream: UnixStream,
    command_id: &str,
    command: &str,
    cwd: &Path,
) -> io::Result<Vec<WorkerEvent>> {
    let marker_name = format!("TT_DONE_{}", safe_id(command_id));
    let marker = format!("\u{1e}{marker_name}");
    let marker_literal = format!("\\036{marker_name}");
    let pid_marker_name = format!("TT_PID_{}", safe_id(command_id));
    let pid_marker = format!("\u{1e}{pid_marker_name}:");
    let pid_marker_literal = format!("\\036{pid_marker_name}");
    let cwd = shell_quote(&cwd.to_string_lossy());
    let command = shell_quote(command);
    let fish_command = shell_quote(&format!("cd -- {cwd}; and eval {command}"));
    let wrapper = match shell.dialect {
        ShellDialect::Posix => format!(
            "( trap - INT; cd -- {cwd} && eval -- {command} ) </dev/null & __tt_pid=$!; printf '{pid_marker_literal}:%s\\n' \"$__tt_pid\"; wait \"$__tt_pid\"; __tt_status=$?; printf '{marker_literal}:%s\\n' \"$__tt_status\"\n"
        ),
        ShellDialect::Fish => format!(
            "command $SHELL -c {fish_command} </dev/null &; set __tt_pid $last_pid; printf '{pid_marker_literal}:%s\\n' $__tt_pid; wait $__tt_pid; set __tt_status $status; printf '{marker_literal}:%s\\n' $__tt_status\n"
        ),
    };
    shell.input.write_all(wrapper.as_bytes())?;
    shell.input.flush()?;

    let control_stream = stream.try_clone()?;
    control_stream.set_nonblocking(true)?;
    let mut control = control_stream;
    let mut control_bytes = Vec::new();

    let mut sequence = 0_u64;
    let mut events = Vec::new();
    let mut active_group = None;
    let mut interrupted = false;
    let mut stdout_buffer = String::new();
    loop {
        let mut chunk = [0_u8; 256];
        loop {
            match control.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => control_bytes.extend_from_slice(&chunk[..read]),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        while let Some(end) = control_bytes.iter().position(|byte| *byte == b'\n') {
            let line: Vec<_> = control_bytes.drain(..=end).collect();
            if serde_json::from_slice::<WorkerRequest>(&line)
                .is_ok_and(|request| matches!(request, WorkerRequest::Interrupt))
            {
                interrupted = true;
                if let Some(pid) = active_group {
                    let _ = signal_process(pid, "-KILL");
                }
            }
        }
        let output = match shell.output.recv_timeout(Duration::from_millis(10)) {
            Ok(output) => output,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shell.child.try_wait()?.is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "persistent shell exited",
                    ));
                }
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "persistent shell output closed",
                ));
            }
        };
        if !output.stderr {
            stdout_buffer.push_str(&output.text);
            if let Some(index) = stdout_buffer.find(&pid_marker)
                && let Some(line_end) = stdout_buffer[index..].find('\n')
            {
                let line_end = index + line_end;
                let prefix = stdout_buffer[..index].to_owned();
                let pid = stdout_buffer[index + pid_marker.len()..line_end]
                    .trim_start_matches(':')
                    .trim()
                    .parse()
                    .ok();
                stdout_buffer.drain(..=line_end);
                if !prefix.is_empty() {
                    record_output(&mut stream, &mut sequence, &mut events, false, &prefix);
                }
                active_group = pid;
                if interrupted && let Some(pid) = active_group {
                    let _ = signal_process(pid, "-KILL");
                }
                continue;
            }
            if let Some(index) = stdout_buffer.find(&marker)
                && let Some(line_end) = stdout_buffer[index..].find('\n')
            {
                let line_end = index + line_end;
                let prefix = stdout_buffer[..index].to_owned();
                let mut code = stdout_buffer[index + marker.len()..line_end]
                    .trim_start_matches(':')
                    .trim()
                    .parse()
                    .unwrap_or(1);
                stdout_buffer.drain(..=line_end);
                if !prefix.is_empty() {
                    record_output(&mut stream, &mut sequence, &mut events, false, &prefix);
                }
                if interrupted {
                    code = 130;
                }
                sequence = sequence.saturating_add(1);
                let event = WorkerEvent::Completed { sequence, code };
                let _ = write_message(&mut stream, &event);
                events.push(event);
                return Ok(events);
            }

            if let Some(index) = stdout_buffer.find('\u{1e}') {
                let candidate = &stdout_buffer[index..];
                let marker_may_continue =
                    pid_marker.starts_with(candidate) || marker.starts_with(candidate);
                if index > 0 {
                    let prefix: String = stdout_buffer.drain(..index).collect();
                    record_output(&mut stream, &mut sequence, &mut events, false, &prefix);
                } else if !marker_may_continue {
                    let prefix: String = stdout_buffer.drain(..1).collect();
                    record_output(&mut stream, &mut sequence, &mut events, false, &prefix);
                }
                continue;
            }

            if !stdout_buffer.is_empty() {
                let text = std::mem::take(&mut stdout_buffer);
                record_output(&mut stream, &mut sequence, &mut events, false, &text);
            }
            continue;
        }
        record_output(
            &mut stream,
            &mut sequence,
            &mut events,
            output.stderr,
            &output.text,
        );
    }
}

fn record_output(
    stream: &mut UnixStream,
    sequence: &mut u64,
    events: &mut Vec<WorkerEvent>,
    stderr: bool,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    for line in text.split_terminator('\n') {
        *sequence = sequence.saturating_add(1);
        let event = WorkerEvent::Output {
            sequence: *sequence,
            stderr,
            text: safe_terminal_text(line),
        };
        let _ = write_message(stream, &event);
        events.push(event);
    }
}

fn signal_process(pid: u32, signal: &str) -> io::Result<()> {
    let status = Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| io::Error::other("could not signal worker command"))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn safe_id(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .collect()
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

fn write_message(writer: &mut impl Write, value: &impl Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value).map_err(io::Error::other)?;
    writer.write_all(b"\n")?;
    writer.flush()
}
