//! Save Markdown (spec 4): the open note's Markdown into a file the user picks. The text is the
//! one the library serves to agents, made by the bundled sidecar (`notedown pipeline note`).
//! The review's button, the File menu item and a note's context menu all run [`save`].

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tauri::menu::{Menu, MenuItem, MenuItemKind, PredefinedMenuItem};
use tauri::{AppHandle, Manager, Runtime, Wry};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};

use crate::commands::ConfigState;
use crate::config::AppConfig;
use crate::notes::{NoteSummary, Notes};
use crate::process::{self, SpawnFn};
use crate::windows::WindowKind;

/// How long the sidecar may take for one note.
pub const NOTE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a save says for a note without text.
pub const NO_TEXT: &str = "This note has no text yet.";

/// The id of the File menu item.
pub const MENU_ID: &str = "save-markdown";

/// The text of the File menu item and of the context menu item.
pub const MENU_TEXT: &str = "Save Markdown...";

/// The title of the dialog that shows a failed save.
const ERROR_TITLE: &str = "Save Markdown";

/// What the File menu item saves: the note the review shows (its wire path), whether it has
/// text, and whether the library window is in front. Only all three enable the item, so
/// Cmd+Shift+S in Settings never saves the note of a hidden library.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SaveState {
    pub path: Option<String>,
    pub can_save: bool,
    pub library_front: bool,
}

impl SaveState {
    /// The note the File menu item saves now, if any.
    pub fn target(&self) -> Option<&str> {
        self.path
            .as_deref()
            .filter(|_| self.can_save && self.library_front)
    }
}

/// The [`SaveState`] of the app, and whether a save panel is open.
#[derive(Default)]
pub struct SaveTarget {
    state: Mutex<SaveState>,
    saving: AtomicBool,
}

/// The one save that runs; dropped, the next may start.
pub struct SaveTurn<'a>(&'a AtomicBool);

impl<'a> SaveTurn<'a> {
    /// The turn, `None` while another save runs (its panel is open).
    pub fn take(saving: &'a AtomicBool) -> Option<Self> {
        (!saving.swap(true, Ordering::SeqCst)).then_some(SaveTurn(saving))
    }
}

impl Drop for SaveTurn<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// The File menu item, kept from when the menu was built, so it can be enabled and disabled.
#[derive(Default)]
pub struct SaveMenuItem(pub Mutex<Option<MenuItem<Wry>>>);

/// The file name a save proposes for a note titled `title`: `<title>.md`, with "/" and ":"
/// (which a Mac file name cannot hold) as "-" and no leading dot. A note without a title is
/// "Note.md".
pub fn file_name(title: &str) -> String {
    // A leading dot would make a hidden file.
    let title = title.trim().trim_start_matches('.').trim();
    if title.is_empty() {
        return "Note.md".to_owned();
    }
    let safe: String = title
        .chars()
        .map(|c| if c == '/' || c == ':' { '-' } else { c })
        .collect();
    format!("{safe}.md")
}

/// The folder the save dialog opens in: the folder of the last save while it is a folder,
/// else Downloads.
pub fn start_folder(last: Option<&Path>, downloads: &Path) -> PathBuf {
    match last {
        Some(dir) if dir.is_dir() => dir.to_path_buf(),
        _ => downloads.to_path_buf(),
    }
}

/// The arguments of `pipeline note`: `--account` only when the config names one, and the key
/// in one argument, so a key that starts with `-` is never read as an option.
fn note_args(config: &AppConfig, key: &str) -> Vec<String> {
    let mut args = vec![
        "pipeline".to_owned(),
        "note".to_owned(),
        "--data".to_owned(),
        config.data_dir.to_string_lossy().into_owned(),
    ];
    if let Some(account) = config.account_slug() {
        args.extend(["--account".to_owned(), account.to_owned()]);
    }
    args.push(format!("--key={key}"));
    args
}

/// The Markdown of note `key`: what `notedown pipeline note` prints, the same text as the
/// note's file in the Markdown folder. Blocking, at most [`NOTE_TIMEOUT`].
pub fn markdown_of(spawn: &SpawnFn, config: &AppConfig, key: &str) -> Result<String, String> {
    let not_read = "Notedown could not read the text of the note.";
    let output = process::run_bounded(spawn, &note_args(config, key), NOTE_TIMEOUT)
        .map_err(|why| format!("{not_read} {why}"))?;
    if !output.status.success() {
        return Err(format!(
            "{not_read} {}",
            process::error_text(&output.stderr, output.status)
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| not_read.to_owned())
}

/// Writes `text` to `path`: into a temp file in the same folder, then renamed over `path`, so
/// the file is never half written. The temp file is removed when that fails. The folder is
/// synced after the rename, so the new name survives a crash too.
pub fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let not_saved = |e: std::io::Error| format!("The file {} was not saved: {e}", path.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        not_saved(e)
    })?;
    // The file is saved; a folder that cannot be synced does not undo that.
    if let Ok(folder) = std::fs::File::open(dir) {
        let _ = folder.sync_all();
    }
    Ok(())
}

