//! The host-file read behind the **File Viewer** card (ADR-0083 §1, §4, §6).
//!
//! The bytes come from the **local** filesystem (`std::fs`) — the same world
//! the Directory Browser's `read_dir`/`stat` already touches, and deliberately
//! not a server route: the read is **generation-neutral** (no V1/V2 branch, no
//! wire fixture) and **backend-neutral** (no OpenCode concept, so a non-OpenCode
//! Backend later keeps this working).
//!
//! A path resolves on the **Path Scope** (§4): `~` is expanded, a relative path
//! resolves against the **Session**'s directory (the conversation's current
//! **Project** while the Session is still **Pending**), an absolute path is
//! passed through — `~`/relative/absolute only, with the scope a *privacy hint,
//! not a boundary* (#673 owns the real one; this module adds none).
//!
//! Everything here is bounded. Magic bytes decide what a file *is* (§6): a text
//! file past [`PREVIEW_READ_CAP`] is not previewed at all — it takes the **File
//! Message** lane under a text label, because its content is still text — and a
//! file past Feishu's 30 MB upload cap is classified from its header alone and
//! never read. A text preview reads at most the cap and the card layer holds it
//! to its own budget, so a huge file can neither exhaust memory nor produce a
//! rejected card. Emptiness and the read bound are decided by the **bytes
//! actually read**, never by the `stat` size: a pseudo-file (procfs, sysfs)
//! reports size 0 while having content.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::FileContent;
use crate::bridge::handles::{CardsHandle, content_cell, content_identity_hash};
use crate::feishu::Platform;
use crate::feishu::card::tool_render::FileDelivery;
use crate::feishu::card::viewer::ViewerFileLane;

/// The most bytes a preview reads (ADR-0083 §6): 8 MB. A text file past it is
/// not previewed at all — it takes the File Message lane (ADR-0083 §5).
pub(crate) const PREVIEW_READ_CAP: usize = 8 * 1024 * 1024;

/// How many bytes [`classify`] sniffs for magic bytes (ADR-0083 §6): well past
/// the longest signature `infer` carries, and a bounded, cheap read.
const SNIFF_BYTES: usize = 8192;

/// The bound on ONE network call of the whole-bytes lane (ADR-0083 §5): the
/// upload and the File Message send each get this, so the 「发为文件」 transfer
/// the click spawns off the card callback can neither run forever nor leave the
/// clicker with a dropped result. The default lives on
/// [`crate::bridge::core::SharedCore`] so a test can shrink it and observe a
/// timeout.
pub(crate) const VIEWER_TRANSFER_TIMEOUT_MS: u64 = 60_000;

/// Resolve a user-supplied viewer path on the **Path Scope** (ADR-0083 §4):
/// `~` expands against home, a relative path joins `session_dir`, an absolute
/// path passes through untouched. The result is canonicalized when it exists —
/// so the card names the **resolved** path (symlinks and `..` included), never
/// the typed spelling — and otherwise keeps the lexical absolute form for the
/// caller to report as missing.
///
/// Advisory only, exactly like the Directory Browser's reach: no boundary is
/// enforced here (#673 owns that work).
pub(crate) fn resolve_cat_path(input: &str, session_dir: &str) -> PathBuf {
    let expanded = crate::bridge::command::expand_against(input, Path::new(session_dir));
    std::fs::canonicalize(&expanded).unwrap_or_else(|_| crate::bridge::command::lexical_normalize(&expanded))
}

/// What a resolved path turned out to be.
#[derive(Debug)]
pub(crate) enum CatOpen {
    /// A directory: the answer is the Directory Browser there, never a viewer.
    Directory(PathBuf),
    /// A file the viewer opens; `size` is the byte count it names.
    File { path: PathBuf, size: u64 },
    /// Nothing to show, with the honest cause.
    Failed(CatFailure),
}

/// Stat a resolved path (symlinks followed) and classify what is there.
pub(crate) fn open_cat_path(path: &Path) -> CatOpen {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => CatOpen::Directory(path.to_path_buf()),
        Ok(meta) => CatOpen::File {
            path: path.to_path_buf(),
            size: meta.len(),
        },
        Err(e) => CatOpen::Failed(stat_failure(&e)),
    }
}

/// The failure a `stat` error answers with: a vanished path is 找不到, every
/// other error (a denied `stat`) is 无法读取 with the OS's own words (#669
/// stories 31/33). Shared by [`open_cat_path`] and the 「发为文件」 transfer, so
/// a path that disappears between the viewer and the click answers the same way
/// it would have on open.
pub(crate) fn stat_failure(error: &std::io::Error) -> CatFailure {
    match error.kind() {
        std::io::ErrorKind::NotFound => CatFailure::Missing,
        _ => CatFailure::Unreadable(error.to_string()),
    }
}

/// Why a path could not be shown (ADR-0083 §2). Every cause answers its own
/// line and every line names the path — an unopenable file is a stated reason,
/// never an empty card. A non-UTF-8 file is no longer a failure: it is a byte
/// lane (#739), so it answers with a viewer card whose tracking line says where
/// its bytes went.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CatFailure {
    /// The resolved path does not exist.
    Missing,
    /// The path exists but cola cannot read it — a permission denial, an I/O
    /// error. `reason` is the OS's own words.
    Unreadable(String),
    /// The file is zero bytes.
    Empty,
}

