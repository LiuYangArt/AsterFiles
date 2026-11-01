use std::{cell::Cell, io, ptr};

use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{CreateSolidBrush, HBRUSH},
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOP, IsWindow, LWA_ALPHA,
        RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes,
        SetWindowPos, ShowWindow, WM_NCHITTEST, WNDCLASSW, WS_DISABLED, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
    },
};

const LIGHT_CLASS_NAME: &[u16] = &[
    65, 115, 116, 101, 114, 70, 105, 108, 101, 115, 46, 84, 97, 98, 73, 110, 115, 101, 114, 116,
    105, 111, 110, 73, 110, 100, 105, 99, 97, 116, 111, 114, 46, 76, 105, 103, 104, 116, 0,
];
const DARK_CLASS_NAME: &[u16] = &[
    65, 115, 116, 101, 114, 70, 105, 108, 101, 115, 46, 84, 97, 98, 73, 110, 115, 101, 114, 116,
    105, 111, 110, 73, 110, 100, 105, 99, 97, 116, 111, 114, 46, 68, 97, 114, 107, 0,
];
pub const INDICATOR_WIDTH: f32 = 5.0;
pub const LIGHT_ACCENT_ARGB: u32 = 0xff4f_6bed;
pub const DARK_ACCENT_ARGB: u32 = 0xff7f_a0ff;
const LIGHT_ACCENT_COLORREF: u32 = 0x00ed_6b4f;
const DARK_ACCENT_COLORREF: u32 = 0x00ff_a07f;

thread_local! {
    static INDICATOR: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
    static INDICATOR_DARK: Cell<bool> = const { Cell::new(false) };
    static LIGHT_INDICATOR_BRUSH: Cell<HBRUSH> = const { Cell::new(ptr::null_mut()) };
    static DARK_INDICATOR_BRUSH: Cell<HBRUSH> = const { Cell::new(ptr::null_mut()) };
}

pub fn show(x: i32, y: i32, width: i32, height: i32, dark_theme: bool) -> io::Result<()> {
    let hwnd = INDICATOR.with(|indicator| {
        let existing = indicator.get();
        if !existing.is_null()
            && unsafe { IsWindow(existing) } != 0
            && INDICATOR_DARK.with(Cell::get) == dark_theme
        {
            return existing;
        }
        if !existing.is_null() && unsafe { IsWindow(existing) } != 0 {
            unsafe { DestroyWindow(existing) };
        }
        indicator.set(ptr::null_mut());
        let class_name = if dark_theme {
            DARK_CLASS_NAME
        } else {
            LIGHT_CLASS_NAME
        };
        let class = WNDCLASSW {
            lpfnWndProc: Some(indicator_window_proc),
            hbrBackground: indicator_brush(dark_theme),
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }
        let created = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class_name.as_ptr(),
                ptr::null(),
                WS_POPUP | WS_DISABLED,
                x,
                y,
                width.max(1),
                height.max(1),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if !created.is_null() {
            let _ = unsafe { SetLayeredWindowAttributes(created, 0, 255, LWA_ALPHA) };
            indicator.set(created);
            INDICATOR_DARK.set(dark_theme);
        }
        created
    });
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    if unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOP,
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

fn indicator_brush(dark_theme: bool) -> HBRUSH {
    let (brush, color) = if dark_theme {
        (&DARK_INDICATOR_BRUSH, DARK_ACCENT_COLORREF)
    } else {
        (&LIGHT_INDICATOR_BRUSH, LIGHT_ACCENT_COLORREF)
    };
    brush.with(|brush| {
        let existing = brush.get();
        if !existing.is_null() {
            existing
        } else {
            let created = unsafe { CreateSolidBrush(color) };
            brush.set(created);
            created
        }
    })
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
