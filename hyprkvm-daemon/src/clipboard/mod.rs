//! Clipboard synchronization module
//!
//! Handles clipboard sharing between HyprKVM machines.

mod hash;
mod wayland;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use hyprkvm_common::protocol::{
    ClipboardDataPayload, ClipboardOfferPayload, ClipboardRequestPayload,
};

use crate::config::ClipboardConfig;

pub use self::wayland::{is_image_mime, is_text_mime};

/// Chunk size for large clipboard data (64KB)
const CHUNK_SIZE: usize = 64 * 1024;

/// Timeout for incomplete chunk buffers (30 seconds)
const CHUNK_TIMEOUT_SECS: u64 = 30;

/// Clipboard synchronization error
#[derive(Debug, thiserror::Error)]
#[allow(dead_code)]
pub enum ClipboardError {
    #[error("Clipboard access denied")]
    AccessDenied,

    #[error("Clipboard empty")]
    Empty,

    #[error("Content too large: {size} bytes (max: {max})")]
    TooLarge { size: u64, max: u64 },

    #[error("Unsupported content type")]
    UnsupportedType,

    #[error("Wayland error: {0}")]
    Wayland(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),

    #[error("Chunk timeout")]
    ChunkTimeout,

    #[error("Chunk sequence error")]
    ChunkSequence,
}

/// Buffer for reassembling chunked clipboard data
struct ChunkBuffer {
    mime_type: String,
    chunks: HashMap<u32, Vec<u8>>,
    total_chunks: u32,
    received_at: Instant,
}

impl ChunkBuffer {
    fn new(mime_type: String, total_chunks: u32) -> Self {
        Self {
            mime_type,
            chunks: HashMap::new(),
            total_chunks,
            received_at: Instant::now(),
        }
    }

    fn is_complete(&self) -> bool {
        self.chunks.len() == self.total_chunks as usize
    }

    fn is_expired(&self) -> bool {
        self.received_at.elapsed().as_secs() > CHUNK_TIMEOUT_SECS
    }

    fn add_chunk(&mut self, index: u32, data: Vec<u8>) {
        self.chunks.insert(index, data);
    }

    fn reassemble(&self) -> Option<Vec<u8>> {
        if !self.is_complete() {
            return None;
        }

        let mut result = Vec::new();
        for i in 0..self.total_chunks {
            if let Some(chunk) = self.chunks.get(&i) {
                result.extend_from_slice(chunk);
            } else {
                return None;
            }
        }
        Some(result)
    }
}

/// Manages clipboard synchronization
pub struct ClipboardManager {
    config: ClipboardConfig,
    clipboard_id_counter: AtomicU64,
    last_content_hash: RwLock<Option<String>>,
    pending_chunks: RwLock<HashMap<u64, ChunkBuffer>>,
}

impl ClipboardManager {
    /// Create a new clipboard manager
    pub fn new(config: ClipboardConfig) -> Self {
        Self {
            config,
            clipboard_id_counter: AtomicU64::new(1),
            last_content_hash: RwLock::new(None),
            pending_chunks: RwLock::new(HashMap::new()),
        }
    }

