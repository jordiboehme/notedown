//! What a pipeline needs to start: where the server's data is, where its own data lives and
//! which accounts it serves. `<local>/pipeline.toml` exists only on a second machine.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use notedown_cloud::auth::{Accounts, validate_slug};
use serde::Deserialize;

use crate::config::{PipelineConfig, Raw};
use crate::rest::RestSource;
use crate::source::{FolderSource, ServerAccount, ServerData};
use crate::store::{OWNER_FILE, PIPELINE_DIR};
use crate::{Error, Result};

/// The file name in the local folder.
pub const FILE: &str = "pipeline.toml";
/// The folder of the token files (`<slug>.token`) in the local folder.
pub const TOKENS_FOLDER: &str = "tokens";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    url: Option<String>,
    folder: Option<PathBuf>,
    accounts: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    server: Option<RawServer>,
    pipeline: Option<Raw>,
}

/// Where the server's data is read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerAt {
    /// The server's REST API (`http://host:port`, stored without a trailing slash).
    Url(String),
    /// The server's data folder, best mounted read-only. For tests; not documented.
    Folder(PathBuf),
}

/// `<local>/pipeline.toml`. Its existence means two machines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    pub server: ServerAt,
    /// The accounts to serve; `None`: every account the server lists.
    pub accounts: Option<Vec<String>>,
    pub pipeline: PipelineConfig,
}

impl LocalConfig {
    /// `None` when the file is missing. Reads only; nothing is created.
    pub fn load(local: &Path) -> Result<Option<LocalConfig>> {
        let path = local.join(FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        let bad = |message: String| Error::Config {
            path: path.clone(),
            message,
        };
        let file: RawFile = toml::from_str(&text).map_err(|e| bad(e.to_string()))?;
        let server = file.server.ok_or_else(|| {
            bad("the [server] table is missing; add [server] url = \"http://host:port\"".into())
        })?;
        let at = match (server.url, server.folder) {
            (Some(_), Some(_)) => {
                return Err(bad(
                    "[server] url and [server] folder are both set; keep only one".into(),
                ));
            }
            (Some(url), None) => {
                let url = url.trim().trim_end_matches('/').to_string();
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(bad(format!(
                        "[server] url {url:?} must start with http:// or https://"
                    )));
                }
                ServerAt::Url(url)
            }
            (None, Some(f)) if !f.as_os_str().is_empty() => ServerAt::Folder(f),
            (None, _) => {
                return Err(bad(
                    "[server] url is missing (or [server] folder, for a mounted folder)".into(),
                ));
            }
        };
        if let Some(list) = &server.accounts {
            for slug in list {
                validate_slug(slug).map_err(|e| bad(format!("[server] accounts: {slug}: {e}")))?;
            }
        }
        let pipeline = match file.pipeline {
            Some(raw) => PipelineConfig::from_raw(raw, &path)?,
            None => PipelineConfig::default(),
        };
        Ok(Some(LocalConfig {
            server: at,
            accounts: server.accounts,
            pipeline,
        }))
    }
}

/// The pipeline's view of both machines.
#[derive(Clone)]
pub struct Setup {
    /// The folder of the pipeline's own data: `pipeline/`, the cursors and the lock.
    pub local: PathBuf,
    pub source: Arc<dyn ServerData>,
    pub cfg: PipelineConfig,
    pub two_machines: bool,
    accounts: Option<Vec<String>>,
    /// The `--markdown-dir` folder given without an account.
    pub markdown_only: Option<PathBuf>,
}

impl Setup {
    /// With `<data>/pipeline.toml`: two machines, the server is `[server] folder`, the settings
    /// come from `[pipeline]`. Without: one machine, the server is `<data>` itself and the
    /// settings come from `server.toml` `[pipeline]`. Reads only; nothing is created.
    pub fn load(data: &Path) -> Result<Setup> {
        match LocalConfig::load(data)? {
            Some(c) => {
                let source: Arc<dyn ServerData> = match &c.server {
                    ServerAt::Folder(f) => Arc::new(FolderSource::shared(f)),
                    ServerAt::Url(url) => {
                        Arc::new(RestSource::new(url, &data.join(TOKENS_FOLDER), data)?)
                    }
                };
                Ok(Setup {
                    local: data.to_path_buf(),
                    source,
                    cfg: c.pipeline,
                    two_machines: true,
                    accounts: c.accounts,
                    markdown_only: None,
                })
            }
            None => Ok(Setup {
                local: data.to_path_buf(),
                source: Arc::new(FolderSource::new(data)),
                cfg: PipelineConfig::load(data)?,
                two_machines: false,
                accounts: None,
                markdown_only: None,
            }),
        }
    }

