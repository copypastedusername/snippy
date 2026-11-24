use serde::Serialize;
use std::ffi::c_void;
use std::mem::size_of;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, GWL_EXSTYLE,
    WS_EX_TOOLWINDOW,
};

#[derive(Debug, Clone, Serialize)]
pub struct WindowEntry {
    pub id: u64,
    pub title: String,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub fn top_level_windows(origin: (i32, i32), bounds: (u32, u32)) -> Vec<WindowEntry> {
    struct Context {
        origin: (i32, i32),
        bounds: (u32, u32),
        process_id: u32,
        windows: Vec<WindowEntry>,
    }

    unsafe extern "system" fn callback(window: HWND, data: LPARAM) -> BOOL {
        let context = &mut *(data.0 as *mut Context);
        if !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() {
            return true.into();
        }

        let mut process_id = 0;
        GetWindowThreadProcessId(window, Some(&mut process_id));
        if process_id == context.process_id {
            return true.into();
        }
        if GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return true.into();
        }

        let mut cloaked = 0_u32;
        if DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast::<c_void>(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return true.into();
        }

        let mut class_buffer = [0_u16; 256];
        let class_length = GetClassNameW(window, &mut class_buffer);
        if class_length > 0 {
            let class_name = String::from_utf16_lossy(&class_buffer[..class_length as usize]);
            if matches!(class_name.as_str(), "Progman" | "WorkerW" | "Shell_TrayWnd") {
                return true.into();
            }
        }

        let title_length = GetWindowTextLengthW(window);
        if title_length <= 0 || title_length > 32_767 {
            return true.into();
        }
        let mut title_buffer = vec![0_u16; title_length as usize + 1];
        let copied = GetWindowTextW(window, &mut title_buffer);
        if copied <= 0 {
            return true.into();
        }
        let title = String::from_utf16_lossy(&title_buffer[..copied as usize]);
        if title.trim().is_empty() {
            return true.into();
        }

        let mut rect = RECT::default();
        if DwmGetWindowAttribute(
            window,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&mut rect as *mut RECT).cast::<c_void>(),
            size_of::<RECT>() as u32,
        )
        .is_err()
            && GetWindowRect(window, &mut rect).is_err()
        {
            return true.into();
        }

        let frame_left = i64::from(context.origin.0);
        let frame_top = i64::from(context.origin.1);
        let frame_right = frame_left + i64::from(context.bounds.0);
        let frame_bottom = frame_top + i64::from(context.bounds.1);
        let left = i64::from(rect.left).clamp(frame_left, frame_right);
        let top = i64::from(rect.top).clamp(frame_top, frame_bottom);
        let right = i64::from(rect.right).clamp(frame_left, frame_right);
        let bottom = i64::from(rect.bottom).clamp(frame_top, frame_bottom);
        if right <= left || bottom <= top {
            return true.into();
        }

        context.windows.push(WindowEntry {
            id: window.0 as usize as u64,
            title,
            x: (left - frame_left) as i32,
            y: (top - frame_top) as i32,
            width: (right - left) as u32,
            height: (bottom - top) as u32,
        });
        true.into()
    }

    let mut context = Context {
        origin,
        bounds,
        process_id: unsafe { GetCurrentProcessId() },
        windows: Vec::new(),
    };
    if let Err(error) = unsafe {
        EnumWindows(
            Some(callback),
            LPARAM((&mut context as *mut Context) as isize),
        )
    } {
        log::warn!("EnumWindows failed: {error}");
    }
    context.windows
}
