//! The line protocol the Linux runner speaks to Game Larper on its stderr.
//!
//! The runner is a separate process, so anything it learns (which display it reached, the X11
//! window it mapped, why it gave up on a window) would otherwise die on a stderr nobody reads.
//! Each line starts with [`PREFIX`]; fields are separated by one tab (shown as `→` here):
//!
//! ```text
//! GLR→log→debug→Display: DISPLAY=set WAYLAND_DISPLAY=unset
//! GLR→ready→x11→0x1a00001→eldenring
//! ```
//!
//! `ready` is sent exactly once, when the runner has finished setting itself up: `x11` with the
//! window id, or `none` with `0x0` when it runs without a window. Lines are capped at
//! [`MAX_LINE`] bytes and never contain tabs or newlines inside a field. Anything else on stderr
//! (a panic message, a library warning) is not a protocol line; the host still surfaces it.

/// First field of every protocol line.
pub const PREFIX: &str = "GLR";
/// Longest line either side writes or accepts, in bytes, newline excluded.
pub const MAX_LINE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "debug" => Self::Debug,
            "info" => Self::Info,
            "warn" => Self::Warn,
            "error" => Self::Error,
            _ => return None,
        })
    }
}

/// How the runner presents itself once it is up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// `"x11"` or `"none"`.
    pub backend: String,
    /// The X11 window id, 0 without a window.
    pub window: u32,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunnerLine {
    Log { level: Level, message: String },
    Ready(Ready),
}

/// One `log` line, newline excluded.
pub fn encode_log(level: Level, message: &str) -> String {
    bounded(format!(
        "{PREFIX}\tlog\t{}\t{}",
        level.as_str(),
        field(message)
    ))
}

/// The `ready` line, newline excluded.
pub fn encode_ready(ready: &Ready) -> String {
    bounded(format!(
        "{PREFIX}\tready\t{}\t0x{:x}\t{}",
        field(&ready.backend),
        ready.window,
        field(&ready.title)
    ))
}

/// Parse one line (without its newline). `None` means it is not a protocol line.
pub fn decode(line: &str) -> Option<RunnerLine> {
    let mut fields = line.splitn(5, '\t');
    if fields.next()? != PREFIX {
        return None;
    }
    match fields.next()? {
        "log" => {
            let level = Level::parse(fields.next()?)?;
            let message = fields.collect::<Vec<_>>().join(" ");
            Some(RunnerLine::Log { level, message })
        }
        "ready" => {
            let backend = fields.next()?.to_string();
            let window = u32::from_str_radix(fields.next()?.strip_prefix("0x")?, 16).ok()?;
            let title = fields.next().unwrap_or_default().to_string();
            Some(RunnerLine::Ready(Ready {
                backend,
                window,
                title,
            }))
        }
        _ => None,
    }
}

/// Fields cannot carry the separators.
fn field(text: &str) -> String {
    text.replace(['\t', '\n', '\r'], " ")
}

fn bounded(mut line: String) -> String {
    if line.len() > MAX_LINE {
        let mut end = MAX_LINE;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}
