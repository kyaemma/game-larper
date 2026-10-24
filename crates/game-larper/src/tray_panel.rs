//! The tray panel: a small frameless window that opens from the tray icon.
//!
//! Slint's `SystemTrayIcon` reports a click and nothing else: no icon rectangle and no way to
//! anchor a popup. So the panel is a second top-level window, placed from the pointer, which
//! is still on the icon when the click arrives. It closes when it loses focus or on Esc.
//!
//! Known limits: a click made from the keyboard has the pointer elsewhere, so the panel falls
//! back to the corner by the taskbar; and with an auto-hidden taskbar there is no taskbar edge
//! to read, so it also takes the bottom-right corner.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use slint::winit_030::winit::event::WindowEvent;
use slint::winit_030::{EventResult, WinitWindowAccessor};
use slint::{ComponentHandle, PhysicalPosition, PhysicalSize, Timer, TimerMode, Weak};

use crate::app::TrayPanelWindow;
use crate::platform::{self, Monitor, Rect};

/// Logical size; matches the fixed size in tray-panel.slint.
const WIDTH: f32 = 308.0;
const HEIGHT: f32 = 404.0;
/// Logical gap between the panel and the taskbar, and between the panel and the screen edge.
const MARGIN: f32 = 10.0;
/// A click on the tray icon takes focus away from an open panel before the click itself
/// arrives. Without this the panel would close and the same click would open it again.
const REOPEN_GUARD: Duration = Duration::from_millis(350);
/// If the panel is not the foreground window by then, nothing could dismiss it, so close it.
const FOCUS_GRACE: Duration = Duration::from_millis(1500);

pub struct TrayPanel {
    window: Weak<TrayPanelWindow>,
    /// Read by the session to know whether the panel's artwork is worth fetching and keeping.
    open_flag: Arc<AtomicBool>,
    open: Cell<bool>,
    /// The panel has held the foreground since it opened. Losing it before that is not a
    /// click elsewhere.
    focused: Cell<bool>,
    closed_at: Cell<Option<Instant>>,
    fade: Timer,
    watchdog: Timer,
    deferred: Timer,
}

impl TrayPanel {
    pub fn new(window: &TrayPanelWindow, open_flag: Arc<AtomicBool>) -> Rc<Self> {
        let panel = Rc::new(Self {
            window: window.as_weak(),
            open_flag,
            open: Cell::new(false),
            focused: Cell::new(false),
            closed_at: Cell::new(None),
            fade: Timer::default(),
            watchdog: Timer::default(),
            deferred: Timer::default(),
        });
        let weak = Rc::downgrade(&panel);
        window.window().on_winit_window_event(move |_, event| {
            if let Some(panel) = weak.upgrade() {
                match event {
                    WindowEvent::Focused(true) => panel.focused.set(true),
                    WindowEvent::Focused(false) if panel.open.get() && panel.focused.get() => {
                        panel.hide_soon();
                    }
                    _ => {}
                }
            }
            EventResult::Propagate
        });
        panel
    }

    /// A single left click on the tray icon: open the panel, or close it if it is open.
    /// `clicked_at` is when the button came up. The caller may act later, to tell a single
    /// click from a double-click, so a panel that this very click took focus from, and so
    /// closed, is judged by that time and stays closed. `anchor` is where the pointer was.
    /// `prepare` runs just before the panel opens, so its first frame already shows current
    /// data. Returns true when the panel was opened.
    pub fn toggle_from_click(
        self: &Rc<Self>,
        clicked_at: Instant,
        anchor: Option<(i32, i32)>,
        prepare: impl FnOnce(),
    ) -> bool {
        if self.open.get() {
            self.hide();
            return false;
        }
        if self
            .closed_at
            .get()
            .is_some_and(|at| clicked_at.saturating_duration_since(at) < REOPEN_GUARD)
        {
            return false;
        }
        prepare();
        self.show(anchor)
    }

    /// Show the panel, or leave it as it is when it is already open. Returns true when it is
    /// open afterwards.
    pub fn open(self: &Rc<Self>, anchor: Option<(i32, i32)>, prepare: impl FnOnce()) -> bool {
        if self.open.get() {
            return true;
        }
        prepare();
        self.show(anchor)
    }

