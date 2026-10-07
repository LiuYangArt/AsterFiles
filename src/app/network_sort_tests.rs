use super::*;
use crate::domain::EntryKind;
use slint::ComponentHandle;
use std::time::UNIX_EPOCH;

const NETWORK_PATH: &str = r"\\issue-141\share";

fn entry(id: u32, name: &str) -> FileEntry {
    let path = PathBuf::from(NETWORK_PATH).join(name);
    FileEntry {
        id: EntryId(id),
        original_name: path.file_name().unwrap().to_owned(),
        display_name: name.to_owned(),
        name_highlights: Vec::new(),
        path,
        kind: EntryKind::File,
        open_target: None,
        library_source_index: None,
        parent_display: NETWORK_PATH.to_owned(),
        size_bytes: Some(u64::from(id)),
        folder_size: FolderSizeState::NotIndexed,
        modified: None,
        created: None,
    }
}

fn sample_entries() -> Vec<FileEntry> {
    [
        (1, "z.txt", false, 10, 30, 20),
        (2, "a.png", false, 30, 10, 40),
        (3, "folder-b", true, 0, 20, 10),
        (4, "folder-a", true, 0, 40, 30),
        (5, "m.txt", false, 20, 50, 50),
    ]
    .into_iter()
    .map(|(id, name, directory, size, modified, created)| {
        let mut entry = entry(id, name);
        if directory {
            entry.kind = EntryKind::Directory;
            entry.size_bytes = None;
        } else {
            entry.size_bytes = Some(size);
        }
        entry.modified = Some(UNIX_EPOCH + Duration::from_secs(modified));
        entry.created = Some(UNIX_EPOCH + Duration::from_secs(created));
        entry
    })
    .collect()
}

fn fixture(field: SortField, direction: SortDirection) -> (WindowSessions, RequestId) {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    app.update_directory_preference(PathBuf::from(NETWORK_PATH), |preference| {
        preference.sort_field = field;
        preference.sort_direction = direction;
        preference.view_mode = ViewMode::Details;
        preference.group_field = GroupField::None;
    });
    let window_id = app.active_window;
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.sort_field = SortField::Name;
    tab.sort_direction = match direction {
        SortDirection::Ascending => SortDirection::Descending,
        SortDirection::Descending => SortDirection::Ascending,
    };
    let (request_id, _) =
        tab.begin_directory_navigation(PathBuf::from(NETWORK_PATH), NavigationKind::Normal);
    (
        WindowSessions::new(Arc::new(Mutex::new(app)), window_id),
        request_id,
    )
}

fn apply_network_batch(state: &SharedSessions, request_id: RequestId, entries: Vec<FileEntry>) {
    let (acknowledgement, applied) = mpsc::channel();
    apply_event(
        state,
        DirectoryEvent::Batch {
            tab_id: TabId(1),
            request_id,
            entries,
            acknowledgement,
        },
    );
    applied
        .recv_timeout(Duration::from_secs(1))
        .expect("applied or rejected batches must release their producer");
}

