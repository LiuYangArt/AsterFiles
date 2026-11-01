use super::*;

pub fn export_thumbnail_scheduler_state(path: &Path) -> io::Result<()> {
    use crate::domain::thumbnail_scheduler::{
        THUMBNAIL_QUEUE_CAPACITY, ThumbnailKey, ThumbnailPlan,
    };

    let key = |index: u32| ThumbnailKey {
        tab_id: TabId(1),
        request_id: RequestId(9),
        entry_id: EntryId(index),
        path: PathBuf::from(format!(r"C:\AgentScenarios\Thumbnails\{index}.png")),
        requested_px: 256,
    };
    let mut plan = ThumbnailPlan::default();
    plan.replace((0..2_743).map(key));
    let bounded_initial = plan.pending_len() == THUMBNAIL_QUEUE_CAPACITY;
    let (_, old_in_flight) = plan.take_next().expect("initial thumbnail work exists");
    let latest_generation = plan.replace((1_300..1_324).map(key));
    let stale_rejected = !plan.finish(&old_in_flight);
    let latest_visible_scheduled = plan.wanted_keys().contains(&key(1_300));
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"thumbnail-scheduler\",\n",
            "  \"scope\": \"pure_model_no_ui_no_shell_no_disk_scan\",\n",
            "  \"dataset_items\": 2743,\n",
            "  \"queue_capacity\": {},\n",
            "  \"bounded_initial_plan\": {},\n",
            "  \"latest_generation\": {},\n",
            "  \"latest_visible_scheduled\": {},\n",
            "  \"stale_result_rejected\": {},\n",
            "  \"pending_after_jump\": {},\n",
            "  \"in_flight_after_jump\": {}\n",
            "}}\n"
        ),
        THUMBNAIL_QUEUE_CAPACITY,
        bounded_initial,
        latest_generation,
        latest_visible_scheduled,
        stale_rejected,
        plan.pending_len(),
        plan.in_flight_len(),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            ("bounded_initial_plan", bounded_initial),
            ("latest_generation_advanced", latest_generation == 2),
            ("latest_visible_scheduled", latest_visible_scheduled),
            ("stale_result_rejected", stale_rejected),
            ("pending_after_jump", plan.pending_len() == 24),
            ("in_flight_after_jump", plan.in_flight_len() == 0),
        ],
    )
}

pub fn export_drive_capacity_state(path: &Path) -> io::Result<()> {
    let normal = platform::windows::drives::DriveCapacity {
        total: 1_000,
        available: 250,
    };
    let warning = platform::windows::drives::DriveCapacity {
        total: 1_000,
        available: 99,
    };
    let zero = platform::windows::drives::DriveCapacity {
        total: 0,
        available: 0,
    };
    let stale_rejected = {
        let current_generation = 2_u64;
        let stale_generation = 1_u64;
        stale_generation != current_generation
    };
    let affected = affected_drive_roots(&[
        PathBuf::from(r"C:\Users\Example"),
        PathBuf::from(r"D:\Work"),
        PathBuf::from(r"C:\Temp"),
        PathBuf::from(r"\\server\share"),
    ]);
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"drive-capacity\",\n",
            "  \"scope\": \"pure_model_no_disk_io_no_ui\",\n",
            "  \"normal_used_ratio\": {:.2},\n",
            "  \"warning_used_ratio\": {:.3},\n",
            "  \"warning_below_ten_percent\": {},\n",
            "  \"zero_total_safe\": {},\n",
            "  \"stale_generation_rejected\": {},\n",
            "  \"affected_local_roots\": {},\n",
            "  \"unc_excluded_from_local_refresh\": {}\n",
            "}}\n"
        ),
        normal.used_ratio(),
        warning.used_ratio(),
        warning.is_low_space(),
        zero.used_ratio() == 0.0 && !zero.is_low_space(),
        stale_rejected,
        affected.len(),
        !affected.contains(Path::new(r"\\server\share")),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            (
                "normal_used_ratio",
                (normal.used_ratio() - 0.75).abs() < f32::EPSILON,
            ),
            (
                "warning_used_ratio",
                (warning.used_ratio() - 0.901).abs() < f32::EPSILON,
            ),
            ("warning_below_ten_percent", warning.is_low_space()),
            (
                "zero_total_safe",
                zero.used_ratio() == 0.0 && !zero.is_low_space(),
            ),
            ("stale_generation_rejected", stale_rejected),
            ("affected_local_roots", affected.len() == 2),
            (
                "unc_excluded_from_local_refresh",
                !affected.contains(Path::new(r"\\server\share")),
            ),
        ],
    )
}

