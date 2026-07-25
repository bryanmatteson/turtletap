use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    path::PathBuf,
};

use serde::de::DeserializeOwned;

use super::{
    AttachmentMode, ClientCapabilities, ClientEnvelope, ClientHello, ClientInstanceId,
    ClientRequest, ControlResult, DriverLease, EventSequence, LeaseEpoch, PROTOCOL_VERSION,
    ProtocolRejection, RequestId, ServerHandshake, ServerHello, ServerMessage, SessionId,
    SessionSelector, SessionSummary, WireError, encode_frame,
    runtime::{Connection as _, FrameWriter as _, Transport},
};

const MAX_PENDING_MESSAGES: usize = 256;

/// Identity of one sequential request/response exchange.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExchangeId(RequestId);

/// One item observed while a request exchange is active.
#[derive(Debug)]
pub enum ExchangeItem {
    /// A server message that precedes or is unrelated to the matching response.
    Push(ServerMessage),
    /// The matching response.
    Response(Result<ControlResult, WireError>),
}

/// Generic resident-client failures.
#[derive(Debug)]
pub enum ClientError {
    /// Local transport failed.
    Io(std::io::Error),
    /// Frame encoding failed.
    Frame(super::FrameError),
    /// Payload decoding failed.
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

/// Identity and resume state for one of the client's attachments.
#[derive(Clone, Debug)]
pub struct Attachment {
    /// How the session was originally selected.
    pub selector: SessionSelector,
    /// The attached session.
    pub session: SessionSummary,
    /// Requested authority.
    pub mode: AttachmentMode,
    /// Whether an existing driver may be displaced on reattachment.
    pub force: bool,
    /// Granted fencing token when driving.
    pub lease: Option<LeaseEpoch>,
    /// Highest event already received, resumed from after a reconnect.
    pub cursor: EventSequence,
}

/// Runtime-generic typed client for a resident leader.
///
/// The client retains its instance identity, request sequence, attachments, and
/// event cursors across [`reconnect`](Self::reconnect). This lets callers resend
/// an ambiguous request with the same [`RequestId`] and receive the leader's
/// deduplicated result without separately rebuilding subscriptions.
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
    inflight: HashMap<RequestId, ClientRequest>,
    attachments: Vec<Attachment>,
    current_attachment: Option<SessionId>,
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
            inflight: HashMap::new(),
            attachments: Vec::new(),
            current_attachment: None,
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

    /// The most recently selected attachment, if any.
    #[must_use]
    pub fn attachment(&self) -> Option<&Attachment> {
        let current = self.current_attachment?;
        self.attachments
            .iter()
            .find(|attachment| attachment.session.id == current)
    }

    /// Every session subscribed through this connection.
    #[must_use]
    pub fn attachments(&self) -> impl ExactSizeIterator<Item = &Attachment> {
        self.attachments.iter()
    }

    /// The fencing token for this client's driving attachment.
    #[must_use]
    pub fn lease(&self) -> Option<LeaseEpoch> {
        self.attachment().and_then(|attachment| attachment.lease)
    }

    /// The attached session's stable identity.
    #[must_use]
    pub fn session(&self) -> Option<SessionId> {
        self.attachment().map(|attachment| attachment.session.id)
    }

    /// Advances the resume point, never moving it backwards.
    ///
    /// Incoming snapshots and events update this automatically. Callers may
    /// also advance it after handling an application-specific message retained
    /// outside this client.
    pub fn set_cursor(&mut self, sequence: EventSequence) {
        if let Some(session) = self.current_attachment {
            self.set_session_cursor(session, sequence);
        }
    }

    /// Records the authority requested after reconnecting.
    ///
    /// Incoming driver notifications and successful driver requests update
    /// this automatically.
    pub fn set_driver(&mut self, lease: Option<LeaseEpoch>, mode: AttachmentMode) -> bool {
        let Some(session) = self.current_attachment else {
            return false;
        };
        self.set_session_driver(session, lease, mode)
    }

    fn set_session_cursor(&mut self, session: SessionId, sequence: EventSequence) {
        if let Some(attachment) = self.attachment_mut(session) {
            attachment.cursor = attachment.cursor.max(sequence);
        }
    }

    fn set_session_driver(
        &mut self,
        session: SessionId,
        lease: Option<LeaseEpoch>,
        mode: AttachmentMode,
    ) -> bool {
        let Some(attachment) = self.attachment_mut(session) else {
            return false;
        };
        let changed = attachment.lease != lease || attachment.mode != mode;
        attachment.lease = lease;
        attachment.mode = mode;
        changed
    }

