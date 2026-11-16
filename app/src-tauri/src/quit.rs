//! The Quit flow: stop every child, then end the app. Every way out goes through it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

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
    /// never quits a menu bar app.
    pub fn on_exit_request(&self, code: Option<i32>) -> ExitDecision {
        match (self.state.load(Ordering::SeqCst), code) {
            (DONE, _) => ExitDecision::Allow,
            (_, None) | (STOPPING, _) => ExitDecision::Prevent,
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
        if self
            .state
            .compare_exchange(IDLE, STOPPING, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return false;
        }
        let flow = self.clone();
        std::thread::spawn(move || {
            stop();
            flow.state.store(DONE, Ordering::SeqCst);
            exit();
        });
        true
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
    fn closing_the_last_window_never_quits() {
        let flow = flow();
        assert_eq!(flow.on_exit_request(None), ExitDecision::Prevent);
        assert_eq!(flow.on_exit_request(Some(0)), ExitDecision::PreventAndQuit);
        assert_eq!(flow.on_exit_request(Some(1)), ExitDecision::PreventAndQuit);
    }
}
