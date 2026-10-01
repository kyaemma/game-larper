//! Desktop glue per target. Callers use the portable names re-exported here and never reach
//! into a target module directly; anything only one target has (the Win32 integrity level,
//! window icons) is called from that target's code alone.

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::*;

/// The usable horizontal span (without a taskbar) of one monitor, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct WorkArea {
    pub left: i32,
    pub right: i32,
}
