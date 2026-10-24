//! Desktop glue per target. Callers use the portable names re-exported here and never reach
//! into a target module directly; anything only one target has (the Win32 integrity level,
//! window icons) is called from that target's code alone.

// Only the Windows tray icon needs it, but the logic is plain and its tests run everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
pub mod click;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod tray_windows;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
pub use linux::*;
#[cfg(windows)]
pub use tray_windows::NativeTray;
#[cfg(windows)]
pub use windows::*;

/// What the user did to the tray icon, after the double-click time has been taken into account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// A single left click. `at` is when the button was released and `anchor` where the
    /// pointer was then, so a delayed reaction still knows what the click saw.
    Click {
        at: std::time::Instant,
        anchor: Option<(i32, i32)>,
    },
    /// Two left clicks within the system double-click time. No `Click` is sent for them.
    DoubleClick,
    RightClick {
        anchor: Option<(i32, i32)>,
    },
}

/// The usable horizontal span (without a taskbar) of one monitor, in physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct WorkArea {
    pub left: i32,
    pub right: i32,
}

/// A rectangle in physical screen pixels. `right` and `bottom` are exclusive, as in Win32.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

/// One monitor: its full bounds, the part the taskbar leaves free, and its DPI scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Monitor {
    pub bounds: Rect,
    pub work: Rect,
    pub scale: f32,
}
