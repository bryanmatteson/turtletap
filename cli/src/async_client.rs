//! Event-driven resident client used by interactive surfaces.

use std::{
    io,
    path::{Path, PathBuf},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use tokio::sync::{mpsc, watch};
use turtletap::resident::{
    AttachmentMode, ClientCapabilities, ClientEnvelope, ClientInstanceId, ClientRequest,
    ControlResult, ExchangeItem, LeaseEpoch, ResidentClient, ServerMessage, SessionSelector,
    SessionSummary,
    runtime::tokio::{TokioUnixTransport, TokioUnixTransport as Transport},
};

use crate::{
    app::SessionSnapshot,
    client::{CLIENT_NAME, client_error, protocol_error},
    commands::ensure_started,
};

const OPERATION_CAPACITY: usize = 64;
const EVENT_CAPACITY: usize = 256;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const RECONNECT_ATTEMPTS: usize = 3;

/// One surface-owned operation.
#[derive(Debug)]
pub(crate) struct ClientOperation {
    pub(crate) id: u64,
    pub(crate) request: ClientRequest,
}

/// Pump output delivered to a surface.
#[derive(Debug)]
pub(crate) enum ClientEvent {
    Message(ServerMessage),
    Completed {
        id: u64,
        result: Result<ControlResult, String>,
    },
    Connection(ConnectionState),
}

/// Visible connection lifecycle.
#[derive(Clone, Debug)]
pub(crate) enum ConnectionState {
    Reconnecting { attempt: usize },
    Reconnected,
    Failed(String),
}

/// Synchronous surface-side handle for an asynchronous connection pump.
pub(crate) struct SessionHandle {
    operations: mpsc::Sender<ClientOperation>,
    events: mpsc::Receiver<ClientEvent>,
    shutdown: watch::Sender<bool>,
    next_operation: u64,
}

impl SessionHandle {
    pub(crate) fn try_request(&mut self, request: ClientRequest) -> io::Result<u64> {
        let id = self.next_operation;
        self.next_operation = self.next_operation.saturating_add(1);
        self.operations
            .try_send(ClientOperation { id, request })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::WouldBlock, "resident request queue is full")
                }
                mpsc::error::TrySendError::Closed(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "resident pump stopped")
                }
            })?;
        Ok(id)
    }

    pub(crate) fn poll_event(&mut self, context: &mut Context<'_>) -> Poll<Option<ClientEvent>> {
        Pin::new(&mut self.events).poll_recv(context)
    }

    pub(crate) fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        self.request_shutdown();
    }
}

pub(crate) async fn connect_and_attach(
    path: &Path,
    selector: SessionSelector,
    mode: AttachmentMode,
    force: bool,
) -> io::Result<(
    SessionSummary,
    ClientInstanceId,
    Option<LeaseEpoch>,
    SessionSnapshot,
    SessionHandle,
)> {
    let mut client = tokio::time::timeout(
        REQUEST_TIMEOUT,
        ResidentClient::connect(
            TokioUnixTransport,
            path,
            env!("CARGO_PKG_VERSION"),
            CLIENT_NAME,
            ClientCapabilities {
                incremental_events: true,
                resumable: true,
                driver_leases: true,
            },
        ),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "resident connect timed out"))?
    .map_err(client_error)?;

    let instance = client.instance();
    let envelope = client.envelope(ClientRequest::Attach {
        session: selector,
        mode,
        after: None,
        force,
    });
    let exchange = client
        .begin_exchange(&envelope)
        .await
        .map_err(client_error)?;
    let mut snapshot = None;
    let mut attached = None;
    let mut initial = Vec::new();
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;

    while snapshot.is_none() || attached.is_none() {
        let item = tokio::time::timeout_at(deadline, client.next_exchange(exchange))
            .await
            .map_err(|_| {
                client.finish_exchange(exchange);
                io::Error::new(io::ErrorKind::TimedOut, "resident attach timed out")
            })?
            .map_err(client_error)?;
        match item {
            ExchangeItem::Push(message) => {
                if let ServerMessage::Snapshot { state, .. } = &message {
                    snapshot = Some(serde_json::from_value(state.clone()).map_err(protocol_error)?);
                } else {
                    initial.push(message.clone());
                }
                client.acknowledge(&message);
            }
            ExchangeItem::Response(result) => match result.map_err(|error| {
                io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
            })? {
                ControlResult::Attached { session, lease } => {
                    attached = Some((session, lease));
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "resident returned the wrong attach response",
                    ));
                }
            },
        }
    }

    let (session, lease) = attached.expect("attach loop requires a response");
    let snapshot = snapshot.expect("attach loop requires a snapshot");
    let (operation_tx, operation_rx) = mpsc::channel(OPERATION_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    for message in initial {
        event_tx
            .try_send(ClientEvent::Message(message))
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "initial resident replay exceeded event capacity",
                )
            })?;
    }
    let path = path.to_path_buf();
    tokio::spawn(run_pump(client, path, operation_rx, event_tx, shutdown_rx));

    Ok((
        session,
        instance,
        lease,
        snapshot,
        SessionHandle {
            operations: operation_tx,
            events: event_rx,
            shutdown: shutdown_tx,
            next_operation: 1,
        },
    ))
}

