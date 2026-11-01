//! Region-scoped window projection (#162). Callers declare which state changed; unrelated
//! updates never visit directory entries or replace their models.
use super::*;

#[derive(Clone, Copy)]
pub(super) struct RefreshRegions(u16);

impl RefreshRegions {
    pub(super) const FILES: Self = Self(1);
    pub(super) const NAVIGATION: Self = Self(2);
    pub(super) const TABS: Self = Self(4);
    pub(super) const SIDEBAR: Self = Self(8);
    pub(super) const HOME: Self = Self(16);
    pub(super) const SETTINGS: Self = Self(32);
    pub(super) const STATUS: Self = Self(64);
    pub(super) const COLUMNS: Self = Self(128);
    pub(super) const CONTENT: Self = Self(
        Self::FILES.0
            | Self::NAVIGATION.0
            | Self::TABS.0
            | Self::HOME.0
            | Self::STATUS.0
            | Self::COLUMNS.0,
    );
    pub(super) const LAYOUT: Self = Self(256);
    pub(super) const ALL: Self =
        Self(Self::CONTENT.0 | Self::SIDEBAR.0 | Self::SETTINGS.0 | Self::LAYOUT.0);

    fn intersects(self, region: Self) -> bool {
        self.0 & region.0 != 0
    }
}

impl std::ops::BitOr for RefreshRegions {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

pub(super) fn refresh_window_regions(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    regions: RefreshRegions,
) {
    if regions.intersects(RefreshRegions::FILES) {
        dismiss_stale_rename(ui, state, window_id);
        ui.invoke_cancel_internal_drag();
    }
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window) = app.window(window_id) else {
        return;
    };
    let Some(tab) = window.tabs.get(&window.active_tab) else {
        return;
    };
    let view_mode = app
        .view_mode_for_tab(tab.id)
        .unwrap_or(app.default_directory_view.view_mode);
    let logical_width = logical_window_width(ui);
    ui.set_window_width(logical_width);
    ui.set_home_content_width(home_content_width_for_window(logical_width));
    let tab_id = tab.id;
    let request_id = tab.latest_request();
    let projection_changed = ui.get_projected_file_tab_id() != tab.id.0 as i32
        || ui.get_projected_file_request_id() != tab.latest_request().0 as i32;
    if regions.intersects(RefreshRegions::FILES) {
        project_files(ui, &app, tab, window_id, view_mode, projection_changed);
    }
    if regions.intersects(RefreshRegions::NAVIGATION) {
        ui.set_accessibility_window_id(format!("window/{}", window_id.0).into());
        ui.set_accessibility_context(
            format!(
                "window/{}/tab/{}/request/{}",
                window_id.0,
                tab.id.0,
                tab.latest_request().0
            )
            .into(),
        );
        project_navigation(
            ui,
            state,
            &app,
            tab,
            window_id,
            view_mode,
            projection_changed,
        );
    }
    if regions.intersects(RefreshRegions::LAYOUT | RefreshRegions::NAVIGATION) {
        breadcrumb::project(ui, tab, app.language);
    }
    if regions.intersects(RefreshRegions::TABS) {
        project_tabs(ui, &app, window);
    }
    if regions.intersects(RefreshRegions::SIDEBAR) {
        project_sidebar(ui, &app, window_id);
    }
    if regions.intersects(RefreshRegions::HOME) {
        project_home(ui, &app, tab);
    }
    if regions.intersects(RefreshRegions::SETTINGS) {
        project_settings(ui, &app);
    }
    if regions.intersects(RefreshRegions::STATUS) {
        project_status(ui, &app, tab);
    }
    if regions.intersects(RefreshRegions::COLUMNS) {
        project_column_layout(ui, tab, &app);
    }
    drop(app);
    if regions.intersects(RefreshRegions::FILES) {
        apply_pending_rename_ui(ui, state, window_id);
    }
    if regions.intersects(RefreshRegions::NAVIGATION) {
        let invalidated = WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            runtimes.get_mut(&window_id).is_some_and(|runtime| {
                runtime
                    .quick_menu_popup
                    .session
                    .invalidate_request(window_id, tab_id, request_id)
            })
        });
        if invalidated {
            dismiss_quick_menu_session(window_id, false);
        }
    }
}

