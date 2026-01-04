//! Network module
//!
//! Handles peer-to-peer connections between HyprKVM instances.

#[allow(dead_code)]
pub mod peer;
pub mod transport;

pub use transport::{connect, FramedConnection, Server};
