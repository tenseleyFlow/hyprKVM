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
mod ipc;
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

    FmtSubscriber::builder()
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

    // Track which direction we're capturing for
    let mut capture_direction: Option<Direction> = None;
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

    // Connection storage: direction -> peer connection
    let peers: Arc<RwLock<HashMap<Direction, network::FramedConnection>>> =
        Arc::new(RwLock::new(HashMap::new()));

    // Start network server
    let listen_addr: SocketAddr = format!("0.0.0.0:{}", config.network.listen_port).parse()?;
    let server = network::Server::bind(listen_addr).await?;
    info!("Listening for connections on {}", server.local_addr());

    // Spawn task to accept incoming connections
    let machine_name = config.machines.self_name.clone();
    let neighbors_for_accept = config.machines.neighbors.clone();
    let peers_for_accept = peers.clone();
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

                            // Determine direction based on peer's machine name
                            let direction = neighbors_for_accept
                                .iter()
                                .find(|n| n.name == hello.machine_name)
                                .map(|n| n.direction);

                            if let Some(dir) = direction {
                                let mut peers = peers_for_accept.write().await;
                                if peers.contains_key(&dir) {
                                    info!("Already have connection for {:?}, dropping incoming from {}", dir, hello.machine_name);
                                    // Drop the incoming connection, keep the existing one
                                } else {
                                    info!("Storing incoming connection from {} as {:?}", hello.machine_name, dir);
                                    peers.insert(dir, conn);
                                }
                            } else {
                                tracing::warn!(
                                    "Unknown peer '{}' connected - not in neighbors list",
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

                tracing::debug!("Connecting to {} at {}...", direction, addr);
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
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            continue;
                        }

                        // Wait for HelloAck
                        match conn.recv().await {
                            Ok(Some(Message::HelloAck(ack))) => {
                                if ack.accepted {
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
                    let ipc_tx = ipc_tx.clone();
                    tokio::spawn(async move {
                        match conn.recv().await {
                            Ok(Some(request)) => {
                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                if ipc_tx.send((request, resp_tx)).await.is_ok() {
                                    if let Ok(response) = resp_rx.await {
                                        let _ = conn.send(&response).await;
                                    }
                                }
                            }
                            Ok(None) => {}
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
                            input::GrabEvent::KeyUp { .. } => {
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
                        input_grabber.stop();

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
                }

                // Handle edge events from layer-shell barriers (for inter-monitor edges)
                while let Some(edge_event) = edge_capture.try_recv() {
                    let direction = edge_event.direction;

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
                                    edge_event.position.0,
                                    edge_event.position.1
                                );
                                if let Err(e) = transfer_manager.return_control().await {
                                    tracing::warn!("Failed to return control: {}", e);
                                }
                                continue;
                            }
                        }

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

                // Cursor-based edge detection (for absolute screen edges)
                // This catches the case where cursor is at the edge and can't go further
                if capture_direction.is_none() {
                    if let Ok(cursor) = hypr_client.cursor_pos().await {
                        let (cx, cy) = (cursor.x, cursor.y);

                        // Determine if cursor is at a screen edge
                        let at_edge: Option<Direction> = if cx <= EDGE_THRESHOLD {
                            Some(Direction::Left)
                        } else if cx >= screen_width as i32 - EDGE_THRESHOLD {
                            Some(Direction::Right)
                        } else if cy <= EDGE_THRESHOLD {
                            Some(Direction::Up)
                        } else if cy >= screen_height as i32 - EDGE_THRESHOLD {
                            Some(Direction::Down)
                        } else {
                            None
                        };

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
                                                    if let transfer::TransferState::ReceivedControl { from, .. } = current_state {
                                                        if from == edge_dir {
                                                            info!(
                                                                "CURSOR EDGE: {:?} at ({}, {}) - returning control",
                                                                edge_dir, cx, cy
                                                            );
                                                            if let Err(e) = transfer_manager.return_control().await {
                                                                tracing::warn!("Failed to return control: {}", e);
                                                            }
                                                            edge_dwell_start = None;
                                                            continue;
                                                        }
                                                    }

                                                    info!(
                                                        "CURSOR EDGE: {:?} at ({}, {}) - initiating transfer",
                                                        edge_dir, cx, cy
                                                    );

                                                    if let Err(e) = transfer_manager.initiate_transfer(
                                                        edge_dir,
                                                        (cx, cy),
                                                        screen_height,
                                                        screen_width,
                                                    ).await {
                                                        tracing::warn!("Failed to initiate transfer: {}", e);
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
                                    }
                                    Message::LeaveAck => {
                                        info!("Received LeaveAck");
                                        // Transfer complete
                                    }
                                    Message::InputEvent(input_payload) => {
                                        tracing::trace!("Received input event: {:?}", input_payload);
                                        // Inject input via emulation module
                                        if let Some(ref mut emu) = input_emulator {
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

            // Handle IPC requests from CLI
            Some((request, response_tx)) = ipc_rx.recv() => {
                use hyprkvm_common::protocol::{IpcRequest, IpcResponse};

                let response = match request {
                    IpcRequest::Move { direction } => {
                        // For keyboard navigation, check if we're at the absolute edge:
                        // 1. On edge monitor (no monitor in that direction)
                        // 2. On edge window of that monitor (no window further in that direction)

                        let at_edge = 'edge_check: {
                            // Get monitors and find focused one
                            let monitors = match hypr_client.monitors().await {
                                Ok(m) => m,
                                Err(_) => break 'edge_check false,
                            };
                            let focused_monitor = match monitors.iter().find(|m| m.focused) {
                                Some(m) => m,
                                None => break 'edge_check false,
                            };

                            // Check if there's another monitor in the requested direction
                            let has_monitor_in_direction = monitors.iter().any(|m| {
                                if m.id == focused_monitor.id { return false; }
                                match direction {
                                    Direction::Left => m.x + m.width as i32 <= focused_monitor.x,
                                    Direction::Right => m.x >= focused_monitor.x + focused_monitor.width as i32,
                                    Direction::Up => m.y + m.height as i32 <= focused_monitor.y,
                                    Direction::Down => m.y >= focused_monitor.y + focused_monitor.height as i32,
                                }
                            });

                            if has_monitor_in_direction {
                                // There's a monitor in that direction, not at edge
                                break 'edge_check false;
                            }

                            // We're on the edge monitor. Now check if we're on the edge window.
                            // Get active window position
                            let active_window: serde_json::Value = match hypr_client.query("activewindow").await {
                                Ok(w) => w,
                                Err(_) => break 'edge_check false,
                            };

                            let win_x = active_window.get("at").and_then(|a| a.get(0)).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                            let win_y = active_window.get("at").and_then(|a| a.get(1)).and_then(|y| y.as_i64()).unwrap_or(0) as i32;
                            let win_w = active_window.get("size").and_then(|s| s.get(0)).and_then(|w| w.as_i64()).unwrap_or(100) as i32;
                            let win_h = active_window.get("size").and_then(|s| s.get(1)).and_then(|h| h.as_i64()).unwrap_or(100) as i32;

                            // Get all clients (windows)
                            let clients: Vec<serde_json::Value> = match hypr_client.query("clients").await {
                                Ok(c) => c,
                                Err(_) => break 'edge_check false,
                            };

                            // Check if any window is further in the requested direction on same monitor
                            let has_window_in_direction = clients.iter().any(|client| {
                                let mon = client.get("monitor").and_then(|m| m.as_i64()).unwrap_or(-1) as i32;
                                if mon != focused_monitor.id { return false; }

                                let cx = client.get("at").and_then(|a| a.get(0)).and_then(|x| x.as_i64()).unwrap_or(0) as i32;
                                let cy = client.get("at").and_then(|a| a.get(1)).and_then(|y| y.as_i64()).unwrap_or(0) as i32;
                                let cw = client.get("size").and_then(|s| s.get(0)).and_then(|w| w.as_i64()).unwrap_or(0) as i32;
                                let ch = client.get("size").and_then(|s| s.get(1)).and_then(|h| h.as_i64()).unwrap_or(0) as i32;

                                match direction {
                                    Direction::Left => cx + cw < win_x + 10, // Window is to the left
                                    Direction::Right => cx > win_x + win_w - 10, // Window is to the right
                                    Direction::Up => cy + ch < win_y + 10,
                                    Direction::Down => cy > win_y + win_h - 10,
                                }
                            });

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

                        // At edge with peer: either return control or initiate transfer
                        if at_edge && has_peer && neighbor_name.is_some() {
                            // Check if we're in ReceivedControl state from this direction
                            let current_state = transfer_manager.state().await;
                            if let transfer::TransferState::ReceivedControl { from, .. } = current_state {
                                if from == direction {
                                    // Return control to source machine
                                    tracing::info!("Keyboard return: at edge, returning control to {:?}", direction);
                                    if let Err(e) = transfer_manager.return_control().await {
                                        tracing::warn!("Failed to return control: {}", e);
                                        IpcResponse::Error { message: format!("Return failed: {}", e) }
                                    } else {
                                        IpcResponse::Transferred { to_machine: neighbor_name.unwrap() }
                                    }
                                } else {
                                    // At edge with peer but received control from different direction
                                    // Initiate new transfer
                                    let cursor_pos = hypr_client.cursor_pos().await
                                        .map(|c| (c.x, c.y))
                                        .unwrap_or((0, 0));

                                    if let Err(e) = transfer_manager.initiate_transfer(
                                        direction,
                                        cursor_pos,
                                        screen_height,
                                        screen_width,
                                    ).await {
                                        IpcResponse::Error { message: format!("Transfer failed: {}", e) }
                                    } else {
                                        IpcResponse::Transferred { to_machine: neighbor_name.unwrap() }
                                    }
                                }
                            } else {
                                // Not in ReceivedControl - initiate new transfer
                                let cursor_pos = hypr_client.cursor_pos().await
                                    .map(|c| (c.x, c.y))
                                    .unwrap_or((0, 0));

                                if let Err(e) = transfer_manager.initiate_transfer(
                                    direction,
                                    cursor_pos,
                                    screen_height,
                                    screen_width,
                                ).await {
                                    IpcResponse::Error { message: format!("Transfer failed: {}", e) }
                                } else {
                                    IpcResponse::Transferred { to_machine: neighbor_name.unwrap() }
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
                            let _ = hypr_client.dispatch("movefocus", hypr_dir).await;
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
                        IpcResponse::Status { state, connected_peers }
                    }
                    IpcRequest::ListPeers => {
                        let peers_guard = peers.read().await;
                        let peer_list: Vec<hyprkvm_common::protocol::PeerInfo> = config.machines.neighbors
                            .iter()
                            .map(|n| hyprkvm_common::protocol::PeerInfo {
                                name: n.name.clone(),
                                direction: n.direction,
                                connected: peers_guard.contains_key(&n.direction),
                            })
                            .collect();
                        IpcResponse::Peers { peers: peer_list }
                    }
                };

                let _ = response_tx.send(response);
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
                    // Daemon already did movefocus, nothing more to do
                    tracing::debug!("Local move handled by daemon");
                }
                Ok(IpcResponse::Error { message }) => {
                    tracing::warn!("Daemon error: {}", message);
                }
                Ok(_) => {
                    tracing::warn!("Unexpected response from daemon");
                }
                Err(e) => {
                    tracing::debug!("IPC request failed: {}", e);
                    // Fall through to local move
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
