use super::*;
use slint::ComponentHandle;

fn entry(id: u32, path: PathBuf) -> FileEntry {
    FileEntry {
        id: EntryId(id),
        original_name: path.file_name().unwrap().to_owned(),
        display_name: path.file_name().unwrap().to_string_lossy().into_owned(),
        name_highlights: Vec::new(),
        parent_display: String::new(),
        path,
        kind: crate::domain::EntryKind::File,
        open_target: None,
        library_source_index: None,
        size_bytes: Some(1),
        folder_size: FolderSizeState::Unknown,
        modified: None,
        created: None,
    }
}

#[test]
fn issue_133_completion_focus_follows_display_order_and_grouping() {
    let root = PathBuf::from(r"C:\copies");
    for direction in [SortDirection::Ascending, SortDirection::Descending] {
        for grouping in [GroupField::None, GroupField::Kind] {
            let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
            app.update_directory_preference(root.clone(), |pref| {
                pref.sort_direction = direction;
                pref.group_field = grouping;
                pref.group_direction = direction;
            });
            let tab_id = app.active().id;
            let a = root.join("a.png");
            let z = root.join("z.txt");
            let middle = root.join("m.png");
            queue_completed_focus(&mut app, &[z.clone(), a.clone()]);
            let request_id = app
                .tab_mut(tab_id)
                .unwrap()
                .begin_directory_navigation(root.clone(), NavigationKind::Refresh)
                .0;
            app.focus_after_refresh.get_mut(&tab_id).unwrap().request_id = Some(request_id);
            let state = Arc::new(Mutex::new(app));
            apply_event(
                &state,
                DirectoryEvent::Batch {
                    tab_id,
                    request_id,
                    entries: vec![
                        entry(10, z.clone()),
                        entry(20, a.clone()),
                        entry(30, middle),
                    ],
                },
            );
            apply_event(
                &state,
                DirectoryEvent::Finished {
                    tab_id,
                    request_id,
                    path: root.clone(),
                    skipped: 0,
                    source_failures: 0,
                    library: None,
                },
            );
            let app = state.lock().unwrap();
            let tab = app.tab(tab_id).unwrap();
            assert_eq!(
                tab.selected.iter().copied().collect::<HashSet<_>>(),
                HashSet::from([EntryId(10), EntryId(20)])
            );
            let expected = if direction == SortDirection::Ascending {
                EntryId(10)
            } else {
                EntryId(20)
            };
            assert_eq!(tab.focused, Some(expected));
            assert_eq!(tab.selection_anchor, Some(expected));
            assert!(!app.focus_after_refresh.contains_key(&tab_id));
        }
    }
}

#[test]
fn issue_133_completed_scroll_shows_whole_span_or_prioritizes_bottom() {
    // All fit: preserve a viewport that already contains every target.
    assert_eq!(
        completed_selection_scroll_target(-80.0, 100.0, 200.0, 160.0, 1000.0),
        -80.0
    );
    assert_eq!(
        completed_selection_scroll_target(0.0, 100.0, 200.0, 160.0, 1000.0),
        -40.0
    );
    assert_eq!(
        completed_selection_scroll_target(-300.0, 100.0, 200.0, 160.0, 1000.0),
        -100.0
    );
    // Widely separated: bottom-align the lower target even when it was already visible.
    for current in [0.0, -700.0, -760.0, -900.0] {
        assert_eq!(
            completed_selection_scroll_target(current, 20.0, 840.0, 160.0, 1000.0),
            -680.0
        );
    }
    assert_eq!(
        completed_selection_scroll_target(-500.0, 0.0, 40.0, 160.0, 0.0),
        0.0
    );
}

#[test]
fn issue_133_selection_span_includes_group_headers_and_grid_rows() {
    let groups = vec![
        group_projection::GroupProjection {
            key: "a".into(),
            label: "A".into(),
            detail: String::new(),
            entries: vec![EntryId(1), EntryId(2), EntryId(3)],
            header_visible: true,
        },
        group_projection::GroupProjection {
            key: "b".into(),
            label: "B".into(),
            detail: String::new(),
            entries: vec![EntryId(4), EntryId(5), EntryId(6)],
            header_visible: true,
        },
    ];
    let header = group_header_height(&groups) as f32;
    for mode in [
        ViewMode::Details,
        ViewMode::List,
        ViewMode::MediumIcons,
        ViewMode::Tiles,
        ViewMode::Content,
    ] {
        let height = file_layout_geometry(mode).row_height;
        let span =
            selected_entry_span(&groups, mode, 2, &[EntryId(6), EntryId(2), EntryId(999)]).unwrap();
        let expected_first = if mode.uses_grid_layout() {
            header
        } else {
            header + height
        };
        let expected_last = 2.0 * header
            + if mode.uses_grid_layout() {
                4.0 * height
            } else {
                6.0 * height
            };
        assert_eq!(span, (expected_first, expected_last), "{mode:?}");
        assert_eq!(selected_entry_span(&groups, mode, 2, &[]), None);
    }
}

