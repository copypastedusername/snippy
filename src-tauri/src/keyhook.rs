//! Win+Shift+S on Windows 10.
//!
//! Windows 11 opens an `ms-screenclip:` link for Win+Shift+S, so the user can make Snippy its
//! default app (see `screenclip.rs`). Windows 10 does not: Explorer registers the chord as its own
//! hotkey when it starts and opens its screen clipping directly, whatever app the links open.
//! Snippy takes it there in one of two ways, both owned by one thread with a message loop:
//!
//! - An ordinary hotkey for the chord, tried first. Windows grants it whenever Explorer does not
//!   hold the chord, for example after the user turned Explorer's own Win+S hotkeys off
//!   (`DisabledHotkeys`, which also turns off Win+S Search) and signed in again. Then no hook runs.
//! - Otherwise a low-level keyboard hook, the way PowerToys remaps shortcuts: it sees Win+Shift+S
//!   before Explorer's hotkey does, swallows the S and starts a snip. Win+S and every other key
//!   pass on. Windows removes a low-level hook without notice when it once answers too late, so
//!   the hook is put back every 2 s (the new one in before the old one comes out) and the hotkey
//!   is tried again.
//!
//! The hook thread only pumps messages; starting the snip and writing the log happen on a separate
//! thread it hands notes to without waiting. A low-level hook is also not called while one of
//! Snippy's own windows has the keyboard unless the app leaves Raw Input alone (`lib.rs` sets
//! `DeviceEventFilter::Always`), and it does not see keys meant for elevated windows when Snippy
//! runs without elevation.

use crate::winreg;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::{
    GetCurrentThread, GetCurrentThreadId, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, RegisterHotKey, SendInput, UnregisterHotKey, INPUT, INPUT_0, INPUT_KEYBOARD,
    KEYBDINPUT, KEYEVENTF_KEYUP, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, VIRTUAL_KEY, VK_CONTROL,
    VK_LSHIFT, VK_LWIN, VK_MENU, VK_RSHIFT, VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, KillTimer, PeekMessageW, PostThreadMessageW, SetTimer,
    SetWindowsHookExW, UnhookWindowsHookEx, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE,
    WH_KEYBOARD_LL, WM_HOTKEY, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_TIMER,
};

const VK_S: u32 = b'S' as u32;
/// An unassigned key, pressed while Win is down so that releasing Win does not open Start
/// (Explorer opens it when Win goes up with no other key in between). PowerToys uses the same key.
const MASK_KEY: VIRTUAL_KEY = VIRTUAL_KEY(0xFF);
/// Marks Snippy's own key presses so the hook lets them through untouched.
const SNIPPY_INPUT: usize = 0x534E_4950;
/// How often the hook is put back and the hotkey tried again.
const REFRESH_MS: u32 = 2000;
/// A refresh this much later than due means the hook thread did not run; it is logged.
const LATE_REFRESH_MS: u32 = 1000;
const HOTKEY_ID: i32 = 0x5353;

const EXPLORER_ADVANCED_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Explorer\Advanced";
/// Letters whose Win hotkeys Explorer leaves alone, read when Explorer starts.
const DISABLED_HOTKEYS_VALUE: &str = "DisabledHotkeys";

/// How the hook decided the S key's current press; its repeats and release follow that decision.
const S_UP: u8 = 0;
const S_PASSED: u8 = 1;
const S_SWALLOWED: u8 = 2;

struct Worker {
    thread_id: u32,
    handle: JoinHandle<()>,
}

static APP: OnceLock<tauri::AppHandle> = OnceLock::new();
static WORKER: Mutex<Option<Worker>> = Mutex::new(None);
static S_STATE: AtomicU8 = AtomicU8::new(S_UP);

/// What the hook thread hands to the dispatch thread.
enum Note {
    /// The hook took Win+Shift+S: start a snip.
    Snip,
    /// The chord arrived as Snippy's own hotkey: start a snip.
    Hotkey,
    /// The hook thread did not run for this many ms.
    LateRefresh(u32),
    Text(String),
}

/// The dispatch thread's queue. Unbounded, so a snip request is never dropped and the hook never
/// waits; it only fills while the dispatcher is busy, which it rarely is.
static NOTES: OnceLock<Sender<Note>> = OnceLock::new();

/// Hands a note to the dispatch thread without waiting.
fn note(note: Note) {
    if let Some(notes) = NOTES.get() {
        let _ = notes.send(note);
    }
}

