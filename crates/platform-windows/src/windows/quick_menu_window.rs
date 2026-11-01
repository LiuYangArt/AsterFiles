use std::{
    ffi::c_void,
    io,
    sync::atomic::{AtomicIsize, Ordering},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
    Graphics::{
        Dwm::{
            DWMSBT_NONE, DWMSBT_TRANSIENTWINDOW, DWMWA_CLOAK, DWMWA_SYSTEMBACKDROP_TYPE,
            DWMWA_USE_IMMERSIVE_DARK_MODE, DwmFlush, DwmSetWindowAttribute,
        },
        Gdi::{
            ClientToScreen, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO,
            MonitorFromPoint,
        },
    },
    System::LibraryLoader::{GetModuleHandleW, GetProcAddress},
    UI::WindowsAndMessaging::{
        GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GWLP_HWNDPARENT, GetForegroundWindow, GetWindow,
        IsWindow, MA_NOACTIVATE, STYLESTRUCT, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE,
        SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, WINDOWPOS, WM_MOUSEACTIVATE, WM_NCDESTROY, WM_STYLECHANGING,
        WM_WINDOWPOSCHANGING, WS_CAPTION, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_POPUP, WS_SYSMENU,
    },
};

use crate::domain::geometry::{PhysicalPoint, PhysicalRect};

static PENDING_WINDOW_OWNER: AtomicIsize = AtomicIsize::new(0);
const POPUP_SUBCLASS_ID: usize = 0x4153_504f;

const WCA_ACCENT_POLICY: u32 = 19;
const ACCENT_ENABLE_ACRYLICBLURBEHIND: i32 = 4;
const ACCENT_DISABLED: i32 = 0;

#[repr(C)]
struct AccentPolicy {
    state: i32,
    flags: u32,
    gradient_color: u32,
    animation_id: u32,
}

#[repr(C)]
struct WindowCompositionAttributeData {
    attribute: u32,
    data: *mut c_void,
    size: usize,
}

type SetWindowCompositionAttribute =
    unsafe extern "system" fn(HWND, *mut WindowCompositionAttributeData) -> i32;

fn backdrop_type(enabled: bool) -> i32 {
    if enabled {
        DWMSBT_TRANSIENTWINDOW
    } else {
        DWMSBT_NONE
    }
}

fn acrylic_gradient_color(dark_theme: bool, opacity: u8) -> u32 {
    let alpha = ((u32::from(opacity.min(100)) * 255 + 50) / 100).max(1);
    let rgb = if dark_theme { 0x0020_2020 } else { 0x00d3_d3d3 };
    (alpha << 24) | rgb
}

