//! Effect adapters for resident leaders.

use std::{future::Future, io, path::Path, time::Duration};

/// Runtime clock used by reconnect and lease timers.
pub trait Clock: Clone + Send + Sync + 'static {
    /// Future returned by [`Clock::sleep`].
    type Sleep: Future<Output = ()> + Send;

    /// Waits without blocking an executor thread.
    fn sleep(&self, duration: Duration) -> Self::Sleep;
}

/// Runtime task spawner.
pub trait Spawner: Clone + Send + Sync + 'static {
    /// Starts a detached task.
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static);
}

/// Starts the product-specific resident subprocess.
pub trait ProcessSpawner: Clone + Send + Sync + 'static {
    /// Process identity returned to the election winner.
    type Child: Send;

    /// Starts a resident for `endpoint` without inheriting terminal streams.
    fn spawn_resident(&self, endpoint: &Path) -> io::Result<Self::Child>;
}

/// A framed byte connection that can be split for independent push delivery.
pub trait Connection: Send {
    /// Read half.
    type Reader: FrameReader + 'static;
    /// Write half.
    type Writer: FrameWriter + 'static;

    /// Splits the connection into independently owned halves.
    fn split(self) -> (Self::Reader, Self::Writer);
}

/// Receives bounded frame payloads without their length prefixes.
pub trait FrameReader: Send {
    /// Receives one complete payload.
    fn receive(&mut self) -> impl Future<Output = io::Result<Vec<u8>>> + Send;
}

/// Sends already-framed messages.
pub trait FrameWriter: Send {
    /// Sends one already-framed message.
    fn send(&mut self, frame: Vec<u8>) -> impl Future<Output = io::Result<()>> + Send;
}

/// Runtime-specific local transport.
pub trait Transport: Clone + Send + Sync + 'static {
    /// Listener implementation.
    type Listener: Listener<Connection = Self::Connection>;
    /// Connection implementation.
    type Connection: Connection;

    /// Binds a local endpoint.
    fn bind(&self, endpoint: &Path) -> impl Future<Output = io::Result<Self::Listener>> + Send;

    /// Connects to a local endpoint.
    fn connect(&self, endpoint: &Path)
    -> impl Future<Output = io::Result<Self::Connection>> + Send;
}

/// Runtime-specific listener.
pub trait Listener: Send {
    /// Accepted connection type.
    type Connection: Connection;

    /// Accepts the next connection.
    fn accept(&mut self) -> impl Future<Output = io::Result<Self::Connection>> + Send;
}

/// Tokio-based production adapters.
#[cfg(unix)]
pub mod tokio;
