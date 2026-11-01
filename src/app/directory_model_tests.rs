//! #165: production projections and input paths stay bounded by the requested viewport.
use super::directory_model::{DirectoryGrid, DirectoryRows};
use super::*;
use std::{ffi::OsString, hint::black_box};

const COUNT: u32 = 100_000;
const ROOT: &str = r"C:\issue-165";

fn fixture(count: u32, mode: ViewMode, group: GroupField) -> SharedSessions {
    let root = PathBuf::from(ROOT);
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
                let extension = match id % 3 {
                    0 => "txt",
                    1 => "png",
                    _ => "pdf",
                };
                let name = format!("item-{id:06}.{extension}");
                FileEntry {
                    id: EntryId(id),
                    original_name: OsString::from(&name),
                    display_name: name.clone(),
                    name_highlights: Vec::new(),
                    path: root.join(name),
                    kind: crate::domain::EntryKind::File,
                    open_target: None,
                    library_source_index: None,
                    parent_display: ROOT.into(),
                    size_bytes: Some(u64::from(id)),
                    folder_size: FolderSizeState::NotIndexed,
                    modified: None,
                    created: None,
                    webdav: None,
                }
            })
            .collect(),
    );
    Arc::new(Mutex::new(app))
}

// Keep the old eager algorithm only as a measured reference, not as a production fallback.
fn eager_reference(
    app: &AppState,
    tab: &TabSession,
    mode: ViewMode,
    columns: usize,
) -> (Vec<FileRow>, Vec<GridRow>) {
    let entries = tab.visible_entries();
    let groups = directory_group_projections(app, tab, entries);
    let by_id = entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<HashMap<_, _>>();
    let texts = Texts::new(app.language);
    let requested_px = if mode.uses_grid_layout() {
        Some(grid_thumbnail_request_px(mode, 1.0))
    } else {
        file_thumbnail_request_px(mode, 1.0)
    };
    if mode.uses_grid_layout() {
        let projection = IconProjection::from_groups(
            &groups,
            columns,
            group_header_height(&groups),
            file_row_height(mode) as u64,
        );
        (
            Vec::new(),
            projection
                .rows
                .into_iter()
                .map(|row| match row {
                    IconVisualRow::GroupHeader {
                        label,
                        detail,
                        entry_count,
                        ..
                    } => GridRow {
                        group_header: true,
                        group_label: label.into(),
                        group_detail: detail.into(),
                        group_count: entry_count as i32,
                        entries: ModelRc::new(VecModel::default()),
                    },
                    IconVisualRow::Entries { entries, .. } => GridRow {
                        entries: ModelRc::new(VecModel::from(
                            entries
                                .into_iter()
                                .map(|id| file_row(by_id[&id], tab, texts, app, requested_px))
                                .collect::<Vec<_>>(),
                        )),
                        ..GridRow::default()
                    },
                })
                .collect(),
        )
    } else {
        let projection = ListProjection::from_groups(
            &groups,
            group_header_height(&groups),
            file_row_height(mode) as u64,
        );
        (
            projection
                .rows
                .into_iter()
                .map(|row| match row {
                    ListVisualRow::GroupHeader {
                        label,
                        detail,
                        entry_count,
                        ..
                    } => group_header_file_row(&label, &detail, entry_count),
                    ListVisualRow::Entry { entry_id } => {
                        file_row(by_id[&entry_id], tab, texts, app, requested_px)
                    }
                })
                .collect(),
            Vec::new(),
        )
    }
}

fn rows(ui: &AppWindow) -> ModelRc<FileRow> {
    if projected_view_mode(ui).uses_grid_layout() {
        ModelRc::from(
            ui.get_grid_rows()
                .as_any()
                .downcast_ref::<DirectoryGrid>()
                .unwrap()
                .rows(),
        )
    } else {
        ui.get_files()
    }
}

