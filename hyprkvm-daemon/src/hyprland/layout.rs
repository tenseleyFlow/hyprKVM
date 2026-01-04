//! Monitor layout management
//!
//! Tracks monitor geometry and calculates adjacency relationships.

use std::collections::HashMap;

use hyprkvm_common::Direction;

use super::ipc::{HyprlandClient, HyprlandError, Monitor};

/// Manages the monitor layout
pub struct MonitorLayout {
    monitors: HashMap<String, MonitorInfo>,
}

/// Monitor information with computed adjacency
#[derive(Debug, Clone)]
pub struct MonitorInfo {
    pub name: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    pub active_workspace_id: i32,
    pub active_workspace_name: String,
    pub focused: bool,
    pub adjacency: Adjacency,
}

/// Adjacent monitors in each direction
#[derive(Debug, Clone, Default)]
pub struct Adjacency {
    pub left: Option<String>,
    pub right: Option<String>,
    pub up: Option<String>,
    pub down: Option<String>,
}

impl MonitorLayout {
    /// Build layout from current Hyprland state
    pub async fn from_hyprland(client: &HyprlandClient) -> Result<Self, HyprlandError> {
        let monitors = client.monitors().await?;
        Ok(Self::from_monitors(&monitors))
    }

    /// Build layout from monitor list
    pub fn from_monitors(monitors: &[Monitor]) -> Self {
        let mut layout = Self {
            monitors: HashMap::new(),
        };

        // First pass: add all monitors
        for mon in monitors {
            layout.monitors.insert(
                mon.name.clone(),
                MonitorInfo {
                    name: mon.name.clone(),
                    x: mon.x,
                    y: mon.y,
                    width: mon.width,
                    height: mon.height,
                    scale: mon.scale,
                    active_workspace_id: mon.active_workspace.id,
                    active_workspace_name: mon.active_workspace.name.clone(),
                    focused: mon.focused,
                    adjacency: Adjacency::default(),
                },
            );
        }

        // Second pass: compute adjacency
        let names: Vec<String> = layout.monitors.keys().cloned().collect();
        for name in &names {
            let adjacency = layout.compute_adjacency(name);
            if let Some(mon) = layout.monitors.get_mut(name) {
                mon.adjacency = adjacency;
            }
        }

        layout
    }

    /// Compute adjacency for a monitor
    fn compute_adjacency(&self, name: &str) -> Adjacency {
        let Some(current) = self.monitors.get(name) else {
            return Adjacency::default();
        };

        let mut adjacency = Adjacency::default();

        for (other_name, other) in &self.monitors {
            if other_name == name {
                continue;
            }

            // Check if monitors are vertically aligned (overlap in Y axis)
            let v_overlap = current.y < other.y + other.height as i32
                && current.y + current.height as i32 > other.y;

            // Check if monitors are horizontally aligned (overlap in X axis)
            let h_overlap = current.x < other.x + other.width as i32
                && current.x + current.width as i32 > other.x;

            // Left neighbor: other is to the left and vertically aligned
            if v_overlap && other.x + other.width as i32 <= current.x {
                if adjacency.left.is_none()
                    || self.is_closer_left(current, other, &adjacency.left)
                {
                    adjacency.left = Some(other_name.clone());
                }
            }

            // Right neighbor: other is to the right and vertically aligned
            if v_overlap && other.x >= current.x + current.width as i32 {
                if adjacency.right.is_none()
                    || self.is_closer_right(current, other, &adjacency.right)
                {
                    adjacency.right = Some(other_name.clone());
                }
            }

            // Up neighbor: other is above and horizontally aligned
            if h_overlap && other.y + other.height as i32 <= current.y {
                if adjacency.up.is_none()
                    || self.is_closer_up(current, other, &adjacency.up)
                {
                    adjacency.up = Some(other_name.clone());
                }
            }

            // Down neighbor: other is below and horizontally aligned
            if h_overlap && other.y >= current.y + current.height as i32 {
                if adjacency.down.is_none()
                    || self.is_closer_down(current, other, &adjacency.down)
                {
                    adjacency.down = Some(other_name.clone());
                }
            }
        }

        adjacency
    }

    // Helper functions to find closest neighbor
    fn is_closer_left(&self, _current: &MonitorInfo, other: &MonitorInfo, existing: &Option<String>) -> bool {
        let Some(existing_name) = existing else { return true };
        let Some(existing_mon) = self.monitors.get(existing_name) else { return true };
        other.x + other.width as i32 > existing_mon.x + existing_mon.width as i32
    }

    fn is_closer_right(&self, _current: &MonitorInfo, other: &MonitorInfo, existing: &Option<String>) -> bool {
        let Some(existing_name) = existing else { return true };
        let Some(existing_mon) = self.monitors.get(existing_name) else { return true };
        other.x < existing_mon.x
    }

