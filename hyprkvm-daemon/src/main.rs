//! HyprKVM Daemon - Main entry point
//!
//! The daemon handles:
//! - Hyprland IPC communication
//! - Edge detection (mouse and keyboard)
//! - Network connections to peer machines
//! - Input capture and injection

use clap::{Parser, Subcommand};
use tracing::{info, Level};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

mod clipboard;
mod config;
#[cfg(feature = "gui")]
mod gui;
mod hyprland;
mod input;
mod ipc;
mod network;
mod state;
mod transfer;

use config::Config;

/// Convert keycode to human-readable name for logging
fn keycode_to_name(keycode: u32) -> &'static str {
    match keycode {
        1 => "ESC",
        14 => "BACKSPACE",
        15 => "TAB",
        28 => "ENTER",
        29 => "LEFTCTRL",
        42 => "LEFTSHIFT",
        54 => "RIGHTSHIFT",
        56 => "LEFTALT",
        57 => "SPACE",
        58 => "CAPSLOCK",
        97 => "RIGHTCTRL",
        100 => "RIGHTALT",
        103 => "UP",
        105 => "LEFT",
        106 => "RIGHT",
        108 => "DOWN",
        125 => "LEFTMETA",
        126 => "RIGHTMETA",
        _ => "OTHER",
    }
}

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
    command: Option<Commands>,
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

    /// Launch the graphical configuration interface
    #[cfg(feature = "gui")]
    Gui,
}

#[derive(Subcommand)]
enum ConfigAction {
    /// Show current configuration
    Show,
    /// Reload configuration
    Reload,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Set up logging with dual output (stderr + file)
    let log_level = match cli.verbose {
        0 => Level::INFO,
        1 => Level::DEBUG,
        _ => Level::TRACE,
    };

    // Create log directory
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("hyprkvm");
    std::fs::create_dir_all(&log_dir).ok();

    // File appender with daily rotation
    let file_appender = tracing_appender::rolling::daily(&log_dir, "daemon.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Build subscriber with both stderr and file layers
    let subscriber = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_writer(std::io::stderr)
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking)
        )
        .with(
            tracing_subscriber::filter::LevelFilter::from_level(log_level)
        );
    subscriber.init();

    // Load configuration path
    let config_path = cli.config.unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("hyprkvm")
            .join("hyprkvm.toml")
    });

    // Handle GUI: either explicit `gui` command OR no command (default)
    #[cfg(feature = "gui")]
    if cli.command.is_none() || matches!(cli.command, Some(Commands::Gui)) {
        info!("Starting HyprKVM GUI...");
        return gui::run_gui(&config_path);
    }

    // If GUI feature not enabled and no command given, show helpful message
    #[cfg(not(feature = "gui"))]
    if cli.command.is_none() {
        eprintln!("No command specified. Available commands:");
        eprintln!("  hyprkvm daemon   - Start the KVM daemon");
        eprintln!("  hyprkvm status   - Show daemon status");
        eprintln!("  hyprkvm config   - Configuration management");
        eprintln!();
        eprintln!("To enable the GUI, rebuild with: cargo build --features gui");
        std::process::exit(1);
    }

    // Run async commands in tokio runtime
    // At this point we know command is Some(...) because None cases are handled above
    let command = cli.command.expect("command should be Some at this point");

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            match command {
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
                #[cfg(feature = "gui")]
                Commands::Gui => unreachable!("GUI handled above"),
            }
        })
}

