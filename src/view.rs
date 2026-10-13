//! The report pane: a ratatui view over the log.
//!
//! It re-reads the log every couple of seconds instead of subscribing to
//! Herdr's socket: the hooks already capture every event, and the log is
//! small, so a file read keeps the view simple and the data in one place.

use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyCode};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Stylize;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::event::Record;
use crate::log::{self, Log};
use crate::stats::{self, Summary, median};

/// The metric columns as (header, description). The table header and the
/// legend both read from here, so they can't drift apart. Every name starts
/// with a single-codepoint emoji: those render two cells wide everywhere,
/// while emoji with variation selectors (like ⚠️) break column alignment.
const COLUMNS: [(&str, &str); 3] = [
    (
        "✋ Blocked",
        "total time agents sat blocked, waiting for your answer",
    ),
    ("🐢 Longest", "longest single block"),
    (
        "👀 Unread",
        "median time from an agent finishing until you looked at it",
    ),
];

const REFRESH: Duration = Duration::from_secs(2);
const HOUR_MS: u64 = 60 * 60 * 1000;

/// The period the report covers, one tab each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Session,
    Day,
    Week,
}

impl Window {
    const ALL: [Window; 3] = [Window::Session, Window::Day, Window::Week];

    fn label(self) -> &'static str {
        match self {
            Window::Session => "this session",
            Window::Day => "last 24h",
            Window::Week => "last 7 days",
        }
    }

    fn since_ms(self, records: &[Record], now_ms: u64) -> u64 {
        match self {
            Window::Session => stats::session_start_ms(records),
            Window::Day => now_ms.saturating_sub(24 * HOUR_MS),
            Window::Week => now_ms.saturating_sub(7 * 24 * HOUR_MS),
        }
    }

    fn next(self) -> Self {
        match self {
            Window::Session => Window::Day,
            Window::Day => Window::Week,
            Window::Week => Window::Session,
        }
    }
}

