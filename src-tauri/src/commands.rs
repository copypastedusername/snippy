//! Tauri commands: the IPC surface described in `docs/contract.md`.

use crate::capture::{self, CaptureResult};
use crate::config::{CaptureMode, Config};
use crate::screen::MonitorInfo;
use crate::storage::{self, CaptureItem};
use crate::winlist::WindowEntry;
use crate::{
    autostart, clipboard, elevation, imageops, overlay, recents, screenclip, shortcuts, tray,
    AppState,
};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use tauri::{Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

#[derive(Debug, Clone, Serialize)]
pub struct OverlayMeta {
    /// The snip these pixels belong to; the overlay passes it back with every request.
    pub session: u64,
    pub mode: CaptureMode,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    /// Radius of window corners in frame pixels: Windows 11 rounds them, Windows 10 does not.
    pub corner_radius: f64,
    pub monitors: Vec<MonitorInfo>,
    pub windows: Vec<WindowEntry>,
}

/// Runs blocking work off the IPC thread.
async fn blocking<T, F>(what: &str, work: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(work)
        .await
        .map_err(|error| format!("{what} worker failed: {error}"))?
}

/// Validates a capture path from the frontend against the current output folder.
fn capture_path(app: &tauri::AppHandle, path: &str) -> Result<PathBuf, String> {
    let directory = app.state::<AppState>().config()?.output.directory;
    storage::resolve_capture(Path::new(path), &directory)
}

// ---------- configuration ----------

#[tauri::command]
pub fn get_config(state: tauri::State<'_, AppState>) -> Result<Config, String> {
    state.config()
}

/// What Snippy had to repair in the settings file at start (shown once on the main page).
#[tauri::command]
pub fn config_recovery() -> Option<String> {
    crate::config::recovery_notice()
}

#[tauri::command]
pub async fn set_config(app: tauri::AppHandle, config: Config) -> Result<Config, String> {
    blocking("Config", move || apply_config(&app, config)).await
}

fn apply_config(app: &tauri::AppHandle, mut config: Config) -> Result<Config, String> {
    config.validate();
    let state = app.state::<AppState>();
    let previous = state.config()?;
    // Change the Windows registration first: if it fails, the old setting stays saved.
    if previous.behavior.windows_notifications != config.behavior.windows_notifications {
        set_notification_identity(config.behavior.windows_notifications)?;
    }
    let wants_link = config.behavior.wants_screenclip_link();
    if previous.behavior.wants_screenclip_link() != wants_link {
        set_screenclip_handler(wants_link)?;
    }
    crate::config::save(&config)?;
    *state
        .config
        .write()
        .map_err(|_| "Config lock is poisoned".to_owned())? = config.clone();

    let previous_directory = storage::normalize_path(&previous.output.directory);
    let next_directory = storage::normalize_path(&config.output.directory);
    if !storage::paths_match(&previous_directory, &next_directory) {
        crate::lock(&state.recents, "recent captures")?.invalidate();
    }
    if previous.behavior.run_as_admin != config.behavior.run_as_admin {
        // The logon task's privilege level follows "Run as administrator".
        let app = app.clone();
        std::thread::spawn(move || {
            let setting = || {
                let config = app.state::<AppState>().config()?;
                Ok(config.behavior.run_as_admin)
            };
            if let Err(error) = autostart::follow_privilege_setting(setting) {
                log::warn!("Could not update the logon task's privileges: {error}");
            }
        });
    }
    if previous.hotkeys != config.hotkeys {
        shortcuts::register(app);
        tray::refresh_hotkeys(app);
    } else if windows_shortcuts_changed(&previous, &config) {
        shortcuts::register(app);
    }
    app.emit("config-changed", &config)
        .map_err(|error| format!("Failed to emit config-changed: {error}"))?;
    Ok(config)
}

fn set_notification_identity(enabled: bool) -> Result<(), String> {
    if enabled {
        let executable =
            std::env::current_exe().map_err(|error| format!("Failed to get exe path: {error}"))?;
        crate::notify::register(&executable)
    } else {
        crate::notify::unregister()
    }
}

fn set_screenclip_handler(enabled: bool) -> Result<(), String> {
    if enabled {
        let executable =
            std::env::current_exe().map_err(|error| format!("Failed to get exe path: {error}"))?;
        screenclip::register(&executable)
    } else {
        screenclip::unregister()
    }
}

fn windows_shortcuts_changed(previous: &Config, next: &Config) -> bool {
    let (a, b) = (&previous.behavior, &next.behavior);
    a.handle_screenclip != b.handle_screenclip
        || a.handle_print_screen != b.handle_print_screen
        || a.windows_shortcuts_only != b.windows_shortcuts_only
}

/// How the Windows capture keys reach Snippy, for the settings page.
#[derive(Debug, Clone, Serialize)]
pub struct WindowsShortcutStatus {
    /// Snippy holds Win+Shift+S itself (Windows left it free).
    pub win_shift_s_taken: bool,
    /// Windows 10: Snippy takes Win+Shift+S from Windows (a keyboard hook, and the hotkey when free).
    pub win_shift_s_hooked: bool,
    /// Windows 10: Explorer's own Win+S hotkeys (Search, Win+Shift+S) are turned off.
    pub windows_hotkey_off: bool,
    /// Windows 10: whether Windows' own clipping overlay is in place or turned off by Snippy.
    pub windows_overlay: crate::windows_overlay::OverlayStatus,
    /// Snippy holds Print Screen as an ordinary hotkey.
    pub print_screen_taken: bool,
    /// Windows' "Print Screen opens screen capture" option: on, off, or unknown (never set on a
    /// build where an update may have turned it on).
    pub print_screen_by_windows: Option<bool>,
    /// The Windows build, so the page can give Windows 10 its own instructions.
    pub windows_build: Option<u32>,
    /// The app Windows opens for Win+Shift+S (and for Snippy-less starts).
    pub closed_handler: Option<String>,
    pub closed_handler_is_snippy: bool,
    /// The links (as Default apps lists them, e.g. `MS-SCREENSKETCH`) still opening another app.
    pub links_to_set: Vec<String>,
}

/// Windows 10: turns Explorer's own Win+S hotkeys (Search and Win+Shift+S) off or back on, so that
/// Win+Shift+S reaches Snippy as an ordinary hotkey after the next sign-in.
#[tauri::command]
pub async fn set_windows_hotkey_off(off: bool) -> Result<(), String> {
    blocking("Windows hotkey", move || {
        crate::keyhook::set_windows_hotkey_off(off)
    })
    .await
}

/// Windows 10: turns Windows' own screen-clipping overlay off or back on (asks for administrator
/// rights). Returns the new status.
#[tauri::command]
pub async fn set_windows_overlay_disabled(
    disabled: bool,
) -> Result<crate::windows_overlay::OverlayStatus, String> {
    blocking("Windows overlay", move || {
        crate::windows_overlay::set_disabled(disabled)
    })
    .await
}

/// Opens Windows' keyboard settings ("Use the Print Screen key to open screen capture").
#[tauri::command]
pub async fn open_keyboard_settings() -> Result<(), String> {
    blocking("Keyboard settings", || {
        storage::open(Path::new(screenclip::KEYBOARD_SETTINGS_PAGE))
    })
    .await
}

#[tauri::command]
pub async fn windows_shortcut_status(
    app: tauri::AppHandle,
) -> Result<WindowsShortcutStatus, String> {
    blocking("Shortcut status", move || {
        let state = app.state::<AppState>();
        let load =
            |flag: &std::sync::atomic::AtomicBool| flag.load(std::sync::atomic::Ordering::Acquire);
        let build = screenclip::windows_build();
        let handlers: Vec<_> = screenclip::required_schemes(build)
            .iter()
            .map(|scheme| (*scheme, screenclip::current_handler(scheme)))
            .collect();
        let not_snippy: Vec<_> = handlers
            .iter()
            .filter(|(_, handler)| !handler.as_ref().is_some_and(|(_, snippy)| *snippy))
            .collect();
        // The app the user sees instead of Snippy: the first link that does not open Snippy.
        let closed_handler = not_snippy
            .iter()
            .find_map(|(_, handler)| handler.as_ref().map(|(name, _)| name.clone()));
        Ok(WindowsShortcutStatus {
            win_shift_s_taken: load(&state.screenclip_hotkey),
            win_shift_s_hooked: crate::keyhook::is_active(),
            windows_hotkey_off: crate::keyhook::windows_hotkey_off(),
            windows_overlay: crate::windows_overlay::status(),
            print_screen_taken: load(&state.print_screen_hotkey),
            print_screen_by_windows: screenclip::windows_routes_print_screen(),
            windows_build: build,
            closed_handler_is_snippy: not_snippy.is_empty(),
            links_to_set: not_snippy
                .iter()
                .map(|(scheme, _)| scheme.to_ascii_uppercase())
                .collect(),
            closed_handler,
        })
    })
    .await
}

/// Checks GitHub for a newer release (the settings page's "Check now").
#[tauri::command]
pub async fn check_for_updates() -> Result<crate::updater::UpdateStatus, String> {
    blocking("update check", crate::updater::check).await
}

/// Downloads, verifies and installs the newest release; Snippy restarts on success.
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle) -> Result<(), String> {
    blocking("update", move || crate::updater::install(&app)).await
}

