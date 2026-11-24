use crate::config::OutputConfig;
use crate::imageops;
use chrono::format::{Item, StrftimeItems};
use chrono::Local;
use image::RgbaImage;
use serde::Serialize;
use std::ffi::OsString;
use std::io::Write;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::{
    SHFileOperationW, ShellExecuteW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
    FOF_WANTNUKEWARNING, FO_DELETE, SHFILEOPSTRUCTW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const IMAGE_EXTENSIONS: [&str; 3] = ["png", "jpg", "jpeg"];
/// Characters Windows rejects in file names, besides the path separators checked separately.
const INVALID_FILENAME_CHARS: [char; 7] = ['<', '>', ':', '"', '|', '?', '*'];
/// Leaves room for a `_NNN` collision suffix and the extension within the 255-character limit.
const MAX_STEM_CHARS: usize = 200;
/// Device names Windows reserves regardless of extension (`NUL.png` is `NUL`), per
/// "Naming Files, Paths, and Namespaces". Superscript digits count as digits for COM/LPT.
const RESERVED_DEVICE_NAMES: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];
const RESERVED_NUMBERED_DEVICES: [&str; 2] = ["COM", "LPT"];

/// Expands the strftime pattern and checks the result is a usable Windows file name.
fn render_stem<Tz>(pattern: &str, now: chrono::DateTime<Tz>) -> Result<String, String>
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let items: Vec<_> = StrftimeItems::new(pattern).collect();
    if items.iter().any(|item| matches!(item, Item::Error)) {
        return Err("The filename pattern contains an invalid date/time directive".to_owned());
    }
    // Formatted fallibly: some directives parse but cannot be formatted (`%#z`), and
    // `to_string()` would panic on them in the middle of a snip.
    let mut stem = String::new();
    std::fmt::Write::write_fmt(
        &mut stem,
        format_args!("{}", now.format_with_items(items.iter().cloned())),
    )
    .map_err(|_| {
        "The filename pattern contains a date/time directive that cannot be used".to_owned()
    })?;
    validate_stem(&stem)?;
    Ok(stem)
}

fn validate_stem(stem: &str) -> Result<(), String> {
    let rejected = |why: &str| Err(format!("The filename pattern produced \"{stem}\": {why}"));
    if stem.trim().is_empty() {
        return Err("The filename pattern produced an empty name".to_owned());
    }
    if stem.contains(['/', '\\']) {
        return Err("The filename pattern must not contain path separators".to_owned());
    }
    if stem.contains(INVALID_FILENAME_CHARS) || stem.chars().any(char::is_control) {
        return rejected("it contains characters Windows does not allow in file names");
    }
    if stem.ends_with(['.', ' ']) {
        return rejected("Windows drops a trailing dot or space from file names");
    }
    // Windows limits names in UTF-16 units, which some characters (emoji) take two of.
    if stem.encode_utf16().count() > MAX_STEM_CHARS {
        return rejected("the name is too long");
    }
    if is_reserved_device_name(stem) {
        return rejected("Windows reserves this name for a device");
    }
    Ok(())
}

fn is_reserved_device_name(stem: &str) -> bool {
    let base = stem.split('.').next().unwrap_or_default().trim_end();
    if RESERVED_DEVICE_NAMES
        .iter()
        .any(|name| base.eq_ignore_ascii_case(name))
    {
        return true;
    }
    let mut chars = base.chars();
    let prefix: String = chars.by_ref().take(3).collect();
    let rest: String = chars.collect();
    RESERVED_NUMBERED_DEVICES
        .iter()
        .any(|name| prefix.eq_ignore_ascii_case(name))
        && matches!(
            rest.as_str(),
            "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
        )
}

#[derive(Debug, Clone, Serialize)]
pub struct CaptureItem {
    pub path: String,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub timestamp: u64,
    pub thumbnail: String,
}

pub fn save(image: &RgbaImage, output: &OutputConfig) -> Result<PathBuf, String> {
    std::fs::create_dir_all(&output.directory).map_err(|error| {
        format!(
            "Failed to create output directory {}: {error}",
            output.directory.display()
        )
    })?;

    let stem = render_stem(&output.filename_pattern, Local::now())?;
    let bytes = imageops::encode(image, output.format, output.jpeg_quality)?;
    let extension = output.format.extension();
    let mut suffix = 1_u32;
    let (path, mut file) = loop {
        let name = if suffix == 1 {
            format!("{stem}.{extension}")
        } else {
            format!("{stem}_{suffix}.{extension}")
        };
        let path = output.directory.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => break (path, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix = suffix
                    .checked_add(1)
                    .ok_or_else(|| "Could not find an available output filename".to_owned())?;
            }
            Err(error) => return Err(format!("Failed to create {}: {error}", path.display())),
        }
    };
    if let Err(error) = file.write_all(&bytes) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(format!("Failed to save {}: {error}", path.display()));
    }
    drop(file);
    path.canonicalize()
        .map(strip_extended_length_prefix)
        .map_err(|error| format!("Failed to resolve saved path {}: {error}", path.display()))
}

