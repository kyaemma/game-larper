//! Unix implementation of the platform glue.
//!
//! Single instance and activation travel over a Unix socket in the runtime
//! directory, startup is an XDG autostart entry, and opening a folder or a URL
//! goes through `xdg-open`. There is no owner window, taskbar hint, or frame
//! decoration to set; those ideas are Win32-only, so the hooks are empty here.

use std::fs;
use std::io::{self, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use game_larper_core::{AppPaths, format_startup_command};

use crate::platform::WorkArea;

/// The socket a second copy knocks on. Held for the process lifetime.
static LISTENER: Mutex<Option<UnixListener>> = Mutex::new(None);
/// A clipboard that stays alive so paste requests can still be answered.
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

fn socket_path(paths: &AppPaths) -> PathBuf {
    paths.runtime().join("activate.sock")
}

/// Bind the activation socket. Returns false when another copy already owns it,
/// in which case that copy has just been asked to show itself.
pub fn claim_primary_instance(paths: &AppPaths) -> bool {
    let path = socket_path(paths);
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(listener) = UnixListener::bind(&path) {
        *LISTENER.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(listener);
        return true;
    }
    match UnixStream::connect(&path) {
        Ok(mut stream) => {
            let _ = stream.write_all(b"activate");
            false
        }
        // Nobody answered: a crashed copy left the file behind. Take it over.
        Err(_) => {
            let _ = fs::remove_file(&path);
            match UnixListener::bind(&path) {
                Ok(listener) => {
                    *LISTENER.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(listener);
                    true
                }
                // Without a socket the app still runs, it just cannot be re-activated.
                Err(_) => true,
            }
        }
    }
}

/// Wake the primary copy whenever a second copy connects.
pub fn watch_activation(on_signal: impl Fn() + Send + 'static) {
    let listener = LISTENER
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .take();
    let Some(listener) = listener else {
        return;
    };
    std::thread::spawn(move || {
        for connection in listener.incoming() {
            if connection.is_ok() {
                on_signal();
            }
        }
    });
}

/// An XDG autostart entry, the desktop-neutral equivalent of the Run key.
pub fn set_run_at_startup(
    enabled: bool,
    start_minimized: bool,
    executable: &Path,
) -> Result<(), String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or_else(|| "The per-user config directory is unavailable.".to_string())?;
    let directory = config.join("autostart");
    let entry = directory.join("game-larper.desktop");
    if !enabled {
        return match fs::remove_file(&entry) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("The autostart entry could not be removed: {error}")),
        };
    }
    let command =
        format_startup_command(executable, start_minimized).map_err(|error| error.to_string())?;
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let desktop = format!(
        "[Desktop Entry]\nType=Application\nName=Game Larper\nExec={command}\nTerminal=false\n"
    );
    fs::write(&entry, desktop).map_err(|error| error.to_string())
}

pub fn open_in_explorer(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// No DWM: frameless Slint windows already look the part under a compositor.
pub fn style_frame(_window: &slint::Window) {}

/// Compositors keep transient windows above their parent on their own.
pub fn set_owner(_window: &slint::Window, _owner: &slint::Window) {}

/// Wayland has no skip-taskbar hint; the panel is a normal toplevel there.
pub fn set_skip_taskbar(_window: &slint::Window, _skip: bool) {}

/// The usable horizontal span of the monitor showing `window`.
pub fn work_area(window: &slint::Window) -> Option<WorkArea> {
    use slint::winit_030::WinitWindowAccessor;
    window
        .with_winit_window(|window| {
            // Wayland exposes no struts and winit does not read X11's _NET_WORKAREA,
            // so a taskbar docked to the left or right edge is the one case this misses.
            let monitor = window
                .current_monitor()
                .or_else(|| window.primary_monitor())?;
            let origin = monitor.position();
            let size = monitor.size();
            Some(WorkArea {
                left: origin.x,
                right: origin.x + size.width as i32,
            })
        })
        .flatten()
}

/// Put plain text on the clipboard, keeping a clipboard of our own alive so
/// later pastes can be served. Falls back to the desktop's own tools.
pub fn copy_text(text: &str) -> Result<(), String> {
    if let Ok(mut slot) = CLIPBOARD.lock() {
        if slot.is_none() {
            *slot = arboard::Clipboard::new().ok();
        }
        if let Some(clipboard) = slot.as_mut()
            && clipboard.set_text(text.to_owned()).is_ok()
        {
            return Ok(());
        }
    }
    let tools: [(&str, &[&str]); 3] = [
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard", "-in"]),
        ("xsel", &["--input", "--clipboard"]),
    ];
    for (program, arguments) in tools {
        if pipe_text(program, arguments, text) {
            return Ok(());
        }
    }
    Err("The clipboard is unavailable.".into())
}

fn pipe_text(program: &str, arguments: &[&str], text: &str) -> bool {
    let child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    if !written {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    }
    child.wait().is_ok_and(|status| status.success())
}

/// The uid the runner runs as: the closest thing to an integrity level here.
pub fn integrity_of(pid: u32) -> String {
    let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status")) else {
        return "unknown".into();
    };
    status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))
        .and_then(|line| line.split_whitespace().nth(1))
        .map(|uid| format!("uid:{uid}"))
        .unwrap_or_else(|| "unknown".into())
}

/// Window icons are set by the window system, not by us.
pub fn destroy_icon(_icon: isize) {}