fn project_settings(ui: &AppWindow, app: &AppState) {
    let (folder_enabled, folder_needs_repair, folder_status, folder_path) =
        shell_integration_projection(app.integration.folder_status.as_ref(), app.language);
    let (win_e_enabled, win_e_needs_repair, win_e_status, win_e_path) =
        shell_integration_projection(app.integration.win_e_status.as_ref(), app.language);
    ui.set_folder_integration_enabled(folder_enabled);
    ui.set_folder_integration_needs_repair(folder_needs_repair);
    ui.set_win_e_integration_enabled(win_e_enabled);
    ui.set_win_e_integration_needs_repair(win_e_needs_repair);
    ui.set_system_integration_busy(app.integration.busy);
    ui.set_folder_integration_status(
        app.integration
            .folder_error
            .as_deref()
            .unwrap_or(&folder_status)
            .into(),
    );
    ui.set_win_e_integration_status(
        app.integration
            .win_e_error
            .as_deref()
            .unwrap_or(&win_e_status)
            .into(),
    );
    ui.set_folder_integration_bound_path(
        folder_path
            .map(|path| display_path(&path))
            .unwrap_or_default()
            .into(),
    );
    ui.set_win_e_integration_bound_path(
        win_e_path
            .map(|path| display_path(&path))
            .unwrap_or_default()
            .into(),
    );
    ui.set_language_mode(match app.language {
        Language::Chinese => 0,
        Language::English => 1,
    });
    ui.set_show_hidden_files(app.file_visibility.show_hidden);
    ui.set_show_system_files(app.file_visibility.show_system);
    ui.set_file_list_quick_search(app.file_list_quick_search);
    ui.set_new_tab_opens_home(app.new_tab_opens_home);
    ui.set_quick_menu_backdrop(app.quick_menu_backdrop);
    ui.set_quick_menu_backdrop_opacity(f32::from(app.quick_menu_backdrop_opacity));
    ui.set_everything_path(
        app.everything
            .config
            .executable_path
            .as_deref()
            .map(display_path)
            .unwrap_or_default()
            .into(),
    );
    ui.set_everything_instance(app.everything.config.instance_name.clone().into());
    ui.set_everything_status(app.everything.status.clone().into());
    ui.set_everything_busy(app.everything.busy);
    ui.set_app_version(env!("CARGO_PKG_VERSION").into());
    ui.set_theme_mode(match app.theme_mode {
        session_store::ThemeMode::System => 0,
        session_store::ThemeMode::Light => 1,
        session_store::ThemeMode::Dark => 2,
    });
    let dark_theme = app.dark_theme();
    ui.set_dark_theme(dark_theme);
    ui.set_insertion_indicator_width(platform::windows::tab_insertion_indicator::INDICATOR_WIDTH);
    let accent = if dark_theme {
        platform::windows::tab_insertion_indicator::DARK_ACCENT_ARGB
    } else {
        platform::windows::tab_insertion_indicator::LIGHT_ACCENT_ARGB
    };
    ui.set_insertion_indicator_color(Color::from_argb_encoded(accent));
    apply_ui_texts(ui, app.language);
}

fn project_home(ui: &AppWindow, app: &AppState, tab: &TabSession) {
    let home_page = app
        .home_pages
        .get(&tab.id)
        .cloned()
        .unwrap_or_else(HomePageState::expanded);
    ui.set_home_quick_access_expanded(home_page.quick_access_expanded);
    ui.set_home_drives_expanded(home_page.drives_expanded);
    ui.set_home_network_locations_expanded(home_page.network_locations_expanded);
    let home_columns = ui.get_home_column_count().max(1) as usize;
    let quick_access = app
        .sidebar
        .iter()
        .filter(|location| location.kind == KnownLocationKind::Pinned)
        .cloned()
        .collect::<Vec<_>>();
    let drives = app
        .sidebar
        .iter()
        .filter(|location| location.kind == KnownLocationKind::Drive)
        .cloned()
        .collect::<Vec<_>>();
    ui.set_home_quick_access_items(ModelRc::new(VecModel::from(home_item_rows(
        app,
        tab.id,
        &quick_access,
        0,
        home_columns,
    ))));
    ui.set_home_drive_items(ModelRc::new(VecModel::from(home_item_rows(
        app,
        tab.id,
        &drives,
        1,
        home_columns,
    ))));
    ui.set_home_network_locations_items(ModelRc::new(VecModel::from(home_network_rows(
        app,
        tab.id,
        home_columns,
    ))));
    ui.set_home_selected_id(home_page.selected.unwrap_or_default().into());
}

