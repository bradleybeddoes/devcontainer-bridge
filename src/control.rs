//! TCP control channel with JSON-line framing.
//!
//! Provides read/write primitives for [`Message`] values over newline-delimited
//! JSON streams, plus [`ControlListener`] (server) and [`connect`] (client)
//! abstractions for the control channel.

use std::net::SocketAddr;

use thiserror::Error;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};

use crate::protocol::Message;

/// Maximum allowed size for a single control message in bytes (64 KB).
pub const MAX_MESSAGE_SIZE: usize = 65_536;

/// Errors that can occur on the control channel.
#[derive(Debug, Error)]
pub enum ControlError {
    /// An I/O error occurred on the underlying TCP stream.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A received message exceeded [`MAX_MESSAGE_SIZE`].
    #[error("message too large: {size} bytes exceeds {MAX_MESSAGE_SIZE} byte limit")]
    MessageTooLarge {
        /// The size of the oversized message in bytes.
        size: usize,
    },

    /// Failed to serialize a message to JSON.
    #[error("serialization error: {0}")]
    Serialization(#[source] serde_json::Error),

    /// Failed to deserialize a received JSON line into a [`Message`].
    #[error("deserialization error: {0}")]
    Deserialization(#[source] serde_json::Error),

    /// The remote peer closed the connection.
    #[error("connection closed by peer")]
    ConnectionClosed,
}

/// Read a single [`Message`] from a newline-delimited JSON stream.
///
/// Uses a bounded read to prevent memory exhaustion from a peer that sends
/// data without a newline. The read is limited to [`MAX_MESSAGE_SIZE`] + 1
/// bytes; if no newline is found within that limit, returns
/// [`ControlError::MessageTooLarge`].
///
/// Returns [`ControlError::ConnectionClosed`] on EOF.
///
/// # Errors
///
/// Returns [`ControlError`] on I/O failures, oversized messages, or
/// deserialization failures.
pub async fn read_message<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Message, ControlError> {
    let mut line = String::new();

    let utf8_err = |e: std::str::Utf8Error| {
        ControlError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    };

    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(ControlError::ConnectionClosed);
        }

        if let Some(newline_pos) = available.iter().position(|&b| b == b'\n') {
            let chunk_str = std::str::from_utf8(&available[..=newline_pos]).map_err(utf8_err)?;
            line.push_str(chunk_str);
            reader.consume(newline_pos + 1);
            break;
        }

        let chunk_len = available.len();
        if line.len() + chunk_len > MAX_MESSAGE_SIZE {
            reader.consume(chunk_len);
            return Err(ControlError::MessageTooLarge {
                size: line.len() + chunk_len,
            });
        }

        let chunk_str = std::str::from_utf8(available).map_err(utf8_err)?;
        line.push_str(chunk_str);
        reader.consume(chunk_len);
    }

    if line.len() > MAX_MESSAGE_SIZE {
        return Err(ControlError::MessageTooLarge { size: line.len() });
    }

    serde_json::from_str(line.trim_end()).map_err(ControlError::Deserialization)
}

/// Write a single [`Message`] as a newline-terminated JSON line.
///
/// # Errors
///
/// Returns [`ControlError`] on serialization or I/O failures, or if the
/// serialized message exceeds [`MAX_MESSAGE_SIZE`].
pub async fn write_message<W: AsyncWrite + Unpin>(
    writer: &mut W,
    msg: &Message,
) -> Result<(), ControlError> {
    let json = serde_json::to_string(msg).map_err(ControlError::Serialization)?;

    if json.len() > MAX_MESSAGE_SIZE {
        return Err(ControlError::MessageTooLarge { size: json.len() });
    }

    writer.write_all(json.as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    Ok(())
}

/// A TCP listener for the control channel, bound to a loopback address.
pub struct ControlListener {
    listener: TcpListener,
}

impl ControlListener {
    /// Bind a control channel listener to `127.0.0.1:<port>`.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Io`] if the bind fails (e.g. port in use).
    pub async fn bind(port: u16) -> Result<Self, ControlError> {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        let listener = TcpListener::bind(addr).await?;
        Ok(Self { listener })
    }

    /// Accept the next incoming control connection.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Io`] on accept failure.
    pub async fn accept(&self) -> Result<(ControlConnection, SocketAddr), ControlError> {
        let (stream, addr) = self.listener.accept().await?;
        Ok((ControlConnection::new(stream), addr))
    }

    /// Returns the local address this listener is bound to.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError::Io`] if the address cannot be retrieved.
    pub fn local_addr(&self) -> Result<SocketAddr, ControlError> {
        self.listener.local_addr().map_err(ControlError::Io)
    }
}

/// A single control channel connection with JSON-line framing.
///
/// Wraps a TCP stream split into buffered read and write halves.
pub struct ControlConnection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
}

