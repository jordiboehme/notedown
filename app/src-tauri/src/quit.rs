//! The Quit flow: stop every child, then end the app. Every way out goes through it, the
//! restart after an update too.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::config::Mode;
use crate::supervisor::ChildState;

/// The exit code of [`tauri::AppHandle::restart`]. Tauri ignores `prevent_exit` for it.
pub use tauri::RESTART_EXIT_CODE;

/// The event that asks the panel to put the Quit question.
pub const CONFIRM_QUIT_EVENT: &str = "confirm-quit";

/// Whether Quit asks first: only in Local mode while the app's own server runs, because the
/// Supernote cannot sync once it stops. The panel's `needsQuitWarning` keeps the same rule.
pub fn quit_needs_question(mode: Mode, server: &ChildState) -> bool {
    mode == Mode::Local && matches!(server, ChildState::Running { .. })
}

/// What to do with an exit request of the event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitDecision {
    /// Let the app end: the children are stopped.
    Allow,
    /// Keep the app running (the last window closed, or the Quit flow runs already).
    Prevent,
    /// Keep the app running for now and start the Quit flow.
    PreventAndQuit,
}

const IDLE: u8 = 0;
const STOPPING: u8 = 1;
const DONE: u8 = 2;

/// Where the Quit flow is. One per app.
#[derive(Debug, Default)]
pub struct QuitFlow {
    state: AtomicU8,
}

impl QuitFlow {
    pub fn new() -> Self {
        Self::default()
    }

    /// The decision for an exit request. `code` is `None` when the last window closed, which
    /// never quits a menu bar app. A restart ([`RESTART_EXIT_CODE`]) never starts the Quit flow:
    /// Tauri ignores `prevent_exit` for it, so the flow's own exit would race the restart. The
    /// restart of [`restart_with`](Self::restart_with) asks only once the children are stopped;
    /// any other restart ends in `RunEvent::Exit`, which stops them.
    pub fn on_exit_request(&self, code: Option<i32>) -> ExitDecision {
        match (self.state.load(Ordering::SeqCst), code) {
            (DONE, _) => ExitDecision::Allow,
            (_, None) | (STOPPING, _) => ExitDecision::Prevent,
            (_, Some(RESTART_EXIT_CODE)) => ExitDecision::Allow,
            _ => ExitDecision::PreventAndQuit,
        }
    }

    /// Starts the Quit flow once: `stop` then `exit`, on a thread of its own. A second call
    /// while it runs does nothing. Returns whether this call started it.
    pub fn quit_with(
        self: &Arc<Self>,
        stop: impl FnOnce() + Send + 'static,
        exit: impl FnOnce() + Send + 'static,
    ) -> bool {
        self.end_with(stop, exit)
    }

    /// The restart after an update: `stop` then `restart`, on a thread of its own, once. It
    /// shares the state with [`quit_with`](Self::quit_with): neither starts while the other
    /// runs, and a Quit during the stop is held back. `restart` runs after the state is Done,
    /// so its exit request is let through.
    pub fn restart_with(
        self: &Arc<Self>,
        stop: impl FnOnce() + Send + 'static,
        restart: impl FnOnce() + Send + 'static,
    ) -> bool {
        self.end_with(stop, restart)
    }

    fn end_with(
        self: &Arc<Self>,
        stop: impl FnOnce() + Send + 'static,
        exit: impl FnOnce() + Send + 'static,
    ) -> bool {
        if self
            .state
            .compare_exchange(IDLE, STOPPING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        let finish = Finish {
            flow: self.clone(),
            exit: Some(exit),
        };
        std::thread::spawn(move || {
            // Runs on drop, so a stop that panics still ends the app.
            let _finish = finish;
            stop();
        });
        true
    }
}

/// The end of the Quit flow: Done, then exit. A drop guard, so it also runs on a panic.
struct Finish<F: FnOnce()> {
    flow: Arc<QuitFlow>,
    exit: Option<F>,
}

impl<F: FnOnce()> Drop for Finish<F> {
    fn drop(&mut self) {
        self.flow.state.store(DONE, Ordering::SeqCst);
        if let Some(exit) = self.exit.take() {
            exit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    fn flow() -> Arc<QuitFlow> {
        Arc::new(QuitFlow::new())
    }

    #[test]
    fn quit_stops_then_exits() {
        let flow = flow();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let caller = std::thread::current().id();
        let (done_tx, done_rx) = mpsc::channel();
        let stop_log = log.clone();
        let exit_log = log.clone();
        let started = flow.quit_with(
            move || {
                assert_ne!(
                    std::thread::current().id(),
                    caller,
                    "stop runs off the caller"
                );
                // While the children stop, an exit request is held back.
                stop_log.lock().unwrap().push("stop".into());
            },
            move || {
                exit_log.lock().unwrap().push("exit".into());
                done_tx.send(()).unwrap();
            },
        );
        assert!(started);
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["stop", "exit"]);
        // The exit that `exit` asks for now goes through.
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::Allow);
    }

