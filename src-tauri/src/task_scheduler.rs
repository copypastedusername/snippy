//! Reads, runs and repoints a scheduled task through the Task Scheduler API. (`schtasks` prints
//! paths in a code page that mangles non-ASCII folders, and cannot change only a task's program.)

use std::ffi::OsString;
use std::marker::PhantomData;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use windows::core::{Interface, BSTR, HRESULT, VARIANT};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, RPC_E_CHANGED_MODE, VARIANT_FALSE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, IRegisteredTask, ITaskDefinition, ITaskFolder, ITaskService, TaskScheduler,
    TASK_LOGON_TYPE, TASK_RUNLEVEL_HIGHEST, TASK_RUNLEVEL_TYPE, TASK_UPDATE,
};

/// What a task does, as far as Snippy cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskInfo {
    /// The program of its first "start a program" action.
    pub program: Option<PathBuf>,
    pub enabled: bool,
    /// Runs with the highest privileges, so only an elevated process may change it.
    pub elevated: bool,
}

/// The task's details, or `None` when there is no such task.
pub fn info(name: &str) -> Result<Option<TaskInfo>, String> {
    let _com = Com::init()?;
    // Every COM object must be released before `_com`: keep them in locals, never in the
    // function's last expression (its temporaries outlive the locals).
    let Some((_, task)) = open(name)? else {
        return Ok(None);
    };
    let info = read(&task)?;
    Ok(Some(info))
}

/// Starts the task when `should_run` approves of what it does, checking and starting the same
/// registered task. Returns whether it was started.
pub fn run_if(name: &str, should_run: impl FnOnce(&TaskInfo) -> bool) -> Result<bool, String> {
    let _com = Com::init()?;
    let Some((_, task)) = open(name)? else {
        return Ok(false);
    };
    if !should_run(&read(&task)?) {
        return Ok(false);
    }
    let no_parameters = VARIANT::default();
    // SAFETY: a COM call on a live interface.
    let started = unsafe { task.Run(&no_parameters) };
    started
        .map(|_| true)
        .map_err(|error| format!("Could not start the task: {error}"))
}

fn read(task: &IRegisteredTask) -> Result<TaskInfo, String> {
    // SAFETY: COM calls on live interfaces; out-parameters are initialised locals.
    let read = || unsafe {
        let definition = task.Definition()?;
        let mut run_level = TASK_RUNLEVEL_TYPE::default();
        definition.Principal()?.RunLevel(&mut run_level)?;
        let program = match exec_action(&definition)? {
            Some(action) => {
                let mut path = BSTR::new();
                action.Path(&mut path)?;
                program_path(path.as_wide())
            }
            None => None,
        };
        windows::core::Result::Ok(TaskInfo {
            program,
            enabled: task.Enabled()? != VARIANT_FALSE,
            elevated: run_level == TASK_RUNLEVEL_HIGHEST,
        })
    };
    read().map_err(|error| format!("Could not read the task: {error}"))
}

/// Makes an existing task start `program` (in its folder), keeping everything else about it.
/// Fails rather than creating a task that is missing.
pub fn repoint(name: &str, program: &Path) -> Result<(), String> {
    let _com = Com::init()?;
    let (folder, task) = open(name)?.ok_or("The task does not exist")?;
    let update = || -> windows::core::Result<()> {
        // SAFETY: COM calls on live interfaces; out-parameters are initialised locals.
        unsafe {
            let definition = task.Definition()?;
            let action = exec_action(&definition)?
                .ok_or_else(|| windows::core::Error::from(ERROR_FILE_NOT_FOUND.to_hresult()))?;
            action.SetPath(&bstr(program))?;
            if let Some(parent) = program.parent() {
                action.SetWorkingDirectory(&bstr(parent))?;
            }
            let mut logon = TASK_LOGON_TYPE::default();
            definition.Principal()?.LogonType(&mut logon)?;
            let none = VARIANT::default();
            folder.RegisterTaskDefinition(
                &BSTR::from(name),
                &definition,
                TASK_UPDATE.0,
                &none,
                &none,
                logon,
                &none,
            )?;
            Ok(())
        }
    };
    update().map_err(|error| format!("Could not update the task: {error}"))
}

