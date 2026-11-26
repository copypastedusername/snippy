use serde::{Deserialize, Deserializer, Serialize};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
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
        // Checked by its form only. The folder is created when a snip is saved: touching it here
        // would hold up the start on an offline network share, and a folder that is merely
        // unavailable right now must not be swapped for the default for good.
        if !self.output.directory.is_absolute() {
            self.output.directory = default_output_directory();
        }
    }
}

pub const PREVIEW_SECONDS: [u8; 3] = [3, 5, 10];

fn default_preview_seconds() -> u8 {
    5
}

/// A config file larger than this is not a Snippy config (it is a few kilobytes).
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// What loading had to repair, for the main page to tell the user (see `recovery_notice`).
static RECOVERY: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// The settings Snippy starts with. Never fails because of what the file holds: an old, partly
/// wrong or unreadable config costs only the settings that cannot be used, and the file as it was
/// is copied aside before Snippy writes over it. `Err` only when there is no place for settings at
/// all (no `%APPDATA%`).
pub fn load() -> Result<Config, String> {
    let path = config_path()?;
    let (config, notice) = load_from(&path);
    if let Some(notice) = &notice {
        note(notice);
    }
    *lock_recovery() = notice;
    Ok(config)
}

/// A message about settings that were reset or a settings file that was kept aside at start.
pub fn recovery_notice() -> Option<String> {
    lock_recovery().clone()
}

fn lock_recovery() -> std::sync::MutexGuard<'static, Option<String>> {
    RECOVERY
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How the file at `path` was read.
enum Loaded {
    Missing,
    /// Present, but not read (locked, no access): it must not be overwritten unseen.
    Unread(String),
    /// Not a config at all (not JSON, not an object, far too large).
    Broken(String),
    /// Read; the keys whose values could not be used, as `section.key`.
    Parsed(Config, Vec<String>),
}

/// The config at `path` and, when something had to be repaired, what to tell the user. Writes the
/// repaired config back (after copying a damaged original aside), unless that is unsafe.
fn load_from(path: &Path) -> (Config, Option<String>) {
    let (mut config, problem) = match read_config(path) {
        Loaded::Missing => (Config::default(), None),
        Loaded::Unread(error) => {
            note(&format!("Could not read {}: {error}", path.display()));
            // Settings changes may replace it only once it can be copied aside.
            protect(path);
            let notice = "Snippy could not open your settings file and started with the default \
                          settings. The file was not changed."
                .to_owned();
            return (validated(Config::default()), Some(notice));
        }
        Loaded::Broken(error) => {
            note(&format!("{} is damaged: {error}", path.display()));
            (
                Config::default(),
                Some(
                    "Your settings file was damaged, so Snippy reset all settings to their \
                     defaults."
                        .to_owned(),
                ),
            )
        }
        Loaded::Parsed(config, dropped) if dropped.is_empty() => (config, None),
        Loaded::Parsed(config, dropped) => (
            config,
            Some(format!(
                "Some values in your settings file were invalid and were reset to their \
                 defaults: {}.",
                dropped.join(", ")
            )),
        ),
    };
    let before = serde_json::to_value(&config).ok();
    config.validate();
    let problem = problem.or_else(|| {
        // Values outside what Snippy accepts were changed by `validate`: also a repair, so the file
        // as it was is kept too.
        (before != serde_json::to_value(&config).ok()).then(|| {
            "Some values in your settings file were out of range and were corrected.".to_owned()
        })
    });
    if problem.is_some() {
        protect(path);
    }
    let notice = match save_to(path, &config) {
        Ok(Some(backup)) => problem.map(|problem| {
            note(&format!(
                "The previous settings file was kept as {}",
                backup.display()
            ));
            format!("{problem} A copy of the old file is in the settings folder.")
        }),
        Ok(None) => problem,
        Err(error) => {
            note(&format!("The settings could not be saved: {error}"));
            problem.map(|problem| {
                format!("{problem} The repair could not be saved; Snippy tries again next time.")
            })
        }
    };
    (config, notice)
}

fn validated(mut config: Config) -> Config {
    config.validate();
    config
}

fn read_config(path: &Path) -> Loaded {
    use std::io::Read;
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Loaded::Missing,
        Err(error) => return Loaded::Unread(error.to_string()),
    };
    let mut bytes = Vec::new();
    if let Err(error) = file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes) {
        return Loaded::Unread(error.to_string());
    }
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Loaded::Broken("the file is far too large".to_owned());
    }
    parse_lenient(&bytes)
}

