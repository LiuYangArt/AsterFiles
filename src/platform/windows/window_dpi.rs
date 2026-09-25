//! Keep the size and cursor anchor of an interactive DPI move in Windows' hands.
//! winit 0.30.13 repositions the DPI rectangle against the still-unmoved HWND,
//! which can send it back to the old monitor. Its single DPI placement is adapted
//! here; ordinary move/resize requests remain untouched.

use std::{cell::RefCell, collections::HashMap, io};

use windows_sys::Win32::{
    Foundation::{HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM},
    UI::{
        HiDpi::GetDpiForWindow,
        Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
        WindowsAndMessaging::{
            GetClientRect, GetWindowRect, HTCAPTION, HTSIZEFIRST, HTSIZELAST, IsIconic, IsZoomed,
            SWP_NOACTIVATE, SWP_NOZORDER, WINDOWPOS, WM_DPICHANGED, WM_ENTERSIZEMOVE,
            WM_EXITSIZEMOVE, WM_GETDPISCALEDSIZE, WM_NCDESTROY, WM_NCLBUTTONDOWN, WM_SIZE,
            WM_WINDOWPOSCHANGING,
        },
    },
};

const SUBCLASS_ID: usize = 0x4153_4450_4930_3031;
const DPI_PLACEMENT_FLAGS: u32 = SWP_NOZORDER | SWP_NOACTIVATE;

