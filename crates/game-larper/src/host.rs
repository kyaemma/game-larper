use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use game_larper_core::{Error as CoreError, resolve_executable};
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, HWND};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
    WaitForInputIdle, WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW, WM_CLOSE,
};

use crate::platform::{self, integrity_of};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

pub struct Staged {
    pub path: PathBuf,
    pub created: bool,
}

pub struct LaunchReport {
    pub generation: u64,
    pub pid: u32,
    pub executable: PathBuf,
    pub basename: String,
    pub working_directory: PathBuf,
    pub hwnd: isize,
    pub title: String,
    pub integrity: String,
    pub waiter: isize,
}

struct Running {
    process: isize,
    hwnd: isize,
    icon: isize,
    staged: Staged,
    generation: u64,
}

pub struct RunnerHost {
    runtime: PathBuf,
    job: isize,
    current: Option<Running>,
    next_generation: u64,
    stopping: bool,
}

impl RunnerHost {
    pub fn new(runtime: PathBuf) -> Self {
        Self {
            runtime,
            job: create_kill_on_close_job(),
            current: None,
            next_generation: 1,
            stopping: false,
        }
    }

    pub fn launch(
        &mut self,
        template: &Path,
        application_id: &str,
        remote_name: &str,
        icon: Option<&Path>,
    ) -> Result<LaunchReport, String> {
        if self.current.is_some() {
            self.stop()?;
        }
        let staged = stage_runner(template, &self.runtime, application_id, remote_name)
            .map_err(|error| error.to_string())?;
        match self.spawn_staged(staged, icon) {
            Ok(report) => Ok(report),
            Err(error) => Err(error),
        }
    }

    fn spawn_staged(
        &mut self,
        staged: Staged,
        icon: Option<&Path>,
    ) -> Result<LaunchReport, String> {
        let directory = staged
            .path
            .parent()
            .ok_or_else(|| "The runner path has no directory.".to_string())?;
        let mut command = wide(&format!("\"{}\"", staged.path.display()));
        if command.iter().filter(|unit| **unit == b'"' as u16).count() != 2 {
            cleanup_staged(&self.runtime, &staged);
            return Err("The runtime path cannot be quoted safely.".into());
        }
        let directory_wide = wide_path(directory);
        let mut startup: STARTUPINFOW = unsafe { std::mem::zeroed() };
        startup.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut info: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
        let created = unsafe {
            CreateProcessW(
                std::ptr::null(),
                command.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                FALSE,
                0,
                std::ptr::null(),
                directory_wide.as_ptr(),
                &startup,
                &mut info,
            )
        };
        if created == 0 {
            let error = io::Error::last_os_error();
            cleanup_staged(&self.runtime, &staged);
            return Err(format!("The runner did not start: {error}"));
        }
        unsafe {
            CloseHandle(info.hThread);
        }
        if self.job != 0 {
            unsafe {
                AssignProcessToJobObject(self.job as HANDLE, info.hProcess);
            }
        }
        let idle = unsafe { WaitForInputIdle(info.hProcess, 1500) };
        let hwnd = find_owned_window(info.dwProcessId, Duration::from_millis(1500));
        let alive = process_alive(info.hProcess);
        if !alive || hwnd == 0 {
            unsafe {
                TerminateProcess(info.hProcess, 1);
                WaitForSingleObject(info.hProcess, 2000);
                CloseHandle(info.hProcess);
            }
            cleanup_staged(&self.runtime, &staged);
            return Err("The native runner did not create its game window.".into());
        }
        let waiter = match duplicate_handle(info.hProcess) {
            Ok(waiter) => waiter,
            Err(error) => {
                unsafe {
                    TerminateProcess(info.hProcess, 1);
                    WaitForSingleObject(info.hProcess, 2000);
                    CloseHandle(info.hProcess);
                }
                cleanup_staged(&self.runtime, &staged);
                return Err(error);
            }
        };
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let basename = staged
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "game.exe".into());
        let report = LaunchReport {
            generation,
            pid: info.dwProcessId,
            executable: staged.path.clone(),
            basename,
            working_directory: directory.to_path_buf(),
            hwnd,
            title: window_title(hwnd),
            integrity: integrity_of(info.hProcess),
            waiter: waiter as isize,
        };
        let _ = idle;
        let icon_handle = icon
            .and_then(|path| platform::apply_window_icon(hwnd, path))
            .unwrap_or(0);
        self.current = Some(Running {
            process: info.hProcess as isize,
            hwnd,
            icon: icon_handle,
            staged,
            generation,
        });
        self.stopping = false;
        Ok(report)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        let Some(running) = self.current.take() else {
            return Ok(());
        };
        self.stopping = true;
        let process = running.process as HANDLE;
        if running.hwnd != 0 {
            unsafe {
                PostMessageW(running.hwnd as HWND, WM_CLOSE, 0, 0);
            }
            unsafe {
                WaitForSingleObject(process, 2000);
            }
        }
        if process_alive(process) {
            unsafe {
                TerminateProcess(process, 0);
                WaitForSingleObject(process, 5000);
            }
        }
        let exited = !process_alive(process);
        unsafe {
            CloseHandle(process);
        }
        platform::destroy_icon(running.icon);
        cleanup_staged(&self.runtime, &running.staged);
        self.stopping = false;
        if !exited {
            self.current = Some(running);
            return Err("The owned runner did not exit.".into());
        }
        Ok(())
    }

    /// Called after the waiter observes the process handle. Returns true when the exit was not requested.
    pub fn take_unexpected_exit(&mut self, generation: u64) -> bool {
        if self.stopping {
            return false;
        }
        let Some(running) = self.current.take() else {
            return false;
        };
        if running.generation != generation {
            self.current = Some(running);
            return false;
        }
        unsafe {
            CloseHandle(running.process as HANDLE);
        }
        platform::destroy_icon(running.icon);
        cleanup_staged(&self.runtime, &running.staged);
        true
    }
}

