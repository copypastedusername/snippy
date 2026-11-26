//! Knows whether the desktop can be grabbed right now.
//!
//! Right after an unlock or a wake, the image Windows hands out can still be the one from before
//! the screens went dark, and the monitor layout may still be settling. The receiver window of
//! `instances.rs` forwards the session, power and display messages here; a snip asks `readiness`
//! before it grabs.

use std::sync::Mutex;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, WPARAM};
use windows::Win32::System::Power::{
    RegisterPowerSettingNotification, UnregisterPowerSettingNotification, HPOWERNOTIFY,
    POWERBROADCAST_SETTING,
};
use windows::Win32::System::RemoteDesktop::{
    WTSFreeMemory, WTSQuerySessionInformationW, WTSRegisterSessionNotification, WTSSessionInfoEx,
    WTSUnRegisterSessionNotification, NOTIFY_FOR_THIS_SESSION, WTSINFOEXW,
    WTS_CURRENT_SERVER_HANDLE, WTS_CURRENT_SESSION, WTS_SESSIONSTATE_LOCK,
};
use windows::Win32::System::SystemServices::GUID_CONSOLE_DISPLAY_STATE;
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_WINDOW_HANDLE, PBT_APMRESUMEAUTOMATIC, PBT_POWERSETTINGCHANGE, WM_DISPLAYCHANGE,
    WM_POWERBROADCAST, WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT, WTS_SESSION_LOCK,
    WTS_SESSION_UNLOCK,
};

/// How long the desktop gets to redraw after it comes back before a snip grabs it.
const SETTLE: Duration = Duration::from_millis(1500);