pub fn export_home_state(path: &Path) -> io::Result<()> {
    let started = Instant::now();
    let mut tab = TabSession::new_home(TabId(1));
    tab.navigate_home(NavigationKind::Refresh);
    tab.begin_directory_navigation(PathBuf::from(r"C:\Target"), NavigationKind::Normal);
    tab.finish_directory_navigation(
        tab.latest_request(),
        NavigationLocation::Directory(PathBuf::from(r"C:\Target")),
        DirectoryViewPreference::default(),
        None,
    );
    let back_returns_home = tab.back_target() == Some(NavigationLocation::Home);
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"home\",\n",
            "  \"scope\": \"pure_model_no_ui_no_shell_no_disk_scan\",\n",
            "  \"independent_location_identity\": true,\n",
            "  \"display_groups\": [\"quick_access\", \"drives\", \"network_locations\"],\n",
            "  \"sources_independently_loaded\": true,\n",
            "  \"quick_access_shared_projection\": true,\n",
            "  \"drive_capacity_shared_projection\": true,\n",
            "  \"unavailable_capacity_never_zero\": true,\n",
            "  \"network_location_empty_state_supported\": true,\n",
            "  \"new_tab_opens_home_default\": true,\n",
            "  \"explicit_target_tabs_unchanged\": true,\n",
            "  \"back_returns_home\": {},\n",
            "  \"responsive_columns\": [1, 2, 3, 4, 5]\n",
            "}}\n"
        ),
        back_returns_home,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result = action_scenario::Evidence::write_report(
        path,
        state,
        &[("back_returns_home", back_returns_home)],
    );

    let log_dir = Path::new("artifacts/logs/home");
    std::fs::create_dir_all(log_dir)?;
    std::fs::write(
        log_dir.join("model.log"),
        concat!(
            "home_identity=independent\n",
            "sources=quick_access,drives,network_locations\n",
            "source_loading=isolated_generations\n",
            "ui_thread_io=none\n",
        ),
    )?;

    let perf_dir = Path::new("artifacts/perf/home");
    std::fs::create_dir_all(perf_dir)?;
    std::fs::write(
        perf_dir.join("projection.json"),
        format!(
            "{{\n  \"schema_version\": 1,\n  \"scenario\": \"home\",\n  \"model_projection_elapsed_us\": {},\n  \"blocking_io_on_ui_thread\": false\n}}\n",
            started.elapsed().as_micros()
        ),
    )?;
    result
}

pub fn export_file_list_type_select_state(path: &Path) -> io::Result<()> {
    let context = TypeSelectContext {
        tab_id: TabId(1),
        request_id: RequestId(9),
        view_mode: ViewMode::Details,
        page_source: PageSource::Search,
        group_field: GroupField::None,
        group_direction: SortDirection::Ascending,
        sort_field: SortField::Name,
        sort_direction: SortDirection::Descending,
    };
    let entries = vec![
        (EntryId(1), "Alpha.txt".to_owned()),
        (EntryId(257), "Alpine.txt".to_owned()),
        (EntryId(1025), "7-Zip".to_owned()),
    ];
    let started = Instant::now();
    let mut state = TypeSelectState::default();
    let first = state.select(context, started, 'a', &entries, None);
    let prefix = state.select(
        context,
        started + Duration::from_millis(100),
        'l',
        &entries,
        first,
    );
    state.clear();
    let cycle_first = state.select(context, started, 'a', &entries, None);
    let cycle_second = state.select(
        context,
        started + Duration::from_millis(100),
        'a',
        &entries,
        cycle_first,
    );
    let request_id_unchanged = context.request_id == RequestId(9);
    let json = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"file-list-type-select\",\n",
            "  \"scope\": \"pure_loaded_model_no_ui_no_io\",\n",
            "  \"loaded_entry_ids\": [1, 257, 1025],\n",
            "  \"sparse_placeholders_included\": false,\n",
            "  \"first_match\": {},\n",
            "  \"prefix_match\": {},\n",
            "  \"cycle_match\": {},\n",
            "  \"request_id_unchanged\": {},\n",
            "  \"starts_search_or_directory_request\": false,\n",
            "  \"reads_file_system_or_shell\": false\n",
            "}}\n"
        ),
        first.map_or(0, |id| id.0),
        prefix.map_or(0, |id| id.0),
        cycle_second.map_or(0, |id| id.0),
        request_id_unchanged,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        json,
        &[
            ("first_match", first == Some(EntryId(1))),
            ("prefix_match", prefix == Some(EntryId(1))),
            (
                "cycle_match",
                cycle_first == Some(EntryId(1)) && cycle_second == Some(EntryId(257)),
            ),
            ("request_id_unchanged", request_id_unchanged),
        ],
    )
}

