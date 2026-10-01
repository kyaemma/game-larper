//! Linux runner mechanics (experimental until checked against a real Discord client).
//!
//! The host owns the runner as a `std::process::Child` and never looks processes up by name:
//!
//! - **Lifeline.** The runner's stdin is a pipe whose write end only this host holds. Rust
//!   creates it close-on-exec, so no other child of Game Larper inherits it. Closing it is the
//!   polite stop; the kernel also closes it if Game Larper dies however it dies, and the runner
//!   exits on EOF. (`PR_SET_PDEATHSIG` is not used: prctl(2) ties it to the *thread* that
//!   forked, and launches happen on short-lived worker threads.)
//! - **Diagnostics and exit.** The runner's stderr is drained by one thread per runner, which
//!   forwards protocol lines (bounded) into the structured log, hands over the single `ready`
//!   line, and fires the exit watch at EOF — the runner's stderr only closes when it exits.
//! - **Stop.** Close the lifeline, wait; then `SIGTERM`, wait; then `SIGKILL`, wait. Signals go
//!   to our own unreaped child, so its pid cannot have been reused. Then reap, then clean up.

use std::io::{BufRead, BufReader, Read};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use game_larper_core::runner_protocol::{self, Level, Ready, RunnerLine};

use super::{LaunchReport, RunnerHost, Staged};
use crate::log::{Area, Log, redact};

/// How `LaunchReport::hwnd` is labelled in the log.
pub const WINDOW_LABEL: &str = "X11 window";
/// Connecting to X and mapping a window is quick, but a busy session can be slow.
const STARTUP_BUDGET: Duration = Duration::from_secs(5);
const AFTER_LIFELINE: Duration = Duration::from_secs(2);
const AFTER_SIGTERM: Duration = Duration::from_secs(2);
const AFTER_SIGKILL: Duration = Duration::from_secs(5);
/// Runner lines forwarded to the log per launch; the rest are drained and dropped.
const MAX_FORWARDED: usize = 200;

/// The runner the host owns.
pub(super) struct Running {
    pub(super) pid: u32,
    child: Child,
    /// Dropping this closes the runner's stdin, which asks it to exit.
    lifeline: Option<ChildStdin>,
    staged: Staged,
    generation: u64,
}

/// Fires when the runner's diagnostics pipe closes, which happens when it exits.
pub struct ExitWatch(mpsc::Receiver<()>);

