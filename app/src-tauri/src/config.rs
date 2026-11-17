//! The app config: a small JSON file in the app config dir.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const FILE: &str = "config.json";
const BROKEN: &str = "config.json.broken";
const TEMP: &str = "config.json.tmp";

/// Where the notes live and which server the app talks to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    /// Nothing chosen yet: no child runs.
    #[default]
    NotSet,
    /// The server runs on this Mac, started by the app.
    Local,
    /// The server runs on another machine.
    Remote,
}

/// How the library shows folders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StartView {
    #[default]
    Tree,
    Columns,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AppConfig {
    /// The file format, 1 for now. Nothing checks it yet; a later format change will.
    pub version: u32,
    pub mode: Mode,
    pub data_dir: PathBuf,
    pub server_url: Option<String>,
    pub account: Option<String>,
    pub email: Option<String>,
    pub start_view: StartView,
    /// The Markdown folder of each account, by its slug. An account without an entry uses
    /// [`default_markdown_dir`].
    pub markdown_dirs: BTreeMap<String, PathBuf>,
    /// What the setup checklist keeps between two starts.
    pub setup: SetupRecord,
    /// When the updater last got an answer from GitHub, in ms since 1970 (this Mac's clock).
    /// It checks again 24 hours later ([`crate::update::should_check`]).
    pub last_update_check_ms: Option<u64>,
    /// The newest version a found update opened Settings on About for. The daily check opens
    /// it again only for a newer one ([`crate::update::should_show`]).
    pub last_update_shown: Option<String>,
}

/// What the setup checklist keeps: the backup and import choices, what was shown or offered
/// once, and the first sync after the import.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SetupRecord {
    /// The address of the Supernote's Browse & Access page the backup reads from.
    pub backup_url: Option<String>,
    /// The folder of the device backup. Cleared once both imports are done.
    pub backup_dir: Option<PathBuf>,
    /// The device's own `.snbak` backup for the app data. Cleared once both imports are done.
    pub snbak: Option<PathBuf>,
    /// Settings opened at Setup once at the first start.
    pub setup_shown: bool,
    /// The first Local mode turned Start at login on once.
    pub autostart_offered: bool,
    /// The journal end when the import was done, for the check of the first sync.
    pub sync_mark: Option<SyncMark>,
    /// What the first sync after the mark deleted (filled by the first sync check).
    pub sync_checked: Option<SyncCheck>,
    /// The user saw the deletes of the first sync.
    pub sync_acknowledged: bool,
}

impl SetupRecord {
    /// Forgets the first sync of an account: another server or another account has its own.
    pub fn forget_sync(&mut self) {
        self.sync_mark = None;
        self.sync_checked = None;
        self.sync_acknowledged = false;
    }
}

/// What the settings window may change of the [`SetupRecord`]. A field that is left out stays
/// as it is; `null` clears it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
pub struct SetupChange {
    #[serde(deserialize_with = "present")]
    pub backup_url: Option<Option<String>>,
    #[serde(deserialize_with = "present")]
    pub backup_dir: Option<Option<PathBuf>>,
    #[serde(deserialize_with = "present")]
    pub snbak: Option<Option<PathBuf>>,
    pub sync_acknowledged: Option<bool>,
}

/// A field that is there, `null` or not: only a missing one is `None` (by `default`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

impl SetupChange {
    pub fn apply(self, record: &mut SetupRecord) {
        if let Some(url) = self.backup_url {
            record.backup_url = url.map(|u| u.trim().to_owned()).filter(|u| !u.is_empty());
        }
        if let Some(dir) = self.backup_dir {
            record.backup_dir = dir;
        }
        if let Some(snbak) = self.snbak {
            record.snbak = snbak;
        }
        if let Some(seen) = self.sync_acknowledged {
            record.sync_acknowledged = seen;
        }
    }
}