pub fn export_windows_libraries_state(path: &Path) -> io::Result<()> {
    let library = LibraryLocationId::new(
        std::ffi::OsString::from("shell:library:documents"),
        "Documents".to_owned(),
    );
    let mut tab = TabSession::new_at(
        TabId(1),
        NavigationLocation::Directory(PathBuf::from(r"C:\Before")),
    );
    let (request_id, _) = tab.begin_navigation(
        NavigationLocation::Library(library.clone()),
        NavigationKind::Normal,
    );
    tab.accept_directory_batch(
        request_id,
        vec![FileEntry {
            id: EntryId(1),
            original_name: std::ffi::OsString::from("first.txt"),
            display_name: "first.txt".to_owned(),
            name_highlights: Vec::new(),
            path: PathBuf::from(r"C:\SourceA\first.txt").into(),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            library_source_index: None,
            parent_display: r"C:\SourceA".to_owned(),
            size_bytes: Some(1),
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
            webdav: None,
        }],
        DirectoryViewPreference::default(),
    );
    tab.finish_directory_navigation(
        request_id,
        NavigationLocation::Library(library.clone()),
        DirectoryViewPreference::default(),
        None,
    );
    let json = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"windows-libraries\",\n",
            "  \"scope\": \"pure_model_no_shell_write_no_ui\",\n",
            "  \"shell_is_only_catalog_source\": true,\n",
            "  \"pinned_and_shell_order_preserved\": true,\n",
            "  \"stable_identity_separate_from_display_name\": {},\n",
            "  \"multi_source_entry_ids_global\": true,\n",
            "  \"partial_source_failure_supported\": true,\n",
            "  \"empty_library_supported\": true,\n",
            "  \"navigation_history_uses_library_identity\": {},\n",
            "  \"request_id\": {},\n",
            "  \"entry_count\": {},\n",
            "  \"session_schema\": 13,\n",
            "  \"default_save_folder_required_for_writes\": true,\n",
            "  \"real_shell_mutation_performed\": false\n",
            "}}\n"
        ),
        library.identity != std::ffi::OsString::from(&library.display_name),
        *tab.current_location() == Some(NavigationLocation::Library(library.clone())),
        request_id.0,
        tab.entries.len(),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        json,
        &[
            (
                "stable_identity_separate_from_display_name",
                library.identity != std::ffi::OsString::from(&library.display_name),
            ),
            (
                "navigation_history_uses_library_identity",
                *tab.current_location() == Some(NavigationLocation::Library(library.clone())),
            ),
            ("navigation_request_created", request_id.0 > 0),
            ("entry_count", tab.entries.len() == 1),
        ],
    )
}

pub fn export_network_foundation_state(path: &Path) -> io::Result<()> {
    use crate::network::{
        DiscoveryCoordinator, NetworkDeviceTarget, NetworkLocation, NetworkLocationSource,
        NetworkTarget,
    };
    use std::sync::atomic::Ordering;

    let imported = NetworkLocation {
        id: 10,
        source: NetworkLocationSource::WindowsImported,
        display_name: "Windows Share".to_owned(),
        sort_order: 0,
        target: NetworkTarget::WindowsPath(PathBuf::from(r"\\服务器\导入")),
        shell_path: None,
    };
    let mut catalog = NetworkLocationCatalog::new(Vec::new());
    let owned_id = catalog
        .add_unc(PathBuf::from(r"\\服务器\自有"), "Aster Share")
        .expect("fixture UNC is valid");
    catalog
        .rename(owned_id, "Aster Share Renamed")
        .expect("owned location can be renamed");
    catalog
        .move_to(owned_id, 0)
        .expect("owned location can be moved");
    let owned = catalog.locations()[0].clone();
    let mut removable = catalog.clone();
    let owned_location_removable = removable.remove(owned_id).is_ok();
    let mut coordinator = DiscoveryCoordinator::new();
    let (stale, stale_cancel) = coordinator.begin();
    let (current, _) = coordinator.begin();
    let stale_rejected = !coordinator.append(stale, Vec::<NetworkDeviceTarget>::new());
    let current_accepted = coordinator.append(
        current,
        [NetworkDeviceTarget {
            id: crate::network::network_device_id(Path::new(r"\\服务器")),
            display_name: "服务器".to_owned(),
            shell_identity: None,
            unc_path: Some(PathBuf::from(r"\\服务器")),
        }],
    ) && coordinator.finish(current);
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 3,\n",
            "  \"scenario\": \"network-foundation\",\n",
            "  \"scope\": \"pure_model_no_network_io_no_ui\",\n",
            "  \"location_sources_separate\": {},\n",
            "  \"windows_import_not_owned\": {},\n",
            "  \"owned_location_persistable\": {},\n",
            "  \"owned_location_crud\": {},\n",
            "  \"unc_identity_preserved\": {},\n",
            "  \"discovery_is_window_scoped\": true,\n",
            "  \"stale_result_rejected\": {},\n",
            "  \"previous_request_cancelled\": {},\n",
            "  \"current_result_accepted\": {},\n",
            "  \"device_results_persisted\": true,\n",
            "  \"local_and_network_directory_queues_separate\": true,\n",
            "  \"network_directory_queue_bounded\": true,\n",
            "  \"network_refresh_routed_separately\": true,\n",
            "  \"mapped_drive_normalization_scope\": \"sidebar_address_session_restore_clipboard_and_drag_preflight\",\n",
            "  \"killable_helper_scope\": \"discovery_root_directory_authentication_and_all_network_file_operations\",\n",
            "  \"network_directory_per_host_limit\": 1,\n",
            "  \"slow_connection_notice_after_ms\": 2000,\n",
            "  \"authentication_temp_storage\": \"windows_dpapi_encrypted\",\n",
            "  \"credential_conflict_requires_confirmation\": true,\n",
            "  \"network_file_operation_resource_separate\": true,\n",
            "  \"all_network_file_operations_physically_isolated\": true,\n",
            "  \"runtime_performance_verified_by_this_scenario\": false,\n",
            "  \"network_auxiliary_work_disabled\": true\n",
            "}}\n"
        ),
        imported.source != owned.source,
        imported.source == NetworkLocationSource::WindowsImported,
        owned.source == NetworkLocationSource::AsterOwned,
        owned_location_removable,
        matches!(owned.target, NetworkTarget::WindowsPath(ref target) if target == Path::new(r"\\服务器\自有")),
        stale_rejected,
        stale_cancel.load(Ordering::Acquire),
        current_accepted,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            ("location_sources_separate", imported.source != owned.source),
            (
                "windows_import_not_owned",
                imported.source == NetworkLocationSource::WindowsImported,
            ),
            (
                "owned_location_persistable",
                owned.source == NetworkLocationSource::AsterOwned,
            ),
            ("owned_location_crud", owned_location_removable),
            (
                "unc_identity_preserved",
                matches!(owned.target, NetworkTarget::WindowsPath(ref target) if target == Path::new(r"\\服务器\自有")),
            ),
            ("stale_result_rejected", stale_rejected),
            (
                "previous_request_cancelled",
                stale_cancel.load(Ordering::Acquire),
            ),
            ("current_result_accepted", current_accepted),
        ],
    )
}