/// Plays a capture sound once, so the settings page can preview it before choosing.
#[tauri::command]
pub fn preview_sound(sound: String) -> Result<(), String> {
    crate::sound::play(sound.parse()?);
    Ok(())
}

/// Opens Settings > Default apps on Snippy's page, where it can be made the Win+Shift+S handler.
#[tauri::command]
pub async fn open_default_apps() -> Result<(), String> {
    blocking("Default apps", || {
        storage::open(Path::new(screenclip::DEFAULT_APPS_PAGE))
    })
    .await
}

#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<Config, String> {
    set_config(app, Config::default()).await
}

#[tauri::command]
pub async fn pick_output_directory(app: tauri::AppHandle) -> Result<Option<String>, String> {
    blocking("Folder picker", move || {
        app.dialog()
            .file()
            .blocking_pick_folder()
            .map(|path| {
                path.into_path()
                    .map(|path| path.to_string_lossy().into_owned())
                    .map_err(|error| format!("Invalid selected folder: {error}"))
            })
            .transpose()
    })
    .await
}

// ---------- capture ----------

#[tauri::command]
pub fn start_snip(app: tauri::AppHandle, mode: String, delay: Option<u64>) -> Result<(), String> {
    capture::request_start(&app, CaptureMode::from_str(&mode)?, delay)
}

