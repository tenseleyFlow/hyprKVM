//! Input grabber - captures all keyboard/mouse input when active
//!
//! Uses layer-shell with exclusive keyboard grab to intercept all input.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
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
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
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
    /// Hotkey detected during recovery monitoring (bypasses libinput stale state)
    RecoveryHotkey { direction: hyprkvm_common::Direction },
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
            GrabEvent::RecoveryHotkey { .. } => {
                // This is a local-only event, should never be sent over network
                panic!("RecoveryHotkey cannot be converted to protocol");
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

    let shm_state = Shm::bind(&globals, &qh)
        .map_err(|e| GrabberError::Protocol(format!("shm: {}", e)))?;

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
        shm_state,
        pool: None,

        layer_surface: None,
        keyboard: None,
        pointer: None,
        last_pointer_pos: (0.0, 0.0),
        configured: false,
        running: true,
        surface_width: 1,
        surface_height: 1,
        is_mapped: false,
        was_active: false,
    };

    // Wait for first output
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| GrabberError::Dispatch(e.to_string()))?;

    // Create shm pool for buffers - needs to be large enough for fullscreen
    // 4K display = 3840x2160x4 = ~33MB, allocate 64MB to be safe
    state.pool = Some(
        SlotPool::new(64 * 1024 * 1024, &state.shm_state)
            .map_err(|e| GrabberError::Protocol(format!("pool: {}", e)))?,
    );

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

        // Configure for input grab - fullscreen transparent surface
        // With all anchors and size 0,0, compositor will expand to fill output
        layer.set_anchor(Anchor::all());
        layer.set_exclusive_zone(-1); // Don't push other windows
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.set_size(0, 0); // Let compositor determine size (fullscreen)

        layer.commit();
        state.layer_surface = Some(layer);
    }

    // Another roundtrip to process configure
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| GrabberError::Dispatch(e.to_string()))?;

    tracing::info!("Input grabber initialized, waiting for activation");

    // Main loop - poll active flag and manage surface
    loop {
        if !state.running {
            break;
        }

        // Check if active state changed
        let is_active = state.active.load(Ordering::SeqCst);
        if is_active != state.was_active {
            state.was_active = is_active;
            state.update_surface_mapping(is_active);
        }

        // Use dispatch with timeout to allow periodic checking of active flag
        event_queue
            .dispatch_pending(&mut state)
            .map_err(|e| GrabberError::Dispatch(e.to_string()))?;

        // Flush and prepare read
        if let Some(guard) = event_queue
            .prepare_read()
        {
            // Read events with timeout (50ms)
            let _ = guard.read();
        }

        // Small sleep to avoid busy-looping
        std::thread::sleep(std::time::Duration::from_millis(10));
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
    shm_state: Shm,
    pool: Option<SlotPool>,

    layer_surface: Option<LayerSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    last_pointer_pos: (f64, f64),
    configured: bool,
    running: bool,
    surface_width: u32,
    surface_height: u32,
    /// Track if surface is currently mapped (has buffer attached)
    is_mapped: bool,
    /// Last known active state - for detecting transitions
    was_active: bool,
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
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Update size from configure event
        if configure.new_size.0 > 0 {
            self.surface_width = configure.new_size.0;
        }
        if configure.new_size.1 > 0 {
            self.surface_height = configure.new_size.1;
        }

        self.configured = true;

        // Only map surface (attach buffer) if we're currently active
        // Otherwise just acknowledge the configure by committing without a buffer
        if self.active.load(Ordering::SeqCst) {
            self.draw_surface(layer);
            self.is_mapped = true;
        } else {
            // Commit without buffer - surface won't be mapped but config is ack'd
            layer.commit();
        }

        tracing::debug!(
            "Grabber surface configured: {}x{}, mapped: {}",
            self.surface_width,
            self.surface_height,
            self.is_mapped
        );
    }
}

impl GrabberState {
    fn update_surface_mapping(&mut self, should_map: bool) {
        // Clone the wl_surface to avoid borrow issues
        let wl_surface = match &self.layer_surface {
            Some(l) => l.wl_surface().clone(),
            None => return,
        };

        if should_map {
            tracing::info!("Grabber activating - mapping surface");

            let pool = match &mut self.pool {
                Some(pool) => pool,
                None => {
                    tracing::warn!("No SHM pool available for drawing");
                    return;
                }
            };

            let width = self.surface_width;
            let height = self.surface_height;
            let stride = width * 4;

            let (buffer, canvas) = match pool.create_buffer(
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            ) {
                Ok(result) => result,
                Err(e) => {
                    tracing::error!("Failed to create buffer: {:?}", e);
                    return;
                }
            };

            // Fill with fully transparent pixels (ARGB = 0x00000000)
            canvas.fill(0);

            // Attach buffer to surface
            wl_surface.attach(Some(buffer.wl_buffer()), 0, 0);
            wl_surface.damage_buffer(0, 0, width as i32, height as i32);
            wl_surface.commit();

            tracing::debug!("Attached {}x{} transparent buffer to grabber surface", width, height);
            self.is_mapped = true;
        } else {
            tracing::info!("Grabber deactivating - unmapping surface");
            // Unmap by attaching null buffer
            wl_surface.attach(None, 0, 0);
            wl_surface.commit();
            self.is_mapped = false;
        }
    }

    fn draw_surface(&mut self, layer: &LayerSurface) {
        let wl_surface = layer.wl_surface().clone();

        let pool = match &mut self.pool {
            Some(pool) => pool,
            None => {
                tracing::warn!("No SHM pool available for drawing");
                return;
            }
        };

        let width = self.surface_width;
        let height = self.surface_height;
        let stride = width * 4;

        let (buffer, canvas) = match pool.create_buffer(
            width as i32,
            height as i32,
            stride as i32,
            wl_shm::Format::Argb8888,
        ) {
            Ok(result) => result,
            Err(e) => {
                tracing::error!("Failed to create buffer: {:?}", e);
                return;
            }
        };

        // Fill with fully transparent pixels (ARGB = 0x00000000)
        canvas.fill(0);

        // Attach buffer to surface
        wl_surface.attach(Some(buffer.wl_buffer()), 0, 0);
        wl_surface.damage_buffer(0, 0, width as i32, height as i32);
        layer.commit();

        tracing::debug!("Attached {}x{} transparent buffer to grabber surface", width, height);
    }
}

impl ShmHandler for GrabberState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
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
        tracing::info!("Grabber keyboard ENTER - we have keyboard focus");
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        tracing::info!("Grabber keyboard LEAVE - lost keyboard focus");
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        tracing::debug!("Grabber received key press: keycode={}, active={}", event.raw_code, self.active.load(Ordering::SeqCst));
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
        for event in events {
            match &event.kind {
                PointerEventKind::Enter { .. } => {
                    tracing::info!("Grabber pointer ENTER at ({}, {})", event.position.0, event.position.1);
                }
                PointerEventKind::Leave { .. } => {
                    tracing::info!("Grabber pointer LEAVE");
                }
                _ => {}
            }
        }

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
delegate_shm!(GrabberState);
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
