//! Pushes to connected devices after a server-side change.
//!
//! [`PushPlanner`] decides, from journal events alone, which device gets a `STARTSYNC` hint and
//! when. It holds no clock and does no I/O: the caller passes the time in milliseconds. The
//! journal follower and the tick ([`Follower`], [`follow_once`], [`tick`]) feed it and hand the
//! due pushes to the socket sessions.

mod detail;
mod follower;
mod planner;

pub use detail::detail_message;
pub use follower::{Follower, follow_once, run, start_seq, tick};
pub use planner::{
    MAX_WAIT_MS, MIN_GAP_MS, Pending, PushPlanner, PushTiming, QUIET_MS, server_message_frame,
    startsync_message,
};
