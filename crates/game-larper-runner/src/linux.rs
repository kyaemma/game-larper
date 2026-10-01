//! The Linux runner (experimental until checked against a real Discord client).
//!
//! What it keeps alive:
//!
//! - **Process identity.** It is the staged file itself, so `/proc/<pid>/exe`, `argv[0]` and
//!   `/proc/<pid>/comm` all name `…/game/eldenring.exe` (`comm` is the basename, cut to 15
//!   bytes by the kernel). Linux Discord is believed to match Windows rules like this against
//!   Wine/Proton processes; that is a hypothesis, not something this code can prove.
//! - **An X11 window, when an X11 display is reachable** (X11 sessions, or XWayland under a
//!   Wayland session): 1×1, unmanaged (override-redirect, so no window manager decoration,
//!   taskbar entry or focus), parked off-screen, with `WM_CLASS`, `_NET_WM_NAME` and
//!   `_NET_WM_PID` set so another X11 client can tie it to this process.
//! - **No Wayland window.** Wayland gives other clients no way to enumerate toplevels and has no
//!   off-screen coordinates, so a toplevel would only risk focus and taskbar noise.
//!
//! Lifetime: the main thread blocks on stdin, which Game Larper holds as a pipe. EOF means Game
//! Larper closed it to stop us, or Game Larper is gone — the kernel closes its end whichever
//! way the process dies. This replaces `PR_SET_PDEATHSIG`, which fires when the *thread* that
//! forked the runner exits (prctl(2)); Game Larper launches from short-lived worker threads, so
//! that signal would end the runner right after launch. `SIGTERM`/`SIGKILL` keep their defaults.
//!
//! Diagnostics go to stderr as `game_larper_core::runner_protocol` lines, which the host turns
//! into its own structured log. Full paths are never sent; the host reads `/proc` itself.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Once;

use game_larper_core::runner_protocol::{self, Level, Ready};

mod x11;

/// Set to `none` to run without a window even when X11 is reachable (manual detection tests).
const WINDOW_ENV: &str = "GAME_LARPER_RUNNER_WINDOW";

pub fn run() -> i32 {
    let identity = Identity::current();
    say(
        Level::Debug,
        &format!(
            "Runner up: pid={} comm={} argv0={} exe={}",
            std::process::id(),
            identity.comm,
            identity.argv0,
            identity.exe
        ),
    );
    if identity.comm != truncated_comm(&identity.exe) {
        say(
            Level::Warn,
            &format!(
                "comm {:?} is not the staged basename {:?}",
                identity.comm, identity.exe
            ),
        );
    }
    say(Level::Debug, &format!("Display: {}", display_summary()));

    let window_wanted = std::env::var(WINDOW_ENV).map_or(true, |value| value != "none");
    if !window_wanted {
        say(
            Level::Info,
            &format!("{WINDOW_ENV}=none: running without a window"),
        );
        ready_without_window(&identity.title);
    } else if is_set("DISPLAY") {
        let title = identity.title.clone();
        let class = identity.exe.clone();
        std::thread::spawn(move || run_x11(&title, &class));
    } else {
        say(
            Level::Warn,
            if is_set("WAYLAND_DISPLAY") {
                "Wayland without an X11 display: no window other clients can enumerate; process identity only"
            } else {
                "No display: running without a window; process identity only"
            },
        );
        ready_without_window(&identity.title);
    }

    wait_for_lifeline();
    say(Level::Debug, "Lifeline closed; exiting");
    0
}

fn run_x11(title: &str, class: &str) {
    let window = match x11::Window::open(title, class) {
        Ok(window) => window,
        Err(error) => {
            say(
                Level::Warn,
                &format!("X11 window unavailable ({error}); process identity only"),
            );
            ready_without_window(title);
            return;
        }
    };
    say(
        Level::Debug,
        &format!(
            "X11 window 0x{:x} {}: title={title:?} WM_CLASS={class:?},{class:?} at ({}, {}) override-redirect={}",
            window.id,
            window.map_state,
            window.position.0,
            window.position.1,
            window.override_redirect
        ),
    );
    announce_ready(Ready {
        backend: "x11".into(),
        window: window.id,
        title: title.into(),
    });
    let error = window.serve();
    say(
        Level::Warn,
        &format!("X11 window gone ({error}); still running with process identity only"),
    );
}