fn private_bytes() -> usize {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak_working_set: usize,
        working_set: usize,
        peak_paged: usize,
        paged: usize,
        peak_non_paged: usize,
        non_paged: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private_usage: usize,
    }
    use windows_sys::Win32::System::{
        LibraryLoader::{GetModuleHandleW, GetProcAddress},
        Threading::GetCurrentProcess,
    };
    type ReadMemory = unsafe extern "system" fn(*mut core::ffi::c_void, *mut Counters, u32) -> i32;
    let kernel_name = "kernel32.dll\0".encode_utf16().collect::<Vec<_>>();
    let read_memory: ReadMemory = unsafe {
        let kernel = GetModuleHandleW(kernel_name.as_ptr());
        assert!(!kernel.is_null());
        std::mem::transmute(
            GetProcAddress(kernel, c"K32GetProcessMemoryInfo".as_ptr().cast())
                .expect("Windows process memory counters"),
        )
    };
    let mut value = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        ..Counters::default()
    };
    let size = value.cb;
    assert_ne!(
        unsafe { read_memory(GetCurrentProcess(), &mut value, size) },
        0
    );
    value.private_usage
}

#[test]
fn issue_165_hundred_thousand_entries_all_views_are_viewport_bounded() {
    i_slint_backend_testing::init_no_event_loop();
    let mut reports = Vec::new();
    for mode in ViewMode::CTRL_WHEEL_ORDER {
        for group in [GroupField::None, GroupField::Kind] {
            let state = fixture(COUNT, mode, group);
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
            let before_memory = private_bytes();
            let started = Instant::now();
            refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
            let projection_ms = started.elapsed().as_secs_f64() * 1000.0;
            let lazy_memory = private_bytes();
            let row_model = rows(&ui);
            let model = row_model.as_any().downcast_ref::<DirectoryRows>().unwrap();
            let initial_generated = model.generated_rows();
            assert!(
                initial_generated < 2048,
                "{mode:?}/{group:?}: initial {initial_generated}"
            );
            let files = ui.get_files();
            let grid = ui.get_grid_rows();
            let (sender, _receiver) = mpsc::channel();
            let scheduler = ThumbnailScheduler::new(sender);
            let maximum = projected_scroll_maximum(&ui, mode, ui.get_file_viewport_height());
            let mut peak_input_ms = 0.0_f64;
            let mut peak_generated = 0;
            for fraction in [0.0, 0.25, 0.5, 0.75, 1.0] {
                ui.set_file_viewport_y(-maximum * fraction);
                let previous = model.generated_rows();
                let started = Instant::now();
                refresh_grouped_grid_viewport(&ui);
                let ids = {
                    let app = state.lock().unwrap();
                    visible_shortcut_entry_ids(&ui, &app, app.tab(TabId(1)).unwrap())
                };
                assert!(!ids.is_empty(), "{mode:?}/{group:?}: viewport {fraction}");
                assert!(
                    ids.len() < 2048,
                    "{mode:?}/{group:?}: request size {}",
                    ids.len()
                );
                let rect = SelectionRect {
                    left: 16.0,
                    top: -ui.get_file_viewport_y(),
                    right: 360.0,
                    bottom: -ui.get_file_viewport_y() + 180.0,
                };
                let hits = projected_rectangle_hits(&ui, rect).expect("directory model hit test");
                let anchor = capture_zoom_anchor(&ui, 16.0, 0.0).unwrap();
                assert!(projected_entry_top(&ui, anchor.entry_id).is_some());
                assert_eq!(
                    projected_scroll_maximum(&ui, mode, ui.get_file_viewport_height()),
                    maximum
                );
                request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
                peak_input_ms = peak_input_ms.max(started.elapsed().as_secs_f64() * 1000.0);
                let generated = model.generated_rows() - previous;
                peak_generated = peak_generated.max(generated);
                assert!(
                    generated < 2048,
                    "{mode:?}/{group:?}: generated {generated} on scroll"
                );
                // Compare identities with the prior geometry implementation, outside the timed path.
                let app = state.lock().unwrap();
                let reference = rectangle_selection_hits_for_app(
                    &app,
                    TabId(1),
                    mode,
                    ui.get_grid_column_count() as usize,
                    ui.get_file_viewport_width(),
                    ui.get_file_viewport_x(),
                    ui.get_details_hit_width(),
                    rect,
                    None,
                );
                assert_eq!(hits, reference, "{mode:?}/{group:?}: hits at {fraction}");
            }
            let changed = {
                let mut app = state.lock().unwrap();
                let tab = app.tab_mut(TabId(1)).unwrap();
                mutate_selection(tab, |tab| tab.select_entry(EntryId(COUNT), false, false))
            };
            let previous = model.generated_rows();
            update_file_rows(&ui, &state, TabId(1), &changed);
            assert!(
                model.generated_rows() - previous < 16,
                "selection materialized the directory"
            );
            let selected_row = model
                .row_data(model.locate(EntryId(COUNT)).unwrap())
                .unwrap();
            assert!(selected_row.selected);
            assert!(std::ptr::eq(files.as_any(), ui.get_files().as_any()));
            assert!(std::ptr::eq(grid.as_any(), ui.get_grid_rows().as_any()));
            let eager_start_memory = private_bytes();
            let started = Instant::now();
            let eager = {
                let app = state.lock().unwrap();
                eager_reference(
                    &app,
                    app.tab(TabId(1)).unwrap(),
                    mode,
                    ui.get_grid_column_count() as usize,
                )
            };
            let eager_ms = started.elapsed().as_secs_f64() * 1000.0;
            let eager_memory = private_bytes();
            let eager_rows = if mode.uses_grid_layout() {
                eager
                    .1
                    .iter()
                    .map(|row| row.entries.row_count())
                    .sum::<usize>()
            } else {
                eager.0.iter().filter(|row| !row.group_header).count()
            };
            assert_eq!(eager_rows, COUNT as usize);
            black_box(&eager);
            reports.push(format!(concat!(
                "{{\"view\":\"{:?}\",\"group\":\"{:?}\",",
                "\"before\":{{\"reference\":\"previous eager algorithm\",\"projection_ms\":{:.3},\"generated_file_rows\":{},\"private_bytes_start\":{},\"private_bytes_end\":{}}},",
                "\"after\":{{\"projection_ms\":{:.3},\"initial_generated_file_rows\":{},\"peak_scroll_generated_file_rows\":{},\"peak_input_ms\":{:.3},\"private_bytes_start\":{},\"private_bytes_end\":{}}}}}"),
                mode, group, eager_ms, eager_rows, eager_start_memory, eager_memory,
                projection_ms, initial_generated, peak_generated, peak_input_ms, before_memory, lazy_memory));
        }
    }
    let output = Path::new("artifacts/perf/issue-165/before-after.json");
    std::fs::create_dir_all(output.parent().unwrap()).unwrap();
    let report = format!(
        "{{\"issue\":165,\"entries\":100000,\"native_window\":false,\"memory_metric\":\"process private bytes; allocator retention may affect deltas\",\"samples\":[{}]}}\n",
        reports.join(",\n")
    );
    std::fs::write(output, &report).unwrap();
    eprintln!("#165 {report}");
}

