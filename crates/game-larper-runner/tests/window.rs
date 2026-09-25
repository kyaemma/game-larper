use std::os::windows::io::AsRawHandle;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT};
use windows_sys::Win32::System::Threading::WaitForInputIdle;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowRect,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};

const WS_EX_APPWINDOW: isize = 0x0004_0000;
const WS_EX_NOACTIVATE: isize = 0x0800_0000;

#[test]
fn native_runner_creates_a_visible_offscreen_hwnd_and_exits_cleanly() {
    let source = runner_binary();
    let directory = std::env::temp_dir().join(format!(
        "game-larper-runner-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let executable = directory.join("eldenring.exe");
    std::fs::copy(&source, &executable).unwrap();
    let mut child = Command::new(&executable)
        .current_dir(&directory)
        .spawn()
        .expect("runner did not start");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let idle = unsafe { WaitForInputIdle(child.as_raw_handle() as _, 2000) };
        assert_eq!(idle, 0, "runner never became input-idle");
        let window = find_visible_window(child.id(), Duration::from_secs(2));
        assert!(
            !window.is_null(),
            "runner did not create a visible top-level window"
        );
        assert_eq!(title(window), "eldenring");
        let style = unsafe { GetWindowLongPtrW(window, GWL_EXSTYLE) };
        assert_ne!(style & WS_EX_APPWINDOW, 0);
        assert_ne!(style & WS_EX_NOACTIVATE, 0);
        let mut rectangle = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        assert_ne!(unsafe { GetWindowRect(window, &mut rectangle) }, 0);
        assert!(rectangle.left < -10000 && rectangle.top < -10000);
        assert_ne!(unsafe { PostMessageW(window, WM_CLOSE, 0, 0) }, 0);
        let finished = child.wait_timeout(Duration::from_secs(3));
        assert!(finished, "runner did not exit cleanly");
        let status = child.wait().unwrap();
        assert_eq!(status.code(), Some(0));
    }));
    if child.try_wait().ok().flatten().is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&directory);
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

fn runner_binary() -> PathBuf {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_game_larper_runner") {
        return PathBuf::from(path);
    }
    let mut path = std::env::current_exe().expect("the test executable path");
    path.pop();
    if path.file_name().and_then(|name| name.to_str()) == Some("deps") {
        path.pop();
    }
    path.join("game-larper-runner.exe")
}

fn find_visible_window(process_id: u32, budget: Duration) -> HWND {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(window) = enum_visible(process_id) {
            return window;
        }
        if Instant::now() >= deadline {
            return std::ptr::null_mut();
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn enum_visible(process_id: u32) -> Option<HWND> {
    let mut found: HWND = std::ptr::null_mut();
    let mut state = EnumState {
        process_id,
        found: &mut found,
    };
    unsafe {
        EnumWindows(Some(enum_proc), &mut state as *mut EnumState as LPARAM);
    }
    if found.is_null() { None } else { Some(found) }
}

struct EnumState<'a> {
    process_id: u32,
    found: &'a mut HWND,
}

unsafe extern "system" fn enum_proc(window: HWND, context: LPARAM) -> i32 {
    // SAFETY: context is the EnumState pointer passed to EnumWindows, alive for the call.
    let state = unsafe { &mut *(context as *mut EnumState) };
    let mut owner = 0_u32;
    unsafe {
        GetWindowThreadProcessId(window, &mut owner);
    }
    let owned = unsafe { GetWindow(window, GW_OWNER) };
    if owner == state.process_id && owned.is_null() && unsafe { IsWindowVisible(window) } != 0 {
        *state.found = window;
        return 0;
    }
    1
}

fn title(window: HWND) -> String {
    let mut buffer = [0u16; 260];
    let length = unsafe { GetWindowTextW(window, buffer.as_mut_ptr(), buffer.len() as i32) };
    if length <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..length as usize])
}

trait WaitTimeout {
    fn wait_timeout(&mut self, budget: Duration) -> bool;
}

impl WaitTimeout for Child {
    fn wait_timeout(&mut self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        loop {
            if self.try_wait().ok().flatten().is_some() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
