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
        Self::start_with(answers, |_, _| {})
    }

    /// [`start`](Self::start), and runs `hook` with the number and the request of each call
    /// before it answers: a test changes things while a call waits for its answer.
    pub fn start_with(
        answers: Vec<String>,
        mut hook: impl FnMut(usize, &Request) + Send + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (tx, requests) = channel();
        let thread = std::thread::spawn(move || {
            for (n, reply) in answers.into_iter().enumerate() {
                let Some(mut stream) = accept(&listener, &flag) else {
                    return;
                };
                let Some(request) = read_request(&mut stream) else {
                    return;
                };
                hook(n, &request);
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

/// What a [`Routed`] server does with one request.
pub enum Act {
    /// Reads the body, then answers.
    Answer(String),
    /// Reads the body, says `arrived`, waits for `go`, then answers.
    Hold {
        answer: String,
        arrived: std::sync::mpsc::Sender<()>,
        go: Receiver<()>,
    },
    /// Reads the body piece by piece and runs `each` with the bytes read so far after each
    /// piece, then answers.
    Slow {
        answer: String,
        each: Box<dyn FnMut(usize) + Send>,
    },
    /// Reads at least `after` bytes of the body and says `reached`; then reads nothing more
    /// until `release` (or the end of the server). Then it reads what is left and says on
    /// `closed` whether the client had closed or reset the connection (true), or the whole
    /// body came or nothing came within the read timeout (false). It never answers.
    Stall {
        after: usize,
        reached: std::sync::mpsc::Sender<()>,
        release: Receiver<()>,
        closed: std::sync::mpsc::Sender<bool>,
    },
    /// Reads at least `after` bytes of the body, then closes the connection without an answer.
    Cut { after: usize },
}

/// A scripted HTTP server on loopback that serves each connection on a thread of its own and
/// picks the act by the request line: the first unused route whose `"<METHOD> <target>"` is the
/// request's. A request no route takes gets a 500. Every thread ends when the server is dropped.
pub struct Routed {
    pub port: u16,
    requests: Receiver<Request>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

type Routes = Arc<std::sync::Mutex<Vec<Option<(String, Act)>>>>;
type Hook = Arc<dyn Fn(&str) + Send + Sync>;

impl Routed {
    /// Serves `routes`, and runs `hook` with each request line (`"PUT /rest/v1/..."`) as soon
    /// as its head is read.
    pub fn start_with(
        routes: Vec<(String, Act)>,
        hook: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let (tx, requests) = channel();
        let routes: Routes = Arc::new(std::sync::Mutex::new(
            routes.into_iter().map(Some).collect(),
        ));
        let hook: Hook = Arc::new(hook);
        let thread = std::thread::spawn(move || {
            let mut served = Vec::new();
            while !flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (routes, tx, flag, hook) =
                            (routes.clone(), tx.clone(), flag.clone(), hook.clone());
                        served.push(std::thread::spawn(move || {
                            serve(stream, &routes, &tx, &flag, &*hook);
                        }));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
            for thread in served {
                let _ = thread.join();
            }
        });
        Self {
            port,
            requests,
            stop,
            thread: Some(thread),
        }
    }

    /// The requests that came so far, without waiting.
    pub fn received(&self) -> Vec<Request> {
        self.requests.try_iter().collect()
    }
}

impl Drop for Routed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The head of a request and what of its body came with it.
fn read_head(stream: &mut TcpStream) -> Option<(Request, usize)> {
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
    let request = Request {
        method,
        target,
        headers,
        body: data[head_end..].to_vec(),
    };
    Some((request, length))
}

/// How a read of the body ended.
#[derive(PartialEq, Eq)]
enum BodyEnd {
    /// It has the bytes it wanted.
    Whole,
    /// The client closed or reset the connection.
    Closed,
    /// Nothing came within the read timeout.
    TimedOut,
}

/// Reads the body until it has `want` bytes (or the connection ends), running `each` after
/// each piece.
fn read_body(
    stream: &mut TcpStream,
    body: &mut Vec<u8>,
    want: usize,
    each: &mut dyn FnMut(usize),
) -> BodyEnd {
    let mut buf = vec![0u8; 64 * 1024];
    while body.len() < want {
        match stream.read(&mut buf) {
            Ok(0) => return BodyEnd::Closed,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return BodyEnd::TimedOut;
            }
            Err(_) => return BodyEnd::Closed,
            Ok(n) => {
                body.extend_from_slice(&buf[..n]);
                each(body.len());
            }
        }
    }
    BodyEnd::Whole
}

fn serve(
    mut stream: TcpStream,
    routes: &Routes,
    tx: &std::sync::mpsc::Sender<Request>,
    stop: &AtomicBool,
    hook: &dyn Fn(&str),
) {
    // An accepted socket keeps the listener's nonblocking flag on macOS.
    if stream.set_nonblocking(false).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .is_err()
    {
        return;
    }
    let Some((mut request, length)) = read_head(&mut stream) else {
        return;
    };
    let line = format!("{} {}", request.method, request.target);
    hook(&line);
    let act = {
        let mut routes = routes.lock().unwrap();
        routes
            .iter_mut()
            .find(|r| r.as_ref().is_some_and(|(key, _)| *key == line))
            .and_then(Option::take)
            .map(|(_, act)| act)
    };
    let mut body = std::mem::take(&mut request.body);
    let reply = match act {
        None => {
            read_body(&mut stream, &mut body, length, &mut |_| {});
            Some(answer(
                "500 Internal Server Error",
                r#"{"error":"no_route_in_test"}"#,
            ))
        }
        Some(Act::Answer(reply)) => {
            read_body(&mut stream, &mut body, length, &mut |_| {});
            Some(reply)
        }
        Some(Act::Hold {
            answer,
            arrived,
            go,
        }) => {
            read_body(&mut stream, &mut body, length, &mut |_| {});
            request.body = std::mem::take(&mut body);
            let _ = tx.send(clone_request(&request));
            let _ = arrived.send(());
            let _ = go.recv_timeout(WAIT);
            let _ = stream.write_all(answer.as_bytes());
            let _ = stream.flush();
            return;
        }
        Some(Act::Slow { answer, mut each }) => {
            read_body(&mut stream, &mut body, length, &mut *each);
            Some(answer)
        }
        Some(Act::Stall {
            after,
            reached,
            release,
            closed,
        }) => {
            read_body(&mut stream, &mut body, after.min(length), &mut |_| {});
            request.body = std::mem::take(&mut body);
            let _ = tx.send(clone_request(&request));
            let _ = reached.send(());
            let until = Instant::now() + WAIT;
            while Instant::now() < until && !stop.load(Ordering::SeqCst) {
                if release.recv_timeout(Duration::from_millis(20)).is_ok() {
                    break;
                }
            }
            // The client that went away closes or resets the stream before the whole body
            // came; a client that only stands still runs into the read timeout.
            let mut rest = request.body.clone();
            let end = read_body(&mut stream, &mut rest, length, &mut |_| {});
            let _ = closed.send(end == BodyEnd::Closed);
            None
        }
        Some(Act::Cut { after }) => {
            read_body(&mut stream, &mut body, after.min(length), &mut |_| {});
            request.body = body;
            let _ = tx.send(request);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            return;
        }
    };
    // A stall sent its request already.
    if let Some(reply) = reply {
        request.body = body;
        let _ = tx.send(request);
        let _ = stream.write_all(reply.as_bytes());
        let _ = stream.flush();
    }
}

fn clone_request(r: &Request) -> Request {
    Request {
        method: r.method.clone(),
        target: r.target.clone(),
        headers: r.headers.clone(),
        body: r.body.clone(),
    }
}