pub(crate) async fn connect_controller(
    path: &Path,
) -> io::Result<(Vec<SessionSummary>, SessionHandle)> {
    let mut client = tokio::time::timeout(
        REQUEST_TIMEOUT,
        ResidentClient::connect(
            TokioUnixTransport,
            path,
            env!("CARGO_PKG_VERSION"),
            CLIENT_NAME,
            ClientCapabilities {
                incremental_events: true,
                resumable: true,
                driver_leases: true,
            },
        ),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "resident connect timed out"))?
    .map_err(client_error)?;
    let envelope = client.envelope(ClientRequest::ListSessions);
    let result = tokio::time::timeout(REQUEST_TIMEOUT, client.send(&envelope))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "resident list timed out"))?
        .map_err(client_error)?;
    let ControlResult::Sessions { sessions } = result else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "resident returned the wrong list response",
        ));
    };
    let (operation_tx, operation_rx) = mpsc::channel(OPERATION_CAPACITY);
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    tokio::spawn(run_pump(
        client,
        path.to_path_buf(),
        operation_rx,
        event_tx,
        shutdown_rx,
    ));
    Ok((
        sessions,
        SessionHandle {
            operations: operation_tx,
            events: event_rx,
            shutdown: shutdown_tx,
            next_operation: 1,
        },
    ))
}

async fn run_pump(
    mut client: ResidentClient<Transport>,
    path: PathBuf,
    mut operations: mpsc::Receiver<ClientOperation>,
    events: mpsc::Sender<ClientEvent>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            operation = operations.recv() => {
                let Some(operation) = operation else {
                    break;
                };
                let id = operation.id;
                let result = drive_operation(
                    &mut client,
                    &path,
                    operation.request,
                    &events,
                    &mut shutdown,
                ).await;
                if events.send(ClientEvent::Completed { id, result }).await.is_err() {
                    break;
                }
            }
            message = client.receive_unacknowledged() => {
                match message {
                    Ok(message) => {
                        if events.send(ClientEvent::Message(message.clone())).await.is_err() {
                            break;
                        }
                        client.acknowledge(&message);
                    }
                    Err(error) => {
                        if reconnect(&mut client, &path, &events).await.is_err() {
                            let _ = events.send(ClientEvent::Connection(
                                ConnectionState::Failed(error.to_string())
                            )).await;
                            break;
                        }
                    }
                }
            }
        }
    }

    while let Ok(operation) = operations.try_recv() {
        let _ = events
            .send(ClientEvent::Completed {
                id: operation.id,
                result: Err("resident operation cancelled during shutdown".to_owned()),
            })
            .await;
    }
}