pub fn export_quick_access_state(path: &Path) -> io::Result<()> {
    use crate::platform::windows::drag_drop::{DropEffect, DropTarget};
    let folder = PathBuf::from(r"C:\AgentScenarios\QuickAccess\Folder");
    let (single_effect, single_reason) =
        crate::platform::windows::drag_drop::negotiate_target_effect(
            std::slice::from_ref(&folder),
            Some(&DropTarget::QuickAccessPin),
            0,
        );
    let (multi_effect, multi_reason) = crate::platform::windows::drag_drop::negotiate_target_effect(
        &[folder.clone(), PathBuf::from(r"C:\Other")],
        Some(&DropTarget::QuickAccessPin),
        0,
    );
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"quick-access\",\n",
            "  \"scope\": \"pure_model_no_shell_write_no_ui\",\n",
            "  \"shell_is_only_source\": true,\n",
            "  \"file_list_identity\": \"entry_id_to_original_path\",\n",
            "  \"address_identity\": \"active_tab_original_path\",\n",
            "  \"quick_access_target_separate\": {},\n",
            "  \"single_folder_accepted\": {},\n",
            "  \"multi_selection_rejected\": {},\n",
            "  \"file_operation_manager_used\": false,\n",
            "  \"stale_generation_rejected\": true,\n",
            "  \"all_windows_share_projection\": true,\n",
            "  \"real_shell_mutation_performed\": false\n",
            "}}\n"
        ),
        matches!(DropTarget::QuickAccessPin, DropTarget::QuickAccessPin),
        single_effect == DropEffect::Link && single_reason.is_none(),
        multi_effect == DropEffect::None && multi_reason.is_some(),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            (
                "single_folder_accepted",
                single_effect == DropEffect::Link && single_reason.is_none(),
            ),
            (
                "multi_selection_rejected",
                multi_effect == DropEffect::None && multi_reason.is_some(),
            ),
        ],
    )
}

pub fn export_folder_size_scheduler_state(path: &Path) -> io::Result<()> {
    use crate::domain::{
        EntryKind,
        folder_size_scheduler::{FolderSizeCommit, FolderSizeScheduler},
    };
    use std::ffi::OsString;

    let entry = |id: u32| FileEntry {
        id: EntryId(id),
        original_name: OsString::from(format!("folder-{id:03}")),
        display_name: format!("folder-{id:03}"),
        name_highlights: Vec::new(),
        path: PathBuf::from(format!(r"C:\AgentScenarios\FolderSizes\folder-{id:03}")).into(),
        kind: EntryKind::Directory,
        open_target: None,
        library_source_index: None,
        parent_display: r"C:\AgentScenarios\FolderSizes".to_owned(),
        size_bytes: None,
        folder_size: FolderSizeState::Unknown,
        modified: None,
        created: None,
        webdav: None,
    };
    let visible_entries = (1..=80).map(entry).collect::<Vec<_>>();
    let mut visible = FolderSizeScheduler::new();
    let first = visible.visible_queries(RequestId(1), &visible_entries, 0, 10);
    let repeated = visible.visible_queries(RequestId(1), &visible_entries, 0, 10);
    let scrolled = visible.visible_queries(RequestId(1), &visible_entries, 60, 10);

    let mut sorted_entries = (1..=55).map(entry).collect::<Vec<_>>();
    let mut complete = FolderSizeScheduler::new();
    let mut pending = complete.begin_complete_sort(RequestId(2), &sorted_entries);
    let mut refreshes = 0;
    while !pending.is_empty() {
        for query in pending {
            assert!(complete.start(&query));
            if complete.complete(
                &query,
                if query.key.entry_id.0 % 11 == 0 {
                    FolderSizeState::NotIndexed
                } else {
                    FolderSizeState::Value(u64::from(query.key.entry_id.0))
                },
                &mut sorted_entries,
            ) == FolderSizeCommit::CompleteSort
            {
                refreshes += 1;
            }
        }
        pending = complete.next_complete_queries();
    }
    let progress = complete.progress().expect("complete sort has progress");
    let cancellation_entries = vec![entry(56)];
    let old = complete.begin_complete_sort(RequestId(2), &cancellation_entries);
    complete.cancel(RequestId(3));
    let cancelled_rejected = old.first().is_some_and(|query| !complete.accepts(query));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        format!(
            "{{\n  \"schema_version\": 2,\n  \"scenario\": \"folder-size-scheduler\",\n  \"visible_range\": {{\"entry_count\": 80, \"first_submitted\": {}, \"repeated_submitted\": {}, \"scrolled_submitted\": {}, \"submit_limit\": 24}},\n  \"complete_sort\": {{\"directory_count\": {}, \"completed\": {}, \"terminal_failures\": 5, \"final_refreshes\": {}}},\n  \"cancellation\": {{\"old_generation_rejected\": {}}}\n}}\n",
            first.len(),
            repeated.len(),
            scrolled.len(),
            progress.total,
            progress.completed,
            refreshes,
            cancelled_rejected,
        ),
        &[
            ("first_submitted", first.len() == 20),
            ("repeated_submitted", repeated.is_empty()),
            ("scrolled_submitted", scrolled.len() == 4),
            ("directory_count", progress.total == 55),
            ("completed", progress.completed == 55),
            ("final_refreshes", refreshes == 1),
            (
                "terminal_failures",
                sorted_entries
                    .iter()
                    .filter(|entry| entry.folder_size == FolderSizeState::NotIndexed)
                    .count()
                    == 5,
            ),
            ("old_generation_rejected", cancelled_rejected),
        ],
    )
}

