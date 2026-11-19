//! The native context menus of the windows. The window names a target; Rust decides the items,
//! builds the menu and does the work of the item the user picks.

use std::sync::{Mutex, PoisonError};

use serde::Deserialize;
use tauri::menu::{Menu, MenuItem};
use tauri::{AppHandle, Emitter, Manager, Runtime, WebviewWindow};

use crate::notes::Notes;
use crate::windows::WindowKind;

/// Menu item ids of the context menus start with this, so the app's menu handler can tell them
/// from the menu bar's.
pub const CONTEXT_PREFIX: &str = "ctx-";
const MARK_ALL_SEEN_ID: &str = "ctx-mark-all-seen";
const SAVE_MARKDOWN_ID: &str = "ctx-save-markdown";

/// What the user opened a context menu on.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MenuTarget {
    /// The New notes row of the sidebar and the header of its list.
    NewNotes,
    /// A note, by its wire path.
    Note { path: String },
}

/// One item of a context menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuEntry {
    pub id: &'static str,
    pub text: &'static str,
    pub enabled: bool,
}

/// The target of the menu that is open. The picked item is read against it.
#[derive(Default)]
pub struct MenuTargetState(pub Mutex<Option<MenuTarget>>);

/// The items for `target`; `has_text` says whether a note target has text.
pub fn context_items(target: &MenuTarget, has_text: bool) -> Vec<MenuEntry> {
    match target {
        MenuTarget::NewNotes => vec![MenuEntry {
            id: MARK_ALL_SEEN_ID,
            text: "Mark All as Seen",
            enabled: true,
        }],
        MenuTarget::Note { .. } => vec![MenuEntry {
            id: SAVE_MARKDOWN_ID,
            text: crate::save_markdown::MENU_TEXT,
            enabled: has_text,
        }],
    }
}

/// Whether the notes index knows the note at `path` with text on some page. It does not wait
/// for a scan (the menu opens on the main thread): while one runs the answer is yes, and the
/// save checks again.
pub fn has_text(notes: &Notes, path: &str) -> bool {
    notes
        .try_at_path(path)
        .is_none_or(|note| note.is_some_and(|n| n.pages_with_text > 0))
}

/// Builds the native menu of `target` and pops it up at the pointer of `window`. The target is
/// kept until the next menu, for [`on_context`].
pub fn popup<R: Runtime>(window: &WebviewWindow<R>, target: MenuTarget) -> Result<(), String> {
    let app = window.app_handle();
    let text = match &target {
        MenuTarget::Note { path } => has_text(&app.state::<Notes>(), path),
        MenuTarget::NewNotes => false,
    };
    let items = context_items(&target, text);
    if items.is_empty() {
        return Ok(());
    }
    let menu = Menu::new(app).map_err(|e| e.to_string())?;
    for entry in &items {
        let item = MenuItem::with_id(app, entry.id, entry.text, entry.enabled, None::<&str>)
            .map_err(|e| e.to_string())?;
        menu.append(&item).map_err(|e| e.to_string())?;
    }
    let state = app.state::<MenuTargetState>();
    *state.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(target);
    window.popup_menu(&menu).map_err(|e| e.to_string())
}

/// What a failed context menu item sends to the library window (the error text), which shows it
/// as its notice.
pub const MENU_ERROR_EVENT: &str = "library-menu-error";

/// Does the work of the context item `id`. Blocking: run it off the main thread.
pub fn run_context(notes: &Notes, id: &str) -> Result<(), String> {
    match id {
        MARK_ALL_SEEN_ID => notes.seen_all().map(|_| ()),
        other => Err(format!("Unknown menu item {other}.")),
    }
}

