//! The context menu of a page image in the review and in the large view (spec 4): Copy Image
//! puts the page as PNG on the clipboard, Save Image... saves it into a file the user picks,
//! and Share... shows the macOS share menu for it. The window names only the note's path and
//! the page's index; Rust reads the PNG it rendered for the review.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use base64::Engine as _;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

use crate::commands::ConfigState;
use crate::menus::MENU_ERROR_EVENT;
use crate::notes::Notes;
use crate::review::Review;
use crate::save_markdown::{SaveTarget, safe_title, start_folder, write_atomically};
use crate::windows::WindowKind;

/// The ids of the menu items.
pub const COPY_ID: &str = "ctx-copy-image";
pub const SAVE_ID: &str = "ctx-save-image";
pub const SHARE_ID: &str = "ctx-share-image";

/// The texts of the menu items.
pub const COPY_TEXT: &str = "Copy Image";
pub const SAVE_TEXT: &str = "Save Image...";
pub const SHARE_TEXT: &str = "Share...";

/// What a failed copy says.
const NOT_COPIED: &str = "The page could not be copied.";
/// What a share says when the library window is gone.
const NOT_SHARED: &str = "The page could not be shared.";

/// What a menu item does with the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageAction {
    Copy,
    Save,
    Share,
}

/// The action of the menu item `id`, if it is one of the page's.
pub fn action_of(id: &str) -> Option<PageAction> {
    match id {
        COPY_ID => Some(PageAction::Copy),
        SAVE_ID => Some(PageAction::Save),
        SHARE_ID => Some(PageAction::Share),
        _ => None,
    }
}

/// The file name of page `index` (0-based) of the note titled `title`: "Plan page 3.png".
pub fn image_name(title: &str, index: usize) -> String {
    format!("{} page {}.png", safe_title(title), index + 1)
}

/// The title of the note at `path` as Save Markdown names it: the notes index's title, else
/// the file name without `.note`.
pub fn note_title(notes: &Notes, path: &str) -> String {
    notes
        .at_path(path)
        .map(|note| note.title)
        .unwrap_or_else(|| crate::review::title_of(path))
}

/// The PNG of a `data:image/png;base64,...` URL, as the review gets its pages.
pub fn png_of(url: &str) -> Result<Vec<u8>, String> {
    let not_read = || crate::review::NO_IMAGE.to_owned();
    let data = url
        .strip_prefix("data:image/png;base64,")
        .ok_or_else(not_read)?;
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| not_read())
}

/// The folder of the shares under the app cache.
fn shares(root: &Path) -> PathBuf {
    root.join("share")
}

/// Writes the PNG for a share into a new folder of its own under `root` (an older share may
/// still read its file), named `name`. Answers the file.
pub fn write_share(root: &Path, name: &str, png: &[u8]) -> Result<PathBuf, String> {
    let not_written = |e: std::io::Error| format!("{NOT_SHARED} {e}");
    let base = shares(root);
    std::fs::create_dir_all(&base).map_err(not_written)?;
    let mut n = 1u64;
    let dir = loop {
        let dir = base.join(n.to_string());
        match std::fs::create_dir(&dir) {
            Ok(()) => break dir,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(not_written(e)),
        }
    };
    let file = dir.join(name);
    write_atomically(&file, png)?;
    Ok(file)
}

/// Removes the files of earlier shares (at the start of the app, when none runs).
pub fn clear_shares(root: &Path) {
    let base = shares(root);
    if let Err(e) = std::fs::remove_dir_all(&base)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("notedown: the old shares were not removed: {e}");
    }
}

/// Save Image... with its steps given: `png` reads the page, `ask` is the save dialog
/// (`None` for Cancel) and `remember` keeps the folder of the saved file. A page that cannot
/// be read shows no dialog.
pub fn save_with(
    png: impl FnOnce() -> Result<Vec<u8>, String>,
    ask: impl FnOnce() -> Option<PathBuf>,
    remember: impl FnOnce(&Path),
) -> Result<Option<PathBuf>, String> {
    let png = png()?;
    let Some(target) = ask() else {
        return Ok(None);
    };
    write_atomically(&target, &png)?;
    if let Some(dir) = target.parent() {
        remember(dir);
    }
    Ok(Some(target))
}

/// The PNG of page `index` of the note at `path`, as the review rendered it. Blocking.
fn page_png(app: &AppHandle, path: &str, index: usize) -> Result<Vec<u8>, String> {
    let review = app.state::<Review>().inner().clone();
    png_of(review.page_image(path, index)?.as_str())
}