fn strip_extended_length_prefix(path: PathBuf) -> PathBuf {
    const PREFIX: [u16; 4] = [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC_PREFIX: [u16; 8] = [
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ];

    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    let plain = if wide.starts_with(&UNC_PREFIX) {
        [b'\\' as u16, b'\\' as u16]
            .into_iter()
            .chain(wide[UNC_PREFIX.len()..].iter().copied())
            .collect()
    } else if wide.starts_with(&PREFIX) {
        wide[PREFIX.len()..].to_vec()
    } else {
        return path;
    };
    PathBuf::from(OsString::from_wide(&plain))
}

/// Canonical form used for comparisons: resolved when the path exists, without the `\\?\` prefix.
pub fn normalize_path(path: &Path) -> PathBuf {
    strip_extended_length_prefix(path.canonicalize().unwrap_or_else(|_| path.to_path_buf()))
}

/// Windows paths compare case-insensitively.
pub fn paths_match(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known))
        })
}

/// Validates a capture path received from a WebView: it must name an existing image file directly
/// inside the output directory. File commands never act on anything else, so a compromised page
/// cannot open, copy or delete arbitrary files.
pub fn resolve_capture(path: &Path, output_directory: &Path) -> Result<PathBuf, String> {
    let not_a_capture = || {
        Err(format!(
            "{} is not a capture in the output folder",
            path.display()
        ))
    };
    // Before touching the path at all, it must be on the output folder's drive or share: resolving
    // a path to another computer (a UNC path) would already make Windows contact it.
    let root = |path: &Path| match path.components().next() {
        Some(std::path::Component::Prefix(prefix)) => {
            Some(prefix.as_os_str().to_string_lossy().to_lowercase())
        }
        _ => None,
    };
    let output = strip_extended_length_prefix(output_directory.to_path_buf());
    let candidate = strip_extended_length_prefix(path.to_path_buf());
    if root(&candidate).is_none() || root(&candidate) != root(&output) {
        return not_a_capture();
    }
    let resolved = normalize_path(path);
    let inside_output = resolved
        .parent()
        .is_some_and(|parent| paths_match(parent, &normalize_path(output_directory)));
    if !inside_output || !is_image_path(&resolved) {
        return not_a_capture();
    }
    if !resolved.is_file() {
        return Err(format!("{} no longer exists", resolved.display()));
    }
    Ok(resolved)
}

pub fn capture_item(path: &Path, image: &RgbaImage, thumbnail: String) -> CaptureItem {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as u64);
    CaptureItem {
        path: path.to_string_lossy().into_owned(),
        name: path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
        width: image.width(),
        height: image.height(),
        timestamp,
        thumbnail,
    }
}

pub fn scan_recent_captures(directory: &Path, limit: usize) -> Vec<CaptureItem> {
    struct Candidate {
        path: PathBuf,
        modified: SystemTime,
    }

    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) => {
            log::warn!(
                "Could not scan recent captures in {}: {error}",
                directory.display()
            );
            return Vec::new();
        }
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                log::warn!(
                    "Could not read an entry while scanning {}: {error}",
                    directory.display()
                );
                continue;
            }
        };
        let path = entry.path();
        if !is_image_path(&path) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) => {
                log::warn!(
                    "Could not read capture metadata for {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        let modified = match metadata.modified() {
            Ok(modified) => modified,
            Err(error) => {
                log::warn!(
                    "Could not read capture timestamp for {}: {error}",
                    path.display()
                );
                continue;
            }
        };
        candidates.push(Candidate { path, modified });
    }
    candidates.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.path.cmp(&right.path))
    });

    let mut items = Vec::with_capacity(limit.min(candidates.len()));
    for candidate in candidates {
        if items.len() == limit {
            break;
        }
        let image = match image::open(&candidate.path) {
            Ok(image) => image.into_rgba8(),
            Err(error) => {
                log::warn!(
                    "Skipping unreadable capture {}: {error}",
                    candidate.path.display()
                );
                continue;
            }
        };
        let path = match candidate.path.canonicalize() {
            Ok(path) => strip_extended_length_prefix(path),
            Err(error) => {
                log::warn!(
                    "Skipping missing capture {}: {error}",
                    candidate.path.display()
                );
                continue;
            }
        };
        items.push(CaptureItem {
            path: path.to_string_lossy().into_owned(),
            name: path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned()),
            width: image.width(),
            height: image.height(),
            timestamp: candidate
                .modified
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis() as u64),
            thumbnail: imageops::thumbnail_data_url(&image),
        });
    }
    items
}

