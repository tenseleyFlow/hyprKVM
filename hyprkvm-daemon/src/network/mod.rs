//! Network module
//!
//! Handles peer-to-peer connections between HyprKVM instances.

#[allow(dead_code)]
pub mod known_hosts;
#[allow(dead_code)]
pub mod peer;
pub mod tls;
pub mod transport;

pub use known_hosts::{KnownHosts, TrustStatus};
pub use tls::{
    create_tls_acceptor, create_tls_connector, ensure_certificate, get_cert_fingerprint,
    Fingerprint, TlsError,
};
pub use transport::{connect, connect_tls, FramedConnection, Server, TransportError};
