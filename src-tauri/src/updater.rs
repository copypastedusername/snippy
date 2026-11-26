//! Update checks and self-update for the portable exe.
//!
//! Releases are published on the project's GitHub repository. A release carries `snippy.exe` and `snippy.exe.sig`, a minisign
//! signature made with the project's private key. Snippy only
//! installs an exe whose signature matches the public key below, so a compromised download link
//! cannot push arbitrary code.
//!
//! An update is offered when the latest release's exe differs from the running one: GitHub reports
//! each asset's SHA-256, which is compared with the running exe's. The version number does not
//! decide anything, so a fix re-published under the same version, or a release whose number went
//! back, still reaches everyone. When either hash is unknown (GitHub gave none, or the running exe
//! could not be read), only a newer version is offered.
//!
//! Installing swaps the files in place: Windows lets a running exe be renamed, so the old one moves
//! to `snippy.exe.old`, the new one takes its name, and the new process (started with
//! `--after-update=<pid>`) waits for this one to exit, then deletes the leftover.

use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{Emitter, Manager};
use windows::core::HSTRING;
use windows::Foundation::Uri;
use windows::Storage::Streams::DataReader;
use windows::Web::Http::HttpClient;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// `owner/name` of the GitHub repository whose releases are checked; overridable at build time.
const REPOSITORY: &str = match option_env!("SNIPPY_UPDATE_REPOSITORY") {
    Some(repository) => repository,
    None => "copypastedusername/snippy",
};
/// The minisign public key matching the private key that signs releases.
const PUBLIC_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IDM0NjRGMDNBQUQwMjREODcKUldTSFRRS3RPdkJrTkZBZ3BtdUE1WDFRamV4cjJwU3RSSjFEZTR5b1JIQk54R0VyNis3eXp6blcK";
const EXE_ASSET: &str = "snippy.exe";
const SIGNATURE_ASSET: &str = "snippy.exe.sig";
pub const AFTER_UPDATE_FLAG: &str = "--after-update=";
const MAX_DOWNLOAD_BYTES: u64 = 128 * 1024 * 1024;
const FIRST_CHECK_DELAY: Duration = Duration::from_secs(20);
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const PREVIOUS_INSTANCE_WAIT_MS: u32 = 15_000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Release {
    version: String,
    page: String,
    exe_url: String,
    signature_url: String,
    /// Lowercase hex SHA-256 of `snippy.exe` as GitHub reports it, when it does.
    exe_sha256: Option<String>,
    /// Same version as the running exe, different build.
    rebuild: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub available: bool,
    /// The offer is a new build of the version already installed.
    pub rebuild: bool,
    pub page: Option<String>,
}

/// Payload of the `update-available` event shown by the flyout.
#[derive(Debug, Clone, Serialize)]
pub struct UpdateNotice {
    pub version: String,
    pub current: String,
    pub rebuild: bool,
}

/// The latest release found by the last check, when its exe differs from this one.
static AVAILABLE: Mutex<Option<Release>> = Mutex::new(None);
/// The build (version and hash) already announced this session, so the periodic check does not nag.
static ANNOUNCED: Mutex<Option<String>> = Mutex::new(None);

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Checks in the background shortly after start, then every few hours, when the user allows it.
pub fn start_background_checks(app: &tauri::AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK_DELAY);
        loop {
            let enabled = app
                .state::<crate::AppState>()
                .config()
                .map(|config| config.behavior.check_updates)
                .unwrap_or(false);
            if enabled {
                match check() {
                    Ok(status) if status.available => announce_once(&app),
                    Ok(_) => {}
                    Err(error) => log::info!("Update check failed: {error}"),
                }
            }
            std::thread::sleep(CHECK_INTERVAL);
        }
    });
}

/// Asks GitHub for the latest release and remembers it when its exe differs from this one.
pub fn check() -> Result<UpdateStatus, String> {
    let url = format!("https://api.github.com/repos/{REPOSITORY}/releases/latest");
    let body = match http_get(&url, "application/vnd.github+json") {
        Ok(body) => body,
        // No published release yet (or the repository is not public).
        Err(HttpError::Status(404)) => {
            *lock(&AVAILABLE) = None;
            return Ok(status(None));
        }
        Err(error) => return Err(error.to_string()),
    };
    let release = parse_release(&body)?;
    let offered = release.as_ref().and_then(|release| {
        let rebuild = offer(
            &release.version,
            release.exe_sha256.as_deref(),
            current_version(),
            own_sha256(),
        )?;
        Some(Release {
            rebuild,
            ..release.clone()
        })
    });
    *lock(&AVAILABLE) = offered.clone();
    Ok(UpdateStatus {
        latest: release
            .as_ref()
            .map(|release| display_version(&release.version)),
        ..status(offered.as_ref())
    })
}

