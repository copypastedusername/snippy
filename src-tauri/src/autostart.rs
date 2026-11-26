//! The logon task that starts Snippy when the user signs in, optionally elevated.
//!
//! Creating and deleting the task go through `schtasks` (an elevated copy of it when the task runs
//! with the highest privileges); reading, running and repointing it go through `task_scheduler`.
//!
//! Where Windows refuses to create the task for a standard user (school and office PCs often
//! forbid new scheduled tasks), Snippy starts through the user's `Run` registry value instead.
//! That entry never runs elevated; with "Run as administrator" on, the started copy asks for
//! elevation as any other start does.

use crate::{elevation, task_scheduler, winreg};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const TASK_NAME: &str = "SnippyStartup";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "Snippy";

/// Held while the task is changed, so a startup repair and the settings page or tray never
/// interleave (a repair must not bring back a task the user just turned off).
static CHANGES: Mutex<()> = Mutex::new(());

/// The logon task, as seen from this exe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogonTask {
    Missing,
    /// The task starts this exe.
    ThisExe {
        enabled: bool,
        elevated: bool,
    },
    /// The task starts another copy (an older download, a moved exe) or a file that is gone.
    OtherExe {
        elevated: bool,
    },
}

fn schtasks(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("Failed to run schtasks: {error}"))
}

pub fn is_autostart_enabled() -> bool {
    if run_entry().is_some() {
        return true;
    }
    match logon_task() {
        Ok(task) => task != LogonTask::Missing,
        Err(error) => {
            log::warn!("{error}");
            false
        }
    }
}

/// The program the user's `Run` value starts, if Snippy set one.
fn run_entry() -> Option<PathBuf> {
    let command = winreg::read_user_string(RUN_KEY, RUN_VALUE)?;
    let program = command.trim().trim_matches('"');
    (!program.is_empty()).then(|| PathBuf::from(program))
}

fn set_run_entry(executable: &Path) -> Result<(), String> {
    winreg::set_user_string(
        RUN_KEY,
        Some(RUN_VALUE),
        &format!("\"{}\"", executable.display()),
    )
}

fn remove_run_entry() -> Result<(), String> {
    winreg::delete_user_value(RUN_KEY, RUN_VALUE)
}

pub fn logon_task() -> Result<LogonTask, String> {
    match task_scheduler::info(TASK_NAME)? {
        Some(task) => classify(&task, &current_exe()?),
        None => Ok(LogonTask::Missing),
    }
}

fn classify(task: &task_scheduler::TaskInfo, current: &Path) -> Result<LogonTask, String> {
    let this_exe = match &task.program {
        Some(program) => is_same_file(program, current)?,
        None => false,
    };
    Ok(if this_exe {
        LogonTask::ThisExe {
            enabled: task.enabled,
            elevated: task.elevated,
        }
    } else {
        LogonTask::OtherExe {
            elevated: task.elevated,
        }
    })
}

/// Starts an elevated copy of this exe through the logon task, without a UAC prompt. Only a task
/// that is enabled, runs with the highest privileges and starts this exe will do; returns false
/// (and starts nothing) otherwise.
pub fn run_elevated_task_for_this_exe() -> Result<bool, String> {
    let current = current_exe()?;
    let wanted = LogonTask::ThisExe {
        enabled: true,
        elevated: true,
    };
    task_scheduler::run_if(
        TASK_NAME,
        |task| matches!(classify(task, &current), Ok(found) if found == wanted),
    )
}

/// Points a logon task left by another copy at this exe, keeping its other settings. Never
/// creates one. A task with the highest privileges is left to an elevated copy, which may change
/// it without a prompt.
pub fn repair_task_path() -> Result<(), String> {
    let _changes = lock_changes();
    let current = current_exe()?;
    if let Some(program) = run_entry() {
        if !is_same_file(&program, &current)? {
            set_run_entry(&current)?;
            log::info!("The Run entry now starts this exe");
        }
    }
    let LogonTask::OtherExe { elevated } = logon_task()? else {
        return Ok(());
    };
    if elevated && !elevation::is_elevated()? {
        log::info!("The logon task starts another exe; the elevated copy will update it");
        return Ok(());
    }
    task_scheduler::repoint(TASK_NAME, &current)?;
    log::info!("The logon task now starts this exe");
    Ok(())
}

