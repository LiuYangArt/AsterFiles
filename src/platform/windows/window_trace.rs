use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    fmt::Write as _,
    fs::OpenOptions,
    io::{self, BufWriter, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM},
    UI::{
        HiDpi::GetDpiForWindow,
        Input::KeyboardAndMouse::{GetAsyncKeyState, GetCapture, VK_LBUTTON},
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetClientRect, GetCursorPos, GetWindowRect, GetWindowThreadProcessId, IsZoomed,
            WINDOWPOS, WM_ACTIVATE, WM_CANCELMODE, WM_CAPTURECHANGED, WM_DPICHANGED,
            WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE, WM_GETDPISCALEDSIZE, WM_LBUTTONDOWN, WM_NCDESTROY,
            WM_NCLBUTTONDOWN, WM_SIZE, WM_WINDOWPOSCHANGED, WM_WINDOWPOSCHANGING,
        },
    },
};

const SUBCLASS_ID: usize = 0x4153_5445_5244_4941;
const QUEUE_CAPACITY: usize = 1024;
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);
static TRACE: OnceLock<TraceSink> = OnceLock::new();
static INSTALL_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static INSTALLED_WINDOWS: RefCell<HashSet<HWND>> = RefCell::new(HashSet::new());
}

struct TraceSink {
    path: PathBuf,
    sender: SyncSender<TraceCommand>,
    dropped: Arc<AtomicU64>,
}

enum TraceCommand {
    Record(String),
    Flush(SyncSender<io::Result<()>>),
}

impl TraceSink {
    fn start(path: &Path) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let worker_path = path.to_path_buf();
        let (ready, started) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("window-trace".into())
            .spawn(move || {
                let file = match (|| {
                    if let Some(parent) = worker_path
                        .parent()
                        .filter(|path| !path.as_os_str().is_empty())
                    {
                        std::fs::create_dir_all(parent)?;
                    }
                    OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(worker_path)
                })() {
                    Ok(file) => file,
                    Err(error) => {
                        let _ = ready.send(Err(error));
                        return;
                    }
                };
                let _ = ready.send(Ok(()));
                if let Err(error) = write_records(BufWriter::new(file), receiver, &worker_dropped) {
                    eprintln!("window trace writer failed: {error}");
                }
            })?;
        started.recv().map_err(writer_stopped)??;
        Ok(Self {
            path: path.to_path_buf(),
            sender,
            dropped,
        })
    }

    fn enqueue(&self, record: String) {
        // 拖动热路径不能等待磁盘或队列空间；缺失记录由后台单独计数落盘。
        if let Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) =
            self.sender.try_send(TraceCommand::Record(record))
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn flush(&self) -> io::Result<()> {
        let (reply, finished) = mpsc::sync_channel(1);
        self.sender
            .send(TraceCommand::Flush(reply))
            .map_err(writer_stopped)?;
        finished.recv().map_err(writer_stopped)?
    }
}

fn writer_stopped(error: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::BrokenPipe,
        format!("window trace writer stopped: {error}"),
    )
}

fn write_records(
    mut writer: impl Write,
    receiver: Receiver<TraceCommand>,
    dropped: &AtomicU64,
) -> io::Result<()> {
    let mut total_dropped = 0_u64;
    loop {
        let message = receiver.recv_timeout(FLUSH_INTERVAL);
        let lost = dropped.swap(0, Ordering::Relaxed);
        if lost != 0 {
            total_dropped += lost;
            writeln!(
                writer,
                "{{\"time_unix_ms\":{},\"event\":\"trace-queue-overflow\",\"dropped_records\":{lost},\"dropped_records_total\":{total_dropped}}}",
                time_unix_ms(),
            )?;
        }
        match message {
            Ok(TraceCommand::Record(record)) => writeln!(writer, "{record}")?,
            Ok(TraceCommand::Flush(reply)) => match writer.flush() {
                Ok(()) => {
                    let _ = reply.send(Ok(()));
                }
                Err(error) => {
                    let _ = reply.send(Err(io::Error::new(error.kind(), error.to_string())));
                    return Err(error);
                }
            },
            Err(RecvTimeoutError::Timeout) => writer.flush()?,
            Err(RecvTimeoutError::Disconnected) => return writer.flush(),
        }
    }
}

