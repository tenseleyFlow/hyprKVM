//! Wayland clipboard access via wl-clipboard-rs
//!
//! Uses the wlr-data-control or ext-data-control protocol for clipboard access
//! without needing a Wayland surface (perfect for daemon use).

use std::io::Read;

use wl_clipboard_rs::copy::{MimeType as CopyMimeType, Options as CopyOptions, Source};
use wl_clipboard_rs::paste::{
    get_contents, get_mime_types, ClipboardType, MimeType as PasteMimeType, Seat,
};

use super::ClipboardError;

/// Read clipboard content for a specific MIME type
///
/// If `mime_type` is None, reads with any available MIME type.
/// Returns the data and the actual MIME type used.
pub fn read_clipboard(mime_type: Option<&str>) -> Result<(Vec<u8>, String), ClipboardError> {
    let mime = match mime_type {
        Some(mt) => PasteMimeType::Specific(mt),
        None => PasteMimeType::Any,
    };

    let (mut pipe, actual_mime) = get_contents(ClipboardType::Regular, Seat::Unspecified, mime)
        .map_err(|e| ClipboardError::Wayland(format!("Failed to get clipboard contents: {}", e)))?;

    let mut data = Vec::new();
    pipe.read_to_end(&mut data)
        .map_err(|e| ClipboardError::Io(e))?;

    if data.is_empty() {
        return Err(ClipboardError::Empty);
    }

    Ok((data, actual_mime.to_string()))
}

/// Get available MIME types from clipboard
pub fn get_available_mime_types() -> Result<Vec<String>, ClipboardError> {
    let mime_types = get_mime_types(ClipboardType::Regular, Seat::Unspecified)
        .map_err(|e| ClipboardError::Wayland(format!("Failed to get MIME types: {}", e)))?;

    Ok(mime_types.into_iter().map(|m| m.to_string()).collect())
}

/// Set clipboard content with a specific MIME type
pub fn set_clipboard(data: &[u8], mime_type: &str) -> Result<(), ClipboardError> {
    let mut opts = CopyOptions::new();

    // Fork to background so the clipboard persists after we return
    opts.foreground(false);
    opts.copy(
        Source::Bytes(data.to_vec().into_boxed_slice()),
        CopyMimeType::Specific(mime_type.to_string()),
    )
    .map_err(|e| ClipboardError::Wayland(format!("Failed to set clipboard: {}", e)))?;

    Ok(())
}

/// MIME type priority lists for selection
pub const IMAGE_MIME_PRIORITY: &[&str] = &["image/png", "image/jpeg", "image/webp", "image/gif"];

pub const TEXT_MIME_PRIORITY: &[&str] = &[
    "text/plain;charset=utf-8",
    "text/plain",
    "UTF8_STRING",
    "STRING",
    "TEXT",
];

/// Select the best MIME type from offered types based on priority
pub fn select_mime_type(offered: &[String], sync_images: bool, sync_text: bool) -> Option<String> {
    // Try images first if enabled (higher priority for screenshots)
    if sync_images {
        for pref in IMAGE_MIME_PRIORITY {
            if offered.iter().any(|m| m == *pref) {
                return Some(pref.to_string());
            }
        }
    }

    // Then text if enabled
    if sync_text {
        for pref in TEXT_MIME_PRIORITY {
            if offered.iter().any(|m| m == *pref) {
                return Some(pref.to_string());
            }
        }
    }

    None
}

/// Check if a MIME type is an image type
pub fn is_image_mime(mime: &str) -> bool {
    mime.starts_with("image/")
}

/// Check if a MIME type is a text type
pub fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/") || mime == "UTF8_STRING" || mime == "STRING" || mime == "TEXT"
}
