//! The macOS backend of [`Deliver`]: UNUserNotificationCenter (objc2-user-notifications).
//!
//! Outside an `.app` bundle (`tauri dev`, tests) the center is never touched: it raises an
//! Objective-C exception there. [`MacDeliver`] is then [`Authorization::Unavailable`] and only
//! logs. A notice carries its click target as JSON in `userInfo`; the delegate parses it and
//! sends it into a channel, also for a notice shown before this start of the app.
//!
//! The delegate methods and the completion handlers run on a queue of the system, not on the
//! main thread: they only parse and send, never call a Tauri window API, and call the
//! completion handler they get on every path.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSBundle, NSDictionary, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use serde::{Deserialize, Serialize};

use super::deliver::{Authorization, Deliver};
use super::{Notice, NoticeKind, Target};
use crate::windows::LibraryAt;

/// The `userInfo` key of the click target (JSON, see [`target_json`]).
const TARGET_KEY: &str = "target";
/// How long [`Deliver::authorization`] waits for the system.
const SETTINGS_WAIT: Duration = Duration::from_secs(2);

/// Whether the app runs from an `.app` bundle with a bundle id. Tauri embeds the Info.plist
/// into the binary, so the id alone does not prove a bundle; the path does.
pub fn bundled(bundle_path: &Path, bundle_id: Option<&str>) -> bool {
    bundle_path.extension().is_some_and(|ext| ext == "app")
        && bundle_id.is_some_and(|id| !id.is_empty())
}

/// The click target as it travels in `userInfo`. Its own shape, so a change of [`Target`] or
/// [`LibraryAt`] does not silently change what an older notice carries.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
enum Wire {
    Library(WireAt),
    Settings(String),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "at", rename_all = "camelCase", deny_unknown_fields)]
enum WireAt {
    Note { path: String },
    New,
    All,
    Bin,
}

/// The click target as JSON, for `userInfo`.
pub fn target_json(target: &Target) -> String {
    let wire = match target {
        Target::Library(at) => Wire::Library(match at {
            LibraryAt::Note { path } => WireAt::Note { path: path.clone() },
            LibraryAt::New => WireAt::New,
            LibraryAt::All => WireAt::All,
            LibraryAt::Bin => WireAt::Bin,
        }),
        Target::Settings(page) => Wire::Settings(page.clone()),
    };
    // Plain strings and enums only: serializing cannot fail.
    serde_json::to_string(&wire).unwrap_or_default()
}

/// The click target from the JSON of `userInfo`; `None` for anything else.
pub fn target_from_json(json: &str) -> Option<Target> {
    Some(match serde_json::from_str::<Wire>(json).ok()? {
        Wire::Library(at) => Target::Library(match at {
            WireAt::Note { path } => LibraryAt::Note { path },
            WireAt::New => LibraryAt::New,
            WireAt::All => LibraryAt::All,
            WireAt::Bin => LibraryAt::Bin,
        }),
        Wire::Settings(page) => Target::Settings(page),
    })
}

/// The `threadIdentifier`: notices of one kind group together, the three problems as one.
pub fn thread_id(kind: NoticeKind) -> &'static str {
    match kind {
        NoticeKind::Sync => "notedown.sync",
        NoticeKind::Ocr => "notedown.ocr",
        NoticeKind::Agent => "notedown.agent",
        NoticeKind::Problem(_) => "notedown.problem",
    }
}

/// What a notice turns into besides its title and body: the request id (a new notice
/// replaces the shown one with this id), the thread and the `userInfo` JSON.
pub fn parts(n: &Notice) -> (String, &'static str, String) {
    (n.kind.id(), thread_id(n.kind), target_json(&n.target))
}

/// `Provisional` and `Ephemeral` show notices too; `NotDetermined` is not asked yet.
pub fn authorization_of(status: UNAuthorizationStatus) -> Authorization {
    match status {
        UNAuthorizationStatus::Authorized
        | UNAuthorizationStatus::Provisional
        | UNAuthorizationStatus::Ephemeral => Authorization::Allowed,
        UNAuthorizationStatus::Denied => Authorization::Denied,
        _ => Authorization::Unknown,
    }
}