/// A save of the note at `path` with its steps given: `ask` is the save dialog (`None` for
/// Cancel), `markdown` reads the text and `remember` keeps the folder of the saved file. A
/// note the index does not know, or one without text, is refused with [`NO_TEXT`].
pub fn save_with(
    notes: &Notes,
    path: &str,
    ask: impl FnOnce(&NoteSummary) -> Option<PathBuf>,
    markdown: impl FnOnce(&NoteSummary) -> Result<String, String>,
    remember: impl FnOnce(&Path),
) -> Result<Option<PathBuf>, String> {
    let Some(note) = notes.at_path(path).filter(|n| n.pages_with_text > 0) else {
        return Err(NO_TEXT.to_owned());
    };
    let Some(target) = ask(&note) else {
        return Ok(None);
    };
    let text = markdown(&note)?;
    write_atomically(&target, &text)?;
    if let Some(dir) = target.parent() {
        remember(dir);
    }
    Ok(Some(target))
}

/// Saves the Markdown of the note at `path` into a file the user picks in the save dialog.
/// `Ok(None)` when the user cancels, or while another save's panel is open. Blocking (the
/// dialog waits for the user): never on the main thread.
pub fn save(app: &AppHandle, path: &str) -> Result<Option<PathBuf>, String> {
    let target = app.state::<SaveTarget>();
    let Some(_turn) = SaveTurn::take(&target.saving) else {
        return Ok(None);
    };
    // The panel is a sheet on the library window when that shows.
    let library = app
        .get_webview_window(WindowKind::Library.label())
        .filter(|w| w.is_visible().unwrap_or(false));
    let notes = app.state::<Notes>().inner().clone();
    // A page the review shows with text may be newer than the index's last scan.
    notes.refresh();
    let config_state = app.state::<ConfigState>().inner().clone();
    let config = config_state.current();
    let spawn = app.state::<crate::setup::Sidecar>().0.clone();
    let downloads = app
        .path()
        .download_dir()
        .or_else(|_| app.path().home_dir().map(|home| home.join("Downloads")))
        .unwrap_or_default();
    save_with(
        &notes,
        path,
        |note| {
            let dialog = app.dialog().file();
            let dialog = match &library {
                Some(window) => dialog.set_parent(window),
                None => dialog,
            };
            dialog
                .set_file_name(file_name(&note.title))
                .set_directory(start_folder(
                    config.last_markdown_save_dir.as_deref(),
                    &downloads,
                ))
                .add_filter("Markdown", &["md"])
                .blocking_save_file()
                .and_then(|file| file.into_path().ok())
        },
        |note| markdown_of(&spawn, &config, &note.key),
        |dir| {
            if let Err(why) = config_state.set_last_markdown_save_dir(dir.to_path_buf()) {
                eprintln!("notedown: the folder of the save was not kept: {why}");
            }
        },
    )
}

/// Shows a failed save in a dialog. It does not wait for the user.
pub fn show_error(app: &AppHandle, error: &str) {
    eprintln!("notedown: Save Markdown failed: {error}");
    app.dialog()
        .message(error)
        .title(ERROR_TITLE)
        .kind(MessageDialogKind::Error)
        .show(|_| {});
}

/// Saves the note at `path` on a thread of its own (menu events come on the main thread) and
/// shows a failure.
pub fn save_in_background(app: &AppHandle, path: String) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(error) = save(&app, &path) {
            show_error(&app, &error);
        }
    });
}

/// The File menu item was picked: saves the note the review shows, only while the library is
/// in front.
pub fn on_menu(app: &AppHandle) {
    let path = lock_state(app).target().map(str::to_owned);
    if let Some(path) = path {
        save_in_background(app, path);
    }
}

fn lock_state(app: &AppHandle) -> std::sync::MutexGuard<'_, SaveState> {
    app.state::<SaveTarget>()
        .inner()
        .state
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
}

