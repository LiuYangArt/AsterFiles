pub mod address_path;
pub mod clipboard;
pub mod context_menu;
#[allow(dead_code)]
pub mod directory_watch;
pub mod drag_drop;
pub mod everything;
pub mod file_operation;
pub mod shortcut;
pub mod window_trace;

use std::io;

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
