use super::*;

pub(super) fn wire_settings(
    ui: &AppWindow,
    state: &WindowSessions,
    workers: &WorkerSenders,
    confirmations: &ConfirmationWindows,
) {
    let network_login_ui = workers.network_login.clone();
    let network_login_state = workers.network_login_state.clone();
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let everything_sender = workers.everything.clone();
    let delete_weak_for_language = confirmations.delete.clone();
    let conflict_weak_for_language = confirmations.conflict.clone();
    let exit_weak_for_language = confirmations.exit.clone();
    let network_login_for_language = network_login_ui.clone();
    let state_for_language = state.clone();
    ui.on_change_language(move |language| {
        let mut app = state_for_language
            .lock()
            .expect("app state mutex is not poisoned");
        app.language = if language == 1 {
            Language::English
        } else {
            Language::Chinese
        };
        drop(app);
        refresh_all_windows(&state_for_language, RefreshRegions::ALL);
        if let (Some(delete_ui), Some(conflict_ui), Some(exit_ui)) = (
            delete_weak_for_language.upgrade(),
            conflict_weak_for_language.upgrade(),
            exit_weak_for_language.upgrade(),
        ) {
            refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state_for_language);
        }
        if let Some(login) = network_login_for_language.upgrade() {
            login.set_dark_theme(
                state_for_language
                    .lock()
                    .map(|app| app.dark_theme())
                    .unwrap_or_default(),
            );
            if let Some(target) = network_login_state.lock().ok().and_then(|coordinator| {
                coordinator
                    .current
                    .as_ref()
                    .map(|value| value.target.clone())
            }) {
                configure_network_login_window(&login, &state_for_language, &target);
            }
        }
    });

    let weak_for_visibility = ui.as_weak();
    let state_for_visibility = state.clone();
    let sender_for_visibility = sender.clone();
    let network_sender_for_visibility = network_sender.clone();
    let state_for_quick_search = state.clone();
    ui.on_change_file_list_quick_search(move |enabled| {
        if let Ok(mut app) = state_for_quick_search.lock() {
            app.file_list_quick_search = enabled;
        }
        refresh_all_windows(&state_for_quick_search, RefreshRegions::SETTINGS);
    });

    let state_for_new_tab_setting = state.clone();
    ui.on_change_new_tab_opens_home(move |enabled| {
        if let Ok(mut app) = state_for_new_tab_setting.lock() {
            app.new_tab_opens_home = enabled;
        }
        refresh_all_windows(&state_for_new_tab_setting, RefreshRegions::SETTINGS);
    });

    let state_for_menu_backdrop = state.clone();
    ui.on_change_quick_menu_backdrop(move |enabled| {
        if let Ok(mut app) = state_for_menu_backdrop.lock() {
            app.quick_menu_backdrop = enabled;
        }
        refresh_all_windows(&state_for_menu_backdrop, RefreshRegions::SETTINGS);
    });

    let state_for_menu_opacity = state.clone();
    ui.on_change_quick_menu_backdrop_opacity(move |opacity| {
        if let Ok(mut app) = state_for_menu_opacity.lock() {
            app.quick_menu_backdrop_opacity = normalized_quick_menu_backdrop_opacity(opacity);
        }
        refresh_all_windows(&state_for_menu_opacity, RefreshRegions::SETTINGS);
    });

    ui.on_change_file_visibility(move |show_hidden, show_system| {
        let targets = {
            let mut app = state_for_visibility
                .lock()
                .expect("app state mutex is not poisoned");
            app.file_visibility = crate::domain::FileVisibility {
                show_hidden,
                show_system,
            };
            app.active_window_state()
                .tabs
                .values()
                .filter_map(|tab| tab.visible_path().map(|path| (tab.id, path.to_path_buf())))
                .collect::<Vec<_>>()
        };
        for (tab_id, path) in targets {
            submit_path_navigation(
                &sender_for_visibility,
                &network_sender_for_visibility,
                &state_for_visibility,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
        }
        refresh_all_windows(&state_for_visibility, RefreshRegions::SETTINGS);
        if let Some(ui) = weak_for_visibility.upgrade() {
            refresh_ui(&ui, &state_for_visibility);
        }
    });
    let delete_weak_for_theme = confirmations.delete.clone();
    let conflict_weak_for_theme = confirmations.conflict.clone();
    let exit_weak_for_theme = confirmations.exit.clone();
    let network_login_for_theme = network_login_ui.clone();
    let state_for_theme = state.clone();
    ui.on_change_theme(move |theme| {
        let mut app = state_for_theme
            .lock()
            .expect("app state mutex is not poisoned");
        app.theme_mode = match theme {
            1 => session_store::ThemeMode::Light,
            2 => session_store::ThemeMode::Dark,
            _ => session_store::ThemeMode::System,
        };
        drop(app);
        refresh_all_windows(&state_for_theme, RefreshRegions::SETTINGS);
        if let (Some(delete_ui), Some(conflict_ui), Some(exit_ui)) = (
            delete_weak_for_theme.upgrade(),
            conflict_weak_for_theme.upgrade(),
            exit_weak_for_theme.upgrade(),
        ) {
            refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state_for_theme);
        }
        if let Some(login) = network_login_for_theme.upgrade() {
            login.set_dark_theme(
                state_for_theme
                    .lock()
                    .map(|app| app.dark_theme())
                    .unwrap_or_default(),
            );
        }
    });

    let weak = ui.as_weak();
    let state_for_everything_config = state.clone();
    let everything_for_config = everything_sender.clone();
    ui.on_update_everything_config(move |path, instance| {
        let mut app = state_for_everything_config
            .lock()
            .expect("app state mutex is not poisoned");
        app.everything.config.executable_path =
            (!path.is_empty()).then(|| PathBuf::from(path.as_str()));
        app.everything.config.instance_name = instance.to_string();
        app.everything.config.verified_version = None;
        app.everything.generation = app.everything.generation.saturating_add(1);
        app.everything.status.clear();
        app.everything.busy = false;
        app.everything.folder_sizes_indexed = None;
        let config = app.everything.config.clone();
        drop(app);
        let _ = everything_for_config.send(EverythingRequest::Configure(config));
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_everything_config, RefreshRegions::SETTINGS);
        }
    });
    let weak = ui.as_weak();
    let everything_for_test = everything_sender.clone();
    let state_for_test = state.clone();
    ui.on_test_everything_connection(move || {
        let Some(generation) =
            begin_everything_operation(&state_for_test, EverythingOperation::Test)
        else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_test, RefreshRegions::SETTINGS);
        }
        let _ = everything_for_test.send(EverythingRequest::TestConnection(generation));
    });
    let weak = ui.as_weak();
    let everything_for_start = everything_sender.clone();
    let state_for_start = state.clone();
    ui.on_start_everything(move || {
        let Some(generation) =
            begin_everything_operation(&state_for_start, EverythingOperation::Start)
        else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_start, RefreshRegions::SETTINGS);
        }
        let _ = everything_for_start.send(EverythingRequest::Start(generation));
    });
    let weak = ui.as_weak();
    let everything_for_discover = everything_sender.clone();
    let state_for_discover = state.clone();
    ui.on_discover_everything(move || {
        let Some(generation) =
            begin_everything_operation(&state_for_discover, EverythingOperation::Discover)
        else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_discover, RefreshRegions::SETTINGS);
        }
        let _ = everything_for_discover.send(EverythingRequest::Discover(generation));
    });
    let weak = ui.as_weak();
    let everything_for_picker = everything_sender.clone();
    let state_for_picker = state.clone();
    ui.on_select_everything_program(move || {
        let Some(generation) =
            begin_everything_operation(&state_for_picker, EverythingOperation::Pick)
        else {
            return;
        };
        let owner_window = weak.upgrade().map_or(0, |ui| {
            refresh_regions(&ui, &state_for_picker, RefreshRegions::SETTINGS);
            native_window_handle(&ui)
        });
        let _ = everything_for_picker.send(EverythingRequest::PickExecutable {
            generation,
            owner_window,
        });
    });
    let weak = ui.as_weak();
    let state_for_download = state.clone();
    ui.on_download_everything(move || {
        if let Err(error) = platform::open_url("https://www.voidtools.com/downloads/") {
            state_for_download
                .lock()
                .expect("app state mutex is not poisoned")
                .everything
                .status = error.to_string();
            if let Some(ui) = weak.upgrade() {
                refresh_regions(&ui, &state_for_download, RefreshRegions::SETTINGS);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_settings = state.clone();
    ui.on_open_settings(move || {
        state_for_settings
            .lock()
            .expect("app state mutex is not poisoned")
            .open_settings();
        refresh_system_integration_state(state_for_settings.shared.clone());
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_settings);
        }
    });
}
