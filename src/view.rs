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

use crate::log::{self, Log};
use crate::stats::{self, Summary, median};

/// The metric columns as (header, description). The table header and the
/// legend both read from here, so they can't drift apart. Every name starts
/// with a single-codepoint emoji: those render two cells wide everywhere,
/// while emoji with variation selectors (like ⚠️) break column alignment.
const COLUMNS: [(&str, &str); 5] = [
    (
        "✋ Blocked",
        "total time agents sat blocked, waiting for your answer",
    ),
    ("🔢 Blocks", "how many times an agent got blocked"),
    ("📊 Typical", "median length of one block"),
    ("🐢 Longest", "longest single block"),
    (
        "👀 Unread",
        "median time from an agent finishing until you looked at it",
    ),
];

const REFRESH: Duration = Duration::from_secs(2);
const HOUR_MS: u64 = 60 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Window {
    Day,
    Week,
}

impl Window {
    fn ms(self) -> u64 {
        match self {
            Window::Day => 24 * HOUR_MS,
            Window::Week => 7 * 24 * HOUR_MS,
        }
    }

    fn toggle(self) -> Self {
        match self {
            Window::Day => Window::Week,
            Window::Week => Window::Day,
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
    let mut window = Window::Day;
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
                KeyCode::Tab => window = window.toggle(),
                _ => {}
            }
        }
    }
}

fn render(frame: &mut Frame, log: &Log, window: Window, now_ms: u64) {
    let report = stats::report(&log.records, now_ms.saturating_sub(window.ms()), now_ms);
    let [legend, header, workspaces, agents, footer] = Layout::vertical([
        Constraint::Length(COLUMNS.len() as u16 + 2),
        Constraint::Length(4),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    let tab = |label: &'static str, this: Window| {
        if this == window {
            Span::from(format!(" {label} ")).reversed()
        } else {
            Span::from(format!(" {label} "))
        }
    };
    let title = Line::from(vec![
        " bottleneck: agents waiting on you ".bold(),
        tab("last 24h", Window::Day),
        tab("last 7 days", Window::Week),
    ]);
    frame.render_widget(
        legend_widget().block(Block::bordered().title(title)),
        legend,
    );
    let summary =
        Paragraph::new(summary_lines(&report.total)).block(Block::bordered().title(" Summary "));
    frame.render_widget(summary, header);
    frame.render_widget(table("📁 Workspace", &report.by_workspace), workspaces);
    frame.render_widget(table("🤖 Agent", &report.by_agent), agents);

    let mut notes = vec![" tab: 24h / 7 days   q: quit".to_owned()];
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
        " ✋ blocked {} in total · 🔢 {} blocks · 📊 typical {} · 🐢 longest {}",
        human(total.blocked_total()),
        total.blocked.len(),
        maybe(median(&total.blocked)),
        maybe(total.blocked.iter().max().copied()),
    );
    if total.abandoned > 0 {
        blocked += &format!(" · {} abandoned", total.abandoned);
    }
    let unseen = format!(
        " 👀 unread: typical {} over {} finished tasks",
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
            s.blocked.len().to_string(),
            maybe(median(&s.blocked)),
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

    fn draw(log: &Log) -> String {
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, log, Window::Day, 2_000_000))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn renders_fixture_report() {
        let screen = draw(&log::parse(include_str!(
            "../fixtures/two-workspaces.jsonl"
        )));
        assert!(screen.contains("blocked 27m in total"), "{screen}");
        assert!(screen.contains("4 blocks"), "{screen}");
        assert!(
            screen.contains("typical 1m over 1 finished tasks"),
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
        let screen = draw(&Log::default());
        for (name, description) in COLUMNS {
            let name = name.split_once(' ').map_or(name, |(_, word)| word);
            assert!(screen.contains(name), "{screen}");
            assert!(screen.contains(description), "{screen}");
        }
        assert!(screen.contains("No waits in this window"), "{screen}");
    }
}
