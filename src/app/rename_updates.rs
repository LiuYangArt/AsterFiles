//! Apply a completed rename to loaded directory snapshots without restarting enumeration.
use super::*;

#[derive(Default)]
pub(super) struct RenameUpdate {
    pub updated_tabs: Vec<TabId>,
    pub reload_tabs: Vec<(TabId, NavigationLocation)>,
    pub search_tabs: Vec<TabId>,
}

fn search_contains(tab: &TabSession, path: &Path) -> bool {
    let in_directory = |directory: &Path| match tab.search_depth {
        SearchDepth::CurrentFolder => path.parent() == Some(directory),
        SearchDepth::Recursive => path.starts_with(directory),
    };
    match &tab.search_scope {
        SearchScope::Global => true,
        SearchScope::Directory(directory) => in_directory(directory),
        SearchScope::Library { sources, .. } => sources
            .iter()
            .any(|source| in_directory(source) || source.starts_with(path)),
    }
}

fn renamed_search_scope(scope: &SearchScope, old: &Path, new: &Path) -> Option<SearchScope> {
    match scope {
        SearchScope::Directory(directory) => directory
            .strip_prefix(old)
            .ok()
            .map(|suffix| SearchScope::Directory(new.join(suffix))),
        // Library sources remain owned by the library definition and are re-read when querying.
        SearchScope::Global | SearchScope::Library { .. } => None,
    }
}

fn location_contains(app: &AppState, location: &NavigationLocation, parent: &Path) -> bool {
    match location {
        NavigationLocation::Directory(path) => path == parent,
        NavigationLocation::Library(library) => app
            .libraries
            .iter()
            .find(|definition| definition.id.as_os_str() == library.identity.as_os_str())
            .is_some_and(|definition| {
                definition
                    .sources
                    .iter()
                    .any(|source| source.path.as_deref() == Some(parent))
            }),
        NavigationLocation::Home | NavigationLocation::WebDav(_) => false,
    }
}

fn migrate_images(
    cache: &mut HashMap<(PathBuf, u32), platform::windows::shell_icons::ShellIconRgba>,
    order: &mut VecDeque<(PathBuf, u32)>,
    old: &Path,
    new: &Path,
    retain: bool,
) {
    let keys = cache
        .keys()
        .filter(|(path, _)| path == old || path == new)
        .cloned()
        .collect::<Vec<_>>();
    let images = keys
        .iter()
        .filter_map(|key| {
            let image = cache.remove(key)?;
            (retain && key.0 == old).then_some(((new.to_path_buf(), key.1), image))
        })
        .collect::<Vec<_>>();
    order.retain(|(path, _)| path != old && path != new);
    for (key, image) in images {
        insert_bounded_image(cache, order, key, image, THUMBNAIL_CACHE_CAPACITY);
    }
}