/// Changes the [`SaveState`] and lets the File menu item follow.
fn change_state(app: &AppHandle, change: impl FnOnce(&mut SaveState)) {
    let enabled = {
        let mut state = lock_state(app);
        change(&mut state);
        state.target().is_some()
    };
    if let Some(item) = app
        .state::<SaveMenuItem>()
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        && let Err(why) = item.set_enabled(enabled)
    {
        eprintln!("notedown: the Save Markdown menu item did not change: {why}");
    }
}

/// The review shows the note at `path` (`None`: no note), which can be saved when `can_save`.
pub fn review_shown(app: &AppHandle, path: Option<String>, can_save: bool) {
    change_state(app, |state| {
        state.path = path;
        state.can_save = can_save;
    });
}

/// The library window came to the front or left it (hidden, or another window in front).
pub fn library_focused(app: &AppHandle, front: bool) {
    change_state(app, |state| state.library_front = front);
}

/// Puts "Save Markdown..." (Cmd+Shift+S, disabled) at the top of the File submenu of `menu`,
/// and gives it a File submenu when it has none. Answers the item.
pub fn add_to_file_menu<R: Runtime>(
    app: &AppHandle<R>,
    menu: &Menu<R>,
) -> tauri::Result<MenuItem<R>> {
    let item = MenuItem::with_id(app, MENU_ID, MENU_TEXT, false, Some("CmdOrCtrl+Shift+S"))?;
    let file = menu.items()?.into_iter().find_map(|kind| match kind {
        MenuItemKind::Submenu(sub) if sub.text().is_ok_and(|t| t == "File") => Some(sub),
        _ => None,
    });
    match file {
        Some(file) => {
            file.prepend(&PredefinedMenuItem::separator(app)?)?;
            file.prepend(&item)?;
        }
        None => {
            let file = tauri::menu::Submenu::with_items(app, "File", true, &[&item])?;
            menu.insert(&file, 1)?;
        }
    }
    Ok(item)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::config::Mode;

    #[test]
    fn the_file_name_is_the_title_with_safe_characters() {
        for (title, name) in [
            ("Plan", "Plan.md"),
            ("a/b:c", "a-b-c.md"),
            ("", "Note.md"),
            ("Meeting 2026", "Meeting 2026.md"),
            // No hidden file: leading dots go.
            (".plan", "plan.md"),
            ("...", "Note.md"),
        ] {
            assert_eq!(file_name(title), name, "{title:?}");
        }
    }

    #[test]
    fn the_file_item_saves_only_a_note_with_text_in_the_library_in_front() {
        let state = |path: Option<&str>, can_save, library_front| SaveState {
            path: path.map(str::to_owned),
            can_save,
            library_front,
        };
        assert_eq!(
            state(Some("N/a.note"), true, true).target(),
            Some("N/a.note")
        );
        // Settings in front of a hidden or unfocused library: nothing to save.
        assert_eq!(state(Some("N/a.note"), true, false).target(), None);
        assert_eq!(state(Some("N/a.note"), false, true).target(), None);
        assert_eq!(state(None, true, true).target(), None);
    }

    #[test]
    fn one_save_at_a_time() {
        let saving = AtomicBool::new(false);
        let first = SaveTurn::take(&saving).expect("the first save");
        assert!(SaveTurn::take(&saving).is_none(), "a second panel");
        drop(first);
        assert!(SaveTurn::take(&saving).is_some(), "free again");
    }

    #[test]
    fn the_start_folder_is_the_last_one_while_it_is_a_folder() {
        let last = tempfile::tempdir().unwrap();
        let downloads = Path::new("/Users/someone/Downloads");
        assert_eq!(start_folder(Some(last.path()), downloads), last.path());
        assert_eq!(start_folder(None, downloads), downloads);
        let gone = last.path().join("gone");
        assert_eq!(start_folder(Some(&gone), downloads), downloads);
        // A file where the folder was is no folder.
        let file = last.path().join("file");
        std::fs::write(&file, "x").unwrap();
        assert_eq!(start_folder(Some(&file), downloads), downloads);
    }

    fn entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_write_leaves_only_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("Plan.md");
        write_atomically(&path, "# Plan\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan\n");
        // A second write replaces the file.
        write_atomically(&path, "# Plan 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Plan 2\n");
        assert_eq!(entries(dir.path()), ["Plan.md"]);
    }

    #[test]
    fn a_failed_write_removes_its_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        // The target is a folder that is not empty: the temp file is written, the rename fails.
        let path = dir.path().join("Plan.md");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("inside"), "x").unwrap();
        assert!(write_atomically(&path, "# Plan\n").is_err());
        assert_eq!(entries(dir.path()), ["Plan.md"]);
        assert_eq!(entries(&path), ["inside"]);
    }

    fn config(account: Option<&str>) -> AppConfig {
        AppConfig {
            mode: Mode::Local,
            data_dir: PathBuf::from("/nowhere/data"),
            account: account.map(str::to_owned),
            ..AppConfig::default()
        }
    }

    /// A fake `notedown` that saves its arguments, one per line, prints `stdout` and ends
    /// with `code` (printing `error: failed` to stderr when that is not 0).
    fn sidecar(dir: &Path, stdout: &str, code: i32) -> SpawnFn {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(dir.join("out.md"), stdout).unwrap();
        let path = dir.join("notedown");
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > '{}'\ncat '{}'\n\
                 [ {code} -ne 0 ] && echo 'error: failed' >&2\nexit {code}\n",
                dir.join("args").display(),
                dir.join("out.md").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        crate::process::spawn_program(path)
    }

    fn args(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("args"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn markdown_of_runs_pipeline_note_and_reads_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let spawn = sidecar(dir.path(), "# Plan\n\n## Page 1\n", 0);
        assert_eq!(
            markdown_of(&spawn, &config(Some("someone")), "F1").unwrap(),
            "# Plan\n\n## Page 1\n"
        );
        assert_eq!(
            args(dir.path()),
            [
                "pipeline",
                "note",
                "--data",
                "/nowhere/data",
                "--account",
                "someone",
                "--key=F1"
            ]
        );
        // Without an account in the config the sidecar picks the only one.
        markdown_of(&spawn, &config(None), "-k").unwrap();
        assert_eq!(
            args(dir.path()),
            ["pipeline", "note", "--data", "/nowhere/data", "--key=-k"]
        );
    }

    #[test]
    fn a_failed_pipeline_note_gives_its_error() {
        let dir = tempfile::tempdir().unwrap();
        let spawn = sidecar(dir.path(), "", 1);
        let error = markdown_of(&spawn, &config(None), "F1").unwrap_err();
        assert!(error.contains("failed"), "{error}");
    }

    /// A notes index over a store with note `A` (`NOTE/Note/Plan.note`): with text on its
    /// first page when `text`, else with its pages still waiting.
    fn notes_with(text: bool) -> (Notes, tempfile::TempDir, tempfile::TempDir) {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let dir = data.path().join("pipeline").join("someone").join("notes");
        std::fs::create_dir_all(&dir).unwrap();
        let page = if text {
            serde_json::json!({"pageId": "P1", "seq": 4, "blocks": [{"id": "b1", "text": "Hi"}]})
        } else {
            serde_json::json!({"pageId": "P1", "blocks": []})
        };
        let note = serde_json::json!({
            "key": "A",
            "path": "NOTE/Note/Plan.note",
            "title": "Plan",
            "pages": [page],
        });
        std::fs::write(dir.join("A.json"), note.to_string()).unwrap();
        let root = data.path().to_path_buf();
        let notes = Notes::new(
            Arc::new(move || (root.clone(), Some("someone".to_owned()))),
            config.path().to_path_buf(),
            Arc::new(|_| {}),
        );
        notes.all();
        (notes, data, config)
    }

    #[test]
    fn a_note_without_text_is_not_saved() {
        let (notes, _data, _config) = notes_with(false);
        for path in ["NOTE/Note/Plan.note", "NOTE/Note/Other.note"] {
            let result = save_with(
                &notes,
                path,
                |_| panic!("no dialog"),
                |_| panic!("no sidecar"),
                |_| panic!("nothing to remember"),
            );
            assert_eq!(result, Err(NO_TEXT.to_owned()), "{path}");
        }
    }

    #[test]
    fn a_cancelled_dialog_saves_nothing() {
        let (notes, _data, _config) = notes_with(true);
        let result = save_with(
            &notes,
            "NOTE/Note/Plan.note",
            |note| {
                assert_eq!(note.title, "Plan");
                None
            },
            |_| panic!("no sidecar"),
            |_| panic!("nothing to remember"),
        );
        assert_eq!(result, Ok(None));
    }

    #[test]
    fn a_save_writes_the_file_and_remembers_its_folder() {
        let (notes, _data, _config) = notes_with(true);
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("Plan.md");
        let mut remembered = None;
        let result = save_with(
            &notes,
            "NOTE/Note/Plan.note",
            |_| Some(target.clone()),
            |note| {
                assert_eq!(note.key, "A");
                Ok("# Plan\n".to_owned())
            },
            |dir| remembered = Some(dir.to_path_buf()),
        );
        assert_eq!(result, Ok(Some(target.clone())));
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "# Plan\n");
        assert_eq!(remembered.as_deref(), Some(out.path()));
    }
}
