//! Peer connection management
//!
//! Handles connections to other HyprKVM instances.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio::time::timeout;

use hyprkvm_common::protocol::{
    HelloAckPayload, HelloPayload, Message, PROTOCOL_VERSION,
};

use super::transport::{connect, FramedConnection, Server, TransportError};

/// Default timeout for handshake
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// A connected peer
pub struct Peer {
    /// Connection to the peer
    pub conn: FramedConnection,
    /// Peer's machine name
    pub name: String,
    /// Peer's protocol version
    pub protocol_version: u32,
    /// Peer's capabilities
    pub capabilities: Vec<String>,
}

impl Peer {
    /// Perform client-side handshake
    pub async fn handshake_client(
        mut conn: FramedConnection,
        our_name: &str,
        our_capabilities: &[String],
    ) -> Result<Self, PeerError> {
        // Send Hello
        let hello = Message::Hello(HelloPayload {
            protocol_version: PROTOCOL_VERSION,
            machine_name: our_name.to_string(),
            capabilities: our_capabilities.to_vec(),
            my_direction_for_you: None, // Direction not known in this context
        });

        conn.send(&hello).await?;

        // Wait for HelloAck
        let response = timeout(HANDSHAKE_TIMEOUT, conn.recv())
            .await
            .map_err(|_| PeerError::HandshakeTimeout)?
            .map_err(PeerError::Transport)?
            .ok_or(PeerError::ConnectionClosed)?;

        match response {
            Message::HelloAck(ack) => {
                if !ack.accepted {
                    return Err(PeerError::HandshakeRejected(
                        ack.error.unwrap_or_else(|| "Unknown reason".to_string()),
                    ));
                }

                tracing::info!(
                    "Handshake successful with peer '{}' (protocol v{})",
                    ack.machine_name,
                    ack.protocol_version
                );

                Ok(Self {
                    conn,
                    name: ack.machine_name,
                    protocol_version: ack.protocol_version,
                    capabilities: vec![], // Server doesn't send capabilities in ack
                })
            }
            _ => Err(PeerError::UnexpectedMessage),
        }
    }

    /// Perform server-side handshake
    pub async fn handshake_server(
        mut conn: FramedConnection,
        our_name: &str,
        _our_capabilities: &[String],
    ) -> Result<Self, PeerError> {
        // Wait for Hello
        let request = timeout(HANDSHAKE_TIMEOUT, conn.recv())
            .await
            .map_err(|_| PeerError::HandshakeTimeout)?
            .map_err(PeerError::Transport)?
            .ok_or(PeerError::ConnectionClosed)?;

        match request {
            Message::Hello(hello) => {
                // Check protocol version compatibility
                let accepted = hello.protocol_version == PROTOCOL_VERSION;
                let error = if !accepted {
                    Some(format!(
                        "Protocol version mismatch: expected {}, got {}",
                        PROTOCOL_VERSION, hello.protocol_version
                    ))
                } else {
                    None
                };

                // Send HelloAck
                let ack = Message::HelloAck(HelloAckPayload {
                    accepted,
                    protocol_version: PROTOCOL_VERSION,
                    machine_name: our_name.to_string(),
                    error: error.clone(),
                });

                conn.send(&ack).await?;

                if !accepted {
                    return Err(PeerError::HandshakeRejected(error.unwrap()));
                }

                tracing::info!(
                    "Handshake successful with peer '{}' (protocol v{})",
                    hello.machine_name,
                    hello.protocol_version
                );

                Ok(Self {
                    conn,
                    name: hello.machine_name,
                    protocol_version: hello.protocol_version,
                    capabilities: hello.capabilities,
                })
            }
            _ => Err(PeerError::UnexpectedMessage),
        }
    }

    /// Send a message to the peer
    pub async fn send(&mut self, msg: &Message) -> Result<(), PeerError> {
        self.conn.send(msg).await.map_err(PeerError::Transport)
    }

    /// Receive a message from the peer
    pub async fn recv(&mut self) -> Result<Option<Message>, PeerError> {
        self.conn.recv().await.map_err(PeerError::Transport)
    }

    /// Get the remote address
    pub fn remote_addr(&self) -> SocketAddr {
        self.conn.remote_addr()
    }
}

/// Manages connections to multiple peers
pub struct PeerManager {
    /// Our machine name
    our_name: String,
    /// Our capabilities
    our_capabilities: Vec<String>,
    /// Connected peers by name
    peers: Arc<RwLock<HashMap<String, Peer>>>,
    /// Server for incoming connections
    server: Option<Server>,
}

impl PeerManager {
    /// Create a new peer manager
    pub fn new(our_name: String, our_capabilities: Vec<String>) -> Self {
        Self {
            our_name,
            our_capabilities,
            peers: Arc::new(RwLock::new(HashMap::new())),
            server: None,
        }
    }

    /// Start listening for incoming connections
    pub async fn listen(&mut self, addr: SocketAddr) -> Result<SocketAddr, PeerError> {
        let server = Server::bind(addr).await.map_err(PeerError::Transport)?;
        let local_addr = server.local_addr();
        self.server = Some(server);
        Ok(local_addr)
    }

    /// Accept a new incoming connection (call in a loop)
    pub async fn accept(&self) -> Result<Option<Peer>, PeerError> {
        let server = match &self.server {
            Some(s) => s,
            None => return Ok(None),
        };

        let conn = server.accept().await.map_err(PeerError::Transport)?;
        let peer = Peer::handshake_server(conn, &self.our_name, &self.our_capabilities).await?;

        // Add to peers map
        {
            let mut peers = self.peers.write().await;
            peers.insert(peer.name.clone(), peer);
        }

        // Return None to indicate we stored the peer internally
        // Callers should use get_peer to access it
        Ok(None)
    }

    /// Connect to a remote peer
    pub async fn connect(&self, addr: SocketAddr) -> Result<String, PeerError> {
        let conn = connect(addr).await.map_err(PeerError::Transport)?;
        let peer = Peer::handshake_client(conn, &self.our_name, &self.our_capabilities).await?;
        let name = peer.name.clone();

        // Add to peers map
        {
            let mut peers = self.peers.write().await;
            peers.insert(name.clone(), peer);
        }

        Ok(name)
    }

    /// Get a peer by name
    pub async fn get_peer(&self, name: &str) -> Option<tokio::sync::RwLockReadGuard<'_, HashMap<String, Peer>>> {
        let peers = self.peers.read().await;
        if peers.contains_key(name) {
            Some(peers)
        } else {
            None
        }
    }

    /// Remove a peer
    pub async fn remove_peer(&self, name: &str) -> Option<Peer> {
        let mut peers = self.peers.write().await;
        peers.remove(name)
    }

    /// Get names of all connected peers
    pub async fn peer_names(&self) -> Vec<String> {
        let peers = self.peers.read().await;
        peers.keys().cloned().collect()
    }

    /// Check if a peer is connected
    pub async fn is_connected(&self, name: &str) -> bool {
        let peers = self.peers.read().await;
        peers.contains_key(name)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error("Transport error: {0}")]
    Transport(#[from] TransportError),

    #[error("Handshake timeout")]
    HandshakeTimeout,

    #[error("Handshake rejected: {0}")]
    HandshakeRejected(String),

    #[error("Connection closed")]
    ConnectionClosed,

    #[error("Unexpected message during handshake")]
    UnexpectedMessage,

    #[error("Peer not found: {0}")]
    PeerNotFound(String),
}
