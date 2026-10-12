//! The append-only JSONL log in `HERDR_PLUGIN_STATE_DIR`.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::event::Record;

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

    #[test]
    fn missing_file_is_an_empty_log() {
        let log = read(Path::new("/nonexistent/bottleneck/events.jsonl")).unwrap();
        assert!(log.records.is_empty());
    }
}
