use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use bottleneck::event::{self, Record};
use bottleneck::{log, stats, view};

const USAGE: &str = "usage: bottleneck <record|view|open>";
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("record") => {
            // A hook that fails would only add noise to Herdr's plugin log on
            // every state change, so we report the problem and exit cleanly.
            if let Err(err) = record() {
                eprintln!("bottleneck record: {err}");
            }
            ExitCode::SUCCESS
        }
        Some("view") => exit_code(view()),
        Some("open") => exit_code(open()),
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// Runs as a Herdr event or startup hook: appends one record and exits.
fn record() -> Result<(), Box<dyn Error>> {
    let event = env::var("HERDR_PLUGIN_EVENT").unwrap_or_default();
    let event_json = env::var("HERDR_PLUGIN_EVENT_JSON").ok();
    let context_json = env::var("HERDR_PLUGIN_CONTEXT_JSON").ok();
    let record = event::from_hook(
        &event,
        event_json.as_deref(),
        context_json.as_deref(),
        now_ms(),
    )?;
    let Some(record) = record else {
        return Ok(());
    };
    let log_path = log::path(&state_dir()?);
    if let Record::SessionStart { ts_ms } = record {
        // Once per Herdr start, drop records past the retention period. A
        // failure here must not stop the session marker being written.
        let since_ms = ts_ms.saturating_sub(log::RETENTION_DAYS * DAY_MS);
        if let Err(err) = log::prune(&log_path, since_ms) {
            eprintln!("bottleneck record: pruning the log failed: {err}");
        }
    }
    log::append(&log_path, &record)?;
    if let Record::Status { ts_ms, pane_id, .. } = &record {
        publish_waited(&log_path, pane_id, *ts_ms)?;
    }
    Ok(())
}

/// Publishes the pane's blocked time over the last 24 hours as the `waited`
/// token, so a sidebar row can show `$waited`.
fn publish_waited(log_path: &Path, pane_id: &str, now_ms: u64) -> Result<(), Box<dyn Error>> {
    let records = log::read(log_path)?.records;
    let waited = stats::pane_blocked_ms(&records, pane_id, now_ms.saturating_sub(DAY_MS), now_ms);
    if waited == 0 {
        return Ok(());
    }
    // Hooks can finish out of order; `--seq` makes Herdr drop stale reports.
    let status = Command::new(herdr_env("HERDR_BIN_PATH")?)
        .args(["pane", "report-metadata", pane_id])
        .args(["--source", "rsdlt.bottleneck"])
        .args(["--seq", &now_ms.to_string()])
        .args(["--token", &format!("waited=waited {}", view::human(waited))])
        .stdout(Stdio::null())
        .status()?;
    if !status.success() {
        return Err(format!("herdr pane report-metadata failed: {status}").into());
    }
    Ok(())
}

/// Runs as the report pane.
fn view() -> Result<(), Box<dyn Error>> {
    Ok(view::run(&log::path(&state_dir()?), now_ms)?)
}

/// Runs as the action you bind to a key: asks Herdr to open the report pane.
fn open() -> Result<(), Box<dyn Error>> {
    let plugin = herdr_env("HERDR_PLUGIN_ID")?;
    let status = Command::new(herdr_env("HERDR_BIN_PATH")?)
        .args([
            "plugin",
            "pane",
            "open",
            "--entrypoint",
            "report",
            "--plugin",
        ])
        .arg(plugin)
        .status()?;
    if !status.success() {
        return Err(format!("herdr plugin pane open failed: {status}").into());
    }
    Ok(())
}

fn exit_code(result: Result<(), Box<dyn Error>>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("bottleneck: {err}");
            ExitCode::FAILURE
        }
    }
}

fn state_dir() -> Result<PathBuf, String> {
    herdr_env("HERDR_PLUGIN_STATE_DIR").map(PathBuf::from)
}

/// Reads a variable Herdr sets for plugin commands.
fn herdr_env(name: &str) -> Result<OsString, String> {
    env::var_os(name).ok_or_else(|| format!("{name} is not set; run me from Herdr"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}