/// Re-registers an existing task with the privileges "Run as administrator" asks for now
/// (`run_as_admin` is read after the change lock is taken, so the newest setting wins). A `Run`
/// entry is replaced by an elevated task when that becomes possible.
pub fn follow_privilege_setting(
    run_as_admin: impl FnOnce() -> Result<bool, String>,
) -> Result<(), String> {
    let _changes = lock_changes();
    let has_run_entry = run_entry().is_some();
    if !has_run_entry && logon_task()? == LogonTask::Missing {
        return Ok(());
    }
    let elevated = run_as_admin()?;
    if has_run_entry && !elevated {
        // The Run entry already starts Snippy without elevation.
        return Ok(());
    }
    enable(elevated)
}

/// Both paths name the same file (resolving case, `..`, links and short names). A task program
/// that no longer exists is simply another file.
fn is_same_file(program: &Path, current: &Path) -> Result<bool, String> {
    let program = match std::fs::canonicalize(program) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("Could not resolve {}: {error}", program.display())),
    };
    let current = std::fs::canonicalize(current)
        .map_err(|error| format!("Could not resolve this exe: {error}"))?;
    Ok(program == current)
}

fn current_exe() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("Failed to get exe path: {error}"))
}

fn lock_changes() -> std::sync::MutexGuard<'static, ()> {
    CHANGES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Registers or removes the logon task. `elevated` (the "Run as administrator" setting) decides
/// whether the task runs with the highest privileges; without it Snippy starts like any app.
pub fn set_enabled(enabled: bool, elevated: bool) -> Result<(), String> {
    let _changes = lock_changes();
    if enabled {
        enable(elevated)
    } else {
        remove_run_entry()?;
        disable_autostart()
    }
}

/// Registers the logon task; when Windows refuses it (a policy, or no administrator to approve an
/// elevated one), falls back to the `Run` entry. Only one of the two is left in place.
fn enable(elevated: bool) -> Result<(), String> {
    match enable_autostart(elevated) {
        Ok(()) => {
            if let Err(error) = remove_run_entry() {
                log::warn!("Could not remove the old Run entry: {error}");
            }
            Ok(())
        }
        Err(task_error) => {
            log::warn!("{task_error}; starting with Windows through the Run entry instead");
            set_run_entry(&current_exe()?).map_err(|run_error| {
                format!("Snippy could not start with Windows: {task_error}. {run_error}")
            })
        }
    }
}

fn enable_autostart(elevated: bool) -> Result<(), String> {
    let definition = task_definition(&current_exe()?, elevated)?;
    // Kept open (and so locked against writes) until schtasks has read it: an elevated schtasks
    // must not read a definition another process could swap in between.
    let (file, _lock) = write_definition(&definition)?;
    let file_argument = file.to_string_lossy().into_owned();

    let result = create_task(&file_argument, elevated);
    drop(_lock);
    let _ = std::fs::remove_file(&file);
    result?;

    if logon_task()? == LogonTask::Missing {
        Err("Windows did not register the Snippy logon task".to_owned())
    } else {
        Ok(())
    }
}

fn disable_autostart() -> Result<(), String> {
    let arguments = ["/Delete", "/TN", TASK_NAME, "/F"];
    let output = schtasks(&arguments)?;
    if output.status.success() || logon_task()? == LogonTask::Missing {
        return Ok(());
    }
    // Deleting a highest-run-level task needs elevation too.
    elevation::run_elevated_and_wait("schtasks.exe", &arguments)?;
    if logon_task()? == LogonTask::Missing {
        Ok(())
    } else {
        Err("Failed to delete the Snippy logon task".to_owned())
    }
}

