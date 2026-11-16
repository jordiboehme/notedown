//! The retention policy of the recycle bin: which index lines to remove.
//!
//! Pure: no file access. The caller passes the parsed index, a lookup for ids the store still
//! knows, the settings and the clock.
//!
//! A line holds the version that was current from the time of the line before it (same id) to
//! its own time (`deletedAt`, when the next version replaced it). The oldest line of an id has
//! an open start. Days and months are local calendar days and months counted back from now
//! (also when they have no line). The end of a day or month is the local midnight after it.
//!
//! - A live file keeps its `keep_latest` newest lines, the line current at the end of each of
//!   the last `keep_daily` local days and the line current at the end of each of the last
//!   `keep_monthly` local months. An end that is still in the future (today, this month) needs
//!   no line: the live file is that version. Every other line is removed.
//! - A deleted file (id unknown to the store) follows the same rules until its newest line is
//!   older than `keep_deleted_days`. Then every line of it is removed.

use std::collections::BTreeMap;

use time::{Date, OffsetDateTime, UtcOffset};

use super::{RecycleEntry, RecycleReason};

const DAY_MS: u64 = 86_400_000;

/// The retention settings (from `server.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Newest lines kept per file, whatever their age.
    pub keep_latest: u32,
    /// Local days (back from today) whose end keeps the line current then.
    pub keep_daily: u32,
    /// Local months (back from this month) whose end keeps the line current then.
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
    let day_of = |d: Date| i64::from(d.to_julian_day());
    let month_of = |d: Date| i64::from(d.year()) * 12 + i64::from(u8::from(d.month())) - 1;
    // First day / month still inside the window; `None` when the window is empty or `now` is
    // not representable.
    let first_day = today
        .filter(|_| retention.keep_daily > 0)
        .map(|d| day_of(d) - (i64::from(retention.keep_daily) - 1));
    let first_month = today
        .filter(|_| retention.keep_monthly > 0)
        .map(|d| month_of(d) - (i64::from(retention.keep_monthly) - 1));
    // Whether the version current from `start` (exclusive, `None`: open) to `end` (inclusive)
    // was current at the end of an in-window day or month that has already ended. The latest
    // such end is the midnight that starts the day (month) of `min(end, now)`; when it is
    // before the window or not after `start`, every earlier end is too.
    let holds_an_end = |start: Option<u64>, end: u64| -> bool {
        // A timestamp `time` can't represent is never removed by the window rules.
        let (Some(end), Some(today)) = (local(end), today) else {
            return true;
        };
        let last = end.min(today);
        let start = start.and_then(local);
        let day = day_of(last) - 1;
        let daily = first_day.is_some_and(|f| day >= f) && start.is_none_or(|s| day >= day_of(s));
        let month = month_of(last) - 1;
        let monthly =
            first_month.is_some_and(|f| month >= f) && start.is_none_or(|s| month >= month_of(s));
        daily || monthly
    };

    let mut groups: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (i, e) in entries.iter().enumerate() {
        groups.entry(e.id).or_default().push(i);
    }

    let mut remove = Vec::new();
    for (id, mut idx) in groups {
        // Oldest first; equal times: the later line is newer.
        idx.sort_by_key(|&i| (entries[i].deleted_at, i));
        if !live(id) {
            let newest = entries[idx[idx.len() - 1]].deleted_at;
            if now_ms.saturating_sub(newest) > u64::from(retention.keep_deleted_days) * DAY_MS {
                remove.extend(idx);
                continue;
            }
        }
        let latest = idx.len().saturating_sub(retention.keep_latest as usize);
        for (k, &i) in idx[..latest].iter().enumerate() {
            let start = k.checked_sub(1).map(|p| entries[idx[p]].deleted_at);
            if !holds_an_end(start, entries[i].deleted_at) {
                remove.push(i);
            }
        }
    }
    remove.sort_unstable();
    remove
}

