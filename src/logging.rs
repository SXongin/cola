//! Daily-rotating file logging.
//!
//! Logs always append to `cola.log` (never truncate). On the first write of a
//! new day the previous day's content is moved to `cola-YYYY-MM-DD.log` and a
//! fresh `cola.log` starts. Files older than `log_days` are swept. Cross-day
//! sessions are queried by `grep session_id=... cola-*.log` — the log layer
//! never knows about sessions as a business object.

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tracing_subscriber::field::MakeVisitor;
use tracing_subscriber::layer::SubscriberExt;

/// Implements `tracing_subscriber::fmt::MakeWriter`: each write goes to the
/// day's `cola.log`, rotating (and sweeping) when the date changes.
pub struct DailyLog {
    /// The active log file path (`.../cola.log`).
    base: PathBuf,
    /// Retention in days; older `cola-YYYY-MM-DD.log` files are deleted.
    log_days: u32,
    state: Mutex<State>,
}

struct State {
    /// The day the open file belongs to (`YYYY-MM-DD`), used to detect a
    /// day rollover on the next write.
    day: Option<String>,
    file: Option<std::fs::File>,
}

impl DailyLog {
    pub fn new(base: PathBuf, log_days: u32) -> io::Result<Self> {
        if let Some(parent) = base.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // If a `cola.log` already exists (a previous run), seed the day from its
        // mtime so a restart across midnight rotates yesterday's content on the
        // first write instead of appending today's logs into the old file.
        let day = std::fs::metadata(&base).and_then(|m| m.modified()).ok().map(|t| {
            let dt: chrono::DateTime<chrono::Local> = t.into();
            dt.format("%Y-%m-%d").to_string()
        });
        let log = Self {
            base,
            log_days,
            state: Mutex::new(State { day, file: None }),
        };
        log.sweep();
        Ok(log)
    }

    fn rotate_if_needed(&self) -> io::Result<()> {
        let today = today_str();
        let mut st = self.state.lock().unwrap();
        if st.day.as_deref() != Some(&today) {
            // Close the previous day's file and rename it to cola-YYYY-MM-DD.log.
            let prev_day = st.day.take();
            st.file.take();
            if let Some(day) = prev_day {
                let dated = dated_path(&self.base, &day);
                if self.base.exists() && !dated.exists() {
                    let _ = std::fs::rename(&self.base, &dated);
                }
            }
            st.day = Some(today.clone());
            self.sweep();
        }
        // Open the file on the FIRST write even when the seeded day already
        // equals today (a same-day restart): the previous code returned early
        // with `file = None`, so every write silently returned Ok(0) and all
        // logs were dropped until a midnight rollover.
        if st.file.is_none() {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.base)?;
            st.file = Some(file);
        }
        drop(st);
        Ok(())
    }

    fn sweep(&self) {
        let keep = i64::from(self.log_days.max(1));
        let Some(dir) = self.base.parent() else { return };
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(day) = stem
                .strip_prefix(&file_stem(&self.base))
                .and_then(|s| s.strip_prefix('-'))
            else {
                continue;
            };
            if !is_date(day) {
                continue;
            }
            // Delete when the file's date is older than `log_days`.
            let Some(age_days) = age_in_days(day) else {
                continue;
            };
            if age_days >= keep {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for DailyLog {
    type Writer = LogWriterGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LogWriterGuard { log: self }
    }
}

/// One event's writer: rotates on the day boundary, then appends to the open
/// file. `Write` must be forwarded so tracing's formatter can emit.
pub struct LogWriterGuard<'a> {
    log: &'a DailyLog,
}

