//! Win32 desktop glue: single instance, startup Run key, DWM frame styling, owner windows,
//! the work area, the clipboard, and the runner's integrity level and window icon.

use std::ffi::c_void;
use std::path::Path;
use std::process::Command;

use crate::log::Log;
use crate::platform::{Monitor, Rect, WorkArea};
use game_larper_core::{AppPaths, format_startup_command};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, POINT, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DWMWCP_ROUNDSMALL,
    DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, GetDC, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
    MONITORINFO, MonitorFromPoint, MonitorFromWindow, ReleaseDC,
};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenProcessToken, SetEvent,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, GWL_EXSTYLE, GWLP_HWNDPARENT, GetCursorPos, GetWindowLongPtrW,
    HICON, ICON_BIG, ICON_SMALL, ICONINFO, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    WM_SETICON, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
};

/// The Settings label for `set_run_at_startup`.
pub const STARTUP_LABEL: &str = "Launch with Windows";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const STARTUP_VALUE: &str = "GameLarper";
const MUTEX_NAME: &str = "Local\\GameLarper.SingleInstance";
const EVENT_NAME: &str = "Local\\GameLarper.Activate";
const CF_UNICODETEXT: u32 = 13;
/// DWM draws this 1px outline around both windows on Windows 11 (COLORREF, 0x00BBGGRR).
const BORDER_COLOR: u32 = 0x003A_2F26;

/// Become the primary instance, or wake the one already running and return false. A named
/// mutex per session; the paths and log are only used on Linux.
pub fn claim_primary_instance(_paths: &AppPaths, _log: &Log) -> bool {
    let name = wide(MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, name.as_ptr()) };
    if mutex.is_null() {
        return true;
    }
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        signal_activation();
        unsafe { CloseHandle(mutex) };
        false
    } else {
        // Leak the mutex for the process lifetime so the name stays owned.
        let _ = mutex;
        true
    }
}

pub fn watch_activation(on_signal: impl Fn() + Send + 'static) {
    let name = wide(EVENT_NAME);
    let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if event.is_null() {
        return;
    }
    let event = event as isize;
    std::thread::spawn(move || {
        loop {
            if unsafe { WaitForSingleObject(event as HANDLE, INFINITE) } == 0 {
                on_signal();
            }
        }
    });
}

/// Nothing to add to the startup log on Windows.
pub fn describe_session() -> Option<String> {
    None
}

fn signal_activation() {
    let name = wide(EVENT_NAME);
    let event = unsafe { OpenEvent(EVENT_MODIFY_STATE, 0, name.as_ptr()) };
    if !event.is_null() {
        unsafe {
            SetEvent(event);
            CloseHandle(event);
        }
    }
}

pub fn set_run_at_startup(
    enabled: bool,
    start_minimized: bool,
    executable: &Path,
) -> Result<(), String> {
    let name = wide(RUN_KEY);
    let mut key: HKEY = std::ptr::null_mut();
    let opened =
        unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, KEY_SET_VALUE, &mut key) };
    if opened != 0 {
        return Err("The per-user startup registry key is unavailable.".into());
    }
    let result = if enabled {
        let command = format_startup_command(executable, start_minimized)
            .map_err(|error| error.to_string())?;
        let mut value = wide(&command);
        let bytes = std::mem::size_of_val(value.as_slice()) as u32;
        unsafe {
            RegSetValueExW(
                key,
                wide(STARTUP_VALUE).as_ptr(),
                0,
                REG_SZ,
                value.as_mut_ptr() as *const u8,
                bytes,
            )
        }
    } else {
        unsafe { RegDeleteValueW(key, wide(STARTUP_VALUE).as_ptr()) }
    };
    unsafe { RegCloseKey(key) };
    if result != 0 && !(!enabled && result == 2) {
        return Err("Windows startup setting could not be changed.".into());
    }
    Ok(())
}

pub fn open_in_explorer(path: &Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|error| error.to_string())?;
    Command::new("explorer")
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The Win32 handle behind a shown Slint window.
fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let handle = window.window_handle();
    let handle = handle.window_handle().ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(window) => Some(window.hwnd.get() as HWND),
        _ => None,
    }
}

/// Small system corners and a quiet outline, so the frameless window still reads as native.
pub fn style_frame(window: &slint::Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let corners = DWMWCP_ROUNDSMALL as u32;
    let border = BORDER_COLOR;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corners as *const _ as *const c_void,
            std::mem::size_of_val(&corners) as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &border as *const _ as *const c_void,
            std::mem::size_of_val(&border) as u32,
        );
    }
}

