//! The stand-in process Discord's game detection is meant to see.
//!
//! The executable basename and the relative path it was staged at are the detection key. On
//! Windows this process keeps a real ownerless, off-screen window with an idle message loop,
//! which is the verified setup. On Linux it keeps the same process identity and, when an X11
//! display is reachable, an unmanaged off-screen X11 window; that path is experimental until it
//! is checked against a real Discord client (see docs/LINUX.md).

#![deny(unsafe_op_in_unsafe_fn)]
#![windows_subsystem = "windows"]

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("game-larper-runner supports Windows and Linux only.");

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use windows as platform;

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(platform::run() as u8)
}