pub(super) fn apply_completed(app: &mut AppState, old: &Path, new: &Path) -> RenameUpdate {
    let mut update = RenameUpdate::default();
    let Some(parent) = old.parent() else {
        return update;
    };
    let retain_images = old.extension().map(|ext| ext.to_ascii_lowercase())
        == new.extension().map(|ext| ext.to_ascii_lowercase())
        || app
            .windows
            .values()
            .flat_map(|window| window.tabs.values())
            .flat_map(|tab| tab.visible_entries())
            .any(|entry| {
                entry.path == old
                    && entry.kind == crate::domain::EntryKind::Directory
                    && entry.open_target.is_none()
            });
    let targets = app
        .windows
        .values()
        .flat_map(|window| window.tabs.values())
        .filter_map(|tab| {
            if tab.kind != TabKind::Files {
                return None;
            }
            if tab.page_source == PageSource::Search {
                return (search_contains(tab, old)
                    || search_contains(tab, new)
                    || renamed_search_scope(&tab.search_scope, old, new).is_some())
                .then_some((tab.id, None, true));
            }
            let location = tab.visible_location()?.clone();
            let nested = location
                .directory_path()
                .and_then(|path| path.strip_prefix(old).ok())
                .map(|suffix| NavigationLocation::Directory(new.join(suffix)));
            if let Some(location) = nested {
                return Some((tab.id, Some(location), false));
            }
            (location_contains(app, &location, parent)
                || (tab.load_state == LoadState::Complete
                    && tab.visible_entries().iter().any(|entry| entry.path == old)))
            .then_some((tab.id, Some(location), false))
        })
        .collect::<Vec<_>>();
    for (tab_id, location, search) in targets {
        if search {
            if let Some(tab) = app.tab_mut(tab_id)
                && let Some(scope) = renamed_search_scope(&tab.search_scope, old, new)
            {
                tab.search_scope = scope;
            }
            update.search_tabs.push(tab_id);
            continue;
        }
        let location = location.expect("directory targets have a location");
        let Some(tab) = app.tab_mut(tab_id) else {
            continue;
        };
        if tab.load_state != LoadState::Complete || tab.visible_location() != Some(&location) {
            update.reload_tabs.push((tab_id, location));
            continue;
        }
        if let Some(entry_id) = tab.rename_loaded_entry(old, new) {
            if !retain_images {
                let request_id = app.tab(tab_id).unwrap().latest_request();
                app.icons.remove(&(tab_id, request_id, entry_id));
            }
            app.focus_after_refresh.remove(&tab_id);
            update.updated_tabs.push(tab_id);
        } else {
            // A missing row may reflect an external change; do not invent its metadata.
            update.reload_tabs.push((tab_id, location));
        }
    }
    migrate_images(
        &mut app.thumbnail_cache,
        &mut app.thumbnail_cache_order,
        old,
        new,
        retain_images,
    );
    migrate_images(
        &mut app.grid_icon_cache,
        &mut app.grid_icon_cache_order,
        old,
        new,
        retain_images,
    );
    let old_icon = app.icon_cache.remove(old);
    app.icon_cache.remove(new);
    if retain_images && let Some(icon) = old_icon {
        app.icon_cache.insert(new.to_path_buf(), icon);
    }
    app.thumbnail_failures
        .retain(|(_, _, path, _), _| path != old && path != new);
    app.ordinary_icon_requests
        .retain(|(_, _, path)| path != old && path != new);
    for path in &mut app.cut_paths {
        if path == old {
            *path = new.to_path_buf();
        }
    }
    update
}

fn same_row(left: &FileRow, right: &FileRow) -> bool {
    left.id == right.id
        && left.name == right.name
        && left.kind == right.kind
        && left.size == right.size
        && left.group_header == right.group_header
        && left.group_label == right.group_label
        && left.group_detail == right.group_detail
        && left.group_count == right.group_count
        && left.selected == right.selected
        && left.focused == right.focused
}

fn preserve_images(row: &mut FileRow, previous: &HashMap<i32, FileRow>) {
    if let Some(old) = previous.get(&row.id)
        && old.icon.size().width > 0
        && (old.name == row.name
            || (old.is_directory && row.is_directory)
            || Path::new(old.name.as_str())
                .extension()
                .map(|ext| ext.to_ascii_lowercase())
                == Path::new(row.name.as_str())
                    .extension()
                    .map(|ext| ext.to_ascii_lowercase()))
    {
        row.icon = old.icon.clone();
        row.thumbnail_state = old.thumbnail_state;
    }
}

fn patch_rows(model: &ModelRc<FileRow>, next: Vec<FileRow>) {
    let rows = model
        .as_any()
        .downcast_ref::<VecModel<FileRow>>()
        .expect("directory projection uses mutable row models");
    while rows.row_count() > next.len() {
        rows.remove(rows.row_count() - 1);
    }
    for (index, row) in next.into_iter().enumerate() {
        if let Some(existing) = rows.row_data(index) {
            if !same_row(&existing, &row) {
                rows.set_row_data(index, row);
            }
        } else {
            rows.push(row);
        }
    }
}

fn patch_grid(model: &ModelRc<GridRow>, next: Vec<GridRow>) {
    let rows = model
        .as_any()
        .downcast_ref::<VecModel<GridRow>>()
        .expect("directory grid projection uses mutable row models");
    while rows.row_count() > next.len() {
        rows.remove(rows.row_count() - 1);
    }
    for (index, mut row) in next.into_iter().enumerate() {
        if let Some(existing) = rows.row_data(index) {
            patch_rows(&existing.entries, row.entries.iter().collect());
            row.entries = existing.entries.clone();
            if existing.group_header != row.group_header
                || existing.group_label != row.group_label
                || existing.group_detail != row.group_detail
                || existing.group_count != row.group_count
            {
                rows.set_row_data(index, row);
            }
        } else {
            rows.push(row);
        }
    }
}

