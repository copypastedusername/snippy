use image::RgbaImage;
use serde::Serialize;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::null_mut;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, EnumDisplayMonitors,
    GetDC, GetMonitorInfoW, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFO, RGBQUAD, SRCCOPY,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    DrawIconEx, GetCursorInfo, GetCursorPos, GetIconInfo, GetSystemMetrics, CURSORINFO,
    CURSOR_SHOWING, DI_NORMAL, HICON, ICONINFO, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

pub struct Frame {
    pub origin: (i32, i32),
    pub rgba: RgbaImage,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonitorInfo {
    pub index: usize,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale: f64,
    pub primary: bool,
}

struct ScreenDc(HDC);

impl Drop for ScreenDc {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(HWND::default(), self.0);
        }
    }
}

struct MemoryDc(HDC);

impl Drop for MemoryDc {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.0);
        }
    }
}

struct Bitmap(HBITMAP);

impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.0 .0));
        }
    }
}

struct Selection {
    dc: HDC,
    previous: HGDIOBJ,
}

impl Drop for Selection {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.previous);
        }
    }
}

pub fn capture_virtual_screen(include_cursor: bool) -> Result<Frame, String> {
    let origin = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
        )
    };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 || width > 100_000 || height > 100_000 {
        return Err(format!(
            "Invalid virtual-screen dimensions: {width}x{height}"
        ));
    }
    let byte_count = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(height as usize))
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= 1_073_741_824)
        .ok_or_else(|| format!("Virtual screen is too large to capture: {width}x{height}"))?;

    let screen_dc = ScreenDc(unsafe { GetDC(HWND::default()) });
    if screen_dc.0.is_invalid() {
        return Err("GetDC failed for the desktop".to_owned());
    }
    let memory_dc = MemoryDc(unsafe { CreateCompatibleDC(screen_dc.0) });
    if memory_dc.0.is_invalid() {
        return Err("CreateCompatibleDC failed".to_owned());
    }

    let bitmap_info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [RGBQUAD::default(); 1],
    };
    let mut bits: *mut c_void = null_mut();
    let bitmap = Bitmap(unsafe {
        CreateDIBSection(
            screen_dc.0,
            &bitmap_info,
            DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        )
        .map_err(|error| format!("CreateDIBSection failed: {error}"))?
    });
    if bits.is_null() {
        return Err("CreateDIBSection returned no pixel buffer".to_owned());
    }

    let previous = unsafe { SelectObject(memory_dc.0, HGDIOBJ(bitmap.0 .0)) };
    if previous.is_invalid() {
        return Err("SelectObject failed for capture bitmap".to_owned());
    }
    let _selection = Selection {
        dc: memory_dc.0,
        previous,
    };

    unsafe {
        BitBlt(
            memory_dc.0,
            0,
            0,
            width,
            height,
            screen_dc.0,
            origin.0,
            origin.1,
            SRCCOPY | CAPTUREBLT,
        )
        .map_err(|error| format!("BitBlt failed: {error}"))?;
    }

    if include_cursor {
        draw_cursor(memory_dc.0, origin);
    }
    // GDI may batch drawing; the DIB's bits are only safe to read once it has run.
    unsafe {
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();
    }

    let bgra = unsafe { std::slice::from_raw_parts(bits.cast::<u8>(), byte_count) };
    let mut rgba = vec![0_u8; byte_count];
    for (source, destination) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        destination[0] = source[2];
        destination[1] = source[1];
        destination[2] = source[0];
        destination[3] = 255;
    }
    let rgba = RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| "Failed to construct captured image".to_owned())?;
    Ok(Frame { origin, rgba })
}

fn draw_cursor(dc: HDC, origin: (i32, i32)) {
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if unsafe { GetCursorInfo(&mut cursor) }.is_err() || cursor.flags != CURSOR_SHOWING {
        return;
    }

    let icon = HICON(cursor.hCursor.0);
    let mut icon_info = ICONINFO::default();
    if unsafe { GetIconInfo(icon, &mut icon_info) }.is_err() {
        return;
    }
    let x = cursor.ptScreenPos.x - origin.0 - icon_info.xHotspot as i32;
    let y = cursor.ptScreenPos.y - origin.1 - icon_info.yHotspot as i32;
    if let Err(error) = unsafe { DrawIconEx(dc, x, y, icon, 0, 0, 0, None, DI_NORMAL) } {
        log::warn!("Could not draw the cursor into the capture: {error}");
    }
    unsafe {
        if !icon_info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmMask.0));
        }
        if !icon_info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(icon_info.hbmColor.0));
        }
    }
}

/// The monitors in desktop coordinates, in the order Windows lists them (the order snips number
/// them in). Compared before and after a grab to tell whether the pixels match the layout.
#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    monitors: Vec<MonitorInfo>,
}

impl Layout {
    pub fn current() -> Self {
        Self {
            monitors: monitors((0, 0)),
        }
    }

    /// The same monitors in the same places, in whatever order Windows listed them.
    pub fn same_as(&self, other: &Self) -> bool {
        let key = |layout: &Self| {
            let mut keys: Vec<_> = layout
                .monitors
                .iter()
                .map(|m| (m.x, m.y, m.width, m.height, m.primary, m.scale.to_bits()))
                .collect();
            keys.sort_unstable();
            keys
        };
        key(self) == key(other)
    }