struct WindowTraceState {
    dpi: Cell<u32>,
}

pub fn requested_path() -> Option<PathBuf> {
    std::env::var_os("ASTERFILES_WINDOW_TRACE")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

pub fn install(hwnd: isize, path: &Path) -> io::Result<()> {
    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window handle is not available",
        ));
    }
    let _install = INSTALL_LOCK
        .lock()
        .map_err(|_| io::Error::other("window trace install lock poisoned"))?;
    if let Some(trace) = TRACE.get() {
        if trace.path != path {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "window trace uses a different path",
            ));
        }
    } else {
        let trace = TraceSink::start(path)?;
        let _ = TRACE.set(trace);
    }
    let hwnd = hwnd as HWND;
    if INSTALLED_WINDOWS.with(|windows| windows.borrow().contains(&hwnd)) {
        return Ok(());
    }
    let state = Box::into_raw(Box::new(WindowTraceState {
        dpi: Cell::new(unsafe { GetDpiForWindow(hwnd) }),
    }));
    if unsafe { SetWindowSubclass(hwnd, Some(trace_window_proc), SUBCLASS_ID, state as usize) } == 0
    {
        let error = io::Error::last_os_error();
        unsafe { drop(Box::from_raw(state)) };
        return Err(error);
    }
    INSTALLED_WINDOWS.with(|windows| {
        windows.borrow_mut().insert(hwnd);
    });
    write_record(hwnd, "trace-installed", "observed", 0, 0, "");
    Ok(())
}

pub fn is_active() -> bool {
    TRACE.get().is_some()
}

pub fn active_path() -> Option<&'static Path> {
    TRACE.get().map(|trace| trace.path.as_path())
}

pub fn default_path() -> PathBuf {
    let relative = PathBuf::from("artifacts/logs/window-interaction-diagnostic.jsonl");
    if cfg!(debug_assertions) {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative)
    } else {
        relative
    }
}

/// 仅在事件循环退出后等待已排队的证据落盘，不能在拖动热路径调用。
pub fn flush() -> io::Result<()> {
    TRACE.get().map_or(Ok(()), TraceSink::flush)
}

pub fn log_request(hwnd: isize, kind: &str) {
    write_record(hwnd as HWND, kind, "request", 0, 0, "");
}

pub fn log_diagnostic(kind: &str, detail: &str) {
    let Some(trace) = TRACE.get() else {
        return;
    };
    trace.enqueue(format!(
        "{{\"time_unix_ms\":{},\"event\":{},\"detail\":{}}}",
        time_unix_ms(),
        json_string(kind),
        json_string(detail),
    ));
}

unsafe extern "system" fn trace_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    state: usize,
) -> LRESULT {
    let Some(name) = message_name(message) else {
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    };
    let details = if message == WM_DPICHANGED {
        let state = unsafe { &*(state as *const WindowTraceState) };
        let old_dpi = state.dpi.replace(wparam as u32 & 0xffff);
        let suggested = unsafe { (lparam as *const RECT).as_ref() }.map(rect_values);
        format!(
            ",\"old_dpi\":{old_dpi},\"new_dpi\":{},\"new_dpi_y\":{},\"suggested_rect\":{}",
            wparam as u32 & 0xffff,
            (wparam as u32 >> 16) & 0xffff,
            json_array(suggested),
        )
    } else {
        message_details(message, wparam, lparam)
    };
    write_record(hwnd, name, "before", wparam, lparam, &details);
    if message == WM_NCDESTROY {
        unsafe {
            RemoveWindowSubclass(hwnd, Some(trace_window_proc), SUBCLASS_ID);
            drop(Box::from_raw(state as *mut WindowTraceState));
        }
        INSTALLED_WINDOWS.with(|windows| {
            windows.borrow_mut().remove(&hwnd);
        });
    }
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    // 保存 DPI 原始建议值；子类只观察，下游如何处理由前后几何数据呈现。
    if message == WM_DPICHANGED {
        write_record(hwnd, name, "after", wparam, lparam, &details);
    } else if matches!(
        message,
        WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED | WM_SIZE | WM_GETDPISCALEDSIZE
    ) {
        write_record(
            hwnd,
            name,
            "after",
            wparam,
            lparam,
            &message_details(message, wparam, lparam),
        );
    }
    result
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
        WM_GETDPISCALEDSIZE => Some("WM_GETDPISCALEDSIZE"),
        WM_DPICHANGED => Some("WM_DPICHANGED"),
        WM_WINDOWPOSCHANGING => Some("WM_WINDOWPOSCHANGING"),
        WM_WINDOWPOSCHANGED => Some("WM_WINDOWPOSCHANGED"),
        WM_SIZE => Some("WM_SIZE"),
        _ => None,
    }
}

