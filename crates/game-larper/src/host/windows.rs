//! Win32 runner mechanics: `CreateProcessW`, the kill-on-close job object, the window
//! search, `WM_CLOSE` then `TerminateProcess`, and a duplicated process handle for the exit
//! watcher. Moved here unchanged from the single-target host.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

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

use super::{LaunchReport, RunnerHost, Staged};
use crate::log::{Area, Log, redact};
use crate::platform::{self, integrity_of};

const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

/// How `LaunchReport::hwnd` is labelled in the log.
pub const WINDOW_LABEL: &str = "HWND";

/// The runner the host owns: its process handle, top-level window, and the icon set on it.
pub(super) struct Running {
    pub(super) pid: u32,
    process: isize,
    hwnd: isize,
    icon: isize,
    staged: Staged,
    generation: u64,
}

/// A duplicated process handle, signalled when the runner exits.
pub struct ExitWatch(isize);

/// Wait on a background thread for the runner to exit, then call `on_exit`.
pub fn watch_exit(watch: ExitWatch, on_exit: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        unsafe {
            WaitForSingleObject(
                watch.0 as HANDLE,
                windows_sys::Win32::System::Threading::INFINITE,
            );
            CloseHandle(watch.0 as HANDLE);
        }
        on_exit();
    });
}

/// The job object that takes every runner down with Game Larper, or 0 when unavailable.
pub(super) fn kill_on_close_job(log: &Arc<Log>) -> isize {
    let job = create_kill_on_close_job();
    if job == 0 {
        log.warn(
            Area::Runner,
            format!(
                "Kill-on-close job unavailable ({}); runners may outlive Game Larper",
                io::Error::last_os_error()
            ),
        );
    }
    job
}

pub(super) fn close_job(job: isize) {
    if job != 0 {
        unsafe {
            CloseHandle(job as HANDLE);
        }
    }
}

impl RunnerHost {
    pub(super) fn spawn_staged(
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
            self.cleanup(&staged);
            return self.fail("The runtime path cannot be quoted safely.".into());
        }
        self.log.debug(
            Area::Runner,
            format!("CreateProcess in {}", redact(directory)),
        );
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
            self.cleanup(&staged);
            return self.fail(format!("The runner did not start: {error}"));
        }
        self.log.debug(
            Area::Runner,
            format!("Process created PID={}", info.dwProcessId),
        );
        unsafe {
            CloseHandle(info.hThread);
        }
        if self.job != 0
            && unsafe { AssignProcessToJobObject(self.job as HANDLE, info.hProcess) } == 0
        {
            self.log.warn(
                Area::Runner,
                format!(
                    "PID={} could not join the kill-on-close job: {}",
                    info.dwProcessId,
                    io::Error::last_os_error()
                ),
            );
        }
        let waited = Instant::now();
        let idle = unsafe { WaitForInputIdle(info.hProcess, 1500) };
        let hwnd = find_owned_window(info.dwProcessId, Duration::from_millis(1500));
        let alive = process_alive(info.hProcess);
        self.log.debug(
            Area::Runner,
            format!(
                "Window search took {} ms: HWND=0x{hwnd:X} alive={alive} input-idle={}",
                waited.elapsed().as_millis(),
                if idle == 0 { "ready" } else { "timed out" }
            ),
        );
        if !alive || hwnd == 0 {
            let exit = exit_code(info.hProcess);
            unsafe {
                TerminateProcess(info.hProcess, 1);
                WaitForSingleObject(info.hProcess, 2000);
                CloseHandle(info.hProcess);
            }
            self.cleanup(&staged);
            return self.fail(if alive {
                "The native runner did not create its game window.".into()
            } else {
                format!("The native runner exited during startup (exit code {exit}).")
            });
        }
        let waiter = match duplicate_handle(info.hProcess) {
            Ok(waiter) => waiter,
            Err(error) => {
                unsafe {
                    TerminateProcess(info.hProcess, 1);
                    WaitForSingleObject(info.hProcess, 2000);
                    CloseHandle(info.hProcess);
                }
                self.cleanup(&staged);
                return self.fail(error);
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
            exit_watch: ExitWatch(waiter as isize),
        };
        let icon_handle = icon
            .and_then(|path| platform::apply_window_icon(hwnd, path))
            .unwrap_or(0);
        self.log.debug(
            Area::Runner,
            match (icon, icon_handle) {
                (None, _) => "No artwork for the window icon".to_string(),
                (Some(_), 0) => "Window icon could not be applied".to_string(),
                (Some(path), _) => format!("Window icon from {}", redact(path)),
            },
        );
        self.current = Some(Running {
            pid: info.dwProcessId,
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
        self.log.info(
            Area::Runner,
            format!(
                "Stopping PID={} HWND=0x{:X} (generation {})",
                running.pid, running.hwnd, running.generation
            ),
        );
        let started = Instant::now();
        if running.hwnd != 0 {
            if unsafe { PostMessageW(running.hwnd as HWND, WM_CLOSE, 0, 0) } == 0 {
                self.log.warn(
                    Area::Runner,
                    format!(
                        "WM_CLOSE could not be posted: {}",
                        io::Error::last_os_error()
                    ),
                );
            }
            unsafe {
                WaitForSingleObject(process, 2000);
            }
        }
        if process_alive(process) {
            self.log.warn(
                Area::Runner,
                format!(
                    "PID={} still running {} ms after WM_CLOSE; terminating it",
                    running.pid,
                    started.elapsed().as_millis()
                ),
            );
            unsafe {
                TerminateProcess(process, 0);
                WaitForSingleObject(process, 5000);
            }
        } else {
            self.log.debug(
                Area::Runner,
                format!(
                    "PID={} closed gracefully in {} ms",
                    running.pid,
                    started.elapsed().as_millis()
                ),
            );
        }
        let exited = !process_alive(process);
        unsafe {
            CloseHandle(process);
        }
        platform::destroy_icon(running.icon);
        self.cleanup(&running.staged);
        self.stopping = false;
        if !exited {
            self.log.error(
                Area::Runner,
                format!("PID={} is still running after termination", running.pid),
            );
            self.current = Some(running);
            return Err("The owned runner did not exit.".into());
        }
        self.log
            .info(Area::Runner, format!("PID={} stopped", running.pid));
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
        self.log.warn(
            Area::Runner,
            format!(
                "PID={} exited on its own with code {} (generation {generation})",
                running.pid,
                exit_code(running.process as HANDLE)
            ),
        );
        unsafe {
            CloseHandle(running.process as HANDLE);
        }
        platform::destroy_icon(running.icon);
        self.cleanup(&running.staged);
        true
    }
}

/// Any reparse point (symlink or junction), whatever it points at.
pub(super) fn is_reparse_path(path: &Path) -> io::Result<bool> {
    use std::os::windows::fs::MetadataExt;
    Ok(fs::symlink_metadata(path)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

pub(super) fn copy_template(template: &Path, destination: &Path) -> io::Result<()> {
    fs::copy(template, destination).map(|_| ())
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

/// The exit code as text, for diagnostics. 259 means the process is still running.
fn exit_code(process: HANDLE) -> String {
    let mut code = 0u32;
    if unsafe { GetExitCodeProcess(process, &mut code) } == 0 {
        return "unknown".into();
    }
    format!("{code} (0x{code:X})")
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
