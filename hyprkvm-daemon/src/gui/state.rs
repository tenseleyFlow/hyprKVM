//! GUI state management

#![allow(dead_code)]

use hyprkvm_common::Direction;
use crate::config::{Config, NeighborConfig};
use std::path::PathBuf;

/// Grid position in the visual layout
/// Self machine is always at (0, 0)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct GridPos {
    pub x: i32,
    pub y: i32,
}

impl GridPos {
    pub fn origin() -> Self {
        Self { x: 0, y: 0 }
    }

    pub fn from_direction(direction: Direction) -> Self {
        match direction {
            Direction::Left => Self { x: -1, y: 0 },
            Direction::Right => Self { x: 1, y: 0 },
            Direction::Up => Self { x: 0, y: -1 },
            Direction::Down => Self { x: 0, y: 1 },
        }
    }

    pub fn to_direction(&self) -> Option<Direction> {
        match (self.x, self.y) {
            (-1, 0) => Some(Direction::Left),
            (1, 0) => Some(Direction::Right),
            (0, -1) => Some(Direction::Up),
            (0, 1) => Some(Direction::Down),
            _ => None, // Origin or invalid position
        }
    }

    /// Check if this is a valid neighbor position (adjacent to origin)
    pub fn is_valid_neighbor(&self) -> bool {
        self.to_direction().is_some()
    }
}

/// Connection status for a machine
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    #[default]
    Disconnected,
    Connecting,
    Connected,
}

/// Visual representation of a machine in the GUI
#[derive(Debug, Clone)]
pub struct MachineNode {
    /// Machine name
    pub name: String,
    /// Network address (empty for self)
    pub address: String,
    /// Position on the visual grid
    pub grid_pos: GridPos,
    /// Connection status
    pub status: ConnectionStatus,
    /// Is this the local machine?
    pub is_self: bool,
    /// Original neighbor index in config (None for self)
    pub config_index: Option<usize>,
}

impl MachineNode {
    pub fn new_self(name: String) -> Self {
        Self {
            name,
            address: String::new(),
            grid_pos: GridPos::origin(),
            status: ConnectionStatus::Connected, // Self is always connected
            is_self: true,
            config_index: None,
        }
    }

    pub fn new_neighbor(
        name: String,
        address: String,
        direction: Direction,
        config_index: usize,
    ) -> Self {
        Self {
            name,
            address,
            grid_pos: GridPos::from_direction(direction),
            status: ConnectionStatus::Disconnected,
            is_self: false,
            config_index: Some(config_index),
        }
    }
}

/// Drag state for machine repositioning
#[derive(Debug, Clone, Default)]
pub struct DragState {
    /// Index of machine being dragged (None if not dragging)
    pub dragging: Option<usize>,
    /// Current cursor position during drag
    pub cursor_pos: Option<iced::Point>,
    /// Snap target (if cursor is near a valid position)
    pub snap_target: Option<GridPos>,
}

impl DragState {
    pub fn is_dragging(&self) -> bool {
        self.dragging.is_some()
    }

    pub fn start(&mut self, machine_index: usize) {
        self.dragging = Some(machine_index);
        self.cursor_pos = None;
        self.snap_target = None;
    }

    pub fn update(&mut self, cursor: iced::Point) {
        self.cursor_pos = Some(cursor);
    }

    pub fn end(&mut self) -> Option<(usize, Option<GridPos>)> {
        let result = self.dragging.map(|idx| (idx, self.snap_target));
        self.dragging = None;
        self.cursor_pos = None;
        self.snap_target = None;
        result
    }
}

/// Main GUI state
#[derive(Debug)]
pub struct GuiState {
    /// Path to config file
    pub config_path: PathBuf,
    /// Current configuration
    pub config: Config,
    /// Machine nodes for rendering
    pub machines: Vec<MachineNode>,
    /// Drag state
    pub drag: DragState,
    /// Error message to display
    pub error: Option<String>,
    /// Success message to display (auto-dismisses)
    pub success: Option<String>,
    /// Whether daemon restart is needed (shows restart button)
    pub needs_restart: bool,
    /// Show add machine modal
    pub show_add_modal: bool,
    /// New machine form data
    pub new_machine: NewMachineForm,
}

/// Form data for adding a new machine
#[derive(Debug, Clone, Default)]
pub struct NewMachineForm {
    pub name: String,
    pub address: String,
}