fn message_details(message: u32, wparam: WPARAM, lparam: LPARAM) -> String {
    if matches!(message, WM_WINDOWPOSCHANGING | WM_WINDOWPOSCHANGED)
        && let Some(pos) = unsafe { (lparam as *const WINDOWPOS).as_ref() }
    {
        format!(
            ",\"window_pos\":{{\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"flags\":{},\"insert_after_hwnd\":{}}}",
            pos.x, pos.y, pos.cx, pos.cy, pos.flags, pos.hwndInsertAfter as usize,
        )
    } else if message == WM_GETDPISCALEDSIZE
        && let Some(size) = unsafe { (lparam as *const SIZE).as_ref() }
    {
        format!(
            ",\"target_dpi\":{wparam},\"candidate_outer\":[{},{}]",
            size.cx, size.cy,
        )
    } else if message == WM_SIZE {
        format!(
            ",\"size_kind\":{wparam},\"size_client\":[{},{}]",
            lparam as u32 & 0xffff,
            (lparam as u32 >> 16) & 0xffff,
        )
    } else {
        String::new()
    }
}

fn write_record(
    hwnd: HWND,
    event: &str,
    phase: &str,
    wparam: WPARAM,
    lparam: LPARAM,
    details: &str,
) {
    let Some(trace) = TRACE.get() else {
        return;
    };
    let mut process_id = 0;
    let thread_id = unsafe { GetWindowThreadProcessId(hwnd, &mut process_id) };
    let capture = unsafe { GetCapture() } as usize;
    let left_down = unsafe { GetAsyncKeyState(VK_LBUTTON as i32) } < 0;
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let maximized = unsafe { IsZoomed(hwnd) } != 0;
    let mut window_rect = RECT::default();
    let mut client_rect = RECT::default();
    let mut cursor = POINT::default();
    let window_rect =
        (unsafe { GetWindowRect(hwnd, &mut window_rect) } != 0).then(|| rect_values(&window_rect));
    let client_rect =
        (unsafe { GetClientRect(hwnd, &mut client_rect) } != 0).then(|| rect_values(&client_rect));
    let cursor = (unsafe { GetCursorPos(&mut cursor) } != 0).then_some([cursor.x, cursor.y]);
    trace.enqueue(format!(
        "{{\"time_unix_ms\":{},\"event\":{},\"phase\":{},\"hwnd\":{},\"thread_id\":{thread_id},\"process_id\":{process_id},\"wparam\":{wparam},\"lparam\":{lparam},\"capture_hwnd\":{capture},\"left_down\":{left_down},\"dpi\":{dpi},\"maximized\":{maximized},\"window_rect\":{},\"client_rect\":{},\"cursor\":{}{details}}}",
        time_unix_ms(), json_string(event), json_string(phase), hwnd as usize,
        json_array(window_rect), json_array(client_rect), json_array(cursor),
    ));
}

fn rect_values(rect: &RECT) -> [i32; 4] {
    [rect.left, rect.top, rect.right, rect.bottom]
}

fn json_array<const N: usize>(values: Option<[i32; N]>) -> String {
    values.map_or_else(|| "null".to_owned(), |values| format!("{values:?}"))
}

