use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::Local;

const MAX_LOG_BYTES: u64 = 512 * 1024;
/// Live console lines kept in memory. Older lines remain in the daily files on disk.
pub const MAX_HISTORY: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Success,
    Warn,
    Error,
}

impl LogLevel {
    pub fn label(self) -> &'static str {
        match self {
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Success => "SUCCESS",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

/// The part of Game Larper a line comes from.
///
/// The console sizes its area column from the widest label ("settings" in log-window.slint),
/// so keep new labels at most that long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    App,
    Catalog,
    Session,
    Art,
    Runner,
    Queue,
    Settings,
    Network,
    Files,
}

impl Area {
    pub fn label(self) -> &'static str {
        match self {
            Self::App => "app",
            Self::Catalog => "catalog",
            Self::Session => "session",
            Self::Art => "artwork",
            Self::Runner => "runner",
            Self::Queue => "queue",
            Self::Settings => "settings",
            Self::Network => "network",
            Self::Files => "files",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    /// Increases by one per line for the whole run, across clears.
    pub seq: u64,
    /// Local wall clock, `HH:MM:SS.mmm`.
    pub time: String,
    pub level: LogLevel,
    pub area: Area,
    pub message: String,
}

impl LogEntry {
    pub fn formatted(&self) -> String {
        format!(
            "[{}] {}",
            self.time,
            line_body(self.level, self.area, &self.message)
        )
    }
}

/// `[LEVEL]   [area] message`, padded so messages line up in plain text.
fn line_body(level: LogLevel, area: Area, message: &str) -> String {
    format!(
        "{:<9} [{}] {message}",
        format!("[{}]", level.label()),
        area.label()
    )
}

/// A path for a log line, with the user's profile folders replaced by their variables so a
/// pasted log shows where files are without naming the Windows account.
pub fn redact(path: &Path) -> String {
    let roots = [
        ("%LOCALAPPDATA%", std::env::var_os("LOCALAPPDATA")),
        ("%USERPROFILE%", std::env::var_os("USERPROFILE")),
    ];
    redact_with(path, &roots)
}

fn redact_with(path: &Path, roots: &[(&str, Option<std::ffi::OsString>)]) -> String {
    let text = path.display().to_string();
    for (name, root) in roots {
        let Some(root) = root.as_ref().and_then(|root| root.to_str()) else {
            continue;
        };
        let root = root.trim_end_matches(['\\', '/']);
        if root.is_empty() || text.len() < root.len() || !text.is_char_boundary(root.len()) {
            continue;
        }
        let (head, tail) = text.split_at(root.len());
        if head.eq_ignore_ascii_case(root) && (tail.is_empty() || tail.starts_with(['\\', '/'])) {
            return format!("{name}{tail}");
        }
    }
    text
}

/// Where a reader of the live history left off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LogCursor {
    epoch: u64,
    next: u64,
}

/// What changed in the live history since a cursor was last advanced.
#[derive(Debug, PartialEq, Eq)]
pub enum LogChanges {
    None,
    /// New lines at the end. The reader drops its oldest lines beyond `MAX_HISTORY`.
    Append(Vec<LogEntry>),
    /// The reader fell behind or the history was cleared: replace everything.
    Reset(Vec<LogEntry>),
}

struct History {
    entries: VecDeque<LogEntry>,
    next_seq: u64,
    /// Bumped by `clear`, so readers know to drop what they show.
    epoch: u64,
}

impl History {
    fn new() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_HISTORY),
            next_seq: 0,
            epoch: 0,
        }
    }

    fn push(&mut self, time: String, level: LogLevel, area: Area, message: String) {
        if self.entries.len() == MAX_HISTORY {
            self.entries.pop_front();
        }
        self.entries.push_back(LogEntry {
            seq: self.next_seq,
            time,
            level,
            area,
            message,
        });
        self.next_seq += 1;
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.epoch += 1;
    }

    fn changes_since(&self, cursor: &mut LogCursor) -> LogChanges {
        let all = || self.entries.iter().cloned().collect::<Vec<_>>();
        let changes = if cursor.epoch != self.epoch {
            LogChanges::Reset(all())
        } else if cursor.next == self.next_seq {
            LogChanges::None
        } else if self
            .entries
            .front()
            .is_some_and(|first| first.seq > cursor.next)
        {
            // Lines the reader never saw were already dropped.
            LogChanges::Reset(all())
        } else {
            LogChanges::Append(
                self.entries
                    .iter()
                    .filter(|entry| entry.seq >= cursor.next)
                    .cloned()
                    .collect(),
            )
        };
        *cursor = LogCursor {
            epoch: self.epoch,
            next: self.next_seq,
        };
        changes
    }
}

