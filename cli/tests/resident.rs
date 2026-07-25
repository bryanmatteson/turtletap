//! Process-level resident supervision and protocol tests.

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Output},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use turtletap::resident::{
    AttachmentMode, ClientCapabilities, ClientEnvelope, ClientHello, ClientInstanceId,
    ClientRequest, ControlResult, LeaseEpoch, MAX_FRAME_SIZE, PROTOCOL_VERSION, ProtocolVersion,
    RequestId, ServerHandshake, ServerHello, ServerMessage, SessionId, SessionSelector,
    VersionRange, encode_frame,
};

struct ResidentSession {
    _test_guard: MutexGuard<'static, ()>,
    directory: PathBuf,
    socket: PathBuf,
    state: PathBuf,
}

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);
static RESIDENT_TEST: Mutex<()> = Mutex::new(());

fn resident_test_guard() -> MutexGuard<'static, ()> {
    RESIDENT_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn worker_connection(socket: &PathBuf, token: &str) -> UnixStream {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match UnixStream::connect(socket) {
            Ok(mut stream) => {
                write_value(
                    &mut stream,
                    &serde_json::json!({"type": "hello", "token": token}),
                );
                let ready: serde_json::Value = read_value(&mut stream);
                assert_eq!(ready["type"], "ready");
                return stream;
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("worker should accept a connection: {error}"),
        }
    }
}

fn worker_events(mut stream: UnixStream) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    loop {
        let event: serde_json::Value = read_value(&mut stream);
        let completed = event["type"] == "completed";
        events.push(event);
        if completed {
            break;
        }
    }
    events
}

fn process_exists(pid: u32) -> bool {
    let exists = Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !exists {
        return false;
    }
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map_or(true, |output| {
            !output.status.success()
                || !String::from_utf8_lossy(&output.stdout)
                    .trim()
                    .starts_with('Z')
        })
}

fn worker_command_is_running(state: &Path, command_id: u64) -> bool {
    fs::read_to_string(state.join("state-v2.json"))
        .ok()
        .and_then(|stored| serde_json::from_str::<serde_json::Value>(&stored).ok())
        .is_some_and(|stored| {
            stored["command"]["id"].as_u64() == Some(command_id)
                && stored["command"]["status"] == "running"
        })
}

#[test]
fn persistent_worker_survives_interrupt_and_worker_crash() {
    let _test_guard = resident_test_guard();
    let nonce = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
    let directory = PathBuf::from("/tmp").join(format!("tt-worker-{}-{nonce}", std::process::id()));
    fs::create_dir(&directory).expect("worker test directory should be created");
    let socket = directory.join("worker.sock");
    let state = directory.join("state");
    fs::create_dir(&state).expect("worker state directory should be created");
    let token = "direct-worker-test-token";
    fs::write(state.join("auth-v1"), format!("{token}\n"))
        .expect("worker authentication token should be written");
    let mut worker = Command::new(env!("CARGO_BIN_EXE_turtletap"))
        .args([
            "__shell-worker",
            "test",
            socket.to_str().expect("socket path should be UTF-8"),
            state.to_str().expect("state path should be UTF-8"),
        ])
        .spawn()
        .expect("worker should spawn");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(socket.exists(), "worker socket did not appear");
    let mut unauthenticated =
        UnixStream::connect(&socket).expect("worker should accept an authentication probe");
    write_value(
        &mut unauthenticated,
        &serde_json::json!({"type": "prepare"}),
    );
    assert!(
        try_read_value::<serde_json::Value>(&mut unauthenticated).is_err(),
        "worker must reject requests before authentication"
    );
    let mut prepared = loop {
        match UnixStream::connect(&socket) {
            Ok(mut stream) => {
                write_value(
                    &mut stream,
                    &serde_json::json!({"type": "hello", "token": token}),
                );
                let ready: serde_json::Value = read_value(&mut stream);
                assert_eq!(ready["type"], "ready");
                break stream;
            }
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => panic!("worker did not accept preparation request: {error}"),
        }
    };
    write_value(&mut prepared, &serde_json::json!({ "type": "prepare" }));
    let ready: serde_json::Value = read_value(&mut prepared);
    assert_eq!(ready["type"], "ready");

    let mut interrupted = worker_connection(&socket, token);
    write_value(
        &mut interrupted,
        &serde_json::json!({
            "type": "run",
            "command_id": 1,
            "command": "sleep 5; printf should-not-print",
            "cwd": "/tmp",
            "after": 0,
        }),
    );
    thread::sleep(Duration::from_millis(100));
    let mut control = worker_connection(&socket, token);
    write_value(&mut control, &serde_json::json!({ "type": "interrupt" }));
    let ready: serde_json::Value = read_value(&mut control);
    assert_eq!(ready["type"], "ready");
    let interrupted = worker_events(interrupted);
    assert!(
        interrupted
            .iter()
            .all(|event| event["text"] != "should-not-print")
    );
    assert_eq!(
        interrupted.last().and_then(|event| event["code"].as_i64()),
        Some(130)
    );

    let mut reused = worker_connection(&socket, token);
    write_value(
        &mut reused,
        &serde_json::json!({
            "type": "run",
            "command_id": 2,
            "command": "printf reusable",
            "cwd": "/tmp",
            "after": 0,
        }),
    );
    let reused = worker_events(reused);
    assert!(reused.iter().any(|event| event["text"] == "reusable"));
    assert_eq!(
        reused.last().and_then(|event| event["code"].as_i64()),
        Some(0)
    );

    let mut multiline = worker_connection(&socket, token);
    write_value(
        &mut multiline,
        &serde_json::json!({
            "type": "run",
            "command_id": 3,
            "command": "printf 'one\\ntwo\\n'",
            "cwd": "/tmp",
            "after": 0,
        }),
    );
    let multiline = worker_events(multiline);
    let lines: Vec<_> = multiline
        .iter()
        .filter_map(|event| event["text"].as_str())
        .filter(|text| matches!(*text, "one" | "two"))
        .collect();
    assert_eq!(lines, ["one", "two"]);
    assert!(
        multiline.iter().all(|event| {
            event["text"]
                .as_str()
                .is_none_or(|text| !text.contains("TT_DONE_") && !text.contains("TT_PID_"))
        }),
        "worker markers must not reach the transcript: {multiline:?}"
    );

    let marker = directory.join("worker-crash-escape");
    let release = directory.join("worker-crash-release");
    let crash_command = format!(
        "/bin/sh -c 'while [ ! -f {} ]; do sleep 0.01; done; printf escaped > {}'",
        release.display(),
        marker.display()
    );
    let mut crashing = worker_connection(&socket, token);
    write_value(
        &mut crashing,
        &serde_json::json!({
            "type": "run",
            "command_id": 4,
            "command": crash_command,
            "cwd": "/tmp",
            "after": 0,
        }),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker_command_is_running(&state, 4) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        worker_command_is_running(&state, 4),
        "crash fixture did not reach durable running state"
    );
    worker.kill().expect("worker crash should be injected");
    worker.wait().expect("crashed worker should be reaped");
    worker = Command::new(env!("CARGO_BIN_EXE_turtletap"))
        .args([
            "__shell-worker",
            "test",
            socket.to_str().expect("socket path should be UTF-8"),
            state.to_str().expect("state path should be UTF-8"),
        ])
        .spawn()
        .expect("replacement worker should spawn");
    let mut recovered = worker_connection(&socket, token);
    write_value(
        &mut recovered,
        &serde_json::json!({
            "type": "run",
            "command_id": 4,
            "command": crash_command,
            "cwd": "/tmp",
            "after": 0,
        }),
    );
    let recovered = worker_events(recovered);
    assert_eq!(
        recovered.last().and_then(|event| event["code"].as_i64()),
        Some(125),
        "worker crash must report an unknown outcome without redispatch"
    );
    fs::write(&release, "release\n").expect("crash command gate should open");
    thread::sleep(Duration::from_millis(250));
    assert!(
        !marker.exists(),
        "the replacement worker did not terminate the orphaned command group"
    );

    let mut shutdown = worker_connection(&socket, token);
    write_value(&mut shutdown, &serde_json::json!({"type": "shutdown"}));
    let stopped: serde_json::Value = read_value(&mut shutdown);
    assert_eq!(stopped["type"], "stopped");
    let _ = worker.wait();
    let _ = fs::remove_dir_all(directory);
}