/// The root folder and the task in it, or `None` when the task does not exist.
fn open(name: &str) -> Result<Option<(ITaskFolder, IRegisteredTask)>, String> {
    let missing = |error: &windows::core::Error| {
        error.code() == ERROR_FILE_NOT_FOUND.to_hresult()
            || error.code() == ERROR_PATH_NOT_FOUND.to_hresult()
    };
    // SAFETY: COM is initialised on this thread (by the caller's `Com`); empty VARIANTs connect to
    // the local machine as the current user.
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            .map_err(|error| format!("Task Scheduler is unavailable: {error}"))?;
        let none = VARIANT::default();
        service
            .Connect(&none, &none, &none, &none)
            .map_err(|error| format!("Could not connect to Task Scheduler: {error}"))?;
        let folder = service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|error| format!("Could not open the task folder: {error}"))?;
        match folder.GetTask(&BSTR::from(name)) {
            Ok(task) => Ok(Some((folder, task))),
            Err(error) if missing(&error) => Ok(None),
            Err(error) => Err(format!("Could not open the task: {error}")),
        }
    }
}

/// The first "start a program" action of a task definition.
fn exec_action(definition: &ITaskDefinition) -> windows::core::Result<Option<IExecAction>> {
    // SAFETY: COM calls on a live interface; the collection counts from 1.
    unsafe {
        let actions = definition.Actions()?;
        let mut count = 0;
        actions.Count(&mut count)?;
        Ok((1..=count)
            .filter_map(|index| actions.get_Item(index).ok())
            .find_map(|action| action.cast::<IExecAction>().ok()))
    }
}

/// A task's program as Task Scheduler stores it (possibly quoted or padded), kept as UTF-16.
fn program_path(raw: &[u16]) -> Option<PathBuf> {
    let unwanted = |unit: &u16| *unit == u16::from(b' ') || *unit == u16::from(b'"');
    let start = raw.iter().position(|unit| !unwanted(unit))?;
    let end = raw.iter().rposition(|unit| !unwanted(unit))? + 1;
    Some(PathBuf::from(OsString::from_wide(&raw[start..end])))
}

fn bstr(path: &Path) -> BSTR {
    let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    BSTR::from_wide(&wide).unwrap_or_default()
}

/// COM for the current thread, released on drop when this guard set it up. A thread that already
/// runs COM in another apartment (a UI thread) can use it as it is.
struct Com {
    uninitialize: bool,
    /// COM must be released on the thread that set it up.
    _same_thread: PhantomData<*const ()>,
}

impl Com {
    fn init() -> Result<Self, String> {
        // SAFETY: balanced by `Drop` whenever it succeeds (S_OK and S_FALSE alike).
        let result: HRESULT = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() || result == RPC_E_CHANGED_MODE {
            Ok(Self {
                uninitialize: result.is_ok(),
                _same_thread: PhantomData,
            })
        } else {
            Err(format!("COM is unavailable: {result}"))
        }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: pairs the successful CoInitializeEx in `init` on this thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_programs_lose_quotes_and_padding() {
        let path = |text: &str| program_path(&text.encode_utf16().collect::<Vec<_>>());
        assert_eq!(
            path(r#" "C:\Tools\snippy (3).exe" "#),
            Some(PathBuf::from(r"C:\Tools\snippy (3).exe"))
        );
        assert_eq!(
            path(r"C:\Папка\snippy.exe"),
            Some(PathBuf::from(r"C:\Папка\snippy.exe"))
        );
        assert_eq!(path(r#""""#), None);
        assert_eq!(path("   "), None);
    }

    #[test]
    fn paths_survive_the_round_trip_through_utf16() {
        let original = Path::new(r"D:\Портативные\snippy (1).exe");
        assert_eq!(
            program_path(bstr(original).as_wide()),
            Some(original.to_path_buf())
        );
    }

    #[test]
    fn a_missing_task_is_none_not_an_error() {
        assert_eq!(info("Snippy.NoSuchTask.Probe"), Ok(None));
    }
}
