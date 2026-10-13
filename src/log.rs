//! The append-only JSONL log in `HERDR_PLUGIN_STATE_DIR`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::event::Record;

/// Records older than this are pruned when Herdr starts. Generous next to
/// the 7-day tab, so a longer view can be added without losing history.
pub const RETENTION_DAYS: u64 = 60;

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("events.jsonl")
}

/// Appends one record as one line.
///
/// Herdr runs hooks concurrently, so several processes may append at once.
/// With `O_APPEND` each `write` lands at the current end of file, and a short
/// line goes out in a single `write`, so lines don't interleave and we need
/// no lock file.
pub fn append(path: &Path, record: &Record) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

#[derive(Debug, Default)]
pub struct Log {
    pub records: Vec<Record>,
    /// Lines that could not be parsed, e.g. after a crash mid-write.
    pub skipped: usize,
}

/// Reads the whole log. A missing file is an empty log, not an error.
pub fn read(path: &Path) -> io::Result<Log> {
    match fs::read(path) {
        // Lossy so that one line of invalid UTF-8 costs one record, not the file.
        Ok(bytes) => Ok(parse(&String::from_utf8_lossy(&bytes))),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Log::default()),
        Err(err) => Err(err),
    }
}

pub fn parse(text: &str) -> Log {
    let mut log = Log::default();
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str(line) {
            Ok(record) => log.records.push(record),
            Err(_) => log.skipped += 1,
        }
    }
    log
}

/// Drops lines older than `since_ms`, and unreadable ones.
///
/// The kept lines go to a temporary file that is renamed over the log. The
/// rename is atomic, so a reader sees the old log or the new one, never a
/// mix. A hook appending between our read and the rename would lose its
/// line; pruning runs once per Herdr start, so that window is tiny.
pub fn prune(path: &Path, since_ms: u64) -> io::Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
    };
    let text = String::from_utf8_lossy(&bytes);
    let kept = recent_lines(&text, since_ms);
    if kept.len() == text.len() {
        return Ok(()); // nothing to drop: don't touch the file
    }
    let tmp = path.with_extension("jsonl.tmp");
    fs::write(&tmp, kept)?;
    fs::rename(&tmp, path)
}

/// The lines of `text` whose record is at or after `since_ms`, kept exactly
/// as written.
fn recent_lines(text: &str, since_ms: u64) -> String {
    text.lines()
        .filter(|line| {
            serde_json::from_str::<Record>(line).is_ok_and(|record| record.ts_ms() >= since_ms)
        })
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Deletes every record. Truncating, rather than removing the file, means a
/// hook appending at the same moment just lands in the now-empty log.
pub fn clear(path: &Path) -> io::Result<()> {
    match OpenOptions::new().write(true).open(path) {
        Ok(file) => file.set_len(0),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Status;

    #[test]
    fn corrupt_and_blank_lines_are_skipped_and_counted() {
        let text = concat!(
            r#"{"kind":"session_start","ts_ms":1}"#,
            "\n\n",
            r#"{"kind":"closed","ts_ms":2,"pane_"#,
            "\n",
            r#"{"kind":"closed","ts_ms":3,"pane_id":"w1:p1"}"#,
            "\n",
        );
        let log = parse(text);
        assert_eq!(log.records.len(), 2);
        assert_eq!(log.skipped, 1);
    }

    #[test]
    fn appended_records_read_back() {
        let dir = std::env::temp_dir().join(format!("bottleneck-test-{}", std::process::id()));
        let path = path(&dir);
        let records = [
            Record::SessionStart { ts_ms: 1 },
            Record::Status {
                ts_ms: 2,
                pane_id: "w1:p1".into(),
                workspace: "api".into(),
                agent: None,
                status: Status::Blocked,
            },
        ];
        for record in &records {
            append(&path, record).unwrap();
        }
        let log = read(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(log.records, records);
        assert_eq!(log.skipped, 0);
    }

    /// A fresh directory under the system temp dir, unique per test.
    fn temp_log(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bottleneck-{test}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        path(&dir)
    }

    const OLD: &str = r#"{"kind":"session_start","ts_ms":1}"#;
    const NEW: &str = r#"{"kind":"closed","ts_ms":9,"pane_id":"w1:p1"}"#;

    #[test]
    fn recent_lines_drop_old_and_corrupt_lines() {
        let text = format!("{OLD}\nnot json\n{NEW}\n");
        assert_eq!(recent_lines(&text, 5), format!("{NEW}\n"));
    }

    #[test]
    fn prune_rewrites_the_log_with_recent_records_only() {
        let path = temp_log("prune");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, format!("{OLD}\n{NEW}\n")).unwrap();
        prune(&path, 5).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(text, format!("{NEW}\n"));
    }

    #[test]
    fn clear_empties_the_log_and_appends_still_work() {
        let path = temp_log("clear");
        append(&path, &Record::SessionStart { ts_ms: 1 }).unwrap();
        clear(&path).unwrap();
        assert!(read(&path).unwrap().records.is_empty());
        append(&path, &Record::SessionStart { ts_ms: 2 }).unwrap();
        let log = read(&path).unwrap();
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
        assert_eq!(log.records, [Record::SessionStart { ts_ms: 2 }]);
    }

    #[test]
    fn prune_and_clear_ignore_a_missing_log() {
        let path = Path::new("/nonexistent/bottleneck/events.jsonl");
        assert!(prune(path, 5).is_ok());
        assert!(clear(path).is_ok());
    }

    #[test]
    fn missing_file_is_an_empty_log() {
        let log = read(Path::new("/nonexistent/bottleneck/events.jsonl")).unwrap();
        assert!(log.records.is_empty());
    }
}