impl ResidentSession {
    fn stopped() -> Self {
        let test_guard = resident_test_guard();
        let nonce = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from("/tmp").join(format!("tt-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).expect("test socket directory should be created");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("test directory permissions should be set");
        Self {
            _test_guard: test_guard,
            socket: directory.join("resident.sock"),
            state: directory.join("state"),
            directory,
        }
    }

    fn start() -> Self {
        let session = Self::stopped();
        assert_success(&session.command(&["start"]), "start");
        session
    }

    fn command(&self, arguments: &[&str]) -> Output {
        let mut command = self.command_builder();
        command
            .args(arguments)
            .output()
            .expect("turtletap command should run")
    }

    fn spawn(&self, arguments: &[&str]) -> Child {
        let mut command = self.command_builder();
        command
            .args(arguments)
            .spawn()
            .expect("turtletap command should spawn")
    }

    fn command_builder(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_turtletap"));
        command
            .env("TURTLETAP_SOCKET", &self.socket)
            .env("TURTLETAP_STATE_DIR", &self.state)
            .env("XDG_CONFIG_HOME", self.directory.join("config"))
            .env_remove("TURTLETAP_CONFIG");
        command
    }

    fn client(&self) -> TestClient {
        TestClient::connect(&self.socket)
    }

    fn pid(&self) -> u32 {
        let status = self.command(&["status"]);
        assert_success(&status, "status");
        let status: serde_json::Value =
            serde_json::from_slice(&status.stdout).expect("captured status should be JSON");
        status["pid"]
            .as_u64()
            .and_then(|pid| u32::try_from(pid).ok())
            .expect("status should contain a numeric PID")
    }

    fn wait_stopped(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.socket.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ResidentSession {
    fn drop(&mut self) {
        let _ = self.command(&["stop"]);
        self.wait_stopped();
        let _ = fs::remove_dir_all(&self.directory);
    }
}

struct TestClient {
    stream: UnixStream,
    instance: ClientInstanceId,
    next_request: u64,
    messages: Vec<ServerMessage>,
}

impl TestClient {
    fn connect(path: &PathBuf) -> Self {
        Self::connect_as(path, ClientInstanceId::new())
    }

    fn connect_as(path: &PathBuf, instance: ClientInstanceId) -> Self {
        let mut stream = UnixStream::connect(path).expect("resident should accept a client");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout should be set");
        write_value(
            &mut stream,
            &ClientHello {
                protocol: VersionRange::current(),
                binary_version: env!("CARGO_PKG_VERSION").to_owned(),
                client_instance: instance,
                client_name: "resident-test".to_owned(),
                capabilities: ClientCapabilities {
                    incremental_events: true,
                    resumable: true,
                    driver_leases: true,
                },
            },
        );
        let hello: ServerHello = read_value(&mut stream);
        assert_eq!(hello.protocol, PROTOCOL_VERSION);
        Self {
            stream,
            instance,
            next_request: 1,
            messages: Vec::new(),
        }
    }

    fn request(&mut self, message: ClientRequest) -> Result<ControlResult, String> {
        let request = RequestId {
            client: self.instance,
            sequence: self.next_request,
        };
        self.next_request += 1;
        self.request_with_id(request, message)
    }

    fn request_with_id(
        &mut self,
        request: RequestId,
        message: ClientRequest,
    ) -> Result<ControlResult, String> {
        write_value(&mut self.stream, &ClientEnvelope { request, message });
        loop {
            let message: ServerMessage = read_value(&mut self.stream);
            match message {
                ServerMessage::Response {
                    request: response,
                    result,
                } if response == request => {
                    return result.map_err(|error| format!("{}: {}", error.code, error.message));
                }
                message => self.messages.push(message),
            }
        }
    }

    fn attach(&mut self, mode: AttachmentMode, force: bool) -> (SessionId, Option<LeaseEpoch>) {
        let result = self
            .request(ClientRequest::Attach {
                session: SessionSelector::Name("default".to_owned()),
                mode,
                after: None,
                force,
            })
            .expect("attach should succeed");
        let ControlResult::Attached { session, lease } = result else {
            panic!("wrong attach response");
        };
        (session.id, lease)
    }

    fn snapshot(&mut self, session: SessionId) -> serde_json::Value {
        if let Some(index) = self.messages.iter().position(|message| {
            matches!(message, ServerMessage::Snapshot { session: id, .. } if *id == session)
        }) {
            let ServerMessage::Snapshot { state, .. } = self.messages.remove(index) else {
                unreachable!();
            };
            return state;
        }
        loop {
            let message: ServerMessage = read_value(&mut self.stream);
            if let ServerMessage::Snapshot {
                session: id, state, ..
            } = message
            {
                if id == session {
                    return state;
                }
            } else {
                self.messages.push(message);
            }
        }
    }
}

#[test]
fn external_command_output_reaches_a_reconnected_snapshot() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({"type": "submit", "line": "printf worker-visible"}),
        })
        .expect("external command should be accepted");
    thread::sleep(Duration::from_millis(500));

    let mut observer = resident.client();
    let (observed, _) = observer.attach(AttachmentMode::View, false);
    assert_eq!(observed, session);
    let snapshot = observer.snapshot(session);
    assert!(
        snapshot["transcript"]
            .as_array()
            .is_some_and(|entries| entries
                .iter()
                .any(|entry| entry["text"] == "worker-visible")),
        "worker output was not committed to the resident snapshot: {snapshot}"
    );
}

#[test]
fn active_command_reconnects_after_leader_crash_without_reexecution() {
    let resident = ResidentSession::start();
    let original = resident.pid();
    let release = resident.directory.join("active-recovery-release");
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({
                "type": "submit",
                "line": format!(
                    "/bin/sh -c 'printf recovery-before; while [ ! -f {} ]; do sleep 0.01; done; printf recovery-after'",
                    release.display()
                )
            }),
        })
        .expect("active command should be accepted");

    let worker_state = resident
        .state
        .join(session.to_string())
        .join("worker")
        .join("state-v2.json");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fs::read_to_string(&worker_state).is_ok_and(|state| state.contains("\"running\""))
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        fs::read_to_string(&worker_state).is_ok_and(|state| state.contains("\"running\"")),
        "worker command did not reach durable running state"
    );
    let killed = Command::new("kill")
        .args(["-KILL", &original.to_string()])
        .status()
        .expect("leader kill should run");
    assert!(killed.success());
    thread::sleep(Duration::from_millis(100));
    assert_success(&resident.command(&["start"]), "start replacement");
    fs::write(&release, "release\n").expect("active command gate should open");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let mut observer = resident.client();
        let (observed, _) = observer.attach(AttachmentMode::View, false);
        assert_eq!(observed, session);
        let snapshot = observer.snapshot(session);
        let transcript = snapshot["transcript"]
            .as_array()
            .expect("transcript should be an array");
        let before = transcript
            .iter()
            .filter(|entry| entry["text"] == "recovery-before")
            .count();
        let after = transcript
            .iter()
            .filter(|entry| entry["text"] == "recovery-after")
            .count();
        if before == 1 && after == 1 && snapshot["running"] == false {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "replacement did not reconnect and replay active output exactly once: {snapshot}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn deleting_an_active_session_terminates_its_process_group_before_state_removal() {
    let resident = ResidentSession::start();
    let marker = resident.directory.join("escaped-command");
    let release = resident.directory.join("delete-release");
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({
                "type": "submit",
                "line": format!(
                    "/bin/sh -c 'while [ ! -f {} ]; do sleep 0.01; done; printf escaped > {}'",
                    release.display(),
                    marker.display()
                )
            }),
        })
        .expect("active command should be accepted");
    let worker_socket = PathBuf::from("/tmp").join(format!("turtletap-worker-{session}.sock"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker_socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(worker_socket.exists(), "session worker did not start");

    client
        .request(ClientRequest::StopSession { session })
        .expect("active session deletion should stop its worker first");
    assert!(
        !resident.state.join(session.to_string()).exists(),
        "session state remained after cleanup was acknowledged"
    );
    assert!(
        !worker_socket.exists(),
        "worker socket remained after session deletion"
    );
    fs::write(&release, "release\n").expect("deleted command gate should open");
    thread::sleep(Duration::from_millis(250));
    assert!(
        !marker.exists(),
        "a descendant escaped after active session deletion"
    );
}

#[test]
fn stopping_the_leader_terminates_active_worker_groups() {
    let resident = ResidentSession::start();
    let leader = resident.pid();
    let marker = resident.directory.join("escaped-stop");
    let release = resident.directory.join("stop-release");
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({
                "type": "submit",
                "line": format!(
                    "/bin/sh -c 'while [ ! -f {} ]; do sleep 0.01; done; printf escaped > {}'",
                    release.display(),
                    marker.display()
                )
            }),
        })
        .expect("active command should be accepted");
    let worker_socket = PathBuf::from("/tmp").join(format!("turtletap-worker-{session}.sock"));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker_socket.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(worker_socket.exists(), "session worker did not start");

    assert_success(&resident.command(&["stop"]), "stop active leader");
    resident.wait_stopped();
    assert!(!process_exists(leader), "resident leader remained alive");
    assert!(
        !worker_socket.exists(),
        "worker socket remained after manual leader stop"
    );
    fs::write(&release, "release\n").expect("stopped command gate should open");
    thread::sleep(Duration::from_millis(250));
    assert!(
        !marker.exists(),
        "a descendant escaped after manual leader stop"
    );
}

