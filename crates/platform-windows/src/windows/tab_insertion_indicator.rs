use std::{cell::Cell, io, ptr};

use windows_sys::Win32::{
    Foundation::HWND,
    Graphics::Gdi::{
        BeginPaint, DC_BRUSH, EndPaint, FillRect, GetStockObject, InvalidateRect, PAINTSTRUCT,
        SetDCBrushColor, UpdateWindow,
    },
    UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_TOP, IsWindow, LWA_ALPHA,
        RegisterClassW, SW_HIDE, SWP_NOACTIVATE, SWP_SHOWWINDOW, SetLayeredWindowAttributes,
        SetWindowPos, ShowWindow, WM_NCHITTEST, WM_PAINT, WNDCLASSW, WS_DISABLED, WS_EX_LAYERED,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
    },
};

const CLASS_NAME: &[u16] = &[
    65, 115, 116, 101, 114, 70, 105, 108, 101, 115, 46, 84, 97, 98, 73, 110, 115, 101, 114, 116,
    105, 111, 110, 73, 110, 100, 105, 99, 97, 116, 111, 114, 0,
];
pub const INDICATOR_WIDTH: f32 = 5.0;

thread_local! {
    static INDICATOR: Cell<HWND> = const { Cell::new(ptr::null_mut()) };
    static ACCENT: Cell<u32> = const { Cell::new(0) };
}

fn colorref(argb: u32) -> u32 {
    (argb & 0xff) << 16 | (argb & 0xff00) | (argb >> 16 & 0xff)
}

pub fn set_accent(argb: u32) {
    let color = colorref(argb);
    if ACCENT.replace(color) == color {
        return;
    }
    INDICATOR.with(|indicator| {
        let hwnd = indicator.get();
        if !hwnd.is_null() {
            // SAFETY: This thread owns the indicator; invalidation retains no Rust pointer.
            unsafe {
                InvalidateRect(hwnd, ptr::null(), 0);
                UpdateWindow(hwnd);
            }
        }
    });
}

pub fn show(x: i32, y: i32, width: i32, height: i32, accent: u32) -> io::Result<()> {
    set_accent(accent);
    let hwnd = INDICATOR.with(|indicator| {
        let existing = indicator.get();
        // SAFETY: IsWindow accepts a possibly stale borrowed HWND and retains no Rust memory.
        if !existing.is_null() && unsafe { IsWindow(existing) } != 0 {
            return existing;
        }
        indicator.set(ptr::null_mut());
        let class = WNDCLASSW {
            lpfnWndProc: Some(indicator_window_proc),
            lpszClassName: CLASS_NAME.as_ptr(),
            ..Default::default()
        };
        // SAFETY: The class uses a static NUL-terminated name and callback, with no owned brush.
        unsafe {
            let _ = RegisterClassW(&class);
        }
        // SAFETY: The class name is static; optional pointers are null; this thread owns the new HWND.
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
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if !created.is_null() {
            // SAFETY: created is this thread's layered HWND; the scalar alpha borrows no memory.
            let _ = unsafe { SetLayeredWindowAttributes(created, 0, 255, LWA_ALPHA) };
            indicator.set(created);
        }
        created
    });
    if hwnd.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: hwnd is this thread's live indicator; dimensions and flags contain no borrowed memory.
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

// SAFETY: Windows invokes this static procedure on its window's owning thread with native parameters.
unsafe extern "system" fn indicator_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: usize,
    lparam: isize,
) -> isize {
    if message == WM_NCHITTEST {
        return -1;
    }
    if message == WM_PAINT {
        let mut paint = PAINTSTRUCT::default();
        // SAFETY: The live HWND owns this paint cycle; DC_BRUSH is a borrowed stock object,
        // and BeginPaint is balanced after painting the invalidated rectangle.
        unsafe {
            let dc = BeginPaint(hwnd, &mut paint);
            SetDCBrushColor(dc, ACCENT.with(Cell::get));
            FillRect(dc, &paint.rcPaint, GetStockObject(DC_BRUSH));
            EndPaint(hwnd, &paint);
        }
        return 0;
    }
    // SAFETY: Untouched native parameters are forwarded on the window's owning thread.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

pub fn hide() {
    INDICATOR.with(|indicator| {
        let hwnd = indicator.get();
        if !hwnd.is_null() {
            // SAFETY: This thread owns the indicator and passes only a scalar show command.
            unsafe { ShowWindow(hwnd, SW_HIDE) };
        }
    });
}

pub fn destroy() {
    INDICATOR.with(|indicator| {
        let hwnd = indicator.replace(ptr::null_mut());
        if !hwnd.is_null() {
            // SAFETY: The sole thread-local owner was cleared before destroying its window once.
            unsafe { DestroyWindow(hwnd) };
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn issue_173_native_colorref_matches_slint_rgb() {
        assert_eq!(super::colorref(0xff12_3456), 0x0056_3412);
        assert_eq!(super::colorref(0xff4f_6bed), 0x00ed_6b4f);
        assert_eq!(super::colorref(0xff7f_a0ff), 0x00ff_a07f);
    }
}
