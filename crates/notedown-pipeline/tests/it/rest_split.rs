//! Two machines over REST, end to end: a real server on a runtime of its own thread, the Mac's
//! worker on the test thread with a local folder made by `connect`, the way a Mac reads a
//! Raspberry Pi.

use std::io::Read;
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use notedown_cloud::auth::Accounts;
use notedown_cloud::journal::{Event, EventBody, Journal, Origin, new_op_id};
use notedown_pipeline::connect::connect;
use notedown_pipeline::cursor;
use notedown_pipeline::model::LineChange;
use notedown_pipeline::rest::RestSource;
use notedown_pipeline::setup::Setup;
use notedown_pipeline::store::TextStore;
use notedown_pipeline::worker::{Clock, Worker};
use tempfile::TempDir;

use crate::rest::{Server, refused_message};
use crate::split::{
    FIRST_ID, SECOND_ID, Said, continuous, create_account, last_seq, load, log, owner_of, point_at,
    put_into, remove_account, removed_message, store_dir, texts_of, tombstones,
};
use crate::worker::{Data, SLUG, TestClock, engine_reading, no_settle, now_ms, setup, stop_at};

/// A proxy in front of the server on a port that never changes. Switched off, it closes every
/// open connection and every new one after its request head, like a server that is not
/// running.
struct Proxy {
    addr: SocketAddr,
    on: Arc<AtomicBool>,
    open: Arc<Mutex<Vec<TcpStream>>>,
}

impl Proxy {
    fn start(backend: SocketAddr) -> Proxy {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let on = Arc::new(AtomicBool::new(true));
        let open: Arc<Mutex<Vec<TcpStream>>> = Arc::default();
        let (on2, open2) = (on.clone(), open.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                let Ok(client) = client else { return };
                // Under the lock, so `off` never misses a connection.
                let mut held = open2.lock().unwrap();
                if !on2.load(Ordering::SeqCst) {
                    drop(held);
                    refuse(client);
                    continue;
                }
                let Ok(server) = TcpStream::connect(backend) else {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                };
                held.push(client.try_clone().unwrap());
                held.push(server.try_clone().unwrap());
                pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
                pipe(server, client);
            }
        });
        Proxy { addr, on, open }
    }

    fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn off(&self) {
        let mut held = self.open.lock().unwrap();
        self.on.store(false, Ordering::SeqCst);
        for s in held.drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn on(&self) {
        self.on.store(true, Ordering::SeqCst);
    }
}

/// Reads the request head, then closes the connection. Closing before the request is read
/// would answer with a reset or a close, depending on timing; this is always a close.
fn refuse(mut client: TcpStream) {
    std::thread::spawn(move || {
        let _ = client.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match client.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                _ => break,
            }
        }
        let _ = client.shutdown(Shutdown::Both);
    });
}

/// Copies `from` to `to` on a thread of its own until either side closes.
fn pipe(mut from: TcpStream, mut to: TcpStream) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if std::io::Write::write_all(&mut to, &buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to.shutdown(Shutdown::Both);
        let _ = from.shutdown(Shutdown::Both);
    });
}

/// The server and the proxy the Mac talks to.
struct ServerHandle {
    proxy: Proxy,
    _server: Server,
}

impl ServerHandle {
    fn start(data: &Path) -> ServerHandle {
        let server = Server::start(data);
        let backend = server.url().trim_start_matches("http://").parse().unwrap();
        ServerHandle {
            proxy: Proxy::start(backend),
            _server: server,
        }
    }

    fn url(&self) -> String {
        self.proxy.url()
    }

    /// Stops serving; the port stays.
    fn stop(&self) {
        self.proxy.off();
    }

    fn serve(&self) {
        self.proxy.on();
    }
}

/// `notedown connect` for account `slug` (`<slug>@example.org`, as the tests create them).
fn connect_as(h: &ServerHandle, local: &Path, slug: &str, password: &str) {
    let c = connect(
        &h.url(),
        local,
        &format!("{slug}@example.org"),
        password,
        "test",
    )
    .unwrap();
    assert_eq!(c.slug, slug);
}

/// The server folder from `setup()`, served; a local folder with `pipeline.toml` and the token
/// of the account `jordi`, both from `connect`.
fn over_rest() -> (Data, TempDir, ServerHandle) {
    let d = setup();
    let h = ServerHandle::start(&d.data);
    let local = tempfile::tempdir().unwrap();
    connect_as(&h, local.path(), SLUG, "secret");
    (d, local, h)
}

