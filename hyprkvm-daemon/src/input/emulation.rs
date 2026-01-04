//! Wayland input emulation
//!
//! Injects keyboard and mouse events via virtual input protocols.
//! Uses wlr-virtual-pointer and virtual-keyboard Wayland protocols.

use std::os::unix::io::{AsFd, OwnedFd};
use std::sync::Arc;

use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    protocol::{wl_registry, wl_seat},
    Connection, Dispatch, EventQueue, QueueHandle,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};
use wayland_client::protocol::wl_pointer::{
    Axis as WlAxis,
    ButtonState as WlButtonState,
};

use hyprkvm_common::{ButtonState, KeyState};

/// Virtual pointer for mouse injection
pub struct VirtualPointer {
    pointer: ZwlrVirtualPointerV1,
    connection: Arc<Connection>,
}

impl VirtualPointer {
    /// Send relative motion
    pub fn motion(&self, dx: f64, dy: f64) {
        // Time in milliseconds (monotonic, we just use 0 for simplicity)
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;

        self.pointer.motion(time, dx, dy);
        self.pointer.frame();
        let _ = self.connection.flush(); // Flush immediately for low latency
    }

    /// Send absolute motion (normalized 0.0-1.0)
    pub fn motion_absolute(&self, x: f64, y: f64, width: u32, height: u32) {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;

        // Convert to fixed-point (wl_fixed)
        let x_fixed = (x * width as f64) as u32;
        let y_fixed = (y * height as f64) as u32;

        self.pointer.motion_absolute(time, x_fixed, y_fixed, width, height);
        self.pointer.frame();
        let _ = self.connection.flush();
    }

    /// Send button event
    pub fn button(&self, button: u32, state: ButtonState) {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;

        let wl_state = match state {
            ButtonState::Pressed => WlButtonState::Pressed,
            ButtonState::Released => WlButtonState::Released,
        };

        self.pointer.button(time, button, wl_state);
        self.pointer.frame();
        let _ = self.connection.flush();
    }

    /// Send scroll (axis) event
    pub fn scroll(&self, horizontal: f64, vertical: f64) {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;

        tracing::debug!("INJECT SCROLL: h={}, v={}", horizontal, vertical);

        // Set axis source to wheel
        use wayland_client::protocol::wl_pointer::AxisSource;
        self.pointer.axis_source(AxisSource::Wheel);

        if vertical.abs() > 0.001 {
            // Calculate discrete steps (each notch is typically 15 units)
            let discrete = (vertical / 15.0).round() as i32;
            if discrete != 0 {
                // Send discrete scroll for wheel mice
                self.pointer.axis_discrete(time, WlAxis::VerticalScroll, vertical, discrete);
            } else {
                // Fallback to smooth scroll for small values
                self.pointer.axis(time, WlAxis::VerticalScroll, vertical);
            }
        }
        if horizontal.abs() > 0.001 {
            let discrete = (horizontal / 15.0).round() as i32;
            if discrete != 0 {
                self.pointer.axis_discrete(time, WlAxis::HorizontalScroll, horizontal, discrete);
            } else {
                self.pointer.axis(time, WlAxis::HorizontalScroll, horizontal);
            }
        }
        self.pointer.frame();
        let _ = self.connection.flush();
    }
}

/// Virtual keyboard for key injection
pub struct VirtualKeyboard {
    keyboard: ZwpVirtualKeyboardV1,
    connection: Arc<Connection>,
    keymap_set: bool,
    /// Track pressed modifier keys for state updates
    modifier_state: ModifierTracker,
    /// Track all currently pressed keys for cleanup
    pressed_keys: std::collections::HashSet<u32>,
}

/// Tracks which modifier keys are currently pressed
#[derive(Default)]
struct ModifierTracker {
    left_shift: bool,
    right_shift: bool,
    left_ctrl: bool,
    right_ctrl: bool,
    left_alt: bool,
    right_alt: bool,
    left_super: bool,
    right_super: bool,
    caps_lock: bool,
}

impl ModifierTracker {
    /// Update state based on key event, returns true if this was a modifier key
    fn update(&mut self, keycode: u32, pressed: bool) -> bool {
        match keycode {
            42 => { self.left_shift = pressed; true }   // KEY_LEFTSHIFT
            54 => { self.right_shift = pressed; true }  // KEY_RIGHTSHIFT
            29 => { self.left_ctrl = pressed; true }    // KEY_LEFTCTRL
            97 => { self.right_ctrl = pressed; true }   // KEY_RIGHTCTRL
            56 => { self.left_alt = pressed; true }     // KEY_LEFTALT
            100 => { self.right_alt = pressed; true }   // KEY_RIGHTALT
            125 => { self.left_super = pressed; true }  // KEY_LEFTMETA
            126 => { self.right_super = pressed; true } // KEY_RIGHTMETA
            58 => { self.caps_lock = pressed; true }    // KEY_CAPSLOCK
            _ => false,
        }
    }

