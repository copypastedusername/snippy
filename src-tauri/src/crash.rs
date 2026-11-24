//! Makes startup failures visible. A release build has no console, so a panic or a failed window
//! would otherwise end the process without a word. Each failure is written to
//! `%APPDATA%\Snippy\logs\crash.log` (readable before the log plugin exists) and shown in a
//! message box.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long creating the windows may take before a stalled start is reported.
const STARTUP_LIMIT: Duration = Duration::from_secs(30);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Reports a start that never finishes (for example WebView2 stuck creating a window) instead of
/// leaving a process with no window behind. Call once the app is about to build its windows.
pub fn watch_startup() {
    std::thread::spawn(|| {
        std::thread::sleep(STARTUP_LIMIT);
        if !STARTED.load(Ordering::Acquire) {
            report("Snippy could not open its window: Microsoft Edge WebView2 did not respond. Updating or reinstalling the WebView2 Runtime usually fixes this.");
            std::process::exit(1);
        }
    });
}

/// Marks the windows as created; the startup watchdog stands down.
pub fn started() {
    STARTED.store(true, Ordering::Release);
}

/// Installs the panic hook. Call first thing in `run`.
pub fn install() {
    std::panic::set_hook(Box::new(|info| {
        let message = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_owned())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "unknown error".to_owned());
        let place = info
            .location()
            .map(|location| format!(" ({}:{})", location.file(), location.line()))
            .unwrap_or_default();
        report(&format!("Snippy stopped unexpectedly: {message}{place}"));
    }));
}

/// Records a fatal error and tells the user where to find the details.
pub fn report(problem: &str) {
    static REPORTING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if REPORTING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    crate::trace::step(&format!("FAILED: {problem}"));
    let text = format!("{problem}\n\n{}", environment());
    let saved = append(&text);
    log::error!("{text}");
    let hint = match (saved, crate::trace::path()) {
        (Some(path), Some(trace)) => format!(
            "\n\nDetails were saved to {}\nStartup steps: {}",
            path.display(),
            trace.display()
        ),
        (Some(path), None) => format!("\n\nDetails were saved to {}", path.display()),
        (None, _) => String::new(),
    };
    crate::elevation::show_error("Snippy", &format!("{problem}{hint}"));
}

/// Windows build and WebView2 version: the two facts every startup report needs.
pub fn environment() -> String {
    let build = crate::winreg::read_machine_string(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "CurrentBuildNumber",
    )
    .unwrap_or_else(|| "unknown".to_owned());
    let edition = crate::winreg::read_machine_string(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "ProductName",
    )
    .unwrap_or_else(|| "Windows".to_owned());
    // Windows 11 still says "Windows 10" in ProductName; the build number tells them apart.
    let edition = match build.parse::<u32>() {
        Ok(number) if number >= 22000 => edition.replace("Windows 10", "Windows 11"),
        _ => edition,
    };
    let webview = tauri::webview_version().unwrap_or_else(|error| format!("unavailable ({error})"));
    format!(
        "Snippy {}, {edition} build {build}, WebView2 {webview}",
        env!("CARGO_PKG_VERSION")
    )
}

fn append(text: &str) -> Option<std::path::PathBuf> {
    let path = crate::config::app_data_dir()
        .ok()?
        .join("logs")
        .join("crash.log");
    std::fs::create_dir_all(path.parent()?).ok()?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    writeln!(
        file,
        "[{}] {text}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    )
    .ok()?;
    Some(path)
}
