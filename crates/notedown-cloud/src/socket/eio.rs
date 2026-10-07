//! Engine.IO 3 and socket.io 2 packets: parsing what the device sends and building what the
//! server sends (`docs/sync-protocol.md` 4.2 and 4.3).
//!
//! A text frame is one Engine.IO packet: a type digit (`0` open, `1` close, `2` ping, `3` pong,
//! `4` message, `5` upgrade, `6` noop) and its data. A message carries one socket.io packet: a
//! type digit (`0` connect, `1` disconnect, `2` event, `3` ack, ...), an optional namespace
//! (`/name,`), an optional ack id (digits) and JSON data.

use serde::Serialize;
use serde_json::Value;

/// Ratta's `socket.pinginterval` in ms (4.1).
pub const PING_INTERVAL_MS: u64 = 5000;
/// Ratta's `socket.pingtimeout` in ms (4.1).
pub const PING_TIMEOUT_MS: u64 = 25000;

pub const EVENT_RATTA_PING: &str = "ratta_ping";
pub const EVENT_CLIENT_MESSAGE: &str = "ClientMessage";
pub const EVENT_SERVER_MESSAGE: &str = "ServerMessage";

/// The socket.io connect packet of the default namespace.
pub const CONNECT: &str = "40";
/// The socket.io disconnect packet of the default namespace.
pub const DISCONNECT: &str = "41";

/// What a frame from the device is.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// Engine.IO ping `2` with its optional data (`2probe`).
    Ping(String),
    /// Engine.IO pong, upgrade or noop: known, nothing to answer.
    Ignored,
    /// Engine.IO close `1` or socket.io disconnect `41`: the device ends the session.
    Close,
    /// socket.io connect `40` to the default namespace.
    Connect,
    /// socket.io event `42` in the default namespace.
    Event {
        name: String,
        args: Vec<Value>,
        ack: Option<u64>,
    },
    /// Anything else: not answered, logged with `unknown_event`.
    Unknown,
}

/// What the server does with a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Send this frame.
    Answer(String),
    /// Known frame without an answer.
    Nothing,
    /// The device closed the session.
    Close,
    /// Unknown frame: log it with `unknown_event`, do not answer, keep the connection.
    Unknown,
    /// Several frames in this order, or an unknown `ClientMessage` that still carried an ack id:
    /// send the frames and, when `unknown`, log the incoming frame with `unknown_event`.
    Reply { frames: Vec<String>, unknown: bool },
}

/// Parse one text frame.
pub fn parse(frame: &str) -> Inbound {
    let mut chars = frame.chars();
    match chars.next() {
        Some('1') if frame.len() == 1 => Inbound::Close,
        Some('2') => Inbound::Ping(frame[1..].to_string()),
        Some('3' | '5' | '6') => Inbound::Ignored,
        Some('4') => parse_message(&frame[1..]),
        _ => Inbound::Unknown,
    }
}

/// The socket.io packet inside an Engine.IO message.
fn parse_message(packet: &str) -> Inbound {
    let Some(kind) = packet.chars().next() else {
        return Inbound::Unknown;
    };
    let mut rest = &packet[kind.len_utf8()..];
    // Namespace: only the default one (`/`) is served.
    if rest.starts_with('/') {
        let (nsp, after) = match rest.find(',') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => (rest, ""),
        };
        if nsp != "/" {
            return Inbound::Unknown;
        }
        rest = after;
    }
    match kind {
        '0' if rest.is_empty() => Inbound::Connect,
        '1' if rest.is_empty() => Inbound::Close,
        '2' => parse_event(rest),
        _ => Inbound::Unknown,
    }
}

fn parse_event(data: &str) -> Inbound {
    let digits = data.chars().take_while(char::is_ascii_digit).count();
    let ack = if digits > 0 {
        match data[..digits].parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => return Inbound::Unknown,
        }
    } else {
        None
    };
    let Ok(Value::Array(mut items)) = serde_json::from_str::<Value>(&data[digits..]) else {
        return Inbound::Unknown;
    };
    if items.is_empty() {
        return Inbound::Unknown;
    }
    let Value::String(name) = items.remove(0) else {
        return Inbound::Unknown;
    };
    Inbound::Event {
        name,
        args: items,
        ack,
    }
}

/// The answer to a frame (`docs/sync-protocol.md` 4.2 and 4.3) of a session without pushes.
pub fn answer(frame: &str) -> Action {
    answer_with(frame, || None, || {})
}

