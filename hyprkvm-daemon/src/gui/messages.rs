//! GUI message types

use super::state::GridPos;

/// All messages that can be sent in the GUI
#[derive(Debug, Clone)]
pub enum Message {
    // Canvas interactions
    CanvasEvent(CanvasEvent),

    // Machine CRUD
    AddMachine,
    RemoveMachine(usize),
    CancelAddMachine,
    ConfirmAddMachine,

    // Form updates
    UpdateNewMachineName(String),
    UpdateNewMachineAddress(String),
    SelectNewMachinePosition(GridPos),

    // Config operations
    SaveConfig,
    ReloadDaemon,
    RestartDaemon,
    ConfigSaved(Result<(), String>),
    DaemonReloaded(Result<(), String>),

    // Status updates
    StatusUpdate(Vec<(String, super::state::ConnectionStatus)>),

    // Notifications
    ClearError,
    ClearSuccess,
}

/// Canvas-specific events
#[derive(Debug, Clone)]
pub enum CanvasEvent {
    /// Mouse pressed on a machine
    MachinePressed(usize),
    /// Mouse released (potentially on a snap target)
    MouseReleased,
    /// Mouse moved during drag, with calculated snap target
    MouseMoved(iced::Point, Option<super::state::GridPos>),
    /// Mouse entered canvas bounds
    MouseEntered,
    /// Mouse left canvas bounds
    MouseExited,
}
