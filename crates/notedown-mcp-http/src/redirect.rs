//! Redirect URI rules: https, or http on a loopback host; no fragment, no user info.

// Used from Task 4 on; removed in Task 10.
#![allow(dead_code)]

use crate::secret::percent_encode;

pub(crate) const MAX_URI_LEN: usize = 2048;

const LOOPBACK: [&str; 3] = ["127.0.0.1", "[::1]", "localhost"];

/// `scheme://authority` split off; `None` when there is no `://`.
fn split(uri: &str) -> Option<(&str, &str, &str)> {
    let (scheme, rest) = uri.split_once("://")?;
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    Some((scheme, &rest[..end], &rest[end..]))
}

/// The authority (`host[:port]`) of `uri`, or `""`.
pub(crate) fn host_of(uri: &str) -> &str {
    split(uri).map_or("", |(_, a, _)| a)
}

fn host_without_port(authority: &str) -> &str {
    if authority.starts_with('[') {
        return authority.split_inclusive(']').next().unwrap_or(authority);
    }
    authority.rsplit_once(':').map_or(authority, |(h, _)| h)
}

pub(crate) fn is_loopback(uri: &str) -> bool {
    let host = host_without_port(host_of(uri)).to_ascii_lowercase();
    LOOPBACK.contains(&host.as_str())
}

/// Why `uri` cannot be a redirect URI, or `None` when it can.
pub(crate) fn problem(uri: &str) -> Option<&'static str> {
    if uri.len() > MAX_URI_LEN {
        return Some("is longer than 2048 characters");
    }
    if uri.bytes().any(|b| b <= b' ' || b == 0x7f) {
        return Some("contains a space or a control character");
    }
    let Some((scheme, authority, _)) = split(uri) else {
        return Some("is not an absolute URL");
    };
    if uri.contains('#') {
        return Some("has a fragment");
    }
    if authority.is_empty() || host_without_port(authority).is_empty() {
        return Some("has no host");
    }
    if authority.contains('@') {
        return Some("has user info");
    }
    match scheme.to_ascii_lowercase().as_str() {
        "https" => None,
        "http" if is_loopback(uri) => None,
        "http" => Some("uses http on a host that is not a loopback address"),
        _ => Some("must use https"),
    }
}

/// `uri` with `params` appended to its query, each value percent-encoded.
pub(crate) fn with_params(uri: &str, params: &[(&str, &str)]) -> String {
    let mut out = uri.to_string();
    let mut sep = if uri.contains('?') { '&' } else { '?' };
    for (k, v) in params {
        out.push(sep);
        out.push_str(k);
        out.push('=');
        out.push_str(&percent_encode(v));
        sep = '&';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirect_uri_rules() {
        assert_eq!(problem("https://claude.ai/api/mcp/auth_callback"), None);
        assert_eq!(problem("http://127.0.0.1:33418/callback"), None);
        assert_eq!(problem("http://localhost/cb"), None);
        assert_eq!(problem("http://[::1]:9000/cb"), None);
        assert!(
            problem("http://claude.ai/cb").is_some(),
            "http only on loopback"
        );
        assert!(problem("https://claude.ai/cb#frag").is_some());
        assert!(problem("https://user@claude.ai/cb").is_some());
        assert!(problem("https:///cb").is_some());
        assert!(problem("javascript:alert(1)").is_some());
        assert!(problem(&format!("https://a.example/{}", "x".repeat(2048))).is_some());
        assert!(problem("https://a.example/ cb").is_some());
    }

    #[test]
    fn loopback_and_host() {
        assert!(is_loopback("http://127.0.0.1:1/x"));
        assert!(is_loopback("http://localhost:1/x"));
        assert!(!is_loopback("https://claude.ai/x"));
        assert_eq!(host_of("https://claude.ai:443/api?x=1"), "claude.ai:443");
    }

    #[test]
    fn params_are_appended_and_encoded() {
        assert_eq!(
            with_params("https://c.example/cb", &[("code", "a b"), ("state", "x&y")]),
            "https://c.example/cb?code=a%20b&state=x%26y"
        );
        assert_eq!(
            with_params("https://c.example/cb?k=1", &[("code", "z")]),
            "https://c.example/cb?k=1&code=z"
        );
    }
}