    /// The setup with the `--markdown-dir` flags ([`parse_markdown_flag`]): a flag with an
    /// account wins over `markdown_dirs` of the config for that account. A flag without one is
    /// for the only account served (see [`markdown_dir`](Self::markdown_dir)); it cannot be
    /// given twice or with flags that name an account, and no account can be named twice.
    pub fn with_markdown_dirs(mut self, flags: Vec<(Option<String>, PathBuf)>) -> Result<Setup> {
        let bad = |message: &str| Error::Invalid {
            what: "--markdown-dir",
            message: message.to_string(),
        };
        let unnamed = flags.iter().filter(|(s, _)| s.is_none()).count();
        if unnamed > 1 || (unnamed == 1 && flags.len() > 1) {
            return Err(bad(
                "a folder without an account is for the only account; with several, give \
                 SLUG=DIR for each",
            ));
        }
        let mut named = BTreeMap::new();
        for (slug, dir) in flags {
            match slug {
                None => self.markdown_only = Some(dir),
                Some(slug) => {
                    validate_slug(&slug).map_err(|e| bad(&format!("{slug}: {e}")))?;
                    if named.insert(slug.clone(), dir).is_some() {
                        return Err(bad(&format!("account {slug} is given twice")));
                    }
                }
            }
        }
        self.cfg.markdown_dirs.extend(named);
        Ok(self)
    }

    /// The Markdown folder of account `slug` when the worker serves `served` accounts: the
    /// flag without an account when exactly one is served, else the account's own folder.
    pub fn markdown_dir(&self, slug: &str, served: usize) -> Option<PathBuf> {
        match &self.markdown_only {
            Some(dir) if served == 1 => Some(dir.clone()),
            _ => self.cfg.markdown_dirs.get(slug).cloned(),
        }
    }

    /// An error when a `--markdown-dir` without an account meets more than one served account
    /// (`served`, their slugs); the text names them.
    pub fn check_markdown(&self, served: &[String]) -> Result<()> {
        if self.markdown_only.is_none() || served.len() <= 1 {
            return Ok(());
        }
        Err(Error::Invalid {
            what: "--markdown-dir",
            message: format!(
                "a folder without an account needs exactly one account, but this pipeline \
                 serves {}; give --markdown-dir SLUG=DIR for each account",
                served.join(", ")
            ),
        })
    }

    /// The accounts the worker serves: the configured list, else every account the server
    /// lists (with a URL: every account that has a token file). The only place this filter lives.
    pub fn served(&self) -> Result<Vec<ServerAccount>> {
        Ok(self.serve(&self.source.accounts()?))
    }

    /// The accounts of `all` (the server's list) that the worker serves.
    pub fn serve(&self, all: &[ServerAccount]) -> Vec<ServerAccount> {
        match &self.accounts {
            Some(list) => all
                .iter()
                .filter(|a| list.contains(&a.slug))
                .cloned()
                .collect(),
            None => all.to_vec(),
        }
    }
}

/// A `--markdown-dir` value: `SLUG=DIR` for one account, or `DIR` alone. The text before
/// the first `=` counts as an account only when it is a valid slug, so `/a=b` is a folder.
pub fn parse_markdown_flag(text: &str) -> (Option<String>, PathBuf) {
    if let Some((slug, dir)) = text.split_once('=')
        && validate_slug(slug).is_ok()
    {
        return (Some(slug.to_string()), PathBuf::from(dir));
    }
    (None, PathBuf::from(text))
}

/// Accounts for MCP and the pipeline subcommands, offline-safe.
/// One machine (no pipeline.toml): the account list of the data folder, as before.
/// Two machines: the configured list, else the stores under `<local>/pipeline` that have an
/// `owner.json`. The listing skips every name that is not a valid slug (`.writer.lock`,
/// `*.cursor`, `.removed-*`). Creates nothing.
pub fn local_accounts(local: &Path) -> Result<Vec<String>> {
    let Some(cfg) = LocalConfig::load(local)? else {
        return Ok(Accounts::new(local)
            .list()?
            .into_iter()
            .map(|a| a.slug)
            .collect());
    };
    if let Some(list) = cfg.accounts {
        return Ok(list);
    }
    local_stores(local, true)
}

