//! Looks for another Notedown before the app starts its own: a server on the port, or a
//! pipeline that holds the writer lock of the data folder.

use std::fs::{File, TryLockError};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// `<data>/pipeline/.writer.lock`, as `notedown_pipeline::store` names it.
const PIPELINE_DIR: &str = "pipeline";
const WRITER_LOCK_FILE: &str = ".writer.lock";

/// What answers on the server's port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerProbe {
    /// Nothing listens there.
    None,
    /// A Notedown server: `GET /rest/v1/account` without a token gives 401 `token_refused`.
    Notedown,
    /// Something else listens there.
    Other,
}

/// How long the probe waits for the connection and for the answer.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);
const ANSWER_TIMEOUT: Duration = Duration::from_millis(1500);
/// The most of an answer the probe reads.
const MAX_ANSWER: usize = 16 * 1024;

/// Asks `addr` whether it is a Notedown server: `GET /rest/v1/account` without a token must
/// give 401 with the error code `token_refused`. The whole probe takes at most the connect
/// timeout plus [`ANSWER_TIMEOUT`], however slowly the other side answers.
pub fn server_at(addr: SocketAddr) -> ServerProbe {
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return ServerProbe::None,
    };
    let deadline = Instant::now() + ANSWER_TIMEOUT;
    if is_notedown(&mut stream, addr, deadline).unwrap_or(false) {
        ServerProbe::Notedown
    } else {
        ServerProbe::Other
    }
}

/// Asks `127.0.0.1:<port>` whether it is a Notedown server.
pub fn server(port: u16) -> ServerProbe {
    server_at(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
}

/// The time left until `deadline`, as a socket timeout; none left is an error.
fn left(deadline: Instant) -> std::io::Result<Option<Duration>> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        Err(std::io::ErrorKind::TimedOut.into())
    } else {
        Ok(Some(left))
    }
}

fn is_notedown(
    stream: &mut TcpStream,
    addr: SocketAddr,
    deadline: Instant,
) -> std::io::Result<bool> {
    stream.set_write_timeout(left(deadline)?)?;
    write!(
        stream,
        "GET /rest/v1/account HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n\
         Connection: close\r\n\r\n"
    )?;
    let mut answer = Vec::new();
    let mut buf = [0u8; 4096];
    // Each read waits at most the time left, so a peer that sends a byte now and then cannot
    // stretch the probe.
    while answer.len() < MAX_ANSWER && !complete(&answer) {
        stream.set_read_timeout(left(deadline)?)?;
        match stream.read(&mut buf)? {
            0 => break,
            n => answer.extend_from_slice(&buf[..n]),
        }
    }
    Ok(token_refused(&answer))
}

/// Whether `answer` holds the head and, by its `Content-Length`, the whole body. Without a
/// length the answer ends with the connection.
fn complete(answer: &[u8]) -> bool {
    let Some(end) = answer.windows(4).position(|w| w == b"\r\n\r\n") else {
        return false;
    };
    let head = String::from_utf8_lossy(&answer[..end]);
    let length = head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().ok())
            .flatten()
    });
    length.is_some_and(|n| answer.len() - (end + 4) >= n)
}

/// Whether an HTTP answer is 401 with the JSON body `{"error":"token_refused", ...}`.
fn token_refused(answer: &[u8]) -> bool {
    let text = String::from_utf8_lossy(answer);
    let Some((head, body)) = text.split_once("\r\n\r\n") else {
        return false;
    };
    let status = head.lines().next().unwrap_or("");
    let mut parts = status.split_whitespace();
    let http = parts.next().is_some_and(|v| v.starts_with("HTTP/"));
    if !http || parts.next() != Some("401") {
        return false;
    }
    // The JSON object of the body; a chunked body has its sizes around it.
    let (Some(from), Some(to)) = (body.find('{'), body.rfind('}')) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&body[from..=to])
        .ok()
        .is_some_and(|v| v["error"] == "token_refused")
}

/// Whether a pipeline holds the writer lock of `data_dir` now, as `notedown pipeline status`
/// asks it: open `<data>/pipeline/.writer.lock` read-only and try a shared lock for a moment.
/// Creates nothing; no file means no writer.
pub fn pipeline_lock(data_dir: &Path) -> bool {
    let path = data_dir.join(PIPELINE_DIR).join(WRITER_LOCK_FILE);
    let Ok(file) = File::open(&path) else {
        return false;
    };
    matches!(file.try_lock_shared(), Err(TryLockError::WouldBlock))
}

/// A small HTTP server on loopback for the tests: it answers every request with one fixed
/// response and stops when dropped.
#[cfg(test)]
pub(crate) mod test_server {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::Duration;

    pub struct TestServer {
        pub port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    /// The answer of a Notedown server to `GET /rest/v1/account` without a token.
    pub fn notedown_401() -> String {
        http(
            "401 Unauthorized",
            r#"{"error":"token_refused","message":"no token"}"#,
        )
    }

