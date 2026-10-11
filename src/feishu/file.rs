//! Feishu's file-message caps and upload typing (ADR-0076).
//!
//! A File Content that is not an embeddable image — or an image past the embed
//! caps — is delivered as a File Message. Feishu caps such a message's upload
//! at 30MB and requires a `file_type` from a fixed set; both are platform
//! facts, so they live here beside the upload and never in `src/backend/`.
//! A content past the cap is not deliverable; the caller marks it `未发送`
//! rather than failing the card.

use crate::backend::FileContent;

/// Feishu's `im/v1/files` upload cap: 30MB — the same ceiling a
/// `msg_type:"file"` message carries. Past it a File Content can never be
/// delivered through this path.
pub(crate) const MAX_FILE_BYTES: u64 = 30 * 1024 * 1024;

/// Whether `content` is within Feishu's File Message upload cap. `false` means
/// the caller marks it `未发送` and uploads nothing.
pub(crate) fn deliverable_file(content: &FileContent) -> bool {
    content.size <= MAX_FILE_BYTES
}

/// The `file_type` Feishu's file upload requires, derived from the content's
/// mime (preferred) or, when the mime is generic, its name's extension.
/// Anything unrecognized reads `stream`, Feishu's catch-all.
pub(crate) fn file_type_for(mime: &str, name: &str) -> &'static str {
    if let Some(file_type) = file_type_for_mime(mime) {
        return file_type;
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .unwrap_or("");
    match extension.to_ascii_lowercase().as_str() {
        "pdf" => "pdf",
        "doc" | "docx" => "doc",
        "xls" | "xlsx" | "csv" => "xls",
        "ppt" | "pptx" => "ppt",
        "mp4" => "mp4",
        "opus" | "ogg" => "opus",
        _ => "stream",
    }
}

/// The `file_type` for a mime Feishu names directly, or `None` when the mime is
/// generic (`application/octet-stream`, `text/plain`, …) and the name must
/// decide.
fn file_type_for_mime(mime: &str) -> Option<&'static str> {
    match mime {
        "application/pdf" => Some("pdf"),
        "application/msword" | "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some("doc")
        }
        "application/vnd.ms-excel" | "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            Some("xls")
        }
        "application/vnd.ms-powerpoint"
        | "application/vnd.openxmlformats-officedocument.presentationml.presentation" => Some("ppt"),
        "video/mp4" => Some("mp4"),
        "audio/ogg" | "audio/opus" => Some("opus"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A File Content from raw bytes and a mime, as the render path decodes it.
    fn content(bytes: &[u8], mime: &str, name: &str) -> FileContent {
        use base64::Engine as _;
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        FileContent::decode(&format!("data:{mime};base64,{encoded}"), Some(mime), Some(name)).unwrap()
    }

    #[test]
    fn a_content_over_the_cap_is_not_deliverable() {
        let mut over = content(b"x", "application/pdf", "a.pdf");
        over.size = MAX_FILE_BYTES + 1;
        assert!(!deliverable_file(&over));
        let mut at = content(b"x", "application/pdf", "a.pdf");
        at.size = MAX_FILE_BYTES;
        assert!(deliverable_file(&at), "exactly at the cap stays deliverable");
    }

    /// The mime decides the `file_type` when it names a document, and the
    /// name's extension decides it when the mime is generic. Anything else is
    /// Feishu's `stream`.
    #[test]
    fn file_type_reads_the_mime_then_the_name() {
        assert_eq!(file_type_for("application/pdf", "a.bin"), "pdf");
        assert_eq!(file_type_for("video/mp4", "clip"), "mp4");
        assert_eq!(
            file_type_for(
                "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "book"
            ),
            "xls"
        );
        // A generic mime falls back to the extension.
        assert_eq!(file_type_for("application/octet-stream", "report.pdf"), "pdf");
        assert_eq!(file_type_for("application/octet-stream", "notes.DOCX"), "doc");
        assert_eq!(file_type_for("text/plain", "rows.csv"), "xls");
        assert_eq!(file_type_for("application/octet-stream", "slides.pptx"), "ppt");
        assert_eq!(file_type_for("", "voice.opus"), "opus");
        // Unknown shape reads the catch-all.
        assert_eq!(file_type_for("application/zip", "archive.zip"), "stream");
        assert_eq!(
            file_type_for("application/octet-stream", "no-extension"),
            "stream"
        );
    }
}
