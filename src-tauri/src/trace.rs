//! The startup trace: every launch writes what it is doing to `startup.log`, from the first line of
//! `run` until the main window is on screen, so a start that shows nothing still leaves a record.
//!
//! - `%APPDATA%\Snippy\logs\startup.log`, or `%TEMP%\snippy-startup.log` when that folder cannot be
//!   written. The file keeps the last few launches (it is trimmed when it grows past 256 KB).
//! - `snippy.exe --debug` also opens a console window that shows the same lines live.
//!
//! Lines are written and flushed one by one, so the file is complete up to the step that hung.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

pub const DEBUG_FLAG: &str = "--debug";
const MAX_BYTES: u64 = 256 * 1024;

struct Trace {
    file: Option<File>,
    path: Option<PathBuf>,
    /// `--debug`: the console device itself. A GUI exe's standard output is not connected even
    /// after it attaches to a console, so the trace writes to `CONOUT$` directly.
    console: Option<File>,
    started: Instant,
}

static TRACE: Mutex<Option<Trace>> = Mutex::new(None);

/// Opens the trace. Call before anything else in `run`.
pub fn init(args: &[String]) {
    let console = args
        .iter()
        .any(|arg| arg == DEBUG_FLAG)
        .then(attach_console)
        .flatten();
    let (file, path) = open_file();
    *TRACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Trace {
        file,
        path,
        console,
        started: Instant::now(),
    });
    step(&format!(
        "===== Snippy {} start, pid {}, {}",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
}

/// Where the trace is being written, for messages that point the user at it.
pub fn path() -> Option<PathBuf> {
    TRACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .and_then(|trace| trace.path.clone())
}

/// Records one startup step.
pub fn step(text: &str) {
    let mut guard = TRACE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(trace) = guard.as_mut() else {
        return;
    };
    // The pid tells apart launches that write at the same time (a second launch handing over).
    let line = format!(
        "[{:>6}] [+{:>6} ms] {text}",
        std::process::id(),
        trace.started.elapsed().as_millis()
    );
    if let Some(file) = trace.file.as_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
    if let Some(console) = trace.console.as_mut() {
        let _ = writeln!(console, "{line}");
    }
}

fn open_file() -> (Option<File>, Option<PathBuf>) {
    let preferred = crate::config::app_data_dir()
        .ok()
        .map(|dir| dir.join("logs").join("startup.log"));
    let fallback = std::env::temp_dir().join("snippy-startup.log");
    for path in preferred.into_iter().chain(std::iter::once(fallback)) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        trim(&path);
        if let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) {
            return (Some(file), Some(path));
        }
    }
    (None, None)
}

/// Keeps the newest launches of an oversized trace: everything from the first launch header in its
/// second half. Works on bytes, so a cut never lands inside a character.
fn trim(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() <= MAX_BYTES {
        return;
    }
    let Ok(bytes) = std::fs::read(path) else {
        return;
    };
    let half = bytes.len() / 2;
    let marker = b"===== Snippy";
    let start = bytes[half..]
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|offset| {
            // Back up to the start of that line.
            let at = half + offset;
            bytes[..at]
                .iter()
                .rposition(|&b| b == b'\n')
                .map_or(0, |newline| newline + 1)
        });
    match start {
        Some(start) => {
            let _ = std::fs::write(path, &bytes[start..]);
        }
        None => {
            let _ = std::fs::write(path, b"");
        }
    }
}

/// `--debug`: show the trace in a console. A GUI-subsystem exe has none, so attach to the one it
/// was started from, or open a new window.
fn attach_console() -> Option<File> {
    use windows::Win32::System::Console::{AllocConsole, AttachConsole, ATTACH_PARENT_PROCESS};
    // SAFETY: plain console API calls on our own process.
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            AllocConsole().ok()?;
        }
    }
    let mut console = OpenOptions::new().write(true).open("CONOUT$").ok()?;
    // Starts on a fresh line after the shell prompt the launch returned to.
    let _ = writeln!(console);
    Some(console)
}

/// Facts that explain most failed starts: the exe, Windows, WebView2 and other Snippy processes.
pub fn environment_report() {
    if let Some(path) = path() {
        step(&format!("trace file: {}", path.display()));
    }
    let exe = std::env::current_exe()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|error| format!("unknown ({error})"));
    step(&format!("exe: {exe}"));
    step(&format!(
        "args: {:?}",
        std::env::args().skip(1).collect::<Vec<_>>()
    ));
    step(&crate::crash::environment());
    match crate::instances::others() {
        Ok(others) if others.is_empty() => step("other Snippy processes: none"),
        Ok(others) => {
            for other in others {
                step(&format!(
                    "other Snippy process: pid {} {} (running {})",
                    other.pid,
                    other.path.as_deref().unwrap_or("(path not readable)"),
                    other
                        .age
                        .map_or("for an unknown time".to_owned(), |age| format!(
                            "{} s",
                            age.as_secs()
                        ))
                ));
            }
        }
        Err(error) => step(&format!("could not list other processes: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trimming_keeps_whole_launches_and_never_splits_a_character() {
        let path =
            std::env::temp_dir().join(format!("snippy-trace-test-{}.log", std::process::id()));
        let mut text = String::new();
        let mut launch = 0;
        while text.len() as u64 <= MAX_BYTES + 1000 {
            launch += 1;
            text.push_str(&format!("[  1] [+     0 ms] ===== Snippy start {launch}\n"));
            for _ in 0..40 {
                text.push_str("[  1] [+     1 ms] шаг запуска, путь C:/Пользователи/снимок\n");
            }
        }
        std::fs::write(&path, &text).unwrap();
        trim(&path);
        let kept = std::fs::read(&path).unwrap();
        let kept = String::from_utf8(kept).expect("the trimmed trace is valid UTF-8");
        assert!(kept.len() < text.len());
        assert!(kept.starts_with("[  1] [+     0 ms] ===== Snippy start"));
        assert!(kept.ends_with("снимок\n"));
        std::fs::remove_file(path).unwrap();
    }
}
