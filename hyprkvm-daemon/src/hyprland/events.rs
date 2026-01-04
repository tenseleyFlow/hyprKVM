//! Hyprland event stream
//!
//! Listens to Hyprland's event socket for real-time updates.

use std::path::PathBuf;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;

use super::ipc::HyprlandError;

/// Stream of Hyprland events
pub struct HyprlandEventStream {
    reader: BufReader<UnixStream>,
}

impl HyprlandEventStream {
    /// Connect to Hyprland's event socket
    pub async fn connect() -> Result<Self, HyprlandError> {
        let socket_path = Self::discover_event_socket_path()?;

        let stream = UnixStream::connect(&socket_path)
            .await
            .map_err(|e| HyprlandError::Connection(e.to_string()))?;

        Ok(Self {
            reader: BufReader::new(stream),
        })
    }

    /// Discover the event socket path (.socket2.sock)
    fn discover_event_socket_path() -> Result<PathBuf, HyprlandError> {
        let signature = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| HyprlandError::NotRunning)?;

        let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| "/tmp".to_string());

        let socket_path = PathBuf::from(runtime_dir)
            .join("hypr")
            .join(&signature)
            .join(".socket2.sock");

        if !socket_path.exists() {
            return Err(HyprlandError::SocketNotFound(
                socket_path.to_string_lossy().to_string(),
            ));
        }

        Ok(socket_path)
    }

    /// Get the next event
    pub async fn next_event(&mut self) -> Result<HyprlandEvent, HyprlandError> {
        let mut line = String::new();

        self.reader
            .read_line(&mut line)
            .await
            .map_err(|e| HyprlandError::Io(e.to_string()))?;

        if line.is_empty() {
            return Err(HyprlandError::Connection("Socket closed".to_string()));
        }

        // Remove trailing newline
        let line = line.trim_end();

        // Parse event format: EVENT>>DATA
        if let Some((event_name, data)) = line.split_once(">>") {
            Ok(HyprlandEvent::parse(event_name, data))
        } else {
            Ok(HyprlandEvent::Unknown {
                raw: line.to_string(),
            })
        }
    }
}

/// Hyprland events
#[derive(Debug, Clone)]
pub enum HyprlandEvent {
    /// Workspace changed
    WorkspaceChanged {
        name: String,
    },

    /// Focused monitor changed
    FocusedMonitorChanged {
        monitor: String,
        workspace: String,
    },

    /// Active window changed
    ActiveWindowChanged {
        class: String,
        title: String,
    },

    /// Active window address changed (v2)
    ActiveWindowV2 {
        address: String,
    },

    /// Monitor added
    MonitorAdded {
        name: String,
    },

    /// Monitor removed
    MonitorRemoved {
        name: String,
    },

    /// Workspace created
    WorkspaceCreated {
        name: String,
    },

    /// Workspace destroyed
    WorkspaceDestroyed {
        name: String,
    },

    /// Workspace moved to monitor
    WorkspaceMoved {
        workspace: String,
        monitor: String,
    },

    /// Fullscreen state changed
    FullscreenChanged {
        fullscreen: bool,
    },

    /// Window opened
    WindowOpened {
        address: String,
        workspace: String,
        class: String,
        title: String,
    },

    /// Window closed
    WindowClosed {
        address: String,
    },

    /// Window moved
    WindowMoved {
        address: String,
        workspace: String,
    },

    /// Unknown event
    Unknown {
        raw: String,
    },
}

impl HyprlandEvent {
    /// Parse an event from name and data
    fn parse(name: &str, data: &str) -> Self {
        match name {
            "workspace" => HyprlandEvent::WorkspaceChanged {
                name: data.to_string(),
            },

            "focusedmon" => {
                let parts: Vec<&str> = data.splitn(2, ',').collect();
                HyprlandEvent::FocusedMonitorChanged {
                    monitor: parts.get(0).unwrap_or(&"").to_string(),
                    workspace: parts.get(1).unwrap_or(&"").to_string(),
                }
            }

            "activewindow" => {
                let parts: Vec<&str> = data.splitn(2, ',').collect();
                HyprlandEvent::ActiveWindowChanged {
                    class: parts.get(0).unwrap_or(&"").to_string(),
                    title: parts.get(1).unwrap_or(&"").to_string(),
                }
            }

            "activewindowv2" => HyprlandEvent::ActiveWindowV2 {
                address: data.to_string(),
            },

            "monitoradded" => HyprlandEvent::MonitorAdded {
                name: data.to_string(),
            },

            "monitorremoved" => HyprlandEvent::MonitorRemoved {
                name: data.to_string(),
            },

            "createworkspace" => HyprlandEvent::WorkspaceCreated {
                name: data.to_string(),
            },

            "destroyworkspace" => HyprlandEvent::WorkspaceDestroyed {
                name: data.to_string(),
            },

            "moveworkspace" => {
                let parts: Vec<&str> = data.splitn(2, ',').collect();
                HyprlandEvent::WorkspaceMoved {
                    workspace: parts.get(0).unwrap_or(&"").to_string(),
                    monitor: parts.get(1).unwrap_or(&"").to_string(),
                }
            }

            "fullscreen" => HyprlandEvent::FullscreenChanged {
                fullscreen: data == "1",
            },

            "openwindow" => {
                let parts: Vec<&str> = data.splitn(4, ',').collect();
                HyprlandEvent::WindowOpened {
                    address: parts.get(0).unwrap_or(&"").to_string(),
                    workspace: parts.get(1).unwrap_or(&"").to_string(),
                    class: parts.get(2).unwrap_or(&"").to_string(),
                    title: parts.get(3).unwrap_or(&"").to_string(),
                }
            }

            "closewindow" => HyprlandEvent::WindowClosed {
                address: data.to_string(),
            },

            "movewindow" => {
                let parts: Vec<&str> = data.splitn(2, ',').collect();
                HyprlandEvent::WindowMoved {
                    address: parts.get(0).unwrap_or(&"").to_string(),
                    workspace: parts.get(1).unwrap_or(&"").to_string(),
                }
            }

            _ => HyprlandEvent::Unknown {
                raw: format!("{}>>{}",name, data),
            },
        }
    }
}
