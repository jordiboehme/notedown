//! Rotation policy: which log files to delete.

use std::path::PathBuf;

use time::Date;

/// Files within this many days of a `firmware_changed` marker are never deleted.
pub const FIRMWARE_PROTECT_DAYS: i32 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFile {
    pub date: Date,
    pub size: u64,
    pub path: PathBuf,
}

/// Decide which files to delete: older than the retention first, then oldest first until the total
/// size fits the cap. Today's file and files near a firmware change are kept (so the cap can be
/// exceeded by protected files).
pub fn plan_deletions(
    files: &[LogFile],
    firmware_dates: &[Date],
    today: Date,
    retention_days: u32,
    max_bytes: u64,
) -> Vec<PathBuf> {
    let protected = |d: Date| {
        d >= today
            || firmware_dates
                .iter()
                .any(|m| (d.to_julian_day() - m.to_julian_day()).abs() <= FIRMWARE_PROTECT_DAYS)
    };
    let mut sorted: Vec<&LogFile> = files.iter().collect();
    sorted.sort_by_key(|f| f.date);

    let oldest_kept = today.to_julian_day() - retention_days as i32;
    let mut total: u64 = sorted.iter().map(|f| f.size).sum();
    let mut out = Vec::new();
    for f in sorted {
        if protected(f.date) {
            continue;
        }
        let expired = f.date.to_julian_day() < oldest_kept;
        if expired || total > max_bytes {
            total = total.saturating_sub(f.size);
            out.push(f.path.clone());
        }
    }
    out
}
