//! The report pane: a ratatui view over the log.
//!
//! It re-reads the log every couple of seconds instead of subscribing to
//! Herdr's socket: the hooks already capture every event, and the log is
//! small, so a file read keeps the view simple and the data in one place.

use std::hash::{BuildHasher, Hasher, RandomState};
use std::io;
use std::path::Path;
use std::time::Duration;

use ratatui::crossterm::event::{self, KeyCode};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Bar, BarChart, BarGroup, Block, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::event::Record;
use crate::log::{self, Log};
use crate::quips;
use crate::stats::{self, Slice, Summary, Wait};

/// The metric columns as (header, description). The table header and the
/// legend both read from here, so they can't drift apart. Every name starts
/// with a single-codepoint emoji: those render two cells wide everywhere,
/// while emoji with variation selectors (like ⚠️) break column alignment.
const COLUMNS: [(&str, &str); 3] = [
    (
        "✋ Blocked",
        "total time agents sat blocked, waiting for your answer",
    ),
    (
        "👀 Unread",
        "total time finished agents waited until you looked at them",
    ),
    ("🐢 Longest", "longest single wait, blocked or unread"),
];

const WORKSPACE: &str = "📁 Workspace";
const AGENT: &str = "🤖 Agent";

/// Names longer than this are truncated so the metrics stay on screen.
const MAX_NAME_WIDTH: usize = 30;