fn project_files(
    ui: &AppWindow,
    app: &AppState,
    tab: &TabSession,
    window_id: WindowId,
    view_mode: ViewMode,
    projection_changed: bool,
) {
    let projected_tab_id = tab.id.0 as i32;
    let projected_request_id = tab.latest_request().0 as i32;
    project_file_geometry(ui, view_mode);
    ui.set_view_mode(view_mode_to_ui(view_mode));
    if projection_changed {
        if ui.get_rectangle_selection_pointer_active() {
            ui.invoke_cancel_rectangle_selection();
        }
        if ui.get_projected_file_tab_id() != projected_tab_id || !tab.retains_directory_view() {
            ui.set_file_viewport_y(0.0);
            ui.set_search_scroll_y(0.0);
        }
        ui.set_projected_file_tab_id(projected_tab_id);
        ui.set_projected_file_request_id(projected_request_id);
    }
    let grid_columns = grid_column_count_for_view(ui, view_mode);
    let total = tab.search_total.unwrap_or(tab.entries.len() as u32);
    let search_index =
        search_result_index_at_scroll(ui.get_search_scroll_y(), total, view_mode, grid_columns);
    let search_window = search_window_for_index(search_index, total, grid_columns);
    if tab.page_source == PageSource::Search {
        ui.set_file_viewport_y(search_window_viewport_y(
            ui.get_search_scroll_y(),
            search_window,
            view_mode,
            grid_columns,
        ));
    }
    if tab.page_source == PageSource::Directory {
        install_directory_models(ui, app, tab, view_mode, false);
    } else {
        let (file_rows, grid_rows) = if view_mode.uses_grid_layout() {
            let requested_px = grid_thumbnail_request_px(view_mode, ui.window().scale_factor());
            let grid_rows = search_window_rows(tab, app, search_window, Some(requested_px))
                .chunks(grid_columns)
                .map(|entries| GridRow {
                    group_header: false,
                    group_label: "".into(),
                    group_detail: "".into(),
                    group_count: 0,
                    entries: ModelRc::new(VecModel::from(entries.to_vec())),
                })
                .collect::<Vec<_>>();
            (Vec::new(), grid_rows)
        } else {
            (
                search_window_rows(
                    tab,
                    app,
                    search_window,
                    file_thumbnail_request_px(view_mode, ui.window().scale_factor()),
                ),
                Vec::new(),
            )
        };
        set_file_rows(ui, file_rows, false);
        ui.set_grid_column_count(grid_columns as i32);
        ui.set_grid_rows(ModelRc::new(VecModel::from(grid_rows)));
        rebuild_grouped_grid_layout(ui, false);
        rebuild_grid_entry_positions(ui, window_id);
    }
    let preference_revision = if tab.page_source == PageSource::Search {
        0
    } else {
        tab.visible_path()
            .map(|path| app.directory_preference(path).group_field.storage_code() as i32)
            .unwrap_or(0)
    };
    ui.set_projected_content_revision(
        projected_request_id
            .wrapping_mul(31)
            .wrapping_add(preference_revision),
    );
    ui.set_search_total_items(total.min(i32::MAX as u32) as i32);
}

