//! 在 Windows 原生移动/缩放循环未启动时，恢复 winit 的拖动状态。
//!
//! 窗口几何和 DPI 由 Windows 与 winit 共同维护；这里不改写 `WM_DPICHANGED` 或
//! `WM_WINDOWPOSCHANGING`，避免应用层与系统的建议矩形同时调整窗口尺寸。

use std::{cell::RefCell, collections::HashMap, io};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetCursorPos, HTCAPTION, HTSIZEFIRST, HTSIZELAST, PostMessageW, WM_ENTERSIZEMOVE,
            WM_EXITSIZEMOVE, WM_NCDESTROY, WM_NCLBUTTONDOWN,
        },
    },
};

const SUBCLASS_ID: usize = 0x4153_4452_4147_3031;

thread_local! {
    static FRAMES: RefCell<HashMap<HWND, DragFrames>> = RefCell::new(HashMap::new());
}

#[derive(Default)]
struct DragFrames {
    entered: Vec<bool>,
}

impl DragFrames {
    fn begin(&mut self) {
        self.entered.push(false);
    }

    fn note_enter(&mut self) {
        if let Some(entered) = self.entered.last_mut() {
            *entered = true;
        }
    }

    /// 只有最外层请求失败时才补发结束消息，避免打断已经开始的移动/缩放循环。
    fn end(&mut self) -> bool {
        let entered = self.entered.pop().unwrap_or(true);
        self.entered.is_empty() && !entered
    }
}

pub fn install(hwnd: isize) -> io::Result<()> {
    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window handle is not available",
        ));
    }
    if unsafe { SetWindowSubclass(hwnd as HWND, Some(drag_recovery_proc), SUBCLASS_ID, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn is_os_drag_hit(hit: u32) -> bool {
    hit == HTCAPTION || (HTSIZEFIRST..=HTSIZELAST).contains(&hit)
}

fn pack_screen_position(x: i32, y: i32) -> LPARAM {
    ((y as u32 & 0xffff) << 16 | (x as u32 & 0xffff)) as LPARAM
}
struct RecoveryGuard {
    hwnd: HWND,
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        let recover = FRAMES.with(|frames| {
            let mut all = frames.borrow_mut();
            let Some(frame) = all.get_mut(&self.hwnd) else {
                return false;
            };
            let recover = frame.end();
            if frame.entered.is_empty() {
                all.remove(&self.hwnd);
            }
            recover
        });
        if !recover {
            return;
        }
        if unsafe { PostMessageW(self.hwnd, WM_EXITSIZEMOVE, 0, 0) } == 0 {
            eprintln!("failed to post WM_EXITSIZEMOVE for window drag recovery");
            return;
        }
        super::window_trace::log_diagnostic(
            "drag-recovery",
            "posted WM_EXITSIZEMOVE because the size/move loop did not start",
        );
    }
}

unsafe extern "system" fn drag_recovery_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    if message == WM_ENTERSIZEMOVE {
        FRAMES.with(|frames| {
            let mut all = frames.borrow_mut();
            if let Some(frame) = all.get_mut(&hwnd) {
                frame.note_enter();
            }
        });
    }
    if message == WM_NCLBUTTONDOWN && is_os_drag_hit(wparam as u32) {
        FRAMES.with(|frames| frames.borrow_mut().entry(hwnd).or_default().begin());
        let _guard = RecoveryGuard { hwnd };
        // winit 0.30 passes a POINTS pointer here instead of packed coordinates.
        // Keep its drag/release lifecycle, but give Windows the actual anchor.
        let mut cursor = POINT::default();
        if unsafe { GetCursorPos(&mut cursor) } == 0 {
            eprintln!(
                "failed to query window drag anchor: {}",
                io::Error::last_os_error()
            );
            return 0;
        }
        let position = pack_screen_position(cursor.x, cursor.y);
        return unsafe { DefSubclassProc(hwnd, message, wparam, position) };
    }
    if message == WM_NCDESTROY {
        FRAMES.with(|frames| {
            frames.borrow_mut().remove(&hwnd);
        });
        unsafe {
            RemoveWindowSubclass(hwnd, Some(drag_recovery_proc), SUBCLASS_ID);
        }
    }
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_caption_and_sizing_border_hits_can_stick_winit_dragging() {
        use windows_sys::Win32::UI::WindowsAndMessaging::{HTCLIENT, HTCLOSE, HTMINBUTTON};

        assert!(is_os_drag_hit(HTCAPTION));
        assert!(is_os_drag_hit(HTSIZEFIRST));
        assert!(is_os_drag_hit(HTSIZELAST));
        assert!(!is_os_drag_hit(HTCLIENT));
        assert!(!is_os_drag_hit(HTMINBUTTON));
        assert!(!is_os_drag_hit(HTCLOSE));
    }

    #[test]
    fn missing_size_move_loop_requests_recovery_once() {
        let mut frames = DragFrames::default();
        frames.begin();
        assert!(frames.end());
        assert!(!frames.end());
    }

    #[test]
    fn completed_size_move_loop_does_not_request_recovery() {
        let mut frames = DragFrames::default();
        frames.begin();
        frames.note_enter();
        assert!(!frames.end());
    }

    #[test]
    fn nested_refusal_waits_until_the_outer_frame_closes() {
        let mut frames = DragFrames::default();
        frames.begin();
        frames.begin();
        assert!(!frames.end());
        assert!(frames.end());
    }

    #[test]
    fn packed_drag_anchor_preserves_negative_monitor_coordinates() {
        for (x, y) in [(-1440, 300), (3840, -1080), (-32768, 32767), (0, 0)] {
            let packed = pack_screen_position(x, y) as u32;
            assert_eq!((packed as u16 as i16) as i32, x);
            assert_eq!(((packed >> 16) as u16 as i16) as i32, y);
        }
    }

    #[test]
    fn refusal_inside_an_active_loop_does_not_cancel_that_loop() {
        let mut frames = DragFrames::default();
        frames.begin();
        frames.note_enter();
        frames.begin();
        assert!(!frames.end());
        assert!(!frames.end());
    }
    #[test]
    fn recovery_frames_are_isolated_by_native_window() {
        let first = 1usize as HWND;
        let second = 2usize as HWND;
        let mut frames = HashMap::new();
        frames.insert(first, DragFrames::default());
        frames.insert(second, DragFrames::default());
        frames.get_mut(&first).unwrap().begin();
        frames.get_mut(&first).unwrap().note_enter();
        frames.get_mut(&second).unwrap().begin();
        assert!(!frames.get_mut(&first).unwrap().end());
        assert!(frames.get_mut(&second).unwrap().end());
    }
}
