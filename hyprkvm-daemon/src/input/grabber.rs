//! Input grabber - captures all keyboard/mouse input when active
//!
//! Uses layer-shell with exclusive keyboard grab to intercept all input.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface},
    Connection, QueueHandle,
};

use hyprkvm_common::protocol::{InputEventPayload, InputEventType};

/// Input event from the grabber
#[derive(Debug, Clone)]
pub enum GrabEvent {
    KeyDown { keycode: u32 },
    KeyUp { keycode: u32 },
    PointerMotion { dx: f64, dy: f64 },
    PointerButton { button: u32, pressed: bool },
    Scroll { horizontal: f64, vertical: f64 },
    ModifiersChanged { mods: Modifiers },
}

impl GrabEvent {
    pub fn to_protocol(&self, seq: u64) -> InputEventPayload {
        let timestamp_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros() as u64;

        let event = match self {
            GrabEvent::KeyDown { keycode } => InputEventType::KeyDown { keycode: *keycode },
            GrabEvent::KeyUp { keycode } => InputEventType::KeyUp { keycode: *keycode },
            GrabEvent::PointerMotion { dx, dy } => InputEventType::PointerMotion { dx: *dx, dy: *dy },
            GrabEvent::PointerButton { button, pressed } => {
                InputEventType::PointerButton {
                    button: *button,
                    pressed: *pressed,
                }
            }
            GrabEvent::Scroll { horizontal, vertical } => {
                InputEventType::Scroll {
                    horizontal: *horizontal,
                    vertical: *vertical,
                }
            }
            GrabEvent::ModifiersChanged { mods } => {
                InputEventType::ModifierState {
                    shift: mods.shift,
                    ctrl: mods.ctrl,
                    alt: mods.alt,
                    super_key: mods.logo,
                }
            }
        };

        InputEventPayload {
            sequence: seq,
            timestamp_us,
            event,
        }
    }
}

/// Input grabber configuration
pub struct InputGrabberConfig {
    /// Hide the cursor while grabbing
    pub hide_cursor: bool,
}

impl Default for InputGrabberConfig {
    fn default() -> Self {
        Self { hide_cursor: true }
    }
}

/// Input grabber - intercepts all keyboard and mouse input
pub struct InputGrabber {
    active: Arc<AtomicBool>,
    event_rx: std::sync::mpsc::Receiver<GrabEvent>,
    _thread: thread::JoinHandle<()>,
}

impl InputGrabber {
    /// Create a new input grabber
    pub fn new(config: InputGrabberConfig) -> Result<Self, GrabberError> {
        let active = Arc::new(AtomicBool::new(false));
        let active_clone = active.clone();

        let (event_tx, event_rx) = std::sync::mpsc::channel();

        let thread = thread::Builder::new()
            .name("input-grabber".to_string())
            .spawn(move || {
                if let Err(e) = run_grabber(active_clone, event_tx, config) {
                    tracing::error!("Grabber thread error: {}", e);
                }
            })
            .map_err(|e| GrabberError::Thread(e.to_string()))?;

        Ok(Self {
            active,
            event_rx,
            _thread: thread,
        })
    }

    /// Start grabbing input
    pub fn start(&self) {
        tracing::info!("Starting input grab");
        self.active.store(true, Ordering::SeqCst);
    }

    /// Stop grabbing input
    pub fn stop(&self) {
        tracing::info!("Stopping input grab");
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

fn run_grabber(
    active: Arc<AtomicBool>,
    event_tx: std::sync::mpsc::Sender<GrabEvent>,
    _config: InputGrabberConfig,
) -> Result<(), GrabberError> {
    let conn = Connection::connect_to_env()
        .map_err(|e| GrabberError::Connection(e.to_string()))?;

    let (globals, mut event_queue) = registry_queue_init(&conn)
        .map_err(|e| GrabberError::Registry(e.to_string()))?;

    let qh = event_queue.handle();

    let mut state = GrabberState {
        active,
        event_tx,
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor_state: CompositorState::bind(&globals, &qh)
            .map_err(|e| GrabberError::Protocol(e.to_string()))?,
        layer_shell: LayerShell::bind(&globals, &qh)
            .map_err(|e| GrabberError::Protocol(e.to_string()))?,

        layer_surface: None,
        keyboard: None,
        pointer: None,
        last_pointer_pos: (0.0, 0.0),
        configured: false,
        running: true,
    };

    // Wait for first output
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| GrabberError::Dispatch(e.to_string()))?;

    // Create grabber surface (invisible fullscreen layer)
    if let Some(output) = state.output_state.outputs().next() {
        let surface = state.compositor_state.create_surface(&qh);

        let layer = state.layer_shell.create_layer_surface(
            &qh,
            surface,
            Layer::Overlay,
            Some("hyprkvm-grabber"),
            Some(&output),
        );

        // Configure for input grab
        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1); // Don't push other windows
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(1, 1); // Minimal size, invisible

        layer.commit();
        state.layer_surface = Some(layer);
    }