/// The user picked the context item `id`. The work runs off the main thread, which is the one
/// that delivers menu events. A failure goes to the library window as its notice.
pub fn on_context(app: &AppHandle, id: &str) {
    if id == SAVE_MARKDOWN_ID {
        // The note the menu was opened on; its save shows its own failure.
        let target = app
            .state::<MenuTargetState>()
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(MenuTarget::Note { path }) = target {
            crate::save_markdown::save_in_background(app, path);
        }
        return;
    }
    let id = id.to_owned();
    let notes = app.state::<Notes>().inner().clone();
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = run_context(&notes, &id) {
            eprintln!("notedown: the menu item {id} failed: {e}");
            let _ = app.emit_to(WindowKind::Library.label(), MENU_ERROR_EVENT, e);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_items_of_each_target() {
        let entry = |id, text, enabled| MenuEntry { id, text, enabled };
        let table = [
            (
                MenuTarget::NewNotes,
                false,
                vec![entry("ctx-mark-all-seen", "Mark All as Seen", true)],
            ),
            (
                MenuTarget::NewNotes,
                true,
                vec![entry("ctx-mark-all-seen", "Mark All as Seen", true)],
            ),
        ];
        for (target, has_text, items) in table {
            assert_eq!(context_items(&target, has_text), items, "{target:?}");
        }
        let note = MenuTarget::Note {
            path: "NOTE/a.note".to_owned(),
        };
        assert_eq!(
            context_items(&note, true),
            [entry("ctx-save-markdown", "Save Markdown...", true)]
        );
        assert_eq!(
            context_items(&note, false),
            [entry("ctx-save-markdown", "Save Markdown...", false)]
        );
    }

    #[test]
    fn a_target_comes_from_the_window_as_tagged_json() {
        let new: MenuTarget = serde_json::from_str(r#"{"kind":"newNotes"}"#).unwrap();
        assert_eq!(new, MenuTarget::NewNotes);
        let note: MenuTarget =
            serde_json::from_str(r#"{"kind":"note","path":"NOTE/a.note"}"#).unwrap();
        assert_eq!(
            note,
            MenuTarget::Note {
                path: "NOTE/a.note".to_owned()
            }
        );
    }

    fn notes_with_one_new_note(unreadable: bool) -> (Notes, tempfile::TempDir, tempfile::TempDir) {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let notes_dir = data.path().join("pipeline").join("someone").join("notes");
        std::fs::create_dir_all(&notes_dir).unwrap();
        if unreadable {
            // A seen.json that is a folder cannot be read.
            std::fs::create_dir(config.path().join("seen.json")).unwrap();
        }
        let root = data.path().to_path_buf();
        let notes = Notes::new(
            std::sync::Arc::new(move || (root.clone(), Some("someone".to_owned()))),
            config.path().to_path_buf(),
            std::sync::Arc::new(|_| {}),
        );
        // The first scan sees an empty store, so the note written after it is new.
        assert!(notes.new_notes().is_empty());
        let note = serde_json::json!({
            "key": "A",
            "path": "NOTE/Note/a.note",
            "pages": [{"pageId": "P1", "seq": 4, "blocks": [{"id": "b1", "text": "Hello"}]}],
        });
        std::fs::write(notes_dir.join("A.json"), note.to_string()).unwrap();
        if !unreadable {
            assert_eq!(notes.new_notes().len(), 1, "the fixture has one new note");
        }
        (notes, data, config)
    }

    #[test]
    fn the_mark_all_seen_item_runs_seen_all_and_an_unknown_item_fails() {
        let (notes, _data, _config) = notes_with_one_new_note(false);
        assert_eq!(run_context(&notes, "ctx-mark-all-seen"), Ok(()));
        assert!(notes.new_notes().is_empty(), "the item marked the note");
        let unknown = run_context(&notes, "ctx-nothing").unwrap_err();
        assert!(unknown.contains("ctx-nothing"), "{unknown}");
    }

    #[test]
    fn a_note_has_text_when_the_index_knows_it_with_text() {
        let (notes, _data, _config) = notes_with_one_new_note(false);
        assert!(has_text(&notes, "NOTE/Note/a.note"));
        assert!(!has_text(&notes, "NOTE/Note/other.note"));
    }

    #[test]
    fn a_failing_item_hands_its_error_back() {
        let (notes, _data, _config) = notes_with_one_new_note(true);
        assert_eq!(
            run_context(&notes, "ctx-mark-all-seen"),
            Err(crate::notes::SEEN_NOT_READ.to_owned())
        );
    }
}
