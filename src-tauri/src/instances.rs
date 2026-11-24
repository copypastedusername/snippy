//! One Snippy per Windows session.
//!
//! The first copy holds a named mutex and runs a hidden receiver window on its own thread. A later
//! launch hands its arguments to that receiver with `WM_COPYDATA` and a timeout, and the receiver
//! answers with an acknowledgement:
//!
//! - `ACCEPTED`: the running copy took the launch; the new process exits.
//! - `STARTING`: the running copy has not finished creating its windows; the launch retries.
//!
//! Every wait is bounded. A copy that does not acknowledge within `GRACE` (hung, or stuck before its
//! windows exist, like builds whose WebView2 never started) is reported, and the user may close it,
//! after the process is verified to be Snippy, so this launch can start.
//!
//! Snippy 0.1 and 0.1.1 used the single-instance plugin instead (`com.snippy.tool-sim` mutex,
//! `com.snippy.tool-sic` receiver). Such a copy cannot take launches from this protocol, so it is
//! recognised as an older version and offered to be replaced.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, FILETIME, HANDLE, HWND, LPARAM, LRESULT,
    WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    CreateMutexW, GetProcessTimes, OpenMutexW, OpenProcess, QueryFullProcessImageNameW,
    TerminateProcess, WaitForSingleObject, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, SYNCHRONIZATION_SYNCHRONIZE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    ChangeWindowMessageFilterEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, FindWindowW,
    GetMessageW, GetWindowThreadProcessId, RegisterClassExW, SendMessageTimeoutW, TranslateMessage,
    MSG, MSGFLT_ALLOW, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, WINDOW_EX_STYLE, WM_COPYDATA,
    WNDCLASSEXW, WS_OVERLAPPED,
};

const MUTEX: &str = "Local\\Snippy.Instance";
const RECEIVER_CLASS: &str = "Snippy.Instance.Receiver";
const RECEIVER_TITLE: &str = "Snippy instance receiver";
/// Marks our `WM_COPYDATA` payload ("SNP1"): the arguments, NUL-separated UTF-16.
const PAYLOAD_TAG: usize = 0x534E_5031;
const ACCEPTED: isize = 1;
const STARTING: isize = 2;

const LEGACY_MUTEX: &str = "com.snippy.tool-sim";
const LEGACY_CLASS: &str = "com.snippy.tool-sic";
const LEGACY_TITLE: &str = "com.snippy.tool-siw";

/// How long a running copy may take to acknowledge before it counts as stuck.
const GRACE: Duration = Duration::from_secs(30);
const SEND_TIMEOUT_MS: u32 = 3000;
const RETRY: Duration = Duration::from_millis(400);

static READY: AtomicBool = AtomicBool::new(false);
static APP: OnceLock<tauri::AppHandle> = OnceLock::new();

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// What `run` does after looking for another copy.
pub enum Outcome {
    /// This process is the Snippy of the session: continue starting.
    Primary,
    /// The launch was handed over, or the user chose not to replace the other copy: exit now.
    Exit,
}

/// Claims the session for this process, or hands the launch to the copy that has it.
pub fn claim(args: &[String]) -> Outcome {
    // An older, plugin-based Snippy holds a different mutex and never answers our receiver.
    if legacy_copy_running() {
        crate::trace::step("single instance: an older Snippy (0.1/0.1.1) is running");
        return recover_from_other_copy(args, true);
    }
    let deadline = Instant::now() + GRACE;
    let mut announced = false;
    loop {
        if take_mutex() {
            return become_primary();
        }
        if !announced {
            crate::trace::step("single instance: another Snippy holds the session");
            announced = true;
        }
        if let Some(window) = find(RECEIVER_CLASS, RECEIVER_TITLE) {
            match hand_over(window, args) {
                Some(ACCEPTED) => {
                    crate::trace::step("handed this launch to the running Snippy; exiting");
                    return Outcome::Exit;
                }
                Some(STARTING) => {}
                answer => {
                    crate::trace::step(&format!("running Snippy did not acknowledge ({answer:?})"))
                }
            }
        }
        if Instant::now() >= deadline {
            return recover_from_other_copy(args, false);
        }
        std::thread::sleep(RETRY);
    }
}

