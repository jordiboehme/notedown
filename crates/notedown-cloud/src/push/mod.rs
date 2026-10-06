//! Pushes to connected devices after a server-side change.
//!
//! [`PushPlanner`] decides, from journal events alone, which device gets a `STARTSYNC` hint and
//! when. It holds no clock and does no I/O: the caller passes the time in milliseconds.

mod planner;

pub use planner::{
    MAX_WAIT_MS, MIN_GAP_MS, Pending, PushPlanner, PushTiming, QUIET_MS, server_message_frame,
    startsync_message,
};