/// The Mac's setup from its `pipeline.toml`, with the REST source on the test clock.
fn mac(local: &Path, h: &ServerHandle, clock: &TestClock) -> Setup {
    let mut s = Setup::load(local).unwrap();
    assert!(s.two_machines);
    assert_eq!(s.source.describe(), h.url(), "pipeline.toml has the url");
    s.cfg = no_settle();
    s.source = Arc::new(
        RestSource::new(&h.url(), &local.join("tokens"), local)
            .unwrap()
            .with_clock(Arc::new(clock.clone())),
    );
    s
}

/// A continuous worker on the Mac with a test clock and the messages in `said`.
fn mac_worker(local: &Path, h: &ServerHandle, said: &Said) -> (Worker, TestClock) {
    let clock = TestClock::at(now_ms());
    let w = Worker::new(&mac(local, h, &clock), Box::new(engine_reading("hello")))
        .with_clock(Arc::new(clock.clone()))
        .with_messages(said.sink());
    (w, clock)
}

/// Runs `w` for `ms` of test time, from a clock past every record written so far.
fn run_for(w: &mut Worker, clock: &TestClock, ms: u64) {
    clock.set(clock.now_ms().max(now_ms()));
    w.run(stop_at(clock, clock.now_ms() + ms)).unwrap();
}

fn refused(h: &ServerHandle, local: &Path, slug: &str) -> String {
    format!(
        "notedown pipeline: {}",
        refused_message(&h.url(), local, slug)
    )
}

fn has_note(local: &Path, key: &str) -> bool {
    TextStore::open(local, SLUG)
        .unwrap()
        .note(key)
        .unwrap()
        .is_some_and(|n| !n.deleted)
}

/// No line of the Mac's log deletes a note.
fn nothing_deleted(local: &Path) -> bool {
    log(local)
        .iter()
        .all(|l| l.change != LineChange::NoteDeleted)
}

#[test]
fn the_pipeline_reads_notes_over_rest() {
    let (d, local, h) = over_rest();
    create_account(&d.data, "anna");
    connect_as(&h, local.path(), "anna", "secret");
    d.put("test1", "NOTE/Note");
    d.put("test2", "NOTE/Note/Sub");
    put_into(&d.data, "anna", "test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    assert_eq!(
        cursor::read(local.path(), SLUG).unwrap(),
        last_seq(&d.data),
        "{:?}",
        said.lines()
    );

    // One machine on a copy of the same folder.
    let tmp = tempfile::tempdir().unwrap();
    let copy = tmp.path().join("data");
    copy_dir(&d.data, &copy);
    Worker::new(&load(&copy), Box::new(engine_reading("hello")))
        .run_once(true)
        .unwrap();
    assert_eq!(texts_of(local.path(), SLUG).len(), 2);
    for slug in [SLUG, "anna"] {
        let (rest, folder) = (texts_of(local.path(), slug), texts_of(&copy, slug));
        assert!(!rest.is_empty());
        assert_eq!(rest, folder, "{slug}: {:?}", said.lines());
        assert_eq!(owner_of(local.path(), slug), owner_of(&copy, slug));
    }
    assert!(said.lines().is_empty(), "{:?}", said.lines());
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        let kind = e.file_type().unwrap();
        if kind.is_dir() {
            copy_dir(&e.path(), &target);
        } else if kind.is_file() {
            std::fs::copy(e.path(), &target).unwrap();
        }
    }
}

#[test]
fn removal_over_rest_deletes_the_text() {
    let (d, local, h) = over_rest();
    d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    let dir = store_dir(local.path(), SLUG);
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));

    remove_account(&d.data, SLUG);
    run_for(&mut w, &clock, 5_000);
    assert!(!dir.exists(), "{:?}", said.lines());
    assert_eq!(said.count(&removed_message(SLUG)), 1, "{:?}", said.lines());
    assert!(tombstones(local.path()).is_empty());

    // The slug again, with notes: the old token reads only the old account's removal.
    create_account(&d.data, SLUG);
    let new = d.put("test2", "NOTE/Note");
    run_for(&mut w, &clock, 25 * 60_000);
    assert!(!dir.exists(), "{:?}", said.lines());
    assert!(
        TextStore::open(local.path(), SLUG)
            .unwrap()
            .note(&new)
            .unwrap()
            .is_none()
    );
    assert_eq!(said.count(&removed_message(SLUG)), 1, "{:?}", said.lines());
    assert_eq!(said.count(&refused(&h, local.path(), SLUG)), 0);
}

