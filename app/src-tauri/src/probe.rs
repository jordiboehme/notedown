//! Looks for another Notedown before the app starts its own: a server on the port, or a
//! pipeline that holds the writer lock of the data folder.

use std::fs::{File, TryLockError};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

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
const MAX_ANSWER: u64 = 16 * 1024;

/// Asks `127.0.0.1:<port>` whether it is a Notedown server: `GET /rest/v1/account` without a
/// token must give 401 with the error code `token_refused`.
pub fn server(port: u16) -> ServerProbe {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => s,
        Err(_) => return ServerProbe::None,
    };
    if is_notedown(&mut stream, port).unwrap_or(false) {
        ServerProbe::Notedown
    } else {
        ServerProbe::Other
    }
}

fn is_notedown(stream: &mut TcpStream, port: u16) -> std::io::Result<bool> {
    stream.set_read_timeout(Some(ANSWER_TIMEOUT))?;
    stream.set_write_timeout(Some(ANSWER_TIMEOUT))?;
    write!(
        stream,
        "GET /rest/v1/account HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\n\
         Connection: close\r\n\r\n"
    )?;
    let mut answer = Vec::new();
    stream.take(MAX_ANSWER).read_to_end(&mut answer)?;
    Ok(token_refused(&answer))
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
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
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
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(listener);
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
