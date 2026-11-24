//! Remembers where the main window was, so it reopens on the same screen instead of the centre.
//!
//! The position is machine-specific (it depends on the monitor layout), so it lives in the local
//! data folder, not in the roaming config. A saved position is used only while a monitor is still
//! there; the window is pulled fully into that monitor's work area, and with no monitor at that
//! spot (unplugged, layout changed) the window stays centred.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{PhysicalPosition, PhysicalSize};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONULL,
};

/// Moves arrive continuously while dragging; the position is written once it settles.
const SAVE_DELAY: Duration = Duration::from_millis(600);
/// Windows parks minimised windows around (-32000, -32000); never remember that.
const MINIMIZED_COORDINATE: i32 = -30_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Placement {
    x: i32,
    y: i32,
}

static LATEST: Mutex<Option<Placement>> = Mutex::new(None);
static SAVE_SCHEDULED: AtomicBool = AtomicBool::new(false);

fn path() -> PathBuf {
    crate::config::local_data_dir().join("window.json")
}

/// Puts the window where it was last time, if that place is still on a monitor.
pub fn restore(window: &tauri::WebviewWindow) {
    let Some(saved) = std::fs::read(path())
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Placement>(&bytes).ok())
    else {
        return;
    };
    let Ok(size) = window.outer_size() else {
        return;
    };
    let Some(work_area) = work_area_at(saved, size) else {
        log::info!("The screen Snippy was last on is gone; centring the window");
        return;
    };
    let target = fit_into(saved, size, work_area);
    if let Err(error) = window.set_position(PhysicalPosition::new(target.x, target.y)) {
        log::warn!("Could not restore the window position: {error}");
    }
}

/// Records a move of the main window; written to disk once the window stops moving.
pub fn remember(position: PhysicalPosition<i32>) {
    if position.x <= MINIMIZED_COORDINATE || position.y <= MINIMIZED_COORDINATE {
        return;
    }
    *LATEST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Placement {
        x: position.x,
        y: position.y,
    });
    if SAVE_SCHEDULED.swap(true, Ordering::AcqRel) {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(SAVE_DELAY);
        SAVE_SCHEDULED.store(false, Ordering::Release);
        let latest = *LATEST
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(placement) = latest {
            save(placement);
        }
    });
}

fn save(placement: Placement) {
    let path = path();
    let result = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            serde_json::to_vec(&placement)
                .map_err(std::io::Error::other)
                .and_then(|json| std::fs::write(&path, json))
        });
    if let Err(error) = result {
        log::warn!("Could not remember the window position: {error}");
    }
}

/// The work area of the monitor under the middle of the window's top edge, if any.
fn work_area_at(at: Placement, size: PhysicalSize<u32>) -> Option<RECT> {
    let point = POINT {
        x: at.x.saturating_add((size.width / 2) as i32),
        y: at.y.saturating_add(16),
    };
    // SAFETY: plain Win32 queries; `info` is sized as the API requires.
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONULL);
        if monitor.is_invalid() {
            return None;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut info)
            .as_bool()
            .then_some(info.rcWork)
    }
}

/// Moves the window just enough to be fully inside `area` (top-left wins if it is too big).
fn fit_into(at: Placement, size: PhysicalSize<u32>, area: RECT) -> Placement {
    let width = i32::try_from(size.width).unwrap_or(i32::MAX);
    let height = i32::try_from(size.height).unwrap_or(i32::MAX);
    let clamp = |value: i32, low: i32, high: i32, extent: i32| {
        value.min(high.saturating_sub(extent)).max(low)
    };
    Placement {
        x: clamp(at.x, area.left, area.right, width),
        y: clamp(at.y, area.top, area.bottom, height),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AREA: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    const SIZE: PhysicalSize<u32> = PhysicalSize {
        width: 560,
        height: 460,
    };

    #[test]
    fn a_window_inside_the_work_area_stays_put() {
        let at = Placement { x: 300, y: 200 };
        assert_eq!(fit_into(at, SIZE, AREA), at);
    }

    #[test]
    fn a_window_hanging_off_an_edge_is_pulled_back_in() {
        assert_eq!(
            fit_into(Placement { x: 1700, y: 900 }, SIZE, AREA),
            Placement { x: 1360, y: 580 }
        );
        assert_eq!(
            fit_into(Placement { x: -50, y: -10 }, SIZE, AREA),
            Placement { x: 0, y: 0 }
        );
    }

    #[test]
    fn a_window_bigger_than_the_area_keeps_its_top_left_visible() {
        let small = RECT {
            left: 100,
            top: 50,
            right: 500,
            bottom: 400,
        };
        assert_eq!(
            fit_into(Placement { x: 300, y: 300 }, SIZE, small),
            Placement { x: 100, y: 50 }
        );
    }

    #[test]
    fn far_away_positions_have_no_monitor() {
        assert!(work_area_at(
            Placement {
                x: 900_000,
                y: 900_000
            },
            SIZE
        )
        .is_none());
    }

    #[test]
    fn saved_placements_round_trip_and_garbage_is_ignored() {
        let json = serde_json::to_vec(&Placement { x: -1080, y: 12 }).unwrap();
        assert_eq!(
            serde_json::from_slice::<Placement>(&json).unwrap(),
            Placement { x: -1080, y: 12 }
        );
        assert!(serde_json::from_slice::<Placement>(b"{\"x\":\"a\"}").is_err());
    }
}
