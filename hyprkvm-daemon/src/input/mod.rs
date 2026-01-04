//! Input handling module
//!
//! Handles input capture (when sending to remote) and injection (when receiving).

pub mod capture;
pub mod emulation;

// TODO: Sprint 3 - Implement input handling
// pub mod router;

pub use capture::{EdgeCapture, EdgeCaptureConfig, EdgeCaptureError, EdgeEvent};
pub use emulation::{InputEmulator, VirtualKeyboard, VirtualPointer, EmulationError, button_codes};
