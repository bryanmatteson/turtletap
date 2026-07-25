//! Integration tests for the resident library's *public* API.
//!
//! Unlike `cli/tests/resident.rs`, which spawns the binary and speaks the wire
//! protocol by hand, these drive a `ResidentHost` and `ResidentClient` in
//! process — the surface a library consumer actually uses. A toy counter
//! application stands in for a real product.

#![cfg(all(unix, feature = "tokio"))]

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use turtletap::resident::{
    ApplicationError, AttachmentMode, ClientCapabilities, ClientRequest, ControlResult,
    EffectContext, EffectRequest, LeaseEpoch, ResidentApplication, ResidentHost,
    ResidentHostConfig, ResidentSession, ServerMessage, SessionId, SessionSelector,
    SessionTransition, blocking,
    blocking::Timeouts,
    runtime::tokio::{TokioRuntime, TokioUnixTransport},
};

// --- The toy application under test ------------------------------------------

#[derive(Clone)]
struct CounterApplication;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum CounterCommand {
    Add { amount: i64 },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CounterEvent {
    delta: i64,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct CounterState {
    value: i64,
}

struct CounterSession {
    value: i64,
}

impl ResidentApplication for CounterApplication {
    type Command = CounterCommand;
    type Event = CounterEvent;
    type Snapshot = CounterState;
    type State = CounterState;
    type Effect = ();
    type EffectOutput = ();
    type Session = CounterSession;

    const STORAGE_VERSION: u32 = 1;

    fn create(&self, _name: &str) -> Result<Self::Session, ApplicationError> {
        Ok(CounterSession { value: 0 })
    }

    fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError> {
        Ok(CounterSession { value: state.value })
    }

    async fn execute(
        &self,
        _context: EffectContext,
        _effect: Self::Effect,
    ) -> Result<Self::EffectOutput, ApplicationError> {
        Ok(())
    }
}

impl ResidentSession for CounterSession {
    type Command = CounterCommand;
    type Event = CounterEvent;
    type Snapshot = CounterState;
    type State = CounterState;
    type Effect = ();
    type EffectOutput = ();

    fn snapshot(&self) -> Self::Snapshot {
        CounterState { value: self.value }
    }

    fn state(&self) -> Self::State {
        CounterState { value: self.value }
    }

    fn command(
        &mut self,
        command: Self::Command,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        let CounterCommand::Add { amount } = command;
        self.value += amount;
        Ok(SessionTransition::events([CounterEvent { delta: amount }]))
    }

    fn effect_completed(
        &mut self,
        _effect: turtletap::resident::EffectId,
        _output: Result<Self::EffectOutput, ApplicationError>,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        Ok(SessionTransition::idle())
    }

    fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError> {
        self.value += event.delta;
        Ok(())
    }
}

// --- Event-driven wake application ------------------------------------------

#[derive(Clone)]
struct WakeApplication;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WakeCommand;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WakeEvent;

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct WakeState {
    ready: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WakeEffect;

struct WakeWorker(Arc<AtomicBool>);

struct WakeSession {
    ready: bool,
    worker: Option<WakeWorker>,
}

impl ResidentApplication for WakeApplication {
    type Command = WakeCommand;
    type Event = WakeEvent;
    type Snapshot = WakeState;
    type State = WakeState;
    type Effect = WakeEffect;
    type EffectOutput = WakeWorker;
    type Session = WakeSession;

    const STORAGE_VERSION: u32 = 1;

    fn create(&self, _name: &str) -> Result<Self::Session, ApplicationError> {
        Ok(WakeSession {
            ready: false,
            worker: None,
        })
    }

    fn restore(&self, state: Self::State) -> Result<Self::Session, ApplicationError> {
        Ok(WakeSession {
            ready: state.ready,
            worker: None,
        })
    }

    async fn execute(
        &self,
        context: EffectContext,
        _effect: Self::Effect,
    ) -> Result<Self::EffectOutput, ApplicationError> {
        let ready = Arc::new(AtomicBool::new(false));
        let worker_ready = Arc::clone(&ready);
        let wake = context.wake_handle();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            worker_ready.store(true, Ordering::Release);
            wake.notify();
        });
        Ok(WakeWorker(ready))
    }
}

impl ResidentSession for WakeSession {
    type Command = WakeCommand;
    type Event = WakeEvent;
    type Snapshot = WakeState;
    type State = WakeState;
    type Effect = WakeEffect;
    type EffectOutput = WakeWorker;

