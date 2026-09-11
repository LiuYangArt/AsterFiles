use super::*;
use slint::{ComponentHandle, Model};

fn fixture(mode: ViewMode) -> (AppWindow, WindowSessions) {
    i_slint_backend_testing::init_no_event_loop();
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\group")], 0, [0, 1, 2, 3]);
    app.update_directory_preference(PathBuf::from(r"C:\group"), |preference| {
        preference.view_mode = mode;
        preference.group_field = GroupField::Kind;
    });
    let entries = (1..=1200)
        .map(|id| {
            let name = format!("file{id}.{}", ["obj", "txt", "png", "mtl"][id as usize % 4]);
            FileEntry {
                id: EntryId(id),
                original_name: name.clone().into(),
                display_name: name.clone(),
                name_highlights: Vec::new(),
                path: PathBuf::from(r"C:\group").join(name),
                kind: crate::domain::EntryKind::File,
                open_target: None,
                library_source_index: None,
                parent_display: String::new(),
                size_bytes: Some(1),
                folder_size: FolderSizeState::Unknown,
                modified: None,
                created: None,
            }
        })
        .collect();
    app.tab_mut(TabId(1)).unwrap().replace_entries(entries);
    let window = app.active_window;
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), window);
    let ui = AppWindow::new().unwrap();
    let weak = ui.as_weak();
    ui.on_refresh_grouped_grid_viewport(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_grouped_grid_viewport(&ui);
        }
    });
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &state);
    ui.show().unwrap();
    settle(&ui);
    (ui, state)
}

fn settle(ui: &AppWindow) {
    use i_slint_backend_testing::ElementRoot;
    for _ in 0..3 {
        ui.window().request_redraw();
        let _ = ui.root_element().query_descendants().find_all();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(16));
    }
}

#[test]
fn issue_91_scoped_grid_keeps_exact_bottom_after_reprojection_and_resize() {
    let (ui, state) = fixture(ViewMode::MediumIcons);
    assert!(ui.get_grouped_grid_enabled());
    let full_count = ui.get_grid_rows().row_count();
    let expected = ui
        .get_grid_rows()
        .iter()
        .map(|row| if row.group_header { 32.0 } else { 132.0 })
        .sum::<f32>();
    assert_eq!(ui.get_grouped_grid_extent(), expected);
    ui.set_file_viewport_y(-1000000.0);
    settle(&ui);
    assert_eq!(
        ui.get_file_viewport_y(),
        -(expected - ui.get_file_viewport_height())
    );
    let visible = ui.get_grouped_grid_visible_rows();
    assert!(visible.row_count() < 30);
    assert!(visible.row_data(0).unwrap().offset > 0.0);
    assert_eq!(ui.get_grid_rows().row_count(), full_count);
    refresh_ui(&ui, &state);
    settle(&ui);
    assert_eq!(
        ui.get_file_viewport_y(),
        -(expected - ui.get_file_viewport_height())
    );
    ui.window().set_size(slint::LogicalSize::new(1180.0, 960.0));
    settle(&ui);
    assert_eq!(
        ui.get_file_viewport_y(),
        -(expected - ui.get_file_viewport_height())
    );
}

#[test]
fn issue_91_selection_updates_preserve_positioned_rows() {
    let (ui, state) = fixture(ViewMode::LargeIcons);
    ui.set_file_viewport_y(-5000.0);
    settle(&ui);
    let placed = ui
        .get_grouped_grid_visible_rows()
        .iter()
        .find(|row| !row.row.group_header)
        .unwrap();
    let entry = placed.row.entries.row_data(0).unwrap();
    let id = EntryId(entry.id as u32);
    {
        let mut app = state.lock().unwrap();
        app.tab_mut(TabId(1))
            .unwrap()
            .select_entry(id, false, false);
    }
    update_file_rows(&ui, &state.shared, TabId(1), &HashSet::from([id]));
    settle(&ui);
    assert!(placed.row.entries.row_data(0).unwrap().selected);
    let after = ui
        .get_grouped_grid_visible_rows()
        .iter()
        .find(|row| row.offset == placed.offset)
        .unwrap();
    assert_eq!(after.extent, placed.extent);
    assert_eq!(after.row.entries.row_data(0).unwrap().id, entry.id);
    assert_eq!(ui.get_file_viewport_y(), -5000.0);
}

