//! How a notice reaches the user: the [`Deliver`] trait, the macOS backend behind it
//! (`notify::macos`), and [`FakeDeliver`] for tests. The real notification center is never
//! called in a test.

use std::sync::Mutex;

use serde::Serialize;

use super::Notice;

/// Whether macOS lets Notedown show notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Authorization {
    /// Not asked yet, or no answer in time.
    Unknown,
    Allowed,
    /// Turned off in System Settings.
    Denied,
    /// Not running from an `.app` bundle (`tauri dev`, tests): nothing is shown.
    Unavailable,
}

/// Shows notices and asks for the permission.
///
/// [`Deliver::authorization`] and [`Deliver::request`] wait for an answer of the system, so
/// they never run on the main thread.
pub trait Deliver: Send + Sync {
    /// Shows a notice. A new notice of the same kind replaces the shown one.
    fn show(&self, n: &Notice);
    /// The permission as it is now, without asking.
    fn authorization(&self) -> Authorization;
    /// Asks for the permission (a system dialog the first time) and gives the answer.
    fn request(&self) -> Authorization;
}

/// A [`Deliver`] that only records: the notices shown and the requests made.
#[derive(Debug)]
pub struct FakeDeliver {
    shown: Mutex<Vec<Notice>>,
    authorization: Mutex<Authorization>,
    /// What [`Deliver::request`] answers (and what the authorization becomes).
    answer: Authorization,
    requests: Mutex<u32>,
}

impl FakeDeliver {
    /// A fake whose authorization is `now` and whose request answers `answer`.
    pub fn new(now: Authorization, answer: Authorization) -> Self {
        Self {
            shown: Mutex::new(Vec::new()),
            authorization: Mutex::new(now),
            answer,
            requests: Mutex::new(0),
        }
    }

    /// The notices shown so far, oldest first.
    pub fn shown(&self) -> Vec<Notice> {
        self.shown.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// How often [`Deliver::request`] was called.
    pub fn requests(&self) -> u32 {
        *self.requests.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for FakeDeliver {
    fn default() -> Self {
        Self::new(Authorization::Allowed, Authorization::Allowed)
    }
}

impl Deliver for FakeDeliver {
    fn show(&self, n: &Notice) {
        self.shown
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(n.clone());
    }

    fn authorization(&self) -> Authorization {
        *self.authorization.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn request(&self) -> Authorization {
        *self.requests.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        *self.authorization.lock().unwrap_or_else(|e| e.into_inner()) = self.answer;
        self.answer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NoticeKind, Target};
    use crate::windows::LibraryAt;

    fn notice() -> Notice {
        Notice {
            kind: NoticeKind::Sync,
            title: "Supernote".to_owned(),
            body: "1 file changed.".to_owned(),
            target: Target::Library(LibraryAt::All),
        }
    }

    #[test]
    fn the_fake_records_what_it_shows() {
        let fake = FakeDeliver::default();
        fake.show(&notice());
        fake.show(&notice());
        assert_eq!(fake.shown(), vec![notice(), notice()]);
    }

    #[test]
    fn the_fake_request_answers_and_changes_the_authorization() {
        let fake = FakeDeliver::new(Authorization::Unknown, Authorization::Denied);
        assert_eq!(fake.authorization(), Authorization::Unknown);
        assert_eq!(fake.request(), Authorization::Denied);
        assert_eq!(fake.authorization(), Authorization::Denied);
        assert_eq!(fake.requests(), 1);
    }

    #[test]
    fn the_authorization_reaches_the_page_in_camel_case() {
        let json = serde_json::to_value(Authorization::Unavailable).unwrap();
        assert_eq!(json, serde_json::json!("unavailable"));
    }
}
