//! Lets Snippy answer the keys Windows keeps for screen capture: Win+Shift+S and Print Screen.
//!
//! Windows 11 answers Win+Shift+S (and Print Screen, when "Use the Print Screen key to open
//! screen capture" is on) by opening an `ms-screenclip:` link. Windows 10 opens the link only for
//! Print Screen; for Win+Shift+S it starts its own clipping, which `keyhook.rs` takes instead.
//! Snippy registers itself, per user and without elevation, as an application that can open
//! `ms-screenclip:` and `ms-screensketch:` (Snip & Sketch's link). Windows protects the user's
//! choice of default handler, so the user picks Snippy once: in the "choose an app" prompt (with
//! "Always") or under Settings > Default apps. The launch arrives as
//! `snippy.exe --screenclip "<link>"` and starts a snip in the default capture mode.
//!
//! Where Windows leaves a key unclaimed (trimmed builds, the Print Screen option turned off, which
//! is the Windows 10 default), `shortcuts.rs` registers it directly instead.

use crate::winreg;
use std::path::Path;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::UI::Shell::{
    AssocQueryStringW, SHChangeNotify, ASSOCF_IS_PROTOCOL, ASSOCSTR_FRIENDLYAPPNAME,
    ASSOCSTR_PROGID, SHCNE_ASSOCCHANGED, SHCNF_IDLIST,
};

/// Command-line flag that marks a launch from an `ms-screenclip:` link.
pub const LAUNCH_FLAG: &str = "--screenclip";
/// Settings page listing the link types Snippy can open, where the user can make it the default.
/// Windows 10 ignores the query and opens the general Default apps page.
pub const DEFAULT_APPS_PAGE: &str = "ms-settings:defaultapps?registeredAppUser=Snippy";
/// Settings page with "Use the Print Screen key to open screen capture".
pub const KEYBOARD_SETTINGS_PAGE: &str = "ms-settings:easeofaccess-keyboard";

const KEYBOARD_KEY: &str = r"Control Panel\Keyboard";
const PRINT_SCREEN_VALUE: &str = "PrintScreenKeyForSnippingEnabled";
/// Windows 11 22H2: from the May 2023 configuration update Print Screen opens screen capture
/// unless the user turned it off; Windows 10 keeps it off.
const PRINT_SCREEN_DEFAULT_ON_BUILD: u32 = 22621;

/// The links Windows opens for its screen capture keys. Both start a snip in Snippy.
const SCHEMES: [&str; 2] = ["ms-screenclip", "ms-screensketch"];
/// Windows 11's first build.
const WINDOWS_11_BUILD: u32 = 22000;
const APP_NAME: &str = "Snippy";
const PROG_ID: &str = "Snippy.ScreenClip";
const PROG_ID_KEY: &str = r"Software\Classes\Snippy.ScreenClip";
const APP_KEY: &str = r"Software\Snippy";
const CAPABILITIES_KEY: &str = r"Software\Snippy\Capabilities";
const REGISTERED_APPLICATIONS_KEY: &str = r"Software\RegisteredApplications";

/// True when the process was started from an `ms-screenclip:` link.
pub fn is_screenclip_launch<S: AsRef<str>>(args: &[S]) -> bool {
    args.iter().skip(1).any(|arg| arg.as_ref() == LAUNCH_FLAG)
}

/// Whether Windows takes Print Screen for its own screen capture. Then a registered Print Screen
/// hotkey may succeed but never fire; the user turns the option off in keyboard settings.
/// Whether Windows' "Print Screen opens screen capture" takes the key: `Some` when the setting says
/// so, `None` when it was never set on a build where Windows may have turned it on by an update.
pub fn windows_routes_print_screen() -> Option<bool> {
    print_screen_routed(
        winreg::read_user_dword(KEYBOARD_KEY, PRINT_SCREEN_VALUE),
        windows_build(),
    )
}

fn print_screen_routed(setting: Option<u32>, build: Option<u32>) -> Option<bool> {
    match setting {
        Some(value) => Some(value != 0),
        // Older Windows has no such option; newer builds turned it on with a later update, so an
        // unset value there is not proof either way.
        None if build.is_some_and(|build| build >= PRINT_SCREEN_DEFAULT_ON_BUILD) => None,
        None => Some(false),
    }
}

pub fn windows_build() -> Option<u32> {
    winreg::read_machine_string(
        r"SOFTWARE\Microsoft\Windows NT\CurrentVersion",
        "CurrentBuildNumber",
    )?
    .trim()
    .parse()
    .ok()
}

/// Windows 10 and older, where Win+Shift+S bypasses the links.
pub fn before_windows_11(build: Option<u32>) -> bool {
    build.is_some_and(|build| build < WINDOWS_11_BUILD)
}

