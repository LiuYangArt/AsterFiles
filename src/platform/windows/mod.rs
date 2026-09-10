pub mod address_path;
pub mod clipboard;
pub mod context_menu;
pub mod copy_file;
#[allow(dead_code)]
pub mod directory_watch;
pub mod drag_drop;
pub mod drives;
pub mod everything;
pub mod everything_file_picker;
pub mod file_operation;
pub mod libraries;
pub mod network;
pub mod quick_access;
pub mod quick_menu_window;
pub mod shell_integration;
pub mod shortcut;
pub mod single_instance;

pub mod tab_insertion_indicator;
pub mod window_trace;

use std::io;

pub fn cursor_screen_position() -> io::Result<(i32, i32)> {
    let mut cursor = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((cursor.x, cursor.y))
}
pub fn move_window(hwnd: isize, x: i32, y: i32) -> io::Result<()> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
    };

    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "main window handle is not available",
        ));
    }
    if unsafe {
        SetWindowPos(
            hwnd as windows_sys::Win32::Foundation::HWND,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub fn restore_and_focus_window(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

    if hwnd != 0 {
        unsafe {
            ShowWindow(hwnd as windows_sys::Win32::Foundation::HWND, SW_RESTORE);
            SetForegroundWindow(hwnd as windows_sys::Win32::Foundation::HWND);
        }
    }
}
pub fn show_error_dialog(owner: isize, title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
    };

    let title = title.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let message = message.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    unsafe {
        MessageBoxW(
            owner as windows_sys::Win32::Foundation::HWND,
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TASKMODAL | MB_SETFOREGROUND,
        );
    }
}

pub fn begin_window_drag(hwnd: isize) -> io::Result<()> {
    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "main window handle is not available",
        ));
    }
    unsafe {
        let mut cursor = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
        windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor);
        let screen_position =
            ((cursor.y as u32 & 0xffff) << 16 | (cursor.x as u32 & 0xffff)) as isize;
        windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture();
        windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW(
            hwnd as windows_sys::Win32::Foundation::HWND,
            windows_sys::Win32::UI::WindowsAndMessaging::WM_NCLBUTTONDOWN,
            windows_sys::Win32::UI::WindowsAndMessaging::HTCAPTION as usize,
            screen_position,
        );
    }
    Ok(())
}

pub fn has_pointer_capture(hwnd: isize) -> bool {
    hwnd != 0
        && unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetCapture() }
            == hwnd as windows_sys::Win32::Foundation::HWND
}

pub fn release_pointer_capture() {
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture() };
}