    fn show(self: &Rc<Self>, anchor: Option<(i32, i32)>) -> bool {
        let Some(window) = self.window.upgrade() else {
            return false;
        };
        self.fade.stop();
        self.deferred.stop();
        self.focused.set(false);
        window.set_shown(false);
        window.set_tab(0);
        let target = target_rect(anchor);
        apply(&window, target);
        if window.show().is_err() {
            return false;
        }
        self.open.set(true);
        self.open_flag.store(true, Ordering::Release);
        platform::style_popup(window.window());
        // The first show creates the native window, which may sit on a different scale.
        apply(&window, target);
        platform::focus_window(window.window());
        let weak = self.window.clone();
        self.fade.start(
            TimerMode::SingleShot,
            Duration::from_millis(16),
            move || {
                if let Some(window) = weak.upgrade() {
                    window.set_shown(true);
                }
            },
        );
        let panel = Rc::downgrade(self);
        self.watchdog
            .start(TimerMode::SingleShot, FOCUS_GRACE, move || {
                if let Some(panel) = panel.upgrade()
                    && panel.open.get()
                    && !panel.focused.get()
                {
                    panel.hide();
                }
            });
        true
    }

    pub fn hide(&self) {
        if !self.open.replace(false) {
            return;
        }
        self.open_flag.store(false, Ordering::Release);
        self.closed_at.set(Some(Instant::now()));
        self.fade.stop();
        self.watchdog.stop();
        self.deferred.stop();
        if let Some(window) = self.window.upgrade() {
            window.set_shown(false);
            let _ = window.hide();
        }
    }

    /// Hide on the next turn of the event loop instead of inside the window's own event.
    fn hide_soon(self: &Rc<Self>) {
        let panel = Rc::downgrade(self);
        self.deferred
            .start(TimerMode::SingleShot, Duration::ZERO, move || {
                if let Some(panel) = panel.upgrade() {
                    panel.hide();
                }
            });
    }
}

/// Where the panel goes now, in physical pixels. `anchor` is where the pointer was when the
/// tray was used, else it is where the pointer is now.
fn target_rect(anchor: Option<(i32, i32)>) -> Option<Rect> {
    let anchor = anchor.or_else(platform::cursor_position);
    let monitor = platform::monitor_at(anchor)?;
    let width = (WIDTH * monitor.scale).round() as i32;
    let height = (HEIGHT * monitor.scale).round() as i32;
    let margin = (MARGIN * monitor.scale).round() as i32;
    let (x, y) = popup_origin(anchor, &monitor, (width, height), margin);
    Some(Rect {
        left: x,
        top: y,
        right: x + width,
        bottom: y + height,
    })
}

fn apply(window: &TrayPanelWindow, target: Option<Rect>) {
    let Some(target) = target else {
        return;
    };
    window.window().set_size(PhysicalSize::new(
        (target.right - target.left) as u32,
        (target.bottom - target.top) as u32,
    ));
    window
        .window()
        .set_position(PhysicalPosition::new(target.left, target.top));
}