#[test]
fn issue_91_scope_excludes_ungrouped_list_content_search_and_library() {
    let (ui, state) = fixture(ViewMode::Tiles);
    for mode in [
        ViewMode::SmallIcons,
        ViewMode::MediumIcons,
        ViewMode::LargeIcons,
        ViewMode::ExtraLargeIcons,
        ViewMode::Tiles,
    ] {
        state
            .lock()
            .unwrap()
            .update_directory_preference(PathBuf::from(r"C:\group"), |p| p.view_mode = mode);
        refresh_ui(&ui, &state);
        settle(&ui);
        assert!(ui.get_grouped_grid_enabled());
    }
    for mode in [ViewMode::Details, ViewMode::List, ViewMode::Content] {
        state
            .lock()
            .unwrap()
            .update_directory_preference(PathBuf::from(r"C:\group"), |p| p.view_mode = mode);
        refresh_ui(&ui, &state);
        settle(&ui);
        assert!(!ui.get_grouped_grid_enabled());
        assert_eq!(ui.get_grouped_grid_visible_rows().row_count(), 0);
    }
    state
        .lock()
        .unwrap()
        .update_directory_preference(PathBuf::from(r"C:\group"), |p| {
            p.view_mode = ViewMode::MediumIcons;
            p.group_field = GroupField::None;
        });
    refresh_ui(&ui, &state);
    settle(&ui);
    assert!(!ui.get_grouped_grid_enabled());
    state.lock().unwrap().tab_mut(TabId(1)).unwrap().page_source = PageSource::Search;
    refresh_ui(&ui, &state);
    settle(&ui);
    assert!(!ui.get_grouped_grid_enabled());
}

#[test]
fn issue_91_wheel_uses_exact_range_and_preserves_hit_identity() {
    let (ui, state) = fixture(ViewMode::MediumIcons);
    for _ in 0..1000 {
        apply_file_scroll_delta(&ui, -120.0);
    }
    settle(&ui);
    let bottom = -(ui.get_grouped_grid_extent() - ui.get_file_viewport_height());
    assert_eq!(ui.get_file_viewport_y(), bottom);
    apply_file_scroll_delta(&ui, -120.0);
    settle(&ui);
    assert_eq!(ui.get_file_viewport_y(), bottom);
    ui.set_file_viewport_y(0.0);
    settle(&ui);
    apply_file_scroll_delta(&ui, -10.0);
    settle(&ui);
    assert_eq!(ui.get_file_viewport_y(), -10.0);
    ui.set_file_viewport_y(-5000.0);
    settle(&ui);
    let row = ui
        .get_grouped_grid_visible_rows()
        .iter()
        .find(|row| !row.row.group_header && row.offset >= 5000.0)
        .unwrap();
    let app = state.lock().unwrap();
    let hit = directory_entry_at_visual_point(
        &app,
        app.active(),
        ViewMode::MediumIcons,
        ui.get_grid_column_count() as usize,
        20.0,
        row.offset + 10.0,
    );
    assert_eq!(
        hit,
        Some(EntryId(row.row.entries.row_data(0).unwrap().id as u32))
    );
}

#[test]
fn issue_91_library_does_not_enable_scoped_layout() {
    let (ui, state) = fixture(ViewMode::MediumIcons);
    {
        let mut app = state.lock().unwrap();
        app.tab_mut(TabId(1)).unwrap().current_location = Some(NavigationLocation::Library(
            crate::domain::LibraryLocationId::new("shell:test-library".into(), "Test".into()),
        ));
    }
    refresh_ui(&ui, &state);
    settle(&ui);
    assert!(!ui.get_grouped_grid_enabled());
    assert_eq!(ui.get_grouped_grid_visible_rows().row_count(), 0);
}

#[test]
fn issue_91_batches_and_shorter_content_keep_full_data_and_bounded_nodes() {
    let (ui, state) = fixture(ViewMode::MediumIcons);
    ui.set_file_viewport_y(-5000.0);
    settle(&ui);
    let original_count = ui
        .get_grid_rows()
        .iter()
        .map(|row| row.entries.row_count())
        .sum::<usize>();
    assert_eq!(original_count, 1200);
    {
        let mut app = state.lock().unwrap();
        let mut entries = app.active().visible_entries().to_vec();
        entries.truncate(26);
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.pending_entries = entries;
        tab.load_state = LoadState::Partial;
    }
    refresh_ui(&ui, &state);
    settle(&ui);
    assert_eq!(
        ui.get_grid_rows()
            .iter()
            .map(|row| row.entries.row_count())
            .sum::<usize>(),
        26
    );
    assert!(
        ui.get_file_viewport_y()
            >= -(ui.get_grouped_grid_extent() - ui.get_file_viewport_height()).max(0.0)
    );
    {
        let mut app = state.lock().unwrap();
        let entries = app.active().entries[..300].to_vec();
        app.tab_mut(TabId(1)).unwrap().pending_entries = entries;
    }
    refresh_ui(&ui, &state);
    settle(&ui);
    assert_eq!(
        ui.get_grid_rows()
            .iter()
            .map(|row| row.entries.row_count())
            .sum::<usize>(),
        300
    );
    assert!(ui.get_grouped_grid_visible_rows().row_count() < ui.get_grid_rows().row_count());
    assert!(ui.get_file_viewport_height() > 0.0);
    ui.window().set_size(slint::LogicalSize::new(900.0, 760.0));
    refresh_ui(&ui, &state);
    settle(&ui);
    assert!(ui.get_file_viewport_width() > 0.0);
    assert_eq!(ui.get_grid_column_count(), 4);
}