#[tauri::command]
pub fn cancel_snip(app: tauri::AppHandle) -> Result<(), String> {
    capture::cancel(&app)
}

#[tauri::command]
pub fn overlay_metadata(state: tauri::State<'_, AppState>) -> Result<OverlayMeta, String> {
    let session = crate::lock(&state.session, "capture session")?;
    let session = session
        .as_ref()
        .ok_or_else(|| "No captured frame is available".to_owned())?;
    let scale = session
        .monitors
        .iter()
        .find(|monitor| monitor.primary)
        .map_or(1.0, |monitor| monitor.scale);
    Ok(OverlayMeta {
        session: session.id,
        mode: session.mode,
        width: session.frame.rgba.width(),
        height: session.frame.rgba.height(),
        scale,
        corner_radius: if screenclip::windows_build().is_some_and(|build| build >= 22000) {
            8.0 * scale
        } else {
            0.0
        },
        monitors: session.monitors.clone(),
        windows: session.windows.clone(),
    })
}

#[tauri::command]
pub async fn overlay_pixels(
    app: tauri::AppHandle,
    session: u64,
) -> Result<tauri::ipc::Response, String> {
    // Copying a whole virtual desktop (tens of MB) must not happen on the UI thread.
    blocking("Overlay pixels", move || {
        let state = app.state::<AppState>();
        let current = crate::lock(&state.session, "capture session")?;
        let bytes = current
            .as_ref()
            .filter(|current| current.id == session)
            .ok_or_else(|| "This snip is no longer current".to_owned())?
            .frame
            .rgba
            .as_raw()
            .clone();
        Ok(tauri::ipc::Response::new(bytes))
    })
    .await
}

#[tauri::command]
pub fn overlay_ready(app: tauri::AppHandle) -> Result<(), String> {
    capture::overlay_ready(&app)
}

