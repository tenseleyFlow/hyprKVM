//! Mouse edge capture via layer-shell
//!
//! Creates invisible 1-pixel barriers at screen edges to detect
//! when the cursor attempts to leave the screen.

use std::sync::mpsc;
use std::time::Instant;

use hyprkvm_common::Direction;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_layer, delegate_output, delegate_pointer, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
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
    protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};

/// Event emitted when cursor hits a screen edge
#[derive(Debug, Clone)]
pub struct EdgeEvent {
    pub direction: Direction,
    pub position: (i32, i32),
    pub timestamp: Instant,
}

/// Configuration for edge barriers
#[derive(Debug, Clone)]
pub struct EdgeCaptureConfig {
    /// Barrier thickness in pixels
    pub barrier_size: u32,
    /// Which edges to create barriers on
    pub enabled_edges: Vec<Direction>,
}

impl Default for EdgeCaptureConfig {
    fn default() -> Self {
        Self {
            barrier_size: 5, // 5 pixels to ensure we catch events
            enabled_edges: vec![
                Direction::Left,
                Direction::Right,
                Direction::Up,
                Direction::Down,
            ],
        }
    }
}

/// An edge barrier surface
struct EdgeBarrier {
    direction: Direction,
    surface: LayerSurface,
    width: u32,
    height: u32,
    configured: bool,
}

/// Edge capture system state
struct EdgeCaptureState {
    registry_state: RegistryState,
    compositor_state: CompositorState,
    output_state: OutputState,
    seat_state: SeatState,
    shm_state: Shm,
    layer_shell: LayerShell,

    barriers: Vec<EdgeBarrier>,
    pool: Option<SlotPool>,
    event_tx: mpsc::Sender<EdgeEvent>,
    config: EdgeCaptureConfig,

    // Track outputs for barrier sizing
    outputs: Vec<OutputInfo>,

    // Track which barrier surface the pointer is currently on
    active_barrier_idx: Option<usize>,
    // Last position on barrier (for detecting edge-pushing motion)
    last_barrier_pos: (f64, f64),
    // Debounce: last time we sent an event for each direction
    last_trigger_time: std::collections::HashMap<Direction, Instant>,
}

#[derive(Debug, Clone)]
struct OutputInfo {
    output: wl_output::WlOutput,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
}

