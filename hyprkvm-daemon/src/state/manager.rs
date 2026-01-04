//! State manager module
//!
//! Single source of truth for all runtime state.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::RwLock;

use hyprkvm_common::Direction;

use crate::config::Config;
use crate::hyprland::ipc::HyprlandClient;
use crate::hyprland::layout::MonitorLayout;
use crate::input::EdgeEvent;

/// The current control state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ControlState {
    /// This machine has keyboard/mouse control
    Local,
    /// Control transferred to another machine
    Remote { machine: String },
}

/// What triggered an edge detection
#[derive(Debug, Clone)]
pub enum EdgeTrigger {
    /// Mouse moved to screen edge
    Mouse {
        direction: Direction,
        position: (i32, i32),
    },
    /// Keyboard navigation at workspace boundary
    Keyboard { direction: Direction },
}

/// Information about a neighbor machine
#[derive(Debug, Clone)]
pub struct NeighborInfo {
    pub name: String,
    pub address: SocketAddr,
    pub direction: Direction,
}

/// Unified state manager for the daemon
pub struct StateManager {
    /// Hyprland client for queries
    hyprland: HyprlandClient,
    /// Current monitor layout
    layout: RwLock<MonitorLayout>,
    /// Daemon configuration
    config: Config,
    /// Whether we have local control
    has_local_control: AtomicBool,
    /// Current remote machine (if control is remote)
    current_remote: RwLock<Option<String>>,
}

impl StateManager {
    /// Create a new state manager
    pub async fn new(config: Config, hyprland: HyprlandClient) -> Result<Self, StateError> {
        // Get initial monitor layout
        let monitors = hyprland
            .monitors()
            .await
            .map_err(|e| StateError::Hyprland(e.to_string()))?;

        let layout = MonitorLayout::from_monitors(&monitors);

        tracing::info!(
            "Initialized state manager with {} monitors",
            monitors.len()
        );

        Ok(Self {
            hyprland,
            layout: RwLock::new(layout),
            config,
            has_local_control: AtomicBool::new(true),
            current_remote: RwLock::new(None),
        })
    }

    /// Get current control state
    pub async fn control_state(&self) -> ControlState {
        if self.has_local_control.load(Ordering::Relaxed) {
            ControlState::Local
        } else {
            let remote = self.current_remote.read().await;
            match remote.as_ref() {
                Some(name) => ControlState::Remote {
                    machine: name.clone(),
                },
                None => ControlState::Local,
            }
        }
    }

    /// Check if an edge trigger should cause a network switch
    pub async fn should_network_switch(&self, trigger: &EdgeTrigger) -> Option<NeighborInfo> {
        let direction = match trigger {
            EdgeTrigger::Mouse { direction, .. } => direction,
            EdgeTrigger::Keyboard { direction } => direction,
        };

        // Check if we have a neighbor in this direction
        self.get_neighbor(*direction).await
    }

    /// Get neighbor machine in the given direction
    pub async fn get_neighbor(&self, direction: Direction) -> Option<NeighborInfo> {
        let layout = self.layout.read().await;

        // First check if we're at the edge of our local monitors
        if !layout.is_at_edge(direction) {
            tracing::debug!(
                "Not at local edge for direction {:?}, no network switch",
                direction
            );
            return None;
        }

        // Check if we have a configured neighbor in this direction
        self.config
            .machines
            .neighbors
            .iter()
            .find(|n| n.direction == direction)
            .map(|n| NeighborInfo {
                name: n.name.clone(),
                address: n.address,
                direction: n.direction,
            })
    }

    /// Set control as transferred to a remote machine
    pub async fn set_control_remote(&self, machine: &str) {
        tracing::info!("Control transferred to: {}", machine);
        self.has_local_control.store(false, Ordering::Relaxed);
        let mut remote = self.current_remote.write().await;
        *remote = Some(machine.to_string());
    }

    /// Set control as returned to local
    pub async fn set_control_local(&self) {
        tracing::info!("Control returned to local");
        self.has_local_control.store(true, Ordering::Relaxed);
        let mut remote = self.current_remote.write().await;
        *remote = None;
    }

    /// Refresh monitor layout from Hyprland
    pub async fn refresh_layout(&self) -> Result<(), StateError> {
        let monitors = self
            .hyprland
            .monitors()
            .await
            .map_err(|e| StateError::Hyprland(e.to_string()))?;

        let new_layout = MonitorLayout::from_monitors(&monitors);

        let mut layout = self.layout.write().await;
        *layout = new_layout;

        tracing::debug!("Refreshed monitor layout: {} monitors", monitors.len());
        Ok(())
    }

    /// Get a read lock on the current layout
    pub async fn layout(&self) -> tokio::sync::RwLockReadGuard<'_, MonitorLayout> {
        self.layout.read().await
    }

    /// Get the configured edges that have network neighbors
    pub fn network_edge_directions(&self) -> Vec<Direction> {
        self.config
            .machines
            .neighbors
            .iter()
            .map(|n| n.direction)
            .collect()
    }

    /// Process an edge event and determine if action needed
    pub async fn process_edge_event(&self, event: EdgeEvent) -> Option<NeighborInfo> {
        let trigger = EdgeTrigger::Mouse {
            direction: event.direction,
            position: event.position,
        };

        if let Some(neighbor) = self.should_network_switch(&trigger).await {
            tracing::info!(
                "Edge event {:?} should switch to {}",
                event.direction,
                neighbor.name
            );
            Some(neighbor)
        } else {
            tracing::debug!(
                "Edge event {:?} has no network neighbor",
                event.direction
            );
            None
        }
    }

    /// Process a keyboard move and determine if action needed
    pub async fn process_keyboard_move(&self, direction: Direction) -> Option<NeighborInfo> {
        let trigger = EdgeTrigger::Keyboard { direction };

        if let Some(neighbor) = self.should_network_switch(&trigger).await {
            tracing::info!(
                "Keyboard move {:?} should switch to {}",
                direction,
                neighbor.name
            );
            Some(neighbor)
        } else {
            tracing::debug!("Keyboard move {:?} has no network neighbor", direction);
            None
        }
    }

    /// Get machine name
    pub fn machine_name(&self) -> &str {
        &self.config.machines.self_name
    }

    /// Get the config reference
    pub fn config(&self) -> &Config {
        &self.config
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("Hyprland error: {0}")]
    Hyprland(String),

    #[error("Configuration error: {0}")]
    Config(String),
}
