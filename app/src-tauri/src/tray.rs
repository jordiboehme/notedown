//! The menu bar icon and the panel under it.

use tauri::image::Image;
use tauri::menu::{IsMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager, Rect, Runtime, Wry};

use crate::pause::{self, OcrPause, PauseChoice};
use crate::status::Attention;
use crate::windows;

/// The id of the one tray icon.
pub const TRAY_ID: &str = "main";

/// Menu item ids. The app menu's own Quit uses [`QUIT_ID`] too.
pub const OPEN_LIBRARY_ID: &str = "open-library";
pub const SETTINGS_ID: &str = "settings";
pub const CHECK_UPDATES_ID: &str = "check-updates";
pub const QUIT_ID: &str = "quit";
/// The Pause OCR submenu, and the lines while a pause holds.
pub const PAUSE_1M_ID: &str = "pause-1m";
pub const PAUSE_5M_ID: &str = "pause-5m";
pub const PAUSE_1H_ID: &str = "pause-1h";
pub const PAUSE_UNTIL_RESUMED_ID: &str = "pause-until-resumed";
pub const RESUME_OCR_ID: &str = "resume-ocr";
/// The disabled line that says until when the OCR is paused.
pub const PAUSE_STATUS_ID: &str = "pause-status";

/// One entry of the tray menu, before it is made into a native one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayEntry {
    Item {
        id: String,
        text: String,
        enabled: bool,
    },
    Submenu {
        text: String,
        items: Vec<TrayEntry>,
    },
    Separator,
}

fn entry(id: &str, text: &str, enabled: bool) -> TrayEntry {
    TrayEntry::Item {
        id: id.to_owned(),
        text: text.to_owned(),
        enabled,
    }
}

/// The tray menu with the OCR pause `pause` at `now_ms`. While it holds, a disabled line says
/// until when (`clock` gives the clock time of its end) and Resume OCR Now ends it. The Pause
/// OCR submenu is always there, also to change a pause.
pub fn tray_entries(
    pause: Option<&OcrPause>,
    now_ms: u64,
    clock: impl Fn(u64) -> String,
) -> Vec<TrayEntry> {
    let mut entries = vec![
        entry(OPEN_LIBRARY_ID, "Open Library", true),
        entry(SETTINGS_ID, "Settings...", true),
        entry(CHECK_UPDATES_ID, "Check for Updates...", true),
        TrayEntry::Separator,
    ];
    if let Some(p) = pause.filter(|p| pause::active(Some(p), now_ms)) {
        entries.push(entry(PAUSE_STATUS_ID, &pause::pause_line(p, clock), false));
        entries.push(entry(RESUME_OCR_ID, "Resume OCR Now", true));
    }
    entries.extend([
        TrayEntry::Submenu {
            text: "Pause OCR".to_owned(),
            items: pause_items(),
        },
        TrayEntry::Separator,
        entry(QUIT_ID, "Quit Notedown", true),
    ]);
    entries
}

/// The items of the Pause OCR menu: the tray's submenu and the panel's menu
/// (`commands::panel_pause_menu`). [`on_menu_event`] pauses for the one picked.
pub fn pause_items() -> Vec<TrayEntry> {
    vec![
        entry(PAUSE_1M_ID, "For 1 Minute", true),
        entry(PAUSE_5M_ID, "For 5 Minutes", true),
        entry(PAUSE_1H_ID, "For 1 Hour", true),
        entry(PAUSE_UNTIL_RESUMED_ID, "Until I Resume", true),
    ]
}

/// The pause a menu item id picks.
pub fn pause_choice(id: &str) -> Option<PauseChoice> {
    match id {
        PAUSE_1M_ID => Some(PauseChoice::OneMinute),
        PAUSE_5M_ID => Some(PauseChoice::FiveMinutes),
        PAUSE_1H_ID => Some(PauseChoice::OneHour),
        PAUSE_UNTIL_RESUMED_ID => Some(PauseChoice::UntilResumed),
        _ => None,
    }
}

// The 2x images: tray-icon draws any image 18 pt high, so these stay sharp on a Retina
// screen. `app/scripts/tray-icons.sh` renders them from docs/assets/icon-menubar.svg.
const NORMAL_PNG: &[u8] = include_bytes!("../icons/tray/icon@2x.png");
const ATTENTION_PNG: &[u8] = include_bytes!("../icons/tray/attention@2x.png");