fn ready_without_window(title: &str) {
    announce_ready(Ready {
        backend: "none".into(),
        window: 0,
        title: title.into(),
    });
}

/// The host waits for exactly one `ready` line.
fn announce_ready(ready: Ready) {
    static READY: Once = Once::new();
    READY.call_once(|| write_line(&runner_protocol::encode_ready(&ready)));
}

fn say(level: Level, message: &str) {
    write_line(&runner_protocol::encode_log(level, message));
}

fn write_line(line: &str) {
    // A closed pipe only means nobody is listening any more; the runner keeps going.
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
}

/// Block until stdin reaches EOF (or fails), which is the host saying "stop" or being gone.
fn wait_for_lifeline() {
    let mut stdin = std::io::stdin().lock();
    let mut buffer = [0u8; 64];
    loop {
        match stdin.read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

/// What a process scanner can read about this process, by basename only.
struct Identity {
    /// `/proc/self/comm`.
    comm: String,
    /// Basename of `argv[0]`.
    argv0: String,
    /// Basename of `/proc/self/exe`: the staged name, `eldenring.exe`.
    exe: String,
    /// The window title: the staged name without `.exe`, like the Win32 runner.
    title: String,
}

impl Identity {
    fn current() -> Self {
        let basename = |path: &Path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        let exe = std::env::current_exe()
            .map(|path| basename(&path))
            .unwrap_or_default();
        let argv0 = std::env::args_os()
            .next()
            .map(|argument| basename(Path::new(&argument)))
            .unwrap_or_default();
        let comm = std::fs::read_to_string("/proc/self/comm")
            .map(|comm| comm.trim_end_matches('\n').to_string())
            .unwrap_or_default();
        let title = window_title(&exe);
        Self {
            comm,
            argv0,
            exe,
            title,
        }
    }
}

/// `eldenring.exe` → `eldenring`, keeping the original case like the Win32 runner.
fn window_title(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let title = match lower.strip_suffix(".exe") {
        Some(stem) => &name[..stem.len()],
        None => name,
    };
    if title.is_empty() {
        "game".into()
    } else {
        title.into()
    }
}

/// The kernel keeps at most 15 bytes of a task name (`TASK_COMM_LEN` is 16 with the NUL).
fn truncated_comm(name: &str) -> &str {
    let mut end = name.len().min(15);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

fn is_set(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| !value.is_empty())
}

/// Which display servers this process could talk to, without their (personal) values.
fn display_summary() -> String {
    let state = |variable| if is_set(variable) { "set" } else { "unset" };
    format!(
        "DISPLAY={} WAYLAND_DISPLAY={} XDG_SESSION_TYPE={}",
        state("DISPLAY"),
        state("WAYLAND_DISPLAY"),
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unset".into())
    )
}

#[cfg(test)]
mod tests {
    use super::{truncated_comm, window_title};

    #[test]
    fn title_drops_the_exe_suffix_case_insensitively() {
        assert_eq!(window_title("eldenring.exe"), "eldenring");
        assert_eq!(window_title("Hades.EXE"), "Hades");
        assert_eq!(window_title("game-larper-runner"), "game-larper-runner");
        assert_eq!(window_title(".exe"), "game");
    }

    #[test]
    fn comm_is_cut_to_fifteen_bytes() {
        assert_eq!(truncated_comm("eldenring.exe"), "eldenring.exe");
        assert_eq!(truncated_comm("Cyberpunk2077.exe"), "Cyberpunk2077.e");
        assert_eq!(truncated_comm("ééééééééé.exe"), "ééééééé");
    }
}
