//! Hyprland integration module
//!
//! Handles communication with Hyprland via IPC sockets.

pub mod ipc;
pub mod events;
pub mod layout;
pub mod edge;

pub use ipc::HyprlandClient;
pub use events::HyprlandEventStream;