    fn attachment_mut(&mut self, session: SessionId) -> Option<&mut Attachment> {
        self.attachments
            .iter_mut()
            .find(|attachment| attachment.session.id == session)
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
        let exchange = self.begin_exchange(envelope).await?;
        loop {
            match self.next_exchange(exchange).await {
                Ok(ExchangeItem::Response(result)) => {
                    return result.map_err(ClientError::Rejected);
                }
                Ok(ExchangeItem::Push(message)) => {
                    if self.pending.len() >= MAX_PENDING_MESSAGES {
                        self.finish_exchange(exchange);
                        return Err(ClientError::Io(std::io::Error::new(
                            std::io::ErrorKind::WouldBlock,
                            "resident pending-message capacity exceeded",
                        )));
                    }
                    self.acknowledge(&message);
                    self.pending.push_back(message);
                }
                Err(error) => {
                    self.finish_exchange(exchange);
                    return Err(error);
                }
            }
        }
    }

    /// Writes one request and begins a sequential exchange.
    ///
    /// The caller must drive [`next_exchange`](Self::next_exchange) until a
    /// response or call [`finish_exchange`](Self::finish_exchange) after an
    /// error. Only one exchange may be active on a client connection.
    pub async fn begin_exchange(
        &mut self,
        envelope: &ClientEnvelope,
    ) -> Result<ExchangeId, ClientError> {
        if !self.inflight.is_empty() && !self.inflight.contains_key(&envelope.request) {
            return Err(unexpected("another resident exchange is already active"));
        }
        let frame = encode_frame(envelope)?;
        self.inflight
            .entry(envelope.request)
            .or_insert_with(|| envelope.message.clone());
        if let Err(error) = self.writer.send(frame).await {
            self.inflight.remove(&envelope.request);
            return Err(error.into());
        }
        Ok(ExchangeId(envelope.request))
    }

    /// Reads one item from an active sequential exchange without buffering it.
    pub async fn next_exchange(
        &mut self,
        exchange: ExchangeId,
    ) -> Result<ExchangeItem, ClientError> {
        if !self.inflight.contains_key(&exchange.0) {
            return Err(unexpected("resident exchange is not active"));
        }
        let message: ServerMessage = receive_json(&mut self.reader).await?;
        match message {
            ServerMessage::Response { request, result } if request == exchange.0 => {
                if let Some(message) = self.inflight.remove(&request)
                    && let Ok(result) = &result
                {
                    self.observe_result(&message, result);
                }
                Ok(ExchangeItem::Response(result))
            }
            message => Ok(ExchangeItem::Push(message)),
        }
    }

    /// Abandons local bookkeeping for an exchange after a terminal error.
    pub fn finish_exchange(&mut self, exchange: ExchangeId) {
        self.inflight.remove(&exchange.0);
    }

    /// Marks a delivered push as retained by the caller.
    ///
    /// Async pumps call this only after the message enters their bounded event
    /// channel. This keeps reconnect cursors from advancing past undelivered
    /// state.
    pub fn acknowledge(&mut self, message: &ServerMessage) {
        self.observe_message(message);
    }

    /// Returns a pushed message already observed while waiting for a response.
    pub fn pending(&mut self) -> Option<ServerMessage> {
        self.pending.pop_front()
    }

    /// Waits for the next pushed server message.
    pub async fn receive(&mut self) -> Result<ServerMessage, ClientError> {
        let message = self.receive_unacknowledged().await?;
        self.acknowledge(&message);
        Ok(message)
    }