    /// Generate a new clipboard ID
    fn next_clipboard_id(&self) -> u64 {
        self.clipboard_id_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Read clipboard and create an offer (if enabled and within limits)
    pub async fn create_offer(&self) -> Result<Option<ClipboardOfferPayload>, ClipboardError> {
        if !self.config.enabled {
            return Ok(None);
        }

        // Get available MIME types
        let mime_types = match wayland::get_available_mime_types() {
            Ok(types) => types,
            Err(ClipboardError::Empty) => {
                debug!("Clipboard is empty, nothing to offer");
                return Ok(None);
            }
            Err(e) => return Err(e),
        };

        if mime_types.is_empty() {
            debug!("No MIME types available in clipboard");
            return Ok(None);
        }

        // Filter MIME types based on config
        let filtered_types: Vec<String> = mime_types
            .into_iter()
            .filter(|m| {
                (self.config.sync_images && is_image_mime(m))
                    || (self.config.sync_text && is_text_mime(m))
            })
            .collect();

        if filtered_types.is_empty() {
            debug!("No supported MIME types in clipboard");
            return Ok(None);
        }

        // Select best MIME type and read content for hash
        let preferred_mime =
            wayland::select_mime_type(&filtered_types, self.config.sync_images, self.config.sync_text);

        let (data, actual_mime) = match preferred_mime {
            Some(ref mime) => wayland::read_clipboard(Some(mime))?,
            None => return Ok(None),
        };

        let size = data.len() as u64;

        // Check size limit
        if size > self.config.max_size {
            warn!(
                "Clipboard content too large: {} bytes (max: {}), skipping sync",
                size, self.config.max_size
            );
            return Ok(None);
        }

        // Compute content hash for deduplication
        let content_hash = hash::compute_hash(&data);

        // Check if content is same as last synced
        {
            let last_hash = self.last_content_hash.read().await;
            if last_hash.as_ref() == Some(&content_hash) {
                debug!("Clipboard content unchanged (hash match), skipping sync");
                return Ok(None);
            }
        }

        let clipboard_id = self.next_clipboard_id();

        info!(
            "Creating clipboard offer: id={}, mime={}, size={} bytes",
            clipboard_id, actual_mime, size
        );

        Ok(Some(ClipboardOfferPayload {
            clipboard_id,
            mime_types: filtered_types,
            size_hint: Some(size),
            content_hash: Some(content_hash),
        }))
    }

    /// Handle incoming offer, return request if interested
    pub async fn handle_offer(
        &self,
        offer: ClipboardOfferPayload,
    ) -> Option<ClipboardRequestPayload> {
        if !self.config.enabled {
            return None;
        }

        // Check for duplicate content via hash
        if let Some(ref hash) = offer.content_hash {
            let last_hash = self.last_content_hash.read().await;
            if last_hash.as_ref() == Some(hash) {
                debug!(
                    "Clipboard offer {} has same hash as current, skipping",
                    offer.clipboard_id
                );
                return None;
            }
        }

        // Check size limit
        if let Some(size) = offer.size_hint {
            if size > self.config.max_size {
                warn!(
                    "Clipboard offer {} too large: {} bytes (max: {}), skipping",
                    offer.clipboard_id, size, self.config.max_size
                );
                return None;
            }
        }

        // Select preferred MIME type
        let mime_type = wayland::select_mime_type(
            &offer.mime_types,
            self.config.sync_images,
            self.config.sync_text,
        )?;

        info!(
            "Requesting clipboard {}: mime={}",
            offer.clipboard_id, mime_type
        );

        Some(ClipboardRequestPayload {
            clipboard_id: offer.clipboard_id,
            mime_type,
        })
    }

    /// Handle incoming request, return data chunks
    pub async fn handle_request(
        &self,
        request: ClipboardRequestPayload,
    ) -> Result<Vec<ClipboardDataPayload>, ClipboardError> {
        if !self.config.enabled {
            return Ok(vec![]);
        }

        info!(
            "Handling clipboard request {}: mime={}",
            request.clipboard_id, request.mime_type
        );

        // Read clipboard with requested MIME type
        let (data, _actual_mime) = wayland::read_clipboard(Some(&request.mime_type))?;

        // Check size limit
        if data.len() as u64 > self.config.max_size {
            warn!(
                "Clipboard content too large for request: {} bytes",
                data.len()
            );
            return Err(ClipboardError::TooLarge {
                size: data.len() as u64,
                max: self.config.max_size,
            });
        }

        // Chunk the data
        let chunks = self.chunk_data(&data, request.clipboard_id, &request.mime_type);

        info!(
            "Sending clipboard {}: {} bytes in {} chunk(s)",
            request.clipboard_id,
            data.len(),
            chunks.len()
        );

        Ok(chunks)
    }

    /// Chunk data for transmission
    fn chunk_data(
        &self,
        data: &[u8],
        clipboard_id: u64,
        mime_type: &str,
    ) -> Vec<ClipboardDataPayload> {
        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;

        let total_chunks = (data.len() + CHUNK_SIZE - 1) / CHUNK_SIZE;

        if total_chunks <= 1 {
            // Single chunk, no chunking needed
            return vec![ClipboardDataPayload {
                clipboard_id,
                mime_type: mime_type.to_string(),
                data: engine.encode(data),
                chunk_index: None,
                total_chunks: None,
            }];
        }

        data.chunks(CHUNK_SIZE)
            .enumerate()
            .map(|(i, chunk)| ClipboardDataPayload {
                clipboard_id,
                mime_type: mime_type.to_string(),
                data: engine.encode(chunk),
                chunk_index: Some(i as u32),
                total_chunks: Some(total_chunks as u32),
            })
            .collect()
    }

    /// Handle incoming data chunk
    pub async fn handle_data(&self, data: ClipboardDataPayload) -> Result<(), ClipboardError> {
        if !self.config.enabled {
            return Ok(());
        }

        use base64::Engine;
        let engine = base64::engine::general_purpose::STANDARD;

        // Decode base64 data
        let decoded = engine.decode(&data.data)?;

        // Check if this is a chunked transfer
        match (data.chunk_index, data.total_chunks) {
            (Some(index), Some(total)) => {
                // Chunked transfer
                debug!(
                    "Received clipboard {} chunk {}/{}",
                    data.clipboard_id,
                    index + 1,
                    total
                );

                let mut pending = self.pending_chunks.write().await;

                // Clean up expired buffers
                pending.retain(|_, buf| !buf.is_expired());

                // Get or create chunk buffer
                let buffer = pending
                    .entry(data.clipboard_id)
                    .or_insert_with(|| ChunkBuffer::new(data.mime_type.clone(), total));

                buffer.add_chunk(index, decoded);

                if buffer.is_complete() {
                    // Reassemble and set clipboard
                    if let Some(full_data) = buffer.reassemble() {
                        let mime_type = buffer.mime_type.clone();
                        pending.remove(&data.clipboard_id);

                        info!(
                            "Clipboard {} complete: {} bytes, setting clipboard",
                            data.clipboard_id,
                            full_data.len()
                        );

                        // Update hash before setting
                        let hash = hash::compute_hash(&full_data);
                        *self.last_content_hash.write().await = Some(hash);

                        wayland::set_clipboard(&full_data, &mime_type)?;
                    }
                }
            }
            _ => {
                // Single chunk (non-chunked transfer)
                info!(
                    "Received clipboard {}: {} bytes, setting clipboard",
                    data.clipboard_id,
                    decoded.len()
                );

                // Update hash before setting
                let hash = hash::compute_hash(&decoded);
                *self.last_content_hash.write().await = Some(hash);

                wayland::set_clipboard(&decoded, &data.mime_type)?;
            }
        }

        Ok(())
    }
}