impl EdgeCaptureState {
    fn create_barriers(&mut self, qh: &QueueHandle<Self>) {
        // Get total screen bounds
        let (min_x, min_y, max_x, max_y) = self.screen_bounds();

        for direction in &self.config.enabled_edges.clone() {
            // Find the correct output for this edge direction
            let target_output = self.find_edge_output(*direction);

            let (width, height, anchor) = match direction {
                Direction::Left => (
                    self.config.barrier_size,
                    target_output.as_ref().map(|o| o.height).unwrap_or((max_y - min_y) as u32),
                    Anchor::LEFT | Anchor::TOP | Anchor::BOTTOM,
                ),
                Direction::Right => (
                    self.config.barrier_size,
                    target_output.as_ref().map(|o| o.height).unwrap_or((max_y - min_y) as u32),
                    Anchor::RIGHT | Anchor::TOP | Anchor::BOTTOM,
                ),
                Direction::Up => (
                    target_output.as_ref().map(|o| o.width).unwrap_or((max_x - min_x) as u32),
                    self.config.barrier_size,
                    Anchor::TOP | Anchor::LEFT | Anchor::RIGHT,
                ),
                Direction::Down => (
                    target_output.as_ref().map(|o| o.width).unwrap_or((max_x - min_x) as u32),
                    self.config.barrier_size,
                    Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT,
                ),
            };

            // Create layer surface on the specific edge output
            let surface = self.compositor_state.create_surface(qh);
            let output_ref = target_output.as_ref().map(|o| &o.output);
            let layer_surface = self.layer_shell.create_layer_surface(
                qh,
                surface,
                Layer::Top, // Top layer to catch pointer
                Some(format!("hyprkvm-edge-{}", direction)),
                output_ref,
            );

            tracing::info!(
                "Creating {:?} barrier on output {:?}, size {}x{}",
                direction,
                target_output.as_ref().map(|o| format!("at ({}, {})", o.x, o.y)),
                width,
                height
            );

            // Configure the layer surface
            layer_surface.set_anchor(anchor);
            layer_surface.set_size(width, height);
            layer_surface.set_exclusive_zone(-1); // Don't reserve space
            layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);

            // Commit to apply configuration
            layer_surface.commit();

            self.barriers.push(EdgeBarrier {
                direction: *direction,
                surface: layer_surface,
                width,
                height,
                configured: false,
            });
        }
    }

    /// Find the output at the edge of the screen for a given direction
    fn find_edge_output(&self, direction: Direction) -> Option<OutputInfo> {
        if self.outputs.is_empty() {
            return None;
        }

        match direction {
            Direction::Left => {
                // Find output with minimum x (leftmost)
                self.outputs.iter().min_by_key(|o| o.x).cloned()
            }
            Direction::Right => {
                // Find output with maximum x + width (rightmost)
                self.outputs.iter().max_by_key(|o| o.x + o.width as i32).cloned()
            }
            Direction::Up => {
                // Find output with minimum y (topmost)
                self.outputs.iter().min_by_key(|o| o.y).cloned()
            }
            Direction::Down => {
                // Find output with maximum y + height (bottommost)
                self.outputs.iter().max_by_key(|o| o.y + o.height as i32).cloned()
            }
        }
    }

    fn screen_bounds(&self) -> (i32, i32, i32, i32) {
        if self.outputs.is_empty() {
            return (0, 0, 1920, 1080); // Fallback
        }

        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        for output in &self.outputs {
            min_x = min_x.min(output.x);
            min_y = min_y.min(output.y);
            max_x = max_x.max(output.x + output.width as i32);
            max_y = max_y.max(output.y + output.height as i32);
        }

        (min_x, min_y, max_x, max_y)
    }

    fn draw_barrier(&mut self, barrier_idx: usize, _qh: &QueueHandle<Self>) {
        let barrier = &self.barriers[barrier_idx];
        if !barrier.configured {
            return;
        }

        let pool = match &mut self.pool {
            Some(pool) => pool,
            None => return,
        };

        let width = barrier.width;
        let height = barrier.height;
        let stride = width * 4;

        let (buffer, canvas) = pool
            .create_buffer(
                width as i32,
                height as i32,
                stride as i32,
                wl_shm::Format::Argb8888,
            )
            .expect("Failed to create buffer");

        // Fill with transparent pixels
        canvas.fill(0);

        // Attach and commit
        barrier.surface.wl_surface().attach(Some(buffer.wl_buffer()), 0, 0);
        barrier.surface.wl_surface().damage_buffer(0, 0, width as i32, height as i32);
        barrier.surface.commit();
    }

    fn find_barrier_by_surface(&self, surface: &wl_surface::WlSurface) -> Option<usize> {
        self.barriers
            .iter()
            .position(|b| b.surface.wl_surface() == surface)
    }
}

// Implement all the required handlers