#[test]
fn committed_submit_is_deduplicated_after_a_crash() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let instance = client.instance;
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    let request = RequestId {
        client: instance,
        sequence: 80_000,
    };
    let command = serde_json::json!({"type": "submit", "line": ":add crashsafe printf once"});
    let first = client
        .request_with_id(
            request,
            ClientRequest::Command {
                session,
                lease: lease.expect("driver lease"),
                command: command.clone(),
            },
        )
        .expect("submit before failure should commit");
    assert!(matches!(
        first,
        ControlResult::Accepted {
            duplicate: false,
            ..
        }
    ));

    let pid = resident.pid();
    assert!(
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("kill should run")
            .success()
    );
    drop(client);
    thread::sleep(Duration::from_millis(100));
    assert_success(
        &resident.command(&["start"]),
        "restart after committed submit",
    );

    let mut retried = TestClient::connect_as(&resident.socket, instance);
    let (restored_session, restored_lease) = retried.attach(AttachmentMode::Drive, false);
    assert_eq!(restored_session, session);
    let result = retried
        .request_with_id(
            request,
            ClientRequest::Command {
                session,
                lease: restored_lease.expect("restored driver lease"),
                command,
            },
        )
        .expect("ambiguous submit should be retryable");
    assert!(matches!(
        result,
        ControlResult::Accepted {
            duplicate: true,
            ..
        }
    ));
    let snapshot = retried.snapshot(session);
    assert_eq!(snapshot["history"].as_array().map(Vec::len), Some(1));
}