fn finish(state: &SharedSessions, request_id: RequestId) {
    apply_event(
        state,
        DirectoryEvent::Finished {
            tab_id: TabId(1),
            request_id,
            path: PathBuf::from(NETWORK_PATH),
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
}

fn ids(entries: &[FileEntry]) -> Vec<u32> {
    entries.iter().map(|entry| entry.id.0).collect()
}

fn assert_index_identity(tab: &TabSession) {
    for (index, entry) in tab.visible_entries().iter().enumerate() {
        assert_eq!(tab.visible_entry_index(entry.id), Some(index));
        assert_eq!(tab.visible_entry(entry.id).unwrap().path, entry.path);
        assert_eq!(
            entry.path.file_name(),
            Some(entry.original_name.as_os_str())
        );
    }
}

fn headless_model(request_id: RequestId) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.set_files(ModelRc::new(VecModel::<FileRow>::default()));
    ui.set_projected_file_tab_id(1);
    ui.set_projected_file_request_id(request_id.0 as i32);
    ui
}

fn list_ids(ui: &AppWindow) -> Vec<u32> {
    ui.get_files()
        .iter()
        .filter(|row| row.loaded && !row.group_header)
        .map(|row| row.id as u32)
        .collect()
}

#[test]
fn issue_141_each_batch_and_completion_follow_target_sort_preference() {
    use SortDirection::{Ascending, Descending};
    use SortField::{Created, Kind, Modified, Name, Size};
    let cases = [
        (Name, Ascending, [4, 3, 2, 5, 1]),
        (Name, Descending, [3, 4, 1, 5, 2]),
        (Kind, Ascending, [4, 3, 2, 5, 1]),
        (Kind, Descending, [5, 1, 2, 4, 3]),
        (Size, Ascending, [4, 3, 1, 5, 2]),
        (Size, Descending, [4, 3, 2, 5, 1]),
        (Modified, Ascending, [3, 4, 2, 1, 5]),
        (Modified, Descending, [4, 3, 5, 1, 2]),
        (Created, Ascending, [3, 4, 1, 2, 5]),
        (Created, Descending, [4, 3, 5, 2, 1]),
    ];
    for (field, direction, expected) in cases {
        let (state, request_id) = fixture(field, direction);
        let all = sample_entries();
        let first_ids = [1, 3, 5];
        apply_network_batch(
            &state.shared,
            request_id,
            all.iter()
                .filter(|entry| first_ids.contains(&entry.id.0))
                .cloned()
                .collect(),
        );
        {
            let mut app = state.lock().unwrap();
            let tab = app.tab_mut(TabId(1)).unwrap();
            assert_eq!(tab.load_state, LoadState::Partial);
            assert_eq!((tab.sort_field, tab.sort_direction), (field, direction));
            assert_eq!(
                ids(tab.visible_entries()),
                expected
                    .into_iter()
                    .filter(|id| first_ids.contains(id))
                    .collect::<Vec<_>>(),
                "first batch: {field:?} {direction:?}"
            );
            tab.select_entry(EntryId(5), false, false);
            assert_index_identity(tab);
        }
        apply_network_batch(
            &state.shared,
            request_id,
            all.into_iter()
                .filter(|entry| !first_ids.contains(&entry.id.0))
                .collect(),
        );
        {
            let app = state.lock().unwrap();
            let tab = app.active();
            assert_eq!(
                ids(tab.visible_entries()),
                expected,
                "second batch: {field:?} {direction:?}"
            );
            assert_eq!(tab.selected, [EntryId(5)]);
            assert_eq!(tab.focused, Some(EntryId(5)));
            assert_index_identity(tab);
        }
        finish(&state.shared, request_id);
        let app = state.lock().unwrap();
        let tab = app.active();
        assert_eq!(tab.load_state, LoadState::Complete);
        assert_eq!(
            ids(tab.visible_entries()),
            expected,
            "completion: {field:?} {direction:?}"
        );
        assert_eq!(tab.selected, [EntryId(5)]);
        assert_eq!(tab.focused, Some(EntryId(5)));
        assert_eq!(tab.selection_anchor, Some(EntryId(5)));
        assert_index_identity(tab);
    }
}

#[test]
fn issue_141_list_model_inserts_before_existing_rows_without_duplicates_or_scroll_jump() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in [ViewMode::Details, ViewMode::List, ViewMode::Content] {
        let (state, request_id) = fixture(SortField::Name, SortDirection::Ascending);
        state
            .lock()
            .unwrap()
            .update_directory_preference(PathBuf::from(NETWORK_PATH), |preference| {
                preference.view_mode = mode
            });
        let ui = headless_model(request_id);
        let original_model = ui.get_files();
        apply_network_batch(
            &state.shared,
            request_id,
            (1..=60)
                .map(|id| entry(id, &format!("item-{:03}.txt", id * 2)))
                .collect(),
        );
        append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
        assert_eq!(list_ids(&ui), (1..=60).collect::<Vec<_>>());
        let row_height = file_row_height(mode);
        ui.set_file_viewport_y(-20.0 * row_height - 7.0);
        apply_network_batch(
            &state.shared,
            request_id,
            vec![
                entry(61, "item-001.txt"),
                entry(62, "item-041.txt"),
                entry(63, "item-121.txt"),
            ],
        );
        append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
        let model_after = ui.get_files();
        assert!(std::ptr::eq(original_model.as_any(), model_after.as_any()));
        let expected = std::iter::once(61)
            .chain(1..=20)
            .chain(std::iter::once(62))
            .chain(21..=60)
            .chain(std::iter::once(63))
            .collect::<Vec<_>>();
        assert_eq!(list_ids(&ui), expected);
        assert_eq!(model_after.row_count(), 63);
        assert_eq!(ui.get_file_viewport_y(), -22.0 * row_height - 7.0);
        assert_eq!(model_after.row_data(22).unwrap().id, 21);
        append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
        assert_eq!(list_ids(&ui), expected);
        assert_eq!(ui.get_file_viewport_y(), -22.0 * row_height - 7.0);
        assert_index_identity(state.lock().unwrap().active());
    }
}

