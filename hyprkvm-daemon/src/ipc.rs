//! IPC module for CLI <-> Daemon communication
//!
//! Uses a Unix socket at $XDG_RUNTIME_DIR/hyprkvm.sock

use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use hyprkvm_common::protocol::{IpcRequest, IpcResponse};

/// Get the IPC socket path
pub fn socket_path() -> PathBuf {
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("hyprkvm.sock")
}

/// IPC Server for receiving CLI commands
pub struct IpcServer {
    listener: UnixListener,
}

impl IpcServer {
    /// Create and bind the IPC server
    pub async fn bind() -> std::io::Result<Self> {
        let path = socket_path();

        // Remove old socket if it exists
        let _ = std::fs::remove_file(&path);

        let listener = UnixListener::bind(&path)?;
        tracing::info!("IPC server listening on {}", path.display());

        Ok(Self { listener })
    }

    /// Accept a new connection
    pub async fn accept(&self) -> std::io::Result<IpcConnection> {
        let (stream, _) = self.listener.accept().await?;
        Ok(IpcConnection { stream })
    }
}

/// A single IPC connection
pub struct IpcConnection {
    stream: UnixStream,
}

impl IpcConnection {
    /// Receive a request
    pub async fn recv(&mut self) -> std::io::Result<Option<IpcRequest>> {
        let mut reader = BufReader::new(&mut self.stream);
        let mut line = String::new();

        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }

        serde_json::from_str(&line)
            .map(Some)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Send a response
    pub async fn send(&mut self, response: &IpcResponse) -> std::io::Result<()> {
        let json = serde_json::to_string(response)?;
        self.stream.write_all(json.as_bytes()).await?;
        self.stream.write_all(b"\n").await?;
        self.stream.flush().await?;
        Ok(())
    }
}

/// IPC client for sending commands to daemon
pub struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    /// Connect to the daemon
    pub async fn connect() -> std::io::Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).await?;
        Ok(Self { stream })
    }

    /// Send a request and get response
    pub async fn request(&mut self, req: &IpcRequest) -> std::io::Result<IpcResponse> {
        // Send request
        let json = serde_json::to_string(req)?;
        self.stream.write_all(json.as_bytes()).await?;
        self.stream.write_all(b"\n").await?;
        self.stream.flush().await?;

        // Read response
        let mut reader = BufReader::new(&mut self.stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;

        serde_json::from_str(&line)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}