fn assert_content(actual: &FileRow, expected: &FileRow) {
    assert_eq!(actual.id, expected.id);
    assert_eq!(actual.name, expected.name);
    assert_eq!(actual.size, expected.size);
    assert_eq!(actual.modified, expected.modified);
    assert_eq!(actual.created, expected.created);
    assert_eq!(actual.kind, expected.kind);
    assert_eq!(actual.parent_path, expected.parent_path);
    assert_eq!(actual.is_directory, expected.is_directory);
    assert_eq!(actual.loaded, expected.loaded);
    assert_eq!(actual.cut, expected.cut);
    assert_eq!(
        actual.name_segments.row_count(),
        expected.name_segments.row_count()
    );
    assert_eq!(actual.selected, expected.selected);
    assert_eq!(actual.focused, expected.focused);
    assert_eq!(actual.group_header, expected.group_header);
    assert_eq!(actual.group_label, expected.group_label);
    assert_eq!(actual.group_detail, expected.group_detail);
    assert_eq!(actual.group_count, expected.group_count);
    assert_eq!(actual.thumbnail_state, expected.thumbnail_state);
}

#[test]
fn issue_165_lazy_rows_match_eager_contents_and_preserve_nested_models() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in ViewMode::CTRL_WHEEL_ORDER {
        for group in [
            GroupField::None,
            GroupField::Name,
            GroupField::Kind,
            GroupField::Size,
            GroupField::Modified,
            GroupField::Created,
        ] {
            let state = fixture(32, mode, group);
            let app = state.lock().unwrap();
            let tab = app.tab(TabId(1)).unwrap();
            let groups = directory_group_projections(&app, tab, tab.visible_entries());
            let requested_px = if mode.uses_grid_layout() {
                Some(grid_thumbnail_request_px(mode, 1.0))
            } else {
                file_thumbnail_request_px(mode, 1.0)
            };
            let model = DirectoryRows::new(
                tab.entries.clone(),
                groups.clone(),
                tab,
                &app,
                requested_px,
                file_row_height(mode),
            );
            let grid = DirectoryGrid::new(model.clone(), &groups, 5, file_row_height(mode));
            assert_eq!(
                model.generated_rows(),
                0,
                "construction must not format entries"
            );
            assert_eq!(model.entry_count(), 32);
            let expected = eager_reference(&app, tab, mode, 5);
            if mode.uses_grid_layout() {
                assert_eq!(grid.row_count(), expected.1.len());
                for (index, reference) in expected.1.iter().enumerate() {
                    let row = grid.row_data(index).unwrap();
                    assert_eq!(row.group_header, reference.group_header);
                    assert_eq!(row.group_label, reference.group_label);
                    assert_eq!(row.group_detail, reference.group_detail);
                    assert_eq!(row.group_count, reference.group_count);
                    let again = grid.row_data(index).unwrap();
                    if !row.group_header {
                        assert!(
                            std::ptr::eq(row.entries.as_any(), again.entries.as_any()),
                            "visible row retains its nested model"
                        );
                    }
                    assert_eq!(row.entries.row_count(), reference.entries.row_count());
                    for slot in 0..row.entries.row_count() {
                        let actual = row.entries.row_data(slot).unwrap();
                        assert_content(&actual, &reference.entries.row_data(slot).unwrap());
                        assert_eq!(grid.locate(EntryId(actual.id as u32)), Some((index, slot)));
                    }
                }
            } else {
                assert_eq!(model.row_count(), expected.0.len());
                for (index, reference) in expected.0.iter().enumerate() {
                    let row = model.row_data(index).unwrap();
                    assert_content(&row, reference);
                    if !row.group_header {
                        assert_eq!(model.locate(EntryId(row.id as u32)), Some(index));
                    }
                }
            }
            assert!(model.row_data(model.row_count()).is_none());
            assert!(grid.row_data(grid.row_count()).is_none());
            assert_eq!(
                model.generated_rows(),
                if mode.uses_grid_layout() {
                    32
                } else {
                    model.row_count()
                },
                "each requested FileRow is formatted once"
            );
        }
    }
}

