//! The snip workflow: grab the screen, let the user select in the overlay, then save, copy and
//! announce the result. See `docs/contract.md` for the event sequence the frontends rely on.

use crate::config::{CaptureMode, Config};
use crate::screen::MonitorInfo;
use crate::storage::{self, CaptureItem};
use crate::{
    clipboard, display_watch, imageops, overlay, screen, winlist, AppState, CaptureSession,
};
use image::RgbaImage;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

/// Pause after hiding Snippy's own windows so the compositor drops them before the grab.
const HIDE_SETTLE: Duration = Duration::from_millis(80);
/// The overlay page must report a painted frame by then, or the snip is given up (showing the
/// window without the frame would expose a blank or previous screen). Generous for slow PCs with
/// large desktops.
const OVERLAY_READY_TIMEOUT: Duration = Duration::from_secs(6);
/// Grabs tried while the monitor layout keeps changing under them.
const GRAB_ATTEMPTS: u32 = 3;
/// How long a snip waits, over all its grabs, for the desktop to be unlocked and settled; and how
/// often it looks. A desktop still settling by then is grabbed anyway; a locked one is not.
const SCREEN_WAIT: Duration = Duration::from_secs(10);
const WAIT_STEP: Duration = Duration::from_millis(100);
pub const MAX_DELAY_SECONDS: u64 = 10;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
/// The snip that currently owns the capture. Cancelling moves it on, so a worker still sleeping
/// through its delay (or grabbing the screen) finds itself stale and stops without touching the
/// newer snip's state.
static CURRENT: AtomicU64 = AtomicU64::new(0);

fn is_current(generation: u64) -> bool {
    CURRENT.load(Ordering::Acquire) == generation
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureResult {
    pub path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub saved: bool,
    pub copied: bool,
    pub thumbnail: String,
    /// Why saving failed, when the snip still reached the clipboard.
    pub save_error: Option<String>,
}

/// Starts a snip on a worker thread. A request while another snip is running is ignored.
pub fn request_start(
    app: &tauri::AppHandle,
    mode: CaptureMode,
    delay: Option<u64>,
) -> Result<(), String> {
    let state = app.state::<AppState>();
    if state
        .capturing
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }

    let configured = match state.config() {
        Ok(config) => config.capture,
        Err(error) => {
            state.capturing.store(false, Ordering::Release);
            return Err(error);
        }
    };
    let delay = delay
        .unwrap_or(configured.delay_seconds)
        .min(MAX_DELAY_SECONDS);
    if let Err(error) = app.emit("capture-started", mode.as_str()) {
        state.capturing.store(false, Ordering::Release);
        return Err(format!("Failed to emit capture-started: {error}"));
    }

    let generation = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    CURRENT.store(generation, Ordering::Release);
    let worker_app = app.clone();
    std::thread::Builder::new()
        .name("snippy-capture".to_owned())
        .spawn(move || {
            let result = run_capture(
                &worker_app,
                generation,
                mode,
                delay,
                configured.include_cursor,
            );
            if let Err(error) = result {
                if is_current(generation) {
                    fail(&worker_app, error);
                }
            }
        })
        .map(|_| ())
        .map_err(|error| {
            let message = format!("Failed to start capture worker: {error}");
            fail(app, message.clone());
            message
        })
}

