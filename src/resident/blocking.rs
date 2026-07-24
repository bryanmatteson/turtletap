//! Blocking wrapper around [`ResidentClient`] for terminal-driven callers.
//!
//! The resident runtime traits are asynchronous, but a terminal attach loop is
//! a blocking read loop. This module owns a current-thread Tokio runtime and
//! drives the ordinary async client inside it, so blocking callers and async
//! callers share one implementation of the handshake, framing, request
//! sequencing, and deduplication rules.

use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use super::{
    AttachmentMode, ClientCapabilities, ClientEnvelope, ClientError, ClientRequest, ControlResult,
    EventSequence, LeaseEpoch, ResidentClient, ServerHello, ServerMessage, SessionId,
    SessionSelector, runtime::tokio::TokioUnixTransport,
};

pub use super::Attachment;

/// How long blocking operations wait before giving up.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// Budget for establishing a connection and completing the handshake.
    pub connect: Duration,
    /// Budget for a single request/response exchange.
    pub request: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(5),
            request: Duration::from_secs(5),
        }
    }
}

/// Number of times an ambiguous request is resent before giving up.
const RECONNECT_ATTEMPTS: usize = 3;
/// Briefly drives the I/O reactor so an already-arrived push becomes readable.
const DRAIN_READINESS_BUDGET: Duration = Duration::from_millis(1);

/// Hook that restarts a dead leader before a reconnect attempt.
type RelaunchHook = Box<dyn FnMut(&Path) -> io::Result<()> + Send>;

