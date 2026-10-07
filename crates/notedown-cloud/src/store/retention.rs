//! The retention policy of the recycle bin: which index lines to remove.
//!
//! Pure: no file access. The caller passes the parsed index, a lookup for ids the store still
//! knows, the settings and the clock. Days and months are local calendar buckets counted back
//! from now (also when a bucket has no line).
//!
//! - A live file keeps its `keep_latest` newest lines, the newest line of each of the last
//!   `keep_daily` local days and the newest line of each of the last `keep_monthly` local
//!   months. Every other line goes.
//! - A deleted file (id unknown to the store) follows the same rules until its newest line is
//!   older than `keep_deleted_days`. Then every line of it goes.

use std::collections::{BTreeMap, HashSet};

use time::{Date, OffsetDateTime, UtcOffset};

use super::RecycleEntry;

const DAY_MS: u64 = 86_400_000;

/// The retention settings (from `server.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Newest lines kept per file, whatever their age.
    pub keep_latest: u32,
    /// Local days (back from today) that keep their newest line.
    pub keep_daily: u32,
    /// Local months (back from this month) that keep their newest line.
    pub keep_monthly: u32,
    /// A deleted file whose newest line is older than this many days is removed completely.
    pub keep_deleted_days: u32,
}

/// Which lines to remove. `entries` is the parsed index in file order; `live(id)` says whether
/// the store still knows the id. Returns the indices into `entries` to remove, ascending.
pub fn plan(
    entries: &[RecycleEntry],
    live: &dyn Fn(u64) -> bool,
    retention: &Retention,
    now_ms: u64,
    offset: UtcOffset,
) -> Vec<usize> {
    let local = |ms: u64| -> Option<Date> {
        OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
            .ok()
            .map(|t| t.to_offset(offset).date())
    };
    let today = local(now_ms);
    let month_of = |d: Date| i64::from(d.year()) * 12 + i64::from(u8::from(d.month())) - 1;
    // First day / month still inside the window; `None` when the window is empty or `now` is
    // not representable.
    let first_day = today
        .filter(|_| retention.keep_daily > 0)
        .map(|d| i64::from(d.to_julian_day()) - (i64::from(retention.keep_daily) - 1));
    let first_month = today
        .filter(|_| retention.keep_monthly > 0)
        .map(|d| month_of(d) - (i64::from(retention.keep_monthly) - 1));

    let mut groups: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        groups.entry(e.id).or_default().push(i);
    }

    let mut remove = Vec::new();
    for (id, mut idx) in groups {
        // Newest first; equal times: the later line is newer.
        idx.sort_by_key(|&i| std::cmp::Reverse((entries[i].deleted_at, i)));
        if !live(id) {
            let newest = entries[idx[0]].deleted_at;
            if now_ms.saturating_sub(newest) > u64::from(retention.keep_deleted_days) * DAY_MS {
                remove.extend(idx);
                continue;
            }
        }
        let mut kept_latest = 0u32;
        let mut days = HashSet::new();
        let mut months = HashSet::new();
        for i in idx {
            let latest = kept_latest < retention.keep_latest;
            if latest {
                kept_latest += 1;
            }
            // A timestamp `time` can't represent is never removed by the window rules.
            let Some(date) = local(entries[i].deleted_at) else {
                continue;
            };
            // Every rule picks the newest line of its bucket on its own, so a line kept as one
            // of the newest also fills its buckets.
            let mut keep = latest;
            let day = i64::from(date.to_julian_day());
            if first_day.is_some_and(|f| day >= f) {
                keep |= days.insert(day);
            }
            if first_month.is_some_and(|f| month_of(date) >= f) {
                keep |= months.insert(month_of(date));
            }
            if !keep {
                remove.push(i);
            }
        }
    }
    remove.sort_unstable();
    remove
}

#[cfg(test)]
mod tests {
    use time::{Month, PrimitiveDateTime, Time};

    use super::super::RecycleReason;
    use super::*;

    fn offset() -> UtcOffset {
        UtcOffset::from_hms(2, 0, 0).unwrap()
    }

    fn at(y: i32, m: u8, d: u8, h: u8, min: u8) -> u64 {
        let date = Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap();
        let t = PrimitiveDateTime::new(date, Time::from_hms(h, min, 0).unwrap())
            .assume_offset(offset());
        (t.unix_timestamp() as u64) * 1000
    }

    fn entry(id: u64, deleted_at: u64, reason: RecycleReason) -> RecycleEntry {
        RecycleEntry {
            id,
            path: format!("NOTE/Note/{id}.note"),
            md5: "m".into(),
            deleted_at,
            reason,
        }
    }

    fn over(id: u64, t: u64) -> RecycleEntry {
        entry(id, t, RecycleReason::Overwritten)
    }

    fn live5(id: u64) -> bool {
        id == 5
    }

    fn r(l: u32, d: u32, m: u32, days: u32) -> Retention {
        Retention {
            keep_latest: l,
            keep_daily: d,
            keep_monthly: m,
            keep_deleted_days: days,
        }
    }