fn run_capture(
    app: &tauri::AppHandle,
    generation: u64,
    mode: CaptureMode,
    delay: u64,
    include_cursor: bool,
) -> Result<(), String> {
    if overlay::hide_visible_snippy_windows(app) {
        std::thread::sleep(HIDE_SETTLE);
    }
    if delay != 0 {
        std::thread::sleep(Duration::from_secs(delay));
    }
    if !is_current(generation) {
        return Ok(()); // Cancelled during the delay.
    }

    let Some((frame, monitors)) = grab(generation, include_cursor)? else {
        return Ok(()); // Cancelled while the screen settled.
    };

    if mode == CaptureMode::Fullscreen {
        let monitor = monitor_under_cursor(&monitors, frame.origin)
            .ok_or_else(|| "No display monitor is available".to_owned())?;
        let image = crop_to(
            &frame.rgba,
            monitor.x,
            monitor.y,
            monitor.width,
            monitor.height,
        )?;
        if !is_current(generation) {
            return Ok(());
        }
        return complete(app, image).map(|_| ());
    }

    let origin = frame.origin;
    let size = frame.rgba.dimensions();
    let windows = winlist::top_level_windows(origin, size);
    let id = generation;
    let state = app.state::<AppState>();
    let mut session = crate::lock(&state.session, "capture session")?;
    if !is_current(generation) {
        return Ok(()); // Cancelled while the screen was being grabbed.
    }
    *session = Some(CaptureSession {
        id,
        frame,
        mode,
        windows,
        monitors,
    });
    drop(session);
    overlay::prepare_overlay(app, origin, size)?;

    // The page reports a painted frame with `overlay_ready`. If it has not by now, revealing the
    // window would show a blank or previous frame: give the snip up and say why instead.
    std::thread::sleep(OVERLAY_READY_TIMEOUT);
    if current_session_id(app) == Some(id) && !overlay::overlay_visible(app) {
        return Err("The selection overlay did not draw the screen in time".to_owned());
    }
    Ok(())
}

/// The desktop's pixels and the monitors they show, once the screen has settled; `None` when the
/// snip was cancelled meanwhile. A layout that changes during a grab (monitors waking up and
/// being re-arranged) would pair the pixels with the wrong monitors, so that grab is repeated.
fn grab(
    generation: u64,
    include_cursor: bool,
) -> Result<Option<(screen::Frame, Vec<MonitorInfo>)>, String> {
    let deadline = Instant::now() + SCREEN_WAIT;
    for _ in 0..GRAB_ATTEMPTS {
        let Some(events) = wait_for_screen(generation, deadline)? else {
            return Ok(None);
        };
        let before = screen::Layout::current();
        let frame = screen::capture_virtual_screen(include_cursor);
        let after = screen::Layout::current();
        // A lock, wake or layout change during the grab may have left the pixels stale or
        // misplaced even when the monitors end up where they were.
        if !before.same_as(&after) || display_watch::readiness().1 != events {
            log::info!("The display changed during the grab; grabbing again");
            continue;
        }
        // The layout held still, so a failed grab is a real failure.
        let frame = frame?;
        if after.covers(&frame) {
            let monitors = after.relative_to(frame.origin);
            return Ok(Some((frame, monitors)));
        }
        log::info!("The grab does not span the monitors; grabbing again");
    }
    Err("The display layout kept changing. Try the snip again.".to_owned())
}

/// Waits until the desktop is unlocked and settled, or `deadline` passes. Returns the display
/// event count at that moment, or `None` when the snip was cancelled.
fn wait_for_screen(generation: u64, deadline: Instant) -> Result<Option<u64>, String> {
    use display_watch::Readiness;
    loop {
        if !is_current(generation) {
            return Ok(None);
        }
        let (readiness, events) = display_watch::readiness();
        let late = Instant::now() >= deadline;
        match readiness {
            Readiness::Ready => return Ok(Some(events)),
            Readiness::Settling(_) if late => return Ok(Some(events)),
            Readiness::Locked if late => return Err("The screen is locked".to_owned()),
            Readiness::Locked => std::thread::sleep(WAIT_STEP),
            Readiness::Settling(left) => std::thread::sleep(left.min(WAIT_STEP)),
        }
    }
}

fn monitor_under_cursor(monitors: &[MonitorInfo], origin: (i32, i32)) -> Option<&MonitorInfo> {
    let (x, y) = screen::cursor_position();
    let (x, y) = (
        i64::from(x) - i64::from(origin.0),
        i64::from(y) - i64::from(origin.1),
    );
    monitors
        .iter()
        .find(|monitor| {
            let left = i64::from(monitor.x);
            let top = i64::from(monitor.y);
            (left..left + i64::from(monitor.width)).contains(&x)
                && (top..top + i64::from(monitor.height)).contains(&y)
        })
        .or_else(|| monitors.iter().find(|monitor| monitor.primary))
        .or_else(|| monitors.first())
}

