//! The windows: the panel under the tray icon, the library and the settings. A menu bar app
//! shows a Dock icon only while the library or the settings window is visible.

use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use tauri::{
    AppHandle, Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent,
};

use crate::poller::Poller;
use crate::tray::{self, Area, PanelAction};

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

/// The event that tells an open settings window which page to show: a page name.
pub const SETTINGS_PAGE_EVENT: &str = "settings-page";

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

/// Tells the poller whether anything is visible, off the calling thread (a poll start reads
/// locks the children's supervisors also hold).
fn tell_poller(app: &AppHandle, visible: &[WindowKind], panel_opened: bool) {
    let any = !visible.is_empty();
    let app = app.clone();
    std::thread::spawn(move || {
        if let Some(poller) = app.try_state::<Poller>() {
            poller.set_visible(any);
            if panel_opened {
                poller.poll_now();
            }
        }
    });
}

/// Shows the library or the settings window, made on first use. Order: Regular, show,
/// unminimize, focus.
pub fn show_window(app: &AppHandle, kind: WindowKind, page: Option<String>) -> tauri::Result<()> {
    let window = match app.get_webview_window(kind.label()) {
        Some(window) => {
            if let Some(page) = &page {
                let _ = window.emit_to(kind.label(), SETTINGS_PAGE_EVENT, page);
            }
            window
        }
        None => build_window(app, kind, page.as_deref())?,
    };
    let shown = app.state::<Shown>();
    let mut visible = shown.visible();
    visible.push(kind);
    apply_policy(app, &visible);
    window.show()?;
    window.unminimize()?;
    window.set_focus()?;
    let visible = shown.set(kind, true);
    tell_poller(app, &visible, false);
    Ok(())
}

fn build_window(
    app: &AppHandle,
    kind: WindowKind,
    page: Option<&str>,
) -> tauri::Result<WebviewWindow> {
    let builder =
        WebviewWindowBuilder::new(app, kind.label(), WebviewUrl::App("index.html".into()))
            .visible(false);
    let builder = match kind {
        WindowKind::Settings => {
            let page = serde_json::to_string(&page).unwrap_or_else(|_| "null".into());
            builder
                .title("Notedown Settings")
                .inner_size(720.0, 540.0)
                .min_inner_size(640.0, 460.0)
                .initialization_script(format!("window.__NOTEDOWN_PAGE__ = {page};"))
        }
        _ => builder
            .title("Notedown Library")
            .inner_size(960.0, 640.0)
            .min_inner_size(640.0, 460.0),
    };
    builder.build()
}

/// Hides a window and sets the policy for what is still visible. Order: hide, then compute.
pub fn hide_window(app: &AppHandle, kind: WindowKind) {
    if let Some(window) = app.get_webview_window(kind.label()) {
        let _ = window.hide();
    }
    let visible = app.state::<Shown>().set(kind, false);
    apply_policy(app, &visible);
    tell_poller(app, &visible, false);
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
        PanelAction::Show => show_panel(app),
        PanelAction::Hide => hide_window(app, WindowKind::Panel),
        PanelAction::Ignore => {}
    }
}

/// Shows the panel under the tray icon and looks at the status at once.
pub fn show_panel(app: &AppHandle) {
    let Some(panel) = app.get_webview_window(WindowKind::Panel.label()) else {
        return;
    };
    let tray_area = app.state::<Shown>().tray().or_else(|| tray::tray_area(app));
    if let Some(area) = tray_area {
        place_panel(app, &panel, area);
    }
    let _ = panel.show();
    let _ = panel.set_focus();
    let visible = app.state::<Shown>().set(WindowKind::Panel, true);
    tell_poller(app, &visible, true);
    crate::reprobe_outside(app);
}

fn place_panel(app: &AppHandle, panel: &WebviewWindow, area: Area) {
    let scale = panel.scale_factor().unwrap_or(1.0);
    let centre = (area.x + area.w / 2.0, area.y + area.h / 2.0);
    let screen = app
        .available_monitors()
        .unwrap_or_default()
        .into_iter()
        .map(|m| Area {
            x: f64::from(m.position().x),
            y: f64::from(m.position().y),
            w: f64::from(m.size().width),
            h: f64::from(m.size().height),
        })
        .find(|s| {
            centre.0 >= s.x && centre.0 < s.x + s.w && centre.1 >= s.y && centre.1 < s.y + s.h
        });
    let screen = screen.unwrap_or(Area {
        x: f64::MIN / 4.0,
        y: 0.0,
        w: f64::MAX / 2.0,
        h: f64::MAX / 2.0,
    });
    let (x, y) = tray::panel_position(area, PANEL_WIDTH * scale, screen);
    let _ = panel.set_position(tauri::PhysicalPosition::new(x, y));
}

/// The window events of the app: closing a window hides it (a menu bar app never quits on a
/// close), and the panel hides when it loses the focus.
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

#[cfg(test)]
mod tests {
    use super::*;
    use WindowKind::*;

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
    fn labels_round_trip() {
        for kind in WindowKind::ALL {
            assert_eq!(WindowKind::from_label(kind.label()), Some(kind));
        }
        assert_eq!(WindowKind::from_label("main"), None);
    }
}
