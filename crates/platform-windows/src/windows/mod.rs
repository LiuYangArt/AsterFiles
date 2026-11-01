pub mod address_path;
pub mod atomic_file;
pub mod clipboard;
pub mod context_menu;
pub(crate) mod copy_execution;
pub(crate) mod copy_file;
#[allow(dead_code)]
pub mod directory_watch;
pub mod drag_drop;
pub mod drives;
pub mod everything;
pub mod everything_file_picker;
pub mod file_operation;
pub mod launch_probe;
pub mod libraries;
pub mod network;
pub(crate) mod network_copy;
pub(crate) mod network_snapshot;
pub mod path_relation;
pub(crate) mod process_job;
pub mod quick_access;
pub mod quick_menu_window;
pub mod shell_icons;
pub mod shell_integration;
pub mod shell_select_trap;
pub mod shortcut;
pub mod single_instance;

pub mod tab_insertion_indicator;
pub mod window_dpi;
pub mod window_drag_recovery;
pub mod window_trace;

pub mod webdav_credentials;

use std::io;

pub fn mark_internal_cleanup_directory(path: &std::path::Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
        SetFileAttributesW,
    };

    let wide = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // SAFETY: wide is a live, NUL-terminated UTF-16 path; the query retains no pointer to it.
    let attributes = unsafe { GetFileAttributesW(wide.as_ptr()) };
    if attributes == INVALID_FILE_ATTRIBUTES {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: The same NUL-terminated path remains live, and attributes contains the successful query result plus documented flags.
    if unsafe {
        SetFileAttributesW(
            wide.as_ptr(),
            attributes | FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
pub fn cursor_screen_position() -> io::Result<(i32, i32)> {
    let mut cursor = windows_sys::Win32::Foundation::POINT { x: 0, y: 0 };
    // SAFETY: cursor is initialized writable POINT storage; GetCursorPos does not retain its address.
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
    // SAFETY: The caller supplies its window HWND; all position data are passed by value and no Rust borrow escapes.
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
        GetForegroundWindow, IsIconic, SW_RESTORE, SetForegroundWindow, ShowWindow,
    };

    if hwnd == 0 {
        crate::operation_audit::record("external-open-activation", "target_hwnd=0 unavailable");
        return;
    }
    let target = hwnd as windows_sys::Win32::Foundation::HWND;
    // SAFETY: target is passed as an opaque window handle; IsIconic retains no Rust pointer or ownership.
    let minimized = unsafe { IsIconic(target) } != 0;
    // Restoring an already maximized window would discard the user's window state.
    if minimized {
        // SAFETY: The caller supplied this window handle; ShowWindow uses only by-value arguments and retains no Rust memory.
        unsafe { ShowWindow(target, SW_RESTORE) };
    }
    // SAFETY: The HWND is an opaque borrowed window identifier, not an address into Rust storage.
    let accepted = unsafe { SetForegroundWindow(target) } != 0;
    // SAFETY: This no-argument query returns a borrowed HWND and does not transfer ownership or borrow Rust memory.
    let foreground = unsafe { GetForegroundWindow() };
    // SetForegroundWindow does not provide a reliable GetLastError failure reason.
    crate::operation_audit::record(
        "external-open-activation",
        format!(
            "target_hwnd={hwnd} minimized={minimized} accepted={accepted} foreground_hwnd={} active={}",
            foreground as isize,
            foreground == target,
        ),
    );
}
pub fn show_error_dialog(owner: isize, title: &str, message: &str) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MB_TASKMODAL, MessageBoxW,
    };

    let title = title.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let message = message.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    // SAFETY: Title and message are NUL-terminated buffers that outlive the synchronous modal call, including its message loop.
    unsafe {
        MessageBoxW(
            owner as windows_sys::Win32::Foundation::HWND,
            message.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR | MB_TASKMODAL | MB_SETFOREGROUND,
        );
    }
}

pub fn has_pointer_capture(hwnd: isize) -> bool {
    hwnd != 0
        // SAFETY: This no-argument, thread-local capture query retains no Rust memory and returns only a borrowed HWND.
        && unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::GetCapture() }
            == hwnd as windows_sys::Win32::Foundation::HWND
}

pub fn shift_key_down() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_SHIFT};

    // SAFETY: VK_SHIFT is a valid virtual-key code; the query borrows no Rust memory.
    unsafe { GetAsyncKeyState(VK_SHIFT as i32) < 0 }
}

pub fn release_pointer_capture() {
    // SAFETY: This thread-local capture operation takes no pointers and does not retain Rust resources.
    unsafe { windows_sys::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture() };
}
