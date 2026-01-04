//! HyprKVM CLI tool
//!
//! Separate CLI for querying daemon status and managing configuration.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use hyprkvm_common::protocol::{IpcRequest, IpcResponse, SwitchTarget};
use hyprkvm_common::Direction;

#[derive(Parser)]
#[command(name = "hyprkvm-ctl")]
#[command(about = "HyprKVM control utility")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    // ========================================================================
    // Status & Diagnostics
    // ========================================================================
    /// Show daemon status
    Status {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// List connected peers
    Peers {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Ping a peer
    Ping {
        /// Peer name to ping
        peer: String,
    },

    // ========================================================================
    // Control Transfer
    // ========================================================================
    /// Transfer control to another machine
    Switch {
        /// Direction (left/right/up/down) or machine name
        target: String,
    },

    /// Return control to this machine
    Return,

    // ========================================================================
    // Input Management
    // ========================================================================
    /// Force release input capture (recovery)
    Release,

    /// Enable/disable edge barrier (lock cursor to this machine)
    Barrier {
        #[command(subcommand)]
        action: BarrierAction,
    },

    // ========================================================================
    // Connection Management
    // ========================================================================
    /// Disconnect from a peer
    Disconnect {
        /// Peer name to disconnect
        peer: String,
    },

    /// Reconnect to a peer
    Reconnect {
        /// Peer name to reconnect
        peer: String,
    },

    // ========================================================================
    // Configuration & Daemon
    // ========================================================================
    /// Show current configuration
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },

    /// Reload configuration from file
    Reload,

    /// Shutdown the daemon
    Shutdown,

    /// Show daemon logs
    Logs {
        /// Number of lines to show
        #[arg(short = 'n', default_value = "50")]
        lines: u32,
    },
}

#[derive(Subcommand)]
enum BarrierAction {
    /// Enable barrier (prevent cursor from leaving)
    On,
    /// Disable barrier (allow cursor to leave)
    Off,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
}

// ============================================================================
// IPC Client
// ============================================================================

/// Get the IPC socket path
fn socket_path() -> PathBuf {
    let runtime_dir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(runtime_dir).join("hyprkvm.sock")
}

/// IPC client for sending commands to daemon
struct IpcClient {
    stream: UnixStream,
}

impl IpcClient {
    /// Connect to the daemon
    async fn connect() -> std::io::Result<Self> {
        let path = socket_path();
        let stream = UnixStream::connect(&path).await?;
        Ok(Self { stream })
    }