/// The journal end when the import of an account was done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncMark {
    /// The account's token key ([`crate::token::token_key`]): the server and the account.
    pub key: String,
    /// The highest journal number at the mark.
    pub seq: u64,
    /// When the mark was taken, by this Mac's clock, in ms since 1970. Shown only; the first
    /// sync is found by the server's times ([`sync_at_mark`](Self::sync_at_mark)). A mark saved
    /// before this field has 0.
    #[serde(default)]
    pub at_ms: u64,
    /// The newest device `lastSyncMs` of the account when the mark was taken (the server's
    /// clock), `None` when no device had synced. The first sync after the mark is a device sync
    /// with a greater `lastSyncMs`: the Mac's clock is never held against the server's.
    #[serde(default)]
    pub sync_at_mark: Option<u64>,
}

/// What the first sync after the mark deleted on the server.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SyncCheck {
    pub files: u64,
    pub folders: u64,
    /// `None` where digests cannot be checked (a server on another machine).
    pub digests: Option<u64>,
    /// The journal had a gap: what the first sync changed is not known.
    pub cannot_tell: bool,
}

/// `~/notedown-data`, or `./notedown-data` when there is no home folder.
pub fn default_data_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("notedown-data")
}

/// `~/Documents/Markdown`: the Markdown folder of an account without one of its own.
pub fn default_markdown_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Documents")
        .join("Markdown")
}

impl AppConfig {
    /// The logged-in account's slug; `None` without one.
    pub fn account_slug(&self) -> Option<&str> {
        self.account.as_deref().filter(|a| !a.is_empty())
    }

    /// The Markdown folder of the logged-in account: its entry in `markdown_dirs`, else
    /// [`default_markdown_dir`]. `None` without an account.
    pub fn markdown_dir(&self) -> Option<PathBuf> {
        let slug = self.account_slug()?;
        Some(
            self.markdown_dirs
                .get(slug)
                .cloned()
                .unwrap_or_else(default_markdown_dir),
        )
    }
}

impl AppConfig {
    /// Gives the logged-in account the Markdown folder the pipeline's own config names for
    /// it, when the app has none for it yet ([`pipeline_markdown_dir`]). Answers whether it
    /// added one. Without one the account keeps [`default_markdown_dir`] and nothing is added.
    pub fn seed_markdown_dir(&mut self) -> bool {
        let Some(slug) = self.account_slug() else {
            return false;
        };
        if self.markdown_dirs.contains_key(slug) {
            return false;
        }
        let Some(dir) = pipeline_markdown_dir(&self.data_dir, slug) else {
            return false;
        };
        let slug = slug.to_owned();
        self.markdown_dirs.insert(slug, dir);
        true
    }
}

/// The `[pipeline]` table of the file the pipeline reads, as far as the app needs it. Other
/// keys are the pipeline's (or the server's) and are not checked here.
#[derive(Deserialize, Default)]
struct PipelineFile {
    #[serde(default)]
    pipeline: Option<PipelineTable>,
}

#[derive(Deserialize, Default)]
struct PipelineTable {
    #[serde(default)]
    markdown_dirs: BTreeMap<String, PathBuf>,
}

/// The Markdown folder that the pipeline's own config names for `slug`: `markdown_dirs` of
/// `[pipeline]` in `<data>/pipeline.toml` when that file is there (two machines), else in
/// `<data>/server.toml` (one machine), as the pipeline chooses. A leading `~` is the home
/// folder (the pipeline itself does not expand it) and a relative folder is below the data
/// folder. `None` for no file, no entry, an empty one or a file that cannot be read.
pub fn pipeline_markdown_dir(data_dir: &Path, slug: &str) -> Option<PathBuf> {
    let pipeline = data_dir.join("pipeline.toml");
    let file = if pipeline.exists() {
        pipeline
    } else {
        data_dir.join("server.toml")
    };
    let text = fs::read_to_string(file).ok()?;
    let parsed: PipelineFile = toml::from_str(&text).ok()?;
    let dir = parsed.pipeline?.markdown_dirs.remove(slug)?;
    let text = dir.to_string_lossy();
    if text.trim().is_empty() {
        return None;
    }
    let dir = if text == "~" || text.starts_with("~/") {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        home.join(text.trim_start_matches('~').trim_start_matches('/'))
    } else if dir.is_relative() {
        data_dir.join(&dir)
    } else {
        dir.clone()
    };
    // `home.join("")` leaves a trailing slash; components drop it.
    Some(dir.components().collect())
}

