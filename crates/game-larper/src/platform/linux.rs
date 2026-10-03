//! Linux desktop glue (experimental; see docs/LINUX.md).
//!
//! - Single instance: an advisory `flock` on a lock file decides who is primary. The kernel
//!   drops it when the holder dies, so a crash never leaves a stale lock, and only the holder
//!   ever removes or binds the activation socket next to it, so two starts cannot race for it.
//!   Both live in `$XDG_RUNTIME_DIR` (per-user, 0700 by the base-directory spec), else in the
//!   data root. A second start knocks on the socket and exits.
//! - Startup: an XDG autostart entry, with `Exec=` quoted by the Desktop Entry rules rather
//!   than the Windows ones.
//! - Clipboard: arboard (already linked by Slint's winit backend), which uses the Wayland
//!   data-control protocol when the compositor offers it and X11 (or XWayland) otherwise. The
//!   clipboard object is kept alive because X11 selections are served by their owner.
//! - Frame styling, owner windows and taskbar hints are Win32 ideas with no portable Wayland
//!   equivalent, so those hooks do nothing; the docked panel degrades to a normal window.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::Duration;

use game_larper_core::AppPaths;

use crate::log::{Area, Log, redact};
use crate::platform::WorkArea;

const AUTOSTART_FILE: &str = "game-larper.desktop";
/// The Settings label for `set_run_at_startup`: an XDG autostart entry starts with the session.
pub const STARTUP_LABEL: &str = "Launch at login";

/// The primary instance's lock (held for the process lifetime) and its activation socket.
struct Instance {
    _lock: File,
    listener: Option<UnixListener>,
}

static INSTANCE: Mutex<Option<Instance>> = Mutex::new(None);
static CLIPBOARD: Mutex<Option<arboard::Clipboard>> = Mutex::new(None);

/// Where the lock and the socket live: `$XDG_RUNTIME_DIR` when it is usable, else the data root.
fn ipc_paths(paths: &AppPaths) -> (PathBuf, PathBuf) {
    match std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|directory| directory.is_absolute() && directory.is_dir())
    {
        Some(runtime) => (
            runtime.join("game-larper.lock"),
            runtime.join("game-larper.sock"),
        ),
        None => (
            paths.root.join("instance.lock"),
            paths.root.join("instance.sock"),
        ),
    }
}

/// Become the primary instance, or wake the one already running and return false.
pub fn claim_primary_instance(paths: &AppPaths, log: &Log) -> bool {
    let (lock_path, socket_path) = ipc_paths(paths);
    if let Some(parent) = lock_path.parent()
        && let Err(error) = fs::create_dir_all(parent)
    {
        log.warn(
            Area::App,
            format!("Single-instance folder unavailable ({error}); not enforcing one instance"),
        );
        return true;
    }
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) => {
            log.warn(
                Area::App,
                format!(
                    "Single-instance lock {} unavailable ({error}); not enforcing one instance",
                    redact(&lock_path)
                ),
            );
            return true;
        }
    };
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => {
            match UnixStream::connect(&socket_path).and_then(|mut stream| {
                stream.set_write_timeout(Some(Duration::from_secs(1)))?;
                stream.write_all(b"activate\n")
            }) {
                Ok(()) => log.debug(Area::App, "Activation sent to the running instance"),
                Err(error) => log.warn(
                    Area::App,
                    format!("The running instance did not answer its socket: {error}"),
                ),
            }
            return false;
        }
        Err(fs::TryLockError::Error(error)) => {
            log.warn(
                Area::App,
                format!("Single-instance lock failed ({error}); not enforcing one instance"),
            );
            return true;
        }
    }
    // We hold the lock, so any socket file left here belongs to a dead primary.
    if fs::symlink_metadata(&socket_path).is_ok_and(|meta| meta.file_type().is_socket()) {
        let _ = fs::remove_file(&socket_path);
        log.debug(Area::App, "Removed a stale activation socket");
    }
    let listener = match UnixListener::bind(&socket_path) {
        Ok(listener) => {
            let _ = fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o600));
            log.debug(
                Area::App,
                format!("Activation socket {}", redact(&socket_path)),
            );
            Some(listener)
        }
        Err(error) => {
            // Still the only instance; a second start just cannot bring this one forward.
            log.warn(
                Area::App,
                format!(
                    "Activation socket {} unavailable: {error}",
                    redact(&socket_path)
                ),
            );
            None
        }
    };
    *INSTANCE.lock().unwrap_or_else(|poison| poison.into_inner()) = Some(Instance {
        _lock: lock,
        listener,
    });
    true
}