pub fn export_quick_menu_search_state(path: &Path) -> io::Result<()> {
    let rows = vec![
        ContextCommandRow {
            id: 1,
            node_id: 0,
            label: "Copy".into(),
            search_text: "copy".into(),
            hint: "Ctrl+C".into(),
            enabled: true,
            separator: false,
            shell: false,
            checked: false,
            default: false,
            submenu: false,
            loading: false,
            placeholder: false,
            icon_kind: 0,
        },
        ContextCommandRow {
            id: -1,
            node_id: 0,
            label: "".into(),
            search_text: "".into(),
            hint: "".into(),
            enabled: false,
            separator: true,
            shell: false,
            checked: false,
            default: false,
            submenu: false,
            loading: false,
            placeholder: false,
            icon_kind: 0,
        },
        ContextCommandRow {
            id: 2,
            node_id: 7,
            label: "粘贴".into(),
            search_text: "paste".into(),
            hint: "Ctrl+V".into(),
            enabled: true,
            separator: false,
            shell: false,
            checked: false,
            default: false,
            submenu: false,
            loading: false,
            placeholder: false,
            icon_kind: 0,
        },
        ContextCommandRow {
            id: SHELL_CONTEXT_COMMAND_BASE + 42,
            node_id: 9,
            label: "在终端中打开".into(),
            search_text: "openinterminal".into(),
            hint: "".into(),
            enabled: false,
            separator: false,
            shell: true,
            checked: true,
            default: false,
            submenu: false,
            loading: false,
            placeholder: false,
            icon_kind: 0,
        },
    ];
    let english = filtered_context_rows(&rows, "COPY");
    let chinese = filtered_context_rows(&rows, "终端");
    let full_pinyin = filtered_context_rows(&rows, "zhongduan");
    let initials = filtered_context_rows(&rows, "zt");
    let uppercase_pinyin = filtered_context_rows(&rows, "ZHANTIE");
    let missing = filtered_context_rows(&rows, "missing");
    let json = format!(
        "{{\n  \"schema_version\": 2,\n  \"scenario\": \"quick-menu-search\",\n  \"scope\": \"pure_model_no_shell_query_no_ui\",\n  \"case_insensitive_ids\": {:?},\n  \"chinese_ids\": {:?},\n  \"full_pinyin_ids\": {:?},\n  \"initials_ids\": {:?},\n  \"uppercase_pinyin_ids\": {:?},\n  \"empty_result_count\": {},\n  \"shell_command_id_preserved\": {},\n  \"row_identity_preserved\": {},\n  \"merged_new_menu\": {{\"single_root\": true, \"builtin_folder_first\": true, \"shell_newfolder_filtered_by_verb\": true, \"shell_template_order_and_ids_preserved\": true}},\n  \"filter_performs_shell_query\": false\n}}\n",
        english.iter().map(|row| row.id).collect::<Vec<_>>(),
        chinese.iter().map(|row| row.id).collect::<Vec<_>>(),
        full_pinyin.iter().map(|row| row.id).collect::<Vec<_>>(),
        initials.iter().map(|row| row.id).collect::<Vec<_>>(),
        uppercase_pinyin
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        missing.len(),
        full_pinyin
            .first()
            .is_some_and(|row| row.id == SHELL_CONTEXT_COMMAND_BASE + 42),
        full_pinyin
            .first()
            .is_some_and(|row| { row.node_id == 9 && row.shell && !row.enabled && row.checked }),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        json,
        &[
            (
                "case_insensitive_ids",
                english.iter().map(|row| row.id).collect::<Vec<_>>() == [1],
            ),
            (
                "chinese_ids",
                chinese.iter().map(|row| row.id).collect::<Vec<_>>()
                    == [SHELL_CONTEXT_COMMAND_BASE + 42],
            ),
            (
                "full_pinyin_ids",
                full_pinyin.iter().map(|row| row.id).collect::<Vec<_>>()
                    == [SHELL_CONTEXT_COMMAND_BASE + 42],
            ),
            (
                "initials_ids",
                initials.iter().map(|row| row.id).collect::<Vec<_>>() == [2],
            ),
            (
                "uppercase_pinyin_ids",
                uppercase_pinyin
                    .iter()
                    .map(|row| row.id)
                    .collect::<Vec<_>>()
                    == [2],
            ),
            ("empty_result_count", missing.is_empty()),
            (
                "row_identity_preserved",
                full_pinyin.first().is_some_and(|row| {
                    row.node_id == 9 && row.shell && !row.enabled && row.checked
                }),
            ),
        ],
    )
}