#[tauri::command]
pub async fn finish_rect(
    app: tauri::AppHandle,
    session: u64,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<CaptureResult, String> {
    capture::finish_with(app, session, move |session| {
        imageops::crop(&session.frame.rgba, x, y, width, height)
    })
    .await
}

#[tauri::command]
pub async fn finish_freeform(
    app: tauri::AppHandle,
    session: u64,
    points: Vec<(i32, i32)>,
) -> Result<CaptureResult, String> {
    capture::finish_with(app, session, move |session| {
        imageops::crop_freeform(&session.frame.rgba, &points)
    })
    .await
}

#[tauri::command]
pub async fn finish_window(
    app: tauri::AppHandle,
    session: u64,
    window_id: u64,
) -> Result<CaptureResult, String> {
    capture::finish_with(app, session, move |session| {
        capture::crop_window(session, window_id)
    })
    .await
}

#[tauri::command]
pub async fn finish_monitor(
    app: tauri::AppHandle,
    session: u64,
    monitor_index: usize,
) -> Result<CaptureResult, String> {
    capture::finish_with(app, session, move |session| {
        capture::crop_monitor(session, monitor_index)
    })
    .await
}

// ---------- captures on disk ----------

#[tauri::command]
pub async fn recent_captures(app: tauri::AppHandle) -> Result<Vec<CaptureItem>, String> {
    blocking("Recent captures", move || {
        let state = app.state::<AppState>();
        // No config lock is held during the scan: the cache is tagged with the scanned folder, so a
        // concurrent folder change only causes a miss, never results from the wrong folder.
        let directory = state.config()?.output.directory;
        recents::load(&state.recents, &directory, storage::scan_recent_captures)
    })
    .await
}

#[tauri::command]
pub async fn open_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    blocking("Open", move || storage::open(&capture_path(&app, &path)?)).await
}

#[tauri::command]
pub async fn reveal_path(app: tauri::AppHandle, path: String) -> Result<(), String> {
    blocking("Reveal", move || {
        storage::reveal(&capture_path(&app, &path)?)
    })
    .await
}

#[tauri::command]
pub async fn open_output_dir(app: tauri::AppHandle) -> Result<(), String> {
    let directory = app.state::<AppState>().config()?.output.directory;
    blocking("Open folder", move || storage::open_directory(&directory)).await
}

#[tauri::command]
pub async fn copy_capture(app: tauri::AppHandle, path: String) -> Result<(), String> {
    blocking("Copy", move || {
        let path = capture_path(&app, &path)?;
        let image = image::open(&path)
            .map_err(|error| format!("Failed to read {}: {error}", path.display()))?
            .into_rgba8();
        clipboard::copy_image_to_clipboard(&image)
    })
    .await
}

#[tauri::command]
pub async fn delete_capture(app: tauri::AppHandle, path: String) -> Result<(), String> {
    blocking("Delete", move || {
        let path = capture_path(&app, &path)?;
        storage::delete(&path)?;
        // Rescanned on the next request, so the list fills up again with the next-newest snip.
        crate::lock(&app.state::<AppState>().recents, "recent captures")?.invalidate();
        Ok(())
    })
    .await
}

// ---------- system integration ----------

// Shortcut (un)registration waits for the main thread, so these run on a worker (sync commands
// would run on the main thread itself).
#[tauri::command]
pub async fn pause_shortcuts(app: tauri::AppHandle) -> Result<(), String> {
    blocking("Shortcut", move || shortcuts::unregister(&app)).await
}

#[tauri::command]
pub async fn resume_shortcuts(app: tauri::AppHandle) -> Result<(), String> {
    blocking("Shortcut", move || {
        shortcuts::register(&app);
        Ok(())
    })
    .await
}

#[tauri::command]
pub async fn get_autostart() -> Result<bool, String> {
    blocking("Autostart", || Ok(autostart::is_autostart_enabled())).await
}

#[tauri::command]
pub async fn set_autostart(app: tauri::AppHandle, enabled: bool) -> Result<(), String> {
    blocking("Autostart", move || {
        let elevated = app
            .state::<AppState>()
            .config()
            .map(|config| config.behavior.run_as_admin)
            .unwrap_or(false);
        let result = autostart::set_enabled(enabled, elevated);
        tray::refresh_autostart(&app);
        result
    })
    .await
}