/// What Windows told us, reduced to what matters for a grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    Locked,
    Unlocked,
    /// The session came back to the physical console, or the machine resumed from sleep.
    Returned,
    LayoutChanged,
    Display(DisplayPower),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisplayPower {
    Off,
    On,
    Dimmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct State {
    locked: bool,
    /// Unknown until Windows first reports it (it does right after registration).
    display: Option<DisplayPower>,
    /// When the desktop last came back; the next grab waits until `SETTLE` after it.
    returned_at: Option<Instant>,
    /// Counts every event, so a grab can tell that something happened while it ran.
    events: u64,
}

/// Whether a grab may happen now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Locked,
    Settling(Duration),
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// The display-power registration, released by `unregister`.
static POWER: Mutex<Option<isize>> = Mutex::new(None);

/// Starts listening on the receiver window. Call it on that window's thread before its message
/// loop runs, so no notification can arrive between the registration and the lock-state reading
/// that follows it (Snippy may start on a locked desktop). Without this, snips never wait.
pub fn register(window: HWND) {
    // SAFETY: `window` is the live receiver window of the calling thread; `unregister` undoes both.
    let (sessions, power) = unsafe {
        (
            WTSRegisterSessionNotification(window, NOTIFY_FOR_THIS_SESSION),
            RegisterPowerSettingNotification(
                HANDLE(window.0),
                &GUID_CONSOLE_DISPLAY_STATE,
                DEVICE_NOTIFY_WINDOW_HANDLE,
            ),
        )
    };
    match power {
        Ok(handle) => *lock_power() = Some(handle.0),
        Err(error) => log::warn!("Monitors waking up are not watched: {error}"),
    }
    // Without session notifications an unlock would go unseen, so a lock must not be remembered.
    let locked = match sessions {
        Ok(()) => session_is_locked(),
        Err(error) => {
            log::warn!("Unlocks are not watched: {error}");
            false
        }
    };
    *lock() = Some(State {
        locked,
        ..State::default()
    });
}

/// Stops listening; snips no longer wait. Call it before the window goes away.
pub fn unregister(window: HWND) {
    *lock() = None;
    // SAFETY: undoes the registrations `register` made for this window.
    unsafe {
        let _ = WTSUnRegisterSessionNotification(window);
        if let Some(handle) = lock_power().take() {
            let _ = UnregisterPowerSettingNotification(HPOWERNOTIFY(handle));
        }
    }
}

/// Records a window message if it is one of ours. The caller still passes every message on to
/// `DefWindowProcW`.
///
/// # Safety
/// The arguments must be those of a message delivered to the window given to `register`.
pub unsafe fn handle(message: u32, wparam: WPARAM, lparam: LPARAM) {
    let Some(event) = decode(message, wparam, lparam) else {
        return;
    };
    let mut state = lock();
    if let Some(current) = *state {
        let next = apply(current, event, Instant::now());
        if next.returned_at != current.returned_at {
            log::info!("The screen came back ({event:?}); the next snip lets it settle first");
        }
        *state = Some(next);
    }
}

/// Whether the desktop can be grabbed now (always `Ready` when nothing is watched), and how many
/// events have been seen, to compare after a grab.
pub fn readiness() -> (Readiness, u64) {
    lock()
        .map(|state| (readiness_of(&state, Instant::now()), state.events))
        .unwrap_or((Readiness::Ready, 0))
}

fn lock() -> std::sync::MutexGuard<'static, Option<State>> {
    STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn lock_power() -> std::sync::MutexGuard<'static, Option<isize>> {
    POWER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// # Safety
/// For `PBT_POWERSETTINGCHANGE`, `lparam` must point at a `POWERBROADCAST_SETTING`, valid for the
/// duration of the message (as Windows delivers it).
unsafe fn decode(message: u32, wparam: WPARAM, lparam: LPARAM) -> Option<Event> {
    match (message, wparam.0 as u32) {
        (WM_WTSSESSION_CHANGE, WTS_SESSION_LOCK) => Some(Event::Locked),
        (WM_WTSSESSION_CHANGE, WTS_SESSION_UNLOCK) => Some(Event::Unlocked),
        (WM_WTSSESSION_CHANGE, WTS_CONSOLE_CONNECT) => Some(Event::Returned),
        (WM_POWERBROADCAST, PBT_APMRESUMEAUTOMATIC) => Some(Event::Returned),
        (WM_DISPLAYCHANGE, _) => Some(Event::LayoutChanged),
        (WM_POWERBROADCAST, PBT_POWERSETTINGCHANGE) if lparam.0 != 0 => {
            // Read through the message pointer: the payload runs past the one-byte `Data` field.
            let message = lparam.0 as *const u8;
            let setting = message.cast::<POWERBROADCAST_SETTING>();
            let guid = std::ptr::addr_of!((*setting).PowerSetting).read_unaligned();
            let length = std::ptr::addr_of!((*setting).DataLength).read_unaligned();
            if guid != GUID_CONSOLE_DISPLAY_STATE || (length as usize) < std::mem::size_of::<u32>()
            {
                return None;
            }
            let data = message.add(std::mem::offset_of!(POWERBROADCAST_SETTING, Data));
            display_power(data.cast::<u32>().read_unaligned()).map(Event::Display)
        }
        _ => None,
    }
}

fn display_power(value: u32) -> Option<DisplayPower> {
    match value {
        0 => Some(DisplayPower::Off),
        1 => Some(DisplayPower::On),
        2 => Some(DisplayPower::Dimmed),
        _ => None,
    }
}

/// The state after `event`. Lock state and the settle deadline change together, so a snip never
/// sees an unlocked desktop without the deadline that goes with it.
fn apply(state: State, event: Event, now: Instant) -> State {
    let state = State {
        events: state.events.wrapping_add(1),
        ..state
    };
    let returned = State {
        returned_at: Some(now),
        ..state
    };
    match event {
        Event::Locked => State {
            locked: true,
            ..state
        },
        Event::Unlocked => State {
            locked: false,
            ..returned
        },
        Event::Returned | Event::LayoutChanged => returned,
        // Only a real Off → On transition brings an image back; the first report after
        // registration and repeated "on" reports do not.
        Event::Display(power) => {
            let woke = power == DisplayPower::On && state.display == Some(DisplayPower::Off);
            let next = if woke { returned } else { state };
            State {
                display: Some(power),
                ..next
            }
        }
    }
}

fn readiness_of(state: &State, now: Instant) -> Readiness {
    if state.locked {
        return Readiness::Locked;
    }
    let remaining = state
        .returned_at
        .map(|at| SETTLE.saturating_sub(now.saturating_duration_since(at)))
        .unwrap_or_default();
    if remaining.is_zero() {
        Readiness::Ready
    } else {
        Readiness::Settling(remaining)
    }
}

/// Whether the session is locked right now; unknown counts as unlocked.
fn session_is_locked() -> bool {
    let mut buffer = windows::core::PWSTR::null();
    let mut size = 0_u32;
    // SAFETY: Windows allocates the buffer; it is read only when it is large enough for a
    // WTSINFOEXW and freed with WTSFreeMemory.
    unsafe {
        if WTSQuerySessionInformationW(
            WTS_CURRENT_SERVER_HANDLE,
            WTS_CURRENT_SESSION,
            WTSSessionInfoEx,
            &mut buffer,
            &mut size,
        )
        .is_err()
            || buffer.is_null()
        {
            return false;
        }
        let locked = (size as usize) >= std::mem::size_of::<WTSINFOEXW>() && {
            let info = &*(buffer.0 as *const WTSINFOEXW);
            info.Level == 1
                && info.Data.WTSInfoExLevel1.SessionFlags as u32 == WTS_SESSIONSTATE_LOCK
        };
        WTSFreeMemory(buffer.0.cast());
        locked
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(start: Instant, ms: u64) -> Instant {
        start + Duration::from_millis(ms)
    }

    #[test]
    fn nothing_happened_means_ready() {
        assert_eq!(
            readiness_of(&State::default(), Instant::now()),
            Readiness::Ready
        );
    }

    #[test]
    fn a_locked_session_is_not_ready_until_unlocked_and_settled() {
        let start = Instant::now();
        let locked = apply(State::default(), Event::Locked, start);
        assert_eq!(readiness_of(&locked, at(start, 60_000)), Readiness::Locked);

        let unlocked = apply(locked, Event::Unlocked, at(start, 1000));
        assert_eq!(
            readiness_of(&unlocked, at(start, 1500)),
            Readiness::Settling(Duration::from_millis(1000))
        );
        assert_eq!(readiness_of(&unlocked, at(start, 2500)), Readiness::Ready);
    }

    #[test]
    fn a_session_found_locked_at_start_waits_for_the_unlock() {
        let state = State {
            locked: true,
            ..State::default()
        };
        assert_eq!(readiness_of(&state, Instant::now()), Readiness::Locked);
    }

    #[test]
    fn time_already_waited_counts_towards_the_settle() {
        let start = Instant::now();
        let state = apply(State::default(), Event::LayoutChanged, start);
        // A snip with a delay that ends after the settle time does not wait any longer.
        assert_eq!(readiness_of(&state, at(start, 3000)), Readiness::Ready);
        assert_eq!(
            readiness_of(&state, at(start, 1499)),
            Readiness::Settling(Duration::from_millis(1))
        );
        assert_eq!(readiness_of(&state, at(start, 1500)), Readiness::Ready);
    }

    #[test]
    fn only_an_off_to_on_transition_counts_as_a_wake() {
        let start = Instant::now();
        // The report right after registration only makes the state known.
        let known = apply(State::default(), Event::Display(DisplayPower::On), start);
        assert_eq!(known.returned_at, None);
        assert_eq!(known.display, Some(DisplayPower::On));
        // Repeated "on" is not a wake either.
        let again = apply(known, Event::Display(DisplayPower::On), start);
        assert_eq!(again.returned_at, None);
        // Dimmed and back keeps the image.
        let dimmed = apply(known, Event::Display(DisplayPower::Dimmed), start);
        assert_eq!(
            apply(dimmed, Event::Display(DisplayPower::On), start).returned_at,
            None
        );

        let off = apply(known, Event::Display(DisplayPower::Off), start);
        let on = apply(off, Event::Display(DisplayPower::On), at(start, 10));
        assert_eq!(on.returned_at, Some(at(start, 10)));
    }

    #[test]
    fn every_event_is_counted() {
        let start = Instant::now();
        let state = apply(State::default(), Event::Display(DisplayPower::On), start);
        let state = apply(state, Event::Locked, start);
        assert_eq!(state.events, 2);
    }

    #[test]
    fn locking_keeps_the_previous_settle_deadline() {
        let start = Instant::now();
        let returned = apply(State::default(), Event::Returned, start);
        let locked = apply(returned, Event::Locked, at(start, 100));
        assert_eq!(locked.returned_at, Some(start));
        assert!(locked.locked);
    }

    #[test]
    fn display_states_decode_from_their_documented_values() {
        assert_eq!(display_power(0), Some(DisplayPower::Off));
        assert_eq!(display_power(1), Some(DisplayPower::On));
        assert_eq!(display_power(2), Some(DisplayPower::Dimmed));
        assert_eq!(display_power(7), None);
    }

    #[test]
    fn a_power_setting_message_is_decoded_from_its_dword() {
        // POWERBROADCAST_SETTING's header followed by its DWORD payload, with no padding.
        #[repr(C)]
        struct Message {
            setting: windows::core::GUID,
            length: u32,
            value: u32,
        }
        let decode_message = |message: &Message| {
            let lparam = LPARAM(message as *const Message as isize);
            let wparam = WPARAM(PBT_POWERSETTINGCHANGE as usize);
            // SAFETY: `lparam` points at a POWERBROADCAST_SETTING header and 4 payload bytes.
            unsafe { decode(WM_POWERBROADCAST, wparam, lparam) }
        };
        let mut message = Message {
            setting: GUID_CONSOLE_DISPLAY_STATE,
            length: 4,
            value: 0,
        };
        assert_eq!(
            decode_message(&message),
            Some(Event::Display(DisplayPower::Off))
        );
        message.value = 1;
        assert_eq!(
            decode_message(&message),
            Some(Event::Display(DisplayPower::On))
        );
        // A too-short payload or another setting is refused before the value is read.
        message.length = 1;
        assert_eq!(decode_message(&message), None);
        message.length = 4;
        message.setting = windows::core::GUID::zeroed();
        assert_eq!(decode_message(&message), None);
    }
}
