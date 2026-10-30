//! #162: unrelated window updates must not project a directory again.
use super::*;

fn fixture(count: u32, mode: ViewMode, group: GroupField) -> (WindowSessions, AppWindow) {
    let root = PathBuf::from(r"C:\issue-162");
    let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
    app.update_directory_preference(root.clone(), |view| {
        view.view_mode = mode;
        view.group_field = group;
    });
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.load_state = LoadState::Complete;
    tab.replace_entries(
        (1..=count)
            .map(|id| {
                let name = format!("item-{id:06}.txt");
                FileEntry {
                    id: EntryId(id),
                    original_name: name.clone().into(),
                    display_name: name.clone(),
                    path: root.join(name),
                    name_highlights: Vec::new(),
                    kind: crate::domain::EntryKind::File,
                    open_target: None,
                    library_source_index: None,
                    parent_display: String::new(),
                    size_bytes: Some(u64::from(id)),
                    folder_size: FolderSizeState::NotIndexed,
                    modified: None,
                    created: None,
                    webdav: None,
                }
            })
            .collect(),
    );
    tab.resort_entries();
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &state);
    assert!(!ui.get_text_settings().is_empty());
    assert!(ui.get_sidebar_items().row_count() > 0);
    (state, ui)
}

#[test]
fn issue_162_resize_performance() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(100_000, ViewMode::Details, GroupField::None);
    ui.set_file_viewport_y(-1200.0);
    let files = ui.get_files();
    let grid = ui.get_grid_rows();
    let mut timings = Vec::new();
    for width in [1181.0, 1182.0, 1183.0, 1184.0, 1185.0] {
        ui.window().set_size(slint::LogicalSize::new(width, 760.0));
        let start = Instant::now();
        reflow_after_window_resize(&ui, &state, WindowId(1));
        timings.push(start.elapsed().as_secs_f64() * 1000.0);
        assert_eq!(ui.get_file_viewport_y(), -1200.0);
        assert_models_unchanged(&ui, &files, &grid);
    }
    let mean = timings.iter().sum::<f64>() / timings.len() as f64;
    let peak = timings.iter().copied().fold(0.0, f64::max);
    let output = PathBuf::from("artifacts/perf/issue-162");
    std::fs::create_dir_all(&output).unwrap();
    let json = format!(
        "{{\"entries\":100000,\"resize_mean_ms\":{mean:.6},\"resize_peak_ms\":{peak:.6},\"samples_ms\":{timings:?}}}\n"
    );
    std::fs::write(output.join("latest.json"), &json).unwrap();
    eprintln!("#162 resize: {json}");
    assert!(peak < 16.0, "resize must fit one frame: {peak:.3} ms");
}

fn assert_models_unchanged(ui: &AppWindow, files: &ModelRc<FileRow>, grid: &ModelRc<GridRow>) {
    assert!(
        std::ptr::eq(files.as_any(), ui.get_files().as_any()),
        "file model changed"
    );
    assert!(
        std::ptr::eq(grid.as_any(), ui.get_grid_rows().as_any()),
        "grid model changed"
    );
}

fn row_for(ui: &AppWindow, id: EntryId) -> FileRow {
    ui.get_files()
        .iter()
        .chain(
            ui.get_grid_rows()
                .iter()
                .flat_map(|row| row.entries.iter().collect::<Vec<_>>()),
        )
        .find(|row| row.id == id.0 as i32)
        .expect("projected entry")
}

fn begin_rename(ui: &AppWindow, state: &WindowSessions, id: EntryId) {
    let request_id = state
        .lock()
        .unwrap()
        .tab(TabId(1))
        .unwrap()
        .latest_request();
    state.lock().unwrap().rename_targets.insert(
        WindowId(1),
        actions::EntryTarget {
            tab: actions::ActionTarget {
                window_id: WindowId(1),
                tab_id: TabId(1),
                request_id,
            },
            entry_id: id,
        },
    );
    ui.set_rename_entry_id(id.0 as i32);
    ui.set_rename_input("editing-name.txt".into());
    ui.set_rename_editing(true);
}