/// Where the mouse is now. A tray click reports no position of its own, but the pointer is
/// still on the icon when Slint's `clicked` fires.
pub fn cursor_position() -> Option<(i32, i32)> {
    let mut point = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some((point.x, point.y))
}

/// The monitor holding `point` (the nearest one if it is outside every monitor), else the
/// primary monitor.
pub fn monitor_at(point: Option<(i32, i32)>) -> Option<Monitor> {
    let (x, y) = point.unwrap_or((0, 0));
    let flags = if point.is_some() {
        MONITOR_DEFAULTTONEAREST
    } else {
        MONITOR_DEFAULTTOPRIMARY
    };
    let monitor = unsafe { MonitorFromPoint(POINT { x, y }, flags) };
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    let scale = if unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) }
        == 0
        && dpi_x > 0
    {
        dpi_x as f32 / 96.0
    } else {
        1.0
    };
    let rect = |rc: windows_sys::Win32::Foundation::RECT| Rect {
        left: rc.left,
        top: rc.top,
        right: rc.right,
        bottom: rc.bottom,
    };
    Some(Monitor {
        bounds: rect(info.rcMonitor),
        work: rect(info.rcWork),
        scale,
    })
}

/// Style a frameless popup that is not a window of the app: Windows 11 corners and outline, a
/// shadow, and no taskbar button. Safe to repeat.
pub fn style_popup(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    // winit rebuilds the whole window style from its own flags whenever one changes, so its
    // flags go first and the raw style bits go last.
    window.with_winit_window(|window| {
        window.set_undecorated_shadow(true);
        window.set_skip_taskbar(true);
    });
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let corners = DWMWCP_ROUND as u32;
    let border = BORDER_COLOR;
    unsafe {
        // A tool window also stays out of Alt+Tab.
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let style = (style | WS_EX_TOOLWINDOW as isize) & !(WS_EX_APPWINDOW as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
        // The style change only reaches the shell once the frame is recalculated.
        SetWindowPos(
            hwnd,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &corners as *const _ as *const c_void,
            std::mem::size_of_val(&corners) as u32,
        );
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR as u32,
            &border as *const _ as *const c_void,
            std::mem::size_of_val(&border) as u32,
        );
    }
}

/// Make `window` the foreground window so it gets keyboard input and a later click elsewhere
/// takes focus away from it. Windows lets the process that just handled a tray click do this.
pub fn focus_window(window: &slint::Window) {
    if let Some(hwnd) = hwnd_of(window) {
        unsafe { SetForegroundWindow(hwnd) };
    }
}

/// Owned windows stay above their owner, minimize with it, and skip the taskbar.
pub fn set_owner(window: &slint::Window, owner: &slint::Window) {
    if let (Some(window), Some(owner)) = (hwnd_of(window), hwnd_of(owner)) {
        unsafe { SetWindowLongPtrW(window, GWLP_HWNDPARENT, owner as isize) };
    }
}

/// The docked panel belongs to the main window, so it gets no taskbar button of its own.
pub fn set_skip_taskbar(window: &slint::Window, skip: bool) {
    use slint::winit_030::WinitWindowAccessor;
    use slint::winit_030::winit::platform::windows::WindowExtWindows;
    window.with_winit_window(|window| window.set_skip_taskbar(skip));
}

/// The usable horizontal span (without the taskbar) of the monitor showing `window`.
pub fn work_area(window: &slint::Window) -> Option<WorkArea> {
    let hwnd = hwnd_of(window)?;
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    if unsafe { GetMonitorInfoW(monitor, &mut info) } == 0 {
        return None;
    }
    Some(WorkArea {
        left: info.rcWork.left,
        right: info.rcWork.right,
    })
}

/// Put plain text on the clipboard. Retries briefly if another app holds it.
pub fn copy_text(text: &str) -> Result<(), String> {
    let wide = wide(text);
    let bytes = std::mem::size_of_val(wide.as_slice());
    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(std::ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(15));
    }
    if !opened {
        return Err("The clipboard is busy.".into());
    }
    let result = unsafe {
        EmptyClipboard();
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes);
        if memory.is_null() {
            Err("Out of memory for the clipboard.".to_string())
        } else {
            let target = GlobalLock(memory) as *mut u16;
            if target.is_null() {
                windows_sys::Win32::Foundation::GlobalFree(memory);
                Err("The clipboard could not be written.".to_string())
            } else {
                std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
                GlobalUnlock(memory);
                // On success the clipboard owns the memory.
                if SetClipboardData(CF_UNICODETEXT, memory).is_null() {
                    windows_sys::Win32::Foundation::GlobalFree(memory);
                    Err("The clipboard could not be written.".to_string())
                } else {
                    Ok(())
                }
            }
        }
    };
    unsafe { CloseClipboard() };
    result
}

