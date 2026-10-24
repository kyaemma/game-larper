//! The Windows tray icon.
//!
//! Slint's `SystemTrayIcon` reports one thing, a left click without position. A right click is
//! wired to its own native menu and there is no double-click, so the tray panel cannot be
//! driven from it. This is a small `Shell_NotifyIconW` icon that reports left click, double
//! click and right click, with the pointer position of the click.
//!
//! It uses the legacy notification protocol, the one Slint's icon uses: the mouse message is
//! the low word of `lParam`. The owner is a hidden top-level window rather than a message-only
//! one, because only top-level windows receive the `TaskbarCreated` broadcast that says the
//! shell restarted and the icon must be added again.
//!
//! Everything runs on the UI thread, inside the message pump of the Slint event loop. Dropping
//! `NativeTray` removes the icon and destroys the window.

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcessId;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, CreateWindowExW, DefWindowProcW, DestroyWindow, GWLP_USERDATA,
    GetMessagePos, GetSystemMetrics, GetWindowLongPtrW, HICON, IDI_APPLICATION, IMAGE_ICON,
    KillTimer, LR_SHARED, LoadIconW, LoadImageW, RegisterClassExW, RegisterWindowMessageW,
    SM_CXSMICON, SM_CYSMICON, SetTimer, SetWindowLongPtrW, UnregisterClassW, WM_APP,
    WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::platform::TrayEvent;
use crate::platform::click::{ClickTracker, LeftUp};

const CLASS_NAME: &str = "GameLarper.TrayWindow";
const TASKBAR_CREATED: &str = "TaskbarCreated";
/// The callback message the shell sends for mouse activity on the icon.
const WM_TRAY: u32 = WM_APP + 1;
const TRAY_UID: u32 = 1;
/// Timer that tells a single click from the first half of a double-click.
const CLICK_TIMER: usize = 1;
/// The id of the application icon `build.rs` embeds.
const APP_ICON_ID: usize = 1;
/// `szTip` holds 128 UTF-16 units including the terminator.
const TIP_UNITS: usize = 127;

type Handler = Box<dyn Fn(TrayEvent)>;

pub struct NativeTray {
    state: Box<State>,
}

struct State {
    hwnd: Cell<HWND>,
    icon: HICON,
    taskbar_created: u32,
    on_event: RefCell<Option<Handler>>,
    clicks: RefCell<ClickTracker>,
    /// Where the pointer was at the first click of a pending single click.
    first_anchor: Cell<Option<(i32, i32)>>,
    tip: RefCell<Vec<u16>>,
}

impl NativeTray {
    /// Add the icon. `None` when the owner window cannot be created, so the caller can fall
    /// back to another tray.
    pub fn new() -> Option<Self> {
        let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) } as HINSTANCE;
        let class = wide(CLASS_NAME);
        let window_class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        // A second registration (it would only happen after a failed first start) is fine.
        unsafe { RegisterClassExW(&window_class) };
        // Never shown, never activated: it only owns the icon.
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class.as_ptr(),
                std::ptr::null(),
                WS_POPUP,
                0,
                0,
                0,
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            )
        };
        if hwnd.is_null() {
            unsafe { UnregisterClassW(class.as_ptr(), hinstance) };
            return None;
        }
        let state = Box::new(State {
            hwnd: Cell::new(hwnd),
            icon: load_icon(hinstance),
            taskbar_created: unsafe { RegisterWindowMessageW(wide(TASKBAR_CREATED).as_ptr()) },
            on_event: RefCell::new(None),
            clicks: RefCell::new(ClickTracker::default()),
            first_anchor: Cell::new(None),
            tip: RefCell::new(tip_units("Game Larper")),
        });
        // The Box keeps this address for as long as the window exists.
        unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, &*state as *const State as isize) };
        state.add_icon();
        Some(Self { state })
    }

    /// Where events go. They run on this thread, from the Slint event loop, and nothing is
    /// delivered until the loop runs, so this can be set after `new`.
    pub fn on_event(&self, handler: impl Fn(TrayEvent) + 'static) {
        *self.state.on_event.borrow_mut() = Some(Box::new(handler));
    }

    /// Change the hover text. Cheap when it did not change.
    pub fn set_tooltip(&self, text: &str) {
        let units = tip_units(text);
        if *self.state.tip.borrow() == units {
            return;
        }
        *self.state.tip.borrow_mut() = units;
        let mut data = self.state.icon_data(NIF_TIP);
        copy_tip(&mut data, &self.state.tip.borrow());
        unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
    }
}

impl Drop for NativeTray {
    fn drop(&mut self) {
        let hwnd = self.state.hwnd.get();
        unsafe {
            KillTimer(hwnd, CLICK_TIMER);
            let data = self.state.icon_data(0);
            Shell_NotifyIconW(NIM_DELETE, &data);
            // After this the window procedure can no longer see the state.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DestroyWindow(hwnd);
            UnregisterClassW(
                wide(CLASS_NAME).as_ptr(),
                GetModuleHandleW(std::ptr::null()),
            );
        }
    }
}

impl State {
    fn icon_data(&self, flags: u32) -> NOTIFYICONDATAW {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd.get();
        data.uID = TRAY_UID;
        data.uFlags = flags;
        data
    }

    fn add_icon(&self) {
        let mut data = self.icon_data(NIF_MESSAGE | NIF_ICON | NIF_TIP);
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = self.icon;
        copy_tip(&mut data, &self.tip.borrow());
        unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
    }

