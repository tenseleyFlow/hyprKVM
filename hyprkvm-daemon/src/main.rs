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
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use hyprkvm_common::Direction;
    use hyprkvm_common::protocol::{Message, HelloPayload, PROTOCOL_VERSION};

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

    // Calculate screen bounds
    let screen_width: u32 = monitors.iter().map(|m| m.x as u32 + m.width).max().unwrap_or(1920);
    let screen_height: u32 = monitors.iter().map(|m| m.y as u32 + m.height).max().unwrap_or(1080);

    // Determine which edges have network neighbors
    let mut enabled_edges = Vec::new();
    let mut neighbor_map: HashMap<Direction, SocketAddr> = HashMap::new();
    for neighbor in &config.machines.neighbors {
        enabled_edges.push(neighbor.direction);
        neighbor_map.insert(neighbor.direction, neighbor.address);
        info!("  Network neighbor: {} ({}) at {}", neighbor.name, neighbor.direction, neighbor.address);
    }

    // If no neighbors configured, just run in demo mode
    if enabled_edges.is_empty() {
        info!("No neighbors configured. Add neighbors in config to enable control transfer.");
        enabled_edges = vec![Direction::Left, Direction::Right];
    }

    // Start edge capture
    info!("Starting edge capture for: {:?}", enabled_edges);
    let edge_capture = input::EdgeCapture::new(input::EdgeCaptureConfig {
        barrier_size: 1,
        enabled_edges: enabled_edges.clone(),
    })?;

    // Create input grabber (for when we send control elsewhere)
    let input_grabber = input::InputGrabber::new(input::InputGrabberConfig::default())?;

    // Create input emulator (for when we receive control from elsewhere)
    // This is created lazily when we first need to inject
    let mut input_emulator: Option<input::InputEmulator> = None;

    // Create transfer manager
    let (transfer_manager, mut transfer_events) = transfer::TransferManager::new(
        config.machines.self_name.clone(),
    );
    let transfer_manager = Arc::new(transfer_manager);

    // Track which direction we're capturing for
    let mut capture_direction: Option<Direction> = None;
    let mut input_sequence: u64 = 0;

    // Connection storage: direction -> peer connection
    let peers: Arc<RwLock<HashMap<Direction, network::FramedConnection>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Start network server
    let listen_addr: SocketAddr = format!("0.0.0.0:{}", config.network.listen_port).parse()?;
    let server = network::Server::bind(listen_addr).await?;
    info!("Listening for connections on {}", server.local_addr());

    // Spawn task to accept incoming connections
    let peers_clone = peers.clone();
    let machine_name = config.machines.self_name.clone();
    let accept_handle = tokio::spawn(async move {
        loop {
            match server.accept().await {
                Ok(mut conn) => {
                    let addr = conn.remote_addr();
                    info!("Incoming connection from {}", addr);

                    // Receive Hello
                    match conn.recv().await {
                        Ok(Some(Message::Hello(hello))) => {
                            info!("Peer {} connected (protocol v{})", hello.machine_name, hello.protocol_version);

                            // Send HelloAck
                            let ack = Message::HelloAck(hyprkvm_common::protocol::HelloAckPayload {
                                accepted: true,
                                protocol_version: PROTOCOL_VERSION,
                                machine_name: machine_name.clone(),
                                error: None,
                            });
                            if let Err(e) = conn.send(&ack).await {
                                tracing::error!("Failed to send HelloAck: {}", e);
                                continue;
                            }

                            // TODO: Determine direction from peer info
                            // For now, assume first connection is from configured neighbor
                            // In production, match by machine name
                        }
                        Ok(Some(other)) => {
                            tracing::warn!("Expected Hello, got {:?}", other);
                        }
                        Ok(None) => {
                            tracing::debug!("Connection closed during handshake");
                        }
                        Err(e) => {
                            tracing::error!("Handshake error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Accept error: {}", e);
                }
            }
        }
    });

    // Channel for incoming messages from peers
    let (peer_msg_tx, mut peer_msg_rx) = tokio::sync::mpsc::channel::<(Direction, Message)>(64);

    // Connect to configured peers
    for neighbor in &config.machines.neighbors {
        let addr = neighbor.address;
        let direction = neighbor.direction;
        let peers_clone = peers.clone();
        let machine_name = config.machines.self_name.clone();
        let msg_tx = peer_msg_tx.clone();

        tokio::spawn(async move {
            info!("Connecting to {} at {}...", direction, addr);
            match network::connect(addr).await {
                Ok(mut conn) => {
                    // Send Hello
                    let hello = Message::Hello(HelloPayload {
                        protocol_version: PROTOCOL_VERSION,
                        machine_name: machine_name.clone(),
                        capabilities: vec![],
                    });

                    if let Err(e) = conn.send(&hello).await {
                        tracing::error!("Failed to send Hello to {}: {}", direction, e);
                        return;
                    }

                    // Wait for HelloAck
                    match conn.recv().await {
                        Ok(Some(Message::HelloAck(ack))) => {
                            if ack.accepted {
                                info!("Connected to {} ({})", ack.machine_name, direction);

                                // Split connection: store for sending, spawn receiver
                                // For now, just store and we'll poll in the main loop
                                let mut peers = peers_clone.write().await;
                                peers.insert(direction, conn);
                            } else {
                                tracing::error!("Connection rejected: {:?}", ack.error);
                            }
                        }
                        Ok(Some(other)) => {
                            tracing::warn!("Expected HelloAck, got {:?}", other);
                        }
                        Ok(None) => {
                            tracing::warn!("Connection closed during handshake");
                        }
                        Err(e) => {
                            tracing::error!("Handshake error: {}", e);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to connect to {} ({}): {}", direction, addr, e);
                }
            }
        });
    }

    // Listen for Hyprland events
    let mut event_stream = hyprland::events::HyprlandEventStream::connect().await?;

    info!("Daemon running. Move mouse to screen edges to trigger transfer. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            // Check for edge events, grabber events, and poll peer messages
            _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {
                // Forward grabbed input to remote peer
                if let Some(cap_dir) = capture_direction {
                    while let Some(grab_event) = input_grabber.try_recv() {
                        let payload = grab_event.to_protocol(input_sequence);
                        input_sequence += 1;

                        let msg = Message::InputEvent(payload);
                        let mut peers = peers.write().await;
                        if let Some(peer) = peers.get_mut(&cap_dir) {
                            if let Err(e) = peer.send(&msg).await {
                                tracing::error!("Failed to send input event: {}", e);
                            }
                        }
                    }
                }

                // Handle edge events
                while let Some(edge_event) = edge_capture.try_recv() {
                    let direction = edge_event.direction;

                    // Check if we have a peer in this direction
                    let has_peer = {
                        let peers = peers.read().await;
                        peers.contains_key(&direction)
                    };

                    if has_peer {
                        info!(
                            "EDGE: {:?} at ({}, {}) - initiating transfer",
                            direction,
                            edge_event.position.0,
                            edge_event.position.1
                        );

                        if let Err(e) = transfer_manager.initiate_transfer(
                            direction,
                            edge_event.position,
                            screen_height,
                            screen_width,
                        ).await {
                            tracing::warn!("Failed to initiate transfer: {}", e);
                        }
                    } else {
                        tracing::debug!(
                            "EDGE: {:?} but no peer connected",
                            direction
                        );
                    }
                }

                // Poll for incoming messages from peers (non-blocking)
                let directions: Vec<Direction> = {
                    let peers = peers.read().await;
                    peers.keys().cloned().collect()
                };

                for direction in directions {
                    let mut peers = peers.write().await;
                    if let Some(peer) = peers.get_mut(&direction) {
                        // Try non-blocking receive using tokio timeout
                        match tokio::time::timeout(
                            std::time::Duration::from_millis(1),
                            peer.recv()
                        ).await {
                            Ok(Ok(Some(msg))) => {
                                tracing::debug!("Received from {:?}: {:?}", direction, msg);
                                // Handle incoming message
                                match msg {
                                    Message::Enter(payload) => {
                                        info!("Received Enter from {:?}", direction);
                                        match transfer_manager.handle_enter(
                                            direction,
                                            payload,
                                            screen_width,
                                            screen_height,
                                        ).await {
                                            Ok(pos) => {
                                                info!("Positioned cursor at {:?}", pos);
                                            }
                                            Err(e) => {
                                                tracing::error!("Failed to handle Enter: {}", e);
                                            }
                                        }
                                    }
                                    Message::EnterAck(ack) => {
                                        info!("Received EnterAck: success={}", ack.success);
                                        if let Err(e) = transfer_manager.handle_enter_ack(ack).await {
                                            tracing::error!("Failed to handle EnterAck: {}", e);
                                        }
                                    }
                                    Message::Leave(payload) => {
                                        info!("Received Leave from {:?}", direction);
                                        if let Err(e) = transfer_manager.handle_leave(payload).await {
                                            tracing::error!("Failed to handle Leave: {}", e);
                                        }
                                    }
                                    Message::LeaveAck => {
                                        info!("Received LeaveAck");
                                        // Transfer complete
                                    }
                                    Message::InputEvent(input_payload) => {
                                        tracing::trace!("Received input event: {:?}", input_payload);
                                        // Inject input via emulation module
                                        if let Some(ref emu) = input_emulator {
                                            use hyprkvm_common::protocol::InputEventType;
                                            match input_payload.event {
                                                InputEventType::KeyDown { keycode } => {
                                                    emu.keyboard.key(keycode, hyprkvm_common::KeyState::Pressed);
                                                }
                                                InputEventType::KeyUp { keycode } => {
                                                    emu.keyboard.key(keycode, hyprkvm_common::KeyState::Released);
                                                }
                                                InputEventType::PointerMotion { dx, dy } => {
                                                    emu.pointer.motion(dx, dy);
                                                }
                                                InputEventType::PointerButton { button, pressed } => {
                                                    let state = if pressed {
                                                        hyprkvm_common::ButtonState::Pressed
                                                    } else {
                                                        hyprkvm_common::ButtonState::Released
                                                    };
                                                    emu.pointer.button(button, state);
                                                }
                                                InputEventType::Scroll { horizontal, vertical } => {
                                                    emu.pointer.scroll(horizontal, vertical);
                                                }
                                                InputEventType::ModifierState { .. } => {
                                                    // Modifier state is informational
                                                }
                                            }
                                        }
                                    }
                                    Message::Ping { timestamp } => {
                                        let _ = peer.send(&Message::Pong { timestamp }).await;
                                    }
                                    Message::Pong { timestamp } => {
                                        tracing::trace!("Pong received, rtt={}ms",
                                            std::time::SystemTime::now()
                                                .duration_since(std::time::UNIX_EPOCH)
                                                .unwrap()
                                                .as_millis() as u64 - timestamp
                                        );
                                    }
                                    _ => {
                                        tracing::debug!("Unhandled message: {:?}", msg);
                                    }
                                }
                            }
                            Ok(Ok(None)) => {
                                // Connection closed
                                info!("Peer {:?} disconnected", direction);
                                peers.remove(&direction);
                            }
                            Ok(Err(e)) => {
                                tracing::error!("Error receiving from {:?}: {}", direction, e);
                                peers.remove(&direction);
                            }
                            Err(_) => {
                                // Timeout - no message available, that's fine
                            }
                        }
                    }
                }
            }

            // Handle transfer events
            Some(event) = transfer_events.recv() => {
                match event {
                    transfer::TransferEvent::SendMessage { direction, message } => {
                        let mut peers = peers.write().await;
                        if let Some(peer) = peers.get_mut(&direction) {
                            tracing::debug!("Sending {:?} to {:?}", message, direction);
                            if let Err(e) = peer.send(&message).await {
                                tracing::error!("Failed to send message: {}", e);
                            }
                        } else {
                            tracing::warn!("No peer for direction {:?}", direction);
                        }
                    }
                    transfer::TransferEvent::StartCapture { direction: cap_dir } => {
                        info!("Starting input capture for {:?}", cap_dir);
                        capture_direction = Some(cap_dir);
                        input_grabber.start();
                    }
                    transfer::TransferEvent::StopCapture => {
                        info!("Stopping input capture");
                        capture_direction = None;
                        input_grabber.stop();
                    }
                    transfer::TransferEvent::StartInjection { from } => {
                        info!("Starting input injection from {:?}", from);
                        // Create input emulator if not exists
                        if input_emulator.is_none() {
                            match input::InputEmulator::new() {
                                Ok(emu) => {
                                    info!("Input emulator created");
                                    input_emulator = Some(emu);
                                }
                                Err(e) => {
                                    tracing::error!("Failed to create input emulator: {}", e);
                                }
                            }
                        }
                    }
                    transfer::TransferEvent::StopInjection => {
                        info!("Stopping input injection");
                        // Keep emulator around for next time
                    }
                }
            }

            // Hyprland events
            event = event_stream.next_event() => {
                match event {
                    Ok(evt) => {
                        tracing::trace!("Hyprland event: {:?}", evt);
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
                accept_handle.abort();
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
