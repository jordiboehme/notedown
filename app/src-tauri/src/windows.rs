//! The windows: the panel under the tray icon, the library and the settings. A menu bar app
//! shows a Dock icon only while the library or the settings window is visible.

use std::sync::mpsc::Sender;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use serde::Serialize;
use serde_json::Value;

use tauri::{
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

use crate::notify::Target;
use crate::poller::Poller;
use crate::tray::{self, Area, PanelAction, Screen};

/// The windows of the app, by their labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WindowKind {
    Panel,
    Library,
    Settings,
}

impl WindowKind {
    pub const ALL: [WindowKind; 3] = [WindowKind::Panel, WindowKind::Library, WindowKind::Settings];

    /// The window label, as the capabilities name it.
    pub fn label(self) -> &'static str {
        match self {
            WindowKind::Panel => "panel",
            WindowKind::Library => "library",
            WindowKind::Settings => "settings",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.label() == label)
    }
}

/// The activation policy of the app.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// A Dock icon and an app menu.
    Regular,
    /// Only the menu bar icon.
    Accessory,
}

/// Regular while the library or the settings window is visible, else Accessory. The panel
/// alone never brings the Dock icon.
pub fn policy_for(visible: &[WindowKind]) -> Policy {
    if visible
        .iter()
        .any(|k| matches!(k, WindowKind::Library | WindowKind::Settings))
    {
        Policy::Regular
    } else {
        Policy::Accessory
    }
}

/// One thread tells the poller what is visible, in order, and reads the visibility when it
/// does so, not when it was asked: a fast show and hide ends on the hidden cadence.
/// `read` says whether anything is visible; `apply(visible, panel_opened)` tells the poller.
/// A send of `true` means the panel opened (poll at once).
pub fn visibility_worker(
    read: impl Fn() -> bool + Send + 'static,
    apply: impl Fn(bool, bool) + Send + 'static,
) -> std::sync::mpsc::Sender<bool> {
    let (tx, rx) = std::sync::mpsc::channel::<bool>();
    std::thread::spawn(move || {
        for panel_opened in rx {
            apply(read(), panel_opened);
        }
    });
    tx
}

/// The event that tells an open settings window which page to show: a page name.
pub const SETTINGS_PAGE_EVENT: &str = "settings-page";

/// The event that tells an open library window where to go: a [`LibraryAt`].
pub const LIBRARY_AT_EVENT: &str = "library-at";

/// Where the library window opens: at a note (its review), or at New.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "at", rename_all = "camelCase")]
pub enum LibraryAt {
    Note {
        path: String,
    },
    New,
    /// All notes (a click on a sync notification).
    All,
    /// The recycle bin (Settings > Setup, after a first sync that deleted files).
    Bin,
}

/// How a window is opened: as it is, at a settings page, or at a place of the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Opening {
    Plain,
    Page(String),
    At(LibraryAt),
}

/// The window and the place a click on a notice opens: the library at its place, or the
/// settings at a page.
pub fn opening_for(target: &Target) -> (WindowKind, Opening) {
    match target {
        Target::Library(at) => (WindowKind::Library, Opening::At(at.clone())),
        Target::Settings(page) => (WindowKind::Settings, Opening::Page(page.clone())),
    }
}

/// Opens what a clicked notice points to, through [`show_window`]: the window comes to the
/// front with the Dock icon, also when none was open. Only the macOS deliverer has clicks.
#[cfg(target_os = "macos")]
pub fn route(app: &AppHandle, target: &Target) {
    let (kind, opening) = opening_for(target);
    if let Err(why) = show_window(app, kind, opening) {
        eprintln!("notedown: a notification click opened nothing: {why}");
    }
}

