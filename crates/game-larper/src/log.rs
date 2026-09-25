use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Local;

const MAX_LOG_BYTES: u64 = 512 * 1024;

pub struct Log {
    directory: PathBuf,
    gate: Mutex<()>,
}

impl Log {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            gate: Mutex::new(()),
        }
    }

    pub fn info(&self, message: impl AsRef<str>) {
        let message = message.as_ref();
        let _guard = self
            .gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = self.write_line(message) {
            eprintln!("log write failed: {error}");
        }
    }

    fn write_line(&self, message: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let now = Local::now();
        let path = self
            .directory
            .join(format!("{}.log", now.format("%Y-%m-%d")));
        rotate(&path)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{} {message}", now.to_rfc3339())?;
        Ok(())
    }
}

fn rotate(path: &Path) -> std::io::Result<()> {
    let length = match fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if length <= MAX_LOG_BYTES {
        return Ok(());
    }
    let backup = path.with_extension("log.1");
    let _ = fs::remove_file(&backup);
    fs::rename(path, backup)
}