impl CatFailure {
    /// The one line this cause answers with, naming `path`.
    pub(crate) fn line(&self, path: &Path) -> String {
        let shown = path.display();
        match self {
            Self::Missing => format!("⚠️ 找不到路径 `{shown}`。"),
            Self::Unreadable(reason) => format!("⚠️ 无法读取 `{shown}`：{reason}"),
            Self::Empty => format!("📄 `{shown}` 是空文件（0 字节）。"),
        }
    }
}

/// What a file's bytes *are* (ADR-0083 §6): magic bytes decide an image or a
/// known binary first, then UTF-8 validity decides text.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum ClassifiedKind {
    /// An image, by magic bytes: the embed lane when Feishu accepts it, else
    /// the File Message lane.
    Image,
    /// UTF-8 text within the preview read cap: the card's preview.
    Text,
    /// UTF-8 text PAST the preview read cap: still text, just too large to
    /// preview — the File Message lane under a 「文本（过大，未预览）」 label,
    /// never 「二进制」. Whether it is text is decided by the content: the whole
    /// file when it is within Feishu's upload cap, else its bounded header.
    TextTooLarge,
    /// A non-text file (a known binary, or an unknown non-UTF-8 file): no
    /// preview, delivered as a File Message.
    File,
}

/// A Host file read locally and classified by **content**, never by name
/// (ADR-0083 §1, §6).
pub(crate) struct Classified {
    /// The file's byte size: the number the card names. For a file whose bytes
    /// were read this is the BYTES' OWN length — the same number
    /// [`FileContent::from_bytes`] hands the Platform — so a file that changed
    /// between the `stat` and the read is never named with a stale size and the
    /// two never drift. For a file past Feishu's 30 MB upload cap (never read)
    /// it is the `stat` size.
    pub(crate) size: u64,
    /// The content's mime — `infer`'s answer when magic bytes named it, else
    /// derived from the header's own shape.
    pub(crate) mime: String,
    /// What the bytes are.
    pub(crate) kind: ClassifiedKind,
    /// The preview text, for a UTF-8 text file within the preview read cap.
    pub(crate) text: Option<String>,
    /// The whole bytes for a delivery lane that hands them to the Platform (an
    /// image or a file), when the file is within Feishu's 30MB upload cap. `None`
    /// for a text preview (which holds only its decoded text) and for a file past
    /// the cap (never read — the card says 未发送).
    pub(crate) bytes: Option<Vec<u8>>,
}

/// What a bounded header's bytes say about a file: `infer`'s name for the magic
/// it found, whether that magic was an image or a known binary (an `infer` Text
/// matcher — html, xml, a shell script — is still TEXT), and whether the header
/// itself is UTF-8 text.
struct Sniff {
    mime_from_infer: Option<String>,
    is_image: bool,
    is_known_binary: bool,
    is_text: bool,
}

impl Sniff {
    /// The mime a byte-carrying lane reports when only a header could name it:
    /// `infer`'s answer when magic bytes found one, else `text/plain` for a
    /// text-shaped header and the generic octet type otherwise.
    fn octet_mime(&self) -> String {
        self.mime_from_infer.clone().unwrap_or_else(|| {
            if self.is_text {
                "text/plain".to_string()
            } else {
                crate::backend::file_content::DEFAULT_FILE_MIME.to_string()
            }
        })
    }
}

/// Sniff a bounded header (ADR-0083 §6): magic bytes decide an image or a known
/// binary first, then the header's own UTF-8 shape decides whether it *looks*
/// like text. Callers pass at most [`SNIFF_BYTES`] bytes — the depth `infer`
/// itself reads — so this is a bounded, cheap read's worth of work.
fn sniff(header: &[u8]) -> Sniff {
    let inferred = infer::get(header);
    Sniff {
        mime_from_infer: inferred.map(|kind| kind.mime_type().to_string()),
        is_image: inferred.is_some_and(|kind| kind.matcher_type() == infer::MatcherType::Image),
        // `infer`'s Text matchers (html, xml, a shell script) are still TEXT:
        // they must fall through to the UTF-8 decision rather than being called
        // binary.
        is_known_binary: inferred.is_some_and(|kind| {
            !matches!(
                kind.matcher_type(),
                infer::MatcherType::Image | infer::MatcherType::Text
            )
        }),
        is_text: is_text_bytes(header),
    }
}

