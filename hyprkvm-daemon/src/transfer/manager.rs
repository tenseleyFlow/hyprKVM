//! Transfer manager - orchestrates control handoff between machines

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    },

    /// We sent control away, forwarding input
    RemoteActive {
        target: Direction,
        transfer_id: u64,
        entered_at: Instant,
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
    StartCapture { direction: Direction },
    /// Stop capturing, return to local
    StopCapture,
    /// Start injecting received input
    StartInjection { from: Direction },
    /// Stop injecting
    StopInjection,
    /// Send a message to a peer
    SendMessage { direction: Direction, message: Message },
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
    pub async fn initiate_transfer(
        &self,
        direction: Direction,
        cursor_pos: (i32, i32),
        screen_height: u32,
        screen_width: u32,
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

        // Calculate edge-relative cursor position
        let edge_relative = match direction {
            Direction::Left | Direction::Right => {
                cursor_pos.1 as f64 / screen_height as f64
            }
            Direction::Up | Direction::Down => {
                cursor_pos.0 as f64 / screen_width as f64
            }
        };

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
                    "Transfer accepted, cursor at {:?}",
                    ack.actual_cursor_pos
                );

                let direction = *target;
                let tid = *transfer_id;

                *state = TransferState::RemoteActive {
                    target: direction,
                    transfer_id: tid,
                    entered_at: Instant::now(),
                };

                // Start capturing input
                self.event_tx
                    .send(TransferEvent::StartCapture { direction })
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
        screen_width: u32,
        screen_height: u32,
    ) -> Result<(i32, i32), TransferError> {
        let mut state = self.state.write().await;

        // Calculate actual cursor position
        let cursor_pos = match payload.cursor_pos {
            CursorEntryPos::EdgeRelative(rel) => {
                // Entry is from the perspective of the sender
                // So if they say "from_direction = Right", they're to our right
                // and we should position cursor at our right edge
                match from_direction {
                    Direction::Left => {
                        // They're to our left, cursor enters from left edge
                        let y = (rel * screen_height as f64) as i32;
                        (0, y)
                    }
                    Direction::Right => {
                        let y = (rel * screen_height as f64) as i32;
                        (screen_width as i32 - 1, y)
                    }
                    Direction::Up => {
                        let x = (rel * screen_width as f64) as i32;
                        (x, 0)
                    }
                    Direction::Down => {
                        let x = (rel * screen_width as f64) as i32;
                        (x, screen_height as i32 - 1)
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

    #[error("Timeout")]
    Timeout,
}
