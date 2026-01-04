//! Edge detection for keyboard navigation
//!
//! Determines when a workspace move would exit local boundaries.

use hyprkvm_common::Direction;

use super::layout::MonitorLayout;

/// Result of checking a keyboard move
#[derive(Debug, Clone)]
pub enum KeyboardEdgeResult {
    /// Move is possible within local Hyprland
    LocalMove,
    /// At edge, should trigger network switch
    NetworkEdge { direction: Direction },
}

/// Keyboard edge detector
pub struct KeyboardEdgeDetector {
    layout: MonitorLayout,
}

impl KeyboardEdgeDetector {
    /// Create a new detector with the given layout
    pub fn new(layout: MonitorLayout) -> Self {
        Self { layout }
    }

    /// Update the layout
    pub fn update_layout(&mut self, layout: MonitorLayout) {
        self.layout = layout;
    }

    /// Check if a move in the given direction would exit local boundaries
    pub fn check_move(&self, direction: Direction) -> KeyboardEdgeResult {
        // If there's a neighbor monitor in that direction, it's a local move
        if self.layout.neighbor(direction).is_some() {
            return KeyboardEdgeResult::LocalMove;
        }

        // No local neighbor - this is an edge
        KeyboardEdgeResult::NetworkEdge { direction }
    }

    /// Get reference to the layout
    pub fn layout(&self) -> &MonitorLayout {
        &self.layout
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hyprland::ipc::{Monitor, WorkspaceRef};

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
    fn test_single_monitor_all_edges() {
        let monitors = vec![make_monitor("DP-1", 0, 0, 1920, 1080, true)];
        let layout = MonitorLayout::from_monitors(&monitors);
        let detector = KeyboardEdgeDetector::new(layout);

        // All directions should be network edges
        assert!(matches!(
            detector.check_move(Direction::Left),
            KeyboardEdgeResult::NetworkEdge { direction: Direction::Left }
        ));
        assert!(matches!(
            detector.check_move(Direction::Right),
            KeyboardEdgeResult::NetworkEdge { direction: Direction::Right }
        ));
    }

    #[test]
    fn test_dual_monitor_horizontal() {
        let monitors = vec![
            make_monitor("DP-1", 0, 0, 1920, 1080, true),
            make_monitor("DP-2", 1920, 0, 1920, 1080, false),
        ];
        let layout = MonitorLayout::from_monitors(&monitors);
        let detector = KeyboardEdgeDetector::new(layout);

        // Left should be network edge, right should be local
        assert!(matches!(
            detector.check_move(Direction::Left),
            KeyboardEdgeResult::NetworkEdge { .. }
        ));
        assert!(matches!(
            detector.check_move(Direction::Right),
            KeyboardEdgeResult::LocalMove
        ));
    }
}