fn become_primary() -> Outcome {
    crate::trace::step("single instance: this is the running Snippy");
    start_receiver();
    Outcome::Primary
}

/// Creates the session mutex; true when no other copy had it. The handle stays open for the life
/// of the process (Windows releases it when Snippy exits, also after a crash).
fn take_mutex() -> bool {
    let name = wide(MUTEX);
    // SAFETY: the name is NUL-terminated. The handle is kept open only when this process created
    // the mutex: a handle to someone else's mutex would keep it alive after they exit, and the
    // session could never be claimed.
    match unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
                unsafe {
                    let _ = CloseHandle(handle);
                }
                false
            } else {
                true
            }
        }
        // A mutex we may not open belongs to an elevated copy of ourselves.
        Err(_) => false,
    }
}

fn legacy_copy_running() -> bool {
    let name = wide(LEGACY_MUTEX);
    // SAFETY: opened only to test existence; the handle is closed at once.
    match unsafe { OpenMutexW(SYNCHRONIZATION_SYNCHRONIZE, false, PCWSTR(name.as_ptr())) } {
        Ok(handle) => {
            unsafe {
                let _ = CloseHandle(handle);
            }
            true
        }
        Err(_) => find(LEGACY_CLASS, LEGACY_TITLE).is_some(),
    }
}

fn find(class: &str, title: &str) -> Option<HWND> {
    let (class, title) = (wide(class), wide(title));
    // SAFETY: both strings are NUL-terminated.
    unsafe { FindWindowW(PCWSTR(class.as_ptr()), PCWSTR(title.as_ptr())) }.ok()
}

/// Sends the arguments with a timeout; `None` when the receiver hung, exited or refused delivery.
fn hand_over(window: HWND, args: &[String]) -> Option<isize> {
    let payload: Vec<u16> = args
        .iter()
        .skip(1)
        .flat_map(|arg| arg.encode_utf16().chain(Some(0)))
        .collect();
    let data = COPYDATASTRUCT {
        dwData: PAYLOAD_TAG,
        cbData: (payload.len() * 2) as u32,
        lpData: payload.as_ptr() as *mut _,
    };
    let mut answer = 0_usize;
    // SAFETY: `data` and `payload` outlive the synchronous call.
    let delivered = unsafe {
        SendMessageTimeoutW(
            window,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(&data as *const COPYDATASTRUCT as isize),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            SEND_TIMEOUT_MS,
            Some(&mut answer),
        )
    };
    (delivered.0 != 0).then_some(answer as isize)
}

/// Lets the receiver accept launches; called once the app's windows exist.
pub fn ready(app: &tauri::AppHandle) {
    let _ = APP.set(app.clone());
    READY.store(true, Ordering::Release);
}

