//! The Linux runner's contract: a process `/proc` identifies as the staged
//! executable, an X11 window a compositor can enumerate when a display is
//! reachable, and a clean stop on `SIGTERM` — the same signals the host uses.
#![cfg(not(windows))]

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, MapState, Window};

/// The staged executable name: the `/proc` command line, the `WM_CLASS` class,
/// and the Wayland app id a scanner would read.
const EXE: &str = "eldenring.exe";
/// The window title, which is also the `WM_CLASS` instance — the Win32 runner's title.
const STEM: &str = "eldenring";

#[test]
fn linux_runner_reports_its_identity_and_maps_an_offscreen_window() {
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
    let executable = directory.join(EXE);
    std::fs::copy(&source, &executable).unwrap();
    let mut child = Command::new(&executable)
        .current_dir(&directory)
        .spawn()
        .expect("runner did not start");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let identity = wait_for_identity(child.id(), Duration::from_secs(2));
        assert_eq!(
            identity.as_deref(),
            Some(EXE),
            "runner did not show the staged basename as its command line"
        );
        // Without a display server there is nothing to map, but the process still counts.
        if display_reachable() && x11_server_available() {
            let (title, class, position) = wait_for_window(Duration::from_secs(3))
                .expect("runner did not map its X11 window");
            assert_eq!(title, STEM, "window title differs from the Win32 runner");
            assert_eq!(class, EXE, "WM_CLASS class is not the executable name");
            assert!(
                position.0 < -10_000 && position.1 < -10_000,
                "window is not parked off-screen: {position:?}"
            );
        }
        // The host stops the runner with SIGTERM first.
        // SAFETY: signalling our own child process.
        unsafe {
            libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
        }
        assert!(
            wait_for_exit(&mut child, Duration::from_secs(3)),
            "runner did not exit on SIGTERM"
        );
        assert_eq!(
            child.wait().unwrap().code(),
            None,
            "SIGTERM should be the cause"
        );
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

/// The staged executable's own name, as a process scanner would read it.
fn wait_for_identity(pid: u32, budget: Duration) -> Option<String> {
    let deadline = Instant::now() + budget;
    loop {
        if let Some(name) = command_name(pid) {
            return Some(name);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn command_name(pid: u32) -> Option<String> {
    let command = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let first = command.split(|byte| *byte == 0).next()?;
    let name = Path::new(std::ffi::OsStr::from_bytes(first)).file_name()?;
    Some(name.to_string_lossy().into_owned())
}

fn display_reachable() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty())
}

/// Whether there is an X server to ask about windows at all.
fn x11_server_available() -> bool {
    x11rb::connect(None).is_ok()
}

/// The runner's mapped top-level window: title, `WM_CLASS` class, and position.
fn wait_for_window(budget: Duration) -> Option<(String, String, (i16, i16))> {
    let (connection, screen) = x11rb::connect(None).ok()?;
    let root = connection.setup().roots[screen].root;
    let title_atom = connection
        .intern_atom(false, b"WM_NAME")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let class_atom = connection
        .intern_atom(false, b"WM_CLASS")
        .ok()?
        .reply()
        .ok()?
        .atom;
    let deadline = Instant::now() + budget;
    loop {
        if let Some(found) = scan(&connection, root, title_atom, class_atom) {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn scan(
    connection: &x11rb::rust_connection::RustConnection,
    root: Window,
    title_atom: u32,
    class_atom: u32,
) -> Option<(String, String, (i16, i16))> {
    let children = connection.query_tree(root).ok()?.reply().ok()?.children;
    for window in children {
        let Some(class) = property_string(connection, window, class_atom) else {
            continue;
        };
        // WM_CLASS is `instance\0class\0`; the class is the staged executable name.
        let mut fields = class.split('\0').filter(|field| !field.is_empty());
        let (Some(instance), Some(class)) = (fields.next(), fields.next()) else {
            continue;
        };
        if instance != STEM || class != EXE {
            continue;
        }
        let attributes = connection
            .get_window_attributes(window)
            .ok()?
            .reply()
            .ok()?;
        if attributes.map_state != MapState::VIEWABLE {
            continue;
        }
        let geometry = connection.get_geometry(window).ok()?.reply().ok()?;
        let title = property_string(connection, window, title_atom).unwrap_or_default();
        return Some((title, class.to_string(), (geometry.x, geometry.y)));
    }
    None
}

fn property_string(
    connection: &x11rb::rust_connection::RustConnection,
    window: Window,
    property: u32,
) -> Option<String> {
    let reply = connection
        .get_property(false, window, property, AtomEnum::STRING, 0, 256)
        .ok()?
        .reply()
        .ok()?;
    let bytes = reply.value;
    let text = bytes
        .strip_suffix(&[0])
        .map(|text| text.to_vec())
        .unwrap_or(bytes);
    String::from_utf8(text).ok()
}

fn wait_for_exit(child: &mut Child, budget: Duration) -> bool {
    let deadline = Instant::now() + budget;
    loop {
        if child.try_wait().ok().flatten().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
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
    path.join("game-larper-runner")
}
