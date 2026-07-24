use std::{future::Future, io, path::Path, pin::Pin, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{Clock, Connection, FrameReader, FrameWriter, Listener, Spawner, Transport};

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
///
/// Frame state lives in the reader rather than in a suspended future, so
/// cancelling a [`FrameReader::receive`] mid-frame — under `tokio::select!` or
/// a timeout — leaves the stream intact and the partial frame buffered.
pub struct TokioUnixReader {
    half: tokio::net::unix::OwnedReadHalf,
    decoder: crate::resident::FrameDecoder,
}

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
        (
            TokioUnixReader {
                half: reader,
                decoder: crate::resident::FrameDecoder::default(),
            },
            TokioUnixWriter(writer),
        )
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
        loop {
            // Drain what is already buffered before touching the socket, so a
            // cancelled read never discards a frame that had fully arrived.
            if let Some(payload) = self
                .decoder
                .next_payload()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?
            {
                return Ok(payload);
            }

            // `read` is the only await, and it is cancel-safe: if the future is
            // dropped while pending, no bytes have been consumed.
            let mut chunk = [0_u8; 8192];
            let read = self.half.read(&mut chunk).await?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "resident connection closed",
                ));
            }
            self.decoder.push(&chunk[..read]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resident::encode_frame;

    /// A frame reader must survive having its read cancelled part-way through a
    /// frame. The previous implementation consumed the four length bytes inside
    /// the cancelled future and desynchronised the stream permanently.
    #[tokio::test(flavor = "current_thread")]
    async fn receive_is_cancel_safe_across_a_partial_frame() {
        let (client, server) = tokio::net::UnixStream::pair().expect("socketpair");
        let (mut reader, _writer) = TokioUnixConnection(client).split();
        let (_server_reader, mut server_writer) = TokioUnixConnection(server).split();

        let frame = encode_frame(&vec!["one", "two"]).expect("encode");
        let (head, tail) = frame.split_at(3);

        server_writer.send(head.to_vec()).await.expect("send head");
        let cancelled = tokio::time::timeout(Duration::from_millis(50), reader.receive()).await;
        assert!(cancelled.is_err(), "partial frame must not resolve");

        server_writer.send(tail.to_vec()).await.expect("send tail");
        let payload = tokio::time::timeout(Duration::from_millis(50), reader.receive())
            .await
            .expect("frame should arrive")
            .expect("frame should decode");

        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&payload).expect("payload"),
            vec!["one".to_owned(), "two".to_owned()]
        );
    }
}