impl Drop for RunnerHost {
    fn drop(&mut self) {
        let _ = self.stop();
        if self.job != 0 {
            unsafe {
                CloseHandle(self.job as HANDLE);
            }
        }
    }
}

pub fn stage_runner(
    template: &Path,
    runtime_root: &Path,
    application_id: &str,
    remote_name: &str,
) -> Result<Staged, CoreError> {
    let destination = resolve_executable(runtime_root, application_id, remote_name)?;
    if let Some(staged) = place_file(template, runtime_root, &destination).map_err(CoreError::Io)? {
        return Ok(staged);
    }
    let session = format!("session-{:08x}{:04x}", std::process::id(), millis_low());
    let nested = format!("{session}/{remote_name}");
    let destination = resolve_executable(runtime_root, application_id, &nested)?;
    place_file(template, runtime_root, &destination)
        .map_err(CoreError::Io)?
        .ok_or(CoreError::UnsafePath(
            "Could not isolate a runtime session.",
        ))
}

fn place_file(
    template: &Path,
    runtime_root: &Path,
    destination: &Path,
) -> io::Result<Option<Staged>> {
    if destination.exists() {
        if is_reparse_path(destination)? {
            return Err(io::Error::other("The selected runtime path is a link."));
        }
        if same_bytes(destination, template)? {
            return Ok(Some(Staged {
                path: destination.to_path_buf(),
                created: false,
            }));
        }
        return Ok(None);
    }
    if let Some(parent) = destination.parent() {
        ensure_private_dirs(runtime_root, parent)?;
    }
    fs::copy(template, destination)?;
    Ok(Some(Staged {
        path: destination.to_path_buf(),
        created: true,
    }))
}

fn ensure_private_dirs(runtime_root: &Path, target: &Path) -> io::Result<()> {
    let root = std::path::absolute(runtime_root)?;
    fs::create_dir_all(&root)?;
    if is_reparse_path(&root)? {
        return Err(io::Error::other(
            "A runtime directory is a link or junction.",
        ));
    }
    let target = std::path::absolute(target)?;
    if !is_under(&target, &root) {
        return Err(io::Error::other(
            "Runtime path escapes the private directory.",
        ));
    }
    let relative = target.strip_prefix(&root).unwrap_or(&target);
    let mut current = root;
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current)?;
        }
        if is_reparse_path(&current)? {
            return Err(io::Error::other(
                "A runtime directory is a link or junction.",
            ));
        }
    }
    Ok(())
}

fn is_under(path: &Path, root: &Path) -> bool {
    let path = path.to_string_lossy().to_ascii_lowercase();
    let root = root.to_string_lossy().to_ascii_lowercase();
    let path = path.trim_end_matches(['\\', '/']);
    let root = root.trim_end_matches(['\\', '/']);
    path == root
        || path.starts_with(&(root.to_string() + "\\"))
        || path.starts_with(&(root.to_string() + "/"))
}

pub fn cleanup_staged(runtime_root: &Path, staged: &Staged) {
    if !staged.created {
        return;
    }
    let Ok(root) = std::path::absolute(runtime_root) else {
        return;
    };
    let Ok(file) = std::path::absolute(&staged.path) else {
        return;
    };
    if !file.starts_with(&root) {
        return;
    }
    if chain_has_reparse(&root, &file) {
        return;
    }
    let _ = fs::remove_file(&file);
    let mut directory = file.parent().map(Path::to_path_buf);
    while let Some(current) = directory {
        if !current.starts_with(&root) || current == root {
            break;
        }
        if is_reparse_path(&current).unwrap_or(true) {
            break;
        }
        if fs::read_dir(&current)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(true)
        {
            break;
        }
        if fs::remove_dir(&current).is_err() {
            break;
        }
        directory = current.parent().map(Path::to_path_buf);
    }
}