#[test]
fn issue_162_non_content_regions_preserve_all_view_models_and_edits() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in ViewMode::CTRL_WHEEL_ORDER {
        for group in [GroupField::None, GroupField::Kind] {
            let (state, ui) = fixture(120, mode, group);
            let selected = EntryId(40);
            let (_, changed) =
                mutate_active_selection(&state, |tab| tab.select_entry(selected, false, false))
                    .unwrap();
            update_file_rows(&ui, &state, TabId(1), &changed);
            begin_rename(&ui, &state, selected);
            ui.set_file_viewport_y(-120.0);
            let files = ui.get_files();
            let grid = ui.get_grid_rows();
            let nested = grid
                .iter()
                .find(|row| !row.group_header)
                .map(|row| row.entries);
            let grouped_layout = ui.get_grouped_grid_layout();
            let interrupted = Rc::new(std::cell::Cell::new(false));
            let interrupted_for_callback = interrupted.clone();
            ui.on_cancel_internal_drag(move || interrupted_for_callback.set(true));
            let columns = ui.get_grid_column_count();
            ui.window().set_size(slint::LogicalSize::new(1180.1, 760.0));
            assert_eq!(grid_column_count_for_view(&ui, mode) as i32, columns);
            reflow_after_window_resize(&ui, &state, WindowId(1));
            assert_models_unchanged(&ui, &files, &grid);
            assert_eq!(ui.get_file_viewport_y(), -120.0);
            {
                let mut app = state.lock().unwrap();
                app.theme_mode = session_store::ThemeMode::Dark;
                app.file_list_quick_search = true;
                app.everything.status = "issue-162-ready".to_owned();
                app.sidebar.push(KnownLocation {
                    kind: KnownLocationKind::Pinned,
                    label: "issue-162-pin".to_owned(),
                    path: PathBuf::from(r"C:\issue-162-pin"),
                    drive_root: None,
                });
                app.operation_notice = Some((162, "issue-162-notice".to_owned()));
            }
            refresh_regions(&ui, &state, RefreshRegions::SETTINGS);
            assert!(ui.get_dark_theme());
            assert!(ui.get_file_list_quick_search());
            assert_eq!(ui.get_everything_status(), "issue-162-ready");
            assert_models_unchanged(&ui, &files, &grid);
            refresh_regions(&ui, &state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
            assert!(
                ui.get_sidebar_items()
                    .iter()
                    .any(|row| row.label == "issue-162-pin")
            );
            assert_models_unchanged(&ui, &files, &grid);
            refresh_regions(
                &ui,
                &state,
                RefreshRegions::STATUS | RefreshRegions::COLUMNS | RefreshRegions::TABS,
            );
            assert_eq!(ui.get_operation_error(), "issue-162-notice");
            assert_eq!(ui.get_selected_count(), 1);
            assert!(row_for(&ui, selected).selected);
            assert!(row_for(&ui, selected).focused);
            assert_models_unchanged(&ui, &files, &grid);
            if let Some(nested) = nested {
                let current = ui
                    .get_grid_rows()
                    .iter()
                    .find(|row| !row.group_header)
                    .unwrap()
                    .entries;
                assert!(std::ptr::eq(nested.as_any(), current.as_any()));
            }
            assert!(std::ptr::eq(
                grouped_layout.as_any(),
                ui.get_grouped_grid_layout().as_any()
            ));
            assert!(
                !interrupted.get(),
                "{mode:?}/{group:?}: unrelated refresh cancelled a gesture"
            );
            assert!(ui.get_rename_editing());
            assert_eq!(ui.get_rename_entry_id(), selected.0 as i32);
            assert_eq!(ui.get_rename_input(), "editing-name.txt");
            assert_eq!(ui.get_file_viewport_y(), -120.0);
        }
    }
}

#[test]
fn issue_162_task_progress_projects_without_replacing_file_models() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(20, ViewMode::Details, GroupField::None);
    let files = ui.get_files();
    let grid = ui.get_grid_rows();
    let id = {
        let mut app = state.lock().unwrap();
        let id = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            Some(TabId(1)),
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );
        app.operations.start_next(OperationResource::Local).unwrap();
        app.operations.mark_running(id).unwrap();
        app.operations.task_mut(id).unwrap().progress.total_bytes = Some(8192);
        app.operations
            .task_mut(id)
            .unwrap()
            .progress
            .scanning_complete = true;
        apply_progress_event_for_test(&mut app, id, 4096, 12);
        id
    };
    let operation_ui = OperationWindow::new().unwrap();
    refresh_operation_progress(&operation_ui, &state);
    refresh_regions(&ui, &state, RefreshRegions::STATUS);
    let row = ui
        .get_operations()
        .iter()
        .find(|row| row.id == id.0 as i32)
        .unwrap();
    assert_eq!(row.state, operation_state_index(OperationState::Running));
    assert_eq!(row.percent, "50%");
    assert!(!row.transferred.is_empty());
    assert_eq!(row.progress, 0.5);
    assert!(
        operation_ui
            .get_operations()
            .iter()
            .any(|row| row.id == id.0 as i32 && row.progress == 0.5)
    );
    assert_models_unchanged(&ui, &files, &grid);
}

#[test]
fn issue_162_changed_grid_columns_preserve_anchor_selection_and_rename() {
    i_slint_backend_testing::init_no_event_loop();
    for group in [GroupField::None, GroupField::Kind] {
        let (state, ui) = fixture(200, ViewMode::MediumIcons, group);
        let selected = EntryId(40);
        let (_, changed) =
            mutate_active_selection(&state, |tab| tab.select_entry(selected, false, false))
                .unwrap();
        update_file_rows(&ui, &state, TabId(1), &changed);
        begin_rename(&ui, &state, selected);
        ui.set_file_viewport_y(-300.0);
        let anchor = capture_zoom_anchor(&ui, 16.0, 0.0).unwrap();
        let columns = ui.get_grid_column_count();
        let grid = ui.get_grid_rows();
        let request = ui.get_projected_file_request_id();
        ui.window().set_size(slint::LogicalSize::new(1500.0, 760.0));
        reflow_after_window_resize(&ui, &state, WindowId(1));
        assert_ne!(ui.get_grid_column_count(), columns);
        assert!(!std::ptr::eq(grid.as_any(), ui.get_grid_rows().as_any()));
        assert!(
            (projected_entry_top(&ui, anchor.entry_id).unwrap() + ui.get_file_viewport_y()
                - anchor.screen_y)
                .abs()
                < 0.1
        );
        assert!(row_for(&ui, selected).selected);
        assert!(row_for(&ui, selected).focused);
        assert!(ui.get_rename_editing());
        assert_eq!(ui.get_rename_input(), "editing-name.txt");
        assert_eq!(ui.get_projected_file_request_id(), request);
    }
}

