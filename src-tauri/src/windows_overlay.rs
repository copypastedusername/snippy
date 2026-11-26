//! Windows 10's own screen-clipping overlay, turned off and back on at the user's request.
//!
//! On Windows 10, Explorer keeps Win+Shift+S as its own hotkey and opens
//! `MicrosoftWindows.Client.CBS_cw5n1h2txyewy!ScreenClipping` (ScreenClippingHost.exe) for it.
//! Snippy takes the chord with a keyboard hook (`keyhook.rs`); in the rare case the hook misses a
//! press, Windows' overlay appears on top of Snippy's. Turning the overlay off renames that one
//! executable to `ScreenClippingHost.exe.snippy-off`, so a missed press opens nothing instead.
//!
//! The change needs administrator rights, so Snippy runs itself elevated with exactly
//! `--windows-overlay disable|restore` (handled before anything else starts) and waits. The helper
//! refuses to work unless it is elevated on Windows 10, and only ever touches the host file and its
//! backup under the system's Windows directory:
//!
//! - First it renames the file with the backup and restore privileges, opened with backup
//!   semantics and never over an existing file. The file keeps its owner (TrustedInstaller) and
//!   access list untouched.
//! - Only if Windows refuses that, the owner and access list are kept in
//!   `HKLM\Software\Snippy\WindowsOverlay` (only administrators can write there), Administrators
//!   take the file just long enough to rename it, and the kept owner and access list go back on it
//!   right after (also if the rename fails). A later run repairs a file left owned by
//!   Administrators before reporting success.
//!
//! A Windows update may bring the overlay back; the status then says so and nothing is redone on
//! its own.

use crate::winreg;
use std::path::{Path, PathBuf};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, LocalFree, BOOL, HANDLE, HLOCAL, LUID};
use windows::Win32::Foundation::{GetLastError, ERROR_NOT_ALL_ASSIGNED};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetNamedSecurityInfoW,
    SetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, GetSecurityDescriptorControl, GetSecurityDescriptorDacl,
    GetSecurityDescriptorOwner, LookupPrivilegeValueW, ACL, DACL_SECURITY_INFORMATION,
    LUID_AND_ATTRIBUTES, OBJECT_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED,
    SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FileDispositionInfo, FileRenameInfo, MoveFileExW, SetFileInformationByHandle,
    DELETE, FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_RENAME_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, MOVE_FILE_FLAGS,
    OPEN_EXISTING,
};
use windows::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

pub const HELPER_FLAG: &str = "--windows-overlay";
const PACKAGE_DIR: &str = r"SystemApps\MicrosoftWindows.Client.CBS_cw5n1h2txyewy";
const HOST: &str = "ScreenClippingHost.exe";
/// The same name the local test script used, so a host turned off by it is recognised.
const BACKUP: &str = "ScreenClippingHost.exe.snippy-off";
const KEPT_KEY: &str = r"Software\Snippy\WindowsOverlay";
const KEPT_VALUE: &str = "HostSecurity";
const TRUSTED_INSTALLER_SID: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";
/// While the file is renamed: owned by Administrators, full control for them and the system.
const TAKEN: &str = "O:BAD:P(A;;FA;;;BA)(A;;FA;;;SY)";

/// Helper exit codes.
pub const DONE: i32 = 0;
pub const UNSUPPORTED: i32 = 10;
pub const NOT_ELEVATED: i32 = 11;
pub const ACCESS: i32 = 12;
pub const CONFLICT: i32 = 13;
pub const FAILED: i32 = 14;

/// Where the overlay stands, for the settings page.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayStatus {
    /// Not Windows 10, or the host is not where Windows 10 keeps it.
    Unsupported,
    /// Windows' overlay is in place.
    Present,
    /// Turned off by Snippy.
    Disabled,
    /// Turned off once, but Windows (an update) put the overlay back.
    RestoredByWindows,
}

struct Paths {
    host: PathBuf,
    backup: PathBuf,
}

fn windows_dir() -> Option<PathBuf> {
    let mut buffer = [0_u16; 260];
    // SAFETY: the buffer and its length match; the call writes at most that many characters.
    let length = unsafe { GetSystemWindowsDirectoryW(Some(&mut buffer)) } as usize;
    (length > 0 && length < buffer.len())
        .then(|| PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

fn paths() -> Option<Paths> {
    let directory = windows_dir()?.join(PACKAGE_DIR);
    Some(Paths {
        host: directory.join(HOST),
        backup: directory.join(BACKUP),
    })
}

/// A plain file, not a link or a reparse point that could lead somewhere else.
fn plain_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.is_file() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    })
}

