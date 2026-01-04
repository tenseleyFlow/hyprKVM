//! Input handling module
//!
//! Handles input capture (when sending to remote) and injection (when receiving).

pub mod capture;
pub mod emulation;
pub mod grabber;

pub use capture::{EdgeCapture, EdgeCaptureConfig, EdgeCaptureError, EdgeEvent};
pub use emulation::{InputEmulator, VirtualKeyboard, VirtualPointer, EmulationError, button_codes};
pub use grabber::{InputGrabber, InputGrabberConfig, GrabEvent, GrabberError};
