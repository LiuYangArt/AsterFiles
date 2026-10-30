use super::*;

pub(super) fn wire_network(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let network_discovery_sender = workers.network_discovery.clone();
    let weak_for_network = ui.as_weak();
    let discovery_sender_for_ui = network_discovery_sender.clone();
    let discovery_state_for_ui = state.clone();
    ui.on_toggle_network(move || {
        let expanded = weak_for_network
            .upgrade()
            .is_some_and(|ui| ui.get_network_expanded());
        let should_discover = expanded
            && discovery_state_for_ui
                .lock()
                .is_ok_and(|app| network_discovery_needed(&app, discovery_state_for_ui.window_id));
        if !expanded || should_discover {
            update_network_discovery(&discovery_state_for_ui, &discovery_sender_for_ui, expanded);
        }
    });
    let discovery_sender_for_refresh = network_discovery_sender.clone();
    let discovery_state_for_refresh = state.clone();
    ui.on_refresh_network(move || {
        update_network_discovery(
            &discovery_state_for_refresh,
            &discovery_sender_for_refresh,
            true,
        );
    });
}

pub(super) fn wire_locations(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let weak = ui.as_weak();
    let sender_for_sidebar = sender.clone();
    let network_sender_for_sidebar = network_sender.clone();
    let state_for_sidebar = state.clone();
    ui.on_navigate_sidebar(move |index| {
        if index == -1 {
            let tab_id = state_for_sidebar
                .lock()
                .ok()
                .map(|app| app.active_window_state().active_tab);
            if let Some(tab_id) = tab_id {
                submit_location_navigation(
                    &sender_for_sidebar,
                    &network_sender_for_sidebar,
                    &state_for_sidebar,
                    tab_id,
                    NavigationLocation::Home,
                    NavigationKind::Normal,
                );
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_sidebar);
                }
            }
            return;
        }
        let target = {
            let app = state_for_sidebar
                .lock()
                .expect("app state mutex is not poisoned");
            usize::try_from(index).ok().and_then(|index| {
                sidebar_navigation_target(&app, index)
                    .map(|path| (app.active_window_state().active_tab, path))
            })
        };
        if let Some((tab_id, path)) = target {
            submit_path_navigation(
                &sender_for_sidebar,
                &network_sender_for_sidebar,
                &state_for_sidebar,
                tab_id,
                path,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_sidebar);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_library = sender.clone();
    let state_for_library = state.clone();
    ui.on_navigate_library(move |stable_id| {
        let target = state_for_library.lock().ok().and_then(|app| {
            app.libraries
                .iter()
                .find(|library| library_stable_id(library.id.as_os_str()) == stable_id.as_str())
                .map(|library| {
                    (
                        app.active_window_state().active_tab,
                        LibraryLocationId::new(
                            library.id.as_os_str().to_owned(),
                            library.display_name.to_string_lossy().into_owned(),
                        ),
                    )
                })
        });
        if let Some((tab_id, library)) = target {
            submit_library_navigation(
                &sender_for_library,
                &state_for_library,
                tab_id,
                library,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_library);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_network_location = sender.clone();
    let network_sender_for_network_location = network_sender.clone();
    let state_for_network_location = state.clone();
    ui.on_navigate_network_location(move |stable_id| {
        let target = state_for_network_location.lock().ok().and_then(|app| {
            network_location_from_stable_id(&app, stable_id.as_str()).map(|location| {
                (
                    app.active_window_state().active_tab,
                    location.target.clone(),
                )
            })
        });
        match target {
            Some((tab_id, NetworkTarget::WindowsPath(path))) => {
                submit_path_navigation(
                    &sender_for_network_location,
                    &network_sender_for_network_location,
                    &state_for_network_location,
                    tab_id,
                    path,
                    NavigationKind::Normal,
                );
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_network_location);
                }
            }
            Some((tab_id, NetworkTarget::WebDav(connection))) => {
                submit_webdav_navigation(
                    &sender_for_network_location,
                    &state_for_network_location,
                    tab_id,
                    crate::domain::WebDavPlace {
                        connection_id: connection.connection_id,
                        revision: connection.revision,
                        root: connection.root.clone(),
                        path: connection.root,
                    },
                    NavigationKind::Normal,
                );
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_network_location);
                }
            }
            Some((_, NetworkTarget::ShellItemId(identity))) => {
                thread::spawn(move || {
                    if let Err(error) = platform::open_path(&identity) {
                        eprintln!("unable to open Windows network location: {error}");
                    }
                });
            }
            None => {}
        }
    });
}

pub(super) fn wire_home(ui: &AppWindow, state: &WindowSessions, workers: &WorkerSenders) {
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let state_for_home_select = state.clone();
    ui.on_select_home_item(move |stable_id| {
        if let Ok(mut app) = state_for_home_select.lock() {
            let tab_id = app.active_window_state().active_tab;
            if home_item_target(&app, stable_id.as_str()).is_some()
                || home_item_network_target(&app, stable_id.as_str()).is_some()
            {
                app.home_pages
                    .entry(tab_id)
                    .or_insert_with(HomePageState::expanded)
                    .selected = Some(stable_id.to_string());
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_home_section = state.clone();
    ui.on_toggle_home_section(move |section| {
        if let Ok(mut app) = state_for_home_section.lock() {
            let tab_id = app.active_window_state().active_tab;
            let page = app
                .home_pages
                .entry(tab_id)
                .or_insert_with(HomePageState::expanded);
            match section {
                0 => page.quick_access_expanded = !page.quick_access_expanded,
                1 => page.drives_expanded = !page.drives_expanded,
                2 => page.network_locations_expanded = !page.network_locations_expanded,
                _ => return,
            }
        }
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_home_section, RefreshRegions::HOME);
        }
    });

    let weak = ui.as_weak();
    let sender_for_home = sender.clone();
    let network_sender_for_home = network_sender.clone();
    let state_for_home = state.clone();
    ui.on_navigate_home_item(move |stable_id| {
        let target = state_for_home.lock().ok().and_then(|app| {
            let tab_id = app.active_window_state().active_tab;
            home_item_network_target(&app, stable_id.as_str())
                .map(|target| (tab_id, target))
                .or_else(|| {
                    home_item_target(&app, stable_id.as_str())
                        .map(|path| (tab_id, NetworkTarget::WindowsPath(path)))
                })
        });
        match target {
            Some((tab_id, NetworkTarget::WindowsPath(path))) => {
                submit_path_navigation(
                    &sender_for_home,
                    &network_sender_for_home,
                    &state_for_home,
                    tab_id,
                    path,
                    NavigationKind::Normal,
                );
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_home);
                }
            }
            Some((tab_id, NetworkTarget::WebDav(connection))) => {
                submit_webdav_navigation(
                    &sender_for_home,
                    &state_for_home,
                    tab_id,
                    crate::domain::WebDavPlace {
                        connection_id: connection.connection_id,
                        revision: connection.revision,
                        root: connection.root.clone(),
                        path: connection.root,
                    },
                    NavigationKind::Normal,
                );
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_home);
                }
            }
            Some((_, NetworkTarget::ShellItemId(identity))) => {
                thread::spawn(move || {
                    if let Err(error) = platform::open_path(&identity) {
                        eprintln!("unable to open Windows network location: {error}");
                    }
                });
            }
            None => {}
        }
    });
}