/// The package folder is a real folder (not a link elsewhere).
fn plain_dir(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    })
}

fn windows_10() -> bool {
    crate::screenclip::before_windows_11(crate::screenclip::windows_build())
}

pub fn status() -> OverlayStatus {
    if !windows_10() {
        return OverlayStatus::Unsupported;
    }
    let Some(paths) = paths() else {
        return OverlayStatus::Unsupported;
    };
    status_of(plain_file(&paths.host), plain_file(&paths.backup))
}

fn status_of(host: bool, backup: bool) -> OverlayStatus {
    match (host, backup) {
        (true, false) => OverlayStatus::Present,
        (false, true) => OverlayStatus::Disabled,
        (true, true) => OverlayStatus::RestoredByWindows,
        (false, false) => OverlayStatus::Unsupported,
    }
}

/// Turns the overlay off (`disable`) or back on, elevating through UAC when needed.
pub fn set_disabled(disabled: bool) -> Result<OverlayStatus, String> {
    if status() == OverlayStatus::Unsupported {
        return Err("Only Windows 10 has this overlay to turn off".to_owned());
    }
    let action = if disabled { "disable" } else { "restore" };
    let exe = std::env::current_exe().map_err(|error| format!("Could not find Snippy: {error}"))?;
    // Always a separate, short-lived process: the privileges it turns on never stay on in Snippy.
    let code = if crate::elevation::is_elevated()? {
        std::process::Command::new(&exe)
            .args([HELPER_FLAG, action])
            .status()
            .map_err(|error| format!("Could not start the helper: {error}"))?
            .code()
            .unwrap_or(FAILED)
    } else {
        crate::elevation::run_elevated_for_code(&exe.to_string_lossy(), &[HELPER_FLAG, action])?
    };
    match code {
        DONE => Ok(status()),
        UNSUPPORTED => Err("Windows' screen-clipping overlay is not where Windows 10 keeps it".to_owned()),
        NOT_ELEVATED | ACCESS => {
            Err("Windows did not allow the change to its screen-clipping overlay".to_owned())
        }
        CONFLICT => Err(
            "Windows' screen-clipping overlay is open or changed meanwhile. Close it and try again."
                .to_owned(),
        ),
        other => Err(format!(
            "Could not change Windows' screen-clipping overlay (code {other})"
        )),
    }
}

/// The elevated helper, for exactly `disable` or `restore`. Returns the process exit code.
pub fn run_helper(action: &str) -> i32 {
    if !windows_10() {
        return UNSUPPORTED;
    }
    if !crate::elevation::is_elevated().unwrap_or(false) {
        return NOT_ELEVATED;
    }
    let Some(paths) = paths() else {
        return UNSUPPORTED;
    };
    if !paths.host.parent().is_some_and(plain_dir) {
        return UNSUPPORTED;
    }
    // One helper at a time, across sessions: a second one waits for the first.
    let _only_one = match HelperLock::take() {
        Some(lock) => lock,
        None => return CONFLICT,
    };
    if enable_privileges().is_err() {
        return ACCESS;
    }
    let outcome = match action {
        "disable" => disable(&paths),
        "restore" => restore(&paths),
        _ => Err(FAILED),
    };
    outcome.err().unwrap_or(DONE)
}

/// A machine-wide mutex held while a helper changes the files.
struct HelperLock(HANDLE);

impl HelperLock {
    fn take() -> Option<Self> {
        // SAFETY: the name is a NUL-terminated literal; the handle is released and closed in Drop.
        unsafe {
            let mutex = windows::Win32::System::Threading::CreateMutexW(
                None,
                false,
                windows::core::w!("Global\\SnippyWindowsOverlay"),
            )
            .ok()?;
            let waited = windows::Win32::System::Threading::WaitForSingleObject(mutex, 120_000);
            if waited == windows::Win32::Foundation::WAIT_OBJECT_0
                || waited == windows::Win32::Foundation::WAIT_ABANDONED
            {
                Some(Self(mutex))
            } else {
                let _ = CloseHandle(mutex);
                None
            }
        }
    }
}

