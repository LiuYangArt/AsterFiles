use super::*;

pub(super) fn wire_navigation(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let network_login_ui = workers.network_login.clone();
    let network_login_state = workers.network_login_state.clone();
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let action_workers = actions::ActionWorkers {
        directory: workers.directory.clone(),
        network_directory: workers.network_directory.clone(),
        operation: workers.operation.clone(),
    };
    let weak = ui.as_weak();
    let sender_for_back = sender.clone();
    let network_sender_for_back = network_sender.clone();
    let state_for_back = state.clone();
    ui.on_navigate_back(move || {
        let (restored, target) = {
            let mut app = state_for_back
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            if app
                .active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .is_some_and(TabSession::restore_successful_location)
            {
                (true, None)
            } else {
                (false, app.active().back_target().map(|path| (tab_id, path)))
            }
        };
        let navigated = target.is_some();
        if let Some((tab_id, path)) = target {
            submit_location_navigation(
                &sender_for_back,
                &network_sender_for_back,
                &state_for_back,
                tab_id,
                path,
                NavigationKind::Back,
            );
        }
        if (restored || navigated)
            && let Some(ui) = weak.upgrade()
        {
            refresh_ui(&ui, &state_for_back);
        }
    });

    let weak = ui.as_weak();
    let sender_for_forward = sender.clone();
    let network_sender_for_forward = network_sender.clone();
    let state_for_forward = state.clone();
    ui.on_navigate_forward(move || {
        let target = {
            let app = state_for_forward
                .lock()
                .expect("app state mutex is not poisoned");
            app.active()
                .forward_target()
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_location_navigation(
                &sender_for_forward,
                &network_sender_for_forward,
                &state_for_forward,
                tab_id,
                path,
                NavigationKind::Forward,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_forward);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_history = sender.clone();
    let network_sender_for_history = network_sender.clone();
    let state_for_history = state.clone();
    ui.on_navigate_history(move |is_back, index| {
        let target = {
            let app = state_for_history
                .lock()
                .expect("app state mutex is not poisoned");
            let tab = app.active();
            let stack = if is_back {
                &tab.back_history()
            } else {
                &tab.forward_history()
            };
            let index = usize::try_from(index).ok();
            index
                .and_then(|index| stack.iter().rev().nth(index))
                .cloned()
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_location_navigation(
                &sender_for_history,
                &network_sender_for_history,
                &state_for_history,
                tab_id,
                path,
                if is_back {
                    NavigationKind::Back
                } else {
                    NavigationKind::Forward
                },
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_history);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_up = sender.clone();
    let network_sender_for_up = network_sender.clone();
    let state_for_up = state.clone();
    ui.on_navigate_up(move || {
        let target = {
            let app = state_for_up
                .lock()
                .expect("app state mutex is not poisoned");
            app.active()
                .visible_path()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_path_navigation(
                &sender_for_up,
                &network_sender_for_up,
                &state_for_up,
                tab_id,
                path,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_up);
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_refresh = state.clone();
    let refresh_workers = action_workers.clone();
    ui.on_refresh(move || {
        let Ok(target) = actions::target(&state_for_refresh) else {
            return;
        };
        let home = state_for_refresh.shared.lock().is_ok_and(|app| {
            app.tab(target.tab_id)
                .is_some_and(|tab| matches!(tab.visible_location(), Some(NavigationLocation::Home)))
        });
        if actions::execute(
            &state_for_refresh.shared,
            &refresh_workers,
            actions::Action::Refresh { target },
        )
        .is_ok()
        {
            if home {
                reload_quick_access(weak.clone(), state_for_refresh.shared.clone());
                reload_drives(weak.clone(), state_for_refresh.shared.clone());
            }
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_refresh);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_access = state.clone();
    let sender_for_access = sender.clone();
    let network_sender_for_access = network_sender.clone();
    let network_login_for_access = network_login_ui.clone();
    let network_login_state_for_access = network_login_state.clone();
    ui.on_request_folder_access(move || {
        let target = {
            let app = state_for_access
                .lock()
                .expect("app state mutex is not poisoned");
            (app.active().load_state == LoadState::PermissionDenied)
                .then(|| {
                    app.active()
                        .requested_location()
                        .as_ref()
                        .and_then(NavigationLocation::directory_path)
                        .map(Path::to_path_buf)
                        .map(|path| {
                            (
                                app.active_window,
                                app.active_window_state().active_tab,
                                app.active().latest_request(),
                                path,
                            )
                        })
                })
                .flatten()
        };
        if let Some((window_id, tab_id, failed_request_id, target)) = target {
            if crate::network::is_unc_path(&target) {
                network_login_state_for_access
                    .lock()
                    .expect("network login coordinator mutex is not poisoned")
                    .begin(window_id, tab_id, failed_request_id, target.clone());
                if let (Some(login), Some(ui)) =
                    (network_login_for_access.upgrade(), weak.upgrade())
                {
                    configure_network_login_window(&login, &state_for_access, &target);
                    login.set_username("".into());
                    login.set_password("".into());
                    login.set_remember(false);
                    login.set_conflict(false);
                    login.set_busy(false);
                    show_centered_dialog(&ui, &login);
                }
                return;
            }
            let state = state_for_access.clone();
            let sender = sender_for_access.clone();
            let network_sender = network_sender_for_access.clone();
            thread::spawn(move || {
                let result =
                    platform::request_folder_access(&target).map_err(|error| error.to_string());
                if result.is_ok() {
                    let _ = slint::invoke_from_event_loop(move || {
                        submit_path_navigation(
                            &sender,
                            &network_sender,
                            &state,
                            tab_id,
                            target,
                            NavigationKind::Refresh,
                        );
                    });
                } else if let Err(error) = result {
                    eprintln!("unable to request folder access through Windows: {error}");
                }
            });
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_access);
        }
    });
}