#[test]
fn reconnecting_to_a_reused_slug_drops_the_old_text() {
    let (d, local, h) = over_rest();
    let old = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    assert!(has_note(local.path(), &old));
    let before = cursor::read(local.path(), SLUG).unwrap();

    // While the Mac is away: the account is removed and made again, with a note.
    remove_account(&d.data, SLUG);
    create_account(&d.data, SLUG);
    let new = d.put("test2", "NOTE/Note");
    connect_as(&h, local.path(), SLUG, "secret");
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), before);
    run_for(&mut w, &clock, 5_000);

    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert!(store.note(&old).unwrap().is_none(), "{:?}", said.lines());
    assert!(has_note(local.path(), &new), "{:?}", said.lines());
    assert_eq!(texts_of(local.path(), SLUG).len(), 1);
    assert_eq!(owner_of(local.path(), SLUG), Some(SECOND_ID));
    let replaced = format!(
        "notedown pipeline: account {SLUG} belongs to a new user id ({SECOND_ID}, was \
         {FIRST_ID}); its old note text is deleted"
    );
    assert_eq!(
        said.count(&replaced) + said.count(&removed_message(SLUG)),
        1,
        "{:?}",
        said.lines()
    );
    assert!(tombstones(local.path()).is_empty());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn a_refused_token_keeps_the_text() {
    let (d, local, h) = over_rest();
    let key1 = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    let state = TextStore::open(local.path(), SLUG)
        .unwrap()
        .note(&key1)
        .unwrap();
    let first = cursor::read(local.path(), SLUG).unwrap();

    assert_eq!(
        Accounts::new(&d.data)
            .set_password(SLUG, "secret2")
            .unwrap(),
        1
    );
    let key2 = d.put("test2", "NOTE/Note");
    run_for(&mut w, &clock, 25 * 60_000);
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert_eq!(store.note(&key1).unwrap(), state, "the store stays");
    assert!(store.note(&key2).unwrap().is_none());
    assert!(nothing_deleted(local.path()));
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), first);
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    let message = refused(&h, local.path(), SLUG);
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());

    // A new connect resumes, without a restart.
    connect_as(&h, local.path(), SLUG, "secret2");
    run_for(&mut w, &clock, 5_000);
    assert!(has_note(local.path(), &key1));
    assert!(has_note(local.path(), &key2), "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());
}

#[test]
fn a_stale_removal_record_and_a_password_change_keep_the_text() {
    let (d, local, h) = over_rest();
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    let first = cursor::read(local.path(), SLUG).unwrap();

    // A crash between the record and the removal leaves the account alive with a record; then
    // the password changes, so the token is refused. The Mac has not read the record yet: a
    // removed token would make it read the record while the server lists no such account.
    Journal::open(&d.data)
        .unwrap()
        .append(&mut [Event::new(
            SLUG,
            Origin::Cli,
            None,
            &new_op_id(),
            EventBody::AccountRemoved { user_id: FIRST_ID },
        )])
        .unwrap();
    Accounts::new(&d.data)
        .set_password(SLUG, "secret2")
        .unwrap();
    run_for(&mut w, &clock, 25 * 60_000);
    assert!(has_note(local.path(), &key), "{:?}", said.lines());
    assert!(nothing_deleted(local.path()));
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    assert_eq!(said.count(&removed_message(SLUG)), 0, "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), first);
    let message = refused(&h, local.path(), SLUG);
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());

    // A new connect reads the record past, with the account listed: the text stays.
    connect_as(&h, local.path(), SLUG, "secret2");
    run_for(&mut w, &clock, 5_000);
    assert!(has_note(local.path(), &key), "{:?}", said.lines());
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    assert_eq!(said.count(&removed_message(SLUG)), 0, "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn an_unknown_user_id_keeps_the_text() {
    let (d, local, h) = over_rest();
    let key = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    let first = cursor::read(local.path(), SLUG).unwrap();

    // An older backup of the account folder: another user id, and no removal record.
    let accounts = Accounts::new(&d.data);
    let mut a = accounts.get(SLUG).unwrap().unwrap();
    a.user_id += 100;
    accounts.save(&a).unwrap();
    run_for(&mut w, &clock, 25 * 60_000);
    assert!(has_note(local.path(), &key), "{:?}", said.lines());
    assert!(nothing_deleted(local.path()));
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    assert_eq!(said.count(&removed_message(SLUG)), 0, "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), first);
    let message = refused(&h, local.path(), SLUG);
    assert_eq!(said.count(&message), 1, "{:?}", said.lines());
}

#[test]
fn a_locked_account_keeps_reading() {
    let (d, local, h) = over_rest();
    let key1 = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);

    // Locked as by too many failed logins: a login is refused, the token still reads.
    let accounts = Accounts::new(&d.data);
    let mut a = accounts.get(SLUG).unwrap().unwrap();
    a.locked = true;
    a.locked_at_ms = Some(now_ms());
    a.failed_logins = notedown_cloud::auth::MAX_FAILED_LOGINS;
    accounts.save(&a).unwrap();
    let other = tempfile::tempdir().unwrap();
    assert!(
        connect(
            &h.url(),
            other.path(),
            "jordi@example.org",
            "secret",
            "test"
        )
        .is_err(),
        "the lock blocks logins"
    );
    let key2 = d.put("test2", "NOTE/Note");
    run_for(&mut w, &clock, 5_000);
    assert!(has_note(local.path(), &key1));
    assert!(has_note(local.path(), &key2), "{:?}", said.lines());
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
    assert!(said.lines().is_empty(), "{:?}", said.lines());
}

#[test]
fn an_unreachable_server_keeps_the_text() {
    let (d, local, h) = over_rest();
    let key1 = d.put("test1", "NOTE/Note");
    let said = Said::default();
    let (mut w, clock) = mac_worker(local.path(), &h, &said);
    run_for(&mut w, &clock, 1);
    let first = cursor::read(local.path(), SLUG).unwrap();
    let dir = store_dir(local.path(), SLUG);
    let state = TextStore::open(local.path(), SLUG)
        .unwrap()
        .note(&key1)
        .unwrap();

    h.stop();
    let key2 = d.put("test2", "NOTE/Note");
    run_for(&mut w, &clock, 25 * 60_000);
    let store = TextStore::open(local.path(), SLUG).unwrap();
    assert_eq!(store.note(&key1).unwrap(), state, "the store is unchanged");
    assert!(store.note(&key2).unwrap().is_none());
    assert_eq!(log(local.path()).len(), 1);
    assert!(nothing_deleted(local.path()));
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), first);
    assert_eq!(owner_of(local.path(), SLUG), Some(FIRST_ID));
    assert!(dir.join("notes").is_dir());
    let away = said.count(&h.url());
    assert_eq!(away, 1, "said once: {:?}", said.lines());

    h.serve();
    run_for(&mut w, &clock, 5_000);
    assert!(has_note(local.path(), &key1));
    assert!(has_note(local.path(), &key2), "{:?}", said.lines());
    assert_eq!(log(local.path()).len(), 2);
    assert_eq!(cursor::read(local.path(), SLUG).unwrap(), last_seq(&d.data));
}

