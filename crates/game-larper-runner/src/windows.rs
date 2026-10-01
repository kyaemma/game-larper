//! The Win32 runner: an ownerless, off-screen, non-activating window and a message loop.

use std::path::Path;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, MSG,
    PostQuitMessage, RegisterClassW, SW_SHOWNOACTIVATE, ShowWindow, TranslateMessage, WM_CLOSE,
    WM_DESTROY, WM_ENDSESSION, WM_MOUSEACTIVATE, WM_QUERYENDSESSION, WNDCLASSW, WS_EX_APPWINDOW,
    WS_EX_NOACTIVATE, WS_OVERLAPPEDWINDOW,
};

const MA_NOACTIVATE: LRESULT = 3;

pub fn run() -> i32 {
    let title = module_title();
    let class_name = wide("GameLarper.RunnerWindow");
    // SAFETY: a null module name asks for this process, which cannot be unloaded while we run.
    let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if instance.is_null() {
        return 1;
    }

    // SAFETY: zero is a valid initial WNDCLASSW; the fields we use are set before registration.
    let mut window_class: WNDCLASSW = unsafe { std::mem::zeroed() };
    window_class.lpfnWndProc = Some(window_proc);
    window_class.hInstance = instance;
    window_class.lpszClassName = class_name.as_ptr();
    // SAFETY: class_name is NUL-terminated and kept alive until the message loop ends.
    let atom = unsafe { RegisterClassW(&window_class) };
    if atom == 0 {
        return 2;
    }

    let title_wide = wide(&title);
    // SAFETY: class and title pointers are NUL-terminated and live for this call.
    // The window is ownerless, off-screen, and non-activating so it does not steal focus.
    let window = unsafe {
        CreateWindowExW(
            WS_EX_APPWINDOW | WS_EX_NOACTIVATE,
            class_name.as_ptr(),
            title_wide.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            -32000,
            -32000,
            1,
            1,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        )
    };
    if window.is_null() {
        return 3;
    }
    // SAFETY: window is a window we just created.
    unsafe {
        ShowWindow(window, SW_SHOWNOACTIVATE);
    }

    // SAFETY: MSG is zeroed before GetMessageW writes it.
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: message is a valid out-buffer for the duration of the call.
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result == 0 {
            return 0;
        }
        if result < 0 {
            return 4;
        }
        // SAFETY: message was written by GetMessageW.
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

fn module_title() -> String {
    let mut buffer = [0u16; 32768];
    // SAFETY: buffer is writable and the capacity includes space for a NUL terminator.
    let length = unsafe {
        GetModuleFileNameW(
            std::ptr::null_mut(),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
    };
    if length == 0 || length as usize >= buffer.len() {
        return "game".into();
    }
    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    let file_name = Path::new(&path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("game");
    let lower = file_name.to_ascii_lowercase();
    if let Some(stem) = lower.strip_suffix(".exe") {
        file_name[..stem.len()].to_string()
    } else {
        file_name.to_string()
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match message {
        WM_MOUSEACTIVATE => MA_NOACTIVATE,
        WM_CLOSE => {
            // SAFETY: window is the hwnd the system passed into this procedure.
            unsafe {
                DestroyWindow(window);
            }
            0
        }
        WM_DESTROY => {
            // SAFETY: PostQuitMessage only posts to this thread's queue.
            unsafe {
                PostQuitMessage(0);
            }
            0
        }
        WM_QUERYENDSESSION => 1,
        WM_ENDSESSION => {
            if w_param != 0 {
                // SAFETY: a logoff should end the dummy process.
                unsafe {
                    PostQuitMessage(0);
                }
            }
            0
        }
        _ => unsafe { DefWindowProcW(window, message, w_param, l_param) },
    }
}
