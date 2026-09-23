//! winit 在发出移动或缩放前把 `dragging` 置为真，并且只在 `WM_EXITSIZEMOVE` 时清除。
//! 最大化窗口会拒绝边缘缩放；Slint 在下一次鼠标移动前仍可能沿用最大化前的缩放方向。
//! 系统因此不进入模态循环，也不会发送 `WM_EXITSIZEMOVE`，随后的移动和缩放都被忽略。
//! 客户区点击不依赖该标记，所以按钮和文件夹仍然可用。

use std::{cell::RefCell, io};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, WPARAM},
    UI::{
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            HTCAPTION, HTSIZEFIRST, HTSIZELAST, PostMessageW, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
            WM_NCDESTROY, WM_NCLBUTTONDOWN,
        },
    },
};

const SUBCLASS_ID: usize = 0x4153_4452_4147_3031;

thread_local! {
    static FRAMES: RefCell<DragFrames> = RefCell::new(DragFrames::default());
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

    /// 只有最外层请求失败时才补发结束消息，避免打断已经开始的缩放循环。
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

struct RecoveryGuard {
    hwnd: HWND,
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        let recover = FRAMES.with(|frames| frames.borrow_mut().end());
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
        FRAMES.with(|frames| frames.borrow_mut().note_enter());
    }
    if message == WM_NCLBUTTONDOWN && is_os_drag_hit(wparam as u32) {
        FRAMES.with(|frames| frames.borrow_mut().begin());
        let _guard = RecoveryGuard { hwnd };
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    }
    if message == WM_NCDESTROY {
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
    fn refusal_inside_an_active_loop_does_not_cancel_that_loop() {
        let mut frames = DragFrames::default();
        frames.begin();
        frames.note_enter();
        frames.begin();
        assert!(!frames.end());
        assert!(!frames.end());
    }
}