/// The receiver runs on its own thread with its own message loop, so it answers (with `STARTING`)
/// even while the main thread is busy creating windows.
fn start_receiver() {
    let spawned = std::thread::Builder::new()
        .name("snippy-instance-receiver".to_owned())
        .spawn(|| {
            let class = wide(RECEIVER_CLASS);
            let title = wide(RECEIVER_TITLE);
            // SAFETY: standard window-class registration and message loop on this thread.
            unsafe {
                let instance = GetModuleHandleW(None).unwrap_or_default();
                let window_class = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    lpfnWndProc: Some(receive),
                    hInstance: instance.into(),
                    lpszClassName: PCWSTR(class.as_ptr()),
                    ..Default::default()
                };
                RegisterClassExW(&window_class);
                let Ok(window) = CreateWindowExW(
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
                ) else {
                    crate::trace::step("single instance: could not create the receiver window");
                    return;
                };
                // An elevated Snippy must still hear launches from normal-integrity processes
                // (Explorer, Win+Shift+S); only this one message is admitted, to this window.
                let _ = ChangeWindowMessageFilterEx(window, WM_COPYDATA, MSGFLT_ALLOW, None);
                let mut message = MSG::default();
                while GetMessageW(&mut message, None, 0, 0).as_bool() {
                    let _ = TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        });
    if let Err(error) = spawned {
        crate::trace::step(&format!("single instance: no receiver thread ({error})"));
    }
}

unsafe extern "system" fn receive(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message != WM_COPYDATA {
        return DefWindowProcW(window, message, wparam, lparam);
    }
    let data = &*(lparam.0 as *const COPYDATASTRUCT);
    if data.dwData != PAYLOAD_TAG {
        return LRESULT(0);
    }
    let (Some(app), true) = (APP.get(), READY.load(Ordering::Acquire)) else {
        return LRESULT(STARTING);
    };
    // Copy the payload now: it is only valid during this call.
    let mut args = vec![String::from("snippy.exe")];
    if data.cbData > 0 && !data.lpData.is_null() {
        let units = std::slice::from_raw_parts(data.lpData as *const u16, data.cbData as usize / 2);
        args.extend(
            String::from_utf16_lossy(units)
                .split('\0')
                .filter(|arg| !arg.is_empty())
                .map(str::to_owned),
        );
    }
    let target = app.clone();
    if app
        .run_on_main_thread(move || crate::on_second_launch(&target, &args))
        .is_err()
    {
        return LRESULT(0);
    }
    LRESULT(ACCEPTED)
}

/// The other copy is an older version, or never acknowledged: identify it, ask, and close it only
/// when it is verifiably Snippy.
fn recover_from_other_copy(args: &[String], legacy: bool) -> Outcome {
    let receiver = if legacy {
        find(LEGACY_CLASS, LEGACY_TITLE)
    } else {
        find(RECEIVER_CLASS, RECEIVER_TITLE)
    };
    let pid = receiver
        .map(window_pid)
        .filter(|pid| *pid != 0)
        .or_else(|| {
            others()
                .ok()
                .and_then(|others| others.into_iter().next().map(|other| other.pid))
        });
    let Some(pid) = pid else {
        crate::trace::step("other copy: none found any more; starting");
        return retry_claim(args);
    };
    let Some(process) = Verified::open(pid) else {
        crate::trace::step(&format!(
            "other copy pid {pid}: not Snippy or cannot be opened"
        ));
        crate::elevation::show_error(
            "Snippy",
            &format!(
                "Another Snippy (process {pid}) is running and cannot be closed from here. If it \
                 runs as administrator, end \"snippy.exe\" in Task Manager, then start Snippy again."
            ),
        );
        return Outcome::Exit;
    };
    let reason = if legacy {
        "An older version of Snippy is running, and it cannot hand over to this version"
    } else {
        "Another Snippy is not responding: it did not finish starting within 30 seconds"
    };
    crate::trace::step(&format!(
        "other copy: pid {pid} {} ({reason})",
        process.path
    ));
    let question = format!(
        "{reason}.\n\n{}\n(process {pid})\n\nClose it and start Snippy now?",
        process.path
    );
    // An older version is replaced by default; a copy of this version only on an explicit Yes.
    let yes = if legacy {
        crate::elevation::ask_yes_no("Snippy", &question)
    } else {
        crate::elevation::ask_yes_no_default_no("Snippy", &question)
    };
    if !yes {
        crate::trace::step("user kept the other copy; exiting");
        return Outcome::Exit;
    }
    // It may have recovered while the question was open.
    if !legacy {
        if let Some(window) = find(RECEIVER_CLASS, RECEIVER_TITLE) {
            if hand_over(window, args) == Some(ACCEPTED) {
                crate::trace::step("the other copy recovered and took the launch; exiting");
                return Outcome::Exit;
            }
        }
    }
    match process.terminate() {
        Ok(()) => {
            crate::trace::step(&format!("closed pid {pid}"));
            retry_claim(args)
        }
        Err(error) => {
            crate::trace::step(&format!("could not close pid {pid}: {error}"));
            crate::elevation::show_error(
                "Snippy",
                &format!("Snippy could not close process {pid}: {error}"),
            );
            Outcome::Exit
        }
    }
}

/// After the other copy is gone the session should be free; anything else is reported, not looped.
fn retry_claim(args: &[String]) -> Outcome {
    for _ in 0..10 {
        if take_mutex() {
            return become_primary();
        }
        if let Some(window) = find(RECEIVER_CLASS, RECEIVER_TITLE) {
            if hand_over(window, args) == Some(ACCEPTED) {
                return Outcome::Exit;
            }
        }
        std::thread::sleep(RETRY);
    }
    crate::trace::step("single instance: the session is still taken; exiting");
    Outcome::Exit
}

fn window_pid(window: HWND) -> u32 {
    let mut pid = 0_u32;
    // SAFETY: `window` came from FindWindowW.
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    pid
}

/// A process handle opened before asking, so the process that is closed is the one that was shown
/// (a PID can be reused, a handle cannot), and only when its exe has this exe's file name.
struct Verified {
    handle: HANDLE,
    path: String,
}

impl Verified {
    fn open(pid: u32) -> Option<Self> {
        // SAFETY: the handle is owned by the returned value and closed on drop.
        let handle = unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION,
                false,
                pid,
            )
        }
        .ok()?;
        let verified = Self {
            path: image_path_of(handle).unwrap_or_default(),
            handle,
        };
        let name = |path: &std::path::Path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
        };
        let own = std::env::current_exe().ok().and_then(|path| name(&path));
        let theirs = name(std::path::Path::new(&verified.path));
        (theirs.is_some() && theirs == own).then_some(verified)
    }

    /// Ends the process and waits until it is really gone.
    fn terminate(&self) -> Result<(), String> {
        // SAFETY: `handle` has PROCESS_TERMINATE | PROCESS_SYNCHRONIZE access.
        unsafe {
            TerminateProcess(self.handle, 1).map_err(|error| error.message().to_string())?;
            if WaitForSingleObject(self.handle, 10_000) == WAIT_OBJECT_0 {
                Ok(())
            } else {
                Err("it did not exit within 10 seconds".to_owned())
            }
        }
    }
}

