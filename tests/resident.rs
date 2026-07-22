//! Process-level resident supervision and protocol tests.

#![cfg(unix)]

use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::PathBuf,
    process::{Child, Command, Output},
    sync::atomic::{AtomicU64, Ordering},
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
    directory: PathBuf,
    socket: PathBuf,
    state: PathBuf,
}

static NEXT_TEST: AtomicU64 = AtomicU64::new(1);

impl ResidentSession {
    fn start() -> Self {
        let nonce = NEXT_TEST.fetch_add(1, Ordering::Relaxed);
        let directory = PathBuf::from("/tmp").join(format!("tt-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).expect("test socket directory should be created");
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755))
            .expect("test directory permissions should be set");
        let session = Self {
            socket: directory.join("resident.sock"),
            state: directory.join("state"),
            directory,
        };
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
            .env("TURTLETAP_STATE_DIR", &self.state);
        command
    }

    fn client(&self) -> TestClient {
        TestClient::connect(&self.socket)
    }

    fn pid(&self) -> u32 {
        let status = self.command(&["status"]);
        assert_success(&status, "status");
        String::from_utf8(status.stdout)
            .expect("status output should be UTF-8")
            .lines()
            .find_map(|line| line.strip_prefix("PID: "))
            .expect("status should contain a PID")
            .parse()
            .expect("PID should be numeric")
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
    assert_eq!(migrated["host_version"], 1);
    assert_eq!(migrated["application_version"], 1);
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