fn create_task(definition_path: &str, elevated: bool) -> Result<(), String> {
    let arguments = ["/Create", "/TN", TASK_NAME, "/XML", definition_path, "/F"];
    let output = schtasks(&arguments)?;
    if output.status.success() {
        return Ok(());
    }
    if !elevated {
        return Err(format!(
            "Windows did not register the logon task: {}",
            message_of(&output)
        ));
    }
    // Registering a task that runs with the highest privileges requires elevation; ask for it once
    // here so that every later start can go through the task without a prompt.
    elevation::run_elevated_and_wait("schtasks.exe", &arguments)
        .map_err(|error| format!("{error} (schtasks said: {})", message_of(&output)))
}

fn message_of(output: &std::process::Output) -> String {
    let stderr = console_text(&output.stderr);
    let text = if stderr.trim().is_empty() {
        console_text(&output.stdout)
    } else {
        stderr
    };
    let text = text.trim();
    if text.is_empty() {
        format!("exit code {}", output.status)
    } else {
        text.to_owned()
    }
}

/// Console tools such as schtasks write in the OEM code page (866 on Russian Windows), not UTF-8.
fn console_text(bytes: &[u8]) -> String {
    use windows::Win32::Globalization::{
        MultiByteToWideChar, CP_OEMCP, MULTI_BYTE_TO_WIDE_CHAR_FLAGS,
    };
    if bytes.is_empty() {
        return String::new();
    }
    let flags = MULTI_BYTE_TO_WIDE_CHAR_FLAGS(0);
    // SAFETY: a `None` output buffer only asks for the length in UTF-16 units.
    let units = unsafe { MultiByteToWideChar(CP_OEMCP, flags, bytes, None) };
    let Ok(length) = usize::try_from(units) else {
        return String::from_utf8_lossy(bytes).into_owned();
    };
    if length == 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut wide = vec![0_u16; length];
    // SAFETY: `wide` holds exactly the length the first call reported.
    let written = unsafe { MultiByteToWideChar(CP_OEMCP, flags, bytes, Some(&mut wide)) };
    let written = usize::try_from(written).unwrap_or(0).min(length);
    String::from_utf16_lossy(&wide[..written])
}

/// Writes the definition to a new file and returns it with the handle that keeps it locked: other
/// processes may read it (schtasks must) but not write, rename or delete it while it is open.
fn write_definition(definition: &str) -> Result<(PathBuf, std::fs::File), String> {
    use std::io::Write;
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_SHARE_READ: u32 = 1;
    // Task Scheduler only accepts UTF-16LE with a BOM for /XML definitions.
    let mut bytes = vec![0xFF, 0xFE];
    for unit in definition.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!("snippy-task-{}-{nonce}.xml", std::process::id()));
    // `create_new` refuses a file someone planted under that name.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .share_mode(FILE_SHARE_READ)
        .open(&path)
        .map_err(|error| format!("Failed to create {}: {error}", path.display()))?;
    file.write_all(&bytes)
        .and_then(|()| file.flush())
        .map_err(|error| format!("Failed to write {}: {error}", path.display()))?;
    Ok((path, file))
}