/// `pipeline.toml` as far as the setup reads it: the server it connects to.
#[derive(Deserialize)]
struct PipelineServerFile {
    #[serde(default)]
    server: Option<PipelineServerTable>,
}

#[derive(Deserialize)]
struct PipelineServerTable {
    #[serde(default)]
    url: Option<String>,
}

/// The server address that `<data>/pipeline.toml` names in `[server] url` (written by
/// `notedown connect`); `None` without the file, a readable file or the key.
pub fn pipeline_server_url(data_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(data_dir.join("pipeline.toml")).ok()?;
    let parsed: PipelineServerFile = toml::from_str(&text).ok()?;
    parsed
        .server?
        .url
        .map(|url| url.trim().to_owned())
        .filter(|url| !url.is_empty())
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            version: 1,
            mode: Mode::NotSet,
            data_dir: default_data_dir(),
            server_url: None,
            account: None,
            email: None,
            start_view: StartView::Tree,
            markdown_dirs: BTreeMap::new(),
            setup: SetupRecord::default(),
            last_update_check_ms: None,
            last_update_shown: None,
        }
    }
}

/// What a data folder already holds, so the setup can adopt it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExistingSetup {
    /// `server.toml` or `accounts/` is there.
    pub server: bool,
    /// `pipeline.toml` is there.
    pub pipeline: bool,
}

pub fn existing_setup(dir: &Path) -> ExistingSetup {
    ExistingSetup {
        server: dir.join("server.toml").is_file() || dir.join("accounts").is_dir(),
        pipeline: dir.join("pipeline.toml").is_file(),
    }
}

/// What `load_checked` found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loaded {
    pub config: AppConfig,
    /// The file was unreadable as a config and was kept aside as `config.json.broken`.
    pub reset: bool,
}

pub fn load(dir: &Path) -> AppConfig {
    load_checked(dir).config
}

/// Reads `<dir>/config.json`. A missing file gives the defaults. A file that is not valid UTF-8
/// or not a valid config is renamed to `config.json.broken` (an older one is replaced, so the
/// newest wins), a line goes to stderr, and `reset` is set. Any other read error (permissions, a
/// failing disk) gives the defaults and leaves the file alone.
pub fn load_checked(dir: &Path) -> Loaded {
    let path = dir.join(FILE);
    let parsed = match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => return not_reset(),
        Err(e) if e.kind() == io::ErrorKind::InvalidData => Err(e.to_string()),
        Err(e) => {
            eprintln!(
                "notedown: the config {} could not be read, using defaults: {e}",
                path.display()
            );
            return not_reset();
        }
    };
    match parsed {
        Ok(config) => Loaded {
            config,
            reset: false,
        },
        Err(why) => {
            // Best effort: if the rename fails, the next save replaces the file anyway.
            let _ = fs::rename(&path, dir.join(BROKEN));
            eprintln!(
                "notedown: the config {} is broken ({why}); kept as {BROKEN}, using defaults",
                path.display()
            );
            Loaded {
                config: AppConfig::default(),
                reset: true,
            }
        }
    }
}

fn not_reset() -> Loaded {
    Loaded {
        config: AppConfig::default(),
        reset: false,
    }
}