#[test]
fn issue_162_background_tab_and_stale_request_leave_active_projection_untouched() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(30, ViewMode::Details, GroupField::None);
    let files = ui.get_files();
    let grid = ui.get_grid_rows();
    let projected_request = ui.get_projected_file_request_id();
    let background = {
        let mut app = state.lock().unwrap();
        let location = NavigationLocation::Directory(PathBuf::from(r"C:\issue-162-background"));
        let background = app.create_tab(location.clone());
        app.tab_mut(background)
            .unwrap()
            .begin_navigation(location, NavigationKind::Normal);
        app.active_window_state_mut().active_tab = TabId(1);
        background
    };
    refresh_regions(&ui, &state, RefreshRegions::TABS);
    assert!(
        ui.get_tabs()
            .iter()
            .any(|row| row.id == background.0 as i32 && row.loading)
    );
    assert_eq!(ui.get_projected_file_tab_id(), 1);
    assert_eq!(ui.get_projected_file_request_id(), projected_request);
    assert_models_unchanged(&ui, &files, &grid);
    let old_request = state
        .lock()
        .unwrap()
        .tab(TabId(1))
        .unwrap()
        .latest_request();
    {
        let mut app = state.lock().unwrap();
        app.tab_mut(TabId(1)).unwrap().begin_navigation(
            NavigationLocation::Directory(PathBuf::from(r"C:\issue-162-new")),
            NavigationKind::Normal,
        );
    }
    let stale = apply_event(
        &state,
        DirectoryEvent::Failed {
            tab_id: TabId(1),
            request_id: old_request,
            kind: io::ErrorKind::PermissionDenied,
            message: "old request".to_owned(),
        },
    );
    assert!(!stale.accepted);
    update_file_rows(&ui, &state, TabId(1), &HashSet::from([EntryId(1)]));
    assert_models_unchanged(&ui, &files, &grid);
    refresh_regions(&ui, &state, RefreshRegions::CONTENT);
    assert_ne!(ui.get_projected_file_request_id(), projected_request);
    assert!(ui.get_files().iter().all(|row| !row.loaded));
}

#[test]
fn issue_162_selection_summary_uses_owner_instead_of_global_active_window() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(20, ViewMode::Details, GroupField::None);
    let files = ui.get_files();
    let grid = ui.get_grid_rows();
    let other_window = {
        let mut app = state.shared.lock().unwrap();
        app.tab_mut(TabId(1))
            .unwrap()
            .select_entry(EntryId(2), false, false);
        app.tab_mut(TabId(1))
            .unwrap()
            .select_entry(EntryId(4), true, false);
        app.update_directory_preference(PathBuf::from(r"C:\issue-162"), |view| {
            view.columns.widths[ColumnKind::Name.storage_code() as usize] = 370;
        });
        let other = app.register_window(
            vec![NavigationLocation::Directory(PathBuf::from(
                r"C:\issue-162-other",
            ))],
            0,
            test_support::test_window_placement(1500),
        );
        app.update_directory_preference(PathBuf::from(r"C:\issue-162-other"), |view| {
            view.columns.widths[ColumnKind::Name.storage_code() as usize] = 617;
        });
        app.active_window = other;
        other
    };
    update_selection_summary(&ui, &state);
    assert_eq!(ui.get_selected_count(), 2);
    assert_eq!(ui.get_normal_name_width(), 370.0);
    assert!(ui.get_context_menu_has_entry());
    assert_models_unchanged(&ui, &files, &grid);
    assert_eq!(state.shared.lock().unwrap().active_window, other_window);
}

#[test]
fn issue_162_view_refresh_updates_headless_owner_without_registered_runtime() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(40, ViewMode::Details, GroupField::None);
    let files = ui.get_files();
    let request_id = ui.get_projected_file_request_id();
    assert!(window_ui(state.window_id).is_none());
    set_view_mode(&state, ViewMode::MediumIcons);
    refresh_view_windows(&state, Some(&ui));
    assert_eq!(ui.get_view_mode(), view_mode_to_ui(ViewMode::MediumIcons));
    assert!(!std::ptr::eq(files.as_any(), ui.get_files().as_any()));
    assert_eq!(
        ui.get_grid_rows()
            .iter()
            .map(|row| row.entries.row_count())
            .sum::<usize>(),
        40
    );
    assert_eq!(ui.get_projected_file_tab_id(), 1);
    assert_eq!(ui.get_projected_file_request_id(), request_id);
}
