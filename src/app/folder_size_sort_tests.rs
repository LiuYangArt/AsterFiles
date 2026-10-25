//! #134: a completed folder-size sort must reach the displayed models in the new order.
use super::*;
use crate::domain::EntryKind;
use std::cell::Cell;
use std::ffi::OsString;
use std::rc::Rc;

const DIRECTORY: &str = r"C:\issue-134";
const DIRECTORY_COUNT: u32 = 200;

fn entry(id: u32) -> FileEntry {
    let (name, kind, size_bytes) = if id <= DIRECTORY_COUNT {
        (format!("dir-{id:03}"), EntryKind::Directory, None)
    } else {
        (
            format!("file-{id:03}.txt"),
            EntryKind::File,
            Some(u64::from(id)),
        )
    };
    let path = PathBuf::from(DIRECTORY).join(&name);
    FileEntry {
        id: EntryId(id),
        original_name: OsString::from(&name),
        display_name: name,
        name_highlights: Vec::new(),
        path,
        kind,
        open_target: None,
        library_source_index: None,
        parent_display: DIRECTORY.to_owned(),
        size_bytes,
        folder_size: FolderSizeState::Unknown,
        modified: None,
        created: None,
        webdav: None,
    }
}

/// Later names receive larger sizes, so a descending size sort reverses the name order.
fn folder_size_for(entry_id: EntryId) -> FolderSizeState {
    FolderSizeState::Value(u64::from(entry_id.0) * 1_000_000)
}

fn fixture(view_mode: ViewMode, group_field: GroupField) -> SharedSessions {
    let mut app = AppState::new_for_test(vec![PathBuf::from(DIRECTORY)], 0, [0, 1, 2, 3]);
    app.update_directory_preference(PathBuf::from(DIRECTORY), |view| {
        view.view_mode = view_mode;
        view.group_field = group_field;
        view.sort_field = SortField::Size;
        view.sort_direction = SortDirection::Descending;
    });
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.load_state = LoadState::Complete;
    tab.sort_field = SortField::Size;
    tab.sort_direction = SortDirection::Descending;
    tab.replace_entries((1..=DIRECTORY_COUNT + 4).map(entry).collect());
    tab.resort_entries();
    tab.select_entry(EntryId(2), false, false);
    Arc::new(Mutex::new(app))
}

fn headless(state: &SharedSessions) -> AppWindow {
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
    ui
}

fn displayed_rows(ui: &AppWindow) -> Vec<FileRow> {
    if projected_view_mode(ui).uses_grid_layout() {
        ui.get_grid_rows()
            .iter()
            .flat_map(|row| row.entries.iter().collect::<Vec<_>>())
            .collect()
    } else {
        ui.get_files()
            .iter()
            .filter(|row| !row.group_header)
            .collect()
    }
}

fn displayed_ids(ui: &AppWindow) -> Vec<EntryId> {
    displayed_rows(ui)
        .iter()
        .map(|row| EntryId(row.id as u32))
        .collect()
}

/// Headers and entries in display order, for comparing against a full redraw.
fn visual_rows(ui: &AppWindow) -> Vec<(bool, String, i32)> {
    let header = |label: &slint::SharedString| (true, label.to_string(), 0);
    let entry = |row: FileRow| (false, row.size.to_string(), row.id);
    if projected_view_mode(ui).uses_grid_layout() {
        ui.get_grid_rows()
            .iter()
            .flat_map(|row| {
                if row.group_header {
                    vec![header(&row.group_label)]
                } else {
                    row.entries.iter().map(entry).collect()
                }
            })
            .collect()
    } else {
        ui.get_files()
            .iter()
            .map(|row| {
                if row.group_header {
                    header(&row.group_label)
                } else {
                    entry(row)
                }
            })
            .collect()
    }
}

/// Feeds every folder through the same scheduler, commit and projection path as the
/// Everything event pump, returning the projection of the final commit.
fn complete_size_sort(state: &SharedSessions) -> Option<FolderSizeProjection> {
    let mut app = state.lock().unwrap();
    let tab = app.tab_mut(TabId(1)).unwrap();
    let request_id = tab.latest_request();
    let mut pending = with_folder_scheduler(tab, |scheduler, entries| {
        scheduler.begin_complete_sort(request_id, entries)
    });
    let mut last = None;
    while !pending.is_empty() {
        for query in std::mem::take(&mut pending) {
            assert!(start_folder_size_query(&mut app, TabId(1), &query));
            let commit = apply_folder_size_event(
                &mut app,
                TabId(1),
                &query,
                folder_size_for(query.key.entry_id),
            );
            if commit == FolderSizeCommit::Staged {
                let tab = app.tab_mut(TabId(1)).unwrap();
                pending.extend(with_folder_scheduler(tab, |scheduler, entries| {
                    scheduler.next_complete_queries(entries)
                }));
            }
            last = folder_size_projection(&mut app, TabId(1), commit);
        }
    }
    last
}