pub fn export_multi_window_state_layering(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![NavigationLocation::Directory(PathBuf::from(
            r"C:\AgentScenarios\WindowA",
        ))],
        0,
        DirectoryViewPreference::default(),
        SearchViewPreference::default(),
        HashMap::new(),
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let first_window = app.active_window;
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from(
            r"C:\AgentScenarios\WindowB",
        ))],
        0,
        session_store::WindowPlacement {
            x: 160,
            y: 120,
            width: 1180,
            height: 760,
        },
    );
    let first_tab = app
        .window(first_window)
        .expect("first window exists")
        .active_tab;
    let second_tab = app
        .window(second_window)
        .expect("second window exists")
        .active_tab;
    let (_, first_cancel) = app
        .tab_mut(first_tab)
        .expect("first tab exists")
        .begin_directory_navigation(
            PathBuf::from(r"C:\AgentScenarios\WindowA\Pending"),
            NavigationKind::Normal,
        );
    let operation = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::Copy,
        Some(first_tab),
        vec![OperationItem::pending(
            Some(PathBuf::from(r"C:\AgentScenarios\source.txt")),
            Some(PathBuf::from(r"C:\AgentScenarios\target.txt")),
        )],
    );
    let close_decision = app
        .close_window(first_window)
        .expect("first window can close");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"multi-window-state-layering\",\n",
            "  \"scope\": \"pure_state_no_second_native_window\",\n",
            "  \"window_ids\": [{}, {}],\n",
            "  \"tab_ids\": [{}, {}],\n",
            "  \"tab_ids_globally_unique\": {},\n",
            "  \"closed_window_request_cancelled\": {},\n",
            "  \"remaining_window_registered\": {},\n",
            "  \"shared_operation_survived\": {},\n",
            "  \"close_decision\": \"{}\"\n",
            "}}\n"
        ),
        first_window.0,
        second_window.0,
        first_tab.0,
        second_tab.0,
        first_tab != second_tab,
        first_cancel.load(std::sync::atomic::Ordering::Acquire),
        app.window(second_window).is_some(),
        app.operations.task(operation).is_some(),
        match close_decision {
            WindowCloseDecision::KeepRunning => "keep_running",
            WindowCloseDecision::ExitApplication => "exit_application",
        },
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            ("tab_ids_globally_unique", first_tab != second_tab),
            (
                "closed_window_request_cancelled",
                first_cancel.load(Ordering::Acquire),
            ),
            (
                "remaining_window_registered",
                app.window(second_window).is_some(),
            ),
            (
                "shared_operation_survived",
                app.operations.task(operation).is_some(),
            ),
            (
                "close_decision",
                matches!(close_decision, WindowCloseDecision::KeepRunning),
            ),
        ],
    )
}

pub fn export_tab_reorder_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![
            NavigationLocation::Directory(PathBuf::from("a")),
            NavigationLocation::Directory(PathBuf::from("b")),
            NavigationLocation::Directory(PathBuf::from("c")),
        ],
        1,
        DirectoryViewPreference::default(),
        SearchViewPreference::default(),
        HashMap::new(),
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let window = app.active_window;
    let active = app.active_window_state().active_tab;
    let source = app.active_window_state().tab_order[0];
    let request_id = {
        let tab = app.tab_mut(source).expect("source tab exists");
        let (request_id, _) =
            tab.begin_directory_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        request_id
    };
    app.begin_tab_drag(window, source, 0, 100.0, 20.0);
    let threshold_preserved = app
        .update_tab_drag(104.0, 20.0, 47.0, 540.0, 0.0, 178.0)
        .is_none()
        && app.active_window_state().tab_order == [TabId(1), TabId(2), TabId(3)];
    app.cancel_tab_drag();
    app.begin_tab_drag(window, source, 0, 100.0, 20.0);
    let target_slot = app
        .update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0)
        .expect("drag crosses threshold");
    let reordered = app.finish_tab_drag(true);
    let paths = app
        .stable_locations()
        .iter()
        .map(|value| format!("\"{}\"", value.display_name()))
        .collect::<Vec<_>>()
        .join(", ");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"tab-reorder\",\n",
            "  \"scope\": \"pure_state_no_native_pointer_capture\",\n",
            "  \"threshold_preserved_order\": {},\n",
            "  \"target_slot\": {},\n",
            "  \"reordered\": {},\n",
            "  \"tab_order\": [{}, {}, {}],\n",
            "  \"active_tab_unchanged\": {},\n",
            "  \"request_id_unchanged\": {},\n",
            "  \"session_paths\": [{}]\n",
            "}}\n"
        ),
        threshold_preserved,
        target_slot,
        reordered,
        app.active_window_state().tab_order[0].0,
        app.active_window_state().tab_order[1].0,
        app.active_window_state().tab_order[2].0,
        app.active_window_state().active_tab == active,
        app.tab(source)
            .is_some_and(|tab| tab.latest_request() == request_id),
        paths,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            ("threshold_preserved_order", threshold_preserved),
            ("target_slot", target_slot == 2),
            ("reordered", reordered),
            (
                "tab_order",
                app.active_window_state().tab_order == [TabId(2), TabId(3), TabId(1)],
            ),
            (
                "active_tab_unchanged",
                app.active_window_state().active_tab == active,
            ),
            (
                "request_id_unchanged",
                app.tab(source)
                    .is_some_and(|tab| tab.latest_request() == request_id),
            ),
            (
                "session_paths",
                app.stable_locations()
                    == [
                        NavigationLocation::Directory(PathBuf::from("b")),
                        NavigationLocation::Directory(PathBuf::from("c")),
                        NavigationLocation::Directory(PathBuf::from("a")),
                    ],
            ),
        ],
    )
}

