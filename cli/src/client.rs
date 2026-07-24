//! Application layer over the library's blocking resident client.
//!
//! Transport, handshake, framing, request sequencing, reconnection, and
//! deduplication all belong to `turtletap::resident::blocking`. What remains
//! here is the part that knows about *this* product's payloads: waiting for the
//! shell snapshot that follows an attach.

use std::{io, path::Path};

use turtletap::resident::{
    ClientCapabilities, ClientRequest, ControlResult,
    blocking::{self, Timeouts},
};

use crate::commands::ensure_started;

/// Name this product reports to the leader during handshakes.
pub(crate) const CLIENT_NAME: &str = "turtletap-cli";

pub(crate) struct SessionClient {
    inner: blocking::Client,
}

impl SessionClient {
    pub(crate) fn connect(path: &Path) -> io::Result<Self> {
        let inner = blocking::Client::connect(
            path,
            env!("CARGO_PKG_VERSION"),
            CLIENT_NAME,
            ClientCapabilities {
                incremental_events: true,
                resumable: true,
                driver_leases: true,
            },
            Timeouts::default(),
        )
        .map_err(client_error)?
        // A reconnect is worthless if the leader exited; restart it first.
        .with_relaunch(|socket| ensure_started(socket).map(|_| ()));
        Ok(Self { inner })
    }

    pub(crate) fn request(&mut self, message: ClientRequest) -> io::Result<ControlResult> {
        self.inner.request(message).map_err(client_error)
    }
}

pub(crate) fn protocol_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

pub(crate) fn client_error(error: turtletap::resident::ClientError) -> io::Error {
    match error {
        turtletap::resident::ClientError::Io(error) => error,
        other => io::Error::other(other.to_string()),
    }
}