/// Saves page `index` of the note at `path` into a file the user picks. Blocking (the
/// dialog waits for the user): never on the main thread. `Ok(None)` for Cancel, or while
/// another save's panel is open.
fn save(app: &AppHandle, path: &str, index: usize) -> Result<Option<PathBuf>, String> {
    let target = app.state::<SaveTarget>();
    let Some(_turn) = target.turn() else {
        return Ok(None);
    };
    let library = crate::save_markdown::shown_library(app);
    let config_state = app.state::<ConfigState>().inner().clone();
    let config = config_state.current();
    let name = image_name(&note_title(&app.state::<Notes>(), path), index);
    let downloads = crate::save_markdown::downloads(app);
    save_with(
        || page_png(app, path, index),
        || {
            let dialog = app.dialog().file();
            let dialog = match &library {
                Some(window) => dialog.set_parent(window),
                None => dialog,
            };
            dialog
                .set_file_name(&name)
                .set_directory(start_folder(
                    config.last_image_save_dir.as_deref(),
                    &downloads,
                ))
                .add_filter("PNG Image", &["png"])
                .blocking_save_file()
                .and_then(|file| file.into_path().ok())
        },
        |dir| {
            if let Err(why) = config_state.set_last_image_save_dir(dir.to_path_buf()) {
                eprintln!("notedown: the folder of the save was not kept: {why}");
            }
        },
    )
}

/// Runs `work` on the main thread and waits for its answer. Blocking: never on the main
/// thread.
fn on_main<T: Send + 'static>(
    app: &AppHandle,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = mpsc::channel();
    app.run_on_main_thread(move || {
        let _ = tx.send(work());
    })
    .map_err(|e| e.to_string())?;
    rx.recv().map_err(|e| e.to_string())
}

/// Does `action` with page `index` of the note at `path`, right-clicked at `x`, `y` of the
/// library window's web view. Blocking.
fn run(
    app: &AppHandle,
    action: PageAction,
    path: &str,
    index: usize,
    at: (f64, f64),
) -> Result<(), String> {
    match action {
        PageAction::Copy => {
            let png = page_png(app, path, index)?;
            on_main(app, move || mac::copy(&png))?
                .then_some(())
                .ok_or_else(|| NOT_COPIED.to_owned())
        }
        PageAction::Save => save(app, path, index).map(|_| ()),
        PageAction::Share => {
            let png = page_png(app, path, index)?;
            let name = image_name(&note_title(&app.state::<Notes>(), path), index);
            let root = app.path().app_cache_dir().map_err(|e| e.to_string())?;
            let file = write_share(&root, &name, &png)?;
            let window = app
                .get_webview_window(WindowKind::Library.label())
                .ok_or_else(|| NOT_SHARED.to_owned())?;
            window
                .with_webview(move |webview| mac::share(webview.inner(), &file, at))
                .map_err(|e| format!("{NOT_SHARED} {e}"))
        }
    }
}

/// The user picked the page item `action` of the menu opened on page `index` of the note at
/// `path`, at `x`, `y`. The work runs off the main thread, which is the one that delivers menu
/// events; a failure goes to the library window as its notice.
pub fn on_menu(app: &AppHandle, action: PageAction, path: String, index: usize, at: (f64, f64)) {
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if let Err(e) = run(&app, action, &path, index, at) {
            eprintln!("notedown: the page menu item failed: {e}");
            let _ = app.emit_to(WindowKind::Library.label(), MENU_ERROR_EVENT, e);
        }
    });
}

/// The AppKit side: the clipboard and the share menu. Main thread only.
mod mac {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::path::Path;

    use objc2::AllocAnyThread;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2_app_kit::{
        NSImage, NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSSharingServicePicker,
        NSView,
    };
    use objc2_foundation::{NSArray, NSData, NSPoint, NSRect, NSRectEdge, NSSize, NSString, NSURL};

    thread_local! {
        /// The share menu that shows: kept until the next one, so it lives while it is open.
        static PICKER: RefCell<Option<Retained<NSSharingServicePicker>>> = const { RefCell::new(None) };
    }