fn scheduler() -> ThumbnailScheduler {
    let (sender, _) = mpsc::channel();
    ThumbnailScheduler::new(sender)
}

#[test]
fn issue_134_completed_size_sort_reorders_every_view_in_place() {
    i_slint_backend_testing::init_no_event_loop();
    let cases = [
        (ViewMode::Details, GroupField::None),
        (ViewMode::List, GroupField::None),
        (ViewMode::Content, GroupField::None),
        (ViewMode::MediumIcons, GroupField::None),
        (ViewMode::Details, GroupField::Size),
        (ViewMode::MediumIcons, GroupField::Size),
    ];
    for (view_mode, group_field) in cases {
        let label = format!("{view_mode:?} grouped by {group_field:?}");
        let state = fixture(view_mode, group_field);
        let ui = headless(&state);
        let grouped = group_field != GroupField::None;
        let before = displayed_ids(&ui);
        if !grouped {
            assert_eq!(
                before[0],
                EntryId(1),
                "{label}: name order before sizes arrive"
            );
        }
        ui.set_file_viewport_y(-120.0);
        let files = ui.get_files();
        let grid = ui.get_grid_rows();
        let nested = grid.row_data(usize::from(grouped)).map(|row| row.entries);
        let placed = ui.get_grouped_grid_layout();
        let interrupted = Rc::new(Cell::new(false));
        let drag = interrupted.clone();
        ui.on_cancel_internal_drag(move || drag.set(true));
        let rectangle = interrupted.clone();
        ui.on_cancel_rectangle_selection(move || rectangle.set(true));

        let projection = complete_size_sort(&state);
        assert!(
            matches!(projection, Some(FolderSizeProjection::Resorted)),
            "{label}: the final size completes the sort"
        );
        let request_id = present_folder_size_projection(
            &ui,
            &state,
            TabId(1),
            projection.unwrap(),
            &scheduler(),
        );

        {
            let app = state.lock().unwrap();
            let tab = app.tab(TabId(1)).unwrap();
            assert_eq!(request_id, Some(tab.latest_request()), "{label}");
            let sorted = tab.entries.iter().map(|entry| entry.id).collect::<Vec<_>>();
            assert_eq!(
                sorted[0],
                EntryId(DIRECTORY_COUNT),
                "{label}: largest folder first"
            );
            let rows = displayed_rows(&ui);
            assert_ne!(
                displayed_ids(&ui),
                before,
                "{label}: sizes change the order"
            );
            for (index, row) in rows.iter().enumerate() {
                let id = EntryId(row.id as u32);
                if !grouped {
                    assert_eq!(id, sorted[index], "{label}: display follows the sort");
                    assert_eq!(
                        tab.visible_entry_index(id),
                        Some(index),
                        "{label}: keyboard index"
                    );
                }
                assert_eq!(
                    row.selected,
                    id == EntryId(2),
                    "{label}: selection identity"
                );
                assert_eq!(row.focused, id == EntryId(2), "{label}: focus identity");
                if id.0 <= DIRECTORY_COUNT {
                    assert_eq!(
                        row.size.as_str(),
                        Texts::new(app.language).folder_size(folder_size_for(id)),
                        "{label}: backfilled size text"
                    );
                }
            }
        }
        if view_mode.uses_grid_layout() {
            let positions = GRID_ENTRY_POSITIONS
                .with_borrow(|by_window| by_window.get(&WindowId(1)).cloned().unwrap());
            for (row_index, row) in ui.get_grid_rows().iter().enumerate() {
                for (entry_index, entry) in row.entries.iter().enumerate() {
                    assert_eq!(
                        positions.get(&EntryId(entry.id as u32)),
                        Some(&(row_index, entry_index)),
                        "{label}: grid slot follows the entry"
                    );
                }
            }
        }
        assert_eq!(
            ui.get_file_viewport_y(),
            -120.0,
            "{label}: scroll is not pulled back"
        );
        assert!(
            !interrupted.get(),
            "{label}: pointer gestures are not cancelled"
        );
        assert!(
            std::ptr::eq(files.as_any(), ui.get_files().as_any()),
            "{label}"
        );
        assert!(
            std::ptr::eq(grid.as_any(), ui.get_grid_rows().as_any()),
            "{label}"
        );
        assert!(
            std::ptr::eq(placed.as_any(), ui.get_grouped_grid_layout().as_any()),
            "{label}"
        );
        if view_mode.uses_grid_layout() {
            assert!(
                std::ptr::eq(
                    nested.unwrap().as_any(),
                    ui.get_grid_rows()
                        .row_data(usize::from(grouped))
                        .unwrap()
                        .entries
                        .as_any()
                ),
                "{label}: nested grid models are reused"
            );
        }
        // The patched models must match what a full redraw of the same state shows.
        let patched = visual_rows(&ui);
        let redrawn = headless(&state);
        assert_eq!(
            patched,
            visual_rows(&redrawn),
            "{label}: same as a full projection"
        );
        if grouped {
            assert_eq!(
                ui.get_grouped_grid_extent(),
                redrawn.get_grouped_grid_extent(),
                "{label}: grouped geometry"
            );
        }
    }
}