/// Read `path` and decide what its bytes are (ADR-0083 §6): magic bytes
/// (image / known binary) are decided first, then UTF-8 validity decides text.
/// A text file past the preview cap is never previewed, but it is still TEXT:
/// it takes the File Message lane under a text label (a text-shaped header, for
/// a file past Feishu's upload cap, is the best a never-read file can be named
/// by).
///
/// A file **within** Feishu's upload cap is read **ONCE** and classified from
/// exactly those bytes, which are then handed to the delivery lane — so a file
/// that changed between two reads can never be described by one read's bytes
/// and delivered from another's, and no byte within the cap is read twice. A
/// file **past** the cap is never delivered, so a single bounded header is all
/// that is read to name it.
///
/// Emptiness is decided by the **bytes actually read**, never by the `stat`
/// size: a pseudo-file (procfs, sysfs) reports size 0 in its metadata while
/// having content. The read is bounded by Feishu's upload cap rather than by
/// the `stat` size for the same reason — a file that grew (or a size-0
/// pseudo-file) is still bounded, and the reported size is the bytes' own
/// length.
pub(crate) fn classify(path: &Path, size: u64) -> Result<Classified, CatFailure> {
    // Past Feishu's 30MB upload cap (by the caller's `stat`): nothing can be
    // delivered, so ONE bounded header is all this reads — the sniff is the only
    // thing that can name a never-read file. A text-shaped header is still TEXT
    // — too large to preview — never 「二进制」.
    if size > crate::feishu::file::MAX_FILE_BYTES {
        let header = read_prefix(path, SNIFF_BYTES)?;
        count_read(header.len());
        if header.is_empty() {
            return Err(CatFailure::Empty);
        }
        let sniffed = sniff(&header);
        return Ok(past_cap(
            size,
            sniffed.octet_mime(),
            sniffed.is_image,
            sniffed.is_known_binary,
            sniffed.is_text,
        ));
    }

    // Within Feishu's upload cap: read the file ONCE, bounded by that cap (plus
    // the one byte that detects growth), and let exactly those bytes decide
    // everything. The bytes' own length is the truth the card reports (`size`
    // below), so a file that shrank — or a pseudo-file whose `stat` size is 0 —
    // is named by what was actually read, and `FileContent::from_bytes`
    // receives those same bytes.
    let Some(bytes) = read_bounded(path, crate::feishu::file::MAX_FILE_BYTES)? else {
        // A file that GREW past the cap after the caller's `stat` reads one
        // byte over it and is reported past-cap — never as its truncated
        // prefix — so the card says 未发送 instead of claiming a partial file
        // was sent. Nothing is deliverable, so only a bounded header names it.
        let grown = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(size);
        let header = read_prefix(path, SNIFF_BYTES)?;
        count_read(header.len());
        let sniffed = sniff(&header);
        return Ok(past_cap(
            grown,
            sniffed.octet_mime(),
            sniffed.is_image,
            sniffed.is_known_binary,
            sniffed.is_text,
        ));
    };
    count_read(bytes.len());
    if bytes.is_empty() {
        return Err(CatFailure::Empty);
    }
    // The sniff sees only a header-BOUNDED prefix of the read bytes (the depth
    // `infer` reads), while the text decision below sees the whole file.
    let sniffed = sniff(&bytes[..bytes.len().min(SNIFF_BYTES)]);
    let size = bytes.len() as u64;
    if sniffed.is_image {
        return Ok(Classified {
            size,
            mime: sniffed.octet_mime(),
            kind: ClassifiedKind::Image,
            text: None,
            bytes: Some(bytes),
        });
    }
    // A NUL byte means this is not text whatever its encoding — rendering it
    // would put a control character into the card (issue #519's family), and
    // the byte lane is where it belongs.
    let is_text = !sniffed.is_known_binary && is_text_bytes(&bytes);
    if is_text && size <= PREVIEW_READ_CAP as u64 {
        let text = String::from_utf8(bytes).expect("validated as UTF-8");
        return Ok(Classified {
            size,
            mime: "text/plain".to_string(),
            kind: ClassifiedKind::Text,
            text: Some(text),
            bytes: None,
        });
    }
    if is_text {
        // UTF-8 text past the preview read cap: TEXT, too large to preview. It
        // takes the File Message lane under a 「文本（过大，未预览）」 label
        // rather than being mislabelled 二进制.
        return Ok(Classified {
            size,
            mime: "text/plain".to_string(),
            kind: ClassifiedKind::TextTooLarge,
            text: None,
            bytes: Some(bytes),
        });
    }
    // The whole file failed the UTF-8 test, so its header's shape must not label
    // it `text/plain`: only `infer` may name it, else it is a generic binary.
    let mime = sniffed
        .mime_from_infer
        .unwrap_or_else(|| crate::backend::file_content::DEFAULT_FILE_MIME.to_string());
    Ok(Classified {
        size,
        mime,
        kind: ClassifiedKind::File,
        text: None,
        bytes: Some(bytes),
    })
}

