//! The typed **File Content** view over the neutral model (GLOSSARY; ADR-0076).
//!
//! A File Content is the file payload a transcript part carries instead of text
//! — a tool's `file` output block (an MCP resource part rides the same shape),
//! or a user message's `files` attachment — and it is always *inline*: the
//! payload arrives in a `data:` URI (or in the bare base64 field a decoder
//! reassembles into one). Every generation's decoder hands it over through this
//! one shape, so the Bridge never branches on a Generation and never re-parses
//! a raw block.
//!
//! A `file` block that only *references* a file (`file://…`, `https://…`) is
//! not a File Content: cola has nothing to upload or embed, so the decoder
//! keeps that block raw and the neutral model's existing raw-block
//! preservation is unchanged.

use base64::Engine;

/// The media type a File Content reads when its block carried none.
pub const DEFAULT_FILE_MIME: &str = "application/octet-stream";

/// The display name a File Content reads when its block carried none.
const DEFAULT_FILE_NAME: &str = "file";

/// One File Content, uniform across Generations and across the tool-output and
/// user-attachment sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    /// The inline `data:` URI the payload arrived in.
    pub uri: String,
    /// The payload's media type — [`DEFAULT_FILE_MIME`] when none was reported.
    pub mime: String,
    /// The payload's display name — [`DEFAULT_FILE_NAME`] when none was
    /// reported.
    pub name: String,
    /// The decoded byte length of the inline payload.
    pub size: u64,
}

impl FileContent {
    /// The one constructor every source's decoder calls, from the fields its
    /// own raw block carries. `None` when `uri` does not inline a base64
    /// payload — a `file://`/`http(s)://` reference, no URI at all, or a
    /// malformed `data:` URI — so the caller keeps that block raw: the payload
    /// is never dropped, and a missing name or mime reads its sane default
    /// rather than panicking.
    pub fn decode(uri: &str, mime: Option<&str>, name: Option<&str>) -> Option<Self> {
        let mut content = Self {
            uri: uri.to_string(),
            mime: non_empty(mime).unwrap_or(DEFAULT_FILE_MIME).to_string(),
            name: non_empty(name).unwrap_or(DEFAULT_FILE_NAME).to_string(),
            size: 0,
        };
        // `size` IS the decoded inline payload's length, read through the same
        // decoding `bytes` hands the Platform, so the two cannot drift.
        content.size = content.bytes()?.len() as u64;
        Some(content)
    }

    /// The inline payload's bytes, base64-decoded from the `data:` URI. `None`
    /// when the URI carries no decodable inline payload.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        inline_payload(&self.uri)
    }

    /// The one line a File Content is recorded with where a message's body is
    /// shown (ADR-0076): `📎 <name> · <mime> · <size>`. The line names the
    /// payload; it never carries its bytes.
    pub fn record_line(&self) -> String {
        format!("📎 {} · {} · {}", self.name, self.mime, human_size(self.size))
    }
}

/// A byte count as a human-readable size: exact bytes below 1 KiB, otherwise
/// one decimal in the largest binary unit that fits (`512 B`, `1.2 MB`).
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The decoded payload of an inline base64 `data:` URI, or `None` when `uri` is
/// not one: any other scheme, a missing `;base64` marker, or malformed base64.
/// A non-base64 `data:` URI is not a shape the protocol emits and is not
/// decoded.
fn inline_payload(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let (metadata, payload) = rest.split_once(',')?;
    if !metadata
        .split(';')
        .any(|part| part.eq_ignore_ascii_case("base64"))
    {
        return None;
    }
    base64::engine::general_purpose::STANDARD.decode(payload).ok()
}

/// A reported string that is present and non-empty: an empty name/mime is the
/// server saying "none reported", not a value.
fn non_empty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The size is the DECODED payload length (`QUJD` is the base64 of `ABC`),
    /// never the base64 length, and the bytes come straight back out.
    #[test]
    fn an_inline_data_uri_yields_its_decoded_bytes_and_size() {
        let content = FileContent::decode("data:image/png;base64,QUJD", Some("image/png"), Some("shot.png"))
            .expect("an inline payload is a File Content");

        assert_eq!(content.uri, "data:image/png;base64,QUJD");
        assert_eq!(content.mime, "image/png");
        assert_eq!(content.name, "shot.png");
        assert_eq!(content.size, 3, "size must be the decoded length");
        assert_eq!(content.bytes().as_deref(), Some(b"ABC".as_slice()));
    }

    /// A missing name or mime reads the sane default — an absent field and an
    /// empty one alike — and never panics.
    #[test]
    fn a_missing_name_or_mime_reads_the_sane_default() {
        let absent = FileContent::decode("data:application/pdf;base64,QUJD", None, None).unwrap();
        assert_eq!(absent.mime, DEFAULT_FILE_MIME);
        assert_eq!(absent.name, DEFAULT_FILE_NAME);

        let empty = FileContent::decode("data:application/pdf;base64,QUJD", Some(""), Some("")).unwrap();
        assert_eq!(empty.mime, DEFAULT_FILE_MIME);
        assert_eq!(empty.name, DEFAULT_FILE_NAME);

        let unnamed =
            FileContent::decode("data:application/pdf;base64,QUJD", Some("application/pdf"), None).unwrap();
        assert_eq!(unnamed.mime, "application/pdf");
        assert_eq!(unnamed.name, DEFAULT_FILE_NAME);
    }

    /// A block that references a file without inlining its payload is not a
    /// File Content, and a malformed inline payload never panics: both stay `None`
    /// so the caller preserves the block raw.
    #[test]
    fn a_uri_that_does_not_inline_its_payload_is_not_a_file_content() {
        assert!(FileContent::decode("file:///a.png", Some("image/png"), None).is_none());
        assert!(FileContent::decode("https://example.com/a.png", None, None).is_none());
        assert!(FileContent::decode("", None, None).is_none());
        assert!(FileContent::decode("data:text/plain,hello", Some("text/plain"), None).is_none());
        assert!(FileContent::decode("data:image/png;base64,!!!", Some("image/png"), None).is_none());
        assert!(FileContent::decode("data:image/png;base64", Some("image/png"), None).is_none());
    }

    /// A record line names the file — `📎 name · mime · size` — with the size
    /// humanized: exact bytes below 1 KiB, one decimal in the largest binary
    /// unit that fits above it (ADR-0076).
    #[test]
    fn a_record_line_names_the_file_and_humanizes_its_size() {
        let small = FileContent {
            uri: String::new(),
            mime: "image/png".into(),
            name: "shot.png".into(),
            size: 512,
        };
        assert_eq!(small.record_line(), "📎 shot.png · image/png · 512 B");

        let large = FileContent {
            uri: String::new(),
            mime: "application/pdf".into(),
            name: "report.pdf".into(),
            size: 1_258_291,
        };
        assert_eq!(large.record_line(), "📎 report.pdf · application/pdf · 1.2 MB");
    }
}