fn status(offered: Option<&Release>) -> UpdateStatus {
    UpdateStatus {
        current: display_version(current_version()),
        latest: offered.map(|release| display_version(&release.version)),
        available: offered.is_some(),
        rebuild: offered.is_some_and(|release| release.rebuild),
        page: offered.map(|release| release.page.clone()),
    }
}

/// Offers the update once per version and session, the same way finished snips are announced.
pub fn announce_once(app: &tauri::AppHandle) {
    let Some(release) = lock(&AVAILABLE).clone() else {
        return;
    };
    {
        let build = format!(
            "{}#{}",
            release.version,
            release.exe_sha256.as_deref().unwrap_or_default()
        );
        let mut announced = lock(&ANNOUNCED);
        if announced.as_deref() == Some(build.as_str()) {
            return;
        }
        *announced = Some(build);
    }
    let windows_notifications = app
        .state::<crate::AppState>()
        .config()
        .map(|config| config.behavior.windows_notifications)
        .unwrap_or(false);
    if windows_notifications {
        match crate::notify::show_update(&display_version(&release.version), release.rebuild) {
            Ok(()) => return,
            Err(error) => log::warn!("Windows notification failed, using the flyout: {error}"),
        }
    }
    let notice = UpdateNotice {
        version: display_version(&release.version),
        current: display_version(current_version()),
        rebuild: release.rebuild,
    };
    if let Err(error) = app.emit("update-available", &notice) {
        log::warn!("Could not announce the update: {error}");
        return;
    }
    if let Err(error) = crate::overlay::show_flyout(app) {
        log::warn!("Could not show the update flyout: {error}");
    }
}

/// Downloads, verifies and installs the newest release, then restarts Snippy on it.
/// Returns only on failure; on success the app exits.
pub fn install(app: &tauri::AppHandle) -> Result<(), String> {
    let installing = Installing::begin().ok_or(ALREADY_INSTALLING)?;
    // Bound first: a guard in the `match` scrutinee would stay locked while `check` relocks it.
    let cached = lock(&AVAILABLE).clone();
    let release = match cached {
        Some(release) => release,
        None => {
            check()?;
            lock(&AVAILABLE)
                .clone()
                .ok_or_else(|| "Snippy is already up to date.".to_owned())?
        }
    };
    let exe = http_get(&release.exe_url, "application/octet-stream").map_err(|e| e.to_string())?;
    let signature =
        http_get(&release.signature_url, "application/octet-stream").map_err(|e| e.to_string())?;
    verify(&exe, &signature)?;
    let downloaded = sha256_hex(&exe);
    if release
        .exe_sha256
        .as_deref()
        .is_some_and(|expected| expected != downloaded)
    {
        return Err(
            "The downloaded update does not match the release; it was not installed.".to_owned(),
        );
    }
    if own_sha256() == Some(downloaded.as_str()) {
        return Err("Snippy is already up to date.".to_owned());
    }
    if !exe.starts_with(b"MZ") {
        return Err("The downloaded update is not a Windows program.".to_owned());
    }

    let current =
        std::env::current_exe().map_err(|error| format!("Could not find Snippy's exe: {error}"))?;
    let expected = parse_version(&release.version).ok_or("The release has no valid version.")?;
    swap_in(&current, &exe, |staged| {
        check_staged_version(staged, expected)
    })?;
    // From here the new exe is in place: another install would only fail on the swapped files, so
    // the guard stays set until this process exits, whether or not the restart works.
    installing.keep();
    log::info!("Installed Snippy {}; restarting", release.version);
    std::process::Command::new(&current)
        .arg(format!("{AFTER_UPDATE_FLAG}{}", std::process::id()))
        .spawn()
        .map_err(|error| {
            format!(
                "The update is installed, but Snippy could not restart ({error}). Quit Snippy \
                 and start it again to use the new version."
            )
        })?;
    app.exit(0);
    Ok(())
}

