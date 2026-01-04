//! Input handling module
//!
//! Handles input capture (when sending to remote) and injection (when receiving).

#![allow(dead_code)]

pub mod capture;
pub mod emulation;
pub mod evdev_grab;
pub mod grabber;

pub use capture::{EdgeCapture, EdgeCaptureConfig, EdgeEvent};
pub use emulation::InputEmulator;
pub use evdev_grab::EvdevGrabber;
pub use grabber::{GrabEvent, InputGrabber, InputGrabberConfig};
