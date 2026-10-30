use super::*;

pub(super) fn wire_new(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let weak = ui.as_weak();
    let sender_for_new = sender.clone();
    let network_sender_for_new = network_sender.clone();
    let state_for_new = state.clone();
    ui.on_new_tab(move || {
        let reload = {
            let mut app = state_for_new
                .lock()
                .expect("app state mutex is not poisoned");
            if app.new_tab_opens_home {
                app.create_tab(NavigationLocation::Home);
                None
            } else if app.duplicate_active_tab().is_some() {
                None
            } else {
                let location = app
                    .active()
                    .current_location()
                    .clone()
                    .unwrap_or_else(|| NavigationLocation::Directory(initial_path()));
                let tab_id = app.create_tab(location.clone());
                Some((tab_id, location))
            }
        };
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_new);
        }
        if let Some((tab_id, location)) = reload {
            submit_location_navigation(
                &sender_for_new,
                &network_sender_for_new,
                &state_for_new,
                tab_id,
                location,
                NavigationKind::Refresh,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_new);
            }
        }
    });
}

pub(super) fn wire_tabs(
    ui: &AppWindow,
    state: &WindowSessions,
    workers: &WorkerSenders,
    confirmations: &ConfirmationWindows,
) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let action_workers = actions::ActionWorkers {
        directory: workers.directory.clone(),
        network_directory: workers.network_directory.clone(),
        operation: workers.operation.clone(),
    };
    let weak = ui.as_weak();
    let state_for_close = state.clone();
    ui.on_close_tab(move |tab_id| {
        let mut app = state_for_close
            .lock()
            .expect("app state mutex is not poisoned");
        let target = if tab_id < 0 {
            app.active_window_state().active_tab
        } else {
            TabId(tab_id as u32)
        };
        let before = app.active_window_state().active_tab;
        if app.active_window_state().tabs.contains_key(&target) {
            app.close_tab(target);
        }
        let active_changed = before != app.active_window_state().active_tab;
        drop(app);
        if let Some(ui) = weak.upgrade() {
            let regions = if active_changed {
                RefreshRegions::CONTENT
            } else {
                RefreshRegions::TABS
            };
            refresh_regions(&ui, &state_for_close, regions);
        }
    });

    let weak = ui.as_weak();
    let sender_for_restore = sender.clone();
    let network_sender_for_restore = network_sender.clone();
    let state_for_restore = state.clone();
    ui.on_restore_tab(move || {
        let restored = state_for_restore
            .lock()
            .expect("app state mutex is not poisoned")
            .restore_closed();
        if let Some((tab_id, location)) = restored {
            submit_location_navigation(
                &sender_for_restore,
                &network_sender_for_restore,
                &state_for_restore,
                tab_id,
                location,
                NavigationKind::Refresh,
            );
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_restore);
        }
    });

    let weak = ui.as_weak();
    let state_for_activate = state.clone();
    let switch_workers = action_workers.clone();
    ui.on_activate_tab(move |tab_id| {
        if let Some(ui) = weak.upgrade()
            && ui.get_column_dragging()
        {
            ui.invoke_cancel_column_drag();
        }
        let _ = actions::execute(
            &state_for_activate.shared,
            &switch_workers,
            actions::Action::SwitchTab {
                window_id: state_for_activate.window_id,
                tab_id: TabId(tab_id as u32),
            },
        );
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_activate);
        }
    });

    let state_for_tab_drag = state.clone();
    ui.on_begin_tab_drag(move |tab_id, source_index, press_x, press_y| {
        if let Ok(mut app) = state_for_tab_drag.lock() {
            let window_id = app.active_window;
            app.begin_tab_drag(
                window_id,
                TabId(tab_id as u32),
                source_index.max(0) as usize,
                press_x,
                press_y,
            );
        }
    });

    let weak = ui.as_weak();
    let state_for_tab_drag = state.clone();
    let senders_for_tab_drag = workers.clone();
    let confirmations_for_tab_drag = confirmations.clone();
    ui.on_update_tab_drag(move |x, y, strip_x, strip_width, viewport_x, tab_width| {
        let update = state_for_tab_drag.lock().ok().map(|mut app| {
            let before = app.tab_drag.map(|drag| drag.phase);
            let insertion = app.update_tab_drag(x, y, strip_x, strip_width, viewport_x, tab_width);
            let after = app.tab_drag.map(|drag| drag.phase);
            let became_dragging = !matches!(before, Some(TabDragPhase::Dragging { .. }))
                && matches!(after, Some(TabDragPhase::Dragging { .. }));
            (insertion, became_dragging)
        });
        let Some((insertion, became_dragging)) = update else {
            return -2;
        };
        if should_start_native_tab_drag(became_dragging, false)
            && let Some(ui) = weak.upgrade()
        {
            let hwnd = native_window_handle(&ui);
            let payload = state_for_tab_drag.lock().ok().and_then(|app| {
                app.tab_drag
                    .map(|drag| platform::windows::drag_drop::TabDragPayload {
                        process_id: std::process::id(),
                        source_hwnd: hwnd,
                        tab_id: drag.tab_id.0,
                    })
            });
            let image = native_tab_drag_image(&ui, &state_for_tab_drag.shared);
            ui.set_tab_dragging(true);
            ui.set_suppress_tab_click(true);
            // End Slint's gesture ownership before Windows starts the modal OLE drag loop.
            ui.invoke_release_tab_drag_pointer();
            let result = payload.zip(image).map(|(payload, image)| {
                platform::windows::drag_drop::begin_tab_drag(payload, &image)
            });
            let drop_point = result
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|result| result.dropped)
                .or_else(|| {
                    result
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .filter(|result| result.released_outside)
                        .and_then(|_| platform::windows::cursor_screen_position().ok())
                        .map(
                            |(screen_x, screen_y)| platform::windows::drag_drop::TabDropPoint {
                                target_hwnd: 0,
                                screen_x,
                                screen_y,
                            },
                        )
                });
            finish_native_tab_drag(
                &ui,
                state_for_tab_drag.window_id,
                drop_point,
                &senders_for_tab_drag,
                &confirmations_for_tab_drag,
                &state_for_tab_drag.shared,
            );
            return -2;
        }
        insertion.map(|index| index as i32).unwrap_or(-2)
    });

    let weak = ui.as_weak();
    let state_for_tab_drag = state.clone();
    ui.on_cancel_tab_drag(move || {
        if let Ok(mut app) = state_for_tab_drag.lock() {
            app.cancel_tab_drag();
        }
        if let Some(ui) = weak.upgrade() {
            ui.invoke_clear_tab_drag();
        }
        project_native_insertion_indicator(None, &state_for_tab_drag.shared);
    });
}
