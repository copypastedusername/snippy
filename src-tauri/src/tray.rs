//! System tray icon and menu.

use crate::config::{CaptureMode, Hotkeys};
use crate::{autostart, capture, commands, overlay, AppState};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{Manager, Wry};

const MODES: [(CaptureMode, &str); 4] = [
    (CaptureMode::Rectangle, "Rectangular snip"),
    (CaptureMode::Freeform, "Free-form snip"),
    (CaptureMode::Window, "Window snip"),
    (CaptureMode::Fullscreen, "Full-screen snip"),
];

/// Menu items whose text or state follows the configuration.
pub struct TrayHandles {
    modes: Vec<(CaptureMode, MenuItem<Wry>)>,
    autostart: CheckMenuItem<Wry>,
}

pub fn setup(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let config = app.state::<AppState>().config()?;
    let new_snip = MenuItem::with_id(app, "new_snip", "New snip", true, None::<&str>)?;
    let modes = MODES
        .iter()
        .map(|&(mode, label)| {
            let text = shortcut_label(label, binding(&config.hotkeys, mode));
            MenuItem::with_id(app, mode.as_str(), text, true, None::<&str>).map(|item| (mode, item))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let open_folder = MenuItem::with_id(
        app,
        "open_folder",
        "Open screenshots folder",
        true,
        None::<&str>,
    )?;
    let open_settings = MenuItem::with_id(app, "open_settings", "Settings", true, None::<&str>)?;
    let autostart_item = CheckMenuItem::with_id(
        app,
        "autostart",
        "Start with Windows",
        true,
        // Filled in from a worker thread: reading the logon task runs `schtasks`, which must not
        // hold up startup.
        false,
        None::<&str>,
    )?;
    let quit = MenuItem::with_id(app, "quit", "Quit Snippy", true, None::<&str>)?;

    let menu = Menu::new(app)?;
    menu.append(&new_snip)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    for (_, item) in &modes {
        menu.append(item)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&open_folder)?;
    menu.append(&open_settings)?;
    menu.append(&autostart_item)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&quit)?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| std::io::Error::other("Application icon is unavailable"))?;
    TrayIconBuilder::new()
        .icon(icon)
        .menu(&menu)
        .tooltip("Snippy")
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| on_menu(app, event.id.as_ref()))
        .on_tray_icon_event(|tray, event| {
            let activate = matches!(
                event,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                }
            );
            if activate {
                show_main(tray.app_handle());
            }
        })
        .build(app)?;

    *crate::lock(&app.state::<AppState>().tray, "tray")? = Some(TrayHandles {
        modes,
        autostart: autostart_item,
    });
    Ok(())
}

fn with_handles(app: &tauri::AppHandle, update: impl FnOnce(&TrayHandles)) {
    match crate::lock(&app.state::<AppState>().tray, "tray") {
        Ok(tray) => {
            if let Some(handles) = tray.as_ref() {
                update(handles);
            }
        }
        Err(error) => log::warn!("{error}"),
    }
}

/// Shows the current hotkeys next to the capture modes.
pub fn refresh_hotkeys(app: &tauri::AppHandle) {
    let Ok(config) = app.state::<AppState>().config() else {
        return;
    };
    with_handles(app, |handles| {
        for ((mode, item), (_, label)) in handles.modes.iter().zip(MODES) {
            let text = shortcut_label(label, binding(&config.hotkeys, *mode));
            if let Err(error) = item.set_text(text) {
                log::warn!("Could not update the tray menu: {error}");
            }
        }
    });
}

/// Re-reads the logon task (a `schtasks` call) and updates the check mark.
pub fn refresh_autostart(app: &tauri::AppHandle) {
    let enabled = autostart::is_autostart_enabled();
    with_handles(app, |handles| {
        if let Err(error) = handles.autostart.set_checked(enabled) {
            log::warn!("Could not update the autostart menu state: {error}");
        }
    });
}

fn on_menu(app: &tauri::AppHandle, id: &str) {
    match id {
        "quit" => app.exit(0),
        "open_settings" => show_main(app),
        "new_snip" => {
            let mode = app
                .state::<AppState>()
                .config()
                .map(|config| config.capture.default_mode)
                .unwrap_or_default();
            start(app, mode);
        }
        "open_folder" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Err(error) = commands::open_output_dir(app).await {
                    log::warn!("Could not open the output directory: {error}");
                }
            });
        }
        "autostart" => {
            let app = app.clone();
            tauri::async_runtime::spawn_blocking(move || {
                let enable = !autostart::is_autostart_enabled();
                let elevated = app
                    .state::<AppState>()
                    .config()
                    .map(|config| config.behavior.run_as_admin)
                    .unwrap_or(false);
                if let Err(error) = autostart::set_enabled(enable, elevated) {
                    log::warn!("Could not change autostart: {error}");
                }
                refresh_autostart(&app);
            });
        }
        other => match other.parse::<CaptureMode>() {
            Ok(mode) => start(app, mode),
            Err(_) => log::warn!("Unknown tray menu item: {other}"),
        },
    }
}

fn show_main(app: &tauri::AppHandle) {
    if let Err(error) = overlay::show_main(app) {
        log::warn!("Could not show the main window: {error}");
    }
}

fn start(app: &tauri::AppHandle, mode: CaptureMode) {
    if let Err(error) = capture::request_start(app, mode, None) {
        log::warn!("Could not start capture: {error}");
    }
}

fn binding(hotkeys: &Hotkeys, mode: CaptureMode) -> &str {
    match mode {
        CaptureMode::Rectangle => &hotkeys.rectangle,
        CaptureMode::Freeform => &hotkeys.freeform,
        CaptureMode::Window => &hotkeys.window,
        CaptureMode::Fullscreen => &hotkeys.fullscreen,
    }
}

fn shortcut_label(label: &str, shortcut: &str) -> String {
    if shortcut.trim().is_empty() {
        label.to_owned()
    } else {
        format!("{label}\t{shortcut}")
    }
}