/// Starts the dispatch thread once. Without it a swallowed S would start nothing, so the hook is
/// not installed when this fails.
fn start_dispatcher() -> Result<(), String> {
    if NOTES.get().is_some() {
        return Ok(());
    }
    let (sender, receiver) = channel();
    std::thread::Builder::new()
        .name("win-shift-s-notes".to_owned())
        .spawn(move || {
            for note in receiver {
                dispatch(note);
            }
        })
        .map_err(|error| format!("Could not start taking Win+Shift+S: {error}"))?;
    let _ = NOTES.set(sender);
    Ok(())
}

fn dispatch(note: Note) {
    match note {
        Note::Snip => start_snip(),
        Note::Hotkey => {
            log::info!("Win+Shift+S arrived as a hotkey");
            start_snip();
        }
        Note::LateRefresh(late_ms) => {
            log::warn!("The Win+Shift+S hook thread did not run for {late_ms} ms")
        }
        Note::Text(text) => log::info!("{text}"),
    }
}

/// Whether Windows takes Win+Shift+S itself on this build, so Snippy must take it from Windows.
pub fn needed(build: Option<u32>) -> bool {
    crate::screenclip::before_windows_11(build)
}

/// Whether Snippy is taking Win+Shift+S (its thread is running).
pub fn is_active() -> bool {
    lock_worker()
        .as_ref()
        .is_some_and(|worker| !worker.handle.is_finished())
}

fn lock_worker() -> MutexGuard<'static, Option<Worker>> {
    WORKER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Starts or stops taking Win+Shift+S. Starting twice is harmless and a thread that ended is
/// replaced. Stopping waits until the hook is gone, so a restart never runs two hooks at once.
pub fn set_active(app: &tauri::AppHandle, active: bool) -> Result<(), String> {
    let mut worker = lock_worker();
    if worker
        .as_ref()
        .is_some_and(|worker| worker.handle.is_finished())
    {
        if let Some(ended) = worker.take() {
            let _ = ended.handle.join();
        }
        log::warn!("The Win+Shift+S thread had stopped");
    }
    match (active, worker.take()) {
        (true, None) => {
            let _ = APP.set(app.clone());
            *worker = Some(start()?);
            log::info!("Snippy takes Win+Shift+S from Windows");
        }
        (true, Some(running)) => *worker = Some(running),
        (false, Some(stopping)) => {
            if let Err((running, error)) = stop(stopping) {
                // Still running: keep it, so no second hook can start beside it.
                *worker = Some(running);
                return Err(error);
            }
        }
        (false, None) => {}
    }
    Ok(())
}