/// Which tray image to show.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayIconKind {
    Normal,
    /// The image with the dot: something needs a look.
    Attention,
}

/// The tray image for the attention list of the status.
pub fn tray_icon_for(attention: &[Attention]) -> TrayIconKind {
    if attention.is_empty() {
        TrayIconKind::Normal
    } else {
        TrayIconKind::Attention
    }
}

/// What a left click on the tray icon does with the panel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelAction {
    Show,
    Hide,
    /// Nothing: the click was the one that took the focus away and hid the panel.
    Ignore,
}

/// A click within this time after a blur hid the panel was that blur's own click.
pub const BLUR_GRACE_MS: u64 = 300;

/// The action for a left click. `ms_since_blur_hide` is the time since a blur last hid the
/// panel, `None` when it never did.
pub fn tray_click(panel_visible: bool, ms_since_blur_hide: Option<u64>) -> PanelAction {
    if panel_visible {
        PanelAction::Hide
    } else if ms_since_blur_hide.is_some_and(|ms| ms < BLUR_GRACE_MS) {
        PanelAction::Ignore
    } else {
        PanelAction::Show
    }
}

/// A rectangle in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Area {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The top left corner of the panel (`panel_w` wide), centred under the tray icon and kept
/// inside the screen. All in physical pixels.
pub fn panel_position(tray: Area, panel_w: f64, screen: Area) -> (f64, f64) {
    let centred = tray.x + tray.w / 2.0 - panel_w / 2.0;
    let right = screen.x + screen.w - panel_w;
    let x = centred.min(right).max(screen.x);
    (x.round(), (tray.y + tray.h).round())
}

impl TrayIconKind {
    fn image(self) -> tauri::Result<Image<'static>> {
        Image::from_bytes(match self {
            TrayIconKind::Normal => NORMAL_PNG,
            TrayIconKind::Attention => ATTENTION_PNG,
        })
    }
}

/// The native items of `entries`.
pub(crate) fn render(
    app: &AppHandle,
    entries: &[TrayEntry],
) -> tauri::Result<Vec<Box<dyn IsMenuItem<Wry>>>> {
    entries
        .iter()
        .map(|e| -> tauri::Result<Box<dyn IsMenuItem<Wry>>> {
            Ok(match e {
                TrayEntry::Item { id, text, enabled } => Box::new(MenuItem::with_id(
                    app,
                    id.as_str(),
                    text,
                    *enabled,
                    None::<&str>,
                )?),
                TrayEntry::Submenu { text, items } => {
                    let items = render(app, items)?;
                    let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(|i| &**i).collect();
                    Box::new(Submenu::with_items(app, text, true, &refs)?)
                }
                TrayEntry::Separator => Box::new(PredefinedMenuItem::separator(app)?),
            })
        })
        .collect()
}

/// The tray menu as the config's pause is now.
fn menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    // The setup builds the icon before the config is managed; the menu is set again after.
    let pause = app
        .try_state::<crate::commands::ConfigState>()
        .and_then(|c| c.current().ocr_pause);
    let now = app
        .try_state::<std::sync::Arc<crate::processes::Processes>>()
        .map_or_else(|| (crate::supervisor::system_clock())(), |p| p.now_ms());
    let items = render(app, &tray_entries(pause.as_ref(), now, pause::local_clock))?;
    let refs: Vec<&dyn IsMenuItem<Wry>> = items.iter().map(|i| &**i).collect();
    Menu::with_items(app, &refs)
}

/// Builds the tray menu again: after every change of the OCR pause, its end included.
pub fn set_menu(app: &AppHandle) {
    if let Some(tray) = app.tray_by_id(TRAY_ID)
        && let Ok(menu) = menu(app)
    {
        let _ = tray.set_menu(Some(menu));
    }
}

/// Builds the menu bar icon: a left click toggles the panel, a right click opens the menu.
pub fn build(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let menu = menu(app)?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(TrayIconKind::Normal.image()?)
        .icon_as_template(true)
        .tooltip("Notedown")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                rect,
                ..
            } = event
            {
                windows::tray_clicked(tray.app_handle(), area_of(&rect));
            }
        })
        .build(app)
}

/// Shows `kind` on the tray icon.
pub fn set_icon(app: &AppHandle, kind: TrayIconKind) {
    if let Some(tray) = app.tray_by_id(TRAY_ID)
        && let Ok(image) = kind.image()
    {
        let _ = tray.set_icon_with_as_template(Some(image), true);
    }
}