/// The top-left corner for a popup of `size` opened from the tray.
///
/// The popup sits against the work area, on the side of the taskbar. Along that edge it is
/// centered on `anchor` when the pointer is on the taskbar, which is where a tray click
/// happens; otherwise (keyboard activation, an overflow flyout, no taskbar to read) it takes
/// the corner nearest the notification area.
pub fn popup_origin(
    anchor: Option<(i32, i32)>,
    monitor: &Monitor,
    size: (i32, i32),
    margin: i32,
) -> (i32, i32) {
    let (width, height) = size;
    let bounds = monitor.bounds;
    let work = monitor.work;
    let insets = [
        (Edge::Bottom, bounds.bottom - work.bottom),
        (Edge::Top, work.top - bounds.top),
        (Edge::Left, work.left - bounds.left),
        (Edge::Right, bounds.right - work.right),
    ];
    let (edge, inset) = insets
        .into_iter()
        .max_by_key(|&(_, inset)| inset)
        .unwrap_or((Edge::Bottom, 0));
    let edge = if inset > 0 { edge } else { Edge::Bottom };

    let strip = match edge {
        Edge::Bottom => Rect {
            top: work.bottom,
            ..bounds
        },
        Edge::Top => Rect {
            bottom: work.top,
            ..bounds
        },
        Edge::Left => Rect {
            right: work.left,
            ..bounds
        },
        Edge::Right => Rect {
            left: work.right,
            ..bounds
        },
    };
    let anchor = anchor.filter(|&(x, y)| inset > 0 && strip.contains(x, y));

    // Keep the panel inside the work area even when it is larger than the space left.
    let clamp = |value: i32, low: i32, high: i32| value.min(high).max(low);
    let across_x = |center: Option<i32>| match center {
        Some(x) => clamp(
            x - width / 2,
            work.left + margin,
            work.right - width - margin,
        ),
        None => clamp(work.right - width - margin, work.left + margin, work.right),
    };
    let across_y = |center: Option<i32>| match center {
        Some(y) => clamp(
            y - height / 2,
            work.top + margin,
            work.bottom - height - margin,
        ),
        None => clamp(
            work.bottom - height - margin,
            work.top + margin,
            work.bottom,
        ),
    };
    match edge {
        Edge::Bottom => (
            across_x(anchor.map(|(x, _)| x)),
            clamp(work.bottom - height - margin, work.top, work.bottom),
        ),
        Edge::Top => (across_x(anchor.map(|(x, _)| x)), work.top + margin),
        Edge::Left => (work.left + margin, across_y(anchor.map(|(_, y)| y))),
        Edge::Right => (
            clamp(work.right - width - margin, work.left, work.right),
            across_y(anchor.map(|(_, y)| y)),
        ),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Edge {
    Bottom,
    Top,
    Left,
    Right,
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: (i32, i32) = (308, 404);
    const MARGIN: i32 = 10;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// 1920x1080 with a 48 px taskbar along `edge`.
    fn monitor(edge: Edge) -> Monitor {
        let bounds = rect(0, 0, 1920, 1080);
        let work = match edge {
            Edge::Bottom => rect(0, 0, 1920, 1032),
            Edge::Top => rect(0, 48, 1920, 1080),
            Edge::Left => rect(48, 0, 1920, 1080),
            Edge::Right => rect(0, 0, 1872, 1080),
        };
        Monitor {
            bounds,
            work,
            scale: 1.0,
        }
    }

    #[test]
    fn bottom_taskbar_centers_on_the_icon_above_the_taskbar() {
        let origin = popup_origin(Some((1700, 1050)), &monitor(Edge::Bottom), SIZE, MARGIN);
        assert_eq!(origin, (1700 - 154, 1032 - 404 - MARGIN));
    }

    #[test]
    fn the_panel_stays_inside_the_screen_near_the_right_edge() {
        let origin = popup_origin(Some((1915, 1050)), &monitor(Edge::Bottom), SIZE, MARGIN);
        assert_eq!(origin.0, 1920 - 308 - MARGIN);
    }

    #[test]
    fn the_panel_stays_inside_the_screen_near_the_left_edge() {
        let origin = popup_origin(Some((5, 1050)), &monitor(Edge::Bottom), SIZE, MARGIN);
        assert_eq!(origin.0, MARGIN);
    }

    #[test]
    fn a_pointer_away_from_the_taskbar_falls_back_to_the_corner() {
        // Keyboard activation or the overflow flyout: the pointer is in the work area.
        let corner = (1920 - 308 - MARGIN, 1032 - 404 - MARGIN);
        assert_eq!(
            popup_origin(Some((900, 500)), &monitor(Edge::Bottom), SIZE, MARGIN),
            corner
        );
        assert_eq!(
            popup_origin(None, &monitor(Edge::Bottom), SIZE, MARGIN),
            corner
        );
    }

    #[test]
    fn top_taskbar_opens_below_it() {
        let origin = popup_origin(Some((1700, 20)), &monitor(Edge::Top), SIZE, MARGIN);
        assert_eq!(origin, (1700 - 154, 48 + MARGIN));
    }

    #[test]
    fn side_taskbars_open_beside_them_centered_on_the_icon() {
        let left = popup_origin(Some((20, 600)), &monitor(Edge::Left), SIZE, MARGIN);
        assert_eq!(left, (48 + MARGIN, 600 - 202));
        let right = popup_origin(Some((1900, 600)), &monitor(Edge::Right), SIZE, MARGIN);
        assert_eq!(right, (1872 - 308 - MARGIN, 600 - 202));
    }

    #[test]
    fn a_hidden_taskbar_takes_the_bottom_right_corner() {
        let monitor = Monitor {
            bounds: rect(0, 0, 1920, 1080),
            work: rect(0, 0, 1920, 1080),
            scale: 1.0,
        };
        assert_eq!(
            popup_origin(Some((1700, 1079)), &monitor, SIZE, MARGIN),
            (1920 - 308 - MARGIN, 1080 - 404 - MARGIN)
        );
    }

    #[test]
    fn a_second_monitor_uses_its_own_coordinates() {
        // A monitor left of the primary one has negative coordinates.
        let monitor = Monitor {
            bounds: rect(-1920, 0, 0, 1080),
            work: rect(-1920, 0, 0, 1032),
            scale: 1.0,
        };
        let origin = popup_origin(Some((-300, 1050)), &monitor, SIZE, MARGIN);
        assert_eq!(origin, (-300 - 154, 1032 - 404 - MARGIN));
    }

    #[test]
    fn a_panel_taller_than_the_work_area_still_starts_on_screen() {
        let monitor = Monitor {
            bounds: rect(0, 0, 1280, 400),
            work: rect(0, 0, 1280, 352),
            scale: 1.0,
        };
        let origin = popup_origin(Some((1200, 380)), &monitor, SIZE, MARGIN);
        assert_eq!(origin.1, 0);
    }
}
