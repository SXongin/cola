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

/// The mime an **inbound** attachment is attached to a prompt as, or `None`
/// when the server would not read it as model content (ADR-0076's inbound
/// amendment).
///
/// The server forwards exactly these families: the four image mimes
/// (`image/png`, `image/jpeg`, `image/gif`, `image/webp`), `application/pdf`,
/// and text. An image is NOT `image/*`: the server re-encodes only an image
/// past its size/dimension limits, so a small `image/bmp`/`tiff`/`heic` keeps
/// its mime and is dropped silently on V2 — hence exactly the four. A text-like
/// file is normalized to `text/plain` instead of its own mime so it is read as
/// text on both generations.
///
/// `header_mime` is the resource response's Content-Type (which may be the
/// generic `application/octet-stream` — the live question the implementation
/// settled defensively) and `name` the display name. The header decides first
/// (a `text/*` is text whatever the name says); only when it names no supported
/// family does the name's extension decide, against the closed list below, so
/// a `.pdf`/`.md` served as `application/octet-stream` is still attached while
/// `notes.docx` is not — never a silent drop either way.
pub(crate) fn inbound_mime(header_mime: &str, name: &str) -> Option<String> {
    let mime = header_mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if let Some(image) = four_image_mimes(&mime) {
        return Some(image.to_string());
    }
    if mime == "application/pdf" {
        return Some("application/pdf".to_string());
    }
    if mime.starts_with("text/") {
        return Some("text/plain".to_string());
    }
    // The header named no supported mime (typically the generic
    // `application/octet-stream`): the name's own extension decides.
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => Some("image/png".to_string()),
        "jpg" | "jpeg" => Some("image/jpeg".to_string()),
        "gif" => Some("image/gif".to_string()),
        "webp" => Some("image/webp".to_string()),
        "pdf" => Some("application/pdf".to_string()),
        ext if TEXT_EXTENSIONS.contains(&ext) => Some("text/plain".to_string()),
        _ => None,
    }
}

/// The four image mimes the server forwards to the model, or `None`. NOT
/// `image/*` (a small `image/bmp` keeps its mime and is dropped). The common
/// `image/jpg` alias is normalized to `image/jpeg` (also one of the four), so a
/// JPEG the platform happens to mislabel is attached rather than reported.
fn four_image_mimes(mime: &str) -> Option<&'static str> {
    match mime {
        "image/png" => Some("image/png"),
        "image/jpeg" | "image/jpg" => Some("image/jpeg"),
        "image/gif" => Some("image/gif"),
        "image/webp" => Some("image/webp"),
        _ => None,
    }
}

/// The closed list of file extensions an inbound attachment whose header mime
/// is generic (`application/octet-stream`) is still read as `text/plain`.
/// Closed on purpose: `notes.rst` and `report.csv` are text, `notes.docx` is
/// not, and an extension outside the list is unsupported like any other mime
/// (ADR-0076's inbound amendment).
const TEXT_EXTENSIONS: &[&str] = &[
    "txt",
    "text",
    "md",
    "markdown",
    "rst",
    "adoc",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "ndjson",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "xml",
    "html",
    "htm",
    "css",
    "scss",
    "less",
    "js",
    "mjs",
    "cjs",
    "ts",
    "tsx",
    "jsx",
    "py",
    "rb",
    "go",
    "rs",
    "java",
    "kt",
    "swift",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "cs",
    "php",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "sql",
    "graphql",
    "gql",
    "proto",
    "tf",
    "hcl",
    "nix",
    "tex",
    "bib",
    "log",
    "diff",
    "patch",
    "svg",
];

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

    /// Only the four image mimes, `application/pdf` and text are attached: a
    /// small non-four image (`bmp`) and any other mime read `None`, so the
    /// bridge reports them instead of the server dropping them silently.
    #[test]
    fn only_the_server_readable_families_are_attachable() {
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            assert_eq!(inbound_mime(mime, ""), Some(mime.to_string()), "{mime}");
        }
        assert_eq!(
            inbound_mime("application/pdf", ""),
            Some("application/pdf".into())
        );
        // The `image/jpg` alias normalizes to a mime the server reads.
        assert_eq!(inbound_mime("image/jpg", ""), Some("image/jpeg".into()));
        // Not `image/*`: the four only.
        assert_eq!(inbound_mime("image/bmp", "shot.bmp"), None);
        assert_eq!(inbound_mime("image/tiff", "scan.tiff"), None);
        assert_eq!(inbound_mime("image/heic", "photo.heic"), None);
        assert_eq!(
            inbound_mime("image/svg+xml", "logo.svg"),
            Some("text/plain".into())
        );
        // The Content-Type's parameters are ignored.
        assert_eq!(
            inbound_mime("image/png; charset=binary", ""),
            Some("image/png".into())
        );
        assert_eq!(
            inbound_mime("text/markdown; charset=utf-8", "a.md"),
            Some("text/plain".into())
        );
    }

    /// A `text/*` header is text whatever the name says; a generic header (the
    /// live `application/octet-stream` question, settled defensively) falls to
    /// the name's extension against the closed list.
    #[test]
    fn a_generic_header_falls_to_the_name_and_the_list_is_closed() {
        // The likely live shape: everything served as octet-stream.
        assert_eq!(
            inbound_mime("application/octet-stream", "P3173R0.pdf"),
            Some("application/pdf".into())
        );
        assert_eq!(
            inbound_mime("application/octet-stream", "notes.md"),
            Some("text/plain".into())
        );
        assert_eq!(
            inbound_mime("application/octet-stream", "data.JSON"),
            Some("text/plain".into()),
            "the extension is matched case-insensitively"
        );
        assert_eq!(
            inbound_mime("application/octet-stream", "shot.png"),
            Some("image/png".into())
        );
        // The list is closed: a docx is unsupported, not text.
        assert_eq!(inbound_mime("application/octet-stream", "notes.docx"), None);
        assert_eq!(inbound_mime("", "archive.zip"), None);
        assert_eq!(inbound_mime("application/octet-stream", "no-extension"), None);
        // A text header wins over a misleading name.
        assert_eq!(
            inbound_mime("text/plain", "notes.docx"),
            Some("text/plain".into())
        );
    }
}
