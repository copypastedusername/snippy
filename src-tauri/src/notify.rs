//! Windows notifications ("toasts") for finished snips, with Edit / Open folder buttons.
//!
//! A portable, unpackaged app may show toasts once its AppUserModelID is described under
//! `HKCU\Software\Classes\AppUserModelId`. Buttons and clicks use protocol activation: Windows
//! opens a `snippy://` link, which starts `snippy.exe <link>`; the single-instance plugin forwards
//! that to the running Snippy (see `handle_link`). No COM activator or Start-menu shortcut needed.

use crate::capture::CaptureResult;
use crate::{winreg, AppState};
use std::path::{Path, PathBuf};
use tauri::Manager;
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

pub const APP_ID: &str = "com.snippy.tool";
const SCHEME: &str = "snippy";
const APP_ID_KEY: &str = r"Software\Classes\AppUserModelId\com.snippy.tool";
const SCHEME_KEY: &str = r"Software\Classes\snippy";
const ICON: &[u8] = include_bytes!("../icons/128x128.png");

/// Describes Snippy to the notification system and registers the `snippy://` link it activates.
/// Re-run at startup so a moved portable exe stays reachable.
pub fn register(executable: &Path) -> Result<(), String> {
    let icon = notification_icon()?;
    winreg::set_user_string(APP_ID_KEY, Some("DisplayName"), "Snippy")?;
    winreg::set_user_string(APP_ID_KEY, Some("IconUri"), &icon.to_string_lossy())?;
    winreg::set_user_string(SCHEME_KEY, None, "URL:Snippy")?;
    winreg::set_user_string(SCHEME_KEY, Some("URL Protocol"), "")?;
    winreg::set_user_string(
        &format!(r"{SCHEME_KEY}\shell\open\command"),
        None,
        &format!("\"{}\" \"%1\"", executable.to_string_lossy()),
    )
}

pub fn unregister() -> Result<(), String> {
    winreg::delete_user_tree(APP_ID_KEY)?;
    winreg::delete_user_tree(SCHEME_KEY)
}

fn notification_icon() -> Result<PathBuf, String> {
    let path = crate::config::local_data_dir().join("notification-icon.png");
    let current = std::fs::read(&path).ok();
    if current.as_deref() != Some(ICON) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
        }
        std::fs::write(&path, ICON)
            .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
    }
    Ok(path)
}

/// Shows "Snip saved" (or "Copied") with the picture and Edit / Open folder buttons.
pub fn show_capture(result: &CaptureResult) -> Result<(), String> {
    let path = result.path.as_deref().map(Path::new);
    let title = if path.is_some() {
        "Snip saved"
    } else {
        "Snip copied to clipboard"
    };
    let name = path
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let detail = format!("{} × {}  {name}", result.width, result.height);
    let edit = link("edit", path);
    let mut actions = format!(
        r#"<action content="Edit" activationType="protocol" arguments="{}"/>"#,
        xml(&edit)
    );
    let mut hero = String::new();
    if let Some(path) = path {
        actions.push_str(&format!(
            r#"<action content="Show in folder" activationType="protocol" arguments="{}"/>"#,
            xml(&link("reveal", Some(path)))
        ));
        hero = format!(
            r#"<image placement="hero" src="{}"/>"#,
            xml(&format!(
                "file:///{}",
                path.to_string_lossy().replace('\\', "/")
            ))
        );
    }
    show(&format!(
        r#"<toast launch="{launch}" activationType="protocol"><visual><binding template="ToastGeneric"><text>{title}</text><text>{detail}</text>{hero}</binding></visual><actions>{actions}</actions></toast>"#,
        launch = xml(&edit),
        title = xml(title),
        detail = xml(&detail),
    ))
}

/// Offers a new version (or a new build of this one), with an Update button that installs it.
pub fn show_update(version: &str, rebuild: bool) -> Result<(), String> {
    let title = if rebuild {
        format!("A fixed build of Snippy {version} is available")
    } else {
        format!("Snippy {version} is available")
    };
    show(&format!(
        r#"<toast launch="{update}" activationType="protocol"><visual><binding template="ToastGeneric"><text>{title}</text><text>Install the update now? Snippy restarts on the new build.</text></binding></visual><actions><action content="Update" activationType="protocol" arguments="{update}"/><action content="Later" activationType="system" arguments="dismiss"/></actions></toast>"#,
        update = xml(&link("update", None)),
        title = xml(&title),
    ))
}

pub fn show_failure(error: &str) -> Result<(), String> {
    show_failure_titled("Snip failed", error)
}

