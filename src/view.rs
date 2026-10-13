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

/// One tab of the report: a period to report on, or the clear-history page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Session,
    Day,
    Week,
    Clear,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Session, Tab::Day, Tab::Week, Tab::Clear];

    fn label(self) -> &'static str {
        match self {
            Tab::Session => "this session",
            Tab::Day => "last 24h",
            Tab::Week => "last 7 days",
            Tab::Clear => "clear history",
        }
    }

    /// Where the tab's period starts, or `None` for the clear-history tab.
    fn since_ms(self, records: &[Record], now_ms: u64) -> Option<u64> {
        match self {
            Tab::Session => Some(stats::session_start_ms(records).unwrap_or(now_ms)),
            Tab::Day => Some(now_ms.saturating_sub(24 * HOUR_MS)),
            Tab::Week => Some(now_ms.saturating_sub(7 * 24 * HOUR_MS)),
            Tab::Clear => None,
        }
    }

    fn next(self) -> Self {
        match self {
            Tab::Session => Tab::Day,
            Tab::Day => Tab::Week,
            Tab::Week => Tab::Clear,
            Tab::Clear => Tab::Session,
        }
    }
}

/// Clearing history takes two presses of `x`, so one stray key can't do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clearing {
    Idle,
    Armed,
    Done,
}

/// What the event loop should do after a key press.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Nothing,
    Quit,
    ClearHistory,
}

struct State {
    tab: Tab,
    clearing: Clearing,
    /// Picks the quip; drawn once so it doesn't change on every refresh.
    seed: u64,
}

impl State {
    fn on_key(&mut self, key: KeyCode) -> Action {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => return Action::Quit,
            KeyCode::Tab => {
                self.tab = self.tab.next();
                self.clearing = Clearing::Idle;
            }
            KeyCode::Char('x') if self.tab == Tab::Clear => {
                if self.clearing == Clearing::Armed {
                    self.clearing = Clearing::Done;
                    return Action::ClearHistory;
                }
                self.clearing = Clearing::Armed;
            }
            // Any other key cancels a pending clear.
            _ => self.clearing = Clearing::Idle,
        }
        Action::Nothing
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
    let mut state = State {
        tab: Tab::Session,
        clearing: Clearing::Idle,
        // std seeds every RandomState with fresh random keys, so hashing
        // nothing gives a random number without a `rand` dependency.
        seed: RandomState::new().build_hasher().finish(),
    };
    loop {
        let log = log::read(log_path)?;
        terminal.draw(|frame| render(frame, &log, &state, now_ms()))?;
        // Wake on a key press, or after REFRESH to pick up new records.
        if !event::poll(REFRESH)? {
            continue;
        }
        let Some(key) = event::read()?.as_key_press_event() else {
            continue;
        };
        match state.on_key(key.code) {
            Action::Quit => return Ok(()),
            Action::ClearHistory => log::clear(log_path)?,
            Action::Nothing => {}
        }
    }
}

fn render(frame: &mut Frame, log: &Log, state: &State, now_ms: u64) {
    let [legend, body] = Layout::vertical([
        Constraint::Length(COLUMNS.len() as u16 + 2),
        Constraint::Fill(1),
    ])
    .areas(frame.area());

    let tabs = Tab::ALL.map(|tab| {
        let span = Span::from(format!(" {} ", tab.label()));
        if tab == state.tab {
            span.reversed()
        } else {
            span
        }
    });
    let mut title = Line::from(" bottleneck ".bold());
    title.extend(tabs);
    frame.render_widget(
        legend_widget().block(Block::bordered().title(title)),
        legend,
    );

    match state.tab.since_ms(&log.records, now_ms) {
        Some(since_ms) => render_report(frame, body, log, since_ms, now_ms, state.seed),
        None => render_clear(frame, body, log, state.clearing, now_ms),
    }
}

