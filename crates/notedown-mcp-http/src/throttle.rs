//! The web login throttle per email. It is separate from the device lockout: it never touches
//! `failed_logins` of an account.
// Used from Task 6 on; removed in Task 10.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

const FREE: u32 = 3;
const MAX_DELAY_STEPS: u32 = 4; // 1, 2, 4, 8 s
const FORGET_MS: u64 = 15 * 60 * 1000;
const PRUNE_ABOVE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Go,
    Wait(Duration),
    Refuse { retry_after_s: u64 },
}

#[derive(Default)]
pub(crate) struct Throttle(Mutex<HashMap<String, (u32, u64)>>);

impl Throttle {
    pub(crate) fn check(&self, email: &str, now_ms: u64) -> Verdict {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let key = email.trim().to_ascii_lowercase();
        let Some(&(fails, last)) = map.get(&key) else {
            return Verdict::Go;
        };
        if now_ms.saturating_sub(last) >= FORGET_MS {
            map.remove(&key);
            return Verdict::Go;
        }
        if fails < FREE {
            Verdict::Go
        } else if fails < FREE + MAX_DELAY_STEPS {
            Verdict::Wait(Duration::from_secs(1 << (fails - FREE)))
        } else {
            let left_ms = FORGET_MS - now_ms.saturating_sub(last);
            Verdict::Refuse {
                retry_after_s: left_ms.div_ceil(1000),
            }
        }
    }

    pub(crate) fn failed(&self, email: &str, now_ms: u64) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() > PRUNE_ABOVE {
            map.retain(|_, (_, last)| now_ms.saturating_sub(*last) < FORGET_MS);
        }
        let e = map
            .entry(email.trim().to_ascii_lowercase())
            .or_insert((0, now_ms));
        e.0 += 1;
        e.1 = now_ms;
    }

    pub(crate) fn succeeded(&self, email: &str) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&email.trim().to_ascii_lowercase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn three_free_then_doubling_delay_then_refusal_then_forgotten() {
        let t = Throttle::default();
        let e = "Anna@Example.com";
        let mut now = 1_000_000;
        for _ in 0..3 {
            assert_eq!(t.check(e, now), Verdict::Go);
            t.failed(e, now);
        }
        for secs in [1, 2, 4, 8] {
            assert_eq!(
                t.check("anna@example.com", now),
                Verdict::Wait(Duration::from_secs(secs))
            );
            t.failed(e, now);
            now += 1000;
        }
        match t.check(e, now) {
            Verdict::Refuse { retry_after_s } => {
                assert!(retry_after_s > 14 * 60 && retry_after_s <= 15 * 60)
            }
            v => panic!("{v:?}"),
        }
        now += 15 * 60 * 1000;
        assert_eq!(t.check(e, now), Verdict::Go);
    }

    #[test]
    fn success_clears_the_count() {
        let t = Throttle::default();
        for _ in 0..5 {
            t.failed("a@x", 0);
        }
        t.succeeded("a@x");
        assert_eq!(t.check("a@x", 0), Verdict::Go);
    }
}