    fn emit(&self, event: TrayEvent) {
        // A panic must not unwind through the window procedure. The panic hook has logged it.
        let handler = self.on_event.borrow();
        if let Some(handler) = handler.as_ref() {
            let _ = catch_unwind(AssertUnwindSafe(|| handler(event)));
        }
    }

    fn tray_message(&self, mouse_message: u32) {
        match mouse_message {
            WM_LBUTTONUP => self.left_up(),
            WM_RBUTTONUP | WM_CONTEXTMENU => {
                // A right click ends any left click that was still waiting.
                self.clicks.borrow_mut().cancel();
                unsafe { KillTimer(self.hwnd.get(), CLICK_TIMER) };
                self.emit(TrayEvent::RightClick {
                    anchor: message_position(),
                });
            }
            _ => {}
        }
    }

    fn left_up(&self) {
        // The click gave this process the right to take the foreground. The reaction to a
        // single click comes after the double-click time, so keep that right for later.
        unsafe { AllowSetForegroundWindow(GetCurrentProcessId()) };
        let delay = Duration::from_millis(u64::from(unsafe { GetDoubleClickTime() }));
        let verdict = self.clicks.borrow_mut().left_up(Instant::now(), delay);
        match verdict {
            LeftUp::Wait => {
                self.first_anchor.set(message_position());
                unsafe { SetTimer(self.hwnd.get(), CLICK_TIMER, delay.as_millis() as u32, None) };
            }
            LeftUp::Double => {
                unsafe { KillTimer(self.hwnd.get(), CLICK_TIMER) };
                self.emit(TrayEvent::DoubleClick);
            }
        }
    }

    fn click_timer(&self) {
        unsafe { KillTimer(self.hwnd.get(), CLICK_TIMER) };
        let first = self.clicks.borrow_mut().expired();
        if let Some(at) = first {
            self.emit(TrayEvent::Click {
                at,
                anchor: self.first_anchor.take(),
            });
        }
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const State;
    // SAFETY: the pointer is set after the Box is built and cleared before it is dropped, on
    // this same thread, so a non-null value always points at a live `State`.
    if let Some(state) = unsafe { state.as_ref() } {
        if msg == WM_TRAY {
            state.tray_message((lparam as u32) & 0xFFFF);
            return 0;
        }
        if msg == WM_TIMER && wparam == CLICK_TIMER {
            state.click_timer();
            return 0;
        }
        // The shell restarted: its tray is empty, so add the icon again.
        if msg == state.taskbar_created && msg != 0 {
            state.add_icon();
            return 0;
        }
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Where the pointer was when the message being handled was sent.
fn message_position() -> Option<(i32, i32)> {
    let packed = unsafe { GetMessagePos() };
    Some((
        i32::from(packed as u16 as i16),
        i32::from((packed >> 16) as u16 as i16),
    ))
}

/// The application icon from the executable's resources, else the stock one.
fn load_icon(hinstance: HINSTANCE) -> HICON {
    let width = unsafe { GetSystemMetrics(SM_CXSMICON) };
    let height = unsafe { GetSystemMetrics(SM_CYSMICON) };
    // A shared icon belongs to the system, so there is nothing to free.
    let icon = unsafe {
        LoadImageW(
            hinstance,
            APP_ICON_ID as *const u16,
            IMAGE_ICON,
            width,
            height,
            LR_SHARED,
        )
    } as HICON;
    if icon.is_null() {
        unsafe { LoadIconW(std::ptr::null_mut(), IDI_APPLICATION) }
    } else {
        icon
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16 for the tooltip, cut to what `szTip` holds. Without a terminator. A character made
/// of two units is dropped whole rather than cut in half.
fn tip_units(text: &str) -> Vec<u16> {
    let mut units: Vec<u16> = text.encode_utf16().take(TIP_UNITS).collect();
    if units
        .last()
        .is_some_and(|unit| (0xD800..0xDC00).contains(unit))
    {
        units.pop();
    }
    units
}

fn copy_tip(data: &mut NOTIFYICONDATAW, units: &[u16]) {
    let length = units.len().min(TIP_UNITS);
    data.szTip[..length].copy_from_slice(&units[..length]);
    data.szTip[length] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_tooltip_is_kept_whole() {
        assert_eq!(
            tip_units("Game Larper"),
            "Game Larper".encode_utf16().collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_long_tooltip_is_cut_to_what_the_shell_holds() {
        assert_eq!(tip_units(&"a".repeat(300)).len(), TIP_UNITS);
    }

    #[test]
    fn a_character_of_two_units_is_not_cut_in_half() {
        // 126 units, then a character that needs two: only the first would fit.
        let text = format!("{}🔮", "a".repeat(TIP_UNITS - 1));
        let units = tip_units(&text);
        assert_eq!(units.len(), TIP_UNITS - 1);
        assert!(String::from_utf16(&units).is_ok());
    }

    #[test]
    fn the_tooltip_is_copied_with_a_terminator() {
        let mut data: NOTIFYICONDATAW = unsafe { std::mem::zeroed() };
        data.szTip = [0x7777; 128];
        copy_tip(&mut data, &tip_units("Hi"));
        assert_eq!(&data.szTip[..3], &[b'H' as u16, b'i' as u16, 0]);
    }

    #[test]
    fn the_pointer_position_of_a_message_keeps_its_sign() {
        // Monitors left of or above the primary one have negative coordinates.
        let packed = ((-20i16 as u16 as u32) << 16) | (-1500i16 as u16 as u32);
        assert_eq!(
            (
                i32::from(packed as u16 as i16),
                i32::from((packed >> 16) as u16 as i16)
            ),
            (-1500, -20)
        );
    }
}
