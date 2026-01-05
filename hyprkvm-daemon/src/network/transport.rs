//! TCP/TLS transport layer for HyprKVM
//!
//! Provides message framing and transport over TCP with optional TLS encryption.

#![allow(dead_code)]

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tokio_rustls::server::TlsStream as ServerTlsStream;
use tokio_rustls::TlsAcceptor;

use hyprkvm_common::protocol::Message;

use super::tls::{self, Fingerprint};

/// Maximum message size (1MB)
const MAX_MESSAGE_SIZE: u32 = 1024 * 1024;

/// Frame header size (4 bytes for length)
const FRAME_HEADER_SIZE: usize = 4;

/// Stream type - either plain TCP or TLS-wrapped
pub enum Stream {
    Plain(TcpStream),
    TlsClient(ClientTlsStream<TcpStream>),
    TlsServer(ServerTlsStream<TcpStream>),
}

impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Stream::TlsClient(s) => Pin::new(s).poll_read(cx, buf),
            Stream::TlsServer(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Stream::TlsClient(s) => Pin::new(s).poll_write(cx, buf),
            Stream::TlsServer(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_flush(cx),
            Stream::TlsClient(s) => Pin::new(s).poll_flush(cx),
            Stream::TlsServer(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Stream::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Stream::TlsClient(s) => Pin::new(s).poll_shutdown(cx),
            Stream::TlsServer(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// A framed connection that can send and receive Messages
pub struct FramedConnection {
    stream: Stream,
    read_buf: BytesMut,
    write_buf: BytesMut,
    remote_addr: SocketAddr,
    /// Peer certificate fingerprint (if TLS)
    peer_fingerprint: Option<Fingerprint>,
}

impl FramedConnection {
    /// Create a new framed connection from a plain TcpStream
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        let remote_addr = stream.peer_addr()?;
        Ok(Self {
            stream: Stream::Plain(stream),
            read_buf: BytesMut::with_capacity(8192),
            write_buf: BytesMut::with_capacity(8192),
            remote_addr,
            peer_fingerprint: None,
        })
    }

    /// Create a new framed connection from a client TLS stream
    pub fn from_tls_client(
        stream: ClientTlsStream<TcpStream>,
        remote_addr: SocketAddr,
        peer_fingerprint: Option<Fingerprint>,
    ) -> Self {
        Self {
            stream: Stream::TlsClient(stream),
            read_buf: BytesMut::with_capacity(8192),
            write_buf: BytesMut::with_capacity(8192),
            remote_addr,
            peer_fingerprint,
        }
    }

    /// Create a new framed connection from a server TLS stream
    pub fn from_tls_server(
        stream: ServerTlsStream<TcpStream>,
        remote_addr: SocketAddr,
    ) -> Self {
        Self {
            stream: Stream::TlsServer(stream),
            read_buf: BytesMut::with_capacity(8192),
            write_buf: BytesMut::with_capacity(8192),
            remote_addr,
            peer_fingerprint: None,
        }
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_addr
    }

    /// Get the peer's certificate fingerprint (if TLS connection)
    pub fn peer_fingerprint(&self) -> Option<&Fingerprint> {
        self.peer_fingerprint.as_ref()
    }

    /// Check if this is a TLS connection
    pub fn is_tls(&self) -> bool {
        !matches!(self.stream, Stream::Plain(_))
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

/// TLS-enabled TCP server that accepts connections
pub struct Server {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls_acceptor: Option<TlsAcceptor>,
}

impl Server {
    /// Bind to an address and start listening (plain TCP)
    pub async fn bind(addr: SocketAddr) -> Result<Self, TransportError> {
        let listener = TcpListener::bind(addr).await.map_err(TransportError::Io)?;
        let local_addr = listener.local_addr().map_err(TransportError::Io)?;

        tracing::info!("Server listening on {} (plain TCP)", local_addr);
        Ok(Self {
            listener,
            local_addr,
            tls_acceptor: None,
        })
    }

    /// Bind to an address with TLS
    pub async fn bind_tls(
        addr: SocketAddr,
        cert_path: &Path,
        key_path: &Path,
    ) -> Result<Self, TransportError> {
        let listener = TcpListener::bind(addr).await.map_err(TransportError::Io)?;
        let local_addr = listener.local_addr().map_err(TransportError::Io)?;

        let acceptor = tls::create_tls_acceptor(cert_path, key_path)
            .map_err(|e| TransportError::Tls(e.to_string()))?;

        tracing::info!("Server listening on {} (TLS)", local_addr);
        Ok(Self {
            listener,
            local_addr,
            tls_acceptor: Some(acceptor),
        })
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Check if TLS is enabled
    pub fn is_tls(&self) -> bool {
        self.tls_acceptor.is_some()
    }

    /// Accept a new connection
    pub async fn accept(&self) -> Result<FramedConnection, TransportError> {
        let (stream, addr) = self.listener.accept().await.map_err(TransportError::Io)?;
        // Disable Nagle's algorithm for low-latency input forwarding
        stream.set_nodelay(true).map_err(TransportError::Io)?;

        if let Some(ref acceptor) = self.tls_acceptor {
            tracing::debug!("Performing TLS handshake with {}", addr);
            let tls_stream = acceptor
                .accept(stream)
                .await
                .map_err(|e| TransportError::Tls(format!("TLS handshake failed: {}", e)))?;

            tracing::info!("Accepted TLS connection from {}", addr);
            Ok(FramedConnection::from_tls_server(tls_stream, addr))
        } else {
            tracing::info!("Accepted connection from {}", addr);
            FramedConnection::new(stream).map_err(TransportError::Io)
        }
    }
}

/// Connect to a remote server (plain TCP)
pub async fn connect(addr: SocketAddr) -> Result<FramedConnection, TransportError> {
    tracing::info!("Connecting to {} (plain TCP)", addr);
    let stream = TcpStream::connect(addr).await.map_err(TransportError::Io)?;
    // Disable Nagle's algorithm for low-latency input forwarding
    stream.set_nodelay(true).map_err(TransportError::Io)?;
    tracing::info!("Connected to {}", addr);
    FramedConnection::new(stream).map_err(TransportError::Io)
}

/// Connect to a remote server with TLS
pub async fn connect_tls(
    addr: SocketAddr,
    server_name: &str,
    expected_fingerprint: Option<&str>,
    tofu_enabled: bool,
) -> Result<FramedConnection, TransportError> {
    tracing::info!("Connecting to {} (TLS, server_name={})", addr, server_name);

    let stream = TcpStream::connect(addr).await.map_err(TransportError::Io)?;
    stream.set_nodelay(true).map_err(TransportError::Io)?;

    let connector = tls::create_tls_connector(expected_fingerprint, tofu_enabled)
        .map_err(|e| TransportError::Tls(e.to_string()))?;

    let server_name = rustls::pki_types::ServerName::try_from(server_name.to_string())
        .map_err(|e| TransportError::Tls(format!("Invalid server name: {}", e)))?;

    tracing::debug!("Performing TLS handshake with {}", addr);
    let tls_stream = connector
        .connect(server_name, stream)
        .await
        .map_err(|e| TransportError::Tls(format!("TLS handshake failed: {}", e)))?;

    // Extract peer certificate fingerprint
    let peer_fingerprint = tls_stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|certs| certs.first())
        .map(|cert| Fingerprint::from_der(cert.as_ref()));

    if let Some(ref fp) = peer_fingerprint {
        tracing::info!("Connected to {} (TLS), peer fingerprint: {}", addr, fp);
    } else {
        tracing::info!("Connected to {} (TLS)", addr);
    }

    Ok(FramedConnection::from_tls_client(tls_stream, addr, peer_fingerprint))
}

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("TLS error: {0}")]
    Tls(String),

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
            my_direction_for_you: None,
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
