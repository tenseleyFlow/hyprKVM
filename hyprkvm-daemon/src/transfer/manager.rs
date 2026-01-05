//! Transfer manager - orchestrates control handoff between machines

#![allow(dead_code)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{mpsc, RwLock};

use hyprkvm_common::protocol::{
    CursorEntryPos, EnterAckPayload, EnterPayload, LeavePayload, Message,
};
use hyprkvm_common::{Direction, ModifierState};

/// Transfer state machine
#[derive(Debug, Clone)]
pub enum TransferState {
    /// Normal operation, we have control
    Local,

    /// Transfer initiated, waiting for ack
    Initiating {
        target: Direction,
        transfer_id: u64,
        started_at: Instant,
        /// True if transfer was triggered via keyboard (Super+Arrow)
        keyboard_initiated: bool,
    },

    /// We sent control away, forwarding input
    RemoteActive {
        target: Direction,
        transfer_id: u64,
        entered_at: Instant,
        /// True if transfer was triggered via keyboard (Super+Arrow)
        keyboard_initiated: bool,
    },

    /// We received control from another machine
    ReceivedControl {
        from: Direction,
        transfer_id: u64,
        entered_at: Instant,
    },
}

impl TransferState {
    pub fn is_local(&self) -> bool {
        matches!(self, TransferState::Local)
    }

    pub fn is_remote_active(&self) -> bool {
        matches!(self, TransferState::RemoteActive { .. })
    }

    pub fn is_receiving(&self) -> bool {
        matches!(self, TransferState::ReceivedControl { .. })
    }
}

/// Events from the transfer manager
#[derive(Debug, Clone)]
pub enum TransferEvent {
    /// Start capturing and forwarding input
    /// `keyboard_initiated` is true if the transfer was triggered via keyboard (Super+Arrow),
    /// false if triggered via CLI or other non-keyboard means
    StartCapture { direction: Direction, keyboard_initiated: bool },
    /// Stop capturing, return to local
    StopCapture,
    /// Start injecting received input
    StartInjection { from: Direction },
    /// Stop injecting
    StopInjection,
    /// Send a message to a peer
    SendMessage { direction: Direction, message: Message },
    /// Sync clipboard to remote machine
    SyncClipboardOutgoing { direction: Direction },
}

/// Manages control transfer between machines
pub struct TransferManager {
    state: RwLock<TransferState>,
    transfer_id_counter: AtomicU64,
    event_tx: mpsc::Sender<TransferEvent>,
    machine_name: String,
}

impl TransferManager {
    pub fn new(machine_name: String) -> (Self, mpsc::Receiver<TransferEvent>) {
        let (event_tx, event_rx) = mpsc::channel(32);

        (
            Self {
                state: RwLock::new(TransferState::Local),
                transfer_id_counter: AtomicU64::new(1),
                event_tx,
                machine_name,
            },
            event_rx,
        )
    }

    fn next_transfer_id(&self) -> u64 {
        self.transfer_id_counter.fetch_add(1, Ordering::Relaxed)
    }

    /// Get current state
    pub async fn state(&self) -> TransferState {
        self.state.read().await.clone()
    }

    /// Initiate transfer to a direction (mouse or keyboard edge hit)
    /// `keyboard_initiated` should be true if this was triggered via Super+Arrow keybind,
    /// false if triggered via CLI, mouse edge, or other non-keyboard means
    pub async fn initiate_transfer(
        &self,
        direction: Direction,
        cursor_pos: (i32, i32),
        screen_min_x: i32,
        screen_min_y: i32,
        screen_max_x: i32,
        screen_max_y: i32,
        keyboard_initiated: bool,
    ) -> Result<(), TransferError> {
        let mut state = self.state.write().await;

        // Only transfer from local or received state
        match &*state {
            TransferState::Local | TransferState::ReceivedControl { .. } => {}
            TransferState::Initiating { .. } => {
                return Err(TransferError::AlreadyTransferring);
            }
            TransferState::RemoteActive { .. } => {
                return Err(TransferError::InvalidState(
                    "Already in remote active state".to_string(),
                ));
            }
        }

        let transfer_id = self.next_transfer_id();

        // Calculate edge-relative cursor position (0.0-1.0 along the edge)
        let screen_width = (screen_max_x - screen_min_x) as f64;
        let screen_height = (screen_max_y - screen_min_y) as f64;

        let edge_relative = match direction {
            Direction::Left | Direction::Right => {
                // Y position relative to screen height
                (cursor_pos.1 - screen_min_y) as f64 / screen_height
            }
            Direction::Up | Direction::Down => {
                // X position relative to screen width
                (cursor_pos.0 - screen_min_x) as f64 / screen_width
            }
        }.clamp(0.0, 1.0); // Ensure within valid range

        tracing::info!(
            "Initiating transfer to {:?}, transfer_id={}",
            direction,
            transfer_id
        );

        // Update state
        *state = TransferState::Initiating {
            target: direction,
            transfer_id,
            started_at: Instant::now(),
            keyboard_initiated,
        };

        // Send Enter message
        let enter = Message::Enter(EnterPayload {
            from_direction: direction.opposite(),
            cursor_pos: CursorEntryPos::EdgeRelative(edge_relative),
            modifiers: ModifierState::default(), // TODO: get actual modifier state
            transfer_id,
        });

        self.event_tx
            .send(TransferEvent::SendMessage {
                direction,
                message: enter,
            })
            .await
            .map_err(|_| TransferError::ChannelClosed)?;

        Ok(())
    }

