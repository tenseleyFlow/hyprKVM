//! Evdev-based input grabbing
//!
//! Grabs input devices at the kernel level using EVIOCGRAB.
//! This is the most reliable way to capture input on Linux.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use evdev::{Device, InputEventKind};

use super::grabber::GrabEvent;

/// Evdev-based input grabber
pub struct EvdevGrabber {
    active: Arc<AtomicBool>,
    event_rx: mpsc::Receiver<GrabEvent>,
    _thread: thread::JoinHandle<()>,
}

impl EvdevGrabber {
    /// Create a new evdev grabber
    pub fn new() -> Result<Self, EvdevGrabError> {
        let active = Arc::new(AtomicBool::new(false));
        let active_clone = active.clone();

        let (event_tx, event_rx) = mpsc::channel();

        let thread = thread::Builder::new()
            .name("evdev-grabber".to_string())
            .spawn(move || {
                if let Err(e) = run_evdev_grabber(active_clone, event_tx) {
                    tracing::error!("Evdev grabber error: {}", e);
                }
            })
            .map_err(|e| EvdevGrabError::Thread(e.to_string()))?;

        Ok(Self {
            active,
            event_rx,
            _thread: thread,
        })
    }

    /// Start grabbing input
    pub fn start(&self) {
        tracing::info!("Starting evdev input grab");
        self.active.store(true, Ordering::SeqCst);
    }

    /// Stop grabbing input
    pub fn stop(&self) {
        tracing::info!("Stopping evdev input grab");
        self.active.store(false, Ordering::SeqCst);
    }

    /// Check if currently grabbing
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Try to receive a grab event (non-blocking)
    pub fn try_recv(&self) -> Option<GrabEvent> {
        self.event_rx.try_recv().ok()
    }
}

fn find_input_devices() -> Vec<PathBuf> {
    let mut devices = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();

    // Method 1: Look in /dev/input/by-id for keyboard and mouse devices
    if let Ok(entries) = fs::read_dir("/dev/input/by-id") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            // Look for keyboard and mouse event devices (not hidraw)
            if name.contains("event") &&
               (name.contains("kbd") || name.contains("keyboard") ||
                name.contains("mouse") || name.contains("pointer")) {
                if let Ok(path) = entry.path().canonicalize() {
                    if seen_paths.insert(path.clone()) {
                        devices.push(path);
                    }
                }
            }
        }
    }

    // Method 2: Scan all /dev/input/event* and check capabilities
    if let Ok(entries) = fs::read_dir("/dev/input") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("event") {
                let path = entry.path();
                if seen_paths.contains(&path) {
                    continue;
                }

                // Try to open and check if it's a keyboard or mouse
                if let Ok(dev) = Device::open(&path) {
                    let has_keys = dev.supported_keys().map(|k| k.iter().count() > 0).unwrap_or(false);
                    let has_rel = dev.supported_relative_axes().map(|r| r.iter().count() > 0).unwrap_or(false);

                    // Include if it has keys (keyboard) or relative axes (mouse)
                    if has_keys || has_rel {
                        let dev_name = dev.name().unwrap_or("unknown");
                        tracing::debug!("Found input device: {} at {} (keys={}, rel={})",
                            dev_name, path.display(), has_keys, has_rel);
                        devices.push(path);
                    }
                }
            }
        }
    }

    devices
}

fn run_evdev_grabber(
    active: Arc<AtomicBool>,
    event_tx: mpsc::Sender<GrabEvent>,
) -> Result<(), EvdevGrabError> {
    let device_paths = find_input_devices();

    if device_paths.is_empty() {
        return Err(EvdevGrabError::NoDevices);
    }

    tracing::info!("Found {} input device paths", device_paths.len());
    for path in &device_paths {
        tracing::debug!("  {}", path.display());
    }

    // We'll open and grab devices fresh each time we activate
    let mut devices: HashMap<PathBuf, Device> = HashMap::new();
    let mut grabbed = false;
    let mut last_active = false;

    loop {
        let is_active = active.load(Ordering::SeqCst);

        // Handle grab/ungrab transitions
        if is_active != last_active {
            last_active = is_active;

            if is_active {
                // Open and grab all devices fresh
                devices.clear();
                tracing::info!("Opening and grabbing input devices...");

                for path in &device_paths {
                    match Device::open(path) {
                        Ok(mut dev) => {
                            let name = dev.name().unwrap_or("unknown").to_string();

                            // Try to grab immediately after opening
                            match dev.grab() {
                                Ok(()) => {
                                    tracing::info!("Grabbed: {} ({})", name, path.display());
                                    devices.insert(path.clone(), dev);
                                }
                                Err(e) => {
                                    tracing::warn!("Cannot grab {} ({}): {}", name, path.display(), e);
                                    // Don't add to devices if we can't grab
                                }
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Failed to open {}: {}", path.display(), e);
                        }
                    }
                }

                if devices.is_empty() {
                    tracing::error!("Failed to grab any input devices!");
                } else {
                    tracing::info!("Successfully grabbed {} devices", devices.len());
                }
                grabbed = true;
            } else {
                // Ungrab and close all devices
                tracing::info!("Releasing {} input devices", devices.len());
                for (path, mut dev) in devices.drain() {
                    if let Err(e) = dev.ungrab() {
                        tracing::warn!("Failed to ungrab {}: {}", path.display(), e);
                    } else {
                        tracing::debug!("Released {}", path.display());
                    }
                    // Device is dropped here, closing the fd
                }
                grabbed = false;
            }
        }

        // Read events if grabbed
        if grabbed && !devices.is_empty() {
            for (_path, dev) in &mut devices {
                // Non-blocking read
                if let Ok(events) = dev.fetch_events() {
                    for ev in events {
                        if let Some(grab_event) = convert_event(&ev) {
                            if event_tx.send(grab_event).is_err() {
                                // Receiver dropped
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }

        // Small sleep to avoid busy-looping
        thread::sleep(std::time::Duration::from_millis(1));
    }
}

fn convert_event(ev: &evdev::InputEvent) -> Option<GrabEvent> {
    match ev.kind() {
        InputEventKind::Key(key) => {
            let keycode = key.code() as u32;
            let pressed = ev.value() == 1;
            let released = ev.value() == 0;

            if pressed {
                Some(GrabEvent::KeyDown { keycode })
            } else if released {
                Some(GrabEvent::KeyUp { keycode })
            } else {
                None // Repeat events, ignore
            }
        }
        InputEventKind::RelAxis(axis) => {
            use evdev::RelativeAxisType;
            match axis {
                RelativeAxisType::REL_X => {
                    Some(GrabEvent::PointerMotion {
                        dx: ev.value() as f64,
                        dy: 0.0,
                    })
                }
                RelativeAxisType::REL_Y => {
                    Some(GrabEvent::PointerMotion {
                        dx: 0.0,
                        dy: ev.value() as f64,
                    })
                }
                RelativeAxisType::REL_WHEEL => {
                    Some(GrabEvent::Scroll {
                        horizontal: 0.0,
                        vertical: ev.value() as f64 * -15.0, // Invert and scale
                    })
                }
                RelativeAxisType::REL_HWHEEL => {
                    Some(GrabEvent::Scroll {
                        horizontal: ev.value() as f64 * 15.0,
                        vertical: 0.0,
                    })
                }
                _ => None,
            }
        }
        _ => None,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EvdevGrabError {
    #[error("No input devices found")]
    NoDevices,

    #[error("Thread error: {0}")]
    Thread(String),

    #[error("Device error: {0}")]
    Device(String),
}