#[test]
fn issue_141_first_batch_updates_loading_state_and_sort_indicators_in_each_view() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in [
        ViewMode::Details,
        ViewMode::List,
        ViewMode::Content,
        ViewMode::MediumIcons,
        ViewMode::Tiles,
    ] {
        for group in [GroupField::None, GroupField::Kind] {
            let (state, request_id) = fixture(SortField::Modified, SortDirection::Descending);
            state.lock().unwrap().update_directory_preference(
                PathBuf::from(NETWORK_PATH),
                |preference| {
                    preference.view_mode = mode;
                    preference.group_field = group;
                },
            );
            let ui = headless_model(request_id);
            ui.set_view_mode(view_mode_to_ui(mode));
            ui.set_page_state(1);
            ui.set_sort_field(0);
            ui.set_sort_descending(false);
            apply_network_batch(&state.shared, request_id, sample_entries());
            append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
            assert_eq!(ui.get_page_state(), 2, "{mode:?}/{group:?}");
            assert_eq!(ui.get_sort_field(), 3);
            assert!(ui.get_sort_descending());
            let projected = if mode.uses_grid_layout() {
                ui.get_grid_rows()
                    .iter()
                    .filter(|row| !row.group_header)
                    .map(|row| row.entries.row_count())
                    .sum()
            } else {
                ui.get_files()
                    .iter()
                    .filter(|row| !row.group_header)
                    .count()
            };
            assert_eq!(projected, 5, "{mode:?}/{group:?}");
        }
    }
}

#[test]
fn issue_141_completion_keeps_only_selection_ids_in_the_final_directory() {
    let (state, request_id) = fixture(SortField::Name, SortDirection::Ascending);
    apply_network_batch(&state.shared, request_id, vec![entry(1, "a.txt")]);
    {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.selected = vec![EntryId(1), EntryId(99)];
        tab.focused = Some(EntryId(99));
        tab.selection_anchor = Some(EntryId(99));
    }
    finish(&state.shared, request_id);
    let app = state.lock().unwrap();
    assert_eq!(app.active().selected, [EntryId(1)]);
    assert_eq!(app.active().focused, None);
    assert_eq!(app.active().selection_anchor, None);
}

#[test]
fn issue_141_partial_sort_changes_apply_before_the_next_batch_and_completion() {
    let (state, request_id) = fixture(SortField::Name, SortDirection::Ascending);
    apply_network_batch(
        &state.shared,
        request_id,
        vec![entry(1, "c.txt"), entry(2, "a.txt"), entry(3, "b.txt")],
    );
    state
        .lock()
        .unwrap()
        .tab_mut(TabId(1))
        .unwrap()
        .select_entry(EntryId(1), false, false);
    set_sort_direction(&state, SortDirection::Descending);
    {
        let app = state.lock().unwrap();
        assert_eq!(ids(app.active().visible_entries()), [1, 3, 2]);
        assert_index_identity(app.active());
    }
    apply_network_batch(&state.shared, request_id, vec![entry(4, "d.txt")]);
    assert_eq!(
        ids(state.lock().unwrap().active().visible_entries()),
        [4, 1, 3, 2]
    );
    set_sort_field(&state, SortField::Size);
    assert_eq!(
        ids(state.lock().unwrap().active().visible_entries()),
        [4, 3, 2, 1]
    );
    finish(&state.shared, request_id);
    let app = state.lock().unwrap();
    assert_eq!(ids(app.active().visible_entries()), [4, 3, 2, 1]);
    assert_eq!(app.active().selected, [EntryId(1)]);
    assert_index_identity(app.active());
}