/// The delegate's state: where a click goes.
struct Clicks {
    clicks: Mutex<Sender<Target>>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and Delegate has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "NotedownNotificationDelegate"]
    #[ivars = Clicks]
    struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// Show a notice also while Notedown is the frontmost app.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion_handler: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion_handler
                .call((UNNotificationPresentationOptions::Banner
                    | UNNotificationPresentationOptions::List,));
        }

        /// A click: parse the target and send it on; the window opens elsewhere.
        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion_handler: &DynBlock<dyn Fn()>,
        ) {
            let target = target_of(response);
            completion_handler.call(());
            if let Some(target) = target
                && let Ok(clicks) = self.ivars().clicks.lock()
            {
                // A closed receiver: the app is quitting.
                let _ = clicks.send(target);
            }
        }
    }
);

impl Delegate {
    fn new(clicks: Sender<Target>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(Clicks {
            clicks: Mutex::new(clicks),
        });
        // SAFETY: NSObject's init on a new object.
        unsafe { msg_send![super(this), init] }
    }
}

/// The target in the `userInfo` of a clicked notice; `None` for any other shape (a notice
/// of an older build, say).
fn target_of(response: &UNNotificationResponse) -> Option<Target> {
    let info = response.notification().request().content().userInfo();
    let value = info.objectForKey(&NSString::from_str(TARGET_KEY))?;
    let json = value.downcast_ref::<NSString>()?.to_string();
    target_from_json(&json)
}

fn center() -> Retained<UNUserNotificationCenter> {
    UNUserNotificationCenter::currentNotificationCenter()
}

/// Notices through UNUserNotificationCenter.
///
/// Made by [`MacDeliver::start`] in the app's setup on every start, whatever the choices say,
/// so a click on a notice of an earlier run has a delegate. Keep it for the app's life: the
/// center holds its delegate weakly.
pub struct MacDeliver {
    /// `None` outside an `.app` bundle: nothing is shown.
    delegate: Option<Retained<Delegate>>,
}

impl MacDeliver {
    /// Sets the delegate that sends each clicked notice's target into `clicks`.
    pub fn start(clicks: Sender<Target>) -> Self {
        let bundle = NSBundle::mainBundle();
        let path = PathBuf::from(bundle.bundlePath().to_string());
        let id = bundle.bundleIdentifier().map(|id| id.to_string());
        if !bundled(&path, id.as_deref()) {
            eprintln!("notedown: not running from an .app bundle, no notifications");
            return Self { delegate: None };
        }
        let delegate = Delegate::new(clicks);
        center().setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
        Self {
            delegate: Some(delegate),
        }
    }
}