pub struct Log {
    directory: PathBuf,
    gate: Mutex<()>,
    history: Mutex<History>,
}

impl Log {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            gate: Mutex::new(()),
            history: Mutex::new(History::new()),
        }
    }

    pub fn debug(&self, area: Area, message: impl AsRef<str>) {
        self.record(LogLevel::Debug, area, message.as_ref());
    }

    pub fn info(&self, area: Area, message: impl AsRef<str>) {
        self.record(LogLevel::Info, area, message.as_ref());
    }

    pub fn success(&self, area: Area, message: impl AsRef<str>) {
        self.record(LogLevel::Success, area, message.as_ref());
    }

    pub fn warn(&self, area: Area, message: impl AsRef<str>) {
        self.record(LogLevel::Warn, area, message.as_ref());
    }

    pub fn error(&self, area: Area, message: impl AsRef<str>) {
        self.record(LogLevel::Error, area, message.as_ref());
    }

    /// Lines added since `cursor`, which is advanced to the current end.
    pub fn changes_since(&self, cursor: &mut LogCursor) -> LogChanges {
        self.history().changes_since(cursor)
    }

    /// Clear only the live console history. Log files on disk are intentionally untouched.
    pub fn clear_history(&self) {
        self.history().clear();
    }

    /// The live history as plain text, one line per entry.
    pub fn history_text(&self) -> String {
        self.history()
            .entries
            .iter()
            .map(LogEntry::formatted)
            .collect::<Vec<_>>()
            .join("\r\n")
    }

    fn history(&self) -> std::sync::MutexGuard<'_, History> {
        self.history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn record(&self, level: LogLevel, area: Area, message: &str) {
        let now = Local::now();
        self.history().push(
            now.format("%H:%M:%S%.3f").to_string(),
            level,
            area,
            message.to_string(),
        );

        let _guard = self
            .gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let line = format!(
            "{} {}",
            now.format("%Y-%m-%dT%H:%M:%S%.3f%:z"),
            line_body(level, area, message)
        );
        if let Err(error) = self.write_line(&line) {
            eprintln!("log write failed: {error}");
        }
    }

    fn write_line(&self, line: &str) -> std::io::Result<()> {
        fs::create_dir_all(&self.directory)?;
        let path = self
            .directory
            .join(format!("{}.log", Local::now().format("%Y-%m-%d")));
        rotate(&path)?;
        let mut file = OpenOptions::new().create(true).append(true).open(path)?;
        writeln!(file, "{line}")?;
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

#[cfg(test)]
mod tests {
    use super::{
        Area, History, LogChanges, LogCursor, LogEntry, LogLevel, MAX_HISTORY, redact_with,
    };
    use std::path::Path;

    fn push(history: &mut History, index: usize) {
        history.push(
            "12:34:56.789".into(),
            LogLevel::Info,
            Area::App,
            format!("entry {index}"),
        );
    }

    fn messages(entries: &[LogEntry]) -> Vec<&str> {
        entries.iter().map(|entry| entry.message.as_str()).collect()
    }

    #[test]
    fn formatting_aligns_level_area_and_message() {
        let entry = |level| LogEntry {
            seq: 0,
            time: "12:34:56.789".into(),
            level,
            area: Area::Network,
            message: "catalog refresh failed".into(),
        };
        assert_eq!(
            entry(LogLevel::Warn).formatted(),
            "[12:34:56.789] [WARN]    [network] catalog refresh failed"
        );
        assert_eq!(
            entry(LogLevel::Success).formatted(),
            "[12:34:56.789] [SUCCESS] [network] catalog refresh failed"
        );
    }

    #[test]
    fn redaction_hides_the_account_folder_only_at_a_path_boundary() {
        let roots = [
            ("%LOCALAPPDATA%", Some(r"C:\Users\Kya\AppData\Local".into())),
            ("%USERPROFILE%", Some(r"C:\Users\Kya".into())),
        ];
        let redact = |path: &str| redact_with(Path::new(path), &roots);
        assert_eq!(
            redact(r"c:\users\kya\AppData\Local\GameLarper\runtime\10\game.exe"),
            r"%LOCALAPPDATA%\GameLarper\runtime\10\game.exe"
        );
        assert_eq!(
            redact(r"C:\Users\Kya\Downloads\GameLarper.exe"),
            r"%USERPROFILE%\Downloads\GameLarper.exe"
        );
        assert_eq!(redact(r"C:\Users\Kyara\file"), r"C:\Users\Kyara\file");
        assert_eq!(redact(r"D:\Games\runner.exe"), r"D:\Games\runner.exe");
    }

    #[test]
    fn area_labels_fit_the_console_column() {
        let areas = [
            Area::App,
            Area::Catalog,
            Area::Session,
            Area::Art,
            Area::Runner,
            Area::Queue,
            Area::Settings,
            Area::Network,
            Area::Files,
        ];
        assert!(
            areas
                .iter()
                .all(|area| area.label().len() <= "settings".len())
        );
    }

    #[test]
    fn live_history_drops_the_oldest_entry_at_capacity() {
        let mut history = History::new();
        for index in 0..=MAX_HISTORY {
            push(&mut history, index);
        }
        assert_eq!(history.entries.len(), MAX_HISTORY);
        assert_eq!(history.entries.front().unwrap().message, "entry 1");
        assert_eq!(
            history.entries.back().unwrap().message,
            format!("entry {MAX_HISTORY}")
        );
    }

    #[test]
    fn readers_receive_only_new_lines() {
        let mut history = History::new();
        let mut cursor = LogCursor::default();
        assert_eq!(history.changes_since(&mut cursor), LogChanges::None);

        push(&mut history, 0);
        push(&mut history, 1);
        let LogChanges::Append(first) = history.changes_since(&mut cursor) else {
            panic!("expected an append");
        };
        assert_eq!(messages(&first), ["entry 0", "entry 1"]);
        assert_eq!(history.changes_since(&mut cursor), LogChanges::None);

        push(&mut history, 2);
        let LogChanges::Append(next) = history.changes_since(&mut cursor) else {
            panic!("expected an append");
        };
        assert_eq!(messages(&next), ["entry 2"]);
    }

    #[test]
    fn clearing_resets_readers_and_keeps_sequence_numbers() {
        let mut history = History::new();
        let mut cursor = LogCursor::default();
        push(&mut history, 0);
        let _ = history.changes_since(&mut cursor);

        history.clear();
        assert_eq!(
            history.changes_since(&mut cursor),
            LogChanges::Reset(Vec::new())
        );
        assert_eq!(history.changes_since(&mut cursor), LogChanges::None);

        push(&mut history, 1);
        let LogChanges::Append(after) = history.changes_since(&mut cursor) else {
            panic!("expected an append");
        };
        assert_eq!(after[0].seq, 1);
    }

    #[test]
    fn a_reader_that_fell_behind_gets_the_whole_history() {
        let mut history = History::new();
        let mut cursor = LogCursor::default();
        push(&mut history, 0);
        let _ = history.changes_since(&mut cursor);
        for index in 1..=MAX_HISTORY + 5 {
            push(&mut history, index);
        }
        let LogChanges::Reset(all) = history.changes_since(&mut cursor) else {
            panic!("expected a reset");
        };
        assert_eq!(all.len(), MAX_HISTORY);
        assert_eq!(
            all.last().unwrap().message,
            format!("entry {}", MAX_HISTORY + 5)
        );
    }
}