fn set_acrylic_composition(
    hwnd: HWND,
    enabled: bool,
    dark_theme: bool,
    opacity: u8,
) -> io::Result<()> {
    const USER32: &[u16] = &[117, 115, 101, 114, 51, 50, 46, 100, 108, 108, 0];
    // SAFETY: USER32 is terminated UTF-16 and names an already-loaded system module that is borrowed,
    // not unloaded.
    let module = unsafe { GetModuleHandleW(USER32.as_ptr()) };
    if module.is_null() {
        return Err(io::Error::last_os_error());
    }
    const FUNCTION_NAME: &[u8] = b"SetWindowCompositionAttribute\0";

    // SAFETY: module was checked non-null and FUNCTION_NAME is the terminated ASCII export name.
    let Some(function) = (unsafe { GetProcAddress(module, FUNCTION_NAME.as_ptr()) }) else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SetWindowCompositionAttribute is unavailable",
        ));
    };
    let set_window_composition_attribute: SetWindowCompositionAttribute =
        // SAFETY: The named user32 export uses the system ABI and the HWND/composition-data signature
        // declared above.
        unsafe { std::mem::transmute(function) };
    let mut policy = AccentPolicy {
        state: if enabled {
            ACCENT_ENABLE_ACRYLICBLURBEHIND
        } else {
            ACCENT_DISABLED
        },
        flags: 0,
        gradient_color: acrylic_gradient_color(dark_theme, opacity),
        animation_id: 0,
    };
    let mut data = WindowCompositionAttributeData {
        attribute: WCA_ACCENT_POLICY,
        data: (&mut policy as *mut AccentPolicy).cast(),
        size: std::mem::size_of::<AccentPolicy>(),
    };
    // SAFETY: data/policy have repr(C) layout and remain writable for the call; size matches
    // AccentPolicy exactly.
    if unsafe { set_window_composition_attribute(hwnd, &mut data) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

unsafe extern "system" fn popup_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    id: usize,
    _: usize,
) -> LRESULT {
    use windows_sys::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    match message {
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        WM_STYLECHANGING if lparam != 0 => {
            // Winit rewrites styles on every visibility transition; keep popup invariants.
            // SAFETY: For WM_STYLECHANGING Windows supplies a writable STYLESTRUCT in nonzero lparam
            // for this callback.
            let style = unsafe { &mut *(lparam as *mut STYLESTRUCT) };
            style.styleNew = popup_style(wparam as i32, style.styleNew);
        }
        WM_WINDOWPOSCHANGING if lparam != 0 => {
            // Winit can use SW_SHOW again when a pooled popup changes visibility or style.
            // SAFETY: For WM_WINDOWPOSCHANGING Windows supplies a writable WINDOWPOS valid until this
            // callback returns.
            unsafe {
                (*(lparam as *mut WINDOWPOS)).flags |= SWP_NOACTIVATE;
            }
        }
        // SAFETY: Windows is destroying this subclassed HWND; id and callback match the installed
        // subclass registration.
        WM_NCDESTROY => unsafe {
            RemoveWindowSubclass(hwnd, Some(popup_window_proc), id);
        },
        _ => {}
    }
    // SAFETY: The unchanged callback payload is forwarded synchronously within the current window's
    // subclass chain.
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn popup_style(index: i32, style: u32) -> u32 {
    match index {
        GWL_STYLE => {
            (style & !(WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX)) | WS_POPUP
        }
        GWL_EXSTYLE => (style & !WS_EX_APPWINDOW) | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        _ => style,
    }
}

pub fn show_without_activation(hwnd: isize) {
    // Make the cloaked HWND visible before Slint calls SW_SHOW, which would activate a hidden HWND.
    // SAFETY: hwnd is borrowed from the popup backend on its owner thread; both calls use only scalar
    // flags and handles.
    unsafe {
        ShowWindow(hwnd as HWND, SW_SHOWNOACTIVATE);
        SetWindowPos(
            hwnd as HWND,
            std::ptr::null_mut(),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    super::window_trace::log_diagnostic(
        "quick_menu_native_show",
        &format!(
            "popup={} foreground={}",
            hwnd,
            // SAFETY: GetForegroundWindow takes no pointers and its borrowed handle is used only as a
            // diagnostic integer.
            unsafe { GetForegroundWindow() } as isize,
        ),
    );
}

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
    // SAFETY: Both IsWindow calls only query scalar HWND values and do not dereference user-provided
    // pointers.
    if unsafe { IsWindow(popup) } == 0 || unsafe { IsWindow(owner as HWND) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "popup owner was destroyed",
        ));
    }
    // SAFETY: The checked popup is owned by this UI thread; the static subclass callback and owner
    // HWND outlive attachment.
    unsafe {
        if windows_sys::Win32::UI::Shell::SetWindowSubclass(
            popup,
            Some(popup_window_proc),
            POPUP_SUBCLASS_ID,
            0,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        if GetWindow(popup, GW_OWNER) != owner as HWND {
            SetWindowLongPtrW(popup, GWLP_HWNDPARENT, owner);
        }
        let window_style =
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(popup, GWL_STYLE);
        let desired_window_style = popup_style(GWL_STYLE, window_style as u32) as isize;
        let extended_style =
            windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(popup, GWL_EXSTYLE);
        let desired_extended_style = popup_style(GWL_EXSTYLE, extended_style as u32) as isize;
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

pub fn set_backdrop(hwnd: isize, enabled: bool, dark_theme: bool, opacity: u8) -> io::Result<()> {
    // SAFETY: IsWindow only queries the scalar HWND and rejects a destroyed window before the
    // attribute calls.
    if hwnd == 0 || unsafe { IsWindow(hwnd as HWND) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "popup window was destroyed",
        ));
    }
    set_acrylic_composition(hwnd as HWND, enabled, dark_theme, opacity)?;
    let backdrop = backdrop_type(enabled);
    let dark = i32::from(dark_theme);
    for (attribute, value, name) in [
        (
            DWMWA_SYSTEMBACKDROP_TYPE,
            backdrop,
            "DWMWA_SYSTEMBACKDROP_TYPE",
        ),
        (
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            dark,
            "DWMWA_USE_IMMERSIVE_DARK_MODE",
        ),
    ] {
        // SAFETY: value is a live i32 and the supplied byte count exactly matches the requested DWM
        // attribute representation.
        let result = unsafe {
            DwmSetWindowAttribute(
                hwnd as HWND,
                attribute as u32,
                (&value as *const i32).cast(),
                std::mem::size_of::<i32>() as u32,
            )
        };
        if result < 0 {
            return Err(io::Error::other(format!(
                "DwmSetWindowAttribute({name}) failed: 0x{:08x}",
                result as u32
            )));
        }
    }
    Ok(())
}

pub fn set_cloaked(hwnd: isize, cloaked: bool) -> io::Result<()> {
    // SAFETY: IsWindow only queries the scalar HWND and rejects a destroyed window before the cloak
    // update.
    if hwnd == 0 || unsafe { IsWindow(hwnd as HWND) } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "popup window was destroyed",
        ));
    }
    let value = i32::from(cloaked);
    // SAFETY: The live i32 cloak value has the exact attribute byte size and is borrowed only during
    // this call.
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
    // SAFETY: DwmFlush takes no pointers or owned handles; its failure is returned to the caller.
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
    // SAFETY: point is writable POINT storage and owner is a borrowed backend HWND, rejected by
    // Windows if stale.
    if unsafe { ClientToScreen(owner as HWND, &mut point) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(PhysicalPoint::new(point.x, point.y))
}