    fn is_closer_up(&self, _current: &MonitorInfo, other: &MonitorInfo, existing: &Option<String>) -> bool {
        let Some(existing_name) = existing else { return true };
        let Some(existing_mon) = self.monitors.get(existing_name) else { return true };
        other.y + other.height as i32 > existing_mon.y + existing_mon.height as i32
    }

    fn is_closer_down(&self, _current: &MonitorInfo, other: &MonitorInfo, existing: &Option<String>) -> bool {
        let Some(existing_name) = existing else { return true };
        let Some(existing_mon) = self.monitors.get(existing_name) else { return true };
        other.y < existing_mon.y
    }

    /// Get the focused monitor
    pub fn focused(&self) -> Option<&MonitorInfo> {
        self.monitors.values().find(|m| m.focused)
    }

    /// Get a monitor by name
    pub fn get(&self, name: &str) -> Option<&MonitorInfo> {
        self.monitors.get(name)
    }

    /// Get monitor in given direction from focused monitor
    pub fn neighbor(&self, direction: Direction) -> Option<&MonitorInfo> {
        let focused = self.focused()?;
        let neighbor_name = match direction {
            Direction::Left => focused.adjacency.left.as_ref()?,
            Direction::Right => focused.adjacency.right.as_ref()?,
            Direction::Up => focused.adjacency.up.as_ref()?,
            Direction::Down => focused.adjacency.down.as_ref()?,
        };
        self.monitors.get(neighbor_name)
    }

    /// Check if we're at an edge (no local monitor in that direction)
    pub fn is_at_edge(&self, direction: Direction) -> bool {
        self.neighbor(direction).is_none()
    }

    /// Get screen bounds (bounding box of all monitors)
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        let mut min_x = i32::MAX;
        let mut min_y = i32::MAX;
        let mut max_x = i32::MIN;
        let mut max_y = i32::MIN;

        for mon in self.monitors.values() {
            min_x = min_x.min(mon.x);
            min_y = min_y.min(mon.y);
            max_x = max_x.max(mon.x + mon.width as i32);
            max_y = max_y.max(mon.y + mon.height as i32);
        }

        (min_x, min_y, max_x, max_y)
    }

    /// Check if a cursor position is at a screen edge
    pub fn cursor_at_edge(&self, x: i32, y: i32, threshold: i32) -> Option<Direction> {
        let (min_x, min_y, max_x, max_y) = self.bounds();

        if x <= min_x + threshold {
            Some(Direction::Left)
        } else if x >= max_x - threshold {
            Some(Direction::Right)
        } else if y <= min_y + threshold {
            Some(Direction::Up)
        } else if y >= max_y - threshold {
            Some(Direction::Down)
        } else {
            None
        }
    }

    /// Get all monitor names
    pub fn monitor_names(&self) -> impl Iterator<Item = &String> {
        self.monitors.keys()
    }

    /// Get number of monitors
    pub fn len(&self) -> usize {
        self.monitors.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.monitors.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyprland::ipc::WorkspaceRef;

    fn make_monitor(name: &str, x: i32, y: i32, width: u32, height: u32, focused: bool) -> Monitor {
        Monitor {
            id: 0,
            name: name.to_string(),
            description: String::new(),
            x,
            y,
            width,
            height,
            scale: 1.0,
            active_workspace: WorkspaceRef {
                id: 1,
                name: "1".to_string(),
            },
            focused,
        }
    }

    #[test]
    fn test_horizontal_layout() {
        let monitors = vec![
            make_monitor("DP-1", 0, 0, 1920, 1080, true),
            make_monitor("DP-2", 1920, 0, 1920, 1080, false),
        ];

        let layout = MonitorLayout::from_monitors(&monitors);

        let dp1 = layout.get("DP-1").unwrap();
        assert!(dp1.adjacency.left.is_none());
        assert_eq!(dp1.adjacency.right, Some("DP-2".to_string()));

        let dp2 = layout.get("DP-2").unwrap();
        assert_eq!(dp2.adjacency.left, Some("DP-1".to_string()));
        assert!(dp2.adjacency.right.is_none());
    }

    #[test]
    fn test_edge_detection() {
        let monitors = vec![
            make_monitor("DP-1", 0, 0, 1920, 1080, true),
        ];

        let layout = MonitorLayout::from_monitors(&monitors);

        assert!(layout.is_at_edge(Direction::Left));
        assert!(layout.is_at_edge(Direction::Right));
        assert!(layout.is_at_edge(Direction::Up));
        assert!(layout.is_at_edge(Direction::Down));
    }
}
