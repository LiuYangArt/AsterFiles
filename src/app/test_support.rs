use super::{AppState, WindowId};
use crate::{domain::TabId, session_store};
use std::path::PathBuf;
pub(super) struct FixtureCleanup(pub(super) PathBuf);

impl Drop for FixtureCleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(windows)]
pub(super) fn process_cpu_millis() -> u64 {
    use windows_sys::Win32::{
        Foundation::FILETIME,
        System::Threading::{GetCurrentProcess, GetProcessTimes},
    };
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        );
    }
    let ticks =
        |value: FILETIME| (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime);
    ticks(kernel).saturating_add(ticks(user)) / 10_000
}

#[cfg(not(windows))]
pub(super) fn process_cpu_millis() -> u64 {
    0
}

pub(super) fn test_window_placement(x: i32) -> session_store::WindowPlacement {
    session_store::WindowPlacement {
        x,
        y: 80,
        width: 1180,
        height: 760,
    }
}

pub(super) fn begin_drag_at(app: &mut AppState, index: usize) -> (WindowId, TabId) {
    let window_id = app.active_window;
    let tab_id = app.active_window_state().tab_order[index];
    assert!(app.begin_tab_drag(window_id, tab_id, index, 100.0, 20.0));
    (window_id, tab_id)
}
