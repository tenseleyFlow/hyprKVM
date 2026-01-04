//! Configuration management for HyprKVM

#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::Path;

use hyprkvm_common::Direction;
use serde::{Deserialize, Serialize};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub daemon: DaemonConfig,

    #[serde(default)]
    pub network: NetworkConfig,

    #[serde(default)]
    pub machines: MachinesConfig,

    #[serde(default)]
    pub input: InputConfig,

    #[serde(default)]
    pub clipboard: ClipboardConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: DaemonConfig::default(),
            network: NetworkConfig::default(),
            machines: MachinesConfig::default(),
            input: InputConfig::default(),
            clipboard: ClipboardConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from file
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)
            .map_err(|e| ConfigError::Io(e.to_string()))?;

        toml::from_str(&content)
            .map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Save configuration to file
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let content = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::Serialize(e.to_string()))?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfigError::Io(e.to_string()))?;
        }

        std::fs::write(path, content)
            .map_err(|e| ConfigError::Io(e.to_string()))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Unix socket path for IPC
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
}

fn default_socket_path() -> String {
    std::env::var("XDG_RUNTIME_DIR")
        .map(|dir| format!("{}/hyprkvm.sock", dir))
        .unwrap_or_else(|_| "/tmp/hyprkvm.sock".to_string())
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            socket_path: default_socket_path(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Port to listen on
    #[serde(default = "default_port")]
    pub listen_port: u16,

    /// Address to bind to
    #[serde(default = "default_bind_address")]
    pub bind_address: String,

    /// Connection timeout in seconds
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,

    /// Ping interval in seconds
    #[serde(default = "default_ping_interval")]
    pub ping_interval_secs: u64,

    /// Ping timeout in seconds
    #[serde(default = "default_ping_timeout")]
    pub ping_timeout_secs: u64,

    /// TLS configuration
    #[serde(default)]
    pub tls: TlsConfig,
}

fn default_port() -> u16 { 24850 }
fn default_bind_address() -> String { "0.0.0.0".to_string() }
fn default_timeout() -> u64 { 5 }
fn default_ping_interval() -> u64 { 5 }
fn default_ping_timeout() -> u64 { 10 }

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            listen_port: default_port(),
            bind_address: default_bind_address(),
            connect_timeout_secs: default_timeout(),
            ping_interval_secs: default_ping_interval(),
            ping_timeout_secs: default_ping_timeout(),
            tls: TlsConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    /// Enable TLS encryption (default: false for backwards compatibility)
    #[serde(default)]
    pub enabled: bool,

    /// Path to certificate file
    #[serde(default = "default_cert_path")]
    pub cert_path: String,

    /// Path to private key file
    #[serde(default = "default_key_path")]
    pub key_path: String,

    /// Enable TOFU (Trust On First Use) for unknown peers
    #[serde(default = "default_true")]
    pub tofu: bool,
}

fn default_cert_path() -> String {
    dirs::config_dir()
        .map(|d| d.join("hyprkvm").join("cert.pem"))
        .unwrap_or_else(|| std::path::PathBuf::from("cert.pem"))
        .to_string_lossy()
        .to_string()
}

fn default_key_path() -> String {
    dirs::config_dir()
        .map(|d| d.join("hyprkvm").join("key.pem"))
        .unwrap_or_else(|| std::path::PathBuf::from("key.pem"))
        .to_string_lossy()
        .to_string()
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            enabled: false, // Default to false for backwards compatibility
            cert_path: default_cert_path(),
            key_path: default_key_path(),
            tofu: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MachinesConfig {
    /// This machine's name
    #[serde(default = "default_machine_name")]
    pub self_name: String,

    /// Neighbor machines
    #[serde(default)]
    pub neighbors: Vec<NeighborConfig>,
}

fn default_machine_name() -> String {
    hostname::get()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".to_string())
}

impl Default for MachinesConfig {
    fn default() -> Self {
        Self {
            self_name: default_machine_name(),
            neighbors: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborConfig {
    /// Machine name
    pub name: String,

    /// Direction relative to this machine
    pub direction: Direction,

    /// Address (ip:port)
    pub address: SocketAddr,

    /// Pre-trusted certificate fingerprint (optional, for TLS pinning)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,

    /// Override TLS setting for this neighbor (uses global setting if None)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Escape hotkey configuration
    #[serde(default)]
    pub escape_hotkey: EscapeHotkeyConfig,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            escape_hotkey: EscapeHotkeyConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscapeHotkeyConfig {
    /// Primary escape key (e.g., "scroll_lock", "pause")
    #[serde(default = "default_escape_key")]
    pub key: String,

    /// Required modifiers
    #[serde(default)]
    pub modifiers: Vec<String>,

    /// Enable triple-tap escape
    #[serde(default = "default_true")]
    pub triple_tap_enabled: bool,

    /// Triple-tap key
    #[serde(default = "default_triple_tap_key")]
    pub triple_tap_key: String,

    /// Triple-tap window in milliseconds
    #[serde(default = "default_triple_tap_window")]
    pub triple_tap_window_ms: u64,
}

fn default_escape_key() -> String { "scroll_lock".to_string() }
fn default_true() -> bool { true }
fn default_triple_tap_key() -> String { "shift".to_string() }
fn default_triple_tap_window() -> u64 { 500 }

impl Default for EscapeHotkeyConfig {
    fn default() -> Self {
        Self {
            key: default_escape_key(),
            modifiers: Vec::new(),
            triple_tap_enabled: true,
            triple_tap_key: default_triple_tap_key(),
            triple_tap_window_ms: default_triple_tap_window(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardConfig {
    /// Enable clipboard sync
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Sync on control transfer enter
    #[serde(default = "default_true")]
    pub sync_on_enter: bool,

    /// Sync on control transfer leave
    #[serde(default = "default_true")]
    pub sync_on_leave: bool,

    /// Sync text content
    #[serde(default = "default_true")]
    pub sync_text: bool,

    /// Sync image content
    #[serde(default = "default_true")]
    pub sync_images: bool,

    /// Maximum clipboard size in bytes
    #[serde(default = "default_max_clipboard_size")]
    pub max_size: u64,
}

fn default_max_clipboard_size() -> u64 { 10 * 1024 * 1024 } // 10MB

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sync_on_enter: true,
            sync_on_leave: true,
            sync_text: true,
            sync_images: true,
            max_size: default_max_clipboard_size(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level (error, warn, info, debug, trace)
    #[serde(default = "default_log_level")]
    pub level: String,
}

fn default_log_level() -> String { "info".to_string() }

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

/// Configuration errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Serialize error: {0}")]
    Serialize(String),

    #[error("Validation error: {0}")]
    Validation(String),
}