/// Ends the worker and waits for it. Gives the worker back if it could not be told to end.
fn stop(worker: Worker) -> Result<(), (Worker, String)> {
    // SAFETY: posting a message to a thread id has no memory-safety requirements.
    let posted = unsafe { PostThreadMessageW(worker.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
    match posted {
        Ok(()) => {
            let _ = worker.handle.join();
            Ok(())
        }
        Err(error) => Err((
            worker,
            format!("Could not stop taking Win+Shift+S: {}", error.message()),
        )),
    }
}

/// Starts the worker thread and waits until it holds the chord (hotkey or hook).
fn start() -> Result<Worker, String> {
    start_dispatcher()?;
    let (sender, receiver) = channel();
    let handle = std::thread::Builder::new()
        .name("win-shift-s".to_owned())
        .spawn(move || run(&sender))
        .map_err(|error| format!("Could not start taking Win+Shift+S: {error}"))?;
    let thread_id = receiver
        .recv()
        .map_err(|_| "The Win+Shift+S thread stopped".to_owned())??;
    Ok(Worker { thread_id, handle })
}

fn run(started: &Sender<Result<u32, String>>) {
    let mut message = MSG::default();
    // SAFETY: these calls have no preconditions. Peeking creates the thread's message queue, so
    // messages posted once `started` is answered are kept. Windows removes a hook that answers
    // late, so this mostly idle thread runs ahead of ordinary work.
    let thread = unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        GetCurrentThreadId()
    };
    S_STATE.store(S_UP, Ordering::Release);
    let mut hotkey = claim_hotkey();
    let mut hook = None;
    if !hotkey {
        match install() {
            Ok(installed) => hook = Some(installed),
            Err(error) => {
                let _ = started.send(Err(error));
                return;
            }
        }
    }
    note(Note::Text(
        if hotkey {
            "Win+Shift+S: Windows left it free; Snippy holds it as a hotkey"
        } else {
            "Win+Shift+S: Windows holds it; Snippy hooks the keyboard"
        }
        .to_owned(),
    ));
    let _ = started.send(Ok(thread));
    // SAFETY: a thread timer without a window posts WM_TIMER to this thread's queue.
    let timer = unsafe { SetTimer(None, 0, REFRESH_MS, None) };
    if timer == 0 {
        note(Note::Text(
            "No timer for the Win+Shift+S hook; a hook Windows drops is not put back".to_owned(),
        ));
    }
    let mut last_refresh = now_ms();

    loop {
        // SAFETY: `message` is a valid MSG; GetMessageW returns 0 on WM_QUIT and -1 on failure.
        let result = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if result == -1 {
            note(Note::Text(format!(
                "The Win+Shift+S thread stopped: {}",
                windows::core::Error::from_win32().message()
            )));
            break;
        }
        if result == 0 {
            break;
        }
        match message.message {
            WM_HOTKEY => note(Note::Hotkey),
            WM_TIMER => {
                let now = now_ms();
                let late_ms = now.wrapping_sub(last_refresh).saturating_sub(REFRESH_MS);
                last_refresh = now;
                if late_ms >= LATE_REFRESH_MS {
                    note(Note::LateRefresh(late_ms));
                }
                if !hotkey && claim_hotkey() {
                    hotkey = true;
                    note(Note::Text(
                        "Win+Shift+S is free now; Snippy holds it as a hotkey".to_owned(),
                    ));
                }
                if hotkey {
                    if let Some(unneeded) = hook.take() {
                        remove(unneeded);
                    }
                } else {
                    refresh(&mut hook);
                }
            }
            _ => {}
        }
    }
    // SAFETY: the timer and the hotkey belong to this thread and are released once.
    unsafe {
        let _ = KillTimer(None, timer);
        if hotkey {
            let _ = UnregisterHotKey(None, HOTKEY_ID);
        }
    }
    if let Some(hook) = hook {
        remove(hook);
    }
    note(Note::Text("Snippy no longer takes Win+Shift+S".to_owned()));
}

/// Milliseconds on the system tick clock.
fn now_ms() -> u32 {
    // SAFETY: GetTickCount has no preconditions.
    unsafe { GetTickCount() }
}

fn install() -> Result<HHOOK, String> {
    // SAFETY: the procedure lives for the whole program and the module is this executable.
    unsafe {
        GetModuleHandleW(None)
            .and_then(|module| SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), module, 0))
    }
    .map_err(|error| format!("Could not take Win+Shift+S: {}", error.message()))
}

/// Puts a fresh hook in before removing the old one, so no press falls between the two.
fn refresh(hook: &mut Option<HHOOK>) {
    match install() {
        Ok(fresh) => {
            if let Some(old) = hook.replace(fresh) {
                remove(old);
            }
        }
        Err(error) => note(Note::Text(error)),
    }
}

fn remove(hook: HHOOK) {
    // SAFETY: the hook was installed by this thread and is removed once.
    if let Err(error) = unsafe { UnhookWindowsHookEx(hook) } {
        note(Note::Text(format!(
            "Windows had already dropped the Win+Shift+S hook: {}",
            error.message()
        )));
    }
}

/// Registers Win+Shift+S as an ordinary hotkey of this thread. Windows grants it only while
/// Explorer does not hold the chord.
fn claim_hotkey() -> bool {
    // SAFETY: a hotkey without a window posts WM_HOTKEY to this thread's queue.
    unsafe { RegisterHotKey(None, HOTKEY_ID, MOD_WIN | MOD_SHIFT | MOD_NOREPEAT, VK_S) }.is_ok()
}

fn start_snip() {
    let Some(app) = APP.get() else { return };
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || crate::start_default_snip(&handle)) {
        log::error!("Could not start the Win+Shift+S snip: {error}");
    }
}

/// Whether Explorer's own Win+S hotkeys (Search and Win+Shift+S) are turned off.
pub fn windows_hotkey_off() -> bool {
    winreg::read_user_string(EXPLORER_ADVANCED_KEY, DISABLED_HOTKEYS_VALUE)
        .is_some_and(|letters| letters.chars().any(|c| c.eq_ignore_ascii_case(&'S')))
}

