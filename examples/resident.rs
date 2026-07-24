//! A minimal resident application driven through the blocking client.
//!
//! The host runs on a background thread over a Unix socket; the main thread
//! attaches with `resident::blocking::Client`, sends a few commands, and reads
//! the committed state back. Run with:
//!
//! ```console
//! cargo run --example resident --features tokio
//! ```

use std::{path::PathBuf, thread, time::Duration};

use serde::{Deserialize, Serialize};
use turtletap::resident::{
    ApplicationError, AttachmentMode, ClientCapabilities, ClientRequest, ControlResult,
    EffectContext, EffectId, ResidentApplication, ResidentHost, ResidentHostConfig,
    ResidentSession, SessionSelector, SessionTransition,
    blocking::{self, Client, Timeouts},
    runtime::tokio::{TokioRuntime, TokioUnixTransport},
};

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
        _effect: EffectId,
        _output: Result<Self::EffectOutput, ApplicationError>,
    ) -> Result<SessionTransition<Self::Event, Self::Effect>, ApplicationError> {
        Ok(SessionTransition::idle())
    }

    fn replay(&mut self, event: &Self::Event) -> Result<(), ApplicationError> {
        self.value += event.delta;
        Ok(())
    }
}

fn main() -> std::io::Result<()> {
    let root = std::env::temp_dir().join(format!("turtletap-example-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    let socket = root.join("resident.sock");

    let host_socket = socket.clone();
    let host_state = root.join("state");
    let host = thread::spawn(move || {
        let config = ResidentHostConfig::new(&host_socket, &host_state, "0.2.0")
            .with_initial_session("counter");
        let host = ResidentHost::new(CounterApplication, TokioRuntime, TokioUnixTransport, config);
        let _ = blocking::serve(host);
    });

    wait_for_socket(&socket);

    let mut client = Client::connect(
        &socket,
        "0.2.0",
        "example",
        ClientCapabilities {
            incremental_events: true,
            resumable: true,
            driver_leases: true,
        },
        Timeouts::default(),
    )
    .map_err(std::io::Error::other)?;

    let attachment = client
        .attach(
            SessionSelector::Name("counter".to_owned()),
            AttachmentMode::Drive,
            false,
        )
        .map_err(std::io::Error::other)?;
    let lease = attachment.lease.expect("a drive attachment grants a lease");
    println!("attached to session {}", attachment.session.name);

    for amount in [5, 10, -3] {
        client
            .request(ClientRequest::Command {
                session: attachment.session.id,
                lease,
                command: serde_json::to_value(CounterCommand::Add { amount })
                    .expect("command serializes"),
            })
            .map_err(std::io::Error::other)?;
        println!("sent Add {{ amount: {amount} }}");
    }

    if let ControlResult::Sessions { sessions } = client
        .request(ClientRequest::ListSessions)
        .map_err(std::io::Error::other)?
    {
        for summary in sessions {
            println!(
                "session {:<10} committed {} event(s)",
                summary.name, summary.sequence.0
            );
        }
    }

    // Stop the leader so the background thread returns, then clean up.
    let _ = client.request(ClientRequest::StopLeader);
    drop(client);
    let _ = host.join();
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn wait_for_socket(socket: &PathBuf) {
    for _ in 0..500 {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("resident did not bind its socket");
}
