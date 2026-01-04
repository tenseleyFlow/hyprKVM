//! TCP transport layer for HyprKVM
//!
//! Provides basic message framing and transport over TCP.

use std::io;
use std::net::SocketAddr;

use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::{TcpListener, TcpStream};

use hyprkvm_common::protocol::Message;

/// Maximum message size (1MB)
const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

/// Frame header size (4 bytes for length)
const FRAME_HEADER_SIZE: usize = 4;

/// A framed connection that can send and receive Messages
pub struct FramedConnection {
    stream: TcpStream,
    read_buf: BytesMut,
    write_buf: BytesMut,
    remote_addr: SocketAddr,
}

impl FramedConnection {
    /// Create a new framed connection from a TcpStream
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        let remote_addr = stream.peer_addr()?;
        Ok(Self {
            stream,
            read_buf: BytesMut::with_capacity(8192),
            write_buf: BytesMut::with_capacity(8192),
            remote_addr,
        })
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Send a message
    pub async fn send(&mut self, msg: &Message) -> Result<(), TransportError> {
        // Serialize the message
        let json = serde_json::to_vec(msg)
            .map_err(|e| TransportError::Serialize(e.to_string()))?;

        if json.len() > MAX_MESSAGE_SIZE as usize {
            return Err(TransportError::MessageTooLarge(json.len()));
        }

        // Write length prefix + data
        self.write_buf.clear();
        self.write_buf.put_u32(json.len() as u32);
        self.write_buf.extend_from_slice(&json);

        self.stream
            .write_all(&self.write_buf)
            .await
            .map_err(TransportError::Io)?;

        self.stream.flush().await.map_err(TransportError::Io)?;

        tracing::trace!("Sent message: {:?}", msg);
        Ok(())
    }

    /// Receive a message (blocking until one is available or connection closes)
    pub async fn recv(&mut self) -> Result<Option<Message>, TransportError> {
        loop {
            // Try to parse a complete frame from the buffer
            if let Some(msg) = self.try_parse_frame()? {
                return Ok(Some(msg));
            }

            // Read more data
            let n = self
                .stream
                .read_buf(&mut self.read_buf)
                .await
                .map_err(TransportError::Io)?;

            if n == 0 {
                // Connection closed
                if self.read_buf.is_empty() {
                    return Ok(None);
                } else {
                    return Err(TransportError::ConnectionReset);
                }
            }
        }
    }

    /// Try to parse a complete frame from the read buffer
    fn try_parse_frame(&mut self) -> Result<Option<Message>, TransportError> {
        if self.read_buf.len() < FRAME_HEADER_SIZE {
            return Ok(None);
        }

        // Peek at the length
        let len = u32::from_be_bytes([
            self.read_buf[0],
            self.read_buf[1],
            self.read_buf[2],
            self.read_buf[3],
        ]) as usize;

        if len > MAX_MESSAGE_SIZE as usize {
            return Err(TransportError::MessageTooLarge(len));
        }

        let total_frame_size = FRAME_HEADER_SIZE + len;
        if self.read_buf.len() < total_frame_size {
            return Ok(None);
        }

        // Consume the frame
        self.read_buf.advance(FRAME_HEADER_SIZE);
        let json_data = self.read_buf.split_to(len);

        // Deserialize
        let msg: Message = serde_json::from_slice(&json_data)
            .map_err(|e| TransportError::Deserialize(e.to_string()))?;

        tracing::trace!("Received message: {:?}", msg);
        Ok(Some(msg))
    }

    /// Shutdown the connection gracefully
    pub async fn shutdown(&mut self) -> io::Result<()> {
        self.stream.shutdown().await
    }
}

/// TCP server that accepts connections
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl Server {
    /// Bind to an address and start listening
    pub async fn bind(addr: SocketAddr) -> Result<Self, TransportError> {
        let listener = TcpListener::bind(addr).await.map_err(TransportError::Io)?;
        let local_addr = listener.local_addr().map_err(TransportError::Io)?;

        tracing::info!("Server listening on {}", local_addr);
        Ok(Self {
            listener,
            local_addr,
        })
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Accept a new connection
    pub async fn accept(&self) -> Result<FramedConnection, TransportError> {
        let (stream, addr) = self.listener.accept().await.map_err(TransportError::Io)?;
        tracing::info!("Accepted connection from {}", addr);
        FramedConnection::new(stream).map_err(TransportError::Io)
    }
}

/// Connect to a remote server
pub async fn connect(addr: SocketAddr) -> Result<FramedConnection, TransportError> {
    tracing::info!("Connecting to {}", addr);
    let stream = TcpStream::connect(addr).await.map_err(TransportError::Io)?;
    tracing::info!("Connected to {}", addr);
    FramedConnection::new(stream).map_err(TransportError::Io)
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("Failed to serialize message: {0}")]
    Serialize(String),

    #[error("Failed to deserialize message: {0}")]
    Deserialize(String),

    #[error("Message too large: {0} bytes")]
    MessageTooLarge(usize),

    #[error("Connection reset while reading")]
    ConnectionReset,
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyprkvm_common::protocol::{HelloPayload, PROTOCOL_VERSION};

    #[tokio::test]
    async fn test_roundtrip() {
        let server = Server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
        let addr = server.local_addr();

        let server_handle = tokio::spawn(async move {
            let mut conn = server.accept().await.unwrap();
            let msg = conn.recv().await.unwrap().unwrap();
            conn.send(&msg).await.unwrap();
            conn.shutdown().await.unwrap();
        });

        let mut client = connect(addr).await.unwrap();
        let msg = Message::Hello(HelloPayload {
            protocol_version: PROTOCOL_VERSION,
            machine_name: "test".to_string(),
            capabilities: vec![],
        });

        client.send(&msg).await.unwrap();
        let echo = client.recv().await.unwrap().unwrap();

        if let (Message::Hello(sent), Message::Hello(received)) = (&msg, &echo) {
            assert_eq!(sent.protocol_version, received.protocol_version);
            assert_eq!(sent.machine_name, received.machine_name);
        } else {
            panic!("Wrong message type");
        }

        server_handle.await.unwrap();
    }
}
