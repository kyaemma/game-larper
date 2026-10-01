//! Stages the native runner into the private runtime directory and owns it while it runs.
//!
//! Staging, cleanup, the launch report, and the log lines around them are shared. Starting,
//! watching, and stopping the process are not: each target module adds those methods to
//! `RunnerHost` with the same contract (`launch` / `stop` / `take_unexpected_exit`, and an
//! `ExitWatch` that fires once when the runner is gone).

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use game_larper_core::{Error as CoreError, resolve_executable};

use crate::log::{Area, Log, redact};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use self::windows as sys;

pub use self::sys::{ExitWatch, WINDOW_LABEL, watch_exit};
use self::sys::{Running, copy_template, is_reparse_path};

pub struct Staged {
    pub path: PathBuf,
    pub created: bool,
    /// The usual path held a different file, so this copy lives in a private session folder.
    pub isolated: bool,
}

/// What `cleanup_staged` did with a staged runner copy.
#[derive(Debug)]
pub enum Cleanup {
    Removed,
    /// An identical copy was already there before launch, so it is left for the next one.
    Kept,
    Refused(&'static str),
    Failed(io::Error),
}

pub struct LaunchReport {
    pub generation: u64,
    pub pid: u32,
    pub executable: PathBuf,
    pub basename: String,
    pub working_directory: PathBuf,
    /// The runner's top-level window (labelled `WINDOW_LABEL` in the log), 0 when it has none.
    pub hwnd: isize,
    pub title: String,
    pub integrity: String,
    /// Fires once when this runner exits, requested or not.
    pub exit_watch: ExitWatch,
}

pub struct RunnerHost {
    runtime: PathBuf,
    log: Arc<Log>,
    #[cfg(windows)]
    job: isize,
    current: Option<Running>,
    next_generation: u64,
    stopping: bool,
}

impl RunnerHost {
    pub fn new(runtime: PathBuf, log: Arc<Log>) -> Self {
        Self {
            runtime,
            #[cfg(windows)]
            job: sys::kill_on_close_job(&log),
            log,
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
        if let Some(running) = &self.current {
            self.log.info(
                Area::Runner,
                format!("Replacing the running runner PID={} first", running.pid),
            );
            self.stop()?;
        }
        let staged = stage_runner(template, &self.runtime, application_id, remote_name).map_err(
            |error| {
                self.log.error(
                    Area::Runner,
                    format!("Staging {remote_name} for {application_id} failed: {error}"),
                );
                error.to_string()
            },
        )?;
        if staged.isolated {
            self.log.warn(
                Area::Runner,
                "A different file already uses the usual runtime path; staging in a private session folder",
            );
        }
        self.log.info(
            Area::Runner,
            format!(
                "Staged {} ({})",
                redact(&staged.path),
                if staged.created {
                    "new copy"
                } else {
                    "identical copy already present"
                }
            ),
        );
        self.spawn_staged(staged, icon)
    }

    fn cleanup(&self, staged: &Staged) {
        match cleanup_staged(&self.runtime, staged) {
            Cleanup::Removed => self.log.debug(
                Area::Runner,
                format!("Removed staged copy {}", redact(&staged.path)),
            ),
            Cleanup::Kept => self.log.debug(
                Area::Runner,
                format!("Kept pre-existing staged copy {}", redact(&staged.path)),
            ),
            Cleanup::Refused(reason) => self.log.warn(
                Area::Runner,
                format!("Left {} in place: {reason}", redact(&staged.path)),
            ),
            Cleanup::Failed(error) => self.log.warn(
                Area::Files,
                format!("Could not remove {}: {error}", redact(&staged.path)),
            ),
        }
    }

    fn fail<T>(&self, message: String) -> Result<T, String> {
        self.log.error(Area::Runner, &message);
        Err(message)
    }
}

impl Drop for RunnerHost {
    fn drop(&mut self) {
        let _ = self.stop();
        #[cfg(windows)]
        sys::close_job(self.job);
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
    // A different file holds the usual path (another build, or a real game). Leave it alone.
    let session = format!("session-{:08x}{:04x}", std::process::id(), millis_low());
    let nested = format!("{session}/{remote_name}");
    let destination = resolve_executable(runtime_root, application_id, &nested)?;
    place_file(template, runtime_root, &destination)
        .map_err(CoreError::Io)?
        .map(|staged| Staged {
            isolated: true,
            ..staged
        })
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
                isolated: false,
            }));
        }
        return Ok(None);
    }
    if let Some(parent) = destination.parent() {
        ensure_private_dirs(runtime_root, parent)?;
    }
    copy_template(template, destination)?;
    Ok(Some(Staged {
        path: destination.to_path_buf(),
        created: true,
        isolated: false,
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

pub fn cleanup_staged(runtime_root: &Path, staged: &Staged) -> Cleanup {
    if !staged.created {
        return Cleanup::Kept;
    }
    let Ok(root) = std::path::absolute(runtime_root) else {
        return Cleanup::Refused("the runtime folder could not be resolved");
    };
    let Ok(file) = std::path::absolute(&staged.path) else {
        return Cleanup::Refused("the staged path could not be resolved");
    };
    if !file.starts_with(&root) {
        return Cleanup::Refused("it is outside the runtime folder");
    }
    if chain_has_reparse(&root, &file) {
        return Cleanup::Refused("its folder chain contains a link");
    }
    if let Err(error) = fs::remove_file(&file) {
        return Cleanup::Failed(error);
    }
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
    Cleanup::Removed
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

fn millis_low() -> u16 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| (duration.subsec_millis() & 0xffff) as u16)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::{Cleanup, Staged, cleanup_staged, stage_runner};
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
        assert!(!first.isolated);
        let second = stage_runner(&template, &runtime, "10", "game/eldenring.exe").unwrap();
        assert_eq!(second.path, first.path);
        assert!(!second.created);
        fs::write(&template, b"runner-b-longer").unwrap();
        let third = stage_runner(&template, &runtime, "10", "game/eldenring.exe").unwrap();
        assert_ne!(third.path, first.path);
        assert!(third.created);
        assert!(third.isolated);
        assert_eq!(fs::read(&first.path).unwrap(), b"runner-a");
        assert!(matches!(cleanup_staged(&runtime, &second), Cleanup::Kept));
        assert!(first.path.exists());
        assert!(matches!(cleanup_staged(&runtime, &third), Cleanup::Removed));
        assert!(!third.path.exists());
        cleanup_staged(
            &runtime,
            &Staged {
                path: first.path.clone(),
                created: true,
                isolated: false,
            },
        );
        assert!(!first.path.exists());
        let _ = fs::remove_dir_all(root);
    }
}
