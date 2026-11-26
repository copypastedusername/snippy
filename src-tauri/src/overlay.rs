use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::webview::PageLoadEvent;
use tauri::window::Color;
use tauri::{Emitter, Manager, PhysicalPosition, PhysicalSize, WebviewUrl};
use windows::Win32::Foundation::RECT;
use windows::Win32::Foundation::{BOOL, HWND, POINT};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_TRANSITIONS_FORCEDISABLED};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindowVisible, ShowWindow, SW_HIDE, SW_SHOWNOACTIVATE,
};

pub const MAIN: &str = "main";
pub const OVERLAY: &str = "overlay";
pub const FLYOUT: &str = "flyout";

/// Flyout size in logical pixels; scaled by the DPI of the monitor it appears on.
const FLYOUT_SIZE: (i64, i64) = (384, 156);
const FLYOUT_MARGIN: i64 = 16;

const DISABLE_CONTEXT_MENU: &str =
    "document.addEventListener('contextmenu',event=>event.preventDefault(),{capture:true});";

/// `--bg-base` of ui/styles/theme.css, painted before a page renders so nothing flashes white.
/// Snippy has one, dark, theme.
pub const BACKGROUND: Color = Color(0x20, 0x20, 0x20, 0xff);
/// Windows-drawn parts of a window (title bar, borders, system menus) are dark too.
pub const NATIVE_THEME: Option<tauri::Theme> = Some(tauri::Theme::Dark);
/// The main page reports its first render through `main_ready`; this is the latest it may take.
const MAIN_READY_TIMEOUT: Duration = Duration::from_secs(3);

/// Set while a startup show of the main window waits for the page's first render.
static MAIN_PENDING: AtomicBool = AtomicBool::new(false);
/// Set once the main page has rendered. The page can finish before setup asks to show the window,
/// so its report must be remembered rather than lost.
static MAIN_PAGE_READY: AtomicBool = AtomicBool::new(false);

