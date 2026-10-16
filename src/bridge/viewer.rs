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
//! Everything here is bounded: a preview reads at most [`PREVIEW_READ_CAP`]
//! bytes and the card layer holds the preview to its own card budget, so a huge
//! file can neither exhaust memory nor produce a rejected card.

use std::io::Read;
use std::path::{Path, PathBuf};

/// The most bytes a preview reads (ADR-0083 §6): 8 MB. A text file past it is
/// not previewed in full — #739 routes such a file to the File Message lane;
/// until that lane exists a preview renders the bounded prefix and the card
/// says it is truncated rather than reading the whole file into memory.
pub(crate) const PREVIEW_READ_CAP: usize = 8 * 1024 * 1024;

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
        // A vanished path and a path whose own `stat` is denied are different
        // answers: "找不到" says the path is wrong, "无法读取" says cola is not
        // allowed to look (#669 stories 31/33).
        Err(e) => CatOpen::Failed(match e.kind() {
            std::io::ErrorKind::NotFound => CatFailure::Missing,
            _ => CatFailure::Unreadable(e.to_string()),
        }),
    }
}

/// Why a path could not be shown (ADR-0083 §2). Every cause answers its own
/// line and every line names the path — an unopenable file is a stated reason,
/// never an empty card. `#739` replaces the [`CatFailure::NotText`] answer with
/// the byte lanes; the honesty requirement is this ticket's.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CatFailure {
    /// The resolved path does not exist.
    Missing,
    /// The path exists but cola cannot read it — a permission denial, an I/O
    /// error. `reason` is the OS's own words.
    Unreadable(String),
    /// The file is zero bytes.
    Empty,
    /// The bytes are not valid UTF-8 (or carry a NUL), so this is not the text
    /// preview the viewer renders.
    NotText,
}

impl CatFailure {
    /// The one line this cause answers with, naming `path`.
    pub(crate) fn line(&self, path: &Path) -> String {
        let shown = path.display();
        match self {
            Self::Missing => format!("⚠️ 找不到路径 `{shown}`。"),
            Self::Unreadable(reason) => format!("⚠️ 无法读取 `{shown}`：{reason}"),
            Self::Empty => format!("📄 `{shown}` 是空文件（0 字节）。"),
            Self::NotText => format!("⚠️ `{shown}` 不是 UTF-8 文本，无法预览。"),
        }
    }
}

/// Read a file's preview bytes and decode them as text: the file's content, or
/// the honest reason there is none. Bounded by [`PREVIEW_READ_CAP`] — a longer
/// file yields its prefix, which the caller reports as truncated.
pub(crate) fn read_preview(path: &Path, size: u64) -> Result<String, CatFailure> {
    read_preview_capped(path, size, PREVIEW_READ_CAP)
}

/// [`read_preview`] with an explicit cap, so its truncation rules are testable
/// without writing an 8 MB fixture.
fn read_preview_capped(path: &Path, size: u64, cap: usize) -> Result<String, CatFailure> {
    if size == 0 {
        return Err(CatFailure::Empty);
    }
    let file = std::fs::File::open(path).map_err(|e| CatFailure::Unreadable(e.to_string()))?;
    let mut bytes = Vec::new();
    file.take(cap as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| CatFailure::Unreadable(e.to_string()))?;
    // A NUL byte means this is not text whatever its encoding: rendering it
    // would put a control character into the card (issue #519's family), and
    // the byte lanes (#739) are where a binary belongs anyway.
    if bytes.contains(&0) {
        return Err(CatFailure::NotText);
    }
    let truncated = size.saturating_sub(cap as u64) > 0;
    match std::str::from_utf8(&bytes) {
        Ok(text) => Ok(text.to_string()),
        // The cap can land mid-character: keep the valid prefix (the caller
        // already reports the truncation) instead of calling real text binary.
        Err(e) if truncated => {
            let valid = e.valid_up_to();
            Ok(String::from_utf8(bytes[..valid].to_vec()).expect("a valid prefix"))
        }
        Err(_) => Err(CatFailure::NotText),
    }
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
    /// every line names the resolved path.
    #[test]
    fn cat_failures_answer_distinctly_and_name_the_path() {
        let path = Path::new("/tmp/some/file.txt");
        let missing = CatFailure::Missing.line(path);
        let unreadable = CatFailure::Unreadable("Permission denied (os error 13)".into()).line(path);
        let empty = CatFailure::Empty.line(path);
        let not_text = CatFailure::NotText.line(path);

        for line in [&missing, &unreadable, &empty, &not_text] {
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
        assert!(not_text.contains("UTF-8"), "{not_text}");
        // Distinct: no two causes share their copy.
        let mut lines: Vec<String> = vec![missing, unreadable, empty, not_text];
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

    /// The preview read is honest about empty, non-UTF-8 and truncated files.
    #[test]
    fn read_preview_reports_empty_binary_and_cap() {
        let dir = tempfile::tempdir().unwrap();

        let empty = dir.path().join("empty.txt");
        std::fs::write(&empty, "").unwrap();
        assert_eq!(read_preview(&empty, 0), Err(CatFailure::Empty));

        let binary = dir.path().join("bin");
        std::fs::write(&binary, [0xff, 0xfe, 0x00, 0x01]).unwrap();
        assert_eq!(read_preview(&binary, 4), Err(CatFailure::NotText));

        // A cap landing mid-character keeps the valid prefix instead of
        // calling real text binary.
        let text = dir.path().join("long.txt");
        std::fs::write(&text, "日本語のテキスト").unwrap();
        let bytes = std::fs::metadata(&text).unwrap().len();
        let short = read_preview_capped(&text, bytes, 4).unwrap();
        assert_eq!(short, "日", "the cap keeps the valid prefix");
        assert_eq!(
            read_preview_capped(&text, bytes, 1024).unwrap(),
            "日本語のテキスト"
        );
    }
}