/// Moves a capture to the Recycle Bin, so a delete from the flyout or recents can be undone.
/// On volumes without a Recycle Bin (some network shares) Windows asks first.
pub fn delete(path: &Path) -> Result<(), String> {
    // pFrom is a list of paths, each NUL-terminated, ending with an extra NUL.
    let from: Vec<u16> = path.as_os_str().encode_wide().chain([0, 0]).collect();
    // FOF_WANTNUKEWARNING: where there is no Recycle Bin (some network folders) Windows asks
    // before deleting for good instead of doing it silently.
    let flags =
        FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI | FOF_WANTNUKEWARNING;
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: flags.0 as u16,
        ..Default::default()
    };
    // SAFETY: `operation` is fully initialised and `from` outlives the call; with the flags above
    // the shell shows no UI, so no window handle is needed.
    let status = unsafe { SHFileOperationW(&mut operation) };
    if status != 0 {
        return Err(format!(
            "Failed to move {} to the Recycle Bin (shell error {status:#x})",
            path.display()
        ));
    }
    if operation.fAnyOperationsAborted.as_bool() || path.exists() {
        return Err(format!("Deleting {} was cancelled", path.display()));
    }
    Ok(())
}

pub fn open(path: &Path) -> Result<(), String> {
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let path_wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(operation.as_ptr()),
            PCWSTR(path_wide.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!(
            "Windows could not open {} (ShellExecute error {})",
            path.display(),
            result.0 as isize
        ))
    }
}

pub fn reveal(path: &Path) -> Result<(), String> {
    spawn_explorer([std::ffi::OsStr::new("/select,"), path.as_os_str()])
}

pub fn open_directory(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path)
        .map_err(|error| format!("Failed to create {}: {error}", path.display()))?;
    spawn_explorer([path.as_os_str()])
}

fn spawn_explorer<'a>(args: impl IntoIterator<Item = &'a std::ffi::OsStr>) -> Result<(), String> {
    std::process::Command::new("explorer")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("Failed to launch Explorer: {error}"))
}

