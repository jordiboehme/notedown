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
const PRUNE_EVERY_MS: u64 = 60 * 1000;
const MAX_KEYS: usize = 10_000;
const MAX_EMAIL_CHARS: usize = 254;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verdict {
    Go,
    Wait(Duration),
    Refuse { retry_after_s: u64 },
}

#[derive(Default)]
struct State {
    /// email key -> (attempts counted, time of the last counted attempt in ms)
    entries: HashMap<String, (u32, u64)>,
    last_prune_ms: u64,
}

/// Throttle of the web login per email.
///
/// `attempt` is called once for every password guess, before the password is checked. It returns
/// the verdict from the count before this guess and counts the guess at once under the same lock,
/// so parallel guesses cannot all see `Go`. A `Refuse` does not count, so refused guesses do not
/// extend the 15-minute window. `succeeded` resets the count after a right password.
pub(crate) struct Throttle {
    state: Mutex<State>,
    max_keys: usize,
}

impl Default for Throttle {
    fn default() -> Self {
        Throttle {
            state: Mutex::default(),
            max_keys: MAX_KEYS,
        }
    }
}

fn key_of(email: &str) -> String {
    email
        .trim()
        .to_ascii_lowercase()
        .chars()
        .take(MAX_EMAIL_CHARS)
        .collect()
}

impl Throttle {
    #[cfg(test)]
    fn with_cap(max_keys: usize) -> Self {
        Throttle {
            state: Mutex::default(),
            max_keys,
        }
    }

    pub(crate) fn attempt(&self, email: &str, now_ms: u64) -> Verdict {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if now_ms.saturating_sub(st.last_prune_ms) >= PRUNE_EVERY_MS {
            st.entries
                .retain(|_, (_, last)| now_ms.saturating_sub(*last) < FORGET_MS);
            st.last_prune_ms = now_ms;
        }
        let key = key_of(email);
        let (fails, last) = match st.entries.get(&key) {
            Some(&(f, l)) if now_ms.saturating_sub(l) < FORGET_MS => (f, l),
            _ => (0, now_ms),
        };
        if fails >= FREE + MAX_DELAY_STEPS {
            let left_ms = FORGET_MS - now_ms.saturating_sub(last);
            return Verdict::Refuse {
                retry_after_s: left_ms.div_ceil(1000),
            };
        }
        if !st.entries.contains_key(&key) && st.entries.len() >= self.max_keys {
            let oldest = st
                .entries
                .iter()
                .min_by_key(|(_, (_, l))| *l)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                st.entries.remove(&k);
            }
        }
        st.entries.insert(key, (fails + 1, now_ms));
        if fails < FREE {
            Verdict::Go
        } else {
            Verdict::Wait(Duration::from_secs(1 << (fails - FREE)))
        }
    }

    pub(crate) fn succeeded(&self, email: &str) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        st.entries.remove(&key_of(email));
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
            assert_eq!(t.attempt(e, now), Verdict::Go);
        }
        for secs in [1, 2, 4, 8] {
            assert_eq!(
                t.attempt("anna@example.com", now),
                Verdict::Wait(Duration::from_secs(secs))
            );
            now += 1000;
        }
        match t.attempt(e, now) {
            Verdict::Refuse { retry_after_s } => {
                assert!(retry_after_s > 14 * 60 && retry_after_s <= 15 * 60)
            }
            v => panic!("{v:?}"),
        }
        now += 15 * 60 * 1000;
        assert_eq!(t.attempt(e, now), Verdict::Go);
    }

    #[test]
    fn back_to_back_guesses_are_counted_at_once() {
        let t = Throttle::default();
        let v: Vec<Verdict> = (0..10).map(|_| t.attempt("a@x", 5)).collect();
        assert_eq!(v[2], Verdict::Go);
        assert_eq!(v[3], Verdict::Wait(Duration::from_secs(1)));
        assert!(matches!(v[7], Verdict::Refuse { .. }));
    }

    #[test]
    fn refused_attempts_do_not_extend_the_window() {
        let t = Throttle::default();
        for _ in 0..7 {
            t.attempt("a@x", 0);
        }
        let almost = 15 * 60 * 1000 - 1;
        assert!(matches!(t.attempt("a@x", almost), Verdict::Refuse { .. }));
        assert_eq!(t.attempt("a@x", 15 * 60 * 1000), Verdict::Go);
    }

    #[test]
    fn success_clears_the_count() {
        let t = Throttle::default();
        for _ in 0..5 {
            t.attempt("a@x", 0);
        }
        t.succeeded("a@x");
        assert_eq!(t.attempt("a@x", 0), Verdict::Go);
    }

    #[test]
    fn the_key_is_cut_and_the_map_is_capped_by_evicting_the_oldest() {
        let t = Throttle::with_cap(2);
        t.attempt("old@x", 1);
        t.attempt("mid@x", 2);
        t.attempt("new@x", 3);
        let st = t.state.lock().unwrap();
        assert_eq!(st.entries.len(), 2);
        assert!(!st.entries.contains_key("old@x"));
        drop(st);
        let long = "é".repeat(400);
        t.attempt(&long, 4);
        let st = t.state.lock().unwrap();
        assert!(st.entries.keys().all(|k| k.chars().count() <= 254));
    }

    #[test]
    fn stale_entries_are_pruned() {
        let t = Throttle::default();
        t.attempt("a@x", 0);
        t.attempt("b@x", 16 * 60 * 1000);
        assert_eq!(t.state.lock().unwrap().entries.len(), 1);
    }
}
