//! Keeps the side panel docked beside the main window.
//!
//! Slint's winit backend draws a `PopupWindow` inside its parent, so the panel is a second,
//! owned top-level window. It sits right of the main window, flips left when the screen edge is
//! too close, and overlays the right edge when the main window is maximized.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, PhysicalPosition, PhysicalSize, Timer, TimerMode, Weak};

use crate::app::{MainWindow, SidePanel};
use crate::platform;

/// Logical width; matches the fixed width in side-panel.slint.
const WIDTH: f32 = 336.0;
const GAP: f32 = 8.0;
const TITLE_BAR: f32 = 40.0;
/// Slightly longer than the slide/fade in side-panel.slint.
const CLOSE_DELAY: Duration = Duration::from_millis(170);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Queue,
    Settings,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Queue => "queue",
            Mode::Settings => "settings",
        }
    }
}

pub struct Dock {
    main: Weak<MainWindow>,
    panel: Weak<SidePanel>,
    mode: Cell<Option<Mode>>,
    styled: Cell<bool>,
    motion: Timer,
}

impl Dock {
    pub fn new(main: &MainWindow, panel: &SidePanel) -> Rc<Self> {
        let dock = Rc::new(Self {
            main: main.as_weak(),
            panel: panel.as_weak(),
            mode: Cell::new(None),
            styled: Cell::new(false),
            motion: Timer::default(),
        });
        let weak = Rc::downgrade(&dock);
        main.window().on_winit_window_event(move |_, event| {
            if matches!(
                event,
                WindowEvent::Moved(_)
                    | WindowEvent::Resized(_)
                    | WindowEvent::ScaleFactorChanged { .. }
            ) && let Some(dock) = weak.upgrade()
                && dock.mode.get().is_some()
            {
                dock.place();
            }
            EventResult::Propagate
        });
        dock
    }

    pub fn mode(&self) -> Option<Mode> {
        self.mode.get()
    }

    /// Open `mode`, or switch the open panel to it.
    pub fn open(&self, mode: Mode) {
        let (Some(main), Some(panel)) = (self.main.upgrade(), self.panel.upgrade()) else {
            return;
        };
        let switching = self.mode.get().is_some();
        self.mode.set(Some(mode));
        main.set_panel(mode.name().into());
        panel.set_mode(mode.name().into());
        self.place();
        if !switching {
            panel.set_shown(false);
            if panel.show().is_err() {
                return;
            }
            self.style_once(&main, &panel);
            // Placement again: the first show creates the native window.
            self.place();
        }
        // Restart the slide on the next frame so the change animates.
        panel.set_shown(false);
        let weak = self.panel.clone();
        self.motion.start(
            TimerMode::SingleShot,
            Duration::from_millis(16),
            move || {
                if let Some(panel) = weak.upgrade() {
                    panel.set_shown(true);
                }
            },
        );
    }

    pub fn close(&self) {
        let (Some(main), Some(panel)) = (self.main.upgrade(), self.panel.upgrade()) else {
            return;
        };
        if self.mode.take().is_none() {
            return;
        }
        main.set_panel("".into());
        panel.set_shown(false);
        let weak = self.panel.clone();
        self.motion
            .start(TimerMode::SingleShot, CLOSE_DELAY, move || {
                if let Some(panel) = weak.upgrade() {
                    let _ = panel.hide();
                }
            });
    }

    /// Hide at once, for example when the main window goes to the tray.
    pub fn dismiss(&self) {
        self.motion.stop();
        self.mode.set(None);
        if let Some(main) = self.main.upgrade() {
            main.set_panel("".into());
        }
        if let Some(panel) = self.panel.upgrade() {
            panel.set_shown(false);
            let _ = panel.hide();
        }
    }

    fn style_once(&self, main: &MainWindow, panel: &SidePanel) {
        if self.styled.replace(true) {
            return;
        }
        platform::set_skip_taskbar(panel.window(), true);
        platform::set_owner(panel.window(), main.window());
        platform::style_frame(panel.window());
    }

    fn place(&self) {
        let (Some(main), Some(panel)) = (self.main.upgrade(), self.panel.upgrade()) else {
            return;
        };
        let window = main.window();
        if window.is_minimized() {
            return;
        }
        let scale = window.scale_factor();
        let position = window.position();
        let size = window.size();
        let width = (WIDTH * scale).round() as i32;
        let gap = (GAP * scale).round() as i32;
        let height = size.height as i32;
        let area = platform::work_area(window);
        let right = position.x + size.width as i32 + gap;
        let left = position.x - gap - width;
        let fits_right = area.is_none_or(|area| right + width <= area.right);
        let fits_left = area.is_none_or(|area| left >= area.left);
        let (x, y, h, from_left) = if !window.is_maximized() && fits_right {
            (right, position.y, height, false)
        } else if !window.is_maximized() && fits_left {
            (left, position.y, height, true)
        } else {
            // No room outside: overlay the right edge, below the title bar.
            let top = (TITLE_BAR * scale).round() as i32;
            (
                position.x + size.width as i32 - width,
                position.y + top,
                height - top,
                false,
            )
        };
        panel.set_from_left(from_left);
        panel
            .window()
            .set_size(PhysicalSize::new(width as u32, h.max(1) as u32));
        panel.window().set_position(PhysicalPosition::new(x, y));
    }
}