/// Wait on a background thread for the runner to exit, then call `on_exit`.
pub fn watch_exit(watch: ExitWatch, on_exit: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        // A value or a disconnect both mean the reader saw EOF.
        let _ = watch.0.recv();
        on_exit();
    });
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
            .ok_or_else(|| "The runner path has no directory.".to_string())?
            .to_path_buf();
        let basename = staged
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "game.exe".into());
        self.log.debug(
            Area::Runner,
            format!(
                "Spawning in {}; session: {}",
                redact(&directory),
                session_summary()
            ),
        );
        if icon.is_some() {
            self.log.debug(
                Area::Runner,
                "Window icons are not applied on Linux (no detection role known)",
            );
        }
        let started = Instant::now();
        let spawned = Command::new(&staged.path)
            .current_dir(&directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                self.cleanup(&staged);
                return self.fail(format!("The runner did not start: {error}"));
            }
        };
        let pid = child.id();
        self.log
            .debug(Area::Runner, format!("Process created PID={pid}"));
        let lifeline = child.stdin.take();
        let (ready_sender, ready) = mpsc::channel();
        let (exit_sender, exited) = mpsc::channel();
        if let Some(stderr) = child.stderr.take() {
            let log = self.log.clone();
            let reader = std::thread::Builder::new()
                .name(format!("runner-{pid}-stderr"))
                .spawn(move || read_runner(stderr, pid, &log, &ready_sender, &exit_sender));
            if let Err(error) = reader {
                kill_and_reap(&mut child);
                self.cleanup(&staged);
                return self.fail(format!("Could not watch the runner: {error}"));
            }
        }
        let ready = match ready.recv_timeout(STARTUP_BUDGET) {
            Ok(ready) => ready,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let status = wait_for(&mut child, Duration::from_secs(1));
                if status.is_none() {
                    kill_and_reap(&mut child);
                }
                self.cleanup(&staged);
                return self.fail(format!(
                    "The native runner exited during startup ({}).",
                    status.map_or_else(|| "it closed its output".into(), describe)
                ));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                kill_and_reap(&mut child);
                self.cleanup(&staged);
                return self.fail(format!(
                    "The native runner did not report ready within {} s.",
                    STARTUP_BUDGET.as_secs()
                ));
            }
        };
        self.log.debug(
            Area::Runner,
            format!(
                "Ready in {} ms: backend={} {WINDOW_LABEL}=0x{:X} title=\"{}\"",
                started.elapsed().as_millis(),
                ready.backend,
                ready.window,
                ready.title
            ),
        );
        self.check_identity(pid, &staged.path, &basename);

        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        let report = LaunchReport {
            generation,
            pid,
            executable: staged.path.clone(),
            basename,
            working_directory: directory,
            hwnd: ready.window as isize,
            title: ready.title,
            integrity: user_of(pid),
            exit_watch: ExitWatch(exited),
        };
        self.current = Some(Running {
            pid,
            child,
            lifeline,
            staged,
            generation,
        });
        self.stopping = false;
        Ok(report)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        let Some(mut running) = self.current.take() else {
            return Ok(());
        };
        self.stopping = true;
        let pid = running.pid;
        self.log.info(
            Area::Runner,
            format!("Stopping PID={pid} (generation {})", running.generation),
        );
        let started = Instant::now();
        drop(running.lifeline.take());
        let mut status = wait_for(&mut running.child, AFTER_LIFELINE);
        if status.is_none() {
            self.log.warn(
                Area::Runner,
                format!(
                    "PID={pid} still running {} ms after its lifeline closed; sending SIGTERM",
                    started.elapsed().as_millis()
                ),
            );
            signal(pid, libc::SIGTERM);
            status = wait_for(&mut running.child, AFTER_SIGTERM);
        }
        if status.is_none() {
            self.log.warn(
                Area::Runner,
                format!("PID={pid} ignored SIGTERM; sending SIGKILL"),
            );
            let _ = running.child.kill();
            status = wait_for(&mut running.child, AFTER_SIGKILL);
        }
        let Some(status) = status else {
            self.stopping = false;
            self.log.error(
                Area::Runner,
                format!("PID={pid} is still running after SIGKILL"),
            );
            self.current = Some(running);
            return Err("The owned runner did not exit.".into());
        };
        self.log.debug(
            Area::Runner,
            format!(
                "PID={pid} exited after {} ms: {}",
                started.elapsed().as_millis(),
                describe(status)
            ),
        );
        self.cleanup(&running.staged);
        self.stopping = false;
        self.log.info(Area::Runner, format!("PID={pid} stopped"));
        Ok(())
    }

    /// Called after the exit watch fired. Returns true when the exit was not requested.
    pub fn take_unexpected_exit(&mut self, generation: u64) -> bool {
        if self.stopping {
            return false;
        }
        let Some(mut running) = self.current.take() else {
            return false;
        };
        if running.generation != generation {
            self.current = Some(running);
            return false;
        }
        // Its stderr closed, so it is exiting; reap it so it does not linger as a zombie.
        match wait_for(&mut running.child, Duration::from_secs(1)) {
            Some(status) => self.log.warn(
                Area::Runner,
                format!(
                    "PID={} exited on its own: {} (generation {generation})",
                    running.pid,
                    describe(status)
                ),
            ),
            None => {
                self.log.warn(
                    Area::Runner,
                    format!(
                        "PID={} closed its output but kept running; killing it",
                        running.pid
                    ),
                );
                kill_and_reap(&mut running.child);
            }
        }
        self.cleanup(&running.staged);
        true
    }

    /// Log what a process scanner reads in `/proc`, and warn if it is not the staged file.
    fn check_identity(&self, pid: u32, staged: &Path, basename: &str) {
        let identity = ProcIdentity::read(pid);
        self.log.debug(
            Area::Runner,
            format!(
                "/proc identity: exe={} argv0={} comm={}",
                identity.exe.as_deref().map_or("unreadable".into(), redact),
                identity
                    .argv0
                    .as_deref()
                    .map_or("unreadable".into(), redact),
                identity.comm.as_deref().unwrap_or("unreadable")
            ),
        );
        let canonical = std::fs::canonicalize(staged).ok();
        let mut mismatches = Vec::new();
        if identity.exe.is_none() || identity.exe != canonical {
            mismatches.push("exe");
        }
        if identity.argv0.as_deref() != Some(staged) {
            mismatches.push("argv0");
        }
        if identity.comm.as_deref() != Some(comm_of(basename)) {
            mismatches.push("comm");
        }
        if !mismatches.is_empty() {
            self.log.warn(
                Area::Runner,
                format!(
                    "/proc {} does not name the staged {basename}",
                    mismatches.join(", ")
                ),
            );
        }
    }
}