/// Where the tray icon is now, in physical pixels.
pub fn tray_area<R: Runtime>(app: &AppHandle<R>) -> Option<Area> {
    let rect = app.tray_by_id(TRAY_ID)?.rect().ok()??;
    Some(area_of(&rect))
}

fn area_of(rect: &Rect) -> Area {
    // tray-icon gives physical values already; the scale only matters for logical ones.
    let position = rect.position.to_physical::<f64>(1.0);
    let size = rect.size.to_physical::<f64>(1.0);
    Area {
        x: position.x,
        y: position.y,
        w: size.width,
        h: size.height,
    }
}

/// The menu bar items: the tray menu and the app menu's own Quit.
pub fn on_menu_event(app: &AppHandle, id: &str) {
    match id {
        OPEN_LIBRARY_ID => {
            let _ =
                windows::show_window(app, windows::WindowKind::Library, windows::Opening::Plain);
        }
        SETTINGS_ID => {
            let _ =
                windows::show_window(app, windows::WindowKind::Settings, windows::Opening::Plain);
        }
        CHECK_UPDATES_ID => crate::check_update_from_menu(app),
        // Off the main thread: the config is saved with an fsync. The menu is built again
        // when the pause changed.
        RESUME_OCR_ID => {
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(e) = crate::commands::resume_ocr(&app) {
                    eprintln!("notedown: the OCR pause did not end: {e}");
                }
            });
        }
        _ if pause_choice(id).is_some() => {
            let (app, id) = (app.clone(), id.to_owned());
            std::thread::spawn(move || {
                let control = crate::commands::AppPause {
                    app: &app,
                    window: None,
                };
                if let Some(Err(e)) = crate::commands::pause_pick_with(&control, &id) {
                    eprintln!("notedown: the OCR was not paused: {e}");
                }
            });
        }
        QUIT_ID => crate::request_quit(app),
        crate::save_markdown::MENU_ID => crate::save_markdown::on_menu(app),
        _ if id.starts_with(crate::menus::CONTEXT_PREFIX) => crate::menus::on_context(app, id),
        _ => {}
    }
}

/// The app menu that macOS shows while a window makes the app Regular, with its native Quit
/// replaced by ours: the native one ends the process without the event loop, so the children
/// would not be stopped first. The File submenu gets "Save Markdown...", whose item is kept in
/// [`crate::save_markdown::SaveMenuItem`].
pub fn app_menu(app: &AppHandle) -> tauri::Result<Menu<tauri::Wry>> {
    use tauri::menu::MenuItemKind;
    let menu = Menu::default(app)?;
    // The app submenu comes first and its last item is the native Quit (Menu::default in
    // tauri 2.12); it is removed only when its text says Quit. A changed layout leaves two
    // Quit items, which the manual check shows.
    if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.first() {
        if let Some(MenuItemKind::Predefined(native_quit)) = app_menu.items()?.last()
            && is_quit_text(&native_quit.text()?)
        {
            app_menu.remove(native_quit)?;
        }
        app_menu.append(&MenuItem::with_id(
            app,
            QUIT_ID,
            "Quit Notedown",
            true,
            Some("CmdOrCtrl+Q"),
        )?)?;
    }
    let save = crate::save_markdown::add_to_file_menu(app, &menu)?;
    if let Some(kept) = app.try_state::<crate::save_markdown::SaveMenuItem>() {
        *kept
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(save);
    }
    Ok(menu)
}

/// A screen as tao reports it on macOS: position and size are its logical (point) values
/// times its own scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Screen {
    pub physical: Area,
    pub scale: f64,
}

impl Screen {
    /// The screen in global logical points, the space every screen shares.
    pub fn logical(&self) -> Area {
        let p = self.physical;
        Area {
            x: p.x / self.scale,
            y: p.y / self.scale,
            w: p.w / self.scale,
            h: p.h / self.scale,
        }
    }
}

/// The height of a menu bar in points: 24 on most screens, 37 under a notch.
const MENU_BAR_POINTS: std::ops::RangeInclusive<f64> = 20.0..=40.0;