/// The error `install` returns while another install is under way.
pub const ALREADY_INSTALLING: &str = "An update is already being installed.";

static INSTALLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// One install at a time; released when an install fails before the exe is swapped.
struct Installing {
    release: bool,
}

impl Installing {
    fn begin() -> Option<Self> {
        use std::sync::atomic::Ordering;
        INSTALLING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| Self { release: true })
    }

    fn keep(mut self) {
        self.release = false;
    }
}

impl Drop for Installing {
    fn drop(&mut self) {
        if self.release {
            INSTALLING.store(false, std::sync::atomic::Ordering::Release);
        }
    }
}

/// Checks the downloaded exe against the release signature and the embedded public key.
fn verify(exe: &[u8], signature_file: &[u8]) -> Result<(), String> {
    use base64::Engine;
    let decode = |value: &[u8]| {
        let text = std::str::from_utf8(value).ok()?.trim();
        base64::engine::general_purpose::STANDARD
            .decode(text)
            .ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
    };
    let key = decode(PUBLIC_KEY.as_bytes())
        .and_then(|key| minisign_verify::PublicKey::decode(&key).ok())
        .ok_or("The update key built into Snippy is invalid.")?;
    let signature = decode(signature_file)
        .and_then(|signature| minisign_verify::Signature::decode(&signature).ok())
        .ok_or("The update's signature is unreadable.")?;
    key.verify(exe, &signature, true)
        .map_err(|_| "The update's signature does not match; it was not installed.".to_owned())
}

/// The signature proves the exe is ours, not which release it belongs to: its own version must be
/// the one the release names. Older numbers are allowed on purpose (the latest release decides).
fn check_staged_version(staged: &Path, expected: (u64, u64, u64)) -> Result<(), String> {
    ensure_version(file_version(staged), expected)
}

fn ensure_version(found: Option<(u64, u64, u64)>, expected: (u64, u64, u64)) -> Result<(), String> {
    let found = found.ok_or("The update carries no version information.")?;
    if found != expected {
        return Err(format!(
            "The downloaded exe is version {}.{}.{}, not the {}.{}.{} that was offered.",
            found.0, found.1, found.2, expected.0, expected.1, expected.2
        ));
    }
    Ok(())
}

/// Reads the `FileVersion` an exe declares in its version resource.
fn file_version(path: &Path) -> Option<(u64, u64, u64)> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{w, PCWSTR};
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: the buffer is sized by GetFileVersionInfoSizeW, and VerQueryValueW returns a pointer
    // into it that is read before the buffer is dropped.
    unsafe {
        let size = GetFileVersionInfoSizeW(PCWSTR(wide.as_ptr()), None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0_u8; size as usize];
        GetFileVersionInfoW(PCWSTR(wide.as_ptr()), 0, size, data.as_mut_ptr().cast()).ok()?;
        let mut info: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut length = 0_u32;
        if !VerQueryValueW(data.as_ptr().cast(), w!("\\"), &mut info, &mut length).as_bool()
            || info.is_null()
            || (length as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = &*(info as *const VS_FIXEDFILEINFO);
        Some((
            u64::from(fixed.dwFileVersionMS >> 16),
            u64::from(fixed.dwFileVersionMS & 0xFFFF),
            u64::from(fixed.dwFileVersionLS >> 16),
        ))
    }
}

