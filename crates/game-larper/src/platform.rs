use std::ffi::c_void;
use std::path::Path;
use std::process::Command;

use game_larper_core::format_startup_command;
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, HWND, LPARAM, WPARAM,
};
use windows_sys::Win32::Graphics::Dwm::{
    DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND, DwmSetWindowAttribute,
};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject, GetDC, ReleaseDC,
};
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_SZ, RegCloseKey, RegDeleteValueW, RegOpenKeyExW,
    RegSetValueExW,
};
use windows_sys::Win32::System::Threading::{
    CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, INFINITE, OpenProcessToken, SetEvent,
    WaitForSingleObject,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, DestroyIcon, HICON, ICON_BIG, ICON_SMALL, ICONINFO, SendMessageW,
    WM_SETICON,
};

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const STARTUP_VALUE: &str = "GameLarper";
const MUTEX_NAME: &str = "Local\\GameLarper.SingleInstance";
const EVENT_NAME: &str = "Local\\GameLarper.Activate";

pub fn claim_primary_instance() -> bool {
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

pub fn round_corners(hwnd: HWND) {
    let preference = DWMWCP_ROUND as u32;
    unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            &preference as *const _ as *const c_void,
            std::mem::size_of_val(&preference) as u32,
        );
    }
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
