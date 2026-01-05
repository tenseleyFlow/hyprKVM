//! HyprKVM GUI module
//!
//! Provides a visual interface for configuring machine neighbors.

mod app;
mod canvas;
mod messages;
mod state;
mod theme;

use std::path::Path;

use app::HyprKvmGui;

/// Run the GUI application
pub fn run_gui(config_path: &Path) -> anyhow::Result<()> {
    let config_path = config_path.to_path_buf();

    iced::application("HyprKVM", HyprKvmGui::update, HyprKvmGui::view)
        .window_size(iced::Size::new(800.0, 600.0))
        .antialiasing(true)
        .run_with(move || HyprKvmGui::new(config_path.clone()))
        .map_err(|e| anyhow::anyhow!("GUI error: {}", e))
}