pub fn create_windows(app: &tauri::App) -> Result<(), String> {
    // One WebView2 profile for every window, next to the rest of Snippy's machine-local data,
    // so the app never scatters folders named after the bundle identifier.
    let profile = crate::config::local_data_dir().join("webview");

    let main = tauri::WebviewWindowBuilder::new(app, MAIN, WebviewUrl::App("index.html".into()))
        .title("Snippy")
        .theme(NATIVE_THEME)
        .background_color(BACKGROUND)
        .inner_size(560.0, 460.0)
        // The page draws its own title bar (ui/index.html); a fixed, non-maximizable window keeps
        // that layout exact. The shadow restores the Windows 11 border and rounded corners.
        .decorations(false)
        .shadow(true)
        .resizable(false)
        .maximizable(false)
        .center()
        .visible(false)
        .data_directory(profile.clone())
        // When WebView2 starts and finishes loading the page, for the startup trace.
        .on_page_load(|_, payload| {
            crate::trace::step(&format!("main page load: {:?}", payload.event()));
        })
        .build()
        .map_err(|error| format!("Failed to create the main window: {error}"))?;
    // Reopen on the screen it was last on (centred when that screen is gone).
    crate::placement::restore(&main);

    let initial_page_loaded = Arc::new(AtomicBool::new(false));
    let page_load_guard = initial_page_loaded.clone();
    let overlay =
        tauri::WebviewWindowBuilder::new(app, OVERLAY, WebviewUrl::App("overlay.html".into()))
            .title("Snippy Overlay")
            .inner_size(1.0, 1.0)
            .position(-10_000.0, -10_000.0)
            .decorations(false)
            // No resize borders or shadow: both keep a hidden frame that pushes the content
            // 8 px right and 1 px down, so the frozen screenshot would not line up with the screen.
            .resizable(false)
            .shadow(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .visible(true)
            .focused(false)
            .data_directory(profile.clone())
            .initialization_script(DISABLE_CONTEXT_MENU)
            .on_page_load(move |window, payload| {
                if payload.event() == PageLoadEvent::Finished
                    && !page_load_guard.swap(true, Ordering::AcqRel)
                {
                    if let Err(error) = window.hide() {
                        log::warn!("Could not hide the pre-warmed overlay: {error}");
                    }
                }
            })
            .build()
            .map_err(|error| format!("Failed to create the overlay window: {error}"))?;
    disable_transitions(&overlay);

    match tauri::WebviewWindowBuilder::new(app, FLYOUT, WebviewUrl::App("flyout.html".into()))
        .title("Snippy Capture")
        .inner_size(FLYOUT_SIZE.0 as f64, FLYOUT_SIZE.1 as f64)
        .transparent(true)
        .decorations(false)
        .resizable(false)
        .shadow(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .visible(false)
        .focused(false)
        .data_directory(profile)
        .initialization_script(DISABLE_CONTEXT_MENU)
        .build()
    {
        Ok(flyout) => disable_transitions(&flyout),
        Err(error) => log::warn!("Flyout window is unavailable: {error}"),
    }
    Ok(())
}

fn disable_transitions(window: &tauri::WebviewWindow) {
    let Ok(raw) = window.hwnd() else {
        return;
    };
    let disabled = BOOL::from(true);
    if let Err(error) = unsafe {
        DwmSetWindowAttribute(
            HWND(raw.0),
            DWMWA_TRANSITIONS_FORCEDISABLED,
            (&disabled as *const BOOL).cast(),
            size_of::<BOOL>() as u32,
        )
    } {
        log::warn!("Could not disable window transitions: {error}");
    }
}

pub fn hide_visible_snippy_windows(app: &tauri::AppHandle) -> bool {
    let mut hidden_any = false;
    for label in [MAIN, OVERLAY, FLYOUT] {
        let Some(window) = app.get_webview_window(label) else {
            continue;
        };
        if label == FLYOUT {
            // Shown natively (see `show_without_focus`), so Tauri's own visibility state is stale.
            hidden_any |= hide_natively(&window);
            continue;
        }
        if window.is_visible().unwrap_or(false) {
            hidden_any = true;
            if label == OVERLAY {
                let _ = window.set_always_on_top(false);
            }
            if let Err(error) = window.hide() {
                log::warn!("Could not hide {label} before capture: {error}");
            }
        }
    }
    hidden_any
}

fn overlay_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, String> {
    app.get_webview_window(OVERLAY)
        .ok_or_else(|| "Overlay window is unavailable".to_owned())
}

/// Sizes the hidden overlay over the virtual screen and asks its page to load the new frame. The
/// page calls `overlay_ready` once the frame is painted, which then shows the window; showing it
/// earlier would flash the previous capture.
pub fn prepare_overlay(
    app: &tauri::AppHandle,
    origin: (i32, i32),
    size: (u32, u32),
) -> Result<(), String> {
    let overlay = overlay_window(app)?;
    // Move first: arriving on a monitor with another DPI makes Windows rescale the window, which
    // must happen before the final physical size is set (and the position re-applied).
    let place = |overlay: &tauri::WebviewWindow| {
        overlay
            .set_position(PhysicalPosition::new(origin.0, origin.1))
            .map_err(|error| format!("Failed to position overlay: {error}"))
    };
    place(&overlay)?;
    overlay
        .set_size(PhysicalSize::new(size.0, size.1))
        .map_err(|error| format!("Failed to size overlay: {error}"))?;
    place(&overlay)?;
    *INTENDED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(RECT {
        left: origin.0,
        top: origin.1,
        right: origin.0 + size.0 as i32,
        bottom: origin.1 + size.1 as i32,
    });
    keep_placed(&overlay, "prepared");
    app.emit("overlay-refresh", ())
        .map_err(|error| format!("Failed to refresh overlay: {error}"))
}

pub fn overlay_visible(app: &tauri::AppHandle) -> bool {
    overlay_window(app)
        .ok()
        .and_then(|overlay| overlay.is_visible().ok())
        .unwrap_or(false)
}

pub fn reveal_overlay(app: &tauri::AppHandle) -> Result<(), String> {
    let overlay = overlay_window(app)?;
    if overlay.is_visible().unwrap_or(false) {
        return Ok(());
    }
    overlay
        .set_always_on_top(true)
        .map_err(|error| format!("Failed to make overlay topmost: {error}"))?;
    overlay
        .show()
        .map_err(|error| format!("Failed to show overlay: {error}"))?;
    let focused = overlay
        .set_focus()
        .map_err(|error| format!("Failed to focus overlay: {error}"));
    keep_placed(&overlay, "shown");
    // Windows may still move or rescale the window a moment later (displays waking up, a DPI
    // change), so look again shortly after.
    let late = overlay.clone();
    let _ = std::thread::Builder::new()
        .name("snippy-overlay-placement".to_owned())
        .spawn(move || {
            for delay in OVERLAY_RECHECKS {
                std::thread::sleep(delay);
                if !late.is_visible().unwrap_or(false) {
                    return;
                }
                keep_placed(&late, "settled");
            }
        });
    focused
}

/// Where the overlay must be (physical pixels): the whole captured desktop. Kept per snip.
static INTENDED: std::sync::Mutex<Option<RECT>> = std::sync::Mutex::new(None);
/// When to look again after showing the overlay.
const OVERLAY_RECHECKS: [Duration; 2] = [Duration::from_millis(150), Duration::from_millis(700)];

/// Puts the overlay window exactly over the captured desktop if Windows (or a DPI rescale) left
/// it elsewhere. After waking displays, Windows can move windows onto one monitor, and a window
/// rescaled for one monitor's DPI shrinks: the overlay then squeezed all monitors into one. The
/// page follows the window's size, so a corrected window shows the frame right again.
fn keep_placed(window: &tauri::WebviewWindow, moment: &str) {
    let Some(target) = *INTENDED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    else {
        return;
    };
    let Ok(raw) = window.hwnd() else {
        return;
    };
    let hwnd = HWND(raw.0);
    // A move can cross a DPI boundary and be rescaled once more, so try a few times.
    for attempt in 0..3 {
        let mut actual = RECT::default();
        // SAFETY: the handle belongs to this live window; the call only reads its rectangle.
        if unsafe { GetWindowRect(hwnd, &mut actual) }.is_err() || actual == target {
            return;
        }
        log::warn!(
            "Snip overlay {moment} at {},{} {}x{} instead of {},{} {}x{}; placing it again (try {})",
            actual.left,
            actual.top,
            actual.right - actual.left,
            actual.bottom - actual.top,
            target.left,
            target.top,
            target.right - target.left,
            target.bottom - target.top,
            attempt + 1
        );
        // SAFETY: as above; the call only moves and sizes this window.
        let _ = unsafe {
            SetWindowPos(
                hwnd,
                None,
                target.left,
                target.top,
                target.right - target.left,
                target.bottom - target.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            )
        };
    }
}

pub fn hide_overlay(app: &tauri::AppHandle) -> Result<(), String> {
    let Some(overlay) = app.get_webview_window(OVERLAY) else {
        return Ok(());
    };
    let topmost_result = overlay.set_always_on_top(false);
    let hide_result = overlay.hide();
    // The page drops its copies of the frame (tens of MB on large desktops) until the next snip.
    if let Err(error) = app.emit("overlay-reset", ()) {
        log::warn!("Could not reset the overlay page: {error}");
    }
    topmost_result.map_err(|error| format!("Failed to release overlay topmost state: {error}"))?;
    hide_result.map_err(|error| format!("Failed to hide overlay: {error}"))
}

pub fn show_flyout(app: &tauri::AppHandle) -> Result<(), String> {
    let Some(flyout) = app.get_webview_window(FLYOUT) else {
        log::warn!("Flyout was not created; skipping capture notification");
        return Ok(());
    };
    let cursor = crate::screen::cursor_position();
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: cursor.0,
                y: cursor.1,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    if monitor.is_invalid() {
        return Err("Could not find a monitor for the flyout".to_owned());
    }
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return Err("Could not read the flyout monitor work area".to_owned());
    }
    let mut dpi_x = 96_u32;
    let mut dpi_y = 96_u32;
    if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }.is_err() {
        dpi_x = 96;
        dpi_y = 96;
    }
    let width = FLYOUT_SIZE.0 * i64::from(dpi_x) / 96;
    let height = FLYOUT_SIZE.1 * i64::from(dpi_y) / 96;
    let margin = FLYOUT_MARGIN * i64::from(dpi_x) / 96;
    let x = i64::from(info.rcWork.right) - width - margin;
    let y = i64::from(info.rcWork.bottom) - height - margin;
    // Size, place and show on the window's own thread, in that order: from a worker, Tauri would
    // queue the size and position while the native show ran at once.
    let shown = flyout.clone();
    flyout
        .run_on_main_thread(move || {
            // Move onto the target monitor first: a size set before the move would be rescaled when
            // the window crosses to a monitor with another DPI. Then size and place it for good.
            let position = PhysicalPosition::new(x as i32, y as i32);
            let placed = shown
                .set_position(position)
                .and_then(|()| shown.set_size(PhysicalSize::new(width as u32, height as u32)))
                .and_then(|()| shown.set_position(position));
            if let Err(error) = placed {
                log::warn!("Could not place the flyout: {error}");
            }
            if let Err(error) = show_without_focus(&shown) {
                log::warn!("{error}");
            }
        })
        .map_err(|error| format!("Failed to show flyout: {error}"))
}