#[test]
fn issue_141_grouped_and_grid_projections_keep_the_sorted_order_within_each_group() {
    i_slint_backend_testing::init_no_event_loop();
    for group in [GroupField::None, GroupField::Kind] {
        let (state, request_id) = fixture(SortField::Name, SortDirection::Descending);
        state
            .lock()
            .unwrap()
            .update_directory_preference(PathBuf::from(NETWORK_PATH), |preference| {
                preference.group_field = group
            });
        let all = sample_entries();
        apply_network_batch(
            &state.shared,
            request_id,
            vec![all[0].clone(), all[2].clone()],
        );
        apply_network_batch(
            &state.shared,
            request_id,
            vec![all[4].clone(), all[1].clone(), all[3].clone()],
        );
        let app = state.lock().unwrap();
        let tab = app.active();
        let expected = if group == GroupField::None {
            vec![3, 4, 1, 5, 2]
        } else {
            vec![3, 4, 2, 1, 5]
        };
        let list =
            projected_directory_rows(tab.visible_entries(), tab, Texts::new(app.language), &app);
        assert_eq!(
            list.iter()
                .filter(|row| !row.group_header)
                .map(|row| row.id as u32)
                .collect::<Vec<_>>(),
            expected
        );
        let grid = projected_directory_grid_rows(
            tab.visible_entries(),
            tab,
            Texts::new(app.language),
            &app,
            2,
            64,
        );
        assert_eq!(
            grid.iter()
                .filter(|row| !row.group_header)
                .flat_map(|row| row.entries.iter().map(|entry| entry.id as u32))
                .collect::<Vec<_>>(),
            expected
        );
        let headers = if group == GroupField::None { 0 } else { 3 };
        assert_eq!(list.iter().filter(|row| row.group_header).count(), headers);
        assert_eq!(grid.iter().filter(|row| row.group_header).count(), headers);
    }
}

#[test]
fn issue_141_rejected_late_batches_acknowledge_navigation_cancellation_and_closed_tabs() {
    for close_tab in [false, true] {
        let (state, request_id) = fixture(SortField::Name, SortDirection::Ascending);
        if close_tab {
            let mut app = state.lock().unwrap();
            let window_id = app.active_window;
            app.close_window(window_id).unwrap();
        } else {
            state
                .lock()
                .unwrap()
                .tab_mut(TabId(1))
                .unwrap()
                .begin_directory_navigation(
                    PathBuf::from(r"C:\after-network"),
                    NavigationKind::Normal,
                );
        }
        apply_network_batch(&state.shared, request_id, vec![entry(1, "late.txt")]);
        finish(&state.shared, request_id);
        let app = state.lock().unwrap();
        if close_tab {
            assert!(app.tab(TabId(1)).is_none());
        } else {
            let tab = app.active();
            assert_eq!(tab.visible_path(), Some(Path::new(r"C:\after-network")));
            assert!(tab.pending_entries.is_empty());
            assert_eq!(tab.load_state, LoadState::Loading);
        }
    }
}

