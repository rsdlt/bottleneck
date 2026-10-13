//! A light-hearted line for the bottom of the report, aimed at whichever
//! wait cost you more.

const BLOCKED: [&str; 10] = [
    "Your agents raised their hands so long their arms fell asleep.",
    "Somewhere, an agent is still waiting for you to press 'y'.",
    "You are the slowest dependency in this pipeline.",
    "Your agents started a support group. You are the main topic.",
    "In the time they waited, your agents could have rewritten it in Rust.",
    "Blocked on human. Have you tried turning yourself off and on again?",
    "Your agents wait at the door like a dog. A very expensive dog.",
    "Fun fact: 'y' is one of the fastest keys on the keyboard. Try it.",
    "Your agents asked one question and aged a year.",
    "Permission prompts: the new 'your call is important to us'.",
];

const UNREAD: [&str; 10] = [
    "Your agents finished, then waited, then wrote poetry about waiting.",
    "Work finished, nobody looked. A tree fell in the forest.",
    "Your agents leave finished work on your desk like a cat with a mouse.",
    "Finished tasks, sitting unread like an unused gym membership.",
    "Your agents shipped. You didn't notice. Classic.",
    "The work is done. The reviewer is also you. Hello?",
    "Agents: done in 4 minutes. You: reading it in 40.",
    "Your pile of finished work is now a museum exhibit.",
    "Done, done, done. Like a microwave beeping in an empty kitchen.",
    "Your agents finished early just to practise patience.",
];

const NOTHING: &str = "Nobody is waiting on you. Suspicious, but well done.";

/// Picks a message for the larger of the two totals. The caller draws `seed`
/// once per report, so the message doesn't change on every refresh.
pub fn pick(blocked_ms: u64, unread_ms: u64, seed: u64) -> &'static str {
    let pool = match (blocked_ms, unread_ms) {
        (0, 0) => return NOTHING,
        (blocked, unread) if blocked >= unread => &BLOCKED,
        _ => &UNREAD,
    };
    pool[(seed % pool.len() as u64) as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_from_the_pool_of_the_longer_wait() {
        for seed in 0..20 {
            assert!(BLOCKED.contains(&pick(5, 3, seed)));
            assert!(UNREAD.contains(&pick(3, 5, seed)));
        }
    }

    #[test]
    fn a_tie_counts_as_blocked() {
        assert!(BLOCKED.contains(&pick(5, 5, 0)));
    }

    #[test]
    fn no_waits_gets_its_own_message() {
        assert_eq!(pick(0, 0, 3), NOTHING);
    }

    #[test]
    fn the_seed_chooses_the_message() {
        assert_ne!(pick(5, 0, 0), pick(5, 0, 1));
        assert_eq!(pick(5, 0, 1), pick(5, 0, 11));
    }

    #[test]
    fn every_message_fits_the_popup() {
        // 80-cell popup, minus borders and padding.
        for message in BLOCKED.iter().chain(&UNREAD).chain([&NOTHING]) {
            assert!(message.chars().count() <= 76, "{message}");
        }
    }
}
