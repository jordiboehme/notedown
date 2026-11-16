//! A scripted HTTP server on loopback for the REST tests: it answers the requests in order,
//! one per connection, and keeps what it got (head and body). Every wait is bounded and the
//! thread ends when the server is dropped.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How long the server waits for a connection, and a test for a request.
const WAIT: Duration = Duration::from_secs(10);

/// One request the server got.
pub struct Request {
    pub method: String,
    /// The path with the query, as sent.
    pub target: String,
    /// Names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("the body is no JSON")
    }
}

/// An answer with a JSON body.
pub fn answer(status: &str, body: &str) -> String {
    answer_with(status, &[], body)
}

/// An answer with more header lines, such as `("retry-after", "5")`.
pub fn answer_with(status: &str, headers: &[(&str, &str)], body: &str) -> String {
    let mut head = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n",
        body.len()
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    format!("{head}\r\n{body}")
}

pub struct Scripted {
    pub port: u16,
    requests: Receiver<Request>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Scripted {
    /// Answers the next connections with `answers`, in order; then accepts no more.
    pub fn start(answers: Vec<String>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (tx, requests) = channel();
        let thread = std::thread::spawn(move || {
            for reply in answers {
                let Some(mut stream) = accept(&listener, &flag) else {
                    return;
                };
                let Some(request) = read_request(&mut stream) else {
                    return;
                };
                let _ = tx.send(request);
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        Self {
            port,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}/rest/v1", self.port)
    }

    /// The next request; fails the test when none comes in time.
    pub fn next(&self) -> Request {
        self.requests.recv_timeout(WAIT).expect("no request came")
    }

    /// The requests that came so far, without waiting.
    pub fn received(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }
}

impl Drop for Scripted {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn accept(listener: &TcpListener, stop: &AtomicBool) -> Option<TcpStream> {
    let until = Instant::now() + WAIT;
    while Instant::now() < until && !stop.load(Ordering::SeqCst) {
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

/// Reads the head, then the body by `Content-Length`.
fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut data = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => data.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&data[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_owned();
    let target = first.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = data[head_end..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return None,
            Ok(n) => body.extend_from_slice(&buf[..n]),
        }
    }
    Some(Request {
        method,
        target,
        headers,
        body,
    })
}