#[test]
fn submit_can_be_retried_after_arriving_during_leader_failure() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let instance = client.instance;
    let (session, _) = client.attach(AttachmentMode::Drive, false);
    let request = RequestId {
        client: instance,
        sequence: 90_000,
    };
    let pid = resident.pid();
    assert!(
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("kill should run")
            .success()
    );
    let envelope = ClientEnvelope {
        request,
        message: ClientRequest::Command {
            session,
            lease: LeaseEpoch(1),
            command: serde_json::json!({"type": "submit", "line": ":add during printf once"}),
        },
    };
    let frame = encode_frame(&envelope).expect("failed-delivery envelope should encode");
    let failed_delivery = client
        .stream
        .write_all(&frame)
        .and_then(|()| client.stream.flush())
        .and_then(|()| try_read_value::<ServerMessage>(&mut client.stream).map(|_| ()));
    assert!(
        failed_delivery.is_err(),
        "delivery to a failed leader must not report success"
    );
    drop(client);
    thread::sleep(Duration::from_millis(100));
    assert_success(
        &resident.command(&["start"]),
        "restart after failed delivery",
    );

    let mut retried = TestClient::connect_as(&resident.socket, instance);
    let (session, lease) = retried.attach(AttachmentMode::Drive, false);
    let result = retried
        .request_with_id(
            request,
            ClientRequest::Command {
                session,
                lease: lease.expect("driver lease"),
                command: serde_json::json!({"type": "submit", "line": ":add during printf once"}),
            },
        )
        .expect("request should apply after recovery");
    assert!(matches!(
        result,
        ControlResult::Accepted {
            duplicate: false,
            ..
        }
    ));
}

