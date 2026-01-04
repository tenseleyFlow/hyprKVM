//! Evdev-based input grabbing
//!
//! Grabs input devices at the kernel level using EVIOCGRAB.
//! This is the most reliable way to capture input on Linux.

use std::collections::HashMap;
use std::fs;
use std::os::unix::io::{AsRawFd, BorrowedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use evdev::{Device, InputEventKind};
use hyprkvm_common::Direction;
use rustix::fs::{fcntl_setfl, OFlags};

use super::grabber::GrabEvent;

// Recovery mode ends when:
// 1. Super key is released (user stopped trying keybinds or will retry with fresh state)
// 2. The target key combo is detected
// 3. A new grab starts

/// Evdev-based input grabber
pub struct EvdevGrabber {
    active: Arc<AtomicBool>,
    /// Flag indicating recovery mode is active (1 = active, 0 = inactive)
    recovery_active: Arc<AtomicU64>,
    /// The direction to watch for in recovery mode (encoded as u8: 1=Up, 2=Down, 3=Left, 4=Right, 0=none)
    recovery_direction: Arc<AtomicU64>,
    event_rx: mpsc::Receiver<GrabEvent>,
    _thread: thread::JoinHandle<()>,
}

impl EvdevGrabber {
    /// Create a new evdev grabber
    pub fn new() -> Result<Self, EvdevGrabError> {
        let active = Arc::new(AtomicBool::new(false));
        let active_clone = active.clone();
        let recovery_active = Arc::new(AtomicU64::new(0));
        let recovery_clone = recovery_active.clone();
        let recovery_direction = Arc::new(AtomicU64::new(0));
        let recovery_dir_clone = recovery_direction.clone();

        let (event_tx, event_rx) = mpsc::channel();

        let thread = thread::Builder::new()
            .name("evdev-grabber".to_string())
            .spawn(move || {
                if let Err(e) = run_evdev_grabber(active_clone, recovery_clone, recovery_dir_clone, event_tx) {
                    tracing::error!("Evdev grabber error: {}", e);
                }
            })
            .map_err(|e| EvdevGrabError::Thread(e.to_string()))?;

        Ok(Self {
            active,
            recovery_active,
            recovery_direction,
            event_rx,
            _thread: thread,
        })
    }

    /// Start grabbing input
    pub fn start(&self) {
        tracing::info!("Starting evdev input grab");
        // Cancel any recovery mode when starting a new grab
        self.recovery_active.store(0, Ordering::SeqCst);
        self.active.store(true, Ordering::SeqCst);
    }

    /// Stop grabbing input and enter recovery monitoring mode
    /// `stale_direction` is the direction of the outgoing transfer - the key that's stale in libinput
    pub fn stop(&self, stale_direction: Option<Direction>) {
        // Encode direction: 1=Up, 2=Down, 3=Left, 4=Right, 0=none
        let dir_code = match stale_direction {
            Some(Direction::Up) => 1,
            Some(Direction::Down) => 2,
            Some(Direction::Left) => 3,
            Some(Direction::Right) => 4,
            None => 0,
        };
        self.recovery_direction.store(dir_code, Ordering::SeqCst);

        tracing::info!("Stopping evdev input grab, entering recovery mode, watching for {:?}",
            stale_direction);

        // Activate recovery mode (will stay active until Super is released or hotkey detected)
        if stale_direction.is_some() {
            self.recovery_active.store(1, Ordering::SeqCst);
        }
        // Deactivate grab
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
    // Be more restrictive - only grab devices that are ACTUALLY keyboards or mice
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
                    let dev_name = dev.name().unwrap_or("unknown").to_lowercase();

                    // Skip devices that are clearly NOT keyboard/mouse
                    if dev_name.contains("power")
                        || dev_name.contains("sleep")
                        || dev_name.contains("button")
                        || dev_name.contains("wmi")
                        || dev_name.contains("hotkey")
                        || dev_name.contains("consumer control")
                        || dev_name.contains("video bus")
                        || dev_name.contains("dualsense")
                        || dev_name.contains("dualshock")
                        || dev_name.contains("controller touchpad")
                    {
                        tracing::debug!("Skipping non-keyboard/mouse device: {}", dev_name);
                        continue;
                    }

                    let has_keys = dev.supported_keys().map(|k| k.iter().count() > 10).unwrap_or(false);
                    let has_rel = dev.supported_relative_axes().map(|r| r.iter().count() > 0).unwrap_or(false);

                    // A real keyboard has many keys (>10), a real mouse has relative axes
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
    recovery_active: Arc<AtomicU64>,
    recovery_direction: Arc<AtomicU64>,
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

    // State machine states
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum State {
        Idle,
        Grabbed,
        Recovery,
        /// Post-recovery: waiting for Super key release while keeping synthetic key-down active
        PostRecovery,
    }

    let mut devices: HashMap<PathBuf, Device> = HashMap::new();
    let mut state = State::Idle;

    // Key state tracking for recovery mode (Super + Arrow detection)
    let mut super_held = false;
    let mut super_was_held_at_start = false; // Track if Super was held when recovery started
    let mut recovery_hotkey_sent = false;

    // Virtual keyboard for synthetic Super key-down (kept alive in PostRecovery state)
    let mut synthetic_keyboard: Option<evdev::uinput::VirtualDevice> = None;

    // Key codes
    const KEY_LEFTMETA: u16 = 125;
    const KEY_RIGHTMETA: u16 = 126;
    const KEY_UP: u16 = 103;
    const KEY_DOWN: u16 = 108;
    const KEY_LEFT: u16 = 105;
    const KEY_RIGHT: u16 = 106;

    loop {
        let is_active = active.load(Ordering::SeqCst);
        let in_recovery = recovery_active.load(Ordering::SeqCst) == 1;

        // State transitions
        match state {
            State::Idle => {
                if is_active {
                    // Transition to Grabbed
                    devices.clear();
                    tracing::info!("Opening and grabbing input devices...");

                    // Small delay before starting grabs to let any pending events settle
                    std::thread::sleep(std::time::Duration::from_millis(50));

                    for path in &device_paths {
                        match Device::open(path) {
                            Ok(mut dev) => {
                                let name = dev.name().unwrap_or("unknown").to_string();
                                let fd = unsafe { BorrowedFd::borrow_raw(dev.as_raw_fd()) };
                                let _ = fcntl_setfl(fd, OFlags::NONBLOCK);
                                let _ = dev.fetch_events(); // Drain pending

                                match dev.grab() {
                                    Ok(()) => {
                                        tracing::info!("Grabbed: {} ({})", name, path.display());
                                        devices.insert(path.clone(), dev);
                                    }
                                    Err(e) => {
                                        tracing::warn!("Cannot grab {}: {}", name, e);
                                    }
                                }
                                // Small delay between grabs to avoid overwhelming libinput
                                std::thread::sleep(std::time::Duration::from_millis(10));
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
                        state = State::Grabbed;
                    }
                }
            }

            State::Grabbed => {
                if !is_active {
                    // Transition to Recovery (ungrab but keep devices open)
                    tracing::info!("Releasing grab, entering recovery mode");

                    for (_path, dev) in &mut devices {
                        if let Err(e) = dev.ungrab() {
                            tracing::warn!("Failed to ungrab: {}", e);
                        }
                    }

                    // Reset recovery state
                    super_held = false;
                    super_was_held_at_start = false;
                    recovery_hotkey_sent = false;

                    // Query current Super key state from physical keyboard
                    for dev in devices.values() {
                        if let Ok(key_state) = dev.get_key_state() {
                            if key_state.contains(evdev::Key::new(KEY_LEFTMETA))
                                || key_state.contains(evdev::Key::new(KEY_RIGHTMETA))
                            {
                                super_held = true;
                                super_was_held_at_start = true;
                                tracing::debug!("Super key is physically held at recovery start");
                            }
                        }
                    }

                    state = State::Recovery;
                    tracing::info!("Now in recovery mode, super_held={}, super_was_held_at_start={}",
                        super_held, super_was_held_at_start);
                } else {
                    // Still grabbed - forward events
                    let mut motion_dx: f64 = 0.0;
                    let mut motion_dy: f64 = 0.0;
                    let mut scroll_h: f64 = 0.0;
                    let mut scroll_v: f64 = 0.0;

                    for dev in devices.values_mut() {
                        if let Ok(events) = dev.fetch_events() {
                            for ev in events {
                                if let InputEventKind::Key(key) = ev.kind() {
                                    tracing::debug!("RAW EVDEV: key={} value={}",
                                        key.code(), ev.value());
                                }
                                match convert_event(&ev) {
                                    Some(GrabEvent::PointerMotion { dx, dy }) => {
                                        motion_dx += dx;
                                        motion_dy += dy;
                                    }
                                    Some(GrabEvent::Scroll { horizontal, vertical }) => {
                                        scroll_h += horizontal;
                                        scroll_v += vertical;
                                    }
                                    Some(other) => {
                                        if event_tx.send(other).is_err() {
                                            return Ok(());
                                        }
                                    }
                                    None => {}
                                }
                            }
                        }
                    }

                    if motion_dx != 0.0 || motion_dy != 0.0 {
                        if event_tx.send(GrabEvent::PointerMotion { dx: motion_dx, dy: motion_dy }).is_err() {
                            return Ok(());
                        }
                    }
                    if scroll_h != 0.0 || scroll_v != 0.0 {
                        if event_tx.send(GrabEvent::Scroll { horizontal: scroll_h, vertical: scroll_v }).is_err() {
                            return Ok(());
                        }
                    }
                }
            }

            State::Recovery => {
                if is_active {
                    // New grab starting, go back to grabbed state
                    // First close current devices, they'll be reopened fresh
                    devices.clear();
                    recovery_active.store(0, Ordering::SeqCst);
                    state = State::Idle;
                    continue;
                }

                if !in_recovery {
                    // Recovery mode was disabled externally
                    tracing::info!("Recovery mode ended (disabled)");
                    devices.clear();
                    state = State::Idle;
                    continue;
                }

                // In recovery mode - monitor for Super+Arrow
                // Read events WITHOUT grab (we're just observing)
                let mut should_end_recovery = false;
                let mut end_reason = "";

                // Decode the direction we're watching for
                let watch_dir_code = recovery_direction.load(Ordering::SeqCst);
                let watch_direction: Option<Direction> = match watch_dir_code {
                    1 => Some(Direction::Up),
                    2 => Some(Direction::Down),
                    3 => Some(Direction::Left),
                    4 => Some(Direction::Right),
                    _ => None,
                };

                for dev in devices.values_mut() {
                    if let Ok(events) = dev.fetch_events() {
                        for ev in events {
                            if let InputEventKind::Key(key) = ev.kind() {
                                let keycode = key.code();
                                let pressed = ev.value() == 1;
                                let released = ev.value() == 0;

                                // Track Super key state
                                if keycode == KEY_LEFTMETA || keycode == KEY_RIGHTMETA {
                                    if pressed {
                                        super_held = true;
                                        tracing::debug!("RECOVERY: Super pressed");
                                    } else if released {
                                        super_held = false;
                                        tracing::debug!("RECOVERY: Super released");

                                        // End recovery when Super is released
                                        // If Super was held at start, user has finished their keybind attempt
                                        // If Super wasn't held at start, they did a fresh Super press+release
                                        if super_was_held_at_start {
                                            tracing::info!("RECOVERY: Super released (was held at start), ending recovery");
                                            should_end_recovery = true;
                                            end_reason = "Super released";
                                            break;
                                        }
                                    }
                                }

                                // Check for Super+Arrow, but ONLY for the direction we're watching
                                if pressed && super_held && !recovery_hotkey_sent {
                                    let key_direction = match keycode {
                                        KEY_UP => Some(Direction::Up),
                                        KEY_DOWN => Some(Direction::Down),
                                        KEY_LEFT => Some(Direction::Left),
                                        KEY_RIGHT => Some(Direction::Right),
                                        _ => None,
                                    };

                                    if let Some(dir) = key_direction {
                                        // Only trigger if this matches the direction we're watching for
                                        if watch_direction == Some(dir) {
                                            tracing::info!("RECOVERY: Detected Super+{:?} (matches watch direction), sending hotkey event", dir);
                                            if event_tx.send(GrabEvent::RecoveryHotkey { direction: dir }).is_err() {
                                                return Ok(());
                                            }
                                            recovery_hotkey_sent = true;
                                            should_end_recovery = true;
                                            end_reason = "hotkey detected";
                                            break;
                                        } else {
                                            tracing::debug!("RECOVERY: Ignoring Super+{:?} (watching for {:?})", dir, watch_direction);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if should_end_recovery {
                        break;
                    }
                }

                // End recovery mode if hotkey was detected or Super was released
                if should_end_recovery {
                    tracing::info!("Recovery mode ended ({})", end_reason);
                    recovery_active.store(0, Ordering::SeqCst);

                    // If we detected a hotkey and Super is still held, we need to send a
                    // synthetic Super key-down via uinput. This informs libinput that Super
                    // is pressed, since it never saw the original key-down (it was grabbed).
                    // CRITICAL: We must keep the virtual device alive until Super is released,
                    // otherwise the kernel will auto-send key-up when the device is destroyed.
                    if recovery_hotkey_sent && super_held {
                        tracing::info!("RECOVERY: Super still held, entering PostRecovery to maintain synthetic key-down");

                        // Create virtual keyboard and send Super key-down
                        match create_synthetic_keyboard_with_super_down() {
                            Ok(virt_dev) => {
                                synthetic_keyboard = Some(virt_dev);
                                // Keep devices open to monitor for Super release
                                state = State::PostRecovery;
                                tracing::info!("PostRecovery: synthetic keyboard created, monitoring for Super release");
                            }
                            Err(e) => {
                                tracing::error!("Failed to create synthetic keyboard: {}", e);
                                devices.clear();
                                state = State::Idle;
                            }
                        }
                    } else {
                        // Super already released or no hotkey detected, clean up normally
                        devices.clear();
                        state = State::Idle;
                    }
                }
            }

            State::PostRecovery => {
                // In post-recovery mode, we're keeping the synthetic keyboard alive
                // with Super key-down. Monitor physical keyboard for Super release.
                if is_active {
                    // New grab starting, clean up and transition
                    tracing::info!("PostRecovery: new grab requested, cleaning up");
                    if let Some(ref mut virt_dev) = synthetic_keyboard {
                        // Send Super key-up before destroying
                        let key_up = evdev::InputEvent::new(evdev::EventType::KEY, KEY_LEFTMETA, 0);
                        let syn = evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
                        let _ = virt_dev.emit(&[key_up, syn]);
                    }
                    synthetic_keyboard = None;
                    devices.clear();
                    state = State::Idle;
                    continue;
                }

                // Monitor for Super key release on physical keyboard
                let mut super_released = false;
                for dev in devices.values_mut() {
                    if let Ok(events) = dev.fetch_events() {
                        for ev in events {
                            if let InputEventKind::Key(key) = ev.kind() {
                                let keycode = key.code();
                                let released = ev.value() == 0;

                                // Check for Super release
                                if (keycode == KEY_LEFTMETA || keycode == KEY_RIGHTMETA) && released {
                                    super_released = true;
                                    break;
                                }
                            }
                        }
                    }
                    if super_released {
                        break;
                    }
                }

                // Handle Super release outside the borrow
                if super_released {
                    tracing::info!("PostRecovery: Super released, sending synthetic key-up and cleaning up");

                    // Send synthetic Super key-up
                    if let Some(ref mut virt_dev) = synthetic_keyboard {
                        let key_up = evdev::InputEvent::new(evdev::EventType::KEY, KEY_LEFTMETA, 0);
                        let syn = evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
                        if let Err(e) = virt_dev.emit(&[key_up, syn]) {
                            tracing::warn!("Failed to send synthetic Super key-up: {}", e);
                        }
                    }

                    // Clean up
                    synthetic_keyboard = None;
                    devices.clear();
                    super_held = false;
                    state = State::Idle;
                }
            }
        }

        // Minimal sleep
        thread::sleep(std::time::Duration::from_micros(100));
    }
}

// Mouse button codes (from linux/input-event-codes.h)
const BTN_MOUSE: u16 = 0x110;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
const BTN_SIDE: u16 = 0x113;
const BTN_EXTRA: u16 = 0x114;
const BTN_FORWARD: u16 = 0x115;
const BTN_BACK: u16 = 0x116;
const BTN_TASK: u16 = 0x117;

fn is_mouse_button(code: u16) -> bool {
    code >= BTN_MOUSE && code <= BTN_TASK
}

fn convert_event(ev: &evdev::InputEvent) -> Option<GrabEvent> {
    match ev.kind() {
        InputEventKind::Key(key) => {
            let keycode = key.code();
            let pressed = ev.value() == 1;
            let released = ev.value() == 0;

            // Check if this is a mouse button
            if is_mouse_button(keycode) {
                if pressed || released {
                    tracing::debug!("MOUSE BUTTON: code={:#x} pressed={}", keycode, pressed);
                    Some(GrabEvent::PointerButton {
                        button: keycode as u32,
                        pressed,
                    })
                } else {
                    None // Repeat events, ignore
                }
            } else {
                // Regular keyboard key
                if pressed {
                    Some(GrabEvent::KeyDown { keycode: keycode as u32 })
                } else if released {
                    Some(GrabEvent::KeyUp { keycode: keycode as u32 })
                } else {
                    None // Repeat events, ignore
                }
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

/// Send synthetic key-up events via uinput for specified keycodes.
/// This creates a temporary virtual keyboard, sends the events, and destroys it.
/// Used to clear stale key state in libinput after releasing the evdev grab.
pub fn send_synthetic_key_ups(keycodes: &[u16]) -> Result<(), std::io::Error> {
    use evdev::uinput::VirtualDeviceBuilder;
    use evdev::{AttributeSet, Key};

    if keycodes.is_empty() {
        return Ok(());
    }

    tracing::debug!("Creating uinput device to send synthetic key-ups for {:?}", keycodes);

    // Build the key set for all keys we might send
    let mut keys = AttributeSet::<Key>::new();
    for &keycode in keycodes {
        keys.insert(Key::new(keycode));
    }

    // Also add common modifier keys in case we need them
    keys.insert(Key::new(125)); // KEY_LEFTMETA
    keys.insert(Key::new(126)); // KEY_RIGHTMETA

    // Create a virtual keyboard device
    let mut device = VirtualDeviceBuilder::new()?
        .name("hyprkvm-synthetic")
        .with_keys(&keys)?
        .build()?;

    // Brief pause to let the device be recognized
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Send key-up events for each keycode
    for &keycode in keycodes {
        let key_up = evdev::InputEvent::new(evdev::EventType::KEY, keycode, 0);
        let syn = evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
        device.emit(&[key_up, syn])?;
        tracing::debug!("Sent synthetic key-up for keycode {}", keycode);
    }

    // Flush and brief pause before device is dropped
    std::thread::sleep(std::time::Duration::from_millis(10));

    tracing::debug!("Synthetic key-ups sent successfully");
    Ok(())
}

/// Send synthetic key-down events via uinput for specified keycodes.
/// This creates a temporary virtual keyboard, sends the events, and destroys it.
/// Used to inform libinput about keys that are physically held after ungrab.
/// NOTE: The device is destroyed after this function returns, which will trigger
/// an automatic key-up. Use `create_synthetic_keyboard_with_super_down` if you
/// need to keep the key pressed.
pub fn send_synthetic_key_downs(keycodes: &[u16]) -> Result<(), std::io::Error> {
    use evdev::uinput::VirtualDeviceBuilder;
    use evdev::{AttributeSet, Key};

    if keycodes.is_empty() {
        return Ok(());
    }

    tracing::debug!("Creating uinput device to send synthetic key-downs for {:?}", keycodes);

    // Build the key set for all keys we might send
    let mut keys = AttributeSet::<Key>::new();
    for &keycode in keycodes {
        keys.insert(Key::new(keycode));
    }

    // Create a virtual keyboard device
    let mut device = VirtualDeviceBuilder::new()?
        .name("hyprkvm-synthetic")
        .with_keys(&keys)?
        .build()?;

    // Brief pause to let the device be recognized
    std::thread::sleep(std::time::Duration::from_millis(10));

    // Send key-down events for each keycode
    for &keycode in keycodes {
        let key_down = evdev::InputEvent::new(evdev::EventType::KEY, keycode, 1);
        let syn = evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
        device.emit(&[key_down, syn])?;
        tracing::debug!("Sent synthetic key-down for keycode {}", keycode);
    }

    // Flush and brief pause before device is dropped
    std::thread::sleep(std::time::Duration::from_millis(10));

    tracing::debug!("Synthetic key-downs sent successfully");
    Ok(())
}

/// Create a virtual keyboard with Super (Left Meta) key pressed.
/// Returns the device which must be kept alive to maintain the key-down state.
/// When the device is dropped, the kernel will automatically send key-up.
fn create_synthetic_keyboard_with_super_down() -> Result<evdev::uinput::VirtualDevice, std::io::Error> {
    use evdev::uinput::VirtualDeviceBuilder;
    use evdev::{AttributeSet, Key};

    tracing::debug!("Creating persistent synthetic keyboard with Super key-down");

    // Build key set with Super keys
    let mut keys = AttributeSet::<Key>::new();
    keys.insert(Key::new(125)); // KEY_LEFTMETA
    keys.insert(Key::new(126)); // KEY_RIGHTMETA

    // Create virtual keyboard
    let mut device = VirtualDeviceBuilder::new()?
        .name("hyprkvm-super-hold")
        .with_keys(&keys)?
        .build()?;

    // Brief pause to let the device be recognized
    std::thread::sleep(std::time::Duration::from_millis(20));

    // Send Super key-down
    let key_down = evdev::InputEvent::new(evdev::EventType::KEY, 125, 1); // KEY_LEFTMETA
    let syn = evdev::InputEvent::new(evdev::EventType::SYNCHRONIZATION, 0, 0);
    device.emit(&[key_down, syn])?;

    tracing::info!("Synthetic keyboard created with Super key-down, device will be kept alive");
    Ok(device)
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
