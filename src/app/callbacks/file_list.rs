use super::*;

pub(super) fn wire_viewport(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let everything_sender = workers.everything.clone();
    let thumbnail_scheduler = workers.thumbnail.clone();
    let shortcut_sender = workers.shortcut.clone();
    let state_for_folder_range = state.clone();
    let everything_for_folder_range = everything_sender.clone();
    let shortcut_for_folder_range = shortcut_sender.clone();
    let thumbnails_for_folder_range = thumbnail_scheduler.clone();
    let weak_for_folder_range = ui.as_weak();
    let weak_for_grouped_grid = ui.as_weak();
    ui.on_refresh_grouped_grid_viewport(move || {
        if let Some(ui) = weak_for_grouped_grid.upgrade() {
            refresh_grouped_grid_viewport(&ui);
        }
    });
    ui.on_request_folder_size_range(move |viewport_y, viewport_height| {
        let target = state_for_folder_range.lock().ok().and_then(|app| {
            let tab = app.active();
            (tab.page_source == PageSource::Directory).then_some((tab.id, tab.latest_request()))
        });
        if let Some((tab_id, request_id)) = target {
            submit_visible_folder_sizes(
                &everything_for_folder_range,
                &state_for_folder_range.shared,
                tab_id,
                request_id,
                viewport_y,
                viewport_height,
            );
            if let Some(ui) = weak_for_folder_range.upgrade() {
                request_visible_file_images(
                    &ui,
                    &state_for_folder_range.shared,
                    state_for_folder_range.window_id,
                    &thumbnails_for_folder_range,
                );
                submit_visible_shortcuts(
                    &shortcut_for_folder_range,
                    &state_for_folder_range.shared,
                    &ui,
                    tab_id,
                    request_id,
                );
            }
        }
    });
}

pub(super) fn wire_open(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let weak = ui.as_weak();
    let sender_for_entry = sender.clone();
    let network_sender_for_entry = network_sender.clone();
    let state_for_entry = state.clone();
    ui.on_open_entry(move |entry_id| {
        let target = {
            let app = state_for_entry
                .lock()
                .expect("app state mutex is not poisoned");
            let entry_id = if entry_id < 0 {
                app.active().focused
            } else {
                Some(EntryId(entry_id as u32))
            };
            entry_id.and_then(|entry_id| {
                let entry = app.active().visible_entry(entry_id)?.clone();
                Some((app.active_window_state().active_tab, entry))
            })
        };
        if let Some((tab_id, entry)) = target.as_ref()
            && entry.webdav.is_some()
        {
            webdav_ui::open_entry(&state_for_entry, &sender_for_entry, *tab_id, entry);
            if let Some(ui) = weak.upgrade() {
                let regions = if entry.kind == crate::domain::EntryKind::Directory {
                    RefreshRegions::CONTENT
                } else {
                    RefreshRegions::STATUS
                };
                refresh_regions(&ui, &state_for_entry, regions);
            }
            return;
        }
        if let Some((tab_id, entry)) = target {
            let is_directory = entry.kind == crate::domain::EntryKind::Directory;
            let target = entry.open_target.unwrap_or(entry.path);
            if is_directory {
                submit_path_navigation(
                    &sender_for_entry,
                    &network_sender_for_entry,
                    &state_for_entry,
                    tab_id,
                    target,
                    NavigationKind::Normal,
                );
            } else {
                thread::spawn(move || {
                    if let Err(error) = platform::open_path(&target) {
                        eprintln!("unable to open file: {error}");
                    }
                });
            }
            if is_directory && let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_entry);
            }
        }
    });
}

