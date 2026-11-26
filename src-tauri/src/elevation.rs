use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    ShellExecuteExW, ShellExecuteW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_YESNO, SW_HIDE,
    SW_SHOWNORMAL,
};

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

pub fn is_elevated() -> Result<bool, String> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }
        .map_err(|error| format!("Failed to open the process token: {error}"))?;
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0_u32;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .map_err(|error| format!("Failed to query token elevation: {error}"))?;
    Ok(elevation.TokenIsElevated != 0)
}

/// Hands this start over to an elevated copy when "Run as administrator" is on and this process
/// is not elevated. Returns true when this process should exit.
pub fn hand_over_if_configured(config: &crate::config::Config) -> bool {
    if !config.behavior.run_as_admin {
        return false;
    }
    match is_elevated() {
        Ok(false) => hand_over(),
        Ok(true) => false,
        Err(error) => {
            log::warn!("Could not determine elevation status: {error}");
            false
        }
    }
}

/// Starts an elevated copy: through the logon task when it starts this exe (no prompt), else
/// through a UAC prompt. False when neither started one, so this copy carries on unelevated.
fn hand_over() -> bool {
    match crate::autostart::run_elevated_task_for_this_exe() {
        Ok(true) => return true,
        Ok(false) => crate::trace::step("no logon task for this exe; asking Windows to elevate"),
        Err(error) => crate::trace::step(&format!(
            "logon task unusable ({error}); asking Windows to elevate"
        )),
    }
    match relaunch_elevated() {
        Ok(true) => true,
        Ok(false) => {
            log::warn!("Elevation was declined; continuing without administrator rights");
            false
        }
        Err(error) => {
            log::warn!("Could not relaunch elevated: {error}");
            false
        }
    }
}

pub fn relaunch_elevated() -> Result<bool, String> {
    let executable = std::env::current_exe()
        .map_err(|error| format!("Failed to determine the executable path: {error}"))?;
    let executable: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let verb: Vec<u16> = "runas\0".encode_utf16().collect();
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(verb.as_ptr()),
            PCWSTR(executable.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    Ok(result.0 as isize > 32)
}

/// Run a program elevated and wait for it to finish. Used for the one-off privileged operations
/// (registering or deleting the logon task) that a medium-integrity process cannot perform.
pub fn run_elevated_and_wait(program: &str, arguments: &[&str]) -> Result<(), String> {
    match run_elevated_for_code(program, arguments)? {
        0 => Ok(()),
        code => Err(format!("Elevated helper failed with exit code {code}")),
    }
}

/// Runs a program elevated, waits for it and returns its exit code. Declining the UAC prompt is
/// reported as such.
pub fn run_elevated_for_code(program: &str, arguments: &[&str]) -> Result<i32, String> {
    let program: Vec<u16> = wide(program);
    let parameters: Vec<u16> = wide(&quote_arguments(arguments));
    let verb: Vec<u16> = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(program.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info) }.map_err(|error| {
        // ERROR_CANCELLED: the user said no to the UAC prompt.
        if error.code() == windows::Win32::Foundation::ERROR_CANCELLED.to_hresult() {
            "Administrator rights were not given".to_owned()
        } else {
            format!(
                "Windows refused to elevate {}: {error}",
                info_file(&program)
            )
        }
    })?;
    if info.hProcess.is_invalid() {
        return Err("Elevated helper did not start".to_owned());
    }
    let process = OwnedHandle(info.hProcess);
    unsafe { WaitForSingleObject(process.0, 60_000) };
    let mut code = 0_u32;
    unsafe { GetExitCodeProcess(process.0, &mut code) }
        .map_err(|error| format!("Could not read the elevated exit code: {error}"))?;
    // STILL_ACTIVE: it did not finish within the wait.
    if code == 259 {
        return Err("The elevated helper did not finish in time".to_owned());
    }
    Ok(code as i32)
}

/// A native Yes/No question, for problems that occur before any WebView exists.
pub fn ask_yes_no(title: &str, text: &str) -> bool {
    let (title, text) = (wide(title), wide(text));
    // SAFETY: both strings are NUL-terminated and outlive the modal call.
    let answer = unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING,
        )
    };
    answer == IDYES
}

/// Like `ask_yes_no`, but No is the default button, for questions whose Yes is destructive.
pub fn ask_yes_no_default_no(title: &str, text: &str) -> bool {
    let (title, text) = (wide(title), wide(text));
    // SAFETY: both strings are NUL-terminated and outlive the modal call.
    let answer = unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2,
        )
    };
    answer == IDYES
}

/// A native error message, for problems that occur before any WebView exists.
pub fn show_error(title: &str, text: &str) {
    let (title, text) = (wide(title), wide(text));
    // SAFETY: both strings are NUL-terminated and outlive the modal call.
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

fn info_file(program: &[u16]) -> String {
    String::from_utf16_lossy(&program[..program.len().saturating_sub(1)])
}

fn quote_arguments(arguments: &[&str]) -> String {
    arguments
        .iter()
        .map(|argument| format!("\"{}\"", argument.replace('"', "\\\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