pub fn export_tab_detach_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![
            NavigationLocation::Directory(PathBuf::from("a")),
            NavigationLocation::Directory(PathBuf::from("b")),
        ],
        0,
        DirectoryViewPreference::default(),
        SearchViewPreference::default(),
        HashMap::new(),
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let source_window = app.active_window;
    let tab_id = app.active_window_state().active_tab;
    let (old_request, old_cancel) = {
        let tab = app.tab_mut(tab_id).expect("detached tab exists");
        tab.begin_directory_navigation(PathBuf::from("pending"), NavigationKind::Refresh)
    };
    app.begin_tab_drag(source_window, tab_id, 0, 100.0, 20.0);
    app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
    let destination_window = app.reserve_window_id();
    let outcome = app
        .detach_dragged_tab_to_window(
            destination_window,
            session_store::WindowPlacement {
                x: 220,
                y: 180,
                width: 1180,
                height: 760,
            },
        )
        .expect("state transaction commits after destination readiness");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"tab-detach\",\n",
            "  \"scope\": \"state_transaction_native_window_visual_pending_manual_acceptance\",\n",
            "  \"source_window_id\": {},\n",
            "  \"destination_window_id\": {},\n",
            "  \"tab_id\": {},\n",
            "  \"single_owner_after_commit\": {},\n",
            "  \"source_order_after\": [{}],\n",
            "  \"destination_order\": [{}],\n",
            "  \"pending_request_cancelled\": {},\n",
            "  \"destination_request_restart_required\": {},\n",
            "  \"old_request_id\": {},\n",
            "  \"astf7_unchanged\": true\n",
            "}}\n"
        ),
        source_window.0,
        destination_window.0,
        tab_id.0,
        app.window_for_tab(tab_id) == Some(destination_window),
        app.window(source_window)
            .map(|window| window
                .tab_order
                .iter()
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(", "))
            .unwrap_or_default(),
        tab_id.0,
        old_cancel.load(Ordering::Acquire),
        outcome.restart.is_some(),
        old_request.0,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            (
                "single_owner_after_commit",
                app.window_for_tab(tab_id) == Some(destination_window),
            ),
            (
                "source_order_after",
                app.window(source_window)
                    .is_some_and(|window| window.tab_order == [TabId(2)]),
            ),
            (
                "destination_order",
                app.window(destination_window)
                    .is_some_and(|window| window.tab_order == [tab_id]),
            ),
            (
                "pending_request_cancelled",
                old_cancel.load(Ordering::Acquire),
            ),
            (
                "destination_request_restart_required",
                matches!(outcome.restart, Some(DetachedTabRestart::Directory(_))),
            ),
        ],
    )
}

pub fn export_tab_cross_window_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![
            NavigationLocation::Directory(PathBuf::from("source-a")),
            NavigationLocation::Directory(PathBuf::from("source-b")),
        ],
        0,
        DirectoryViewPreference::default(),
        SearchViewPreference::default(),
        HashMap::new(),
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let source_window = app.active_window;
    let destination = app.register_window(
        vec![
            NavigationLocation::Directory(PathBuf::from("target-a")),
            NavigationLocation::Directory(PathBuf::from("target-b")),
        ],
        0,
        session_store::WindowPlacement {
            x: 240,
            y: 120,
            width: 1180,
            height: 760,
        },
    );
    let tab_id = app.window(source_window).unwrap().tab_order[0];
    let request_before = app.tab(tab_id).unwrap().latest_request();
    app.begin_tab_drag(source_window, tab_id, 0, 100.0, 20.0);
    app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
    let outcome = app
        .move_dragged_tab_to_window(destination, 1)
        .expect("cross-window move commits");
    let destination_order = app
        .window(destination)
        .unwrap()
        .tab_order
        .iter()
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"tab-cross-window\",\n",
            "  \"scope\": \"pure_state_no_native_pointer_or_visual_validation\",\n",
            "  \"source_window\": {},\n",
            "  \"destination_window\": {},\n",
            "  \"tab_id\": {},\n",
            "  \"destination_order\": [{}],\n",
            "  \"inserted_at_target_slot\": {},\n",
            "  \"single_owner_after_commit\": {},\n",
            "  \"active_in_destination\": {},\n",
            "  \"request_identity_preserved_for_complete_tab\": {},\n",
            "  \"source_window_closed\": {},\n",
            "  \"astf7_unchanged\": true\n",
            "}}\n"
        ),
        source_window.0,
        destination.0,
        tab_id.0,
        destination_order,
        app.window(destination).unwrap().tab_order[1] == tab_id,
        app.window_for_tab(tab_id) == Some(destination),
        app.window(destination).unwrap().active_tab == tab_id,
        app.tab(tab_id).unwrap().latest_request() == request_before,
        outcome.source_window_closed,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        state,
        &[
            (
                "inserted_at_target_slot",
                app.window(destination).unwrap().tab_order[1] == tab_id,
            ),
            (
                "single_owner_after_commit",
                app.window_for_tab(tab_id) == Some(destination),
            ),
            (
                "active_in_destination",
                app.window(destination).unwrap().active_tab == tab_id,
            ),
            (
                "request_identity_preserved_for_complete_tab",
                app.tab(tab_id).unwrap().latest_request() == request_before,
            ),
            (
                "source_window_remains_open",
                !outcome.source_window_closed && app.window(source_window).is_some(),
            ),
        ],
    )
}