    /// Waits up to `deadline` for a connection on a nonblocking `listener`. The stream it gives
    /// blocks again and has a read timeout of 5 s, so a test that reads from it cannot hang.
    pub fn accept_within(listener: &TcpListener, deadline: Duration) -> Option<TcpStream> {
        let until = std::time::Instant::now() + deadline;
        while std::time::Instant::now() < until {
            match listener.accept() {
                Ok((stream, _)) => {
                    // An accepted socket keeps the listener's nonblocking flag on macOS.
                    stream.set_nonblocking(false).ok()?;
                    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
                    return Some(stream);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        None
    }

    pub fn http(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             www-authenticate: Bearer\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    impl TestServer {
        /// Answers each connection with `response`; `None` closes it without a word.
        pub fn start(response: Option<String>) -> Self {
            Self::start_on("127.0.0.1:0", response)
        }

        /// [`start`](Self::start) on another address, such as `[::1]:0`.
        pub fn start_on(addr: &str, response: Option<String>) -> Self {
            let listener = TcpListener::bind(addr).unwrap();
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::spawn(move || {
                while !flag.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((stream, _)) => answer(stream, response.as_deref()),
                        Err(_) => std::thread::sleep(Duration::from_millis(5)),
                    }
                }
            });
            Self {
                port,
                stop,
                thread: Some(thread),
            }
        }
    }

    fn answer(mut stream: TcpStream, response: Option<&str>) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut seen = Vec::new();
        let mut buf = [0u8; 1024];
        while !seen.windows(4).any(|w| w == b"\r\n\r\n") {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.extend_from_slice(&buf[..n]),
            }
        }
        if let Some(response) = response {
            let _ = stream.write_all(response.as_bytes());
        }
    }

    impl Drop for TestServer {
        /// Stops the accept loop; the listener closes with the thread, so the port is free
        /// when this returns.
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// A port nothing listens on (taken from the system, then let go).
    pub fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::*;
    use super::*;
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    #[test]
    fn a_notedown_server_is_recognised() {
        let s = TestServer::start(Some(notedown_401()));
        assert_eq!(server(s.port), ServerProbe::Notedown);
    }

    #[test]
    fn a_free_port_is_none() {
        assert_eq!(server(free_port()), ServerProbe::None);
    }

    #[test]
    fn another_program_on_the_port_is_not_notedown() {
        let ok = TestServer::start(Some(http("200 OK", r#"{"error":"token_refused"}"#)));
        assert_eq!(server(ok.port), ServerProbe::Other, "200");
        let other = TestServer::start(Some(http("401 Unauthorized", r#"{"error":"nope"}"#)));
        assert_eq!(
            server(other.port),
            ServerProbe::Other,
            "401 with another body"
        );
        let text = TestServer::start(Some(http("401 Unauthorized", "token_refused")));
        assert_eq!(server(text.port), ServerProbe::Other, "401 without JSON");
        let silent = TestServer::start(None);
        assert_eq!(server(silent.port), ServerProbe::Other, "closes at once");
    }

    #[test]
    fn a_listener_that_never_answers_is_other_and_does_not_hang() {
        // Accepts (the backlog does) but never reads or writes.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let started = Instant::now();
        assert_eq!(server(port), ServerProbe::Other);
        assert!(started.elapsed() < crate::test_procs::HANG_GUARD);
        drop(listener);
    }

    #[test]
    fn a_dripping_listener_does_not_hold_the_probe() {
        // Answers one byte every 200 ms, for 200 s: longer than the guard below.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let _done = done_tx;
            let Some(mut stream) = accept_within(&listener, crate::test_procs::HANG_GUARD) else {
                return;
            };
            let answer = notedown_401();
            for b in answer.as_bytes().iter().cycle().take(1000) {
                if flag.load(std::sync::atomic::Ordering::SeqCst) {
                    break;
                }
                if std::io::Write::write_all(&mut stream, &[*b]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        });
        let started = Instant::now();
        let got = server(port);
        let took = started.elapsed();
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        // The sender drops when the drip ends; a drip that does not end fails the test.
        assert_eq!(
            done_rx.recv_timeout(crate::test_procs::HANG_GUARD),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
        );
        assert_eq!(got, ServerProbe::Other);
        assert!(
            took < crate::test_procs::HANG_GUARD,
            "the probe took {took:?}"
        );
    }

    #[test]
    fn a_server_on_ipv6_loopback_is_recognised() {
        let s = TestServer::start_on("[::1]:0", Some(notedown_401()));
        let addr: SocketAddr = format!("[::1]:{}", s.port).parse().unwrap();
        assert_eq!(server_at(addr), ServerProbe::Notedown);
        let other = TestServer::start_on("[::1]:0", Some(http("200 OK", "{}")));
        let addr: SocketAddr = format!("[::1]:{}", other.port).parse().unwrap();
        assert_eq!(server_at(addr), ServerProbe::Other);
    }

    #[test]
    fn the_pipeline_lock_probe_sees_a_writer_and_creates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("pipeline").join(".writer.lock");
        assert!(!pipeline_lock(dir.path()));
        assert!(!lock.exists(), "the probe created the lock file");

        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        let writer = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock)
            .unwrap();
        assert!(
            !pipeline_lock(dir.path()),
            "a file without a lock is no writer"
        );
        writer.try_lock().unwrap();
        assert!(pipeline_lock(dir.path()));
        writer.unlock().unwrap();
        assert!(!pipeline_lock(dir.path()));
    }
}