/// The answer to a frame of a registered session. After the reply to `ratta_ping` or
/// `ClientMessage "status"` it sends what `deliver` returns (the session's queued file push),
/// before the ack, as Ratta does (4.3). `ClientMessage "Received"` calls `received`.
pub fn answer_with(
    frame: &str,
    deliver: impl FnOnce() -> Option<String>,
    received: impl FnOnce(),
) -> Action {
    match parse(frame) {
        Inbound::Ping(data) => Action::Answer(format!("3{data}")),
        Inbound::Ignored => Action::Nothing,
        Inbound::Close => Action::Close,
        Inbound::Connect => Action::Answer(CONNECT.to_string()),
        Inbound::Event { name, args, ack } => answer_event(&name, &args, ack, deliver, received),
        Inbound::Unknown => Action::Unknown,
    }
}

/// The answer to an event: its own answer and the queued push first, then the ack when the
/// packet carried an id (`43<id>[]`, or `43<id>["Received"]` for a `ClientMessage` that is not
/// `Received` or `status`). An event no listener handles gets no ack (4.1).
fn answer_event(
    name: &str,
    args: &[Value],
    ack: Option<u64>,
    deliver: impl FnOnce() -> Option<String>,
    received: impl FnOnce(),
) -> Action {
    let mut frames = Vec::new();
    let mut unknown = false;
    let mut ack_arg = None;
    match (name, args.first()) {
        (EVENT_RATTA_PING, _) => {
            frames.push(event(EVENT_RATTA_PING, "Received"));
            frames.extend(deliver());
        }
        (EVENT_CLIENT_MESSAGE, Some(Value::String(s))) if s == "status" => {
            frames.push(event(EVENT_SERVER_MESSAGE, "true"));
            frames.extend(deliver());
        }
        // Marks the sent file push delivered.
        (EVENT_CLIENT_MESSAGE, Some(Value::String(s))) if s == "Received" => received(),
        (EVENT_CLIENT_MESSAGE, _) => {
            unknown = true;
            ack_arg = Some("Received");
        }
        _ => return Action::Unknown,
    }
    if let Some(id) = ack {
        frames.push(match ack_arg {
            Some(arg) => format!("43{id}{}", Value::from(vec![arg])),
            None => format!("43{id}[]"),
        });
    }
    match (frames.len(), unknown) {
        (0, true) => Action::Unknown,
        (0, false) => Action::Nothing,
        (1, false) => Action::Answer(frames.remove(0)),
        _ => Action::Reply { frames, unknown },
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenData<'a> {
    sid: &'a str,
    upgrades: [&'a str; 0],
    ping_interval: u64,
    ping_timeout: u64,
}

/// The Engine.IO open packet: `0{"sid":...,"upgrades":[],"pingInterval":5000,"pingTimeout":25000}`
/// (fields in this order).
pub fn open_packet(sid: &str) -> String {
    let data = OpenData {
        sid,
        upgrades: [],
        ping_interval: PING_INTERVAL_MS,
        ping_timeout: PING_TIMEOUT_MS,
    };
    // Serializing a struct of strings and numbers cannot fail.
    format!("0{}", serde_json::to_string(&data).unwrap_or_default())
}

/// A socket.io event in the default namespace with one string argument: `42["name","arg"]`.
pub fn event(name: &str, arg: &str) -> String {
    format!("42{}", Value::from(vec![name, arg]))
}

/// The rejection of a bad token: `ServerMessage` with `{"code":"403","msg":...}` as one string
/// argument holding JSON (4.2, 4.4).
pub fn rejection(msg: &str) -> String {
    let payload = serde_json::json!({"code": "403", "msg": msg});
    event(EVENT_SERVER_MESSAGE, &payload.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeats() {
        assert_eq!(answer("2"), Action::Answer("3".into()));
        assert_eq!(answer("2probe"), Action::Answer("3probe".into()));
        let rp = Action::Answer(r#"42["ratta_ping","Received"]"#.into());
        assert_eq!(answer(r#"42["ratta_ping"]"#), rp);
        assert_eq!(
            answer(r#"427["ratta_ping",{"a":1}]"#),
            Action::Reply {
                frames: vec![r#"42["ratta_ping","Received"]"#.into(), "437[]".into()],
                unknown: false
            }
        );
        assert_eq!(answer(r#"42/,["ratta_ping"]"#), rp);
        assert_eq!(
            answer(r#"42["ClientMessage","status"]"#),
            Action::Answer(r#"42["ServerMessage","true"]"#.into())
        );
        assert_eq!(answer(r#"42["ClientMessage","Received"]"#), Action::Nothing);
        assert_eq!(
            answer(r#"4212["ratta_ping"]"#),
            Action::Reply {
                frames: vec![r#"42["ratta_ping","Received"]"#.into(), "4312[]".into()],
                unknown: false
            }
        );
        assert_eq!(
            answer(r#"42123["ClientMessage","status"]"#),
            Action::Reply {
                frames: vec![r#"42["ServerMessage","true"]"#.into(), "43123[]".into()],
                unknown: false
            }
        );
        assert_eq!(
            answer(r#"420["ClientMessage","Received"]"#),
            Action::Answer("430[]".into())
        );
        assert_eq!(
            answer(r#"425["ClientMessage","hello"]"#),
            Action::Reply {
                frames: vec![r#"435["Received"]"#.into()],
                unknown: true
            }
        );
        assert_eq!(answer("40"), Action::Answer("40".into()));
        assert_eq!(answer("40/,"), Action::Answer("40".into()));
        assert_eq!(answer("3"), Action::Nothing);
        assert_eq!(answer("6"), Action::Nothing);
        assert_eq!(answer("1"), Action::Close);
        assert_eq!(answer("41"), Action::Close);
    }

    #[test]
    fn unknown() {
        for f in [
            "",
            "0",
            "9",
            "4",
            "1x",
            "hello",
            "42",
            "42[]",
            "42[1]",
            "42{}",
            "42[broken",
            "43[1]",
            "45-[\"x\"]",
            "40/other,",
            "42/other,[\"ratta_ping\"]",
            "4299999999999999999999999[\"ratta_ping\"]",
            // Only an event a listener handles gets an ack (4.1).
            r#"429["mystery"]"#,
            r#"42x["ratta_ping"]"#,
            r#"42x5["ratta_ping"]"#,
            r#"42/ns,5["ratta_ping"]"#,
            r#"42["mystery"]"#,
            r#"42["ClientMessage","other"]"#,
            r#"42["ClientMessage"]"#,
            r#"42["ClientMessage",1]"#,
            "é",
            "4é",
            "42/é",
        ] {
            assert_eq!(answer(f), Action::Unknown, "{f:?}");
        }
    }

    /// `answer_with` with a queued push `P` and a counter of `Received` calls.
    fn with_push(frame: &str) -> (Action, usize, usize) {
        let delivered = std::cell::Cell::new(0);
        let received = std::cell::Cell::new(0);
        let action = answer_with(
            frame,
            || {
                delivered.set(delivered.get() + 1);
                Some("P".to_string())
            },
            || received.set(received.get() + 1),
        );
        (action, delivered.get(), received.get())
    }

    #[test]
    fn the_push_follows_the_heartbeat_reply_and_precedes_the_ack() {
        let reply = |frames: &[&str]| Action::Reply {
            frames: frames.iter().map(|f| f.to_string()).collect(),
            unknown: false,
        };
        assert_eq!(
            with_push(r#"42["ratta_ping"]"#),
            (reply(&[r#"42["ratta_ping","Received"]"#, "P"]), 1, 0)
        );
        assert_eq!(
            with_push(r#"4212["ratta_ping"]"#),
            (
                reply(&[r#"42["ratta_ping","Received"]"#, "P", "4312[]"]),
                1,
                0
            )
        );
        assert_eq!(
            with_push(r#"427["ClientMessage","status"]"#),
            (
                reply(&[r#"42["ServerMessage","true"]"#, "P", "437[]"]),
                1,
                0
            )
        );
        assert_eq!(
            with_push(r#"420["ClientMessage","Received"]"#),
            (Action::Answer("430[]".into()), 0, 1)
        );
        assert_eq!(
            with_push(r#"42["ClientMessage","Received"]"#),
            (Action::Nothing, 0, 1)
        );
        // Nothing else takes the queue or clears it.
        for f in [
            "2",
            "40",
            r#"425["ClientMessage","hello"]"#,
            r#"42["mystery"]"#,
            r#"42/ns,["ratta_ping"]"#,
            r#"42x["ratta_ping"]"#,
        ] {
            let (_, d, r) = with_push(f);
            assert_eq!((d, r), (0, 0), "{f}");
        }
        // An empty queue adds nothing.
        assert_eq!(
            answer_with(r#"42["ratta_ping"]"#, || None, || {}),
            Action::Answer(r#"42["ratta_ping","Received"]"#.into())
        );
    }

    #[test]
    fn server_packets() {
        assert_eq!(
            open_packet("abc"),
            r#"0{"sid":"abc","upgrades":[],"pingInterval":5000,"pingTimeout":25000}"#
        );
        assert_eq!(event("a", "b\"c"), r#"42["a","b\"c"]"#);
        assert_eq!(
            rejection("bad"),
            r#"42["ServerMessage","{\"code\":\"403\",\"msg\":\"bad\"}"]"#
        );
    }
}
