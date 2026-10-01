//! Stages the native runner into the private runtime directory and keeps it running.
//!
//! Staging, cleanup, and the public types are shared. Starting, watching, and stopping
//! the process is not: Win32 uses a job object, a window handle, and a duplicated
//! process handle, while Unix uses a parent-death signal, a pipe, and `SIGTERM`.

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use game_larper_core::{Error as CoreError, resolve_executable};

use crate::platform::{self, integrity_of};

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, FALSE, HANDLE, HWND};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, PROCESS_INFORMATION, STARTUPINFOW, TerminateProcess,
    WaitForInputIdle, WaitForSingleObject,
};
#[cfg(windows)]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GetWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    PostMessageW, WM_CLOSE,
};

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
/// How long the starter waits for the runner to prove it is up.
const STARTUP_BUDGET: Duration = Duration::from_millis(1500);
#[cfg(windows)]
const RUNNER_NOT_UP: &str = "The native runner did not create its game window.";
#[cfg(not(windows))]
const RUNNER_NOT_UP: &str = "The native runner did not stay running.";

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
    #[cfg(windows)]
    process: isize,
    #[cfg(windows)]
    hwnd: isize,
    #[cfg(not(windows))]
    child: std::process::Child,
    icon: isize,
    staged: Staged,
    generation: u64,
}

pub struct RunnerHost {
    runtime: PathBuf,
    #[cfg(windows)]
    job: isize,
    current: Option<Running>,
    next_generation: u64,
    stopping: bool,
}