fn project_navigation(
    ui: &AppWindow,
    state: &SharedSessions,
    app: &AppState,
    tab: &TabSession,
    window_id: WindowId,
    view_mode: ViewMode,
    projection_changed: bool,
) {
    let texts = Texts::new(app.language);
    let active_is_settings = tab.kind == TabKind::Settings;
    ui.set_active_is_settings(active_is_settings);
    ui.set_active_is_home(matches!(
        tab.visible_location(),
        Some(NavigationLocation::Home)
    ));
    let visible_path = tab
        .visible_location()
        .map(|location| navigation_display_name(location, app.language))
        .unwrap_or_default();
    let address_input = if tab.address_editing {
        tab.address_input.clone()
    } else {
        visible_path.clone()
    };
    let current_location_path = tab.visible_path();
    ui.set_current_location_icon(
        tab.visible_location()
            .and_then(|location| match location {
                NavigationLocation::Home | NavigationLocation::WebDav(_) => None,
                NavigationLocation::Directory(path) => app.image_cache.ordinary.get(path),
                NavigationLocation::Library(library) => app.library_icons.get(&library.identity),
            })
            .map(shell_icon_image)
            .unwrap_or_default(),
    );
    ui.set_current_location_is_drive(current_location_path.is_some_and(is_drive_root));
    ui.set_address_input(address_input.into());
    ui.set_address_editing(tab.address_editing);
    ui.set_search_mode(tab.address_mode == AddressMode::Smart);
    ui.set_search_results_mode(tab.page_source == PageSource::Search);
    ui.set_search_depth_enabled(matches!(tab.search_scope, SearchScope::Directory(_)));
    ui.set_search_recursive(tab.search_depth == SearchDepth::Recursive);

    let network_permission = tab.load_state == LoadState::PermissionDenied
        && tab
            .requested_location()
            .as_ref()
            .and_then(NavigationLocation::directory_path)
            .is_some_and(crate::network::is_unc_path);
    let library_failed = tab.load_state == LoadState::Failed
        && matches!(
            tab.requested_location(),
            Some(NavigationLocation::Library(_))
        );
    let (error_page_title, error_page_description) = if library_failed {
        match app.language {
            Language::Chinese => ("无法打开此库", texts.library_all_sources_failed()),
            Language::English => (
                "This library could not be opened",
                texts.library_all_sources_failed(),
            ),
        }
    } else if network_permission {
        match app.language {
            Language::Chinese => (
                "需要登录此网络位置",
                "Windows 当前凭据无法访问此位置。登录后会重新读取当前标签页。",
            ),
            Language::English => (
                "Sign-in required",
                "The current Windows credentials cannot access this location. The tab will retry after sign-in.",
            ),
        }
    } else if tab.page_source == PageSource::Search {
        search_error_page_text(tab.search_state, texts)
    } else {
        error_page_text(tab.load_state, texts)
    };
    ui.set_error_page_title(error_page_title.into());
    ui.set_error_page_description(error_page_description.into());
    ui.set_back_history(ModelRc::new(VecModel::from(
        tab.back_history()
            .iter()
            .rev()
            .enumerate()
            .map(|(index, path)| HistoryRow {
                index: index as i32,
                label: navigation_display_name(path, app.language).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_forward_history(ModelRc::new(VecModel::from(
        tab.forward_history()
            .iter()
            .rev()
            .enumerate()
            .map(|(index, path)| HistoryRow {
                index: index as i32,
                label: navigation_display_name(path, app.language).into(),
            })
            .collect::<Vec<_>>(),
    )));
    let (sort_field, sort_direction) = if tab.page_source == PageSource::Search {
        (tab.search_sort_field, tab.search_sort_direction)
    } else {
        (tab.sort_field, tab.sort_direction)
    };
    ui.set_sort_field(sort_field_ui_value(sort_field));
    ui.set_sort_descending(sort_direction == crate::domain::SortDirection::Descending);
    let page_projection =
        agent_debug::page_projection(tab.displayed_load_state(), tab.entries.is_empty());
    let page_state = if tab.page_source == PageSource::Search {
        match tab.search_state {
            SearchState::Searching if tab.entries.is_empty() => 1,
            SearchState::NoResults => 3,
            SearchState::NotConfigured
            | SearchState::Disconnected
            | SearchState::NotIndexed
            | SearchState::UnsupportedVersion
            | SearchState::UnsupportedArchitecture
            | SearchState::SyntaxError
            | SearchState::TimedOut
            | SearchState::Failed => 9,
            _ => 4,
        }
    } else if tab.load_state == LoadState::Failed
        && !tab.entries.is_empty()
        && matches!(tab.visible_location(), Some(NavigationLocation::WebDav(_)))
    {
        4
    } else {
        page_projection.index
    };
    loading_hint::project_page_state(ui, state, window_id, tab, projection_changed, page_state);
    let show_request_access = page_projection
        .visible_page_operations
        .contains(&agent_debug::PageOperation::RequestWindowsAccess);
    ui.set_show_request_access(show_request_access);
    if show_request_access
        && tab
            .requested_location()
            .as_ref()
            .and_then(NavigationLocation::directory_path)
            .is_some_and(crate::network::is_unc_path)
    {
        ui.set_text_request_access(
            match app.language {
                Language::Chinese => "登录网络位置",
                Language::English => "Sign in to network location",
            }
            .into(),
        );
    }
    ui.set_show_everything_help(
        tab.page_source == PageSource::Search
            && matches!(
                tab.search_state,
                SearchState::NotConfigured
                    | SearchState::Disconnected
                    | SearchState::NotIndexed
                    | SearchState::UnsupportedVersion
                    | SearchState::UnsupportedArchitecture
                    | SearchState::Failed
            ),
    );
    ui.set_show_start_everything(
        tab.page_source == PageSource::Search && tab.search_state == SearchState::Disconnected,
    );
    ui.set_can_navigate_back(
        !active_is_settings && (tab.has_failed_location() || !tab.back_history().is_empty()),
    );
    ui.set_can_navigate_forward(!active_is_settings && !tab.forward_history().is_empty());
    ui.set_can_navigate_up(
        !active_is_settings && tab.visible_path().and_then(Path::parent).is_some(),
    );
    ui.set_can_refresh(
        !active_is_settings && !matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
    );
    if tab.retains_directory_view() {
        let maximum = projected_scroll_maximum(ui, view_mode, ui.get_file_viewport_height());
        ui.set_file_viewport_y(ui.get_file_viewport_y().clamp(-maximum, 0.0));
    }
}

fn project_tabs(ui: &AppWindow, app: &AppState, window: &WindowState) {
    let texts = Texts::new(app.language);
    ui.set_active_tab_index(
        window
            .tab_order
            .iter()
            .position(|id| *id == window.active_tab)
            .unwrap_or(0) as i32,
    );
    ui.set_tabs(ModelRc::new(VecModel::from(
        window
            .tab_order
            .iter()
            .filter_map(|id| window.tabs.get(id))
            .map(|tab| TabRow {
                id: tab.id.0 as i32,
                title: if tab.kind == TabKind::Settings {
                    texts.settings().to_owned()
                } else {
                    tab.visible_location()
                        .map(|location| navigation_display_name(location, app.language))
                        .unwrap_or_else(|| display_path(Path::new("C:\\")))
                }
                .into(),
                active: tab.id == window.active_tab,
                loading: matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
                icon: tab
                    .visible_path()
                    .and_then(|path| app.image_cache.ordinary.get(path))
                    .map(shell_icon_image)
                    .unwrap_or_default(),
                is_drive: tab.visible_path().is_some_and(is_drive_root),
                is_home: matches!(tab.visible_location(), Some(NavigationLocation::Home)),
                is_settings: tab.kind == TabKind::Settings,
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_can_close_tab(window.tab_order.len() > 1);
    ui.set_can_restore_tab(!window.closed_tabs.is_empty());
}

fn project_sidebar(ui: &AppWindow, app: &AppState, window_id: WindowId) {
    let mut sidebar_rows = app
        .sidebar
        .iter()
        .enumerate()
        .map(|(index, location)| {
            let (drive_progress, drive_state, drive_status) =
                if let Some(root) = location.drive_root.as_deref() {
                    drive_projection(app.language, app.drive_capacity.get(root))
                } else {
                    (0.0, 3, String::new())
                };
            SidebarRow {
                index: index as i32,
                stable_id: format!("location:{index}").into(),
                label: location.label.as_str().into(),
                icon_kind: match location.kind {
                    KnownLocationKind::Drive => 7,
                    KnownLocationKind::Pinned => 3,
                },
                is_drive: location.kind == KnownLocationKind::Drive,
                drive_progress,
                drive_state,
                drive_status: drive_status.into(),
                group_kind: if location.kind == KnownLocationKind::Drive {
                    1
                } else {
                    0
                },
                source_kind: 0,
                icon: app
                    .sidebar_icons
                    .get(&location.path)
                    .map(shell_icon_image)
                    .unwrap_or_default(),
            }
        })
        .collect::<Vec<_>>();
    sidebar_rows.insert(
        0,
        SidebarRow {
            index: -1,
            stable_id: "".into(),
            label: match app.language {
                Language::Chinese => "主页",
                Language::English => "Home",
            }
            .into(),
            icon_kind: 0,
            group_kind: 0,
            source_kind: 0,
            is_drive: false,
            icon: Image::default(),
            drive_progress: 0.0,
            drive_state: 3,
            drive_status: "".into(),
        },
    );
    for (row, location) in sidebar_rows.iter_mut().skip(1).zip(app.sidebar.iter()) {
        if location.kind == KnownLocationKind::Pinned {
            row.source_kind = 3;
        }
    }
    for library in &app.libraries {
        sidebar_rows.push(SidebarRow {
            index: -1,
            stable_id: library_stable_id(library.id.as_os_str()).into(),
            label: library.display_name.to_string_lossy().into_owned().into(),
            icon_kind: 9,
            group_kind: 4,
            source_kind: 0,
            is_drive: false,
            drive_progress: 0.0,
            drive_state: 3,
            drive_status: "".into(),
            icon: app
                .library_icons
                .get(library.id.as_os_str())
                .map(shell_icon_image)
                .unwrap_or_default(),
        });
    }
    let mut network_row_index = app.sidebar.len();
    for location in sorted_network_locations(app) {
        let index = network_row_index;
        network_row_index += 1;
        sidebar_rows.push(SidebarRow {
            index: index as i32,
            stable_id: format!("network-location:{}", location.id).into(),
            label: location.display_name.clone().into(),
            icon_kind: 7,
            group_kind: 2,
            source_kind: if location.source == NetworkLocationSource::WindowsImported {
                1
            } else {
                2
            },
            is_drive: false,
            drive_progress: 0.0,
            drive_state: 3,
            drive_status: "".into(),
            icon: match &location.target {
                NetworkTarget::WindowsPath(path) | NetworkTarget::ShellItemId(path) => app
                    .sidebar_icons
                    .get(path)
                    .map(shell_icon_image)
                    .unwrap_or_default(),
                NetworkTarget::WebDav(_) => Image::default(),
            },
        });
    }
    if let Some(discovery) = app.network_discovery.get(&window_id) {
        for device in discovery.devices() {
            let Some(_path) = crate::network::device_root_target(device) else {
                continue;
            };
            let index = network_row_index;
            network_row_index += 1;
            sidebar_rows.push(SidebarRow {
                index: index as i32,
                stable_id: format!("network-device:{index}").into(),
                label: device.display_name.clone().into(),
                icon_kind: 7,
                group_kind: 3,
                source_kind: 0,
                is_drive: false,
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
                icon: Image::default(),
            });
        }
    }
    ui.set_sidebar_items(ModelRc::new(VecModel::from(sidebar_rows)));
    let sidebar_visibility_changed = (0..5).any(|index| {
        let projected = match index {
            0 => ui.get_quick_access_visible(),
            1 => ui.get_libraries_visible(),
            2 => ui.get_drives_visible(),
            3 => ui.get_network_locations_visible(),
            _ => ui.get_network_visible(),
        };
        projected != app.sidebar_visibility.is_visible(index)
    });
    ui.set_quick_access_visible(app.sidebar_visibility.is_visible(0));
    ui.set_libraries_visible(app.sidebar_visibility.is_visible(1));
    ui.set_drives_visible(app.sidebar_visibility.is_visible(2));
    ui.set_network_locations_visible(app.sidebar_visibility.is_visible(3));
    ui.set_network_visible(app.sidebar_visibility.is_visible(4));
    if sidebar_visibility_changed {
        ui.set_sidebar_viewport_y(0.0);
    }

    let discovery_state = app
        .network_discovery
        .get(&window_id)
        .map(DiscoveryCoordinator::state)
        .unwrap_or(DiscoveryState::Idle);
    ui.set_network_discovery_loading(discovery_state == DiscoveryState::Discovering);
    ui.set_network_discovery_empty(discovery_state == DiscoveryState::Empty);
    ui.set_network_discovery_error(
        app.network_discovery_errors
            .get(&window_id)
            .cloned()
            .unwrap_or_default()
            .into(),
    );
}

fn project_status(ui: &AppWindow, app: &AppState, tab: &TabSession) {
    let operation_rows = operation_rows(app);
    ui.set_operations(ModelRc::new(VecModel::from(operation_rows)));
    ui.set_operation_error(
        app.operation_notice
            .as_ref()
            .map(|(_, message)| message.clone())
            .or_else(|| {
                ui.get_rename_editing()
                    .then(|| app.operation_errors.last().cloned())
                    .flatten()
            })
            .unwrap_or_default()
            .into(),
    );
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_context_menu_has_entry(!tab.selected.is_empty() || tab.focused.is_some());
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
}