#[cfg(target_os = "macos")]
#[test]
fn bare_command_starts_and_opens_the_dashboard() {
    let resident = ResidentSession::stopped();
    let script = format!(
        "set stty_init \"rows 30 columns 120\"\n\
         log_user 0\n\
         set timeout 10\n\
         spawn env SHELL=/bin/sh TURTLETAP_SOCKET={} TURTLETAP_STATE_DIR={} XDG_CONFIG_HOME={} TURTLETAP_CONFIG= {}\n\
         after 500\n\
         send -- \"\\007\"\n\
         after 100\n\
         send -- \"d\"\n\
         expect {{\n\
           eof {{}}\n\
           timeout {{ exit 124 }}\n\
         }}\n",
        resident.socket.display(),
        resident.state.display(),
        resident.directory.join("config").display(),
        env!("CARGO_BIN_EXE_turtletap"),
    );
    let status = Command::new("/usr/bin/expect")
        .args(["-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("expect should drive the bare command");
    assert!(status.success(), "bare command failed with {status}");

    let _pid = resident.pid();
}

#[cfg(target_os = "macos")]
#[test]
fn ctrl_backtick_terminal_encoding_enters_the_action_bar() {
    let resident = ResidentSession::stopped();
    let script = format!(
        "log_user 0\n\
         set timeout 10\n\
         spawn env SHELL=/bin/sh TURTLETAP_SOCKET={} TURTLETAP_STATE_DIR={} XDG_CONFIG_HOME={} TURTLETAP_CONFIG= {}\n\
         after 500\n\
         send -null\n\
         after 100\n\
         send -- \"\\033d\"\n\
         expect {{\n\
           eof {{}}\n\
           timeout {{ exit 124 }}\n\
         }}\n",
        resident.socket.display(),
        resident.state.display(),
        resident.directory.join("config").display(),
        env!("CARGO_BIN_EXE_turtletap"),
    );
    let status = Command::new("/usr/bin/expect")
        .args(["-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("expect should drive Ctrl-backtick");
    assert!(
        status.success(),
        "Ctrl-backtick did not enter the action bar, where Alt-D detaches: {status}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn ctrl_backtick_enhanced_terminal_encoding_enters_the_action_bar() {
    let resident = ResidentSession::stopped();
    let script = format!(
        "log_user 0\n\
         set timeout 10\n\
         spawn env SHELL=/bin/sh TURTLETAP_SOCKET={} TURTLETAP_STATE_DIR={} XDG_CONFIG_HOME={} TURTLETAP_CONFIG= {}\n\
         after 500\n\
         send -- \"\\033\\[96;5u\"\n\
         after 100\n\
         send -- \"\\033d\"\n\
         expect {{\n\
           eof {{}}\n\
           timeout {{ exit 124 }}\n\
         }}\n",
        resident.socket.display(),
        resident.state.display(),
        resident.directory.join("config").display(),
        env!("CARGO_BIN_EXE_turtletap"),
    );
    let status = Command::new("/usr/bin/expect")
        .args(["-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("expect should drive enhanced Ctrl-backtick");
    assert!(
        status.success(),
        "enhanced Ctrl-backtick did not enter the action bar, where Alt-D detaches: {status}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn escape_on_an_empty_prompt_enters_the_action_bar() {
    let resident = ResidentSession::stopped();
    let script = format!(
        "log_user 0\n\
         set timeout 10\n\
         spawn env SHELL=/bin/sh TURTLETAP_SOCKET={} TURTLETAP_STATE_DIR={} XDG_CONFIG_HOME={} TURTLETAP_CONFIG= {} attach default\n\
         after 500\n\
         send -- \"\\033\"\n\
         after 100\n\
         send -- \"\\033d\"\n\
         expect {{\n\
           eof {{}}\n\
           timeout {{ exit 124 }}\n\
         }}\n",
        resident.socket.display(),
        resident.state.display(),
        resident.directory.join("config").display(),
        env!("CARGO_BIN_EXE_turtletap"),
    );
    let status = Command::new("/usr/bin/expect")
        .args(["-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .expect("expect should drive empty-prompt Escape");
    assert!(
        status.success(),
        "empty-prompt Escape did not enter the action bar, where Alt-D detaches: {status}"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn attached_tui_reconnects_and_accepts_input_after_leader_crash() {
    let resident = ResidentSession::start();
    let pid = resident.pid();
    let ready = resident.directory.join("tui-ready");
    let terminal_log = resident.directory.join("tui.log");
    let script = format!(
        "log_user 1\n\
         log_file -noappend {}\n\
         set timeout 15\n\
         spawn env TURTLETAP_SOCKET={} TURTLETAP_STATE_DIR={} {} attach default\n\
         after 500\n\
         send -- \"printf before\\r\"\n\
         after 500\n\
         exec touch {}\n\
         after 4000\n\
         send -- \"printf after\\r\"\n\
         after 1000\n\
         send -- \"\\007\"\n\
         after 100\n\
         send -- \"d\"\n\
         expect {{\n\
           eof {{}}\n\
           timeout {{ exit 124 }}\n\
         }}\n",
        terminal_log.display(),
        resident.socket.display(),
        resident.state.display(),
        env!("CARGO_BIN_EXE_turtletap"),
        ready.display(),
    );
    let mut attached = Command::new("/usr/bin/expect")
        .args(["-c", &script])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("expect should start the attached TUI");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !ready.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(ready.exists(), "attached TUI did not become ready");

    assert!(
        Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status()
            .expect("kill should run")
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while attached
        .try_wait()
        .expect("attached TUI status should be readable")
        .is_none()
        && Instant::now() < deadline
    {
        thread::sleep(Duration::from_millis(20));
    }
    if attached
        .try_wait()
        .expect("attached TUI status should be readable")
        .is_none()
    {
        let _ = attached.kill();
        let _ = attached.wait();
        panic!("attached TUI did not detach within twenty seconds");
    }
    let status = attached.wait().expect("attached TUI should finish");
    assert!(status.success(), "attached TUI failed with status {status}");
    assert_ne!(
        resident.pid(),
        pid,
        "the attached TUI should replace the leader"
    );
    let mut observer = resident.client();
    let (session, _) = observer.attach(AttachmentMode::View, false);
    let snapshot = observer.snapshot(session);
    assert!(
        snapshot["history"]
            .as_array()
            .is_some_and(|history| history.iter().any(|line| line == "printf after")),
        "post-reconnect input was not accepted: {snapshot}"
    );
}

#[cfg(target_os = "macos")]
fn latency_budget(variable: &str, default: u64) -> u64 {
    match std::env::var(variable) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{variable} must be an unsigned integer")),
        Err(std::env::VarError::NotPresent) => default,
        Err(std::env::VarError::NotUnicode(_)) => {
            panic!("{variable} must contain valid UTF-8")
        }
    }
}

#[cfg(target_os = "macos")]
#[test]
fn command_latency_stays_within_the_responsiveness_budgets() {
    if std::env::var_os("TURTLETAP_LATENCY_TEST_CHILD").is_none() {
        let _test_guard = resident_test_guard();
        let executable = std::env::current_exe().expect("test executable should resolve");
        let mut failures = String::new();
        for attempt in 1..=3 {
            let output = Command::new(&executable)
                .args([
                    "--exact",
                    "command_latency_stays_within_the_responsiveness_budgets",
                    "--nocapture",
                ])
                .env("TURTLETAP_LATENCY_TEST_CHILD", "1")
                .output()
                .expect("isolated latency test should start");
            if output.status.success() {
                return;
            }
            failures.push_str(&format!(
                "\nattempt {attempt}:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
        panic!("isolated latency test failed three sampling windows:{failures}");
    }

    const SAMPLES: usize = 20;
    const DEFAULT_ENTER_TO_OUTPUT_P95_BUDGET_MS: u64 = 100;
    const DEFAULT_OUTPUT_TO_SCREEN_P95_BUDGET_MS: u64 = 16;
    let enter_to_output_budget = latency_budget(
        "TURTLETAP_ENTER_TO_OUTPUT_P95_BUDGET_MS",
        DEFAULT_ENTER_TO_OUTPUT_P95_BUDGET_MS,
    );
    let output_to_screen_budget = latency_budget(
        "TURTLETAP_OUTPUT_TO_SCREEN_P95_BUDGET_MS",
        DEFAULT_OUTPUT_TO_SCREEN_P95_BUDGET_MS,
    );

    let resident = ResidentSession::start();
    let script = format!(
        "set stty_init \"rows 30 columns 120\"\n\
         log_user 0\n\
         set timeout 5\n\
         spawn env SHELL=/bin/sh TURTLETAP_SOCKET={} TURTLETAP_STATE_DIR={} XDG_CONFIG_HOME={} TURTLETAP_CONFIG= {} attach default\n\
         after 400\n\
         for {{set i 0}} {{$i < {SAMPLES}}} {{incr i}} {{\n\
           set suffix [format \"%03d\" $i]\n\
           set command \"printf Q%s $suffix\"\n\
           send -- $command\n\
           after 100\n\
           set started [clock milliseconds]\n\
           send -- \"\\r\"\n\
           expect {{\n\
             -exact \"$ $command\" {{}}\n\
             timeout {{ exit 124 }}\n\
           }}\n\
           expect {{\n\
             -exact \"Q$suffix\" {{\n\
               puts \"enter:[expr {{[clock milliseconds] - $started}}]\"\n\
             }}\n\
             timeout {{ exit 125 }}\n\
           }}\n\
           after 50\n\
         }}\n\
         for {{set i 0}} {{$i < {SAMPLES}}} {{incr i}} {{\n\
           set suffix [format \"%03d\" $i]\n\
           set command \"'{}' __latency_probe $suffix\"\n\
           send -- $command\n\
           after 100\n\
           send -- \"\\r\"\n\
           expect {{\n\
             -exact \"TT_PROBE $suffix \" {{}}\n\
             timeout {{ exit 130 }}\n\
           }}\n\
           expect {{\n\
             -re {{([0-9]{{13}})}} {{\n\
               puts \"screen:[expr {{[clock milliseconds] - $expect_out(1,string)}}]\"\n\
             }}\n\
             timeout {{ exit 129 }}\n\
           }}\n\
           after 50\n\
         }}\n\
         send -- \"\\007\"\n\
         after 50\n\
         send -- \"d\"\n\
         expect {{\n\
           eof {{}}\n\
           timeout {{ exit 126 }}\n\
         }}\n",
        resident.socket.display(),
        resident.state.display(),
        resident.directory.join("config").display(),
        env!("CARGO_BIN_EXE_turtletap"),
        env!("CARGO_BIN_EXE_turtletap"),
    );
    let output = Command::new("/usr/bin/expect")
        .args(["-c", &script])
        .output()
        .expect("expect should measure the attached TUI");
    assert!(
        output.status.success(),
        "latency probe failed with {}: {}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout),
    );
    let output = String::from_utf8(output.stdout).expect("latency samples should be UTF-8");
    let mut enter_samples: Vec<u64> = output
        .lines()
        .filter_map(|line| line.strip_prefix("enter:"))
        .map(|line| line.parse().expect("enter latency should be numeric"))
        .collect();
    let mut screen_samples: Vec<u64> = output
        .lines()
        .filter_map(|line| line.strip_prefix("screen:"))
        .map(|line| line.parse().expect("screen latency should be numeric"))
        .collect();
    assert_eq!(enter_samples.len(), SAMPLES);
    assert_eq!(screen_samples.len(), SAMPLES);
    enter_samples.sort_unstable();
    screen_samples.sort_unstable();
    let p95_index = SAMPLES.saturating_mul(95).div_ceil(100).saturating_sub(1);
    let enter_p95 = enter_samples[p95_index];
    let screen_p95 = screen_samples[p95_index];
    eprintln!(
        "latency samples: enter-to-output={enter_samples:?} ms, output-to-screen={screen_samples:?} ms"
    );
    assert!(
        enter_p95 <= enter_to_output_budget,
        "enter-to-output p95 {enter_p95} ms exceeded {enter_to_output_budget} ms; samples={enter_samples:?}"
    );
    assert!(
        screen_p95 <= output_to_screen_budget,
        "output-to-screen p95 {screen_p95} ms exceeded {output_to_screen_budget} ms; samples={screen_samples:?}"
    );
}

#[test]
fn retrying_an_ambiguous_request_executes_it_once() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    let request = RequestId {
        client: client.instance,
        sequence: 50_000,
    };
    let message = ClientRequest::Command {
        session,
        lease: lease.expect("driver lease"),
        command: serde_json::json!({"type": "submit", "line": ":add once printf once"}),
    };
    let first = client
        .request_with_id(request, message.clone())
        .expect("first delivery");
    let retried = client
        .request_with_id(request, message)
        .expect("retried delivery");
    assert!(matches!(
        first,
        ControlResult::Accepted {
            duplicate: false,
            ..
        }
    ));
    assert!(matches!(
        retried,
        ControlResult::Accepted {
            duplicate: true,
            ..
        }
    ));

    let mut viewer = resident.client();
    let (session, _) = viewer.attach(AttachmentMode::View, false);
    let snapshot = viewer.snapshot(session);
    assert_eq!(snapshot["history"].as_array().map(Vec::len), Some(1));
}

#[test]
fn state_survives_disconnect_and_graceful_leader_restart() {
    let resident = ResidentSession::start();
    let mode = fs::metadata(&resident.directory)
        .expect("test directory metadata should be readable")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o755, "existing socket-parent permissions changed");
    let state_mode = fs::metadata(&resident.state)
        .expect("state directory metadata should be readable")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(state_mode, 0o700, "resident state must be private");

    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    let command = serde_json::json!({
        "type": "submit",
        "line": ":add greet printf hello"
    });
    let accepted = client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command,
        })
        .expect("command should be accepted");
    assert!(matches!(
        accepted,
        ControlResult::Accepted {
            duplicate: false,
            ..
        }
    ));
    drop(client);

    assert_success(&resident.command(&["stop"]), "stop");
    resident.wait_stopped();
    assert_success(&resident.command(&["start"]), "restart");

    let mut restored = resident.client();
    let (session, _) = restored.attach(AttachmentMode::Drive, false);
    let snapshot = restored.snapshot(session);
    assert_eq!(snapshot["commands"][0], "greet");
    assert_eq!(snapshot["history"][0], ":add greet printf hello");
}

#[test]
fn multiple_viewers_and_forced_driver_takeover_are_fenced() {
    let resident = ResidentSession::start();
    let mut first = resident.client();
    let (session, first_lease) = first.attach(AttachmentMode::Drive, false);
    let mut second = resident.client();
    let (same_session, no_lease) = second.attach(AttachmentMode::View, false);
    assert_eq!(same_session, session);
    assert_eq!(no_lease, None);

    let takeover = second
        .request(ClientRequest::AcquireDriver {
            session,
            force: true,
        })
        .expect("takeover should succeed");
    let ControlResult::Driver {
        lease: Some(current),
        ..
    } = takeover
    else {
        panic!("takeover should return a lease");
    };
    assert!(current.epoch > first_lease.expect("first lease"));

    let rejected = first.request(ClientRequest::Command {
        session,
        lease: first_lease.expect("first lease"),
        command: serde_json::json!({"type": "clear"}),
    });
    assert!(
        rejected
            .expect_err("old driver must be fenced")
            .contains("driver")
    );
}

#[test]
fn simultaneous_starters_elect_one_leader() {
    let resident = ResidentSession::start();
    assert_success(&resident.command(&["stop"]), "initial stop");
    resident.wait_stopped();

    let mut starters: Vec<_> = (0..8).map(|_| resident.spawn(&["start"])).collect();
    for starter in &mut starters {
        let status = starter.wait().expect("starter should exit");
        assert!(status.success(), "concurrent starter failed: {status}");
    }
    let first_pid = resident.pid();
    let second_pid = resident.pid();
    assert_eq!(first_pid, second_pid);
}

#[test]
fn oversized_handshake_does_not_kill_the_leader() {
    let resident = ResidentSession::start();
    let mut stream = UnixStream::connect(&resident.socket).expect("socket should accept");
    let oversized = u32::try_from(MAX_FRAME_SIZE + 1).expect("frame limit should fit u32");
    stream
        .write_all(&oversized.to_be_bytes())
        .expect("oversized prefix should write");
    drop(stream);
    thread::sleep(Duration::from_millis(50));

    let status = resident.command(&["status"]);
    assert_success(&status, "status after malformed peer");
}

#[test]
fn incompatible_handshake_is_explicitly_rejected() {
    let resident = ResidentSession::start();
    let mut stream = UnixStream::connect(&resident.socket).expect("socket should accept");
    write_value(
        &mut stream,
        &ClientHello {
            protocol: VersionRange {
                minimum: ProtocolVersion(99),
                maximum: ProtocolVersion(99),
            },
            binary_version: "future-client".to_owned(),
            client_instance: ClientInstanceId::new(),
            client_name: "incompatible-test".to_owned(),
            capabilities: ClientCapabilities::default(),
        },
    );
    let handshake: ServerHandshake = read_value(&mut stream);
    let ServerHandshake::Rejected(rejection) = handshake else {
        panic!("incompatible client should be rejected");
    };
    assert!(rejection.rejected);
    assert_eq!(rejection.supported, VersionRange::current());
    assert_success(&resident.command(&["status"]), "status after rejection");
}

#[test]
fn only_a_newer_compatible_binary_can_replace_the_leader() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let rejected = client.request(ClientRequest::ReplaceLeader {
        binary_version: "0.0.1".to_owned(),
    });
    assert!(
        rejected
            .expect_err("older replacement should be rejected")
            .contains("will not be replaced")
    );
    assert!(resident.socket.exists());

    let accepted = client
        .request(ClientRequest::ReplaceLeader {
            binary_version: "99.0.0".to_owned(),
        })
        .expect("newer replacement should be accepted");
    assert_eq!(accepted, ControlResult::Stopping);
    drop(client);
    resident.wait_stopped();
    assert!(!resident.socket.exists());
    assert_success(&resident.command(&["start"]), "replacement start");
}

#[test]
fn journal_replays_changes_newer_than_the_checkpoint() {
    let resident = ResidentSession::start();
    let session_directory = only_session_directory(&resident.state);
    let checkpoint = session_directory.join("checkpoint.json");
    let stale_checkpoint = fs::read(&checkpoint).expect("initial checkpoint should be readable");

    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({"type": "submit", "line": ":add replayed printf replayed"}),
        })
        .expect("command should be accepted");
    drop(client);
    assert_success(&resident.command(&["stop"]), "stop");
    resident.wait_stopped();
    fs::write(&checkpoint, stale_checkpoint).expect("checkpoint should be rewound");

    assert_success(&resident.command(&["start"]), "restart");
    let mut restored = resident.client();
    let (session, _) = restored.attach(AttachmentMode::View, false);
    let snapshot = restored.snapshot(session);
    assert_eq!(snapshot["commands"][0], "replayed");
}

#[test]
fn corrupt_checkpoint_recovers_from_manifest_and_journal() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({"type": "submit", "line": ":add recovered printf recovered"}),
        })
        .expect("command should be accepted");
    drop(client);
    assert_success(&resident.command(&["stop"]), "stop");
    resident.wait_stopped();

    let checkpoint = only_session_directory(&resident.state).join("checkpoint.json");
    fs::write(&checkpoint, b"not json\n").expect("checkpoint should be corrupted");
    assert_success(&resident.command(&["start"]), "restart after corruption");

    let mut restored = resident.client();
    let (session, _) = restored.attach(AttachmentMode::View, false);
    let snapshot = restored.snapshot(session);
    assert_eq!(snapshot["commands"][0], "recovered");
}

#[test]
fn version_zero_checkpoint_is_migrated_in_place() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let (session, lease) = client.attach(AttachmentMode::Drive, false);
    client
        .request(ClientRequest::Command {
            session,
            lease: lease.expect("driver lease"),
            command: serde_json::json!({"type": "submit", "line": ":add migrated printf migrated"}),
        })
        .expect("command should be accepted");
    drop(client);
    assert_success(&resident.command(&["stop"]), "stop");
    resident.wait_stopped();

    let checkpoint = only_session_directory(&resident.state).join("checkpoint.json");
    let mut legacy: serde_json::Value =
        serde_json::from_slice(&fs::read(&checkpoint).expect("checkpoint should be readable"))
            .expect("checkpoint should be JSON");
    let object = legacy
        .as_object_mut()
        .expect("checkpoint should be an object");
    object.remove("host_version");
    object.remove("application_version");
    fs::write(
        &checkpoint,
        serde_json::to_vec(&legacy).expect("legacy checkpoint should encode"),
    )
    .expect("legacy checkpoint should write");

    assert_success(&resident.command(&["start"]), "migrate restart");
    let mut restored = resident.client();
    let (session, _) = restored.attach(AttachmentMode::View, false);
    let snapshot = restored.snapshot(session);
    assert_eq!(snapshot["commands"][0], "migrated");

    let migrated: serde_json::Value = serde_json::from_slice(
        &fs::read(&checkpoint).expect("migrated checkpoint should be readable"),
    )
    .expect("migrated checkpoint should be JSON");
    assert_eq!(migrated["host_version"], 2);
    assert_eq!(migrated["application_version"], 2);
}

#[test]
fn root_only_legacy_session_is_imported_into_per_session_storage() {
    let resident = ResidentSession::start();
    let mut client = resident.client();
    let created = client
        .request(ClientRequest::CreateSession {
            name: "legacy-only".to_owned(),
        })
        .expect("legacy session should be created");
    let ControlResult::Created { session } = created else {
        panic!("wrong create result");
    };
    drop(client);
    assert_success(&resident.command(&["stop"]), "stop");
    resident.wait_stopped();

    let session_directory = resident.state.join(session.id.to_string());
    let checkpoint: serde_json::Value = serde_json::from_slice(
        &fs::read(session_directory.join("checkpoint.json"))
            .expect("session checkpoint should be readable"),
    )
    .expect("session checkpoint should be JSON");
    fs::write(
        resident.state.join("checkpoint.json"),
        serde_json::to_vec(&serde_json::json!({
            "sessions": [{
                "control": checkpoint["control"],
                "state": checkpoint["state"]
            }]
        }))
        .expect("legacy root should encode"),
    )
    .expect("legacy root should write");
    fs::remove_dir_all(&session_directory).expect("per-session state should be removed");

    assert_success(&resident.command(&["start"]), "legacy root import");
    let sessions = resident.command(&["list"]);
    assert_success(&sessions, "list after legacy root import");
    assert!(
        String::from_utf8_lossy(&sessions.stdout).contains("legacy-only"),
        "legacy-only session was not imported"
    );
    assert!(
        session_directory.join("checkpoint.json").exists(),
        "legacy session was not migrated to per-session storage"
    );
    assert!(
        !resident.state.join("checkpoint.json").exists(),
        "legacy root should be archived after a successful import"
    );
    assert!(
        fs::read_dir(&resident.state)
            .expect("state directory should be readable")
            .filter_map(Result::ok)
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .starts_with("checkpoint.legacy-v0-")),
        "legacy root archive should be retained"
    );

    assert_success(
        &resident.command(&["delete", "legacy-only", "--yes"]),
        "delete imported session",
    );
    assert_success(&resident.command(&["stop"]), "stop after delete");
    resident.wait_stopped();
    assert_success(&resident.command(&["start"]), "restart after delete");
    let sessions = resident.command(&["list"]);
    assert_success(&sessions, "list after deleting imported session");
    assert!(
        !String::from_utf8_lossy(&sessions.stdout).contains("legacy-only"),
        "deleted imported session should not be resurrected"
    );
}

#[test]
fn killed_leader_is_replaced_and_sessions_restore() {
    let resident = ResidentSession::start();
    let original = resident.pid();
    let killed = Command::new("kill")
        .args(["-9", &original.to_string()])
        .status()
        .expect("kill should run");
    assert!(killed.success());
    thread::sleep(Duration::from_millis(100));

    assert_success(&resident.command(&["start"]), "start after crash");
    let replacement = resident.pid();
    assert_ne!(replacement, original);
    let mut client = resident.client();
    let (_session, lease) = client.attach(AttachmentMode::Drive, false);
    assert!(lease.is_some());
}

fn write_value(stream: &mut UnixStream, value: &impl serde::Serialize) {
    let frame = encode_frame(value).expect("value should encode");
    stream.write_all(&frame).expect("frame should write");
    stream.flush().expect("frame should flush");
}

fn read_value<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> T {
    try_read_value(stream).expect("frame should read")
}

fn try_read_value<T: serde::de::DeserializeOwned>(stream: &mut UnixStream) -> std::io::Result<T> {
    let mut length = [0; 4];
    stream.read_exact(&mut length)?;
    let size = u32::from_be_bytes(length) as usize;
    let mut payload = vec![0; size];
    stream.read_exact(&mut payload)?;
    serde_json::from_slice(&payload).map_err(std::io::Error::other)
}

fn assert_success(output: &Output, command: &str) {
    assert!(
        output.status.success(),
        "turtletap {command} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn only_session_directory(state: &PathBuf) -> PathBuf {
    let directories: Vec<_> = fs::read_dir(state)
        .expect("state directory should be readable")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    assert_eq!(directories.len(), 1, "expected one resident session");
    directories.into_iter().next().expect("one directory")
}