/// Shows the report until the user quits.
pub fn run(log_path: &Path, now_ms: fn() -> u64) -> io::Result<()> {
    ratatui::run(|terminal| event_loop(terminal, log_path, now_ms))
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    log_path: &Path,
    now_ms: fn() -> u64,
) -> io::Result<()> {
    let mut window = Window::Session;
    loop {
        let log = log::read(log_path)?;
        terminal.draw(|frame| render(frame, &log, window, now_ms()))?;
        // Wake on a key press, or after REFRESH to pick up new records.
        if !event::poll(REFRESH)? {
            continue;
        }
        if let Some(key) = event::read()?.as_key_press_event() {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Tab => window = window.next(),
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame, log: &Log, window: Window, now_ms: u64) {
    let since_ms = window.since_ms(&log.records, now_ms);
    let report = stats::report(&log.records, since_ms, now_ms);
    let [legend, header, workspaces, agents, footer] = Layout::vertical([
        Constraint::Length(COLUMNS.len() as u16 + 2),
        Constraint::Length(4),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let tabs = Window::ALL.map(|tab| {
        let span = Span::from(format!(" {} ", tab.label()));
        if tab == window { span.reversed() } else { span }
    });
    let mut title = Line::from(" bottleneck: agents waiting on you ".bold());
    title.extend(tabs);
    frame.render_widget(
        legend_widget().block(Block::bordered().title(title)),
        legend,
    );
    let summary =
        Paragraph::new(summary_lines(&report.total)).block(Block::bordered().title(" Summary "));
    frame.render_widget(summary, header);
    frame.render_widget(table("📁 Workspace", &report.by_workspace), workspaces);
    frame.render_widget(table("🤖 Agent", &report.by_agent), agents);

    let mut notes = vec![" tab: switch period   q: quit".to_owned()];
    if log.skipped > 0 {
        notes.push(format!("{} unreadable log lines skipped", log.skipped));
    }
    if report.interrupted > 0 {
        notes.push(format!(
            "{} waits cut off by a Herdr restart, not counted",
            report.interrupted
        ));
    }
    frame.render_widget(Line::from(notes.join("  ·  ")).dim(), footer);
}

fn legend_widget() -> Paragraph<'static> {
    let lines = COLUMNS.map(|(name, description)| {
        // Padding counts chars, not cells; every name has one emoji, so the
        // descriptions still line up.
        Line::from(vec![format!(" {name:<11}").bold(), description.dim()])
    });
    Paragraph::new(lines.to_vec())
}

fn summary_lines(total: &Summary) -> Vec<Line<'static>> {
    if total.blocked.is_empty() && total.unseen.is_empty() {
        return vec![Line::from(
            " No waits in this window. Agents that block or finish show up here.",
        )];
    }
    let mut blocked = format!(
        " ✋ blocked {} in total · 🐢 longest {}",
        human(total.blocked_total()),
        maybe(total.blocked.iter().max().copied()),
    );
    if total.abandoned > 0 {
        blocked += &format!(" · {} abandoned", total.abandoned);
    }
    let unseen = format!(
        " 👀 unread {} (median over {} finished tasks)",
        maybe(median(&total.unseen)),
        total.unseen.len(),
    );
    vec![Line::from(blocked), Line::from(unseen)]
}

fn table<'a>(kind: &'a str, rows: &'a [(String, Summary)]) -> Table<'a> {
    let header = Row::new(std::iter::once(kind).chain(COLUMNS.map(|(name, _)| name))).bold();
    let rows = rows.iter().map(|(name, s)| {
        Row::new([
            name.clone(),
            human(s.blocked_total()),
            maybe(s.blocked.iter().max().copied()),
            maybe(median(&s.unseen)),
        ])
    });
    let mut widths = vec![Constraint::Fill(1)];
    widths.extend(COLUMNS.map(|_| Constraint::Length(11)));
    Table::new(rows, widths)
        .header(header)
        .block(Block::bordered())
}

/// A coarse duration such as `45s`, `14m` or `2h05m`. Waits are measured in
/// minutes, so finer detail would only be noise.
pub fn human(ms: u64) -> String {
    let secs = ms / 1000;
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h{:02}m", secs / 3600, secs % 3600 / 60),
    }
}

fn maybe(ms: Option<u64>) -> String {
    ms.map_or_else(|| "–".to_owned(), human)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn human_durations() {
        assert_eq!(human(0), "0s");
        assert_eq!(human(59_999), "59s");
        assert_eq!(human(14 * 60_000 + 59_000), "14m");
        assert_eq!(human(2 * HOUR_MS + 5 * 60_000), "2h05m");
    }

    fn draw(log: &Log, window: Window) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, log, window, 2_000_000))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn renders_fixture_report() {
        let log = log::parse(include_str!("../fixtures/two-workspaces.jsonl"));
        let screen = draw(&log, Window::Day);
        assert!(screen.contains("blocked 27m in total"), "{screen}");
        assert!(screen.contains("longest 10m"), "{screen}");
        assert!(
            screen.contains("unread 1m (median over 1 finished tasks)"),
            "{screen}"
        );
        assert!(screen.contains("1 abandoned"), "{screen}");
        assert!(screen.contains("web"), "{screen}");
        assert!(screen.contains("codex"), "{screen}");
        assert!(
            screen.contains("1 unreadable log lines skipped"),
            "{screen}"
        );
    }

    #[test]
    fn renders_empty_log() {
        let screen = draw(&Log::default(), Window::Session);
        for (name, description) in COLUMNS {
            let name = name.split_once(' ').map_or(name, |(_, word)| word);
            assert!(screen.contains(name), "{screen}");
            assert!(screen.contains(description), "{screen}");
        }
        assert!(screen.contains("No waits in this window"), "{screen}");
    }

    #[test]
    fn session_tab_counts_only_since_the_last_restart() {
        let log = log::parse(concat!(
            r#"{"kind":"status","ts_ms":0,"pane_id":"w1:p1","workspace":"api","status":"blocked"}"#,
            "\n",
            r#"{"kind":"status","ts_ms":60000,"pane_id":"w1:p1","workspace":"api","status":"working"}"#,
            "\n",
            r#"{"kind":"session_start","ts_ms":100000}"#,
            "\n",
            r#"{"kind":"status","ts_ms":200000,"pane_id":"w1:p1","workspace":"api","status":"blocked"}"#,
            "\n",
            r#"{"kind":"status","ts_ms":500000,"pane_id":"w1:p1","workspace":"api","status":"working"}"#,
        ));
        let session = draw(&log, Window::Session);
        assert!(session.contains("blocked 5m in total"), "{session}");
        let day = draw(&log, Window::Day);
        assert!(day.contains("blocked 6m in total"), "{day}");
    }
}