thread_local! {
    static WINDOWS: RefCell<HashMap<HWND, WindowDpiState>> = RefCell::new(HashMap::new());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClientBaseline {
    width: u32,
    height: u32,
    dpi: u32,
}

impl ClientBaseline {
    fn at_dpi(self, dpi: u32) -> (i32, i32) {
        let scale = |value: u32| {
            ((u64::from(value) * u64::from(dpi) + u64::from(self.dpi.max(1)) / 2)
                / u64::from(self.dpi.max(1)))
            .clamp(1, i32::MAX as u64) as i32
        };
        (scale(self.width), scale(self.height))
    }
}

#[derive(Default)]
struct WindowDpiState {
    caption: bool,
    moving: bool,
    dpi_started: bool,
    baseline: Option<ClientBaseline>,
    placements: Vec<Option<RECT>>,
}

impl WindowDpiState {
    fn begin_move(&mut self, caption: bool, baseline: Option<ClientBaseline>) {
        self.caption = caption;
        self.moving = false;
        self.dpi_started = false;
        self.baseline = if caption { baseline } else { None };
    }

    fn enter_move(&mut self, baseline: Option<ClientBaseline>) {
        self.moving = true;
        self.note_size(baseline);
    }

    fn note_size(&mut self, baseline: Option<ClientBaseline>) {
        // Maximized/Snap restore may finish after the caption press. Accept its
        // geometry until the first DPI query, then freeze it for the entire move.
        // Later ordinary WM_SIZE messages also carry DPI-generated dimensions.
        if self.caption && !self.dpi_started && self.placements.is_empty() {
            self.baseline = baseline;
        }
    }

    fn end_move(&mut self) {
        self.caption = false;
        self.moving = false;
        self.dpi_started = false;
        self.baseline = None;
        // Keep depth until each nested WM_DPICHANGED unwinds, but invalidate every
        // pending rectangle so a completed move cannot rewrite a later request.
        for placement in &mut self.placements {
            *placement = None;
        }
    }

    fn scaled_candidate(&mut self, dpi: u32) -> Option<(i32, i32)> {
        if !self.caption || !self.moving || dpi == 0 {
            return None;
        }
        self.dpi_started = true;
        self.baseline.map(|baseline| baseline.at_dpi(dpi))
    }

    fn begin_dpi(&mut self, suggested: Option<RECT>) {
        let placement = if self.caption && self.moving {
            self.dpi_started = true;
            suggested.filter(|rect| rect.right > rect.left && rect.bottom > rect.top)
        } else {
            None
        };
        self.placements.push(placement);
    }

    fn end_dpi(&mut self) {
        self.placements.pop();
    }

    fn adapt_dpi_placement(&mut self, position: &mut WINDOWPOS) -> bool {
        // The locked winit version uses exactly these flags for its DPI placement.
        // Consume once, only inside that native message, never clamp later moves.
        if position.flags != DPI_PLACEMENT_FLAGS {
            return false;
        }
        let Some(rect) = self.placements.last_mut().and_then(Option::take) else {
            return false;
        };
        position.x = rect.left;
        position.y = rect.top;
        position.cx = rect.right - rect.left;
        position.cy = rect.bottom - rect.top;
        true
    }
}

pub fn install(hwnd: isize) -> io::Result<()> {
    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window handle is not available",
        ));
    }
    let hwnd = hwnd as HWND;
    if unsafe { SetWindowSubclass(hwnd, Some(window_dpi_proc), SUBCLASS_ID, 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    WINDOWS.with(|windows| {
        windows.borrow_mut().entry(hwnd).or_default();
    });
    Ok(())
}

fn borderless_normal_baseline(hwnd: HWND) -> Option<ClientBaseline> {
    if unsafe { IsZoomed(hwnd) != 0 || IsIconic(hwnd) != 0 } {
        return None;
    }
    let mut client = RECT::default();
    let mut outer = RECT::default();
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    if dpi == 0
        || unsafe { GetClientRect(hwnd, &mut client) == 0 || GetWindowRect(hwnd, &mut outer) == 0 }
    {
        return None;
    }
    let width = u32::try_from(client.right - client.left)
        .ok()
        .filter(|v| *v > 0)?;
    let height = u32::try_from(client.bottom - client.top)
        .ok()
        .filter(|v| *v > 0)?;
    // WM_GETDPISCALEDSIZE expects an outer size. AsterFiles' adapted windows
    // have no native frame; do not reuse client dimensions for a framed window.
    if outer.right - outer.left != width as i32 || outer.bottom - outer.top != height as i32 {
        return None;
    }
    Some(ClientBaseline { width, height, dpi })
}

unsafe extern "system" fn window_dpi_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    _: usize,
) -> LRESULT {
    match message {
        WM_NCLBUTTONDOWN
            if wparam as u32 == HTCAPTION
                || (HTSIZEFIRST..=HTSIZELAST).contains(&(wparam as u32)) =>
        {
            let baseline = borderless_normal_baseline(hwnd);
            WINDOWS.with(|windows| {
                if let Some(state) = windows.borrow_mut().get_mut(&hwnd) {
                    state.begin_move(wparam as u32 == HTCAPTION, baseline);
                }
            });
        }
        WM_ENTERSIZEMOVE => {
            let baseline = borderless_normal_baseline(hwnd);
            WINDOWS.with(|windows| {
                if let Some(state) = windows.borrow_mut().get_mut(&hwnd) {
                    state.enter_move(baseline);
                }
            });
        }
        WM_GETDPISCALEDSIZE if lparam != 0 => {
            let dpi = wparam as u32;
            let candidate =
                WINDOWS.with(|windows| windows.borrow_mut().get_mut(&hwnd)?.scaled_candidate(dpi));
            if let Some((width, height)) = candidate {
                unsafe {
                    *(lparam as *mut SIZE) = SIZE {
                        cx: width,
                        cy: height,
                    };
                }
                if super::window_trace::is_active() {
                    super::window_trace::log_diagnostic(
                        "dpi-scale-size",
                        &format!("hwnd={} dpi={dpi} outer={width}x{height}", hwnd as usize),
                    );
                }
                return 1;
            }
        }
        WM_DPICHANGED => {
            let suggested = if lparam != 0 && unsafe { IsZoomed(hwnd) == 0 && IsIconic(hwnd) == 0 }
            {
                Some(unsafe { *(lparam as *const RECT) })
            } else {
                None
            };
            WINDOWS.with(|windows| {
                if let Some(state) = windows.borrow_mut().get_mut(&hwnd) {
                    state.begin_dpi(suggested);
                }
            });
        }
        WM_WINDOWPOSCHANGING if lparam != 0 => {
            let position = unsafe { &mut *(lparam as *mut WINDOWPOS) };
            let adapted = WINDOWS.with(|windows| {
                windows
                    .borrow_mut()
                    .get_mut(&hwnd)
                    .is_some_and(|state| state.adapt_dpi_placement(position))
            });
            if adapted && super::window_trace::is_active() {
                super::window_trace::log_diagnostic(
                    "dpi-system-placement",
                    &format!(
                        "hwnd={} rect={},{},{},{}",
                        hwnd as usize, position.x, position.y, position.cx, position.cy
                    ),
                );
            }
        }
        WM_SIZE => {
            let baseline = borderless_normal_baseline(hwnd);
            WINDOWS.with(|windows| {
                if let Some(state) = windows.borrow_mut().get_mut(&hwnd) {
                    state.note_size(baseline);
                }
            });
        }
        WM_EXITSIZEMOVE => WINDOWS.with(|windows| {
            if let Some(state) = windows.borrow_mut().get_mut(&hwnd) {
                state.end_move();
            }
        }),
        WM_NCDESTROY => {
            WINDOWS.with(|windows| {
                windows.borrow_mut().remove(&hwnd);
            });
            unsafe {
                RemoveWindowSubclass(hwnd, Some(window_dpi_proc), SUBCLASS_ID);
            }
        }
        _ => {}
    }
    // winit/Slint still receive every DPI event. No borrow survives this call:
    // SetWindowPos and application callbacks can re-enter the native procedure.
    let result = unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    if message == WM_DPICHANGED {
        WINDOWS.with(|windows| {
            if let Some(state) = windows.borrow_mut().get_mut(&hwnd) {
                state.end_dpi();
            }
        });
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::UI::WindowsAndMessaging::{SWP_NOMOVE, SWP_NOSIZE};

    fn baseline(width: u32, height: u32, dpi: u32) -> ClientBaseline {
        ClientBaseline { width, height, dpi }
    }
    fn moving(origin: ClientBaseline) -> WindowDpiState {
        let mut state = WindowDpiState::default();
        state.begin_move(true, Some(origin));
        state.enter_move(Some(origin));
        state
    }
    fn rect(left: i32, top: i32, width: i32, height: i32) -> RECT {
        RECT {
            left,
            top,
            right: left + width,
            bottom: top + height,
        }
    }
    fn request(x: i32, y: i32, width: i32, height: i32, flags: u32) -> WINDOWPOS {
        WINDOWPOS {
            x,
            y,
            cx: width,
            cy: height,
            flags,
            ..WINDOWPOS::default()
        }
    }
    fn geometry(position: &WINDOWPOS) -> (i32, i32, i32, i32) {
        (position.x, position.y, position.cx, position.cy)
    }

    #[test]
    fn recorded_boundary_transition_keeps_windows_target_monitor_and_size() {
        // #129, PID 51732: winit changed suggested x=3274 to 3201, triggered
        // reverse DPI, then the next native move grew 1915x1207 to 2873x1811.
        let mut state = moving(baseline(1915, 1207, 144));
        assert_eq!(state.scaled_candidate(96), Some((1277, 805)));
        state.begin_dpi(Some(rect(3274, 131, 1277, 805)));
        let mut position = request(3201, 131, 1277, 805, DPI_PLACEMENT_FLAGS);
        assert!(state.adapt_dpi_placement(&mut position));
        assert_eq!(geometry(&position), (3274, 131, 1277, 805));
        state.note_size(Some(baseline(1277, 805, 96)));
        state.end_dpi();
        // A late same-DPI size cannot poison the next candidate.
        state.note_size(Some(baseline(2873, 1811, 144)));
        assert_eq!(state.scaled_candidate(144), Some((1915, 1207)));
        assert_eq!(state.scaled_candidate(96), Some((1277, 805)));
    }

    #[test]
    fn every_round_trip_uses_the_same_baseline() {
        for dpi in [96, 120, 144] {
            let origin = baseline(1117, 781, dpi);
            let mut state = moving(origin);
            for next in [144, 96, 120, 144, dpi, 96, dpi] {
                assert_eq!(state.scaled_candidate(next), Some(origin.at_dpi(next)));
                state.note_size(Some(baseline(5000, 4000, next)));
            }
            assert_eq!(state.baseline, Some(origin));
        }
    }

    #[test]
    fn maximized_and_snap_restore_can_set_the_baseline_before_dpi_begins() {
        for initial in [None, Some(baseline(1920, 2160, 144))] {
            let mut state = WindowDpiState::default();
            state.begin_move(true, initial);
            state.enter_move(initial);
            state.note_size(Some(baseline(1600, 900, 144)));
            assert_eq!(state.scaled_candidate(96), Some((1067, 600)));
            state.note_size(Some(baseline(2400, 1350, 144)));
            assert_eq!(state.scaled_candidate(144), Some((1600, 900)));
        }
    }

    #[test]
    fn dpi_before_restore_does_not_promote_an_intermediate_size() {
        let mut state = WindowDpiState::default();
        state.begin_move(true, None);
        state.enter_move(None);
        state.begin_dpi(Some(rect(50, 50, 1000, 700)));
        state.note_size(Some(baseline(1000, 700, 96)));
        state.end_dpi();
        state.note_size(Some(baseline(1500, 1050, 144)));
        assert_eq!(state.scaled_candidate(96), None);
    }

    #[test]
    fn ordinary_moves_and_border_resizes_are_never_rewritten() {
        let mut state = moving(baseline(1000, 700, 96));
        let mut position = request(10, 20, 1300, 800, DPI_PLACEMENT_FLAGS);
        assert!(!state.adapt_dpi_placement(&mut position));
        state.begin_move(false, Some(baseline(1000, 700, 96)));
        state.enter_move(None);
        assert_eq!(state.scaled_candidate(144), None);
        state.begin_dpi(Some(rect(0, 0, 1500, 1050)));
        assert!(!state.adapt_dpi_placement(&mut position));
        state.end_dpi();
        assert_eq!(geometry(&position), (10, 20, 1300, 800));
    }

    #[test]
    fn dpi_placement_is_consumed_once_and_ignores_size_only_callbacks() {
        let mut state = moving(baseline(1000, 700, 96));
        state.begin_dpi(Some(rect(-1440, 100, 1250, 875)));
        for flag in [SWP_NOMOVE, SWP_NOSIZE] {
            let mut other = request(10, 20, 30, 40, DPI_PLACEMENT_FLAGS | flag);
            assert!(!state.adapt_dpi_placement(&mut other));
        }
        let mut position = request(0, 0, 1000, 700, DPI_PLACEMENT_FLAGS);
        assert!(state.adapt_dpi_placement(&mut position));
        assert_eq!(geometry(&position), (-1440, 100, 1250, 875));
        assert!(!state.adapt_dpi_placement(&mut position));
        state.end_dpi();
        assert!(!state.adapt_dpi_placement(&mut position));
    }

    #[test]
    fn nested_native_messages_do_not_consume_each_others_rectangle() {
        let mut state = moving(baseline(1000, 700, 96));
        state.begin_dpi(Some(rect(100, 200, 1500, 1050)));
        state.begin_dpi(Some(rect(-200, 10, 1250, 875)));
        let mut position = request(0, 0, 1, 1, DPI_PLACEMENT_FLAGS);
        assert!(state.adapt_dpi_placement(&mut position));
        assert_eq!(geometry(&position), (-200, 10, 1250, 875));
        assert!(!state.adapt_dpi_placement(&mut position));
        state.end_dpi();
        assert!(state.adapt_dpi_placement(&mut position));
        assert_eq!(geometry(&position), (100, 200, 1500, 1050));
        state.end_dpi();
    }

    #[test]
    fn ending_a_move_cancels_its_pending_placement_and_next_move_rebaselines() {
        let mut state = moving(baseline(1000, 700, 96));
        state.begin_dpi(Some(rect(100, 200, 1500, 1050)));
        state.begin_dpi(Some(rect(-200, 10, 1250, 875)));
        state.end_move();
        assert_eq!(state.placements.len(), 2);
        assert!(state.placements.iter().all(Option::is_none));
        let mut position = request(0, 0, 1, 1, DPI_PLACEMENT_FLAGS);
        assert!(!state.adapt_dpi_placement(&mut position));
        state.end_dpi();
        assert_eq!(state.placements.len(), 1);
        assert!(!state.adapt_dpi_placement(&mut position));
        state.end_dpi();
        assert!(state.placements.is_empty());
        state.begin_move(true, Some(baseline(1800, 1200, 144)));
        state.enter_move(Some(baseline(1800, 1200, 144)));
        assert_eq!(state.scaled_candidate(96), Some((1200, 800)));
    }

    #[test]
    fn separate_windows_do_not_share_candidates_or_placements() {
        let mut windows = HashMap::new();
        windows.insert(1usize, moving(baseline(1915, 1207, 144)));
        windows.insert(2usize, moving(baseline(1000, 700, 96)));
        windows
            .get_mut(&1)
            .unwrap()
            .begin_dpi(Some(rect(0, 0, 1277, 805)));
        let mut position = request(1, 2, 3, 4, DPI_PLACEMENT_FLAGS);
        assert!(
            !windows
                .get_mut(&2)
                .unwrap()
                .adapt_dpi_placement(&mut position)
        );
        assert_eq!(
            windows.get_mut(&2).unwrap().scaled_candidate(144),
            Some((1500, 1050))
        );
        windows.remove(&1);
        assert_eq!(
            windows.get_mut(&2).unwrap().scaled_candidate(96),
            Some((1000, 700))
        );
    }
}