#[test]
fn issue_133_completed_selection_reveals_real_projected_list_and_grid() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.show().unwrap();
    for mode in [
        ViewMode::Details,
        ViewMode::List,
        ViewMode::MediumIcons,
        ViewMode::Tiles,
    ] {
        for grouping in [GroupField::None, GroupField::Kind] {
            let root = PathBuf::from(r"C:\copies");
            let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
            app.update_directory_preference(root.clone(), |pref| {
                pref.view_mode = mode;
                pref.group_field = grouping;
            });
            let window_id = app.active_window;
            let tab_id = app.active().id;
            let tab = app.tab_mut(tab_id).unwrap();
            tab.replace_entries(
                (1..=120)
                    .map(|id| entry(id, root.join(format!("{id:03}.txt"))))
                    .collect(),
            );
            tab.load_state = LoadState::Complete;
            tab.selected = vec![EntryId(118), EntryId(2), EntryId(116)];
            let request_id = tab.latest_request;
            focus_bottom_selected_entry(&mut app, tab_id);
            let state = Arc::new(Mutex::new(app));
            refresh_ui_inner(&ui, &state, window_id);
            use i_slint_backend_testing::ElementRoot;
            let _ = ui.root_element().query_descendants().find_all();
            reveal_completed_selection(&ui, &state, tab_id, request_id);
            let app = state.lock().unwrap();
            let tab = app.active();
            let groups = directory_group_projections(&app, tab, &tab.entries);
            let (_, bottom) = selected_entry_span(
                &groups,
                mode,
                ui.get_grid_column_count() as usize,
                &tab.selected,
            )
            .unwrap();
            let viewport = ui.get_file_viewport_height();
            let y = ui.get_file_viewport_y();
            assert!(
                bottom + y <= viewport + 1.0,
                "{mode:?}/{grouping:?}: bottom {bottom} y {y} viewport {viewport}"
            );
            assert!(bottom + y - file_layout_geometry(mode).row_height >= -1.0);
            assert_eq!(tab.focused, Some(EntryId(118)));
            let before = y;
            drop(app);
            reveal_completed_selection(&ui, &state, tab_id, RequestId(request_id.0 + 1));
            assert_eq!(ui.get_file_viewport_y(), before);
        }
    }
}

