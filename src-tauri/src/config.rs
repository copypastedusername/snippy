use serde::{Deserialize, Deserializer, Serialize};
use std::ffi::c_void;
use std::path::PathBuf;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{FOLDERID_Pictures, SHGetKnownFolderPath, KF_FLAG_DEFAULT};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptureMode {
    #[default]
    Rectangle,
    Freeform,
    Window,
    Fullscreen,
}

impl CaptureMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rectangle => "rectangle",
            Self::Freeform => "freeform",
            Self::Window => "window",
            Self::Fullscreen => "fullscreen",
        }
    }
}

impl std::str::FromStr for CaptureMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "rectangle" => Ok(Self::Rectangle),
            "freeform" => Ok(Self::Freeform),
            "window" => Ok(Self::Window),
            "fullscreen" => Ok(Self::Fullscreen),
            _ => Err(format!("Unknown capture mode: {value}")),
        }
    }
}

impl<'de> Deserialize<'de> for CaptureMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(value.parse().unwrap_or_default())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Png,
    Jpeg,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
        }
    }
}

impl<'de> Deserialize<'de> for Format {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Ok(if value == "jpeg" {
            Self::Jpeg
        } else {
            Self::Png
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    #[serde(default = "default_rectangle_hotkey")]
    pub rectangle: String,
    #[serde(default = "default_freeform_hotkey")]
    pub freeform: String,
    #[serde(default = "default_window_hotkey")]
    pub window: String,
    #[serde(default = "default_fullscreen_hotkey")]
    pub fullscreen: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            rectangle: default_rectangle_hotkey(),
            freeform: default_freeform_hotkey(),
            window: default_window_hotkey(),
            fullscreen: default_fullscreen_hotkey(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    #[serde(default)]
    pub default_mode: CaptureMode,
    #[serde(default)]
    pub delay_seconds: u64,
    #[serde(default)]
    pub include_cursor: bool,
    /// Sound played when a snip is taken; off by default.
    #[serde(default)]
    pub sound: crate::sound::CaptureSound,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            default_mode: CaptureMode::Rectangle,
            delay_seconds: 0,
            include_cursor: false,
            sound: crate::sound::CaptureSound::Off,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    #[serde(default = "default_output_directory")]
    pub directory: PathBuf,
    #[serde(default)]
    pub format: Format,
    #[serde(default = "default_jpeg_quality")]
    pub jpeg_quality: u8,
    #[serde(default = "default_filename_pattern")]
    pub filename_pattern: String,
    #[serde(default = "default_true")]
    pub auto_save: bool,
    #[serde(default = "default_true")]
    pub copy_to_clipboard: bool,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            directory: default_output_directory(),
            format: Format::Png,
            jpeg_quality: default_jpeg_quality(),
            filename_pattern: default_filename_pattern(),
            auto_save: true,
            copy_to_clipboard: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct BehaviorConfig {
    #[serde(default)]
    pub run_as_admin: bool,
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub start_minimized: bool,
    #[serde(default = "default_true")]
    pub show_flyout: bool,
    /// Registered as a handler for Win+Shift+S (`ms-screenclip:` links); see `screenclip.rs`.
    #[serde(default)]
    pub handle_screenclip: bool,
    /// Print Screen snips: registered directly, or via the `ms-screenclip:` link when Windows
    /// routes the key to screen capture.
    #[serde(default)]
    pub handle_print_screen: bool,
    /// Turns off Snippy's own hotkeys, leaving only the Windows keys above.
    #[serde(default)]
    pub windows_shortcuts_only: bool,
    /// Announce snips as Windows notifications (with an Edit button) instead of Snippy's flyout.
    #[serde(default)]
    pub windows_notifications: bool,
    /// How long Snippy's preview stays on screen: 3, 5 or 10 seconds.
    #[serde(default = "default_preview_seconds")]
    pub preview_seconds: u8,
    /// Check GitHub for a newer release and offer to install it.
    #[serde(default = "default_true")]
    pub check_updates: bool,
}

impl Default for BehaviorConfig {
    fn default() -> Self {
        Self {
            run_as_admin: false,
            close_to_tray: true,
            start_minimized: false,
            show_flyout: true,
            handle_screenclip: false,
            handle_print_screen: false,
            windows_shortcuts_only: false,
            windows_notifications: false,
            preview_seconds: default_preview_seconds(),
            check_updates: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppearanceConfig {
    #[serde(default = "default_true")]
    pub show_magnifier: bool,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            show_magnifier: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub hotkeys: Hotkeys,
    #[serde(default)]
    pub capture: CaptureConfig,
    #[serde(default)]
    pub output: OutputConfig,
    #[serde(default)]
    pub behavior: BehaviorConfig,
    #[serde(default)]
    pub appearance: AppearanceConfig,
}

fn default_rectangle_hotkey() -> String {
    "Ctrl+Shift+S".to_owned()
}

fn default_freeform_hotkey() -> String {
    "Ctrl+Shift+D".to_owned()
}

fn default_window_hotkey() -> String {
    "Ctrl+Shift+W".to_owned()
}

fn default_fullscreen_hotkey() -> String {
    "Ctrl+Shift+F".to_owned()
}

fn default_jpeg_quality() -> u8 {
    90
}

fn default_filename_pattern() -> String {
    "Snippy_%Y%m%d_%H%M%S".to_owned()
}

fn default_true() -> bool {
    true
}

/// Roaming settings live in `%APPDATA%\Snippy`; everything the user may want to inspect or back up
/// (config, logs) goes here, so the app owns exactly one folder per data scope.
pub fn app_data_dir() -> Result<PathBuf, String> {
    let app_data = std::env::var_os("APPDATA")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "APPDATA is not available".to_owned())?;
    Ok(PathBuf::from(app_data).join("Snippy"))
}

/// Machine-local caches (the WebView2 profile) live in `%LOCALAPPDATA%\Snippy`.
pub fn local_data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| app_data_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Snippy")
}

pub fn config_path() -> Result<PathBuf, String> {
    Ok(app_data_dir()?.join("config.json"))
}

pub fn default_output_directory() -> PathBuf {
    known_pictures_directory()
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .filter(|value| !value.is_empty())
                .map(|value| PathBuf::from(value).join("Pictures"))
        })
        .unwrap_or_else(|| PathBuf::from("Pictures"))
        .join("Snippy")
}

fn known_pictures_directory() -> Option<PathBuf> {
    let raw = unsafe { SHGetKnownFolderPath(&FOLDERID_Pictures, KF_FLAG_DEFAULT, None).ok()? };
    let path = unsafe { raw.to_string().ok().map(PathBuf::from) };
    unsafe { CoTaskMemFree(Some(raw.0.cast::<c_void>())) };
    path
}

impl BehaviorConfig {
    /// Whether the `ms-screenclip:` link handler must be registered.
    pub fn wants_screenclip_link(&self) -> bool {
        self.handle_screenclip || self.handle_print_screen
    }
}

impl Config {
    pub fn validate(&mut self) {
        self.capture.delay_seconds = self.capture.delay_seconds.min(10);
        if !PREVIEW_SECONDS.contains(&self.behavior.preview_seconds) {
            self.behavior.preview_seconds = default_preview_seconds();
        }
        self.output.jpeg_quality = self.output.jpeg_quality.clamp(1, 100);
        // "Windows shortcuts only" without any Windows shortcut would leave no way to snip by key.
        if !self.behavior.handle_screenclip && !self.behavior.handle_print_screen {
            self.behavior.windows_shortcuts_only = false;
        }
        if !self.output.auto_save && !self.output.copy_to_clipboard {
            self.output.copy_to_clipboard = true;
        }
        if std::fs::create_dir_all(&self.output.directory).is_err() {
            self.output.directory = default_output_directory();
            if let Err(error) = std::fs::create_dir_all(&self.output.directory) {
                log::warn!("Could not create the default output directory: {error}");
            }
        }
    }
}

pub const PREVIEW_SECONDS: [u8; 3] = [3, 5, 10];

fn default_preview_seconds() -> u8 {
    5
}

pub fn load() -> Result<Config, String> {
    let path = config_path()?;
    let mut config = match std::fs::read_to_string(&path) {
        Ok(json) => match serde_json::from_str(&json) {
            Ok(config) => config,
            Err(error) => {
                log::warn!("Ignoring invalid config at {}: {error}", path.display());
                Config::default()
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Config::default(),
        Err(error) => return Err(format!("Failed to read {}: {error}", path.display())),
    };
    config.validate();
    save(&config)?;
    Ok(config)
}

/// Saves are serialised: two at once (a settings change racing the editor's preferences, say)
/// would otherwise write the same temporary file and could leave the older one in place.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn save(config: &Config) -> Result<(), String> {
    let _saving = SAVING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = config_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| "Config path has no parent directory".to_owned())?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create {}: {error}", parent.display()))?;
    let bytes = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("Failed to serialize config: {error}"))?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("Failed to write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path).map_err(|error| {
        format!(
            "Failed to replace config {} with {}: {error}",
            path.display(),
            temporary.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn validation_keeps_at_least_one_capture_output_enabled() {
        let mut config = Config::default();
        config.output.directory = std::env::temp_dir();
        config.output.auto_save = false;
        config.output.copy_to_clipboard = false;

        config.validate();

        assert!(config.output.copy_to_clipboard);
    }
}