/// Where the panel goes, in logical points: centred under the tray icon on the screen it is
/// on. `tray` is physical, scaled by its own screen, which is not known; `cursor` (logical
/// points, at the click) names that screen. Without a cursor the screens whose logical area
/// holds the scaled-down icon are candidates, and the one where the icon is as high as a menu
/// bar wins (else the first). `None` when no screen fits.
pub fn panel_place(
    tray: Area,
    cursor: Option<(f64, f64)>,
    panel_w: f64,
    screens: &[Screen],
) -> Option<(f64, f64)> {
    let holds =
        |a: Area, (x, y): (f64, f64)| x >= a.x && x < a.x + a.w && y >= a.y && y < a.y + a.h;
    let scaled = |s: &Screen| Area {
        x: tray.x / s.scale,
        y: tray.y / s.scale,
        w: tray.w / s.scale,
        h: tray.h / s.scale,
    };
    let screen = match cursor {
        Some(point) => screens.iter().find(|s| holds(s.logical(), point)),
        None => {
            let candidates: Vec<&Screen> = screens
                .iter()
                .filter(|s| {
                    let icon = scaled(s);
                    holds(s.logical(), (icon.x + icon.w / 2.0, icon.y + icon.h / 2.0))
                })
                .collect();
            // Read at the wrong scale, the icon is not as high as a menu bar.
            candidates
                .iter()
                .find(|s| MENU_BAR_POINTS.contains(&scaled(s).h))
                .or(candidates.first())
                .copied()
        }
    }?;
    Some(panel_position(scaled(screen), panel_w, screen.logical()))
}