/// Writes `<dir>/config.json` durably: the new content goes to a temp file that is flushed to
/// disk (`sync_all`, a full fsync on macOS), then renamed over the old file, then the folder is
/// flushed too. A crash leaves the old file or the new one, never half of one. A failed flush
/// of the folder after the rename is logged, not an error.
pub fn save(dir: &Path, config: &AppConfig) -> io::Result<()> {
    use std::io::Write;
    fs::create_dir_all(dir)?;
    let temp = dir.join(TEMP);
    let json = serde_json::to_string_pretty(config).map_err(io::Error::other)?;
    let written = (|| {
        let mut file = fs::File::create(&temp)?;
        file.write_all(json.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, dir.join(FILE))
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    sync_folder(dir);
    Ok(())
}

/// Flushes the rename to disk. The new file is in place already, so a failure here is only
/// logged: the save worked, it may just not survive a power cut.
fn sync_folder(dir: &Path) {
    if let Err(e) = fs::File::open(dir).and_then(|folder| folder.sync_all()) {
        eprintln!(
            "notedown: the config folder {} could not be flushed: {e}",
            dir.display()
        );
    }
}

/// The address without a user or a password in it, for a status line or the Keychain key.
/// Everything else stays as written (no parser that changes the case or adds a slash), so an
/// address without a login comes back unchanged.
pub fn without_userinfo(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    match authority.rsplit_once('@') {
        Some((_, host)) => format!("{scheme}://{host}{tail}"),
        None => url.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> AppConfig {
        AppConfig {
            version: 1,
            mode: Mode::Remote,
            data_dir: PathBuf::from("/data/notes"),
            server_url: Some("https://notes.example.org".into()),
            account: Some("jordi".into()),
            email: Some("jordi@example.org".into()),
            start_view: StartView::Columns,
            markdown_dirs: [("jordi".to_owned(), PathBuf::from("/notes/md"))].into(),
            setup: SetupRecord {
                backup_url: Some("http://192.168.1.20:8089".into()),
                backup_dir: Some(PathBuf::from("/Users/me/supernote-backup-2026-11-17")),
                snbak: Some(PathBuf::from("/Users/me/Supernote_1.snbak")),
                setup_shown: true,
                autostart_offered: true,
                sync_mark: Some(SyncMark {
                    key: "https://notes.example.org|jordi".into(),
                    seq: 813,
                    at_ms: 1_763_300_000_000,
                    sync_at_mark: Some(1_763_299_000_000),
                }),
                sync_checked: Some(SyncCheck {
                    files: 2,
                    folders: 1,
                    digests: None,
                    cannot_tell: false,
                }),
                sync_acknowledged: true,
            },
            last_update_check_ms: Some(1_763_300_000_000),
            last_update_shown: Some("0.2.0".into()),
        }
    }

    #[test]
    fn the_last_update_check_round_trips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = AppConfig {
            last_update_check_ms: Some(1_763_400_000_000),
            ..AppConfig::default()
        };
        save(dir.path(), &config).unwrap();
        assert_eq!(
            load(dir.path()).last_update_check_ms,
            Some(1_763_400_000_000)
        );
        let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains("\"lastUpdateCheckMs\""), "{text}");
        // A config of an earlier version never checked.
        fs::write(dir.path().join(FILE), r#"{"mode":"local"}"#).unwrap();
        assert_eq!(load(dir.path()).last_update_check_ms, None);
        assert_eq!(AppConfig::default().last_update_check_ms, None);
        assert_eq!(AppConfig::default().last_update_shown, None);
        let config = AppConfig {
            last_update_shown: Some("0.2.0".into()),
            ..AppConfig::default()
        };
        save(dir.path(), &config).unwrap();
        let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
        assert!(text.contains("\"lastUpdateShown\""), "{text}");
        assert_eq!(load(dir.path()).last_update_shown.as_deref(), Some("0.2.0"));
    }

    #[test]
    fn the_setup_record_round_trips_and_defaults() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &sample()).unwrap();
        assert_eq!(load(dir.path()).setup, sample().setup);
        // The file uses the names the settings window reads.
        let text = fs::read_to_string(dir.path().join(FILE)).unwrap();
        for key in [
            "\"backupUrl\"",
            "\"backupDir\"",
            "\"setupShown\"",
            "\"autostartOffered\"",
            "\"syncMark\"",
            "\"atMs\"",
            "\"syncAtMark\"",
            "\"syncChecked\"",
            "\"cannotTell\"",
            "\"syncAcknowledged\"",
        ] {
            assert!(text.contains(key), "{key} in {text}");
        }
        // A config of an earlier version has no record: all off and empty.
        fs::write(dir.path().join(FILE), r#"{"mode":"local"}"#).unwrap();
        assert_eq!(load(dir.path()).setup, SetupRecord::default());
        // A record with only some fields keeps the others at their defaults.
        fs::write(
            dir.path().join(FILE),
            r#"{"mode":"local","setup":{"setupShown":true}}"#,
        )
        .unwrap();
        let setup = load(dir.path()).setup;
        assert!(setup.setup_shown);
        assert!(!setup.autostart_offered);
        assert_eq!(setup.sync_mark, None);
        // A mark saved before it had its time keeps the config and gets 0.
        fs::write(
            dir.path().join(FILE),
            r#"{"mode":"local","setup":{"syncMark":{"key":"local|me","seq":3}}}"#,
        )
        .unwrap();
        let loaded = load_checked(dir.path());
        assert!(!loaded.reset);
        assert_eq!(
            loaded.config.setup.sync_mark,
            Some(SyncMark {
                key: "local|me".into(),
                seq: 3,
                at_ms: 0,
                sync_at_mark: None,
            })
        );
        let mut record = sample().setup;
        record.forget_sync();
        assert_eq!(
            (
                record.sync_mark,
                record.sync_checked,
                record.sync_acknowledged
            ),
            (None, None, false)
        );
        assert!(record.backup_dir.is_some());
    }

    #[test]
    fn an_account_without_an_entry_writes_to_documents_markdown() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let mut config = AppConfig {
            account: Some("jordi".into()),
            ..AppConfig::default()
        };
        assert_eq!(
            default_markdown_dir(),
            home.join("Documents").join("Markdown")
        );
        assert_eq!(
            config.markdown_dir(),
            Some(home.join("Documents").join("Markdown"))
        );
        // An entry of another account changes nothing for this one.
        config
            .markdown_dirs
            .insert("other".into(), PathBuf::from("/elsewhere"));
        assert_eq!(config.markdown_dir(), Some(default_markdown_dir()));
        config
            .markdown_dirs
            .insert("jordi".into(), PathBuf::from("/notes/md"));
        assert_eq!(config.markdown_dir(), Some(PathBuf::from("/notes/md")));
        // No account, no folder.
        config.account = None;
        assert_eq!(config.markdown_dir(), None);
        config.account = Some(String::new());
        assert_eq!(config.markdown_dir(), None);
    }

    fn logged_in(data_dir: &Path) -> AppConfig {
        AppConfig {
            data_dir: data_dir.to_owned(),
            account: Some("jordi".into()),
            ..AppConfig::default()
        }
    }

    #[test]
    fn the_markdown_folder_is_seeded_from_the_pipeline_config() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let data = tempfile::tempdir().unwrap();
        // One machine: `server.toml` `[pipeline]`, next to the server's own keys.
        fs::write(
            data.path().join("server.toml"),
            "listen = \"127.0.0.1:19072\"\n[pipeline]\nocr_settle = \"5m\"\n\
             [pipeline.markdown_dirs]\nother = \"/elsewhere\"\njordi = \"/notes/md\"\n",
        )
        .unwrap();
        let mut config = logged_in(data.path());
        assert!(config.seed_markdown_dir());
        assert_eq!(config.markdown_dir(), Some(PathBuf::from("/notes/md")));
        assert_eq!(config.markdown_dirs.len(), 1, "{:?}", config.markdown_dirs);
        // Seeded once: an entry of the app is never replaced.
        fs::write(
            data.path().join("server.toml"),
            "[pipeline.markdown_dirs]\njordi = \"/new/md\"\n",
        )
        .unwrap();
        assert!(!config.seed_markdown_dir());
        assert_eq!(config.markdown_dir(), Some(PathBuf::from("/notes/md")));

        // Two machines: only `pipeline.toml` counts, `~` is the home folder and a relative
        // folder is below the data folder, as the pipeline reads it.
        fs::write(
            data.path().join("pipeline.toml"),
            "[server]\nurl = \"http://pi.local:19072\"\n[pipeline.markdown_dirs]\njordi = \"~/md\"\nother = \"md\"\n",
        )
        .unwrap();
        let mut config = logged_in(data.path());
        assert!(config.seed_markdown_dir());
        assert_eq!(config.markdown_dir(), Some(home.join("md")));
        let mut other = logged_in(data.path());
        other.account = Some("other".into());
        assert!(other.seed_markdown_dir());
        assert_eq!(other.markdown_dir(), Some(data.path().join("md")));
    }

    #[test]
    fn without_a_pipeline_entry_the_default_is_used() {
        let data = tempfile::tempdir().unwrap();
        // No file, no entry for this account, an empty entry, a broken file: nothing is seeded.
        for server_toml in [
            None,
            Some("[pipeline.markdown_dirs]\nother = \"/elsewhere\"\n"),
            Some("[pipeline.markdown_dirs]\njordi = \"\"\n"),
            Some("[pipeline"),
        ] {
            let _ = fs::remove_file(data.path().join("server.toml"));
            if let Some(text) = server_toml {
                fs::write(data.path().join("server.toml"), text).unwrap();
            }
            let mut config = logged_in(data.path());
            assert!(!config.seed_markdown_dir(), "{server_toml:?}");
            assert!(config.markdown_dirs.is_empty());
            assert_eq!(config.markdown_dir(), Some(default_markdown_dir()));
        }
        // With `pipeline.toml`, an entry in `server.toml` is not the pipeline's.
        fs::write(
            data.path().join("server.toml"),
            "[pipeline.markdown_dirs]\njordi = \"/notes/md\"\n",
        )
        .unwrap();
        fs::write(
            data.path().join("pipeline.toml"),
            "[server]\nurl = \"http://pi.local:19072\"\n",
        )
        .unwrap();
        let mut config = logged_in(data.path());
        assert!(!config.seed_markdown_dir());
        // No account: nothing to seed.
        let mut config = AppConfig {
            data_dir: data.path().to_owned(),
            ..AppConfig::default()
        };
        assert!(!config.seed_markdown_dir());
    }

    #[test]
    fn a_missing_config_is_not_set_with_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = load(dir.path());
        assert_eq!(config.mode, Mode::NotSet);
        assert_eq!(config.start_view, StartView::Tree);
        assert!(config.data_dir.ends_with("notedown-data"));
        assert_eq!(config.server_url, None);
        assert_eq!(config.account, None);
    }

    #[test]
    fn a_config_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &sample()).unwrap();
        assert_eq!(load(dir.path()), sample());
    }

    #[test]
    fn a_broken_config_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), "{ not json").unwrap();
        let config = load(dir.path());
        assert_eq!(config.mode, Mode::NotSet);
        assert!(!dir.path().join("config.json").exists());
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json.broken")).unwrap(),
            "{ not json"
        );
    }

    #[test]
    fn the_login_part_of_an_address_is_dropped() {
        for (url, shown) in [
            ("https://user:secret@n.example.org", "https://n.example.org"),
            ("https://user@n.example.org/x", "https://n.example.org/x"),
            (
                "http://u:p@pi.local:19072/rest/v1?a=b",
                "http://pi.local:19072/rest/v1?a=b",
            ),
            // Unchanged without a login, also the case and a trailing slash.
            ("https://Notes.Example.org/", "https://Notes.Example.org/"),
            (
                "https://n.example.org/x?to=a@b",
                "https://n.example.org/x?to=a@b",
            ),
            ("https://n.example.org#a@b", "https://n.example.org#a@b"),
            ("no address", "no address"),
        ] {
            assert_eq!(without_userinfo(url), shown, "{url}");
        }
    }

    #[test]
    fn save_writes_through_a_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // A stale temp file from a crash is replaced, and none is left behind.
        fs::write(dir.path().join("config.json.tmp"), "stale").unwrap();
        save(dir.path(), &sample()).unwrap();
        assert!(!dir.path().join("config.json.tmp").exists());
        let text = fs::read_to_string(dir.path().join("config.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["mode"], "remote");
        assert_eq!(value["startView"], "columns");
        // A second save replaces the first.
        let mut next = sample();
        next.mode = Mode::Local;
        save(dir.path(), &next).unwrap();
        assert_eq!(load(dir.path()).mode, Mode::Local);
    }

    #[test]
    fn a_failed_folder_sync_after_the_rename_is_no_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("config");
        fs::create_dir(&folder).unwrap();
        // Write and search, but no read: the file goes in, the folder cannot be opened.
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o300)).unwrap();
        let saved = save(&folder, &sample());
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o755)).unwrap();
        saved.unwrap();
        assert_eq!(load(&folder), sample());
    }

    #[test]
    fn existing_setup_sees_server_and_pipeline() {
        let dir = tempfile::tempdir().unwrap();
        let none = existing_setup(dir.path());
        assert!(!none.server && !none.pipeline);

        fs::write(dir.path().join("server.toml"), "").unwrap();
        let server = existing_setup(dir.path());
        assert!(server.server && !server.pipeline);

        fs::remove_file(dir.path().join("server.toml")).unwrap();
        fs::create_dir(dir.path().join("accounts")).unwrap();
        fs::write(dir.path().join("pipeline.toml"), "").unwrap();
        let both = existing_setup(dir.path());
        assert!(both.server && both.pipeline);
    }

    #[test]
    fn a_partial_config_loads_through_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), r#"{"mode":"local"}"#).unwrap();
        let loaded = load_checked(dir.path());
        assert!(!loaded.reset);
        assert_eq!(loaded.config.mode, Mode::Local);
        assert_eq!(loaded.config.start_view, StartView::Tree);
        assert!(loaded.config.data_dir.ends_with("notedown-data"));
    }

    #[test]
    fn the_default_folder_is_absolute_under_home() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let dir = default_data_dir();
        assert!(dir.is_absolute());
        assert!(dir.starts_with(&home));
        assert_eq!(dir, home.join("notedown-data"));
    }

    #[test]
    fn a_read_error_keeps_the_file_and_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        // A folder where the file should be: reading fails, but not with NotFound.
        fs::create_dir(dir.path().join("config.json")).unwrap();
        let loaded = load_checked(dir.path());
        assert!(!loaded.reset);
        assert_eq!(loaded.config.mode, Mode::NotSet);
        assert!(dir.path().join("config.json").is_dir());
        assert!(!dir.path().join("config.json.broken").exists());
    }

    #[test]
    fn invalid_utf8_is_kept_aside() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), [0xff, 0xfe, 0x00]).unwrap();
        let loaded = load_checked(dir.path());
        assert!(loaded.reset);
        assert!(dir.path().join("config.json.broken").is_file());
    }

    #[test]
    fn the_newest_broken_file_wins_and_reset_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json.broken"), "older").unwrap();
        fs::write(dir.path().join("config.json"), "newer {").unwrap();
        let loaded = load_checked(dir.path());
        assert!(loaded.reset);
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json.broken")).unwrap(),
            "newer {"
        );
        // A healthy file reports no reset.
        save(dir.path(), &sample()).unwrap();
        assert!(!load_checked(dir.path()).reset);
    }
}