pub(super) fn wire_selection(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let action_workers = actions::ActionWorkers {
        directory: workers.directory.clone(),
        network_directory: workers.network_directory.clone(),
        operation: workers.operation.clone(),
    };
    let weak = ui.as_weak();
    let sender_for_activate_entry = sender.clone();
    let network_sender_for_activate_entry = network_sender.clone();
    let state_for_activate_entry = state.clone();
    let last_click = std::rc::Rc::new(RefCell::new(None::<(TabId, ActivateTarget, Instant)>));
    ui.on_activate_entry(move |entry_id, toggle, extend| {
        let entry_id = EntryId(entry_id as u32);
        let now = Instant::now();
        let (tab_id, target) = {
            let app = state_for_activate_entry
                .lock()
                .expect("app state mutex is not poisoned");
            (
                app.active_window_state().active_tab,
                app.active()
                    .visible_entry(entry_id)
                    .map(ActivateTarget::from),
            )
        };
        let double_click_interval = platform::double_click_interval();
        let should_open = last_click.borrow().as_ref().is_some_and(
            |(previous_tab, previous_target, previous_time)| {
                *previous_tab == tab_id
                    && target.as_ref() == Some(previous_target)
                    && now.saturating_duration_since(*previous_time) <= double_click_interval
            },
        ) && target.is_some();
        *last_click.borrow_mut() = if should_open {
            None
        } else {
            target.map(|target| (tab_id, target, now))
        };

        if should_open {
            let target = {
                let app = state_for_activate_entry
                    .lock()
                    .expect("app state mutex is not poisoned");
                app.active()
                    .visible_entry(entry_id)
                    .map(|entry| (app.active_window_state().active_tab, entry.clone()))
            };
            if let Some((tab_id, entry)) = target {
                let is_directory = entry.kind == crate::domain::EntryKind::Directory;
                if entry.webdav.is_some() {
                    webdav_ui::open_entry(
                        &state_for_activate_entry,
                        &sender_for_activate_entry,
                        tab_id,
                        &entry,
                    );
                } else if entry.kind == crate::domain::EntryKind::Directory {
                    submit_path_navigation(
                        &sender_for_activate_entry,
                        &network_sender_for_activate_entry,
                        &state_for_activate_entry,
                        tab_id,
                        entry.open_target.unwrap_or(entry.path),
                        NavigationKind::Normal,
                    );
                } else {
                    let path = entry.open_target.unwrap_or(entry.path);
                    thread::spawn(move || {
                        if let Err(error) = platform::open_path(&path) {
                            eprintln!("unable to open file: {error}");
                        }
                    });
                }
                if let Some(ui) = weak.upgrade() {
                    let regions = if is_directory {
                        RefreshRegions::CONTENT
                    } else {
                        RefreshRegions::STATUS
                    };
                    refresh_regions(&ui, &state_for_activate_entry, regions);
                }
            }
            return;
        }

        let changed_rows = {
            let mut app = state_for_activate_entry
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) else {
                return;
            };
            mutate_selection(tab, |tab| tab.select_entry(entry_id, toggle, extend))
        };
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_activate_entry, tab_id, &changed_rows);
            update_selection_summary(&ui, &state_for_activate_entry);
        }
    });
    let weak = ui.as_weak();
    let state_for_select = state.clone();
    let selection_workers = action_workers.clone();
    ui.on_select_entry(move |entry_id, toggle, extend| {
        let Ok(target) = actions::target(&state_for_select) else {
            return;
        };
        let previous = state_for_select.shared.lock().ok().and_then(|app| {
            app.tab(target.tab_id)
                .map(|tab| (tab.selected.clone(), tab.focused))
        });
        if actions::execute(
            &state_for_select.shared,
            &selection_workers,
            actions::Action::Select {
                target: actions::EntryTarget {
                    tab: target,
                    entry_id: EntryId(entry_id as u32),
                },
                toggle,
                extend,
            },
        )
        .is_err()
        {
            return;
        }
        let changed = state_for_select
            .shared
            .lock()
            .ok()
            .and_then(|app| {
                app.tab(target.tab_id).map(|tab| match &previous {
                    Some((selected, focused)) => selection_changed_rows(selected, *focused, tab),
                    None => tab.selected.iter().copied().chain(tab.focused).collect(),
                })
            })
            .unwrap_or_default();
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_select, target.tab_id, &changed);
            update_selection_summary(&ui, &state_for_select);
        }
    });

    let weak = ui.as_weak();
    let state_for_clear = state.clone();
    ui.on_clear_selection(move || {
        let update = mutate_active_selection(&state_for_clear, TabSession::clear_selection);
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_clear, tab_id, &changed);
            update_selection_summary(&ui, &state_for_clear);
        }
    });

    let weak = ui.as_weak();
    let state_for_all = state.clone();
    ui.on_select_all(move || {
        let update = mutate_active_selection(&state_for_all, TabSession::select_all);
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_all, tab_id, &changed);
            update_selection_summary(&ui, &state_for_all);
        }
    });

    let weak = ui.as_weak();
    let state_for_focus = state.clone();
    ui.on_move_focus(move |delta, extend| {
        let update = mutate_active_selection(&state_for_focus, |tab| {
            tab.move_focus(delta as isize, extend);
        });
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_focus, tab_id, &changed);
            update_selection_summary(&ui, &state_for_focus);
        }
    });

    let weak = ui.as_weak();
    let state_for_boundary = state.clone();
    ui.on_focus_boundary(move |last, extend| {
        let update = mutate_active_selection(&state_for_boundary, |tab| {
            tab.focus_boundary(last, extend);
        });
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_boundary, tab_id, &changed);
            update_selection_summary(&ui, &state_for_boundary);
        }
    });

    let weak = ui.as_weak();
    let state_for_toggle = state.clone();
    ui.on_toggle_focused(move || {
        let update = mutate_active_selection(&state_for_toggle, TabSession::toggle_focused);
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_toggle, tab_id, &changed);
            update_selection_summary(&ui, &state_for_toggle);
        }
    });
}

