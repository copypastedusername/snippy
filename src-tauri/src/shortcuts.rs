//! Global hotkeys that start a snip in each capture mode.

use crate::config::CaptureMode;
use crate::{capture, keyhook, AppState};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Shortcut, ShortcutState};

/// The hotkeys Snippy registered, so they can be released one by one. The plugin's
/// `unregister_all` holds its shortcut table locked while it waits for the main thread, and the
/// main thread needs that lock to deliver a pressed hotkey: pressing one at that moment deadlocked
/// the UI. `unregister_multiple` does not hold the lock while waiting.
static REGISTERED: Mutex<Vec<Shortcut>> = Mutex::new(Vec::new());

/// Releases every global hotkey, e.g. while the settings page records a new binding.
pub fn unregister(app: &tauri::AppHandle) -> Result<(), String> {
    let state = app.state::<AppState>();
    let _guard = crate::lock(&state.shortcuts, "shortcut")?;
    keyhook::set_active(app, false)?;
    release_all(app)
}

fn release_all(app: &tauri::AppHandle) -> Result<(), String> {
    let registered = std::mem::take(&mut *REGISTERED.lock().unwrap_or_else(|p| p.into_inner()));
    if registered.is_empty() {
        return Ok(());
    }
    app.global_shortcut()
        .unregister_multiple(registered)
        .map_err(|error| format!("Failed to unregister global shortcuts: {error}"))
}

/// Re-registers the hotkeys from the current config. Invalid or unavailable bindings are logged
/// and skipped so one bad binding never disables the others.
pub fn register(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let Ok(_guard) = crate::lock(&state.shortcuts, "shortcut") else {
        log::warn!("Shortcut lock is poisoned; global shortcuts were not registered");
        return;
    };
    if let Err(error) = release_all(app) {
        log::warn!("{error}");
    }
    let config = match state.config() {
        Ok(config) => config,
        Err(error) => {
            log::warn!("{error}; global shortcuts were not registered");
            return;
        }
    };
    let behavior = &config.behavior;

    let mut failures = Vec::new();
    if behavior.windows_shortcuts_only {
        log::info!("Snippy's own hotkeys are off; only the Windows keys snip");
    } else {
        let hotkeys = &config.hotkeys;
        for (binding, mode) in [
            (&hotkeys.rectangle, CaptureMode::Rectangle),
            (&hotkeys.freeform, CaptureMode::Freeform),
            (&hotkeys.window, CaptureMode::Window),
            (&hotkeys.fullscreen, CaptureMode::Fullscreen),
        ] {
            if let Err(error) = bind(app, binding, Some(mode)) {
                log::warn!("Skipping {} shortcut '{binding}': {error}", mode.as_str());
                if !binding.trim().is_empty() {
                    failures.push(format!("{binding} ({error})"));
                }
            }
        }
    }

    // Windows 10 starts its own clipping for Win+Shift+S whatever app the links open, so only a
    // keyboard hook takes it there (see keyhook.rs). Elsewhere, registering a Windows key helps
    // where Windows leaves it free; success does not prove the key reaches Snippy.
    let hook_win_shift_s =
        behavior.handle_screenclip && keyhook::needed(crate::screenclip::windows_build());
    if let Err(error) = keyhook::set_active(app, hook_win_shift_s) {
        log::warn!("{error}");
        failures.push(format!("Win+Shift+S ({error})"));
    }
    let owns_win_shift_s = if hook_win_shift_s {
        keyhook::is_active()
    } else {
        behavior.handle_screenclip && claim(app, WIN_SHIFT_S)
    };
    let owns_print_screen = behavior.handle_print_screen && claim(app, PRINT_SCREEN);
    state
        .screenclip_hotkey
        .store(owns_win_shift_s, Ordering::Release);
    state
        .print_screen_hotkey
        .store(owns_print_screen, Ordering::Release);
    // A binding another program holds would otherwise look saved but do nothing.
    if !failures.is_empty() {
        if let Err(error) = app.emit("shortcut-errors", &failures) {
            log::warn!("Could not report hotkey errors: {error}");
        }
    }
}

const WIN_SHIFT_S: &str = "Super+Shift+S";
const PRINT_SCREEN: &str = "PrintScreen";

/// Registers a key Windows may already hold. Returns whether Snippy now owns it.
fn claim(app: &tauri::AppHandle, binding: &str) -> bool {
    match bind(app, binding, None) {
        Ok(()) => {
            log::info!("Snippy handles {binding} directly");
            true
        }
        Err(error) => {
            log::info!("{binding} is held elsewhere ({error})");
            false
        }
    }
}

/// Binds a hotkey. `None` snips in the default mode configured at the moment of the press.
fn bind(app: &tauri::AppHandle, binding: &str, mode: Option<CaptureMode>) -> Result<(), String> {
    if binding.trim().is_empty() {
        return Err("no key is bound".to_owned());
    }
    let shortcut = parse(binding)?;
    let registered = app
        .global_shortcut()
        .on_shortcut(shortcut, move |app, _shortcut, event| {
            if event.state != ShortcutState::Pressed {
                return;
            }
            let mode = mode.unwrap_or_else(|| {
                app.state::<AppState>()
                    .config()
                    .map(|config| config.capture.default_mode)
                    .unwrap_or_default()
            });
            if let Err(error) = capture::request_start(app, mode, None) {
                log::error!("Shortcut capture failed to start: {error}");
            }
        })
        .map_err(|error| error.to_string());
    if registered.is_ok() {
        REGISTERED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(shortcut);
    }
    registered
}

/// Parses a binding and refuses a bare typing key: a global hook on plain `S` would swallow that
/// letter in every application. Keys nobody types text with may stand alone.
fn parse(binding: &str) -> Result<Shortcut, String> {
    let shortcut = binding
        .parse::<Shortcut>()
        .map_err(|error| error.to_string())?;
    if shortcut.mods.is_empty() && !may_stand_alone(shortcut.key) {
        return Err("a key without Ctrl, Shift or Alt would block typing it everywhere".to_owned());
    }
    Ok(shortcut)
}

fn may_stand_alone(key: Code) -> bool {
    matches!(
        key,
        Code::PrintScreen
            | Code::Pause
            | Code::ScrollLock
            | Code::F1
            | Code::F2
            | Code::F3
            | Code::F4
            | Code::F5
            | Code::F6
            | Code::F7
            | Code::F8
            | Code::F9
            | Code::F10
            | Code::F11
            | Code::F12
            | Code::F13
            | Code::F14
            | Code::F15
            | Code::F16
            | Code::F17
            | Code::F18
            | Code::F19
            | Code::F20
            | Code::F21
            | Code::F22
            | Code::F23
            | Code::F24
    )
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn bindings_need_a_modifier_unless_the_key_is_not_for_typing() {
        assert!(parse("Ctrl+Shift+S").is_ok());
        assert!(parse("PrintScreen").is_ok());
        assert!(parse("F9").is_ok());
        assert!(parse("Alt+Num1").is_ok());
        assert!(parse("S").is_err());
        assert!(parse("Space").is_err());
        assert!(parse("Ctrl+Bogus").is_err());
        assert!(parse("Super+Shift+S").is_ok());
    }
}