impl Deliver for MacDeliver {
    fn show(&self, n: &Notice) {
        if self.delegate.is_none() {
            eprintln!(
                "notedown: a notification ({}) was not shown: no .app bundle",
                n.kind.id()
            );
            return;
        }
        let (id, thread, json) = parts(n);
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&n.title));
        content.setBody(&NSString::from_str(&n.body));
        content.setThreadIdentifier(&NSString::from_str(thread));
        let key = NSString::from_str(TARGET_KEY);
        let value = NSString::from_str(&json);
        let info = NSDictionary::from_slices(&[&*key], &[&*value]);
        // SAFETY: an NSDictionary of strings is an NSDictionary of objects, and strings are
        // property list values, as userInfo wants.
        unsafe {
            let info: Retained<NSDictionary> = Retained::cast_unchecked(info);
            content.setUserInfo(&info);
        }
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&id),
            &content,
            None,
        );
        let done = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: the system hands a valid error or null.
            if let Some(error) = unsafe { error.as_ref() } {
                eprintln!(
                    "notedown: a notification ({id}) was not shown: {}",
                    error.localizedDescription()
                );
            }
        });
        center().addNotificationRequest_withCompletionHandler(&request, Some(&*done));
    }

    fn authorization(&self) -> Authorization {
        if self.delegate.is_none() {
            return Authorization::Unavailable;
        }
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(Some(tx));
        let done = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
            // SAFETY: the system hands valid settings.
            let status = unsafe { settings.as_ref() }.authorizationStatus();
            if let Some(tx) = tx.lock().ok().and_then(|mut tx| tx.take()) {
                let _ = tx.send(authorization_of(status));
            }
        });
        center().getNotificationSettingsWithCompletionHandler(&done);
        rx.recv_timeout(SETTINGS_WAIT)
            .unwrap_or(Authorization::Unknown)
    }

    fn request(&self) -> Authorization {
        if self.delegate.is_none() {
            return Authorization::Unavailable;
        }
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(Some(tx));
        let done = RcBlock::new(move |_granted: Bool, error: *mut NSError| {
            // SAFETY: the system hands a valid error or null.
            if let Some(error) = unsafe { error.as_ref() } {
                eprintln!(
                    "notedown: the notification permission was not asked: {}",
                    error.localizedDescription()
                );
            }
            if let Some(tx) = tx.lock().ok().and_then(|mut tx| tx.take()) {
                let _ = tx.send(());
            }
        });
        center().requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert,
            &done,
        );
        // The system keeps its own copy of the block. Without ours, a block the system drops
        // without calling it closes the channel, and the wait below ends with Unknown.
        drop(done);
        // The user answers a system dialog: no short timeout. Then the state as the settings
        // say it, so Provisional and a denial map as in `authorization`.
        match rx.recv() {
            Ok(()) => self.authorization(),
            Err(_) => Authorization::Unknown,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::windows::LibraryAt;

    #[test]
    fn only_an_app_bundle_with_an_id_is_bundled() {
        let cases = [
            (
                "/Applications/Notedown.app",
                Some("io.github.jordiboehme.notedown"),
                true,
            ),
            ("/Users/me/Applications/Notedown.app", Some("x.y"), true),
            (
                "/Users/me/notedown/app/src-tauri/target/debug",
                Some("x.y"),
                false,
            ),
            ("/Users/me/notedown/app/src-tauri/target/debug", None, false),
            ("/Applications/Notedown.app", None, false),
            ("/Applications/Notedown.app", Some(""), false),
            ("/Applications/Notedown.application", Some("x.y"), false),
        ];
        for (path, id, want) in cases {
            assert_eq!(bundled(&PathBuf::from(path), id), want, "{path} {id:?}");
        }
    }

    #[test]
    fn a_target_goes_through_user_info_and_back() {
        let targets = [
            Target::Library(LibraryAt::All),
            Target::Library(LibraryAt::New),
            Target::Library(LibraryAt::Bin),
            Target::Library(LibraryAt::Note {
                path: "Meetings/Q3 \"plan\" \u{fc}ber.note".to_owned(),
            }),
            Target::Settings("ocr".to_owned()),
            Target::Settings("agents".to_owned()),
        ];
        for target in targets {
            let json = target_json(&target);
            assert_eq!(target_from_json(&json), Some(target), "{json}");
        }
    }

    #[test]
    fn the_user_info_json_is_plain() {
        assert_eq!(
            target_json(&Target::Library(LibraryAt::All)),
            r#"{"library":{"at":"all"}}"#
        );
        assert_eq!(
            target_json(&Target::Settings("server".to_owned())),
            r#"{"settings":"server"}"#
        );
    }

    #[test]
    fn a_broken_or_unknown_user_info_gives_no_target() {
        for json in [
            "",
            "not json",
            "{}",
            r#"{"library":{"at":"trash"}}"#,
            r#"{"library":{"at":"note"}}"#,
            r#"{"window":"main"}"#,
            r#"{"settings":3}"#,
        ] {
            assert_eq!(target_from_json(json), None, "{json}");
        }
    }

    #[test]
    fn each_kind_has_its_request_id_and_thread() {
        let cases = [
            (NoticeKind::Sync, "notedown.sync", "notedown.sync"),
            (NoticeKind::Ocr, "notedown.ocr", "notedown.ocr"),
            (NoticeKind::Agent, "notedown.agent", "notedown.agent"),
            (
                NoticeKind::Problem("server"),
                "notedown.problem.server",
                "notedown.problem",
            ),
            (
                NoticeKind::Problem("ocr"),
                "notedown.problem.ocr",
                "notedown.problem",
            ),
            (
                NoticeKind::Problem("oldsync"),
                "notedown.problem.oldsync",
                "notedown.problem",
            ),
        ];
        for (kind, id, thread) in cases {
            let notice = Notice {
                kind,
                title: kind.title().to_owned(),
                body: "Body.".to_owned(),
                target: Target::Settings("server".to_owned()),
            };
            let (got_id, got_thread, info) = parts(&notice);
            assert_eq!(got_id, id);
            assert_eq!(got_thread, thread);
            assert_eq!(info, r#"{"settings":"server"}"#);
        }
    }

    #[test]
    fn the_status_maps_to_an_authorization() {
        let cases = [
            (UNAuthorizationStatus::NotDetermined, Authorization::Unknown),
            (UNAuthorizationStatus::Denied, Authorization::Denied),
            (UNAuthorizationStatus::Authorized, Authorization::Allowed),
            (UNAuthorizationStatus::Provisional, Authorization::Allowed),
            (UNAuthorizationStatus::Ephemeral, Authorization::Allowed),
            (UNAuthorizationStatus(99), Authorization::Unknown),
        ];
        for (status, want) in cases {
            assert_eq!(authorization_of(status), want, "{status:?}");
        }
    }
}