fn crop_to(
    frame: &RgbaImage,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
) -> Result<RgbaImage, String> {
    let width = i32::try_from(width).map_err(|_| "Selection is too wide".to_owned())?;
    let height = i32::try_from(height).map_err(|_| "Selection is too tall".to_owned())?;
    imageops::crop(frame, x, y, width, height)
}

fn current_session_id(app: &tauri::AppHandle) -> Option<u64> {
    app.state::<AppState>()
        .session
        .lock()
        .ok()
        .and_then(|session| session.as_ref().map(|session| session.id))
}

/// Drops the captured frame and lets a new snip start.
fn reset(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    if let Ok(mut session) = state.session.lock() {
        *session = None;
    }
    state.capturing.store(false, Ordering::Release);
}

/// Called by the overlay once it has painted the frame for the current session.
pub fn overlay_ready(app: &tauri::AppHandle) -> Result<(), String> {
    if current_session_id(app).is_some() {
        overlay::reveal_overlay(app)
    } else {
        Ok(())
    }
}

pub fn cancel(app: &tauri::AppHandle) -> Result<(), String> {
    // Any worker of the cancelled snip is now stale.
    CURRENT.store(
        NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
        Ordering::Release,
    );
    let hidden = overlay::hide_overlay(app);
    reset(app);
    app.emit("capture-cancelled", ())
        .map_err(|error| format!("Failed to emit capture-cancelled: {error}"))?;
    hidden
}

pub fn fail(app: &tauri::AppHandle, error: String) {
    log::error!("Capture failed: {error}");
    if let Err(hide_error) = overlay::hide_overlay(app) {
        log::warn!("{hide_error}");
    }
    reset(app);
    if let Err(emit_error) = app.emit("capture-failed", &error) {
        log::error!("Could not report the capture failure: {emit_error}");
    }
    announce(app, Announcement::Failed(&error));
}