pub fn integrity_of(process: HANDLE) -> String {
    let _ = process;
    let mut token: HANDLE = std::ptr::null_mut();
    let opened = unsafe { OpenProcessToken(process, 0x0008, &mut token) };
    if opened == 0 {
        return "unknown".into();
    }
    let label = integrity_label(token);
    unsafe { CloseHandle(token) };
    label
}

fn integrity_label(token: HANDLE) -> String {
    use windows_sys::Win32::Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TOKEN_MANDATORY_LABEL,
        TokenIntegrityLevel,
    };
    let mut size = 0u32;
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            std::ptr::null_mut(),
            0,
            &mut size,
        );
    }
    if size == 0 {
        return "unknown".into();
    }
    let mut buffer = vec![0u8; size as usize];
    let read = unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            buffer.as_mut_ptr() as *mut c_void,
            size,
            &mut size,
        )
    };
    if read == 0 {
        return "unknown".into();
    }
    let label = unsafe { &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
    let count = unsafe { *GetSidSubAuthorityCount(label.Label.Sid) };
    if count == 0 {
        return "unknown".into();
    }
    let rid = unsafe { *GetSidSubAuthority(label.Label.Sid, (count - 1) as u32) };
    match rid {
        0x0000 => "untrusted",
        0x1000 => "low",
        0x2000 => "medium",
        0x3000 => "high",
        0x4000 => "system",
        _ => "unknown",
    }
    .into()
}

pub fn apply_window_icon(hwnd: isize, image_path: &Path) -> Option<isize> {
    let icon = icon_from_image(image_path)?;
    unsafe {
        SendMessageW(hwnd as HWND, WM_SETICON, ICON_BIG as WPARAM, icon as LPARAM);
        SendMessageW(
            hwnd as HWND,
            WM_SETICON,
            ICON_SMALL as WPARAM,
            icon as LPARAM,
        );
    }
    Some(icon as isize)
}

pub fn destroy_icon(icon: isize) {
    if icon != 0 {
        unsafe { DestroyIcon(icon as HICON) };
    }
}

fn icon_from_image(path: &Path) -> Option<HICON> {
    let image = image::open(path)
        .ok()?
        .resize_exact(32, 32, image::imageops::FilterType::Triangle)
        .to_rgba8();
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: 32,
            biHeight: -32,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..unsafe { std::mem::zeroed() }
        },
        ..unsafe { std::mem::zeroed() }
    };
    let mut bits: *mut c_void = std::ptr::null_mut();
    let screen = unsafe { GetDC(std::ptr::null_mut()) };
    let color = unsafe {
        CreateDIBSection(
            screen,
            &info,
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        )
    };
    if !screen.is_null() {
        unsafe { ReleaseDC(std::ptr::null_mut(), screen) };
    }
    if color.is_null() || bits.is_null() {
        return None;
    }
    let pixels = unsafe { std::slice::from_raw_parts_mut(bits as *mut u8, 32 * 32 * 4) };
    for (index, pixel) in image.pixels().enumerate() {
        let offset = index * 4;
        pixels[offset] = pixel[2];
        pixels[offset + 1] = pixel[1];
        pixels[offset + 2] = pixel[0];
        pixels[offset + 3] = pixel[3];
    }
    let mask = unsafe { CreateBitmap(32, 32, 1, 1, std::ptr::null()) };
    let mut icon_info: ICONINFO = unsafe { std::mem::zeroed() };
    icon_info.fIcon = 1;
    icon_info.hbmColor = color;
    icon_info.hbmMask = mask;
    let icon = unsafe { CreateIconIndirect(&icon_info) };
    unsafe {
        DeleteObject(color);
        if !mask.is_null() {
            DeleteObject(mask);
        }
    }
    if icon.is_null() { None } else { Some(icon) }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

use windows_sys::Win32::System::Threading::OpenEventW as OpenEvent;