/// Reads a config value by value: a value that does not fit (a wrong type, out of range, a section
/// that is not an object) falls back to its default and is reported; everything else is kept.
fn parse_lenient(bytes: &[u8]) -> Loaded {
    use serde_json::Value;
    let stored = match serde_json::from_slice::<Value>(bytes) {
        Ok(Value::Object(stored)) => stored,
        Ok(_) => return Loaded::Broken("it does not hold settings".to_owned()),
        Err(error) => return Loaded::Broken(error.to_string()),
    };
    let Ok(mut merged) = serde_json::to_value(Config::default()) else {
        return Loaded::Parsed(Config::default(), Vec::new());
    };
    let fits = |merged: &Value| serde_json::from_value::<Config>(merged.clone()).is_ok();
    let mut dropped = Vec::new();
    for (section, value) in stored {
        let Some(defaults) = merged.get(&section).cloned() else {
            continue; // Not a setting of this version.
        };
        let Value::Object(entries) = value else {
            dropped.push(section);
            continue;
        };
        for (key, value) in entries {
            if defaults.get(&key).is_none() {
                continue;
            }
            let previous = merged[&section][&key].clone();
            merged[&section][&key] = value.clone();
            // Kept only when it reads back as the same value: an unknown mode or sound name would
            // otherwise turn into the default without notice.
            let kept = fits(&merged)
                && serde_json::from_value::<Config>(merged.clone())
                    .ok()
                    .and_then(|config| serde_json::to_value(config).ok())
                    .is_some_and(|read| read[&section][&key] == value);
            if !kept {
                merged[&section][&key] = previous;
                dropped.push(format!("{section}.{key}"));
            }
        }
    }
    match serde_json::from_value::<Config>(merged) {
        Ok(config) => Loaded::Parsed(config, dropped),
        Err(error) => Loaded::Broken(error.to_string()),
    }
}

/// Startup problems go to the startup trace as well: the log file does not exist yet.
fn note(message: &str) {
    log::warn!("{message}");
    crate::trace::step(message);
}

/// Saves are serialised: two at once (a settings change racing the editor's preferences, say)
/// would otherwise write the same temporary file and could leave the older one in place. It also
/// guards `PROTECTED`.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Settings files whose current contents were not fully used: before one is replaced, those
/// contents are copied aside.
static PROTECTED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn protect(path: &Path) {
    let mut protected = PROTECTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !protected.iter().any(|known| known == path) {
        protected.push(path.to_path_buf());
    }
}

pub fn save(config: &Config) -> Result<(), String> {
    save_to(&config_path()?, config).map(|_| ())
}

/// Copies the file at `path` to a new `config.json.bad-<UTC time>` (never replacing an older one).
/// `None` when there was no file to keep.
fn back_up(path: &Path) -> Result<Option<PathBuf>, String> {
    let mut source = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("could not open it ({error})")),
    };
    let stamp = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for attempt in 1..=100_u32 {
        let suffix = if attempt == 1 {
            String::new()
        } else {
            format!("_{attempt}")
        };
        let backup = path.with_file_name(format!("{name}.bad-{stamp}{suffix}"));
        let mut target = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create {} ({error})", backup.display())),
        };
        if let Err(error) = std::io::copy(&mut source, &mut target) {
            drop(target);
            let _ = std::fs::remove_file(&backup);
            return Err(format!("could not copy it ({error})"));
        }
        return Ok(Some(backup));
    }
    Err("too many backups were made this second".to_owned())
}

