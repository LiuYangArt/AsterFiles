use std::{
    io,
    sync::atomic::{AtomicIsize, Ordering},
};

use windows_sys::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::{
        Dwm::{DWMWA_CLOAK, DwmFlush, DwmSetWindowAttribute},
        Gdi::{
            ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromPoint,
        },
    },
    UI::WindowsAndMessaging::{
        GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GetForegroundWindow, GetWindow,
        IsWindow, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
        SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, WS_CAPTION, WS_EX_APPWINDOW,
        WS_EX_TOOLWINDOW, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
    },
};

use crate::quick_menu_popup::{PhysicalPoint, PhysicalRect};

static PENDING_WINDOW_OWNER: AtomicIsize = AtomicIsize::new(0);

pub fn prepare_window(owner: isize) {
    PENDING_WINDOW_OWNER.store(owner, Ordering::Release);
}

pub fn cancel_prepared_window() {
    PENDING_WINDOW_OWNER.store(0, Ordering::Release);
}

pub fn take_pending_window_owner() -> Option<isize> {
    let owner = PENDING_WINDOW_OWNER.swap(0, Ordering::AcqRel);
    (owner != 0).then_some(owner)
}

pub fn attach_owner(popup: isize, owner: isize) -> io::Result<()> {
    if popup == 0 || owner == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "popup owner is unavailable",
        ));
    }
    let popup = popup as HWND;
    if unsafe { IsWindow(popup) } == 0 || unsafe { IsWindow(owner as HWND) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "popup owner was destroyed",
        ));
    }
    unsafe {
        if GetWindow(popup, GW_OWNER) != owner as HWND {
            SetWindowLongPtrW(popup, GWLP_HWNDPARENT, owner);
        }
        let window_style =
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(popup, GWL_STYLE);
        let frame_bits = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
        let desired_window_style = (window_style & !(frame_bits as isize)) | WS_POPUP as isize;
        let extended_style =
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(popup, GWL_EXSTYLE);
        let desired_extended_style =
            (extended_style & !(WS_EX_APPWINDOW as isize)) | WS_EX_TOOLWINDOW as isize;
        let frame_changed =
            window_style != desired_window_style || extended_style != desired_extended_style;
        if window_style != desired_window_style {
            SetWindowLongPtrW(popup, GWL_STYLE, desired_window_style);
        }
        if extended_style != desired_extended_style {
            SetWindowLongPtrW(popup, GWL_EXSTYLE, desired_extended_style);
        }
        if frame_changed
            && SetWindowPos(
                popup,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            ) == 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

pub fn set_cloaked(hwnd: isize, cloaked: bool) -> io::Result<()> {
    if hwnd == 0 || unsafe { IsWindow(hwnd as HWND) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "popup window was destroyed",
        ));
    }
    let value = i32::from(cloaked);
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd as HWND,
            DWMWA_CLOAK as u32,
            (&value as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        )
    };
    if result < 0 {
        return Err(io::Error::other(format!(
            "DwmSetWindowAttribute(DWMWA_CLOAK) failed: 0x{:08x}",
            result as u32
        )));
    }
    Ok(())
}

pub fn flush_compositor() -> io::Result<()> {
    let result = unsafe { DwmFlush() };
    if result < 0 {
        return Err(io::Error::other(format!(
            "DwmFlush failed: 0x{:08x}",
            result as u32
        )));
    }
    Ok(())
}

pub fn client_point_to_screen(owner: isize, point: PhysicalPoint) -> io::Result<PhysicalPoint> {
    if owner == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "popup owner is unavailable",
        ));
    }
    let mut point = POINT {
        x: point.x,
        y: point.y,
    };
    if unsafe { ClientToScreen(owner as HWND, &mut point) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PhysicalPoint::new(point.x, point.y))
}

pub fn work_area_for_point(point: PhysicalPoint) -> io::Result<PhysicalRect> {
    let monitor = unsafe {
        MonitorFromPoint(
            POINT {
                x: point.x,
                y: point.y,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    };
    if monitor.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut information = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        rcMonitor: RECT::default(),
        rcWork: RECT::default(),
        dwFlags: 0,
    };
    if unsafe { GetMonitorInfoW(monitor, &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let work = information.rcWork;
    Ok(PhysicalRect::new(
        work.left,
        work.top,
        work.right.saturating_sub(work.left),
        work.bottom.saturating_sub(work.top),
    ))
}

pub fn foreground_belongs_to(owner: isize, popups: &[isize]) -> bool {
    let foreground = unsafe { GetForegroundWindow() };
    if foreground.is_null() {
        return false;
    }
    if foreground == owner as HWND
        || popups
            .iter()
            .any(|popup| *popup != 0 && foreground == *popup as HWND)
    {
        return true;
    }
    let mut current = foreground;
    while !current.is_null() {
        current = unsafe { GetWindow(current, GW_OWNER) };
        if current == owner as HWND
            || popups
                .iter()
                .any(|popup| *popup != 0 && current == *popup as HWND)
        {
            return true;
        }
    }
    false
}

pub fn focus_window(hwnd: isize) {
    if hwnd != 0 && unsafe { IsWindow(hwnd as HWND) } != 0 {
        unsafe { SetForegroundWindow(hwnd as HWND) };
    }
}
