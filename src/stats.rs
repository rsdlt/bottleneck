//! Wait statistics. Pure: no I/O and no clock (`now_ms` is a parameter), so
//! every rule here is pinned down by a unit test.
//!
//! An agent waits on you in two ways:
//! - **blocked**: it needs an answer, from `blocked` until any other known state;
//! - **unseen**: it finished, from `done` until you view it. Herdr defines
//!   `done` as "idle and not yet seen", so leaving `done` *is* the view.

use std::collections::{BTreeMap, HashMap};

use crate::event::{Record, Status};

/// The two ways an agent waits on you.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// Needs an answer: from `blocked` until any other known state.
    Blocked,
    /// Finished: from `done` until you view it.
    Unseen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum End {
    /// The agent moved on, or is still waiting now.
    Resolved,
    /// The pane closed while the agent was waiting.
    Abandoned,
    /// Herdr restarted while the agent was waiting, so the real end is unknown.
    Interrupted,
}

#[derive(Debug)]
struct Episode<'a> {
    wait: Wait,
    workspace: &'a str,
    agent: &'a str,
    start_ms: u64,
    end_ms: u64,
    end: End,
}

impl Episode<'_> {
    /// Which total this episode adds to, if any. Interrupted waits have no
    /// known end, and an unread pane closed unseen was never viewed.
    fn counted_as(&self) -> Option<Wait> {
        match (self.wait, self.end) {
            (_, End::Interrupted) | (Wait::Unseen, End::Abandoned) => None,
            (wait, _) => Some(wait),
        }
    }
}

/// Turns records into wait episodes, replaying each pane's states in time order.
fn episodes(records: &[Record], now_ms: u64) -> Vec<Episode<'_>> {
    // Hooks run concurrently, so the file order can disagree with the
    // timestamps. A stable sort keeps file order for equal timestamps.
    let mut sorted: Vec<&Record> = records.iter().collect();
    sorted.sort_by_key(|record| record.ts_ms());

    let mut open: HashMap<&str, Episode> = HashMap::new();
    let mut done = Vec::new();
    for record in sorted {
        match record {
            Record::Status {
                ts_ms,
                pane_id,
                workspace,
                agent,
                status,
            } => {
                let wait = match status {
                    // Herdr briefly reports `unknown` between real states;
                    // treat it as "no news" so it can't cut a wait short.
                    Status::Unknown => continue,
                    Status::Blocked => Some(Wait::Blocked),
                    Status::Done => Some(Wait::Unseen),
                    Status::Idle | Status::Working => None,
                };
                if let Some(current) = open.get(pane_id.as_str()) {
                    if Some(current.wait) == wait {
                        continue; // a repeat of the current state
                    }
                }
                if let Some(episode) = open.remove(pane_id.as_str()) {
                    done.push(Episode {
                        end_ms: *ts_ms,
                        ..episode
                    });
                }
                if let Some(wait) = wait {
                    let episode = Episode {
                        wait,
                        workspace,
                        agent: agent.as_deref().unwrap_or("(no agent)"),
                        start_ms: *ts_ms,
                        end_ms: *ts_ms,
                        end: End::Resolved,
                    };
                    open.insert(pane_id, episode);
                }
            }
            Record::Closed { ts_ms, pane_id } => {
                if let Some(episode) = open.remove(pane_id.as_str()) {
                    let end = End::Abandoned;
                    done.push(Episode {
                        end_ms: *ts_ms,
                        end,
                        ..episode
                    });
                }
            }
            Record::SessionStart { ts_ms } => {
                let end = End::Interrupted;
                done.extend(open.drain().map(|(_, episode)| Episode {
                    end_ms: *ts_ms,
                    end,
                    ..episode
                }));
            }
        }
    }
    // Whatever is still open is waiting right now.
    done.extend(open.into_values().map(|episode| Episode {
        end_ms: now_ms.max(episode.start_ms),
        ..episode
    }));
    done
}

