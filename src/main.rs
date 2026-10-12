use std::env;
use std::error::Error;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use bottleneck::{event, log, view};

const USAGE: &str = "usage: bottleneck <record|view|open>";

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
    if let Some(record) = record {
        log::append(&log::path(&state_dir()?), &record)?;
    }
    Ok(())
}

/// Runs as the report pane.
fn view() -> Result<(), Box<dyn Error>> {
    Ok(view::run(&log::path(&state_dir()?), now_ms)?)
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

fn state_dir() -> Result<PathBuf, &'static str> {
    env::var_os("HERDR_PLUGIN_STATE_DIR")
        .map(PathBuf::from)
        .ok_or("HERDR_PLUGIN_STATE_DIR is not set; run me from Herdr")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64)
}
