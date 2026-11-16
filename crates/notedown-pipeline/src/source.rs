//! Where the pipeline reads the server's data from. [`ServerData`] is the only way the worker
//! sees the server; [`FolderSource`] reads a server data folder, which may be mounted
//! read-only and is never written to.

use std::path::{Path, PathBuf};

use notedown_cloud::appdata::Digest;
use notedown_cloud::auth::Accounts;
use notedown_cloud::read;

pub use notedown_cloud::read::{JournalBatch, NoteBytes, Removal, ServerAccount};

use crate::records::{Record, Records};
use crate::{Error, Result};

/// Every method may block for seconds. Call it from the worker thread or CLI code, never on a
/// tokio runtime thread. `Err` always means "nothing known": skip and retry, never "deleted".
pub trait ServerData: Send + Sync {
    fn describe(&self) -> String;
    fn accounts(&self) -> Result<Vec<ServerAccount>>;
    fn records(&self, slug: &str) -> Result<Records>;
    /// `Ok(None)`: no such record. The caller skips the note in both `Err` and `None`.
    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>>;
    /// `Missing` only when the account's `files/` exists and the note does not; `NotAFile` when
    /// something that is not a file is at its path; a missing account is `Err`. Any `Err` is
    /// "nothing known": the worker skips the note and counts no try.
    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes>;
    /// A cheap check without the bytes: `Some(false)` as for `Missing`, `None` when nothing is
    /// known.
    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>>;
    fn digests(&self, slug: &str) -> Result<Vec<Digest>>;
    /// The account's events with `seq >= from`, at most `limit` lines. Never creates anything.
    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch>;
    /// The accounts whose token the server refused or that hold another account's token, as
    /// (slug, message for the user), as of the last calls. They are left out of
    /// [`ServerData::accounts`]. A source prints nothing itself; the worker says these.
    fn refusals(&self) -> Vec<(String, String)> {
        Vec::new()
    }
    /// Whether the source knows for sure that account `slug`, left out of the last
    /// [`ServerData::accounts`], is gone. Only then may a removal delete its note text; a
    /// slug that is only missing from the list (a refused token, a failed request) waits.
    /// The default is `false`: a source that cannot tell never deletes.
    fn knows_gone(&self, slug: &str) -> bool {
        let _ = slug;
        false
    }
}

/// The server data in a folder on this machine.
pub struct FolderSource {
    root: PathBuf,
    /// A folder of another machine (a mount): a missing `accounts/` may mean it is not mounted.
    shared: bool,
}

impl FolderSource {
    /// The data folder of the server on this machine.
    pub fn new(root: &Path) -> FolderSource {
        FolderSource {
            root: root.to_path_buf(),
            shared: false,
        }
    }

    /// The data folder of the server on another machine.
    pub fn shared(root: &Path) -> FolderSource {
        FolderSource {
            root: root.to_path_buf(),
            shared: true,
        }
    }
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

/// An error of the folder readers, worded as the pipeline words its own: a file error stays
/// "<path>: <error>".
fn from_cloud(e: notedown_cloud::Error) -> Error {
    match e {
        notedown_cloud::Error::Io { path, source } => Error::Io { path, source },
        e => Error::Cloud(e),
    }
}

impl ServerData for FolderSource {
    fn describe(&self) -> String {
        format!("server folder {}", self.root.display())
    }

    /// A server folder without `accounts/` (a share that is not or only half mounted) is no
    /// answer, never "no accounts": a server whose last account was removed still has the
    /// folder (it holds `.next_user_id`).
    fn accounts(&self) -> Result<Vec<ServerAccount>> {
        let dir = self.root.join("accounts");
        match std::fs::metadata(&dir) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(Error::Read(format!(
                    "{} is not a folder; is the server folder mounted?",
                    dir.display()
                )));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !self.shared => {
                return Err(Error::NoAccounts(self.root.clone()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::Read(format!(
                    "{} is missing; is the server folder mounted?",
                    dir.display()
                )));
            }
            Err(e) => return Err(io(&dir)(e)),
        }
        Ok(Accounts::new(&self.root)
            .list()?
            .into_iter()
            .map(|a| ServerAccount {
                slug: a.slug,
                user_id: a.user_id,
                devices: Vec::new(),
            })
            .collect())
    }

    fn records(&self, slug: &str) -> Result<Records> {
        read::records(&self.root, slug).map_err(from_cloud)
    }

    fn record(&self, slug: &str, key: &str) -> Result<Option<Record>> {
        read::record(&self.root, slug, key).map_err(from_cloud)
    }

    fn note(&self, slug: &str, path: &str) -> Result<NoteBytes> {
        read::note(&self.root, slug, path).map_err(from_cloud)
    }

    fn note_exists(&self, slug: &str, path: &str) -> Result<Option<bool>> {
        read::note_exists(&self.root, slug, path).map_err(from_cloud)
    }

    fn digests(&self, slug: &str) -> Result<Vec<Digest>> {
        read::digests(&self.root, slug).map_err(from_cloud)
    }

    fn journal(&self, slug: &str, from: u64, limit: usize) -> Result<JournalBatch> {
        read::journal(&self.root, slug, from, limit).map_err(from_cloud)
    }

    /// The folder lists every account it holds: a slug it leaves out is gone.
    fn knows_gone(&self, _slug: &str) -> bool {
        true
    }
}
