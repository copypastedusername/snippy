//! A never-shown window with its own thread and message loop, for Windows messages that only reach
//! a window (launch hand-overs, session and display notifications).

use std::sync::mpsc;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_CLASS_ALREADY_EXISTS, HWND};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DispatchMessageW, GetMessageW, RegisterClassExW, TranslateMessage, MSG,
    WINDOW_EX_STYLE, WNDCLASSEXW, WNDPROC, WS_OVERLAPPED,
};

/// Creates the window on a thread named `thread`, calls `on_created` with it on that thread, then
/// runs its message loop; `on_closed` runs if that loop ever ends. Returns once the window exists,
/// or why it could not be created.
///
/// It is a top-level window rather than a message-only one: message-only windows miss broadcasts
/// such as `WM_DISPLAYCHANGE`, and other processes cannot find them by class and title.
pub fn spawn(
    thread: &str,
    class: &'static str,
    title: &'static str,
    procedure: WNDPROC,
    on_created: impl FnOnce(HWND) + Send + 'static,
    on_closed: impl FnOnce(HWND) + Send + 'static,
) -> Result<(), String> {
    let (created, outcome) = mpsc::channel();
    std::thread::Builder::new()
        .name(thread.to_owned())
        .spawn(move || match create(class, title, procedure) {
            Ok(window) => {
                let _ = created.send(Ok(()));
                on_created(window);
                run_message_loop();
                on_closed(window);
            }
            Err(error) => {
                let _ = created.send(Err(error));
            }
        })
        .map_err(|error| format!("Could not start the {thread} thread: {error}"))?;
    outcome
        .recv()
        .map_err(|_| format!("The {thread} thread stopped before creating its window"))?
}

fn create(class: &str, title: &str, procedure: WNDPROC) -> Result<HWND, String> {
    let class = wide(class);
    let title = wide(title);
    // SAFETY: the class and title are NUL-terminated and outlive both calls; the window belongs
    // to the calling thread, which runs its message loop afterwards.
    unsafe {
        let instance =
            GetModuleHandleW(None).map_err(|error| format!("GetModuleHandleW failed: {error}"))?;
        let window_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: procedure,
            hInstance: instance.into(),
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        if RegisterClassExW(&window_class) == 0 {
            let error = windows::core::Error::from_win32();
            if error.code() != ERROR_CLASS_ALREADY_EXISTS.to_hresult() {
                return Err(format!("RegisterClassExW failed: {error}"));
            }
        }
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            instance,
            None,
        )
        .map_err(|error| format!("CreateWindowExW failed: {error}"))
    }
}

fn run_message_loop() {
    let mut message = MSG::default();
    loop {
        // SAFETY: the standard loop for the windows this thread created.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        match result {
            0 => return, // WM_QUIT
            -1 => {
                log::warn!(
                    "A hidden window's message loop failed: {}",
                    windows::core::Error::from_win32()
                );
                return;
            }
            _ => {
                // SAFETY: `message` was just filled in by GetMessageW.
                unsafe {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