impl Write for LogWriterGuard<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if let Err(e) = self.log.rotate_if_needed() {
            // Never let logging failure take down cola; degrade to stderr.
            eprintln!("cola log rotate failed: {e}");
        }
        let mut st = self.log.state.lock().unwrap();
        match st.file.as_mut() {
            Some(f) => f.write(buf),
            None => Ok(0),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut st = self.log.state.lock().unwrap();
        match st.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

/// A field formatter that renders exactly like the default one but is its own
/// type.
///
/// `tracing-subscriber` keys a span's formatted-field extension on
/// `FormattedFields<N>` — the FIELD formatter type, not the layer. Two `fmt`
/// layers that both use the default formatter share one extension, so every
/// `Span::record` (the `session`/`chat`/`topic` fills in `bridge::span`) is
/// appended once per layer and each line's prefix prints its fields twice:
/// `turn{session=ses_x session=ses_x chat=oc_x chat=oc_x}`. The mirror layer
/// wraps its formatter in this newtype so each layer owns its extension;
/// `tests::a_mirror_layer_does_not_double_the_span_fields` pins it.
#[derive(Debug, Default)]
struct MirrorFields(tracing_subscriber::fmt::format::DefaultFields);

impl<'a> MakeVisitor<tracing_subscriber::fmt::format::Writer<'a>> for MirrorFields {
    type Visitor = tracing_subscriber::fmt::format::DefaultVisitor<'a>;

    fn make_visitor(&self, target: tracing_subscriber::fmt::format::Writer<'a>) -> Self::Visitor {
        self.0.make_visitor(target)
    }
}

/// The two-layer subscriber cola installs (ADR-0048): the plain daily file
/// plus, for a real terminal, the ANSI mirror, under the level `filter`.
/// `main` passes `EnvFilter::try_from_default_env()` with cola's `cola=info`
/// default; the tests install the same wiring over in-memory writers.
pub(crate) fn subscriber<W, M>(
    file: W,
    mirror: Option<M>,
    filter: tracing_subscriber::EnvFilter,
) -> impl tracing::Subscriber + Send + Sync + 'static
where
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
    M: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    // Stamp every line in the machine's local time, with the offset spelled
    // out. Without an explicit timer tracing prints UTC, so the line clock
    // would disagree with both the card clocks (also `chrono::Local`) and the
    // daily rotation date — the log file's day would start at 08:00 Beijing.
    let timer = tracing_subscriber::fmt::time::ChronoLocal::rfc_3339();
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file)
        .with_ansi(false)
        .with_timer(timer.clone());
    let mirror_layer = mirror.map(|writer| {
        tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(true)
            .with_timer(timer)
            .fmt_fields(MirrorFields::default())
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(mirror_layer)
}

/// The rotated file for a day (`cola-YYYY-MM-DD.log`), mirroring the rotation
/// naming so `cola logs --date` finds what `rotate_if_needed` wrote.
pub fn rotated_path(base: &Path, day: &str) -> PathBuf {
    dated_path(base, day)
}

/// `cola logs`: print the tail of the active log file (or a rotated day's file)
/// and, with `follow`, stream new lines as they are appended — including across
/// a midnight rotation, which reopens the fresh base file.
pub fn run_logs(base: &Path, follow: bool, lines: usize, date: Option<&str>) -> io::Result<()> {
    run_logs_to(base, follow, lines, date, &mut io::stdout())
}

/// [`run_logs`] over an explicit writer, so the selection and the tail are
/// unit-testable without capturing the process's stdout.
pub fn run_logs_to(
    base: &Path,
    follow: bool,
    lines: usize,
    date: Option<&str>,
    out: &mut impl Write,
) -> io::Result<()> {
    let path = match date {
        // The date is only ever an exact `YYYY-MM-DD`: anything else could
        // carry path-traversal components and point the tail at another file.
        Some(day) => {
            if !is_date(day) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("--date 需要 YYYY-MM-DD，收到 {day:?}"),
                ));
            }
            rotated_path(base, day)
        }
        None => base.to_path_buf(),
    };
    if !path.exists() {
        eprintln!("日志文件不存在: {}", path.display());
        return Ok(());
    }
    let (tail, end, identity) = tail_lines(&path, lines)?;
    for line in &tail {
        writeln!(out, "{line}")?;
    }
    out.flush()?;
    if follow {
        // Continue from exactly where the tail stopped, so a line appended
        // between the tail read and the follow loop is printed, not skipped.
        follow_file(&path, end, identity, out)?;
    }
    Ok(())
}

/// The last `n` lines of `path`, plus the byte offset the tail ends at and the
/// handle's identity at read time — what [`follow_file`] needs to resume from
/// that exact point. A file that does not end in a newline still yields its
/// final line. Read from the end so a large log is never loaded whole.
fn tail_lines(path: &Path, n: usize) -> io::Result<(Vec<String>, u64, Option<u128>)> {
    use io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let identity = handle_identity(&file);
    if n == 0 {
        return Ok((Vec::new(), len, identity));
    }
    let block: u64 = 8192;
    let mut pos = len;
    let mut buf: Vec<u8> = Vec::new();
    let mut newlines = 0usize;
    while pos > 0 && newlines <= n {
        let read = pos.min(block);
        pos -= read;
        file.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read as usize];
        file.read_exact(&mut chunk)?;
        chunk.extend_from_slice(&buf);
        buf = chunk;
        newlines = buf.iter().filter(|&&b| b == b'\n').count();
    }
    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.len() > n {
        lines.drain(0..lines.len() - n);
    }
    Ok((lines, len, identity))
}

