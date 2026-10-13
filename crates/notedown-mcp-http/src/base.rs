//! The public address of the server, from `external_base_url`, and the Host check.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, Request};

/// `external_base_url` when it is `https://<host>[:<port>]` (a trailing `/` is allowed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    /// `https://<authority>`, lowercase, no trailing slash.
    pub origin: String,
    /// `<host>[:<port>]`, lowercase; no port when it was 443 or not given.
    pub authority: String,
}

impl Base {
    pub fn parse(url: &str) -> Option<Base> {
        let rest = url.strip_prefix("https://")?;
        let rest = rest.strip_suffix('/').unwrap_or(rest);
        if rest.is_empty()
            || rest.contains(['/', '?', '#', '@'])
            || rest.contains(char::is_whitespace)
        {
            return None;
        }
        let authority = rest.to_ascii_lowercase();
        let authority = authority
            .strip_suffix(":443")
            .unwrap_or(&authority)
            .to_string();
        Some(Base {
            origin: format!("https://{authority}"),
            authority,
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.origin)
    }

    pub fn resource(&self) -> String {
        self.url("/mcp")
    }

    pub fn metadata_url(&self) -> String {
        self.url("/.well-known/oauth-protected-resource/mcp")
    }
}

/// The peer address of a request (the real one, or the stand-in of `MockConnectInfo` tests).
pub fn peer_of(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|c| c.0.ip())
        .or_else(|| {
            request
                .extensions()
                .get::<axum::extract::connect_info::MockConnectInfo<SocketAddr>>()
                .map(|c| c.0.ip())
        })
}

/// Whether the request is addressed to `base`: its `Host`, or `X-Forwarded-Host` when the peer
/// is a trusted proxy. A `:443` port is the same as none.
pub fn host_ok(base: &Base, request: &Request, trusted: &[IpAddr]) -> bool {
    // One header line, readable ASCII, no list: anything else is refused, never guessed at.
    let single = |name: &str| -> Option<Option<String>> {
        let mut lines = request.headers().get_all(name).iter();
        let Some(first) = lines.next() else {
            return Some(None);
        };
        if lines.next().is_some() {
            return None;
        }
        let v = first.to_str().ok()?.trim();
        if v.contains(',') {
            return None;
        }
        Some(Some(v.to_ascii_lowercase()))
    };
    let from_proxy = peer_of(request).is_some_and(|p| trusted.contains(&p));
    let host = if from_proxy {
        match single("x-forwarded-host") {
            None => return false,
            Some(Some(h)) => Some(h),
            Some(None) => single("host").flatten(),
        }
    } else {
        single("host").flatten()
    };
    host.is_some_and(|h| h.strip_suffix(":443").unwrap_or(&h) == base.authority)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_https_is_a_base() {
        assert!(Base::parse("http://notedown.example").is_none());
        assert!(Base::parse("").is_none());
        assert!(Base::parse("https://").is_none());
        assert!(
            Base::parse("https://a.example/x").is_none(),
            "a path is not supported"
        );
        let b = Base::parse("https://Notedown.Example/").unwrap();
        assert_eq!(b.origin, "https://notedown.example");
        assert_eq!(b.authority, "notedown.example");
        assert_eq!(b.resource(), "https://notedown.example/mcp");
        assert_eq!(
            b.metadata_url(),
            "https://notedown.example/.well-known/oauth-protected-resource/mcp"
        );
        let b = Base::parse("https://n.example:8443").unwrap();
        assert_eq!(b.authority, "n.example:8443");
    }

    #[test]
    fn the_host_must_match_and_forwarded_host_counts_only_from_a_trusted_proxy() {
        let b = Base::parse("https://n.example").unwrap();
        let proxy: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        let req = |host: &str, fwd: Option<&str>, peer: &str| {
            let mut r = axum::http::Request::builder()
                .uri("/mcp")
                .header("host", host);
            if let Some(f) = fwd {
                r = r.header("x-forwarded-host", f);
            }
            let mut r = r.body(axum::body::Body::empty()).unwrap();
            r.extensions_mut().insert(axum::extract::ConnectInfo(
                format!("{peer}:1234")
                    .parse::<std::net::SocketAddr>()
                    .unwrap(),
            ));
            r
        };
        assert!(host_ok(&b, &req("n.example", None, "1.2.3.4"), &[]));
        assert!(host_ok(&b, &req("N.EXAMPLE:443", None, "1.2.3.4"), &[]));
        assert!(!host_ok(&b, &req("evil.example", None, "1.2.3.4"), &[]));
        assert!(host_ok(
            &b,
            &req("10.0.0.2:19072", Some("n.example"), "10.0.0.1"),
            &[proxy]
        ));
        assert!(!host_ok(
            &b,
            &req("10.0.0.2:19072", Some("n.example"), "1.2.3.4"),
            &[proxy]
        ));
    }

    #[test]
    fn lists_repeated_lines_and_unreadable_values_are_refused() {
        let b = Base::parse("https://n.example").unwrap();
        let proxy: std::net::IpAddr = "10.0.0.1".parse().unwrap();
        let build = |headers: &[(&str, &[u8])], peer: &str| {
            let mut r = axum::http::Request::builder().uri("/mcp");
            for (k, v) in headers {
                r = r.header(*k, axum::http::HeaderValue::from_bytes(v).unwrap());
            }
            let mut r = r.body(axum::body::Body::empty()).unwrap();
            r.extensions_mut().insert(axum::extract::ConnectInfo(
                format!("{peer}:1234")
                    .parse::<std::net::SocketAddr>()
                    .unwrap(),
            ));
            r
        };
        let t = [proxy];
        assert!(host_ok(
            &b,
            &build(&[("x-forwarded-host", b"n.example")], "10.0.0.1"),
            &t
        ));
        let list = build(
            &[
                ("host", b"x"),
                ("x-forwarded-host", b"n.example, evil.example"),
            ],
            "10.0.0.1",
        );
        assert!(!host_ok(&b, &list, &t));
        assert!(!host_ok(
            &b,
            &build(&[("host", b"n.example, evil.example")], "1.2.3.4"),
            &[]
        ));
        let two = build(
            &[
                ("x-forwarded-host", b"n.example"),
                ("x-forwarded-host", b"evil.example"),
            ],
            "10.0.0.1",
        );
        assert!(!host_ok(&b, &two, &t));
        assert!(!host_ok(
            &b,
            &build(&[("host", b"n.example.")], "1.2.3.4"),
            &[]
        ));
        let bad = build(
            &[
                ("host", b"n.example"),
                ("x-forwarded-host", b"n.exampl\xe9"),
            ],
            "10.0.0.1",
        );
        assert!(!host_ok(&b, &bad, &t));
    }
}
