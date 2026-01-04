//! Evdev-based input grabbing
//!
//! Grabs input devices at the kernel level using EVIOCGRAB.
//! This is the most reliable way to capture input on Linux.

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::{AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use evdev::{Device, InputEventKind};
use rustix::fs::{fcntl_setfl, OFlags};

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

                            // Set non-blocking mode
                            // SAFETY: dev owns the fd and will outlive this borrow
                            let fd = unsafe { BorrowedFd::borrow_raw(dev.as_raw_fd()) };
                            if let Err(e) = fcntl_setfl(fd, OFlags::NONBLOCK) {
                                tracing::warn!("Failed to set non-blocking on {}: {}", name, e);
                            }

                            // Drain any pending events before grabbing to start fresh
                            let _ = dev.fetch_events();

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

                // Keys we care about: modifiers and arrows
                let modifier_keys: &[u16] = &[
                    125, 126,  // KEY_LEFTMETA, KEY_RIGHTMETA (Super)
                    42, 54,    // KEY_LEFTSHIFT, KEY_RIGHTSHIFT
                    29, 97,    // KEY_LEFTCTRL, KEY_RIGHTCTRL
                    56, 100,   // KEY_LEFTALT, KEY_RIGHTALT
                ];
                let arrow_keys: &[u16] = &[
                    103, 108, 105, 106, // KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT
                ];

                for (path, mut dev) in devices.drain() {
                    // First ungrab so libinput can receive events
                    if let Err(e) = dev.ungrab() {
                        tracing::warn!("Failed to ungrab {}: {}", path.display(), e);
                        continue;
                    }
                    tracing::debug!("Released {}", path.display());

                    // Query actual physical key state AFTER ungrab
                    let key_state = match dev.get_key_state() {
                        Ok(state) => state,
                        Err(e) => {
                            tracing::debug!("Could not query key state for {}: {}", path.display(), e);
                            continue;
                        }
                    };

                    // For ARROW keys: if NOT pressed, send UP to clear stale state
                    // (Hyprland thought they were pressed from before grab)
                    for &keycode in arrow_keys {
                        let key = evdev::Key::new(keycode);
                        if !key_state.contains(key) {
                            // Key is not pressed - send UP to clear stale state
                            let key_event = evdev::InputEvent::new(
                                evdev::EventType::KEY, keycode, 0,
                            );
                            let syn_event = evdev::InputEvent::new(
                                evdev::EventType::SYNCHRONIZATION, 0, 0,
                            );
                            let _ = dev.send_events(&[key_event, syn_event]);
                        }
                        // If key IS pressed, don't send anything - user is still holding it
                    }

                    // For MODIFIER keys: if pressed, send UP then DOWN (fresh edge)
                    // This gives Hyprland a clean state transition
                    for &keycode in modifier_keys {
                        let key = evdev::Key::new(keycode);
                        if key_state.contains(key) {
                            // Key is pressed - send UP then DOWN for fresh edge
                            let up_event = evdev::InputEvent::new(
                                evdev::EventType::KEY, keycode, 0,
                            );
                            let down_event = evdev::InputEvent::new(
                                evdev::EventType::KEY, keycode, 1,
                            );
                            let syn_event = evdev::InputEvent::new(
                                evdev::EventType::SYNCHRONIZATION, 0, 0,
                            );
                            let _ = dev.send_events(&[up_event, syn_event, down_event, syn_event]);
                            tracing::debug!("Sent UP+DOWN for held modifier key {}", keycode);
                        }
                        // If not pressed, don't send anything - already released
                    }

                    // Device is dropped here, closing the fd
                }
                grabbed = false;
            }
        }

        // Read events if grabbed
        if grabbed && !devices.is_empty() {
            // Accumulate motion deltas across all devices and events
            let mut motion_dx: f64 = 0.0;
            let mut motion_dy: f64 = 0.0;
            let mut scroll_h: f64 = 0.0;
            let mut scroll_v: f64 = 0.0;

            for (_path, dev) in &mut devices {
                // Non-blocking read
                if let Ok(events) = dev.fetch_events() {
                    for ev in events {
                        // Log raw key events from kernel for debugging
                        if let InputEventKind::Key(key) = ev.kind() {
                            tracing::debug!("RAW EVDEV: key={} value={} (1=press, 0=release, 2=repeat)",
                                key.code(), ev.value());
                        }
                        match convert_event(&ev) {
                            Some(GrabEvent::PointerMotion { dx, dy }) => {
                                // Accumulate motion instead of sending immediately
                                motion_dx += dx;
                                motion_dy += dy;
                            }
                            Some(GrabEvent::Scroll { horizontal, vertical }) => {
                                // Accumulate scroll
                                scroll_h += horizontal;
                                scroll_v += vertical;
                            }
                            Some(other) => {
                                // Key events etc. - send immediately
                                if event_tx.send(other).is_err() {
                                    return Ok(());
                                }
                            }
                            None => {}
                        }
                    }
                }
            }

            // Send accumulated motion as single event (if any)
            if motion_dx != 0.0 || motion_dy != 0.0 {
                if event_tx.send(GrabEvent::PointerMotion { dx: motion_dx, dy: motion_dy }).is_err() {
                    return Ok(());
                }
            }

            // Send accumulated scroll as single event (if any)
            if scroll_h != 0.0 || scroll_v != 0.0 {
                if event_tx.send(GrabEvent::Scroll { horizontal: scroll_h, vertical: scroll_v }).is_err() {
                    return Ok(());
                }
            }
        }

        // Minimal sleep to avoid busy-looping while keeping latency low
        thread::sleep(std::time::Duration::from_micros(100));
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
