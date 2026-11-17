//! Wire types and limits for terminal file drops (browser -> server -> host).
//!
//! A writer drops files on the live terminal; the server relays them to the
//! session author's host over a dedicated SSE stream and waits for the host's
//! explicit acknowledgement before reporting success. The host derives its own
//! safe destination paths: names here are display hints only.

use serde::{Deserialize, Serialize};

/// Maximum number of files in one drop.
pub const MAX_FILES: usize = 16;
/// Maximum combined decoded size of one drop (9 MiB).
pub const MAX_TOTAL_BYTES: usize = 9 * 1024 * 1024;
/// Maximum decoded size of one pasted clipboard image (9 MiB; the server's
/// 12 MiB base64 limit).
pub const MAX_IMAGE_BYTES: usize = 9 * 1024 * 1024;
/// Image types the host will place on the clipboard.
pub const IMAGE_MIMES: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];
/// Maximum length of an original file name, in UTF-8 bytes.
pub const MAX_NAME_BYTES: usize = 255;
/// How long a validated transfer stays valid while awaiting its host result.
/// Upload collection and validation have a separate bounded admission period.
pub const TRANSFER_TIMEOUT_SECS: u64 = 20;

/// One file in a drop: original name plus standard base64 content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFile {
    pub name: String,
    pub b64: String,
}

/// `POST /api/live/{id}/files` request body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFilesRequest {
    pub files: Vec<TerminalFile>,
    /// The `transfer` id the browser signed for this drop. Opaque to the server;
    /// the host applies the files only against its own authorization of that id.
    #[serde(default)]
    pub host_transfer: String,
}

/// An SSE event on `GET /api/live/{id}/files/stream`, serialized as
/// `{"type":"files_v2","transfer_id":...,"expires_at":0,"files":[...]}`.
/// Version 2 requires author-only preflight after receipt. `expires_at` is zero
/// and is retained only to fail closed in legacy consumers; it is not a clock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalFilesEvent {
    #[serde(rename = "files_v2")]
    Files {
        transfer_id: String,
        expires_at: i64,
        files: Vec<TerminalFile>,
        #[serde(default)]
        host_transfer: String,
    },
}

/// Remaining lifetime from the server's monotonic deadline. The host adds this
/// to its Instant recorded BEFORE the preflight request, never receipt time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFilePreflight {
    pub remaining_ms: u64,
}

/// Outcome a host reports for a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalFileStatus {
    Sent,
    Failed,
}

/// `POST /api/live/{id}/files/{transfer_id}/result` request body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFileResultRequest {
    pub status: TerminalFileStatus,
}

/// Successful `POST /api/live/{id}/files` response body: `{"status":"sent"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFilesResponse {
    pub status: TerminalFileStatus,
}

/// Why an original file name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FileNameError {
    #[error("file name is empty")]
    Empty,
    #[error("file name is longer than {MAX_NAME_BYTES} bytes")]
    TooLong,
    #[error("file name contains a path separator, control character, or is a dot name")]
    Unsafe,
}

/// Validate an original file name: bounded, no path separators, no control
/// characters, not `.`/`..`. Unicode is allowed. The host sanitizes
/// independently; this only rejects names that cannot be honest file names.
pub fn validate_file_name(name: &str) -> Result<(), FileNameError> {
    if name.is_empty() {
        return Err(FileNameError::Empty);
    }
    if name.len() > MAX_NAME_BYTES {
        return Err(FileNameError::TooLong);
    }
    if name == "."
        || name == ".."
        || name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\')
    {
        return Err(FileNameError::Unsafe);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_unicode_and_spaces() {
        assert_eq!(validate_file_name("Résumé final (1).pdf"), Ok(()));
        assert_eq!(validate_file_name("日本語.png"), Ok(()));
    }

    #[test]
    fn rejects_paths_controls_and_dot_names() {
        for bad in [
            "", ".", "..", "a/b", "a\\b", "a\nb", "a\0b", "\u{7f}x", "\u{85}x",
        ] {
            assert!(
                validate_file_name(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
        assert_eq!(
            validate_file_name(&"a".repeat(MAX_NAME_BYTES + 1)),
            Err(FileNameError::TooLong)
        );
        assert_eq!(validate_file_name(&"a".repeat(MAX_NAME_BYTES)), Ok(()));
    }

    #[test]
    fn event_wire_shape_is_tagged() {
        let event = TerminalFilesEvent::Files {
            transfer_id: "t".into(),
            expires_at: 5,
            files: vec![TerminalFile {
                name: "a".into(),
                b64: "YQ==".into(),
            }],
            host_transfer: "ab".into(),
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type":"files_v2","transfer_id":"t","expires_at":5,"files":[{"name":"a","b64":"YQ=="}],"host_transfer":"ab"})
        );
        let status: TerminalFileResultRequest =
            serde_json::from_str(r#"{"status":"failed"}"#).unwrap();
        assert_eq!(status.status, TerminalFileStatus::Failed);
        assert!(serde_json::from_str::<TerminalFileResultRequest>(r#"{"status":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<TerminalFilesEvent>(
                r#"{"type":"files","transfer_id":"legacy","expires_at":9999999999999,"files":[]}"#
            )
            .is_err(),
            "legacy hosts/servers must not silently use cross-machine expiry"
        );
    }
}
