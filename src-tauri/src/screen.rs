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

#[derive(Debug, Clone, Serialize)]
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

pub fn monitors(origin: (i32, i32)) -> Vec<MonitorInfo> {
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