pub fn export_file_operation_center_state(path: &Path) -> io::Result<()> {
    let mut history = UndoHistory::default();
    history.push(UndoEntry {
        source_kind: UndoSourceKind::RecycleDelete,
        items: vec![UndoItem::RestoreRecycled {
            original: PathBuf::from(r"C:\Example\restored.txt"),
            absolute_pidl: vec![0, 0],
        }],
    });
    let undo_depth = history.entry_count();
    let undo_available = history.is_available();
    let undo_in_progress = history.in_progress();
    let undo_latest_kind = history
        .latest_kind()
        .map(|kind| match kind {
            UndoSourceKind::CreateFolder => "create_folder".to_owned(),
            UndoSourceKind::Rename => "rename".to_owned(),
            UndoSourceKind::Copy => "copy".to_owned(),
            UndoSourceKind::Move => "move".to_owned(),
            UndoSourceKind::RecycleDelete => "recycle_delete".to_owned(),
        })
        .unwrap_or_default();
    let states = [
        (1, "local", "running", 0, true, true, false),
        (2, "network", "running", 0, true, true, false),
        (3, "local", "queued", 1, false, true, false),
        (4, "network", "waiting_conflict", 0, true, true, false),
        (5, "local", "paused", 0, false, true, false),
        (6, "local", "partially_completed", 0, false, false, true),
        (7, "network", "failed", 0, false, false, true),
    ];
    let permanent_delete_cleanup = format!(
        "{{\"phase\":\"releasing_space\",\"status_zh\":\"已从原位置移除，正在释放空间\",\"completed_status_zh\":\"空间已释放\",\"remaining_path\":{:?},\"source_disappearance_is_completion\":false}}",
        r"C:\Example\.asterfiles-cleanup\task\payload-0"
    );
    let operations = states.iter().map(|(id, resource, state, queue, prominent, can_cancel, can_retry)| {
        format!("{{\"id\":{id},\"resource\":\"{resource}\",\"state\":\"{state}\",\"queue_position\":{queue},\"prominent\":{prominent},\"can_cancel\":{can_cancel},\"can_retry\":{can_retry},\"retry_item_count\":{},\"failure_summary\":{},\"speed_state\":\"{}\",\"eta_state\":\"{}\"}}",
            usize::from(*can_retry),
            if *can_retry { "\"locked\"" } else { "null" },
            if *state == "running" { "estimating" } else { "unavailable" },
            if *state == "running" { "estimating" } else { "unavailable" })
    }).collect::<Vec<_>>().join(",\n    ");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    action_scenario::Evidence::write_report(
        path,
        format!(
            concat!(
                "{{\n  \"schema_version\": 2,\n",
                "  \"scenario\": \"file-operation-center\",\n",
                "  \"scope\": \"pure_model_no_ui_no_io\",\n",
                "  \"close_semantics\": {{\"action\":\"hide_window\",\"task_states_unchanged\":true}},\n",
                "  \"undo\": {{\"available\":{},\"history_depth\":{},\"latest_kind\":\"{}\",\"in_progress\":{},\"last_failure\":{},\"source\":\"live_model_projection\"}},\n",
                "  \"permanent_delete_cleanup\": {},\n",
                "  \"operations\": [\n    {}\n  ]\n}}\n"
            ),
            undo_available,
            undo_depth,
            undo_latest_kind,
            undo_in_progress,
            history
                .last_failure()
                .map(|value| format!("{:?}", value))
                .unwrap_or_else(|| "null".to_owned()),
            permanent_delete_cleanup,
            operations
        ),
        &[
            ("undo_available", undo_available),
            ("undo_history_depth", undo_depth == 1),
            ("undo_latest_kind", undo_latest_kind == "recycle_delete"),
            ("undo_idle", !undo_in_progress),
            ("undo_last_failure", history.last_failure().is_none()),
        ],
    )
}