impl ControlConnection {
    /// Wrap an existing [`TcpStream`] as a control connection.
    fn new(stream: TcpStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        Self {
            reader: BufReader::new(read_half),
            writer: write_half,
        }
    }

    /// Receive the next message from the remote peer.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError`] on I/O, framing, or deserialization failures.
    pub async fn recv(&mut self) -> Result<Message, ControlError> {
        read_message(&mut self.reader).await
    }

    /// Send a message to the remote peer.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError`] on serialization or I/O failures.
    pub async fn send(&mut self, msg: &Message) -> Result<(), ControlError> {
        write_message(&mut self.writer, msg).await
    }
}

/// Connect to a control channel server and return a [`ControlConnection`].
///
/// # Errors
///
/// Returns [`ControlError::Io`] if the TCP connection cannot be established.
pub async fn connect(addr: SocketAddr) -> Result<ControlConnection, ControlError> {
    let stream = TcpStream::connect(addr).await?;
    Ok(ControlConnection::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[tokio::test]
    async fn read_write_roundtrip() {
        let msg = Message::Ping;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();

        let mut reader = Cursor::new(buf);
        let decoded = read_message(&mut reader).await.unwrap();
        assert_eq!(decoded, Message::Ping);
    }

    #[tokio::test]
    async fn read_message_eof_returns_connection_closed() {
        let mut reader = Cursor::new(Vec::<u8>::new());
        let result = read_message(&mut reader).await;
        assert!(matches!(result, Err(ControlError::ConnectionClosed)));
    }

    #[tokio::test]
    async fn read_message_invalid_json() {
        let mut reader = Cursor::new(b"not json\n".to_vec());
        let result = read_message(&mut reader).await;
        assert!(matches!(result, Err(ControlError::Deserialization(_))));
    }

    #[tokio::test]
    async fn read_message_too_large() {
        // Create a line larger than MAX_MESSAGE_SIZE
        let mut large = vec![b'x'; MAX_MESSAGE_SIZE + 100];
        large.push(b'\n');
        let mut reader = Cursor::new(large);
        let result = read_message(&mut reader).await;
        assert!(matches!(result, Err(ControlError::MessageTooLarge { .. })));
    }

    #[tokio::test]
    async fn write_message_format() {
        let msg = Message::Pong;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).await.unwrap();
        let written = String::from_utf8(buf).unwrap();
        assert_eq!(written, "{\"type\":\"Pong\"}\n");
    }

    #[tokio::test]
    async fn multiple_messages_roundtrip() {
        let messages = vec![
            Message::Ping,
            Message::Pong,
            Message::Register {
                container_id: "c1".into(),
                hostname: "dev".into(),
            },
            Message::RegisterAck { success: true },
        ];

        let mut buf = Vec::new();
        for msg in &messages {
            write_message(&mut buf, msg).await.unwrap();
        }

        let mut reader = Cursor::new(buf);
        for expected in &messages {
            let decoded = read_message(&mut reader).await.unwrap();
            assert_eq!(&decoded, expected);
        }
    }

    #[tokio::test]
    async fn listener_and_client_roundtrip() {
        let listener = ControlListener::bind(0).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let msg = conn.recv().await.unwrap();
            conn.send(&Message::Pong).await.unwrap();
            msg
        });

        let mut client = connect(addr).await.unwrap();
        client.send(&Message::Ping).await.unwrap();
        let response = client.recv().await.unwrap();
        assert_eq!(response, Message::Pong);

        let received = server.await.unwrap();
        assert_eq!(received, Message::Ping);
    }

    #[tokio::test]
    async fn connection_closed_on_drop() {
        let listener = ControlListener::bind(0).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            // Client will drop, so recv should return ConnectionClosed
            conn.recv().await
        });

        let client = connect(addr).await.unwrap();
        drop(client);

        let result = server.await.unwrap();
        assert!(matches!(result, Err(ControlError::ConnectionClosed)));
    }
}
