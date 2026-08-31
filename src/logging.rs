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
        if st.day.as_deref() == Some(&today) {
            return Ok(());
        }
        // Close the previous day's file and rename it to cola-YYYY-MM-DD.log.
        let prev_day = st.day.take();
        st.file.take();
        if let Some(day) = prev_day {
            let dated = dated_path(&self.base, &day);
            if self.base.exists() && !dated.exists() {
                let _ = std::fs::rename(&self.base, &dated);
            }
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.base)?;
        st.day = Some(today);
        st.file = Some(file);
        drop(st);
        self.sweep();
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
}
