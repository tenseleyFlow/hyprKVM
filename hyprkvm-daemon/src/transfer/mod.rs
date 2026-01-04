//! Control transfer module
//!
//! Manages the transfer of keyboard/mouse control between machines.

pub mod manager;

pub use manager::{TransferEvent, TransferManager, TransferState};