#[test]
fn issue_165_selection_and_backfill_patch_only_requested_entry() {
    i_slint_backend_testing::init_no_event_loop();
    let state = fixture(100_000, ViewMode::MediumIcons, GroupField::Kind);
    let (model, grid) = {
        let app = state.lock().unwrap();
        let tab = app.tab(TabId(1)).unwrap();
        let groups = directory_group_projections(&app, tab, &tab.entries);
        let original_owners = Arc::strong_count(&tab.entries);
        let model = DirectoryRows::new(
            tab.entries.clone(),
            groups.clone(),
            tab,
            &app,
            Some(96),
            file_row_height(ViewMode::MediumIcons),
        );
        assert_eq!(
            Arc::strong_count(&tab.entries),
            original_owners + 1,
            "projection shares the committed raw entries"
        );
        let grid = DirectoryGrid::new(
            model.clone(),
            &groups,
            5,
            file_row_height(ViewMode::MediumIcons),
        );
        (model, grid)
    };
    let id = EntryId(99_999);
    let (row_index, column) = grid.locate(id).unwrap();
    let nested = grid.row_data(row_index).unwrap().entries;
    assert_eq!(
        model.generated_rows(),
        0,
        "grid wrappers do not format file data"
    );
    {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.select_entry(id, false, false);
        model.patch_selection(tab, &HashSet::from([id]));
    }
    assert_eq!(
        model.generated_rows(),
        0,
        "selection only sends notifications"
    );
    let selected = nested.row_data(column).unwrap();
    assert_eq!(selected.id, id.0 as i32);
    assert!(selected.selected && selected.focused);
    {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(TabId(1)).unwrap();
        let index = tab.visible_entry_index(id).unwrap();
        let entry = &mut Arc::make_mut(&mut tab.entries)[index];
        entry.display_name = "renamed-entry.txt".into();
        entry.original_name = "renamed-entry.txt".into();
        entry.path = Path::new(ROOT).join("renamed-entry.txt");
        entry.size_bytes = Some(123_456);
        let tab = app.tab(TabId(1)).unwrap();
        model.patch_entries(tab, &app, &HashSet::from([id]));
    }
    assert_eq!(
        model.generated_rows(),
        1,
        "backfill does not regenerate hidden rows"
    );
    let updated = nested.row_data(column).unwrap();
    assert_eq!(updated.name.as_str(), "renamed-entry.txt");
    assert!(updated.selected && updated.focused);
    assert_eq!(
        updated.size.as_str(),
        Texts::new(state.lock().unwrap().language).size(Some(123_456))
    );
    assert_eq!(model.generated_rows(), 2);
    assert!(std::ptr::eq(
        nested.as_any(),
        grid.row_data(row_index).unwrap().entries.as_any()
    ));
    assert_eq!(
        model.rows_between(model.extent() + 1.0, model.extent() + 100.0),
        model.row_count()..model.row_count()
    );
    assert_eq!(
        grid.rows_between(grid.extent() + 1.0, grid.extent() + 100.0),
        grid.row_count()..grid.row_count()
    );
}