    /// Get XKB modifier mask for depressed modifiers
    fn depressed(&self) -> u32 {
        let mut mask = 0u32;
        if self.left_shift || self.right_shift { mask |= 1; }      // Shift
        if self.left_ctrl || self.right_ctrl { mask |= 4; }        // Control
        if self.left_alt || self.right_alt { mask |= 8; }          // Mod1 (Alt)
        if self.left_super || self.right_super { mask |= 64; }     // Mod4 (Super)
        mask
    }

    /// Get XKB modifier mask for locked modifiers (Caps Lock)
    fn locked(&self) -> u32 {
        if self.caps_lock { 2 } else { 0 }  // Lock bit
    }
}

impl VirtualKeyboard {
    /// Send key event
    pub fn key(&mut self, keycode: u32, state: KeyState) {
        if !self.keymap_set {
            tracing::warn!("Keymap not set, key event may not work correctly");
        }

        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;

        let pressed = state == KeyState::Pressed;
        let wl_state = match state {
            KeyState::Pressed => wl_keyboard_key_state::PRESSED,
            KeyState::Released => wl_keyboard_key_state::RELEASED,
        };

        // Log the key event with modifier state for debugging
        let key_name = keycode_name(keycode);
        tracing::debug!(
            "INJECT KEY: {} {} (keycode={}, modifiers: depressed={:#x}, super={})",
            key_name,
            if pressed { "DOWN" } else { "UP" },
            keycode,
            self.modifier_state.depressed(),
            self.modifier_state.left_super || self.modifier_state.right_super
        );

        // Track pressed keys for cleanup
        if pressed {
            self.pressed_keys.insert(keycode);
        } else {
            self.pressed_keys.remove(&keycode);
        }

        // Send the key event
        self.keyboard.key(time, keycode, wl_state);

        // If this is a modifier key, update and send modifier state
        if self.modifier_state.update(keycode, pressed) {
            tracing::debug!(
                "INJECT MODIFIERS: depressed={:#x} (super={})",
                self.modifier_state.depressed(),
                self.modifier_state.left_super || self.modifier_state.right_super
            );
            self.keyboard.modifiers(
                self.modifier_state.depressed(),
                0, // latched
                self.modifier_state.locked(),
                0, // group
            );
        }

        let _ = self.connection.flush();
    }

    /// Send modifier state directly
    pub fn modifiers(&self, depressed: u32, latched: u32, locked: u32, group: u32) {
        self.keyboard.modifiers(depressed, latched, locked, group);
        let _ = self.connection.flush();
    }

    /// Release all pressed keys and reset internal state
    /// Call this when stopping injection to ensure clean state for next session
    pub fn reset_all_keys(&mut self) {
        let time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u32;

        // Release ALL pressed keys (including arrow keys, not just modifiers)
        let keys_to_release: Vec<u32> = self.pressed_keys.iter().copied().collect();
        for keycode in keys_to_release {
            tracing::debug!("RESET: Releasing key {} ({})", keycode, keycode_name(keycode));
            self.keyboard.key(time, keycode, wl_keyboard_key_state::RELEASED);
        }
        self.pressed_keys.clear();

        // Reset modifier tracking
        self.modifier_state = ModifierTracker::default();

        // Send clean modifier state to compositor
        self.keyboard.modifiers(0, 0, 0, 0);
        let _ = self.connection.flush();

        tracing::debug!("All keys reset complete");
    }
}

// Key state constants
mod wl_keyboard_key_state {
    pub const RELEASED: u32 = 0;
    pub const PRESSED: u32 = 1;
}

/// State for input emulation setup
struct EmulationState {
    pointer_manager: Option<ZwlrVirtualPointerManagerV1>,
    keyboard_manager: Option<ZwpVirtualKeyboardManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for EmulationState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_manager = Some(registry.bind(name, version.min(2), qh, ()));
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_manager = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "wl_seat" => {
                    state.seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
                _ => {}
            }
        }
    }
}