async fn drive_operation(
    client: &mut ResidentClient<Transport>,
    path: &Path,
    request: ClientRequest,
    events: &mpsc::Sender<ClientEvent>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ControlResult, String> {
    let retryable = matches!(
        request,
        ClientRequest::Command { .. }
            | ClientRequest::ListSessions
            | ClientRequest::Status
            | ClientRequest::Ping
    );
    let envelope = client.envelope(request);
    for attempt in 0..=RECONNECT_ATTEMPTS {
        match exchange(client, &envelope, events, shutdown).await {
            Ok(result) => return Ok(result),
            Err(error) if retryable && attempt < RECONNECT_ATTEMPTS => {
                reconnect(client, path, events)
                    .await
                    .map_err(|reconnect| format!("{error}; reconnect failed: {reconnect}"))?;
            }
            Err(error) => {
                return Err(if retryable {
                    error
                } else {
                    format!("operation outcome is unknown after connection loss: {error}")
                });
            }
        }
    }
    Err("resident retry budget exhausted".to_owned())
}

async fn exchange(
    client: &mut ResidentClient<Transport>,
    envelope: &ClientEnvelope,
    events: &mpsc::Sender<ClientEvent>,
    shutdown: &mut watch::Receiver<bool>,
) -> Result<ControlResult, String> {
    let exchange = client
        .begin_exchange(envelope)
        .await
        .map_err(|error| error.to_string())?;
    let deadline = tokio::time::sleep(REQUEST_TIMEOUT);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                client.finish_exchange(exchange);
                return Err(if changed.is_err() {
                    "resident pump shutdown sender closed".to_owned()
                } else {
                    "resident operation cancelled during shutdown".to_owned()
                });
            }
            _ = &mut deadline => {
                client.finish_exchange(exchange);
                return Err("resident response timed out".to_owned());
            }
            item = client.next_exchange(exchange) => {
                let item = match item {
                    Ok(item) => item,
                    Err(error) => {
                        client.finish_exchange(exchange);
                        return Err(error.to_string());
                    }
                };
                match item {
                    ExchangeItem::Push(message) => {
                        events.send(ClientEvent::Message(message.clone())).await
                            .map_err(|_| "surface event receiver closed".to_owned())?;
                        client.acknowledge(&message);
                    }
                    ExchangeItem::Response(result) => {
                        return result.map_err(|error| error.to_string());
                    }
                }
            }
        }
    }
}

async fn reconnect(
    client: &mut ResidentClient<Transport>,
    path: &Path,
    events: &mpsc::Sender<ClientEvent>,
) -> Result<(), String> {
    for attempt in 1..=RECONNECT_ATTEMPTS {
        let _ = events
            .send(ClientEvent::Connection(ConnectionState::Reconnecting {
                attempt,
            }))
            .await;
        let path = path.to_path_buf();
        let started = tokio::task::spawn_blocking(move || ensure_started(&path))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string());
        if let Err(error) = started {
            if attempt == RECONNECT_ATTEMPTS {
                return Err(error);
            }
            tokio::time::sleep(reconnect_backoff(attempt)).await;
            continue;
        }
        let delivered = client
            .reconnect_streaming(|message| {
                let events = events.clone();
                async move {
                    events
                        .send(ClientEvent::Message(message))
                        .await
                        .map_err(|_| {
                            turtletap::resident::ClientError::Io(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "surface event receiver closed",
                            ))
                        })
                }
            })
            .await;
        match delivered {
            Ok(()) => {
                let _ = events
                    .send(ClientEvent::Connection(ConnectionState::Reconnected))
                    .await;
                return Ok(());
            }
            Err(error) if attempt < RECONNECT_ATTEMPTS => {
                tokio::time::sleep(reconnect_backoff(attempt)).await;
                let _ = error;
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("resident reconnect budget exhausted".to_owned())
}

fn reconnect_backoff(attempt: usize) -> Duration {
    match attempt {
        1 => Duration::from_millis(10),
        2 => Duration::from_millis(25),
        _ => Duration::from_millis(50),
    }
}