/// Replaces `current` with `bytes`, keeping the old exe as `.old` until the next start. `check`
/// inspects the staged file before anything is moved.
fn swap_in(
    current: &Path,
    bytes: &[u8],
    check: impl FnOnce(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let staged = sibling(current, "new");
    let old = sibling(current, "old");
    std::fs::write(&staged, bytes).map_err(|error| {
        format!(
            "Snippy cannot write next to itself in {} ({error}). Download the update from the release page instead.",
            current.parent().unwrap_or(current).display()
        )
    })?;
    if let Err(error) = check(&staged) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    let _ = std::fs::remove_file(&old);
    if let Err(error) = std::fs::rename(current, &old) {
        let _ = std::fs::remove_file(&staged);
        return Err(format!("Could not move the running exe aside: {error}"));
    }
    if let Err(error) = std::fs::rename(&staged, current) {
        let _ = std::fs::rename(&old, current);
        let _ = std::fs::remove_file(&staged);
        return Err(format!("Could not put the new exe in place: {error}"));
    }
    Ok(())
}

fn sibling(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{suffix}"));
    exe.with_file_name(name)
}

/// The process that installed this exe and restarted on it (`--after-update=<pid>`), if any.
pub fn predecessor(args: &[String]) -> Option<u32> {
    args.iter()
        .find_map(|arg| arg.strip_prefix(AFTER_UPDATE_FLAG))
        .and_then(|pid| pid.parse::<u32>().ok())
}

/// Run first thing at start: after an update, gives the previous instance time to exit (it holds
/// the single-instance lock). `instances::claim` never hands this launch to it, however long it
/// takes.
pub fn wait_for_predecessor(predecessor: Option<u32>) {
    if let Some(pid) = predecessor {
        wait_for_process(pid);
    }
}

/// Removes the exe a previous update left behind; run once this copy owns the session.
pub fn remove_leftover() {
    if let Ok(current) = std::env::current_exe() {
        let old = sibling(&current, "old");
        if old.exists() {
            let _ = std::fs::remove_file(old);
        }
    }
}

fn wait_for_process(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };
    // SAFETY: the handle is only waited on and closed here.
    unsafe {
        if let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(process, PREVIOUS_INSTANCE_WAIT_MS);
            let _ = CloseHandle(process);
        }
    }
}

#[derive(Debug)]
enum HttpError {
    Status(i32),
    Other(String),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Status(code) => write!(f, "the server answered HTTP {code}"),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl From<windows::core::Error> for HttpError {
    fn from(error: windows::core::Error) -> Self {
        Self::Other(format!("network error: {}", error.message()))
    }
}

/// A blocking HTTPS GET through Windows' own HTTP stack (system proxy and certificates).
fn http_get(url: &str, accept: &str) -> Result<Vec<u8>, HttpError> {
    // SAFETY: joins the calling worker thread to the multithreaded apartment; repeated calls are
    // harmless (they report the apartment already exists).
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let client = HttpClient::new()?;
    let headers = client.DefaultRequestHeaders()?;
    headers.TryAppendWithoutValidation(
        &HSTRING::from("User-Agent"),
        &HSTRING::from(format!("Snippy/{}", current_version())),
    )?;
    headers.TryAppendWithoutValidation(&HSTRING::from("Accept"), &HSTRING::from(accept))?;
    let response = client
        .GetAsync(&Uri::CreateUri(&HSTRING::from(url))?)?
        .get()?;
    if !response.IsSuccessStatusCode()? {
        return Err(HttpError::Status(response.StatusCode()?.0));
    }
    let content = response.Content()?;
    if let Ok(length) = content
        .Headers()
        .and_then(|headers| headers.ContentLength())
    {
        if length
            .Value()
            .is_ok_and(|length| length > MAX_DOWNLOAD_BYTES)
        {
            return Err(HttpError::Other(
                "the download is unexpectedly large".to_owned(),
            ));
        }
    }
    let buffer = content.ReadAsBufferAsync()?.get()?;
    let length = buffer.Length()?;
    if u64::from(length) > MAX_DOWNLOAD_BYTES {
        return Err(HttpError::Other(
            "the download is unexpectedly large".to_owned(),
        ));
    }
    let mut bytes = vec![0_u8; length as usize];
    DataReader::FromBuffer(&buffer)?.ReadBytes(&mut bytes)?;
    Ok(bytes)
}

/// Reads GitHub's "latest release" JSON. A release without both assets is not installable.
fn parse_release(body: &[u8]) -> Result<Option<Release>, String> {
    let json: serde_json::Value = serde_json::from_slice(body)
        .map_err(|error| format!("Unreadable release data: {error}"))?;
    let Some(tag) = json["tag_name"].as_str() else {
        return Ok(None);
    };
    let version = tag.trim_start_matches(['v', 'V']).to_owned();
    if parse_version(&version).is_none() {
        return Ok(None);
    }
    let asset = |name: &str| {
        json["assets"]
            .as_array()?
            .iter()
            .find(|asset| asset["name"].as_str() == Some(name))?["browser_download_url"]
            .as_str()
            .filter(|url| url.starts_with("https://"))
            .map(str::to_owned)
    };
    let (Some(exe_url), Some(signature_url)) = (asset(EXE_ASSET), asset(SIGNATURE_ASSET)) else {
        return Ok(None);
    };
    let exe_sha256 = json["assets"].as_array().and_then(|assets| {
        let digest = assets
            .iter()
            .find(|asset| asset["name"].as_str() == Some(EXE_ASSET))?["digest"]
            .as_str()?
            .strip_prefix("sha256:")?
            .to_ascii_lowercase();
        (digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(digest)
    });
    Ok(Some(Release {
        version,
        page: json["html_url"].as_str().unwrap_or_default().to_owned(),
        exe_url,
        signature_url,
        exe_sha256,
        rebuild: false,
    }))
}

/// Whether the latest release should be offered, and if so whether it is a rebuild (`true`: same
/// version, different exe). Any exe other than the running one is offered, whatever its version;
/// without both hashes only a newer version is.
fn offer(
    release_version: &str,
    release_sha256: Option<&str>,
    current_version: &str,
    own_sha256: Option<&str>,
) -> Option<bool> {
    let same_version = parse_version(release_version).is_some()
        && parse_version(release_version) == parse_version(current_version);
    match (release_sha256, own_sha256) {
        (Some(release), Some(own)) => (release != own).then_some(same_version),
        _ => is_newer(release_version, current_version).then_some(false),
    }
}

/// SHA-256 of the running exe, computed once.
fn own_sha256() -> Option<&'static str> {
    static OWN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    OWN.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        std::fs::read(exe).ok().map(|bytes| sha256_hex(&bytes))
    })
    .as_deref()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `major.minor[.patch]` (a missing patch is 0), ignoring any `-pre` or `+build` suffix.
