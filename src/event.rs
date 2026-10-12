//! The records we keep, and how Herdr hook payloads become records.
//!
//! Our log format is deliberately our own rather than a copy of Herdr's
//! payload: it is smaller, and a protocol change in Herdr only touches
//! `from_hook`, not the stats or the files already on disk.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Idle,
    Working,
    Blocked,
    Done,
    /// Also absorbs any status a future Herdr adds, so older logs and
    /// binaries keep working.
    #[serde(other)]
    Unknown,
}

/// One line of `events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    Status {
        ts_ms: u64,
        pane_id: String,
        /// Workspace label when Herdr gives one, otherwise its id. Labels are
        /// stable names; ids like `w1` are reused after a restart.
        workspace: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
        status: Status,
    },
    Closed {
        ts_ms: u64,
        pane_id: String,
    },
    /// Herdr (re)started. Any wait still open at this point has an unknown end.
    SessionStart {
        ts_ms: u64,
    },
}

impl Record {
    pub fn ts_ms(&self) -> u64 {
        match self {
            Record::Status { ts_ms, .. }
            | Record::Closed { ts_ms, .. }
            | Record::SessionStart { ts_ms } => *ts_ms,
        }
    }
}

// The subset of Herdr's payloads we read. serde ignores unknown fields by
// default, so new fields in Herdr's protocol don't break us.

#[derive(Deserialize)]
struct HookEvent {
    event: String,
    data: HookData,
}

#[derive(Deserialize)]
struct HookData {
    pane_id: String,
    workspace_id: String,
    agent_status: Option<Status>,
    agent: Option<String>,
}

#[derive(Deserialize)]
struct Context {
    workspace_label: Option<String>,
}

/// Turns the hook environment into a record, or `None` for events we don't
/// track.
///
/// `event_name` is `HERDR_PLUGIN_EVENT`; the JSON arguments are
/// `HERDR_PLUGIN_EVENT_JSON` and `HERDR_PLUGIN_CONTEXT_JSON`. Herdr does not
/// timestamp events, so the caller passes the time the hook started.
pub fn from_hook(
    event_name: &str,
    event_json: Option<&str>,
    context_json: Option<&str>,
    ts_ms: u64,
) -> Result<Option<Record>, serde_json::Error> {
    if event_name == "startup" {
        return Ok(Some(Record::SessionStart { ts_ms }));
    }
    let Some(event_json) = event_json else {
        return Ok(None);
    };
    let HookEvent { event, data } = serde_json::from_str(event_json)?;

    let record = match (event.as_str(), data.agent_status) {
        ("pane_agent_status_changed", Some(status)) => {
            // The label is cosmetic, so a bad context must not lose the event.
            let label = context_json
                .and_then(|json| serde_json::from_str::<Context>(json).ok())
                .and_then(|context| context.workspace_label);
            Record::Status {
                ts_ms,
                pane_id: data.pane_id,
                workspace: label.unwrap_or(data.workspace_id),
                agent: data.agent,
                status,
            }
        }
        ("pane_closed" | "pane_exited", _) => Record::Closed {
            ts_ms,
            pane_id: data.pane_id,
        },
        _ => return Ok(None),
    };
    Ok(Some(record))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Payloads captured from Herdr 0.9.3.
    const BLOCKED: &str = r#"{"event":"pane_agent_status_changed","data":{"type":"pane_agent_status_changed","pane_id":"w8:p2","workspace_id":"w8","agent_status":"blocked","agent":"probe"}}"#;
    const CONTEXT: &str = r#"{"workspace_id":"w8","workspace_label":"herdr-plugins","tab_id":"w8:t1","focused_pane_id":"w8:p2","focused_pane_status":"blocked","invocation_source":"api"}"#;
    const CLOSED: &str = r#"{"event":"pane_closed","data":{"type":"pane_closed","pane_id":"w8:p2","workspace_id":"w8"}}"#;

    fn status(workspace: &str, status: Status) -> Record {
        Record::Status {
            ts_ms: 7,
            pane_id: "w8:p2".into(),
            workspace: workspace.into(),
            agent: Some("probe".into()),
            status,
        }
    }

    #[test]
    fn status_change_uses_workspace_label() {
        let record = from_hook("pane.agent_status_changed", Some(BLOCKED), Some(CONTEXT), 7);
        assert_eq!(
            record.unwrap(),
            Some(status("herdr-plugins", Status::Blocked))
        );
    }

    #[test]
    fn missing_or_bad_context_falls_back_to_workspace_id() {
        let expected = Some(status("w8", Status::Blocked));
        let event = "pane.agent_status_changed";
        assert_eq!(from_hook(event, Some(BLOCKED), None, 7).unwrap(), expected);
        assert_eq!(
            from_hook(event, Some(BLOCKED), Some("{oops"), 7).unwrap(),
            expected
        );
    }

    #[test]
    fn closed_and_exited_panes_become_closed_records() {
        let closed = Some(Record::Closed {
            ts_ms: 7,
            pane_id: "w8:p2".into(),
        });
        assert_eq!(
            from_hook("pane.closed", Some(CLOSED), None, 7).unwrap(),
            closed
        );
        let exited = CLOSED.replace("pane_closed", "pane_exited");
        assert_eq!(
            from_hook("pane.exited", Some(&exited), None, 7).unwrap(),
            closed
        );
    }

    #[test]
    fn startup_needs_no_payload() {
        let record = from_hook("startup", None, None, 7).unwrap();
        assert_eq!(record, Some(Record::SessionStart { ts_ms: 7 }));
    }

    #[test]
    fn unrelated_events_are_ignored() {
        let focused = CLOSED.replace("pane_closed", "pane_focused");
        assert_eq!(
            from_hook("pane.focused", Some(&focused), None, 7).unwrap(),
            None
        );
    }

    #[test]
    fn unrecognised_status_maps_to_unknown() {
        let future = BLOCKED.replace("\"blocked\"", "\"sleeping\"");
        let record = from_hook("pane.agent_status_changed", Some(&future), None, 7);
        assert_eq!(record.unwrap(), Some(status("w8", Status::Unknown)));
    }

    #[test]
    fn malformed_event_is_an_error_not_a_panic() {
        assert!(from_hook("pane.closed", Some("not json"), None, 7).is_err());
    }
}