    /// Send a request and get response
    async fn request(&mut self, req: &IpcRequest) -> std::io::Result<IpcResponse> {
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

/// Connect to daemon or exit with error
async fn connect_or_exit() -> IpcClient {
    match IpcClient::connect().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: daemon not running ({})", e);
            std::process::exit(1);
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Format uptime in human-readable form
fn format_uptime(secs: u64) -> String {
    let days = secs / 86400;
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;

    if days > 0 {
        format!("{}d {}h {}m {}s", days, hours, mins, secs)
    } else if hours > 0 {
        format!("{}h {}m {}s", hours, mins, secs)
    } else if mins > 0 {
        format!("{}m {}s", mins, secs)
    } else {
        format!("{}s", secs)
    }
}

/// Get colored status indicator
fn status_indicator(status: &str) -> &'static str {
    match status {
        "connected" => "\x1b[32m●\x1b[0m",    // Green dot
        "connecting" => "\x1b[33m●\x1b[0m",   // Yellow dot
        "disconnected" => "\x1b[31m●\x1b[0m", // Red dot
        _ => "○",                              // Empty dot
    }
}

/// Parse target as direction or machine name
fn parse_switch_target(target: &str) -> SwitchTarget {
    match target.to_lowercase().as_str() {
        "left" | "l" => SwitchTarget::Direction(Direction::Left),
        "right" | "r" => SwitchTarget::Direction(Direction::Right),
        "up" | "u" => SwitchTarget::Direction(Direction::Up),
        "down" | "d" => SwitchTarget::Direction(Direction::Down),
        _ => SwitchTarget::MachineName(target.to_string()),
    }
}

/// Handle common response types
fn handle_ok_or_error(response: IpcResponse) -> anyhow::Result<()> {
    match response {
        IpcResponse::Ok { message } => {
            println!("{}", message);
            Ok(())
        }
        IpcResponse::Transferred { to_machine } => {
            println!("Control transferred to {}", to_machine);
            Ok(())
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }
}

// ============================================================================
// Command Handlers
// ============================================================================

async fn handle_status(json_output: bool) -> anyhow::Result<()> {
    let mut client = match IpcClient::connect().await {
        Ok(c) => c,
        Err(e) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "error": "daemon not running",
                        "details": e.to_string()
                    })
                );
            } else {
                eprintln!("Error: daemon not running ({})", e);
            }
            std::process::exit(1);
        }
    };

    let response = client.request(&IpcRequest::Status).await?;

    match response {
        IpcResponse::Status {
            state,
            connected_peers,
            uptime_secs,
            machine_name,
        } => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "machine_name": machine_name,
                        "state": state,
                        "connected_peers": connected_peers,
                        "uptime_secs": uptime_secs,
                    })
                );
            } else {
                println!("HyprKVM Status");
                println!("──────────────────────────────");
                println!("Machine:  {}", machine_name);
                println!("State:    {}", state);
                println!("Uptime:   {}", format_uptime(uptime_secs));
                println!("Peers:    {} connected", connected_peers.len());
                if !connected_peers.is_empty() {
                    println!("          {}", connected_peers.join(", "));
                }
            }
        }
        IpcResponse::Error { message } => {
            if json_output {
                println!("{}", serde_json::json!({ "error": message }));
            } else {
                eprintln!("Error: {}", message);
            }
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn handle_peers(json_output: bool) -> anyhow::Result<()> {
    let mut client = match IpcClient::connect().await {
        Ok(c) => c,
        Err(e) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::json!({
                        "error": "daemon not running",
                        "details": e.to_string()
                    })
                );
            } else {
                eprintln!("Error: daemon not running ({})", e);
            }
            std::process::exit(1);
        }
    };

    let response = client.request(&IpcRequest::ListPeers).await?;

    match response {
        IpcResponse::Peers { peers } => {
            if json_output {
                println!("{}", serde_json::to_string_pretty(&peers)?);
            } else {
                if peers.is_empty() {
                    println!("No peers configured");
                } else {
                    println!("Configured Peers");
                    println!("──────────────────────────────────────────────────");
                    for peer in &peers {
                        let indicator = status_indicator(&peer.status);
                        println!(
                            "{} {} ({:?}) - {}",
                            indicator, peer.name, peer.direction, peer.address
                        );
                    }
                    println!("──────────────────────────────────────────────────");
                    let connected = peers.iter().filter(|p| p.connected).count();
                    println!("{}/{} peers connected", connected, peers.len());
                }
            }
        }
        IpcResponse::Error { message } => {
            if json_output {
                println!("{}", serde_json::json!({ "error": message }));
            } else {
                eprintln!("Error: {}", message);
            }
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn handle_ping(peer_name: String) -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;

    println!("Pinging {}...", peer_name);

    let response = client
        .request(&IpcRequest::PingPeer {
            peer_name: peer_name.clone(),
        })
        .await?;

    match response {
        IpcResponse::PingResult {
            peer_name,
            latency_ms,
            error,
        } => {
            if let Some(err) = error {
                eprintln!("Ping failed: {}", err);
                std::process::exit(1);
            } else if let Some(ms) = latency_ms {
                println!("Reply from {}: time={}ms", peer_name, ms);
            } else {
                eprintln!("Ping failed: no response");
                std::process::exit(1);
            }
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }

    Ok(())
}

async fn handle_switch(target: String) -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let switch_target = parse_switch_target(&target);
    let response = client.request(&IpcRequest::Switch { target: switch_target }).await?;
    handle_ok_or_error(response)
}

async fn handle_return() -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::Return).await?;
    handle_ok_or_error(response)
}

async fn handle_release() -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::Release).await?;
    handle_ok_or_error(response)
}

async fn handle_barrier(enabled: bool) -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::SetBarrier { enabled }).await?;
    handle_ok_or_error(response)
}

async fn handle_disconnect(peer: String) -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::Disconnect { peer_name: peer }).await?;
    handle_ok_or_error(response)
}

async fn handle_reconnect(peer: String) -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::Reconnect { peer_name: peer }).await?;
    handle_ok_or_error(response)
}

async fn handle_config_show() -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::GetConfig).await?;

    match response {
        IpcResponse::Config { toml } => {
            println!("{}", toml);
            Ok(())
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }
}

async fn handle_reload() -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::Reload).await?;
    handle_ok_or_error(response)
}

async fn handle_shutdown() -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client.request(&IpcRequest::Shutdown).await?;
    handle_ok_or_error(response)
}

async fn handle_logs(lines: u32) -> anyhow::Result<()> {
    let mut client = connect_or_exit().await;
    let response = client
        .request(&IpcRequest::GetLogs {
            lines: Some(lines),
            follow: false,
        })
        .await?;

    match response {
        IpcResponse::Logs { lines } => {
            for line in lines {
                println!("{}", line);
            }
            Ok(())
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {}", message);
            std::process::exit(1);
        }
        _ => {
            eprintln!("Unexpected response from daemon");
            std::process::exit(1);
        }
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        // Status & Diagnostics
        Commands::Status { json } => handle_status(json).await?,
        Commands::Peers { json } => handle_peers(json).await?,
        Commands::Ping { peer } => handle_ping(peer).await?,

        // Control Transfer
        Commands::Switch { target } => handle_switch(target).await?,
        Commands::Return => handle_return().await?,

        // Input Management
        Commands::Release => handle_release().await?,
        Commands::Barrier { action } => {
            let enabled = matches!(action, BarrierAction::On);
            handle_barrier(enabled).await?
        }

        // Connection Management
        Commands::Disconnect { peer } => handle_disconnect(peer).await?,
        Commands::Reconnect { peer } => handle_reconnect(peer).await?,

        // Configuration & Daemon
        Commands::Config { action } => match action {
            ConfigAction::Show => handle_config_show().await?,
        },
        Commands::Reload => handle_reload().await?,
        Commands::Shutdown => handle_shutdown().await?,
        Commands::Logs { lines } => handle_logs(lines).await?,
    }

    Ok(())
}