/// Turns Explorer's own Win+S hotkeys off or back on. Explorer reads this when it starts, so it
/// takes effect after the user signs in again. Other letters in the value are kept. Only Windows
/// 10 needs it; elsewhere the request is refused.
pub fn set_windows_hotkey_off(off: bool) -> Result<(), String> {
    static WRITING: Mutex<()> = Mutex::new(());
    if !needed(crate::screenclip::windows_build()) {
        return Err("Only Windows 10 needs Windows' own Win+Shift+S turned off".to_owned());
    }
    let _writing = WRITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let current =
        winreg::read_user_string(EXPLORER_ADVANCED_KEY, DISABLED_HOTKEYS_VALUE).unwrap_or_default();
    let letters = with_letter(&current, 'S', off);
    // Turning it back on always writes, so a value that could not be read is still cleared.
    if off && letters == current {
        return Ok(());
    }
    if letters.is_empty() {
        winreg::delete_user_value(EXPLORER_ADVANCED_KEY, DISABLED_HOTKEYS_VALUE)
    } else {
        winreg::set_user_string(
            EXPLORER_ADVANCED_KEY,
            Some(DISABLED_HOTKEYS_VALUE),
            &letters,
        )
    }
}

fn with_letter(letters: &str, letter: char, present: bool) -> String {
    let mut kept: String = letters
        .chars()
        .filter(|c| !c.eq_ignore_ascii_case(&letter))
        .collect();
    if present {
        kept.push(letter);
    }
    kept
}

