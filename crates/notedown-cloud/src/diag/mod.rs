//! Diagnostics: the always-on protocol log.

mod capture;
mod log;
mod mask;
mod rotate;

pub use capture::Capture;
pub use log::{Clock, FLAG_FIRMWARE_CHANGED, LogEntry, LogKind, ProtocolLog, SystemClock};
pub use mask::{
    MAX_BODY_BYTES, MAX_FIELD_BYTES, Origin, cap_text, contains_jwt, cut_body, is_masked,
    is_sensitive_header, is_sensitive_key, is_sensitive_key_from, mask_form, mask_headers,
    mask_json, mask_json_from, mask_query, mask_text, mask_text_from, placeholder,
};
pub use rotate::{FIRMWARE_PROTECT_DAYS, LogFile, plan_deletions};
