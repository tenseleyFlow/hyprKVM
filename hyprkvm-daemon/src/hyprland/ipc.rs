//! Hyprland IPC client
//!
//! Communicates with Hyprland via its Unix socket.

use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Hyprland IPC client
pub struct HyprlandClient {
    socket_path: PathBuf,
}

impl HyprlandClient {
    /// Create a new Hyprland client
    ///
    /// Discovers the socket path from environment variables.
    pub async fn new() -> Result<Self, HyprlandError> {
        let socket_path = Self::discover_socket_path()?;

        // Verify we can connect
        let _ = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| HyprlandError::Connection(e.to_string()))?;

        Ok(Self { socket_path })
    }

    /// Discover the Hyprland socket path
    fn discover_socket_path() -> Result<PathBuf, HyprlandError> {
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| HyprlandError::NotRunning)?;

        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| "/tmp".to_string());

        let socket_path = PathBuf::from(runtime_dir)
            .join("hypr")
            .join(&signature)
            .join(".socket.sock");

        if !socket_path.exists() {
            return Err(HyprlandError::SocketNotFound(
                socket_path.to_string_lossy().to_string(),
            ));
        }

        Ok(socket_path)
    }

    /// Execute a command and get the response
    async fn execute(&self, command: &str) -> Result<String, HyprlandError> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| HyprlandError::Connection(e.to_string()))?;

        stream
            .write_all(command.as_bytes())
            .await
            .map_err(|e| HyprlandError::Io(e.to_string()))?;

        stream
            .shutdown()
            .await
            .map_err(|e| HyprlandError::Io(e.to_string()))?;

        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .map_err(|e| HyprlandError::Io(e.to_string()))?;

        Ok(response)
    }

    /// Execute a command and parse JSON response
    pub async fn query<T: DeserializeOwned>(&self, command: &str) -> Result<T, HyprlandError> {
        // Prefix with j/ for JSON output
        let json_command = format!("j/{}", command);
        let response = self.execute(&json_command).await?;

        serde_json::from_str(&response)
            .map_err(|e| HyprlandError::Parse(format!("{}: {}", e, response)))
    }

    /// Get all monitors
    pub async fn monitors(&self) -> Result<Vec<Monitor>, HyprlandError> {
        self.query("monitors").await
    }

    /// Get all workspaces
    pub async fn workspaces(&self) -> Result<Vec<Workspace>, HyprlandError> {
        self.query("workspaces").await
    }

    /// Get active workspace
    pub async fn active_workspace(&self) -> Result<Workspace, HyprlandError> {
        self.query("activeworkspace").await
    }

    /// Get cursor position
    pub async fn cursor_pos(&self) -> Result<CursorPos, HyprlandError> {
        self.query("cursorpos").await
    }

    /// Execute a dispatcher command
    pub async fn dispatch(&self, dispatcher: &str, args: &str) -> Result<(), HyprlandError> {
        let command = format!("dispatch {} {}", dispatcher, args);
        let response = self.execute(&command).await?;

        // Check for error in response
        if response.starts_with("err") || response.contains("error") {
            return Err(HyprlandError::Dispatch(response));
        }

        Ok(())
    }
}

// ============================================================================
// Data Types
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Monitor {
    pub id: i32,
    pub name: String,
    pub description: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WorkspaceRef,
    pub focused: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceRef {
    pub id: i32,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: i32,
    pub name: String,
    pub monitor: String,
    pub windows: u32,
    #[serde(rename = "hasfullscreen")]
    pub has_fullscreen: bool,
    #[serde(rename = "lastwindow")]
    pub last_window: String,
    #[serde(rename = "lastwindowtitle")]
    pub last_window_title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CursorPos {
    pub x: i32,
    pub y: i32,
}

// ============================================================================
// Errors
// ============================================================================

#[derive(Debug, thiserror::Error)]
pub enum HyprlandError {
    #[error("Hyprland is not running (HYPRLAND_INSTANCE_SIGNATURE not set)")]
    NotRunning,

    #[error("Hyprland socket not found: {0}")]
    SocketNotFound(String),

    #[error("Connection error: {0}")]
    Connection(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Dispatch error: {0}")]
    Dispatch(String),
}