    fn snapshot(&self) -> Self::Snapshot {
        WakeState { ready: self.ready }
    }

    fn state(&self) -> Self::State {
        WakeState { ready: self.ready }
    }

    fn command(
        &mut self,
        _command: Self::Command,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        Ok(SessionTransition::with_effects(
            Vec::new(),
            vec![EffectRequest::at_most_once(WakeEffect)],
        ))
    }

    fn poll(
        &mut self,
        _elapsed: Duration,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        let became_ready = self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.0.load(Ordering::Acquire));
        if !self.ready && became_ready {
            self.ready = true;
            self.worker = None;
            Ok(SessionTransition::events([WakeEvent]))
        } else {
            Ok(SessionTransition::idle())
        }
    }

    fn effect_completed(
        &mut self,
        _effect: turtletap::resident::EffectId,
        output: Result<Self::EffectOutput, ApplicationError>,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        self.worker = Some(output?);
        Ok(SessionTransition::idle())
    }

    fn replay(&mut self, _event: &Self::Event) -> Result<(), ApplicationError> {
        self.ready = true;
        Ok(())
    }
}

// --- Harness -----------------------------------------------------------------

/// A temporary directory removed on drop, so a panicking test cannot leak state.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn new(_label: &str) -> Self {
        // Unix socket paths are capped near 104 bytes, so keep this short and
        // in /tmp rather than the deep per-user temp directory.
        static NONCE: AtomicU32 = AtomicU32::new(0);
        let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(format!("/tmp/ttapi-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&root).expect("scratch directory should be created");
        Self { root }
    }

    fn socket(&self) -> PathBuf {
        self.root.join("resident.sock")
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn config(&self) -> ResidentHostConfig {
        ResidentHostConfig::new(self.socket(), self.state(), "0.2.0")
            .with_initial_session("counter")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn capabilities() -> ClientCapabilities {
    ClientCapabilities {
        incremental_events: true,
        resumable: true,
        driver_leases: true,
    }
}

/// Serves a host on a task and waits for its socket to accept connections.
async fn serve(config: ResidentHostConfig) -> tokio::task::JoinHandle<()> {
    serve_application(CounterApplication, config).await
}

async fn serve_application<A>(
    application: A,
    config: ResidentHostConfig,
) -> tokio::task::JoinHandle<()>
where
    A: ResidentApplication,
{
    let socket = config.endpoint.clone();
    let handle = tokio::spawn(async move {
        let host = ResidentHost::new(application, TokioRuntime, TokioUnixTransport, config);
        if let Err(error) = host.serve().await {
            eprintln!("resident host exited: {error}");
        }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::net::UnixStream::connect(&socket).await.is_err() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "resident did not bind its socket"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    handle
}

type Client = turtletap::resident::ResidentClient<TokioUnixTransport>;

async fn connect(socket: &std::path::Path) -> Client {
    turtletap::resident::ResidentClient::connect(
        TokioUnixTransport,
        socket,
        "0.2.0",
        "test",
        capabilities(),
    )
    .await
    .expect("client should connect and handshake")
}

/// Attaches to the initial session in drive mode, returning its id and lease.
async fn attach_counter(client: &mut Client) -> (SessionId, LeaseEpoch) {
    let envelope = client.envelope(ClientRequest::Attach {
        session: SessionSelector::Name("counter".to_owned()),
        mode: AttachmentMode::Drive,
        after: None,
        force: false,
    });
    let ControlResult::Attached { session, lease } =
        client.send(&envelope).await.expect("attach should succeed")
    else {
        panic!("attach returned the wrong result");
    };
    (session.id, lease.expect("drive attach grants a lease"))
}

async fn next_event(client: &mut Client) -> CounterEvent {
    loop {
        match client.receive().await.expect("server message") {
            ServerMessage::Event { event, .. } => {
                return serde_json::from_value(event).expect("counter event decodes");
            }
            _ => continue,
        }
    }
}

// --- Tests -------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn attach_then_command_emits_an_ordered_event() {
    let scratch = Scratch::new("ordered");
    let handle = serve(scratch.config()).await;

    let mut client = connect(&scratch.socket()).await;
    let (session, lease) = attach_counter(&mut client).await;

    let envelope = client.envelope(ClientRequest::Command {
        session,
        lease,
        command: serde_json::to_value(CounterCommand::Add { amount: 5 })
            .expect("command serializes"),
    });
    let ControlResult::Accepted {
        sequence,
        duplicate,
        ..
    } = client.send(&envelope).await.expect("command accepted")
    else {
        panic!("command returned the wrong result");
    };
    assert!(!duplicate, "a fresh command is not a duplicate");

    let event = next_event(&mut client).await;
    assert_eq!(event.delta, 5);
    assert_eq!(sequence.0, 1, "the first committed event has sequence 1");

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn effect_wake_advances_a_session_before_the_fallback_tick() {
    let scratch = Scratch::new("effect-wake");
    let mut config = scratch.config();
    config.tick_rate = Duration::from_secs(5);
    let handle = serve_application(WakeApplication, config).await;

    let mut client = connect(&scratch.socket()).await;
    let (session, lease) = attach_counter(&mut client).await;
    let envelope = client.envelope(ClientRequest::Command {
        session,
        lease,
        command: serde_json::to_value(WakeCommand).expect("wake command serializes"),
    });
    assert!(matches!(
        client.send(&envelope).await.expect("wake command accepted"),
        ControlResult::Accepted { .. }
    ));

    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let ServerMessage::Event {
                session: changed,
                event,
                ..
            } = client.receive().await.expect("wake event")
                && changed == session
            {
                break serde_json::from_value::<WakeEvent>(event).expect("wake event decodes");
            }
        }
    })
    .await
    .expect("effect wake should beat the five-second fallback tick");
    let _ = event;

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resent_command_is_deduplicated_not_applied_twice() {
    let scratch = Scratch::new("dedup");
    let handle = serve(scratch.config()).await;

    let mut client = connect(&scratch.socket()).await;
    let (session, lease) = attach_counter(&mut client).await;

    // Allocate the envelope once, then send it twice with a reconnect between —
    // the shape of an ambiguous delivery the client cannot tell resolved.
    let envelope = client.envelope(ClientRequest::Command {
        session,
        lease,
        command: serde_json::to_value(CounterCommand::Add { amount: 3 })
            .expect("command serializes"),
    });
    let ControlResult::Accepted {
        sequence: first,
        duplicate: first_dup,
        ..
    } = client.send(&envelope).await.expect("first send")
    else {
        panic!("wrong result");
    };
    assert!(!first_dup);

    client
        .reconnect()
        .await
        .expect("reconnect restores the attachment");
    assert_eq!(client.session(), Some(session));
    assert!(
        client.lease().is_some(),
        "the async client should reacquire its driver lease"
    );
    let ControlResult::Accepted {
        sequence: second,
        duplicate: second_dup,
        ..
    } = client.send(&envelope).await.expect("resend")
    else {
        panic!("wrong result");
    };

    assert!(
        second_dup,
        "the resent command must be reported as duplicate"
    );
    assert_eq!(
        first, second,
        "dedup must return the original event sequence, not a new one"
    );

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn async_reconnect_reattaches_by_stable_id_after_a_rename() {
    let scratch = Scratch::new("async-reconnect");
    let handle = serve(scratch.config()).await;
    let mut client = connect(&scratch.socket()).await;
    let (session, _) = attach_counter(&mut client).await;

    let envelope = client.envelope(ClientRequest::RenameSession {
        session,
        name: "renamed".to_owned(),
    });
    let ControlResult::Renamed { .. } = client.send(&envelope).await.expect("rename") else {
        panic!("rename returned the wrong result");
    };

    client
        .reconnect()
        .await
        .expect("async reconnect should use the stable session id");
    let attachment = client.attachment().expect("reattached state");
    assert_eq!(attachment.session.id, session);
    assert_eq!(attachment.session.name, "renamed");
    assert!(
        attachment.lease.is_some(),
        "driver lease should be reacquired"
    );

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn async_reconnect_resumes_after_received_events_without_replay() {
    let scratch = Scratch::new("async-cursor");
    let handle = serve(scratch.config()).await;
    let mut client = connect(&scratch.socket()).await;
    let (session, lease) = attach_counter(&mut client).await;

    let envelope = client.envelope(ClientRequest::Command {
        session,
        lease,
        command: serde_json::to_value(CounterCommand::Add { amount: 1 })
            .expect("command serializes"),
    });
    let ControlResult::Accepted { sequence, .. } =
        client.send(&envelope).await.expect("command accepted")
    else {
        panic!("command returned the wrong result");
    };

    client
        .reconnect()
        .await
        .expect("reconnect resumes the attachment");
    let mut received = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(50), client.receive()).await {
            Ok(Ok(ServerMessage::Event {
                session: changed,
                sequence,
                ..
            })) if changed == session => received.push(sequence),
            Ok(Ok(_)) => {}
            Ok(Err(error)) => panic!("receive failed: {error}"),
            Err(_) => break,
        }
    }
    assert_eq!(
        received,
        vec![sequence],
        "the event retained before reconnect must not also be replayed"
    );

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn async_reconnect_restores_every_session_attachment() {
    let scratch = Scratch::new("async-multi-attach");
    let handle = serve(scratch.config()).await;
    let mut client = connect(&scratch.socket()).await;
    let (counter, _) = attach_counter(&mut client).await;

    let envelope = client.envelope(ClientRequest::CreateSession {
        name: "observer".to_owned(),
    });
    let ControlResult::Created { session: observer } =
        client.send(&envelope).await.expect("create second session")
    else {
        panic!("create returned the wrong result");
    };
    let envelope = client.envelope(ClientRequest::Attach {
        session: SessionSelector::Id(observer.id),
        mode: AttachmentMode::View,
        after: None,
        force: false,
    });
    let ControlResult::Attached { .. } =
        client.send(&envelope).await.expect("attach second session")
    else {
        panic!("attach returned the wrong result");
    };
    assert_eq!(client.attachments().len(), 2);

    client
        .reconnect()
        .await
        .expect("reconnect restores both subscriptions");
    assert_eq!(
        client.session(),
        Some(observer.id),
        "reconnect should preserve the most recently selected attachment"
    );
    let attachments: Vec<_> = client.attachments().collect();
    assert_eq!(attachments.len(), 2);
    assert!(attachments.iter().any(|attachment| {
        attachment.session.id == observer.id && attachment.mode == AttachmentMode::View
    }));
    let lease = attachments
        .iter()
        .find(|attachment| attachment.session.id == counter)
        .and_then(|attachment| attachment.lease)
        .expect("counter driver lease should be reacquired");

    let envelope = client.envelope(ClientRequest::Command {
        session: counter,
        lease,
        command: serde_json::to_value(CounterCommand::Add { amount: 1 })
            .expect("command serializes"),
    });
    assert!(matches!(
        client
            .send(&envelope)
            .await
            .expect("command after reconnect"),
        ControlResult::Accepted { .. }
    ));

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_committed_counter_survives_a_leader_restart() {
    let scratch = Scratch::new("recovery");

    // First leader: apply two commands, then shut it down.
    let handle = serve(scratch.config()).await;
    let activity_before = {
        let mut client = connect(&scratch.socket()).await;
        let (session, lease) = attach_counter(&mut client).await;
        for amount in [10, 7] {
            let envelope = client.envelope(ClientRequest::Command {
                session,
                lease,
                command: serde_json::to_value(CounterCommand::Add { amount })
                    .expect("command serializes"),
            });
            client.send(&envelope).await.expect("command accepted");
        }
        let envelope = client.envelope(ClientRequest::ListSessions);
        let ControlResult::Sessions { sessions } =
            client.send(&envelope).await.expect("list sessions")
        else {
            panic!("list returned the wrong result");
        };
        sessions
            .into_iter()
            .find(|summary| summary.id == session)
            .and_then(|summary| summary.last_event_at)
            .expect("committed session should have an activity timestamp")
    };
    handle.abort();
    let _ = handle.await;
    // Give the OS a moment to release the socket path.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second leader over the same state directory: the counter is restored.
    let handle = serve(scratch.config()).await;
    let mut client = connect(&scratch.socket()).await;
    let envelope = client.envelope(ClientRequest::Attach {
        session: SessionSelector::Name("counter".to_owned()),
        mode: AttachmentMode::View,
        after: None,
        force: false,
    });
    let ControlResult::Attached { session, .. } = client.send(&envelope).await.expect("reattach")
    else {
        panic!("wrong result");
    };
    let state = loop {
        match client.receive().await.expect("server message") {
            ServerMessage::Snapshot {
                state, session: id, ..
            } if id == session.id => {
                break serde_json::from_value::<CounterState>(state).expect("state decodes");
            }
            _ => continue,
        }
    };
    assert_eq!(state.value, 17, "10 + 7 must survive the restart");
    let envelope = client.envelope(ClientRequest::ListSessions);
    let ControlResult::Sessions { sessions } = client
        .send(&envelope)
        .await
        .expect("list restored sessions")
    else {
        panic!("list returned the wrong result");
    };
    let activity_after = sessions
        .into_iter()
        .find(|summary| summary.id == session.id)
        .and_then(|summary| summary.last_event_at)
        .expect("restored session should retain its activity timestamp");
    assert_eq!(activity_after, activity_before);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_takeover_notifies_the_previous_driver() {
    let scratch = Scratch::new("takeover");
    let handle = serve(scratch.config()).await;
    let mut first = connect(&scratch.socket()).await;
    let (session, _) = attach_counter(&mut first).await;
    let mut second = connect(&scratch.socket()).await;
    let envelope = second.envelope(ClientRequest::Attach {
        session: SessionSelector::Id(session),
        mode: AttachmentMode::Drive,
        after: None,
        force: true,
    });
    let ControlResult::Attached { lease, .. } =
        second.send(&envelope).await.expect("forced attach")
    else {
        panic!("attach returned the wrong result");
    };
    let expected = lease.expect("forced driver receives a lease");

    let second_instance = second.instance();
    let driver = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match first.receive().await.expect("driver notification") {
                ServerMessage::DriverChanged {
                    session: changed,
                    lease: Some(driver),
                } if changed == session && driver.owner == second_instance => break driver,
                _ => {}
            }
        }
    })
    .await
    .expect("forced takeover notification");
    assert_eq!(driver.epoch, expected);

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_blocking_client_drives_the_same_host() {
    let scratch = Scratch::new("blocking");
    let handle = serve(scratch.config()).await;
    let socket = scratch.socket();

    // The blocking client owns its own runtime, so it must run off the async
    // test thread to avoid nesting runtimes.
    let value = tokio::task::spawn_blocking(move || {
        let mut client = blocking::Client::connect(
            &socket,
            "0.2.0",
            "test-blocking",
            capabilities(),
            Timeouts::default(),
        )
        .expect("blocking connect");

        let attachment = client
            .attach(
                SessionSelector::Name("counter".to_owned()),
                AttachmentMode::Drive,
                false,
            )
            .expect("blocking attach");
        let lease = attachment.lease.expect("drive lease");

        client
            .request(ClientRequest::Command {
                session: attachment.session.id,
                lease,
                command: serde_json::to_value(CounterCommand::Add { amount: 42 })
                    .expect("command serializes"),
            })
            .expect("blocking command");

        // Re-list to read the committed sequence back through the same client.
        match client.request(ClientRequest::ListSessions).expect("list") {
            ControlResult::Sessions { sessions } => {
                sessions
                    .into_iter()
                    .find(|summary| summary.name == "counter")
                    .expect("counter session listed")
                    .sequence
                    .0
            }
            _ => panic!("wrong list result"),
        }
    })
    .await
    .expect("blocking task");

    assert_eq!(value, 1, "one committed command advances the sequence to 1");

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_drain_observes_a_push_without_a_followup_request() {
    let scratch = Scratch::new("blocking-drain");
    let handle = serve(scratch.config()).await;
    let socket = scratch.socket();
    let (attached_tx, attached_rx) = tokio::sync::oneshot::channel();
    let (drain_tx, drain_rx) = std::sync::mpsc::channel();

    let observer = tokio::task::spawn_blocking(move || {
        let mut client = blocking::Client::connect(
            &socket,
            "0.2.0",
            "test-blocking-observer",
            capabilities(),
            Timeouts::default(),
        )
        .expect("blocking connect");
        let attachment = client
            .attach(
                SessionSelector::Name("counter".to_owned()),
                AttachmentMode::View,
                false,
            )
            .expect("blocking view attach");
        attached_tx
            .send(attachment.session.id)
            .expect("test should still be waiting for the attachment");
        drain_rx
            .recv()
            .expect("test should release the observer after the push");

        client
            .drain()
            .expect("blocking drain")
            .into_iter()
            .any(|message| {
                matches!(
                    message,
                    ServerMessage::DriverChanged {
                        session,
                        lease: Some(_),
                    } if session == attachment.session.id
                )
            })
    });

    let session = attached_rx
        .await
        .expect("blocking observer should report its attachment");
    let mut driver = connect(&scratch.socket()).await;
    let envelope = driver.envelope(ClientRequest::Attach {
        session: SessionSelector::Id(session),
        mode: AttachmentMode::Drive,
        after: None,
        force: false,
    });
    assert!(matches!(
        driver.send(&envelope).await.expect("driver attach"),
        ControlResult::Attached { lease: Some(_), .. }
    ));
    tokio::time::sleep(Duration::from_millis(20)).await;
    drain_tx
        .send(())
        .expect("blocking observer should still be running");

    assert!(
        observer.await.expect("blocking observer task"),
        "drain should receive a server push without using a request as an I/O pump"
    );

    handle.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn blocking_reconnect_reattaches_by_stable_id_after_a_rename() {
    let scratch = Scratch::new("blocking-reconnect");
    let handle = serve(scratch.config()).await;
    let socket = scratch.socket();

    let session = tokio::task::spawn_blocking(move || {
        let mut client = blocking::Client::connect(
            &socket,
            "0.2.0",
            "test-blocking",
            capabilities(),
            Timeouts::default(),
        )
        .expect("blocking connect");
        let attachment = client
            .attach(
                SessionSelector::Name("counter".to_owned()),
                AttachmentMode::Drive,
                false,
            )
            .expect("blocking attach");
        let session = attachment.session.id;
        let ControlResult::Renamed { .. } = client
            .request(ClientRequest::RenameSession {
                session,
                name: "renamed".to_owned(),
            })
            .expect("rename attached session")
        else {
            panic!("rename returned the wrong result");
        };

        client
            .reconnect()
            .expect("reconnect should use the stable session id");
        assert_eq!(client.session(), Some(session));
        assert!(
            client.lease().is_some(),
            "driver lease should be reacquired"
        );
        session
    })
    .await
    .expect("blocking task");

    let mut observer = connect(&scratch.socket()).await;
    let envelope = observer.envelope(ClientRequest::ListSessions);
    let ControlResult::Sessions { sessions } =
        observer.send(&envelope).await.expect("list sessions")
    else {
        panic!("list returned the wrong result");
    };
    assert!(
        sessions
            .iter()
            .any(|summary| summary.id == session && summary.name == "renamed")
    );

    handle.abort();
}
