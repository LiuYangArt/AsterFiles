use super::*;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct RenameClickTarget {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub request_id: RequestId,
    pub entry_id: EntryId,
    pub identity: ActivateTarget,
}

impl RenameClickTarget {
    pub(super) fn current(state: &WindowSessions, entry_id: EntryId) -> Option<Self> {
        let app = state.lock().ok()?;
        let tab = app.active();
        Some(Self {
            window_id: state.window_id,
            tab_id: tab.id,
            request_id: tab.latest_request(),
            entry_id,
            identity: ActivateTarget::from(tab.visible_entry(entry_id)?),
        })
    }

    pub(super) fn still_selected(&self, state: &WindowSessions) -> bool {
        Self::current(state, self.entry_id).as_ref() == Some(self)
            && state
                .lock()
                .ok()
                .is_some_and(|app| app.active().selected == [self.entry_id])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ClickAction {
    Select,
    Open,
    ScheduleRename,
}

#[derive(Default)]
pub(super) struct RenameClickState {
    last: Option<(RenameClickTarget, Instant)>,
    pub(super) pending: Option<RenameClickTarget>,
}

impl RenameClickState {
    pub(super) fn cancel(&mut self, reset_clicks: bool) {
        self.pending = None;
        if reset_clicks {
            self.last = None;
        }
    }

    pub(super) fn click(
        &mut self,
        target: Option<RenameClickTarget>,
        now: Instant,
        interval: Duration,
        only_selected: bool,
        name_hit: bool,
        modified: bool,
    ) -> ClickAction {
        self.pending = None;
        let double = !modified
            && only_selected
            && target.is_some()
            && self.last.as_ref().is_some_and(|(last, time)| {
                target.as_ref() == Some(last) && now.saturating_duration_since(*time) <= interval
            });
        self.last = if double || modified {
            None
        } else {
            target.clone().map(|target| (target, now))
        };
        if double {
            ClickAction::Open
        } else if !modified && only_selected && name_hit && target.is_some() {
            self.pending = target;
            ClickAction::ScheduleRename
        } else {
            ClickAction::Select
        }
    }
}

pub(super) fn schedule(
    weak: slint::Weak<AppWindow>,
    state: WindowSessions,
    clicks: Rc<RefCell<RenameClickState>>,
    timer: &slint::Timer,
    interval: Duration,
) {
    timer.start(slint::TimerMode::SingleShot, interval, move || {
        let Some(target) = clicks.borrow_mut().pending.take() else {
            return;
        };
        crate::operation_audit::record(
            "rename-delay-fired",
            format!(
                "window={} tab={} request={} entry={}",
                target.window_id.0, target.tab_id.0, target.request_id.0, target.entry_id.0
            ),
        );
        if target.still_selected(&state)
            && let Some(ui) = weak.upgrade()
            && !ui.get_rename_editing()
            && !ui.get_context_menu_open()
        {
            begin_rename_ui(&ui.as_weak(), &state);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> RenameClickTarget {
        RenameClickTarget {
            window_id: WindowId(1),
            tab_id: TabId(1),
            request_id: RequestId(1),
            entry_id: EntryId(1),
            identity: ActivateTarget::Path(PathBuf::from("file.txt")),
        }
    }

    #[test]
    fn issue_188_first_click_selects_second_slow_name_click_waits_and_double_opens() {
        for interval in [Duration::from_millis(200), Duration::from_millis(900)] {
            let mut clicks = RenameClickState::default();
            let start = Instant::now();
            assert_eq!(
                clicks.click(Some(target()), start, interval, false, true, false),
                ClickAction::Select
            );
            let slow = start + interval + Duration::from_millis(1);
            assert_eq!(
                clicks.click(Some(target()), slow, interval, true, true, false),
                ClickAction::ScheduleRename
            );
            assert!(clicks.pending.is_some());
            assert_eq!(
                clicks.click(
                    Some(target()),
                    slow + interval / 2,
                    interval,
                    true,
                    true,
                    false
                ),
                ClickAction::Open
            );
            assert!(clicks.pending.is_none());
        }
    }

    #[test]
    fn issue_188_modified_non_name_and_invalid_targets_never_schedule() {
        let now = Instant::now();
        for (selected, name, modified) in [
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            let mut clicks = RenameClickState::default();
            assert_eq!(
                clicks.click(
                    Some(target()),
                    now,
                    Duration::from_millis(500),
                    selected,
                    name,
                    modified
                ),
                ClickAction::Select
            );
            assert!(clicks.pending.is_none());
        }
        let mut clicks = RenameClickState::default();
        assert_eq!(
            clicks.click(None, now, Duration::from_millis(500), true, true, false),
            ClickAction::Select
        );
    }

    #[test]
    fn issue_188_different_tab_request_or_identity_is_not_a_double_click() {
        for field in 0..4 {
            let mut clicks = RenameClickState::default();
            let now = Instant::now();
            clicks.click(
                Some(target()),
                now,
                Duration::from_millis(500),
                true,
                true,
                false,
            );
            let mut other = target();
            match field {
                0 => other.tab_id = TabId(2),
                1 => other.request_id = RequestId(2),
                2 => other.entry_id = EntryId(2),
                _ => other.identity = ActivateTarget::Path(PathBuf::from("other.txt")),
            }
            assert_eq!(
                clicks.click(
                    Some(other),
                    now + Duration::from_millis(10),
                    Duration::from_millis(500),
                    false,
                    true,
                    false
                ),
                ClickAction::Select
            );
            assert!(clicks.pending.is_none());
        }
    }
    #[test]
    fn issue_188_interrupted_click_sequence_selects_instead_of_opening() {
        let interval = Duration::from_millis(500);
        let now = Instant::now();
        for reset in [false, true] {
            let mut clicks = RenameClickState::default();
            clicks.click(Some(target()), now, interval, true, true, false);
            clicks.cancel(reset);
            assert!(clicks.pending.is_none());
            assert_eq!(
                clicks.click(
                    Some(target()),
                    now + interval / 2,
                    interval,
                    true,
                    true,
                    false
                ),
                if reset {
                    ClickAction::ScheduleRename
                } else {
                    ClickAction::Open
                }
            );
        }
        let mut clicks = RenameClickState::default();
        clicks.click(Some(target()), now, interval, true, true, false);
        clicks.cancel(true);
        assert_eq!(
            clicks.click(
                Some(target()),
                now + interval / 2,
                interval,
                false,
                true,
                false
            ),
            ClickAction::Select
        );
    }

    fn fixture() -> (
        AppWindow,
        WindowSessions,
        Rc<RefCell<RenameClickState>>,
        slint::Timer,
    ) {
        let root = PathBuf::from(r"C:\issue-188");
        let mut app = AppState::new_for_test(vec![root.clone(), root.clone()], 0, [0, 1, 2, 3]);
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.load_state = LoadState::Complete;
        tab.replace_entries(vec![FileEntry {
            id: EntryId(1),
            original_name: "file.txt".into(),
            display_name: "file.txt".into(),
            path: root.join("file.txt").into(),
            name_highlights: Vec::new(),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            library_source_index: None,
            icon_requires_path: false,
            parent_display: String::new(),
            size_bytes: Some(1),
            folder_size: FolderSizeState::NotIndexed,
            modified: None,
            created: None,
            webdav: None,
        }]);
        tab.select_entry(EntryId(1), false, false);
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
        let ui = AppWindow::new().unwrap();
        refresh_ui(&ui, &state);
        let clicks = Rc::new(RefCell::new(RenameClickState::default()));
        clicks.borrow_mut().pending = RenameClickTarget::current(&state, EntryId(1));
        (ui, state, clicks, slint::Timer::default())
    }

    #[test]
    fn issue_188_real_timer_waits_until_double_click_window_ends() {
        i_slint_backend_testing::init_no_event_loop();
        let (ui, state, clicks, timer) = fixture();
        schedule(
            ui.as_weak(),
            state,
            clicks,
            &timer,
            Duration::from_millis(500),
        );
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(499));
        assert!(!ui.get_rename_editing());
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(2));
        assert!(ui.get_rename_editing());
        assert_eq!(ui.get_rename_entry_id(), 1);
        assert_eq!(ui.get_rename_selection_end(), 4);
    }

    #[test]
    fn issue_188_timer_rejects_cancelled_selection_navigation_and_replaced_identity() {
        i_slint_backend_testing::init_no_event_loop();
        for invalidation in 0..5 {
            let (ui, state, clicks, timer) = fixture();
            schedule(
                ui.as_weak(),
                state.clone(),
                clicks.clone(),
                &timer,
                Duration::from_millis(500),
            );
            match invalidation {
                0 => clicks.borrow_mut().pending = None,
                1 => state.lock().unwrap().active_window_state_mut().active_tab = TabId(2),
                2 => state
                    .lock()
                    .unwrap()
                    .tab_mut(TabId(1))
                    .unwrap()
                    .clear_selection(),
                3 => {
                    state
                        .lock()
                        .unwrap()
                        .tab_mut(TabId(1))
                        .unwrap()
                        .begin_directory_navigation(
                            PathBuf::from(r"C:\elsewhere"),
                            NavigationKind::Normal,
                        );
                }
                _ => {
                    let mut app = state.lock().unwrap();
                    let mut entries = app.active().entries.as_ref().clone();
                    entries[0].path = PathBuf::from(r"C:\replacement.txt").into();
                    app.tab_mut(TabId(1)).unwrap().replace_entries(entries);
                }
            }
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(501));
            assert!(!ui.get_rename_editing(), "invalidation {invalidation}");
            assert!(clicks.borrow().pending.is_none());
        }
    }
}
