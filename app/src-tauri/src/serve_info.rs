//! What `notedown serve` says about itself when it starts: where it listens, the device URL,
//! the REST bases and the data folder. And the address the app uses to talk to it.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The server's port when `server.toml` names none (as `notedown_cloud::Config::default`).
pub const DEFAULT_PORT: u16 = 19072;

/// The first line `serve` prints; a new run starts a new [`ServeInfo`].
pub const HEADER: &str = "notedown cloud server";

/// The start lines of one `serve` run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServeInfo {
    pub listen: Vec<SocketAddr>,
    /// The addresses for the Supernote; empty when `serve` found none.
    pub device_urls: Vec<String>,
    /// The REST bases on the network (with `/rest/v1`), for display only.
    pub rest_bases: Vec<String>,
    pub data_dir: Option<PathBuf>,
}

/// Takes what `line` says into `info`. The header line starts over; other lines change
/// nothing. `none found` for the device URL or the REST API adds nothing.
pub fn parse_line(info: &mut ServeInfo, line: &str) {
    let line = line.trim_end();
    if line == HEADER {
        *info = ServeInfo::default();
    } else if let Some(addr) = line.strip_prefix("listening on: ") {
        if let Ok(addr) = addr.trim().parse() {
            info.listen.push(addr);
        }
    } else if let Some(url) = line.strip_prefix("device URL: ") {
        if let Some(url) = address(url) {
            info.device_urls.push(url);
        }
    } else if let Some(base) = line.strip_prefix("REST API: ") {
        if let Some(base) = address(base) {
            info.rest_bases.push(base);
        }
    } else if let Some(dir) = line.strip_prefix("data folder: ") {
        info.data_dir = Some(PathBuf::from(dir));
    }
}

/// The URL at the start of `rest`; none for `none found, ...`. The REST line goes on with
/// `(connect a pipeline with ...)`.
fn address(rest: &str) -> Option<String> {
    let url = rest.split_whitespace().next()?;
    url.starts_with("http").then(|| url.to_string())
}

/// The address the app talks to for a server that listens on `listen`:
/// - an unspecified address (`0.0.0.0`, `[::]`) or an IPv4 loopback: `127.0.0.1`;
/// - else an IPv6 loopback: `[::1]`;
/// - else the first address, as it is the only one that answers.
pub fn reachable_addr(listen: &[SocketAddr]) -> Option<SocketAddr> {
    let v4_loopback = |port| SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if let Some(a) = listen
        .iter()
        .find(|a| a.ip().is_unspecified() || (a.is_ipv4() && a.ip().is_loopback()))
    {
        return Some(v4_loopback(a.port()));
    }
    if let Some(a) = listen.iter().find(|a| a.ip().is_loopback()) {
        return Some(SocketAddr::from((Ipv6Addr::LOCALHOST, a.port())));
    }
    listen.first().copied()
}

/// The base URL the app uses for its own server: on loopback where it can, so macOS asks for
/// no Local Network access. The address is the one `serve` listens on, else `server_toml`
/// ([`server_toml_addr`]).
pub fn local_rest_base(info: &ServeInfo, server_toml: SocketAddr) -> String {
    let addr = reachable_addr(&info.listen).unwrap_or(server_toml);
    format!("http://{addr}/rest/v1")
}

#[derive(Deserialize)]
struct ServerToml {
    #[serde(default)]
    listen: Vec<SocketAddr>,
}