async fn run_daemon(config_path: &std::path::Path) -> anyhow::Result<()> {
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use hyprkvm_common::Direction;
    use hyprkvm_common::protocol::{Message, HelloPayload, PROTOCOL_VERSION};

    // Load or create default config
    let mut config = match Config::load(config_path) {
        Ok(cfg) => cfg,
        Err(e) => {
            tracing::warn!("Failed to load config: {e}, using defaults");
            Config::default()
        }
    };

    info!("Machine name: {}", config.machines.self_name);
    info!("Listening on port: {}", config.network.listen_port);

    // Track daemon start time for uptime reporting
    let daemon_start_time = std::time::Instant::now();

    // State flags for CLI control
    let barrier_enabled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_requested = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Connect to Hyprland
    info!("Connecting to Hyprland...");
    let hypr_client = hyprland::ipc::HyprlandClient::new().await?;

    // Query monitors to validate connection
    let monitors = hypr_client.monitors().await?;
    info!("Connected to Hyprland. Monitors: {}", monitors.len());
    for mon in &monitors {
        info!("  {} at ({}, {}) {}x{}", mon.name, mon.x, mon.y, mon.width, mon.height);
    }

    // Calculate screen bounds in LOGICAL coordinates (cursor position uses logical coords)
    // Hyprland reports physical dimensions, but cursor_pos() returns logical coordinates
    // Logical size = physical size / scale
    let screen_min_x: i32 = monitors.iter().map(|m| m.x).min().unwrap_or(0);
    let screen_min_y: i32 = monitors.iter().map(|m| m.y).min().unwrap_or(0);
    let screen_max_x: i32 = monitors.iter().map(|m| {
        let logical_width = (m.width as f32 / m.scale).round() as i32;
        m.x + logical_width
    }).max().unwrap_or(1920);
    let screen_max_y: i32 = monitors.iter().map(|m| {
        let logical_height = (m.height as f32 / m.scale).round() as i32;
        m.y + logical_height
    }).max().unwrap_or(1080);
    let screen_width: u32 = (screen_max_x - screen_min_x) as u32;
    let screen_height: u32 = (screen_max_y - screen_min_y) as u32;
    info!("Screen bounds (logical): ({}, {}) to ({}, {}), dimensions: {}x{}",
          screen_min_x, screen_min_y, screen_max_x, screen_max_y, screen_width, screen_height);

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
    let monitor_infos: Vec<input::MonitorInfo> = monitors.iter().map(|m| input::MonitorInfo {
        name: m.name.clone(),
        x: m.x,
        y: m.y,
        width: m.width,
        height: m.height,
        scale: m.scale,
    }).collect();

    // Store per-monitor logical bounds for cursor edge detection
    // Each tuple: (x, y, logical_width, logical_height)
    let monitor_logical_bounds: Vec<(i32, i32, i32, i32)> = monitors.iter().map(|m| {
        let logical_width = (m.width as f32 / m.scale).round() as i32;
        let logical_height = (m.height as f32 / m.scale).round() as i32;
        (m.x, m.y, logical_width, logical_height)
    }).collect();

    let edge_capture = input::EdgeCapture::new(input::EdgeCaptureConfig {
        barrier_size: 1,
        enabled_edges: enabled_edges.clone(),
        monitors: monitor_infos,
    })?;

    // Create input grabber (for when we send control elsewhere)
    // Use evdev-based grabber for reliable input capture at kernel level
    let input_grabber = input::EvdevGrabber::new()?;

    // Create input emulator (for when we receive control from elsewhere)
    // This is created lazily when we first need to inject
    let mut input_emulator: Option<input::InputEmulator> = None;

    // Create transfer manager
    let (transfer_manager, mut transfer_events) = transfer::TransferManager::new(
        config.machines.self_name.clone(),
    );
    let transfer_manager = Arc::new(transfer_manager);

    // Create clipboard manager
    let clipboard_manager = std::sync::Arc::new(clipboard::ClipboardManager::new(
        config.clipboard.clone(),
    ));

    // Track which direction we're capturing for
    let mut capture_direction: Option<Direction> = None;
    // Track relay mode: (from, to) when forwarding input from one peer to another
    let mut relay_mode: Option<(Direction, Direction)> = None;
    let mut input_sequence: u64 = 0;

    // Escape key detection
    // KEY_SCROLLLOCK = 70 in Linux evdev keycodes
    const KEY_SCROLLLOCK: u32 = 70;
    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_RIGHTSHIFT: u32 = 54;
    let mut shift_tap_times: Vec<std::time::Instant> = Vec::new();
    let triple_tap_window = std::time::Duration::from_millis(
        config.input.escape_hotkey.triple_tap_window_ms
    );

    // Cursor-based edge detection state
    let mut last_cursor_pos: Option<(i32, i32)> = None;
    let mut edge_dwell_start: Option<(Direction, std::time::Instant)> = None;
    const EDGE_THRESHOLD: i32 = 2; // Pixels from edge to count as "at edge"
    const EDGE_DWELL_MS: u64 = 50; // How long cursor must be at edge to trigger

    // Cooldown after control returns to prevent immediate bounce-back
    let mut last_control_return: Option<std::time::Instant> = None;
    const CONTROL_RETURN_COOLDOWN_MS: u64 = 1000; // 1000ms cooldown after control returns (prevents bounce-back)

    // Connection storage: direction -> peer connection
    let peers: Arc<RwLock<HashMap<Direction, network::FramedConnection>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // TLS setup (if enabled)
    let tls_enabled = config.network.tls.enabled;
    if tls_enabled {
        info!("TLS is enabled, ensuring certificates exist...");
        let cert_path = std::path::Path::new(&config.network.tls.cert_path);
        let key_path = std::path::Path::new(&config.network.tls.key_path);

        if let Err(e) = network::ensure_certificate(cert_path, key_path, &config.machines.self_name) {
            anyhow::bail!("Failed to setup TLS certificates: {}", e);
        }

        // Print certificate fingerprint for users to share
        match network::get_cert_fingerprint(cert_path) {
            Ok(fp) => {
                info!("Certificate fingerprint: {}", fp);
                info!("Share this fingerprint with peers for secure verification");
            }
            Err(e) => {
                tracing::warn!("Could not read certificate fingerprint: {}", e);
            }
        }
    } else {
        info!("TLS is disabled (plain TCP mode for backwards compatibility)");
    }

    // Load known hosts for TOFU
    let known_hosts_path = network::KnownHosts::default_path();
    let known_hosts = match network::KnownHosts::load(&known_hosts_path) {
        Ok(kh) => {
            if !kh.hosts.is_empty() {
                info!("Loaded {} known host(s) from {:?}", kh.hosts.len(), known_hosts_path);
            }
            Arc::new(RwLock::new(kh))
        }
        Err(e) => {
            tracing::warn!("Failed to load known hosts: {}, starting fresh", e);
            Arc::new(RwLock::new(network::KnownHosts::default()))
        }
    };
    let tofu_enabled = config.network.tls.tofu;

    // Start network server
    let listen_addr: SocketAddr = format!("0.0.0.0:{}", config.network.listen_port).parse()?;
    let server = if tls_enabled {
        let cert_path = std::path::Path::new(&config.network.tls.cert_path);
        let key_path = std::path::Path::new(&config.network.tls.key_path);
        network::Server::bind_tls(listen_addr, cert_path, key_path).await?
    } else {
        network::Server::bind(listen_addr).await?
    };
    info!("Listening for connections on {} (TLS: {})", server.local_addr(), tls_enabled);

    // Channel for signaling config changes that require restart
    let (restart_tx, mut restart_rx) = tokio::sync::mpsc::channel::<String>(1);

    // Spawn task to accept incoming connections
    let machine_name = config.machines.self_name.clone();
    let neighbors_for_accept = config.machines.neighbors.clone();
    let peers_for_accept = peers.clone();
    let known_hosts_for_accept = known_hosts.clone();
    let known_hosts_path_for_accept = known_hosts_path.clone();
    let config_path_for_accept = config_path.to_path_buf();
    let restart_tx_for_accept = restart_tx.clone();
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

                            // Verify TLS fingerprint if this is a TLS connection
                            if let Some(peer_fp) = conn.peer_fingerprint() {
                                let mut kh = known_hosts_for_accept.write().await;
                                match kh.is_trusted(&hello.machine_name, peer_fp) {
                                    network::TrustStatus::Trusted => {
                                        info!("Peer {} fingerprint verified", hello.machine_name);
                                        kh.touch(&hello.machine_name);
                                    }
                                    network::TrustStatus::Unknown => {
                                        if tofu_enabled {
                                            info!("TOFU: Trusting new peer {} with fingerprint {}", hello.machine_name, peer_fp);
                                            kh.trust_host(&hello.machine_name, peer_fp);
                                            if let Err(e) = kh.save(&known_hosts_path_for_accept) {
                                                tracing::error!("Failed to save known hosts: {}", e);
                                            }
                                        } else {
                                            tracing::warn!("Unknown peer {} fingerprint and TOFU disabled, rejecting", hello.machine_name);
                                            continue;
                                        }
                                    }
                                    network::TrustStatus::Changed { old_fingerprint, new_fingerprint } => {
                                        tracing::error!(
                                            "SECURITY WARNING: Peer {} fingerprint CHANGED!\n  Old: {}\n  New: {}\n  This could indicate a man-in-the-middle attack!",
                                            hello.machine_name, old_fingerprint, new_fingerprint
                                        );
                                        continue; // Reject the connection
                                    }
                                }
                            }

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

                            // Determine direction: use opposite of what peer told us, or fall back to config
                            let direction = if let Some(peer_dir) = hello.my_direction_for_you {
                                // Peer says "I have you as X", so we store them as opposite(X)
                                Some(peer_dir.opposite())
                            } else {
                                // Legacy: look up in our config
                                neighbors_for_accept
                                    .iter()
                                    .find(|n| n.name == hello.machine_name)
                                    .map(|n| n.direction)
                            };

                            // NOTE: We no longer auto-correct direction based on peer claims.
                            // Direction changes are now handled explicitly via DirectionChange messages
                            // sent when the user changes direction in the GUI.
                            // This prevents the config from being overwritten on reconnect.
                            if let Some(peer_dir) = hello.my_direction_for_you {
                                let new_dir = peer_dir.opposite();
                                let existing = neighbors_for_accept
                                    .iter()
                                    .find(|n| n.name == hello.machine_name);

                                if let Some(existing_neighbor) = existing {
                                    if existing_neighbor.direction != new_dir {
                                        // Just log the mismatch, don't auto-correct
                                        // The user should update both configs via the GUI
                                        tracing::warn!(
                                            "Direction mismatch for {}: our config says {:?}, peer claims {:?}. \
                                             Use the GUI to update directions on both machines.",
                                            hello.machine_name, existing_neighbor.direction, new_dir
                                        );
                                    }
                                }
                            }

                            if let Some(dir) = direction {
                                let mut peers = peers_for_accept.write().await;
                                if peers.contains_key(&dir) {
                                    info!("Already have connection for {:?}, dropping incoming from {}", dir, hello.machine_name);
                                    // Drop the incoming connection, keep the existing one
                                } else {
                                    info!("Storing incoming connection from {} as {:?} (peer claimed {:?})",
                                          hello.machine_name, dir, hello.my_direction_for_you);
                                    peers.insert(dir, conn);
                                }
                            } else {
                                tracing::warn!(
                                    "Unknown peer '{}' connected - not in neighbors list and no direction provided",
                                    hello.machine_name
                                );
                                // Connection will be dropped
                            }
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

    // Connect to configured peers (with retry loop)
    for neighbor in &config.machines.neighbors {
        let addr = neighbor.address;
        let direction = neighbor.direction;
        let peers_clone = peers.clone();
        let machine_name = config.machines.self_name.clone();
        let neighbor_name = neighbor.name.clone();

        // Determine if TLS should be used for this neighbor
        // Per-neighbor override takes precedence over global setting
        let use_tls = neighbor.tls.unwrap_or(tls_enabled);
        let fingerprint = neighbor.fingerprint.clone();
        let tofu_enabled = config.network.tls.tofu;
        let known_hosts_clone = known_hosts.clone();
        let known_hosts_path_clone = known_hosts_path.clone();

        tokio::spawn(async move {
            loop {
                // Check if already connected
                {
                    let peers = peers_clone.read().await;
                    if peers.contains_key(&direction) {
                        // Already connected, wait and check again
                        drop(peers);
                        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        continue;
                    }
                }

                tracing::debug!("Connecting to {} at {} (TLS: {})...", direction, addr, use_tls);

                // Connect with or without TLS
                let conn_result = if use_tls {
                    network::connect_tls(
                        addr,
                        &neighbor_name,
                        fingerprint.as_deref(),
                        tofu_enabled,
                    ).await
                } else {
                    network::connect(addr).await
                };

                match conn_result {
                    Ok(mut conn) => {
                        // Send Hello with our direction for this peer
                        // Peer will use the opposite direction to store us
                        let hello = Message::Hello(HelloPayload {
                            protocol_version: PROTOCOL_VERSION,
                            machine_name: machine_name.clone(),
                            capabilities: vec![],
                            my_direction_for_you: Some(direction),
                        });

                        if let Err(e) = conn.send(&hello).await {
                            tracing::error!("Failed to send Hello to {}: {}", direction, e);
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            continue;
                        }

                        // Wait for HelloAck
                        match conn.recv().await {
                            Ok(Some(Message::HelloAck(ack))) => {
                                if ack.accepted {
                                    // Verify TLS fingerprint if this is a TLS connection
                                    if let Some(peer_fp) = conn.peer_fingerprint() {
                                        let mut kh = known_hosts_clone.write().await;
                                        match kh.is_trusted(&neighbor_name, peer_fp) {
                                            network::TrustStatus::Trusted => {
                                                info!("Peer {} fingerprint verified", neighbor_name);
                                                kh.touch(&neighbor_name);
                                            }
                                            network::TrustStatus::Unknown => {
                                                if tofu_enabled {
                                                    info!("TOFU: Trusting new peer {} with fingerprint {}", neighbor_name, peer_fp);
                                                    kh.trust_host(&neighbor_name, peer_fp);
                                                    if let Err(e) = kh.save(&known_hosts_path_clone) {
                                                        tracing::error!("Failed to save known hosts: {}", e);
                                                    }
                                                } else {
                                                    tracing::warn!("Unknown peer {} fingerprint and TOFU disabled, rejecting", neighbor_name);
                                                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                    continue;
                                                }
                                            }
                                            network::TrustStatus::Changed { old_fingerprint, new_fingerprint } => {
                                                tracing::error!(
                                                    "SECURITY WARNING: Peer {} fingerprint CHANGED!\n  Old: {}\n  New: {}\n  This could indicate a man-in-the-middle attack!",
                                                    neighbor_name, old_fingerprint, new_fingerprint
                                                );
                                                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                                                continue; // Reject and retry
                                            }
                                        }
                                    }

                                    let mut peers = peers_clone.write().await;
                                    if peers.contains_key(&direction) {
                                        info!("Already have connection for {:?}, dropping outbound to {}", direction, ack.machine_name);
                                        // Drop this connection, keep the existing one
                                    } else {
                                        info!("Connected to {} ({})", ack.machine_name, direction);
                                        peers.insert(direction, conn);
                                    }
                                    // Stay in loop to reconnect if connection drops
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
                        tracing::debug!("Failed to connect to {} ({}): {}", direction, addr, e);
                    }
                }

                // Retry after delay
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        });
    }

    // Listen for Hyprland events
    let mut event_stream = hyprland::events::HyprlandEventStream::connect().await?;

    // Start IPC server for CLI commands
    let (ipc_tx, mut ipc_rx) = tokio::sync::mpsc::channel::<(
        hyprkvm_common::protocol::IpcRequest,
        tokio::sync::oneshot::Sender<hyprkvm_common::protocol::IpcResponse>,
    )>(16);

    tokio::spawn(async move {
        let server = match ipc::IpcServer::bind().await {
            Ok(s) => s,
            Err(e) => {
                tracing::error!("Failed to start IPC server: {}", e);
                return;
            }
        };

        loop {
            match server.accept().await {
                Ok(mut conn) => {
                    tracing::debug!("IPC: connection accepted");
                    let ipc_tx = ipc_tx.clone();
                    tokio::spawn(async move {
                        match conn.recv().await {
                            Ok(Some(request)) => {
                                tracing::debug!("IPC: received {:?}", request);
                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                if ipc_tx.send((request, resp_tx)).await.is_ok() {
                                    tracing::debug!("IPC: sent to main loop, awaiting response");
                                    match resp_rx.await {
                                        Ok(response) => {
                                            tracing::debug!("IPC: got response, sending to client");
                                            if let Err(e) = conn.send(&response).await {
                                                tracing::error!("IPC: failed to send response: {}", e);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!("IPC: response channel error: {}", e);
                                        }
                                    }
                                } else {
                                    tracing::error!("IPC: failed to send request to main loop");
                                }
                            }
                            Ok(None) => {
                                tracing::debug!("IPC: connection closed by client");
                            }
                            Err(e) => {
                                tracing::debug!("IPC recv error: {}", e);
                            }
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("IPC accept error: {}", e);
                }
            }
        }
    });

    info!("Daemon running. Move mouse to screen edges to trigger transfer. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            // Check for edge events, grabber events, and poll peer messages
            _ = tokio::time::sleep(std::time::Duration::from_micros(100)) => {
                // Forward grabbed input to remote peer
                if let Some(cap_dir) = capture_direction {
                    let mut should_escape = false;

                    // Coalesce motion events - drain queue and accumulate
                    let mut motion_dx: f64 = 0.0;
                    let mut motion_dy: f64 = 0.0;
                    let mut scroll_h: f64 = 0.0;
                    let mut scroll_v: f64 = 0.0;
                    let mut other_events: Vec<input::GrabEvent> = Vec::new();

                    while let Some(grab_event) = input_grabber.try_recv() {
                        // Check for escape key before forwarding
                        match &grab_event {
                            input::GrabEvent::KeyDown { keycode } => {
                                tracing::debug!("CAPTURE KeyDown: keycode={} ({})",
                                    keycode, keycode_to_name(*keycode));

                                // Check for scroll_lock
                                if *keycode == KEY_SCROLLLOCK {
                                    info!("Scroll Lock pressed - returning control to local");
                                    should_escape = true;
                                    continue; // Don't forward this key
                                }

                                // Check for triple-tap shift
                                if config.input.escape_hotkey.triple_tap_enabled {
                                    if *keycode == KEY_LEFTSHIFT || *keycode == KEY_RIGHTSHIFT {
                                        let now = std::time::Instant::now();
                                        // Remove old taps outside the window
                                        shift_tap_times.retain(|t| now.duration_since(*t) < triple_tap_window);
                                        shift_tap_times.push(now);

                                        if shift_tap_times.len() >= 3 {
                                            info!("Triple-tap Shift detected - returning control to local");
                                            should_escape = true;
                                            shift_tap_times.clear();
                                            continue;
                                        }
                                    }
                                }
                                other_events.push(grab_event);
                            }
                            input::GrabEvent::KeyUp { keycode } => {
                                tracing::debug!("CAPTURE KeyUp: keycode={} ({})",
                                    keycode, keycode_to_name(*keycode));
                                other_events.push(grab_event);
                            }
                            input::GrabEvent::PointerMotion { dx, dy } => {
                                motion_dx += dx;
                                motion_dy += dy;
                            }
                            input::GrabEvent::PointerButton { .. } => {
                                other_events.push(grab_event);
                            }
                            input::GrabEvent::Scroll { horizontal, vertical } => {
                                scroll_h += horizontal;
                                scroll_v += vertical;
                            }
                            input::GrabEvent::ModifiersChanged { .. } => {
                                other_events.push(grab_event);
                            }
                            input::GrabEvent::RecoveryHotkey { .. } => {
                                // Should not happen during capture, ignore
                                tracing::warn!("RecoveryHotkey received during capture, ignoring");
                            }
                        }
                    }

                    // Send non-motion events first (preserve order for key events)
                    {
                        let mut peers_guard = peers.write().await;
                        if let Some(peer) = peers_guard.get_mut(&cap_dir) {
                            for event in other_events {
                                let payload = event.to_protocol(input_sequence);
                                input_sequence += 1;
                                if let Err(e) = peer.send(&Message::InputEvent(payload)).await {
                                    tracing::error!("Failed to send input event: {}", e);
                                }
                            }

                            // Send coalesced motion as single event
                            if motion_dx != 0.0 || motion_dy != 0.0 {
                                let motion_event = input::GrabEvent::PointerMotion { dx: motion_dx, dy: motion_dy };
                                let payload = motion_event.to_protocol(input_sequence);
                                input_sequence += 1;
                                if let Err(e) = peer.send(&Message::InputEvent(payload)).await {
                                    tracing::error!("Failed to send motion event: {}", e);
                                }
                            }

                            // Send coalesced scroll as single event
                            if scroll_h != 0.0 || scroll_v != 0.0 {
                                let scroll_event = input::GrabEvent::Scroll { horizontal: scroll_h, vertical: scroll_v };
                                let payload = scroll_event.to_protocol(input_sequence);
                                input_sequence += 1;
                                if let Err(e) = peer.send(&Message::InputEvent(payload)).await {
                                    tracing::error!("Failed to send scroll event: {}", e);
                                }
                            }
                        }
                    }

                    // If escape was triggered, stop capture and send Leave
                    if should_escape {
                        info!("Escape triggered - stopping capture");
                        capture_direction = None;
                        input_grabber.stop(None); // No recovery needed for escape

                        // Send Leave message - we're leaving in the opposite direction (returning to us)
                        let leave = Message::Leave(hyprkvm_common::protocol::LeavePayload {
                            to_direction: cap_dir.opposite(),
                            cursor_pos: hyprkvm_common::protocol::CursorEntryPos::EdgeRelative(0.5),
                            modifiers: hyprkvm_common::ModifierState::default(),
                            transfer_id: input_sequence, // Use as a simple unique ID
                        });
                        let mut peers_guard = peers.write().await;
                        if let Some(peer) = peers_guard.get_mut(&cap_dir) {
                            if let Err(e) = peer.send(&leave).await {
                                tracing::error!("Failed to send Leave: {}", e);
                            }
                        }
                    }
                } else {
                    // Not capturing - check for RecoveryHotkey events from recovery mode
                    // These bypass libinput's stale state by detecting keypresses directly at evdev level
                    while let Some(grab_event) = input_grabber.try_recv() {
                        if let input::GrabEvent::RecoveryHotkey { direction } = grab_event {
                            info!("RECOVERY HOTKEY: Super+{:?} detected via evdev", direction);

                            // Same at_edge check as IPC Move - only transfer if at edge monitor+window
                            let at_edge = 'edge_check: {
                                // Get monitors and find focused one
                                let monitors = match hypr_client.monitors().await {
                                    Ok(m) => m,
                                    Err(e) => {
                                        info!("  RECOVERY edge_check: monitors query failed: {}", e);
                                        break 'edge_check false;
                                    }
                                };
                                let focused_monitor = match monitors.iter().find(|m| m.focused) {
                                    Some(m) => m,
                                    None => {
                                        info!("  RECOVERY edge_check: no focused monitor found");
                                        break 'edge_check false;
                                    }
                                };

                                // Check if there's another monitor in the requested direction
                                // Use logical dimensions (physical / scale) since positions are logical
                                let has_monitor_in_direction = monitors.iter().any(|m| {
                                    if m.id == focused_monitor.id { return false; }
                                    let m_logical_w = (m.width as f32 / m.scale).round() as i32;
                                    let m_logical_h = (m.height as f32 / m.scale).round() as i32;
                                    let focused_logical_w = (focused_monitor.width as f32 / focused_monitor.scale).round() as i32;
                                    let focused_logical_h = (focused_monitor.height as f32 / focused_monitor.scale).round() as i32;
                                    match direction {
                                        Direction::Left => m.x + m_logical_w <= focused_monitor.x,
                                        Direction::Right => m.x >= focused_monitor.x + focused_logical_w,
                                        Direction::Up => m.y + m_logical_h <= focused_monitor.y,
                                        Direction::Down => m.y >= focused_monitor.y + focused_logical_h,
                                    }
                                });

                                if has_monitor_in_direction {
                                    info!("  RECOVERY edge_check: has monitor in direction {:?}", direction);
                                    break 'edge_check false;
                                }

                                // On edge monitor. Check if at edge window.
                                let active_window: serde_json::Value = match hypr_client.query("activewindow").await {
                                    Ok(w) => w,
                                    Err(e) => {
                                        info!("  RECOVERY edge_check: activewindow query failed: {}", e);
                                        break 'edge_check false;
                                    }
                                };

                                let win_x = active_window.get("at").and_then(|a| a.get(0)).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                                let win_y = active_window.get("at").and_then(|a| a.get(1)).and_then(|y| y.as_i64()).unwrap_or(0) as i32;
                                let win_w = active_window.get("size").and_then(|s| s.get(0)).and_then(|w| w.as_i64()).unwrap_or(100) as i32;
                                let win_h = active_window.get("size").and_then(|s| s.get(1)).and_then(|h| h.as_i64()).unwrap_or(100) as i32;

                                // Get all clients (windows)
                                let clients: Vec<serde_json::Value> = match hypr_client.query("clients").await {
                                    Ok(c) => c,
                                    Err(e) => {
                                        info!("  RECOVERY edge_check: clients query failed: {}", e);
                                        break 'edge_check false;
                                    }
                                };

                                // Check if any window is further in the requested direction on same monitor
                                // For Up/Down: use monitor proximity instead of window detection (bars/panels cause false positives)
                                let mon_logical_h = (focused_monitor.height as f32 / focused_monitor.scale).round() as i32;
                                let has_window_in_direction = match direction {
                                    Direction::Up => {
                                        // Window is at top edge if its top is within 100px of monitor top
                                        let near_top = win_y <= focused_monitor.y + 100;
                                        !near_top
                                    }
                                    Direction::Down => {
                                        // Window is at bottom edge if its bottom is within 100px of monitor bottom
                                        let win_bottom = win_y + win_h;
                                        let mon_bottom = focused_monitor.y + mon_logical_h;
                                        let near_bottom = win_bottom >= mon_bottom - 100;
                                        !near_bottom
                                    }
                                    Direction::Left | Direction::Right => {
                                        // Window-based detection for horizontal directions
                                        clients.iter().any(|client| {
                                            let mon = client.get("monitor").and_then(|m| m.as_i64()).unwrap_or(-1) as i32;
                                            if mon != focused_monitor.id { return false; }

                                            let cx = client.get("at").and_then(|a| a.get(0)).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                                            let cw = client.get("size").and_then(|s| s.get(0)).and_then(|w| w.as_i64()).unwrap_or(0) as i32;

                                            match direction {
                                                Direction::Left => cx + cw < win_x + 10,
                                                Direction::Right => cx > win_x + win_w - 10,
                                                _ => false,
                                            }
                                        })
                                    }
                                };

                                info!("  RECOVERY edge_check: has_window_in_direction={} -> at_edge={}", has_window_in_direction, !has_window_in_direction);
                                !has_window_in_direction
                            };

                            // Check if we have a peer in this direction
                            let has_peer = {
                                let peers = peers.read().await;
                                peers.contains_key(&direction)
                            };

                            if at_edge && has_peer {
                                // Get cursor position for transfer
                                let cursor_pos = hypr_client.cursor_pos().await
                                    .map(|c| (c.x, c.y))
                                    .unwrap_or((0, 0));

                                if barrier_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                                    info!("RECOVERY HOTKEY: Barrier enabled, blocking transfer");
                                } else {
                                    info!("RECOVERY HOTKEY: At edge with peer, initiating transfer to {:?}", direction);
                                    if let Err(e) = transfer_manager.initiate_transfer(
                                        direction,
                                        cursor_pos,
                                        screen_min_x,
                                        screen_min_y,
                                        screen_max_x,
                                        screen_max_y,
                                        true, // keyboard-initiated (recovery hotkey)
                                    ).await {
                                        tracing::error!("Failed to initiate transfer from recovery hotkey: {}", e);
                                    }
                                }
                            } else if !at_edge {
                                // Not at edge - need to do movefocus ourselves because libinput
                                // DROPPED the keypress due to stale state (it thinks the arrow key
                                // is still pressed from before the grab). This is the whole reason
                                // recovery mode exists.
                                let hypr_dir = match direction {
                                    Direction::Left => "l",
                                    Direction::Right => "r",
                                    Direction::Up => "u",
                                    Direction::Down => "d",
                                };
                                info!("RECOVERY HOTKEY: Not at edge, doing movefocus {} (libinput dropped the keypress)", hypr_dir);
                                match hypr_client.dispatch("movefocus", hypr_dir).await {
                                    Ok(()) => info!("  RECOVERY movefocus succeeded"),
                                    Err(e) => tracing::error!("  RECOVERY movefocus failed: {}", e),
                                }
                            } else {
                                info!("RECOVERY HOTKEY: No peer in direction {:?}", direction);
                            }
                        }
                    }
                }

                // Handle edge events from layer-shell barriers (for inter-monitor edges)
                while let Some(edge_event) = edge_capture.try_recv() {
                    let direction = edge_event.direction;

                    // Verify cursor is actually at screen boundary using Hyprland
                    // (barrier placement can be wrong on multi-monitor setups)
                    let cursor_pos = match hypr_client.cursor_pos().await {
                        Ok(pos) => (pos.x, pos.y),
                        Err(_) => continue, // Can't verify, skip this event
                    };
                    let is_at_screen_edge = match direction {
                        Direction::Left => cursor_pos.0 <= screen_min_x + 5,
                        Direction::Right => cursor_pos.0 >= screen_max_x - 5,
                        Direction::Up => cursor_pos.1 <= screen_min_y + 5,
                        Direction::Down => cursor_pos.1 >= screen_max_y - 5,
                    };

                    if !is_at_screen_edge {
                        tracing::debug!(
                            "EDGE: {:?} barrier triggered but cursor at ({}, {}) not at screen edge (bounds: {} to {}), ignoring",
                            direction,
                            cursor_pos.0,
                            cursor_pos.1,
                            screen_min_x,
                            screen_max_x
                        );
                        continue;
                    }

                    // Check if we have a peer in this direction
                    let has_peer = {
                        let peers = peers.read().await;
                        peers.contains_key(&direction)
                    };

                    if has_peer {
                        // Check if we're in ReceivedControl state from this direction
                        // If so, return control instead of initiating a new transfer
                        let current_state = transfer_manager.state().await;
                        if let transfer::TransferState::ReceivedControl { from, .. } = current_state {
                            if from == direction {
                                info!(
                                    "EDGE: {:?} at ({}, {}) - returning control",
                                    direction,
                                    cursor_pos.0,
                                    cursor_pos.1
                                );
                                if let Err(e) = transfer_manager.return_control().await {
                                    tracing::warn!("Failed to return control: {}", e);
                                } else {
                                    // Set cooldown to prevent immediate re-transfer
                                    last_control_return = Some(std::time::Instant::now());
                                }
                                continue;
                            }
                        }

                        // Check cooldown to prevent bounce-back loops
                        if let Some(last_return) = last_control_return {
                            if last_return.elapsed().as_millis() < CONTROL_RETURN_COOLDOWN_MS as u128 {
                                tracing::debug!("EDGE: {:?} - in cooldown, ignoring", direction);
                                continue;
                            }
                        }

                        if barrier_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                            info!(
                                "EDGE: {:?} at ({}, {}) - barrier enabled, blocking",
                                direction,
                                cursor_pos.0,
                                cursor_pos.1
                            );
                        } else {
                            info!(
                                "EDGE: {:?} at ({}, {}) - initiating transfer",
                                direction,
                                cursor_pos.0,
                                cursor_pos.1
                            );

                            if let Err(e) = transfer_manager.initiate_transfer(
                                direction,
                                cursor_pos,
                                screen_min_x,
                                screen_min_y,
                                screen_max_x,
                                screen_max_y,
                                false, // not keyboard-initiated (mouse edge)
                            ).await {
                                tracing::warn!("Failed to initiate transfer: {}", e);
                            }
                        }
                    } else {
                        tracing::debug!(
                            "EDGE: {:?} but no peer connected",
                            direction
                        );
                    }
                }

                // Cursor-based edge detection (for absolute screen edges)
                // This catches the case where cursor is at the edge and can't go further
                if capture_direction.is_none() {
                    if let Ok(cursor) = hypr_client.cursor_pos().await {
                        let (cx, cy) = (cursor.x, cursor.y);

                        // Determine if cursor is at a screen edge
                        // For Left/Right: use global screen bounds
                        // For Up/Down: check per-monitor bounds (different monitors have different heights)
                        let at_edge: Option<Direction> = if cx <= EDGE_THRESHOLD {
                            Some(Direction::Left)
                        } else if cx >= screen_width as i32 - EDGE_THRESHOLD {
                            Some(Direction::Right)
                        } else {
                            // Check per-monitor Up/Down edges
                            let mut edge_found = None;
                            for &(mon_x, mon_y, mon_w, mon_h) in &monitor_logical_bounds {
                                // Check if cursor is within this monitor's x range
                                if cx >= mon_x && cx < mon_x + mon_w {
                                    // Check Up edge (top of this monitor)
                                    if cy <= mon_y + EDGE_THRESHOLD && cy >= mon_y {
                                        edge_found = Some(Direction::Up);
                                        break;
                                    }
                                    // Check Down edge (bottom of this monitor)
                                    if cy >= mon_y + mon_h - EDGE_THRESHOLD && cy <= mon_y + mon_h {
                                        edge_found = Some(Direction::Down);
                                        break;
                                    }
                                }
                            }
                            edge_found
                        };

                        // Debug: Log when cursor is at Up/Down edge
                        if matches!(at_edge, Some(Direction::Up) | Some(Direction::Down)) {
                            let current_state = transfer_manager.state().await;
                            tracing::debug!(
                                "CURSOR at {:?} edge: pos=({}, {}), bounds=(0,0)-({}x{}), state={:?}, enabled_edges={:?}",
                                at_edge, cx, cy, screen_width, screen_height, current_state, enabled_edges
                            );
                        }

                        // Check if we should trigger based on dwell time and movement
                        if let Some(edge_dir) = at_edge {
                            // Only care about edges with neighbors
                            if enabled_edges.contains(&edge_dir) {
                                let now = std::time::Instant::now();

                                // Check if cursor is moving toward the edge (or staying at it)
                                let moving_toward_edge = if let Some((last_x, last_y)) = last_cursor_pos {
                                    match edge_dir {
                                        Direction::Left => cx <= last_x,
                                        Direction::Right => cx >= last_x,
                                        Direction::Up => cy <= last_y,
                                        Direction::Down => cy >= last_y,
                                    }
                                } else {
                                    true
                                };

                                if moving_toward_edge {
                                    match &edge_dwell_start {
                                        Some((dir, start)) if *dir == edge_dir => {
                                            // Already tracking this edge, check if dwell time exceeded
                                            if now.duration_since(*start).as_millis() >= EDGE_DWELL_MS as u128 {
                                                // Trigger!
                                                let has_peer = {
                                                    let peers = peers.read().await;
                                                    peers.contains_key(&edge_dir)
                                                };

                                                if has_peer {
                                                    // Check if we're in ReceivedControl state from this direction
                                                    let current_state = transfer_manager.state().await;
                                                    if let transfer::TransferState::ReceivedControl { from, .. } = &current_state {
                                                        if *from == edge_dir {
                                                            info!(
                                                                "CURSOR EDGE: {:?} at ({}, {}) - returning control",
                                                                edge_dir, cx, cy
                                                            );
                                                            if let Err(e) = transfer_manager.return_control().await {
                                                                tracing::warn!("Failed to return control: {}", e);
                                                            } else {
                                                                // Set cooldown to prevent immediate re-transfer
                                                                last_control_return = Some(std::time::Instant::now());
                                                            }
                                                            edge_dwell_start = None;
                                                            continue;
                                                        } else {
                                                            tracing::debug!(
                                                                "CURSOR EDGE: {:?} - ReceivedControl from {:?}, not matching",
                                                                edge_dir, from
                                                            );
                                                        }
                                                    } else {
                                                        tracing::debug!(
                                                            "CURSOR EDGE: {:?} - state is {:?}, not ReceivedControl",
                                                            edge_dir, current_state
                                                        );
                                                    }

                                                    // Check cooldown to prevent bounce-back
                                                    if let Some(last_return) = last_control_return {
                                                        if last_return.elapsed().as_millis() < CONTROL_RETURN_COOLDOWN_MS as u128 {
                                                            tracing::debug!("CURSOR EDGE: {:?} - in cooldown", edge_dir);
                                                            edge_dwell_start = None;
                                                            continue;
                                                        }
                                                    }

                                                    if barrier_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                                                        info!(
                                                            "CURSOR EDGE: {:?} at ({}, {}) - barrier enabled, blocking",
                                                            edge_dir, cx, cy
                                                        );
                                                    } else {
                                                        info!(
                                                            "CURSOR EDGE: {:?} at ({}, {}) - initiating transfer",
                                                            edge_dir, cx, cy
                                                        );

                                                        if let Err(e) = transfer_manager.initiate_transfer(
                                                            edge_dir,
                                                            (cx, cy),
                                                            screen_min_x,
                                                            screen_min_y,
                                                            screen_max_x,
                                                            screen_max_y,
                                                            false, // not keyboard-initiated (cursor edge)
                                                        ).await {
                                                            tracing::warn!("Failed to initiate transfer: {}", e);
                                                        }
                                                    }
                                                } else {
                                                    tracing::debug!(
                                                        "CURSOR EDGE: {:?} at ({}, {}) but no peer connected",
                                                        edge_dir, cx, cy
                                                    );
                                                }

                                                // Reset to avoid repeated triggers
                                                edge_dwell_start = None;
                                            }
                                        }
                                        _ => {
                                            // Start tracking this edge
                                            tracing::trace!("Started edge dwell tracking for {:?} at ({}, {})", edge_dir, cx, cy);
                                            edge_dwell_start = Some((edge_dir, now));
                                        }
                                    }
                                } else {
                                    // Moving away from edge, reset
                                    edge_dwell_start = None;
                                }
                            }
                        } else {
                            // Not at any edge, reset
                            edge_dwell_start = None;
                        }

                        last_cursor_pos = Some((cx, cy));
                    }
                }

                // Check for transfer timeout (stuck in Initiating state)
                if let transfer::TransferState::Initiating { started_at, .. } = transfer_manager.state().await {
                    const TRANSFER_TIMEOUT_MS: u128 = 3000;
                    if started_at.elapsed().as_millis() > TRANSFER_TIMEOUT_MS {
                        tracing::warn!("Transfer timed out after {}ms, aborting", TRANSFER_TIMEOUT_MS);
                        transfer_manager.abort().await;
                    }
                }

                // Poll for incoming messages from peers (non-blocking)
                let directions: Vec<Direction> = {
                    let peers = peers.read().await;
                    peers.keys().cloned().collect()
                };

                // Debug: log state and peers occasionally (every ~5 seconds at 100μs polling)
                static POLL_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let count = POLL_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if count % 50000 == 0 {
                    let state = transfer_manager.state().await;
                    tracing::info!("Poll #{}: state={:?}, peers={:?}", count, state, directions);
                }

                for direction in directions {
                    // Clone Arc before shadowing for use in spawned tasks
                    let peers_arc = peers.clone();
                    let mut peers = peers.write().await;
                    if let Some(peer) = peers.get_mut(&direction) {
                        // Try non-blocking receive using tokio timeout
                        // Use minimal timeout to avoid blocking the event loop
                        match tokio::time::timeout(
                            std::time::Duration::from_micros(50),
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
                                            screen_min_x,
                                            screen_min_y,
                                            screen_max_x,
                                            screen_max_y,
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
                                            // Usually a benign race condition (collision resolved)
                                            tracing::debug!("Failed to handle EnterAck: {}", e);
                                        }
                                    }
                                    Message::Leave(payload) => {
                                        info!("Received Leave from {:?}", direction);
                                        if let Err(e) = transfer_manager.handle_leave(payload).await {
                                            // Usually a benign race condition
                                            tracing::debug!("Failed to handle Leave: {}", e);
                                        }
                                        // Set cooldown to prevent bounce-back loop
                                        // When we receive Leave, control is returning to us
                                        last_control_return = Some(std::time::Instant::now());
                                        tracing::debug!("Set control return cooldown");
                                    }
                                    Message::LeaveAck => {
                                        info!("Received LeaveAck");
                                        // Transfer complete
                                    }
                                    Message::InputEvent(input_payload) => {
                                        // Check if we're in relay mode and this is from the relay source
                                        if let Some((relay_from, relay_to)) = relay_mode {
                                            if direction == relay_from {
                                                // Forward to relay target instead of injecting locally
                                                tracing::trace!("Relaying input from {:?} to {:?}", relay_from, relay_to);
                                                // Need to drop current borrow and get relay target
                                                drop(peers);
                                                let mut peers_guard = peers_arc.write().await;
                                                if let Some(target_peer) = peers_guard.get_mut(&relay_to) {
                                                    if let Err(e) = target_peer.send(&Message::InputEvent(input_payload)).await {
                                                        tracing::error!("Failed to relay input to {:?}: {}", relay_to, e);
                                                    }
                                                }
                                                continue; // Skip local injection
                                            }
                                        }

                                        // Normal case: inject input via emulation module
                                        if let Some(ref mut emu) = input_emulator {
                                            use hyprkvm_common::protocol::InputEventType;
                                            match input_payload.event {
                                                InputEventType::KeyDown { keycode } => {
                                                    tracing::debug!("RECV KeyDown: keycode={} ({})",
                                                        keycode, keycode_to_name(keycode));
                                                    emu.keyboard.key(keycode, hyprkvm_common::KeyState::Pressed);
                                                }
                                                InputEventType::KeyUp { keycode } => {
                                                    tracing::debug!("RECV KeyUp: keycode={} ({})",
                                                        keycode, keycode_to_name(keycode));
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
                                    Message::ClipboardOffer(offer) => {
                                        // Handle clipboard offer from peer
                                        let cm = clipboard_manager.clone();
                                        let peers_clone = peers_arc.clone();
                                        let dir = direction;
                                        tokio::spawn(async move {
                                            if let Some(request) = cm.handle_offer(offer).await {
                                                let mut peers_guard = peers_clone.write().await;
                                                if let Some(peer) = peers_guard.get_mut(&dir) {
                                                    if let Err(e) = peer.send(&Message::ClipboardRequest(request)).await {
                                                        tracing::warn!("Failed to send clipboard request: {}", e);
                                                    }
                                                }
                                            }
                                        });
                                    }
                                    Message::ClipboardRequest(request) => {
                                        // Handle clipboard request from peer
                                        let cm = clipboard_manager.clone();
                                        let peers_clone = peers_arc.clone();
                                        let dir = direction;
                                        tokio::spawn(async move {
                                            match cm.handle_request(request).await {
                                                Ok(data_chunks) => {
                                                    let mut peers_guard = peers_clone.write().await;
                                                    if let Some(peer) = peers_guard.get_mut(&dir) {
                                                        for chunk in data_chunks {
                                                            if let Err(e) = peer.send(&Message::ClipboardData(chunk)).await {
                                                                tracing::warn!("Failed to send clipboard data: {}", e);
                                                                break;
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::warn!("Clipboard request failed: {}", e);
                                                }
                                            }
                                        });
                                    }
                                    Message::ClipboardData(data) => {
                                        // Handle clipboard data from peer
                                        let cm = clipboard_manager.clone();
                                        tokio::spawn(async move {
                                            if let Err(e) = cm.handle_data(data).await {
                                                tracing::warn!("Clipboard data handling failed: {}", e);
                                            }
                                        });
                                    }
                                    Message::DirectionChange(payload) => {
                                        // Peer is notifying us that they changed our relative direction
                                        // We need to update our config to store them in the opposite direction
                                        let new_dir_for_peer = payload.your_direction_from_me.opposite();
                                        info!("Received DirectionChange: peer says we are {:?} from them, so we store them as {:?}",
                                            payload.your_direction_from_me, new_dir_for_peer);

                                        // Find the peer's name from config based on direction
                                        let peer_name = config.machines.neighbors
                                            .iter()
                                            .find(|n| n.direction == direction)
                                            .map(|n| n.name.clone())
                                            .unwrap_or_else(|| "unknown".to_string());

                                        // Load, update, and save config
                                        let config_path_clone = config_path.clone();
                                        match Config::load(&config_path_clone) {
                                            Ok(mut cfg) => {
                                                let mut found = false;
                                                for neighbor in &mut cfg.machines.neighbors {
                                                    if neighbor.name == peer_name {
                                                        info!("DirectionChange: updating {} direction {:?} -> {:?}",
                                                            neighbor.name, neighbor.direction, new_dir_for_peer);
                                                        neighbor.direction = new_dir_for_peer;
                                                        found = true;
                                                        break;
                                                    }
                                                }

                                                if found {
                                                    if let Err(e) = cfg.save(&config_path_clone) {
                                                        tracing::error!("DirectionChange: failed to save config: {}", e);
                                                        let _ = peer.send(&Message::DirectionChangeAck { success: false }).await;
                                                    } else {
                                                        info!("DirectionChange: config updated, signaling restart");
                                                        let _ = peer.send(&Message::DirectionChangeAck { success: true }).await;
                                                        // Signal restart to apply new edge barriers
                                                        let _ = restart_tx.try_send(format!("Direction sync from {}", peer_name));
                                                    }
                                                } else {
                                                    tracing::warn!("DirectionChange: peer {} not found in config", peer_name);
                                                    let _ = peer.send(&Message::DirectionChangeAck { success: false }).await;
                                                }
                                            }
                                            Err(e) => {
                                                tracing::error!("DirectionChange: failed to load config: {}", e);
                                                let _ = peer.send(&Message::DirectionChangeAck { success: false }).await;
                                            }
                                        }
                                    }
                                    Message::DirectionChangeAck { success } => {
                                        if success {
                                            info!("DirectionChangeAck: peer acknowledged direction update");
                                        } else {
                                            tracing::warn!("DirectionChangeAck: peer failed to update direction");
                                        }
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
                            info!("Sending {:?} to {:?}", message, direction);
                            if let Err(e) = peer.send(&message).await {
                                tracing::error!("Failed to send message to {:?}: {}", direction, e);
                                // If send fails, abort the transfer
                                transfer_manager.abort().await;
                            }
                        } else {
                            tracing::warn!("No peer for direction {:?}, aborting transfer", direction);
                            transfer_manager.abort().await;
                        }
                    }
                    transfer::TransferEvent::StartCapture { direction: cap_dir, keyboard_initiated } => {
                        info!("StartCapture event received for {:?}, keyboard_initiated={}", cap_dir, keyboard_initiated);
                        capture_direction = Some(cap_dir);

                        // Only send synthetic Super key-down if the transfer was keyboard-initiated.
                        // When triggered via Super+Arrow keybinding, Super was already held when
                        // the grab started. The evdev grabber won't see the initial Super key-down,
                        // so we send it explicitly so the destination knows Super is pressed.
                        // For CLI-initiated switches, the user isn't holding Super, so don't send it.
                        if keyboard_initiated {
                            let mut peers_guard = peers.write().await;
                            if let Some(peer) = peers_guard.get_mut(&cap_dir) {
                                let super_down = input::GrabEvent::KeyDown { keycode: 125 }; // KEY_LEFTMETA
                                let payload = super_down.to_protocol(input_sequence);
                                input_sequence += 1;
                                tracing::debug!("Sending synthetic Super key-down to destination (keyboard-initiated)");
                                if let Err(e) = peer.send(&Message::InputEvent(payload)).await {
                                    tracing::error!("Failed to send synthetic Super: {}", e);
                                }
                            }
                        } else {
                            tracing::debug!("Skipping synthetic Super key-down (CLI-initiated switch)");
                            // Add delay for CLI-initiated switches to let the terminal settle
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }

                        input_grabber.start();
                    }
                    transfer::TransferEvent::StopCapture => {
                        info!("Stopping input capture");
                        let was_capturing_direction = capture_direction;
                        capture_direction = None;

                        // Release the evdev grab and enter recovery mode for the stale direction
                        // The stale key is the arrow key used to initiate the original outgoing transfer
                        input_grabber.stop(was_capturing_direction);

                        // Drain any remaining events
                        while input_grabber.try_recv().is_some() {}

                        // CRITICAL FIX: After releasing the evdev grab, libinput has stale state.
                        // The arrow key that initiated the original transfer (before we went remote)
                        // is still seen as "pressed" by libinput because it never saw the release.
                        //
                        // We use uinput to create a virtual keyboard and send synthetic key-up
                        // events for ALL arrow keys. This gives libinput fresh key-up events,
                        // which should clear the stale state.
                        if let Some(dir) = was_capturing_direction {
                            // The stale key is the one used to initiate the OUTGOING transfer
                            let stale_keycode: u16 = match dir {
                                Direction::Left => 105,  // KEY_LEFT
                                Direction::Right => 106, // KEY_RIGHT
                                Direction::Up => 103,    // KEY_UP
                                Direction::Down => 108,  // KEY_DOWN
                            };

                            tracing::info!("Sending synthetic key-ups via uinput to clear stale libinput state");

                            // Send key-ups for all arrow keys to be safe
                            let all_arrows: [u16; 4] = [103, 105, 106, 108];
                            if let Err(e) = input::send_synthetic_key_ups(&all_arrows) {
                                tracing::warn!("Failed to send synthetic key-ups: {}", e);
                            }

                            // Also inject via virtual keyboard for Wayland-level cleanup
                            if input_emulator.is_none() {
                                if let Ok(emu) = input::InputEmulator::new() {
                                    input_emulator = Some(emu);
                                }
                            }
                            if let Some(ref mut emu) = input_emulator {
                                emu.keyboard.key(stale_keycode as u32, hyprkvm_common::KeyState::Released);
                                emu.keyboard.reset_all_keys();
                            }
                        }
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
                        // Reset ALL pressed keys so next session starts clean
                        // This prevents Hyprland from seeing stale key state
                        // (e.g., arrow key that triggered return was never released)
                        if let Some(ref mut emu) = input_emulator {
                            emu.keyboard.reset_all_keys();
                        }
                    }
                    transfer::TransferEvent::StartRelay { from, to } => {
                        info!("Starting input relay: {:?} -> {:?}", from, to);
                        relay_mode = Some((from, to));
                        // No local device grabbing needed - we're forwarding received input
                    }
                    transfer::TransferEvent::StopRelay => {
                        info!("Stopping input relay");
                        relay_mode = None;
                    }
                    transfer::TransferEvent::SyncClipboardOutgoing { direction } => {
                        // Sync clipboard to the peer in the given direction
                        // Check if clipboard sync is enabled and appropriate for this event
                        if config.clipboard.enabled {
                            let cm = clipboard_manager.clone();
                            let peers_clone = peers.clone();
                            tokio::spawn(async move {
                                match cm.create_offer().await {
                                    Ok(Some(offer)) => {
                                        let mut peers_guard = peers_clone.write().await;
                                        if let Some(peer) = peers_guard.get_mut(&direction) {
                                            info!("Syncing clipboard to {:?}", direction);
                                            if let Err(e) = peer.send(&Message::ClipboardOffer(offer)).await {
                                                tracing::warn!("Failed to send clipboard offer: {}", e);
                                            }
                                        }
                                    }
                                    Ok(None) => {
                                        tracing::debug!("No clipboard content to sync");
                                    }
                                    Err(e) => {
                                        tracing::warn!("Failed to read clipboard for sync: {}", e);
                                    }
                                }
                            });
                        }
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

            // Handle IPC requests from CLI
            Some((request, response_tx)) = ipc_rx.recv() => {
                use hyprkvm_common::protocol::{IpcRequest, IpcResponse};

                let response = match request {
                    IpcRequest::Move { direction } => {
                        // Log current state for debugging
                        let current_state = transfer_manager.state().await;
                        info!("IPC Move {:?}: state={:?}", direction, current_state);

                        // For keyboard navigation, check if we're at the absolute edge:
                        // 1. On edge monitor (no monitor in that direction)
                        // 2. On edge window of that monitor (no window further in that direction)

                        let at_edge = 'edge_check: {
                            // Get monitors and find focused one
                            let monitors = match hypr_client.monitors().await {
                                Ok(m) => m,
                                Err(e) => {
                                    info!("  edge_check: monitors query failed: {}", e);
                                    break 'edge_check false;
                                }
                            };
                            let focused_monitor = match monitors.iter().find(|m| m.focused) {
                                Some(m) => m,
                                None => {
                                    info!("  edge_check: no focused monitor found");
                                    break 'edge_check false;
                                }
                            };

                            // Check if there's another monitor in the requested direction
                            // Use logical dimensions (physical / scale) since positions are logical
                            let has_monitor_in_direction = monitors.iter().any(|m| {
                                if m.id == focused_monitor.id { return false; }
                                let m_logical_w = (m.width as f32 / m.scale).round() as i32;
                                let m_logical_h = (m.height as f32 / m.scale).round() as i32;
                                let focused_logical_w = (focused_monitor.width as f32 / focused_monitor.scale).round() as i32;
                                let focused_logical_h = (focused_monitor.height as f32 / focused_monitor.scale).round() as i32;
                                match direction {
                                    Direction::Left => m.x + m_logical_w <= focused_monitor.x,
                                    Direction::Right => m.x >= focused_monitor.x + focused_logical_w,
                                    Direction::Up => m.y + m_logical_h <= focused_monitor.y,
                                    Direction::Down => m.y >= focused_monitor.y + focused_logical_h,
                                }
                            });

                            if has_monitor_in_direction {
                                // There's a monitor in that direction, not at edge
                                info!("  edge_check: has monitor in direction {:?}", direction);
                                break 'edge_check false;
                            }

                            // We're on the edge monitor. Now check if we're on the edge window.
                            // Get active window position
                            let active_window: serde_json::Value = match hypr_client.query("activewindow").await {
                                Ok(w) => w,
                                Err(e) => {
                                    info!("  edge_check: activewindow query failed: {}", e);
                                    break 'edge_check false;
                                }
                            };

                            let win_x = active_window.get("at").and_then(|a| a.get(0)).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                            let win_y = active_window.get("at").and_then(|a| a.get(1)).and_then(|y| y.as_i64()).unwrap_or(0) as i32;
                            let win_w = active_window.get("size").and_then(|s| s.get(0)).and_then(|w| w.as_i64()).unwrap_or(100) as i32;
                            let win_h = active_window.get("size").and_then(|s| s.get(1)).and_then(|h| h.as_i64()).unwrap_or(100) as i32;

                            // Get all clients (windows)
                            let clients: Vec<serde_json::Value> = match hypr_client.query("clients").await {
                                Ok(c) => c,
                                Err(e) => {
                                    info!("  edge_check: clients query failed: {}", e);
                                    break 'edge_check false;
                                }
                            };

                            // Calculate monitor bounds in logical coordinates
                            let mon_logical_h = (focused_monitor.height as f32 / focused_monitor.scale).round() as i32;

                            info!("  edge_check: active window at ({},{}) size {}x{}, {} clients on monitor, mon_y={}, mon_h={}",
                                  win_x, win_y, win_w, win_h,
                                  clients.iter().filter(|c| c.get("monitor").and_then(|m| m.as_i64()).unwrap_or(-1) as i32 == focused_monitor.id).count(),
                                  focused_monitor.y, mon_logical_h);

                            // For Up/Down: check if window is near monitor edge (accounts for bars/panels)
                            // For Left/Right: check if any window is further in that direction
                            let has_window_in_direction = match direction {
                                Direction::Up => {
                                    // Window is at top edge if its top is within 100px of monitor top (allows for bars)
                                    let near_top = win_y <= focused_monitor.y + 100;
                                    !near_top // has_window_in_direction = !near_top, so at_edge = near_top
                                }
                                Direction::Down => {
                                    // Window is at bottom edge if its bottom is within 100px of monitor bottom
                                    let win_bottom = win_y + win_h;
                                    let mon_bottom = focused_monitor.y + mon_logical_h;
                                    let near_bottom = win_bottom >= mon_bottom - 100;
                                    !near_bottom // has_window_in_direction = !near_bottom, so at_edge = near_bottom
                                }
                                Direction::Left | Direction::Right => {
                                    // For Left/Right, use window-based detection
                                    clients.iter().any(|client| {
                                        let mon = client.get("monitor").and_then(|m| m.as_i64()).unwrap_or(-1) as i32;
                                        if mon != focused_monitor.id { return false; }

                                        let cx = client.get("at").and_then(|a| a.get(0)).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                                        let cw = client.get("size").and_then(|s| s.get(0)).and_then(|w| w.as_i64()).unwrap_or(0) as i32;

                                        match direction {
                                            Direction::Left => cx + cw < win_x + 10,
                                            Direction::Right => cx > win_x + win_w - 10,
                                            _ => false,
                                        }
                                    })
                                }
                            };

                            info!("  edge_check: has_window_in_direction={} -> at_edge={}", has_window_in_direction, !has_window_in_direction);
                            !has_window_in_direction
                        };

                        // Check if we have a peer in this direction
                        let has_peer = {
                            let peers = peers.read().await;
                            peers.contains_key(&direction)
                        };

                        // Get neighbor name if configured
                        let neighbor_name = config.machines.neighbors
                            .iter()
                            .find(|n| n.direction == direction)
                            .map(|n| n.name.clone());

                        info!("IPC Move {:?}: at_edge={}, has_peer={}, neighbor={:?}", direction, at_edge, has_peer, neighbor_name);

                        // At edge with peer: either return control or initiate transfer
                        if at_edge && has_peer && neighbor_name.is_some() {
                            // Check if we're in ReceivedControl state from this direction
                            if let transfer::TransferState::ReceivedControl { from, .. } = current_state {
                                if from == direction {
                                    // Return control to source machine
                                    tracing::info!("Keyboard return: at edge, returning control to {:?}", direction);
                                    if let Err(e) = transfer_manager.return_control().await {
                                        tracing::warn!("Failed to return control: {}", e);
                                        IpcResponse::Error { message: format!("Return failed: {}", e) }
                                    } else {
                                        // Set cooldown to prevent immediate re-transfer (bounce-back)
                                        last_control_return = Some(std::time::Instant::now());
                                        IpcResponse::Transferred { to_machine: neighbor_name.unwrap() }
                                    }
                                } else {
                                    // At edge with peer but received control from different direction
                                    if barrier_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                                        IpcResponse::Error { message: "Barrier enabled".to_string() }
                                    } else {
                                        // Initiate new transfer
                                        let cursor_pos = hypr_client.cursor_pos().await
                                            .map(|c| (c.x, c.y))
                                            .unwrap_or((0, 0));

                                        if let Err(e) = transfer_manager.initiate_transfer(
                                            direction,
                                            cursor_pos,
                                            screen_min_x,
                                            screen_min_y,
                                            screen_max_x,
                                            screen_max_y,
                                            true, // keyboard-initiated (IPC Move from keybind)
                                        ).await {
                                            IpcResponse::Error { message: format!("Transfer failed: {}", e) }
                                        } else {
                                            IpcResponse::Transferred { to_machine: neighbor_name.unwrap() }
                                        }
                                    }
                                }
                            } else {
                                // Not in ReceivedControl - check cooldown first
                                let in_cooldown = if let Some(last_return) = last_control_return {
                                    last_return.elapsed().as_millis() < CONTROL_RETURN_COOLDOWN_MS as u128
                                } else {
                                    false
                                };

                                if in_cooldown {
                                    tracing::info!("IPC Move {:?}: in cooldown, doing local movefocus", direction);
                                    let hypr_dir = match direction {
                                        Direction::Left => "l",
                                        Direction::Right => "r",
                                        Direction::Up => "u",
                                        Direction::Down => "d",
                                    };
                                    match hypr_client.dispatch("movefocus", hypr_dir).await {
                                        Ok(_) => IpcResponse::Ok { message: "movefocus (cooldown)".to_string() },
                                        Err(e) => IpcResponse::Error { message: format!("movefocus failed: {}", e) },
                                    }
                                } else if barrier_enabled.load(std::sync::atomic::Ordering::SeqCst) {
                                    IpcResponse::Error { message: "Barrier enabled".to_string() }
                                } else {
                                    // Initiate new transfer
                                    let cursor_pos = hypr_client.cursor_pos().await
                                        .map(|c| (c.x, c.y))
                                        .unwrap_or((0, 0));

                                    if let Err(e) = transfer_manager.initiate_transfer(
                                        direction,
                                        cursor_pos,
                                        screen_min_x,
                                        screen_min_y,
                                        screen_max_x,
                                        screen_max_y,
                                        true, // keyboard-initiated (IPC Move from keybind)
                                    ).await {
                                        IpcResponse::Error { message: format!("Transfer failed: {}", e) }
                                    } else {
                                        IpcResponse::Transferred { to_machine: neighbor_name.unwrap() }
                                    }
                                }
                            }
                        } else {
                            // Either not at edge, or at edge but no peer - do local movefocus
                            let hypr_dir = match direction {
                                Direction::Left => "l",
                                Direction::Right => "r",
                                Direction::Up => "u",
                                Direction::Down => "d",
                            };
                            info!("IPC Move {:?}: doing local movefocus {}", direction, hypr_dir);
                            match hypr_client.dispatch("movefocus", hypr_dir).await {
                                Ok(()) => info!("  movefocus succeeded"),
                                Err(e) => tracing::error!("  movefocus failed: {}", e),
                            }
                            IpcResponse::DoLocalMove
                        }
                    }
                    IpcRequest::Status => {
                        let state = format!("{:?}", transfer_manager.state().await);
                        let connected_peers: Vec<String> = {
                            let peers = peers.read().await;
                            config.machines.neighbors
                                .iter()
                                .filter(|n| peers.contains_key(&n.direction))
                                .map(|n| n.name.clone())
                                .collect()
                        };
                        let uptime_secs = daemon_start_time.elapsed().as_secs();
                        IpcResponse::Status {
                            state,
                            connected_peers,
                            uptime_secs,
                            machine_name: config.machines.self_name.clone(),
                        }
                    }
                    IpcRequest::ListPeers => {
                        let peers_guard = peers.read().await;
                        let peer_list: Vec<hyprkvm_common::protocol::PeerInfo> = config.machines.neighbors
                            .iter()
                            .map(|n| {
                                let connected = peers_guard.contains_key(&n.direction);
                                let status = if connected {
                                    "connected".to_string()
                                } else {
                                    "disconnected".to_string()
                                };
                                hyprkvm_common::protocol::PeerInfo {
                                    name: n.name.clone(),
                                    direction: n.direction,
                                    connected,
                                    address: n.address.to_string(),
                                    status,
                                }
                            })
                            .collect();
                        IpcResponse::Peers { peers: peer_list }
                    }
                    IpcRequest::PingPeer { peer_name } => {
                        // Find the peer by name
                        let neighbor = config.machines.neighbors
                            .iter()
                            .find(|n| n.name == peer_name);

                        match neighbor {
                            Some(n) => {
                                let direction = n.direction;
                                let mut peers_guard = peers.write().await;

                                if let Some(peer_conn) = peers_guard.get_mut(&direction) {
                                    // Send Ping with current timestamp
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap()
                                        .as_millis() as u64;

                                    if let Err(e) = peer_conn.send(&Message::Ping { timestamp }).await {
                                        IpcResponse::PingResult {
                                            peer_name,
                                            latency_ms: None,
                                            error: Some(format!("Send failed: {}", e)),
                                        }
                                    } else {
                                        // Wait for Pong with timeout
                                        match tokio::time::timeout(
                                            std::time::Duration::from_secs(5),
                                            peer_conn.recv()
                                        ).await {
                                            Ok(Ok(Some(Message::Pong { timestamp: pong_ts }))) => {
                                                let now = std::time::SystemTime::now()
                                                    .duration_since(std::time::UNIX_EPOCH)
                                                    .unwrap()
                                                    .as_millis() as u64;
                                                let latency = now.saturating_sub(pong_ts);
                                                IpcResponse::PingResult {
                                                    peer_name,
                                                    latency_ms: Some(latency),
                                                    error: None,
                                                }
                                            }
                                            Ok(Ok(Some(_))) => {
                                                IpcResponse::PingResult {
                                                    peer_name,
                                                    latency_ms: None,
                                                    error: Some("Unexpected response".to_string()),
                                                }
                                            }
                                            Ok(Ok(None)) => {
                                                // Connection closed
                                                peers_guard.remove(&direction);
                                                IpcResponse::PingResult {
                                                    peer_name,
                                                    latency_ms: None,
                                                    error: Some("Connection closed".to_string()),
                                                }
                                            }
                                            Ok(Err(e)) => {
                                                IpcResponse::PingResult {
                                                    peer_name,
                                                    latency_ms: None,
                                                    error: Some(format!("Receive error: {}", e)),
                                                }
                                            }
                                            Err(_) => {
                                                IpcResponse::PingResult {
                                                    peer_name,
                                                    latency_ms: None,
                                                    error: Some("Timeout".to_string()),
                                                }
                                            }
                                        }
                                    }
                                } else {
                                    IpcResponse::PingResult {
                                        peer_name,
                                        latency_ms: None,
                                        error: Some("Peer not connected".to_string()),
                                    }
                                }
                            }
                            None => {
                                IpcResponse::Error {
                                    message: format!("Unknown peer: {}", peer_name),
                                }
                            }
                        }
                    }

                    // ================================================================
                    // CLI Expansion: Control Transfer
                    // ================================================================

                    IpcRequest::Switch { target } => {
                        use hyprkvm_common::protocol::SwitchTarget;

                        // Resolve target to a direction
                        let direction = match &target {
                            SwitchTarget::Direction(dir) => Some(*dir),
                            SwitchTarget::MachineName(name) => {
                                config.machines.neighbors
                                    .iter()
                                    .find(|n| &n.name == name)
                                    .map(|n| n.direction)
                            }
                        };

                        match direction {
                            Some(dir) => {
                                let peers_guard = peers.read().await;
                                if peers_guard.get(&dir).is_some() {
                                    drop(peers_guard);

                                    // Get cursor position (use center of total screen)
                                    let cursor_pos = hypr_client.cursor_pos().await
                                        .map(|c| (c.x, c.y))
                                        .unwrap_or(((screen_min_x + screen_max_x) / 2, (screen_min_y + screen_max_y) / 2));

                                    // Initiate transfer (CLI-initiated, not keyboard)
                                    info!("IPC Switch: calling initiate_transfer");
                                    match transfer_manager.initiate_transfer(dir, cursor_pos, screen_min_x, screen_min_y, screen_max_x, screen_max_y, false).await {
                                        Ok(()) => {
                                            let machine_name = config.machines.neighbors
                                                .iter()
                                                .find(|n| n.direction == dir)
                                                .map(|n| n.name.clone())
                                                .unwrap_or_else(|| format!("{:?}", dir));
                                            info!("IPC Switch: initiate_transfer succeeded, returning response to CLI");
                                            IpcResponse::Transferred { to_machine: machine_name }
                                        }
                                        Err(e) => IpcResponse::Error {
                                            message: format!("Transfer failed: {}", e),
                                        }
                                    }
                                } else {
                                    IpcResponse::Error {
                                        message: format!("Peer not connected in direction {:?}", dir),
                                    }
                                }
                            }
                            None => {
                                let name = match target {
                                    SwitchTarget::MachineName(n) => n,
                                    _ => "unknown".to_string(),
                                };
                                IpcResponse::Error {
                                    message: format!("Unknown machine: {}", name),
                                }
                            }
                        }
                    }

                    IpcRequest::Return => {
                        match transfer_manager.return_control().await {
                            Ok(()) => IpcResponse::Ok {
                                message: "Control returned".to_string(),
                            },
                            Err(e) => IpcResponse::Error {
                                message: format!("Return failed: {}", e),
                            }
                        }
                    }

                    // ================================================================
                    // CLI Expansion: Input Management
                    // ================================================================

                    IpcRequest::Release => {
                        // Stop input grabbing
                        input_grabber.stop(None);
                        // Abort any pending transfer
                        transfer_manager.abort().await;
                        IpcResponse::Ok {
                            message: "Input released".to_string(),
                        }
                    }

                    IpcRequest::SetBarrier { enabled } => {
                        barrier_enabled.store(enabled, std::sync::atomic::Ordering::SeqCst);
                        let status = if enabled { "enabled" } else { "disabled" };
                        IpcResponse::Ok {
                            message: format!("Barrier {}", status),
                        }
                    }

                    // ================================================================
                    // CLI Expansion: Connection Management
                    // ================================================================

                    IpcRequest::Disconnect { peer_name } => {
                        let neighbor = config.machines.neighbors
                            .iter()
                            .find(|n| n.name == peer_name);

                        match neighbor {
                            Some(n) => {
                                let direction = n.direction;
                                let mut peers_guard = peers.write().await;
                                if let Some(mut peer_conn) = peers_guard.remove(&direction) {
                                    // Send goodbye before disconnecting
                                    let _ = peer_conn.send(&Message::Goodbye).await;
                                    IpcResponse::Ok {
                                        message: format!("Disconnected from {}", peer_name),
                                    }
                                } else {
                                    IpcResponse::Error {
                                        message: format!("Peer {} not connected", peer_name),
                                    }
                                }
                            }
                            None => IpcResponse::Error {
                                message: format!("Unknown peer: {}", peer_name),
                            }
                        }
                    }

                    IpcRequest::Reconnect { peer_name } => {
                        let neighbor = config.machines.neighbors
                            .iter()
                            .find(|n| n.name == peer_name)
                            .cloned();

                        match neighbor {
                            Some(n) => {
                                let direction = n.direction;
                                let addr = n.address;
                                // Remove existing connection if any
                                {
                                    let mut peers_guard = peers.write().await;
                                    if let Some(mut peer_conn) = peers_guard.remove(&direction) {
                                        let _ = peer_conn.send(&Message::Goodbye).await;
                                    }
                                }
                                // Spawn reconnection task (same logic as initial connection)
                                let peers_clone = peers.clone();
                                let machine_name = config.machines.self_name.clone();
                                let neighbor_name = n.name.clone();

                                // Determine TLS settings for this neighbor
                                let use_tls = n.tls.unwrap_or(tls_enabled);
                                let fingerprint = n.fingerprint.clone();
                                let tofu = config.network.tls.tofu;

                                tokio::spawn(async move {
                                    // Connect with or without TLS
                                    let conn_result = if use_tls {
                                        network::connect_tls(addr, &neighbor_name, fingerprint.as_deref(), tofu).await
                                    } else {
                                        network::connect(addr).await
                                    };

                                    match conn_result {
                                        Ok(mut conn) => {
                                            // Send Hello with direction for peer sync
                                            let hello = Message::Hello(HelloPayload {
                                                protocol_version: PROTOCOL_VERSION,
                                                machine_name,
                                                capabilities: vec![],
                                                my_direction_for_you: Some(direction),
                                            });
                                            if let Err(e) = conn.send(&hello).await {
                                                tracing::error!("Reconnect: failed to send Hello: {}", e);
                                                return;
                                            }
                                            // Wait for HelloAck
                                            match conn.recv().await {
                                                Ok(Some(Message::HelloAck(ack))) if ack.accepted => {
                                                    let mut peers_guard = peers_clone.write().await;
                                                    peers_guard.insert(direction, conn);
                                                    info!("Reconnected to {}", neighbor_name);
                                                }
                                                Ok(Some(Message::HelloAck(ack))) => {
                                                    tracing::error!("Reconnect rejected: {:?}", ack.error);
                                                }
                                                _ => {
                                                    tracing::error!("Reconnect: handshake failed");
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            tracing::error!("Reconnect: connection failed: {}", e);
                                        }
                                    }
                                });
                                IpcResponse::Ok {
                                    message: format!("Reconnecting to {}", peer_name),
                                }
                            }
                            None => IpcResponse::Error {
                                message: format!("Unknown peer: {}", peer_name),
                            }
                        }
                    }

                    // ================================================================
                    // CLI Expansion: Configuration
                    // ================================================================

                    IpcRequest::GetConfig => {
                        match toml::to_string_pretty(&config) {
                            Ok(toml_str) => IpcResponse::Config { toml: toml_str },
                            Err(e) => IpcResponse::Error {
                                message: format!("Failed to serialize config: {}", e),
                            }
                        }
                    }

                    IpcRequest::Reload => {
                        // Re-read and validate config file
                        match config::Config::load(&config_path) {
                            Ok(new_config) => {
                                // Check what changed
                                let mut changes = Vec::new();
                                let mut needs_restart = false;

                                if new_config.machines.self_name != config.machines.self_name {
                                    changes.push(format!(
                                        "machine name: {} -> {} (requires restart)",
                                        config.machines.self_name, new_config.machines.self_name
                                    ));
                                    needs_restart = true;
                                }

                                if new_config.network.listen_port != config.network.listen_port {
                                    changes.push(format!(
                                        "listen port: {} -> {} (requires restart)",
                                        config.network.listen_port, new_config.network.listen_port
                                    ));
                                    needs_restart = true;
                                }

                                if new_config.machines.neighbors.len() != config.machines.neighbors.len() {
                                    changes.push(format!(
                                        "neighbors: {} -> {} (requires restart)",
                                        config.machines.neighbors.len(), new_config.machines.neighbors.len()
                                    ));
                                    needs_restart = true;
                                }

                                // Check for direction changes (requires restart for edge barriers)
                                // Also send DirectionChange messages to notify peers
                                let mut direction_changes: Vec<(String, Direction)> = Vec::new();
                                for new_neighbor in &new_config.machines.neighbors {
                                    if let Some(old_neighbor) = config.machines.neighbors
                                        .iter()
                                        .find(|n| n.name == new_neighbor.name)
                                    {
                                        if old_neighbor.direction != new_neighbor.direction {
                                            changes.push(format!(
                                                "neighbor '{}' direction: {:?} -> {:?} (requires restart)",
                                                new_neighbor.name, old_neighbor.direction, new_neighbor.direction
                                            ));
                                            needs_restart = true;
                                            // Track this change to notify the peer
                                            direction_changes.push((new_neighbor.name.clone(), new_neighbor.direction));
                                        }
                                    }
                                }

                                // Send DirectionChange messages to affected peers
                                // We need to send on the OLD direction since that's where the peer is connected
                                for (peer_name, new_direction) in &direction_changes {
                                    // Find the OLD direction for this peer from current config
                                    let old_direction = config.machines.neighbors
                                        .iter()
                                        .find(|n| &n.name == peer_name)
                                        .map(|n| n.direction);

                                    if let Some(old_dir) = old_direction {
                                        let mut peers_guard = peers.write().await;
                                        if let Some(peer) = peers_guard.get_mut(&old_dir) {
                                            info!("Sending DirectionChange to {}: you are now {:?} from me",
                                                peer_name, new_direction);
                                            let msg = Message::DirectionChange(
                                                hyprkvm_common::protocol::DirectionChangePayload {
                                                    your_direction_from_me: *new_direction,
                                                }
                                            );
                                            if let Err(e) = peer.send(&msg).await {
                                                tracing::warn!("Failed to send DirectionChange to {}: {}", peer_name, e);
                                            }
                                        } else {
                                            tracing::warn!("Peer {} not connected on {:?}", peer_name, old_dir);
                                        }
                                    }
                                }

                                // Apply the new config (only if no restart needed)
                                if !needs_restart {
                                    config = new_config;
                                }

                                if changes.is_empty() {
                                    IpcResponse::Ok {
                                        message: "Config unchanged".to_string(),
                                    }
                                } else if needs_restart {
                                    IpcResponse::Ok {
                                        message: format!(
                                            "Config saved (restart required to apply):\n  - {}",
                                            changes.join("\n  - ")
                                        ),
                                    }
                                } else {
                                    IpcResponse::Ok {
                                        message: format!(
                                            "Config reloaded:\n  - {}",
                                            changes.join("\n  - ")
                                        ),
                                    }
                                }
                            }
                            Err(e) => IpcResponse::Error {
                                message: format!("Failed to load config: {}", e),
                            }
                        }
                    }

                    // ================================================================
                    // CLI Expansion: Daemon Control
                    // ================================================================

                    IpcRequest::Shutdown => {
                        info!("Shutdown requested via IPC");
                        shutdown_requested.store(true, std::sync::atomic::Ordering::SeqCst);
                        IpcResponse::Ok {
                            message: "Shutting down...".to_string(),
                        }
                    }

                    IpcRequest::GetLogs { lines, follow: _ } => {
                        // Find log files (rolling appender creates daemon.log.YYYY-MM-DD)
                        let log_dir = dirs::data_local_dir()
                            .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
                            .join("hyprkvm");

                        // Find the most recent log file
                        let log_file = std::fs::read_dir(&log_dir)
                            .ok()
                            .and_then(|entries| {
                                entries
                                    .filter_map(|e| e.ok())
                                    .filter(|e| {
                                        e.file_name()
                                            .to_string_lossy()
                                            .starts_with("daemon.log")
                                    })
                                    .max_by_key(|e| e.metadata().ok().and_then(|m| m.modified().ok()))
                                    .map(|e| e.path())
                            });

                        match log_file {
                            Some(path) => {
                                match std::fs::read_to_string(&path) {
                                    Ok(content) => {
                                        let n = lines.unwrap_or(50) as usize;
                                        let log_lines: Vec<String> = content
                                            .lines()
                                            .rev()
                                            .take(n)
                                            .map(|s| s.to_string())
                                            .collect::<Vec<_>>()
                                            .into_iter()
                                            .rev()
                                            .collect();
                                        IpcResponse::Logs { lines: log_lines }
                                    }
                                    Err(e) => IpcResponse::Error {
                                        message: format!("Failed to read log file: {}", e),
                                    }
                                }
                            }
                            None => {
                                IpcResponse::Logs {
                                    lines: vec!["No log files found.".to_string()],
                                }
                            }
                        }
                    }
                };

                let _ = response_tx.send(response);
            }

            // Handle restart signal from direction change
            Some(reason) = restart_rx.recv() => {
                info!("Restart required: {}", reason);
                info!("Exiting to allow restart with updated config...");
                accept_handle.abort();
                // Exit with code 75 (EX_TEMPFAIL) to signal that we need to restart
                // This allows systemd or the GUI to restart us
                std::process::exit(75);
            }

            // Shutdown (Ctrl+C or IPC request)
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down (Ctrl+C)...");
                accept_handle.abort();
                break;
            }

            // Check for IPC shutdown request
            _ = async {
                while !shutdown_requested.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } => {
                info!("Shutting down (IPC request)...");
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
    use hyprkvm_common::protocol::{IpcRequest, IpcResponse};

    let dir: Direction = direction.parse()?;

    // Try to connect to daemon
    match ipc::IpcClient::connect().await {
        Ok(mut client) => {
            // Ask daemon to handle the move (it does movefocus internally)
            let request = IpcRequest::Move { direction: dir };
            match client.request(&request).await {
                Ok(IpcResponse::Transferred { to_machine }) => {
                    tracing::info!("Transferred control to {}", to_machine);
                }
                Ok(IpcResponse::DoLocalMove) => {
                    // Daemon handled it
                }
                Ok(IpcResponse::Error { message }) => {
                    tracing::warn!("Daemon error: {}", message);
                }
                Ok(_) => {
                    tracing::warn!("Unexpected response from daemon");
                }
                Err(e) => {
                    tracing::debug!("IPC request failed: {}, falling back to local", e);
                    do_local_move(dir).await?;
                }
            }
        }
        Err(e) => {
            tracing::debug!("Daemon not running ({}), doing local move", e);
            do_local_move(dir).await?;
        }
    }

    Ok(())
}

async fn do_local_move(dir: hyprkvm_common::Direction) -> anyhow::Result<()> {
    use hyprkvm_common::Direction;

    let hypr_dir = match dir {
        Direction::Left => "l",
        Direction::Right => "r",
        Direction::Up => "u",
        Direction::Down => "d",
    };

    let output = tokio::process::Command::new("hyprctl")
        .args(["dispatch", "movefocus", hypr_dir])
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