    /// Puts `png` on the general clipboard as PNG, and as TIFF for apps that read no PNG.
    pub fn copy(png: &[u8]) -> bool {
        let data = NSData::with_bytes(png);
        let tiff =
            NSImage::initWithData(NSImage::alloc(), &data).and_then(|i| i.TIFFRepresentation());
        // SAFETY: the type constants are AppKit's own statics.
        let (png_type, tiff_type) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF) };
        let board = NSPasteboard::generalPasteboard();
        let types = match &tiff {
            Some(_) => NSArray::from_slice(&[png_type, tiff_type]),
            None => NSArray::from_slice(&[png_type]),
        };
        // SAFETY: the array holds pasteboard types; no owner is given.
        unsafe { board.declareTypes_owner(&types, None) };
        let copied = board.setData_forType(Some(&data), png_type);
        if let Some(tiff) = &tiff {
            board.setData_forType(Some(tiff), tiff_type);
        }
        copied
    }

    /// Shows the share menu for `file` at `at` (CSS pixels from the top left) of `view`, the
    /// library window's WKWebView.
    pub fn share(view: *mut c_void, file: &Path, at: (f64, f64)) {
        if view.is_null() {
            return;
        }
        // SAFETY: Tauri hands the WKWebView of the window, an NSView, on the main thread, and
        // it lives while the window does.
        let view: &NSView = unsafe { &*view.cast::<NSView>() };
        let url = NSURL::fileURLWithPath(&NSString::from_str(&file.to_string_lossy()));
        let item: Retained<AnyObject> = url.into();
        let items = NSArray::from_retained_slice(&[item]);
        // SAFETY: the items are file URLs, which the picker takes.
        let picker = unsafe {
            NSSharingServicePicker::initWithItems(NSSharingServicePicker::alloc(), &items)
        };
        let height = view.bounds().size.height;
        let y = if view.isFlipped() {
            at.1
        } else {
            height - at.1
        };
        let rect = NSRect::new(NSPoint::new(at.0, y), NSSize::new(1.0, 1.0));
        picker.showRelativeToRect_ofView_preferredEdge(rect, view, NSRectEdge::MinY);
        PICKER.with(|kept| *kept.borrow_mut() = Some(picker));
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[test]
    fn each_item_has_its_action() {
        assert_eq!(action_of("ctx-copy-image"), Some(PageAction::Copy));
        assert_eq!(action_of("ctx-save-image"), Some(PageAction::Save));
        assert_eq!(action_of("ctx-share-image"), Some(PageAction::Share));
        assert_eq!(action_of("ctx-save-markdown"), None);
    }

    #[test]
    fn the_image_is_named_after_the_note_and_the_page() {
        assert_eq!(image_name("Plan", 0), "Plan page 1.png");
        assert_eq!(image_name("Meeting 2026", 11), "Meeting 2026 page 12.png");
        // The same safe characters as Save Markdown.
        assert_eq!(image_name("a/b:c", 1), "a-b-c page 2.png");
        assert_eq!(image_name("", 0), "Note page 1.png");
        assert_eq!(image_name(".plan", 0), "plan page 1.png");
    }

    #[test]
    fn the_title_is_the_index_s_else_the_file_name() {
        let data = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        let dir = data.path().join("pipeline").join("someone").join("notes");
        std::fs::create_dir_all(&dir).unwrap();
        let note = serde_json::json!({
            "key": "A",
            "path": "NOTE/Note/plan-2026.note",
            "title": "Plan",
            "pages": [{"pageId": "P1", "seq": 4, "blocks": [{"id": "b1", "text": "Hi"}]}],
        });
        std::fs::write(dir.join("A.json"), note.to_string()).unwrap();
        let root = data.path().to_path_buf();
        let notes = Notes::new(
            Arc::new(move || (root.clone(), Some("someone".to_owned()))),
            config.path().to_path_buf(),
            Arc::new(|_| {}),
        );
        notes.all();
        assert_eq!(note_title(&notes, "NOTE/Note/plan-2026.note"), "Plan");
        assert_eq!(note_title(&notes, "NOTE/Note/Unread.note"), "Unread");
    }

    #[test]
    fn the_png_comes_out_of_its_data_url() {
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        assert_eq!(png_of(&url), Ok(png));
        assert!(png_of("data:image/jpeg;base64,AAAA").is_err());
        assert!(png_of("data:image/png;base64,***").is_err());
    }

    #[test]
    fn each_share_has_a_folder_of_its_own() {
        let root = tempfile::tempdir().unwrap();
        let first = write_share(root.path(), "Plan page 1.png", b"one").unwrap();
        let second = write_share(root.path(), "Plan page 1.png", b"two").unwrap();
        assert_ne!(first, second);
        assert_eq!(first.file_name().unwrap(), "Plan page 1.png");
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert_eq!(std::fs::read(&second).unwrap(), b"two");
        assert!(first.starts_with(root.path().join("share")));
        clear_shares(root.path());
        assert!(!root.path().join("share").exists());
        // Nothing to clear is no error.
        clear_shares(root.path());
    }

    #[test]
    fn a_save_writes_the_png_and_remembers_its_folder() {
        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("Plan page 1.png");
        let mut remembered = None;
        let saved = save_with(
            || Ok(b"png".to_vec()),
            || Some(target.clone()),
            |dir| remembered = Some(dir.to_path_buf()),
        );
        assert_eq!(saved, Ok(Some(target.clone())));
        assert_eq!(std::fs::read(&target).unwrap(), b"png");
        assert_eq!(remembered.as_deref(), Some(out.path()));
    }

    #[test]
    fn a_cancelled_save_writes_nothing_and_a_missing_page_asks_nothing() {
        let saved = save_with(
            || Ok(b"png".to_vec()),
            || None,
            |_| panic!("nothing to remember"),
        );
        assert_eq!(saved, Ok(None));
        let failed = save_with(
            || Err("The page could not be shown.".to_owned()),
            || panic!("no dialog"),
            |_| panic!("nothing to remember"),
        );
        assert_eq!(failed, Err("The page could not be shown.".to_owned()));
    }
}
