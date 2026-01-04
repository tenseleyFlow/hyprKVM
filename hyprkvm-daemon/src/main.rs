//! HyprKVM Daemon - Main entry point
//!
//! The daemon handles:
//! - Hyprland IPC communication
//! - Edge detection (mouse and keyboard)
//! - Network connections to peer machines
//! - Input capture and injection

use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::FmtSubscriber;

mod config;
mod hyprland;
mod input;
mod network;
mod state;
mod transfer;

use config::Config;

#[derive(Parser)]
#[command(name = "hyprkvm")]
#[command(about = "Hyprland-native software KVM switch")]
#[command(version)]
struct Cli {
    /// Config file path
    #[arg(short, long)]
    config: Option<std::path::PathBuf>,

    /// Increase log verbosity (-v, -vv, -vvv)
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the HyprKVM daemon
    Daemon,

    /// Show daemon status
    Status,

    /// Handle a move request (called by keybinding script)
    Move {
        /// Direction to move
        direction: String,
    },

    /// Configuration management
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Reload configuration
    Reload,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Set up logging
    let log_level = match cli.verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_target(false)
        .init();

    // Load configuration
    let config_path = cli.config.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("hyprkvm")
            .join("hyprkvm.toml")
    });

    match cli.command {
        Commands::Daemon => {
            info!("Starting HyprKVM daemon...");
            run_daemon(&config_path).await
        }
        Commands::Status => {
            show_status().await
        }
        Commands::Move { direction } => {
            handle_move(&direction).await
        }
        Commands::Config { action } => {
            match action {
                ConfigAction::Show => show_config(&config_path),
                ConfigAction::Reload => reload_config().await,
            }
        }
    }
}

async fn run_daemon(config_path: &std::path::Path) -> anyhow::Result<()> {
    // Load or create default config
    let config = match Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("Failed to load config: {e}, using defaults");
            Config::default()
        }
    };

    info!("Machine name: {}", config.machines.self_name);
    info!("Listening on port: {}", config.network.listen_port);

    // Connect to Hyprland
    info!("Connecting to Hyprland...");
    let hypr_client = hyprland::ipc::HyprlandClient::new().await?;

    // Query monitors to validate connection
    let monitors = hypr_client.monitors().await?;
    info!("Connected to Hyprland. Monitors: {}", monitors.len());
    for mon in &monitors {
        info!("  {} at ({}, {}) {}x{}", mon.name, mon.x, mon.y, mon.width, mon.height);
    }

    // Determine which edges have network neighbors
    let mut enabled_edges = Vec::new();
    for neighbor in &config.machines.neighbors {
        enabled_edges.push(neighbor.direction);
        info!("  Network neighbor: {} ({})", neighbor.name, neighbor.direction);
    }

    // If no neighbors configured, enable all edges for testing
    if enabled_edges.is_empty() {
        info!("No neighbors configured, enabling all edges for testing");
        enabled_edges = vec![
            hyprkvm_common::Direction::Left,
            hyprkvm_common::Direction::Right,
        ];
    }

    // Start edge capture
    info!("Starting edge capture for: {:?}", enabled_edges);
    let edge_capture = input::EdgeCapture::new(input::EdgeCaptureConfig {
        barrier_size: 1,
        enabled_edges,
    })?;

    // Listen for Hyprland events
    let mut event_stream = hyprland::events::HyprlandEventStream::connect().await?;

    info!("Daemon running. Move mouse to screen edges to test. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            // Check for edge events (non-blocking via channel)
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                while let Some(edge_event) = edge_capture.try_recv() {
                    info!(
                        "EDGE EVENT: {:?} at ({}, {})",
                        edge_event.direction,
                        edge_event.position.0,
                        edge_event.position.1
                    );

                    // TODO: Sprint 4 - Trigger network switch
                    // For now, just log it
                }
            }

            // Hyprland events
            event = event_stream.next_event() => {
                match event {
                    Ok(evt) => {
                        tracing::debug!("Hyprland event: {:?}", evt);
                    }
                    Err(e) => {
                        tracing::error!("Event error: {e}");
                        break;
                    }
                }
            }

            // Shutdown
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                break;
            }
        }
    }

    Ok(())
}

async fn show_status() -> anyhow::Result<()> {
    // TODO: Connect to running daemon via IPC and get status
    println!("HyprKVM Status");
    println!("==============");
    println!("Daemon: not implemented yet");
    Ok(())
}

async fn handle_move(direction: &str) -> anyhow::Result<()> {
    use hyprkvm_common::Direction;

    let dir: Direction = direction.parse()?;
    tracing::debug!("Move request: {}", dir);

    // TODO: Connect to daemon, check if network switch needed
    // For now, just execute local hyprctl move
    let output = tokio::process::Command::new("hyprctl")
        .args(["dispatch", "movefocus", &dir.to_string()])
        .output()
        .await?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::error!("hyprctl failed: {}", stderr);
    }

    Ok(())
}

fn show_config(config_path: &std::path::Path) -> anyhow::Result<()> {
    if config_path.exists() {
        let content = std::fs::read_to_string(config_path)?;
        println!("{}", content);
    } else {
        println!("No config file at {:?}", config_path);
        println!("\nDefault configuration:");
        let default = Config::default();
        println!("{}", toml::to_string_pretty(&default)?);
    }
    Ok(())
}

async fn reload_config() -> anyhow::Result<()> {
    // TODO: Send reload signal to daemon
    println!("Config reload not implemented yet");
    Ok(())
}