impl RunnerHost {
    pub fn new(runtime: PathBuf) -> Self {
        Self {
            runtime,
            #[cfg(windows)]
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
        self.spawn_staged(staged, icon)
    }

    #[cfg(windows)]
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
        let idle = unsafe { WaitForInputIdle(info.hProcess, STARTUP_BUDGET.as_millis() as u32) };
        let hwnd = find_owned_window(info.dwProcessId, STARTUP_BUDGET);
        let alive = process_alive(info.hProcess);
        if !alive || hwnd == 0 {
            unsafe {
                TerminateProcess(info.hProcess, 1);
                WaitForSingleObject(info.hProcess, 2000);
                CloseHandle(info.hProcess);
            }
            cleanup_staged(&self.runtime, &staged);
            return Err(RUNNER_NOT_UP.into());
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

    /// Start the staged runner and wait until `/proc` shows it running as itself.
    #[cfg(not(windows))]
    fn spawn_staged(
        &mut self,
        staged: Staged,
        _icon: Option<&Path>,
    ) -> Result<LaunchReport, String> {
        let directory = staged
            .path
            .parent()
            .ok_or_else(|| "The runner path has no directory.".to_string())?;
        let basename = staged
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "game".into());
        let mut pipe: [libc::c_int; 2] = [-1, -1];
        // SAFETY: the pipe writes two fresh descriptors into our own array.
        if unsafe { libc::pipe(pipe.as_mut_ptr()) } != 0 {
            cleanup_staged(&self.runtime, &staged);
            return Err(format!(
                "The runner did not start: {}",
                io::Error::last_os_error()
            ));
        }
        let mut command = std::process::Command::new(&staged.path);
        command.current_dir(directory);
        use std::os::unix::process::CommandExt;
        // The Win32 job object took the runner down with us; the kernel does it here.
        // SAFETY: prctl only arms a signal for this child, before it execs.
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                // SAFETY: both ends come from the pipe above and are closed exactly once.
                unsafe {
                    libc::close(pipe[0]);
                    libc::close(pipe[1]);
                }
                cleanup_staged(&self.runtime, &staged);
                return Err(format!("The runner did not start: {error}"));
            }
        };
        // Our copy of the write end closes here, so the read end reaches EOF exactly
        // when the runner is gone. That fd is handed to the exit watcher on success.
        unsafe {
            libc::close(pipe[1]);
        }
        let waiter = pipe[0];
        let deadline = std::time::Instant::now() + STARTUP_BUDGET;
        loop {
            if child.try_wait().ok().flatten().is_some() {
                unsafe {
                    libc::close(waiter);
                }
                cleanup_staged(&self.runtime, &staged);
                return Err(RUNNER_NOT_UP.into());
            }
            if runner_identity(child.id(), &basename) {
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                unsafe {
                    libc::close(waiter);
                }
                cleanup_staged(&self.runtime, &staged);
                return Err(RUNNER_NOT_UP.into());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let pid = child.id();
        let title = window_title_of(&basename);
        let report = LaunchReport {
            generation,
            pid,
            executable: staged.path.clone(),
            basename,
            working_directory: directory.to_path_buf(),
            hwnd: 0,
            title,
            integrity: integrity_of(pid),
            waiter: waiter as isize,
        };
        self.current = Some(Running {
            child,
            icon: 0,
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
        let mut running = running;
        let exited = stop_process(&mut running);
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
        // Unix reaps the exit status below; Win32 only reads the handle.
        #[cfg_attr(windows, allow(unused_mut))]
        let Some(mut running) = self.current.take() else {
            return false;
        };
        if running.generation != generation {
            self.current = Some(running);
            return false;
        }
        #[cfg(windows)]
        unsafe {
            CloseHandle(running.process as HANDLE);
        }
        #[cfg(not(windows))]
        reap(&mut running);
        platform::destroy_icon(running.icon);
        cleanup_staged(&self.runtime, &running.staged);
        true
    }
}

impl Drop for RunnerHost {
    fn drop(&mut self) {
        let _ = self.stop();
        #[cfg(windows)]
        if self.job != 0 {
            unsafe {
                CloseHandle(self.job as HANDLE);
            }
        }
    }
}

/// Collect the runner's exit status if it has already reported one.
#[cfg(not(windows))]
fn reap(running: &mut Running) {
    // The watcher only sees EOF once every writer is gone, so the runner has
    // exited; a non-blocking reap is enough to keep it from lingering as a zombie.
    match running.child.try_wait() {
        Ok(Some(status)) => eprintln!("GL-DEBUG runner exit status: {status}"),
        Ok(None) => eprintln!("GL-DEBUG runner still running at reap?!"),
        Err(error) => eprintln!("GL-DEBUG runner try_wait error: {error}"),
    }
}

#[cfg(windows)]
fn stop_process(running: &mut Running) -> bool {
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
    exited
}

#[cfg(not(windows))]
fn stop_process(running: &mut Running) -> bool {
    let pid = running.child.id() as libc::pid_t;
    // SIGTERM is the polite WM_CLOSE, SIGKILL the TerminateProcess after it.
    // SAFETY: signalling our own child with a standard signal.
    unsafe {
        libc::kill(pid, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if running.child.try_wait().ok().flatten().is_some() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = running.child.kill();
    running.child.wait().is_ok()
}

/// What `/proc` says this process is: its `comm`, or a command line argument
/// naming the staged file — a script is executed as `sh <path>`, so only the
/// real binary carries the name in `argv[0]`. Stands in for enumerating its windows.
#[cfg(not(windows))]
fn runner_identity(pid: u32, expected: &str) -> bool {
    use std::os::unix::ffi::OsStrExt;
    if fs::read_to_string(format!("/proc/{pid}/comm"))
        .is_ok_and(|comm| comm.trim_end() == expected)
    {
        return true;
    }
    let Ok(command) = fs::read(format!("/proc/{pid}/cmdline")) else {
        return false;
    };
    command
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .any(|argument| {
            Path::new(std::ffi::OsStr::from_bytes(argument))
                .file_name()
                .is_some_and(|name| name == std::ffi::OsStr::new(expected))
        })
}

/// The title the Linux runner gives its window: the executable name without `.exe`.
#[cfg(not(windows))]
fn window_title_of(basename: &str) -> String {
    basename
        .strip_suffix(".exe")
        .unwrap_or(basename)
        .to_string()
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
        if is_reparse_path(runtime_root, destination)? {
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
    if is_reparse_path(&root, &root)? {
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
    let mut current = root.clone();
    for component in relative.components() {
        current.push(component);
        if !current.exists() {
            fs::create_dir(&current)?;
        }
        if is_reparse_path(&root, &current)? {
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
        if is_reparse_path(&root, &current).unwrap_or(true) {
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
        if is_reparse_path(root, current).unwrap_or(true) {
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

/// Windows: any reparse point, whatever it points at.
#[cfg(windows)]
fn is_reparse_path(_root: &Path, path: &Path) -> io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    Ok(fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Unix: a link only matters when it leads out of the private runtime directory,
/// so a data directory behind a symlink of one's own still works.
#[cfg(not(windows))]
fn is_reparse_path(root: &Path, path: &Path) -> io::Result<bool> {
    if !fs::symlink_metadata(path)?.file_type().is_symlink() {
        return Ok(false);
    }
    let (Ok(root), Ok(path)) = (fs::canonicalize(root), fs::canonicalize(path)) else {
        return Ok(true);
    };
    Ok(!path.starts_with(&root))
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

#[cfg(windows)]
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

#[cfg(windows)]
struct EnumState<'a> {
    process_id: u32,
    found: &'a mut HWND,
}

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
fn process_alive(process: HANDLE) -> bool {
    let mut code = 0u32;
    let read = unsafe { GetExitCodeProcess(process, &mut code) };
    read != 0 && code == 259
}

#[cfg(windows)]
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

#[cfg(windows)]
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

#[cfg(windows)]
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
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

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "gl-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn stage_reuses_identical_bytes_and_isolates_a_different_file() {
        let root = scratch("stage");
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

    /// Stage, start, watch, and stop a stand-in runner: the whole Linux host path.
    /// The template is a script so the test does not depend on the runner being built.
    #[cfg(not(windows))]
    #[test]
    fn host_starts_watches_and_stops_a_staged_process() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("host");
        let template = root.join("template");
        fs::write(&template, "#!/bin/sh\nwhile :; do sleep 1; done\n").unwrap();
        fs::set_permissions(&template, fs::Permissions::from_mode(0o755)).unwrap();
        let runtime = root.join("runtime");
        let mut host = super::RunnerHost::new(runtime);

        let report = host
            .launch(&template, "10", "game/eldenring.exe", None)
            .expect("the staged process did not start");
        assert_eq!(report.basename, "eldenring.exe");
        assert_eq!(report.title, "eldenring");
        assert_eq!(report.hwnd, 0, "there is no window handle on Unix");
        assert!(report.integrity.starts_with("uid:"), "{}", report.integrity);
        assert!(report.pid > 0);
        assert!(report.executable.exists());

        // A crash: the watcher's pipe reaches EOF, the host cleans up after it.
        // SAFETY: signalling the process this test owns.
        unsafe {
            libc::kill(report.pid as libc::pid_t, libc::SIGKILL);
        }
        assert!(pipe_ends_at_eof(report.waiter), "the exit watcher would hang");
        assert!(host.take_unexpected_exit(report.generation));
        assert!(!report.executable.exists(), "the staged copy was left behind");

        // A deliberate stop is not an unexpected exit.
        let second = host
            .launch(&template, "10", "game/eldenring.exe", None)
            .expect("the staged process did not restart");
        host.stop().expect("the staged process did not stop");
        assert!(pipe_ends_at_eof(second.waiter));
        assert!(!host.take_unexpected_exit(second.generation));
        assert!(!second.executable.exists());
        let _ = fs::remove_dir_all(root);
    }

    /// Read the watcher's pipe: it only ends when every writer is gone.
    #[cfg(not(windows))]
    fn pipe_ends_at_eof(waiter: isize) -> bool {
        use std::io::Read;
        use std::os::fd::FromRawFd;
        // SAFETY: the test owns this read end; nothing else closes it.
        let mut pipe = unsafe { std::fs::File::from_raw_fd(waiter as i32) };
        let mut buffer = [0u8; 8];
        let mut ended = false;
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => {
                    ended = true;
                    break;
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        drop(pipe);
        ended
    }
}