    /// Handle EnterAck from remote
    pub async fn handle_enter_ack(&self, ack: EnterAckPayload) -> Result<(), TransferError> {
        let mut state = self.state.write().await;

        match &*state {
            TransferState::Initiating {
                target,
                transfer_id,
                keyboard_initiated,
                ..
            } => {
                if *transfer_id != ack.transfer_id {
                    tracing::warn!(
                        "EnterAck transfer_id mismatch: expected {}, got {}",
                        transfer_id,
                        ack.transfer_id
                    );
                    return Err(TransferError::TransferIdMismatch);
                }

                if !ack.success {
                    tracing::warn!("EnterAck rejected: {:?}", ack.error);
                    *state = TransferState::Local;
                    return Err(TransferError::Rejected(
                        ack.error.unwrap_or_else(|| "Unknown".to_string()),
                    ));
                }

                tracing::info!(
                    "Transfer accepted, cursor at {:?}, keyboard_initiated={}",
                    ack.actual_cursor_pos,
                    keyboard_initiated
                );

                let direction = *target;
                let tid = *transfer_id;
                let kbd_init = *keyboard_initiated;

                *state = TransferState::RemoteActive {
                    target: direction,
                    transfer_id: tid,
                    entered_at: Instant::now(),
                    keyboard_initiated: kbd_init,
                };

                // Start capturing input
                self.event_tx
                    .send(TransferEvent::StartCapture { direction, keyboard_initiated: kbd_init })
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                // Trigger clipboard sync (if enabled, handled by main loop)
                self.event_tx
                    .send(TransferEvent::SyncClipboardOutgoing { direction })
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                Ok(())
            }
            _ => Err(TransferError::InvalidState(
                "Not in Initiating state".to_string(),
            )),
        }
    }

    /// Handle incoming Enter from another machine
    pub async fn handle_enter(
        &self,
        from_direction: Direction,
        payload: EnterPayload,
        screen_min_x: i32,
        screen_min_y: i32,
        screen_max_x: i32,
        screen_max_y: i32,
    ) -> Result<(i32, i32), TransferError> {
        let mut state = self.state.write().await;

        // Validate current state - we can only receive Enter if we're Local or ReceivedControl
        match &*state {
            TransferState::Local => {
                // Normal case - we're idle, ready to receive control
            }
            TransferState::ReceivedControl { .. } => {
                // Already receiving, this is a re-entry - accept it
                tracing::info!("Re-receiving control (was already in ReceivedControl)");
            }
            TransferState::Initiating { .. } => {
                // We're trying to send control, but they're also trying to send to us
                // This is a collision - let them win (accept their Enter)
                tracing::debug!("Enter collision: we were Initiating, accepting their Enter");
            }
            TransferState::RemoteActive { .. } => {
                // We're forwarding to them, but they're sending control back to us
                // This shouldn't happen normally - they should send Leave, not Enter
                tracing::warn!("Received Enter while in RemoteActive - unusual but accepting");
            }
        }

        // Calculate actual cursor position using proper screen bounds
        let screen_width = (screen_max_x - screen_min_x) as f64;
        let screen_height = (screen_max_y - screen_min_y) as f64;

        let cursor_pos = match payload.cursor_pos {
            CursorEntryPos::EdgeRelative(rel) => {
                // from_direction indicates which edge the cursor enters from
                // e.g., from_direction=Left means cursor enters at our left edge
                match from_direction {
                    Direction::Left => {
                        // Cursor enters from left edge
                        let y = screen_min_y + (rel * screen_height) as i32;
                        (screen_min_x, y)
                    }
                    Direction::Right => {
                        // Cursor enters from right edge
                        let y = screen_min_y + (rel * screen_height) as i32;
                        (screen_max_x - 1, y)
                    }
                    Direction::Up => {
                        // Cursor enters from top edge
                        let x = screen_min_x + (rel * screen_width) as i32;
                        (x, screen_min_y)
                    }
                    Direction::Down => {
                        // Cursor enters from bottom edge
                        let x = screen_min_x + (rel * screen_width) as i32;
                        (x, screen_max_y - 1)
                    }
                }
            }
            CursorEntryPos::Absolute { x, y } => (x, y),
        };

        tracing::info!(
            "Receiving control from {:?}, cursor at ({}, {})",
            from_direction,
            cursor_pos.0,
            cursor_pos.1
        );

        *state = TransferState::ReceivedControl {
            from: from_direction,
            transfer_id: payload.transfer_id,
            entered_at: Instant::now(),
        };

        // Start injection mode
        self.event_tx
            .send(TransferEvent::StartInjection {
                from: from_direction,
            })
            .await
            .map_err(|_| TransferError::ChannelClosed)?;

        // Send ack
        let ack = Message::EnterAck(EnterAckPayload {
            success: true,
            transfer_id: payload.transfer_id,
            actual_cursor_pos: Some(cursor_pos),
            error: None,
        });

        self.event_tx
            .send(TransferEvent::SendMessage {
                direction: from_direction,
                message: ack,
            })
            .await
            .map_err(|_| TransferError::ChannelClosed)?;

        Ok(cursor_pos)
    }

