use super::*;
use slint::ComponentHandle;

const NETWORK_LOADING_HINT_DELAY: Duration = Duration::from_millis(500);

fn is_network_directory(tab: &TabSession) -> bool {
    tab.page_source == PageSource::Directory
        && tab.visible_path().is_some_and(crate::network::is_unc_path)
}

pub(super) fn project_page_state(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    tab: &TabSession,
    projection_changed: bool,
    page_state: i32,
) {
    let delayed = is_network_directory(tab);
    if page_state != 1 || !delayed {
        ui.set_loading_hint_generation(ui.get_loading_hint_generation().wrapping_add(1));
        ui.set_loading_hint_visible(page_state == 1);
    } else if projection_changed || ui.get_page_state() != 1 {
        let generation = ui.get_loading_hint_generation().wrapping_add(1);
        ui.set_loading_hint_generation(generation);
        ui.set_loading_hint_visible(false);
        let weak = ui.as_weak();
        let state = state.clone();
        let tab_id = tab.id;
        let request_id = tab.latest_request;
        // Delay only the hint; navigation and batches continue without waiting for this timer.
        slint::Timer::single_shot(NETWORK_LOADING_HINT_DELAY, move || {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_loading_hint_generation() != generation || ui.get_page_state() != 1 {
                return;
            }
            let still_waiting = state.lock().ok().is_some_and(|app| {
                app.window(window_id).is_some_and(|window| {
                    window.active_tab == tab_id
                        && window.tabs.get(&tab_id).is_some_and(|tab| {
                            tab.accepts(request_id)
                                && tab.load_state == LoadState::Loading
                                && tab.pending_entries.is_empty()
                                && is_network_directory(tab)
                        })
                })
            });
            if still_waiting {
                ui.set_loading_hint_visible(true);
            }
        });
    }
    ui.set_page_state(page_state);
}

#[cfg(test)]
mod tests {
    use super::*;

    const NETWORK: &str = r"\\issue-143\share";

    fn fixture() -> (AppWindow, WindowSessions) {
        let mut app = AppState::new_for_test(vec![PathBuf::from(NETWORK)], 0, [0, 1, 2, 3]);
        let window_id = app.active_window;
        app.tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(PathBuf::from(NETWORK), NavigationKind::Normal);
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
        let ui = AppWindow::new().unwrap();
        refresh_ui(&ui, &state);
        (ui, state)
    }

    fn advance(milliseconds: u64) {
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(milliseconds));
    }

    #[test]
    fn issue_143_loading_hint_waits_500ms_without_restarting_on_refresh() {
        i_slint_backend_testing::init_no_event_loop();
        let (ui, state) = fixture();
        assert_eq!(ui.get_page_state(), 1);
        assert!(!ui.get_loading_hint_visible());
        advance(499);
        assert!(!ui.get_loading_hint_visible());
        refresh_ui(&ui, &state);
        advance(1);
        assert!(ui.get_loading_hint_visible());
        assert_eq!(ui.get_page_state(), 1);
    }

    #[test]
    fn issue_143_first_batch_hides_hint_and_is_visible_without_delay() {
        i_slint_backend_testing::init_no_event_loop();
        for elapsed in [100, 500] {
            let (ui, state) = fixture();
            advance(elapsed);
            assert_eq!(ui.get_loading_hint_visible(), elapsed == 500);
            let request_id = state.lock().unwrap().active().latest_request;
            let item = FileEntry {
                id: EntryId(1),
                original_name: "file.txt".into(),
                display_name: "file.txt".into(),
                name_highlights: Vec::new(),
                path: PathBuf::from(NETWORK).join("file.txt"),
                kind: crate::domain::EntryKind::File,
                open_target: None,
                library_source_index: None,
                parent_display: NETWORK.into(),
                size_bytes: Some(1),
                folder_size: FolderSizeState::NotIndexed,
                modified: None,
                created: None,
            };
            apply_event(
                &state.shared,
                DirectoryEvent::Batch {
                    tab_id: TabId(1),
                    request_id,
                    entries: vec![item],
                },
            );
            append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
            assert_eq!(ui.get_page_state(), 2);
            assert_eq!(ui.get_files().row_count(), 1);
            assert!(!ui.get_loading_hint_visible());
            advance(500);
            assert!(!ui.get_loading_hint_visible());
        }
    }

    #[test]
    fn issue_143_terminal_states_are_immediate_and_reject_old_timers() {
        i_slint_backend_testing::init_no_event_loop();
        for terminal in [
            LoadState::Complete,
            LoadState::Cancelled,
            LoadState::PermissionDenied,
            LoadState::Disconnected,
        ] {
            let (ui, state) = fixture();
            advance(100);
            state.lock().unwrap().tab_mut(TabId(1)).unwrap().load_state = terminal;
            refresh_ui(&ui, &state);
            let expected = agent_debug::page_projection(terminal, true).index;
            assert_eq!(ui.get_page_state(), expected);
            assert!(!ui.get_loading_hint_visible());
            advance(500);
            assert_eq!(ui.get_page_state(), expected);
            assert!(!ui.get_loading_hint_visible());
        }
    }

    #[test]
    fn issue_143_new_navigation_and_returning_to_a_tab_restart_the_hint_delay() {
        i_slint_backend_testing::init_no_event_loop();
        for switch_tab in [false, true] {
            let (ui, state) = fixture();
            advance(400);
            if switch_tab {
                state.lock().unwrap().create_tab(PathBuf::from(r"C:\local"));
                refresh_ui(&ui, &state);
                state.lock().unwrap().active_window_state_mut().active_tab = TabId(1);
            } else {
                state
                    .lock()
                    .unwrap()
                    .tab_mut(TabId(1))
                    .unwrap()
                    .begin_directory_navigation(
                        PathBuf::from(r"\\issue-143\share\next"),
                        NavigationKind::Normal,
                    );
            }
            refresh_ui(&ui, &state);
            advance(100);
            assert!(!ui.get_loading_hint_visible());
            advance(399);
            assert!(!ui.get_loading_hint_visible());
            advance(1);
            assert!(ui.get_loading_hint_visible());
        }
    }

    #[test]
    fn issue_143_closed_window_and_cancelled_request_ignore_pending_timer() {
        i_slint_backend_testing::init_no_event_loop();
        for close_window in [false, true] {
            let (ui, state) = fixture();
            if close_window {
                state.lock().unwrap().close_window(state.window_id).unwrap();
            } else {
                state
                    .lock()
                    .unwrap()
                    .tab_mut(TabId(1))
                    .unwrap()
                    .cancel_pending();
            }
            advance(500);
            assert!(!ui.get_loading_hint_visible());
        }
    }

    #[test]
    fn issue_143_local_loading_keeps_its_existing_immediate_hint() {
        i_slint_backend_testing::init_no_event_loop();
        let (ui, state) = fixture();
        state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(PathBuf::from(r"C:\local"), NavigationKind::Normal);
        refresh_ui(&ui, &state);
        assert_eq!(ui.get_page_state(), 1);
        assert!(ui.get_loading_hint_visible());
    }
}