/// Which lines Empty removes: every `deleted` line, also one the bin does not list (a hidden
/// path, a folder a delete left with only dot-files), and never an `overwritten` line. The old
/// versions of replaced files keep following [`plan`]. Returns the indices into `entries`,
/// ascending.
pub fn plan_empty(entries: &[RecycleEntry]) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.reason == RecycleReason::Deleted)
        .map(|(i, _)| i)
        .collect()
}

#[cfg(test)]
mod tests {
    use time::{Month, PrimitiveDateTime, Time};

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
            pruned: false,
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

    /// Why each line stays or goes. Now is Wed 2026-10-07 20:00 local (+02:00), settings 3 / 7
    /// / 12 / 90. In-window ends already past: the end of Oct 1 to Oct 6 and the end of Nov 2025
    /// to Sep 2026. A line holds the version current from the line before it (same id) to its
    /// own time.
    #[test]
    fn table() {
        let now = at(2026, 10, 7, 20, 0);
        let ow = RecycleReason::Overwritten;
        let del = RecycleReason::Deleted;
        #[rustfmt::skip]
        let lines = [
            // id 5, live
            (5, at(2025, 9, 15, 10, 0), ow, false),  // 0 current until Sep 2025: no in-window end
            (5, at(2025, 11, 2, 9, 0), ow, false),   // 1 holds only the end of Oct 2025 (outside)
            (5, at(2025, 11, 20, 9, 0), ow, false),  // 2 no end inside
            (5, at(2026, 1, 10, 9, 0), ow, true),    // 3 current at the end of Nov and Dec 2025
            (5, at(2026, 8, 31, 23, 30), ow, true),  // 4 current at the end of Jan to Jul
            (5, at(2026, 9, 1, 0, 10), ow, true),    // 5 end of Aug, local midnight (UTC: line 6)
            (5, at(2026, 9, 30, 23, 59), ow, false), // 6 replaced a minute before Sep ended
            (5, at(2026, 10, 1, 0, 0), ow, true),    // 7 replaced at midnight: the end of Sep
            (5, at(2026, 10, 3, 8, 0), ow, true),    // 8 end of Oct 1 and Oct 2
            (5, at(2026, 10, 5, 8, 0), ow, true),    // 9 end of Oct 3 and of Oct 4 (no upload)
            (5, at(2026, 10, 5, 18, 0), ow, false),  // 10 no end inside
            (5, at(2026, 10, 6, 9, 0), ow, true),    // 11 end of Oct 5
            (5, at(2026, 10, 6, 18, 0), ow, false),  // 12 no end inside
            (5, at(2026, 10, 7, 9, 0), ow, true),    // 13 end of Oct 6
            (5, at(2026, 10, 7, 10, 0), ow, true),   // 14 one of the 3 newest
            (5, at(2026, 10, 7, 11, 0), ow, true),   // 15 one of the 3 newest
            (5, at(2026, 10, 7, 12, 0), ow, true),   // 16 one of the 3 newest
            // id 6, live, unchanged for 14 months, then 4 uploads today
            (6, at(2025, 8, 1, 10, 0), ow, false),   // 17 current until Aug 2025: no in-window end
            (6, at(2026, 10, 7, 9, 0), ow, true),    // 18 current at every in-window end
            (6, at(2026, 10, 7, 10, 0), ow, true),   // 19 one of the 3 newest
            (6, at(2026, 10, 7, 11, 0), ow, true),   // 20 one of the 3 newest
            (6, at(2026, 10, 7, 12, 0), ow, true),   // 21 one of the 3 newest
            // id 9, deleted 98 days ago: all of it goes
            (9, at(2026, 6, 1, 10, 0), ow, false),   // 22
            (9, at(2026, 7, 1, 10, 0), del, false),  // 23
            // id 10, deleted 36 days ago: the rules of a live file still apply
            (10, at(2026, 8, 1, 10, 0), ow, true),   // 24 current at the end of Nov 2025 to Jul
            (10, at(2026, 8, 15, 10, 0), ow, true),  // 25 one of the 3 newest
            (10, at(2026, 9, 1, 10, 0), del, true),  // 26 the end of Aug
            // deleted exactly 90 days ago (stays) and 1 ms longer (goes)
            (11, now - 90 * DAY_MS, del, true),      // 27
            (12, now - 90 * DAY_MS - 1, del, false), // 28
        ];
        let entries: Vec<_> = lines
            .iter()
            .map(|&(id, t, why, _)| entry(id, t, why))
            .collect();
        let want: Vec<usize> = (0..lines.len()).filter(|&i| !lines[i].3).collect();
        let live = |id: u64| id == 5 || id == 6;
        let got = plan(&entries, &live, &r(3, 7, 12, 90), now, offset());
        assert_eq!(got, want);
    }

