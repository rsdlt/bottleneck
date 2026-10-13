//! bottleneck: measures how long Herdr agents sit waiting on you.
//!
//! The binary in `main.rs` is a thin shell over these modules:
//! [`event`] turns Herdr hook payloads into records, [`log`] stores them,
//! [`stats`] turns them into a report,
//! and [`view`] shows it in a terminal.

pub mod event;
pub mod log;
pub mod quips;
pub mod stats;
pub mod view;
