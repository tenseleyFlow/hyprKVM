//! Color theme constants for HyprKVM GUI

#![allow(dead_code)]

use iced::Color;

/// Warm off-white background
pub const BACKGROUND: Color = Color::from_rgb(0.961, 0.949, 0.941); // #F5F2F0

/// Rich purple for neighbor machines
pub const MACHINE_NEIGHBOR: Color = Color::from_rgb(0.549, 0.361, 0.820); // #8C5CD1

/// Darker purple for self machine
pub const MACHINE_SELF: Color = Color::from_rgb(0.439, 0.220, 0.722); // #7038B8

/// Green for connected status
pub const STATUS_CONNECTED: Color = Color::from_rgb(0.302, 0.690, 0.310); // #4DB04F

/// Amber for connecting status
pub const STATUS_CONNECTING: Color = Color::from_rgb(1.0, 0.761, 0.031); // #FFC208

/// Red for disconnected status
pub const STATUS_DISCONNECTED: Color = Color::from_rgb(0.914, 0.302, 0.239); // #E94D3D

/// Snap highlight (purple at 30% opacity)
pub const SNAP_HIGHLIGHT: Color = Color::from_rgba(0.549, 0.361, 0.820, 0.3);

/// Text color (dark gray)
pub const TEXT_PRIMARY: Color = Color::from_rgb(0.2, 0.2, 0.2);

/// Text color (light, for on purple backgrounds)
pub const TEXT_LIGHT: Color = Color::from_rgb(1.0, 1.0, 1.0);

/// Grid line color (light gray)
pub const GRID_LINE: Color = Color::from_rgba(0.6, 0.6, 0.6, 0.3);

/// Machine rectangle dimensions
pub const MACHINE_WIDTH: f32 = 160.0;
pub const MACHINE_HEIGHT: f32 = 100.0;

/// Gap between machines (same in both directions for equidistant spacing)
pub const MACHINE_GAP: f32 = 24.0;

/// Grid cell spacing (horizontal: width + gap, vertical: height + gap)
pub const GRID_SPACING_H: f32 = MACHINE_WIDTH + MACHINE_GAP;   // 184
pub const GRID_SPACING_V: f32 = MACHINE_HEIGHT + MACHINE_GAP;  // 124

/// Snap radius for drag operations
pub const SNAP_RADIUS: f32 = 50.0;

/// Connection dot radius
pub const CONNECTION_DOT_RADIUS: f32 = 4.0;

/// Connection dot color (subtle gray)
pub const CONNECTION_DOT: Color = Color::from_rgba(0.5, 0.5, 0.5, 0.5);
