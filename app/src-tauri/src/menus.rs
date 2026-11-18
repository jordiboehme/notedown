//! The native context menus of the windows. The window names a target; Rust decides the items,
//! builds the menu and does the work of the item the user picks.

use std::sync::{Mutex, PoisonError};

use serde::Deserialize;
use tauri::menu::{Menu, MenuItem};
use tauri::{AppHandle, Manager, Runtime, WebviewWindow};

use crate::notes::Notes;

/// Menu item ids of the context menus start with this, so the app's menu handler can tell them
/// from the menu bar's.
pub const CONTEXT_PREFIX: &str = "ctx-";
const MARK_ALL_SEEN_ID: &str = "ctx-mark-all-seen";

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
pub fn context_items(target: &MenuTarget, _has_text: bool) -> Vec<MenuEntry> {
    match target {
        MenuTarget::NewNotes => vec![MenuEntry {
            id: MARK_ALL_SEEN_ID,
            text: "Mark All as Seen",
            enabled: true,
        }],
        MenuTarget::Note { .. } => Vec::new(),
    }
}

/// Builds the native menu of `target` and pops it up at the pointer of `window`. The target is
/// kept until the next menu, for [`on_context`].
pub fn popup<R: Runtime>(window: &WebviewWindow<R>, target: MenuTarget) -> Result<(), String> {
    let items = context_items(&target, false);
    if items.is_empty() {
        return Ok(());
    }
    let app = window.app_handle();
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

/// The user picked the context item `id`. The work runs off the main thread, which is the one
/// that delivers menu events.
pub fn on_context(app: &AppHandle, id: &str) {
    let id = id.to_owned();
    let notes = app.state::<Notes>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || match id.as_str() {
        MARK_ALL_SEEN_ID => {
            if let Err(e) = notes.seen_all() {
                eprintln!("notedown: Mark All as Seen failed: {e}");
            }
        }
        other => eprintln!("notedown: unknown context menu item {other}"),
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
        assert!(context_items(&note, true).is_empty());
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
}
