//! The Linux runner's contract, checked against the real binary staged as `eldenring.exe`:
//! the `/proc` identity, the `ready` handshake on stderr, exiting when stdin (the lifeline)
//! closes, a clean `SIGTERM`, and — when an X server is reachable — the X11 window's
//! properties. CI runs this under Xvfb with `GAME_LARPER_REQUIRE_X11=1` so the X11 half cannot
//! be skipped silently there. None of this says anything about what Discord detects.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use game_larper_core::runner_protocol::{Ready, RunnerLine, decode};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, MapState};

const EXE: &str = "eldenring.exe";
const TITLE: &str = "eldenring";
const EXEC_RETRY_BUDGET: Duration = Duration::from_millis(250);
const EXEC_RETRY_DELAY: Duration = Duration::from_millis(10);

struct Staged {
    directory: PathBuf,
    executable: PathBuf,
}

impl Staged {
    fn new(test: &str) -> Self {
        let directory = std::env::temp_dir().join(format!(
            "gl-runner-{test}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let game = directory.join("game");
        std::fs::create_dir_all(&game).unwrap();
        let executable = game.join(EXE);
        std::fs::copy(env!("CARGO_BIN_EXE_game-larper-runner"), &executable).unwrap();
        Self {
            directory,
            executable,
        }
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// A started runner with its protocol lines arriving on a channel.
struct Runner {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Runner {
    fn start(staged: &Staged, configure: impl FnOnce(&mut Command)) -> Self {
        let mut command = Command::new(&staged.executable);
        command
            .current_dir(staged.executable.parent().unwrap())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure(&mut command);
        let mut child = spawn_runner(&mut command);
        let stderr = child.stderr.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = sender.send(line);
            }
        });
        Self { child, lines }
    }

    /// Lines until `ready`, which must arrive within `budget`.
    fn wait_ready(&self, budget: Duration) -> (Ready, Vec<String>) {
        let deadline = Instant::now() + budget;
        let mut seen = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let line = self
                .lines
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("no ready line; runner said: {seen:#?}"));
            if let Some(RunnerLine::Ready(ready)) = decode(&line) {
                return (ready, seen);
            }
            seen.push(line);
        }
    }

