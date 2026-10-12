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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_writes_one_line_per_record() {
        let dir = std::env::temp_dir().join(format!("bottleneck-test-{}", std::process::id()));
        let path = path(&dir);
        append(&path, &Record::SessionStart { ts_ms: 1 }).unwrap();
        append(&path, &Record::SessionStart { ts_ms: 2 }).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(text.lines().count(), 2);
    }
}
