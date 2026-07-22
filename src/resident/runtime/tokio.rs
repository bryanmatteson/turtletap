use std::{future::Future, io, path::Path, pin::Pin, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Clock, Connection, FrameReader, FrameWriter, Listener, Spawner, Transport};
use crate::resident::MAX_FRAME_SIZE;

/// Tokio clock and task spawner.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioRuntime;

impl Clock for TokioRuntime {
    type Sleep = Pin<Box<dyn Future<Output = ()> + Send>>;

    fn sleep(&self, duration: Duration) -> Self::Sleep {
        Box::pin(tokio::time::sleep(duration))
    }
}

impl Spawner for TokioRuntime {
    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        tokio::spawn(future);
    }
}

/// Tokio Unix-domain-socket transport.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioUnixTransport;

/// Tokio Unix listener adapter.
pub struct TokioUnixListener(tokio::net::UnixListener);

/// Tokio Unix connection adapter.
pub struct TokioUnixConnection(tokio::net::UnixStream);

/// Tokio Unix read half.
pub struct TokioUnixReader(tokio::net::unix::OwnedReadHalf);

/// Tokio Unix write half.
pub struct TokioUnixWriter(tokio::net::unix::OwnedWriteHalf);

impl Transport for TokioUnixTransport {
    type Listener = TokioUnixListener;
    type Connection = TokioUnixConnection;

    async fn bind(&self, endpoint: &Path) -> io::Result<Self::Listener> {
        tokio::net::UnixListener::bind(endpoint).map(TokioUnixListener)
    }

    async fn connect(&self, endpoint: &Path) -> io::Result<Self::Connection> {
        tokio::net::UnixStream::connect(endpoint)
            .await
            .map(TokioUnixConnection)
    }
}

impl Listener for TokioUnixListener {
    type Connection = TokioUnixConnection;

    async fn accept(&mut self) -> io::Result<Self::Connection> {
        self.0
            .accept()
            .await
            .map(|(stream, _)| TokioUnixConnection(stream))
    }
}

impl Connection for TokioUnixConnection {
    type Reader = TokioUnixReader;
    type Writer = TokioUnixWriter;

    fn split(self) -> (Self::Reader, Self::Writer) {
        let (reader, writer) = self.0.into_split();
        (TokioUnixReader(reader), TokioUnixWriter(writer))
    }
}

impl FrameWriter for TokioUnixWriter {
    async fn send(&mut self, frame: Vec<u8>) -> io::Result<()> {
        self.0.write_all(&frame).await?;
        self.0.flush().await
    }
}

impl FrameReader for TokioUnixReader {
    async fn receive(&mut self) -> io::Result<Vec<u8>> {
        let size = self.0.read_u32().await? as usize;
        if size > MAX_FRAME_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("resident frame is {size} bytes; maximum is {MAX_FRAME_SIZE} bytes"),
            ));
        }
        let mut payload = vec![0; size];
        self.0.read_exact(&mut payload).await?;
        Ok(payload)
    }
}