    /// Whether `frame` spans exactly the bounding box of these monitors (the virtual screen).
    pub fn covers(&self, frame: &Frame) -> bool {
        let bounds = self
            .monitors
            .iter()
            .fold(None::<(i64, i64, i64, i64)>, |bounds, m| {
                let right = i64::from(m.x) + i64::from(m.width);
                let bottom = i64::from(m.y) + i64::from(m.height);
                let (x, y) = (i64::from(m.x), i64::from(m.y));
                Some(match bounds {
                    None => (x, y, right, bottom),
                    Some((l, t, r, b)) => (l.min(x), t.min(y), r.max(right), b.max(bottom)),
                })
            });
        let Some((left, top, right, bottom)) = bounds else {
            return false;
        };
        (i64::from(frame.origin.0), i64::from(frame.origin.1)) == (left, top)
            && i64::from(frame.rgba.width()) == right - left
            && i64::from(frame.rgba.height()) == bottom - top
    }

    /// The monitors with positions relative to the frame's top-left corner.
    pub fn relative_to(&self, origin: (i32, i32)) -> Vec<MonitorInfo> {
        self.monitors
            .iter()
            .map(|m| MonitorInfo {
                x: m.x - origin.0,
                y: m.y - origin.1,
                ..m.clone()
            })
            .collect()
    }
}

fn monitors(origin: (i32, i32)) -> Vec<MonitorInfo> {
    struct Context {
        origin: (i32, i32),
        monitors: Vec<MonitorInfo>,
    }

    unsafe extern "system" fn callback(
        monitor: HMONITOR,
        _dc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let context = &mut *(data.0 as *mut Context);
        let mut info = MONITORINFO {
            cbSize: size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return true.into();
        }
        let width = info.rcMonitor.right - info.rcMonitor.left;
        let height = info.rcMonitor.bottom - info.rcMonitor.top;
        if width <= 0 || height <= 0 {
            return true.into();
        }
        let mut dpi_x = 96;
        let mut dpi_y = 96;
        let scale = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_ok()
        {
            f64::from(dpi_x) / 96.0
        } else {
            1.0
        };
        context.monitors.push(MonitorInfo {
            index: context.monitors.len(),
            x: info.rcMonitor.left - context.origin.0,
            y: info.rcMonitor.top - context.origin.1,
            width: width as u32,
            height: height as u32,
            scale,
            primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
        });
        true.into()
    }

    let mut context = Context {
        origin,
        monitors: Vec::new(),
    };
    let success = unsafe {
        EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(callback),
            LPARAM((&mut context as *mut Context) as isize),
        )
    };
    if !success.as_bool() {
        log::warn!("EnumDisplayMonitors failed");
    }
    context.monitors
}

pub fn cursor_position() -> (i32, i32) {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_ok() {
        (point.x, point.y)
    } else {
        (0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(index: usize, x: i32, y: i32, width: u32, height: u32) -> MonitorInfo {
        MonitorInfo {
            index,
            x,
            y,
            width,
            height,
            scale: 1.0,
            primary: index == 0,
        }
    }

    fn frame(origin: (i32, i32), width: u32, height: u32) -> Frame {
        Frame {
            origin,
            rgba: RgbaImage::new(width, height),
        }
    }

    /// A portrait monitor on the left and two landscape ones at different heights.
    fn staggered() -> Layout {
        Layout {
            monitors: vec![
                monitor(0, 0, 0, 1920, 1080),
                monitor(1, -1080, -400, 1080, 1920),
                monitor(2, 1920, -200, 1920, 1080),
            ],
        }
    }

    #[test]
    fn the_same_monitors_listed_in_another_order_are_the_same_layout() {
        let layout = staggered();
        let mut reordered = layout.clone();
        reordered.monitors.reverse();
        assert!(layout.same_as(&reordered));
    }

    #[test]
    fn a_moved_rescaled_or_missing_monitor_is_another_layout() {
        let layout = staggered();
        let mut moved = layout.clone();
        moved.monitors[2].y = 0;
        assert!(!layout.same_as(&moved));
        let mut rescaled = layout.clone();
        rescaled.monitors[0].scale = 1.5;
        assert!(!layout.same_as(&rescaled));
        let mut fewer = layout.clone();
        fewer.monitors.pop();
        assert!(!layout.same_as(&fewer));
    }

    #[test]
    fn a_frame_matches_the_layout_only_when_it_spans_its_bounding_box() {
        let layout = staggered();
        // Left -1080, top -400, right 3840, bottom 1520.
        assert!(layout.covers(&frame((-1080, -400), 4920, 1920)));
        assert!(!layout.covers(&frame((0, 0), 4920, 1920)));
        assert!(!layout.covers(&frame((-1080, -400), 3840, 1920)));
        let empty = Layout {
            monitors: Vec::new(),
        };
        assert!(!empty.covers(&frame((0, 0), 1, 1)));
    }

    #[test]
    fn monitors_are_placed_relative_to_the_frame_in_their_original_order() {
        let relative = staggered().relative_to((-1080, -400));
        let places: Vec<_> = relative.iter().map(|m| (m.index, m.x, m.y)).collect();
        assert_eq!(places, vec![(0, 1080, 400), (1, 0, 0), (2, 3000, 200)]);
    }
}