/// Waits for one group: everything, one workspace, or one agent.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Summary {
    /// Length of each blocked wait, in ms.
    pub blocked: Vec<u64>,
    /// Blocked waits that ended because the pane closed.
    pub abandoned: usize,
    /// Time from `done` until viewed, in ms. Panes closed unseen are left
    /// out: they were never viewed, so there is no time to measure.
    pub unseen: Vec<u64>,
}

impl Summary {
    fn add(&mut self, episode: &Episode, ms: u64) {
        match episode.counted_as() {
            Some(Wait::Blocked) => {
                self.blocked.push(ms);
                self.abandoned += usize::from(episode.end == End::Abandoned);
            }
            Some(Wait::Unseen) => self.unseen.push(ms),
            None => {}
        }
    }

    pub fn blocked_total(&self) -> u64 {
        self.blocked.iter().sum()
    }

    pub fn unseen_total(&self) -> u64 {
        self.unseen.iter().sum()
    }

    /// The longest single wait of either kind. On a tie it reports blocked,
    /// the wait that needed you to act.
    pub fn longest(&self) -> Option<(u64, Wait)> {
        let unseen = self.unseen.iter().map(|&ms| (ms, Wait::Unseen));
        let blocked = self.blocked.iter().map(|&ms| (ms, Wait::Blocked));
        // `max_by_key` returns the last of equal maxima, so blocked goes last.
        unseen.chain(blocked).max_by_key(|&(ms, _)| ms)
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub total: Summary,
    /// Sorted by total blocked time, longest first.
    pub by_workspace: Vec<(String, Summary)>,
    /// Sorted by total blocked time, longest first.
    pub by_agent: Vec<(String, Summary)>,
    /// Waits cut off by a Herdr restart. Shown, but not counted.
    pub interrupted: usize,
}

/// Summarises waits in the window `since_ms..=now_ms`. A wait that started
/// before the window counts only its part inside it.
pub fn report(records: &[Record], since_ms: u64, now_ms: u64) -> Report {
    let mut report = Report::default();
    let mut by_workspace: BTreeMap<&str, Summary> = BTreeMap::new();
    let mut by_agent: BTreeMap<&str, Summary> = BTreeMap::new();

    let episodes = episodes(records, now_ms);
    for episode in episodes.iter().filter(|e| e.end_ms >= since_ms) {
        if episode.end == End::Interrupted {
            report.interrupted += 1;
            continue;
        }
        let ms = episode.end_ms - episode.start_ms.max(since_ms);
        report.total.add(episode, ms);
        by_workspace
            .entry(episode.workspace)
            .or_default()
            .add(episode, ms);
        by_agent.entry(episode.agent).or_default().add(episode, ms);
    }

    report.by_workspace = sorted_by_blocked(by_workspace);
    report.by_agent = sorted_by_blocked(by_agent);
    report
}

/// Blocked time for one pane in the window: the `$waited` sidebar token.
pub fn pane_blocked_ms(records: &[Record], pane_id: &str, since_ms: u64, now_ms: u64) -> u64 {
    // Session starts have no pane but must stay: they interrupt open waits.
    let pane: Vec<Record> = records
        .iter()
        .filter(|record| record.pane_id().is_none_or(|id| id == pane_id))
        .cloned()
        .collect();
    report(&pane, since_ms, now_ms).total.blocked_total()
}

/// When the current Herdr session started: the latest `session_start`.
/// Herdr doesn't run startup hooks when a plugin is first linked, so until
/// its next restart the session starts at the first record. `None` for an
/// empty log.
pub fn session_start_ms(records: &[Record]) -> Option<u64> {
    let restart = records
        .iter()
        .filter(|record| matches!(record, Record::SessionStart { .. }))
        .map(Record::ts_ms)
        .max();
    restart.or_else(|| records.iter().map(Record::ts_ms).min())
}

/// Time spent waiting within one slice of the histogram.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Slice {
    pub blocked_ms: u64,
    pub unseen_ms: u64,
}

