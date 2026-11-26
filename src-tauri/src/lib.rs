mod autostart;
mod capture;
mod clipboard;
mod commands;
mod config;
mod crash;
mod display_watch;
mod editor;
mod elevation;
mod hidden_window;
mod imageops;
mod instances;
mod keyhook;
mod notify;
mod ocr;
mod overlay;
mod placement;
mod recents;
mod screen;
mod screenclip;
mod shortcuts;
mod sound;
mod storage;
mod task_scheduler;
mod trace;
mod tray;
mod updater;
mod windows_overlay;
mod winlist;
mod winreg;

use config::{CaptureMode, Config};
use recents::RecentCaptureCache;
use screen::{Frame, MonitorInfo};
use std::sync::atomic::AtomicBool;
use std::sync::{Mutex, MutexGuard, RwLock};
use tauri::Manager;
use winlist::WindowEntry;

/// Everything captured for one snip, kept while the overlay is open.
pub struct CaptureSession {
    pub id: u64,
    pub frame: Frame,
    pub mode: CaptureMode,
    pub windows: Vec<WindowEntry>,
    pub monitors: Vec<MonitorInfo>,
}

pub struct AppState {
    pub session: Mutex<Option<CaptureSession>>,
    pub recents: Mutex<RecentCaptureCache>,
    pub config: RwLock<Config>,
    /// Set while a snip is in progress; a second hotkey press is ignored until it clears.
    pub capturing: AtomicBool,
    /// Serialises global-shortcut unregister/register sequences.
    pub shortcuts: Mutex<()>,
    pub tray: Mutex<Option<tray::TrayHandles>>,
    /// True when Snippy registered Win+Shift+S itself (Windows left it free).
    pub screenclip_hotkey: AtomicBool,
    /// True when Snippy registered Print Screen itself.
    pub print_screen_hotkey: AtomicBool,
    /// The latest snip, kept so it can be edited even when it was not saved.
    pub last_capture: Mutex<Option<std::sync::Arc<image::RgbaImage>>>,
    /// The image open in the editor window.
    pub editor: Mutex<Option<editor::EditorDocument>>,
}

impl AppState {
    fn new(config: Config) -> Self {
        Self {
            session: Mutex::new(None),
            recents: Mutex::new(RecentCaptureCache::default()),
            config: RwLock::new(config),
            capturing: AtomicBool::new(false),
            shortcuts: Mutex::new(()),
            tray: Mutex::new(None),
            screenclip_hotkey: AtomicBool::new(false),
            print_screen_hotkey: AtomicBool::new(false),
            last_capture: Mutex::new(None),
            editor: Mutex::new(None),
        }
    }

    /// A copy of the current configuration.
    pub fn config(&self) -> Result<Config, String> {
        self.config
            .read()
            .map(|config| config.clone())
            .map_err(|_| "Config lock is poisoned".to_owned())
    }
}

/// Locks a mutex, turning poisoning into a readable command error.
pub fn lock<'a, T>(mutex: &'a Mutex<T>, what: &str) -> Result<MutexGuard<'a, T>, String> {
    mutex
        .lock()
        .map_err(|_| format!("The {what} lock is poisoned"))
}

/// One log file's size limit; with the previous file kept, the logs take about 8 MB at most.
const LOG_FILE_BYTES: u128 = 4 * 1024 * 1024;

/// Logs sit beside the config in `%APPDATA%\Snippy\logs`; the OS log directory would create a
/// second, identifier-named folder for no benefit.
fn log_target() -> tauri_plugin_log::TargetKind {
    match config::app_data_dir() {
        Ok(directory) => tauri_plugin_log::TargetKind::Folder {
            path: directory.join("logs"),
            file_name: Some("snippy".to_owned()),
        },
        Err(_) => tauri_plugin_log::TargetKind::LogDir { file_name: None },
    }
}