#[cfg(test)]
mod tests {
    use super::{
        render_stem, resolve_capture, scan_recent_captures, strip_extended_length_prefix,
        validate_stem,
    };
    use chrono::TimeZone;
    use image::{Rgba, RgbaImage};
    use std::fs::FileTimes;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, UNIX_EPOCH};

    static TEST_DIRECTORY_ID: AtomicU64 = AtomicU64::new(0);

    fn test_directory() -> PathBuf {
        let id = TEST_DIRECTORY_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("snippy-recents-{}-{id}", std::process::id()))
    }

    #[test]
    fn strips_extended_length_path_prefixes() {
        assert_eq!(
            strip_extended_length_prefix(PathBuf::from(r"\\?\C:\Pictures\Snippy\capture.png")),
            PathBuf::from(r"C:\Pictures\Snippy\capture.png")
        );
        assert_eq!(
            strip_extended_length_prefix(PathBuf::from(r"\\?\UNC\server\share\Snippy\capture.png")),
            PathBuf::from(r"\\server\share\Snippy\capture.png")
        );
    }

    #[test]
    fn capture_paths_must_be_images_inside_the_output_directory() {
        let directory = test_directory();
        std::fs::create_dir(&directory).unwrap_or_else(|error| panic!("{error}"));
        let image = directory.join("Capture.PNG");
        let text = directory.join("notes.txt");
        std::fs::write(&image, []).unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(&text, []).unwrap_or_else(|error| panic!("{error}"));
        let dotted = directory
            .join("..")
            .join(directory.file_name().unwrap_or_default())
            .join("Capture.PNG");

        assert!(resolve_capture(&image, &directory).is_ok());
        assert!(resolve_capture(&dotted, &directory).is_ok());
        assert!(resolve_capture(&text, &directory).is_err());
        assert!(resolve_capture(&directory.join("missing.png"), &directory).is_err());
        assert!(resolve_capture(&image, &std::env::temp_dir()).is_err());

        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn capture_paths_reject_traversal_streams_and_foreign_locations() {
        let directory = test_directory();
        let nested = directory.join("nested");
        std::fs::create_dir_all(&nested).unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(directory.join("outside.png"), []).unwrap_or_else(|error| panic!("{error}"));
        std::fs::write(nested.join("inner.png"), []).unwrap_or_else(|error| panic!("{error}"));

        // `..` that climbs out of the output folder resolves to a file that is not inside it.
        assert!(resolve_capture(&nested.join(r"..\outside.png"), &nested).is_err());
        // Files in subfolders are not captures of this folder either.
        assert!(resolve_capture(&nested.join("inner.png"), &directory).is_err());
        // An NTFS alternate data stream does not count as a PNG.
        assert!(resolve_capture(&nested.join("inner.png:evil.exe"), &nested).is_err());
        // `::$DATA` names the file's own default stream; it must resolve to that very file.
        let default_stream = resolve_capture(&nested.join("inner.png::$DATA"), &nested);
        assert_eq!(
            default_stream
                .as_deref()
                .ok()
                .and_then(|path| path.file_name()),
            Some(std::ffi::OsStr::new("inner.png"))
        );
        // UNC paths and system files are outside the output folder.
        assert!(resolve_capture(
            std::path::Path::new(r"\\127.0.0.1\c$\Windows\win.ini"),
            &nested
        )
        .is_err());
        assert!(resolve_capture(std::path::Path::new(r"C:\Windows\win.ini"), &nested).is_err());

        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }

    /// Touches the real Recycle Bin, so it only runs on request: `cargo test -- --ignored`.
    #[test]
    #[ignore = "moves a file into the user's Recycle Bin"]
    fn delete_moves_the_file_to_the_recycle_bin() {
        let directory = test_directory();
        std::fs::create_dir(&directory).unwrap_or_else(|error| panic!("{error}"));
        let path = directory.join("snippy-recycle-check.png");
        std::fs::write(&path, []).unwrap_or_else(|error| panic!("{error}"));

        super::delete(&path).unwrap_or_else(|error| panic!("{error}"));

        assert!(!path.exists());
        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }

    #[test]
    fn filename_patterns_render_with_the_capture_time() {
        let at = chrono::Utc
            .with_ymd_and_hms(2026, 11, 21, 9, 5, 7)
            .single()
            .unwrap_or_else(|| panic!("valid date"));
        assert_eq!(
            render_stem("Snippy_%Y%m%d_%H%M%S", at).as_deref(),
            Ok("Snippy_20261121_090507")
        );
        assert!(render_stem("%Q", at).is_err());
        // Parses, but cannot be formatted: an error, not a panic in the middle of a snip.
        assert!(render_stem("%#z", at).is_err());
        // 101 emoji are 202 UTF-16 units: too long for the name, though only 101 characters.
        assert!(render_stem(&"😀".repeat(101), at).is_err());
        assert!(render_stem("%Y/%m", at).is_err());
    }

    #[test]
    fn filename_stems_windows_cannot_store_are_rejected() {
        for bad in [
            "",
            "   ",
            "a\\b",
            "a/b",
            "a:b",
            "a*b",
            "a?b",
            "a|b",
            "a\"b",
            "a<b",
            "a>b",
            "tab\there",
            "trailing.",
            "trailing ",
            "CON",
            "con",
            "NUL.backup",
            "Aux ",
            "COM1",
            "lpt9",
            "COM¹",
        ] {
            assert!(validate_stem(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(validate_stem(&"x".repeat(201)).is_err());
        for good in [
            "Snippy_20261121",
            "CONSOLE",
            "COM10",
            "my.capture",
            "Скриншот 1",
            "LPT",
        ] {
            assert!(validate_stem(good).is_ok(), "{good:?} should be accepted");
        }
    }

    #[test]
    fn recent_scan_returns_eight_newest_images() {
        let directory = test_directory();
        std::fs::create_dir(&directory).unwrap_or_else(|error| panic!("{error}"));

        for index in 0..10_u64 {
            let path = directory.join(format!("capture-{index}.png"));
            RgbaImage::from_pixel(index as u32 + 1, 2, Rgba([index as u8, 0, 0, 255]))
                .save(&path)
                .unwrap_or_else(|error| panic!("{error}"));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .unwrap_or_else(|error| panic!("{error}"));
            file.set_times(
                FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(index * 1_000)),
            )
            .unwrap_or_else(|error| panic!("{error}"));
        }
        let corrupt_path = directory.join("corrupt.jpg");
        std::fs::write(&corrupt_path, b"not an image").unwrap_or_else(|error| panic!("{error}"));
        let corrupt_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&corrupt_path)
            .unwrap_or_else(|error| panic!("{error}"));
        corrupt_file
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_millis(20_000)))
            .unwrap_or_else(|error| panic!("{error}"));

        let items = scan_recent_captures(&directory, 8);

        assert_eq!(items.len(), 8);
        assert_eq!(items[0].name, "capture-9.png");
        assert_eq!(items[0].width, 10);
        assert_eq!(items[0].height, 2);
        assert_eq!(items[0].timestamp, 9_000);
        assert_eq!(items[7].name, "capture-2.png");
        assert!(items
            .iter()
            .all(|item| item.thumbnail.starts_with("data:image/png;base64,")));

        std::fs::remove_dir_all(&directory).unwrap_or_else(|error| panic!("{error}"));
    }
}