/// The links that should open Snippy on this Windows build.
pub fn required_schemes(build: Option<u32>) -> &'static [&'static str] {
    if before_windows_11(build) {
        &SCHEMES
    } else {
        &SCHEMES[..1]
    }
}

/// The app Windows opens for `scheme` links, and whether that is Snippy. `None` when no app is
/// set.
pub fn current_handler(scheme: &str) -> Option<(String, bool)> {
    let query = |what| {
        let scheme: Vec<u16> = scheme.encode_utf16().chain(std::iter::once(0)).collect();
        let mut buffer = [0_u16; 512];
        let mut length = buffer.len() as u32;
        // SAFETY: the scheme is NUL-terminated and `length` is the buffer size in characters.
        let result = unsafe {
            AssocQueryStringW(
                ASSOCF_IS_PROTOCOL,
                what,
                PCWSTR(scheme.as_ptr()),
                PCWSTR::null(),
                PWSTR(buffer.as_mut_ptr()),
                &mut length,
            )
        };
        result.is_ok().then(|| {
            let end = buffer
                .iter()
                .position(|&unit| unit == 0)
                .unwrap_or(buffer.len());
            String::from_utf16_lossy(&buffer[..end])
        })
    };
    let prog_id = query(ASSOCSTR_PROGID)?;
    let name = query(ASSOCSTR_FRIENDLYAPPNAME).unwrap_or_else(|| prog_id.clone());
    Some((name, prog_id.eq_ignore_ascii_case(PROG_ID)))
}

/// Registers (or refreshes, after the portable exe moved) Snippy as a handler of both links.
pub fn register(executable: &Path) -> Result<(), String> {
    let executable = executable.to_string_lossy();
    let command = format!("\"{executable}\" {LAUNCH_FLAG} \"%1\"");
    let icon = format!("\"{executable}\",0");

    winreg::set_user_string(PROG_ID_KEY, None, "Snippy screen snip")?;
    winreg::set_user_string(&format!(r"{PROG_ID_KEY}\DefaultIcon"), None, &icon)?;
    winreg::set_user_string(
        &format!(r"{PROG_ID_KEY}\shell\open\command"),
        None,
        &command,
    )?;
    winreg::set_user_string(CAPABILITIES_KEY, Some("ApplicationName"), APP_NAME)?;
    winreg::set_user_string(
        CAPABILITIES_KEY,
        Some("ApplicationDescription"),
        "Takes a snip when you press Win+Shift+S or Print Screen.",
    )?;
    for scheme in SCHEMES {
        winreg::set_user_string(
            &format!(r"{CAPABILITIES_KEY}\URLAssociations"),
            Some(scheme),
            PROG_ID,
        )?;
    }
    winreg::set_user_string(
        REGISTERED_APPLICATIONS_KEY,
        Some(APP_NAME),
        CAPABILITIES_KEY,
    )?;
    notify_shell();
    Ok(())
}

/// Removes everything `register` wrote. Missing keys are not an error.
pub fn unregister() -> Result<(), String> {
    winreg::delete_user_value(REGISTERED_APPLICATIONS_KEY, APP_NAME)?;
    winreg::delete_user_tree(APP_KEY)?;
    winreg::delete_user_tree(PROG_ID_KEY)?;
    notify_shell();
    Ok(())
}

fn notify_shell() {
    // SAFETY: SHCNE_ASSOCCHANGED with SHCNF_IDLIST takes no item pointers.
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

#[cfg(test)]
mod tests {
    use super::{is_screenclip_launch, print_screen_routed, required_schemes};

    #[test]
    fn windows_10_needs_both_links_and_windows_11_only_screenclip() {
        assert_eq!(
            required_schemes(Some(19045)),
            ["ms-screenclip", "ms-screensketch"]
        );
        assert_eq!(required_schemes(Some(22631)), ["ms-screenclip"]);
        assert_eq!(required_schemes(None), ["ms-screenclip"]);
    }

    #[test]
    fn only_the_exact_flag_after_the_executable_counts() {
        assert!(is_screenclip_launch(&[
            "snippy.exe",
            "--screenclip",
            "ms-screenclip:?source=HotKey"
        ]));
        assert!(!is_screenclip_launch(&["snippy.exe"]));
        assert!(!is_screenclip_launch(&["--screenclip"]));
        assert!(!is_screenclip_launch(&["snippy.exe", "--screenclip=1"]));
    }

    #[test]
    fn print_screen_routing_follows_the_setting_then_the_build_default() {
        assert_eq!(print_screen_routed(Some(1), Some(19045)), Some(true));
        assert_eq!(print_screen_routed(Some(0), Some(26100)), Some(false));
        // Windows 10 (19045) has no such option; on Windows 11 22H2+ an unset value is unknown.
        assert_eq!(print_screen_routed(None, Some(19045)), Some(false));
        assert_eq!(print_screen_routed(None, Some(22621)), None);
        assert_eq!(print_screen_routed(None, None), Some(false));
    }
}