fn task_definition(executable: &Path, elevated: bool) -> Result<String, String> {
    let run_level = if elevated {
        "HighestAvailable"
    } else {
        "LeastPrivilege"
    };
    let user = std::env::var("USERNAME").unwrap_or_default();
    let domain = std::env::var("USERDOMAIN").unwrap_or_default();
    let principal = if domain.is_empty() || user.is_empty() {
        user.clone()
    } else {
        format!("{domain}\\{user}")
    };
    if principal.is_empty() {
        return Err("Could not determine the current Windows account".to_owned());
    }
    let command = escape_xml(&executable.to_string_lossy());
    let working_directory = executable
        .parent()
        .map(|parent| escape_xml(&parent.to_string_lossy()))
        .unwrap_or_default();
    let principal = escape_xml(&principal);

    // `ExecutionTimeLimit` of zero keeps Windows from killing a long-running tray app after the
    // schtasks default of 72 hours; `IgnoreNew` keeps the logon trigger from stacking instances.
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.3" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Starts Snippy at logon.</Description>
    <URI>\{TASK_NAME}</URI>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>{principal}</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{principal}</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
      <WorkingDirectory>{working_directory}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    ))
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::{console_text, escape_xml, is_same_file, schtasks, task_definition};

    #[test]
    fn console_output_is_read_in_the_oem_code_page() {
        // Plain ASCII reads the same in every OEM code page.
        assert_eq!(
            console_text(b"ERROR: Access is denied."),
            "ERROR: Access is denied."
        );
        assert_eq!(console_text(b""), "");
        // SAFETY: GetOEMCP takes no arguments and only reads system state.
        if unsafe { windows::Win32::Globalization::GetOEMCP() } == 866 {
            // "ОШИБКА" in code page 866.
            assert_eq!(
                console_text(&[0x8E, 0x98, 0x88, 0x81, 0x8A, 0x80]),
                "ОШИБКА"
            );
        }
    }
    use crate::task_scheduler;
    use std::path::Path;

    #[test]
    fn the_same_file_matches_however_its_path_is_written() {
        let folder = std::env::temp_dir().join(format!("snippy-same-{}", std::process::id()));
        std::fs::create_dir_all(folder.join("Sub")).unwrap_or_else(|error| panic!("{error}"));
        let exe = folder.join("Snippy (1).exe");
        let other = folder.join("Snippy (3).exe");
        for file in [&exe, &other] {
            std::fs::write(file, b"MZ").unwrap_or_else(|error| panic!("{error}"));
        }
        let upper = folder.join("SNIPPY (1).EXE");
        let dotted = folder.join("Sub").join("..").join("Snippy (1).exe");
        let same = [
            is_same_file(&upper, &exe),
            is_same_file(&dotted, &exe),
            is_same_file(&other, &exe),
            is_same_file(&folder.join("gone.exe"), &exe),
        ];
        let _ = std::fs::remove_dir_all(&folder);
        assert_eq!(same, [Ok(true), Ok(true), Ok(false), Ok(false)]);
    }

    #[test]
    #[ignore = "creates and deletes a scheduled task"]
    fn a_task_is_read_and_repointed_with_its_exact_unicode_path() {
        let name = format!("SnippyProbe-{}", std::process::id());
        let program = r"C:\Temp\Папка & (1)\snippy (3).exe";
        let quoted = format!("\"{program}\"");
        let created = schtasks(&[
            "/Create", "/TN", &name, "/TR", &quoted, "/SC", "ONCE", "/ST", "23:59",
        ])
        .unwrap_or_else(|error| panic!("{error}"));
        assert!(created.status.success(), "{created:?}");
        let read = task_scheduler::info(&name);
        let declined = task_scheduler::run_if(&name, |_| false);
        let moved = Path::new(r"D:\Портативные\snippy.exe");
        let repointed = task_scheduler::repoint(&name, moved);
        let reread = task_scheduler::info(&name);
        let _ = schtasks(&["/Delete", "/TN", &name, "/F"]);

        let read = read.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(
            read.and_then(|task| task.program).as_deref(),
            Some(Path::new(program))
        );
        assert_eq!(declined, Ok(false));
        repointed.unwrap_or_else(|error| panic!("{error}"));
        let reread = reread.unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(reread.and_then(|task| task.program).as_deref(), Some(moved));
    }

    #[test]
    fn definition_has_no_time_limit_and_elevates_only_when_asked() {
        let definition = task_definition(Path::new(r"C:\Program Files\Snippy\snippy.exe"), true)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(definition.contains("<RunLevel>HighestAvailable</RunLevel>"));
        let plain = task_definition(Path::new(r"C:\Snippy\snippy.exe"), false)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(plain.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(definition.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(definition.contains(r"C:\Program Files\Snippy\snippy.exe"));
    }

    #[test]
    fn xml_special_characters_are_escaped() {
        assert_eq!(escape_xml(r#"a&b<c>"d'"#), "a&amp;b&lt;c&gt;&quot;d&apos;");
    }
}
