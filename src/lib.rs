//! bottleneck: measures how long Herdr agents sit waiting on you.
//!
//! The binary in `main.rs` is a thin shell over these modules:
//! [`event`] turns Herdr hook payloads into records, [`log`] stores them,
//! and [`stats`] turns them into a report.

pub mod event;
pub mod log;
pub mod stats;
