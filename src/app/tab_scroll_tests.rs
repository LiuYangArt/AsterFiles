//! #190: viewport ownership follows tabs, never the shared window projection.
use super::*;

fn fixture() -> (WindowSessions, AppWindow) {
    let first = PathBuf::from(r"C:\issue-190");
    let grid = PathBuf::from(r"C:\issue-190-grid");
    let mut app = AppState::new_for_test(
        vec![first.clone(), first.clone(), grid.clone()],
        0,
        [0, 1, 2, 3],
    );
    app.update_directory_preference(grid, |view| {
        view.view_mode = ViewMode::LargeIcons;
        view.group_field = GroupField::Kind;
    });
    for id in [TabId(1), TabId(2), TabId(3)] {
        let tab = app.tab_mut(id).unwrap();
        let root = tab.visible_path().unwrap().to_path_buf();
        tab.load_state = LoadState::Complete;
        tab.replace_entries(
            (1..=1200)
                .map(|entry| {
                    let name = format!("item-{entry:04}.txt");
                    FileEntry {
                        id: EntryId(entry),
                        original_name: name.clone().into(),
                        display_name: name.clone(),
                        path: root.join(name).into(),
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
                    }
                })
                .collect(),
        );
        tab.selected = [EntryId(5)].into_iter().collect();
        tab.focused = Some(EntryId(5));
    }
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &state);
    (state, ui)
}

fn switch(state: &WindowSessions, ui: &AppWindow, tab: TabId) {
    state.lock().unwrap().active_window_state_mut().active_tab = tab;
    refresh_ui(ui, state);
}

#[test]
fn issue_190_three_tabs_same_directory_and_mixed_layouts_restore_independently() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture();
    for (id, y) in [(TabId(1), -173.0), (TabId(2), -697.0), (TabId(3), -1321.0)] {
        switch(&state, &ui, id);
        ui.set_file_viewport_y(y);
    }
    for _ in 0..3 {
        for (id, y) in [(TabId(1), -173.0), (TabId(3), -1321.0), (TabId(2), -697.0)] {
            switch(&state, &ui, id);
            assert_eq!(ui.get_file_viewport_y(), y);
            let app = state.lock().unwrap();
            let tab = app.tab(id).unwrap();
            assert_eq!(tab.focused, Some(EntryId(5)));
            assert_eq!(tab.selected, [EntryId(5)]);
            assert_eq!(tab.latest_request(), RequestId(0));
        }
    }
    let settings = state.lock().unwrap().open_settings();
    refresh_ui(&ui, &state);
    assert_eq!(ui.get_projected_file_tab_id(), settings.0 as i32);
    switch(&state, &ui, TabId(1));
    assert_eq!(ui.get_file_viewport_y(), -173.0);
    state.lock().unwrap().create_tab(NavigationLocation::Home);
    refresh_ui(&ui, &state);
    switch(&state, &ui, TabId(2));
    assert_eq!(ui.get_file_viewport_y(), -697.0);
}

#[test]
fn issue_190_search_restores_global_offset_not_page_window_offset() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture();
    ui.set_file_viewport_y(-291.0);
    {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(2)).unwrap();
        tab.begin_search(SearchScope::Global, "item".to_owned());
        tab.search_total = Some(50_000);
        tab.search_state = SearchState::Complete;
    }
    switch(&state, &ui, TabId(2));
    let logical = search_scroll_for_index(12_345, ViewMode::Details, 1) - 7.0;
    ui.set_search_scroll_y(logical);
    let window = search_window_for_scroll(logical, 50_000, ViewMode::Details, 1);
    let local = search_window_viewport_y(logical, window, ViewMode::Details, 1);
    ui.set_file_viewport_y(local);
    assert_ne!(logical, local);
    switch(&state, &ui, TabId(1));
    assert_eq!(ui.get_file_viewport_y(), -291.0);
    switch(&state, &ui, TabId(2));
    assert_eq!(ui.get_search_scroll_y(), logical);
    assert_eq!(ui.get_file_viewport_y(), local);
    refresh_regions(&ui, &state, RefreshRegions::CONTENT);
    assert_eq!(ui.get_search_scroll_y(), logical);
    assert_eq!(ui.get_file_viewport_y(), local);
}