#[test]
fn issue_141_waiting_network_producer_does_not_block_local_navigation_or_cancellation() {
    use super::directory_loading::deliver_directory_batch;
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    let state = Arc::new(Mutex::new(app));
    let (network_sender, network_requests) = mpsc::sync_channel(1);
    let (local_sender, local_requests) = mpsc::channel();
    assert!(submit_path_navigation(
        &local_sender,
        &network_sender,
        &state,
        TabId(1),
        PathBuf::from(NETWORK_PATH),
        NavigationKind::Normal
    ));
    let request = network_requests
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let network_cancel = request.cancel.clone();
    let (events, receiver) = mpsc::sync_channel(32);
    let (done, completion) = mpsc::channel();
    let worker = thread::spawn(move || {
        let result = deliver_directory_batch(&request, &events, vec![entry(1, "pending.txt")]);
        done.send(result.map_err(|error| error.kind())).unwrap();
    });
    let pending = receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(completion.try_recv().is_err());
    let local_path = PathBuf::from(r"C:\after-network");
    assert!(submit_path_navigation(
        &local_sender,
        &network_sender,
        &state,
        TabId(1),
        local_path.clone(),
        NavigationKind::Normal
    ));
    let local = local_requests.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(local.path, local_path);
    assert!(network_cancel.load(Ordering::Acquire));
    assert!(!local.cancelled());
    assert_eq!(
        completion.recv_timeout(Duration::from_secs(2)).unwrap(),
        Err(io::ErrorKind::Interrupted)
    );
    worker.join().unwrap();
    apply_event(&state, pending);
    let mut local_entry = entry(1, "local.txt");
    local_entry.path = local_path.join("local.txt");
    local_entry.parent_display = local_path.to_string_lossy().into_owned();
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id: local.request_id,
            entries: vec![local_entry],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id: TabId(1),
            request_id: local.request_id,
            path: local_path.clone(),
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    let app = state.lock().unwrap();
    assert_eq!(app.active().load_state, LoadState::Complete);
    assert_eq!(app.active().visible_path(), Some(local_path.as_path()));
    assert_eq!(app.active().entries.len(), 1);
    assert_eq!(app.active().entries[0].path, local_path.join("local.txt"));
}

#[test]
fn issue_141_large_directory_reports_merge_and_model_projection_cost() {
    i_slint_backend_testing::init_no_event_loop();
    const COUNT: u32 = 100_000;
    let (state, request_id) = fixture(SortField::Name, SortDirection::Ascending);
    let ui = headless_model(request_id);
    let mut total_merge = Duration::ZERO;
    let mut peak_merge = Duration::ZERO;
    let mut batches = 0;
    for start in (1..=COUNT).step_by(256) {
        let entries = (start..=COUNT.min(start + 255))
            .map(|id| entry(id, &format!("item-{:06}.txt", COUNT - id)))
            .collect::<Vec<_>>();
        let started = Instant::now();
        apply_network_batch(&state.shared, request_id, entries);
        let elapsed = started.elapsed();
        total_merge += elapsed;
        peak_merge = peak_merge.max(elapsed);
        batches += 1;
    }
    {
        let app = state.lock().unwrap();
        let tab = app.active();
        assert_eq!(tab.pending_entries.len(), COUNT as usize);
        assert_eq!(tab.pending_entries.first().unwrap().id, EntryId(COUNT));
        assert_eq!(tab.pending_entries.last().unwrap().id, EntryId(1));
        assert!(
            tab.pending_entries
                .windows(2)
                .all(|pair| pair[0].display_name < pair[1].display_name)
        );
        assert_index_identity(tab);
    }
    let started = Instant::now();
    append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
    let initial_projection = started.elapsed();
    assert_eq!(ui.get_files().row_count(), COUNT as usize);
    assert_eq!(ui.get_files().row_data(0).unwrap().id, COUNT as i32);
    assert_eq!(ui.get_files().row_data(COUNT as usize - 1).unwrap().id, 1);
    let additions = (0..256_u32)
        .map(|offset| {
            entry(
                COUNT + offset + 1,
                &format!("item-050000-new-{offset:03}.txt"),
            )
        })
        .collect::<Vec<_>>();
    let started = Instant::now();
    apply_network_batch(&state.shared, request_id, additions);
    let incremental_merge = started.elapsed();
    let started = Instant::now();
    append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
    let incremental_projection = started.elapsed();
    assert_eq!(ui.get_files().row_count(), COUNT as usize + 256);
    let visible_ids = list_ids(&ui);
    assert_eq!(
        visible_ids.iter().copied().collect::<HashSet<_>>().len(),
        visible_ids.len()
    );
    assert_eq!(
        visible_ids,
        ids(state.lock().unwrap().active().visible_entries())
    );
    eprintln!(
        "#141 entries={COUNT} batches={batches} merge_total_ms={} merge_peak_ms={} initial_model_projection_ms={} incremental_256_merge_ms={} incremental_256_projection_ms={} native_window=false",
        total_merge.as_millis(),
        peak_merge.as_millis(),
        initial_projection.as_millis(),
        incremental_merge.as_millis(),
        incremental_projection.as_millis(),
    );
}
