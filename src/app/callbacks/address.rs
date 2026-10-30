use super::*;

pub(super) fn wire_path(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let everything_sender = workers.everything.clone();
    let action_workers = actions::ActionWorkers {
        directory: workers.directory.clone(),
        network_directory: workers.network_directory.clone(),
        operation: workers.operation.clone(),
    };
    let weak = ui.as_weak();
    let state_for_path = state.clone();
    let everything_for_accept = everything_sender.clone();
    let path_workers = action_workers.clone();
    ui.on_navigate_path(move |path| {
        let Ok(action_target) = actions::target(&state_for_path) else {
            return;
        };
        let remote = state_for_path.lock().ok().is_some_and(|app| {
            matches!(
                app.active().visible_location(),
                Some(NavigationLocation::WebDav(_))
            )
        });
        if remote {
            return;
        }
        let input = path.to_string();
        let (tab_id, mode) = {
            let app = state_for_path
                .lock()
                .expect("app state mutex is not poisoned");
            (
                app.active_window_state().active_tab,
                app.active().address_mode,
            )
        };
        let query = if mode == AddressMode::Smart {
            let mut app = state_for_path
                .lock()
                .expect("app state mutex is not poisoned");
            app.active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .map(|tab| {
                    tab.update_address_input(input.clone());
                    tab.search_query.clone()
                })
                .unwrap_or_else(|| input.clone())
        } else {
            input.clone()
        };
        let target = platform::windows::address_path::normalize_address_path(path.as_str());
        let state_for_validation = state_for_path.clone();
        let workers_for_validation = path_workers.clone();
        let everything_for_validation = everything_for_accept.clone();
        let weak = weak.clone();
        thread::spawn(move || {
            let target =
                platform::windows::network::network_drive_to_unc(&target).unwrap_or(target);
            let directory =
                (crate::network::is_unc_path(&target) || target.is_dir()).then_some(target);
            let _ = weak.upgrade_in_event_loop(move |ui| {
                address_edit::finish_validation(
                    &ui,
                    &state_for_validation,
                    &workers_for_validation,
                    &everything_for_validation,
                    action_target,
                    directory,
                    query,
                );
            });
        });
    });
}

pub(super) fn wire_search(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let everything_sender = workers.everything.clone();
    let thumbnail_scheduler = workers.thumbnail.clone();
    let weak = ui.as_weak();
    let state_for_search_edit = state.clone();
    ui.on_begin_search(move || {
        let mut app = state_for_search_edit
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
            tab.begin_smart_address_edit();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_search_edit, RefreshRegions::NAVIGATION);
            ui.invoke_focus_address_editor();
        }
    });

    let state_for_changed = state.clone();
    let everything_for_changed = everything_sender.clone();
    let weak_for_changed = ui.as_weak();
    ui.on_address_input_changed(move |value| {
        let input = value.to_string();
        let (tab_id, is_search) = {
            let mut app = state_for_changed
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .expect("active tab exists");
            tab.update_address_input(input.clone());
            (tab_id, tab.address_mode == AddressMode::Smart)
        };
        if is_search {
            schedule_search(
                everything_for_changed.clone(),
                state_for_changed.clone(),
                weak_for_changed.clone(),
                tab_id,
                input,
            );
        }
    });

    let state_for_search_depth = state.clone();
    let everything_for_search_depth = everything_sender.clone();
    let weak_for_search_depth = ui.as_weak();
    ui.on_toggle_search_depth(move || {
        let (tab_id, query, changed) = {
            let mut app = state_for_search_depth
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .expect("active tab exists");
            let changed = tab.address_mode == AddressMode::Smart && tab.toggle_search_depth();
            (tab_id, tab.search_query.clone(), changed)
        };
        if changed {
            submit_search(
                &everything_for_search_depth,
                &state_for_search_depth,
                weak_for_search_depth.upgrade().as_ref(),
                tab_id,
                query,
            );
            if let Some(ui) = weak_for_search_depth.upgrade() {
                ui.invoke_focus_address_editor();
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_cancel_edit = state.clone();
    ui.on_cancel_address_edit(move || {
        let mut app = state_for_cancel_edit
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
            tab.cancel_address_edit();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_cancel_edit);
        }
    });

    let weak = ui.as_weak();
    let state_for_next_search_page = state.clone();
    let everything_for_next_search_page = everything_sender.clone();
    let images_for_next_search_page = thumbnail_scheduler.clone();
    ui.on_request_search_position(move |requested_scroll| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let (tab_id, total, view_mode) = {
            let app = state_for_next_search_page
                .lock()
                .expect("app state mutex is not poisoned");
            let tab = app.active();
            let mode = app.active_view_mode();
            (
                app.active_window_state().active_tab,
                tab.search_total.unwrap_or(0),
                mode,
            )
        };
        let columns = ui.get_grid_column_count().max(1) as usize;
        let maximum =
            search_logical_maximum(total, view_mode, columns, ui.get_file_viewport_height());
        let scroll = requested_scroll.clamp(-maximum, 0.0);
        ui.set_search_scroll_y(scroll);
        let index = search_result_index_at_scroll(scroll, total, view_mode, columns);
        let window = search_window_for_index(index, total, columns);
        ui.set_file_viewport_y(search_window_viewport_y(scroll, window, view_mode, columns));
        submit_search_page(
            &everything_for_next_search_page,
            &state_for_next_search_page,
            tab_id,
            index,
        );
        refresh_tab_window(&state_for_next_search_page.shared, tab_id);
        request_visible_file_images(
            &ui,
            &state_for_next_search_page.shared,
            state_for_next_search_page.window_id,
            &images_for_next_search_page,
        );
    });
}

pub(super) fn wire_breadcrumb(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let weak = ui.as_weak();
    let sender_for_breadcrumb = sender.clone();
    let network_sender_for_breadcrumb = network_sender.clone();
    let state_for_breadcrumb = state.clone();
    ui.on_navigate_breadcrumb(move |index, generation| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        if generation != ui.get_breadcrumb_generation() {
            return;
        }
        let target = {
            let app = state_for_breadcrumb
                .lock()
                .expect("app state mutex is not poisoned");
            breadcrumb::resolve_target(&ui, app.active(), index, generation)
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, location)) = target {
            if location
                .directory_path()
                .is_some_and(crate::network::is_unc_server_root)
            {
                platform::windows::network::record_runtime_event(
                    "network_device_navigation_submitted",
                );
            }
            submit_location_navigation(
                &sender_for_breadcrumb,
                &network_sender_for_breadcrumb,
                &state_for_breadcrumb,
                tab_id,
                location,
                NavigationKind::Normal,
            );
            refresh_ui(&ui, &state_for_breadcrumb);
        }
    });
}