/// Writes `config` to `path`. Returns where the replaced file was copied to, when it had to be.
fn save_to(path: &Path, config: &Config) -> Result<Option<PathBuf>, String> {
    let _saving = SAVING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let protected = PROTECTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|known| known == path);
    let backup = if protected {
        back_up(path).map_err(|error| {
            format!(
                "The previous settings file {} could not be kept aside, so it was not replaced: \
                 {error}",
                path.display()
            )
        })?
    } else {
        None
    };
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
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!(
            "Failed to replace config {} with {}: {error}",
            path.display(),
            temporary.display()
        ));
    }
    // Only now is the old content gone from `path`, and it is in `backup`.
    PROTECTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .retain(|known| known != path);
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::{load_from, Config};

    fn test_folder(name: &str) -> std::path::PathBuf {
        let folder =
            std::env::temp_dir().join(format!("snippy-config-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&folder);
        std::fs::create_dir_all(&folder).unwrap_or_else(|error| panic!("{error}"));
        folder
    }

    fn with_output(folder: &std::path::Path) -> String {
        let mut config = Config::default();
        config.output.directory = folder.to_path_buf();
        config.behavior.run_as_admin = true;
        serde_json::to_string(&config).unwrap_or_else(|error| panic!("{error}"))
    }

    /// The `config.json.bad-*` copies next to `path`, with their contents.
    fn backups(folder: &std::path::Path) -> Vec<String> {
        let mut found: Vec<String> = std::fs::read_dir(folder)
            .unwrap_or_else(|error| panic!("{error}"))
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("config.json.bad-")
            })
            .map(|entry| {
                std::fs::read_to_string(entry.path()).unwrap_or_else(|error| panic!("{error}"))
            })
            .collect();
        found.sort();
        found
    }

    #[test]
    fn a_good_config_loads_as_it_is_without_a_backup() {
        let folder = test_folder("good");
        let path = folder.join("config.json");
        std::fs::write(&path, with_output(&folder)).unwrap_or_else(|error| panic!("{error}"));

        let (config, notice) = load_from(&path);
        let copies = backups(&folder);
        let _ = std::fs::remove_dir_all(&folder);

        assert!(config.behavior.run_as_admin);
        assert_eq!(config.output.directory, folder);
        assert_eq!(notice, None);
        assert!(copies.is_empty());
    }

    #[test]
    fn a_config_that_cannot_be_written_back_is_still_used() {
        let folder = test_folder("unwritable");
        let path = folder.join("config.json");
        let original = with_output(&folder);
        std::fs::write(&path, &original).unwrap_or_else(|error| panic!("{error}"));
        // A folder where the temporary file goes makes every write fail.
        let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::create_dir(&temporary).unwrap_or_else(|error| panic!("{error}"));

        let (config, notice) = load_from(&path);
        let after = std::fs::read_to_string(&path);
        let _ = std::fs::remove_dir_all(&folder);

        assert!(config.behavior.run_as_admin);
        assert_eq!(config.output.directory, folder);
        assert_eq!(notice, None);
        assert_eq!(after.ok(), Some(original));
    }

    #[test]
    fn a_damaged_config_is_copied_aside_byte_for_byte_before_defaults_replace_it() {
        let folder = test_folder("broken");
        let path = folder.join("config.json");
        let damaged = "{ \"behavior\": { , }";
        std::fs::write(&path, damaged).unwrap_or_else(|error| panic!("{error}"));

        let (config, notice) = load_from(&path);
        let copies = backups(&folder);
        let replaced = std::fs::read_to_string(&path);
        let _ = std::fs::remove_dir_all(&folder);

        assert!(!config.behavior.run_as_admin);
        assert_eq!(copies, [damaged]);
        assert!(replaced.is_ok_and(|json| json.contains("filename_pattern")));
        assert!(notice.is_some_and(|notice| notice.contains("damaged")));
    }

    #[test]
    fn only_the_unusable_values_of_an_old_config_are_reset() {
        let folder = test_folder("partial");
        let path = folder.join("config.json");
        let directory = serde_json::to_string(&folder).unwrap_or_else(|error| panic!("{error}"));
        let old = format!(
            r#"{{"hotkeys":[],"capture":{{"default_mode":"lasso","delay_seconds":"3",
                "include_cursor":true}},"output":{{"directory":{directory},"jpeg_quality":300}},
                "behavior":{{"run_as_admin":true,"preview_seconds":7}},"removed":{{"x":1}}}}"#
        );
        std::fs::write(&path, &old).unwrap_or_else(|error| panic!("{error}"));

        let (config, notice) = load_from(&path);
        let copies = backups(&folder);
        let _ = std::fs::remove_dir_all(&folder);

        // Kept.
        assert!(config.behavior.run_as_admin);
        assert!(config.capture.include_cursor);
        assert_eq!(config.output.directory, folder);
        // Reset to defaults, each on its own.
        let defaults = Config::default();
        assert_eq!(config.hotkeys, defaults.hotkeys);
        assert_eq!(config.capture.default_mode, defaults.capture.default_mode);
        assert_eq!(config.capture.delay_seconds, 0);
        assert_eq!(config.output.jpeg_quality, defaults.output.jpeg_quality);
        assert_eq!(config.behavior.preview_seconds, 5);
        let notice = notice.unwrap_or_else(|| panic!("the repair is reported"));
        for key in [
            "hotkeys",
            "capture.default_mode",
            "capture.delay_seconds",
            "output.jpeg_quality",
        ] {
            assert!(notice.contains(key), "{key} missing from {notice}");
        }
        assert_eq!(copies, [old]);
    }

    #[test]
    fn a_file_that_cannot_be_read_is_never_overwritten() {
        use std::os::windows::fs::OpenOptionsExt;
        let folder = test_folder("locked");
        let path = folder.join("config.json");
        let original = with_output(&folder);
        std::fs::write(&path, &original).unwrap_or_else(|error| panic!("{error}"));
        // Opened with no sharing: nobody else may read, write or replace it meanwhile.
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap_or_else(|error| panic!("{error}"));

        let (config, notice) = load_from(&path);
        let refused = super::save_to(&path, &config);
        drop(lock);
        let after = std::fs::read_to_string(&path);
        let _ = std::fs::remove_dir_all(&folder);

        assert!(!config.behavior.run_as_admin);
        assert!(notice.is_some_and(|notice| notice.contains("was not changed")));
        assert!(refused.is_err());
        assert_eq!(after.ok(), Some(original));
    }

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
