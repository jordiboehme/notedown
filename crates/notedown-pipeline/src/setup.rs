//! What a pipeline needs to start: where the server's data is, where its own data lives and
//! which accounts it serves. `<local>/pipeline.toml` exists only on a second machine.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use notedown_cloud::auth::{Accounts, validate_slug};
use serde::Deserialize;

use crate::config::{PipelineConfig, Raw};
use crate::source::{FolderSource, ServerAccount, ServerData};
use crate::store::{OWNER_FILE, PIPELINE_DIR};
use crate::{Error, Result};

/// The file name in the local folder.
pub const FILE: &str = "pipeline.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    folder: Option<PathBuf>,
    accounts: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    server: Option<RawServer>,
    pipeline: Option<Raw>,
}

/// `<local>/pipeline.toml`. Its existence means two machines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalConfig {
    /// The server's data folder, best mounted read-only.
    pub server_folder: PathBuf,
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
            bad("the [server] table with a folder is missing; add [server] folder = \"...\"".into())
        })?;
        let server_folder = match server.folder {
            Some(f) if !f.as_os_str().is_empty() => f,
            _ => return Err(bad("[server] folder is missing".into())),
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
            server_folder,
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
}

impl Setup {
    /// With `<data>/pipeline.toml`: two machines, the server is `[server] folder`, the settings
    /// come from `[pipeline]`. Without: one machine, the server is `<data>` itself and the
    /// settings come from `server.toml` `[pipeline]`. Reads only; nothing is created.
    pub fn load(data: &Path) -> Result<Setup> {
        match LocalConfig::load(data)? {
            Some(c) => Ok(Setup {
                local: data.to_path_buf(),
                source: Arc::new(FolderSource::new(&c.server_folder)),
                cfg: c.pipeline,
                two_machines: true,
                accounts: c.accounts,
            }),
            None => Ok(Setup {
                local: data.to_path_buf(),
                source: Arc::new(FolderSource::new(data)),
                cfg: PipelineConfig::load(data)?,
                two_machines: false,
                accounts: None,
            }),
        }
    }

    /// The accounts the worker serves: the configured list, else every account the server
    /// lists. The only place this filter lives.
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
            m.contains("pipeline.toml") && m.contains("[server] folder"),
            "{m}"
        );
        write(d.path(), FILE, "[pipeline]\nocr_settle = \"1m\"\n");
        let m = message(LocalConfig::load(d.path()));
        assert!(m.contains("[server]") && m.contains("folder"), "{m}");
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
        assert_eq!(c.server_folder, PathBuf::from("/mnt/n"));
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
        let s = Setup::load(d.path()).unwrap();
        assert!(s.two_machines);
        assert_eq!(
            s.source.describe(),
            FolderSource::new(srv.path()).describe()
        );
        assert!(s.served().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
        assert_eq!(std::fs::read_dir(srv.path()).unwrap().count(), 0);
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