impl GuiState {
    /// Create state from config
    pub fn from_config(config_path: PathBuf, config: Config) -> Self {
        let mut machines = Vec::new();

        // Add self machine at origin
        machines.push(MachineNode::new_self(config.machines.self_name.clone()));

        // Add neighbors at their positions
        for (idx, neighbor) in config.machines.neighbors.iter().enumerate() {
            machines.push(MachineNode::new_neighbor(
                neighbor.name.clone(),
                neighbor.address.to_string(),
                neighbor.direction,
                idx,
            ));
        }

        Self {
            config_path,
            config,
            machines,
            drag: DragState::default(),
            error: None,
            success: None,
            needs_restart: false,
            show_add_modal: false,
            new_machine: NewMachineForm::default(),
        }
    }

    /// Find machine at grid position
    pub fn machine_at(&self, pos: GridPos) -> Option<usize> {
        self.machines.iter().position(|m| m.grid_pos == pos)
    }

    /// Get available neighbor positions (not occupied, excluding dragged machine)
    pub fn available_positions(&self) -> Vec<GridPos> {
        let occupied: std::collections::HashSet<_> = self.machines
            .iter()
            .enumerate()
            .filter(|(idx, _)| Some(*idx) != self.drag.dragging) // Exclude dragged machine
            .map(|(_, m)| m.grid_pos)
            .collect();

        [
            GridPos { x: -1, y: 0 },
            GridPos { x: 1, y: 0 },
            GridPos { x: 0, y: -1 },
            GridPos { x: 0, y: 1 },
        ]
        .into_iter()
        .filter(|pos| !occupied.contains(pos))
        .collect()
    }

    /// Move a machine to a new position
    pub fn move_machine(&mut self, machine_idx: usize, new_pos: GridPos) -> bool {
        // Can't move self
        if self.machines[machine_idx].is_self {
            return false;
        }

        // Check if position is valid
        if !new_pos.is_valid_neighbor() {
            return false;
        }

        // Check if position is occupied by another machine (not the one being moved)
        if let Some(occupant_idx) = self.machine_at(new_pos) {
            if occupant_idx != machine_idx {
                return false;
            }
            // Same position - no need to update
            return true;
        }

        // Update the machine position
        self.machines[machine_idx].grid_pos = new_pos;

        // Update config
        if let Some(config_idx) = self.machines[machine_idx].config_index {
            if let Some(direction) = new_pos.to_direction() {
                self.config.machines.neighbors[config_idx].direction = direction;
            }
        }

        true
    }

    /// Add a new neighbor machine
    pub fn add_machine(&mut self, name: String, address: String, pos: GridPos) -> Result<(), String> {
        // Validate position
        let direction = pos.to_direction().ok_or("Invalid position")?;

        // Check if position is occupied
        if self.machine_at(pos).is_some() {
            return Err("Position already occupied".to_string());
        }

        // Parse address
        let socket_addr: std::net::SocketAddr = address
            .parse()
            .map_err(|e| format!("Invalid address: {}", e))?;

        // Add to config
        let config_idx = self.config.machines.neighbors.len();
        self.config.machines.neighbors.push(NeighborConfig {
            name: name.clone(),
            direction,
            address: socket_addr,
            fingerprint: None,
            tls: None,
        });

        // Add to machines list
        self.machines.push(MachineNode::new_neighbor(
            name,
            address,
            direction,
            config_idx,
        ));

        Ok(())
    }

    /// Remove a neighbor machine
    pub fn remove_machine(&mut self, machine_idx: usize) -> bool {
        // Can't remove self
        if machine_idx == 0 || self.machines[machine_idx].is_self {
            return false;
        }

        // Get config index before removal
        let config_idx = match self.machines[machine_idx].config_index {
            Some(idx) => idx,
            None => return false,
        };

        // Remove from machines
        self.machines.remove(machine_idx);

        // Remove from config
        self.config.machines.neighbors.remove(config_idx);

        // Update config indices for remaining machines
        for machine in &mut self.machines {
            if let Some(idx) = machine.config_index {
                if idx > config_idx {
                    machine.config_index = Some(idx - 1);
                }
            }
        }

        true
    }

    /// Save config to file
    pub fn save_config(&mut self) -> Result<(), String> {
        self.config
            .save(&self.config_path)
            .map_err(|e| e.to_string())
    }
}