/// Call `on_signal` each time a second start knocks on the activation socket.
pub fn watch_activation(on_signal: impl Fn() + Send + 'static) {
    let listener = INSTANCE
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_mut()
        .and_then(|instance| instance.listener.take());
    let Some(listener) = listener else {
        return;
    };
    std::thread::spawn(move || {
        for mut stream in listener.incoming().map_while(Result::ok) {
            // The message does not matter; read a little so the peer is not cut off mid-write.
            let _ = stream.set_read_timeout(Some(Duration::from_millis(200)));
            let _ = stream.read(&mut [0u8; 16]);
            on_signal();
        }
    });
}

/// Create or remove `$XDG_CONFIG_HOME/autostart/game-larper.desktop`.
pub fn set_run_at_startup(
    enabled: bool,
    start_minimized: bool,
    executable: &Path,
) -> Result<(), String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })
        .ok_or_else(|| "The per-user config folder is unknown.".to_string())?;
    let directory = config.join("autostart");
    let entry = directory.join(AUTOSTART_FILE);
    if !enabled {
        return match fs::remove_file(&entry) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("The autostart entry could not be removed: {error}")),
        };
    }
    let executable = executable
        .to_str()
        .ok_or_else(|| "The executable path is not valid UTF-8.".to_string())?;
    let mut arguments = vec![executable];
    if start_minimized {
        arguments.push("--minimized");
    }
    let exec = desktop_exec(&arguments).ok_or_else(|| {
        "The executable path cannot be written to an autostart entry.".to_string()
    })?;
    let contents = format!(
        "[Desktop Entry]\nType=Application\nName=Game Larper\nComment=Start Game Larper at login\nExec={exec}\nTerminal=false\n"
    );
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    // Write a sibling, then rename over the entry, so a half-written file is never read.
    let staging = directory.join(format!(".{AUTOSTART_FILE}.{}", std::process::id()));
    let written = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(&staging)
        .and_then(|mut file| file.write_all(contents.as_bytes()).and(file.sync_all()))
        .and_then(|()| fs::rename(&staging, &entry));
    if let Err(error) = written {
        let _ = fs::remove_file(&staging);
        return Err(format!("The autostart entry could not be written: {error}"));
    }
    Ok(())
}

/// Characters the Desktop Entry spec reserves inside `Exec=` arguments.
const RESERVED: &[char] = &[
    ' ', '\t', '\n', '"', '\'', '\\', '>', '<', '~', '|', '&', ';', '$', '*', '?', '#', '(', ')',
    '`',
];

/// Build an `Exec=` value from literal arguments, following the Desktop Entry spec
/// (<https://specifications.freedesktop.org/desktop-entry-spec/latest/exec-variables.html>):
/// `%` becomes `%%` (field codes), an argument with a reserved character is double-quoted with
/// `"`, `` ` ``, `$` and `\` backslash-escaped inside, and finally the whole value gets the
/// string-type escaping, which doubles every backslash again. Control characters cannot be
/// represented safely, so such an argument is refused.
pub fn desktop_exec(arguments: &[&str]) -> Option<String> {
    let mut quoted = Vec::with_capacity(arguments.len());
    for argument in arguments {
        if argument.chars().any(char::is_control) {
            return None;
        }
        let argument = argument.replace('%', "%%");
        if argument.is_empty() || argument.contains(RESERVED) {
            let mut text = String::from("\"");
            for character in argument.chars() {
                if matches!(character, '"' | '`' | '$' | '\\') {
                    text.push('\\');
                }
                text.push(character);
            }
            text.push('"');
            quoted.push(text);
        } else {
            quoted.push(argument);
        }
    }
    Some(quoted.join(" ").replace('\\', "\\\\"))
}

