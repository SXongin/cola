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
pub(crate) const DEFAULT_FILE_NAME: &str = "file";

/// One File Content, uniform across Generations and across the tool-output and
/// user-attachment sources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContent {
    /// The inline `data:` URI the payload arrived in. Empty for a byte-backed
    /// File Content (a locally-read file), whose payload lives in [`Self::data`]
    /// instead.
    pub uri: String,
    /// The payload's media type — [`DEFAULT_FILE_MIME`] when none was reported.
    pub mime: String,
    /// The payload's display name — [`DEFAULT_FILE_NAME`] when none was
    /// reported.
    pub name: String,
    /// The byte length actually handed to the Platform: the decoded inline
    /// payload's length for a [`Self::decode`]d File Content, the raw slice's
    /// length for a [`Self::from_bytes`] one. The invariant is one number for
    /// both sources, so they cannot drift.
    pub size: u64,
    /// The raw payload of a **byte-backed** File Content (a locally-read file,
    /// ADR-0083 §7), so a viewer's bytes reach the Platform's upload paths with
    /// NO base64 round trip. `None` for a [`Self::decode`]d File Content, whose
    /// payload lives in its `data:` URI.
    pub data: Option<Vec<u8>>,
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
            data: None,
        };
        // `size` IS the decoded inline payload's length, read through the same
        // decoding `bytes` hands the Platform, so the two cannot drift.
        content.size = content.bytes()?.len() as u64;
        Some(content)
    }

    /// A byte-backed File Content from a locally-read file (ADR-0083 §7): the
    /// bytes a viewer read from the **Host** filesystem, handed to the
    /// Platform's upload paths directly — no `data:` URI, no base64 round trip,
    /// so a 30MB file is not re-encoded into a ~40MB string only to be decoded
    /// again. `size` is the slice's own length, the same number `bytes()` hands
    /// the Platform, so the two constructions keep one invariant. An empty
    /// `mime`/`name` reads its sane default, exactly like [`Self::decode`].
    pub fn from_bytes(mime: &str, name: &str, bytes: Vec<u8>) -> Self {
        Self {
            uri: String::new(),
            mime: non_empty(Some(mime)).unwrap_or(DEFAULT_FILE_MIME).to_string(),
            name: non_empty(Some(name)).unwrap_or(DEFAULT_FILE_NAME).to_string(),
            size: bytes.len() as u64,
            data: Some(bytes),
        }
    }

    /// The payload's bytes: the raw slice for a byte-backed File Content,
    /// otherwise base64-decoded from the `data:` URI. `None` when the URI
    /// carries no decodable inline payload.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        match &self.data {
            Some(bytes) => Some(bytes.clone()),
            None => inline_payload(&self.uri),
        }
    }

    /// The one line a File Content is recorded with where a message's body is
    /// shown (ADR-0076): `📎 <name> · <mime> · <size>`. The line names the
    /// payload; it never carries its bytes.
    pub fn record_line(&self) -> String {
        format!("📎 {} · {} · {}", self.name, self.mime, human_size(self.size))
    }

    /// The record lines for `files`, newline-joined — the one body fragment
    /// wherever a message's File Contents are shown (ADR-0076). An empty
    /// string when there are none, so a caller may append it unconditionally.
    pub fn record_lines<'a>(files: impl IntoIterator<Item = &'a FileContent>) -> String {
        files
            .into_iter()
            .map(Self::record_line)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `text` then `records`, newline-joined — the one "message body" fragment
    /// wherever a message's text and its File Contents are shown together
    /// (ADR-0076): the Session Snapshot「最近对话」tail and the External Message
    /// 「有新消息」preview both compose it, so the two cannot drift. An empty
    /// `records` leaves `text` byte-for-byte; an empty `text` yields the
    /// records alone.
    pub fn join_records(text: &str, records: &str) -> String {
        if records.is_empty() {
            return text.to_string();
        }
        if text.is_empty() {
            return records.to_string();
        }
        format!("{text}\n{records}")
    }
}

/// A byte count as a human-readable size: exact bytes below 1 KiB, otherwise
/// one decimal in the largest binary unit that fits (`512 B`, `1.2 MB`).
/// Shared by the File Content record line and the File Viewer's naming block
/// (ADR-0083 §3), so the two surfaces spell a size the same way.
pub(crate) fn human_size(bytes: u64) -> String {
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

    /// A byte-backed File Content hands its raw bytes straight back — no
    /// `data:` URI, no base64 round trip — and `size` is the byte count those
    /// bytes ARE, the same number `bytes()` returns (ADR-0083 §7).
    #[test]
    fn from_bytes_keeps_size_equal_to_the_bytes_handed_to_the_platform() {
        let bytes = vec![0x89, b'P', b'N', b'G', 0, 1, 2, 3];
        let content = FileContent::from_bytes("image/png", "shot.png", bytes.clone());

        assert!(content.uri.is_empty(), "a byte-backed content has no URI");
        assert_eq!(content.data.as_deref(), Some(bytes.as_slice()));
        assert_eq!(content.mime, "image/png");
        assert_eq!(content.name, "shot.png");
        assert_eq!(
            content.size,
            bytes.len() as u64,
            "size IS the bytes the Platform receives"
        );
        assert_eq!(content.bytes().as_deref(), Some(bytes.as_slice()));
    }

    /// A byte-backed construction reads the same sane defaults for a missing
    /// name/mime as `decode` — an empty field is "none reported", not a value.
    #[test]
    fn from_bytes_reads_the_sane_defaults() {
        let content = FileContent::from_bytes("", "", vec![1, 2, 3]);
        assert_eq!(content.mime, DEFAULT_FILE_MIME);
        assert_eq!(content.name, DEFAULT_FILE_NAME);
        assert_eq!(content.size, 3);
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
            data: None,
        };
        assert_eq!(small.record_line(), "📎 shot.png · image/png · 512 B");

        let large = FileContent {
            uri: String::new(),
            mime: "application/pdf".into(),
            name: "report.pdf".into(),
            size: 1_258_291,
            data: None,
        };
        assert_eq!(large.record_line(), "📎 report.pdf · application/pdf · 1.2 MB");
    }

    /// The humanizer's 1 KiB switch: exactly the byte before stays exact bytes,
    /// and 1024 B becomes `1.0 KB` in the binary unit (ADR-0076).
    #[test]
    fn the_size_humanizer_switches_at_the_kib_boundary() {
        let line = |size: u64| {
            FileContent {
                uri: String::new(),
                mime: "image/png".into(),
                name: "shot.png".into(),
                size,
                data: None,
            }
            .record_line()
        };
        assert_eq!(line(1023), "📎 shot.png · image/png · 1023 B");
        assert_eq!(line(1024), "📎 shot.png · image/png · 1.0 KB");
    }
}
