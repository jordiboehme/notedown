//! Pushes to connected devices after a server-side change.
//!
//! [`PushPlanner`] decides, from journal events alone, which device gets a push and when, and
//! keeps the pending changes collapsed per file or folder ([`Changes`]). It holds no clock and
//! does no I/O: the caller passes the time in milliseconds. The journal follower and the tick
//! ([`Follower`], [`follow_once`], [`tick`]) feed it and hand the due pushes to the socket
//! sessions.

mod changes;
mod detail;
mod follower;
mod planner;

pub use changes::{Change, Changes};
pub use detail::detail_items;
pub use follower::{Follower, follow_once, run, start_seq, tick};
pub use planner::{
    MAX_WAIT_MS, MIN_GAP_MS, Pending, PushItem, PushPlanner, PushTiming, QUIET_MS, RETRY_MS,
    equipment_no, file_syn_message, server_message_frame, startsync_item,
};
