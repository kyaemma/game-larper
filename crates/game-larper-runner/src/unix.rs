//! The Linux runner: a window Discord can see plus the process identity its scanner
//! reads from `/proc`.
//!
//! The detection key is still the executable basename and the relative path it was
//! copied to (`…/game/eldenring.exe`), exactly like the Win32 runner. The window is a
//! 1×1, undecorated, off-screen X11 window when a display is reachable: X11 is the only
//! window system another X11 client can enumerate (Discord runs under XWayland too) and
//! the only one that accepts an off-screen position. Under a pure Wayland compositor the
//! fallback is a Wayland toplevel that is minimized the moment it appears, so it never
//! keeps keyboard focus.

use std::time::Duration;

use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::platform::x11::{ActiveEventLoopExtX11, EventLoopBuilderExtX11, WindowAttributesExtX11};
use winit::window::{Window, WindowId};

/// Same off-screen corner the Win32 runner uses.
const OFFSCREEN: (i32, i32) = (-32000, -32000);

pub fn run() -> i32 {
    set_parent_death_signal();
    let identity = Identity::detect();
    eprintln!("GL-DEBUG runner up: name={} stem={}", identity.name, identity.stem);
    if run_window(&identity).is_err() {
        eprintln!("GL-DEBUG runner: no window, idling");
        // No display server to talk to: the process name and path alone still count.
        idle();
    }
    eprintln!("GL-DEBUG runner: event loop exited normally");
    0
}

/// What a window manager and a process scanner see.
#[derive(Clone)]
struct Identity {
    /// `eldenring.exe`, the detection key: WM_CLASS class, Wayland app id.
    name: String,
    /// `eldenring`, the window title, matching the Win32 runner.
    stem: String,
}

impl Identity {
    fn detect() -> Self {
        let name = std::env::current_exe()
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|part| part.to_string_lossy().into_owned())
            })
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "game".into());
        let stem = match name.to_ascii_lowercase().strip_suffix(".exe") {
            Some(suffix) => name[..suffix.len()].to_string(),
            None => name.clone(),
        };
        Self { name, stem }
    }
}

fn run_window(identity: &Identity) -> Result<(), ()> {
    // Prefer X11 whenever a display is reachable, then fall back to the native backend.
    if display_set("DISPLAY") && attempt(true, identity).is_ok() {
        eprintln!("GL-DEBUG runner: X11 window path done");
        return Ok(());
    }
    let fallback = attempt(false, identity);
    eprintln!("GL-DEBUG runner: native attempt result {:?}", fallback.is_ok());
    fallback
}

fn attempt(force_x11: bool, identity: &Identity) -> Result<(), ()> {
    let mut builder = EventLoop::builder();
    if force_x11 {
        builder.with_x11();
    }
    let event_loop = builder.build().map_err(|_| ())?;
    eprintln!("GL-DEBUG runner: event loop built (x11={force_x11})");
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut runner = Runner {
        identity: identity.clone(),
        window: None,
    };
    event_loop.run_app(&mut runner).map_err(|_| ())?;
    eprintln!(
        "GL-DEBUG runner: run_app returned, window={}",
        runner.window.is_some()
    );
    if runner.window.is_some() {
        Ok(())
    } else {
        Err(())
    }
}

fn display_set(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| !value.is_empty())
}

struct Runner {
    identity: Identity,
    window: Option<Window>,
}

impl ApplicationHandler for Runner {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let x11 = event_loop.is_x11();
        match event_loop.create_window(window_attributes(&self.identity, x11)) {
            Ok(window) => {
                if !x11 {
                    // Wayland has no off-screen coordinates: minimize at once so the
                    // 1×1 surface never holds on to keyboard focus.
                    window.set_minimized(true);
                }
                self.window = Some(window);
            }
            Err(_) => {
                eprintln!("GL-DEBUG runner: create_window failed (x11={x11})");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        if matches!(event, WindowEvent::CloseRequested | WindowEvent::Destroyed) {
            eprintln!("GL-DEBUG runner: window event {event:?} -> exit");
            event_loop.exit();
        }
    }
}

fn window_attributes(identity: &Identity, x11: bool) -> winit::window::WindowAttributes {
    let attributes = winit::window::WindowAttributes::default()
        .with_title(identity.stem.clone())
        .with_inner_size(PhysicalSize::new(1_u32, 1_u32))
        .with_decorations(false)
        .with_resizable(false);
    if x11 {
        // Unmanaged and off-screen: no window manager decoration, no focus stealing,
        // and still a mapped window in the X11 window tree.
        let attributes = WindowAttributesExtX11::with_name(
            attributes,
            identity.name.clone(),
            identity.stem.clone(),
        );
        let attributes = WindowAttributesExtX11::with_override_redirect(attributes, true);
        attributes.with_position(PhysicalPosition::new(OFFSCREEN.0, OFFSCREEN.1))
    } else {
        WindowAttributesExtWayland::with_name(
            attributes,
            identity.name.clone(),
            identity.stem.clone(),
        )
    }
}

/// The Win32 runner dies with its job object. Here the kernel does it for us as soon
/// as the app process is gone.
fn set_parent_death_signal() {
    // SAFETY: prctl only changes this process's own settings, and these are the
    // documented constants for arming a signal on parent death.
    let armed = unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) };
    // SAFETY: getppid has no preconditions.
    if armed == 0 && unsafe { libc::getppid() } == 1 {
        // The parent was already gone, so no signal will ever arrive.
        std::process::exit(0);
    }
}

/// Stay alive without a display server. The process name and path are the real signal.
fn idle() {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}
