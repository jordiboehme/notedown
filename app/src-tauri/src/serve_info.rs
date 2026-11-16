//! What `notedown serve` says about itself when it starts: where it listens, the device URL,
//! the REST bases and the data folder. And the address the app uses to talk to it.

use std::net::SocketAddr;
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

/// The address the app reaches on this Mac: an unspecified or loopback one before another.
fn reachable(listen: &[SocketAddr]) -> Option<&SocketAddr> {
    listen
        .iter()
        .find(|a| a.ip().is_unspecified() || a.ip().is_loopback())
        .or_else(|| listen.first())
}

/// The base URL the app uses for its own server: always loopback, so macOS asks for no Local
/// Network access. The port is the one `serve` listens on, else `server_toml_port`.
pub fn local_rest_base(info: &ServeInfo, server_toml_port: u16) -> String {
    let port = reachable(&info.listen).map_or(server_toml_port, SocketAddr::port);
    format!("http://127.0.0.1:{port}/rest/v1")
}

#[derive(Deserialize)]
struct ServerToml {
    #[serde(default)]
    listen: Vec<SocketAddr>,
}

/// The port in `<data>/server.toml` `listen`, else [`DEFAULT_PORT`]. A missing or broken file
/// is the default, as `serve` would write it.
pub fn server_toml_port(data_dir: &Path) -> u16 {
    std::fs::read_to_string(data_dir.join("server.toml"))
        .ok()
        .and_then(|text| toml::from_str::<ServerToml>(&text).ok())
        .and_then(|t| reachable(&t.listen).map(SocketAddr::port))
        .unwrap_or(DEFAULT_PORT)
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

    #[test]
    fn the_app_talks_to_its_server_on_loopback() {
        let info = parse(&["listening on: 0.0.0.0:19072"]);
        assert_eq!(local_rest_base(&info, 1), "http://127.0.0.1:19072/rest/v1");
        let v6 = parse(&["listening on: [::]:19074"]);
        assert_eq!(local_rest_base(&v6, 1), "http://127.0.0.1:19074/rest/v1");
        let lo = parse(&["listening on: 127.0.0.1:19075"]);
        assert_eq!(local_rest_base(&lo, 1), "http://127.0.0.1:19075/rest/v1");
        // Several: the one loopback reaches wins over a LAN address.
        let both = parse(&[
            "listening on: 192.168.1.20:19076",
            "listening on: 0.0.0.0:19077",
        ]);
        assert_eq!(local_rest_base(&both, 1), "http://127.0.0.1:19077/rest/v1");
        // No line yet: the port from server.toml.
        assert_eq!(
            local_rest_base(&ServeInfo::default(), 19090),
            "http://127.0.0.1:19090/rest/v1"
        );
    }

    #[test]
    fn the_port_comes_from_server_toml_or_the_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(server_toml_port(dir.path()), 19072, "no file");

        let toml = dir.path().join("server.toml");
        std::fs::write(&toml, "protect = true\nlisten = [\"0.0.0.0:19100\"]\n").unwrap();
        assert_eq!(server_toml_port(dir.path()), 19100);

        std::fs::write(
            &toml,
            "listen = [\"192.168.1.20:19101\", \"127.0.0.1:19102\"]\n",
        )
        .unwrap();
        assert_eq!(server_toml_port(dir.path()), 19102, "loopback reaches it");

        std::fs::write(&toml, "listen = []\n").unwrap();
        assert_eq!(server_toml_port(dir.path()), 19072, "empty list");

        std::fs::write(&toml, "protect = true\n").unwrap();
        assert_eq!(server_toml_port(dir.path()), 19072, "no listen");

        std::fs::write(&toml, "listen = [nonsense\n").unwrap();
        assert_eq!(server_toml_port(dir.path()), 19072, "broken file");
    }
}