/// What a window is told about its opening: a new window by its init script (it runs before
/// the page's code), an open one by an event.
#[derive(Debug, Clone, PartialEq)]
pub enum Tell {
    Script(String),
    Emit(&'static str, serde_json::Value),
}

/// Where `open_library(note, new_notes)` opens the library: at the note (its path) when given,
/// at New with `new_notes`, else as it is.
pub fn library_opening(note: Option<String>, new_notes: Option<bool>) -> Opening {
    match (note, new_notes) {
        (Some(path), _) => Opening::At(LibraryAt::Note { path }),
        (None, Some(true)) => Opening::At(LibraryAt::New),
        (None, _) => Opening::Plain,
    }
}

/// What the window `kind` is told when it opens as `opening`; `open` says it exists already.
/// The value goes into the script as JSON, so a path is never code.
pub fn tell(kind: WindowKind, opening: &Opening, open: bool) -> Option<Tell> {
    let (name, event, value) = match (kind, opening) {
        (WindowKind::Settings, Opening::Page(page)) => (
            "__NOTEDOWN_PAGE__",
            SETTINGS_PAGE_EVENT,
            serde_json::to_value(page).ok()?,
        ),
        (WindowKind::Settings, _) => ("__NOTEDOWN_PAGE__", SETTINGS_PAGE_EVENT, Value::Null),
        (WindowKind::Library, Opening::At(at)) => (
            "__NOTEDOWN_AT__",
            LIBRARY_AT_EVENT,
            serde_json::to_value(at).ok()?,
        ),
        (WindowKind::Library, _) => ("__NOTEDOWN_AT__", LIBRARY_AT_EVENT, Value::Null),
        (WindowKind::Panel, _) => return None,
    };
    if open {
        (!value.is_null()).then_some(Tell::Emit(event, value))
    } else {
        Some(Tell::Script(format!("window.{name} = {value};")))
    }
}

/// The width of the panel in logical pixels.
pub const PANEL_WIDTH: f64 = 340.0;

/// What is visible, as the app showed and hid it, and when a blur last hid the panel. Kept
/// here and not asked from the windows: right after `hide()` a window may still say visible.
#[derive(Debug, Default)]
pub struct Shown {
    inner: Mutex<ShownInner>,
}

#[derive(Debug, Default)]
struct ShownInner {
    visible: Vec<WindowKind>,
    blur_hidden_at: Option<Instant>,
    /// The tray icon's place from its last event, for a panel opened by a second launch.
    tray: Option<Area>,
}

impl Shown {
    fn lock(&self) -> MutexGuard<'_, ShownInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn visible(&self) -> Vec<WindowKind> {
        self.lock().visible.clone()
    }

    pub fn is_visible(&self, kind: WindowKind) -> bool {
        self.lock().visible.contains(&kind)
    }

    fn set(&self, kind: WindowKind, visible: bool) -> Vec<WindowKind> {
        let mut inner = self.lock();
        inner.visible.retain(|k| *k != kind);
        if visible {
            inner.visible.push(kind);
        }
        inner.visible.clone()
    }

    fn blur_hidden(&self) {
        self.lock().blur_hidden_at = Some(Instant::now());
    }