/// The native Quit of the app menu, by its text ("Quit Notedown" on macOS, "&Quit" elsewhere).
pub fn is_quit_text(text: &str) -> bool {
    text.trim_start_matches('&').starts_with("Quit")
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 2x laptop (1512x982 points) and a 1x display of 1920x1080 points to its right.
    fn two_screens() -> [Screen; 2] {
        [
            Screen {
                physical: Area {
                    x: 0.0,
                    y: 0.0,
                    w: 3024.0,
                    h: 1964.0,
                },
                scale: 2.0,
            },
            Screen {
                physical: Area {
                    x: 1512.0,
                    y: 0.0,
                    w: 1920.0,
                    h: 1080.0,
                },
                scale: 1.0,
            },
        ]
    }

    #[test]
    fn the_panel_is_placed_in_points_on_screens_of_different_scales() {
        let screens = two_screens();
        assert_eq!(
            screens[0].logical(),
            Area {
                x: 0.0,
                y: 0.0,
                w: 1512.0,
                h: 982.0
            }
        );
        assert_eq!(
            screens[1].logical(),
            Area {
                x: 1512.0,
                y: 0.0,
                w: 1920.0,
                h: 1080.0
            }
        );
        // On the laptop: the icon at 1000 points is 2000 physical.
        let laptop = Area {
            x: 2000.0,
            y: 0.0,
            w: 48.0,
            h: 48.0,
        };
        assert_eq!(
            panel_place(laptop, Some((1010.0, 10.0)), 340.0, &screens),
            Some((842.0, 24.0))
        );
        assert_eq!(
            panel_place(laptop, None, 340.0, &screens),
            Some((842.0, 24.0))
        );
        // Near the laptop's right edge: kept on the laptop.
        let edge = Area {
            x: 2960.0,
            y: 0.0,
            w: 48.0,
            h: 48.0,
        };
        assert_eq!(
            panel_place(edge, Some((1490.0, 10.0)), 340.0, &screens),
            Some((1172.0, 24.0))
        );
        // On the 1x display at 3000 points: 3000 physical, which halved would also be on the
        // laptop. The cursor says which screen it is.
        let display = Area {
            x: 3000.0,
            y: 0.0,
            w: 24.0,
            h: 24.0,
        };
        assert_eq!(
            panel_place(display, Some((3010.0, 10.0)), 340.0, &screens),
            Some((2842.0, 24.0))
        );
        // Without a cursor, a place only the display holds; kept inside it on the right.
        let far = Area {
            x: 3300.0,
            y: 0.0,
            w: 24.0,
            h: 24.0,
        };
        assert_eq!(
            panel_place(far, None, 340.0, &screens),
            Some((3092.0, 24.0))
        );
        // Nothing fits.
        assert_eq!(panel_place(far, None, 340.0, &[]), None);
    }

    #[test]
    fn a_second_launch_finds_the_screen_by_the_menu_bar_height() {
        let screens = two_screens();
        // On the display at 3000 points: halved, its centre lies on the laptop too, but
        // there the icon would be 12 points high, too low for a menu bar.
        let display = Area {
            x: 3000.0,
            y: 0.0,
            w: 24.0,
            h: 24.0,
        };
        assert_eq!(
            panel_place(display, None, 340.0, &screens),
            Some((2842.0, 24.0))
        );
        // A 1x display below the laptop, at (0, 982) in points.
        let below = [
            screens[0],
            Screen {
                physical: Area {
                    x: 0.0,
                    y: 982.0,
                    w: 1920.0,
                    h: 1080.0,
                },
                scale: 1.0,
            },
        ];
        let tray = Area {
            x: 1000.0,
            y: 982.0,
            w: 24.0,
            h: 24.0,
        };
        assert_eq!(
            panel_place(tray, None, 340.0, &below),
            Some((842.0, 1006.0))
        );
        // A notch laptop (a 37 point menu bar) still counts as a menu bar.
        let notch = Area {
            x: 2000.0,
            y: 0.0,
            w: 48.0,
            h: 74.0,
        };
        assert_eq!(
            panel_place(notch, None, 340.0, &screens),
            Some((842.0, 37.0))
        );
    }

    const NOW: u64 = 1_763_300_000_000;

    fn item(id: &str, text: &str, enabled: bool) -> TrayEntry {
        TrayEntry::Item {
            id: id.to_owned(),
            text: text.to_owned(),
            enabled,
        }
    }

    /// The menu with `status` (the pause lines) between the first separator and the submenu.
    fn menu_with(status: Vec<TrayEntry>) -> Vec<TrayEntry> {
        let mut entries = vec![
            item(OPEN_LIBRARY_ID, "Open Library", true),
            item(SETTINGS_ID, "Settings...", true),
            item(CHECK_UPDATES_ID, "Check for Updates...", true),
            TrayEntry::Separator,
        ];
        entries.extend(status);
        entries.extend([
            TrayEntry::Submenu {
                text: "Pause OCR".to_owned(),
                items: vec![
                    item(PAUSE_1M_ID, "For 1 Minute", true),
                    item(PAUSE_5M_ID, "For 5 Minutes", true),
                    item(PAUSE_1H_ID, "For 1 Hour", true),
                    item(PAUSE_UNTIL_RESUMED_ID, "Until I Resume", true),
                ],
            },
            TrayEntry::Separator,
            item(QUIT_ID, "Quit Notedown", true),
        ]);
        entries
    }

    fn clock(ms: u64) -> String {
        assert_eq!(ms, NOW + 60_000);
        "14:32".to_owned()
    }

    #[test]
    fn the_menu_without_a_pause_offers_one() {
        assert_eq!(tray_entries(None, NOW, clock), menu_with(Vec::new()));
        // A pause whose time came is over.
        let over = OcrPause {
            until_ms: Some(NOW),
        };
        assert_eq!(tray_entries(Some(&over), NOW, clock), menu_with(Vec::new()));
    }

    #[test]
    fn the_menu_of_a_timed_pause_says_until_when() {
        let timed = OcrPause {
            until_ms: Some(NOW + 60_000),
        };
        assert_eq!(
            tray_entries(Some(&timed), NOW, clock),
            menu_with(vec![
                item(PAUSE_STATUS_ID, "OCR paused until 14:32", false),
                item(RESUME_OCR_ID, "Resume OCR Now", true),
            ])
        );
    }

    #[test]
    fn the_menu_of_a_pause_until_resumed() {
        let open = OcrPause { until_ms: None };
        assert_eq!(
            tray_entries(Some(&open), NOW, |_| unreachable!()),
            menu_with(vec![
                item(PAUSE_STATUS_ID, "OCR paused", false),
                item(RESUME_OCR_ID, "Resume OCR Now", true),
            ])
        );
    }

    #[test]
    fn the_panels_pause_menu_is_the_tray_submenu() {
        // The panel's Pause OCR menu has the tray's items and ids: a pick goes through
        // on_menu_event to pause_ocr, as one in the tray menu does.
        let submenu = tray_entries(None, NOW, clock)
            .into_iter()
            .find_map(|e| match e {
                TrayEntry::Submenu { text, items } if text == "Pause OCR" => Some(items),
                _ => None,
            })
            .unwrap();
        assert_eq!(pause_items(), submenu);
        let picks: Vec<_> = pause_items()
            .iter()
            .map(|e| match e {
                TrayEntry::Item { id, text, enabled } => {
                    assert!(enabled, "{text}");
                    (text.clone(), pause_choice(id))
                }
                other => panic!("not an item: {other:?}"),
            })
            .collect();
        assert_eq!(
            picks,
            [
                ("For 1 Minute".to_owned(), Some(PauseChoice::OneMinute)),
                ("For 5 Minutes".to_owned(), Some(PauseChoice::FiveMinutes)),
                ("For 1 Hour".to_owned(), Some(PauseChoice::OneHour)),
                ("Until I Resume".to_owned(), Some(PauseChoice::UntilResumed)),
            ]
        );
    }

    #[test]
    fn the_pause_ids_are_the_choices() {
        assert_eq!(pause_choice(PAUSE_1M_ID), Some(PauseChoice::OneMinute));
        assert_eq!(pause_choice(PAUSE_5M_ID), Some(PauseChoice::FiveMinutes));
        assert_eq!(pause_choice(PAUSE_1H_ID), Some(PauseChoice::OneHour));
        assert_eq!(
            pause_choice(PAUSE_UNTIL_RESUMED_ID),
            Some(PauseChoice::UntilResumed)
        );
        assert_eq!(pause_choice(RESUME_OCR_ID), None);
        assert_eq!(pause_choice(QUIT_ID), None);
        assert_eq!(
            [
                PAUSE_1M_ID,
                PAUSE_5M_ID,
                PAUSE_1H_ID,
                PAUSE_UNTIL_RESUMED_ID,
                RESUME_OCR_ID
            ],
            [
                "pause-1m",
                "pause-5m",
                "pause-1h",
                "pause-until-resumed",
                "resume-ocr"
            ]
        );
    }

    #[test]
    fn only_the_quit_item_is_taken_for_the_native_quit() {
        assert!(is_quit_text("Quit Notedown"));
        assert!(is_quit_text("&Quit"));
        assert!(!is_quit_text("Hide Others"));
        assert!(!is_quit_text("Services"));
    }

    #[test]
    fn tray_icon_shows_attention() {
        assert_eq!(tray_icon_for(&[]), TrayIconKind::Normal);
        assert_eq!(
            tray_icon_for(&[Attention::OcrError]),
            TrayIconKind::Attention
        );
        assert_eq!(
            tray_icon_for(&[Attention::ServerUnreachable, Attention::OldSync]),
            TrayIconKind::Attention
        );
    }

    #[test]
    fn a_click_right_after_blur_does_not_reopen() {
        // The click on the icon took the focus away first; the blur hid the panel.
        assert_eq!(tray_click(false, Some(0)), PanelAction::Ignore);
        assert_eq!(tray_click(false, Some(120)), PanelAction::Ignore);
        assert_eq!(
            tray_click(false, Some(BLUR_GRACE_MS - 1)),
            PanelAction::Ignore
        );
        // Later clicks open it again.
        assert_eq!(tray_click(false, Some(BLUR_GRACE_MS)), PanelAction::Show);
        assert_eq!(tray_click(false, Some(5_000)), PanelAction::Show);
        assert_eq!(tray_click(false, None), PanelAction::Show);
        // A visible panel (it had no focus, so no blur) closes.
        assert_eq!(tray_click(true, None), PanelAction::Hide);
        assert_eq!(tray_click(true, Some(10)), PanelAction::Hide);
    }

    #[test]
    fn the_panel_sits_under_the_icon_inside_the_screen() {
        let screen = Area {
            x: 0.0,
            y: 0.0,
            w: 3024.0,
            h: 1964.0,
        };
        let icon = |x| Area {
            x,
            y: 0.0,
            w: 48.0,
            h: 48.0,
        };
        // Centred under the icon, right below the menu bar.
        assert_eq!(panel_position(icon(2000.0), 680.0, screen), (1684.0, 48.0));
        // Near the right edge: kept on the screen.
        assert_eq!(panel_position(icon(2950.0), 680.0, screen), (2344.0, 48.0));
        // Near the left edge.
        assert_eq!(panel_position(icon(10.0), 680.0, screen), (0.0, 48.0));
        // A second screen to the right.
        let second = Area {
            x: 3024.0,
            y: 0.0,
            w: 1920.0,
            h: 1080.0,
        };
        let on_second = Area {
            x: 4800.0,
            y: 0.0,
            w: 24.0,
            h: 24.0,
        };
        assert_eq!(panel_position(on_second, 340.0, second), (4604.0, 24.0));
    }
}