/// Poll `path` for appended bytes and print them, starting from the top when the
/// file is rotated out from under us — the base `cola.log` moves to a dated file
/// and a fresh base starts, which the next poll detects as a **changed file
/// identity**, not merely a shorter file (the fresh base may already have grown
/// past the old offset). A trailing partial line is held back until its newline
/// arrives, so a write caught mid-line is neither split nor duplicated. Blocks
/// forever; the caller exits on Ctrl-C.
fn follow_file(path: &Path, start: u64, identity: Option<u128>, out: &mut impl Write) -> io::Result<()> {
    use io::{Read, Seek, SeekFrom};
    let poll = std::time::Duration::from_millis(300);
    let mut identity = identity;
    let mut offset = start;
    let mut pending: Vec<u8> = Vec::new();
    loop {
        std::thread::sleep(poll);
        // Identity AND length come from the SAME opened handle, so a rotation
        // between the two cannot pair a new file's length with an old offset
        // (which would skip a prefix and then replay it as duplicates).
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        let Ok(meta) = file.metadata() else {
            continue;
        };
        // A new file at the same path (rotation) restarts from the top: only
        // comparing lengths would miss a fresh base that grew past the offset.
        let current = handle_identity(&file);
        if current != identity {
            identity = current;
            offset = 0;
            pending.clear();
        }
        let len = meta.len();
        if len < offset {
            offset = 0;
            pending.clear();
        }
        if len == offset {
            continue;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = Vec::new();
        file.take(len - offset).read_to_end(&mut chunk)?;
        offset += chunk.len() as u64;
        pending.extend_from_slice(&chunk);
        drain_complete_lines(&mut pending, out)?;
        out.flush()?;
    }
}

/// Write every complete line in `pending` to `out`, leaving any trailing
/// bytes-without-newline buffered (a write caught mid-line lands whole next
/// time). `pending` is drained of what it wrote.
fn drain_complete_lines(pending: &mut Vec<u8>, out: &mut impl Write) -> io::Result<()> {
    while let Some(newline) = pending.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = pending.drain(..=newline).collect();
        out.write_all(&line)?;
    }
    Ok(())
}

/// A stable identity for an OPEN file handle, used to notice rotation. `None`
/// when it cannot be read (rotation then falls back to the length-decrease
/// check). Unchanged by appends; a rotation replaces the file at the path with a
/// new one, so the identity moves. Read from the handle itself — never from the
/// path — so it always describes the bytes the caller is about to read.
#[cfg(unix)]
fn handle_identity(file: &std::fs::File) -> Option<u128> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().ok()?;
    Some(((meta.dev() as u128) << 64) | meta.ino() as u128)
}

/// [`handle_identity`] elsewhere: the file's creation time stands in — a
/// rotation creates a NEW file (a new timestamp), which an append never
/// changes. The unix `(dev, ino)` pair is preferred where available because a
/// creation timestamp has coarse filesystem resolution.
#[cfg(not(unix))]
fn handle_identity(file: &std::fs::File) -> Option<u128> {
    let created = file.metadata().ok()?.created().ok()?;
    Some(created.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos())
}

/// `YYYY-MM-DD` for the local timezone, as chrono is already a dependency.
fn today_str() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// The base file stem (e.g. `cola` from `~/.cola/cola.log`), used to recognize
/// `cola-YYYY-MM-DD.log` rotation files in the same directory.
fn file_stem(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cola".to_string())
}

fn dated_path(base: &Path, day: &str) -> PathBuf {
    base.with_file_name(format!("{}-{}.log", file_stem(base), day))
}

fn is_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
}