impl Drop for HelperLock {
    fn drop(&mut self) {
        // SAFETY: this thread owns the mutex; it is released and closed once.
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

fn wide(value: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    value
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Lets this (elevated) process take ownership and set any owner, whatever the file's access list.
fn enable_privileges() -> Result<(), i32> {
    let mut token = HANDLE::default();
    // SAFETY: the token handle is closed below; the privilege names are NUL-terminated literals.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|_| ACCESS)?;
        let mut result = Ok(());
        for name in [
            windows::core::w!("SeTakeOwnershipPrivilege"),
            windows::core::w!("SeRestorePrivilege"),
            windows::core::w!("SeBackupPrivilege"),
        ] {
            let mut luid = LUID::default();
            if LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid).is_err() {
                result = Err(ACCESS);
                continue;
            }
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            // Success can still mean "not assigned": the token does not hold the privilege.
            if AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None).is_err()
                || GetLastError() == ERROR_NOT_ALL_ASSIGNED
            {
                result = Err(ACCESS);
            }
        }
        let _ = CloseHandle(token);
        result
    }
}

/// The file's owner and access list as an SDDL string.
fn read_security(path: &Path) -> Result<String, i32> {
    let name = wide(path);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the name is NUL-terminated; the descriptor and the string are freed with LocalFree.
    unsafe {
        let status = GetNamedSecurityInfoW(
            PCWSTR(name.as_ptr()),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            None,
            None,
            None,
            None,
            &mut descriptor,
        );
        if status.is_err() {
            return Err(ACCESS);
        }
        let mut text = PWSTR::null();
        let converted = ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut text,
            None,
        );
        let _ = LocalFree(HLOCAL(descriptor.0));
        converted.map_err(|_| FAILED)?;
        let sddl = text.to_string().map_err(|_| FAILED);
        let _ = LocalFree(HLOCAL(text.0.cast()));
        sddl
    }
}