/// Splits `since_ms..now_ms` into equal slices and adds each wait's time to
/// the slices it overlaps, so a long wait spreads across several bars
/// instead of piling into the one where it started.
pub fn histogram(records: &[Record], since_ms: u64, now_ms: u64, slices: usize) -> Vec<Slice> {
    let mut histogram = vec![Slice::default(); slices];
    let span = now_ms.saturating_sub(since_ms);
    let count = slices as u64;
    for episode in episodes(records, now_ms) {
        let Some(wait) = episode.counted_as() else {
            continue;
        };
        for (i, slice) in (0..).zip(histogram.iter_mut()) {
            // Integer bounds that tile the span exactly: no gaps, no overlaps.
            let start = since_ms + span * i / count;
            let end = since_ms + span * (i + 1) / count;
            let overlap = episode
                .end_ms
                .min(end)
                .saturating_sub(episode.start_ms.max(start));
            match wait {
                Wait::Blocked => slice.blocked_ms += overlap,
                Wait::Unseen => slice.unseen_ms += overlap,
            }
        }
    }
    histogram
}

fn sorted_by_blocked(groups: BTreeMap<&str, Summary>) -> Vec<(String, Summary)> {
    let mut rows: Vec<_> = groups
        .into_iter()
        .map(|(name, summary)| (name.to_owned(), summary))
        .collect();
    // Stable sort: ties stay in name order from the BTreeMap.
    rows.sort_by_key(|(_, summary)| std::cmp::Reverse(summary.blocked_total()));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    fn status(ts_ms: u64, pane_id: &str, status: Status) -> Record {
        let workspace = pane_id.split(':').next().unwrap_or_default();
        Record::Status {
            ts_ms,
            pane_id: pane_id.into(),
            workspace: workspace.into(),
            agent: Some("claude".into()),
            status,
        }
    }

    fn closed(ts_ms: u64, pane_id: &str) -> Record {
        Record::Closed {
            ts_ms,
            pane_id: pane_id.into(),
        }
    }

    /// Report over all time, with "now" well after every record.
    fn all(records: &[Record]) -> Report {
        report(records, 0, 1_000 * MIN)
    }

    #[test]
    fn blocked_wait_lasts_until_the_next_state() {
        let r = all(&[
            status(MIN, "w1:p1", Status::Blocked),
            status(3 * MIN, "w1:p1", Status::Working),
        ]);
        assert_eq!(r.total.blocked, [2 * MIN]);
        assert!(r.total.unseen.is_empty());
    }

    #[test]
    fn done_wait_lasts_until_viewed() {
        let r = all(&[
            status(0, "w1:p1", Status::Done),
            status(MIN, "w1:p1", Status::Idle),
        ]);
        assert_eq!(r.total.unseen, [MIN]);
        assert!(r.total.blocked.is_empty());
    }

    #[test]
    fn unknown_does_not_end_a_wait() {
        let r = all(&[
            status(0, "w1:p1", Status::Blocked),
            status(300, "w1:p1", Status::Unknown),
            status(1_200, "w1:p1", Status::Idle),
        ]);
        assert_eq!(r.total.blocked, [1_200]);
    }

    #[test]
    fn repeated_state_does_not_restart_the_wait() {
        let r = all(&[
            status(0, "w1:p1", Status::Blocked),
            status(500, "w1:p1", Status::Blocked),
            status(1_000, "w1:p1", Status::Working),
        ]);
        assert_eq!(r.total.blocked, [1_000]);
    }

    #[test]
    fn blocked_straight_to_done_is_two_waits() {
        let r = all(&[
            status(0, "w1:p1", Status::Blocked),
            status(MIN, "w1:p1", Status::Done),
            status(3 * MIN, "w1:p1", Status::Idle),
        ]);
        assert_eq!(r.total.blocked, [MIN]);
        assert_eq!(r.total.unseen, [2 * MIN]);
    }

    #[test]
    fn out_of_order_records_are_replayed_by_time() {
        let r = all(&[
            status(MIN, "w1:p1", Status::Working),
            status(0, "w1:p1", Status::Blocked),
        ]);
        assert_eq!(r.total.blocked, [MIN]);
    }

    #[test]
    fn pane_closed_while_blocked_counts_as_abandoned() {
        let r = all(&[
            status(0, "w1:p1", Status::Blocked),
            closed(5 * MIN, "w1:p1"),
        ]);
        assert_eq!(r.total.blocked, [5 * MIN]);
        assert_eq!(r.total.abandoned, 1);
    }

    #[test]
    fn pane_closed_while_done_has_no_view_time() {
        let r = all(&[status(0, "w1:p1", Status::Done), closed(MIN, "w1:p1")]);
        assert!(r.total.unseen.is_empty());
        assert_eq!(r.total.abandoned, 0);
    }

    #[test]
    fn restart_interrupts_open_waits_without_counting_them() {
        let r = all(&[
            status(0, "w1:p1", Status::Blocked),
            Record::SessionStart { ts_ms: MIN },
            // After a restart, w1:p1 may be a different pane: no wait to end.
            status(2 * MIN, "w1:p1", Status::Working),
        ]);
        assert!(r.total.blocked.is_empty());
        assert_eq!(r.interrupted, 1);
    }

    #[test]
    fn open_wait_counts_until_now() {
        let r = report(&[status(0, "w1:p1", Status::Blocked)], 0, 10 * MIN);
        assert_eq!(r.total.blocked, [10 * MIN]);
    }

    #[test]
    fn window_clips_waits_that_started_before_it() {
        let records = [
            status(0, "w1:p1", Status::Blocked),
            status(MIN, "w1:p1", Status::Working),
            status(2 * MIN, "w1:p1", Status::Blocked),
            status(10 * MIN, "w1:p1", Status::Working),
        ];
        let r = report(&records, 4 * MIN, 20 * MIN);
        assert_eq!(r.total.blocked, [6 * MIN]);
    }

    #[test]
    fn groups_are_sorted_by_blocked_time() {
        let mut records = vec![
            status(0, "a:p1", Status::Blocked),
            status(MIN, "a:p1", Status::Working),
            status(0, "b:p1", Status::Blocked),
            status(5 * MIN, "b:p1", Status::Working),
        ];
        records.push(Record::Status {
            ts_ms: 0,
            pane_id: "b:p2".into(),
            workspace: "b".into(),
            agent: None,
            status: Status::Blocked,
        });
        records.push(closed(2 * MIN, "b:p2"));
        let r = all(&records);

        let workspaces: Vec<_> = r
            .by_workspace
            .iter()
            .map(|(n, s)| (n.as_str(), s.blocked_total()))
            .collect();
        assert_eq!(workspaces, [("b", 7 * MIN), ("a", MIN)]);
        let agents: Vec<_> = r
            .by_agent
            .iter()
            .map(|(n, s)| (n.as_str(), s.blocked_total()))
            .collect();
        assert_eq!(agents, [("claude", 6 * MIN), ("(no agent)", 2 * MIN)]);
    }

    #[test]
    fn pane_blocked_counts_only_that_pane() {
        let records = [
            status(0, "w1:p1", Status::Blocked),
            status(0, "w1:p2", Status::Blocked),
            status(2 * MIN, "w1:p1", Status::Working),
            Record::SessionStart { ts_ms: 3 * MIN },
        ];
        assert_eq!(pane_blocked_ms(&records, "w1:p1", 0, 9 * MIN), 2 * MIN);
        // w1:p2's wait was cut off by the restart, so it doesn't count.
        assert_eq!(pane_blocked_ms(&records, "w1:p2", 0, 9 * MIN), 0);
    }

    #[test]
    fn session_starts_at_the_latest_restart() {
        assert_eq!(session_start_ms(&[]), None);
        // No restart recorded yet: the session starts at the first record.
        let records = [
            status(7, "w1:p1", Status::Idle),
            status(5, "w1:p1", Status::Done),
        ];
        assert_eq!(session_start_ms(&records), Some(5));
        let records = [
            Record::SessionStart { ts_ms: 9 },
            Record::SessionStart { ts_ms: 3 },
        ];
        assert_eq!(session_start_ms(&records), Some(9));
    }

    fn slice(blocked_ms: u64, unseen_ms: u64) -> Slice {
        Slice {
            blocked_ms,
            unseen_ms,
        }
    }

    #[test]
    fn histogram_spreads_a_wait_across_the_slices_it_spans() {
        let records = [
            status(MIN / 2, "w1:p1", Status::Blocked),
            status(3 * MIN / 2, "w1:p1", Status::Done),
            status(3 * MIN, "w1:p1", Status::Idle),
        ];
        let half = MIN / 2;
        let expected = [
            slice(half, 0),
            slice(half, half),
            slice(0, MIN),
            slice(0, 0),
        ];
        assert_eq!(histogram(&records, 0, 4 * MIN, 4), expected);
    }

    #[test]
    fn histogram_skips_waits_the_totals_skip() {
        let records = [
            status(0, "w1:p1", Status::Blocked),
            Record::SessionStart { ts_ms: MIN },
            status(0, "w1:p2", Status::Done),
            closed(MIN, "w1:p2"),
        ];
        assert_eq!(histogram(&records, 0, 2 * MIN, 2), [slice(0, 0); 2]);
    }

    #[test]
    fn histogram_of_an_empty_span_is_empty() {
        let records = [status(0, "w1:p1", Status::Blocked)];
        assert_eq!(histogram(&records, 5, 5, 3), [slice(0, 0); 3]);
    }

    #[test]
    fn longest_wait_is_either_kind_and_ties_go_to_blocked() {
        let summary = |blocked: &[u64], unseen: &[u64]| Summary {
            blocked: blocked.to_vec(),
            unseen: unseen.to_vec(),
            abandoned: 0,
        };
        assert_eq!(summary(&[], &[]).longest(), None);
        assert_eq!(summary(&[5, 2], &[3]).longest(), Some((5, Wait::Blocked)));
        assert_eq!(summary(&[2], &[3, 7]).longest(), Some((7, Wait::Unseen)));
        assert_eq!(summary(&[4], &[4]).longest(), Some((4, Wait::Blocked)));
    }

    #[test]
    fn fixture_with_mixed_edge_cases() {
        // Two workspaces, an unknown flicker, a pane closed while blocked,
        // records out of file order, and one truncated line.
        let log = crate::log::parse(include_str!("../fixtures/two-workspaces.jsonl"));
        assert_eq!(log.skipped, 1);

        let r = report(&log.records, 0, 2_000_000);
        assert_eq!(r.total.blocked_total(), 27 * MIN);
        assert_eq!(r.total.blocked.len(), 4);
        assert_eq!(r.total.blocked.iter().max(), Some(&(10 * MIN)));
        assert_eq!(r.total.abandoned, 1);
        assert_eq!(r.total.unseen, [MIN]);

        // The histogram adds up to the same totals as the report.
        let slices = histogram(&log.records, 0, 2_000_000, 24);
        let blocked: u64 = slices.iter().map(|s| s.blocked_ms).sum();
        let unseen: u64 = slices.iter().map(|s| s.unseen_ms).sum();
        assert_eq!((blocked, unseen), (27 * MIN, MIN));

        let names: Vec<_> = r.by_workspace.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["web", "api"]);
    }
}