pub fn open_in_explorer(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path).map_err(|error| error.to_string())?;
    let mut child = Command::new("xdg-open")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("xdg-open: {error}"))?;
    // xdg-open hands off to the desktop and exits; reap it so it does not linger as a zombie.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Compositors draw their own frames; there is no DWM to ask for corners.
pub fn style_frame(_window: &slint::Window) {}

/// No portable owner-window hint (Wayland has none for unrelated toplevels).
pub fn set_owner(_window: &slint::Window, _owner: &slint::Window) {}

/// winit only exposes skip-taskbar on Windows.
pub fn set_skip_taskbar(_window: &slint::Window, _skip: bool) {}

/// The bounds of the monitor showing `window`. Panels and struts are not subtracted (winit does
/// not read `_NET_WORKAREA`, Wayland exposes none), and under Wayland the compositor may ignore
/// window positions entirely, so the docked panel is best effort there.
pub fn work_area(window: &slint::Window) -> Option<WorkArea> {
    use slint::winit_030::WinitWindowAccessor;
    window
        .with_winit_window(|window| {
            let monitor = window
                .current_monitor()
                .or_else(|| window.primary_monitor())?;
            let origin = monitor.position();
            Some(WorkArea {
                left: origin.x,
                right: origin.x + monitor.size().width as i32,
            })
        })
        .flatten()
}

/// Put plain text on the clipboard. The clipboard object stays alive for later pastes.
pub fn copy_text(text: &str) -> Result<(), String> {
    let mut slot = CLIPBOARD
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if slot.is_none() {
        *slot = Some(
            arboard::Clipboard::new()
                .map_err(|error| format!("The clipboard is unavailable: {error}"))?,
        );
    }
    slot.as_mut().map_or(
        Err("The clipboard is unavailable.".to_string()),
        |clipboard| {
            clipboard
                .set_text(text.to_owned())
                .map_err(|error| format!("The clipboard could not be written: {error}"))
        },
    )
}

/// One line about the desktop session for the startup log, without personal values.
pub fn describe_session() -> Option<String> {
    let state = |variable: &str| {
        if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
            "set"
        } else {
            "unset"
        }
    };
    let value = |variable: &str| std::env::var(variable).unwrap_or_else(|_| "unset".into());
    Some(format!(
        "Linux session: XDG_SESSION_TYPE={} XDG_CURRENT_DESKTOP={} DISPLAY={} WAYLAND_DISPLAY={} (experimental platform; the tray needs a StatusNotifierItem host)",
        value("XDG_SESSION_TYPE"),
        value("XDG_CURRENT_DESKTOP"),
        state("DISPLAY"),
        state("WAYLAND_DISPLAY")
    ))
}

#[cfg(test)]
mod tests {
    use super::desktop_exec;

    #[test]
    fn plain_arguments_stay_bare() {
        assert_eq!(
            desktop_exec(&["/usr/bin/game-larper", "--minimized"]).as_deref(),
            Some("/usr/bin/game-larper --minimized")
        );
    }

    #[test]
    fn reserved_characters_are_quoted_and_escaped() {
        assert_eq!(
            desktop_exec(&["/home/kya/My Games/game-larper"]).as_deref(),
            Some("\"/home/kya/My Games/game-larper\"")
        );
        assert_eq!(
            desktop_exec(&["/opt/a\"b$c`d/game-larper"]).as_deref(),
            Some("\"/opt/a\\\\\"b\\\\$c\\\\`d/game-larper\"")
        );
        // A literal backslash ends up as four, as the spec spells out.
        assert_eq!(
            desktop_exec(&["/opt/a\\b"]).as_deref(),
            Some("\"/opt/a\\\\\\\\b\"")
        );
        assert_eq!(
            desktop_exec(&["/opt/~games/run"]).as_deref(),
            Some("\"/opt/~games/run\"")
        );
    }

    #[test]
    fn percent_signs_are_not_field_codes() {
        assert_eq!(
            desktop_exec(&["/opt/100%/game-larper"]).as_deref(),
            Some("/opt/100%%/game-larper")
        );
    }

    #[test]
    fn control_characters_are_refused() {
        assert_eq!(desktop_exec(&["/opt/bad\nExec=evil"]), None);
        assert_eq!(desktop_exec(&["/opt/tab\there"]), None);
    }
}