#[test]
fn issue_133_f5_still_clears_selection_and_old_completion_cannot_restore_it() {
    let root = PathBuf::from(r"C:\copies");
    let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active().id;
    let tab = app.tab_mut(tab_id).unwrap();
    tab.replace_entries(vec![entry(1, root.join("copied.txt"))]);
    tab.load_state = LoadState::Complete;
    tab.select_entry(EntryId(1), false, false);
    queue_completed_focus(&mut app, &[root.join("copied.txt")]);
    let state = Arc::new(Mutex::new(app));
    let (sender, requests) = mpsc::channel();
    assert!(submit_navigation(
        &sender,
        &state,
        tab_id,
        root.clone(),
        NavigationKind::Refresh,
        None
    ));
    let request = requests.try_recv().unwrap();
    apply_event(
        &state,
        DirectoryEvent::Batch {
            tab_id,
            request_id: request.request_id,
            entries: vec![
                entry(9, root.join("other.txt")),
                entry(1, root.join("copied.txt")),
            ],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id: request.request_id,
            path: root,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    let app = state.lock().unwrap();
    assert!(app.active().selected.is_empty());
    assert!(app.active().focused.is_none());
    assert!(app.focus_after_refresh.is_empty());
}

fn background_reorder_fixture() -> (AppWindow, WindowSessions, RequestId) {
    let root = PathBuf::from(r"C:\copies");
    let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
    app.update_directory_preference(root.clone(), |pref| {
        pref.view_mode = ViewMode::Details;
        pref.sort_field = SortField::Name;
    });
    let window_id = app.active_window;
    let tab_id = app.active().id;
    let tab = app.tab_mut(tab_id).unwrap();
    tab.latest_request = RequestId(10);
    tab.load_state = LoadState::Complete;
    tab.sort_field = SortField::Name;
    tab.replace_entries(
        (1..=120)
            .map(|id| {
                let extension = if id == 118 { "lnk" } else { "txt" };
                entry(id, root.join(format!("{id:03}.{extension}")))
            })
            .collect(),
    );
    tab.resort_entries();
    tab.select_entry(EntryId(118), false, false);
    let request_id = tab.latest_request;
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_window_ui(&ui, &state, window_id);
    ui.show().unwrap();
    use i_slint_backend_testing::ElementRoot;
    let _ = ui.root_element().query_descendants().find_all();
    reveal_completed_selection(&ui, &state, tab_id, request_id);
    (ui, state, request_id)
}

fn assert_bottom_selection_visible(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().unwrap();
    let tab = app.active();
    let mode = app.view_mode_for_tab(tab.id).unwrap();
    let groups = directory_group_projections(&app, tab, &tab.entries);
    let (_, bottom) = selected_entry_span(&groups, mode, 1, &tab.selected).unwrap();
    let bottom = bottom + ui.get_file_viewport_y();
    assert!(bottom <= ui.get_file_viewport_height() + 0.5);
    assert!(bottom - file_layout_geometry(mode).row_height >= -0.5);
}

fn folder_shortcut_event(request_id: RequestId) -> ShortcutEvent {
    ShortcutEvent {
        request: ShortcutRequest {
            tab_id: TabId(1),
            request_id,
            entry_id: EntryId(118),
            path: PathBuf::from(r"C:\copies\118.lnk"),
        },
        target: platform::ShortcutTarget {
            path: PathBuf::from(r"C:\destination"),
            is_directory: Some(true),
        },
    }
}

#[test]
fn issue_133_shortcut_reorder_keeps_visible_results_without_undoing_user_scroll() {
    i_slint_backend_testing::init_no_event_loop();
    for user_scrolled_away in [false, true] {
        let (ui, state, request_id) = background_reorder_fixture();
        if user_scrolled_away {
            ui.set_file_viewport_y(-30.0 * file_layout_geometry(ViewMode::Details).row_height);
        }
        let before_y = ui.get_file_viewport_y();
        let before =
            visible_selection_before_reorder(&ui, &state.lock().unwrap(), TabId(1), request_id);
        assert_eq!(before.is_some(), !user_scrolled_away);
        assert_eq!(
            apply_shortcut_event(&state, folder_shortcut_event(request_id)),
            Some((TabId(1), EntryId(118)))
        );
        refresh_window_ui(&ui, &state, state.window_id);
        restore_visible_selection_after_reorder(&ui, &state, before);
        assert_eq!(ui.get_files().row_data(0).unwrap().id, 118);
        assert!(ui.get_files().row_data(0).unwrap().is_directory);
        assert_eq!(state.lock().unwrap().active().selected, vec![EntryId(118)]);
        if user_scrolled_away {
            assert_eq!(ui.get_file_viewport_y(), before_y);
        } else {
            assert_bottom_selection_visible(&ui, &state);
            assert_ne!(ui.get_file_viewport_y(), before_y);
        }
    }
}

#[test]
fn issue_133_background_reveal_rejects_stale_request_and_changed_selection() {
    i_slint_backend_testing::init_no_event_loop();
    for request_changed in [false, true] {
        let (ui, state, request_id) = background_reorder_fixture();
        let before =
            visible_selection_before_reorder(&ui, &state.lock().unwrap(), TabId(1), request_id);
        assert!(before.is_some());
        {
            let mut app = state.lock().unwrap();
            let tab = app.tab_mut(TabId(1)).unwrap();
            if request_changed {
                tab.latest_request = RequestId(request_id.0 + 1);
            } else {
                tab.select_entry(EntryId(119), false, false);
            }
        }
        ui.set_file_viewport_y(0.0);
        restore_visible_selection_after_reorder(&ui, &state, before);
        assert_eq!(ui.get_file_viewport_y(), 0.0);
        if request_changed {
            assert!(apply_shortcut_event(&state, folder_shortcut_event(request_id)).is_none());
            assert_eq!(ui.get_file_viewport_y(), 0.0);
            assert!(
                visible_selection_before_reorder(&ui, &state.lock().unwrap(), TabId(1), request_id)
                    .is_none()
            );
        }
    }
}