fn parse_version(version: &str) -> Option<(u64, u64, u64)> {
    let core = version.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|part| part.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = match parts.next() {
        Some(patch) => patch?,
        None => 0,
    };
    parts.next().is_none().then_some((major, minor, patch))
}

/// How versions are shown: "0.1" for 0.1.0, "0.1.2" otherwise.
pub fn display_version(version: &str) -> String {
    match parse_version(version) {
        Some((major, minor, 0)) => format!("{major}.{minor}"),
        Some((major, minor, patch)) => format!("{major}.{minor}.{patch}"),
        None => version.to_owned(),
    }
}

fn is_newer(candidate: &str, current: &str) -> bool {
    matches!(
        (parse_version(candidate), parse_version(current)),
        (Some(candidate), Some(current)) if candidate > current
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn any_other_exe_is_offered_whatever_its_version() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        assert_eq!(offer("0.2", Some(&a), "0.1.0", Some(&b)), Some(false));
        assert_eq!(offer("0.1", Some(&a), "0.1.0", Some(&b)), Some(true));
        // A lower number is still offered when its exe differs.
        assert_eq!(offer("0.1.1", Some(&a), "0.1.4", Some(&b)), Some(false));
        // The same exe is never offered, whatever the number says.
        assert_eq!(offer("0.1", Some(&a), "0.1.0", Some(&a)), None);
        assert_eq!(offer("0.2", Some(&a), "0.1.0", Some(&a)), None);
        // Without both hashes only a newer version is offered.
        assert_eq!(offer("0.2", None, "0.1.0", Some(&a)), Some(false));
        assert_eq!(offer("0.1", None, "0.1.0", Some(&a)), None);
        assert_eq!(offer("0.0.9", None, "0.1.0", None), None);
    }

    #[test]
    fn a_missing_hash_on_either_side_falls_back_to_the_version() {
        let a = "a".repeat(64);
        assert_eq!(offer("0.2", Some(&a), "0.1.0", None), Some(false));
        assert_eq!(offer("0.1", Some(&a), "0.1.0", None), None);
        assert_eq!(offer("0.0.9", None, "0.1.0", Some(&a)), None);
        assert_eq!(offer("0.1", None, "0.1.0", None), None);
    }

    #[test]
    fn the_installed_exe_must_carry_the_offered_version_even_when_lower() {
        assert!(ensure_version(Some((0, 1, 1)), (0, 1, 1)).is_ok());
        assert!(ensure_version(Some((0, 1, 4)), (0, 1, 1)).is_err());
        assert!(ensure_version(None, (0, 1, 1)).is_err());
    }

    #[test]
    fn hashes_are_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("garbage", "0.1.0"));
        assert_eq!(parse_version("1.2.3-beta+5"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("1"), None);
        assert!(is_newer("0.1", "0.0.9"));
        assert!(!is_newer("0.1", "0.1.0"));
        assert_eq!(display_version("0.1.0"), "0.1");
        assert_eq!(display_version("0.1.2"), "0.1.2");
        assert_eq!(parse_version("1.2.3.4"), None);
    }

    #[test]
    fn releases_need_both_assets_over_https() {
        let json = |exe: &str| {
            format!(
                r#"{{"tag_name":"v0.2.0","html_url":"https://github.com/o/r/releases/tag/v0.2.0",
                "assets":[{{"name":"snippy.exe","browser_download_url":"{exe}","digest":"sha256:ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789"}},
                          {{"name":"snippy.exe.sig","browser_download_url":"https://x/snippy.exe.sig"}}]}}"#
            )
        };
        let release = parse_release(json("https://x/snippy.exe").as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(release.version, "0.2.0");
        assert_eq!(release.exe_url, "https://x/snippy.exe");
        assert_eq!(
            release.exe_sha256.as_deref(),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789")
        );
        assert_eq!(
            parse_release(json("http://x/snippy.exe").as_bytes()).unwrap(),
            None
        );
        assert_eq!(
            parse_release(br#"{"tag_name":"v0.2.0","assets":[]}"#).unwrap(),
            None
        );
        assert_eq!(parse_release(br#"{"message":"Not Found"}"#).unwrap(), None);
        assert!(parse_release(b"<html>").is_err());
    }

    #[test]
    fn a_file_signed_with_the_release_key_passes_and_any_change_fails() {
        // `tauri signer sign` output for a five-byte file containing "hello".
        let signature = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTSFRRS3RPdkJrTk5iK0F0dUxkMi9NVk40Vm1OZlNYcjBOd2QvdTdPaHhoV096ZEphOGNFOUo0cHdtNndGeWw4ZVczRkRWM1hiWWtNdGc3dXVhU29SZWxQQTE5amJ3a0EwPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk1MjYxMDIyCWZpbGU6c2lndGVzdC5iaW4KRDVMcTUvWkpPMmNwOGZtMlo2anZSMmJSVEVwMVVxS29XZlRvOGVvUlE3aEpOd21MRm5WcWFCZy9yYkNhWkNrNC80RWk1MGJEK01yUDFKYURpM2sxRGc9PQo=";
        assert_eq!(verify(b"hello", signature.as_bytes()), Ok(()));
        assert!(verify(b"hellO", signature.as_bytes()).is_err());
    }

    #[test]
    fn only_files_signed_with_the_release_key_pass() {
        // A signature made by a different key must be rejected, as must garbage.
        assert!(verify(b"MZ...", b"not base64").is_err());
        let foreign = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUY3F6Nk5vQUc0dk1SYTAxZkdKbm9VUjBPQzFWMnZmbXRzVWZ6Q0JnN3lQRFRyM2JUemV2NzA3WnZsRWtUbzlNdkNDVk1CczRFYzFIRmkyR2pRMEp1clRmRXJLUXVKTVFZPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzAwMDAwMDAwCWZpbGU6eApMSENLRmV4cEpWZ0lNdmJQQ2h5czVCZm1FV0IwVk5lNnQzbzBQSGNhWExsaGpQZnNPQVZLSlFCSHN6S1JjZ2xSaTRaeUZMTlJ0Q05nTDZyL2t5bUhCQT09Cg==";
        assert!(verify(b"MZ...", foreign.as_bytes()).is_err());
    }

    #[test]
    #[ignore = "talks to api.github.com"]
    fn github_is_reachable_through_the_windows_http_stack() {
        let body = http_get(
            "https://api.github.com/repos/tauri-apps/tauri/releases/latest",
            "application/vnd.github+json",
        )
        .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["tag_name"].as_str().is_some());
        // Snippy's own repository answers too (404 while it has no public release).
        let status = check().unwrap();
        assert_eq!(status.current, display_version(current_version()));
    }

    #[test]
    fn the_swap_keeps_the_old_exe_until_the_next_start() {
        let dir = std::env::temp_dir().join(format!("snippy-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("snippy.exe");
        std::fs::write(&exe, b"old").unwrap();
        assert!(swap_in(&exe, b"bad", |_| Err("refused".to_owned())).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert!(!sibling(&exe, "new").exists());
        swap_in(&exe, b"new", |_| Ok(())).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new");
        assert_eq!(std::fs::read(sibling(&exe, "old")).unwrap(), b"old");
        assert!(!sibling(&exe, "new").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