impl Drop for Verified {
    fn drop(&mut self) {
        // SAFETY: the handle was opened by `open` and is closed once.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

fn image_path_of(process: HANDLE) -> Option<String> {
    let mut buffer = [0_u16; 1024];
    let mut length = buffer.len() as u32;
    // SAFETY: the buffer and its length match.
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

pub struct OtherProcess {
    pub pid: u32,
    pub path: Option<String>,
    pub age: Option<Duration>,
}

/// Running processes with the same file name as this exe, other than this one.
pub fn others() -> Result<Vec<OtherProcess>, String> {
    let own_name = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
        })
        .unwrap_or_else(|| "snippy.exe".to_owned());
    let own_pid = std::process::id();
    // SAFETY: the snapshot handle is closed below; the entry is sized as the API requires.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
            .map_err(|error| error.message().to_string())?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Vec::new();
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let length = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
            let name = String::from_utf16_lossy(&entry.szExeFile[..length]).to_lowercase();
            if name == own_name && entry.th32ProcessID != own_pid {
                let pid = entry.th32ProcessID;
                let (path, age) = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                    Ok(process) => {
                        let details = (image_path_of(process), age_of(process));
                        let _ = CloseHandle(process);
                        details
                    }
                    Err(_) => (None, None),
                };
                found.push(OtherProcess { pid, path, age });
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        Ok(found)
    }
}

fn age_of(process: HANDLE) -> Option<Duration> {
    let (mut created, mut exited, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    // SAFETY: plain query on a handle with limited query access.
    unsafe { GetProcessTimes(process, &mut created, &mut exited, &mut kernel, &mut user) }.ok()?;
    let created = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    // FILETIME counts 100 ns from 1601.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos() as u64
        / 100
        + 116_444_736_000_000_000;
    Some(Duration::from_nanos(now.saturating_sub(created) * 100))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_not_listed_among_the_others() {
        assert!(others()
            .unwrap()
            .iter()
            .all(|other| other.pid != std::process::id()));
    }

    #[test]
    fn only_a_process_with_this_exes_file_name_is_verified() {
        assert!(Verified::open(std::process::id()).is_some());
        // A process with another file name (the session's Explorer) is never accepted.
        if let Some(explorer) = process_named("explorer.exe") {
            assert!(Verified::open(explorer).is_none());
        }
    }

    fn process_named(name: &str) -> Option<u32> {
        // SAFETY: as in `others`.
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
            let mut found = None;
            while more {
                let length = entry.szExeFile.iter().position(|&c| c == 0).unwrap_or(0);
                if String::from_utf16_lossy(&entry.szExeFile[..length]).eq_ignore_ascii_case(name) {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                more = Process32NextW(snapshot, &mut entry).is_ok();
            }
            let _ = CloseHandle(snapshot);
            found
        }
    }
}