/// Bars in the histogram. Each slice draws a blocked and an unread bar plus
/// a gap, so 24 slices take 71 cells and fit the 80-cell popup.
const SLICES: usize = 24;
const BLOCKED_COLOR: Color = Color::Red;
const UNREAD_COLOR: Color = Color::Yellow;

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
            Window::Session => stats::session_start_ms(records).unwrap_or(now_ms),
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
    // std seeds every RandomState with fresh random keys, so hashing nothing
    // gives a random number without pulling in a `rand` dependency.
    let seed = RandomState::new().build_hasher().finish();
    loop {
        let log = log::read(log_path)?;
        terminal.draw(|frame| render(frame, &log, window, now_ms(), seed))?;
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

fn render(frame: &mut Frame, log: &Log, window: Window, now_ms: u64, seed: u64) {
    let since_ms = window.since_ms(&log.records, now_ms);
    let report = stats::report(&log.records, since_ms, now_ms);
    let [legend, header, workspaces, agents, chart, quip, footer] = Layout::vertical([
        Constraint::Length(COLUMNS.len() as u16 + 2),
        Constraint::Length(5),
        Constraint::Fill(1),
        Constraint::Fill(1),
        Constraint::Length(9),
        Constraint::Length(3),
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
    // Size the name column to its widest entry instead of stretching it, so
    // the tables pack left and leftover space falls at the right edge. Both
    // tables share the width so their metric columns line up.
    let names = report.by_workspace.iter().chain(&report.by_agent);
    let name_width = names
        .map(|(name, _)| Span::from(name.as_str()).width())
        .chain([WORKSPACE, AGENT].map(|header| Span::from(header).width()))
        .max()
        .unwrap_or_default()
        .min(MAX_NAME_WIDTH);
    let name_width = name_width as u16 + 2;
    frame.render_widget(
        table(WORKSPACE, &report.by_workspace, name_width),
        workspaces,
    );
    frame.render_widget(table(AGENT, &report.by_agent, name_width), agents);

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
    let slices = stats::histogram(&log.records, since_ms, now_ms, SLICES);
    render_histogram(frame, chart, &slices, now_ms - since_ms.min(now_ms));

    let message = quips::pick(
        report.total.blocked_total(),
        report.total.unseen_total(),
        seed,
    );
    let quip_widget =
        Paragraph::new(format!(" {message}").italic()).block(Block::bordered().title(" 💬 "));
    frame.render_widget(quip_widget, quip);
    frame.render_widget(Line::from(notes.join("  ·  ")).dim(), footer);
}

/// Draws the histogram with a time axis underneath. There is no y-axis, so
/// the title gives the scale: what one bar covers and the tallest bar.
fn render_histogram(frame: &mut Frame, area: Rect, slices: &[Slice], span_ms: u64) {
    let tallest = slices
        .iter()
        .map(|slice| slice.blocked_ms.max(slice.unseen_ms))
        .max()
        .unwrap_or_default();
    let title = format!(
        " 📈 over time · one bar = {} · tallest {} ",
        human(span_ms / SLICES as u64),
        human(tallest),
    );
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let width = (SLICES * 3 - 1) as u16;
    let [inner] = Layout::horizontal([Constraint::Length(width)]).areas(inner);
    let [bars, axis] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(inner);

    let groups: Vec<BarGroup> = slices
        .iter()
        .map(|slice| {
            // An empty text value stops ratatui printing numbers in the bars.
            BarGroup::new([
                Bar::new(slice.blocked_ms)
                    .style(BLOCKED_COLOR)
                    .text_value(""),
                Bar::new(slice.unseen_ms).style(UNREAD_COLOR).text_value(""),
            ])
        })
        .collect();
    let chart = BarChart::grouped(groups)
        .bar_width(1)
        .bar_gap(0)
        .group_gap(1);
    frame.render_widget(chart, bars);

    // Three lines share the axis row: start on the left, key in the middle,
    // "now" on the right. The x-axis is relative because we don't know the
    // local timezone, so it can't show clock times.
    frame.render_widget(Line::from(format!("{} ago", human(span_ms))).dim(), axis);
    let key = Line::from(vec![
        "█".fg(BLOCKED_COLOR),
        " blocked   ".into(),
        "█".fg(UNREAD_COLOR),
        " unread".into(),
    ]);
    frame.render_widget(key.centered(), axis);
    frame.render_widget(Line::from("now").dim().right_aligned(), axis);
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
    let mut blocked = format!(" ✋ blocked {} in total", human(total.blocked_total()));
    if total.abandoned > 0 {
        blocked += &format!(" · {} abandoned", total.abandoned);
    }
    let unseen = format!(
        " 👀 unread {} in total over {} finished tasks",
        human(total.unseen_total()),
        total.unseen.len(),
    );
    let longest = format!(" 🐢 longest {}", longest(total));
    vec![Line::from(blocked), Line::from(unseen), Line::from(longest)]
}

fn table<'a>(kind: &'a str, rows: &'a [(String, Summary)], name_width: u16) -> Table<'a> {
    let header = Row::new(std::iter::once(kind).chain(COLUMNS.map(|(name, _)| name))).bold();
    let rows = rows.iter().map(|(name, s)| {
        Row::new([
            name.clone(),
            human(s.blocked_total()),
            human(s.unseen_total()),
            longest(s),
        ])
    });
    let widths = [
        Constraint::Length(name_width),
        Constraint::Length(11),
        Constraint::Length(11),
        // The last column takes the rest: "2h05m blocked" needs 13 cells.
        Constraint::Fill(1),
    ];
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

/// The longest wait and its kind, such as `10m blocked`.
fn longest(summary: &Summary) -> String {
    match summary.longest() {
        Some((ms, Wait::Blocked)) => format!("{} blocked", human(ms)),
        Some((ms, Wait::Unseen)) => format!("{} unread", human(ms)),
        None => "–".to_owned(),
    }
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
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, log, window, 2_000_000, 0))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn renders_fixture_report() {
        let log = log::parse(include_str!("../fixtures/two-workspaces.jsonl"));
        let screen = draw(&log, Window::Day);
        assert!(screen.contains("blocked 27m in total"), "{screen}");
        assert!(screen.contains("longest 10m blocked"), "{screen}");
        // Blocked (27m) outweighs unread (1m); seed 0 picks the first quip.
        assert!(screen.contains("arms fell asleep"), "{screen}");
        assert!(
            screen.contains("unread 1m in total over 1 finished tasks"),
            "{screen}"
        );
        assert!(screen.contains("1 abandoned"), "{screen}");
        // The name column fits its content, so metrics sit next to names.
        let web = screen.lines().find(|line| line.contains("│web")).unwrap();
        assert!(
            web.find("15m").unwrap() - web.find("web").unwrap() < 20,
            "{web}"
        );
        assert!(screen.contains("codex"), "{screen}");
        // "Last 24h" is capped at the first record, 33 minutes before now.
        assert!(screen.contains("over time · one bar = 1m"), "{screen}");
        assert!(screen.contains("33m ago"), "{screen}");
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
        assert!(screen.contains("Nobody is waiting on you"), "{screen}");
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