    #[test]
    fn table() {
        let now = at(2026, 10, 7, 12, 0);
        let live_lines = [
            at(2025, 9, 15, 10, 0),  // 0 a remove
            at(2025, 11, 20, 9, 0),  // 1 b keep
            at(2025, 11, 2, 9, 0),   // 2 c remove
            at(2026, 8, 31, 23, 30), // 3 d keep
            at(2026, 9, 30, 23, 59), // 4 e keep
            at(2026, 10, 1, 0, 10),  // 5 f keep
            at(2026, 10, 5, 8, 0),   // 6 g remove
            at(2026, 10, 5, 18, 0),  // 7 h keep
            at(2026, 10, 7, 9, 0),   // 8 i keep
            at(2026, 10, 7, 10, 0),  // 9 j keep
            at(2026, 10, 7, 11, 0),  // 10 k keep
        ];
        let mut entries: Vec<_> = live_lines.iter().map(|&t| over(5, t)).collect();
        entries.push(over(9, at(2026, 6, 1, 10, 0))); // 11
        entries.push(entry(9, at(2026, 7, 1, 10, 0), RecycleReason::Deleted)); // 12
        entries.push(over(10, at(2026, 8, 1, 10, 0))); // 13
        entries.push(over(10, at(2026, 8, 15, 10, 0))); // 14
        entries.push(entry(10, at(2026, 9, 1, 10, 0), RecycleReason::Deleted)); // 15
        entries.push(entry(11, now - 90 * DAY_MS, RecycleReason::Deleted)); // 16
        entries.push(entry(12, now - 90 * DAY_MS - 1, RecycleReason::Deleted)); // 17
        let got = plan(&entries, &live5, &r(3, 7, 12, 90), now, offset());
        assert_eq!(got, vec![0, 2, 6, 11, 12, 17]);
    }

    #[test]
    fn minimum_keeps_the_newest_line_of_a_live_id() {
        let now = at(2026, 10, 7, 12, 0);
        let entries = vec![
            over(5, at(2026, 1, 1, 10, 0)),
            over(5, at(2026, 3, 1, 10, 0)),
            over(5, at(2026, 2, 1, 10, 0)),
        ];
        let got = plan(&entries, &live5, &r(1, 0, 0, 1), now, offset());
        assert_eq!(got, vec![0, 2]);
    }

    #[test]
    fn empty_index_gives_empty_plan() {
        let got = plan(&[], &live5, &r(3, 7, 12, 90), 1_000, offset());
        assert!(got.is_empty());
    }

    #[test]
    fn interleaved_ids_are_grouped() {
        let now = at(2026, 10, 7, 12, 0);
        let t = at(2025, 1, 10, 10, 0);
        // Ids 5 (live) and 7 (live too) alternate; each keeps only its own newest line.
        let entries = vec![over(5, t), over(7, t), over(5, t + 1000), over(7, t + 1000)];
        let got = plan(&entries, &|_| true, &r(1, 0, 0, 90), now, offset());
        assert_eq!(got, vec![0, 1]);
    }

    #[test]
    fn equal_times_later_line_is_newer() {
        let now = at(2026, 10, 7, 12, 0);
        let t = at(2025, 1, 10, 10, 0);
        let entries = vec![over(5, t), over(5, t)];
        let got = plan(&entries, &live5, &r(1, 0, 0, 90), now, offset());
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn a_newest_line_also_fills_its_day_and_month() {
        let now = at(2026, 10, 7, 12, 0);
        let entries: Vec<_> = [8, 9, 10, 11]
            .iter()
            .map(|&h| over(5, at(2026, 10, 7, h, 0)))
            .collect();
        let got = plan(&entries, &live5, &r(3, 7, 12, 90), now, offset());
        assert_eq!(got, vec![0]);
    }

    #[test]
    fn huge_windows_do_not_overflow() {
        let now = at(2026, 10, 7, 12, 0);
        let entries = vec![over(5, at(2001, 1, 1, 0, 0)), over(5, at(2026, 1, 1, 0, 0))];
        let big = r(1, u32::MAX, u32::MAX, u32::MAX);
        assert!(plan(&entries, &live5, &big, now, offset()).is_empty());
    }

    #[test]
    fn unrepresentable_timestamp_is_kept() {
        let now = at(2026, 10, 7, 12, 0);
        let entries = vec![over(5, u64::MAX), over(5, u64::MAX - 1)];
        let got = plan(&entries, &live5, &r(1, 7, 12, u32::MAX), now, offset());
        assert!(got.is_empty());
    }

    #[test]
    fn keep_latest_alone() {
        let now = at(2026, 10, 7, 12, 0);
        let entries: Vec<_> = (1..=4).map(|d| over(5, at(2025, 1, d, 10, 0))).collect();
        let got = plan(&entries, &live5, &r(2, 0, 0, 90), now, offset());
        assert_eq!(got, vec![0, 1]);
    }
}