/// A blocking resident client.
///
/// Request identity and client instance survive [`Self::reconnect`], so a
/// request whose delivery became ambiguous is resent with its original
/// [`super::RequestId`] and answered from the leader's deduplication table
/// rather than executed twice.
pub struct Client {
    runtime: tokio::runtime::Runtime,
    inner: ResidentClient<TokioUnixTransport>,
    socket: PathBuf,
    timeouts: Timeouts,
    relaunch: Option<RelaunchHook>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Client")
            .field("socket", &self.socket)
            .field("timeouts", &self.timeouts)
            .field("attachment", &self.inner.attachment())
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connects to `socket` and completes the versioned handshake.
    pub fn connect(
        socket: &Path,
        binary_version: &str,
        client_name: &str,
        capabilities: ClientCapabilities,
        timeouts: Timeouts,
    ) -> Result<Self, ClientError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let inner = runtime.block_on(async {
            tokio::time::timeout(
                timeouts.connect,
                ResidentClient::connect(
                    TokioUnixTransport,
                    socket,
                    binary_version,
                    client_name,
                    capabilities,
                ),
            )
            .await
            .unwrap_or_else(|_| Err(timed_out("resident connection timed out")))
        })?;
        Ok(Self {
            runtime,
            inner,
            socket: socket.to_owned(),
            timeouts,
            relaunch: None,
        })
    }

    /// Installs a hook invoked before each reconnect, letting the product
    /// restart a leader that exited. Without one, reconnects assume the leader
    /// is already listening.
    #[must_use]
    pub fn with_relaunch(
        mut self,
        hook: impl FnMut(&Path) -> io::Result<()> + Send + 'static,
    ) -> Self {
        self.relaunch = Some(Box::new(hook));
        self
    }

    /// The leader's registration from the most recent handshake.
    #[must_use]
    pub fn leader(&self) -> &ServerHello {
        self.inner.leader()
    }

    /// Stable identity retained across reconnects.
    #[must_use]
    pub fn instance(&self) -> super::ClientInstanceId {
        self.inner.instance()
    }

    /// The socket this client connects to.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// The current attachment, if this client has attached to a session.
    #[must_use]
    pub fn attachment(&self) -> Option<&Attachment> {
        self.inner.attachment()
    }

    /// Sends a request, reconnecting and resending the same envelope when
    /// delivery becomes ambiguous.
    ///
    /// A [`ClientRequest::Command`] carries a fencing token, so its lease is
    /// refreshed from the reattachment before each resend; a client that fails
    /// to regain the driver lease reports that rather than sending a stale one.
    pub fn request(&mut self, message: ClientRequest) -> Result<ControlResult, ClientError> {
        let mut envelope = self.inner.envelope(message);
        for attempt in 0..=RECONNECT_ATTEMPTS {
            match self.send_once(&envelope) {
                Ok(result) => return Ok(result),
                Err(error) if attempt < RECONNECT_ATTEMPTS && is_recoverable(&error) => {
                    self.reconnect()?;
                    if let ClientRequest::Command { lease, .. } = &mut envelope.message {
                        *lease = self.inner.lease().ok_or_else(|| {
                            ClientError::Io(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "the reconnected client did not regain the driver lease",
                            ))
                        })?;
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(ClientError::Io(io::Error::new(
            io::ErrorKind::ConnectionReset,
            "resident reconnect attempts were exhausted",
        )))
    }

    fn send_once(&mut self, envelope: &ClientEnvelope) -> Result<ControlResult, ClientError> {
        let Self {
            runtime,
            inner,
            timeouts,
            ..
        } = self;
        runtime.block_on(async {
            tokio::time::timeout(timeouts.request, inner.send(envelope))
                .await
                .unwrap_or_else(|_| Err(timed_out("resident request timed out")))
        })
    }

    /// Attaches to a session and records the attachment for later reconnects.
    pub fn attach(
        &mut self,
        selector: SessionSelector,
        mode: AttachmentMode,
        force: bool,
    ) -> Result<Attachment, ClientError> {
        let result = self.request(ClientRequest::Attach {
            session: selector.clone(),
            mode,
            after: None,
            force,
        })?;
        if !matches!(result, ControlResult::Attached { .. }) {
            return Err(unexpected("resident returned the wrong attach response"));
        }
        self.inner
            .attachment()
            .cloned()
            .ok_or_else(|| unexpected("resident client did not retain the successful attachment"))
    }

    /// The fencing token for this client's driving attachment.
    #[must_use]
    pub fn lease(&self) -> Option<LeaseEpoch> {
        self.inner.lease()
    }

    /// The attached session's identity.
    #[must_use]
    pub fn session(&self) -> Option<SessionId> {
        self.inner.session()
    }

    /// Records the authority this client holds after acquiring, losing, or
    /// releasing the driver lease, and reports whether anything changed.
    ///
    /// Keeping this current matters beyond display: a reconnect reattaches with
    /// the recorded mode, so a stale value would silently return as a viewer.
    pub fn set_driver(&mut self, lease: Option<LeaseEpoch>, mode: AttachmentMode) -> bool {
        self.inner.set_driver(lease, mode)
    }

    /// Advances the resume point, never moving it backwards.
    pub fn set_cursor(&mut self, sequence: EventSequence) {
        self.inner.set_cursor(sequence);
    }

    /// Collects every message already available, using a one-millisecond
    /// readiness probe to give the current-thread I/O driver a chance to
    /// observe pushes that arrived between blocking calls.
    pub fn drain(&mut self) -> Result<Vec<ServerMessage>, ClientError> {
        let mut messages = Vec::new();
        while let Some(message) = self.inner.pending() {
            messages.push(message);
        }
        loop {
            let Self { runtime, inner, .. } = self;
            let ready = runtime.block_on(async {
                tokio::time::timeout(DRAIN_READINESS_BUDGET, inner.receive())
                    .await
                    .ok()
            });
            match ready {
                Some(Ok(message)) => messages.push(message),
                Some(Err(error)) => return Err(error),
                None => return Ok(messages),
            }
        }
    }

    /// Waits up to `timeout` for the next pushed message.
    pub fn receive(&mut self, timeout: Duration) -> Result<Option<ServerMessage>, ClientError> {
        if let Some(message) = self.inner.pending() {
            return Ok(Some(message));
        }
        let Self { runtime, inner, .. } = self;
        runtime.block_on(async {
            match tokio::time::timeout(timeout, inner.receive()).await {
                Ok(Ok(message)) => Ok(Some(message)),
                Ok(Err(error)) => Err(error),
                Err(_elapsed) => Ok(None),
            }
        })
    }

    /// Reconnects with the same client identity and, when this client was
    /// attached, reattaches from its cursor so no events are replayed twice.
    pub fn reconnect(&mut self) -> Result<(), ClientError> {
        if let Some(relaunch) = self.relaunch.as_mut() {
            relaunch(&self.socket)?;
        }
        let Self {
            runtime,
            inner,
            timeouts,
            ..
        } = self;
        runtime.block_on(async {
            tokio::time::timeout(timeouts.connect, inner.reconnect())
                .await
                .unwrap_or_else(|_| Err(timed_out("resident reconnect timed out")))
        })
    }
}

/// Runs a resident host to completion on a multi-threaded runtime.
pub fn serve<A, R, T>(host: super::ResidentHost<A, R, T>) -> io::Result<()>
where
    A: super::ResidentApplication,
    R: super::runtime::Clock + super::runtime::Spawner,
    T: super::runtime::Transport,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(host.serve())
}

fn timed_out(message: &'static str) -> ClientError {
    ClientError::Io(io::Error::new(io::ErrorKind::TimedOut, message))
}

fn unexpected(message: &'static str) -> ClientError {
    ClientError::Io(io::Error::new(io::ErrorKind::InvalidData, message))
}

/// Whether a failure is worth retrying against a fresh connection.
fn is_recoverable(error: &ClientError) -> bool {
    match error {
        ClientError::Io(error) => matches!(
            error.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::NotConnected
                | io::ErrorKind::TimedOut
                | io::ErrorKind::UnexpectedEof
        ),
        ClientError::Closed => true,
        _ => false,
    }
}