/// Copy the template into place, refusing to write through anything already there.
///
/// `create_new` maps to `O_CREAT | O_EXCL`, which fails on an existing file and on a symlink
/// planted at the destination (even a dangling one), so the copy cannot be redirected between
/// the existence check and the write. The copy is owner-only (0700): it is executed, never
/// shared.
pub(super) fn copy_template(template: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut source = std::fs::File::open(template)?;
    let mut target = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o700)
        .open(destination)?;
    if let Err(error) = std::io::copy(&mut source, &mut target).and_then(|_| target.sync_all()) {
        drop(target);
        let _ = std::fs::remove_file(destination);
        return Err(error);
    }
    Ok(())
}

/// Any symlink, wherever it points. Same rule as Windows reparse points: the runtime tree is
/// Game Larper's own, so a link in it is never expected.
pub(super) fn is_reparse_path(path: &Path) -> std::io::Result<bool> {
    Ok(std::fs::symlink_metadata(path)?.file_type().is_symlink())
}

/// Drain the runner's stderr: forward diagnostics, hand over `ready`, signal EOF.
fn read_runner(
    stderr: ChildStderr,
    pid: u32,
    log: &Log,
    ready: &mpsc::Sender<Ready>,
    exited: &mpsc::Sender<()>,
) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    let mut forwarded = 0;
    loop {
        line.clear();
        // Bounded per line, so a runaway writer cannot grow this buffer.
        match (&mut reader)
            .take(runner_protocol::MAX_LINE as u64 + 1)
            .read_until(b'\n', &mut line)
        {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\n', '\r']);
        if let Some(RunnerLine::Ready(announced)) = runner_protocol::decode(text) {
            let _ = ready.send(announced);
            continue;
        }
        if text.trim().is_empty() {
            continue;
        }
        forwarded += 1;
        if forwarded > MAX_FORWARDED {
            if forwarded == MAX_FORWARDED + 1 {
                log.warn(
                    Area::Runner,
                    format!("PID={pid}: further runner output is not logged"),
                );
            }
            continue;
        }
        match runner_protocol::decode(text) {
            Some(RunnerLine::Log { level, message }) => {
                let message = format!("PID={pid}: {message}");
                match level {
                    Level::Debug => log.debug(Area::Runner, message),
                    Level::Info => log.info(Area::Runner, message),
                    Level::Warn => log.warn(Area::Runner, message),
                    Level::Error => log.error(Area::Runner, message),
                }
            }
            // Not ours: a panic message or a library talking. Unexpected, so worth a warning.
            _ => log.warn(Area::Runner, format!("PID={pid} stderr: {text}")),
        }
    }
    let _ = exited.send(());
}

/// Poll for the child's exit for up to `budget`, reaping it when it has.
fn wait_for(child: &mut Child, budget: Duration) -> Option<ExitStatus> {
    let deadline = Instant::now() + budget;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) => {}
            Err(_) => return None,
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn kill_and_reap(child: &mut Child) {
    let _ = child.kill();
    let _ = wait_for(child, AFTER_SIGKILL);
}

fn signal(pid: u32, signal: libc::c_int) {
    // SAFETY: kill(2) has no memory-safety preconditions. `pid` is our own child, not yet
    // reaped, so it still names that process and cannot have been recycled.
    unsafe {
        libc::kill(pid as libc::pid_t, signal);
    }
}

/// "exit code 0", "signal 15 (SIGTERM)".
fn describe(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    match status.signal() {
        Some(signal) => {
            let name = match signal {
                libc::SIGHUP => " (SIGHUP)",
                libc::SIGINT => " (SIGINT)",
                libc::SIGABRT => " (SIGABRT)",
                libc::SIGKILL => " (SIGKILL)",
                libc::SIGSEGV => " (SIGSEGV)",
                libc::SIGPIPE => " (SIGPIPE)",
                libc::SIGTERM => " (SIGTERM)",
                _ => "",
            };
            format!("signal {signal}{name}")
        }
        None => "unknown status".into(),
    }
}

/// What `/proc/<pid>` says the process is.
struct ProcIdentity {
    exe: Option<PathBuf>,
    argv0: Option<PathBuf>,
    comm: Option<String>,
}

impl ProcIdentity {
    fn read(pid: u32) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
        let argv0 = std::fs::read(format!("/proc/{pid}/cmdline"))
            .ok()
            .and_then(|cmdline| {
                let first = cmdline.split(|byte| *byte == 0).next()?;
                (!first.is_empty()).then(|| PathBuf::from(std::ffi::OsStr::from_bytes(first)))
            });
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .ok()
            .map(|comm| comm.trim_end_matches('\n').to_string());
        Self { exe, argv0, comm }
    }
}