    /// Milliseconds since a blur last hid the panel.
    pub fn ms_since_blur_hide(&self) -> Option<u64> {
        self.lock()
            .blur_hidden_at
            .map(|at| u64::try_from(at.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    pub fn set_tray(&self, area: Area) {
        self.lock().tray = Some(area);
    }

    pub fn tray(&self) -> Option<Area> {
        self.lock().tray
    }
}

/// Sets the activation policy for the visible windows. Before a library or settings window
/// shows, so it comes to the front with its menu; after one hides.
fn apply_policy(app: &AppHandle, visible: &[WindowKind]) {
    #[cfg(target_os = "macos")]
    {
        let policy = match policy_for(visible) {
            Policy::Regular => tauri::ActivationPolicy::Regular,
            Policy::Accessory => tauri::ActivationPolicy::Accessory,
        };
        let _ = app.set_activation_policy(policy);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = (app, visible);
}

/// The sender of the [`visibility_worker`], managed by the app.
pub struct PollerVisibility(Mutex<Sender<bool>>);

impl PollerVisibility {
    /// Starts the worker: it reads [`Shown`] and tells the poller, off the main thread (a poll
    /// start reads locks the children's supervisors also hold).
    pub fn start(app: &AppHandle) -> Self {
        let read_app = app.clone();
        let apply_app = app.clone();
        Self(Mutex::new(visibility_worker(
            move || {
                read_app
                    .try_state::<Shown>()
                    .is_some_and(|shown| !shown.visible().is_empty())
            },
            move |visible, panel_opened| {
                if let Some(poller) = apply_app.try_state::<Poller>() {
                    poller.set_visible(visible);
                    if panel_opened {
                        poller.poll_now();
                    }
                }
            },
        )))
    }
}

/// Asks the worker to tell the poller what is visible now.
fn tell_poller(app: &AppHandle, panel_opened: bool) {
    if let Some(worker) = app.try_state::<PollerVisibility>() {
        let _ = worker
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .send(panel_opened);
    }
}

/// Shows the library or the settings window, made on first use. Order: Regular, show,
/// unminimize, focus.
pub fn show_window(app: &AppHandle, kind: WindowKind, opening: Opening) -> tauri::Result<()> {
    let window = match app.get_webview_window(kind.label()) {
        Some(window) => {
            if let Some(Tell::Emit(event, value)) = tell(kind, &opening, true) {
                let _ = window.emit_to(kind.label(), event, value);
            }
            window
        }
        None => build_window(app, kind, &opening)?,
    };
    let shown = app.state::<Shown>();
    let mut visible = shown.visible();
    visible.push(kind);
    apply_policy(app, &visible);
    window.show()?;
    window.unminimize()?;
    window.set_focus()?;
    shown.set(kind, true);
    tell_poller(app, false);
    tell_library(app, kind, true);
    tell_notes(app);
    Ok(())
}

fn build_window(
    app: &AppHandle,
    kind: WindowKind,
    opening: &Opening,
) -> tauri::Result<WebviewWindow> {
    let builder =
        WebviewWindowBuilder::new(app, kind.label(), WebviewUrl::App("index.html".into()))
            .visible(false);
    let builder = match kind {
        WindowKind::Settings => builder
            .title("Notedown Settings")
            .inner_size(720.0, 540.0)
            .min_inner_size(640.0, 460.0),
        _ => builder
            .title("Notedown Library")
            .inner_size(960.0, 640.0)
            .min_inner_size(640.0, 460.0),
    };
    let builder = match tell(kind, opening, false) {
        Some(Tell::Script(script)) => builder.initialization_script(script),
        _ => builder,
    };
    builder.build()
}

/// Hides a window and sets the policy for what is still visible. Order: hide, then compute.
pub fn hide_window(app: &AppHandle, kind: WindowKind) {
    if let Some(window) = app.get_webview_window(kind.label()) {
        let _ = window.hide();
    }
    let visible = app.state::<Shown>().set(kind, false);
    if kind == WindowKind::Library {
        // A hidden library is not in front, whatever focus event comes or not.
        crate::save_markdown::library_focused(app, false);
    }
    apply_policy(app, &visible);
    tell_poller(app, false);
    tell_library(app, kind, false);
    tell_notes(app);
}

/// Tells the library's follower that its window was shown or hidden: it follows the changes
/// only while the window is visible. It never blocks (a flag and a wake of its thread).
fn tell_library(app: &AppHandle, kind: WindowKind, visible: bool) {
    if kind == WindowKind::Library
        && let Some(library) = app.try_state::<crate::library::Library>()
    {
        library.set_visible(visible);
    }
    // A hidden library shows no review: its renders stop (short locks and signals only).
    if kind == WindowKind::Library
        && !visible
        && let Some(review) = app.try_state::<crate::review::Review>()
    {
        review.close_all();
    }
}

/// Tells the notes index whether a window that shows notes (the panel, the library) is
/// visible: it scans the store only then. It never blocks (a flag and a wake of its thread).
fn tell_notes(app: &AppHandle) {
    if let Some(notes) = app.try_state::<crate::notes::Notes>() {
        let shown = app.state::<Shown>();
        notes.set_visible(
            shown.is_visible(WindowKind::Panel) || shown.is_visible(WindowKind::Library),
        );
    }
}

/// Hides every window (Quit).
pub fn hide_all(app: &AppHandle) {
    for kind in WindowKind::ALL {
        hide_window(app, kind);
    }
}

/// A left click on the tray icon, at `tray` (physical pixels).
pub fn tray_clicked(app: &AppHandle, tray: Area) {
    let shown = app.state::<Shown>();
    shown.set_tray(tray);
    let panel_visible = shown.is_visible(WindowKind::Panel);
    match tray::tray_click(panel_visible, shown.ms_since_blur_hide()) {
        PanelAction::Show => open_panel(app, Some(tray), cursor_points(app)),
        PanelAction::Hide => hide_window(app, WindowKind::Panel),
        PanelAction::Ignore => {}
    }
}

/// Shows the panel under the tray icon (a second launch): where the icon is now, else where
/// it was at the last click.
pub fn show_panel(app: &AppHandle) {
    let tray = tray::tray_area(app).or_else(|| app.state::<Shown>().tray());
    open_panel(app, tray, None);
}

/// Shows the panel under `tray` and looks at the status at once. `cursor` (logical points)
/// names the screen of a click.
fn open_panel(app: &AppHandle, tray: Option<Area>, cursor: Option<(f64, f64)>) {
    let Some(panel) = app.get_webview_window(WindowKind::Panel.label()) else {
        return;
    };
    if let Some(area) = tray {
        place_panel(app, &panel, area, cursor);
    }
    let _ = panel.show();
    let _ = panel.set_focus();
    app.state::<Shown>().set(WindowKind::Panel, true);
    tell_poller(app, true);
    tell_notes(app);
    crate::reprobe_outside(app);
}

/// The cursor in global logical points. tao gives it scaled by the primary screen, whichever
/// screen it is on, so this undoes one known scale.
fn cursor_points(app: &AppHandle) -> Option<(f64, f64)> {
    let cursor = app.cursor_position().ok()?;
    let scale = app.primary_monitor().ok()??.scale_factor();
    Some((cursor.x / scale, cursor.y / scale))
}

/// Places the panel in logical points: every screen has its own scale, so physical values of
/// two screens do not compare.
fn place_panel(app: &AppHandle, panel: &WebviewWindow, area: Area, cursor: Option<(f64, f64)>) {
    let screens: Vec<Screen> = app
        .available_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|m| Screen {
            physical: Area {
                x: f64::from(m.position().x),
                y: f64::from(m.position().y),
                w: f64::from(m.size().width),
                h: f64::from(m.size().height),
            },
            scale: m.scale_factor(),
        })
        .collect();
    // No screen fits: the panel opens where it was last.
    if let Some((x, y)) = tray::panel_place(area, cursor, PANEL_WIDTH, &screens) {
        let _ = panel.set_position(tauri::LogicalPosition::new(x, y));
    }
}

/// The window events of the app: closing a window hides it (a menu bar app never quits on a
/// close), the panel hides when it loses the focus, and files dropped on the library are kept
/// for an upload.
pub fn on_window_event(window: &tauri::Window, event: &WindowEvent) {
    let Some(kind) = WindowKind::from_label(window.label()) else {
        return;
    };
    let app = window.app_handle();
    match event {
        WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            hide_window(app, kind);
        }
        // Files dropped on the library: Rust keeps their paths, the window gets a drop id. A
        // short lock and a channel send, so the main thread never waits.
        WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, position })
            if kind == WindowKind::Library =>
        {
            if let Some(library) = app.try_state::<crate::library::Library>() {
                // wry on macOS gives points of the web view; the event says physical pixels.
                let scale = if cfg!(target_os = "macos") {
                    window.scale_factor().unwrap_or(1.0)
                } else {
                    1.0
                };
                let (x, y) = drop_pixels(position.x, position.y, scale);
                library.dropped(paths.clone(), x, y);
            }
        }
        // Save Markdown in the File menu follows whether the library is in front.
        WindowEvent::Focused(front) if kind == WindowKind::Library => {
            crate::save_markdown::library_focused(app, *front);
        }
        WindowEvent::Focused(false) if kind == WindowKind::Panel => {
            let shown = app.state::<Shown>();
            if shown.is_visible(WindowKind::Panel) {
                shown.blur_hidden();
                hide_window(app, WindowKind::Panel);
            }
        }
        _ => {}
    }
}