    /// An upload creates a line with the version before it. Mon to Wed with Auto Sync: the
    /// version at the end of Mon (line 2) and of Tue (line 5) stays.
    #[test]
    fn the_version_at_the_end_of_each_day_is_kept() {
        let now = at(2026, 10, 7, 20, 0);
        let times = [
            at(2026, 10, 5, 12, 0), // 0 v0, current at the end of Sun and before: keep
            at(2026, 10, 5, 18, 0), // 1 v1, Mon 12 to 18: remove
            at(2026, 10, 6, 8, 0),  // 2 v2, the end of Mon: keep
            at(2026, 10, 6, 12, 0), // 3 remove
            at(2026, 10, 6, 18, 0), // 4 remove
            at(2026, 10, 7, 8, 0),  // 5 v5, the end of Tue: keep
            at(2026, 10, 7, 10, 0), // 6 remove
            at(2026, 10, 7, 12, 0), // 7 one of the 3 newest
            at(2026, 10, 7, 14, 0), // 8 one of the 3 newest
            at(2026, 10, 7, 16, 0), // 9 one of the 3 newest
        ];
        let entries: Vec<_> = times.iter().map(|&t| over(5, t)).collect();
        let got = plan(&entries, &live5, &r(3, 7, 12, 90), now, offset());
        assert_eq!(got, vec![1, 3, 4, 6]);
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
    fn the_oldest_line_holds_the_version_before_it() {
        let now = at(2026, 10, 7, 12, 0);
        // Line 0 holds the version current before 08:00 today, so at the end of yesterday.
        let entries: Vec<_> = [8, 9, 10, 11]
            .iter()
            .map(|&h| over(5, at(2026, 10, 7, h, 0)))
            .collect();
        let got = plan(&entries, &live5, &r(3, 7, 12, 90), now, offset());
        assert!(got.is_empty());
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
    fn plan_empty_takes_every_deleted_line() {
        let del = RecycleReason::Deleted;
        let mut folder = entry(7, 1_000, del);
        folder.path = "NOTE/Note/W".into();
        folder.md5 = String::new();
        let mut parent = folder.clone();
        parent.path = "NOTE/Note/P".into();
        parent.pruned = true;
        let mut dotted = entry(8, 2_000, del);
        dotted.path = "NOTE/Note/.hidden".into();
        let entries = vec![
            entry(5, 500, del),
            folder,
            parent,
            dotted,
            entry(9, u64::MAX, del),
        ];
        assert_eq!(plan_empty(&entries), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn plan_empty_never_takes_an_overwritten_line() {
        let del = RecycleReason::Deleted;
        // Old versions of a live file, of a deleted file and of a restored one stay.
        let entries = vec![
            over(5, 100),
            over(9, 200),
            entry(9, 300, del),
            over(5, 400),
            over(11, 500),
        ];
        assert_eq!(plan_empty(&entries), vec![2]);
        assert!(plan_empty(&[over(5, 1)]).is_empty());
        assert!(plan_empty(&[]).is_empty());
    }

    #[test]
    fn keep_latest_alone() {
        let now = at(2026, 10, 7, 12, 0);
        let entries: Vec<_> = (1..=4).map(|d| over(5, at(2025, 1, d, 10, 0))).collect();
        let got = plan(&entries, &live5, &r(2, 0, 0, 90), now, offset());
        assert_eq!(got, vec![0, 1]);
    }
}
