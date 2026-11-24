//! Small, typed helpers over the Win32 registry API. Writes go to HKEY_CURRENT_USER only.

use std::os::windows::ffi::OsStrExt;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn check(status: WIN32_ERROR, action: &str, key: &str) -> Result<(), String> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "Could not {action} HKCU\\{key}: {}",
            windows::core::Error::from(status.to_hresult()).message()
        ))
    }
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the handle came from RegCreateKeyExW and is closed exactly once.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

/// Creates `HKCU\key` if needed and writes a string value (`None` is the default value).
pub fn set_user_string(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    let key_name = wide(key);
    let mut handle = HKEY::default();
    // SAFETY: the key name is NUL-terminated and `handle` receives the opened key.
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut handle,
            None,
        )
    };
    check(status, "create", key)?;
    let handle = Key(handle);

    let name = name.map(wide);
    let data: Vec<u8> = wide(value)
        .iter()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    // SAFETY: REG_SZ data is the NUL-terminated UTF-16 string as bytes.
    let status = unsafe {
        RegSetValueExW(
            handle.0,
            name.as_ref()
                .map_or(PCWSTR::null(), |name| PCWSTR(name.as_ptr())),
            0,
            REG_SZ,
            Some(&data),
        )
    };
    check(status, "write", key)
}

/// Deletes one value under HKCU; a missing key or value is not an error.
pub fn delete_user_value(key: &str, name: &str) -> Result<(), String> {
    let (key_name, value_name) = (wide(key), wide(name));
    // SAFETY: both strings are NUL-terminated.
    let status = unsafe {
        RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            PCWSTR(value_name.as_ptr()),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "clean up", key)
}

/// Deletes a key and everything below it under HKCU; a missing key is not an error.
pub fn delete_user_tree(key: &str) -> Result<(), String> {
    let key_name = wide(key);
    // SAFETY: the key name is NUL-terminated.
    let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key_name.as_ptr())) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(());
    }
    check(status, "remove", key)
}

/// Reads a DWORD from HKCU; `None` when the key or value is missing or has another type.
pub fn read_user_dword(key: &str, name: &str) -> Option<u32> {
    let (key_name, value_name) = (wide(key), wide(name));
    let mut value = 0_u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    // SAFETY: both names are NUL-terminated and the buffer is a u32 of the declared size.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(key_name.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut value as *mut u32).cast()),
            Some(&mut size),
        )
    };
    (status == ERROR_SUCCESS).then_some(value)
}

/// Reads a short string from HKLM; `None` when missing or longer than 255 characters.
pub fn read_machine_string(key: &str, name: &str) -> Option<String> {
    let (key_name, value_name) = (wide(key), wide(name));
    let mut buffer = [0_u16; 256];
    let mut size = std::mem::size_of_val(&buffer) as u32;
    // SAFETY: both names are NUL-terminated; `size` is the buffer size in bytes.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key_name.as_ptr()),
            PCWSTR(value_name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buffer
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}
