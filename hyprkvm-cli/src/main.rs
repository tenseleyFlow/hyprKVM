//! HyprKVM CLI tool
//!
//! Separate CLI for querying daemon status and managing configuration.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use hyprkvm_common::protocol::{IpcRequest, IpcResponse};

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
    let mut client = match IpcClient::connect().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: daemon not running ({})", e);
            std::process::exit(1);
        }
    };

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

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Status { json } => handle_status(json).await?,
        Commands::Peers { json } => handle_peers(json).await?,
        Commands::Ping { peer } => handle_ping(peer).await?,
    }

    Ok(())
}