#[cfg(test)]
thread_local! {
    /// Bytes [`classify`] read for the calling thread — test-only instrumentation
    /// for ADR-0083 §6's single-read invariant (finding #739 §1, round 2). A
    /// thread-local keeps each test to its own reads, so the parallel test
    /// threads cannot see each other's parses.
    static CLASSIFY_BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Record that [`classify`] read `n` bytes from a file (tests only; a no-op in
/// the production build).
#[inline]
fn count_read(_n: usize) {
    #[cfg(test)]
    CLASSIFY_BYTES_READ.with(|count| count.set(count.get() + _n as u64));
}

/// Whether `bytes` are UTF-8 text: valid UTF-8 and carrying no NUL byte (a NUL
/// is a control character the card must never hold — issue #519's family).
fn is_text_bytes(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// The classification for a file whose bytes are past Feishu's 30 MB upload
/// cap, whether the caller's `stat` said so or the read discovered it (the file
/// grew): nothing is read, so only the sniffed header can name it. A
/// text-shaped header is still TEXT — too large to preview — never 「二进制」;
/// `size` is the best figure available (the `stat`, or a fresh one for a file
/// that grew) and carries no bytes, so every lane reports 未发送.
fn past_cap(
    size: u64,
    mime: String,
    is_image: bool,
    is_known_binary: bool,
    header_is_text: bool,
) -> Classified {
    let kind = if is_image {
        ClassifiedKind::Image
    } else if !is_known_binary && header_is_text {
        ClassifiedKind::TextTooLarge
    } else {
        ClassifiedKind::File
    };
    Classified {
        size,
        mime,
        kind,
        text: None,
        bytes: None,
    }
}

/// The whole-bytes lane a viewer opens with (ADR-0083 §5): an image within
/// Feishu's caps uploads once and is embedded, anything else uploads once and is
/// posted as exactly ONE File Message, and a file past the 30MB cap — or a
/// failed upload/send — is 未发送. `None` for a text preview, which takes no
/// lane on open.
pub(crate) async fn open_lane(
    platform: &Arc<dyn Platform>,
    cards: &CardsHandle,
    reply_target: &str,
    name: &str,
    classified: &mut Classified,
    timeout_ms: u64,
) -> Option<ViewerFileLane> {
    if classified.kind == ClassifiedKind::Text {
        return None;
    }
    let (mime, size) = (classified.mime.clone(), classified.size);
    let Some(bytes) = classified.bytes.take() else {
        // Past Feishu's 30MB cap: nothing was read, nothing can be uploaded.
        return Some(ViewerFileLane {
            mime,
            name: name.to_string(),
            size,
            delivery: FileDelivery::Undelivered,
        });
    };
    let content = FileContent::from_bytes(&mime, name, bytes);
    let delivery = deliver(
        platform,
        cards,
        reply_target,
        classified.kind,
        content,
        timeout_ms,
    )
    .await;
    Some(ViewerFileLane {
        mime,
        name: name.to_string(),
        size,
        delivery,
    })
}

/// The 「发为文件」 transfer (ADR-0083 §5): read the file's whole bytes and post
/// them as exactly ONE File Message, whatever the file is — this is the action
/// that gets a file whose preview fits nowhere out of the chat, and the one that
/// reaches a text preview's or an embedded image's **full** bytes. Reads at most
/// Feishu's 30MB cap; past it (or on a failed upload/send) the lane is 未发送 and
/// no message is posted. Off the card callback, bounded by `timeout_ms` per
/// network call.
pub(crate) async fn send_as_file(
    platform: &Arc<dyn Platform>,
    cards: &CardsHandle,
    reply_target: &str,
    path: &Path,
    name: &str,
    classified: &mut Classified,
    timeout_ms: u64,
) -> ViewerFileLane {
    let (mime, stat_size) = (classified.mime.clone(), classified.size);
    // Reuse the bytes the open already read (an image/binary lane), and read
    // them for a text preview, whose open held only the decoded text.
    let bytes = match classified.bytes.take() {
        Some(bytes) => Some(bytes),
        None => read_deliverable(path, stat_size).ok().flatten(),
    };
    // The lane names the bytes that actually go out, never the earlier `stat`
    // size: a file that shrank between the two is described by what was handed
    // to the Platform (the same number `FileContent::from_bytes` carries).
    let size = bytes.as_ref().map_or(stat_size, |bytes| bytes.len() as u64);
    let delivery = match bytes {
        Some(bytes) => {
            let content = FileContent::from_bytes(&mime, name, bytes);
            deliver(
                platform,
                cards,
                reply_target,
                ClassifiedKind::File,
                content,
                timeout_ms,
            )
            .await
        }
        None => FileDelivery::Undelivered,
    };
    ViewerFileLane {
        mime,
        name: name.to_string(),
        size,
        delivery,
    }
}

/// Upload `bytes` once (content-hash cached, single-flight across the process)
/// and deliver them: an image the Platform accepts is embedded, anything else is
/// posted as exactly ONE File Message under `reply_target`. Every failure
/// degrades to [`FileDelivery::Undelivered`] — never an error, never a duplicate
/// message.
///
/// Deliberately NOT the same function as the transcript path's
/// `turn::render::resolve_file_delivery`, though both walk an
/// image→File-Message→未发送 ladder (ADR-0076 vs ADR-0083 §5): this one bounds
/// every platform call by the transfer's own timeout, replies under the
/// command's message id rather than a live card it must re-validate, and keeps
/// no `(session, content)` once-guard (an explicit 「发为文件」 click is a
/// deliberate re-send, not a render poll). Extracting the shared shape would
/// need all three differences parameterized, an interface wider than the
/// duplication it removes — so the two stay separate on purpose.
async fn deliver(
    platform: &Arc<dyn Platform>,
    cards: &CardsHandle,
    reply_target: &str,
    kind: ClassifiedKind,
    content: FileContent,
    timeout_ms: u64,
) -> FileDelivery {
    let hash = content_identity_hash(
        &content.mime,
        &content.name,
        content.data.as_deref().unwrap_or_default(),
    );
    if kind == ClassifiedKind::Image {
        let cell = content_cell(&cards.file_images, hash).await;
        let key = cell
            .cell()
            .get_or_init(|| async {
                // Infallible on purpose (ADR-0076 review, B1): a failed or
                // timed-out embed settles the cell as `None`, the same verdict a
                // decided non-image gets, so a concurrent resolver cannot embed
                // while this one sends a File Message.
                match crate::bridge::bounded_call(
                    "viewer image upload",
                    timeout_ms,
                    platform.upload_image(&content),
                )
                .await
                {
                    Some(Ok(key)) => key,
                    Some(Err(error)) => {
                        tracing::warn!("viewer image upload failed: {error}");
                        None
                    }
                    None => None,
                }
            })
            .await
            .clone();
        if let Some(image_key) = key {
            return FileDelivery::Embedded { image_key };
        }
        // A non-embeddable image (past the 10MB / dimension caps) falls through
        // to the File Message lane.
    }
    send_file_message(platform, cards, reply_target, &content, hash, timeout_ms).await
}

/// Upload `content` once (content-hash cached) and post exactly ONE File Message
/// carrying its key under `reply_target`. `Undelivered` past Feishu's 30MB cap,
/// on a failed upload, or on a failed/timed-out send.
async fn send_file_message(
    platform: &Arc<dyn Platform>,
    cards: &CardsHandle,
    reply_target: &str,
    content: &FileContent,
    hash: u64,
    timeout_ms: u64,
) -> FileDelivery {
    if !crate::feishu::file::deliverable_file(content) {
        return FileDelivery::Undelivered;
    }
    let cell = content_cell(&cards.file_uploads, hash).await;
    let file_key = match cell
        .cell()
        .get_or_try_init(|| async {
            match crate::bridge::bounded_call("viewer file upload", timeout_ms, platform.upload_file(content))
                .await
            {
                Some(Ok(Some(file_key))) => Ok(Some(file_key)),
                Some(Ok(None)) => Ok(None),
                Some(Err(error)) => {
                    tracing::warn!("viewer file upload failed: {error}");
                    Err(())
                }
                None => Err(()),
            }
        })
        .await
    {
        Ok(Some(file_key)) => file_key.clone(),
        // Past Feishu's cap, a failed upload, or a timeout.
        Ok(None) | Err(()) => return FileDelivery::Undelivered,
    };
    let payload = serde_json::json!({ "file_key": file_key });
    match crate::bridge::bounded_call(
        "viewer file message",
        timeout_ms,
        platform.reply_message(reply_target, "file", &payload),
    )
    .await
    {
        Some(Ok(_)) => FileDelivery::SentAsFile,
        Some(Err(error)) => {
            tracing::warn!("viewer File Message send failed: {error}");
            FileDelivery::Undelivered
        }
        None => FileDelivery::Undelivered,
    }
}

/// The whole bytes for a delivery lane, or `None` when the file is past
/// Feishu's 30MB upload cap — which `stat_size` may already say, or which the
/// read itself discovers when the file grew after that `stat`. Either way a
/// truncated prefix is never returned, so a partial file is never delivered as
/// the whole. The read itself is bounded by the cap (never by `stat_size`), so
/// a size-0 pseudo-file still yields its content.
fn read_deliverable(path: &Path, stat_size: u64) -> Result<Option<Vec<u8>>, CatFailure> {
    if stat_size > crate::feishu::file::MAX_FILE_BYTES {
        return Ok(None);
    }
    read_bounded(path, crate::feishu::file::MAX_FILE_BYTES)
}

/// Read `path`, at most `limit` bytes, reporting the OS's reason when it cannot
/// be read. The bound is the CALLER's, never the `stat` size: a pseudo-file
/// (procfs, sysfs) reports size 0 while having content, and a file that grows
/// between the `stat` and this read is still bounded.
///
/// Reads ONE byte past `limit` to detect that more bytes remain, and `None`
/// means exactly that: the file holds more than `limit` bytes. A file that
/// outgrew the cap after the caller's `stat` is therefore reported past-cap
/// rather than delivered as a truncated prefix (finding #739 §1, round 2).
/// `Some(bytes)` is the whole file, at most `limit` bytes.
fn read_bounded(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, CatFailure> {
    let file = std::fs::File::open(path).map_err(|e| CatFailure::Unreadable(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| CatFailure::Unreadable(e.to_string()))?;
    if bytes.len() as u64 > limit {
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// Read at most `n` bytes — the bounded header [`classify`] sniffs.
fn read_prefix(path: &Path, n: usize) -> Result<Vec<u8>, CatFailure> {
    let file = std::fs::File::open(path).map_err(|e| CatFailure::Unreadable(e.to_string()))?;
    let mut bytes = Vec::with_capacity(n);
    file.take(n as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| CatFailure::Unreadable(e.to_string()))?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The Path Scope (ADR-0083 §4): a relative path resolves against the
    /// SESSION's directory — the base it is handed, never an ambient one —
    /// while an absolute path is passed through and `~` expands against home.
    #[test]
    fn resolve_cat_path_follows_the_path_scope() {
        let session = tempfile::tempdir().unwrap();
        let session_dir = session.path().to_string_lossy().to_string();

        let relative = resolve_cat_path("notes/today.md", &session_dir);
        assert_eq!(relative, session.path().join("notes").join("today.md"));

        // The base is what decides: the same relative path under another base
        // resolves elsewhere, so nothing ambient (the process cwd, which the
        // integration tests move around) can be what resolved it.
        let other = tempfile::tempdir().unwrap();
        let other_dir = other.path().to_string_lossy().to_string();
        assert_eq!(
            resolve_cat_path("notes/today.md", &other_dir),
            other.path().join("notes").join("today.md")
        );
        assert_ne!(relative, resolve_cat_path("notes/today.md", &other_dir));

        let outside = tempfile::tempdir().unwrap();
        let absolute = outside.path().join("elsewhere.txt");
        assert_eq!(
            resolve_cat_path(&absolute.to_string_lossy(), &session_dir),
            absolute,
            "an absolute path passes through, session dir or not"
        );

        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolve_cat_path("~/notes.md", &session_dir),
            home.join("notes.md")
        );
        assert_eq!(resolve_cat_path("~", &session_dir), home);
    }

    /// An existing path resolves to its canonical spelling — symlinks and `..`
    /// followed — so the card names the file it actually opened.
    #[test]
    fn resolve_cat_path_canonicalizes_what_exists_and_keeps_what_does_not() {
        let session = tempfile::tempdir().unwrap();
        let session_dir = session.path().to_string_lossy().to_string();
        let file = session.path().join("real.txt");
        std::fs::write(&file, "hi").unwrap();

        // `real.txt/../real.txt` exists as a file only after the `..` is
        // resolved, which canonicalize does and a lexical join does not.
        let spellings = resolve_cat_path("./real.txt", &session_dir);
        assert_eq!(spellings, std::fs::canonicalize(&file).unwrap());

        let missing = resolve_cat_path("nope/missing.txt", &session_dir);
        assert_eq!(missing, session.path().join("nope").join("missing.txt"));
    }

    /// The three distinct failures (ADR-0083 §2) each answer their own line and
    /// every line names the resolved path. A non-UTF-8 file is no longer among
    /// them: it is a byte lane (#739), not a failure.
    #[test]
    fn cat_failures_answer_distinctly_and_name_the_path() {
        let path = Path::new("/tmp/some/file.txt");
        let missing = CatFailure::Missing.line(path);
        let unreadable = CatFailure::Unreadable("Permission denied (os error 13)".into()).line(path);
        let empty = CatFailure::Empty.line(path);

        for line in [&missing, &unreadable, &empty] {
            assert!(
                line.contains("/tmp/some/file.txt"),
                "the line names the path: {line}"
            );
        }
        assert!(missing.contains("找不到"), "{missing}");
        assert!(
            unreadable.contains("无法读取") && unreadable.contains("Permission denied"),
            "{unreadable}"
        );
        assert!(empty.contains("空文件") && empty.contains("0 字节"), "{empty}");
        // Distinct: no two causes share their copy.
        let mut lines: Vec<String> = vec![missing, unreadable, empty];
        lines.sort();
        let before = lines.len();
        lines.dedup();
        assert_eq!(lines.len(), before, "each cause has its own answer");
    }

    /// A directory is a browse target, a file reports its own size, and a
    /// path whose `stat` fails reports the OS's reason.
    #[test]
    fn open_cat_path_classifies_directory_file_and_failure() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(open_cat_path(dir.path()), CatOpen::Directory(_)));

        let file = dir.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();
        match open_cat_path(&file) {
            CatOpen::File { path, size } => {
                assert_eq!(path, file);
                assert_eq!(size, 5);
            }
            other => panic!("expected a file, got {other:?}"),
        }

        assert!(matches!(
            open_cat_path(&dir.path().join("ghost")),
            CatOpen::Failed(CatFailure::Missing)
        ));
    }

    /// A tiny, structurally real PNG header (the 8-byte signature plus an IHDR
    /// declaring `width`×`height`) — the magic bytes `infer` reads.
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
        bytes
    }

    fn write(path: &Path, bytes: &[u8]) -> u64 {
        std::fs::write(path, bytes).unwrap();
        std::fs::metadata(path).unwrap().len()
    }

    /// Classification is by CONTENT, never by name (ADR-0083 §6): a PNG named
    /// `.txt` is an image, a text file named `.png` is text, a non-UTF-8 file
    /// named `.txt` is a binary, and a known binary (a PDF header) is a file
    /// whatever its name says. A non-UTF-8 file is NEVER rendered as text.
    #[test]
    fn classify_decides_by_magic_bytes_and_utf8_validity_not_by_name() {
        let dir = tempfile::tempdir().unwrap();

        let image = dir.path().join("actually-an-image.txt");
        let size = write(&image, &png(4, 4));
        let classified = classify(&image, size).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::Image, "magic bytes win");
        assert_eq!(classified.mime, "image/png");
        assert!(classified.text.is_none());
        assert_eq!(classified.bytes.as_deref(), Some(png(4, 4).as_slice()));

        // A text file with an image's name is TEXT: UTF-8 validity decides.
        let text = dir.path().join("actually-text.png");
        let size = write(&text, b"# just text\n");
        let classified = classify(&text, size).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::Text);
        assert_eq!(classified.text.as_deref(), Some("# just text\n"));
        assert!(classified.bytes.is_none(), "a text preview holds no bytes");

        // A non-UTF-8 file is a binary, never text.
        let binary = dir.path().join("looks-like-a-text-file.txt");
        let size = write(&binary, &[0xff, 0xfe, 0x00, 0x01]);
        let classified = classify(&binary, size).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::File);
        assert!(classified.text.is_none(), "a non-UTF-8 file is never text");
        assert_eq!(
            classified.bytes.as_deref(),
            Some([0xff, 0xfe, 0x00, 0x01].as_slice())
        );

        // A known binary (a PDF magic) is a file even though it is UTF-8-ish.
        let pdf = dir.path().join("report.txt");
        let size = write(&pdf, b"%PDF-1.7\nplain-ish");
        let classified = classify(&pdf, size).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::File);
        assert_eq!(classified.mime, "application/pdf");

        // A NUL byte means not text, whatever the rest decodes to.
        let nul = dir.path().join("nul.txt");
        let size = write(&nul, b"hello\0world");
        assert_eq!(classify(&nul, size).unwrap().kind, ClassifiedKind::File);
    }

    /// A text file past the preview read cap is NOT previewed — but it is still
    /// TEXT, so it takes the File lane under a text label, never 二进制. A file
    /// past Feishu's 30MB upload cap is classified from its header alone,
    /// carrying no bytes at all (ADR-0083 §5, §6).
    #[test]
    fn classify_routes_a_past_cap_file_to_the_file_lane_and_never_previews_it() {
        let dir = tempfile::tempdir().unwrap();

        // A real sparse file, one byte past the preview cap: the bytes are read
        // (they decide text-ness), but the preview never is.
        let big = dir.path().join("big.log");
        let file = std::fs::File::create(&big).unwrap();
        file.set_len(PREVIEW_READ_CAP as u64 + 1).unwrap();
        drop(file);
        let classified = classify(&big, std::fs::metadata(&big).unwrap().len()).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::File, "past the cap, no preview");
        assert!(classified.text.is_none());

        // A real UTF-8 text file just past the preview cap: TEXT too large to
        // preview, NOT 二进制 — the finding this pins.
        let big_text = dir.path().join("big_text.log");
        let size = write(&big_text, &vec![b'a'; PREVIEW_READ_CAP + 1]);
        let classified = classify(&big_text, size).unwrap();
        assert_eq!(
            classified.kind,
            ClassifiedKind::TextTooLarge,
            "a big UTF-8 file is still text, just too large to preview"
        );
        assert!(classified.text.is_none(), "never previewed");
        assert_eq!(classified.size, size);
        assert_eq!(
            classified.bytes.as_ref().map(Vec::len),
            Some(size as usize),
            "its whole bytes take the File lane"
        );

        // Exactly at the preview cap stays text (the other side of the same
        // boundary): real text bytes, not a sparse hole.
        let at_cap = dir.path().join("at_cap.log");
        let size = write(&at_cap, &vec![b'a'; PREVIEW_READ_CAP]);
        let classified = classify(&at_cap, size).unwrap();
        assert_eq!(
            classified.kind,
            ClassifiedKind::Text,
            "exactly at the cap previews"
        );
        assert_eq!(classified.text.as_deref().map(str::len), Some(PREVIEW_READ_CAP));

        // A file past the 30MB upload cap: no bytes are ever read. A text-shaped
        // header still names it TEXT (too large to preview), not 二进制.
        let huge = dir.path().join("huge.bin");
        let file = std::fs::File::create(&huge).unwrap();
        file.set_len(crate::feishu::file::MAX_FILE_BYTES + 1).unwrap();
        drop(file);
        let size = std::fs::metadata(&huge).unwrap().len();
        let classified = classify(&huge, size).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::File);
        assert!(classified.bytes.is_none(), "past 30MB, nothing is read");
        assert_eq!(classified.size, size, "the card still names the real size");

        let huge_text = dir.path().join("huge.log");
        {
            use std::io::Write as _;
            let mut file = std::fs::File::create(&huge_text).unwrap();
            // A text-shaped header — the whole sniff window — then sparse zeros
            // out to past the 30MB cap, so nothing has to be read.
            file.write_all(&vec![b'a'; SNIFF_BYTES]).unwrap();
            file.set_len(crate::feishu::file::MAX_FILE_BYTES + 1).unwrap();
        }
        let size = std::fs::metadata(&huge_text).unwrap().len();
        let classified = classify(&huge_text, size).unwrap();
        assert_eq!(
            classified.kind,
            ClassifiedKind::TextTooLarge,
            "a text header past 30MB is still text, not 二进制"
        );
        assert!(classified.bytes.is_none(), "past 30MB, nothing is read");
    }

    /// A file that GREW past a read limit after the caller's `stat` is detected
    /// by the one-byte-over read: it is never returned as a truncated prefix,
    /// so a 「发为文件」 click can never report a partial file as sent
    /// (finding #739 §1, round 2).
    #[test]
    fn read_bounded_detects_a_file_that_grew_past_its_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("grew.bin");
        std::fs::write(&file, b"0123456789").unwrap();

        // Exactly the limit, and under it, is the whole file.
        assert_eq!(read_bounded(&file, 10).unwrap().unwrap(), b"0123456789".to_vec());
        assert_eq!(read_bounded(&file, 11).unwrap().unwrap(), b"0123456789".to_vec());
        // One byte past the limit means MORE bytes remain: reported as `None`,
        // never as a 5-byte prefix of a 10-byte file.
        assert_eq!(read_bounded(&file, 5).unwrap(), None);
    }

    /// ADR-0083 §6: a file WITHIN the 30MB upload cap is read **once**, and
    /// exactly those bytes both decide its type and reach the delivery lane — so
    /// a file changing between two reads can never be described by one read's
    /// bytes and delivered from another's. Pinned by counting the bytes
    /// `classify` actually read: a second header read would show up as a count
    /// above the file's own length, and for a bytes lane the delivered bytes must
    /// BE the read bytes (finding #739 §1, round 2).
    #[test]
    fn classify_reads_a_file_within_the_cap_once() {
        let dir = tempfile::tempdir().unwrap();

        // The bytes lane: the delivered bytes are exactly the classified bytes,
        // and nothing is read beyond them.
        let binary = dir.path().join("blob.bin");
        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let size = write(&binary, &payload);
        CLASSIFY_BYTES_READ.with(|count| count.set(0));
        let classified = classify(&binary, size).unwrap();
        assert_eq!(
            classified.bytes.as_deref(),
            Some(payload.as_slice()),
            "the delivered bytes ARE the classified bytes"
        );
        assert_eq!(
            CLASSIFY_BYTES_READ.with(|count| count.get()),
            size,
            "the file is read exactly once"
        );

        // The text lane: the preview is the read text, and the read is still one.
        let text = dir.path().join("note.txt");
        let body = b"one read, one classification";
        let size = write(&text, body);
        CLASSIFY_BYTES_READ.with(|count| count.set(0));
        let classified = classify(&text, size).unwrap();
        assert_eq!(classified.text.as_deref(), Some("one read, one classification"));
        assert_eq!(
            CLASSIFY_BYTES_READ.with(|count| count.get()),
            size,
            "the text preview is also read exactly once"
        );

        // A file past the cap is never read: one bounded header names it.
        let huge = dir.path().join("huge.bin");
        let handle = std::fs::File::create(&huge).unwrap();
        handle.set_len(crate::feishu::file::MAX_FILE_BYTES + 1).unwrap();
        drop(handle);
        let size = std::fs::metadata(&huge).unwrap().len();
        CLASSIFY_BYTES_READ.with(|count| count.set(0));
        let classified = classify(&huge, size).unwrap();
        assert!(classified.bytes.is_none(), "a past-cap file delivers nothing");
        assert_eq!(
            CLASSIFY_BYTES_READ.with(|count| count.get()),
            SNIFF_BYTES as u64,
            "a past-cap file is named from one bounded header, never read"
        );
    }

    /// The same growth at the two callers that guard the 30MB cap: `classify`
    /// reads one byte past it and reports the file past-cap (no bytes to
    /// deliver, the real grown size named), and `read_deliverable` returns no
    /// bytes for it — so the lane can only be 未发送, never a truncated send.
    #[test]
    fn a_file_that_grew_past_the_cap_is_classified_and_delivered_as_past_cap() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("grew.bin");
        // A sparse file one byte past Feishu's cap, while the caller's `stat`
        // (simulated by the small `size` argument) still says it fits.
        let handle = std::fs::File::create(&file).unwrap();
        handle.set_len(crate::feishu::file::MAX_FILE_BYTES + 1).unwrap();
        drop(handle);

        let classified = classify(&file, 10).unwrap();
        assert!(
            classified.bytes.is_none(),
            "a file that grew past the cap holds no deliverable bytes"
        );
        assert_eq!(
            classified.size,
            crate::feishu::file::MAX_FILE_BYTES + 1,
            "the grown size is the one named, not the stale stat"
        );
        assert_eq!(
            classified.kind,
            ClassifiedKind::File,
            "a NUL header is a file, never text"
        );
        assert_eq!(
            read_deliverable(&file, 10).unwrap(),
            None,
            "no truncated prefix is ever delivered"
        );
    }

    /// A size-0 pseudo-file (procfs, sysfs) has real content: emptiness is
    /// decided by the bytes actually read, never by the `stat` size, so
    /// `/proc/cpuinfo` answers a text preview instead of 「空文件」.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_size_zero_pseudo_file_is_classified_by_its_content() {
        let path = Path::new("/proc/self/status");
        if std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(1) != 0 {
            return; // no procfs-metadata-0 file on this machine to observe
        }
        let classified = classify(path, 0).expect("a size-0 pseudo-file with content is not empty");
        assert_eq!(
            classified.kind,
            ClassifiedKind::Text,
            "procfs text is text, whatever its metadata size says"
        );
        let text = classified.text.as_deref().unwrap_or_default();
        assert!(!text.is_empty(), "the content is read");
        assert_eq!(
            classified.size,
            text.len() as u64,
            "the card names the bytes actually read"
        );
    }

    /// The reported size is the BYTES' own length, never the caller's `stat`
    /// figure: a file that shrank between the `stat` and the read is described
    /// by what was actually read — the same bytes `FileContent::from_bytes`
    /// hands the Platform — so the card and the upload can never disagree.
    #[test]
    fn classify_names_the_bytes_read_not_the_stat_size() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("shrank.log");
        write(&file, b"tiny");
        let classified = classify(&file, 4096).unwrap();
        assert_eq!(classified.kind, ClassifiedKind::Text);
        assert_eq!(classified.size, 4, "the bytes actually read, not the stat figure");
        assert_eq!(classified.text.as_deref(), Some("tiny"));
    }

    /// An empty file is its own answer, never a lane.
    #[test]
    fn classify_reports_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "").unwrap();
        assert!(matches!(classify(&empty, 0), Err(CatFailure::Empty)));
    }
}