/// The address in `<data>/server.toml` `listen` the app talks to ([`reachable_addr`]), else
/// `127.0.0.1` with [`DEFAULT_PORT`]. A missing or broken file is the default, as `serve`
/// would write it.
pub fn server_toml_addr(data_dir: &Path) -> SocketAddr {
    std::fs::read_to_string(data_dir.join("server.toml"))
        .ok()
        .and_then(|text| toml::from_str::<ServerToml>(&text).ok())
        .and_then(|t| reachable_addr(&t.listen))
        .unwrap_or(SocketAddr::from((Ipv4Addr::LOCALHOST, DEFAULT_PORT)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(lines: &[&str]) -> ServeInfo {
        let mut info = ServeInfo::default();
        for line in lines {
            parse_line(&mut info, line);
        }
        info
    }

    #[test]
    fn serve_lines_give_addresses() {
        let info = parse(&[
            "notedown cloud server",
            "listening on: 0.0.0.0:19072",
            "listening on: [::]:19073",
            "device URL: http://192.168.1.20:19072",
            "device URL: http://10.0.0.5:19072",
            "REST API: http://192.168.1.20:19072/rest/v1 (connect a pipeline with notedown \
             connect http://192.168.1.20:19072)",
            "REST API: http://10.0.0.5:19072/rest/v1 (connect a pipeline with notedown connect \
             http://10.0.0.5:19072)",
            "data folder: /Users/me/notedown data",
            "log folder: /Users/me/notedown data/logs",
            "GET /rest/v1/account 401",
        ]);
        assert_eq!(
            info.listen,
            vec![
                "0.0.0.0:19072".parse().unwrap(),
                "[::]:19073".parse().unwrap()
            ]
        );
        assert_eq!(
            info.device_urls,
            vec!["http://192.168.1.20:19072", "http://10.0.0.5:19072"]
        );
        assert_eq!(
            info.rest_bases,
            vec![
                "http://192.168.1.20:19072/rest/v1",
                "http://10.0.0.5:19072/rest/v1"
            ]
        );
        assert_eq!(
            info.data_dir,
            Some(PathBuf::from("/Users/me/notedown data"))
        );
    }

    #[test]
    fn serve_lines_without_an_address() {
        let info = parse(&[
            "notedown cloud server",
            "listening on: 127.0.0.1:19072",
            "device URL: none found, use the IP address of this host",
            "REST API: none found, use the IP address of this host",
            "data folder: /d",
        ]);
        assert_eq!(info.listen, vec!["127.0.0.1:19072".parse().unwrap()]);
        assert!(info.device_urls.is_empty());
        assert!(info.rest_bases.is_empty());
        assert_eq!(info.data_dir, Some(PathBuf::from("/d")));
    }

    #[test]
    fn a_new_run_starts_over() {
        let info = parse(&[
            "notedown cloud server",
            "listening on: 0.0.0.0:19072",
            "device URL: http://192.168.1.20:19072",
            "notedown cloud server",
            "listening on: 0.0.0.0:19080",
        ]);
        assert_eq!(info.listen, vec!["0.0.0.0:19080".parse().unwrap()]);
        assert!(info.device_urls.is_empty());
    }

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_app_talks_to_its_server_on_loopback() {
        let fallback = addr("127.0.0.1:1");
        let info = parse(&["listening on: 0.0.0.0:19072"]);
        assert_eq!(
            local_rest_base(&info, fallback),
            "http://127.0.0.1:19072/rest/v1"
        );
        let v6 = parse(&["listening on: [::]:19074"]);
        assert_eq!(
            local_rest_base(&v6, fallback),
            "http://127.0.0.1:19074/rest/v1"
        );
        let lo = parse(&["listening on: 127.0.0.1:19075"]);
        assert_eq!(
            local_rest_base(&lo, fallback),
            "http://127.0.0.1:19075/rest/v1"
        );
        // Several: the one loopback reaches wins over a LAN address.
        let both = parse(&[
            "listening on: 192.168.1.20:19076",
            "listening on: 0.0.0.0:19077",
        ]);
        assert_eq!(
            local_rest_base(&both, fallback),
            "http://127.0.0.1:19077/rest/v1"
        );
        // No line yet: the address from server.toml.
        assert_eq!(
            local_rest_base(&ServeInfo::default(), addr("127.0.0.1:19090")),
            "http://127.0.0.1:19090/rest/v1"
        );
    }

    #[test]
    fn ipv6_loopback_alone_is_used_as_it_is() {
        let info = parse(&["listening on: [::1]:19078"]);
        assert_eq!(
            local_rest_base(&info, addr("127.0.0.1:1")),
            "http://[::1]:19078/rest/v1"
        );
        // Next to an IPv4 loopback or an unspecified address, 127.0.0.1 wins.
        let both = parse(&["listening on: [::1]:19079", "listening on: 0.0.0.0:19079"]);
        assert_eq!(
            local_rest_base(&both, addr("127.0.0.1:1")),
            "http://127.0.0.1:19079/rest/v1"
        );
        assert_eq!(
            reachable_addr(&[addr("[::]:19080")]),
            Some(addr("127.0.0.1:19080"))
        );
    }

    #[test]
    fn a_lan_only_server_is_reached_on_its_address() {
        let info = parse(&[
            "listening on: 192.168.1.20:19081",
            "listening on: 10.0.0.5:19082",
        ]);
        assert_eq!(
            local_rest_base(&info, addr("127.0.0.1:1")),
            "http://192.168.1.20:19081/rest/v1"
        );
        assert_eq!(
            local_rest_base(&ServeInfo::default(), addr("192.168.1.20:19083")),
            "http://192.168.1.20:19083/rest/v1"
        );
    }

    #[test]
    fn the_port_comes_from_server_toml_or_the_default() {
        let dir = tempfile::tempdir().unwrap();
        let at = |d: &Path| server_toml_addr(d);
        assert_eq!(at(dir.path()), addr("127.0.0.1:19072"), "no file");

        let toml = dir.path().join("server.toml");
        std::fs::write(&toml, "protect = true\nlisten = [\"0.0.0.0:19100\"]\n").unwrap();
        assert_eq!(at(dir.path()), addr("127.0.0.1:19100"));

        std::fs::write(
            &toml,
            "listen = [\"192.168.1.20:19101\", \"127.0.0.1:19102\"]\n",
        )
        .unwrap();
        assert_eq!(
            at(dir.path()),
            addr("127.0.0.1:19102"),
            "loopback reaches it"
        );

        std::fs::write(&toml, "listen = [\"[::1]:19103\"]\n").unwrap();
        assert_eq!(at(dir.path()), addr("[::1]:19103"), "IPv6 loopback only");

        std::fs::write(&toml, "listen = [\"192.168.1.20:19104\"]\n").unwrap();
        assert_eq!(at(dir.path()), addr("192.168.1.20:19104"), "LAN only");

        std::fs::write(&toml, "listen = []\n").unwrap();
        assert_eq!(at(dir.path()), addr("127.0.0.1:19072"), "empty list");

        std::fs::write(&toml, "protect = true\n").unwrap();
        assert_eq!(at(dir.path()), addr("127.0.0.1:19072"), "no listen");

        std::fs::write(&toml, "listen = [nonsense\n").unwrap();
        assert_eq!(at(dir.path()), addr("127.0.0.1:19072"), "broken file");
    }
}