fn patch_placed(model: &ModelRc<PositionedGridRow>, next: Vec<PositionedGridRow>) {
    let rows = model
        .as_any()
        .downcast_ref::<VecModel<PositionedGridRow>>()
        .expect("grouped directory projection uses mutable row models");
    while rows.row_count() > next.len() {
        rows.remove(rows.row_count() - 1);
    }
    for (index, row) in next.into_iter().enumerate() {
        if let Some(existing) = rows.row_data(index) {
            if existing.offset != row.offset
                || existing.extent != row.extent
                || existing.row.group_header != row.row.group_header
                || existing.row.group_label != row.row.group_label
                || existing.row.group_detail != row.row.group_detail
                || existing.row.group_count != row.row.group_count
                || !std::ptr::eq(existing.row.entries.as_any(), row.row.entries.as_any())
            {
                rows.set_row_data(index, row);
            }
        } else {
            rows.push(row);
        }
    }
}

pub(super) fn refresh_projection(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    scheduler: &ThumbnailScheduler,
) {
    {
        let sessions = WindowSessions::new(state.clone(), window_id);
        let app = sessions.lock().expect("app state mutex is not poisoned");
        let tab = app.active();
        let mode = app.active_view_mode();
        let texts = Texts::new(app.language);
        if mode.uses_grid_layout() {
            let previous = ui
                .get_grid_rows()
                .iter()
                .flat_map(|row| {
                    row.entries
                        .iter()
                        .map(|entry| (entry.id, entry))
                        .collect::<Vec<_>>()
                })
                .collect::<HashMap<_, _>>();
            let mut next = projected_directory_grid_rows(
                tab.visible_entries(),
                tab,
                texts,
                &app,
                grid_column_count_for_view(ui, mode),
                grid_thumbnail_request_px(mode, ui.window().scale_factor()),
            );
            for row in &mut next {
                if let Some(entries) = row.entries.as_any().downcast_ref::<VecModel<FileRow>>() {
                    for index in 0..entries.row_count() {
                        let mut entry = entries.row_data(index).unwrap();
                        preserve_images(&mut entry, &previous);
                        entries.set_row_data(index, entry);
                    }
                }
            }
            patch_grid(&ui.get_grid_rows(), next);
        } else {
            let previous = ui
                .get_files()
                .iter()
                .map(|row| (row.id, row))
                .collect::<HashMap<_, _>>();
            let mut next = projected_directory_rows(
                tab.visible_entries(),
                tab,
                texts,
                &app,
                file_thumbnail_request_px(mode, ui.window().scale_factor()),
            );
            for row in &mut next {
                preserve_images(row, &previous);
            }
            patch_rows(&ui.get_files(), next);
        }
    }
    rebuild_grid_entry_positions(ui, window_id);
    if ui.get_grouped_grid_enabled() {
        let mut extent = 0.0;
        let mut layout = Vec::new();
        for row in ui.get_grid_rows().iter() {
            let height = if row.group_header {
                group_header_height_for_detail(row.group_detail.as_str()) as f32
            } else {
                file_row_height(projected_view_mode(ui))
            };
            layout.push(PositionedGridRow {
                row,
                offset: extent,
                extent: height,
            });
            extent += height;
        }
        patch_placed(&ui.get_grouped_grid_layout(), layout.clone());
        ui.set_grouped_grid_extent(extent);
        let height = ui.get_file_viewport_height().max(0.0);
        let viewport = ui
            .get_file_viewport_y()
            .clamp(-(extent - height).max(0.0), 0.0);
        ui.set_file_viewport_y(viewport);
        let overscan = file_row_height(projected_view_mode(ui)) * 2.0;
        let start = (-viewport - overscan).max(0.0);
        let end = -viewport + height + overscan;
        patch_placed(
            &ui.get_grouped_grid_visible_rows(),
            layout
                .into_iter()
                .filter(|row| row.offset + row.extent > start && row.offset < end)
                .collect(),
        );
    }
    // Keep unaffected displayed images and models; only absent images enter the existing scheduler.
    let (tab_id, _, mut requests) = grid_thumbnail_plan(ui, state, window_id);
    requests.extend(list_icon_plan(ui, state, window_id).1);
    if tab_id.0 != 0 {
        scheduler.replace(tab_id, requests);
    }
}

