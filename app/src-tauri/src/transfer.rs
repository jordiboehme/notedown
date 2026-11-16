//! A file transfer and its watch: the body of an upload streamed from the file, a counter of
//! the bytes it handed on, and the loop that shows the progress about four times a second, ends
//! a transfer whose counter has not moved for a minute, and drops the request on a cancel.
//!
//! A transfer runs on a small runtime of the calling thread (a job's own thread). When the watch
//! ends it, the request is dropped with the runtime: the connection closes and the server
//! discards what it staged. The blocking client cannot do this: while it waits for the socket it
//! calls nothing of the app's, so no flag and no watchdog would reach it.
//!
//! The watch reads the injected clock, so tests decide when time passes; it looks every
//! [`TICK`] of real time, which never decides a result.

use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};

use crate::supervisor::Clock;

/// A transfer whose byte counter stands still this long is ended.
pub const STALL: Duration = Duration::from_secs(60);
/// The progress of a transfer is shown at most this often.
pub const PROGRESS_EVERY: Duration = Duration::from_millis(250);
/// How often the watch looks, in real time.
pub const TICK: Duration = Duration::from_millis(50);
/// The size of one piece of the body.
const CHUNK: usize = 64 * 1024;

fn as_ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// What the watch saw at one look.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// Nothing to show.
    Quiet,
    /// Show this many bytes as done.
    Progress(u64),
    /// The counter has not moved for [`STALL`].
    Stalled,
}

/// The watch of one transfer: when its counter last moved and what was last shown.
#[derive(Debug)]
pub struct Watch {
    count: u64,
    moved_at: u64,
    shown: u64,
    shown_at: u64,
}

impl Watch {
    pub fn new(now: u64) -> Self {
        Self {
            count: 0,
            moved_at: now,
            shown: 0,
            shown_at: now,
        }
    }

    /// One look at `now` (milliseconds) with the counter at `count`.
    pub fn look(&mut self, now: u64, count: u64) -> Look {
        if count != self.count {
            self.count = count;
            self.moved_at = now;
        } else if now.saturating_sub(self.moved_at) >= as_ms(STALL) {
            return Look::Stalled;
        }
        if count != self.shown && now.saturating_sub(self.shown_at) >= as_ms(PROGRESS_EVERY) {
            self.shown = count;
            self.shown_at = now;
            return Look::Progress(count);
        }
        Look::Quiet
    }
}

/// How a watched transfer ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Watched<T> {
    /// The transfer answered.
    Done(T),
    /// `cancelled` said so; the request was dropped.
    Cancelled,
    /// The counter stood still for [`STALL`]; the request was dropped.
    Stalled,
}

/// Runs `work` on a runtime of this thread and watches `count`: `progress` gets the count about
/// four times a second while it moves, a counter that stands still for [`STALL`] ends the work,
/// and so does `cancelled`. Blocking: run it on a job's own thread. Fails only when no runtime
/// can be made.
pub fn watched<T>(
    work: impl Future<Output = T>,
    count: &AtomicU64,
    clock: &Clock,
    cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(u64),
) -> Result<Watched<T>, String> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| format!("no runtime for the transfer: {e}"))?;
    let ended = runtime.block_on(async {
        let mut work = std::pin::pin!(work);
        let mut tick = tokio::time::interval(TICK);
        let mut watch = Watch::new(clock());
        loop {
            let answer = std::future::poll_fn(|cx| {
                if let Poll::Ready(answer) = work.as_mut().poll(cx) {
                    return Poll::Ready(Some(answer));
                }
                if tick.poll_tick(cx).is_ready() {
                    return Poll::Ready(None);
                }
                Poll::Pending
            })
            .await;
            if let Some(answer) = answer {
                return Watched::Done(answer);
            }
            if cancelled() {
                return Watched::Cancelled;
            }
            match watch.look(clock(), count.load(Ordering::SeqCst)) {
                Look::Stalled => return Watched::Stalled,
                Look::Progress(n) => progress(n),
                Look::Quiet => {}
            }
        }
    });
    // The work is gone with the block; the runtime ends its connection tasks here.
    drop(runtime);
    Ok(ended)
}

/// The body of an upload: `len` bytes of a file, read in pieces as the connection takes them.
/// Each piece adds to the shared counter. A file that got shorter, or cannot be read, ends the
/// body with an error and sets the shared flag.
pub struct FileBody {
    file: File,
    left: u64,
    count: Arc<AtomicU64>,
    unreadable: Arc<AtomicBool>,
    buf: Vec<u8>,
}

impl FileBody {
    pub fn new(file: File, len: u64, count: Arc<AtomicU64>, unreadable: Arc<AtomicBool>) -> Self {
        Self {
            file,
            left: len,
            count,
            unreadable,
            buf: vec![0; CHUNK],
        }
    }

    /// The bytes still to send (all of them before the first poll).
    pub fn remaining(&self) -> u64 {
        self.left
    }
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, std::io::Error>>> {
        let this = self.get_mut();
        if this.left == 0 {
            return Poll::Ready(None);
        }
        let want = usize::try_from(this.left).unwrap_or(CHUNK).min(CHUNK);
        match this.file.read(&mut this.buf[..want]) {
            Ok(0) => {
                this.unreadable.store(true, Ordering::SeqCst);
                Poll::Ready(Some(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "the file got shorter",
                ))))
            }
            Ok(n) => {
                this.left -= n as u64;
                this.count.fetch_add(n as u64, Ordering::SeqCst);
                Poll::Ready(Some(Ok(Frame::data(Bytes::copy_from_slice(
                    &this.buf[..n],
                )))))
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Try again at the next poll.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            Err(e) => {
                this.unreadable.store(true, Ordering::SeqCst);
                Poll::Ready(Some(Err(e)))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.left == 0
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.left)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_shows_about_four_times_a_second() {
        let mut watch = Watch::new(1_000);
        assert_eq!(watch.look(1_000, 0), Look::Quiet);
        // Moves, but too soon to show.
        assert_eq!(watch.look(1_100, 10), Look::Quiet);
        assert_eq!(watch.look(1_250, 20), Look::Progress(20));
        assert_eq!(watch.look(1_400, 30), Look::Quiet);
        assert_eq!(watch.look(1_500, 40), Look::Progress(40));
        // Nothing new to show.
        assert_eq!(watch.look(2_000, 40), Look::Quiet);
        assert_eq!(watch.look(2_100, 50), Look::Progress(50));
    }

    #[test]
    fn a_counter_that_stands_still_for_a_minute_stalls() {
        let mut watch = Watch::new(0);
        assert_eq!(watch.look(59_999, 0), Look::Quiet);
        assert_eq!(watch.look(60_000, 0), Look::Stalled);
        let mut watch = Watch::new(0);
        assert_eq!(watch.look(30_000, 5), Look::Progress(5));
        // A minute from the last move, not from the start.
        assert_eq!(watch.look(89_999, 5), Look::Quiet);
        assert_eq!(watch.look(90_000, 5), Look::Stalled);
    }
}
