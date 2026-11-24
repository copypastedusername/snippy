use crate::elevation;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};

const TASK_NAME: &str = "SnippyStartup";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn schtasks(args: &[&str]) -> Result<std::process::Output, String> {
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("Failed to run schtasks: {error}"))
}

pub fn is_autostart_enabled() -> bool {
    matches!(schtasks(&["/Query", "/TN", TASK_NAME]), Ok(output) if output.status.success())
}

/// Start the elevated instance through the logon task.
///
/// This is the whole point of registering the task with the highest run level: Task Scheduler
/// launches it elevated without a UAC prompt, so an already-configured autostart makes manual
/// elevation prompts unnecessary.
pub fn run_task() -> Result<(), String> {
    let output = schtasks(&["/Run", "/TN", TASK_NAME])?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to start the scheduled task: {}",
            message_of(&output)
        ))
    }
}

/// Registers or removes the logon task. `elevated` (the "Run as administrator" setting) decides
/// whether the task runs with the highest privileges; without it Snippy starts like any app.
pub fn set_enabled(enabled: bool, elevated: bool) -> Result<(), String> {
    if enabled {
        enable_autostart(elevated)
    } else {
        disable_autostart()
    }
}

fn enable_autostart(elevated: bool) -> Result<(), String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("Failed to get exe path: {error}"))?;
    let definition = task_definition(&executable, elevated)?;
    // Kept open (and so locked against writes) until schtasks has read it: an elevated schtasks
    // must not read a definition another process could swap in between.
    let (file, _lock) = write_definition(&definition)?;
    let file_argument = file.to_string_lossy().into_owned();

    let result = create_task(&file_argument, elevated);
    drop(_lock);
    let _ = std::fs::remove_file(&file);
    result?;

    if is_autostart_enabled() {
        Ok(())
    } else {
        Err("Windows did not register the Snippy logon task".to_owned())
    }
}

fn disable_autostart() -> Result<(), String> {
    let arguments = ["/Delete", "/TN", TASK_NAME, "/F"];
    let output = schtasks(&arguments)?;
    if output.status.success() || !is_autostart_enabled() {
        return Ok(());
    }
    // Deleting a highest-run-level task needs elevation too.
    elevation::run_elevated_and_wait("schtasks.exe", &arguments)?;
    if is_autostart_enabled() {
        Err("Failed to delete the Snippy logon task".to_owned())
    } else {
        Ok(())
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    let text = if stderr.trim().is_empty() {
        String::from_utf8_lossy(&output.stdout).into_owned()
    } else {
        stderr.into_owned()
    };
    let text = text.trim();
    if text.is_empty() {
        format!("exit code {}", output.status)
    } else {
        text.to_owned()
    }
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
    use super::{escape_xml, task_definition};
    use std::path::Path;

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