#[test]
fn issue_165_zoom_anchor_inside_group_header_keeps_nearest_entry() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in ViewMode::CTRL_WHEEL_ORDER {
        for detailed_header in [false, true] {
            let state = fixture(32, mode, GroupField::Kind);
            let app = state.lock().unwrap();
            let tab = app.tab(TabId(1)).unwrap();
            let mut groups = directory_group_projections(&app, tab, &tab.entries);
            if detailed_header {
                for group in &mut groups {
                    group.detail = ROOT.into();
                }
            }
            let first_entry = groups[0].entries[0];
            let rows = DirectoryRows::new(
                tab.entries.clone(),
                groups.clone(),
                tab,
                &app,
                None,
                file_row_height(mode),
            );
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
            ui.set_view_mode(view_mode_to_ui(mode));
            ui.set_grid_column_count(5);
            ui.set_files(ModelRc::from(rows.clone()));
            if mode.uses_grid_layout() {
                ui.set_grid_rows(ModelRc::from(DirectoryGrid::new(
                    rows.clone(),
                    &groups,
                    5,
                    file_row_height(mode),
                )));
            }
            ui.set_file_viewport_y(-1.0);
            let anchor = capture_zoom_anchor(&ui, 16.0, 0.0)
                .expect("header before the first entry still has an anchor");
            assert_eq!(
                anchor.entry_id, first_entry.0 as i32,
                "{mode:?}/{detailed_header}"
            );
            let top = if detailed_header { 48.0 } else { 32.0 };
            assert_eq!(anchor.screen_y, top - 1.0);
            assert_eq!(
                rows.generated_rows(),
                0,
                "anchor uses geometry, not FileRow"
            );
        }
    }
}

#[test]
fn issue_165_file_only_viewport_does_not_clone_shared_entries_for_folder_queries() {
    i_slint_backend_testing::init_no_event_loop();
    let state = fixture(COUNT, ViewMode::MediumIcons, GroupField::Kind);
    {
        let mut app = state.lock().unwrap();
        for entry in Arc::make_mut(&mut app.tab_mut(TabId(1)).unwrap().entries) {
            entry.folder_size = FolderSizeState::Unknown;
        }
    }
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
    let (before, request_id) = {
        let app = state.lock().unwrap();
        let tab = app.tab(TabId(1)).unwrap();
        (tab.entries.clone(), tab.latest_request())
    };
    let (sender, receiver) = mpsc::channel();
    submit_visible_folder_sizes(&sender, &state, TabId(1), request_id, &ui);
    assert!(receiver.try_recv().is_err());
    assert!(Arc::ptr_eq(
        &before,
        &state.lock().unwrap().tab(TabId(1)).unwrap().entries
    ));
}

#[test]
fn issue_165_visible_requests_reject_other_tab_and_old_request_models() {
    i_slint_backend_testing::init_no_event_loop();
    let state = fixture(32, ViewMode::MediumIcons, GroupField::Kind);
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
    let app = state.lock().unwrap();
    let tab = app.tab(TabId(1)).unwrap();
    assert!(!visible_shortcut_entry_ids(&ui, &app, tab).is_empty());
    ui.set_projected_file_tab_id(2);
    assert!(visible_shortcut_entry_ids(&ui, &app, tab).is_empty());
    ui.set_projected_file_tab_id(1);
    ui.set_projected_file_request_id(ui.get_projected_file_request_id() + 1);
    assert!(visible_shortcut_entry_ids(&ui, &app, tab).is_empty());
}