/// A drop position in points of the web view as physical pixels of the web view.
fn drop_pixels(x: f64, y: f64, scale: f64) -> (f64, f64) {
    (x * scale, y * scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use WindowKind::*;

    #[test]
    fn a_drop_position_in_points_becomes_physical_pixels() {
        // wry on macOS gives the drop in points of the web view (draggingLocation, flipped
        // by the view's height), though the type says physical pixels.
        assert_eq!(drop_pixels(120.4, 48.0, 2.0), (240.8, 96.0));
        assert_eq!(drop_pixels(120.0, 48.0, 1.0), (120.0, 48.0));
    }

    #[test]
    fn policy_follows_the_visible_windows() {
        assert_eq!(policy_for(&[]), Policy::Accessory);
        assert_eq!(policy_for(&[Panel]), Policy::Accessory);
        assert_eq!(policy_for(&[Library]), Policy::Regular);
        assert_eq!(policy_for(&[Settings]), Policy::Regular);
        assert_eq!(policy_for(&[Panel, Settings]), Policy::Regular);
        assert_eq!(policy_for(&[Library, Settings]), Policy::Regular);
    }

    #[test]
    fn the_poller_gets_the_visibility_of_the_moment_in_order() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex, mpsc};

        let visible = Arc::new(AtomicBool::new(true));
        let applied = Arc::new(Mutex::new(Vec::new()));
        let (gate_tx, gate_rx) = mpsc::channel::<()>();
        let (seen_tx, seen_rx) = mpsc::channel::<()>();
        let gate = Mutex::new(gate_rx);
        let read_visible = visible.clone();
        let log = applied.clone();
        let tx = visibility_worker(
            move || read_visible.load(Ordering::SeqCst),
            move |v, opened| {
                log.lock().unwrap().push((v, opened));
                seen_tx.send(()).unwrap();
                // The first apply waits, as a slow poll start would.
                if opened {
                    gate.lock().unwrap().recv().unwrap();
                }
            },
        );
        // The panel opens, then a blur hides it before the first call is through.
        tx.send(true).unwrap();
        seen_rx.recv_timeout(crate::test_procs::HANG_GUARD).unwrap();
        visible.store(false, Ordering::SeqCst);
        tx.send(false).unwrap();
        gate_tx.send(()).unwrap();
        seen_rx.recv_timeout(crate::test_procs::HANG_GUARD).unwrap();
        assert_eq!(*applied.lock().unwrap(), vec![(true, true), (false, false)]);
    }

    #[test]
    fn open_library_at_a_note_tells_the_window() {
        let at = Opening::At(LibraryAt::Note {
            path: "/Note/Work/Plan \"A\" </script>.note".into(),
        });
        // A new window learns it from its init script, as JSON (quotes and tags escaped).
        let Some(Tell::Script(script)) = tell(Library, &at, false) else {
            panic!("no init script");
        };
        assert_eq!(
            script,
            r#"window.__NOTEDOWN_AT__ = {"at":"note","path":"/Note/Work/Plan \"A\" </script>.note"};"#
        );
        // An open window gets an event.
        assert_eq!(
            tell(Library, &at, true),
            Some(Tell::Emit(
                LIBRARY_AT_EVENT,
                serde_json::json!({"at": "note", "path": "/Note/Work/Plan \"A\" </script>.note"})
            ))
        );
        // "N more" opens it at New.
        assert_eq!(
            tell(Library, &Opening::At(LibraryAt::New), true),
            Some(Tell::Emit(
                LIBRARY_AT_EVENT,
                serde_json::json!({"at": "new"})
            ))
        );
        // The setup opens it at the recycle bin.
        assert_eq!(
            tell(Library, &Opening::At(LibraryAt::Bin), true),
            Some(Tell::Emit(
                LIBRARY_AT_EVENT,
                serde_json::json!({"at": "bin"})
            ))
        );
        // A click on a sync notification opens it at All notes.
        assert_eq!(
            tell(Library, &Opening::At(LibraryAt::All), true),
            Some(Tell::Emit(
                LIBRARY_AT_EVENT,
                serde_json::json!({"at": "all"})
            ))
        );
        assert_eq!(
            tell(Library, &Opening::At(LibraryAt::All), false),
            Some(Tell::Script(
                r#"window.__NOTEDOWN_AT__ = {"at":"all"};"#.into()
            ))
        );
        // Open Library keeps an open window as it is; a new one starts where it starts.
        assert_eq!(tell(Library, &Opening::Plain, true), None);
        assert_eq!(
            tell(Library, &Opening::Plain, false),
            Some(Tell::Script("window.__NOTEDOWN_AT__ = null;".into()))
        );
        // The settings window keeps its page.
        let page = Opening::Page("setup".into());
        assert_eq!(
            tell(Settings, &page, false),
            Some(Tell::Script(
                r#"window.__NOTEDOWN_PAGE__ = "setup";"#.into()
            ))
        );
        assert_eq!(
            tell(Settings, &Opening::Plain, false),
            Some(Tell::Script("window.__NOTEDOWN_PAGE__ = null;".into()))
        );
        assert_eq!(
            tell(Settings, &page, true),
            Some(Tell::Emit(SETTINGS_PAGE_EVENT, serde_json::json!("setup")))
        );
        assert_eq!(tell(Settings, &Opening::Plain, true), None);
        // A page is no place in the library, a note none in the settings.
        assert_eq!(tell(Library, &page, true), None);
        assert_eq!(tell(Settings, &at, true), None);
        assert_eq!(tell(Panel, &at, false), None);
    }

    #[test]
    fn open_library_opens_at_the_note_at_new_or_as_it_is() {
        assert_eq!(
            library_opening(Some("NOTE/Note/A.note".into()), Some(true)),
            Opening::At(LibraryAt::Note {
                path: "NOTE/Note/A.note".into()
            })
        );
        assert_eq!(
            library_opening(None, Some(true)),
            Opening::At(LibraryAt::New)
        );
        assert_eq!(library_opening(None, Some(false)), Opening::Plain);
        assert_eq!(library_opening(None, None), Opening::Plain);
    }

    #[test]
    fn a_click_on_a_notice_opens_its_window_and_place() {
        use crate::notify::Target;
        assert_eq!(
            opening_for(&Target::Library(LibraryAt::All)),
            (Library, Opening::At(LibraryAt::All))
        );
        assert_eq!(
            opening_for(&Target::Library(LibraryAt::New)),
            (Library, Opening::At(LibraryAt::New))
        );
        assert_eq!(
            opening_for(&Target::Library(LibraryAt::Note {
                path: "NOTE/Note/A.note".into()
            })),
            (
                Library,
                Opening::At(LibraryAt::Note {
                    path: "NOTE/Note/A.note".into()
                })
            )
        );
        for page in ["agents", "server", "ocr", "supernote"] {
            assert_eq!(
                opening_for(&Target::Settings(page.into())),
                (Settings, Opening::Page(page.into()))
            );
        }
    }

    #[test]
    fn labels_round_trip() {
        for kind in WindowKind::ALL {
            assert_eq!(WindowKind::from_label(kind.label()), Some(kind));
        }
        assert_eq!(WindowKind::from_label("main"), None);
    }
}
