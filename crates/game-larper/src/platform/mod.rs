//! Platform-specific glue for the desktop shell.
//!
//! Everything the rest of the crate needs is re-exported from here, so the
//! call sites only deal in portable names and never touch `windows` or `unix`
//! directly.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use unix::*;
#[cfg(windows)]
pub use windows::*;

/// The usable horizontal span (without a taskbar) of one monitor, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct WorkArea {
    pub left: i32,
    pub right: i32,
}