#[tauri::command]
pub async fn is_elevated() -> Result<bool, String> {
    blocking("Elevation", elevation::is_elevated).await
}

#[tauri::command]
pub fn hide_flyout(app: tauri::AppHandle) -> Result<(), String> {
    overlay::hide_flyout(&app)
}

// ---------- editor and text recognition ----------

/// Opens the editor on a capture in the output folder, or on the latest snip when `path` is null.
#[tauri::command]
pub async fn open_editor(app: tauri::AppHandle, path: Option<String>) -> Result<(), String> {
    blocking("Editor", move || {
        crate::editor::open(&app, path.as_deref().map(Path::new))
    })
    .await
}

#[tauri::command]
pub fn editor_info(app: tauri::AppHandle) -> Result<crate::editor::EditorInfo, String> {
    crate::editor::info(&app)
}

#[tauri::command]
pub fn editor_pixels(
    app: tauri::AppHandle,
    document: Option<u64>,
) -> Result<tauri::ipc::Response, String> {
    crate::editor::pixels(&app, document).map(tauri::ipc::Response::new)
}

/// The editor page reports whether it holds changes that were not saved or copied.
#[tauri::command]
pub fn editor_unsaved(unsaved: bool) {
    crate::editor::set_unsaved(unsaved);
}

/// Reads the text in what the editor shows (the picture with its marks, cropped), sent as a raw
/// PNG body; the optional `snippy-language` header picks the OCR language.
#[tauri::command]
pub async fn editor_ocr(request: tauri::ipc::Request<'_>) -> Result<crate::ocr::OcrText, String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("The editor must send the image as raw PNG bytes".to_owned());
    };
    let language = request
        .headers()
        .get("snippy-language")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let png = png.clone();
    blocking("Text recognition", move || {
        let image = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
            .map_err(|error| format!("The image to read is not a valid PNG: {error}"))?
            .into_rgba8();
        crate::ocr::recognize(&image, language.as_deref())
    })
    .await
}

#[tauri::command]
pub async fn ocr_languages() -> Result<Vec<crate::ocr::OcrLanguage>, String> {
    blocking("Text recognition", crate::ocr::languages).await
}

/// Receives the edited image as a raw PNG body; the `snippy-action` header says what to do.
#[tauri::command]
pub async fn editor_export(
    app: tauri::AppHandle,
    request: tauri::ipc::Request<'_>,
) -> Result<Option<String>, String> {
    let tauri::ipc::InvokeBody::Raw(png) = request.body() else {
        return Err("The editor must send the image as raw PNG bytes".to_owned());
    };
    let action: crate::editor::ExportAction = request
        .headers()
        .get("snippy-action")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| "Missing editor action".to_owned())?
        .parse()?;
    // The document the picture was drawn from, so it is never saved under another one's name.
    let document = request
        .headers()
        .get("snippy-document")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let png = png.clone();
    blocking("Editor", move || {
        crate::editor::export(&app, &png, action, document)
    })
    .await
}

/// The main page has rendered for the first time; a pending startup show happens now. `stages`
/// are the page's own milestones (name, milliseconds since navigation), kept in the startup trace.
#[tauri::command]
pub fn main_ready(app: tauri::AppHandle, stages: Option<Vec<(String, u32)>>) {
    if let Some(stages) = stages {
        crate::trace::step(&format!("main page stages: {}", describe_stages(&stages)));
    }
    overlay::main_ready(&app);
}

/// `script 120 ms, config 180 ms, ...`, bounded so a page cannot flood the trace.
fn describe_stages(stages: &[(String, u32)]) -> String {
    stages
        .iter()
        .take(8)
        .map(|(name, ms)| {
            let name: String = name
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .take(16)
                .collect();
            format!("{name} {ms} ms")
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::describe_stages;

    #[test]
    fn page_stages_are_listed_briefly_and_safely() {
        let stages = vec![("script".to_owned(), 120), ("con\nfig!".to_owned(), 180)];
        assert_eq!(describe_stages(&stages), "script 120 ms, config 180 ms");
        let many: Vec<_> = (0..20).map(|i| (format!("s{i}"), i)).collect();
        assert_eq!(describe_stages(&many).matches(" ms").count(), 8);
    }
}
