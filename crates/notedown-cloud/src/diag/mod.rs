//! Diagnostics: the always-on protocol log.

mod log;
mod mask;
mod rotate;

pub use log::{Clock, LogEntry, LogKind, ProtocolLog, SystemClock};
pub use mask::{
    MAX_BODY_BYTES, SELECTED_HEADERS, cut_body, is_sensitive_key, mask_headers, mask_json,
    mask_query, select_headers,
};
pub use rotate::{FIRMWARE_PROTECT_DAYS, LogFile, plan_deletions};