    fn wait_exit(&mut self, budget: Duration) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + budget;
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Runner {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_runner(command: &mut Command) -> Child {
    let deadline = Instant::now() + EXEC_RETRY_BUDGET;
    loop {
        match command.spawn() {
            Ok(child) => return child,
            Err(error)
                if error.raw_os_error() == Some(libc::ETXTBSY) && Instant::now() < deadline =>
            {
                std::thread::sleep(EXEC_RETRY_DELAY);
            }
            Err(error) => panic!("the runner did not start: {error}"),
        }
    }
}

fn headless(command: &mut Command) {
    command.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY");
}

#[test]
fn headless_runner_shows_the_staged_identity_and_exits_when_the_lifeline_closes() {
    let staged = Staged::new("lifeline");
    let mut runner = Runner::start(&staged, headless);
    let (ready, said) = runner.wait_ready(Duration::from_secs(5));
    assert_eq!(ready.backend, "none");
    assert_eq!(ready.window, 0);
    assert_eq!(ready.title, TITLE);
    assert!(
        said.iter()
            .all(|line| !line.contains(staged.directory.to_str().unwrap())),
        "the runner must not send full paths: {said:#?}"
    );

    let pid = runner.child.id();
    assert_eq!(
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap()
            .trim_end(),
        EXE
    );
    assert_eq!(
        std::fs::read_link(format!("/proc/{pid}/exe")).unwrap(),
        std::fs::canonicalize(&staged.executable).unwrap()
    );
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let argv0 = cmdline.split(|byte| *byte == 0).next().unwrap();
    assert_eq!(
        Path::new(std::str::from_utf8(argv0).unwrap()),
        staged.executable
    );
    assert!(
        std::str::from_utf8(argv0)
            .unwrap()
            .ends_with("game/eldenring.exe")
    );

    // Still alive while the lifeline is open.
    std::thread::sleep(Duration::from_millis(300));
    assert!(runner.child.try_wait().unwrap().is_none());

    drop(runner.child.stdin.take());
    let status = runner
        .wait_exit(Duration::from_secs(5))
        .expect("the runner kept running after its lifeline closed");
    assert_eq!(status.code(), Some(0), "{status:?}");
}

#[test]
fn runner_ends_on_sigterm() {
    let staged = Staged::new("sigterm");
    let mut runner = Runner::start(&staged, headless);
    runner.wait_ready(Duration::from_secs(5));
    // SAFETY: signalling the child this test owns and has not reaped.
    assert_eq!(
        unsafe { libc::kill(runner.child.id() as libc::pid_t, libc::SIGTERM) },
        0
    );
    let status = runner
        .wait_exit(Duration::from_secs(5))
        .expect("the runner ignored SIGTERM");
    assert_eq!(status.signal(), Some(libc::SIGTERM));
}

#[test]
fn window_none_skips_x11_even_with_a_display() {
    let staged = Staged::new("nowindow");
    let runner = Runner::start(&staged, |command| {
        command.env("GAME_LARPER_RUNNER_WINDOW", "none");
    });
    let (ready, _) = runner.wait_ready(Duration::from_secs(5));
    assert_eq!(ready.backend, "none");
}

#[test]
fn x11_window_is_unmanaged_offscreen_and_labelled() {
    let required = std::env::var_os("GAME_LARPER_REQUIRE_X11").is_some();
    let Ok((connection, _)) = x11rb::connect(None) else {
        assert!(
            !required,
            "GAME_LARPER_REQUIRE_X11 is set but no X server is reachable"
        );
        eprintln!("skipped: no X server");
        return;
    };
    let staged = Staged::new("x11");
    let runner = Runner::start(&staged, |_| {});
    let (ready, said) = runner.wait_ready(Duration::from_secs(10));
    assert_eq!(ready.backend, "x11", "{said:#?}");
    assert_ne!(ready.window, 0);
    let window = ready.window;

    let attributes = connection
        .get_window_attributes(window)
        .unwrap()
        .reply()
        .unwrap();
    assert!(attributes.override_redirect);
    assert_eq!(attributes.map_state, MapState::VIEWABLE);
    let geometry = connection.get_geometry(window).unwrap().reply().unwrap();
    assert_eq!((geometry.x, geometry.y), (-32000, -32000));
    assert_eq!((geometry.width, geometry.height), (1, 1));
    let tree = connection
        .query_tree(connection.setup().roots[0].root)
        .unwrap()
        .reply()
        .unwrap();
    assert!(tree.children.contains(&window), "not a child of the root");

    let property = |name: &[u8], kind: u32| {
        let atom = connection
            .intern_atom(false, name)
            .unwrap()
            .reply()
            .unwrap()
            .atom;
        connection
            .get_property(false, window, atom, kind, 0, 1024)
            .unwrap()
            .reply()
            .unwrap()
    };
    let class = property(b"WM_CLASS", AtomEnum::STRING.into()).value;
    assert_eq!(class, b"eldenring.exe\0eldenring.exe\0");
    let name = property(b"WM_NAME", AtomEnum::STRING.into()).value;
    assert_eq!(name, TITLE.as_bytes());
    let utf8 = connection
        .intern_atom(false, b"UTF8_STRING")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    assert_eq!(property(b"_NET_WM_NAME", utf8).value, TITLE.as_bytes());
    let pid = property(b"_NET_WM_PID", AtomEnum::CARDINAL.into());
    assert_eq!(
        pid.value32().unwrap().next(),
        Some(runner.child.id()),
        "_NET_WM_PID does not name the runner"
    );
}