fn time_unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn json_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');
    for character in value.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0'..='\u{1f}' => {
                let _ = write!(result, "\\u{:04x}", character as u32);
            }
            _ => result.push(character),
        }
    }
    result.push('"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traces_dpi_geometry_and_move_lifecycle_without_mouse_move_spam() {
        for message in [
            WM_ENTERSIZEMOVE,
            WM_EXITSIZEMOVE,
            WM_GETDPISCALEDSIZE,
            WM_DPICHANGED,
            WM_WINDOWPOSCHANGING,
            WM_WINDOWPOSCHANGED,
            WM_SIZE,
        ] {
            assert!(message_name(message).is_some());
        }
        assert_eq!(message_name(0x0200), None);
    }

    #[test]
    fn diagnostic_logging_is_safe_before_trace_installation() {
        log_diagnostic("quick-menu-test", "not installed");
        assert!(flush().is_ok());
    }

    #[test]
    fn json_strings_keep_control_characters_and_unicode_valid() {
        assert_eq!(
            json_string("路径\n\0\u{1b}\t\"\\"),
            "\"路径\\n\\u0000\\u001b\\t\\\"\\\\\""
        );
        assert_eq!(
            json_array(Some([-1440, -200, 0, 2360])),
            "[-1440, -200, 0, 2360]"
        );
        assert_eq!(json_array::<4>(None), "null");
    }

    #[test]
    fn a_full_queue_is_nonblocking_and_reports_lost_records_when_drained() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let trace = TraceSink {
            path: PathBuf::new(),
            sender,
            dropped: Arc::clone(&dropped),
        };
        trace.enqueue("{\"event\":\"first\"}".into());
        trace.enqueue("{\"event\":\"second\"}".into());
        trace.enqueue("{\"event\":\"third\"}".into());
        assert_eq!(dropped.load(Ordering::Relaxed), 2);
        drop(trace);
        let mut output = Vec::new();
        write_records(&mut output, receiver, &dropped).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains(
            "\"event\":\"trace-queue-overflow\",\"dropped_records\":2,\"dropped_records_total\":2"
        ));
        assert!(output.contains("\"event\":\"first\""));
        assert!(!output.contains("\"event\":\"second\""));
    }

    #[test]
    fn window_position_payload_is_observed_without_mutating_it() {
        let position = WINDOWPOS {
            x: -1320,
            y: 140,
            cx: 1000,
            cy: 800,
            flags: 0x0004,
            ..WINDOWPOS::default()
        };
        let details = message_details(
            WM_WINDOWPOSCHANGING,
            0,
            &position as *const WINDOWPOS as LPARAM,
        );
        assert!(
            details.contains("\"x\":-1320,\"y\":140,\"width\":1000,\"height\":800,\"flags\":4")
        );
        assert_eq!(
            (
                position.x,
                position.y,
                position.cx,
                position.cy,
                position.flags
            ),
            (-1320, 140, 1000, 800, 4)
        );
    }

    #[test]
    fn flush_reports_background_write_failure() {
        struct FailingFlush;
        impl Write for FailingFlush {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Err(io::Error::other("test flush failure"))
            }
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let trace = TraceSink {
            path: PathBuf::new(),
            sender,
            dropped,
        };
        let worker = thread::spawn(move || write_records(FailingFlush, receiver, &worker_dropped));
        assert_eq!(trace.flush().unwrap_err().to_string(), "test flush failure");
        assert!(worker.join().unwrap().is_err());
    }
    #[test]
    fn flush_waits_for_queued_records_to_reach_the_writer() {
        let (sender, receiver) = mpsc::sync_channel(2);
        let dropped = Arc::new(AtomicU64::new(0));
        let worker_dropped = Arc::clone(&dropped);
        let trace = TraceSink {
            path: PathBuf::new(),
            sender,
            dropped,
        };
        let worker = thread::spawn(move || {
            let mut output = Vec::new();
            write_records(&mut output, receiver, &worker_dropped).unwrap();
            output
        });
        trace.enqueue("{\"event\":\"before-exit\"}".into());
        trace.flush().unwrap();
        drop(trace);
        assert_eq!(
            String::from_utf8(worker.join().unwrap()).unwrap(),
            "{\"event\":\"before-exit\"}\n"
        );
    }
}