// Implement empty dispatchers for the protocols we use
impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for EmulationState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerManagerV1,
        _event: <ZwlrVirtualPointerManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrVirtualPointerV1, ()> for EmulationState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwlrVirtualPointerV1,
        _event: <ZwlrVirtualPointerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for EmulationState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardManagerV1,
        _event: <ZwpVirtualKeyboardManagerV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, ()> for EmulationState {
    fn event(
        _state: &mut Self,
        _proxy: &ZwpVirtualKeyboardV1,
        _event: <ZwpVirtualKeyboardV1 as wayland_client::Proxy>::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for EmulationState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_seat::WlSeat,
        _event: wl_seat::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

/// Input emulator combining virtual pointer and keyboard
pub struct InputEmulator {
    pub pointer: VirtualPointer,
    pub keyboard: VirtualKeyboard,
    connection: Arc<Connection>,
    event_queue: EventQueue<EmulationState>,
}

impl InputEmulator {
    /// Create a new input emulator
    pub fn new() -> Result<Self, EmulationError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| EmulationError::Connection(e.to_string()))?;
        let conn = Arc::new(conn);

        let (globals, mut event_queue) = registry_queue_init(&conn)
            .map_err(|e| EmulationError::Registry(e.to_string()))?;

        let qh = event_queue.handle();

        let mut state = EmulationState {
            pointer_manager: None,
            keyboard_manager: None,
            seat: None,
        };

        // Bind to required globals
        for global in globals.contents().clone_list() {
            match global.interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => {
                    state.pointer_manager = Some(
                        globals
                            .registry()
                            .bind(global.name, global.version.min(2), &qh, ()),
                    );
                }
                "zwp_virtual_keyboard_manager_v1" => {
                    state.keyboard_manager = Some(
                        globals
                            .registry()
                            .bind(global.name, global.version.min(1), &qh, ()),
                    );
                }
                "wl_seat" => {
                    state.seat = Some(
                        globals
                            .registry()
                            .bind(global.name, global.version.min(7), &qh, ()),
                    );
                }
                _ => {}
            }
        }

        // Roundtrip to ensure we have all globals
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| EmulationError::Dispatch(e.to_string()))?;

        // Check we have required protocols
        let pointer_manager = state.pointer_manager.clone().ok_or_else(|| {
            EmulationError::Protocol("zwlr_virtual_pointer_manager_v1 not available".to_string())
        })?;

        let keyboard_manager = state.keyboard_manager.clone().ok_or_else(|| {
            EmulationError::Protocol("zwp_virtual_keyboard_manager_v1 not available".to_string())
        })?;

        let seat = state.seat.clone().ok_or_else(|| {
            EmulationError::Protocol("wl_seat not available".to_string())
        })?;

        // Create virtual pointer
        let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());

        // Create virtual keyboard
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());

        // Set up a basic keymap for the keyboard
        // This is a minimal xkb keymap
        let keymap_string = include_str!("minimal_keymap.xkb");
        let keymap_size = keymap_string.len() as u32;

        // Create a memfd for the keymap
        let keymap_fd = create_keymap_fd(keymap_string)
            .map_err(|e| EmulationError::Keymap(e.to_string()))?;

        keyboard.keymap(
            1, // WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1
            keymap_fd.as_fd(),
            keymap_size,
        );

        // Roundtrip to ensure everything is set up
        event_queue
            .roundtrip(&mut state)
            .map_err(|e| EmulationError::Dispatch(e.to_string()))?;

        Ok(Self {
            pointer: VirtualPointer {
                pointer,
                connection: conn.clone(),
            },
            keyboard: VirtualKeyboard {
                keyboard,
                connection: conn.clone(),
                keymap_set: true,
                modifier_state: ModifierTracker::default(),
                pressed_keys: std::collections::HashSet::new(),
            },
            connection: conn,
            event_queue,
        })
    }

    /// Dispatch any pending events
    pub fn dispatch(&mut self) -> Result<(), EmulationError> {
        let mut state = EmulationState {
            pointer_manager: None,
            keyboard_manager: None,
            seat: None,
        };
        self.event_queue
            .dispatch_pending(&mut state)
            .map_err(|e| EmulationError::Dispatch(e.to_string()))?;
        self.connection
            .flush()
            .map_err(|e| EmulationError::Dispatch(e.to_string()))?;
        Ok(())
    }
}

/// Create a memfd containing the keymap string
fn create_keymap_fd(keymap: &str) -> std::io::Result<OwnedFd> {
    use std::io::Write;

    // Create memfd
    let fd = rustix::fs::memfd_create(
        "hyprkvm-keymap",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )?;

    // Write keymap
    let mut file = std::fs::File::from(fd);
    file.write_all(keymap.as_bytes())?;
    file.write_all(&[0])?; // Null terminator

    // Seal the file
    let fd = OwnedFd::from(file);

    Ok(fd)
}

#[derive(Debug, thiserror::Error)]
pub enum EmulationError {
    #[error("Failed to connect to Wayland: {0}")]
    Connection(String),

    #[error("Registry error: {0}")]
    Registry(String),

    #[error("Protocol not available: {0}")]
    Protocol(String),

    #[error("Dispatch error: {0}")]
    Dispatch(String),

    #[error("Keymap error: {0}")]
    Keymap(String),
}

// Button codes (from linux/input-event-codes.h)
pub mod button_codes {
    pub const BTN_LEFT: u32 = 0x110;
    pub const BTN_RIGHT: u32 = 0x111;
    pub const BTN_MIDDLE: u32 = 0x112;
    pub const BTN_SIDE: u32 = 0x113;
    pub const BTN_EXTRA: u32 = 0x114;
    pub const BTN_FORWARD: u32 = 0x115;
    pub const BTN_BACK: u32 = 0x116;
}

/// Convert keycode to human-readable name for logging
fn keycode_name(keycode: u32) -> &'static str {
    match keycode {
        1 => "ESC",
        14 => "BACKSPACE",
        15 => "TAB",
        28 => "ENTER",
        29 => "LEFTCTRL",
        42 => "LEFTSHIFT",
        54 => "RIGHTSHIFT",
        56 => "LEFTALT",
        57 => "SPACE",
        58 => "CAPSLOCK",
        97 => "RIGHTCTRL",
        100 => "RIGHTALT",
        103 => "UP",
        105 => "LEFT",
        106 => "RIGHT",
        108 => "DOWN",
        125 => "LEFTMETA",
        126 => "RIGHTMETA",
        _ => "OTHER",
    }
}