#[test]
fn issue_134_range_selection_after_resort_follows_displayed_order() {
    i_slint_backend_testing::init_no_event_loop();
    let state = fixture(ViewMode::Details, GroupField::None);
    let ui = headless(&state);
    let projection = complete_size_sort(&state).unwrap();
    present_folder_size_projection(&ui, &state, TabId(1), projection, &scheduler());
    let shown = displayed_ids(&ui);
    let mut app = state.lock().unwrap();
    let tab = app.tab_mut(TabId(1)).unwrap();
    let anchor = shown.iter().position(|id| *id == EntryId(2)).unwrap();
    tab.select_entry(shown[anchor - 3], false, true);
    let mut selected = tab.selected.clone();
    selected.sort_by_key(|id| shown.iter().position(|shown| shown == id));
    assert_eq!(selected, shown[anchor - 3..=anchor]);
}

#[test]
fn issue_134_stale_results_leave_the_current_page_unchanged() {
    i_slint_backend_testing::init_no_event_loop();
    // A new navigation supersedes the size request before its last result arrives.
    let state = fixture(ViewMode::Details, GroupField::None);
    let ui = headless(&state);
    let files = ui.get_files();
    let before = displayed_ids(&ui);
    let query = {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(1)).unwrap();
        let request_id = tab.latest_request();
        let query = with_folder_scheduler(tab, |scheduler, entries| {
            scheduler.begin_complete_sort(request_id, entries)
        })
        .remove(0);
        assert!(start_folder_size_query(&mut app, TabId(1), &query));
        app.tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(PathBuf::from(r"C:\elsewhere"), NavigationKind::Normal);
        query
    };
    let commit = apply_folder_size_event(
        &mut state.lock().unwrap(),
        TabId(1),
        &query,
        folder_size_for(query.key.entry_id),
    );
    assert_eq!(commit, FolderSizeCommit::Ignored);
    assert!(folder_size_projection(&mut state.lock().unwrap(), TabId(1), commit).is_none());
    assert_eq!(displayed_ids(&ui), before);
    assert!(std::ptr::eq(files.as_any(), ui.get_files().as_any()));

    // A closed tab ignores its late result.
    let state = fixture(ViewMode::Details, GroupField::None);
    let query = {
        let mut app = state.lock().unwrap();
        let other = app.create_tab(PathBuf::from(DIRECTORY));
        let tab = app.tab_mut(TabId(1)).unwrap();
        let request_id = tab.latest_request();
        let query = with_folder_scheduler(tab, |scheduler, entries| {
            scheduler.begin_complete_sort(request_id, entries)
        })
        .remove(0);
        assert!(start_folder_size_query(&mut app, TabId(1), &query));
        app.active_window_state_mut().active_tab = TabId(1);
        assert_eq!(app.close_tab(TabId(1)), Some(other));
        query
    };
    let mut app = state.lock().unwrap();
    let commit = apply_folder_size_event(
        &mut app,
        TabId(1),
        &query,
        folder_size_for(query.key.entry_id),
    );
    assert_eq!(commit, FolderSizeCommit::Ignored);
    assert!(folder_size_projection(&mut app, TabId(1), FolderSizeCommit::CompleteSort).is_none());
}

#[test]
fn issue_134_resort_for_a_page_that_is_no_longer_projected_is_not_drawn() {
    i_slint_backend_testing::init_no_event_loop();
    let state = fixture(ViewMode::Details, GroupField::None);
    let ui = headless(&state);
    let before = displayed_ids(&ui);
    let projection = complete_size_sort(&state).unwrap();
    // The window now shows a newer request, as after a navigation the tab has not seen yet.
    ui.set_projected_file_request_id(ui.get_projected_file_request_id() + 1);
    let request_id =
        present_folder_size_projection(&ui, &state, TabId(1), projection, &scheduler());
    assert_eq!(request_id, None);
    assert_eq!(displayed_ids(&ui), before);
}
