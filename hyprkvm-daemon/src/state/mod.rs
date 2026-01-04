//! State management module
//!
//! Maintains the unified state of the daemon.

pub mod manager;

pub use manager::{ControlState, EdgeTrigger, NeighborInfo, StateError, StateManager};