/// The modifiers held when a key arrives.
#[derive(Clone, Copy, Default)]
struct Modifiers {
    win: bool,
    shift: bool,
    ctrl: bool,
    alt: bool,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Action {
    PassOn = 0,
    /// Swallow the key and start a snip.
    Snip = 1,
    /// Swallow the key (a repeat or the release of a swallowed press).
    Swallow = 2,
}

/// Decides what to do with one key event. `s_state` is how the S key's current press was decided;
/// its repeats and release follow that first decision, so a press is never half taken.
fn decide(vk: u32, down: bool, ours: bool, held: Modifiers, s_state: &AtomicU8) -> Action {
    if ours || vk != VK_S {
        return Action::PassOn;
    }
    if !down {
        return if s_state.swap(S_UP, Ordering::AcqRel) == S_SWALLOWED {
            Action::Swallow
        } else {
            Action::PassOn
        };
    }
    match s_state.load(Ordering::Acquire) {
        S_SWALLOWED => Action::Swallow,
        S_PASSED => Action::PassOn,
        _ if held.win && held.shift && !held.ctrl && !held.alt => {
            s_state.store(S_SWALLOWED, Ordering::Release);
            Action::Snip
        }
        _ => {
            s_state.store(S_PASSED, Ordering::Release);
            Action::PassOn
        }
    }
}

fn is_held(key: VIRTUAL_KEY) -> bool {
    // SAFETY: GetAsyncKeyState only reads the key state. The high bit is set while the key is down.
    unsafe { GetAsyncKeyState(i32::from(key.0)) < 0 }
}

/// The other keys' state when S goes down. Only the key being processed lags behind in
/// GetAsyncKeyState inside the hook; Win, Shift, Ctrl and Alt went down before it.
fn held_modifiers() -> Modifiers {
    Modifiers {
        win: is_held(VK_LWIN) || is_held(VK_RWIN),
        shift: is_held(VK_LSHIFT) || is_held(VK_RSHIFT),
        ctrl: is_held(VK_CONTROL),
        alt: is_held(VK_MENU),
    }
}

/// Presses and releases the mask key so that releasing Win afterwards does not open Start.
fn mask_start_menu() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: MASK_KEY,
                dwFlags: flags,
                dwExtraInfo: SNIPPY_INPUT,
                ..Default::default()
            },
        },
    };
    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    // SAFETY: the inputs are valid keyboard events and the size is that of INPUT.
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent as usize != inputs.len() {
        note(Note::Text(
            "Windows refused the key that keeps Start closed after Win+Shift+S".to_owned(),
        ));
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lparam points to the event's KBDLLHOOKSTRUCT.
        let event = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let message = wparam.0 as u32;
        let down = message == WM_KEYDOWN || message == WM_SYSKEYDOWN;
        let ours = event.dwExtraInfo == SNIPPY_INPUT;
        let held = if event.vkCode == VK_S && down && !ours {
            held_modifiers()
        } else {
            Modifiers::default()
        };
        let action = decide(event.vkCode, down, ours, held, &S_STATE);
        match action {
            Action::PassOn => {}
            Action::Swallow => return LRESULT(1),
            Action::Snip => {
                mask_start_menu();
                note(Note::Snip);
                return LRESULT(1);
            }
        }
    }
    // SAFETY: passes the event on unchanged, as every hook must for keys it does not take.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::{decide, needed, with_letter, Action, Modifiers, S_UP, VK_S};
    use std::sync::atomic::AtomicU8;

    const WIN_SHIFT: Modifiers = Modifiers {
        win: true,
        shift: true,
        ctrl: false,
        alt: false,
    };
    const NONE: Modifiers = Modifiers {
        win: false,
        shift: false,
        ctrl: false,
        alt: false,
    };

    #[test]
    fn win_shift_s_snips_once_and_its_repeats_and_release_are_swallowed() {
        let state = AtomicU8::new(S_UP);
        assert_eq!(decide(VK_S, true, false, WIN_SHIFT, &state), Action::Snip);
        assert_eq!(
            decide(VK_S, true, false, WIN_SHIFT, &state),
            Action::Swallow
        );
        // The release is swallowed even when Win and Shift went up first.
        assert_eq!(decide(VK_S, false, false, NONE, &state), Action::Swallow);
        assert_eq!(decide(VK_S, true, false, NONE, &state), Action::PassOn);
        assert_eq!(decide(VK_S, false, false, NONE, &state), Action::PassOn);
    }

    #[test]
    fn a_held_s_that_passed_is_not_taken_when_win_and_shift_come_later() {
        let state = AtomicU8::new(S_UP);
        assert_eq!(decide(VK_S, true, false, NONE, &state), Action::PassOn);
        assert_eq!(decide(VK_S, true, false, WIN_SHIFT, &state), Action::PassOn);
        assert_eq!(
            decide(VK_S, false, false, WIN_SHIFT, &state),
            Action::PassOn
        );
        assert_eq!(decide(VK_S, true, false, WIN_SHIFT, &state), Action::Snip);
    }

    #[test]
    fn other_chords_and_keys_pass_through() {
        let state = AtomicU8::new(S_UP);
        let win_s = Modifiers {
            shift: false,
            ..WIN_SHIFT
        };
        let ctrl_win_shift = Modifiers {
            ctrl: true,
            ..WIN_SHIFT
        };
        assert_eq!(decide(VK_S, true, false, win_s, &state), Action::PassOn);
        assert_eq!(decide(VK_S, false, false, win_s, &state), Action::PassOn);
        assert_eq!(
            decide(VK_S, true, false, ctrl_win_shift, &state),
            Action::PassOn
        );
        assert_eq!(decide(VK_S, false, false, NONE, &state), Action::PassOn);
        assert_eq!(
            decide(u32::from(b'A'), true, false, WIN_SHIFT, &state),
            Action::PassOn
        );
        assert_eq!(decide(VK_S, true, true, WIN_SHIFT, &state), Action::PassOn);
    }

    #[test]
    fn only_windows_10_needs_the_takeover() {
        assert!(needed(Some(19045)));
        assert!(!needed(Some(22000)));
        assert!(!needed(Some(26100)));
        assert!(!needed(None));
    }

    #[test]
    fn a_thread_timer_without_a_window_reaches_the_message_loop() {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetMessageW, KillTimer, PeekMessageW, SetTimer, MSG, PM_NOREMOVE, WM_TIMER,
        };
        let ticks = std::thread::spawn(|| {
            let mut message = MSG::default();
            // SAFETY: as in `run`: a queue, a windowless timer, and a bounded message loop.
            unsafe {
                let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
                let timer = SetTimer(None, 0, 50, None);
                let mut ticks = 0;
                while ticks < 3 && GetMessageW(&mut message, None, 0, 0).0 > 0 {
                    if message.message == WM_TIMER {
                        ticks += 1;
                    }
                }
                let _ = KillTimer(None, timer);
                ticks
            }
        })
        .join()
        .expect("timer thread");
        assert_eq!(ticks, 3);
    }

    #[test]
    fn explorer_hotkey_letters_keep_the_others() {
        assert_eq!(with_letter("", 'S', true), "S");
        assert_eq!(with_letter("QE", 'S', true), "QES");
        assert_eq!(with_letter("QsE", 'S', true), "QES");
        assert_eq!(with_letter("QSE", 'S', false), "QE");
        assert_eq!(with_letter("S", 'S', false), "");
    }
}
