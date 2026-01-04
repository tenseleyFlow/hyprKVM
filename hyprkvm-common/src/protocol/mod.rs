//! Network protocol definitions for HyprKVM
//!
//! All messages exchanged between HyprKVM daemons are defined here.

use serde::{Deserialize, Serialize};

use crate::{Direction, ModifierState};

/// Protocol version - increment on breaking changes
pub const PROTOCOL_VERSION: u32 = 1;

/// All possible network messages
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    // Connection lifecycle
    Hello(HelloPayload),
    HelloAck(HelloAckPayload),
    Goodbye,

    // Topology
    TopologyUpdate(TopologyPayload),
    TopologyAck,

    // Control transfer
    Enter(EnterPayload),
    EnterAck(EnterAckPayload),
    Leave(LeavePayload),
    LeaveAck,

    // Input events
    InputEvent(InputEventPayload),
    InputBatch(Vec<InputEventPayload>),

    // Clipboard
    ClipboardOffer(ClipboardOfferPayload),
    ClipboardRequest(ClipboardRequestPayload),
    ClipboardData(ClipboardDataPayload),

    // Health
    Ping { timestamp: u64 },
    Pong { timestamp: u64 },
}

// ============================================================================
// Connection Messages
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloPayload {
    /// Protocol version
    pub protocol_version: u32,
    /// Machine name
    pub machine_name: String,
    /// Supported capabilities
    pub capabilities: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloAckPayload {
    /// Whether handshake was accepted
    pub accepted: bool,
    /// Protocol version (for negotiation)
    pub protocol_version: u32,
    /// Machine name
    pub machine_name: String,
    /// Error message if not accepted
    pub error: Option<String>,
}

// ============================================================================
// Topology Messages
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyPayload {
    /// This machine's name
    pub machine_name: String,
    /// Known neighbors
    pub neighbors: Vec<NeighborInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborInfo {
    pub name: String,
    pub direction: Direction,
}

// ============================================================================
// Control Transfer Messages
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnterPayload {
    /// Direction we're entering from (from sender's perspective)
    pub from_direction: Direction,
    /// Cursor entry position
    pub cursor_pos: CursorEntryPos,
    /// Current modifier state
    pub modifiers: ModifierState,
    /// Unique transfer ID
    pub transfer_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CursorEntryPos {
    /// Position along the edge (0.0 = top/left, 1.0 = bottom/right)
    EdgeRelative(f64),
    /// Absolute coordinates
    Absolute { x: i32, y: i32 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnterAckPayload {
    /// Whether entry was accepted
    pub success: bool,
    /// Transfer ID for correlation
    pub transfer_id: u64,
    /// Actual cursor position after entry
    pub actual_cursor_pos: Option<(i32, i32)>,
    /// Error message if not success
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeavePayload {
    /// Direction we're leaving towards
    pub to_direction: Direction,
    /// Cursor position for target machine
    pub cursor_pos: CursorEntryPos,
    /// Current modifier state
    pub modifiers: ModifierState,
    /// Transfer ID for correlation
    pub transfer_id: u64,
}

// ============================================================================
// Input Messages
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputEventPayload {
    /// Sequence number for ordering
    pub sequence: u64,
    /// Timestamp in microseconds
    pub timestamp_us: u64,
    /// The actual event
    pub event: InputEventType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEventType {
    KeyDown { keycode: u32 },
    KeyUp { keycode: u32 },
    ModifierState {
        shift: bool,
        ctrl: bool,
        alt: bool,
        super_key: bool,
    },
    PointerMotion { dx: f64, dy: f64 },
    PointerButton { button: u32, pressed: bool },
    Scroll { horizontal: f64, vertical: f64 },
}

// ============================================================================
// Clipboard Messages
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardOfferPayload {
    /// Unique ID for this clipboard state
    pub clipboard_id: u64,
    /// Available MIME types
    pub mime_types: Vec<String>,
    /// Size hint in bytes (if known)
    pub size_hint: Option<u64>,
    /// Content hash for deduplication
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardRequestPayload {
    /// Which clipboard to fetch
    pub clipboard_id: u64,
    /// Preferred MIME type
    pub mime_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardDataPayload {
    /// Clipboard ID
    pub clipboard_id: u64,
    /// MIME type of data
    pub mime_type: String,
    /// Base64-encoded data
    pub data: String,
    /// Chunk index (if chunked)
    pub chunk_index: Option<u32>,
    /// Total chunks (if chunked)
    pub total_chunks: Option<u32>,
}

// ============================================================================
// IPC Messages (CLI <-> Daemon)
// ============================================================================

/// IPC request from CLI to daemon
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Request to move focus in a direction (keyboard navigation)
    Move { direction: Direction },
    /// Get daemon status
    Status,
    /// List connected peers
    ListPeers,
    /// Ping a specific peer by name
    PingPeer { peer_name: String },
}

/// IPC response from daemon to CLI
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    /// Move was handled - transferred to another machine
    Transferred { to_machine: String },
    /// Move should be handled locally (no edge crossing)
    DoLocalMove,
    /// Status response
    Status {
        state: String,
        connected_peers: Vec<String>,
        /// Daemon uptime in seconds
        uptime_secs: u64,
        /// This machine's name
        machine_name: String,
    },
    /// Peer list
    Peers { peers: Vec<PeerInfo> },
    /// Ping result
    PingResult {
        peer_name: String,
        /// Round-trip time in milliseconds (None if peer not connected)
        latency_ms: Option<u64>,
        /// Error message if ping failed
        error: Option<String>,
    },
    /// Error occurred
    Error { message: String },
}

/// Info about a connected peer
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub name: String,
    pub direction: Direction,
    pub connected: bool,
    /// Configured address for this peer
    pub address: String,
    /// Connection status: "connected", "disconnected", "connecting"
    pub status: String,
}