    // Main loop
    loop {
        if !state.running {
            break;
        }

        event_queue
            .blocking_dispatch(&mut state)
            .map_err(|e| GrabberError::Dispatch(e.to_string()))?;
    }

    Ok(())
}

struct GrabberState {
    active: Arc<AtomicBool>,
    event_tx: std::sync::mpsc::Sender<GrabEvent>,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor_state: CompositorState,
    layer_shell: LayerShell,

    layer_surface: Option<LayerSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    last_pointer_pos: (f64, f64),
    configured: bool,
    running: bool,
}

impl CompositorHandler for GrabberState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for GrabberState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for GrabberState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.running = false;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        _configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        if !self.configured {
            self.configured = true;
            // Commit to acknowledge the configure
            layer.commit();
        }
    }
}

impl SeatHandler for GrabberState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            let keyboard = self.seat_state.get_keyboard(qh, &seat, None)
                .expect("Failed to get keyboard");
            self.keyboard = Some(keyboard);
        }

        if capability == Capability::Pointer && self.pointer.is_none() {
            let pointer = self.seat_state.get_pointer(qh, &seat)
                .expect("Failed to get pointer");
            self.pointer = Some(pointer);
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for GrabberState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.active.load(Ordering::SeqCst) {
            let _ = self.event_tx.send(GrabEvent::KeyDown {
                keycode: event.raw_code,
            });
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if self.active.load(Ordering::SeqCst) {
            let _ = self.event_tx.send(GrabEvent::KeyUp {
                keycode: event.raw_code,
            });
        }
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: u32,
    ) {
        if self.active.load(Ordering::SeqCst) {
            let _ = self.event_tx.send(GrabEvent::ModifiersChanged { mods: modifiers });
        }
    }
}

impl PointerHandler for GrabberState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        if !self.active.load(Ordering::SeqCst) {
            return;
        }

        for event in events {
            match event.kind {
                PointerEventKind::Motion { .. } => {
                    let (x, y) = event.position;
                    let dx = x - self.last_pointer_pos.0;
                    let dy = y - self.last_pointer_pos.1;
                    self.last_pointer_pos = (x, y);

                    // Only send if there's actual motion
                    if dx.abs() > 0.001 || dy.abs() > 0.001 {
                        let _ = self.event_tx.send(GrabEvent::PointerMotion { dx, dy });
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    let _ = self.event_tx.send(GrabEvent::PointerButton {
                        button,
                        pressed: true,
                    });
                }
                PointerEventKind::Release { button, .. } => {
                    let _ = self.event_tx.send(GrabEvent::PointerButton {
                        button,
                        pressed: false,
                    });
                }
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    let _ = self.event_tx.send(GrabEvent::Scroll {
                        horizontal: horizontal.absolute,
                        vertical: vertical.absolute,
                    });
                }
                _ => {}
            }
        }
    }
}

impl ProvidesRegistryState for GrabberState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(GrabberState);
delegate_output!(GrabberState);
delegate_seat!(GrabberState);
delegate_keyboard!(GrabberState);
delegate_pointer!(GrabberState);
delegate_layer!(GrabberState);
delegate_registry!(GrabberState);

#[derive(Debug, thiserror::Error)]
pub enum GrabberError {
    #[error("Failed to connect to Wayland: {0}")]
    Connection(String),

    #[error("Failed to initialize registry: {0}")]
    Registry(String),

    #[error("Protocol not available: {0}")]
    Protocol(String),

    #[error("Dispatch error: {0}")]
    Dispatch(String),

    #[error("Thread error: {0}")]
    Thread(String),
}
