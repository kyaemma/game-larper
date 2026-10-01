//! Off-screen top-level window that Discord's process scanner can see.
//! The executable basename and relative path are the detection key; this process only
//! has to stay alive with a real ownerless window and an idle message loop.

#![deny(unsafe_op_in_unsafe_fn)]
#![windows_subsystem = "windows"]

#[cfg(not(windows))]
#[path = "unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "windows.rs"]
mod platform;

use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(platform::run() as u8)
}