/// The kernel keeps at most 15 bytes of a task name (`TASK_COMM_LEN` is 16 with the NUL).
fn comm_of(name: &str) -> &str {
    let mut end = name.len().min(15);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// The runner's real uid, standing in for the Windows integrity level in the launch line.
fn user_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Uid:"))
                .and_then(|uids| uids.split_whitespace().next())
                .map(|uid| format!("uid={uid}"))
        })
        .unwrap_or_else(|| "unknown".into())
}

/// Which display servers the runner will inherit, without their values.
fn session_summary() -> String {
    let state = |variable: &str| {
        if std::env::var_os(variable).is_some_and(|value| !value.is_empty()) {
            "set"
        } else {
            "unset"
        }
    };
    format!(
        "DISPLAY={} WAYLAND_DISPLAY={} XDG_SESSION_TYPE={}",
        state("DISPLAY"),
        state("WAYLAND_DISPLAY"),
        std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unset".into())
    )
}

#[cfg(test)]
mod tests {
    use super::{MAX_FORWARDED, RunnerHost, comm_of, describe};
    use crate::log::{Area, Log, LogChanges, LogCursor};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::{Path, PathBuf};
    use std::process::ExitStatus;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    /// Speaks the protocol, then holds the lifeline open with `cat`: exits exactly at EOF.
    const STAND_IN: &str = r"printf 'GLR\tlog\tdebug\tstand-in up\n' >&2
printf 'GLR\tready\tnone\t0x0\teldenring\n' >&2
cat >/dev/null";

    /// Two thousand diagnostics before `ready`, far more than the pipe buffer holds.
    const CHATTY: &str = r"i=0
while [ $i -lt 2000 ]; do printf 'GLR\tlog\tdebug\tline %s\n' $i >&2; i=$((i+1)); done
printf 'GLR\tready\tnone\t0x0\teldenring\n' >&2
cat >/dev/null";

    /// A scratch directory with its own log and stand-in runner templates.
    struct Scratch {
        root: PathBuf,
        log: Arc<Log>,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "gl-host-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let log = Arc::new(Log::new(root.join("logs")));
            Self { root, log }
        }

        fn template(&self, body: &str) -> PathBuf {
            let path = self.root.join(format!("template-{}", body.len()));
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        }

        fn host(&self) -> RunnerHost {
            RunnerHost::new(self.root.join("runtime"), self.log.clone())
        }

        fn runner_lines(&self) -> Vec<String> {
            let mut cursor = LogCursor::default();
            let entries = match self.log.changes_since(&mut cursor) {
                LogChanges::Append(entries) | LogChanges::Reset(entries) => entries,
                LogChanges::None => Vec::new(),
            };
            entries
                .into_iter()
                .filter(|entry| entry.area == Area::Runner)
                .map(|entry| entry.message)
                .collect()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Running and not a zombie.
    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
            !stat
                .rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
    }

    #[test]
    fn launch_stop_and_unexpected_exit_follow_the_shared_contract() {
        let scratch = Scratch::new("lifecycle");
        let template = scratch.template(STAND_IN);
        let mut host = scratch.host();

        let report = host
            .launch(&template, "10", "game/eldenring.exe", None)
            .expect("the stand-in did not start");
        assert_eq!(report.basename, "eldenring.exe");
        assert_eq!(report.title, "eldenring");
        assert_eq!(report.hwnd, 0);
        assert!(report.integrity.starts_with("uid="), "{}", report.integrity);
        assert!(report.executable.ends_with("10/game/eldenring.exe"));
        assert!(alive(report.pid));
        assert!(
            scratch
                .runner_lines()
                .iter()
                .any(|line| line.ends_with(": stand-in up")),
            "runner diagnostics were not forwarded"
        );

        // A crash: the exit watch fires and the host cleans up after it.
        // SAFETY: signalling the child this test's host owns.
        unsafe {
            libc::kill(report.pid as libc::pid_t, libc::SIGKILL);
        }
        report
            .exit_watch
            .0
            .recv_timeout(Duration::from_secs(5))
            .expect("the exit watch did not fire");
        assert!(host.take_unexpected_exit(report.generation));
        assert!(!alive(report.pid), "the crashed runner was not reaped");
        assert!(
            !report.executable.exists(),
            "the staged copy was left behind"
        );

        // A requested stop closes the lifeline and is not an unexpected exit.
        let second = host
            .launch(&template, "10", "game/eldenring.exe", None)
            .expect("the stand-in did not restart");
        host.stop().expect("the stand-in did not stop");
        assert!(!alive(second.pid));
        second
            .exit_watch
            .0
            .recv_timeout(Duration::from_secs(5))
            .expect("the exit watch did not fire after stop");
        assert!(!host.take_unexpected_exit(second.generation));
        assert!(!second.executable.exists());
        assert!(
            !scratch
                .runner_lines()
                .iter()
                .any(|line| line.contains("SIGTERM") || line.contains("SIGKILL")),
            "closing the lifeline should have been enough"
        );
    }