pub fn work_area_for_point(point: PhysicalPoint) -> io::Result<PhysicalRect> {
    // SAFETY: MonitorFromPoint receives the POINT by value; its returned monitor is borrowed and
    // never freed here.
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
    // SAFETY: monitor was checked non-null; information has the required cbSize and writable
    // MONITORINFO storage.
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
    // SAFETY: GetForegroundWindow takes no pointers and returns a borrowed handle used only for
    // comparison and queries.
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
        // SAFETY: current is a borrowed window handle; GetWindow only queries its owner and may
        // return null.
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
    if hwnd != 0
        // SAFETY: IsWindow queries only the scalar handle before any request to bring the window
        // forward.
        && unsafe { IsWindow(hwnd as HWND) } != 0
        // SAFETY: GetForegroundWindow takes no pointers and its borrowed handle is only compared
        // here.
        && unsafe { GetForegroundWindow() } != hwnd as HWND
        // SAFETY: Both window calls only query borrowed handles; no pointer-valued message or owned
        // resource is passed.
        && unsafe { GetWindow(GetForegroundWindow(), GW_OWNER) } == hwnd as HWND
    {
        // SAFETY: The checked backend HWND is borrowed and this scalar-handle request does not
        // transfer ownership.
        unsafe { SetForegroundWindow(hwnd as HWND) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quick_menu_backdrop_setting_maps_to_transient_or_none() {
        assert_eq!(backdrop_type(true), DWMSBT_TRANSIENTWINDOW);
        assert_eq!(backdrop_type(false), DWMSBT_NONE);
        assert_eq!(acrylic_gradient_color(true, 60), 0x9920_2020);
        assert_eq!(acrylic_gradient_color(false, 60), 0x99d3_d3d3);
        assert_eq!(acrylic_gradient_color(true, 0), 0x0120_2020);
        assert_eq!(acrylic_gradient_color(false, 100), 0xffd3_d3d3);
    }

    #[test]
    fn quick_menu_popup_preserves_styles_across_backend_rewrites() {
        let frame = WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX;
        let style = popup_style(GWL_STYLE, frame | WS_POPUP);
        assert_eq!(style & frame, 0);
        assert_ne!(style & WS_POPUP, 0);
        let extended = popup_style(GWL_EXSTYLE, WS_EX_APPWINDOW);
        assert_eq!(extended & WS_EX_APPWINDOW, 0);
        assert_eq!(
            extended & (WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW),
            WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW
        );
        assert_eq!(popup_style(GWL_EXSTYLE, extended), extended);
    }

    #[test]
    fn quick_menu_popup_mouse_activation_keeps_click_delivery() {
        // SAFETY: WM_MOUSEACTIVATE returns before any HWND or lparam access, so this branch permits
        // null test handles.
        let result = unsafe {
            popup_window_proc(
                std::ptr::null_mut(),
                WM_MOUSEACTIVATE,
                0,
                0,
                POPUP_SUBCLASS_ID,
                0,
            )
        };
        assert_eq!(result, MA_NOACTIVATE as LRESULT);
    }
}
