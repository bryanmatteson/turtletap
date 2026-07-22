use std::{collections::VecDeque, path::PathBuf};

use serde::de::DeserializeOwned;

use super::{
    ClientCapabilities, ClientEnvelope, ClientHello, ClientInstanceId, ClientRequest,
    ControlResult, PROTOCOL_VERSION, ProtocolRejection, RequestId, ServerHandshake, ServerHello,
    ServerMessage, WireError, encode_frame,
    runtime::{Connection as _, FrameWriter as _, Transport},
};

/// Generic resident-client failures.
#[derive(Debug)]
pub enum ClientError {
    /// Local transport failed.
    Io(std::io::Error),
    /// A frame could not be encoded.
    Frame(super::FrameError),
    /// A payload could not be decoded.
    Json(serde_json::Error),
    /// Leader selected an unsupported protocol.
    IncompatibleProtocol(ProtocolRejection),
    /// Leader rejected a request.
    Rejected(WireError),
    /// Connection ended before the expected response.
    Closed,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => error.fmt(formatter),
            Self::Frame(error) => error.fmt(formatter),
            Self::Json(error) => error.fmt(formatter),
            Self::IncompatibleProtocol(rejection) => rejection.message.fmt(formatter),
            Self::Rejected(error) => error.fmt(formatter),
            Self::Closed => formatter.write_str("resident connection closed"),
        }
    }
}

impl std::error::Error for ClientError {}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<super::FrameError> for ClientError {
    fn from(error: super::FrameError) -> Self {
        Self::Frame(error)
    }
}

impl From<serde_json::Error> for ClientError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

/// Runtime-generic typed client for a resident leader.
///
/// The client retains its instance identity and request sequence across
/// [`reconnect`](Self::reconnect), allowing callers to resend an ambiguous
/// request with the same [`RequestId`] and receive the leader's deduplicated result.
pub struct ResidentClient<T: Transport> {
    transport: T,
    endpoint: PathBuf,
    reader: <T::Connection as super::runtime::Connection>::Reader,
    writer: <T::Connection as super::runtime::Connection>::Writer,
    instance: ClientInstanceId,
    next_request: u64,
    binary_version: String,
    name: String,
    capabilities: ClientCapabilities,
    leader: ServerHello,
    pending: VecDeque<ServerMessage>,
}

impl<T: Transport> ResidentClient<T> {
    /// Connects and completes the versioned handshake.
    pub async fn connect(
        transport: T,
        endpoint: impl Into<PathBuf>,
        binary_version: impl Into<String>,
        name: impl Into<String>,
        capabilities: ClientCapabilities,
    ) -> Result<Self, ClientError> {
        let endpoint = endpoint.into();
        let instance = ClientInstanceId::new();
        let binary_version = binary_version.into();
        let name = name.into();
        let (reader, writer, leader) = handshake(
            &transport,
            &endpoint,
            instance,
            &binary_version,
            &name,
            &capabilities,
        )
        .await?;
        Ok(Self {
            transport,
            endpoint,
            reader,
            writer,
            instance,
            next_request: 1,
            binary_version,
            name,
            capabilities,
            leader,
            pending: VecDeque::new(),
        })
    }

    /// Stable identity retained across reconnects.
    #[must_use]
    pub fn instance(&self) -> ClientInstanceId {
        self.instance
    }

    /// Current leader registration.
    #[must_use]
    pub fn leader(&self) -> &ServerHello {
        &self.leader
    }

    /// Allocates a request envelope without sending it.
    ///
    /// Retain the returned envelope until its response is observed. If delivery
    /// becomes ambiguous, reconnect and resend this same value.
    pub fn envelope(&mut self, message: ClientRequest) -> ClientEnvelope {
        let envelope = ClientEnvelope {
            request: RequestId {
                client: self.instance,
                sequence: self.next_request,
            },
            message,
        };
        self.next_request = self.next_request.saturating_add(1);
        envelope
    }

    /// Sends an envelope and waits for its response while retaining pushed events.
    pub async fn send(&mut self, envelope: &ClientEnvelope) -> Result<ControlResult, ClientError> {
        self.writer.send(encode_frame(envelope)?).await?;
        loop {
            let message: ServerMessage = receive_json(&mut self.reader).await?;
            match message {
                ServerMessage::Response { request, result } if request == envelope.request => {
                    return result.map_err(ClientError::Rejected);
                }
                message => self.pending.push_back(message),
            }
        }
    }

    /// Returns a pushed message already observed while waiting for a response.
    pub fn pending(&mut self) -> Option<ServerMessage> {
        self.pending.pop_front()
    }

    /// Waits for the next pushed server message.
    pub async fn receive(&mut self) -> Result<ServerMessage, ClientError> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        receive_json(&mut self.reader).await
    }

    /// Reconnects with the same client identity and a fresh leader registration.
    pub async fn reconnect(&mut self) -> Result<(), ClientError> {
        let (reader, writer, leader) = handshake(
            &self.transport,
            &self.endpoint,
            self.instance,
            &self.binary_version,
            &self.name,
            &self.capabilities,
        )
        .await?;
        self.reader = reader;
        self.writer = writer;
        self.leader = leader;
        Ok(())
    }
}

async fn handshake<T: Transport>(
    transport: &T,
    endpoint: &std::path::Path,
    instance: ClientInstanceId,
    binary_version: &str,
    name: &str,
    capabilities: &ClientCapabilities,
) -> Result<
    (
        <T::Connection as super::runtime::Connection>::Reader,
        <T::Connection as super::runtime::Connection>::Writer,
        ServerHello,
    ),
    ClientError,
> {
    let connection = transport.connect(endpoint).await?;
    let (mut reader, mut writer) = connection.split();
    writer
        .send(encode_frame(&ClientHello {
            protocol: super::VersionRange::current(),
            binary_version: binary_version.to_owned(),
            client_instance: instance,
            client_name: name.to_owned(),
            capabilities: capabilities.clone(),
        })?)
        .await?;
    let handshake: ServerHandshake = receive_json(&mut reader).await?;
    let hello = match handshake {
        ServerHandshake::Accepted(hello) => hello,
        ServerHandshake::Rejected(rejection) => {
            return Err(ClientError::IncompatibleProtocol(rejection));
        }
    };
    if hello.protocol != PROTOCOL_VERSION {
        return Err(ClientError::IncompatibleProtocol(ProtocolRejection {
            rejected: true,
            supported: super::VersionRange {
                minimum: hello.protocol,
                maximum: hello.protocol,
            },
            binary_version: hello.binary_version,
            message: format!(
                "leader selected protocol {}; client requires {}",
                hello.protocol.0, PROTOCOL_VERSION.0
            ),
        }));
    }
    Ok((reader, writer, hello))
}

async fn receive_json<T, C>(connection: &mut C) -> Result<T, ClientError>
where
    T: DeserializeOwned,
    C: super::runtime::FrameReader,
{
    let payload = connection.receive().await?;
    serde_json::from_slice(&payload).map_err(Into::into)
}
