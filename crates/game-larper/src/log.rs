use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Local;

const MAX_LOG_BYTES: u64 = 512 * 1024;
const MAX_HISTORY: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Success,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "INFO",
            Self::Success => "SUCCESS",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub time: String,
    pub level: LogLevel,
    pub message: String,
}

impl LogEntry {
    pub fn formatted(&self) -> String {
        format!("[{}] [{}] {}", self.time, self.level.label(), self.message)
    }
}

pub struct Log {
    directory: PathBuf,
    gate: Mutex<()>,
    history: Mutex<VecDeque<LogEntry>>,
}

impl Log {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            gate: Mutex::new(()),
            history: Mutex::new(VecDeque::with_capacity(MAX_HISTORY)),
        }
    }

    pub fn info(&self, message: impl AsRef<str>) {
        self.record(LogLevel::Info, message.as_ref());
    }

    pub fn success(&self, message: impl AsRef<str>) {
        self.record(LogLevel::Success, message.as_ref());
    }

    pub fn warn(&self, message: impl AsRef<str>) {
        self.record(LogLevel::Warn, message.as_ref());
    }

    pub fn error(&self, message: impl AsRef<str>) {
        self.record(LogLevel::Error, message.as_ref());
    }

    pub fn history(&self) -> Vec<LogEntry> {
        self.history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Clear only the live console history. Log files on disk are intentionally untouched.
    pub fn clear_history(&self) {
        self.history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
    }

    pub fn history_text(&self) -> String {
        self.history()
            .into_iter()
            .map(|entry| entry.formatted())
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    fn record(&self, level: LogLevel, message: &str) {
        let now = Local::now();
        let entry = LogEntry {
            time: now.format("%H:%M:%S").to_string(),
            level,
            message: message.to_string(),
        };
        {
            let mut history = self
                .history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            push_bounded(&mut history, entry);
        }

        let _guard = self
            .gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Err(error) = self.write_line(now.to_rfc3339(), level, message) {
            eprintln!("log write failed: {error}");
        }
    }

    fn write_line(&self, timestamp: String, level: LogLevel, message: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let path = self
            .directory
            .join(format!("{}.log", Local::now().format("%Y-%m-%d")));
        rotate(&path)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{timestamp} [{}] {message}", level.label())?;
        Ok(())
    }
}

fn push_bounded(history: &mut VecDeque<LogEntry>, entry: LogEntry) {
    if history.len() == MAX_HISTORY {
        history.pop_front();
    }
    history.push_back(entry);
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

#[cfg(test)]
mod tests {
    use super::{LogEntry, LogLevel, MAX_HISTORY, push_bounded};
    use std::collections::VecDeque;

    fn entry(index: usize) -> LogEntry {
        LogEntry {
            time: "12:34:56".into(),
            level: LogLevel::Info,
            message: format!("entry {index}"),
        }
    }

    #[test]
    fn formatting_keeps_time_level_and_message() {
        let entry = LogEntry {
            time: "12:34:56".into(),
            level: LogLevel::Warn,
            message: "Discord scan is taking a while".into(),
        };
        assert_eq!(
            entry.formatted(),
            "[12:34:56] [WARN] Discord scan is taking a while"
        );
    }

    #[test]
    fn live_history_drops_the_oldest_entry_at_capacity() {
        let mut history = VecDeque::new();
        for index in 0..=MAX_HISTORY {
            push_bounded(&mut history, entry(index));
        }
        assert_eq!(history.len(), MAX_HISTORY);
        assert_eq!(history.front().unwrap().message, "entry 1");
        assert_eq!(
            history.back().unwrap().message,
            format!("entry {MAX_HISTORY}")
        );
    }
}