/// Puts the owner and access list of `sddl` on the file.
fn write_security(path: &Path, sddl: &str) -> Result<(), i32> {
    let name = wide(path);
    let text: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: both strings are NUL-terminated; the owner and access list point into `descriptor`,
    // which lives until LocalFree after the call.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(text.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|_| FAILED)?;
        let mut owner = PSID::default();
        let mut defaulted = BOOL::default();
        let mut present = BOOL::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut control = 0_u16;
        let mut revision = 0_u32;
        let read = GetSecurityDescriptorOwner(descriptor, &mut owner, &mut defaulted)
            .and_then(|()| {
                GetSecurityDescriptorDacl(descriptor, &mut present, &mut dacl, &mut defaulted)
            })
            .and_then(|()| GetSecurityDescriptorControl(descriptor, &mut control, &mut revision));
        let result = if read.is_err() || owner.is_invalid() || !present.as_bool() {
            Err(FAILED)
        } else {
            let protection: OBJECT_SECURITY_INFORMATION = if control & SE_DACL_PROTECTED.0 != 0 {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
            let status = SetNamedSecurityInfoW(
                PCWSTR(name.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | protection,
                owner,
                PSID::default(),
                Some(dacl),
                None,
            );
            if status.is_ok() {
                Ok(())
            } else {
                Err(ACCESS)
            }
        };
        let _ = LocalFree(HLOCAL(descriptor.0));
        result
    }
}

/// Renames with the backup and restore privileges, which let an administrator move a system file
/// without changing who owns it or its access list. Never replaces a file already at `to`.
fn rename_keeping_security(from: &Path, to: &Path) -> Result<(), i32> {
    let source = wide(from);
    let target: Vec<u16> = wide(to);
    let name_units = target.len() - 1; // without the NUL
                                       // FILE_RENAME_INFO ends in a one-unit name array; the full name follows in the same buffer.
    let size = std::mem::size_of::<FILE_RENAME_INFO>() + name_units * 2;
    let mut buffer = vec![0_u64; size.div_ceil(8)];
    // SAFETY: the buffer is large and aligned enough for the header plus the name; the handle is
    // closed below; the name is copied within the buffer's bounds.
    unsafe {
        let file = CreateFileW(
            PCWSTR(source.as_ptr()),
            DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .map_err(open_error)?;
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
        (*info).Anonymous.Flags = 0; // do not replace
        (*info).RootDirectory = HANDLE::default();
        (*info).FileNameLength = (name_units * 2) as u32;
        std::ptr::copy_nonoverlapping(
            target.as_ptr(),
            std::ptr::addr_of_mut!((*info).FileName).cast::<u16>(),
            name_units,
        );
        let renamed = SetFileInformationByHandle(file, FileRenameInfo, info.cast(), size as u32);
        let _ = CloseHandle(file);
        renamed.map_err(|error| {
            if error.code() == windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult() {
                ACCESS
            } else {
                CONFLICT
            }
        })
    }
}

/// Only "access denied" may lead to changing the file's security; a file in use or anything else
/// is a conflict to retry later.
fn open_error(error: windows::core::Error) -> i32 {
    if error.code() == windows::Win32::Foundation::ERROR_ACCESS_DENIED.to_hresult() {
        ACCESS
    } else {
        CONFLICT
    }
}

/// Deletes a file with the backup and restore privileges, leaving its security untouched.
fn delete_keeping_security(path: &Path) -> Result<(), i32> {
    let name = wide(path);
    // SAFETY: the name is NUL-terminated; the handle is closed below; the disposition structure
    // is a plain value of the size passed.
    unsafe {
        let file = CreateFileW(
            PCWSTR(name.as_ptr()),
            DELETE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
        .map_err(open_error)?;
        let disposition = FILE_DISPOSITION_INFO {
            DeleteFile: windows::Win32::Foundation::BOOLEAN(1),
        };
        let deleted = SetFileInformationByHandle(
            file,
            FileDispositionInfo,
            std::ptr::addr_of!(disposition).cast(),
            std::mem::size_of::<FILE_DISPOSITION_INFO>() as u32,
        );
        let _ = CloseHandle(file);
        deleted.map_err(open_error)
    }
}

/// Renames without ever replacing a file already at `to`.
fn rename_new(from: &Path, to: &Path) -> Result<(), i32> {
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both names are NUL-terminated; no flags means an existing target makes it fail.
    unsafe {
        MoveFileExW(
            PCWSTR(from.as_ptr()),
            PCWSTR(to.as_ptr()),
            MOVE_FILE_FLAGS(0),
        )
    }
    .map_err(|_| CONFLICT)
}

fn owned_by_trusted_installer(sddl: &str) -> bool {
    sddl.starts_with(&format!("O:{TRUSTED_INSTALLER_SID}"))
}

/// The host's original owner and access list: kept in HKLM, or, for a host renamed by the old
/// test script (nothing kept), what Windows gives its system apps: TrustedInstaller owns it and
/// may change it; Administrators, the system, users and app packages may read and run it.
fn kept_security() -> String {
    winreg::read_machine_long_string(KEPT_KEY, KEPT_VALUE)
        .filter(|sddl| owned_by_trusted_installer(sddl))
        .unwrap_or_else(|| {
            format!(
                "O:{TRUSTED_INSTALLER_SID}D:PAI(A;;FA;;;{TRUSTED_INSTALLER_SID})(A;;0x1200a9;;;BA)\
                 (A;;0x1200a9;;;SY)(A;;0x1200a9;;;BU)(A;;0x1200a9;;;AC)(A;;0x1200a9;;;S-1-15-2-2)"
            )
        })
}

/// Puts the kept owner and access list back if a failed earlier run left `file` owned by
/// Administrators.
fn repair(file: &Path) -> Result<(), i32> {
    if owned_by_trusted_installer(&read_security(file)?) {
        return Ok(());
    }
    // Only what was kept before the change is put back; nothing kept means nothing to repair.
    match kept_record() {
        Some(kept) => write_security(file, &kept),
        None => Ok(()),
    }
}

/// The owner and access list kept before an earlier change, if any.
fn kept_record() -> Option<String> {
    winreg::read_machine_long_string(KEPT_KEY, KEPT_VALUE)
        .filter(|sddl| owned_by_trusted_installer(sddl))
}

/// Renames `from` to `to`: with privileges and no change to the file's security if Windows allows
/// it, otherwise by taking the file for the moment of the rename and giving it back right after.
fn move_file(from: &Path, to: &Path) -> Result<(), i32> {
    match rename_keeping_security(from, to) {
        Ok(()) => return Ok(()),
        Err(CONFLICT) => return Err(CONFLICT),
        Err(_) => {}
    }
    let current = read_security(from)?;
    if owned_by_trusted_installer(&current) {
        winreg::set_machine_string(KEPT_KEY, KEPT_VALUE, &current).map_err(|_| ACCESS)?;
    }
    let original = kept_security();
    write_security(from, TAKEN)?;
    match rename_new(from, to) {
        Ok(()) => write_security(to, &original),
        Err(code) => {
            write_security(from, &original)?;
            Err(code)
        }
    }
}

fn disable(paths: &Paths) -> Result<(), i32> {
    match status_of(plain_file(&paths.host), plain_file(&paths.backup)) {
        OverlayStatus::Disabled => return repair(&paths.backup),
        OverlayStatus::Present => repair(&paths.host)?,
        // Windows put a fresh host back: the stale copy goes, and the fresh host is turned off.
        OverlayStatus::RestoredByWindows => remove_stale(&paths.backup)?,
        OverlayStatus::Unsupported => return Err(UNSUPPORTED),
    }
    move_file(&paths.host, &paths.backup)
}

fn restore(paths: &Paths) -> Result<(), i32> {
    match status_of(plain_file(&paths.host), plain_file(&paths.backup)) {
        OverlayStatus::Present => repair(&paths.host)?,
        OverlayStatus::Disabled => {
            repair(&paths.backup)?;
            move_file(&paths.backup, &paths.host)?;
        }
        // Windows put a fresh host back: keep it and drop the stale copy.
        OverlayStatus::RestoredByWindows => remove_stale(&paths.backup)?,
        OverlayStatus::Unsupported => return Err(UNSUPPORTED),
    }
    let _ = winreg::delete_machine_value(KEPT_KEY, KEPT_VALUE);
    Ok(())
}

/// Deletes the stale copy left beside a host Windows put back, without touching its security.
fn remove_stale(backup: &Path) -> Result<(), i32> {
    if std::fs::remove_file(backup).is_ok() {
        return Ok(());
    }
    delete_keeping_security(backup)
}

#[cfg(test)]
mod tests {
    use super::{kept_security, owned_by_trusted_installer, status_of, OverlayStatus};

    #[test]
    fn the_status_follows_which_files_exist() {
        assert_eq!(status_of(true, false), OverlayStatus::Present);
        assert_eq!(status_of(false, true), OverlayStatus::Disabled);
        assert_eq!(status_of(true, true), OverlayStatus::RestoredByWindows);
        assert_eq!(status_of(false, false), OverlayStatus::Unsupported);
    }

    #[test]
    fn a_files_owner_and_access_list_go_back_exactly_as_read() {
        let path = std::env::temp_dir().join(format!("snippy-sddl-{}.txt", std::process::id()));
        std::fs::write(&path, b"x").expect("file");
        let original = super::read_security(&path).expect("read");
        assert!(
            original.starts_with("O:") && original.contains("D:"),
            "{original}"
        );
        // A protected list with one entry for the owner, then the original back.
        let owner = original.split("D:").next().expect("owner part").to_owned();
        let narrowed = format!("{owner}D:P(A;;FA;;;OW)");
        super::write_security(&path, &narrowed).expect("narrowed");
        let read_back = super::read_security(&path).expect("read back");
        assert!(read_back.contains("D:P"), "{read_back}");
        super::write_security(&path, &original).expect("restored");
        assert_eq!(super::read_security(&path).expect("read again"), original);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_rename_keeps_the_file_and_never_replaces_one() {
        let directory = std::env::temp_dir().join(format!("snippy-rename-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("dir");
        let (from, to) = (
            directory.join("host.exe"),
            directory.join("host.exe.snippy-off"),
        );
        std::fs::write(&from, b"host").expect("host");
        let before = super::read_security(&from).expect("security");
        super::rename_keeping_security(&from, &to).expect("renamed");
        assert!(!from.exists());
        assert_eq!(std::fs::read(&to).expect("read"), b"host");
        assert_eq!(super::read_security(&to).expect("security after"), before);
        std::fs::write(&from, b"other").expect("other");
        assert_eq!(
            super::rename_keeping_security(&from, &to),
            Err(super::CONFLICT)
        );
        assert_eq!(std::fs::read(&to).expect("kept"), b"host");
        super::delete_keeping_security(&to).expect("deleted");
        assert!(!to.exists());
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn only_a_trusted_installer_owner_counts_as_windows_own() {
        assert!(owned_by_trusted_installer(
            "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464D:PAI(A;;FA;;;BA)"
        ));
        assert!(!owned_by_trusted_installer("O:BAD:P(A;;FA;;;BA)"));
        // Nothing kept on this machine: the default is Windows' own owner and a readable list.
        let fallback = kept_security();
        assert!(owned_by_trusted_installer(&fallback));
    }
}