/// How many days ago a `YYYY-MM-DD` string is (0 = today).
fn age_in_days(day: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d").ok()?;
    let today = chrono::Local::now().date_naive();
    Some(today.signed_duration_since(d).num_days())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::fmt::MakeWriter;

    fn tmp_log() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cola.log");
        (dir, base)
    }

    /// An in-memory [`MakeWriter`] shared by the two layers of the
    /// span-field regression test.
    #[derive(Clone, Default)]
    struct TestBuffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for TestBuffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for TestBuffer {
        type Writer = TestBuffer;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    #[test]
    fn writes_append_to_base() {
        let (_dir, base) = tmp_log();
        let log = DailyLog::new(base.clone(), 14).unwrap();
        log.make_writer().write_all(b"line one\n").unwrap();
        log.make_writer().write_all(b"line two\n").unwrap();
        let content = std::fs::read_to_string(&base).unwrap();
        assert_eq!(content, "line one\nline two\n");
    }

    #[test]
    fn rotate_moves_previous_day_to_dated_file() {
        let (_dir, base) = tmp_log();
        let log = DailyLog::new(base.clone(), 14).unwrap();
        // Force the writer onto "yesterday" (recent enough to survive the
        // retention sweep — a real rotation moves yesterday's content).
        let yesterday = chrono::Local::now()
            .date_naive()
            .pred_opt()
            .unwrap()
            .format("%Y-%m-%d")
            .to_string();
        {
            let mut st = log.state.lock().unwrap();
            st.day = Some(yesterday.clone());
            st.file = Some(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&base)
                    .unwrap(),
            );
            std::fs::write(&base, b"yesterday\n").unwrap();
        }
        // Next write detects the rollover: base → cola-<yesterday>.log, fresh base.
        log.make_writer().write_all(b"today\n").unwrap();
        let dated = dated_path(&base, &yesterday);
        assert!(dated.exists(), "yesterday's file should be rotated: {dated:?}");
        assert_eq!(std::fs::read_to_string(&dated).unwrap(), "yesterday\n");
        let content = std::fs::read_to_string(&base).unwrap();
        assert!(content.contains("today"), "fresh base appends: {content}");
    }

    #[test]
    fn restart_seeds_day_from_existing_file_mtime() {
        let (_dir, base) = tmp_log();
        // Simulate a file left by a previous run: new() must not truncate it and
        // must seed the current day from its mtime (so a restart across midnight
        // rotates on the first write). The exact seeded value is the file's
        // mtime day; we only assert it is Some (a real restart's file would
        // carry yesterday's mtime, which rotate_if_needed then moves).
        std::fs::write(&base, b"old\n").unwrap();
        let log = DailyLog::new(base.clone(), 14).unwrap();
        assert!(
            log.state.lock().unwrap().day.is_some(),
            "day seeded from file mtime"
        );
        assert_eq!(std::fs::read_to_string(&base).unwrap(), "old\n");
    }

    #[test]
    fn same_day_restart_still_appends() {
        let (_dir, base) = tmp_log();
        // Regression: a restart the SAME day seeds `day` from the existing
        // file's mtime, and the first write used to return Ok(0) without ever
        // opening the file — logs silently dropped until a midnight rollover.
        std::fs::write(&base, b"old\n").unwrap();
        let log = DailyLog::new(base.clone(), 14).unwrap();
        log.make_writer().write_all(b"line\n").unwrap();
        let content = std::fs::read_to_string(&base).unwrap();
        assert_eq!(content, "old\nline\n", "same-day restart must append");
    }

    #[test]
    fn tail_lines_returns_the_last_n_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cola.log");
        let mut body = String::new();
        for i in 1..=10 {
            body.push_str(&format!("line {i}\n"));
        }
        std::fs::write(&file, &body).unwrap();
        let (lines, end, identity) = tail_lines(&file, 3).unwrap();
        assert_eq!(lines, vec!["line 8", "line 9", "line 10"]);
        // The tail ends at the file's length, so follow resumes there.
        assert_eq!(end, body.len() as u64);
        #[cfg(unix)]
        assert!(identity.is_some(), "the handle identity travels to follow");
        // Fewer lines than requested yields the whole file.
        assert_eq!(tail_lines(&file, 50).unwrap().0.len(), 10);
        // Zero means nothing (but still reports where to resume).
        let (none, end0, _) = tail_lines(&file, 0).unwrap();
        assert!(none.is_empty());
        assert_eq!(end0, body.len() as u64);
    }

    #[test]
    fn tail_lines_keeps_a_final_unterminated_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("cola.log");
        std::fs::write(&file, b"a\nb\nc").unwrap();
        assert_eq!(tail_lines(&file, 2).unwrap().0, vec!["b", "c"]);
    }

    #[test]
    fn drain_complete_lines_writes_only_complete_lines() {
        let mut pending = b"a\nb\npartial".to_vec();
        let mut out: Vec<u8> = Vec::new();
        drain_complete_lines(&mut pending, &mut out).unwrap();
        assert_eq!(out, b"a\nb\n");
        assert_eq!(pending, b"partial", "an incomplete tail stays buffered");
        // Completing the line flushes it on the next poll.
        pending.extend_from_slice(b" done\n");
        drain_complete_lines(&mut pending, &mut out).unwrap();
        assert_eq!(out, b"a\nb\npartial done\n");
        assert!(pending.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn handle_identity_changes_when_the_file_is_replaced() {
        // Rotation writes a NEW file at the same path; identity must move so the
        // follow loop restarts from the top even if the new file grew past the
        // old offset.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cola.log");
        std::fs::write(&path, b"one\n").unwrap();
        let first = handle_identity(&std::fs::File::open(&path).unwrap());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"two\n").unwrap();
        assert!(first.is_some());
        assert_ne!(first, handle_identity(&std::fs::File::open(&path).unwrap()));
    }

    #[test]
    fn run_logs_selects_the_rotated_file_by_date() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cola.log");
        let dated = rotated_path(&base, "2026-01-02");
        std::fs::write(&dated, b"old one\nold two\n").unwrap();
        // The date picks that day's file, not the (absent) base: the output must
        // carry the dated file's content, so a `--date` that were ignored would
        // fail here.
        let mut out: Vec<u8> = Vec::new();
        run_logs_to(&base, false, 10, Some("2026-01-02"), &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "old one\nold two\n");
    }

    #[test]
    fn run_logs_to_prints_the_base_tail_without_a_date() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cola.log");
        std::fs::write(&base, b"a\nb\nc\n").unwrap();
        let mut out: Vec<u8> = Vec::new();
        run_logs_to(&base, false, 2, None, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "b\nc\n");
    }

    #[test]
    fn run_logs_rejects_a_non_date_selector() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cola.log");
        // A traversal component must be refused before it becomes a path.
        let err = run_logs(&base, false, 10, Some("../secret")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn run_logs_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().join("cola.log");
        // No file at all: the command prints a hint and exits 0.
        run_logs(&base, false, 10, None).unwrap();
    }

    #[test]
    fn sweep_deletes_files_older_than_retention() {
        let (_dir, base) = tmp_log();
        // Create an old dated file.
        let old = dated_path(&base, "2000-01-01");
        std::fs::write(&old, b"old\n").unwrap();
        // A recent one must survive.
        let recent = dated_path(&base, &today_str());
        std::fs::write(&recent, b"recent\n").unwrap();
        let log = DailyLog::new(base.clone(), 14).unwrap();
        log.sweep();
        assert!(!old.exists(), "old dated file swept");
        assert!(recent.exists(), "recent file kept");
    }

    /// Two `fmt` layers must not share their span-fields extension.
    ///
    /// `tracing-subscriber` stores a span's formatted prefix in a span
    /// extension keyed by `FormattedFields<N>` — the FIELD formatter type, not
    /// the layer. Two layers that format fields with the same type share one
    /// extension, so every `Span::record` (the `session`/`chat`/`topic` fills
    /// in `bridge::span`) is appended once per layer and each line's prefix
    /// prints its fields twice:
    /// `turn{session=ses_x session=ses_x chat=oc_x chat=oc_x}`. Single-layer
    /// tests never see it; it fires exactly when stdout is a real terminal,
    /// because only then does the mirror layer exist.
    #[test]
    fn a_mirror_layer_does_not_double_the_span_fields() {
        // A second, never-installed dispatcher keeps the callsite interest
        // cache from short-circuiting (the capture harness documents the same
        // trap).
        let _interest_keepalive = tracing::Dispatch::new(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(io::sink)
                .finish(),
        );
        let file = TestBuffer::default();
        let mirror = TestBuffer::default();
        let subscriber = subscriber(
            file.clone(),
            Some(mirror.clone()),
            tracing_subscriber::EnvFilter::new("info"),
        );
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!(
            "turn",
            session = tracing::field::Empty,
            chat = tracing::field::Empty,
            topic = tracing::field::Empty,
        );
        span.record("session", tracing::field::display("ses_x"));
        span.record("chat", tracing::field::display("oc_x"));
        span.record("topic", tracing::field::display("omt_x"));
        let _entered = span.enter();
        tracing::info!("span-fields probe");

        for (name, buffer) in [("file", &file), ("mirror", &mirror)] {
            let text = String::from_utf8(buffer.0.lock().unwrap().clone())
                .expect("the fmt layer writes valid UTF-8");
            let line = text
                .lines()
                .find(|line| line.contains("span-fields probe"))
                .unwrap_or_else(|| panic!("the {name} layer must render the probe event: {text:?}"));
            // Counted, not matched as one substring: the mirror layer renders
            // ANSI, so its prefix is styled (the file layer is plain).
            for value in ["ses_x", "oc_x", "omt_x"] {
                assert_eq!(
                    line.matches(value).count(),
                    1,
                    "the {name} layer must print {value} exactly once: {line}"
                );
            }
        }
    }
}
