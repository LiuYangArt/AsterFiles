use super::*;

pub(super) fn wire_open(
    ui: &AppWindow,
    state: &WindowSessions,
    workers: &WorkerSenders,
    quick_menu: &SharedQuickMenu,
    context_anchor: Arc<Mutex<(bool, i32, i32)>>,
) {
    let shell_menu_worker = workers.shell_menu.clone();
    let weak = ui.as_weak();
    let state_for_entry_menu = state.clone();
    let anchor_for_entry = context_anchor.clone();
    let quick_menu_for_entry = quick_menu.clone();
    let shell_menu_for_entry = shell_menu_worker.clone();
    ui.on_show_entry_menu(move |entry_id, x, y| {
        *anchor_for_entry.lock().expect("context anchor mutex") =
            (false, x.round() as i32, y.round() as i32);
        let mut app = state_for_entry_menu
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        let mut quick_access_path = None;
        if entry_id >= 0 {
            let id = EntryId(entry_id as u32);
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id)
                && !tab.selected.contains(&id)
            {
                tab.select_entry(id, false, false);
            }
            quick_access_path = app.active().visible_entry(id).map(|entry| entry.path());
        }
        drop(app);
        if let Ok(mut menu) = quick_menu_for_entry.lock() {
            menu.active_quick_access_path = quick_access_path;
        }
        let remote = state_for_entry_menu
            .lock()
            .ok()
            .is_some_and(|app| webdav_ui::location_is_remote(&app));
        if let Some(ui) = weak.upgrade() {
            ui.set_context_menu_anchor_x(x);
            ui.set_context_menu_anchor_y(y);
            if remote {
                let language = state_for_entry_menu
                    .lock()
                    .ok()
                    .map(|app| app.language)
                    .unwrap_or(Language::Chinese);
                let directory = state_for_entry_menu.lock().ok().is_some_and(|app| {
                    app.active().selected.iter().any(|id| {
                        app.active()
                            .visible_entry(*id)
                            .is_some_and(|entry| entry.kind == crate::domain::EntryKind::Directory)
                    })
                });
                let rows = webdav_ui::entry_menu_rows(language, directory);
                if let Ok(mut menu) = quick_menu_for_entry.lock() {
                    menu.built_in_rows = rows.clone();
                    menu.all_rows = rows;
                    menu.submenu_rows.clear();
                }
                ui.set_context_shell_loading(false);
                ui.set_context_menu_open(true);
                project_filtered_context_menu(&ui, &quick_menu_for_entry, "");
            } else {
                project_context_menu(&ui, &state_for_entry_menu, &quick_menu_for_entry, false);
                begin_shell_menu_load(
                    &ui,
                    &state_for_entry_menu,
                    &quick_menu_for_entry,
                    &shell_menu_for_entry,
                    false,
                );
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_sidebar_menu = state.clone();
    let quick_menu_for_sidebar = quick_menu.clone();
    let shell_menu_for_sidebar = shell_menu_worker.clone();
    ui.on_show_sidebar_menu(move |group, stable_id, x, y| {
        let Some(ui) = weak.upgrade() else { return };
        let resolved = state_for_sidebar_menu.lock().ok().and_then(|app| {
            sidebar_menu_target(
                &app,
                state_for_sidebar_menu.window_id,
                group,
                stable_id.as_str(),
            )
        });
        let Some(resolved) = resolved else { return };
        let (language, tab_id, navigation_request, quick_access_pending) = state_for_sidebar_menu
            .lock()
            .ok()
            .map(|app| {
                let tab = app.active();
                (
                    app.language,
                    tab.id,
                    tab.latest_request(),
                    resolved
                        .quick_access_path
                        .as_ref()
                        .is_some_and(|path| app.quick_access_pending.contains(path)),
                )
            })
            .unwrap();
        let rows = sidebar_context_rows(&resolved, language, quick_access_pending);
        let key = QuickMenuKey {
            window_id: state_for_sidebar_menu.window_id,
            tab_id,
            navigation_request,
            scope: QuickMenuScope::Sidebar {
                group,
                stable_id: resolved.stable_id.clone(),
                generation: resolved.generation,
                target: resolved.target.clone(),
            },
        };
        if let Ok(mut menu) = quick_menu_for_sidebar.lock() {
            menu.identity = None;
            menu.active_sidebar_target = Some(resolved.clone());
            menu.active_network_location = resolved.network_location_id;
            menu.active_quick_access_path = resolved.quick_access_path.clone();
            menu.built_in_rows = rows.clone();
            menu.all_rows = compose_quick_menu_rows(
                &rows,
                &(0..QUICK_MENU_PLACEHOLDER_ROWS)
                    .map(|_| quick_menu_placeholder())
                    .collect::<Vec<_>>(),
            );
            menu.submenu_rows.clear();
            menu.submenu_history.clear();
        }
        ui.set_context_menu_anchor_x(x);
        ui.set_context_menu_anchor_y(y);
        ui.set_context_search("".into());
        let remote_location = matches!(resolved.destination, SidebarDestination::WebDav(_));
        if remote_location && let Ok(mut menu) = quick_menu_for_sidebar.lock() {
            menu.all_rows = rows.clone();
        }
        ui.set_context_shell_loading(!remote_location);
        ui.set_context_submenu_open(false);
        project_filtered_context_menu(&ui, &quick_menu_for_sidebar, "");
        ui.set_context_menu_open(true);
        if let Some(window_id) = window_id_for_ui(&ui) {
            open_quick_menu_popup(window_id, x, y);
        }
        if remote_location {
            return;
        }
        begin_shell_menu_load_for(
            &ui,
            &quick_menu_for_sidebar,
            &shell_menu_for_sidebar,
            key,
            platform::windows::context_menu::ShellMenuLoadTarget::Item(resolved.target),
        );
    });
    let weak = ui.as_weak();
    let state_for_sidebar_background = state.clone();
    let quick_menu_for_sidebar_background = quick_menu.clone();
    ui.on_show_sidebar_background_menu(move |x, y| {
        let Some(ui) = weak.upgrade() else { return };
        let (visibility, language, tab_id, navigation_request) = state_for_sidebar_background
            .lock()
            .ok()
            .map(|app| {
                let tab = app.active();
                (
                    app.sidebar_visibility,
                    app.language,
                    tab.id,
                    tab.latest_request(),
                )
            })
            .unwrap();
        let mut rows = sidebar_visibility_rows(visibility, language);
        rows.push(quick_menu_separator());
        rows.push(quick_menu_row(
            webdav_ui::CMD_ADD_WEBDAV_LOCATION,
            0,
            if language == Language::Chinese {
                "添加 WebDAV 位置"
            } else {
                "Add WebDAV location"
            },
            true,
            false,
            false,
        ));
        let key = QuickMenuKey {
            window_id: state_for_sidebar_background.window_id,
            tab_id,
            navigation_request,
            scope: QuickMenuScope::SidebarBackground,
        };
        if let Ok(mut menu) = quick_menu_for_sidebar_background.lock() {
            menu.identity = None;
            menu.built_in_key = Some(key);
            menu.active_sidebar_target = None;
            menu.active_network_location = None;
            menu.active_quick_access_path = None;
            menu.built_in_rows = rows.clone();
            menu.all_rows = rows;
            menu.submenu_rows.clear();
            menu.submenu_history.clear();
        }
        ui.set_context_menu_anchor_x(x);
        ui.set_context_menu_anchor_y(y);
        ui.set_context_search("".into());
        ui.set_context_shell_loading(false);
        ui.set_context_submenu_open(false);
        project_filtered_context_menu(&ui, &quick_menu_for_sidebar_background, "");
        ui.set_context_menu_open(true);
        if let Some(window_id) = window_id_for_ui(&ui) {
            open_quick_menu_popup(window_id, x, y);
        }
    });
    let weak = ui.as_weak();
    let state_for_column_menu = state.clone();
    let quick_menu_for_column = quick_menu.clone();
    let anchor_for_column = context_anchor.clone();
    ui.on_show_column_menu(move |kind, x, y| {
        let Some(ui) = weak.upgrade() else { return };
        if !(0..ColumnKind::COUNT as i32).contains(&kind) {
            return;
        }
        *anchor_for_column.lock().expect("context anchor mutex") =
            (true, x.round() as i32, y.round() as i32);
        let (rows, submenu_rows) = {
            let app = state_for_column_menu
                .lock()
                .expect("app state mutex is not poisoned");
            let language = app.language;
            let layout = app.active_column_layout();
            let zh = |chinese: &'static str, english: &'static str| {
                if language == Language::Chinese {
                    chinese
                } else {
                    english
                }
            };
            let labels = [
                (ColumnKind::Name, "名称", "Name"),
                (ColumnKind::Modified, "修改时间", "Date modified"),
                (ColumnKind::Kind, "类型", "Type"),
                (ColumnKind::Size, "大小", "Size"),
                (ColumnKind::Created, "创建时间", "Date created"),
            ];
            let columns = labels
                .into_iter()
                .map(|(column, chinese, english)| {
                    let code = column.storage_code() as usize;
                    quick_menu_row(
                        CMD_COLUMN_TOGGLE_BASE + code as i32,
                        0,
                        zh(chinese, english),
                        column != ColumnKind::Name,
                        layout.visible[code],
                        false,
                    )
                })
                .collect::<Vec<_>>();
            let rows = vec![
                quick_menu_row(
                    CMD_COLUMN_FIT,
                    0,
                    zh("调整当前列宽以适应内容", "Size column to fit"),
                    true,
                    false,
                    false,
                ),
                quick_menu_row(
                    CMD_COLUMNS_FIT,
                    0,
                    zh("调整所有列宽以适应内容", "Size all columns to fit"),
                    true,
                    false,
                    false,
                ),
                quick_menu_separator(),
                quick_menu_row(-1, NODE_COLUMNS, zh("列", "Columns"), true, false, true),
            ];
            (rows, HashMap::from([(NODE_COLUMNS, columns)]))
        };
        if let Ok(mut menu) = quick_menu_for_column.lock() {
            menu.identity = None;
            menu.active_column = ColumnKind::from_storage_code(kind as u8);
            menu.built_in_rows = rows.clone();
            menu.all_rows = rows;
            menu.built_in_submenu_rows = submenu_rows;
            menu.submenu_rows.clear();
            menu.submenu_history.clear();
        }
        ui.set_context_menu_anchor_x(x);
        ui.set_context_menu_anchor_y(y);
        ui.set_context_search("".into());
        ui.set_context_shell_loading(false);
        ui.set_context_submenu_open(false);
        project_filtered_context_menu(&ui, &quick_menu_for_column, "");
        ui.set_context_menu_open(true);
        if let Some(window_id) = window_id_for_ui(&ui) {
            open_quick_menu_popup(window_id, x, y);
        }
    });
    let weak = ui.as_weak();
    let state_for_background_menu = state.clone();
    let anchor_for_background = context_anchor.clone();
    let quick_menu_for_background = quick_menu.clone();
    let shell_menu_for_background = shell_menu_worker.clone();
    ui.on_show_background_menu(move |x, y| {
        *anchor_for_background.lock().expect("context anchor mutex") =
            (true, x.round() as i32, y.round() as i32);
        if let Ok(mut app) = state_for_background_menu.lock() {
            let tab_id = app.active_window_state().active_tab;
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
                tab.clear_selection();
            }
        }
        let remote = state_for_background_menu
            .lock()
            .ok()
            .is_some_and(|app| webdav_ui::location_is_remote(&app));
        if let Some(ui) = weak.upgrade() {
            ui.set_context_menu_anchor_x(x);
            ui.set_context_menu_anchor_y(y);
            if remote {
                let language = state_for_background_menu
                    .lock()
                    .ok()
                    .map(|app| app.language)
                    .unwrap_or(Language::Chinese);
                let rows = vec![quick_menu_row(
                    CMD_REFRESH,
                    0,
                    if language == Language::Chinese {
                        "刷新"
                    } else {
                        "Refresh"
                    },
                    true,
                    false,
                    false,
                )];
                if let Ok(mut menu) = quick_menu_for_background.lock() {
                    menu.built_in_rows = rows.clone();
                    menu.all_rows = rows;
                    menu.submenu_rows.clear();
                }
                ui.set_context_shell_loading(false);
                ui.set_context_menu_open(true);
                project_filtered_context_menu(&ui, &quick_menu_for_background, "");
            } else {
                project_context_menu(
                    &ui,
                    &state_for_background_menu,
                    &quick_menu_for_background,
                    true,
                );
                begin_shell_menu_load(
                    &ui,
                    &state_for_background_menu,
                    &quick_menu_for_background,
                    &shell_menu_for_background,
                    true,
                );
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_reopen_menu = state.clone();
    let anchor_for_reopen = context_anchor.clone();
    let quick_menu_for_reopen = quick_menu.clone();
    let shell_menu_for_reopen = shell_menu_worker.clone();
    ui.on_reopen_context_menu(move |x, y| {
        let ui = weak.upgrade();
        let (entry_id, background) = context_target_at(
            &state_for_reopen_menu,
            x,
            y,
            ui.as_ref()
                .map_or_else(FileHitGeometry::default, |ui| FileHitGeometry {
                    list_left: ui.get_file_list_left(),
                    list_top: ui.get_file_list_top(),
                    viewport_x: ui.get_file_viewport_x(),
                    viewport_y: ui.get_file_viewport_y(),
                    viewport_width: ui.get_file_viewport_width(),
                    columns_width: ui.get_details_hit_width(),
                }),
            ui.as_ref().map_or(0.0, |ui| ui.get_search_scroll_y()),
            ui.as_ref()
                .map_or(1, |ui| ui.get_grid_column_count().max(1) as usize),
        );
        *anchor_for_reopen.lock().expect("context anchor mutex") =
            (background, x.round() as i32, y.round() as i32);
        if let Ok(mut app) = state_for_reopen_menu.lock() {
            let tab_id = app.active_window_state().active_tab;
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
                if let Some(id) = entry_id {
                    if !tab.selected.contains(&id) {
                        tab.select_entry(id, false, false);
                    }
                } else {
                    tab.clear_selection();
                }
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_context_menu_anchor_x(x);
            ui.set_context_menu_anchor_y(y);
            project_context_menu(
                &ui,
                &state_for_reopen_menu,
                &quick_menu_for_reopen,
                background,
            );
            begin_shell_menu_load(
                &ui,
                &state_for_reopen_menu,
                &quick_menu_for_reopen,
                &shell_menu_for_reopen,
                background,
            );
        }
    });
}

pub(super) fn wire_interaction(
    ui: &AppWindow,
    workers: &WorkerSenders,
    quick_menu: &SharedQuickMenu,
) {
    let shell_menu_worker = workers.shell_menu.clone();
    let weak = ui.as_weak();
    let quick_menu_for_filter = quick_menu.clone();
    ui.on_filter_context_menu(move |query| {
        if let Some(ui) = weak.upgrade() {
            ui.set_context_search(query.clone());
            ui.invoke_cancel_context_submenu_hover();
            if let Ok(mut menu) = quick_menu_for_filter.lock() {
                menu.submenu_rows.clear();
                menu.submenu_history.clear();
                menu.active_submenu_token = None;
                menu.create_submenu_open = false;
                menu.active_submenu_request = menu.active_submenu_request.wrapping_add(1).max(1);
            }
            ui.set_context_submenu_open(false);
            ui.set_context_submenu_parent_open(false);
            ui.set_context_submenu_loading(false);
            ui.set_context_submenu_content_height(0.0);
            ui.set_context_submenu_parent_content_height(0.0);
            project_filtered_context_menu(&ui, &quick_menu_for_filter, query.as_str());
        }
    });

    let weak = ui.as_weak();
    ui.on_move_context_selection(move |direction| {
        let Some(ui) = weak.upgrade() else { return };
        let model = ui.get_context_commands();
        let rows = (0..model.row_count())
            .filter_map(|index| model.row_data(index))
            .collect::<Vec<_>>();
        ui.set_context_active_index(next_enabled_context_index(
            &rows,
            ui.get_context_active_index(),
            direction,
        ));
    });

    let weak = ui.as_weak();
    ui.on_move_context_submenu_selection(move |direction| {
        let Some(ui) = weak.upgrade() else { return };
        let model = ui.get_context_submenu_commands();
        let rows = (0..model.row_count())
            .filter_map(|index| model.row_data(index))
            .collect::<Vec<_>>();
        ui.set_context_submenu_active_index(next_enabled_context_index(
            &rows,
            ui.get_context_submenu_active_index(),
            direction,
        ));
    });

    let weak = ui.as_weak();
    ui.on_activate_context_selection(move || {
        let Some(ui) = weak.upgrade() else { return };
        let index = ui.get_context_active_index();
        if index >= 0
            && let Some(row) = ui.get_context_commands().row_data(index as usize)
            && row.enabled
            && !row.separator
        {
            if row.submenu {
                ui.invoke_cancel_context_submenu_hover();
                ui.set_context_submenu_anchor_y(0.0);
                ui.invoke_open_context_submenu(index);
            } else {
                ui.set_context_menu_open(false);
                ui.invoke_invoke_context_command(row.id);
                if !row.shell {
                    ui.invoke_dismiss_context_menu();
                }
            }
        }
    });

    let weak = ui.as_weak();
    let quick_menu_for_submenu = quick_menu.clone();
    let shell_menu_for_submenu = shell_menu_worker.clone();
    ui.on_open_context_submenu(move |encoded_index| {
        let Some(ui) = weak.upgrade() else { return };
        let (model, index) = if encoded_index < 0 {
            (
                ui.get_context_submenu_commands(),
                (-encoded_index - 1) as usize,
            )
        } else {
            (ui.get_context_commands(), encoded_index as usize)
        };
        let Some(row) = model.row_data(index) else {
            trace_quick_menu(
                "quick_menu_submenu_open_rejected",
                format!("encoded_index={} reason=row_missing", encoded_index),
            );
            return;
        };
        trace_quick_menu(
            "quick_menu_submenu_open_requested",
            format!(
                "encoded_index={} label={:?} node={} enabled={} submenu={}",
                encoded_index,
                row.label.as_str(),
                row.node_id,
                row.enabled,
                row.submenu,
            ),
        );
        if !row.enabled || !row.submenu || row.node_id <= 0 {
            trace_quick_menu(
                "quick_menu_submenu_open_rejected",
                format!(
                    "encoded_index={} node={} reason=invalid_row",
                    encoded_index, row.node_id,
                ),
            );
            return;
        }
        if encoded_index >= 0 { ui.set_context_active_index(index as i32); }
        if row.node_id != NODE_CREATE
            && let Ok(mut menu) = quick_menu_for_submenu.lock()
        {
            menu.create_submenu_open = false;
        }
        if row.node_id == NODE_CREATE {
            let token_ready = quick_menu_for_submenu.lock().ok().is_some_and(|mut menu| {
                menu.create_submenu_open = true;
                menu.create_submenu_token.is_some()
            });
            if !token_ready {
                let language = quick_menu_for_submenu
                    .lock()
                    .ok()
                    .and_then(|menu| menu.language)
                    .unwrap_or(Language::English);
                let rows = vec![built_in_folder_row(language)];
                if let Ok(mut menu) = quick_menu_for_submenu.lock() {
                    menu.submenu_rows = rows.clone();
                }
                ui.set_context_submenu_open(true);
                ui.set_context_submenu_parent_open(false);
                ui.set_context_submenu_loading(ui.get_context_shell_loading());
                ui.set_context_submenu_active_index(0);
                ui.set_context_submenu_content_height(context_menu_content_height(&rows));
                ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
                return;
            }
        }
        let built_in = quick_menu_for_submenu.lock().ok().and_then(|mut menu| {
            let rows = menu.built_in_submenu_rows.get(&row.node_id)?.clone();
            menu.submenu_history.clear();
            menu.submenu_rows = rows.clone();
            Some(rows)
        });
        if let Some(rows) = built_in {
            ui.set_context_submenu_open(true);
            ui.set_context_submenu_parent_open(false);
            ui.set_context_submenu_loading(false);
            ui.set_context_submenu_active_index(first_enabled_context_index(&rows));
            ui.set_context_submenu_content_height(context_menu_content_height(&rows));
            ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
            return;
        }
        let submenu_is_open = ui.get_context_submenu_open();
        let request = quick_menu_for_submenu.lock().ok().and_then(|mut menu| {
            let identity = menu.identity.clone()?;
            let token = *menu.submenu_tokens.get(&row.node_id)?;
            if submenu_request_is_duplicate(submenu_is_open, menu.active_submenu_token, token) {
                return None;
            }
            if encoded_index < 0 {
                if let Some(token) = menu.active_submenu_token {
                    let rows = menu.submenu_rows.clone();
                    menu.submenu_history.push((token, rows));
                }
            } else {
                menu.submenu_history.clear();
            }
            menu.active_submenu_request = menu.active_submenu_request.wrapping_add(1).max(1);
            menu.active_submenu_token = Some(token);
            let preloaded = cached_submenu_rows(&menu, token);
            menu.submenu_rows.clear();
            Some((identity, menu.active_submenu_request, token, preloaded))
        });
        let Some((identity, submenu_request_id, token, preloaded)) = request else {
            trace_quick_menu(
                "quick_menu_submenu_request_skipped",
                format!(
                    "encoded_index={} node={} submenu_open={} reason=missing_identity_token_or_duplicate",
                    encoded_index, row.node_id, submenu_is_open,
                ),
            );
            return;
        };
        trace_quick_menu(
            "quick_menu_submenu_request_started",
            format!(
                "session={} request={} submenu_request={} token={} encoded_index={} cached={}",
                identity.session_id,
                identity.request_id,
                submenu_request_id,
                token,
                encoded_index,
                preloaded.is_some(),
            ),
        );
        ui.set_context_submenu_open(true);
        if encoded_index < 0 {
            ui.set_context_submenu_parent_anchor_y(ui.get_context_submenu_anchor_y());
            ui.set_context_submenu_anchor_y(0.0);
            let parent_rows = quick_menu_for_submenu
                .lock()
                .ok()
                .and_then(|menu| menu.submenu_history.last().map(|(_, rows)| rows.clone()))
                .unwrap_or_default();
            ui.set_context_submenu_parent_content_height(context_menu_content_height(&parent_rows));
            ui.set_context_submenu_parent_commands(ModelRc::new(VecModel::from(parent_rows)));
            ui.set_context_submenu_parent_open(true);
        } else {
            ui.set_context_submenu_parent_open(false);
        }
        if let Some(rows) = preloaded {
            eprintln!(
                "{{\"event\":\"shell_submenu_cache_hit\",\"session\":{},\"request\":{},\"submenu_request\":{},\"token\":{},\"item_count\":{}}}",
                identity.session_id,
                identity.request_id,
                submenu_request_id,
                token,
                rows.len(),
            );
            if let Ok(mut menu) = quick_menu_for_submenu.lock() {
                menu.submenu_rows = filtered_context_rows(&rows, "");
            }
            ui.set_context_submenu_loading(false);
            project_context_submenu(&ui, &quick_menu_for_submenu);
            return;
        }
        eprintln!(
            "{{\"event\":\"shell_submenu_cache_miss\",\"session\":{},\"request\":{},\"submenu_request\":{},\"token\":{}}}",
            identity.session_id,
            identity.request_id,
            submenu_request_id,
            token,
        );
        ui.set_context_submenu_loading(true);
        if row.node_id == NODE_CREATE {
            let rows = vec![quick_menu_row(
                1,
                0,
                Texts::new(
                    quick_menu_for_submenu
                        .lock()
                        .ok()
                        .and_then(|menu| menu.language)
                        .unwrap_or(Language::English),
                )
                .folder(),
                true,
                false,
                false,
            )];
            if let Ok(mut menu) = quick_menu_for_submenu.lock() {
                menu.submenu_rows = rows.clone();
            }
            ui.set_context_submenu_active_index(0);
            ui.set_context_submenu_content_height(context_menu_content_height(&rows));
            ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
        } else {
            ui.set_context_submenu_content_height(0.0);
            ui.set_context_submenu_commands(ModelRc::new(VecModel::from(
                Vec::<ContextCommandRow>::new(),
            )));
        }
        let _ = shell_menu_for_submenu.send(
            platform::windows::context_menu::ShellMenuCommand::LoadSubmenu {
                session_id: identity.session_id,
                request_id: identity.request_id,
                submenu_request_id,
                token,
            },
        );
    });

    let weak = ui.as_weak();
    let quick_menu_for_close_submenu = quick_menu.clone();
    ui.on_close_context_submenu(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_context_submenu_loading(false);
            if let Ok(mut menu) = quick_menu_for_close_submenu.lock() {
                menu.create_submenu_open = false;
            }
            let restored = quick_menu_for_close_submenu
                .lock()
                .ok()
                .and_then(|mut menu| {
                    let (token, rows) = menu.submenu_history.pop()?;
                    menu.active_submenu_token = Some(token);
                    menu.submenu_rows = rows.clone();
                    Some(rows)
                });
            if let Some(rows) = restored {
                if let Some(window_id) = window_id_for_ui(&ui) {
                    let depth = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                        runtimes.get(&window_id).and_then(|runtime| {
                            runtime
                                .quick_menu_popup
                                .session
                                .branches()
                                .len()
                                .checked_sub(2)
                        })
                    });
                    if let Some(depth) = depth {
                        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
                            if let Some(runtime) = runtimes.get_mut(&window_id) {
                                hide_quick_submenu_slots_from(&mut runtime.quick_menu_popup, depth);
                            }
                        });
                    }
                }
                ui.set_context_submenu_parent_open(false);
                ui.set_context_submenu_parent_content_height(0.0);
                ui.set_context_submenu_anchor_y(ui.get_context_submenu_parent_anchor_y());
                ui.set_context_submenu_active_index(first_enabled_context_index(&rows));
                ui.set_context_submenu_content_height(context_menu_content_height(&rows));
                ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
            } else {
                ui.set_context_submenu_open(false);
                ui.set_context_submenu_parent_open(false);
                ui.set_context_submenu_content_height(0.0);
                ui.set_context_submenu_parent_content_height(0.0);
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_activate_context_submenu_selection(move || {
        let Some(ui) = weak.upgrade() else { return };
        let index = ui.get_context_submenu_active_index();
        if index >= 0
            && let Some(row) = ui.get_context_submenu_commands().row_data(index as usize)
            && row.enabled
            && !row.separator
        {
            if row.submenu {
                ui.invoke_cancel_context_submenu_hover();
                ui.invoke_open_context_submenu(-index - 1);
            } else {
                ui.set_context_menu_open(false);
                ui.invoke_invoke_context_command(row.id);
                if !row.shell {
                    ui.invoke_dismiss_context_menu();
                }
            }
        }
    });

    let quick_menu_for_dismiss = quick_menu.clone();
    let shell_menu_for_dismiss = shell_menu_worker.clone();
    ui.on_dismiss_context_menu(move || {
        let identity = quick_menu_for_dismiss.lock().ok().and_then(|mut menu| {
            menu.active_sidebar_target = None;
            menu.active_network_location = None;
            menu.active_quick_access_path = None;
            menu.built_in_key = None;
            menu.create_submenu_open = false;
            menu.active_submenu_token = None;
            menu.active_submenu_request = menu.active_submenu_request.wrapping_add(1).max(1);
            menu.identity.take()
        });
        if let Some(identity) = identity {
            let _ = shell_menu_for_dismiss.send(
                platform::windows::context_menu::ShellMenuCommand::Close {
                    session_id: identity.session_id,
                    request_id: identity.request_id,
                },
            );
        }
    });
}

pub(super) fn wire_command(
    ui: &AppWindow,
    state: &WindowSessions,
    workers: &WorkerSenders,
    confirmations: &ConfirmationWindows,
    quick_menu: &SharedQuickMenu,
    context_anchor: Arc<Mutex<(bool, i32, i32)>>,
) {
    let network_location_rename_ui = workers.network_location_rename.clone();
    let webdav_location_ui = workers.webdav_location.clone();
    let sender = workers.directory.clone();
    let network_sender = workers.network_directory.clone();
    let operation_sender = workers.operation.clone();
    let clipboard_sender = workers.clipboard.clone();
    let everything_sender = workers.everything.clone();
    let thumbnail_scheduler = workers.thumbnail.clone();
    let shell_menu_worker = workers.shell_menu.clone();
    let weak = ui.as_weak();
    let state_for_context_command = state.clone();
    let sender_for_context_command = operation_sender.clone();
    let delete_weak_for_context = confirmations.delete.clone();
    let clipboard_for_context = clipboard_sender.clone();
    let webdav_location_ui = webdav_location_ui.clone();
    let quick_menu_for_command = quick_menu.clone();
    let shell_menu_for_command = shell_menu_worker.clone();
    let everything_for_context_command = everything_sender.clone();
    ui.on_invoke_context_command(move |command| {
        let mut regions = RefreshRegions::STATUS | RefreshRegions::COLUMNS;
        match command {
            1 => {
                let current = quick_menu_for_command
                    .lock()
                    .ok()
                    .and_then(|menu| menu.built_in_key.clone())
                    .is_some_and(|key| quick_menu_key_is_current(&state_for_context_command.shared, &key));
                if current {
                    create_default_folder(&state_for_context_command, &sender_for_context_command);
                }
            }
            2 => request_clipboard_write(&state_for_context_command, &clipboard_for_context, false),
            3 => request_clipboard_write(&state_for_context_command, &clipboard_for_context, true),
            4 => request_clipboard_paste(&state_for_context_command, &clipboard_for_context),
            5 => begin_rename_ui(&weak, &state_for_context_command),
            6 => {
                if context_delete_request(6, platform::windows::shift_key_down()) == Some(true) {
                    if let (Some(ui), Some(delete_ui)) =
                        (weak.upgrade(), delete_weak_for_context.upgrade())
                    {
                        request_permanent_delete(&state_for_context_command, &ui, &delete_ui);
                    }
                } else {
                    submit_delete(
                        &state_for_context_command,
                        &sender_for_context_command,
                        false,
                    );
                }
            }
            CMD_REFRESH => {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_refresh();
                }
            }
            CMD_OPEN_FILE_LOCATION => {
                regions = regions | RefreshRegions::CONTENT;
                let target = quick_menu_for_command
                    .lock()
                    .ok()
                    .and_then(|menu| menu.built_in_key.clone())
                    .filter(|key| quick_menu_key_is_current(&state_for_context_command.shared, key))
                    .and_then(|key| match key.scope {
                        QuickMenuScope::Content { paths, .. } if paths.len() == 1 => {
                            Some((key.window_id, key.tab_id, paths.into_iter().next()?))
                        }
                        _ => None,
                    });
                if let Some((window_id, origin_tab_id, path)) = target {
                    let language = state_for_context_command
                        .lock()
                        .map(|app| app.language)
                        .unwrap_or(Language::Chinese);
                    let Some(parent) = path.parent().filter(|parent| !parent.as_os_str().is_empty()) else {
                        if let Ok(mut app) = state_for_context_command.lock()
                            && let Some(tab) = app.tab_mut(origin_tab_id)
                        {
                            tab.error = Some(format!(
                                "reveal_target_missing:{}",
                                Texts::new(language).no_parent_location()
                            ));
                        }
                        if let Some(ui) = weak.upgrade() {
                            refresh_regions(&ui, &state_for_context_command, RefreshRegions::STATUS);
                        }
                        return;
                    };
                    submit_reveal_navigation(
                        &sender,
                        &network_sender,
                        &state_for_context_command,
                        window_id,
                        origin_tab_id,
                        parent.to_path_buf(),
                        path,
                    );
                }
            }
            command if (CMD_SIDEBAR_VISIBILITY_BASE..CMD_SIDEBAR_VISIBILITY_BASE + 5)
                .contains(&command) =>
            {
                let is_sidebar_background = quick_menu_for_command.lock().ok().is_some_and(|menu| {
                    menu.built_in_key.as_ref().is_some_and(|key| {
                        matches!(key.scope, QuickMenuScope::SidebarBackground)
                            && quick_menu_key_is_current(&state_for_context_command.shared, key)
                    })
                });
                if is_sidebar_background
                    && let Ok(mut app) = state_for_context_command.lock()
                {
                    toggle_sidebar_visibility(&mut app.sidebar_visibility, command);
                }
                refresh_all_windows(&state_for_context_command.shared, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
            }
            CMD_QUICK_ACCESS_PIN | CMD_QUICK_ACCESS_UNPIN => {
                let path = current_sidebar_menu_target(&state_for_context_command, &quick_menu_for_command)
                    .and_then(|target| target.quick_access_path);
                if let Some(path) = path {
                    submit_quick_access_change(
                        state_for_context_command.shared.clone(),
                        path,
                        command == CMD_QUICK_ACCESS_PIN,
                    );
                }
            }
            webdav_ui::CMD_ADD_WEBDAV_LOCATION => {
                let Some(window) = webdav_location_ui.upgrade() else { return };
                let Some(owner) = weak.upgrade() else { return };
                if let Ok(app) = state_for_context_command.lock() {
                    webdav_ui::show_new_dialog(&window, &owner, &app);
                }
            }
            webdav_ui::CMD_EDIT_WEBDAV_LOCATION => {
                let Some(id) = quick_menu_for_command.lock().ok().and_then(|menu| menu.active_network_location) else { return };
                let Some(window) = webdav_location_ui.upgrade() else { return };
                let Some(owner) = weak.upgrade() else { return };
                if let Ok(app) = state_for_context_command.lock() {
                    webdav_ui::show_edit_dialog(&window, &owner, &app, id);
                }
            }
            webdav_ui::CMD_WEBDAV_DOWNLOAD | webdav_ui::CMD_WEBDAV_OPEN => {
                let opened = state_for_context_command.lock().ok().and_then(|app| {
                    let entries = webdav_ui::selected_remote_entries(&app)?;
                    (command == webdav_ui::CMD_WEBDAV_OPEN
                        && entries.len() == 1
                        && entries[0].kind == crate::domain::EntryKind::Directory)
                        .then(|| (app.active_window_state().active_tab, entries))
                });
                if let Some((tab_id, entries)) = opened {
                    regions = regions | RefreshRegions::CONTENT;
                    webdav_ui::open_entry(
                        &state_for_context_command,
                        &sender,
                        tab_id,
                        &entries[0],
                    );
                } else if let Some(entries) = state_for_context_command
                    .lock()
                    .ok()
                    .and_then(|app| webdav_ui::selected_remote_entries(&app))
                {
                    webdav_ui::start_download(
                        state_for_context_command.shared.clone(),
                        entries,
                        command == webdav_ui::CMD_WEBDAV_OPEN,
                    );
                }
            }
            CMD_ADD_NETWORK_LOCATION => {
                if let Ok(mut app) = state_for_context_command.lock()
                    && let Some(path) = app.active().visible_path().map(Path::to_path_buf)
                {
                    let name = network_location_default_name(&path);
                    let mut catalog = NetworkLocationCatalog::new(app.network_locations.clone());
                    match catalog.add_unc(path, name) {
                        Ok(_) => { app.network_locations = catalog.locations().to_vec(); app.network_location_generation = app.network_location_generation.wrapping_add(1).max(1); },
                        Err(crate::network::NetworkLocationCatalogError::DuplicateTarget) => {}
                        Err(error) => app
                            .operation_errors
                            .push(format!("Failed to add network location: {error:?}")),
                    }
                }
                refresh_all_windows(&state_for_context_command, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
            }
            CMD_NETWORK_LOCATION_OPEN => {
                regions = regions | RefreshRegions::CONTENT;
                let sidebar = current_sidebar_menu_target(&state_for_context_command, &quick_menu_for_command);
                if let Some(sidebar) = sidebar {
                    match sidebar.destination {
                        SidebarDestination::Directory(path) => {
                            let tab_id = state_for_context_command.lock().ok().map(|app| app.active_window_state().active_tab);
                            if let Some(tab_id) = tab_id { submit_path_navigation(&sender, &network_sender, &state_for_context_command, tab_id, path, NavigationKind::Normal); }
                        }
                        SidebarDestination::Library(library) => {
                            let tab_id = state_for_context_command.lock().ok().map(|app| app.active_window_state().active_tab);
                            if let Some(tab_id) = tab_id { submit_library_navigation(&sender, &state_for_context_command, tab_id, library, NavigationKind::Normal); }
                        }
                        SidebarDestination::External(identity) => { thread::spawn(move || { let _ = platform::open_path(&identity); }); }
                        SidebarDestination::WebDav(place) => {
                            let tab_id = state_for_context_command.lock().ok().map(|app| app.active_window_state().active_tab);
                            if let Some(tab_id) = tab_id {
                                submit_webdav_navigation(&sender, &state_for_context_command, tab_id, place, NavigationKind::Normal);
                            }
                        }
                    }
                    if let Some(ui) = weak.upgrade() { refresh_regions(&ui, &state_for_context_command, regions); }
                    return;
                }
                let target = selected_network_location_target(&state_for_context_command, &quick_menu_for_command);
                match target {
                    Some(NetworkTarget::WindowsPath(path)) => {
                        let tab_id = state_for_context_command
                            .lock()
                            .ok()
                            .map(|app| app.active_window_state().active_tab);
                        if let Some(tab_id) = tab_id {
                            submit_path_navigation(
                                &sender,
                                &network_sender,
                                &state_for_context_command,
                                tab_id,
                                path,
                                NavigationKind::Normal,
                            );
                        }
                    }
                    Some(NetworkTarget::WebDav(connection)) => {
                        let tab_id = state_for_context_command
                            .lock()
                            .ok()
                            .map(|app| app.active_window_state().active_tab);
                        if let Some(tab_id) = tab_id {
                            submit_webdav_navigation(
                                &sender,
                                &state_for_context_command,
                                tab_id,
                                crate::domain::WebDavPlace {
                                    connection_id: connection.connection_id,
                                    revision: connection.revision,
                                    root: connection.root.clone(),
                                    path: connection.root,
                                },
                                NavigationKind::Normal,
                            );
                        }
                    }
                    Some(NetworkTarget::ShellItemId(identity)) => {
                        thread::spawn(move || {
                            if let Err(error) = platform::open_path(&identity) {
                                eprintln!("unable to open Windows network location: {error}");
                            }
                        });
                    }
                    None => {}
                }
            }
            CMD_NETWORK_LOCATION_OPEN_NEW_TAB => {
                regions = regions | RefreshRegions::CONTENT;
                let sidebar = current_sidebar_menu_target(&state_for_context_command, &quick_menu_for_command);
                if let Some(sidebar) = sidebar {
                    match sidebar.destination {
                        SidebarDestination::Directory(path) => {
                            let tab_id = state_for_context_command.lock().ok().map(|mut app| app.create_tab(NavigationLocation::Directory(path.clone())));
                            if let Some(tab_id) = tab_id { submit_path_navigation(&sender, &network_sender, &state_for_context_command, tab_id, path, NavigationKind::Refresh); }
                        }
                        SidebarDestination::Library(library) => {
                            let tab_id = state_for_context_command.lock().ok().map(|mut app| app.create_tab(NavigationLocation::Library(library.clone())));
                            if let Some(tab_id) = tab_id { submit_library_navigation(&sender, &state_for_context_command, tab_id, library, NavigationKind::Refresh); }
                        }
                        SidebarDestination::WebDav(place) => {
                            let tab_id = state_for_context_command.lock().ok().map(|mut app| {
                                app.create_tab(NavigationLocation::WebDav(place.clone()))
                            });
                            if let Some(tab_id) = tab_id {
                                submit_webdav_navigation(
                                    &sender,
                                    &state_for_context_command,
                                    tab_id,
                                    place,
                                    NavigationKind::Refresh,
                                );
                            }
                        }
                        SidebarDestination::External(_) => {}
                    }
                    if let Some(ui) = weak.upgrade() { refresh_regions(&ui, &state_for_context_command, regions); }
                    return;
                }
                let path = selected_network_location_path(&state_for_context_command, &quick_menu_for_command);
                if let Some(path) = path {
                    let tab_id = state_for_context_command
                        .lock()
                        .ok()
                        .map(|mut app| app.create_tab(NavigationLocation::Directory(path.clone())));
                    if let Some(tab_id) = tab_id {
                        submit_path_navigation(
                            &sender,
                            &network_sender,
                            &state_for_context_command,
                            tab_id,
                            path,
                            NavigationKind::Refresh,
                        );
                    }
                }
            }
            CMD_NETWORK_LOCATION_MANAGE_CREDENTIALS => {
                if let Err(error) = platform::open_windows_credentials()
                    && let Ok(mut app) = state_for_context_command.lock()
                {
                    app.operation_errors
                        .push(format!("Failed to open Windows credentials: {error}"));
                }
            }
            CMD_NETWORK_LOCATION_RENAME => {
                let selected = selected_network_location(
                    &state_for_context_command,
                    &quick_menu_for_command,
                )
                .and_then(|location| {
                    state_for_context_command.lock().ok().map(|app| {
                        (
                            location,
                            app.language,
                            app.dark_theme(),
                        )
                    })
                });
                if let (Some((location, language, dark_theme)), Some(window), Some(ui)) =
                    (selected, network_location_rename_ui.upgrade(), weak.upgrade())
                {
                    configure_network_location_rename_window(
                        &window,
                        location.id,
                        &location.display_name,
                        language,
                        dark_theme,
                        location.source,
                    );
                    show_centered_dialog(&ui, &window);
                }
            }
            CMD_NETWORK_LOCATION_COPY_ADDRESS => {
                let sidebar_path = current_sidebar_menu_target(&state_for_context_command, &quick_menu_for_command).and_then(|target| match target.destination { SidebarDestination::Directory(path) | SidebarDestination::External(path) => Some(path), SidebarDestination::Library(_) | SidebarDestination::WebDav(_) => None });
                if let Some(path) = sidebar_path.or_else(|| selected_network_location_path(&state_for_context_command, &quick_menu_for_command)) {
                    let result = platform::windows::clipboard::write_text(path.as_os_str());
                    if let Err(error) = result
                        && let Ok(mut app) = state_for_context_command.lock()
                    {
                        app.operation_errors
                            .push(format!("Failed to copy network address: {error}"));
                    }
                }
            }
            CMD_NETWORK_LOCATION_REMOVE
                if selected_network_location(
                    &state_for_context_command,
                    &quick_menu_for_command,
                )
                .is_some_and(|location| {
                    location.source == NetworkLocationSource::WindowsImported
                        && location.shell_path.is_some()
                }) =>
            {
                let selected = selected_network_location(
                    &state_for_context_command,
                    &quick_menu_for_command,
                );
                if let Some(location) = selected
                    && let Some(delete_ui) = delete_weak_for_context.upgrade()
                {
                    let language = state_for_context_command
                        .lock()
                        .map(|app| app.language)
                        .unwrap_or(Language::Chinese);
                    delete_ui.set_kind(0);
                    delete_ui.set_title_text(match language {
                        Language::Chinese => "移除此网络位置？",
                        Language::English => "Remove this network location?",
                    }.into());
                    delete_ui.set_detail_text(explorer_network_location_warning(language).into());
                    delete_ui.set_primary_text(match language {
                        Language::Chinese => "移除",
                        Language::English => "Remove",
                    }.into());
                    if let Ok(mut app) = state_for_context_command.lock() {
                        app.pending_permanent_delete = None;
                        app.pending_imported_network_location_remove = Some(location);
                    }
                    if let Some(ui) = weak.upgrade() {
                        show_confirmation_window(&ui, None, &delete_ui);
                    }
                }
            }
            CMD_NETWORK_LOCATION_REMOVE
            | CMD_NETWORK_LOCATION_MOVE_UP
            | CMD_NETWORK_LOCATION_MOVE_DOWN => {
                let id = quick_menu_for_command
                    .lock()
                    .ok()
                    .and_then(|menu| menu.active_network_location);
                if let Some(id) = id
                    && let Ok(mut app) = state_for_context_command.lock()
                {
                    let mut catalog = NetworkLocationCatalog::new(app.network_locations.clone());
                    let result = match command {
                        CMD_NETWORK_LOCATION_REMOVE => catalog.remove(id).map(|_| ()),
                        CMD_NETWORK_LOCATION_MOVE_UP | CMD_NETWORK_LOCATION_MOVE_DOWN => {
                            let positions = catalog
                                .locations()
                                .iter()
                                .position(|location| location.id == id);
                            positions
                                .map(|position| {
                                    let next = if command == CMD_NETWORK_LOCATION_MOVE_UP {
                                        position.saturating_sub(1)
                                    } else {
                                        position
                                            .saturating_add(1)
                                            .min(catalog.locations().len().saturating_sub(1))
                                    };
                                    catalog.move_to(id, next)
                                })
                                .unwrap_or(Err(
                                    crate::network::NetworkLocationCatalogError::NotFound,
                                ))
                        }
                        _ => unreachable!(),
                    };
                    match result {
                        Ok(_) => { app.network_locations = catalog.locations().to_vec(); app.network_location_generation = app.network_location_generation.wrapping_add(1).max(1); },
                        Err(error) => app
                            .operation_errors
                            .push(format!("Failed to manage network location: {error:?}")),
                    }
                }
                refresh_all_windows(&state_for_context_command.shared, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
            }
            command if (CMD_VIEW_BASE..CMD_VIEW_BASE + 8).contains(&command) => {
                if let Some(mode) = ViewMode::from_storage_code((command - CMD_VIEW_BASE) as u8) {
                    let anchor = weak.upgrade().and_then(|ui| capture_zoom_anchor(&ui, 16.0, 0.0));
                    set_view_mode(&state_for_context_command, mode);
                    refresh_view_windows(&state_for_context_command, weak.upgrade().as_ref());
                    if let Some(ui) = weak.upgrade() {
                        restore_zoom_anchor(&ui, anchor);
                        request_visible_file_images(
                            &ui,
                            &state_for_context_command.shared,
                            state_for_context_command.window_id,
                            &thumbnail_scheduler,
                        );
                    }
                }
            }
            command if (CMD_SORT_BASE..CMD_SORT_BASE + 5).contains(&command) => {
                if let Some(field) = SortField::from_storage_code((command - CMD_SORT_BASE) as u8) {
                    set_sort_field(&state_for_context_command, field);
                    let search = state_for_context_command.lock().ok().and_then(|app| {
                        let tab = app.active();
                        (tab.page_source == PageSource::Search)
                            .then(|| (tab.id, tab.search_query.clone()))
                    });
                    if let Some((tab_id, query)) = search {
                        submit_search(
                            &everything_for_context_command,
                            &state_for_context_command.shared,
                            weak.upgrade().as_ref(),
                            tab_id,
                            query,
                        );
                    }
                    refresh_view_windows(&state_for_context_command, weak.upgrade().as_ref());
                }
            }
            CMD_SORT_ASC | CMD_SORT_DESC => {
                set_sort_direction(
                    &state_for_context_command,
                    if command == CMD_SORT_ASC {
                        SortDirection::Ascending
                    } else {
                        SortDirection::Descending
                    },
                );
                let search = state_for_context_command.lock().ok().and_then(|app| {
                    let tab = app.active();
                    (tab.page_source == PageSource::Search)
                        .then(|| (tab.id, tab.search_query.clone()))
                });
                if let Some((tab_id, query)) = search {
                    submit_search(
                        &everything_for_context_command,
                        &state_for_context_command.shared,
                        weak.upgrade().as_ref(),
                        tab_id,
                        query,
                    );
                }
                refresh_view_windows(&state_for_context_command, weak.upgrade().as_ref());
            }
            command
                if ((CMD_GROUP_BASE..CMD_GROUP_BASE + 6).contains(&command)
                    || matches!(command, CMD_GROUP_ASC | CMD_GROUP_DESC))
                    && apply_group_command(&state_for_context_command, command) =>
            {
                refresh_view_windows(&state_for_context_command, weak.upgrade().as_ref());
            }
            CMD_COLUMN_FIT | CMD_COLUMNS_FIT => {
                if let Ok(mut app) = state_for_context_command.lock() {
                    let current = quick_menu_for_command
                        .lock()
                        .ok()
                        .and_then(|menu| menu.active_column)
                        .unwrap_or(ColumnKind::Name);
                    let widths = std::array::from_fn(|index| {
                        fitted_column_width(
                            &app,
                            ColumnKind::from_storage_code(index as u8).unwrap(),
                        )
                    });
                    if command == CMD_COLUMNS_FIT {
                        app.update_active_column_layout(|layout| layout.widths = widths);
                    } else {
                        app.update_active_column_layout(|layout| {
                            layout.widths[current.storage_code() as usize] =
                                widths[current.storage_code() as usize]
                        });
                    }
                }
            }
            command
                if (CMD_COLUMN_TOGGLE_BASE..CMD_COLUMN_TOGGLE_BASE + ColumnKind::COUNT as i32)
                    .contains(&command) =>
            {
                if let Some(kind) =
                    ColumnKind::from_storage_code((command - CMD_COLUMN_TOGGLE_BASE) as u8)
                    && kind != ColumnKind::Name
                {
                    let index = kind.storage_code() as usize;
                    if let Ok(mut app) = state_for_context_command.lock() {
                        app.update_active_column_layout(|layout| {
                            layout.visible[index] = !layout.visible[index]
                        });
                    }
                }
            }
            command if command >= SHELL_CONTEXT_COMMAND_BASE => {
                let Some(identity) = quick_menu_for_command
                    .lock()
                    .ok()
                    .and_then(|menu| menu.identity.clone())
                    .filter(|identity| identity.ready)
                else {
                    return;
                };
                let command_id = (command - SHELL_CONTEXT_COMMAND_BASE) as u32;
                let shell_delete_permanent = quick_menu_for_command.lock().ok().is_some_and(|menu| {
                    menu.all_rows.iter().any(|row| {
                        row.id == command && row.search_text.as_str().eq_ignore_ascii_case("delete")
                    })
                }) && platform::windows::shift_key_down();
                if let Ok(mut menu) = quick_menu_for_command.lock() {
                    menu.pending_shell_delete_permanent = shell_delete_permanent;
                }
                let creates_item = quick_menu_for_command.lock().ok().is_some_and(|menu| {
                    menu.active_submenu_token
                        .is_some_and(|token| menu.create_submenu_token == Some(token))
                });
                if !quick_menu_key_is_current(&state_for_context_command.shared, &identity.key) {
                    if let Ok(mut app) = state_for_context_command.lock() {
                        app.operation_errors
                            .push("Discarded stale Shell menu invocation".to_owned());
                    }
                    return;
                }
                let (_, x, y) = *context_anchor.lock().expect("context anchor mutex");
                if let Some(ui) = weak.upgrade() {
                    let scale = ui.window().scale_factor();
                    let client = crate::quick_menu_popup::PhysicalPoint::new(
                        (x as f32 * scale).round() as i32,
                        (y as f32 * scale).round() as i32,
                    );
                    let Ok(screen) = platform::windows::quick_menu_window::client_point_to_screen(
                        native_window_handle(&ui),
                        client,
                    ) else {
                        return;
                    };
                    if creates_item
                        && let QuickMenuScope::Content { paths, location: Some(platform::windows::context_menu::ShellMenuBackgroundTarget::FileSystem(folder)) } = identity.key.scope.clone()
                        && paths.is_empty()
                        && let Ok(mut app) = state_for_context_command.lock()
                        && let Some(baseline) =
                            app.tab(identity.key.tab_id).map(directory_path_snapshot)
                    {
                        app.pending_shell_creates.insert(
                            identity.key.window_id,
                            PendingShellCreate {
                                window_id: identity.key.window_id,
                                tab_id: identity.key.tab_id,
                                directory: folder,
                                baseline,
                                started_at: Instant::now(),
                            },
                        );
                    }
                    if shell_menu_for_command
                        .send(platform::windows::context_menu::ShellMenuCommand::Invoke {
                            session_id: identity.session_id,
                            request_id: identity.request_id,
                            command_id,
                            owner_window: native_window_handle(&ui),
                            screen_x: screen.x,
                            screen_y: screen.y,
                        })
                        .is_err()
                        && let Ok(mut app) = state_for_context_command.lock()
                    {
                        app.pending_shell_creates.remove(&identity.key.window_id);
                    }
                }
            }
            _ => {}
        }
        if let Some(ui) = weak.upgrade() {
            refresh_regions(&ui, &state_for_context_command, regions);
        }
    });
}
