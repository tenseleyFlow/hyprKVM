//! Network module
//!
//! Handles peer-to-peer connections between HyprKVM instances.

pub mod peer;
pub mod transport;

pub use peer::{Peer, PeerError, PeerManager};
pub use transport::{connect, FramedConnection, Server, TransportError};
