//! Diagnostics: the always-on protocol log.

mod log;
mod mask;
mod rotate;

pub use log::{Clock, LogEntry, LogKind, ProtocolLog, SystemClock};
pub use mask::{
    MAX_BODY_BYTES, MAX_FIELD_BYTES, cap_text, contains_jwt, cut_body, is_masked,
    is_sensitive_header, is_sensitive_key, mask_form, mask_headers, mask_json, mask_query,
    mask_text, placeholder,
};
pub use rotate::{FIRMWARE_PROTECT_DAYS, LogFile, plan_deletions};