/// The account stores under `<local>/pipeline`, sorted; with `owned`, only those that have an
/// `owner.json` (or one whose presence cannot be told: such a store is never left out). Every
/// name that is not a valid slug (`.writer.lock`, `*.cursor`, `.removed-*`) is skipped.
/// Creates nothing.
pub fn local_stores(local: &Path, owned: bool) -> Result<Vec<String>> {
    let dir = local.join(PIPELINE_DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(Error::Io { path: dir, source }),
    };
    let mut slugs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| Error::Io {
            path: dir.clone(),
            source,
        })?;
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let has_owner = || {
            !matches!(
                std::fs::symlink_metadata(entry.path().join(OWNER_FILE)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound
            )
        };
        if validate_slug(&name).is_ok() && entry.path().is_dir() && (!owned || has_owner()) {
            slugs.push(name);
        }
    }
    slugs.sort();
    Ok(slugs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, text: &str) {
        std::fs::write(dir.join(name), text).unwrap();
    }

    fn message(r: Result<Option<LocalConfig>>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn local_config_refuses_unknown_keys() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), FILE, "[server]\nfolder = \"x\"\nfoo = 1\n");
        let m = message(LocalConfig::load(d.path()));
        assert!(m.contains("pipeline.toml") && m.contains("foo"), "{m}");
    }

    #[test]
    fn local_config_needs_a_server_folder() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), FILE, "[server]\naccounts = [\"a\"]\n");
        let m = message(LocalConfig::load(d.path()));
        assert!(
            m.contains("pipeline.toml") && m.contains("[server] url"),
            "{m}"
        );
        write(d.path(), FILE, "[pipeline]\nocr_settle = \"1m\"\n");
        let m = message(LocalConfig::load(d.path()));
        assert!(m.contains("[server]") && m.contains("url"), "{m}");
    }

    #[test]
    fn local_config_reads_all_keys() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            FILE,
            "[server]\nfolder = \"/mnt/n\"\naccounts = [\"jordi\"]\n[pipeline]\nocr_settle = \"3m\"\n",
        );
        let c = LocalConfig::load(d.path()).unwrap().unwrap();
        assert_eq!(c.server, ServerAt::Folder(PathBuf::from("/mnt/n")));
        assert_eq!(c.accounts, Some(vec!["jordi".to_string()]));
        assert_eq!(c.pipeline.ocr_settle, std::time::Duration::from_secs(180));
        assert!(LocalConfig::load(&d.path().join("none")).unwrap().is_none());
    }

    #[test]
    fn without_pipeline_toml_the_setup_is_one_machine() {
        let d = tempfile::tempdir().unwrap();
        let s = Setup::load(d.path()).unwrap();
        assert!(!s.two_machines);
        assert_eq!(s.local, d.path());
        assert_eq!(s.source.describe(), FolderSource::new(d.path()).describe());
    }

    #[test]
    fn two_machines_load_creates_nothing_and_filters_served() {
        let d = tempfile::tempdir().unwrap();
        let srv = tempfile::tempdir().unwrap();
        write(
            d.path(),
            FILE,
            &format!("[server]\nfolder = {:?}\naccounts = [\"a\"]\n", srv.path()),
        );
        std::fs::create_dir(srv.path().join("accounts")).unwrap();
        let s = Setup::load(d.path()).unwrap();
        assert!(s.two_machines);
        assert_eq!(
            s.source.describe(),
            FolderSource::new(srv.path()).describe()
        );
        assert!(s.served().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(srv.path()).unwrap().count(), 1);
        assert!(
            srv.path()
                .join("accounts")
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
    }

    #[test]
    fn local_config_takes_a_url() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            FILE,
            "[server]\nurl = \"http://pi.local:19072\"\n",
        );
        let c = LocalConfig::load(d.path()).unwrap().unwrap();
        assert_eq!(c.server, ServerAt::Url("http://pi.local:19072".into()));
        assert_eq!(c.accounts, None);
    }

    #[test]
    fn local_config_refuses_url_and_folder_together() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            FILE,
            "[server]\nurl = \"http://pi:1\"\nfolder = \"/x\"\n",
        );
        let m = message(LocalConfig::load(d.path()));
        assert!(
            m.contains("pipeline.toml") && m.contains("url") && m.contains("folder"),
            "{m}"
        );
    }

    #[test]
    fn local_config_refuses_a_url_without_a_scheme() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), FILE, "[server]\nurl = \"pi.local:19072\"\n");
        let m = message(LocalConfig::load(d.path()));
        assert!(
            m.contains("pipeline.toml") && m.contains("http://") && m.contains("https://"),
            "{m}"
        );
    }

    #[test]
    fn a_trailing_slash_is_the_same_url() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), FILE, "[server]\nurl = \"http://pi:1///\"\n");
        let c = LocalConfig::load(d.path()).unwrap().unwrap();
        assert_eq!(c.server, ServerAt::Url("http://pi:1".into()));
    }

    /// A server that answers every request as the account `slug` and stops with the test.
    fn fake_server(slug: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                let body = format!("{{\"slug\":\"{slug}\",\"userId\":1000001}}");
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        url
    }

    #[test]
    fn served_accounts_come_from_the_token_files() {
        use base64::Engine;
        let d = tempfile::tempdir().unwrap();
        let url = fake_server("jordi");
        write(d.path(), FILE, &format!("[server]\nurl = \"{url}\"\n"));
        let s = Setup::load(d.path()).unwrap();
        assert!(s.two_machines);
        // No tokens folder yet: nothing is known, and nothing is created.
        assert!(
            s.served()
                .unwrap_err()
                .to_string()
                .contains("notedown connect")
        );
        assert!(!d.path().join(TOKENS_FOLDER).exists());
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"slug":"jordi","userId":1000001,"id":"0123abcd","nonce":"n"}"#);
        let tokens = d.path().join(TOKENS_FOLDER);
        std::fs::create_dir(&tokens).unwrap();
        write(&tokens, "jordi.token", &format!("ndt1.{payload}.sig\n"));
        let served: Vec<String> = s.served().unwrap().into_iter().map(|a| a.slug).collect();
        assert_eq!(served, vec!["jordi"]);
        // A configured list narrows it.
        write(
            d.path(),
            FILE,
            &format!("[server]\nurl = \"{url}\"\naccounts = [\"other\"]\n"),
        );
        assert!(Setup::load(d.path()).unwrap().served().unwrap().is_empty());
    }

    fn account(data: &Path, slug: &str) {
        Accounts::new(data)
            .create(slug, &format!("{slug}@example.org"), "secret")
            .unwrap();
    }

    #[test]
    fn one_machine_lists_the_accounts_of_the_folder() {
        let d = tempfile::tempdir().unwrap();
        account(d.path(), "a");
        account(d.path(), "b");
        std::fs::create_dir_all(d.path().join(PIPELINE_DIR).join("a")).unwrap();
        assert_eq!(local_accounts(d.path()).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn two_machines_list_owned_stores_offline() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            FILE,
            "[server]\nfolder = \"/nonexistent/mount\"\n",
        );
        let p = d.path().join(PIPELINE_DIR);
        for s in ["a", "b"] {
            std::fs::create_dir_all(p.join(s)).unwrap();
        }
        write(&p, "a/owner.json", "{}");
        write(&p, ".writer.lock", "");
        write(&p, "a.cursor", "1");
        std::fs::create_dir_all(p.join(".removed-b")).unwrap();
        assert_eq!(local_accounts(d.path()).unwrap(), vec!["a"]);
    }

    #[test]
    fn two_machines_prefer_the_configured_list() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            FILE,
            "[server]\nfolder = \"/x\"\naccounts = [\"z\", \"y\"]\n",
        );
        assert_eq!(local_accounts(d.path()).unwrap(), vec!["z", "y"]);
    }

    #[test]
    fn a_typo_account_creates_no_folder() {
        let d = tempfile::tempdir().unwrap();
        account(d.path(), "a");
        let r = crate::mcp::resolve_account(d.path(), Some("typo"));
        assert!(r.is_err());
        assert!(!d.path().join(PIPELINE_DIR).join("typo").exists());
    }
}