    /// Waits for the next pushed message without advancing its resume cursor.
    ///
    /// Bounded async pumps use this variant so they can enqueue the message
    /// before acknowledging it. If the sink is full or closed, reconnect then
    /// resumes from the last message the consumer actually retained.
    pub async fn receive_unacknowledged(&mut self) -> Result<ServerMessage, ClientError> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        receive_json(&mut self.reader).await
    }

    /// Reconnects with the same client identity and a fresh leader registration.
    ///
    /// The client rejoins every subscribed stable session ID with its current
    /// authority and resumes each after the highest event already received.
    /// Pending pushed messages remain available after reconnect.
    pub async fn reconnect(&mut self) -> Result<(), ClientError> {
        let mut buffered = Vec::new();
        self.reconnect_streaming(|message| {
            buffered.push(message);
            std::future::ready(Ok(()))
        })
        .await?;
        for message in buffered {
            if self.pending.len() >= MAX_PENDING_MESSAGES {
                return Err(ClientError::Io(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "resident reconnect replay exceeded pending-message capacity",
                )));
            }
            self.acknowledge(&message);
            self.pending.push_back(message);
        }
        Ok(())
    }

    /// Reconnects and streams every reattachment replay item through a caller
    /// supplied bounded sink before advancing its cursor.
    pub async fn reconnect_streaming<F, Fut>(&mut self, mut deliver: F) -> Result<(), ClientError>
    where
        F: FnMut(ServerMessage) -> Fut,
        Fut: Future<Output = Result<(), ClientError>>,
    {
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

        if self.attachments.is_empty() {
            return Ok(());
        }
        let previous_attachments = self.attachments.clone();
        let previous_current = self.current_attachment;
        for previous in previous_attachments {
            let envelope = self.envelope(ClientRequest::Attach {
                session: SessionSelector::Id(previous.session.id),
                mode: previous.mode,
                after: Some(previous.cursor),
                force: previous.force,
            });
            let exchange = self.begin_exchange(&envelope).await?;
            let result = loop {
                match self.next_exchange(exchange).await {
                    Ok(ExchangeItem::Push(message)) => {
                        if let Err(error) = deliver(message.clone()).await {
                            self.finish_exchange(exchange);
                            self.current_attachment = previous_current;
                            return Err(error);
                        }
                        self.acknowledge(&message);
                    }
                    Ok(ExchangeItem::Response(result)) => {
                        break result.map_err(ClientError::Rejected)?;
                    }
                    Err(error) => {
                        self.finish_exchange(exchange);
                        self.current_attachment = previous_current;
                        return Err(error);
                    }
                }
            };
            if !matches!(result, ControlResult::Attached { .. }) {
                self.current_attachment = previous_current;
                return Err(unexpected("resident returned the wrong reconnect response"));
            }
            if let Some(attachment) = self.attachment_mut(previous.session.id) {
                attachment.selector = previous.selector;
                attachment.cursor = attachment.cursor.max(previous.cursor);
            }
        }
        self.current_attachment = previous_current;
        Ok(())
    }

    fn observe_result(&mut self, request: &ClientRequest, result: &ControlResult) {
        match (request, result) {
            (
                ClientRequest::Attach {
                    session: selector,
                    mode,
                    force,
                    ..
                },
                ControlResult::Attached { session, lease },
            ) => {
                let attachment = Attachment {
                    selector: selector.clone(),
                    session: session.clone(),
                    mode: *mode,
                    force: *force,
                    lease: *lease,
                    cursor: session.sequence,
                };
                if let Some(existing) = self.attachment_mut(session.id) {
                    *existing = attachment;
                } else {
                    self.attachments.push(attachment);
                }
                self.current_attachment = Some(session.id);
            }
            (ClientRequest::Detach { session }, ControlResult::Detached { session: detached })
                if session == detached =>
            {
                self.remove_attachment(*session);
            }
            (
                ClientRequest::AcquireDriver { session, force },
                ControlResult::Driver {
                    session: changed,
                    lease,
                },
            ) if session == changed && self.has_attachment(*session) => {
                self.observe_driver(*session, *lease);
                if let Some(attachment) = self.attachment_mut(*session)
                    && attachment.lease.is_some()
                {
                    attachment.force = *force;
                }
            }
            (
                ClientRequest::ReleaseDriver { session },
                ControlResult::Driver {
                    session: changed,
                    lease,
                },
            ) if session == changed && self.has_attachment(*session) => {
                self.observe_driver(*session, *lease);
            }
            (
                ClientRequest::RenameSession { session, .. },
                ControlResult::Renamed { session: renamed },
            ) if *session == renamed.id && self.has_attachment(*session) => {
                if let Some(attachment) = self.attachment_mut(*session) {
                    attachment.session = renamed.clone();
                }
            }
            (ClientRequest::StopSession { session }, ControlResult::Stopping) => {
                self.remove_attachment(*session);
            }
            _ => {}
        }
    }

    fn observe_message(&mut self, message: &ServerMessage) {
        match message {
            ServerMessage::Response { request, result } => {
                if let Some(message) = self.inflight.remove(request)
                    && let Ok(result) = result
                {
                    self.observe_result(&message, result);
                }
            }
            ServerMessage::Snapshot {
                session, sequence, ..
            }
            | ServerMessage::Event {
                session, sequence, ..
            } if self.has_attachment(*session) => self.set_session_cursor(*session, *sequence),
            ServerMessage::DriverChanged { session, lease } if self.has_attachment(*session) => {
                self.observe_driver(*session, *lease);
            }
            _ => {}
        }
    }

    fn observe_driver(&mut self, session: SessionId, lease: Option<DriverLease>) {
        let owned = lease
            .filter(|lease| lease.owner == self.instance)
            .map(|lease| lease.epoch);
        let mode = if owned.is_some() {
            AttachmentMode::Drive
        } else {
            AttachmentMode::View
        };
        let _ = self.set_session_driver(session, owned, mode);
    }

    fn has_attachment(&self, session: SessionId) -> bool {
        self.attachments
            .iter()
            .any(|attachment| attachment.session.id == session)
    }

    fn remove_attachment(&mut self, session: SessionId) {
        self.attachments
            .retain(|attachment| attachment.session.id != session);
        if self.current_attachment == Some(session) {
            self.current_attachment = self
                .attachments
                .last()
                .map(|attachment| attachment.session.id);
        }
    }
}

fn unexpected(message: &'static str) -> ClientError {
    ClientError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        message,
    ))
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