pub(super) fn wire_layout(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let everything_sender = workers.everything.clone();
    let thumbnail_scheduler = workers.thumbnail.clone();
    let weak = ui.as_weak();
    let state_for_sort = state.clone();
    let everything_for_sort = everything_sender.clone();
    ui.on_change_sort(move |field| {
        let field = match field {
            1 => SortField::Kind,
            2 => SortField::Size,
            3 => SortField::Modified,
            _ => SortField::Name,
        };
        let mut app = state_for_sort
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        let mut folder_queries = Vec::new();
        let search_query = if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
            if tab.page_source == PageSource::Search {
                tab.set_search_sort(field);
                Some(tab.search_query.clone())
            } else {
                cancel_folder_sizes(tab);
                tab.set_sort(field);
                if tab.sort_field == SortField::Size {
                    let request_id = tab.latest_request();
                    folder_queries = with_folder_scheduler(tab, |scheduler, entries| {
                        scheduler.begin_complete_sort(request_id, entries)
                    });
                }
                None
            }
        } else {
            None
        };
        let directory_sort = app
            .tab(tab_id)
            .filter(|tab| tab.page_source == PageSource::Directory)
            .and_then(|tab| {
                tab.visible_path()
                    .map(|path| (path.to_path_buf(), tab.sort_field, tab.sort_direction))
            });
        if let Some((path, field, direction)) = directory_sort {
            app.update_directory_preference(path, |preference| {
                preference.sort_field = field;
                preference.sort_direction = direction;
            });
        }
        drop(app);
        submit_folder_size_queries(
            &everything_for_sort,
            &state_for_sort.shared,
            tab_id,
            folder_queries,
        );
        if let Some(query) = search_query
            && let Some(ui) = weak.upgrade()
        {
            submit_search(
                &everything_for_sort,
                &state_for_sort,
                Some(&ui),
                tab_id,
                query,
            );
        }
        if let Some(ui) = weak.upgrade() {
            refresh_view_windows(&state_for_sort, Some(&ui));
        }
    });

    let weak = ui.as_weak();
    let state_for_view = state.clone();
    let thumbnails_for_view = thumbnail_scheduler.clone();
    ui.on_change_view_mode(move |mode| {
        if let Some(ui) = weak.upgrade()
            && ui.get_column_dragging()
        {
            ui.invoke_cancel_column_drag();
        }
        let mode = view_mode_from_ui(mode);
        let anchor = weak
            .upgrade()
            .and_then(|ui| capture_zoom_anchor(&ui, 16.0, 0.0));
        set_view_mode(&state_for_view, mode);
        if let Some(ui) = weak.upgrade() {
            refresh_view_windows(&state_for_view, Some(&ui));
            restore_zoom_anchor(&ui, anchor);
            request_visible_file_images(
                &ui,
                &state_for_view.shared,
                state_for_view.window_id,
                &thumbnails_for_view,
            );
        }
    });
    let state_for_columns = state.clone();
    ui.on_begin_column_drag(move |kind, press_x, press_y| {
        if let Ok(mut app) = state_for_columns.lock() {
            let window_id = app.active_window;
            app.begin_column_drag(window_id, kind as u8, press_x, press_y);
        }
    });
    let state_for_columns = state.clone();
    ui.on_update_column_drag(move |x, y, header_x, header_y, header_width, viewport_x| {
        let Ok(mut app) = state_for_columns.lock() else {
            return -2;
        };
        let slot = app.update_column_drag(x, y, header_x, header_y, header_width, viewport_x);
        match app.column_drag.map(|drag| drag.phase) {
            Some(ColumnDragPhase::Dragging { .. }) => slot.map(|slot| slot as i32).unwrap_or(-1),
            _ => -2,
        }
    });
    let weak = ui.as_weak();
    let state_for_columns = state.clone();
    ui.on_end_column_drag(move |valid_release| {
        let changed = state_for_columns
            .lock()
            .is_ok_and(|mut app| app.finish_column_drag(valid_release));
        if let Some(ui) = weak.upgrade() {
            ui.invoke_clear_column_drag();
            if changed {
                refresh_regions(&ui, &state_for_columns, RefreshRegions::COLUMNS);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_columns = state.clone();
    ui.on_cancel_column_drag(move || {
        if let Ok(mut app) = state_for_columns.lock() {
            app.cancel_column_drag();
        }
        if let Some(ui) = weak.upgrade() {
            ui.invoke_clear_column_drag();
        }
    });
    let state_for_column_widths = state.clone();
    ui.on_resize_column(move |kind, width| {
        if !(0..4).contains(&kind) || !width.is_finite() {
            return;
        }
        let width = width.round().clamp(64.0, 4_096.0) as u32;
        let mut app = state_for_column_widths
            .lock()
            .expect("app state mutex is not poisoned");
        if app.active().page_source == PageSource::Search {
            app.search_view.columns.widths[kind as usize] = width;
        } else {
            app.update_active_column_layout(|layout| layout.widths[kind as usize] = width);
        }
        drop(app);
    });
}
