//! Frozen wire and storage fixtures for compatibility review.

use serde::Deserialize;
use turtletap::resident::{
    ClientEnvelope, ClientHello, JournalRecord, PROTOCOL_VERSION, ServerHandshake, ServerHello,
    SessionControlSnapshot,
};

#[derive(Deserialize)]
struct StoredSessionFixture {
    #[serde(default)]
    host_version: u32,
    #[serde(default)]
    application_version: u32,
    control: SessionControlSnapshot,
    state: serde_json::Value,
}

#[derive(Deserialize)]
struct LegacyRootFixture {
    sessions: Vec<StoredSessionFixture>,
}

#[test]
fn protocol_v1_fixtures_remain_readable() {
    let client: ClientHello = fixture("protocol/client-hello-v1.json");
    assert_eq!(client.protocol.minimum, PROTOCOL_VERSION);
    assert_eq!(client.protocol.maximum, PROTOCOL_VERSION);

    let handshake: ServerHandshake = fixture("protocol/server-accepted-v1.json");
    assert!(
        matches!(handshake, ServerHandshake::Accepted(ServerHello { protocol, .. }) if protocol == PROTOCOL_VERSION)
    );

    let rejection: ServerHandshake = fixture("protocol/server-rejected-v1.json");
    assert!(matches!(rejection, ServerHandshake::Rejected(_)));

    let envelope: ClientEnvelope = fixture("protocol/command-envelope-v1.json");
    assert_eq!(envelope.request.sequence, 7);
}

#[test]
fn storage_v0_and_v1_fixtures_remain_readable() {
    let legacy: StoredSessionFixture = fixture("storage/checkpoint-host-v0.json");
    assert_eq!(legacy.host_version, 0);
    assert_eq!(legacy.application_version, 0);
    assert_eq!(legacy.control.sequence.0, 1);
    assert_eq!(legacy.state["value"], 40);

    let current: StoredSessionFixture = fixture("storage/checkpoint-host-v1.json");
    assert_eq!(current.host_version, 1);
    assert_eq!(current.application_version, 3);
    assert_eq!(current.control.sequence.0, 2);
    assert_eq!(current.state["value"], 41);

    let record: JournalRecord<serde_json::Value> = fixture("storage/journal-record-v1.json");
    assert_eq!(record.sequence.0, 2);
    assert_eq!(record.event["value"], 41);

    let root: LegacyRootFixture = fixture("storage/root-checkpoint-host-v0.json");
    assert_eq!(root.sessions.len(), 1);
    assert_eq!(root.sessions[0].control.name, "fixture");
}

fn fixture<T: serde::de::DeserializeOwned>(path: &str) -> T {
    let source = match path {
        "protocol/client-hello-v1.json" => {
            include_str!("fixtures/protocol/client-hello-v1.json")
        }
        "protocol/server-accepted-v1.json" => {
            include_str!("fixtures/protocol/server-accepted-v1.json")
        }
        "protocol/server-rejected-v1.json" => {
            include_str!("fixtures/protocol/server-rejected-v1.json")
        }
        "protocol/command-envelope-v1.json" => {
            include_str!("fixtures/protocol/command-envelope-v1.json")
        }
        "storage/checkpoint-host-v0.json" => {
            include_str!("fixtures/storage/checkpoint-host-v0.json")
        }
        "storage/checkpoint-host-v1.json" => {
            include_str!("fixtures/storage/checkpoint-host-v1.json")
        }
        "storage/journal-record-v1.json" => {
            include_str!("fixtures/storage/journal-record-v1.json")
        }
        "storage/root-checkpoint-host-v0.json" => {
            include_str!("fixtures/storage/root-checkpoint-host-v0.json")
        }
        _ => panic!("unknown fixture: {path}"),
    };
    serde_json::from_str(source).unwrap_or_else(|error| panic!("invalid {path}: {error}"))
}