    /// Regression for "the runner stops right after being launched" on the Linux PR:
    /// `PR_SET_PDEATHSIG` fires when the forking *thread* exits, and Game Larper launches from
    /// short-lived worker threads.
    #[test]
    fn runner_outlives_the_thread_that_launched_it() {
        let scratch = Scratch::new("thread");
        let template = scratch.template(STAND_IN);
        let host = Arc::new(Mutex::new(scratch.host()));
        let launcher = {
            let host = host.clone();
            std::thread::spawn(move || {
                host.lock()
                    .unwrap()
                    .launch(&template, "10", "game/eldenring.exe", None)
                    .expect("the stand-in did not start")
            })
        };
        let report = launcher.join().unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            alive(report.pid),
            "the runner died with its launching thread"
        );
        host.lock().unwrap().stop().unwrap();
    }

    #[test]
    fn a_runner_that_dies_during_startup_is_reported_and_cleaned() {
        let scratch = Scratch::new("startup");
        let template = scratch.template("echo 'not a protocol line' >&2\nexit 3");
        let mut host = scratch.host();
        let Err(error) = host.launch(&template, "10", "game/eldenring.exe", None) else {
            panic!("a dead runner was accepted");
        };
        assert!(error.contains("exit code 3"), "{error}");
        assert!(
            scratch
                .runner_lines()
                .iter()
                .any(|line| line.ends_with("stderr: not a protocol line")),
            "unexpected runner output must still surface"
        );
        assert!(!scratch.root.join("runtime/10/game/eldenring.exe").exists());
    }

    #[test]
    fn runner_output_is_bounded_and_never_blocks_the_runner() {
        let scratch = Scratch::new("flood");
        let template = scratch.template(CHATTY);
        let mut host = scratch.host();
        host.launch(&template, "10", "game/eldenring.exe", None)
            .expect("a chatty runner must still reach ready");
        host.stop().unwrap();
        let lines = scratch.runner_lines();
        let forwarded = lines.iter().filter(|line| line.contains(": line ")).count();
        assert_eq!(forwarded, MAX_FORWARDED);
        assert!(
            lines
                .iter()
                .any(|line| line.ends_with("further runner output is not logged"))
        );
    }

    #[test]
    fn staged_copies_and_runtime_folders_are_owner_only() {
        let scratch = Scratch::new("modes");
        let template = scratch.template(STAND_IN);
        let mut host = scratch.host();
        let report = host
            .launch(&template, "10", "game/eldenring.exe", None)
            .unwrap();
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&report.executable), 0o700);
        assert_eq!(mode(report.executable.parent().unwrap()), 0o700);
        host.stop().unwrap();
    }

    #[test]
    fn a_symlink_planted_at_the_destination_is_not_written_through() {
        let scratch = Scratch::new("symlink");
        let template = scratch.template(STAND_IN);
        let victim = scratch.root.join("victim");
        std::fs::write(&victim, b"keep me").unwrap();
        let destination = scratch.root.join("runtime/10/game");
        std::fs::create_dir_all(&destination).unwrap();
        std::os::unix::fs::symlink(&victim, destination.join("eldenring.exe")).unwrap();
        let mut host = scratch.host();
        assert!(
            host.launch(&template, "10", "game/eldenring.exe", None)
                .is_err()
        );
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
    }

    #[test]
    fn comm_is_the_basename_cut_to_fifteen_bytes() {
        assert_eq!(comm_of("eldenring.exe"), "eldenring.exe");
        assert_eq!(comm_of("Cyberpunk2077.exe"), "Cyberpunk2077.e");
    }

    #[test]
    fn exit_statuses_read_naturally() {
        assert_eq!(describe(ExitStatus::from_raw(0)), "exit code 0");
        assert_eq!(describe(ExitStatus::from_raw(3 << 8)), "exit code 3");
        assert_eq!(describe(ExitStatus::from_raw(15)), "signal 15 (SIGTERM)");
        assert_eq!(describe(ExitStatus::from_raw(9)), "signal 9 (SIGKILL)");
    }
}