/// Snippy's windows are created once and reused, so a close request (the title bar, Alt+F4) must
/// never destroy them: the overlay cancels the snip, the preview hides, and the main window hides
/// to the tray or quits Snippy, depending on the setting.
fn hide_main_instead_of_closing(window: &tauri::Window, event: &tauri::WindowEvent) {
    let tauri::WindowEvent::CloseRequested { api, .. } = event else {
        return;
    };
    let app = window.app_handle();
    match window.label() {
        overlay::OVERLAY => {
            api.prevent_close();
            if let Err(error) = capture::cancel(app) {
                log::warn!("Could not cancel the snip: {error}");
            }
        }
        overlay::FLYOUT => {
            api.prevent_close();
            if let Some(flyout) = window.get_webview_window(overlay::FLYOUT) {
                overlay::hide_natively(&flyout);
            }
        }
        overlay::MAIN => {
            let close_to_tray = window
                .state::<AppState>()
                .config()
                .map(|config| config.behavior.close_to_tray)
                .unwrap_or(false);
            api.prevent_close();
            if close_to_tray {
                if let Err(error) = window.hide() {
                    log::warn!("Could not hide the main window: {error}");
                }
            } else {
                app.exit(0);
            }
        }
        _ => {}
    }
}

/// The oldest WebView2 the UI works with: the page uses `structuredClone` (98) and Snippy sets the
/// dark preferred colour scheme (101); older runtimes fail in ways that show nothing.
const MIN_WEBVIEW2_MAJOR: u32 = 101;

fn webview_major(version: &str) -> u32 {
    version
        .split('.')
        .next()
        .and_then(|major| major.trim().parse().ok())
        .unwrap_or(0)
}

const WEBVIEW2_DOWNLOAD_PAGE: &str =
    "https://developer.microsoft.com/microsoft-edge/webview2/#download-section";

/// The UI runs in Microsoft Edge WebView2. Windows 11 ships it, Windows 10 usually receives it
/// through Edge updates, but trimmed builds often remove it; without it Tauri would exit silently.
/// Explains the problem and offers the official download page instead.
fn webview_runtime_available() -> bool {
    let text = match tauri::webview_version() {
        Ok(version) if webview_major(&version) >= MIN_WEBVIEW2_MAJOR => {
            trace::step(&format!("WebView2 {version}: ok"));
            return true;
        }
        Ok(version) => {
            trace::step(&format!(
                "WebView2 {version} is older than {MIN_WEBVIEW2_MAJOR}; asking to update"
            ));
            format!(
                "Snippy needs a newer Microsoft Edge WebView2 Runtime: this PC has {version}, \
                 Snippy needs {MIN_WEBVIEW2_MAJOR} or later.\n\nOpen the download page now? Install \
                 the \"Evergreen\" runtime, then start Snippy again."
            )
        }
        Err(error) => {
            trace::step(&format!("WebView2 not found: {error}"));
            "Snippy needs the Microsoft Edge WebView2 Runtime, which is missing on this PC.\n\n\
             Open the download page now? Install the \"Evergreen\" runtime, then start Snippy again."
                .to_owned()
        }
    };
    if elevation::ask_yes_no("Snippy", &text) {
        if let Err(error) = storage::open(std::path::Path::new(WEBVIEW2_DOWNLOAD_PAGE)) {
            elevation::show_error("Snippy", &error);
        }
    }
    false
}

/// A second `snippy.exe` start was forwarded here: Win+Shift+S snips, anything else shows the window.
pub(crate) fn on_second_launch(app: &tauri::AppHandle, args: &[String]) {
    if let Some(action) = notify::parse_link(args) {
        notify::handle_link(app, action);
    } else if screenclip::is_screenclip_launch(args) {
        start_default_snip(app);
    } else if let Err(error) = overlay::show_main(app) {
        log::warn!("Could not show the main window for the second instance: {error}");
    }
}