    #[test]
    fn an_exit_request_waits_for_the_stop() {
        let flow = flow();
        let (in_stop_tx, in_stop_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::PreventAndQuit);
        assert!(flow.quit_with(
            move || {
                in_stop_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
            move || done_tx.send(()).unwrap(),
        ));
        in_stop_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Cmd+Q again, or the Dock: still stopping, so held back without a second flow.
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::Prevent);
        assert!(!flow.quit_with(|| panic!("a second stop"), || panic!("a second exit")));
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::Allow);
    }

    #[test]
    fn a_stop_that_panics_still_exits() {
        let flow = flow();
        let (done_tx, done_rx) = mpsc::channel();
        assert!(flow.quit_with(
            || panic!("the stop failed (expected in this test)"),
            move || done_tx.send(()).unwrap(),
        ));
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::Allow);
    }

    #[test]
    fn quit_asks_only_in_local_mode_while_the_server_runs() {
        let running = ChildState::Running {
            pid: 7,
            since_ms: 1,
        };
        assert!(quit_needs_question(Mode::Local, &running));
        assert!(!quit_needs_question(Mode::Remote, &running));
        assert!(!quit_needs_question(Mode::NotSet, &running));
        let others = [
            ChildState::Stopped,
            ChildState::NotConnected,
            ChildState::Starting,
            ChildState::Stopping,
            ChildState::Backoff {
                until_ms: 5,
                last_error: "boom".into(),
            },
            // Another Notedown keeps serving after the app quits.
            ChildState::Outside {
                what: "server".into(),
            },
        ];
        for state in &others {
            assert!(!quit_needs_question(Mode::Local, state), "{state:?}");
        }
    }

    #[test]
    fn restart_stops_then_restarts() {
        let flow = flow();
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let (done_tx, done_rx) = mpsc::channel();
        let stop_log = log.clone();
        let restart_log = log.clone();
        let started = flow.restart_with(
            move || stop_log.lock().unwrap().push("stop".into()),
            move || {
                restart_log.lock().unwrap().push("restart".into());
                done_tx.send(()).unwrap();
            },
        );
        assert!(started);
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["stop", "restart"]);
        // The restart's own exit request goes through once the children are stopped.
        assert_eq!(
            flow.on_exit_request(Some(RESTART_EXIT_CODE)),
            ExitDecision::Allow
        );
    }

    #[test]
    fn a_restart_never_starts_the_quit_flow() {
        // Tauri ignores prevent_exit for a restart, so a Quit flow would race the restart. An
        // outside restart is let through: RunEvent::Exit stops the children.
        let flow = flow();
        assert_eq!(
            flow.on_exit_request(Some(RESTART_EXIT_CODE)),
            ExitDecision::Allow
        );
        assert_eq!(RESTART_EXIT_CODE, tauri::RESTART_EXIT_CODE);
    }

    #[test]
    fn a_quit_waits_for_the_restarts_stop_and_neither_runs_twice() {
        let flow = flow();
        let (in_stop_tx, in_stop_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        assert!(flow.restart_with(
            move || {
                in_stop_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
            move || done_tx.send(()).unwrap(),
        ));
        in_stop_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Cmd+Q while the restart stops the children: held back, no Quit flow.
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::Prevent);
        assert!(!flow.quit_with(|| panic!("a second stop"), || panic!("an exit")));
        assert!(!flow.restart_with(|| panic!("a second stop"), || panic!("a second restart")));
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            flow.on_exit_request(Some(RESTART_EXIT_CODE)),
            ExitDecision::Allow
        );
    }

    #[test]
    fn a_restart_while_quitting_does_nothing() {
        let flow = flow();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel();
        assert!(flow.quit_with(
            move || release_rx.recv().unwrap(),
            move || done_tx.send(()).unwrap(),
        ));
        assert!(!flow.restart_with(|| panic!("a stop"), || panic!("a restart")));
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn a_restart_whose_stop_panics_still_restarts() {
        let flow = flow();
        let (done_tx, done_rx) = mpsc::channel();
        assert!(flow.restart_with(
            || panic!("the stop failed (expected in this test)"),
            move || done_tx.send(()).unwrap(),
        ));
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }

    #[test]
    fn closing_the_last_window_never_quits() {
        let flow = flow();
        assert_eq!(flow.on_exit_request(None), ExitDecision::Prevent);
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::PreventAndQuit);
        assert_eq!(flow.on_exit_request(Some(1)), ExitDecision::PreventAndQuit);
    }
}