#[test]
fn issue_190_late_zero_search_page_keeps_restored_logical_position() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture();
    ui.set_file_viewport_y(-291.0);
    let request = {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(2)).unwrap();
        let (request, _) = tab.begin_search(SearchScope::Global, "item".to_owned());
        tab.search_total = Some(50_000);
        tab.search_state = SearchState::Partial;
        request
    };
    switch(&state, &ui, TabId(2));
    // Returning near page one can prefetch page zero after it has left the seven-page cache.
    let logical = search_scroll_for_index(400, ViewMode::Details, 1) - 7.0;
    let window = search_window_for_scroll(logical, 50_000, ViewMode::Details, 1);
    let local = search_window_viewport_y(logical, window, ViewMode::Details, 1);
    ui.set_search_scroll_y(logical);
    ui.set_file_viewport_y(local);
    switch(&state, &ui, TabId(1));
    project_search_page(&ui, &state, TabId(2), request, 50_000);
    assert_eq!(ui.get_file_viewport_y(), -291.0);
    switch(&state, &ui, TabId(2));
    assert_eq!(ui.get_search_scroll_y(), logical);
    assert_eq!(ui.get_file_viewport_y(), local);
    project_search_page(&ui, &state, TabId(2), request, 50_000);
    assert_eq!(ui.get_search_scroll_y(), logical);
    assert_eq!(ui.get_file_viewport_y(), local);
    // The fixture has no native window registry; run the production refresh explicitly too.
    refresh_regions(&ui, &state, RefreshRegions::CONTENT);
    switch(&state, &ui, TabId(1));
    switch(&state, &ui, TabId(2));
    assert_eq!(ui.get_search_scroll_y(), logical);
    assert_eq!(ui.get_file_viewport_y(), local);
}

#[test]
fn issue_190_shorter_content_clamps_and_stale_events_cannot_replace_viewport() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture();
    ui.set_file_viewport_y(-2000.0);
    switch(&state, &ui, TabId(2));
    ui.set_file_viewport_y(-377.0);
    let old_request = {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(1)).unwrap();
        let old = tab.latest_request();
        let location = tab.current_location().clone().unwrap();
        tab.begin_navigation(location.clone(), NavigationKind::BackgroundRefresh);
        tab.accept_directory_batch(
            tab.latest_request(),
            Vec::new(),
            DirectoryViewPreference::default(),
        );
        tab.finish_directory_navigation(
            tab.latest_request(),
            location,
            DirectoryViewPreference::default(),
            None,
        );
        old
    };
    let stale = apply_event(
        &state,
        DirectoryEvent::Failed {
            tab_id: TabId(1),
            request_id: old_request,
            kind: io::ErrorKind::PermissionDenied,
            message: "stale".to_owned(),
        },
    );
    assert!(!stale.accepted);
    refresh_regions(&ui, &state, RefreshRegions::TABS);
    assert_eq!(ui.get_file_viewport_y(), -377.0);
    switch(&state, &ui, TabId(1));
    assert_eq!(ui.get_file_viewport_y(), 0.0);
    switch(&state, &ui, TabId(2));
    assert_eq!(ui.get_file_viewport_y(), -377.0);
}

#[test]
fn issue_190_navigation_invalidates_saved_viewport_but_background_refresh_retains_it() {
    let path = NavigationLocation::Directory(PathBuf::from(r"C:\issue-190"));
    let mut tab = TabSession::new_at(TabId(1), path.clone());
    tab.load_state = LoadState::Complete;
    assert!(tab.remember_viewport(RequestId(0), -100.0, -10000.0));
    let (background, _) = tab.begin_navigation(path.clone(), NavigationKind::BackgroundRefresh);
    assert_eq!(tab.viewport(), (-100.0, -10000.0));
    assert!(tab.remember_viewport(RequestId(0), -200.0, -20000.0));
    let (navigation, _) = tab.begin_navigation(path, NavigationKind::Normal);
    assert_eq!(tab.viewport(), (0.0, 0.0));
    assert!(!tab.remember_viewport(background, -800.0, -90000.0));
    assert_eq!(tab.viewport(), (0.0, 0.0));
    assert!(tab.remember_viewport(navigation, -300.0, -30000.0));
    tab.begin_search(SearchScope::Global, "new".to_owned());
    assert_eq!(tab.viewport(), (0.0, 0.0));
}

#[test]
fn issue_190_all_layouts_and_grouping_restore_without_restarting_requests() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in ViewMode::CTRL_WHEEL_ORDER {
        for group in [GroupField::None, GroupField::Kind] {
            let (state, ui) = fixture();
            state.lock().unwrap().update_directory_preference(
                PathBuf::from(r"C:\issue-190"),
                |view| {
                    view.view_mode = mode;
                    view.group_field = group;
                },
            );
            refresh_ui(&ui, &state);
            ui.set_file_viewport_y(-513.0);
            switch(&state, &ui, TabId(2));
            ui.set_file_viewport_y(-911.0);
            switch(&state, &ui, TabId(1));
            assert_eq!(ui.get_file_viewport_y(), -513.0, "{mode:?} {group:?}");
            switch(&state, &ui, TabId(2));
            assert_eq!(ui.get_file_viewport_y(), -911.0, "{mode:?} {group:?}");
            assert_eq!(
                state
                    .lock()
                    .unwrap()
                    .tab(TabId(1))
                    .unwrap()
                    .latest_request(),
                RequestId(0)
            );
        }
    }
}