#[test]
fn catch_up_over_rest_gives_the_same_text() {
    use crate::worker::with_parts;
    let d = setup();
    create_account(&d.data, "anna");
    let h = ServerHandle::start(&d.data);
    // Mac A reads the folder all the time, as in plan 1's catch-up test.
    let a = tempfile::tempdir().unwrap();
    point_at(a.path(), &d.data);
    let said = Said::default();
    let (mut mac_a, clock) = continuous(&load(a.path()), &said);
    let mut step = |f: &dyn Fn()| {
        f();
        clock.set(clock.now_ms().max(now_ms()));
        mac_a.run(stop_at(&clock, clock.now_ms() + 1)).unwrap();
    };
    let key1 = std::cell::RefCell::new(String::new());
    step(&|| *key1.borrow_mut() = d.put("test1", "NOTE/Note"));
    step(&|| {
        d.edit_record(&key1.borrow(), |r| {
            r.pages[0] = with_parts(&r.pages[0], Some("0123456789abcdef"), None, None);
        })
    });
    step(&|| {
        d.put("test2", "NOTE/Note");
    });
    let old = std::cell::RefCell::new(String::new());
    step(&|| *old.borrow_mut() = put_into(&d.data, "anna", "test1", "NOTE/Note/Old"));
    assert_eq!(texts_of(a.path(), "anna").len(), 1);
    step(&|| {
        remove_account(&d.data, "anna");
        create_account(&d.data, "anna");
        put_into(&d.data, "anna", "test2", "NOTE/Note/New");
    });

    // Mac B connects only now and reads from cursor 0 over REST.
    let b = tempfile::tempdir().unwrap();
    connect_as(&h, b.path(), SLUG, "secret");
    connect_as(&h, b.path(), "anna", "secret");
    let said_b = Said::default();
    let (mut mac_b, clock_b) = mac_worker(b.path(), &h, &said_b);
    run_for(&mut mac_b, &clock_b, 1);
    for slug in [SLUG, "anna"] {
        let (ta, tb) = (texts_of(a.path(), slug), texts_of(b.path(), slug));
        assert!(!ta.is_empty());
        assert_eq!(ta, tb, "{slug}: {:?}", said.lines());
        assert!(owner_of(a.path(), slug).is_some());
        assert_eq!(owner_of(a.path(), slug), owner_of(b.path(), slug));
        assert_eq!(
            cursor::read(b.path(), slug).unwrap(),
            cursor::read(a.path(), slug).unwrap()
        );
    }
    assert_eq!(texts_of(b.path(), "anna").len(), 1, "only the new note");
    let anna = TextStore::open(b.path(), "anna").unwrap();
    assert!(
        anna.note(&old.borrow()).unwrap().is_none(),
        "no trace of the old account's note"
    );
    assert!(said_b.lines().is_empty(), "{:?}", said_b.lines());
}