/// Shows a window without activating it, the way notifications appear: the window the user works
/// in keeps the keyboard. Clicking the window still activates it. Tauri does not see this show, so
/// the window must be hidden with `hide_natively`.
fn show_without_focus(window: &tauri::WebviewWindow) -> Result<(), String> {
    let raw = window
        .hwnd()
        .map_err(|error| format!("Failed to show flyout: {error}"))?;
    // SAFETY: the handle belongs to this live window; ShowWindow only changes its visibility.
    let _ = unsafe { ShowWindow(HWND(raw.0), SW_SHOWNOACTIVATE) };
    Ok(())
}

pub fn hide_flyout(app: &tauri::AppHandle) -> Result<(), String> {
    if let Some(flyout) = app.get_webview_window(FLYOUT) {
        hide_natively(&flyout);
    }
    Ok(())
}

/// Hides a window shown with `show_without_focus`. Tauri did not see that show, so its own
/// `hide` would think the window is already hidden and do nothing. Returns whether it was visible.
pub fn hide_natively(window: &tauri::WebviewWindow) -> bool {
    let Ok(raw) = window.hwnd() else {
        return false;
    };
    let hwnd = HWND(raw.0);
    // SAFETY: the handle belongs to this live window; these calls only read or change visibility.
    unsafe {
        if !IsWindowVisible(hwnd).as_bool() {
            return false;
        }
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
    true
}

/// Shows the main window once its page has rendered (`main_ready`), or after a timeout if the page
/// never reports, so a slow or broken page cannot keep the window hidden.
pub fn show_main_when_ready(app: &tauri::AppHandle) {
    MAIN_PENDING.store(true, Ordering::SeqCst);
    if MAIN_PAGE_READY.load(Ordering::SeqCst) {
        reveal_pending_main(app);
        return;
    }
    let worker = app.clone();
    let spawned = std::thread::Builder::new()
        .name("snippy-main-reveal".to_owned())
        .spawn(move || {
            std::thread::sleep(MAIN_READY_TIMEOUT);
            if MAIN_PENDING.load(Ordering::Acquire) {
                log::warn!("The main page did not report ready in time; showing it anyway");
                crate::trace::step(
                    "main page did not report ready in 3 s; showing the window anyway",
                );
                reveal_pending_main(&worker);
            }
        });
    if let Err(error) = spawned {
        log::warn!("Could not start the main window timer ({error}); showing it now");
        reveal_pending_main(app);
    }
}

/// Called by the main page after its first render.
pub fn main_ready(app: &tauri::AppHandle) {
    MAIN_PAGE_READY.store(true, Ordering::SeqCst);
    reveal_pending_main(app);
}

/// Shows the main window if a startup show is waiting for it (exactly once).
fn reveal_pending_main(app: &tauri::AppHandle) {
    if MAIN_PENDING.swap(false, Ordering::SeqCst) {
        match show_main(app) {
            Ok(()) => crate::trace::step("main window shown: startup complete"),
            Err(error) => {
                log::warn!("Could not show the main window: {error}");
                crate::trace::step(&format!("could not show the main window: {error}"));
            }
        }
    }
}

pub fn show_main(app: &tauri::AppHandle) -> Result<(), String> {
    MAIN_PENDING.store(false, Ordering::Release);
    let main = app
        .get_webview_window(MAIN)
        .ok_or_else(|| "Main window is unavailable".to_owned())?;
    main.show()
        .map_err(|error| format!("Failed to show main window: {error}"))?;
    main.unminimize()
        .map_err(|error| format!("Failed to restore main window: {error}"))?;
    main.set_focus()
        .map_err(|error| format!("Failed to focus main window: {error}"))
}
