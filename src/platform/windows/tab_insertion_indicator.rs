use std::{cell::Cell, io, ptr};

use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::CreateSolidBrush,
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOPMOST, LWA_ALPHA, RegisterClassW,
        SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes, SetWindowPos,
        ShowWindow, WM_NCHITTEST, WNDCLASSW, WS_DISABLED, WS_EX_LAYERED, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
    },
};

const CLASS_NAME: &[u16] = &[
    65, 115, 116, 101, 114, 70, 105, 108, 101, 115, 46, 84, 97, 98, 73, 110, 115, 101, 114, 116,
    105, 111, 110, 73, 110, 100, 105, 99, 97, 116, 111, 114, 0,
];
const ACCENT_COLORREF: u32 = 0x00ed_6b4f;

thread_local! {
    static INDICATOR: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
}

pub fn show(owner: isize, x: i32, y: i32, width: i32, height: i32) -> io::Result<()> {
    let hwnd = INDICATOR.with(|indicator| {
        let existing = indicator.get();
        if !existing.is_null() {
            return existing;
        }
        let class = WNDCLASSW {
            lpfnWndProc: Some(indicator_window_proc),
            hbrBackground: unsafe { CreateSolidBrush(ACCENT_COLORREF) },
            lpszClassName: CLASS_NAME.as_ptr(),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }
        let created = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                CLASS_NAME.as_ptr(),
                ptr::null(),
                WS_POPUP | WS_DISABLED,
                x,
                y,
                width.max(1),
                height.max(1),
                owner as HWND,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if !created.is_null() {
            let _ = unsafe { SetLayeredWindowAttributes(created, 0, 255, LWA_ALPHA) };
            indicator.set(created);
        }
        created
    });
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            x,
            y,
            width.max(1),
            height.max(1),
            SWP_NOACTIVATE | SWP_SHOWWINDOW,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

unsafe extern "system" fn indicator_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if message == WM_NCHITTEST {
        return -1;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

pub fn hide() {
    INDICATOR.with(|indicator| {
        let hwnd = indicator.get();
        if !hwnd.is_null() {
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    });
}

pub fn destroy() {
    INDICATOR.with(|indicator| {
        let hwnd = indicator.replace(ptr::null_mut());
        if !hwnd.is_null() {
            unsafe { DestroyWindow(hwnd) };
        }
    });
}