enum Announcement<'a> {
    Finished(&'a CaptureResult),
    Failed(&'a str),
}

/// Tells the user how the snip went: in Snippy's flyout, or as a Windows notification when that
/// is chosen (falling back to the flyout if Windows refuses the notification).
fn announce(app: &tauri::AppHandle, announcement: Announcement<'_>) {
    let Ok(config) = app.state::<AppState>().config() else {
        return;
    };
    if !config.behavior.show_flyout {
        return;
    }
    if config.behavior.windows_notifications {
        let shown = match announcement {
            Announcement::Finished(result) => crate::notify::show_capture(result),
            Announcement::Failed(error) => crate::notify::show_failure(error),
        };
        match shown {
            Ok(()) => return,
            Err(error) => log::warn!("Windows notification failed, using the flyout: {error}"),
        }
    }
    if let Err(error) = overlay::show_flyout(app) {
        log::warn!("Could not show the capture flyout: {error}");
    }
}

/// Finishes the open session with the region chosen by `select`. The overlay is hidden before the
/// slow encode/save so the desktop is usable again immediately.
pub async fn finish_with<F>(
    app: tauri::AppHandle,
    session_id: u64,
    select: F,
) -> Result<CaptureResult, String>
where
    F: FnOnce(&CaptureSession) -> Result<RgbaImage, String> + Send + 'static,
{
    // A request from an overlay that belongs to an earlier snip must not touch the current one
    // (hiding its overlay, or saving its frame with the old selection).
    if current_session_id(&app) != Some(session_id) {
        return Err("This snip is no longer current".to_owned());
    }
    if let Err(error) = overlay::hide_overlay(&app) {
        fail(&app, error.clone());
        return Err(error);
    }
    let worker_app = app.clone();
    let outcome = tauri::async_runtime::spawn_blocking(move || {
        let session = {
            let state = worker_app.state::<AppState>();
            let mut current = crate::lock(&state.session, "capture session")?;
            if current.as_ref().map(|session| session.id) != Some(session_id) {
                return Err("This snip is no longer current".to_owned());
            }
            current
                .take()
                .ok_or_else(|| "No captured frame is available".to_owned())?
        };
        let image = select(&session)?;
        drop(session);
        complete(&worker_app, image)
    })
    .await
    .unwrap_or_else(|error| Err(format!("Capture worker failed: {error}")));

    if let Err(error) = &outcome {
        fail(&app, error.clone());
    }
    outcome
}

pub fn crop_window(session: &CaptureSession, window_id: u64) -> Result<RgbaImage, String> {
    let window = session
        .windows
        .iter()
        .find(|window| window.id == window_id)
        .ok_or_else(|| "Selected window is no longer in the captured window list".to_owned())?;
    crop_to(
        &session.frame.rgba,
        window.x,
        window.y,
        window.width,
        window.height,
    )
}

pub fn crop_monitor(session: &CaptureSession, monitor_index: usize) -> Result<RgbaImage, String> {
    let monitor = session
        .monitors
        .iter()
        .find(|monitor| monitor.index == monitor_index)
        .ok_or_else(|| "Selected monitor is not in the captured monitor list".to_owned())?;
    crop_to(
        &session.frame.rgba,
        monitor.x,
        monitor.y,
        monitor.width,
        monitor.height,
    )
}

fn saved_path_reported(save_error: &Option<String>, auto_save: bool) -> bool {
    auto_save && save_error.is_none()
}

fn complete(app: &tauri::AppHandle, image: RgbaImage) -> Result<CaptureResult, String> {
    let config = app.state::<AppState>().config()?;
    // The shutter sounds when the area is chosen, before the slower encode and save.
    crate::sound::play(config.capture.sound);
    let result = deliver(app, &image, &config)?;
    app.state::<AppState>()
        .capturing
        .store(false, Ordering::Release);
    *crate::lock(&app.state::<AppState>().last_capture, "last capture")? =
        Some(std::sync::Arc::new(image));
    app.emit("capture-finished", &result)
        .map_err(|error| format!("Failed to emit capture-finished: {error}"))?;
    announce(app, Announcement::Finished(&result));
    Ok(result)
}

/// Saves and/or copies the image as configured. A clipboard failure is only fatal when the image
/// was not saved either, because then the capture would be lost.
fn deliver(
    app: &tauri::AppHandle,
    image: &RgbaImage,
    config: &Config,
) -> Result<CaptureResult, String> {
    let output = &config.output;
    let thumbnail = imageops::thumbnail_data_url(image);
    // Each destination is tried on its own: a full disk or a lost network folder must not cost the
    // snip that the clipboard can still take (and the editor can still save, see `last_capture`).
    let (saved_path, save_error) = if output.auto_save {
        match storage::save(image, output) {
            Ok(path) => (Some(path), None),
            Err(error) => {
                log::warn!("Capture not saved: {error}");
                (None, Some(error))
            }
        }
    } else {
        (None, None)
    };
    let (copied, copy_error) = if output.copy_to_clipboard {
        match clipboard::copy_image_to_clipboard(image) {
            Ok(()) => (true, None),
            Err(error) => {
                log::warn!("Capture not copied: {error}");
                (false, Some(error))
            }
        }
    } else {
        (false, None)
    };
    if saved_path.is_none() && !copied {
        return Err(match (save_error, copy_error) {
            (Some(save), Some(copy)) => format!("{save}; {copy}"),
            (Some(error), None) | (None, Some(error)) => error,
            (None, None) => "The snip was neither saved nor copied".to_owned(),
        });
    }

    if let Some(path) = &saved_path {
        remember(
            app,
            storage::capture_item(path, image, thumbnail.clone()),
            config,
        );
    }
    Ok(CaptureResult {
        path: saved_path.map(|path| path.to_string_lossy().into_owned()),
        width: image.width(),
        height: image.height(),
        saved: saved_path_reported(&save_error, output.auto_save),
        copied,
        thumbnail,
        save_error,
    })
}

/// Adds a fresh capture to the recents, unless the output folder changed while it was saving.
pub(crate) fn remember(app: &tauri::AppHandle, item: CaptureItem, used: &Config) {
    let state = app.state::<AppState>();
    let still_current = state
        .config()
        .is_ok_and(|current| current.output.directory == used.output.directory);
    if !still_current {
        return;
    }
    match crate::lock(&state.recents, "recent captures") {
        Ok(mut recents) => recents.push(item),
        Err(error) => log::warn!("{error}"),
    };
}