    /// Return control to sender (escape hotkey or reverse edge)
    pub async fn return_control(&self) -> Result<(), TransferError> {
        let mut state = self.state.write().await;

        match &*state {
            TransferState::ReceivedControl {
                from, transfer_id, ..
            } => {
                let direction = *from;
                let tid = *transfer_id;

                tracing::info!("Returning control to {:?}", direction);

                // Trigger clipboard sync before leaving (if enabled, handled by main loop)
                self.event_tx
                    .send(TransferEvent::SyncClipboardOutgoing { direction })
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                // Send Leave message
                let leave = Message::Leave(LeavePayload {
                    to_direction: direction,
                    cursor_pos: CursorEntryPos::EdgeRelative(0.5), // Center for now
                    modifiers: ModifierState::default(),
                    transfer_id: tid,
                });

                self.event_tx
                    .send(TransferEvent::SendMessage {
                        direction,
                        message: leave,
                    })
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                // Stop injection
                self.event_tx
                    .send(TransferEvent::StopInjection)
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                *state = TransferState::Local;
                Ok(())
            }
            _ => Err(TransferError::InvalidState(
                "Not receiving control".to_string(),
            )),
        }
    }

    /// Handle incoming Leave (control returning to us)
    pub async fn handle_leave(&self, payload: LeavePayload) -> Result<(), TransferError> {
        let mut state = self.state.write().await;

        match &*state {
            TransferState::RemoteActive { transfer_id, .. } => {
                if *transfer_id != payload.transfer_id {
                    tracing::warn!("Leave transfer_id mismatch");
                }

                tracing::info!("Control returned to local");

                // Stop capturing
                self.event_tx
                    .send(TransferEvent::StopCapture)
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                // Send LeaveAck
                let direction = payload.to_direction.opposite();
                self.event_tx
                    .send(TransferEvent::SendMessage {
                        direction,
                        message: Message::LeaveAck,
                    })
                    .await
                    .map_err(|_| TransferError::ChannelClosed)?;

                *state = TransferState::Local;
                Ok(())
            }
            _ => Err(TransferError::InvalidState(
                "Not in RemoteActive state".to_string(),
            )),
        }
    }

    /// Abort any pending transfer (timeout, error)
    pub async fn abort(&self) {
        let mut state = self.state.write().await;

        match &*state {
            TransferState::Initiating { .. } => {
                tracing::warn!("Aborting pending transfer");
                *state = TransferState::Local;
            }
            TransferState::RemoteActive { .. } => {
                tracing::warn!("Aborting remote active state");
                let _ = self.event_tx.send(TransferEvent::StopCapture).await;
                *state = TransferState::Local;
            }
            TransferState::ReceivedControl { .. } => {
                tracing::warn!("Aborting received control state");
                let _ = self.event_tx.send(TransferEvent::StopInjection).await;
                *state = TransferState::Local;
            }
            TransferState::Local => {}
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransferError {
    #[error("Already transferring")]
    AlreadyTransferring,

    #[error("Invalid state: {0}")]
    InvalidState(String),

    #[error("Transfer ID mismatch")]
    TransferIdMismatch,

    #[error("Transfer rejected: {0}")]
    Rejected(String),

    #[error("Channel closed")]
    ChannelClosed,

}
