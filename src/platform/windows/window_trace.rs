use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, GetCapture, VK_LBUTTON},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetWindowThreadProcessId, WM_ACTIVATE, WM_CANCELMODE, WM_CAPTURECHANGED,
            WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_NCDESTROY, WM_NCLBUTTONDOWN,
        },
    },
};

const SUBCLASS_ID: usize = 0x4153_5445_5244_4941;
static TRACE_FILE: OnceLock<Mutex<File>> = OnceLock::new();
static TRACE_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn requested_path() -> Option<PathBuf> {
    std::env::var_os("ASTERFILES_WINDOW_TRACE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn install(hwnd: isize, path: &Path) -> io::Result<()> {
    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "main window handle is not available",
        ));
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    TRACE_FILE.set(Mutex::new(file)).map_err(|_| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "window trace already installed",
        )
    })?;
    let _ = TRACE_PATH.set(path.to_path_buf());

    if unsafe { SetWindowSubclass(hwnd as HWND, Some(trace_window_proc), SUBCLASS_ID, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    write_record(hwnd as HWND, "trace-installed", 0, 0);
    Ok(())
}

pub fn is_active() -> bool {
    TRACE_FILE.get().is_some()
}

pub fn active_path() -> Option<&'static Path> {
    TRACE_PATH.get().map(PathBuf::as_path)
}

pub fn default_path() -> PathBuf {
    PathBuf::from("artifacts/logs/window-interaction-diagnostic.jsonl")
}
pub fn log_request(hwnd: isize, kind: &str) {
    if TRACE_FILE.get().is_some() {
        write_record(hwnd as HWND, kind, 0, 0);
    }
}

unsafe extern "system" fn trace_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    if let Some(name) = message_name(message) {
        write_record(hwnd, name, wparam, lparam);
    }
    if message == WM_NCDESTROY {
        unsafe {
            RemoveWindowSubclass(hwnd, Some(trace_window_proc), SUBCLASS_ID);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

fn message_name(message: u32) -> Option<&'static str> {
    match message {
        WM_LBUTTONDOWN => Some("WM_LBUTTONDOWN"),
        WM_NCLBUTTONDOWN => Some("WM_NCLBUTTONDOWN"),
        WM_ENTERSIZEMOVE => Some("WM_ENTERSIZEMOVE"),
        WM_EXITSIZEMOVE => Some("WM_EXITSIZEMOVE"),
        WM_CAPTURECHANGED => Some("WM_CAPTURECHANGED"),
        WM_CANCELMODE => Some("WM_CANCELMODE"),
        WM_ACTIVATE => Some("WM_ACTIVATE"),
        WM_NCDESTROY => Some("WM_NCDESTROY"),
        _ => None,
    }
}

fn write_record(hwnd: HWND, event: &str, wparam: WPARAM, lparam: LPARAM) {
    let Some(file) = TRACE_FILE.get() else {
        return;
    };
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    let capture = unsafe { GetCapture() } as usize;
    let left_down = unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } < 0;
    if let Ok(mut file) = file.try_lock() {
        let _ = writeln!(
            file,
            "{{\"time_unix_ms\":{},\"event\":\"{}\",\"hwnd\":{},\"thread_id\":{},\"process_id\":{},\"wparam\":{},\"lparam\":{},\"capture_hwnd\":{},\"left_down\":{}}}",
            elapsed.as_millis(),
            event,
            hwnd as usize,
            thread_id,
            process_id,
            wparam,
            lparam,
            capture,
            left_down,
        );
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_only_the_window_lifecycle_messages_needed_for_diagnosis() {
        assert_eq!(message_name(WM_ENTERSIZEMOVE), Some("WM_ENTERSIZEMOVE"));
        assert_eq!(message_name(WM_EXITSIZEMOVE), Some("WM_EXITSIZEMOVE"));
        assert_eq!(message_name(0x0200), None);
    }
}