fn show_failure_titled(title: &str, error: &str) -> Result<(), String> {
    show(&format!(
        r#"<toast><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual></toast>"#,
        xml(title),
        xml(error)
    ))
}

fn show(content: &str) -> Result<(), String> {
    let document = XmlDocument::new().map_err(|e| e.message().to_string())?;
    document
        .LoadXml(&HSTRING::from(content))
        .map_err(|e| format!("Invalid notification: {}", e.message()))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|e| e.message().to_string())?;
    ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_ID))
        .and_then(|notifier| notifier.Show(&toast))
        .map_err(|e| format!("Windows refused the notification: {}", e.message()))
}

/// What a `snippy://` link asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkAction {
    /// Open the editor on a capture file, or on the latest snip.
    Edit(Option<PathBuf>),
    Reveal(PathBuf),
    /// Install the available update.
    Update,
}

/// Finds a `snippy://` link among the launch arguments.
pub fn parse_link<S: AsRef<str>>(args: &[S]) -> Option<LinkAction> {
    let link = args
        .iter()
        .skip(1)
        .map(AsRef::as_ref)
        .find(|arg| arg.starts_with(&format!("{SCHEME}://")))?;
    let rest = &link[SCHEME.len() + 3..];
    let (verb, query) = rest.split_once('?').unwrap_or((rest, ""));
    let path = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("path="))
        .map(percent_decode)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    match verb.trim_end_matches('/') {
        "edit" => Some(LinkAction::Edit(path)),
        "reveal" => path.map(LinkAction::Reveal),
        "update" => Some(LinkAction::Update),
        _ => None,
    }
}

/// Carries out a link. Paths are re-validated: anything outside the output folder is refused.
pub fn handle_link(app: &tauri::AppHandle, action: LinkAction) {
    let result = match action {
        LinkAction::Edit(path) => crate::editor::open(app, path.as_deref()),
        LinkAction::Reveal(path) => app
            .state::<AppState>()
            .config()
            .and_then(|config| crate::storage::resolve_capture(&path, &config.output.directory))
            .and_then(|path| crate::storage::reveal(&path)),
        LinkAction::Update => {
            // Only a signed, newer release can be installed, so a stray link cannot do harm.
            let app = app.clone();
            std::thread::spawn(move || {
                if let Err(error) = crate::updater::install(&app) {
                    log::warn!("Update failed: {error}");
                    if let Err(error) = show_failure_titled("Update failed", &error) {
                        log::warn!("Could not report the failed update: {error}");
                    }
                }
            });
            Ok(())
        }
    };
    if let Err(error) = result {
        log::warn!("Could not follow the notification link: {error}");
    }
}

fn link(verb: &str, path: Option<&Path>) -> String {
    match path {
        Some(path) => format!(
            "{SCHEME}://{verb}?path={}",
            percent_encode(&path.to_string_lossy())
        ),
        None => format!("{SCHEME}://{verb}"),
    }
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (byte as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

fn percent_decode(value: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        char::from(byte).to_digit(16).map(|digit| digit as u8)
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high << 4 | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::{link, parse_link, percent_decode, percent_encode, LinkAction};
    use std::path::{Path, PathBuf};

    #[test]
    fn links_round_trip_paths_with_spaces_and_unicode() {
        let path = Path::new(r"C:\Users\Ann\Pictures\Snippy\Скрин 1 & 2.png");
        let encoded = link("edit", Some(path));
        assert!(!encoded.contains(' '));
        assert_eq!(
            parse_link(&["snippy.exe", encoded.as_str()]),
            Some(LinkAction::Edit(Some(path.to_path_buf())))
        );
        assert_eq!(percent_decode(&percent_encode("a b%")), "a b%");
    }

    #[test]
    fn unknown_or_missing_links_are_ignored() {
        assert_eq!(parse_link(&["snippy.exe"]), None);
        assert_eq!(
            parse_link(&["snippy.exe", "snippy://delete?path=C%3A%5Cx"]),
            None
        );
        assert_eq!(parse_link(&["snippy.exe", "snippy://reveal"]), None);
        assert_eq!(
            parse_link(&["snippy.exe", "snippy://update/"]),
            Some(LinkAction::Update)
        );
        assert_eq!(
            parse_link(&["snippy.exe", "snippy://edit"]),
            Some(LinkAction::Edit(None))
        );
        assert_eq!(
            parse_link(&["snippy.exe", "snippy://reveal?path=C%3A%5Cx.png"]),
            Some(LinkAction::Reveal(PathBuf::from(r"C:\x.png")))
        );
        // A truncated escape is kept literally instead of panicking.
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%4"), "%4");
    }
}