impl CompositorHandler for EdgeCaptureState {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for EdgeCaptureState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // We'll get geometry info from the output state
        if let Some(info) = self.output_state.info(&output) {
            if let Some(size) = info.logical_size {
                let (x, y) = info.location;
                self.outputs.push(OutputInfo {
                    output,
                    width: size.0 as u32,
                    height: size.1 as u32,
                    x,
                    y,
                });
            }
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // Update output info
        if let Some(info) = self.output_state.info(&output) {
            if let Some(size) = info.logical_size {
                let (x, y) = info.location;
                if let Some(out) = self.outputs.iter_mut().find(|o| o.output == output) {
                    out.width = size.0 as u32;
                    out.height = size.1 as u32;
                    out.x = x;
                    out.y = y;
                }
            }
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        self.outputs.retain(|o| o.output != output);
    }
}

impl LayerShellHandler for EdgeCaptureState {
    fn closed(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _layer: &LayerSurface) {
        // Barrier closed, could recreate if needed
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        // Find which barrier this is
        if let Some(idx) = self.barriers.iter().position(|b| &b.surface == layer) {
            let barrier = &mut self.barriers[idx];
            barrier.configured = true;

            // Update size if compositor changed it
            if configure.new_size.0 > 0 {
                barrier.width = configure.new_size.0;
            }
            if configure.new_size.1 > 0 {
                barrier.height = configure.new_size.1;
            }

            // Draw the (transparent) barrier
            self.draw_barrier(idx, qh);
        }
    }
}

impl SeatHandler for EdgeCaptureState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            self.seat_state.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        _capability: Capability,
    ) {
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl PointerHandler for EdgeCaptureState {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        const DEBOUNCE_MS: u128 = 300; // Minimum time between triggers
        const EDGE_THRESHOLD: f64 = 2.0; // Position threshold for "at edge"

        for event in events {
            match &event.kind {
                PointerEventKind::Enter { .. } => {
                    // Pointer entered one of our barrier surfaces
                    if let Some(idx) = self.find_barrier_by_surface(&event.surface) {
                        self.active_barrier_idx = Some(idx);
                        self.last_barrier_pos = event.position;
                        tracing::debug!(
                            "Pointer entered {:?} barrier at ({}, {})",
                            self.barriers[idx].direction,
                            event.position.0,
                            event.position.1
                        );
                    }
                }
                PointerEventKind::Leave { .. } => {
                    if self.find_barrier_by_surface(&event.surface).is_some() {
                        self.active_barrier_idx = None;
                        tracing::debug!("Pointer left barrier");
                    }
                }
                PointerEventKind::Motion { .. } => {
                    // Check if we're on a barrier surface and detect edge-pushing motion
                    if let Some(idx) = self.active_barrier_idx {
                        let barrier = &self.barriers[idx];
                        let (x, y) = event.position;
                        let (last_x, last_y) = self.last_barrier_pos;

                        tracing::trace!(
                            "Motion on {:?} barrier: pos=({:.1}, {:.1}) last=({:.1}, {:.1}) size={}x{}",
                            barrier.direction,
                            x, y, last_x, last_y, barrier.width, barrier.height
                        );

                        // Calculate motion delta
                        let dx = x - last_x;
                        let dy = y - last_y;

                        // Check if motion is toward the edge while at the edge
                        let is_edge_push = match barrier.direction {
                            Direction::Left => x <= EDGE_THRESHOLD && dx < -0.1,
                            Direction::Right => x >= (barrier.width as f64 - EDGE_THRESHOLD) && dx > 0.1,
                            Direction::Up => y <= EDGE_THRESHOLD && dy < -0.1,
                            Direction::Down => y >= (barrier.height as f64 - EDGE_THRESHOLD) && dy > 0.1,
                        };

                        // Also trigger if cursor is stuck at edge position (x=0 or similar)
                        let is_at_edge = match barrier.direction {
                            Direction::Left => x < 1.0,
                            Direction::Right => x > (barrier.width as f64 - 1.0),
                            Direction::Up => y < 1.0,
                            Direction::Down => y > (barrier.height as f64 - 1.0),
                        };

                        if is_edge_push || is_at_edge {
                            // Check debounce
                            let now = Instant::now();
                            let should_trigger = self
                                .last_trigger_time
                                .get(&barrier.direction)
                                .map(|t| now.duration_since(*t).as_millis() >= DEBOUNCE_MS)
                                .unwrap_or(true);

                            if should_trigger {
                                self.last_trigger_time.insert(barrier.direction, now);

                                // Calculate screen position
                                let (min_x, min_y, max_x, max_y) = self.screen_bounds();
                                let screen_pos = match barrier.direction {
                                    Direction::Left => (min_x, min_y + y as i32),
                                    Direction::Right => (max_x - 1, min_y + y as i32),
                                    Direction::Up => (min_x + x as i32, min_y),
                                    Direction::Down => (min_x + x as i32, max_y - 1),
                                };

                                let edge_event = EdgeEvent {
                                    direction: barrier.direction,
                                    position: screen_pos,
                                    timestamp: now,
                                };

                                tracing::info!(
                                    "Edge trigger: {:?} at screen ({}, {})",
                                    barrier.direction,
                                    screen_pos.0,
                                    screen_pos.1
                                );

                                let _ = self.event_tx.send(edge_event);
                            }
                        }

                        self.last_barrier_pos = event.position;
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for EdgeCaptureState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl ProvidesRegistryState for EdgeCaptureState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers!(OutputState, SeatState);
}

// Delegate macros
delegate_compositor!(EdgeCaptureState);
delegate_output!(EdgeCaptureState);
delegate_layer!(EdgeCaptureState);
delegate_seat!(EdgeCaptureState);
delegate_pointer!(EdgeCaptureState);
delegate_shm!(EdgeCaptureState);
delegate_registry!(EdgeCaptureState);

/// Handle for controlling edge capture
pub struct EdgeCapture {
    event_rx: mpsc::Receiver<EdgeEvent>,
    // The event loop runs in a separate thread
    _thread: std::thread::JoinHandle<()>,
}

impl EdgeCapture {
    /// Create and start edge capture
    pub fn new(config: EdgeCaptureConfig) -> Result<Self, EdgeCaptureError> {
        let (event_tx, event_rx) = mpsc::channel();

        let thread = std::thread::spawn(move || {
            if let Err(e) = run_capture_loop(config, event_tx) {
                tracing::error!("Edge capture error: {}", e);
            }
        });

        Ok(Self {
            event_rx,
            _thread: thread,
        })
    }

    /// Try to receive an edge event (non-blocking)
    pub fn try_recv(&self) -> Option<EdgeEvent> {
        self.event_rx.try_recv().ok()
    }

    /// Receive an edge event (blocking)
    pub fn recv(&self) -> Option<EdgeEvent> {
        self.event_rx.recv().ok()
    }

    /// Get the receiver for async integration
    pub fn receiver(&self) -> &mpsc::Receiver<EdgeEvent> {
        &self.event_rx
    }
}

fn run_capture_loop(
    config: EdgeCaptureConfig,
    event_tx: mpsc::Sender<EdgeEvent>,
) -> Result<(), EdgeCaptureError> {
    let conn = Connection::connect_to_env().map_err(|e| EdgeCaptureError::Connection(e.to_string()))?;

    let (globals, mut event_queue) =
        registry_queue_init(&conn).map_err(|e| EdgeCaptureError::Registry(e.to_string()))?;
    let qh = event_queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)
        .map_err(|e| EdgeCaptureError::Protocol(format!("compositor: {}", e)))?;
    let layer_shell = LayerShell::bind(&globals, &qh)
        .map_err(|e| EdgeCaptureError::Protocol(format!("layer_shell: {}", e)))?;
    let shm_state =
        Shm::bind(&globals, &qh).map_err(|e| EdgeCaptureError::Protocol(format!("shm: {}", e)))?;

    let mut state = EdgeCaptureState {
        registry_state: RegistryState::new(&globals),
        compositor_state,
        output_state: OutputState::new(&globals, &qh),
        seat_state: SeatState::new(&globals, &qh),
        shm_state,
        layer_shell,
        barriers: Vec::new(),
        pool: None,
        event_tx,
        config,
        outputs: Vec::new(),
        active_barrier_idx: None,
        last_barrier_pos: (0.0, 0.0),
        last_trigger_time: std::collections::HashMap::new(),
    };

    // Initial roundtrip to get outputs
    event_queue.roundtrip(&mut state).map_err(|e| EdgeCaptureError::Dispatch(e.to_string()))?;

    // Create shm pool for buffers
    state.pool = Some(
        SlotPool::new(256 * 256 * 4, &state.shm_state)
            .map_err(|e| EdgeCaptureError::Shm(e.to_string()))?,
    );

    // Create barriers after we have output info
    state.create_barriers(&qh);

    // Another roundtrip to configure barriers
    event_queue.roundtrip(&mut state).map_err(|e| EdgeCaptureError::Dispatch(e.to_string()))?;

    tracing::info!("Edge capture running with {} barriers", state.barriers.len());

    // Main event loop
    loop {
        event_queue.blocking_dispatch(&mut state).map_err(|e| EdgeCaptureError::Dispatch(e.to_string()))?;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EdgeCaptureError {
    #[error("Failed to connect to Wayland: {0}")]
    Connection(String),

    #[error("Registry error: {0}")]
    Registry(String),

    #[error("Protocol not available: {0}")]
    Protocol(String),

    #[error("Dispatch error: {0}")]
    Dispatch(String),

    #[error("SHM error: {0}")]
    Shm(String),
}