fn render_report(frame: &mut Frame, area: Rect, log: &Log, since_ms: u64, now_ms: u64, seed: u64) {
    let report = stats::report(&log.records, since_ms, now_ms);
    let [header, workspaces, agents, chart, quip, footer] = Layout::vertical([
        Constraint::Length(5),
        Constraint::Fill(1),
        Constraint::Fill(1),
        // Borders, 3 rows of bars, a spacer and the axis row.
        Constraint::Length(7),
        Constraint::Length(3),
        Constraint::Length(1),
    ])
    .areas(area);

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

    let mut notes = vec![" tab: switch tab   q: quit".to_owned()];
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

/// The clear-history page: what is stored, the two-step action, a warning.
fn render_clear(frame: &mut Frame, area: Rect, log: &Log, clearing: Clearing, now_ms: u64) {
    let [page, footer] = Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);

    let stored = match log.records.iter().map(Record::ts_ms).min() {
        Some(oldest) => format!(
            " {} events recorded, the oldest from {} ago.",
            log.records.len(),
            human(now_ms.saturating_sub(oldest)),
        ),
        None => " Nothing recorded yet.".to_owned(),
    };
    let action = match clearing {
        Clearing::Idle => Line::from(" Press x to clear all recorded history."),
        Clearing::Armed => Line::from(" Press x again to confirm. Any other key cancels.".bold()),
        Clearing::Done => Line::from(" History cleared.".green()),
    };
    let lines = vec![
        Line::from(""),
        Line::from(stored),
        Line::from(format!(
            " Records older than {} days are pruned automatically when Herdr starts.",
            log::RETENTION_DAYS,
        ))
        .dim(),
        Line::from(""),
        action,
        Line::from(""),
        Line::from(" Warning: this permanently deletes every recorded wait for all".red()),
        Line::from(" agents and workspaces. It can't be undone.".red()),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" Clear history ")),
        page,
    );
    frame.render_widget(
        Line::from(" x: clear   tab: switch tab   q: quit".dim()),
        footer,
    );
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
    // The blank row keeps the colour key from reading as part of the bars.
    let [bars, _, axis] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(inner);

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
    // local timezone, so it can't show clock times. Style the spans, not the
    // lines: a line's style covers its whole area, so a dim line drawn last
    // would also dim the colour key.
    frame.render_widget(Line::from(format!("{} ago", human(span_ms)).dim()), axis);
    let key = Line::from(vec![
        "█".fg(BLOCKED_COLOR),
        " blocked   ".into(),
        "█".fg(UNREAD_COLOR),
        " unread".into(),
    ]);
    frame.render_widget(key.centered(), axis);
    frame.render_widget(Line::from("now".dim()).right_aligned(), axis);
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
    use ratatui::style::Modifier;

    #[test]
    fn human_durations() {
        assert_eq!(human(0), "0s");
        assert_eq!(human(59_999), "59s");
        assert_eq!(human(14 * 60_000 + 59_000), "14m");
        assert_eq!(human(2 * HOUR_MS + 5 * 60_000), "2h05m");
    }

    fn state(tab: Tab) -> State {
        State {
            tab,
            clearing: Clearing::Idle,
            seed: 0,
        }
    }

    fn draw(log: &Log, tab: Tab) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, log, &state(tab), 2_000_000))
            .unwrap();
        terminal.backend().to_string()
    }

    #[test]
    fn renders_fixture_report() {
        let log = log::parse(include_str!("../fixtures/two-workspaces.jsonl"));
        let screen = draw(&log, Tab::Day);
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
    fn histogram_key_is_not_dimmed() {
        let log = log::parse(include_str!("../fixtures/two-workspaces.jsonl"));
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &log, &state(Tab::Day), 2_000_000))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let row = (0..buffer.area.height)
            .find(|&y| {
                (0..buffer.area.width).any(|x| {
                    buffer[(x, y)].symbol() == "n"
                        && buffer[(x + 1, y)].symbol() == "o"
                        && buffer[(x + 2, y)].symbol() == "w"
                })
            })
            .unwrap();
        let swatches: Vec<_> = (0..buffer.area.width)
            .map(|x| &buffer[(x, row)])
            .filter(|cell| cell.symbol() == "█")
            .collect();
        assert_eq!(swatches.len(), 2);
        for cell in swatches {
            assert!(!cell.modifier.contains(Modifier::DIM), "{cell:?}");
        }
    }

    #[test]
    fn clearing_needs_two_presses_of_x() {
        let mut state = state(Tab::Clear);
        assert_eq!(state.on_key(KeyCode::Char('x')), Action::Nothing);
        assert_eq!(state.clearing, Clearing::Armed);
        assert_eq!(state.on_key(KeyCode::Char('x')), Action::ClearHistory);
        assert_eq!(state.clearing, Clearing::Done);
    }

    #[test]
    fn any_other_key_cancels_a_pending_clear() {
        let mut state = state(Tab::Clear);
        state.on_key(KeyCode::Char('x'));
        state.on_key(KeyCode::Char('a'));
        assert_eq!(state.on_key(KeyCode::Char('x')), Action::Nothing);
        assert_eq!(state.clearing, Clearing::Armed);
        // Leaving the tab cancels too, and lands back on the first tab.
        state.on_key(KeyCode::Tab);
        assert_eq!((state.tab, state.clearing), (Tab::Session, Clearing::Idle));
    }

    #[test]
    fn x_does_nothing_outside_the_clear_tab() {
        let mut state = state(Tab::Day);
        assert_eq!(state.on_key(KeyCode::Char('x')), Action::Nothing);
        assert_eq!(state.on_key(KeyCode::Char('x')), Action::Nothing);
    }

    #[test]
    fn renders_clear_page_with_warning() {
        let log = log::parse(include_str!("../fixtures/two-workspaces.jsonl"));
        let screen = draw(&log, Tab::Clear);
        assert!(screen.contains("clear history"), "{screen}");
        assert!(
            screen.contains("11 events recorded, the oldest from 33m ago"),
            "{screen}"
        );
        assert!(screen.contains("older than 60 days are pruned"), "{screen}");
        assert!(
            screen.contains("Press x to clear all recorded history"),
            "{screen}"
        );
        assert!(screen.contains("It can't be undone"), "{screen}");

        let mut armed = state(Tab::Clear);
        armed.clearing = Clearing::Armed;
        let mut terminal = Terminal::new(TestBackend::new(80, 40)).unwrap();
        terminal
            .draw(|frame| render(frame, &log, &armed, 2_000_000))
            .unwrap();
        let screen = terminal.backend().to_string();
        assert!(screen.contains("Press x again to confirm"), "{screen}");
    }

    #[test]
    fn renders_empty_log() {
        let screen = draw(&Log::default(), Tab::Session);
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
        let session = draw(&log, Tab::Session);
        assert!(session.contains("blocked 5m in total"), "{session}");
        let day = draw(&log, Tab::Day);
        assert!(day.contains("blocked 6m in total"), "{day}");
    }
}