pub(crate) fn start_default_snip(app: &tauri::AppHandle) {
    let mode = app
        .state::<AppState>()
        .config()
        .map(|config| config.capture.default_mode)
        .unwrap_or_default();
    if let Err(error) = capture::request_start(app, mode, None) {
        log::error!("Could not start the Win+Shift+S snip: {error}");
    }
}

/// Re-registers the Win+Shift+S handler with the current exe path, which changes whenever the
/// portable exe is moved.
fn refresh_screenclip_registration(config: &Config) {
    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            log::warn!("Could not find the exe to refresh registrations: {error}");
            return;
        }
    };
    if config.behavior.wants_screenclip_link() {
        if let Err(error) = screenclip::register(&executable) {
            log::warn!("Could not refresh the Win+Shift+S registration: {error}");
        }
    }
    if config.behavior.windows_notifications {
        if let Err(error) = notify::register(&executable) {
            log::warn!("Could not refresh the notification registration: {error}");
        }
    }
}

pub fn run() {
    let args: Vec<String> = std::env::args().collect();
    // The elevated helper that turns Windows 10's own clipping overlay off or on: nothing else
    // starts (no window, no single-instance claim), it only does that and exits with a code.
    if args.get(1).map(String::as_str) == Some(windows_overlay::HELPER_FLAG) {
        let code = match args.as_slice() {
            [_, _, action] => windows_overlay::run_helper(action),
            _ => windows_overlay::UNSUPPORTED,
        };
        std::process::exit(code);
    }
    // The crash hook first, then the startup trace (startup.log, and a console with --debug), so
    // even a start that shows nothing leaves a record of how far it got.
    crash::install();
    trace::init(&args);
    trace::environment_report();
    // After a self-update, wait for the old instance to exit before claiming the session; the
    // launch is never handed to it, even when it is slow to go.
    let predecessor = updater::predecessor(&args);
    updater::wait_for_predecessor(predecessor);
    if let instances::Outcome::Exit = instances::claim(&args, predecessor) {
        return;
    }
    updater::remove_leftover();
    let (initial_config, load_error) = match config::load() {
        Ok(config) => (config, None),
        Err(error) => {
            let mut config = Config::default();
            config.validate();
            (config, Some(error))
        }
    };
    trace::step(&match &load_error {
        Some(error) => format!("config unreadable, using defaults: {error}"),
        None => format!(
            "config: run_as_admin={} start_minimized={} windows_notifications={}",
            initial_config.behavior.run_as_admin,
            initial_config.behavior.start_minimized,
            initial_config.behavior.windows_notifications
        ),
    });
    // A link launch (Win+Shift+S, a notification button) carries a request that a relaunch
    // through the logon task would drop, so it is handled at the current integrity.
    let screenclip_launch =
        screenclip::is_screenclip_launch(&args) || notify::parse_link(&args).is_some();
    // Handing a Win+Shift+S launch over to an elevated instance would lose the request (the
    // logon task takes no arguments), so such a launch snips at normal integrity instead.
    if !screenclip_launch && elevation::hand_over_if_configured(&initial_config) {
        trace::step("handed over to an elevated instance; this process exits");
        return;
    }
    if !webview_runtime_available() {
        return;
    }
    crash::watch_startup();
    trace::step("building the app (plugins: dialog, global-shortcut, log)");

    let builder = tauri::Builder::default()
        // Tao otherwise registers Snippy's windows for Raw Input from the keyboard, and while one of
        // them has the keyboard Windows stops calling Snippy's own low-level keyboard hook, so
        // Win+Shift+S went to Windows 10's snipping instead (keyhook.rs; wry issue 1761). Snippy
        // reads no device events.
        .device_event_filter(tauri::DeviceEventFilter::Always)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(
            tauri_plugin_log::Builder::default()
                .level(log::LevelFilter::Info)
                // The log stays small: the current file up to about 4 MB and the one before it.
                .max_file_size(LOG_FILE_BYTES)
                .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(1))
                .clear_targets()
                .target(tauri_plugin_log::Target::new(log_target()))
                .build(),
        )
        .manage(AppState::new(initial_config))
        .on_window_event(|window, event| {
            if window.label() == editor::EDITOR {
                match event {
                    tauri::WindowEvent::CloseRequested { api, .. } => {
                        editor::close_requested(window, api);
                    }
                    tauri::WindowEvent::Destroyed => editor::closed(window.app_handle()),
                    _ => {}
                }
            }
            if window.label() == overlay::MAIN {
                if let tauri::WindowEvent::Moved(position) = event {
                    placement::remember(*position);
                }
            }
            hide_main_instead_of_closing(window, event);
        })
        .setup(move |app| {
            // The logger exists only once the plugin is set up, so report a broken config here.
            if let Some(error) = &load_error {
                log::error!("Failed to load config, using defaults: {error}");
            }
            log::info!("Starting: {}", crash::environment());
            trace::step("setup: plugins ready");
            let config = app.state::<AppState>().config()?;
            tray::setup(app)?;
            trace::step("setup: tray icon ready");
            let tray_app = app.handle().clone();
            std::thread::spawn(move || tray::refresh_autostart(&tray_app));
            overlay::create_windows(app).map_err(std::io::Error::other)?;
            trace::step("setup: windows created (main, overlay, flyout)");
            crash::started();
            // Only now may later launches be handed to this copy.
            instances::ready(app.handle());
            shortcuts::register(app.handle());
            trace::step("setup: hotkeys registered");
            refresh_screenclip_registration(&config);
            std::thread::spawn(|| {
                if let Err(error) = autostart::repair_task_path() {
                    log::warn!("Could not check the logon task: {error}");
                }
            });
            updater::start_background_checks(app.handle());
            if let Some(action) = notify::parse_link(&args) {
                notify::handle_link(app.handle(), action);
            } else if screenclip_launch {
                start_default_snip(app.handle());
            } else if !config.behavior.start_minimized {
                // Shown once the page has rendered, so the window never opens blank.
                overlay::show_main_when_ready(app.handle());
            } else {
                trace::step("setup: start minimized, main window stays hidden (tray only)");
            }
            trace::step("setup: done, waiting for the main page to render");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_config,
            commands::config_recovery,
            commands::set_config,
            commands::reset_config,
            commands::pick_output_directory,
            commands::start_snip,
            commands::cancel_snip,
            commands::overlay_metadata,
            commands::overlay_pixels,
            commands::overlay_ready,
            commands::finish_rect,
            commands::finish_freeform,
            commands::finish_window,
            commands::finish_monitor,
            commands::recent_captures,
            commands::open_path,
            commands::reveal_path,
            commands::open_output_dir,
            commands::copy_capture,
            commands::delete_capture,
            commands::pause_shortcuts,
            commands::resume_shortcuts,
            commands::get_autostart,
            commands::set_autostart,
            commands::is_elevated,
            commands::hide_flyout,
            commands::open_default_apps,
            commands::preview_sound,
            commands::check_for_updates,
            commands::install_update,
            commands::windows_shortcut_status,
            commands::open_keyboard_settings,
            commands::set_windows_hotkey_off,
            commands::set_windows_overlay_disabled,
            commands::main_ready,
            commands::open_editor,
            commands::editor_info,
            commands::editor_pixels,
            commands::editor_unsaved,
            commands::editor_ocr,
            commands::ocr_languages,
            commands::editor_export,
        ]);

    let app = match builder.build(tauri::generate_context!()) {
        Ok(app) => app,
        Err(error) => {
            crash::report(&format!("Snippy could not start: {error}"));
            std::process::exit(1);
        }
    };
    app.run(|_, event| {
        if let tauri::RunEvent::Exit = event {
            instances::closing();
        }
    });
    trace::step("event loop ended; Snippy exits");
}