fn chain_has_reparse(root: &Path, file: &Path) -> bool {
    let mut directory = file.parent();
    while let Some(current) = directory {
        if is_reparse_path(current).unwrap_or(true) {
            return true;
        }
        if current == root {
            return false;
        }
        if !current.starts_with(root) {
            return true;
        }
        directory = current.parent();
    }
    true
}

fn is_reparse_path(path: &Path) -> io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    Ok(fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

fn same_bytes(left: &Path, right: &Path) -> io::Result<bool> {
    let left_meta = fs::metadata(left)?;
    let right_meta = fs::metadata(right)?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left_file = File::open(left)?;
    let mut right_file = File::open(right)?;
    let mut left_buf = [0u8; 8192];
    let mut right_buf = [0u8; 8192];
    loop {
        let left_read = left_file.read(&mut left_buf)?;
        let right_read = right_file.read(&mut right_buf)?;
        if left_read != right_read || left_buf[..left_read] != right_buf[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn find_owned_window(process_id: u32, budget: Duration) -> isize {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let mut found: HWND = std::ptr::null_mut();
        let mut state = EnumState {
            process_id,
            found: &mut found,
        };
        unsafe {
            EnumWindows(Some(enum_window), &mut state as *mut EnumState as isize);
        }
        if !found.is_null() {
            return found as isize;
        }
        if std::time::Instant::now() >= deadline {
            return 0;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct EnumState<'a> {
    process_id: u32,
    found: &'a mut HWND,
}

unsafe extern "system" fn enum_window(window: HWND, context: isize) -> i32 {
    let state = unsafe { &mut *(context as *mut EnumState) };
    let mut owner = 0u32;
    unsafe {
        GetWindowThreadProcessId(window, &mut owner);
    }
    let owned_by = unsafe { GetWindow(window, GW_OWNER) };
    if owner == state.process_id && owned_by.is_null() && unsafe { IsWindowVisible(window) } != 0 {
        *state.found = window;
        return 0;
    }
    1
}

fn window_title(hwnd: isize) -> String {
    if hwnd == 0 {
        return String::new();
    }
    let mut buffer = [0u16; 260];
    let length = unsafe { GetWindowTextW(hwnd as HWND, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..length as usize])
}

fn process_alive(process: HANDLE) -> bool {
    let mut code = 0u32;
    let read = unsafe { GetExitCodeProcess(process, &mut code) };
    read != 0 && code == 259
}

fn duplicate_handle(process: HANDLE) -> Result<HANDLE, String> {
    use windows_sys::Win32::Foundation::{DUPLICATE_SAME_ACCESS, DuplicateHandle};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut duplicate: HANDLE = std::ptr::null_mut();
    let copied = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            process,
            GetCurrentProcess(),
            &mut duplicate,
            0,
            FALSE,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if copied == 0 {
        Err(format!(
            "Could not watch the runner: {}",
            io::Error::last_os_error()
        ))
    } else {
        Ok(duplicate)
    }
}

fn create_kill_on_close_job() -> isize {
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return 0;
    }
    let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const _,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        unsafe { CloseHandle(job) };
        0
    } else {
        job as isize
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn millis_low() -> u16 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| (duration.subsec_millis() & 0xffff) as u16)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{Staged, cleanup_staged, stage_runner};
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn stage_reuses_identical_bytes_and_isolates_a_different_file() {
        let root = std::env::temp_dir().join(format!(
            "gl-stage-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let template = root.join("template.exe");
        fs::write(&template, b"runner-a").unwrap();
        let runtime = root.join("runtime");
        let first = stage_runner(&template, &runtime, "10", "game/eldenring.exe").unwrap();
        assert!(
            first.path.ends_with(
                std::path::Path::new("10")
                    .join("game")
                    .join("eldenring.exe")
            )
        );
        assert!(first.created);
        let second = stage_runner(&template, &runtime, "10", "game/eldenring.exe").unwrap();
        assert_eq!(second.path, first.path);
        assert!(!second.created);
        fs::write(&template, b"runner-b-longer").unwrap();
        let third = stage_runner(&template, &runtime, "10", "game/eldenring.exe").unwrap();
        assert_ne!(third.path, first.path);
        assert!(third.created);
        assert_eq!(fs::read(&first.path).unwrap(), b"runner-a");
        cleanup_staged(&runtime, &third);
        assert!(!third.path.exists());
        cleanup_staged(
            &runtime,
            &Staged {
                path: first.path.clone(),
                created: true,
            },
        );
        assert!(!first.path.exists());
        let _ = fs::remove_dir_all(root);
    }
}