pub(super) fn present(
    state: &SharedSessions,
    update: RenameUpdate,
    directory: &mpsc::Sender<DirectoryRequest>,
    network: &mpsc::SyncSender<DirectoryRequest>,
    search: &mpsc::Sender<EverythingRequest>,
    shortcuts: &mpsc::SyncSender<ShortcutRequest>,
    scheduler: &ThumbnailScheduler,
) {
    for tab_id in update.updated_tabs {
        let window_id = state.lock().ok().and_then(|app| {
            let window_id = app.window_for_tab(tab_id)?;
            (app.window(window_id)?.active_tab == tab_id).then_some(window_id)
        });
        if let Some(window_id) = window_id
            && let Some(ui) = window_ui(window_id)
        {
            refresh_projection(&ui, state, window_id, scheduler);
            let request_id = state
                .lock()
                .ok()
                .and_then(|app| app.tab(tab_id).map(TabSession::latest_request));
            if let Some(request_id) = request_id {
                submit_visible_shortcuts(shortcuts, state, &ui, tab_id, request_id);
            }
        }
    }
    for (tab_id, location) in update.reload_tabs {
        submit_location_navigation(
            directory,
            network,
            state,
            tab_id,
            location,
            NavigationKind::Refresh,
        );
    }
    for tab_id in update.search_tabs {
        let query = state
            .lock()
            .ok()
            .and_then(|app| app.tab(tab_id).map(|tab| tab.search_query.clone()));
        if let Some(query) = query {
            submit_search(search, state, None, tab_id, query);
            refresh_tab_window(state, tab_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EntryKind, file_operations::OperationResult};

    fn entry(id: u32, path: &str) -> FileEntry {
        let path = PathBuf::from(path);
        let name = path.file_name().unwrap().to_owned();
        FileEntry {
            id: EntryId(id),
            display_name: name.to_string_lossy().into_owned(),
            original_name: name,
            name_highlights: Vec::new(),
            path,
            kind: EntryKind::File,
            open_target: None,
            library_source_index: None,
            parent_display: String::new(),
            size_bytes: Some(7),
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
            webdav: None,
        }
    }

    fn image(px: u32) -> platform::windows::shell_icons::ShellIconRgba {
        platform::windows::shell_icons::ShellIconRgba {
            width: px,
            height: px,
            pixels: vec![255; (px * px * 4) as usize],
        }
    }

    fn state() -> AppState {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\photos")], 0, [0, 1, 2, 3]);
        app.update_directory_preference(PathBuf::from(r"C:\photos"), |view| {
            view.view_mode = ViewMode::MediumIcons
        });
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.load_state = LoadState::Complete;
        tab.replace_entries(vec![
            entry(1, r"C:\photos\a.png"),
            entry(2, r"C:\photos\b.png"),
        ]);
        tab.selected = vec![EntryId(1)];
        tab.focused = Some(EntryId(1));
        tab.selection_anchor = Some(EntryId(1));
        for name in ["a.png", "b.png"] {
            let key = (PathBuf::from(r"C:\photos").join(name), 48);
            app.thumbnail_cache.insert(key.clone(), image(48));
            app.thumbnail_cache_order.push_back(key);
        }
        app
    }

    #[test]
    fn issue_155_local_rename_retains_ids_selection_requests_images_and_other_tabs() {
        let mut app = state();
        let other = app.create_tab(PathBuf::from(r"C:\photos"));
        let tab = app.tab_mut(other).unwrap();
        tab.load_state = LoadState::Complete;
        tab.replace_entries(vec![entry(91, r"C:\photos\a.png")]);
        let before_request = app.tab(TabId(1)).unwrap().latest_request();
        let before_entries = app.tab(TabId(1)).unwrap().entries.clone();
        let update = apply_completed(
            &mut app,
            Path::new(r"C:\photos\a.png"),
            Path::new(r"C:\photos\z.png"),
        );
        assert!(update.reload_tabs.is_empty());
        assert_eq!(update.updated_tabs.len(), 2);
        let tab = app.tab(TabId(1)).unwrap();
        assert_eq!(tab.latest_request(), before_request);
        assert_eq!(tab.selected, [EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
        assert_eq!(tab.selection_anchor, Some(EntryId(1)));
        assert_eq!(
            tab.entries.iter().map(|entry| entry.id).collect::<Vec<_>>(),
            [EntryId(2), EntryId(1)]
        );
        assert_eq!(
            tab.visible_entry(EntryId(1)).unwrap().path,
            Path::new(r"C:\photos\z.png")
        );
        assert_eq!(
            app.tab(other)
                .unwrap()
                .visible_entry(EntryId(91))
                .unwrap()
                .display_name,
            "z.png"
        );
        assert!(!Arc::ptr_eq(&before_entries, &tab.entries));
        assert_eq!(before_entries[0].display_name, "a.png");
        assert!(
            app.thumbnail_cache
                .contains_key(&(PathBuf::from(r"C:\photos\z.png"), 48))
        );
        assert!(
            app.thumbnail_cache
                .contains_key(&(PathBuf::from(r"C:\photos\b.png"), 48))
        );
        assert!(
            !app.thumbnail_cache
                .contains_key(&(PathBuf::from(r"C:\photos\a.png"), 48))
        );
        let stale = IconEvent {
            tab_id: TabId(1),
            request_id: before_request,
            target: IconTarget::Entry(EntryId(1)),
            path: PathBuf::from(r"C:\photos\a.png"),
            icon: image(48),
            actual_thumbnail: true,
            requested_px: 48,
            type_key: None,
            thumbnail_failed: false,
            thumbnail_plan: None,
        };
        assert!(!icon_event_is_current(&app, &stale));
    }

    #[test]
    fn issue_155_directory_rename_moves_nested_search_scope_and_requeries() {
        let mut app = state();
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.page_source = PageSource::Search;
        tab.search_scope = SearchScope::Directory(PathBuf::from(r"C:\photos\old\child"));
        let update = apply_completed(
            &mut app,
            Path::new(r"C:\photos\old"),
            Path::new(r"C:\photos\new"),
        );
        assert_eq!(update.search_tabs, [TabId(1)]);
        assert_eq!(
            app.tab(TabId(1)).unwrap().search_scope,
            SearchScope::Directory(PathBuf::from(r"C:\photos\new\child"))
        );
        let scope = SearchScope::Library {
            display_name: "Library".into(),
            sources: vec![
                PathBuf::from(r"C:\photos\old\child"),
                PathBuf::from(r"D:\other"),
            ],
        };
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.search_scope = scope.clone();
        let update = apply_completed(
            &mut app,
            Path::new(r"C:\photos\old"),
            Path::new(r"C:\photos\new"),
        );
        assert_eq!(update.search_tabs, [TabId(1)]);
        assert_eq!(
            app.tab(TabId(1)).unwrap().search_scope,
            scope,
            "library configuration stays authoritative when the query is restarted"
        );
    }

    #[test]
    fn issue_155_extension_change_invalidates_only_the_renamed_entry() {
        let mut app = state();
        apply_completed(
            &mut app,
            Path::new(r"C:\photos\a.png"),
            Path::new(r"C:\photos\a.jpg"),
        );
        let tab = app.tab(TabId(1)).unwrap();
        let request = tab.latest_request();
        let renamed = tab.visible_entry(EntryId(1)).unwrap().clone();
        let unchanged = tab.visible_entry(EntryId(2)).unwrap().clone();
        assert!(thumbnail_request(&mut app, TabId(1), request, &renamed, 48).is_some());
        assert!(thumbnail_request(&mut app, TabId(1), request, &unchanged, 48).is_none());
        assert_eq!(app.thumbnail_cache.len(), 1);
    }

    #[test]
    fn issue_155_search_and_loading_are_requeried_without_reloading_complete_tabs() {
        let mut app = state();
        let loading = app.create_tab(PathBuf::from(r"C:\photos"));
        app.tab_mut(loading).unwrap().load_state = LoadState::Loading;
        let search = app.create_tab(PathBuf::from(r"C:\photos"));
        let tab = app.tab_mut(search).unwrap();
        tab.page_source = PageSource::Search;
        tab.search_scope = SearchScope::Global;
        let nested = app.create_tab(PathBuf::from(r"C:\photos\a.png\child"));
        let update = apply_completed(
            &mut app,
            Path::new(r"C:\photos\a.png"),
            Path::new(r"C:\photos\z.png"),
        );
        assert_eq!(update.updated_tabs, [TabId(1)]);
        assert_eq!(update.search_tabs, [search]);
        assert!(update.reload_tabs.contains(&(
            loading,
            NavigationLocation::Directory(PathBuf::from(r"C:\photos"))
        )));
        assert!(update.reload_tabs.contains(&(
            nested,
            NavigationLocation::Directory(PathBuf::from(r"C:\photos\z.png\child"))
        )));
    }

    #[test]
    fn issue_155_rename_completion_does_not_restart_unrelated_new_navigation() {
        let mut app = state();
        let tab = app.tab_mut(TabId(1)).unwrap();
        let (request, _) = tab.begin_navigation(
            NavigationLocation::Directory(PathBuf::from(r"D:\other")),
            NavigationKind::Normal,
        );
        assert_eq!(tab.load_state, LoadState::Loading);
        let update = apply_completed(
            &mut app,
            Path::new(r"C:\photos\a.png"),
            Path::new(r"C:\photos\z.png"),
        );
        assert!(update.updated_tabs.is_empty());
        assert!(update.reload_tabs.is_empty());
        let tab = app.tab(TabId(1)).unwrap();
        assert_eq!(tab.latest_request(), request);
        assert_eq!(tab.visible_path(), Some(Path::new(r"D:\other")));
    }

    #[test]
    fn issue_155_completion_does_not_enqueue_directory_refresh_but_keeps_deferred_changes() {
        for deferred in [false, true] {
            let mut app = state();
            let item = OperationItem::pending(
                Some(PathBuf::from(r"C:\photos\a.png")),
                Some(PathBuf::from(r"C:\photos\z.png")),
            );
            let id = app.operations.submit(
                OperationResource::Local,
                FileOperationKind::Rename,
                Some(TabId(1)),
                vec![item.clone()],
            );
            app.operations.start_next(OperationResource::Local).unwrap();
            mark_operation_running_if_ready(&mut app.operations, id);
            register_operation_directories(&mut app, FileOperationKind::Rename, &[item]);
            if deferred {
                app.deferred_watch_directories
                    .insert(PathBuf::from(r"C:\photos"));
            }
            let state = Arc::new(Mutex::new(app));
            let completion = finish_file_operation(
                &state,
                FileOperationEvent::Finished {
                    id,
                    result: OperationResult {
                        succeeded: vec![PathBuf::from(r"C:\photos\z.png")],
                        skipped: Vec::new(),
                        failed: Vec::new(),
                        affected_directories: vec![PathBuf::from(r"C:\photos")],
                    },
                    item_states: vec![(0, ItemState::Succeeded, None)],
                    completed_targets: vec![PathBuf::from(r"C:\photos\z.png")],
                    undo_items: None,
                    failed_undo_items: Vec::new(),
                },
            )
            .unwrap();
            assert_eq!(completion.affected.is_empty(), !deferred);
            assert_eq!(completion.rename_update.updated_tabs, [TabId(1)]);
            assert!(state.lock().unwrap().focus_after_refresh.is_empty());
        }
    }

    #[test]
    fn issue_155_grouped_rename_adds_and_removes_groups_without_resetting_models() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        let mut app = state();
        app.update_directory_preference(PathBuf::from(r"C:\photos"), |view| {
            view.group_field = GroupField::Kind
        });
        let state = Arc::new(Mutex::new(app));
        refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
        assert!(ui.get_grouped_grid_enabled());
        let grid = ui.get_grid_rows();
        let placed = ui.get_grouped_grid_layout();
        let visible = ui.get_grouped_grid_visible_rows();
        let (sender, _) = mpsc::channel();
        let scheduler = ThumbnailScheduler::new(sender);
        apply_completed(
            &mut state.lock().unwrap(),
            Path::new(r"C:\photos\a.png"),
            Path::new(r"C:\photos\a.jpg"),
        );
        refresh_projection(&ui, &state, WindowId(1), &scheduler);
        assert_eq!(grid.iter().filter(|row| row.group_header).count(), 2);
        let rows = grid
            .iter()
            .flat_map(|row| row.entries.iter().collect::<Vec<_>>())
            .collect::<Vec<_>>();
        assert_eq!(
            rows.iter()
                .find(|row| row.id == 1)
                .unwrap()
                .icon
                .size()
                .width,
            0
        );
        assert_eq!(
            rows.iter()
                .find(|row| row.id == 2)
                .unwrap()
                .icon
                .size()
                .width,
            48
        );
        apply_completed(
            &mut state.lock().unwrap(),
            Path::new(r"C:\photos\b.png"),
            Path::new(r"C:\photos\b.jpg"),
        );
        refresh_projection(&ui, &state, WindowId(1), &scheduler);
        assert_eq!(grid.iter().filter(|row| row.group_header).count(), 1);
        assert_eq!(
            grid.iter()
                .filter(|row| !row.group_header)
                .map(|row| row.entries.row_count())
                .sum::<usize>(),
            2
        );
        assert!(std::ptr::eq(grid.as_any(), ui.get_grid_rows().as_any()));
        assert!(std::ptr::eq(
            placed.as_any(),
            ui.get_grouped_grid_layout().as_any()
        ));
        assert!(std::ptr::eq(
            visible.as_any(),
            ui.get_grouped_grid_visible_rows().as_any()
        ));
    }

    #[test]
    fn issue_155_same_extension_keeps_resident_image_even_after_cache_eviction() {
        let old = FileRow {
            id: 1,
            name: "a.png".into(),
            icon: shell_icon_image(&image(48)),
            thumbnail_state: 2,
            ..FileRow::default()
        };
        let old_image = old.icon.clone();
        let previous = HashMap::from([(1, old)]);
        let mut same = FileRow {
            id: 1,
            name: "z.PNG".into(),
            thumbnail_state: 1,
            ..FileRow::default()
        };
        preserve_images(&mut same, &previous);
        assert_eq!(same.icon, old_image);
        let mut changed = FileRow {
            id: 1,
            name: "z.jpg".into(),
            thumbnail_state: 2,
            icon: shell_icon_image(&image(32)),
            ..FileRow::default()
        };
        preserve_images(&mut changed, &previous);
        assert_eq!(changed.icon.size().width, 32);
    }

    #[test]
    fn issue_155_projection_keeps_models_scroll_and_resident_images() {
        i_slint_backend_testing::init_no_event_loop();
        for mode in [ViewMode::MediumIcons, ViewMode::Details, ViewMode::Content] {
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
            let mut app = state();
            app.update_directory_preference(PathBuf::from(r"C:\photos"), |view| {
                view.view_mode = mode
            });
            let px = grid_thumbnail_request_px(mode, 1.0);
            for name in ["a.png", "b.png"] {
                app.thumbnail_cache
                    .insert((PathBuf::from(r"C:\photos").join(name), px), image(px));
            }
            let state = Arc::new(Mutex::new(app));
            refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
            ui.set_file_viewport_y(-37.0);
            let files = ui.get_files();
            let grids = ui.get_grid_rows();
            let nested = mode
                .uses_grid_layout()
                .then(|| grids.row_data(0).unwrap().entries);
            let old_rows = if let Some(model) = &nested {
                model.iter().collect::<Vec<_>>()
            } else {
                files.iter().collect()
            };
            let old_image = old_rows
                .iter()
                .find(|row| row.id == 2)
                .unwrap()
                .icon
                .clone();
            let request = state
                .lock()
                .unwrap()
                .tab(TabId(1))
                .unwrap()
                .latest_request();
            apply_completed(
                &mut state.lock().unwrap(),
                Path::new(r"C:\photos\a.png"),
                Path::new(r"C:\photos\z.png"),
            );
            let (sender, _) = mpsc::channel();
            let scheduler = ThumbnailScheduler::new(sender);
            refresh_projection(&ui, &state, WindowId(1), &scheduler);
            assert_eq!(ui.get_file_viewport_y(), -37.0);
            assert_eq!(
                state
                    .lock()
                    .unwrap()
                    .tab(TabId(1))
                    .unwrap()
                    .latest_request(),
                request
            );
            assert!(std::ptr::eq(files.as_any(), ui.get_files().as_any()));
            assert!(std::ptr::eq(grids.as_any(), ui.get_grid_rows().as_any()));
            let rows = if let Some(model) = nested {
                assert!(std::ptr::eq(
                    model.as_any(),
                    ui.get_grid_rows().row_data(0).unwrap().entries.as_any()
                ));
                model.iter().collect::<Vec<_>>()
            } else {
                files.iter().collect()
            };
            assert_eq!(rows.iter().find(|row| row.id == 2).unwrap().icon, old_image);
            assert_eq!(
                rows.iter().find(|row| row.id == 1).unwrap().name.as_str(),
                "z.png"
            );
            assert_eq!(
                rows.iter().find(|row| row.id == 1).unwrap().thumbnail_state,
                2
            );
        }
    }
}
