use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    io,
    ops::Deref,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use slint::{
    Color, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    winit_030::winit::event::MouseScrollDelta,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{
    agent_debug::{self, AgentScenario},
    domain::{
        AddressMode, ColumnKind, ColumnLayout, DirectoryViewPreference, EntryId, FileEntry,
        FolderSizeState, GroupField, LoadState, MAX_DIRECTORY_VIEW_PREFERENCES,
        NameHighlightSegment, NavigationKind, PageSource, RectangleSelectionMode, RequestId,
        SearchDepth, SearchScope, SearchState, SearchViewPreference, SortDirection, SortField,
        TabId, TabKind, TabSession, ViewMode,
        file_operations::{
            FileOperationKind, ItemState, OperationId, OperationItem, OperationManager,
            OperationResource, OperationResult, OperationState,
        },
        folder_size_scheduler::{FOLDER_SIZE_QUEUE_CAPACITY, FolderSizeCommit, FolderSizeQuery},
    },
    fs::{ReadOutcome, read_directory_batches_filtered},
    group_projection::{
        self, GroupProjectionContext, IconProjection, IconVisualRow, ListProjection, ListVisualRow,
    },
    i18n::{Language, Texts},
    network::{
        DiscoveryCoordinator, DiscoveryRequestId, DiscoveryState, NetworkDeviceTarget,
        NetworkExecutionKey, NetworkLocation, NetworkLocationCatalog, NetworkLocationSource,
        NetworkTarget,
    },
    platform::{self, KnownLocation, KnownLocationKind},
    session_store,
};

slint::include_modules!();

const WORKER_COUNT: usize = 4;
const NETWORK_WORKER_COUNT: usize = 2;
const ICON_WORKER_COUNT: usize = 2;
const DIRECTORY_EVENT_INTERVAL: Duration = Duration::from_millis(16);
const REBUILT_PROJECTION_BATCH_SIZE: usize = 256;
const THUMBNAIL_CACHE_CAPACITY: usize = 128;
const LARGE_ICON_CACHE_CAPACITY: usize = 128;
const TYPE_SELECT_TIMEOUT: Duration = Duration::from_millis(1_000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypeSelectContext {
    tab_id: TabId,
    request_id: RequestId,
    view_mode: ViewMode,
    page_source: PageSource,
    group_field: GroupField,
    group_direction: SortDirection,
    sort_field: SortField,
    sort_direction: SortDirection,
}

#[derive(Debug, Default)]
struct TypeSelectState {
    buffer: String,
    last_input: Option<Instant>,
    context: Option<TypeSelectContext>,
}

impl TypeSelectState {
    fn clear(&mut self) -> bool {
        let changed =
            !self.buffer.is_empty() || self.last_input.is_some() || self.context.is_some();
        self.buffer.clear();
        self.last_input = None;
        self.context = None;
        changed
    }

    fn select(
        &mut self,
        context: TypeSelectContext,
        now: Instant,
        typed: char,
        entries: &[(EntryId, String)],
        focused: Option<EntryId>,
    ) -> Option<EntryId> {
        if self.context != Some(context) {
            self.clear();
        }
        self.context = Some(context);
        if entries.is_empty() {
            return None;
        }

        let typed = typed.to_lowercase().collect::<String>();
        let expired = self
            .last_input
            .is_none_or(|last| now.saturating_duration_since(last) >= TYPE_SELECT_TIMEOUT);
        let cycle = !expired && self.buffer == typed;
        let start_after_focus = cycle || expired;
        if expired || cycle || self.buffer.is_empty() {
            self.buffer.clone_from(&typed);
        } else {
            self.buffer.push_str(&typed);
        }
        self.last_input = Some(now);

        let focused_index =
            focused.and_then(|id| entries.iter().position(|(entry_id, _)| *entry_id == id));
        let start = match focused_index {
            Some(index) if start_after_focus => (index + 1) % entries.len(),
            Some(index) => index,
            None => 0,
        };
        (0..entries.len())
            .map(|offset| (start + offset) % entries.len())
            .find_map(|index| {
                entries[index]
                    .1
                    .to_lowercase()
                    .starts_with(&self.buffer)
                    .then_some(entries[index].0)
            })
    }

    fn is_active(&self) -> bool {
        !self.buffer.is_empty()
    }
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
    std::fs::write(path, json)
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
            "  \"killable_helper_scope\": \"discovery_root_directory_authentication_create_folder_and_rename\",\n",
            "  \"network_directory_per_host_limit\": 1,\n",
            "  \"slow_connection_notice_after_ms\": 2000,\n",
            "  \"authentication_temp_storage\": \"windows_dpapi_encrypted\",\n",
            "  \"credential_conflict_requires_confirmation\": true,\n",
            "  \"network_file_operation_resource_separate\": true,\n",
            "  \"all_network_file_operations_physically_isolated\": false,\n",
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
    std::fs::write(path, state)
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
    std::fs::write(path, state)
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
        path: PathBuf::from(format!(r"C:\AgentScenarios\FolderSizes\folder-{id:03}")),
        kind: EntryKind::Directory,
        open_target: None,
        parent_display: r"C:\AgentScenarios\FolderSizes".to_owned(),
        size_bytes: None,
        folder_size: FolderSizeState::Unknown,
        modified: None,
        created: None,
    };
    let mut visible_entries = (1..=80).map(entry).collect::<Vec<_>>();
    let mut visible = FolderSizeScheduler::new();
    let first = visible.visible_queries(RequestId(1), &mut visible_entries, 0, 10);
    let repeated = visible.visible_queries(RequestId(1), &mut visible_entries, 0, 10);
    let scrolled = visible.visible_queries(RequestId(1), &mut visible_entries, 60, 10);

    let mut sorted_entries = (1..=55).map(entry).collect::<Vec<_>>();
    let mut complete = FolderSizeScheduler::new();
    let mut pending = complete.begin_complete_sort(RequestId(2), &mut sorted_entries);
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
        pending = complete.next_complete_queries(&mut sorted_entries);
    }
    let progress = complete.progress().expect("complete sort has progress");
    let old = complete.begin_complete_sort(RequestId(2), &mut sorted_entries);
    complete.cancel(RequestId(3));
    let cancelled_rejected = old.first().is_none_or(|query| !complete.accepts(query));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(
        path,
        format!(
            "{{\n  \"schema_version\": 1,\n  \"scenario\": \"folder-size-scheduler\",\n  \"visible_range\": {{\"entry_count\": 80, \"first_submitted\": {}, \"repeated_submitted\": {}, \"scrolled_submitted\": {}, \"submit_limit\": 24}},\n  \"complete_sort\": {{\"directory_count\": {}, \"completed\": {}, \"terminal_failures\": 5, \"final_refreshes\": {}}},\n  \"cancellation\": {{\"old_generation_rejected\": {}}}\n}}\n",
            first.len(),
            repeated.len(),
            scrolled.len(),
            progress.total,
            progress.completed,
            refreshes,
            cancelled_rejected,
        ),
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
            id: SHELL_CONTEXT_COMMAND_BASE + 42,
            node_id: 0,
            label: "在终端中打开".into(),
            search_text: "openinterminal".into(),
            hint: "".into(),
            enabled: true,
            separator: false,
            shell: true,
            checked: false,
            default: false,
            submenu: false,
            loading: false,
            placeholder: false,
            icon_kind: 0,
        },
    ];
    let english = filtered_context_rows(&rows, "COPY");
    let chinese = filtered_context_rows(&rows, "终端");
    let missing = filtered_context_rows(&rows, "missing");
    let json = format!(
        "{{\n  \"schema_version\": 1,\n  \"scenario\": \"quick-menu-search\",\n  \"scope\": \"pure_model_no_shell_query_no_ui\",\n  \"case_insensitive_ids\": {:?},\n  \"chinese_ids\": {:?},\n  \"empty_result_count\": {},\n  \"shell_command_id_preserved\": {},\n  \"filter_performs_shell_query\": false\n}}\n",
        english.iter().map(|row| row.id).collect::<Vec<_>>(),
        chinese.iter().map(|row| row.id).collect::<Vec<_>>(),
        missing.len(),
        chinese
            .first()
            .is_some_and(|row| row.id == SHELL_CONTEXT_COMMAND_BASE + 42),
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, json)
}

pub fn export_multi_window_state_layering(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from(r"C:\AgentScenarios\WindowA")],
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
        vec![PathBuf::from(r"C:\AgentScenarios\WindowB")],
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
        .begin_navigation(
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
    std::fs::write(path, state)
}

pub fn export_tab_reorder_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
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
            tab.begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
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
        .stable_paths()
        .iter()
        .map(|value| format!("\"{}\"", value.display()))
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
            .is_some_and(|tab| tab.latest_request == request_id),
        paths,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

pub fn export_tab_detach_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from("a"), PathBuf::from("b")],
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
    let old_request = {
        let tab = app.tab_mut(tab_id).expect("detached tab exists");
        tab.begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh)
            .0
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
        matches!(outcome.restart, Some(DetachedTabRestart::Directory(_))),
        outcome.restart.is_some(),
        old_request.0,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

pub fn export_tab_cross_window_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from("source-a"), PathBuf::from("source-b")],
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
        vec![PathBuf::from("target-a"), PathBuf::from("target-b")],
        0,
        session_store::WindowPlacement {
            x: 240,
            y: 120,
            width: 1180,
            height: 760,
        },
    );
    let tab_id = app.window(source_window).unwrap().tab_order[0];
    let request_before = app.tab(tab_id).unwrap().latest_request;
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
        app.tab(tab_id).unwrap().latest_request == request_before,
        outcome.source_window_closed,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

type SharedSessions = Arc<Mutex<AppState>>;

#[derive(Clone)]
struct WorkerSenders {
    directory: mpsc::Sender<DirectoryRequest>,
    network_directory: mpsc::SyncSender<DirectoryRequest>,
    network_discovery: mpsc::SyncSender<NetworkDiscoveryRequest>,
    operation: mpsc::Sender<FileOperationRequest>,
    clipboard: mpsc::Sender<ClipboardRequest>,
    shell_menu: platform::windows::context_menu::ShellMenuWorker,
    everything: mpsc::Sender<EverythingRequest>,
    icon: mpsc::Sender<IconRequest>,
    network_login: slint::Weak<NetworkLoginWindow>,
    network_login_state: Arc<Mutex<NetworkLoginCoordinator>>,
    network_location_rename: slint::Weak<NetworkLocationRenameWindow>,
}

#[derive(Clone)]
struct ConfirmationWindows {
    delete: slint::Weak<ConfirmationWindow>,
    conflict: slint::Weak<ConfirmationWindow>,
    exit: slint::Weak<ConfirmationWindow>,
}

impl ConfirmationWindows {
    fn new(
        delete: &ConfirmationWindow,
        conflict: &ConfirmationWindow,
        exit: &ConfirmationWindow,
    ) -> Self {
        Self {
            delete: delete.as_weak(),
            conflict: conflict.as_weak(),
            exit: exit.as_weak(),
        }
    }
}

#[derive(Clone)]
struct WindowSessions {
    shared: SharedSessions,
    window_id: WindowId,
}

impl WindowSessions {
    fn new(shared: SharedSessions, window_id: WindowId) -> Self {
        Self { shared, window_id }
    }

    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, AppState>> {
        let mut app = self.shared.lock()?;
        if app.windows.contains_key(&self.window_id) {
            app.active_window = self.window_id;
        }
        Ok(app)
    }

    #[cfg(test)]
    fn peek(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, AppState>> {
        self.shared.lock()
    }
}

impl Deref for WindowSessions {
    type Target = SharedSessions;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

struct WindowRuntime {
    ui: AppWindow,
    sessions: WindowSessions,
    _native_drop_timer: slint::Timer,
    _rectangle_selection_timer: Rc<slint::Timer>,
    _quick_menu: SharedQuickMenu,
    _quick_submenu_timer: Rc<slint::Timer>,
    _quick_menu_prewarm_timer: slint::Timer,
    quick_menu_popup: QuickMenuPopupRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupPresentation {
    Hidden,
    Cloaked,
    ShownCloaked,
    Presented,
}
struct QuickMenuPopupRuntime {
    root: QuickMenuWindow,
    branches: Vec<QuickSubmenuPopupRuntime>,
    session: crate::quick_menu_popup::QuickMenuPopupSession,
    owner_hwnd: isize,
    client_anchor: crate::quick_menu_popup::PhysicalPoint,
    work_area: crate::quick_menu_popup::PhysicalRect,
    root_rect: Option<crate::quick_menu_popup::PhysicalRect>,
    next_generation: u64,
    next_branch: u64,
    shown_once: bool,
    presentation: PopupPresentation,
    cloak_generation: Option<u64>,
}

struct QuickSubmenuPopupRuntime {
    window: QuickSubmenuWindow,
    event: Option<crate::quick_menu_popup::MenuEventIdentity>,
    rows: Vec<ContextCommandRow>,
    active_index: i32,
    anchor_y: f32,
    presentation: PopupPresentation,
    cloak_event: Option<crate::quick_menu_popup::MenuEventIdentity>,
}

thread_local! {
    static WINDOW_RUNTIMES: RefCell<HashMap<WindowId, WindowRuntime>> = RefCell::new(HashMap::new());
}

fn native_tab_drag_image(
    ui: &AppWindow,
    state: &SharedSessions,
) -> Option<platform::windows::drag_drop::TabDragImage> {
    let scale = ui.window().scale_factor();
    let app = state.lock().ok()?;
    let drag = app.tab_drag?;
    let tab = app.tab(drag.tab_id)?;
    let icon = tab
        .visible_path()
        .and_then(|path| app.icon_cache.get(path))
        .map(|icon| (icon.width, icon.height, icon.pixels.clone()));
    let title = tab
        .visible_path()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| display_path(tab.visible_path().unwrap_or(Path::new("C:\\"))));
    Some(platform::windows::drag_drop::TabDragImage {
        title,
        icon,
        width_px: (ui.get_tab_current_width().max(80.0) * scale).round() as u32,
        height_px: (34.0 * scale).round() as u32,
        grab_x_px: ((drag.press_x
            - 47.0
            - ui.get_tab_viewport_x()
            - drag.source_index as f32 * (ui.get_tab_current_width() + 5.0))
            * scale)
            .round() as i32,
        dark: app.dark_theme(),
        active: app
            .window(drag.window_id)
            .is_some_and(|window| window.active_tab == drag.tab_id),
    })
}

fn screen_to_client_physical(screen_x: i32, screen_y: i32, left: i32, top: i32) -> (f64, f64) {
    (f64::from(screen_x - left), f64::from(screen_y - top))
}

fn grid_thumbnail_request_px(view_mode: ViewMode, scale: f32) -> u32 {
    const STANDARD_SIZES: [u32; 8] = [16, 24, 32, 48, 64, 96, 128, 256];
    let requested = (file_layout_geometry(view_mode).icon_request_px as f32 * scale)
        .round()
        .max(32.0) as u32;
    STANDARD_SIZES
        .into_iter()
        .find(|size| *size >= requested)
        .unwrap_or(requested)
}

fn grid_thumbnail_request_rows(
    row_extents: &[f32],
    viewport_y: f32,
    visible_height: f32,
    prefetch_extent: f32,
) -> Vec<usize> {
    let visible_top = (-viewport_y).max(0.0);
    let request_top = (visible_top - prefetch_extent).max(0.0);
    let request_bottom = visible_top + visible_height.max(0.0) + prefetch_extent;
    let mut row_top = 0.0;
    row_extents
        .iter()
        .enumerate()
        .filter_map(|(index, extent)| {
            let row_bottom = row_top + extent.max(0.0);
            let visible = row_bottom >= request_top && row_top <= request_bottom;
            row_top = row_bottom;
            visible.then_some(index)
        })
        .collect()
}

fn grid_thumbnail_requests(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
) -> Vec<IconRequest> {
    let visible_height = ui.window().size().height as f32 / ui.window().scale_factor()
        - ui.get_file_list_top()
        - 30.0;
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let Some(window) = app.window(window_id) else {
        return Vec::new();
    };
    let Some(tab) = window.tabs.get(&window.active_tab) else {
        return Vec::new();
    };
    let Some(view_mode) = app.view_mode_for_tab(tab.id) else {
        return Vec::new();
    };
    if tab.kind != TabKind::Files || !view_mode.uses_grid_layout() {
        return Vec::new();
    }
    let requested_px = grid_thumbnail_request_px(view_mode, ui.window().scale_factor());

    let row_height = file_layout_geometry(view_mode).row_height;
    let grid_rows = ui.get_grid_rows();
    let request_rows = grid_thumbnail_request_rows(
        &grid_rows
            .iter()
            .map(|row| if row.group_header { 32.0 } else { row_height })
            .collect::<Vec<_>>(),
        ui.get_file_viewport_y(),
        visible_height,
        row_height * 2.0,
    );
    let entry_ids = request_rows
        .into_iter()
        .flat_map(|row_index| {
            grid_rows.row_data(row_index).map_or_else(Vec::new, |row| {
                row.entries
                    .iter()
                    .filter_map(|entry| (entry.id > 0).then_some(EntryId(entry.id as u32)))
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let requests = entry_ids
        .into_iter()
        .filter_map(|entry_id| {
            let entry = tab.visible_entry(entry_id)?;
            (!app
                .thumbnail_cache
                .contains_key(&(entry.path.clone(), requested_px))
                && !app
                    .large_icon_cache
                    .contains_key(&(entry.path.clone(), requested_px))
                && !app.thumbnail_requests.contains(&(
                    tab.id,
                    tab.latest_request,
                    entry.path.clone(),
                    requested_px,
                )))
            .then(|| IconRequest {
                tab_id: tab.id,
                request_id: tab.latest_request,
                target: IconTarget::Entry(entry.id),
                path: entry.path.clone(),
                thumbnail: true,
                requested_px,
            })
        })
        .collect::<Vec<_>>();
    for request in &requests {
        app.thumbnail_requests.insert((
            request.tab_id,
            request.request_id,
            request.path.clone(),
            request.requested_px,
        ));
    }
    requests
}

fn request_grid_thumbnails(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    sender: &mpsc::Sender<IconRequest>,
) {
    for request in grid_thumbnail_requests(ui, state, window_id) {
        let _ = sender.send(request);
    }
}

fn defer_grid_thumbnails(state: SharedSessions, tab_id: TabId, sender: mpsc::Sender<IconRequest>) {
    slint::Timer::single_shot(DIRECTORY_EVENT_INTERVAL, move || {
        let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id)) else {
            return;
        };
        let Some(ui) = window_ui(window_id) else {
            return;
        };
        request_grid_thumbnails(&ui, &state, window_id, &sender);
    });
}

fn refresh_all_windows(state: &SharedSessions) {
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .map(|(id, runtime)| (*id, runtime.ui.clone_strong()))
            .collect::<Vec<_>>()
    });
    for (window_id, ui) in windows {
        if state
            .lock()
            .is_ok_and(|app| app.window(window_id).is_some())
        {
            refresh_window_ui(&ui, state, window_id);
        }
    }
}

fn refresh_tab_window(state: &SharedSessions, tab_id: TabId) {
    let window_id = state.lock().ok().and_then(|app| app.window_for_tab(tab_id));
    if let Some(window_id) = window_id
        && let Some(ui) = window_ui(window_id)
    {
        refresh_window_ui(&ui, state, window_id);
    }
}

fn window_ui(window_id: WindowId) -> Option<AppWindow> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .get(&window_id)
            .map(|runtime| runtime.ui.clone_strong())
    })
}

fn cross_window_drop_target(
    source_window: WindowId,
    screen_x: i32,
    screen_y: i32,
    state: &SharedSessions,
) -> Option<(WindowId, usize)> {
    let screen_x = f64::from(screen_x);
    let screen_y = f64::from(screen_y);
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes.iter().find_map(|(window_id, runtime)| {
            if *window_id == source_window {
                return None;
            }
            let (target_left, target_top, target_right, target_bottom) =
                platform::windows::drag_drop::client_screen_rect(native_window_handle(&runtime.ui))
                    .ok()?;
            if screen_x < f64::from(target_left)
                || screen_x >= f64::from(target_right)
                || screen_y < f64::from(target_top)
                || screen_y >= f64::from(target_bottom)
            {
                return None;
            }
            let (x, y) = physical_client_to_logical(
                screen_x,
                screen_y,
                target_left,
                target_top,
                runtime.ui.window().scale_factor(),
            );
            if !(0.0..=96.0).contains(&y) {
                return None;
            }
            let file_boundary = state.lock().ok().and_then(|app| {
                let window = app.window(*window_id)?;
                Some(
                    window
                        .tab_order
                        .iter()
                        .position(|id| {
                            window
                                .tabs
                                .get(id)
                                .is_some_and(|tab| tab.kind == TabKind::Settings)
                        })
                        .unwrap_or(window.tab_order.len()),
                )
            })?;
            let slot = external_tab_insertion_slot(
                x,
                47.0,
                runtime.ui.get_tab_strip_width(),
                runtime.ui.get_tab_viewport_x(),
                runtime.ui.get_tab_current_width(),
                file_boundary,
            )
            .or_else(|| (x >= 0.0 && x <= runtime.ui.get_window_width()).then_some(file_boundary));
            slot.map(|slot| (*window_id, slot))
        })
    })
}

fn source_tab_drop_is_valid(
    source_ui: &AppWindow,
    screen_x: i32,
    screen_y: i32,
) -> Option<(bool, winit::dpi::PhysicalPosition<f64>)> {
    let (left, top, right, _) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(source_ui)).ok()?;
    let (client_x, client_y) = screen_to_client_physical(screen_x, screen_y, left, top);
    let logical = winit::dpi::PhysicalPosition::new(client_x, client_y)
        .to_logical::<f32>(f64::from(source_ui.window().scale_factor()));
    let strip_x = 47.0;
    let strip_right = strip_x + source_ui.get_tab_strip_width();
    let valid = screen_x >= left
        && screen_x < right
        && (0.0..=46.0).contains(&logical.y)
        && (strip_x..=strip_right).contains(&logical.x);
    Some((valid, winit::dpi::PhysicalPosition::new(client_x, client_y)))
}

fn physical_client_to_logical(
    screen_x: f64,
    screen_y: f64,
    client_left: i32,
    client_top: i32,
    scale: f32,
) -> (f32, f32) {
    (
        ((screen_x - f64::from(client_left)) / f64::from(scale)) as f32,
        ((screen_y - f64::from(client_top)) / f64::from(scale)) as f32,
    )
}

fn insertion_indicator_screen_rect(
    ui: &AppWindow,
    insertion_index: usize,
) -> Option<(i32, i32, i32, i32)> {
    let (left, top, _, _) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(ui)).ok()?;
    let scale = ui.window().scale_factor();
    let strip_width = ui.get_tab_strip_width();
    let logical_x = (47.0
        + ui.get_tab_viewport_x()
        + if insertion_index == 0 {
            0.0
        } else {
            insertion_index as f32 * (ui.get_tab_current_width() + 5.0) - 5.0
        })
    .clamp(47.0, 47.0 + strip_width - 5.0);
    Some((
        left + (logical_x * scale).round() as i32,
        top + (12.0 * scale).round() as i32,
        (platform::windows::tab_insertion_indicator::INDICATOR_WIDTH * scale)
            .round()
            .max(1.0) as i32,
        (34.0 * scale).round().max(1.0) as i32,
    ))
}

fn project_native_insertion_indicator(target: Option<(WindowId, usize)>, state: &SharedSessions) {
    let dark_theme = state.lock().ok().is_some_and(|app| app.dark_theme());
    let projected = target.and_then(|(window_id, insertion_index)| {
        let ui = window_ui(window_id)?;
        let (x, y, width, height) = insertion_indicator_screen_rect(&ui, insertion_index)?;
        platform::windows::tab_insertion_indicator::show(x, y, width, height, dark_theme).ok()?;
        Some(())
    });
    if projected.is_none() {
        platform::windows::tab_insertion_indicator::hide();
    }
}

fn project_native_tab_target(
    window_id: WindowId,
    event: platform::windows::drag_drop::TabTargetEvent,
    state: &SharedSessions,
) {
    let Some(ui) = window_ui(window_id) else {
        return;
    };
    let source_hover = matches!(
        event,
        platform::windows::drag_drop::TabTargetEvent::Hover { .. }
    ) && state
        .lock()
        .ok()
        .and_then(|app| app.tab_drag.map(|drag| drag.window_id))
        == Some(window_id);
    let target = match event {
        platform::windows::drag_drop::TabTargetEvent::Hover { screen_x, screen_y } => {
            let source_window = state
                .lock()
                .ok()
                .and_then(|app| app.tab_drag.map(|drag| drag.window_id));
            source_window.and_then(|source_window| {
                if source_window == window_id {
                    let (left, top, _, _) =
                        platform::windows::drag_drop::client_screen_rect(native_window_handle(&ui))
                            .ok()?;
                    let (x, y) = physical_client_to_logical(
                        f64::from(screen_x),
                        f64::from(screen_y),
                        left,
                        top,
                        ui.window().scale_factor(),
                    );
                    let insertion = state.lock().ok().and_then(|mut app| {
                        app.update_tab_drag(
                            x,
                            y,
                            47.0,
                            ui.get_tab_strip_width(),
                            ui.get_tab_viewport_x(),
                            ui.get_tab_current_width(),
                        )
                    });
                    project_native_insertion_indicator(
                        insertion.map(|index| (window_id, index)),
                        state,
                    );
                    None
                } else {
                    cross_window_drop_target(source_window, screen_x, screen_y, state)
                }
            })
        }
        platform::windows::drag_drop::TabTargetEvent::Leave
        | platform::windows::drag_drop::TabTargetEvent::Drop { .. } => None,
    };
    if !source_hover {
        project_native_insertion_indicator(target, state);
    }
}

fn remove_window_runtime(window_id: WindowId) {
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        if let Some(mut runtime) = runtimes.remove(&window_id) {
            runtime.quick_menu_popup.session.invalidate_owner(window_id);
            close_quick_submenu_windows(&mut runtime.quick_menu_popup);
            let root_hwnd = component_window_handle(&runtime.quick_menu_popup.root);
            let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
            let _ = runtime.quick_menu_popup.root.hide();
            platform::windows::drag_drop::revoke(native_window_handle(&runtime.ui));
        }
    });
}

fn clear_window_runtimes() {
    platform::windows::drag_drop::begin_shutdown_current();
    platform::windows::tab_insertion_indicator::destroy();
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .values()
            .map(|runtime| runtime.ui.clone_strong())
            .collect::<Vec<_>>()
    });
    for window in windows {
        window.invoke_dismiss_context_menu();
    }
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        for (window_id, runtime) in runtimes.iter_mut() {
            runtime
                .quick_menu_popup
                .session
                .invalidate_owner(*window_id);
            close_quick_submenu_windows(&mut runtime.quick_menu_popup);
            let root_hwnd = component_window_handle(&runtime.quick_menu_popup.root);
            let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
            let _ = runtime.quick_menu_popup.root.hide();
        }
        runtimes.clear();
    });
}

fn hide_all_app_windows() {
    platform::windows::drag_drop::begin_shutdown_current();
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .values()
            .map(|runtime| runtime.ui.clone_strong())
            .collect::<Vec<_>>()
    });
    for window in windows {
        let _ = window.hide();
    }
}

fn open_task_center_on_live_window(state: &SharedSessions) {
    let preferred = state.lock().ok().map(|app| app.active_window);
    let target = preferred.and_then(window_ui).or_else(|| {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .values()
                .next()
                .map(|runtime| runtime.ui.clone_strong())
        })
    });
    if let Some(ui) = target {
        ui.set_task_center_open(true);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WindowId(pub(crate) u32);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFocus {
    directory: PathBuf,
    request_id: Option<RequestId>,
    paths: Vec<PathBuf>,
}

#[derive(Debug)]
struct WindowState {
    tabs: HashMap<TabId, TabSession>,
    tab_order: Vec<TabId>,
    active_tab: TabId,
    closed_tabs: VecDeque<PathBuf>,
    placement: session_store::WindowPlacement,
}

fn movable_tab_range(window: &WindowState, source_index: usize) -> Option<std::ops::Range<usize>> {
    window.tab_order.get(source_index)?;
    let start = window.tab_order[..source_index]
        .iter()
        .rposition(|id| {
            window
                .tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        })
        .map_or(0, |index| index + 1);
    let end = window.tab_order[source_index + 1..]
        .iter()
        .position(|id| {
            window
                .tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        })
        .map_or(window.tab_order.len(), |index| source_index + 1 + index);
    Some(start..end)
}

fn tab_insertion_slot(
    pointer_x: f32,
    strip_x: f32,
    strip_width: f32,
    viewport_x: f32,
    tab_width: f32,
    range: std::ops::Range<usize>,
) -> Option<usize> {
    if pointer_x < strip_x || pointer_x > strip_x + strip_width || range.is_empty() {
        return None;
    }
    let pitch = tab_width + 5.0;
    let range_x = pointer_x - strip_x - viewport_x - range.start as f32 * pitch;
    let remaining = range.len().saturating_sub(1);
    if range_x <= tab_width / 2.0 {
        return Some(range.start);
    }
    for slot in 1..remaining {
        let midpoint = slot as f32 * pitch + tab_width / 2.0;
        if range_x < midpoint {
            return Some(range.start + slot);
        }
    }
    Some(range.start + remaining)
}

fn external_tab_insertion_slot(
    pointer_x: f32,
    strip_x: f32,
    strip_width: f32,
    viewport_x: f32,
    tab_width: f32,
    tab_count: usize,
) -> Option<usize> {
    if pointer_x < strip_x || pointer_x > strip_x + strip_width {
        return None;
    }
    if tab_count == 0 {
        return Some(0);
    }
    let pitch = tab_width + 5.0;
    let content_x = pointer_x - strip_x - viewport_x;
    for index in 0..tab_count {
        if content_x < index as f32 * pitch + tab_width / 2.0 {
            return Some(index);
        }
    }
    Some(tab_count)
}

const TAB_DRAG_THRESHOLD: f32 = 6.0;
const COLUMN_DRAG_THRESHOLD: f32 = TAB_DRAG_THRESHOLD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnDragPhase {
    Pressed,
    Dragging { insertion_slot: Option<usize> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnDragSession {
    window_id: WindowId,
    source: PageSource,
    kind: u8,
    press_x: f32,
    press_y: f32,
    phase: ColumnDragPhase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnHeaderGeometry {
    x: f32,
    y: f32,
    width: f32,
    viewport_x: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum TabDragPhase {
    Pressed,
    Dragging { insertion_index: Option<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DetachedTabRestart {
    Directory(PathBuf),
    Search {
        scope: SearchScope,
        depth: SearchDepth,
        query: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetachedTabOutcome {
    source_window: WindowId,
    destination_window: WindowId,
    tab_id: TabId,
    source_index: usize,
    source_placement: session_store::WindowPlacement,
    source_closed_tabs: VecDeque<PathBuf>,
    source_active_tab: TabId,
    source_window_closed: bool,
    restart: Option<DetachedTabRestart>,
}

fn detached_tab_restart(tab: &mut TabSession) -> Option<DetachedTabRestart> {
    match tab.page_source {
        PageSource::Search
            if matches!(
                tab.search_state,
                SearchState::Searching | SearchState::Partial
            ) =>
        {
            let restart = DetachedTabRestart::Search {
                scope: tab.search_scope.clone(),
                depth: tab.search_depth,
                query: tab.search_query.clone(),
            };
            tab.cancel_pending();
            Some(restart)
        }
        _ if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) => {
            let path = tab.visible_path().map(Path::to_path_buf);
            tab.cancel_pending();
            path.map(DetachedTabRestart::Directory)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TabDragSession {
    window_id: WindowId,
    tab_id: TabId,
    source_index: usize,
    press_x: f32,
    press_y: f32,
    phase: TabDragPhase,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCloseDecision {
    KeepRunning,
    ExitApplication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCloseAction {
    CloseWindow,
    ExitApplication,
    ConfirmApplicationExit,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnmatchedTabDropAction {
    MoveSourceWindow,
    DetachToNewWindow,
}

fn unmatched_tab_drop_action(
    source_has_single_file_tab: bool,
    target_window_hit: bool,
) -> Option<UnmatchedTabDropAction> {
    if source_has_single_file_tab {
        Some(UnmatchedTabDropAction::MoveSourceWindow)
    } else if !target_window_hit {
        Some(UnmatchedTabDropAction::DetachToNewWindow)
    } else {
        None
    }
}

#[derive(Debug)]
struct AppState {
    windows: HashMap<WindowId, WindowState>,
    active_window: WindowId,
    #[cfg_attr(not(test), allow(dead_code))]
    next_window_id: u32,
    next_tab_id: u32,
    language: Language,
    theme_mode: session_store::ThemeMode,
    file_visibility: crate::domain::FileVisibility,
    system_dark_theme: bool,
    icons: HashMap<(TabId, RequestId, EntryId), platform::windows_shell_icons::ShellIconRgba>,
    icon_cache: HashMap<PathBuf, platform::windows_shell_icons::ShellIconRgba>,
    sidebar_icons: HashMap<PathBuf, platform::windows_shell_icons::ShellIconRgba>,
    thumbnail_cache: HashMap<(PathBuf, u32), platform::windows_shell_icons::ShellIconRgba>,
    thumbnail_cache_order: VecDeque<(PathBuf, u32)>,
    large_icon_cache: HashMap<(PathBuf, u32), platform::windows_shell_icons::ShellIconRgba>,
    large_icon_cache_order: VecDeque<(PathBuf, u32)>,
    thumbnail_requests: std::collections::HashSet<(TabId, RequestId, PathBuf, u32)>,
    sidebar: Vec<KnownLocation>,
    quick_access_generation: u64,
    quick_access_pending: HashSet<PathBuf>,

    network_locations: Vec<NetworkLocation>,
    imported_network_locations: Vec<NetworkLocation>,
    network_discovery: HashMap<WindowId, DiscoveryCoordinator>,
    network_discovery_errors: HashMap<WindowId, String>,
    default_directory_view: DirectoryViewPreference,
    directory_views: HashMap<PathBuf, DirectoryViewPreference>,
    directory_view_lru: VecDeque<PathBuf>,
    search_view: SearchViewPreference,
    operations: OperationManager,
    operation_errors: Vec<String>,
    rename_targets: HashMap<WindowId, (TabId, EntryId, Option<std::ffi::OsString>)>,
    focus_after_refresh: HashMap<TabId, PendingFocus>,
    pending_permanent_delete: Option<(TabId, Vec<OperationItem>)>,
    exit_after_cancel: bool,
    clipboard_has_files: bool,
    cut_paths: Vec<PathBuf>,
    cut_generation: u64,
    conflict_responses:
        HashMap<OperationId, mpsc::Sender<crate::domain::file_operations::ConflictDecision>>,

    everything_config: crate::domain::EverythingConfig,
    everything_generation: u64,
    everything_status: String,
    everything_busy: bool,
    everything_folder_sizes_indexed: Option<bool>,
    pending_right_drops: HashMap<WindowId, (TabId, platform::windows::drag_drop::DropIntent)>,
    tab_drag: Option<TabDragSession>,
    column_drag: Option<ColumnDragSession>,
}

impl AppState {
    fn active_window_state(&self) -> &WindowState {
        self.windows
            .get(&self.active_window)
            .expect("active window state exists")
    }

    fn active_window_state_mut(&mut self) -> &mut WindowState {
        self.windows
            .get_mut(&self.active_window)
            .expect("active window state exists")
    }
    fn tab(&self, tab_id: TabId) -> Option<&TabSession> {
        self.windows
            .values()
            .find_map(|window| window.tabs.get(&tab_id))
    }

    fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut TabSession> {
        self.windows
            .values_mut()
            .find_map(|window| window.tabs.get_mut(&tab_id))
    }

    fn window_for_tab(&self, tab_id: TabId) -> Option<WindowId> {
        self.windows
            .iter()
            .find_map(|(id, window)| window.tabs.contains_key(&tab_id).then_some(*id))
    }
    fn directory_preference(&self, path: &Path) -> DirectoryViewPreference {
        self.directory_views
            .get(path)
            .copied()
            .unwrap_or(self.default_directory_view)
    }

    fn view_mode_for_tab(&self, tab_id: TabId) -> Option<ViewMode> {
        let tab = self.tab(tab_id)?;
        Some(if tab.page_source == PageSource::Search {
            self.search_view.view_mode
        } else {
            tab.visible_path()
                .map(|path| self.directory_preference(path).view_mode)
                .unwrap_or(self.default_directory_view.view_mode)
        })
    }

    fn active_view_mode(&self) -> ViewMode {
        self.view_mode_for_tab(self.active_window_state().active_tab)
            .unwrap_or(self.default_directory_view.view_mode)
    }

    fn active_column_layout(&self) -> ColumnLayout {
        if self.active().page_source == PageSource::Search {
            self.search_view.columns
        } else {
            self.active()
                .visible_path()
                .map(|path| self.directory_preference(path).columns)
                .unwrap_or(self.default_directory_view.columns)
        }
    }

    fn update_directory_preference(
        &mut self,
        path: PathBuf,
        update: impl FnOnce(&mut DirectoryViewPreference),
    ) {
        let preference = self
            .directory_views
            .entry(path.clone())
            .or_insert(self.default_directory_view);
        update(preference);
        self.directory_view_lru
            .retain(|candidate| candidate != &path);
        self.directory_view_lru.push_back(path);
        while self.directory_view_lru.len() > MAX_DIRECTORY_VIEW_PREFERENCES {
            if let Some(evicted) = self.directory_view_lru.pop_front() {
                self.directory_views.remove(&evicted);
            }
        }
    }

    fn update_active_column_layout(&mut self, update: impl FnOnce(&mut ColumnLayout)) {
        if self.active().page_source == PageSource::Search {
            update(&mut self.search_view.columns);
        } else if let Some(path) = self.active().visible_path().map(Path::to_path_buf) {
            self.update_directory_preference(path, |preference| update(&mut preference.columns));
        } else {
            update(&mut self.default_directory_view.columns);
        }
    }

    #[cfg(test)]
    fn new_for_test(
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        _column_order: [u8; 4],
    ) -> Self {
        let mut default_directory_view = DirectoryViewPreference::default();
        default_directory_view.columns.order = [
            ColumnKind::Name,
            ColumnKind::Kind,
            ColumnKind::Size,
            ColumnKind::Modified,
            ColumnKind::Created,
        ];
        Self::new(
            initial_paths,
            active_index,
            default_directory_view,
            SearchViewPreference::default(),
            HashMap::new(),
            crate::domain::EverythingConfig::default(),
            session_store::ThemeMode::System,
            Language::Chinese,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        default_directory_view: DirectoryViewPreference,
        search_view: SearchViewPreference,
        directory_views: HashMap<PathBuf, DirectoryViewPreference>,
        everything_config: crate::domain::EverythingConfig,
        theme_mode: session_store::ThemeMode,
        language: Language,
        system_dark_theme: bool,
    ) -> Self {
        let initial_paths = if initial_paths.is_empty() {
            vec![initial_path()]
        } else {
            initial_paths
        };
        let active_window = WindowId(1);
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        let mut next_tab_id = 1;
        for path in initial_paths {
            let id = TabId(next_tab_id);
            next_tab_id += 1;
            let mut tab = TabSession::new(id);
            let preference = directory_views
                .get(&path)
                .copied()
                .unwrap_or(default_directory_view);
            tab.sort_field = preference.sort_field;
            tab.sort_direction = preference.sort_direction;
            tab.search_sort_field = search_view.sort_field;
            tab.search_sort_direction = search_view.sort_direction;
            tab.current_path = Some(path);
            tabs.insert(id, tab);
            tab_order.push(id);
        }
        let active_tab = tab_order[active_index.min(tab_order.len() - 1)];
        let window = WindowState {
            tabs,
            tab_order,
            active_tab,
            closed_tabs: VecDeque::new(),
            placement: session_store::WindowPlacement {
                x: 80,
                y: 80,
                width: 1180,
                height: 760,
            },
        };
        Self {
            windows: HashMap::from([(active_window, window)]),
            active_window,
            next_window_id: 2,
            next_tab_id,
            language,
            theme_mode,
            file_visibility: crate::domain::FileVisibility::default(),
            system_dark_theme,
            icons: HashMap::new(),
            icon_cache: HashMap::new(),
            sidebar_icons: HashMap::new(),
            thumbnail_cache: HashMap::new(),
            thumbnail_cache_order: VecDeque::new(),
            large_icon_cache: HashMap::new(),
            large_icon_cache_order: VecDeque::new(),
            thumbnail_requests: std::collections::HashSet::new(),
            sidebar: Vec::new(),
            quick_access_generation: 0,
            quick_access_pending: HashSet::new(),

            network_locations: Vec::new(),
            imported_network_locations: Vec::new(),
            network_discovery: HashMap::new(),
            network_discovery_errors: HashMap::new(),
            default_directory_view,
            directory_view_lru: directory_views.keys().cloned().collect(),
            directory_views,
            search_view,
            operations: OperationManager::new(),
            operation_errors: Vec::new(),
            rename_targets: HashMap::new(),
            focus_after_refresh: HashMap::new(),
            pending_permanent_delete: None,
            exit_after_cancel: false,
            clipboard_has_files: false,
            cut_paths: Vec::new(),
            cut_generation: 0,
            conflict_responses: HashMap::new(),

            everything_config,
            everything_generation: 0,
            everything_status: String::new(),
            everything_busy: false,
            everything_folder_sizes_indexed: None,
            pending_right_drops: HashMap::new(),
            tab_drag: None,
            column_drag: None,
        }
    }

    fn allocate_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id = self
            .next_tab_id
            .checked_add(1)
            .expect("tab identity space is exhausted");
        id
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn register_window(
        &mut self,
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        placement: session_store::WindowPlacement,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window identity space is exhausted");
        let paths = if initial_paths.is_empty() {
            vec![initial_path()]
        } else {
            initial_paths
        };
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        for path in paths {
            let tab_id = self.allocate_tab_id();
            let mut tab = TabSession::new(tab_id);
            tab.current_path = Some(path);
            tabs.insert(tab_id, tab);
            tab_order.push(tab_id);
        }
        let active_tab = tab_order[active_index.min(tab_order.len() - 1)];
        self.windows.insert(
            id,
            WindowState {
                tabs,
                tab_order,
                active_tab,
                closed_tabs: VecDeque::new(),
                placement,
            },
        );
        id
    }

    fn reserve_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window identity space is exhausted");
        id
    }

    fn detach_dragged_tab_to_window(
        &mut self,
        destination_window: WindowId,
        placement: session_store::WindowPlacement,
    ) -> Option<DetachedTabOutcome> {
        let drag = self.tab_drag?;
        if !matches!(drag.phase, TabDragPhase::Dragging { .. })
            || self.windows.contains_key(&destination_window)
        {
            return None;
        }
        let source = self.windows.get(&drag.window_id)?;
        let source_placement = source.placement;
        let source_closed_tabs = source.closed_tabs.clone();
        let source_active_tab = source.active_tab;
        if source.tab_order.get(drag.source_index) != Some(&drag.tab_id)
            || source
                .tabs
                .get(&drag.tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return None;
        }

        let mut tab = self
            .windows
            .get_mut(&drag.window_id)?
            .tabs
            .remove(&drag.tab_id)?;
        self.windows
            .get_mut(&drag.window_id)?
            .tab_order
            .remove(drag.source_index);
        let restart = detached_tab_restart(&mut tab);
        self.windows.insert(
            destination_window,
            WindowState {
                tabs: HashMap::from([(drag.tab_id, tab)]),
                tab_order: vec![drag.tab_id],
                active_tab: drag.tab_id,
                closed_tabs: VecDeque::new(),
                placement,
            },
        );

        let source_has_tabs = self
            .windows
            .get(&drag.window_id)
            .is_some_and(|window| !window.tab_order.is_empty());
        let source_window_closed = if source_has_tabs {
            let source = self.windows.get_mut(&drag.window_id)?;
            if source.active_tab == drag.tab_id {
                source.active_tab = *source.tab_order.first()?;
            }
            false
        } else {
            self.windows.remove(&drag.window_id);
            true
        };
        self.active_window = destination_window;
        self.tab_drag = None;
        Some(DetachedTabOutcome {
            source_window: drag.window_id,
            destination_window,
            tab_id: drag.tab_id,
            source_index: drag.source_index,
            source_placement,
            source_closed_tabs,
            source_active_tab,
            source_window_closed,
            restart,
        })
    }

    fn move_dragged_tab_to_window(
        &mut self,
        destination_window: WindowId,
        insertion_index: usize,
    ) -> Option<DetachedTabOutcome> {
        let drag = self.tab_drag?;
        if drag.window_id == destination_window || !self.windows.contains_key(&destination_window) {
            return None;
        }
        let source = self.windows.get(&drag.window_id)?;
        if source.tab_order.get(drag.source_index) != Some(&drag.tab_id)
            || source
                .tabs
                .get(&drag.tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return None;
        }
        let destination = self.windows.get(&destination_window)?;
        let movable_end = destination
            .tab_order
            .iter()
            .position(|id| {
                destination
                    .tabs
                    .get(id)
                    .is_some_and(|tab| tab.kind == TabKind::Settings)
            })
            .unwrap_or(destination.tab_order.len());
        let insertion_index = insertion_index.min(movable_end);

        let source_placement = source.placement;
        let source_closed_tabs = source.closed_tabs.clone();
        let source_active_tab = source.active_tab;
        let mut tab = self
            .windows
            .get_mut(&drag.window_id)?
            .tabs
            .remove(&drag.tab_id)?;
        self.windows
            .get_mut(&drag.window_id)?
            .tab_order
            .remove(drag.source_index);
        let restart = detached_tab_restart(&mut tab);
        let destination = self.windows.get_mut(&destination_window)?;
        destination.tabs.insert(drag.tab_id, tab);
        destination.tab_order.insert(insertion_index, drag.tab_id);
        destination.active_tab = drag.tab_id;

        let source_has_tabs = self
            .windows
            .get(&drag.window_id)
            .is_some_and(|window| !window.tab_order.is_empty());
        let source_window_closed = if source_has_tabs {
            let source = self.windows.get_mut(&drag.window_id)?;
            if source.active_tab == drag.tab_id {
                source.active_tab = *source.tab_order.first()?;
            }
            false
        } else {
            self.windows.remove(&drag.window_id);
            true
        };
        self.active_window = destination_window;
        self.tab_drag = None;
        Some(DetachedTabOutcome {
            source_window: drag.window_id,
            destination_window,
            tab_id: drag.tab_id,
            source_index: drag.source_index,
            source_placement,
            source_closed_tabs,
            source_active_tab,
            source_window_closed,
            restart,
        })
    }

    fn window(&self, id: WindowId) -> Option<&WindowState> {
        self.windows.get(&id)
    }

    #[cfg(test)]
    fn window_mut(&mut self, id: WindowId) -> Option<&mut WindowState> {
        self.windows.get_mut(&id)
    }

    fn close_window(&mut self, id: WindowId) -> Option<WindowCloseDecision> {
        let mut window = self.windows.remove(&id)?;
        for tab in window.tabs.values_mut() {
            cancel_folder_sizes(tab);
            tab.cancel_pending();
        }
        if let Some(mut discovery) = self.network_discovery.remove(&id) {
            discovery.cancel_current();
        }
        self.network_discovery_errors.remove(&id);
        self.icons
            .retain(|(tab_id, _, _), _| !window.tabs.contains_key(tab_id));
        self.focus_after_refresh
            .retain(|tab_id, _| !window.tabs.contains_key(tab_id));
        self.rename_targets.remove(&id);
        if self.windows.is_empty() {
            return Some(WindowCloseDecision::ExitApplication);
        }
        if self.active_window == id {
            self.active_window = *self.windows.keys().min_by_key(|id| id.0)?;
        }
        Some(WindowCloseDecision::KeepRunning)
    }

    fn request_window_close(&self, id: WindowId) -> WindowCloseAction {
        if !self.windows.contains_key(&id) {
            return WindowCloseAction::Ignore;
        }
        if self.windows.len() > 1 {
            WindowCloseAction::CloseWindow
        } else if self.operations.has_active_tasks() {
            WindowCloseAction::ConfirmApplicationExit
        } else {
            WindowCloseAction::ExitApplication
        }
    }

    fn begin_tab_drag(
        &mut self,
        window_id: WindowId,
        tab_id: TabId,
        source_index: usize,
        press_x: f32,
        press_y: f32,
    ) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if window.tab_order.get(source_index) != Some(&tab_id)
            || window
                .tabs
                .get(&tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return false;
        }
        self.tab_drag = Some(TabDragSession {
            window_id,
            tab_id,
            source_index,
            press_x,
            press_y,
            phase: TabDragPhase::Pressed,
        });
        true
    }

    fn update_tab_drag(
        &mut self,
        pointer_x: f32,
        pointer_y: f32,
        strip_x: f32,
        strip_width: f32,
        viewport_x: f32,
        tab_width: f32,
    ) -> Option<usize> {
        let mut drag = self.tab_drag?;
        if matches!(drag.phase, TabDragPhase::Pressed)
            && (pointer_x - drag.press_x).hypot(pointer_y - drag.press_y) < TAB_DRAG_THRESHOLD
        {
            return None;
        }
        let window = self.windows.get(&drag.window_id)?;
        let range = movable_tab_range(window, drag.source_index)?;
        let insertion_index = tab_insertion_slot(
            pointer_x,
            strip_x,
            strip_width,
            viewport_x,
            tab_width,
            range,
        );
        drag.phase = TabDragPhase::Dragging { insertion_index };
        self.tab_drag = Some(drag);
        ((0.0..=46.0).contains(&pointer_y))
            .then_some(insertion_index)
            .flatten()
    }

    fn finish_tab_drag(&mut self, valid_release: bool) -> bool {
        let Some(drag) = self.tab_drag.take() else {
            return false;
        };
        let TabDragPhase::Dragging {
            insertion_index: Some(insertion_index),
        } = drag.phase
        else {
            return false;
        };
        if !valid_release {
            return false;
        }
        let Some(window) = self.windows.get_mut(&drag.window_id) else {
            return false;
        };
        if window.tab_order.get(drag.source_index) != Some(&drag.tab_id) {
            return false;
        }
        let moved = window.tab_order.remove(drag.source_index);
        let target = insertion_index.min(window.tab_order.len());
        window.tab_order.insert(target, moved);
        target != drag.source_index
    }

    fn drag_source_is_single_file_tab_window(&self, source_window: WindowId) -> bool {
        let Some(drag) = self.tab_drag.filter(|drag| drag.window_id == source_window) else {
            return false;
        };
        self.windows.get(&source_window).is_some_and(|window| {
            window.tab_order.len() == 1
                && window.tab_order[0] == drag.tab_id
                && window
                    .tabs
                    .get(&drag.tab_id)
                    .is_some_and(|tab| tab.kind == TabKind::Files)
        })
    }

    fn commit_drag_source_window_move(&mut self, source_window: WindowId, x: i32, y: i32) -> bool {
        if !self.drag_source_is_single_file_tab_window(source_window)
            || !self.tab_drag.is_some_and(|drag| {
                drag.window_id == source_window
                    && matches!(drag.phase, TabDragPhase::Dragging { .. })
            })
        {
            return false;
        }
        let Some(window) = self.windows.get_mut(&source_window) else {
            return false;
        };
        window.placement.x = x;
        window.placement.y = y;
        self.tab_drag = None;
        true
    }

    fn cancel_tab_drag(&mut self) -> bool {
        self.tab_drag.take().is_some()
    }

    fn begin_column_drag(
        &mut self,
        window_id: WindowId,
        kind: u8,
        press_x: f32,
        press_y: f32,
    ) -> bool {
        if kind >= ColumnKind::COUNT as u8 || !press_x.is_finite() || !press_y.is_finite() {
            return false;
        }
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        let source = window.active().page_source;
        let order = if source == PageSource::Search {
            self.search_view.columns.order
        } else {
            let path = window.active().visible_path().map(Path::to_path_buf);
            path.as_deref()
                .map(|path| self.directory_preference(path).columns.order)
                .unwrap_or(self.default_directory_view.columns.order)
        };
        let Some(column_kind) = ColumnKind::from_storage_code(kind) else {
            return false;
        };
        if !order.contains(&column_kind) {
            return false;
        }
        self.column_drag = Some(ColumnDragSession {
            window_id,
            source,
            kind,
            press_x,
            press_y,
            phase: ColumnDragPhase::Pressed,
        });
        true
    }

    fn update_column_drag(
        &mut self,
        pointer_x: f32,
        pointer_y: f32,
        header_x: f32,
        header_y: f32,
        header_width: f32,
        viewport_x: f32,
    ) -> Option<usize> {
        let mut drag = self.column_drag?;
        if !pointer_x.is_finite() || !pointer_y.is_finite() {
            return None;
        }
        if matches!(drag.phase, ColumnDragPhase::Pressed)
            && (pointer_x - drag.press_x).hypot(pointer_y - drag.press_y) < COLUMN_DRAG_THRESHOLD
        {
            return None;
        }
        let current_source = self.windows.get(&drag.window_id)?.active().page_source;
        if current_source != drag.source {
            self.column_drag = None;
            return None;
        }
        let layout = if drag.source == PageSource::Search {
            self.search_view.columns
        } else {
            let path = self.windows.get(&drag.window_id)?.active().visible_path()?;
            self.directory_preference(path).columns
        };
        let order = layout.order;
        let widths = layout.widths;
        let insertion_slot = column_insertion_slot(
            pointer_x,
            pointer_y,
            ColumnHeaderGeometry {
                x: header_x,
                y: header_y,
                width: header_width,
                viewport_x,
            },
            &order,
            &widths,
        );
        drag.phase = ColumnDragPhase::Dragging { insertion_slot };
        self.column_drag = Some(drag);
        insertion_slot
    }

    fn finish_column_drag(&mut self, valid_release: bool) -> bool {
        let Some(drag) = self.column_drag.take() else {
            return false;
        };
        let ColumnDragPhase::Dragging {
            insertion_slot: Some(insertion_slot),
        } = drag.phase
        else {
            return false;
        };
        if !valid_release
            || self
                .windows
                .get(&drag.window_id)
                .map(WindowState::active)
                .map(|tab| tab.page_source)
                != Some(drag.source)
        {
            return false;
        }
        self.commit_column_reorder(drag.source, drag.kind, insertion_slot)
    }

    fn commit_column_reorder(
        &mut self,
        source: PageSource,
        kind: u8,
        insertion_slot: usize,
    ) -> bool {
        let Some(kind) = ColumnKind::from_storage_code(kind) else {
            return false;
        };
        let mut changed = false;
        let mut reorder = |layout: &mut ColumnLayout| {
            let Some(source_index) = layout.order.iter().position(|candidate| *candidate == kind)
            else {
                return;
            };
            let target = normalized_column_slot(source_index, insertion_slot, layout.order.len());
            changed = reorder_column_to_slot(&mut layout.order, kind, target);
        };
        if source == PageSource::Search {
            reorder(&mut self.search_view.columns);
        } else if let Some(path) = self.active().visible_path().map(Path::to_path_buf) {
            self.update_directory_preference(path, |preference| reorder(&mut preference.columns));
        }
        changed
    }

    fn cancel_column_drag(&mut self) -> bool {
        self.column_drag.take().is_some()
    }

    fn cancel_tab_drag_for_window(&mut self, window_id: WindowId) -> bool {
        if self
            .tab_drag
            .is_some_and(|drag| drag.window_id == window_id)
        {
            self.cancel_tab_drag()
        } else {
            false
        }
    }

    fn duplicate_active_tab(&mut self) -> Option<TabId> {
        let source_id = self.active_window_state().active_tab;
        let id = self.allocate_tab_id();
        let tab = {
            let source = self.active_window_state().tabs.get(&source_id)?;
            if source.kind != TabKind::Files || source.load_state != LoadState::Complete {
                return None;
            }
            TabSession::duplicate_complete(id, source)
        };
        let window = self.active_window_state_mut();
        window.tabs.insert(id, tab);
        window.tab_order.push(id);
        window.active_tab = id;
        Some(id)
    }
    fn create_tab(&mut self, path: PathBuf) -> TabId {
        let id = self.allocate_tab_id();
        let mut tab = TabSession::new(id);
        tab.current_path = Some(path);
        let window = self.active_window_state_mut();
        window.tabs.insert(id, tab);
        window.tab_order.push(id);
        window.active_tab = id;
        id
    }

    fn open_settings(&mut self) -> TabId {
        if let Some(id) = self
            .active_window_state()
            .tab_order
            .iter()
            .copied()
            .find(|id| {
                self.active_window_state()
                    .tabs
                    .get(id)
                    .is_some_and(|tab| tab.kind == TabKind::Settings)
            })
        {
            self.active_window_state_mut().active_tab = id;
            return id;
        }
        let id = self.allocate_tab_id();
        let window = self.active_window_state_mut();
        window.tabs.insert(id, TabSession::new_settings(id));
        window.tab_order.push(id);
        window.active_tab = id;
        id
    }

    fn close_tab(&mut self, closing: TabId) -> Option<TabId> {
        if self.active_window_state().tab_order.len() == 1 {
            return None;
        }
        let closing_kind = self.active_window_state().tabs.get(&closing)?.kind;
        if closing_kind == TabKind::Files
            && self
                .active_window_state()
                .tabs
                .values()
                .filter(|tab| tab.kind == TabKind::Files)
                .count()
                == 1
        {
            return None;
        }
        let index = self
            .active_window_state()
            .tab_order
            .iter()
            .position(|id| *id == closing)?;
        let closing_was_active = closing == self.active_window_state().active_tab;
        let removed = self.active_window_state_mut().tabs.remove(&closing);
        if let Some(mut tab) = removed {
            cancel_folder_sizes(&mut tab);
            tab.cancel_pending();
            self.icons.retain(|(tab_id, _, _), _| *tab_id != closing);
            if tab.kind == TabKind::Files
                && let Some(path) = tab.current_path.take()
            {
                let window = self.active_window_state_mut();
                window.closed_tabs.push_front(path);
                window.closed_tabs.truncate(10);
            }
        }
        self.active_window_state_mut().tab_order.remove(index);
        if closing_was_active {
            let window = self.active_window_state_mut();
            window.active_tab = window.tab_order[index.min(window.tab_order.len() - 1)];
        }
        Some(self.active_window_state().active_tab)
    }

    fn restore_closed(&mut self) -> Option<(TabId, PathBuf)> {
        let path = self.active_window_state_mut().closed_tabs.pop_front()?;
        let tab_id = self.create_tab(path.clone());
        Some((tab_id, path))
    }

    fn active(&self) -> &TabSession {
        self.active_window_state().active()
    }

    fn stable_paths(&self) -> Vec<PathBuf> {
        self.active_window_state().stable_paths()
    }

    #[cfg(test)]
    fn stable_active_path_index(&self) -> usize {
        self.active_window_state().stable_active_path_index()
    }

    fn dark_theme(&self) -> bool {
        match self.theme_mode {
            session_store::ThemeMode::System => self.system_dark_theme,
            session_store::ThemeMode::Light => false,
            session_store::ThemeMode::Dark => true,
        }
    }
}

impl WindowState {
    fn active(&self) -> &TabSession {
        self.tabs
            .get(&self.active_tab)
            .expect("active tab session exists")
    }

    fn stable_paths(&self) -> Vec<PathBuf> {
        self.tab_order
            .iter()
            .filter_map(|id| self.tabs.get(id))
            .filter_map(|tab| tab.current_path.clone())
            .collect()
    }

    fn stable_active_path_index(&self) -> usize {
        let mut file_index = 0;
        for id in &self.tab_order {
            let Some(tab) = self.tabs.get(id) else {
                continue;
            };
            if tab.kind != TabKind::Files {
                continue;
            }
            if *id == self.active_tab {
                return file_index;
            }
            file_index += 1;
        }
        file_index.saturating_sub(1)
    }
}

fn reorder_column_to_slot(
    order: &mut [ColumnKind; 5],
    kind: ColumnKind,
    insertion_slot: usize,
) -> bool {
    let Some(from) = order.iter().position(|candidate| *candidate == kind) else {
        return false;
    };
    let target = insertion_slot.min(order.len() - 1);
    if from == target {
        return false;
    }
    let moved = order[from];
    if from < target {
        order.copy_within(from + 1..=target, from);
    } else {
        order.copy_within(target..from, target + 1);
    }
    order[target] = moved;
    true
}

fn column_insertion_slot(
    pointer_x: f32,
    pointer_y: f32,
    geometry: ColumnHeaderGeometry,
    order: &[ColumnKind; 5],
    widths: &[u32; 5],
) -> Option<usize> {
    if !pointer_x.is_finite()
        || !geometry.x.is_finite()
        || !geometry.width.is_finite()
        || !geometry.viewport_x.is_finite()
        || pointer_x < geometry.x
        || pointer_x > geometry.x + geometry.width
        || !(geometry.y..=geometry.y + 38.0).contains(&pointer_y)
    {
        return None;
    }
    let content_x = pointer_x - geometry.x - geometry.viewport_x;
    let mut left = 0.0;
    for (index, kind) in order.iter().enumerate() {
        let width = widths.get(kind.storage_code() as usize).copied()? as f32;
        if content_x < left + width / 2.0 {
            return Some(index);
        }
        left += width;
    }
    Some(order.len())
}

fn normalized_column_slot(source_index: usize, insertion_slot: usize, len: usize) -> usize {
    let slot = insertion_slot.min(len);
    if slot > source_index { slot - 1 } else { slot }
}
#[derive(Debug)]
struct DirectoryRequest {
    tab_id: TabId,
    request_id: RequestId,
    path: PathBuf,
    visibility: crate::domain::FileVisibility,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

struct NetworkDirectoryCompletion {
    key: NetworkExecutionKey,
}

#[derive(Default)]
struct NetworkDirectoryScheduler {
    pending: VecDeque<(NetworkExecutionKey, DirectoryRequest)>,
    active: HashSet<NetworkExecutionKey>,
}

impl NetworkDirectoryScheduler {
    fn push(&mut self, key: NetworkExecutionKey, request: DirectoryRequest) {
        self.pending.push_back((key, request));
    }

    fn complete(&mut self, key: &NetworkExecutionKey) {
        self.active.remove(key);
    }

    fn next_ready(&mut self) -> Option<(NetworkExecutionKey, DirectoryRequest)> {
        let index = self
            .pending
            .iter()
            .position(|(key, request)| !self.active.contains(key) && !request.cancelled())?;
        let (key, request) = self.pending.remove(index)?;
        self.active.insert(key.clone());
        Some((key, request))
    }

    fn take_cancelled(&mut self) -> Vec<DirectoryRequest> {
        let mut kept = VecDeque::with_capacity(self.pending.len());
        let mut cancelled = Vec::new();
        while let Some((key, request)) = self.pending.pop_front() {
            if request.cancelled() {
                cancelled.push(request);
            } else {
                kept.push_back((key, request));
            }
        }
        self.pending = kept;
        cancelled
    }
}

impl DirectoryRequest {
    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
fn network_directory_request(path: &str) -> DirectoryRequest {
    DirectoryRequest {
        tab_id: TabId(1),
        request_id: RequestId(1),
        path: PathBuf::from(path),
        visibility: crate::domain::FileVisibility::default(),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

#[derive(Debug)]
enum DirectoryEvent {
    Batch {
        tab_id: TabId,
        request_id: RequestId,
        entries: Vec<FileEntry>,
    },
    Finished {
        tab_id: TabId,
        request_id: RequestId,
        path: PathBuf,
        skipped: usize,
    },
    Cancelled {
        tab_id: TabId,
        request_id: RequestId,
    },
    Failed {
        tab_id: TabId,
        request_id: RequestId,
        kind: io::ErrorKind,
        message: String,
    },
    Slow {
        tab_id: TabId,
        request_id: RequestId,
    },
}

impl DirectoryEvent {
    fn request_identity(&self) -> (TabId, RequestId) {
        match self {
            Self::Batch {
                tab_id, request_id, ..
            }
            | Self::Finished {
                tab_id, request_id, ..
            }
            | Self::Cancelled { tab_id, request_id }
            | Self::Failed {
                tab_id, request_id, ..
            }
            | Self::Slow { tab_id, request_id } => (*tab_id, *request_id),
        }
    }
}
#[derive(Debug)]
enum NetworkDiscoveryRequest {
    Discover {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
}

#[derive(Debug)]
enum NetworkDiscoveryEvent {
    Batch {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
        devices: Vec<NetworkDeviceTarget>,
    },
    Finished {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
    },
    Failed {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
        error: crate::network::NetworkErrorKind,
    },
}

const SEARCH_PAGE_LIMIT: u32 = 256;
const SEARCH_WINDOW_PAGE_COUNT: u32 = 3;
const SEARCH_WINDOW_ITEM_LIMIT: usize = (SEARCH_PAGE_LIMIT * SEARCH_WINDOW_PAGE_COUNT) as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchWindow {
    start: u32,
    len: usize,
}

fn view_mode_from_ui(mode: i32) -> ViewMode {
    ViewMode::from_storage_code(mode.clamp(0, u8::MAX as i32) as u8).unwrap_or(ViewMode::Details)
}

fn view_mode_to_ui(mode: ViewMode) -> i32 {
    i32::from(mode.storage_code())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FileLayoutGeometry {
    row_height: f32,
    card_width: f32,
    card_height: f32,
    icon_request_px: u32,
    grid: bool,
}

fn file_layout_geometry(view_mode: ViewMode) -> FileLayoutGeometry {
    match view_mode {
        ViewMode::Details => FileLayoutGeometry {
            row_height: 40.0,
            card_width: 0.0,
            card_height: 40.0,
            icon_request_px: 32,
            grid: false,
        },
        ViewMode::List => FileLayoutGeometry {
            row_height: 34.0,
            card_width: 0.0,
            card_height: 34.0,
            icon_request_px: 32,
            grid: false,
        },
        ViewMode::SmallIcons => FileLayoutGeometry {
            row_height: 86.0,
            card_width: 88.0,
            card_height: 78.0,
            icon_request_px: 32,
            grid: true,
        },
        ViewMode::MediumIcons => FileLayoutGeometry {
            row_height: 148.0,
            card_width: 140.0,
            card_height: 140.0,
            icon_request_px: 100,
            grid: true,
        },
        ViewMode::LargeIcons => FileLayoutGeometry {
            row_height: 196.0,
            card_width: 188.0,
            card_height: 188.0,
            icon_request_px: 148,
            grid: true,
        },
        ViewMode::ExtraLargeIcons => FileLayoutGeometry {
            row_height: 220.0,
            card_width: 196.0,
            card_height: 212.0,
            icon_request_px: 168,
            grid: true,
        },
        ViewMode::Tiles => FileLayoutGeometry {
            row_height: 86.0,
            card_width: 292.0,
            card_height: 78.0,
            icon_request_px: 48,
            grid: true,
        },
        ViewMode::Content => FileLayoutGeometry {
            row_height: 76.0,
            card_width: 0.0,
            card_height: 76.0,
            icon_request_px: 48,
            grid: false,
        },
    }
}

fn file_row_height(view_mode: ViewMode) -> f32 {
    file_layout_geometry(view_mode).row_height
}

fn projected_scroll_maximum(ui: &AppWindow, view_mode: ViewMode, visible_height: f32) -> f32 {
    if view_mode.uses_grid_layout() {
        let extent = ui
            .get_grid_rows()
            .iter()
            .map(|row| {
                if row.group_header {
                    32.0
                } else {
                    file_row_height(view_mode)
                }
            })
            .sum::<f32>();
        (extent - visible_height).max(0.0)
    } else {
        let extent = ui
            .get_files()
            .iter()
            .map(|row| {
                if row.group_header {
                    32.0
                } else {
                    file_row_height(view_mode)
                }
            })
            .sum::<f32>();
        (extent - visible_height).max(0.0)
    }
}
fn file_scroll_maximum(
    item_count: usize,
    view_mode: ViewMode,
    grid_columns: usize,
    visible_height: f32,
) -> f32 {
    let rows = if file_layout_geometry(view_mode).grid {
        item_count.div_ceil(grid_columns.max(1))
    } else {
        item_count
    };
    (rows as f32 * file_row_height(view_mode) - visible_height).max(0.0)
}

const CTRL_WHEEL_PIXEL_THRESHOLD: f32 = 80.0;

fn ctrl_wheel_step(
    delta: &MouseScrollDelta,
    scale_factor: f32,
    accumulator: &mut f32,
) -> Option<bool> {
    match delta {
        MouseScrollDelta::LineDelta(_, y) if *y != 0.0 => {
            *accumulator = 0.0;
            Some(*y > 0.0)
        }
        MouseScrollDelta::PixelDelta(position) => {
            let value = position.y as f32 / scale_factor.max(f32::EPSILON);
            if value == 0.0 {
                return None;
            }
            if accumulator.signum() != value.signum() {
                *accumulator = 0.0;
            }
            *accumulator += value;
            if accumulator.abs() >= CTRL_WHEEL_PIXEL_THRESHOLD {
                let toward_larger = *accumulator > 0.0;
                *accumulator -= CTRL_WHEEL_PIXEL_THRESHOLD.copysign(*accumulator);
                Some(toward_larger)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn anchored_viewport(
    old_viewport: f32,
    pointer_y: f32,
    old_mode: ViewMode,
    new_mode: ViewMode,
    columns: usize,
    item_count: usize,
    visible_height: f32,
) -> f32 {
    if item_count == 0 {
        return 0.0;
    }
    let old = file_layout_geometry(old_mode);
    let old_row = ((-old_viewport + pointer_y).max(0.0) / old.row_height).floor() as usize;
    let anchor_index = if old.grid {
        old_row.saturating_mul(columns.max(1))
    } else {
        old_row
    }
    .min(item_count - 1);
    let new = file_layout_geometry(new_mode);
    let new_row = if new.grid {
        anchor_index / columns.max(1)
    } else {
        anchor_index
    };
    let relative = (-old_viewport + pointer_y) - old_row as f32 * old.row_height;
    let candidate = -(new_row as f32 * new.row_height + relative - pointer_y);
    let maximum = file_scroll_maximum(item_count, new_mode, columns, visible_height);
    candidate.clamp(-maximum, 0.0)
}
fn logical_scroll_delta(delta: &MouseScrollDelta, view_mode: ViewMode, scale_factor: f32) -> f32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => *y * file_row_height(view_mode) * 3.0,
        MouseScrollDelta::PixelDelta(position) => {
            position.y as f32 / scale_factor.max(f32::EPSILON)
        }
    }
}

fn pointer_targets_file_area(
    pointer_x: f32,
    pointer_y: f32,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
) -> bool {
    pointer_x >= left && pointer_x <= left + width && pointer_y >= top && pointer_y <= top + height
}
fn search_logical_maximum(
    total: u32,
    view_mode: ViewMode,
    columns: usize,
    visible_height: f32,
) -> f32 {
    file_scroll_maximum(total as usize, view_mode, columns, visible_height)
}

fn search_result_index_at_scroll(
    scroll_y: f32,
    total: u32,
    view_mode: ViewMode,
    columns: usize,
) -> u32 {
    if total == 0 {
        return 0;
    }
    let row = ((-scroll_y).max(0.0) / file_row_height(view_mode)).floor() as usize;
    let index = if file_layout_geometry(view_mode).grid {
        row.saturating_mul(columns.max(1))
    } else {
        row
    };
    index.min(total.saturating_sub(1) as usize) as u32
}

fn search_window_for_index(index: u32, total: u32, columns: usize) -> SearchWindow {
    if total == 0 {
        return SearchWindow { start: 0, len: 0 };
    }
    let page = index / SEARCH_PAGE_LIMIT * SEARCH_PAGE_LIMIT;
    let start = page.saturating_sub(SEARCH_PAGE_LIMIT);
    let columns = columns.max(1) as u32;
    let start = start / columns * columns;
    SearchWindow {
        start,
        len: total
            .saturating_sub(start)
            .min(SEARCH_WINDOW_ITEM_LIMIT as u32) as usize,
    }
}
fn search_window_for_scroll(
    scroll_y: f32,
    total: u32,
    view_mode: ViewMode,
    columns: usize,
) -> SearchWindow {
    search_window_for_index(
        search_result_index_at_scroll(scroll_y, total, view_mode, columns),
        total,
        columns,
    )
}

fn search_window_viewport_y(
    index: u32,
    window: SearchWindow,
    view_mode: ViewMode,
    columns: usize,
) -> f32 {
    let local = index.saturating_sub(window.start) as usize;
    let row = if file_layout_geometry(view_mode).grid {
        local / columns.max(1)
    } else {
        local
    };
    -(row as f32 * file_row_height(view_mode))
}
fn search_scroll_for_index(index: u32, view_mode: ViewMode, columns: usize) -> f32 {
    let row = if file_layout_geometry(view_mode).grid {
        index as usize / columns.max(1)
    } else {
        index as usize
    };
    -(row as f32 * file_row_height(view_mode))
}

fn search_window_rows(
    tab: &TabSession,
    app: &AppState,
    window: SearchWindow,
    grid_requested_px: Option<u32>,
) -> Vec<FileRow> {
    let texts = Texts::new(app.language);
    (0..window.len)
        .map(|local| {
            let result_index = window.start.saturating_add(local as u32);
            tab.visible_entry(EntryId(result_index.saturating_add(1)))
                .map(|entry| file_row(entry, tab, texts, app, grid_requested_px))
                .unwrap_or_else(empty_file_row)
        })
        .collect()
}

#[derive(Debug)]
enum EverythingRequest {
    Search {
        tab_id: TabId,
        request_id: RequestId,
        scope: SearchScope,
        depth: SearchDepth,
        query: String,
        sort: platform::windows::everything::EverythingSort,
        offset: u32,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
    FolderSize {
        tab_id: TabId,
        query: FolderSizeQuery,
    },
    Configure(crate::domain::EverythingConfig),
    Discover(u64),
    TestConnection(u64),
    Start(u64),
    PickExecutable {
        generation: u64,
        owner_window: isize,
    },
}

#[derive(Debug)]
enum EverythingEvent {
    SearchPage {
        tab_id: TabId,
        request_id: RequestId,
        offset: u32,
        entries: Vec<FileEntry>,
        total: u32,
        file_total: u32,
        response_valid: bool,
    },
    SearchFailed {
        tab_id: TabId,
        request_id: RequestId,
        offset: u32,
        error: platform::windows::everything::EverythingError,
    },
    SearchSkipped {
        tab_id: TabId,
        request_id: RequestId,
        offset: u32,
    },
    FolderSize {
        tab_id: TabId,
        query: FolderSizeQuery,
        state: FolderSizeState,
    },
    Status {
        generation: u64,
        result: Result<
            platform::windows::everything::EverythingStatus,
            platform::windows::everything::EverythingError,
        >,
    },
    Discovered {
        generation: u64,
        config: crate::domain::EverythingConfig,
        status: platform::windows::everything::EverythingStatus,
    },
    ExecutablePicked {
        generation: u64,
        result: std::io::Result<Option<PathBuf>>,
    },
}

enum FolderSizeWork {
    Query {
        tab_id: TabId,
        query: FolderSizeQuery,
    },
    Configure(crate::domain::EverythingConfig),
}
struct IconRequest {
    tab_id: TabId,
    request_id: RequestId,
    target: IconTarget,
    path: PathBuf,
    thumbnail: bool,
    requested_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconTarget {
    Entry(EntryId),
    Location,
}

#[derive(Debug)]
struct FileOperationRequest {
    id: OperationId,
    kind: FileOperationKind,
    resource: OperationResource,
    items: Vec<OperationItem>,
    cancellation: crate::domain::file_operations::CancellationToken,
}

#[derive(Debug)]
#[allow(dead_code)]
enum FileOperationEvent {
    DestinationCreated {
        id: OperationId,
        path: PathBuf,
    },
    Progress {
        id: OperationId,
        completed_items: usize,
        completed_files: usize,
        total_files: Option<usize>,
        processed_bytes: u64,
        total_bytes: Option<u64>,
        current_item: PathBuf,
        started: Instant,
    },
    Conflict {
        id: OperationId,
        conflict: crate::domain::file_operations::OperationConflict,
        response: mpsc::Sender<crate::domain::file_operations::ConflictDecision>,
    },
    Finished {
        id: OperationId,
        result: OperationResult,
        item_states: Vec<(usize, ItemState, Option<String>)>,
        completed_targets: Vec<PathBuf>,
    },
}

#[derive(Debug)]
enum ClipboardRequest {
    Write { paths: Vec<PathBuf>, cut: bool },
    ReadPaste { origin_tab: TabId, target: PathBuf },
    CheckAvailability,
}
#[derive(Debug)]
enum ClipboardEvent {
    Written {
        result: Result<(), String>,
        paths: Vec<PathBuf>,
        cut: bool,
    },
    Paste {
        origin_tab: TabId,
        result: Result<Option<(FileOperationKind, Vec<OperationItem>)>, String>,
    },
    Availability(Result<bool, String>),
}

#[derive(Clone)]
struct NetworkLoginSession {
    generation: u64,
    window_id: WindowId,
    tab_id: TabId,
    failed_request_id: RequestId,
    target: PathBuf,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
struct NetworkLoginCoordinator {
    next_generation: u64,
    current: Option<NetworkLoginSession>,
}

impl NetworkLoginCoordinator {
    fn begin(
        &mut self,
        window_id: WindowId,
        tab_id: TabId,
        failed_request_id: RequestId,
        target: PathBuf,
    ) -> NetworkLoginSession {
        self.cancel();
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let session = NetworkLoginSession {
            generation: self.next_generation,
            window_id,
            tab_id,
            failed_request_id,
            target,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        self.current = Some(session.clone());
        session
    }

    fn cancel(&mut self) {
        if let Some(current) = self.current.take() {
            current
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| current.generation == generation)
    }

    fn finish(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.current = None;
        }
    }

    fn cancel_generation(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.cancel();
        }
    }
}

struct IconEvent {
    tab_id: TabId,
    request_id: RequestId,
    target: IconTarget,
    path: PathBuf,
    icon: platform::windows_shell_icons::ShellIconRgba,
    actual_thumbnail: bool,
    requested_px: u32,
}

pub fn run(scenario: Option<AgentScenario>) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let network_login_ui = NetworkLoginWindow::new()?;
    let network_location_rename_ui = NetworkLocationRenameWindow::new()?;
    let operation_ui = OperationWindow::new()?;
    let delete_ui = ConfirmationWindow::new()?;
    let conflict_ui = ConfirmationWindow::new()?;
    let exit_ui = ConfirmationWindow::new()?;
    let delete_weak = delete_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    let restored = scenario
        .is_none()
        .then(|| session_store::default_path().and_then(|path| session_store::load(&path).ok()))
        .flatten();
    let default_window = session_store::WindowPlacement {
        x: 80,
        y: 80,
        width: 1180,
        height: 760,
    };
    let (
        restored_paths,
        active_index,
        window,
        additional_windows,
        default_directory_view,
        search_view,
        directory_views,
        everything_config,
        theme_mode,
        language,
        file_visibility,
        network_locations,
        network_devices,
    ) = restored
        .filter(|session| {
            session
                .windows
                .first()
                .is_some_and(|window| !window.tab_paths.is_empty())
        })
        .map(|session| {
            let mut windows = session.windows;
            let first = windows.remove(0);
            (
                first.tab_paths,
                first.active_tab,
                first.placement,
                windows,
                session.default_directory_view,
                session.search_view,
                session
                    .directory_views
                    .into_iter()
                    .collect::<HashMap<_, _>>(),
                session.everything,
                session.theme_mode,
                session.language,
                session.file_visibility,
                session.network_locations,
                session.network_devices,
            )
        })
        .unwrap_or_else(|| {
            (
                vec![initial_path()],
                0,
                default_window,
                Vec::new(),
                DirectoryViewPreference::default(),
                SearchViewPreference::default(),
                HashMap::new(),
                crate::domain::EverythingConfig::default(),
                session_store::ThemeMode::System,
                Language::Chinese,
                crate::domain::FileVisibility::default(),
                Vec::new(),
                Vec::new(),
            )
        });
    ui.window()
        .set_position(slint::PhysicalPosition::new(window.x, window.y));
    ui.window().set_size(slint::LogicalSize::new(
        window.width as f32,
        window.height as f32,
    ));
    let restored_paths = restored_paths
        .into_iter()
        .map(|path| platform::windows::network::network_drive_to_unc(&path).unwrap_or(path))
        .collect();
    let additional_windows = additional_windows
        .into_iter()
        .map(|mut window| {
            window.tab_paths = window
                .tab_paths
                .into_iter()
                .map(|path| platform::windows::network::network_drive_to_unc(&path).unwrap_or(path))
                .collect();
            window
        })
        .collect::<Vec<_>>();
    let state = Arc::new(Mutex::new(AppState::new(
        restored_paths,
        active_index,
        default_directory_view,
        search_view,
        directory_views,
        everything_config,
        theme_mode,
        language,
        platform::system_uses_dark_theme(),
    )));
    let network_login = Arc::new(Mutex::new(NetworkLoginCoordinator::default()));
    if let Ok(mut app) = state.lock() {
        app.file_visibility = file_visibility;
        app.network_locations = network_locations;
        if !network_devices.is_empty() {
            let active_window = app.active_window;
            app.network_discovery.insert(
                active_window,
                DiscoveryCoordinator::with_devices(network_devices.clone()),
            );
        }
        app.active_window_state_mut().placement = window;
        for restored in &additional_windows {
            if !restored.tab_paths.is_empty() {
                let window_id = app.register_window(
                    restored.tab_paths.clone(),
                    restored.active_tab,
                    restored.placement,
                );
                if !network_devices.is_empty() {
                    app.network_discovery.insert(
                        window_id,
                        DiscoveryCoordinator::with_devices(network_devices.clone()),
                    );
                }
            }
        }
        let _ = app.window(app.active_window);
    }
    if let Some(scenario) = scenario {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let active_tab = app.active_window_state().active_tab;
        agent_debug::apply_scenario(
            app.active_window_state_mut()
                .tabs
                .get_mut(&active_tab)
                .expect("active tab session exists"),
            scenario,
        );
    }
    let (request_sender, network_request_sender, event_receiver) =
        spawn_directory_workers(WORKER_COUNT, NETWORK_WORKER_COUNT);
    let (network_discovery_sender, network_discovery_receiver) = spawn_network_discovery_worker();
    let event_receiver = Arc::new(Mutex::new(event_receiver));
    let everything_config = state
        .lock()
        .expect("app state mutex is not poisoned")
        .everything_config
        .clone();
    let should_discover_everything = everything_config.executable_path.is_none();
    let (everything_sender, everything_receiver) =
        spawn_everything_worker(everything_config, state.clone());
    let (icon_sender, icon_receiver) = spawn_icon_workers(ICON_WORKER_COUNT, state.clone());
    let (operation_sender, operation_receiver) = spawn_file_operation_worker();
    let (clipboard_sender, clipboard_receiver) = spawn_clipboard_worker();
    let (shell_menu_worker, shell_menu_receiver) =
        platform::windows::context_menu::ShellMenuWorker::spawn();
    let quick_menu = Arc::new(Mutex::new(QuickMenuState::default()));

    let senders = WorkerSenders {
        directory: request_sender.clone(),
        network_directory: network_request_sender.clone(),
        network_discovery: network_discovery_sender.clone(),
        operation: operation_sender.clone(),
        clipboard: clipboard_sender.clone(),
        shell_menu: shell_menu_worker.clone(),
        everything: everything_sender.clone(),
        icon: icon_sender.clone(),
        network_login: network_login_ui.as_weak(),
        network_login_state: network_login.clone(),
        network_location_rename: network_location_rename_ui.as_weak(),
    };
    let initial_window_id = state
        .lock()
        .expect("app state mutex is not poisoned")
        .active_window;

    let scoped_state = WindowSessions::new(state.clone(), initial_window_id);
    let quick_menu_popup = create_quick_menu_popup(&ui, &scoped_state)?;
    wire_root_popup_callbacks(&quick_menu_popup.root, initial_window_id);

    wire_callbacks(
        &ui,
        network_login_ui.as_weak(),
        network_login.clone(),
        network_location_rename_ui.as_weak(),
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        request_sender.clone(),
        network_request_sender.clone(),
        network_discovery_sender.clone(),
        operation_sender.clone(),
        clipboard_sender,
        everything_sender.clone(),
        icon_sender.clone(),
        shell_menu_worker.clone(),
        quick_menu.clone(),
        scoped_state.clone(),
    );
    let quick_submenu_timer = wire_context_submenu_hover(&ui);
    let quick_menu_prewarm_timer = wire_quick_menu_prewarm(
        &ui,
        scoped_state.clone(),
        quick_menu.clone(),
        shell_menu_worker.clone(),
    );
    wire_internal_drag_drop(
        &ui,
        operation_sender.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        scoped_state.clone(),
    );
    wire_address_drag(&ui, scoped_state.clone());
    let rectangle_selection_timer = wire_rectangle_selection(&ui, scoped_state.clone());
    wire_mouse_navigation(
        &ui,
        ConfirmationWindows::new(&delete_ui, &conflict_ui, &exit_ui),
        scoped_state.clone(),
        initial_window_id,
        senders.clone(),
        state.clone(),
    );
    wire_window_controls(&ui);
    wire_confirmation_windows(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        operation_sender.clone(),
        scoped_state.clone(),
    );
    wire_network_login_window(
        &ui,
        &network_login_ui,
        request_sender.clone(),
        network_request_sender.clone(),
        state.clone(),
        network_login.clone(),
    );
    wire_network_location_rename_window(&network_location_rename_ui, state.clone());
    wire_debug_showcase(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        state.clone(),
    );
    let _operation_timer =
        wire_operation_window(&ui, &operation_ui, operation_sender.clone(), state.clone());
    start_event_pump(
        &ui,
        event_receiver,
        icon_sender,
        everything_sender.clone(),
        state.clone(),
    );
    start_everything_event_pump(
        &ui,
        everything_receiver,
        everything_sender.clone(),
        state.clone(),
    );
    let _ = everything_sender.send(if should_discover_everything {
        EverythingRequest::Discover(0)
    } else {
        EverythingRequest::TestConnection(0)
    });
    start_icon_event_pump(&ui, icon_receiver, state.clone());
    start_file_operation_event_pump(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        operation_receiver,
        operation_sender.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        state.clone(),
    );
    start_shell_menu_event_pump(
        &ui,
        shell_menu_receiver,
        shell_menu_worker.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        quick_menu.clone(),
        state.clone(),
    );
    start_clipboard_event_pump(
        &ui,
        clipboard_receiver,
        operation_sender.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        state.clone(),
    );
    scan_cleanup_diagnostics(&ui, state.clone());
    start_sidebar_loader(&ui, state.clone());
    start_network_location_loader(&ui, state.clone());
    start_network_discovery_event_pump(&ui, network_discovery_receiver, state.clone());

    refresh_window_ui(&ui, &state, initial_window_id);
    refresh_operation_window(&operation_ui, &state);
    refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state);
    network_login_ui.set_dark_theme(state.lock().map(|app| app.dark_theme()).unwrap_or_default());
    let initial_tabs = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.active_window_state()
            .tab_order
            .iter()
            .filter_map(|id| {
                app.active_window_state()
                    .tabs
                    .get(id)
                    .and_then(|tab| tab.current_path.clone())
                    .map(|path| (*id, path))
            })
            .collect::<Vec<_>>()
    };
    if scenario.is_none() {
        for (tab_id, path) in initial_tabs {
            submit_path_navigation(
                &request_sender,
                &network_request_sender,
                &state,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
        }
    }

    let drag_drop_target_timer =
        wire_native_drag_drop(&ui, operation_sender.clone(), scoped_state.clone());
    let directory_watch_timer = start_directory_watchers(request_sender.clone(), state.clone());
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        runtimes.insert(
            initial_window_id,
            WindowRuntime {
                ui: ui.clone_strong(),
                sessions: scoped_state.clone(),
                _native_drop_timer: drag_drop_target_timer,
                _rectangle_selection_timer: rectangle_selection_timer,
                _quick_menu: quick_menu.clone(),
                _quick_submenu_timer: quick_submenu_timer.clone(),
                _quick_menu_prewarm_timer: quick_menu_prewarm_timer,
                quick_menu_popup,
            },
        );
    });
    let restored_window_ids = state
        .lock()
        .map(|app| {
            app.windows
                .keys()
                .copied()
                .filter(|id| *id != initial_window_id)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for window_id in restored_window_ids {
        install_app_window(
            AppWindow::new()?,
            window_id,
            &delete_ui,
            &conflict_ui,
            &exit_ui,
            &senders,
            state.clone(),
        )?;
    }
    ui.show()?;
    platform::windows::network::record_runtime_event("event_loop_started");
    let result = slint::run_event_loop();
    platform::windows::network::record_runtime_event("event_loop_returned");
    if let Ok(mut coordinator) = network_login.lock() {
        coordinator.cancel();
    }
    drop(directory_watch_timer);
    for weak in [delete_weak, conflict_weak, exit_weak] {
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
    }
    let _ = network_login_ui.hide();
    let _ = network_location_rename_ui.hide();
    let live_placements = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .map(|(id, runtime)| {
                let position = runtime.ui.window().position();
                let size = runtime.ui.window().size();
                let scale = runtime.ui.window().scale_factor();
                (
                    *id,
                    session_store::WindowPlacement {
                        x: position.x,
                        y: position.y,
                        width: (size.width as f32 / scale).round() as u32,
                        height: (size.height as f32 / scale).round() as u32,
                    },
                )
            })
            .collect::<HashMap<_, _>>()
    });
    let (
        windows,
        default_directory_view,
        search_view,
        directory_views,
        everything_config,
        theme_mode,
        language,
        file_visibility,
        network_locations,
        network_devices,
    ) = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        for (id, placement) in &live_placements {
            if let Some(window) = app.windows.get_mut(id) {
                window.placement = *placement;
            }
        }
        let mut window_ids = app.windows.keys().copied().collect::<Vec<_>>();
        window_ids.sort_by_key(|id| id.0);
        let windows = window_ids
            .into_iter()
            .filter_map(|id| {
                let window = app.windows.get(&id)?;
                let paths = window.stable_paths();
                (!paths.is_empty()).then_some(session_store::WindowSessionState {
                    placement: window.placement,
                    active_tab: window.stable_active_path_index(),
                    tab_paths: paths,
                })
            })
            .collect::<Vec<_>>();
        let mut directory_views = app
            .directory_views
            .iter()
            .map(|(path, preference)| (path.clone(), *preference))
            .collect::<Vec<_>>();
        directory_views.sort_by_key(|(path, _)| {
            app.directory_view_lru
                .iter()
                .position(|candidate| candidate == path)
                .unwrap_or(usize::MAX)
        });
        (
            windows,
            app.default_directory_view,
            app.search_view,
            directory_views,
            app.everything_config.clone(),
            app.theme_mode,
            app.language,
            app.file_visibility,
            app.network_locations.clone(),
            app.network_discovery
                .values()
                .flat_map(|discovery| discovery.successful_devices().iter())
                .filter(|device| device.unc_path.is_some())
                .fold(Vec::new(), |mut devices, device| {
                    if !devices
                        .iter()
                        .any(|known: &NetworkDeviceTarget| known.id == device.id)
                    {
                        devices.push(device.clone());
                    }
                    devices
                }),
        )
    };
    if scenario.is_none()
        && let Some(path) = session_store::default_path()
        && let Ok(session) = session_store::SessionState::with_windows_and_settings(
            windows,
            default_directory_view,
            search_view,
            directory_views,
            theme_mode,
            language,
            everything_config,
            file_visibility,
            network_locations,
            network_devices,
        )
    {
        let _ = session_store::save(&path, &session);
    }
    clear_window_runtimes();
    platform::windows::drag_drop::shutdown_current();
    let _ = state.lock().ok().and_then(|mut app| {
        let active_window = app.active_window;
        app.close_window(active_window)
    });
    result
}

fn submit_navigation(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
) -> bool {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.cancel_column_drag();
        if let Some(tab) = app.tab_mut(tab_id) {
            cancel_folder_sizes(tab);
        }
        app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
        app.thumbnail_requests
            .retain(|(request_tab, _, _, _)| *request_tab != tab_id);
        app.focus_after_refresh.remove(&tab_id);
        let Some(tab) = app.tab_mut(tab_id) else {
            return false;
        };
        if tab.kind != TabKind::Files {
            return false;
        }
        if kind == NavigationKind::Normal && tab.current_path.as_ref() == Some(&path) {
            tab.cancel_address_edit();
            return false;
        }
        let (request_id, cancel) = tab.begin_navigation(path.clone(), kind);
        DirectoryRequest {
            tab_id,
            request_id,
            path,
            visibility: app.file_visibility,
            cancel,
        }
    };
    sender.send(request).is_ok()
}

fn submit_network_navigation(
    sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
) -> bool {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.cancel_column_drag();
        if let Some(tab) = app.tab_mut(tab_id) {
            cancel_folder_sizes(tab);
        }
        app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
        app.thumbnail_requests
            .retain(|(request_tab, _, _, _)| *request_tab != tab_id);
        app.focus_after_refresh.remove(&tab_id);
        let Some(tab) = app.tab_mut(tab_id) else {
            return false;
        };
        if tab.kind != TabKind::Files {
            return false;
        }
        if kind == NavigationKind::Normal && tab.current_path.as_ref() == Some(&path) {
            tab.cancel_address_edit();
            return false;
        }
        let (request_id, cancel) = tab.begin_navigation(path.clone(), kind);
        DirectoryRequest {
            tab_id,
            request_id,
            path,
            visibility: app.file_visibility,
            cancel,
        }
    };
    match sender.try_send(request) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(request)) => {
            if let Ok(mut app) = state.lock()
                && let Some(tab) = app.tab_mut(request.tab_id)
                && tab.latest_request == request.request_id
            {
                tab.load_state = LoadState::Failed;
                tab.error = Some("network directory queue is busy".to_owned());
            }
            false
        }
        Err(mpsc::TrySendError::Disconnected(_)) => false,
    }
}

fn submit_path_navigation(
    local_sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
) -> bool {
    if crate::network::is_unc_path(&path) {
        submit_network_navigation(network_sender, state, tab_id, path, kind)
    } else {
        submit_navigation(local_sender, state, tab_id, path, kind)
    }
}

fn sidebar_navigation_target(app: &AppState, index: usize) -> Option<PathBuf> {
    if let Some(location) = app.sidebar.get(index) {
        return Some(location.path.clone());
    }
    let mut location_targets = app
        .imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .map(|location| match &location.target {
            NetworkTarget::WindowsPath(path) => (location.sort_order, Some(path.clone())),
            NetworkTarget::ShellItemId(_) => (location.sort_order, None),
        })
        .collect::<Vec<_>>();
    location_targets.sort_by_key(|(order, _)| *order);
    let offset = index.saturating_sub(app.sidebar.len());
    if offset < location_targets.len() {
        return location_targets
            .get(offset)
            .and_then(|(_, path)| path.clone());
    }
    app.network_discovery
        .get(&app.active_window)
        .and_then(|discovery| {
            discovery
                .devices()
                .iter()
                .filter_map(crate::network::device_root_target)
                .nth(offset - location_targets.len())
        })
}
fn restart_detached_tab(
    outcome: &DetachedTabOutcome,
    senders: &WorkerSenders,
    state: &SharedSessions,
) {
    match &outcome.restart {
        Some(DetachedTabRestart::Directory(path)) => {
            submit_path_navigation(
                &senders.directory,
                &senders.network_directory,
                state,
                outcome.tab_id,
                path.clone(),
                NavigationKind::Refresh,
            );
        }
        Some(DetachedTabRestart::Search {
            scope,
            depth,
            query,
        }) => {
            if let Ok(mut app) = state.lock()
                && let Some(tab) = app.tab_mut(outcome.tab_id)
            {
                tab.search_scope = scope.clone();
                tab.search_depth = *depth;
            }
            submit_search(
                &senders.everything,
                state,
                None,
                outcome.tab_id,
                query.clone(),
            );
        }
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn install_app_window(
    ui: AppWindow,
    window_id: WindowId,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    senders: &WorkerSenders,
    state: SharedSessions,
) -> Result<(), slint::PlatformError> {
    let placement = state
        .lock()
        .ok()
        .and_then(|app| app.window(window_id).map(|window| window.placement))
        .ok_or(slint::PlatformError::NoPlatform)?;
    install_app_window_at(
        ui,
        window_id,
        placement,
        true,
        delete_ui,
        conflict_ui,
        exit_ui,
        senders,
        state,
    )
}

#[allow(clippy::too_many_arguments)]
fn install_app_window_at(
    ui: AppWindow,
    window_id: WindowId,
    placement: session_store::WindowPlacement,
    refresh_before_show: bool,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    senders: &WorkerSenders,
    state: SharedSessions,
) -> Result<(), slint::PlatformError> {
    let scoped = WindowSessions::new(state.clone(), window_id);
    let quick_menu = Arc::new(Mutex::new(QuickMenuState::default()));
    let quick_menu_popup = create_quick_menu_popup(&ui, &scoped)?;
    wire_root_popup_callbacks(&quick_menu_popup.root, window_id);
    ui.window()
        .set_position(slint::PhysicalPosition::new(placement.x, placement.y));
    ui.window().set_size(slint::LogicalSize::new(
        placement.width as f32,
        placement.height as f32,
    ));
    wire_callbacks(
        &ui,
        senders.network_login.clone(),
        senders.network_login_state.clone(),
        senders.network_location_rename.clone(),
        delete_ui,
        conflict_ui,
        exit_ui,
        senders.directory.clone(),
        senders.network_directory.clone(),
        senders.network_discovery.clone(),
        senders.operation.clone(),
        senders.clipboard.clone(),
        senders.everything.clone(),
        senders.icon.clone(),
        senders.shell_menu.clone(),
        quick_menu.clone(),
        scoped.clone(),
    );
    let quick_submenu_timer = wire_context_submenu_hover(&ui);
    let quick_menu_prewarm_timer = wire_quick_menu_prewarm(
        &ui,
        scoped.clone(),
        quick_menu.clone(),
        senders.shell_menu.clone(),
    );
    wire_internal_drag_drop(
        &ui,
        senders.operation.clone(),
        senders.directory.clone(),
        senders.network_directory.clone(),
        scoped.clone(),
    );
    wire_address_drag(&ui, scoped.clone());
    let rectangle_selection_timer = wire_rectangle_selection(&ui, scoped.clone());
    wire_mouse_navigation(
        &ui,
        ConfirmationWindows::new(delete_ui, conflict_ui, exit_ui),
        scoped.clone(),
        window_id,
        senders.clone(),
        state.clone(),
    );
    wire_window_controls(&ui);
    let native_drop_timer = wire_native_drag_drop(&ui, senders.operation.clone(), scoped.clone());
    if refresh_before_show {
        refresh_ui(&ui, &scoped);
    }
    ui.show()?;
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        runtimes.insert(
            window_id,
            WindowRuntime {
                ui,
                sessions: scoped.clone(),
                _native_drop_timer: native_drop_timer,
                _rectangle_selection_timer: rectangle_selection_timer,
                _quick_menu: quick_menu.clone(),
                _quick_submenu_timer: quick_submenu_timer.clone(),
                _quick_menu_prewarm_timer: quick_menu_prewarm_timer,
                quick_menu_popup,
            },
        );
    });
    Ok(())
}

fn detach_tab_into_new_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    screen_x: i32,
    screen_y: i32,
    senders: &WorkerSenders,
    confirmations: &ConfirmationWindows,
    state: &SharedSessions,
) -> bool {
    let destination = {
        let Ok(mut app) = state.lock() else {
            return false;
        };
        let Some(drag) = app.tab_drag else {
            return false;
        };
        if drag.window_id != source_window || !matches!(drag.phase, TabDragPhase::Dragging { .. }) {
            return false;
        }
        app.reserve_window_id()
    };
    let scale = source_ui.window().scale_factor();
    let placement = session_store::WindowPlacement {
        x: screen_x - (178.0 * scale / 2.0).round() as i32,
        y: screen_y - (20.0 * scale).round() as i32,
        width: 1180,
        height: 760,
    };
    let candidate = match AppWindow::new() {
        Ok(candidate) => candidate,
        Err(_) => return false,
    };
    if install_app_window_at(
        candidate,
        destination,
        placement,
        false,
        &confirmations
            .delete
            .upgrade()
            .expect("shared delete window exists"),
        &confirmations
            .conflict
            .upgrade()
            .expect("shared conflict window exists"),
        &confirmations
            .exit
            .upgrade()
            .expect("shared exit window exists"),
        senders,
        state.clone(),
    )
    .is_err()
    {
        return false;
    }
    let outcome = {
        let Ok(mut app) = state.lock() else {
            remove_window_runtime(destination);
            return false;
        };
        app.detach_dragged_tab_to_window(destination, placement)
    };
    let Some(outcome) = outcome else {
        if let Some(destination_ui) = window_ui(destination) {
            let _ = destination_ui.hide();
        }
        remove_window_runtime(destination);
        return false;
    };
    if let Some(destination_ui) = window_ui(destination) {
        refresh_window_ui(&destination_ui, state, destination);
    }
    restart_detached_tab(&outcome, senders, state);
    if outcome.source_window_closed {
        let _ = source_ui.hide();
        remove_window_runtime(source_window);
    } else if let Some(source) = window_ui(source_window) {
        refresh_ui(&source, &WindowSessions::new(state.clone(), source_window));
        source.invoke_clear_tab_drag();
    }
    true
}

fn move_tab_into_existing_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    destination_window: WindowId,
    insertion_index: usize,
    senders: &WorkerSenders,
    state: &SharedSessions,
) -> bool {
    if window_ui(destination_window).is_none() {
        return false;
    }
    let outcome = state
        .lock()
        .ok()
        .and_then(|mut app| app.move_dragged_tab_to_window(destination_window, insertion_index));
    let Some(outcome) = outcome else {
        return false;
    };
    restart_detached_tab(&outcome, senders, state);
    if let Some(destination) = window_ui(destination_window) {
        refresh_window_ui(&destination, state, destination_window);
    }
    if outcome.source_window_closed {
        let _ = source_ui.hide();
        remove_window_runtime(source_window);
    } else {
        refresh_window_ui(source_ui, state, source_window);
        source_ui.invoke_clear_tab_drag();
    }
    true
}

fn finish_native_tab_drag(
    source_ui: &AppWindow,
    source_window: WindowId,
    drop_point: Option<platform::windows::drag_drop::TabDropPoint>,
    senders: &WorkerSenders,
    confirmations: &ConfirmationWindows,
    state: &SharedSessions,
) -> bool {
    let dragging = state.lock().is_ok_and(|app| {
        app.tab_drag.is_some_and(|drag| {
            drag.window_id == source_window && matches!(drag.phase, TabDragPhase::Dragging { .. })
        })
    });
    if !dragging {
        return false;
    }
    let Some(drop_point) = drop_point else {
        if let Ok(mut app) = state.lock() {
            app.cancel_tab_drag();
        }
        project_native_insertion_indicator(None, state);
        source_ui.invoke_clear_tab_drag();
        refresh_window_ui(source_ui, state, source_window);
        return false;
    };
    let screen_x = drop_point.screen_x;
    let screen_y = drop_point.screen_y;
    let target_window = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes.iter().find_map(|(window_id, runtime)| {
            (native_window_handle(&runtime.ui) == drop_point.target_hwnd).then_some(*window_id)
        })
    });
    let cross_target = target_window
        .filter(|window_id| *window_id != source_window)
        .and_then(|_| cross_window_drop_target(source_window, screen_x, screen_y, state));
    let moved = cross_target.is_some_and(|(destination, insertion)| {
        move_tab_into_existing_window(
            source_ui,
            source_window,
            destination,
            insertion,
            senders,
            state,
        )
    });
    let (valid_source, _) = source_tab_drop_is_valid(source_ui, screen_x, screen_y)
        .unwrap_or((false, winit::dpi::PhysicalPosition::new(0.0, 0.0)));
    let source_has_single_file_tab = state
        .lock()
        .is_ok_and(|app| app.drag_source_is_single_file_tab_window(source_window));
    let unmatched_action = (!moved && cross_target.is_none() && !valid_source)
        .then(|| unmatched_tab_drop_action(source_has_single_file_tab, target_window.is_some()))
        .flatten();
    let moved_source_window =
        matches!(
            unmatched_action,
            Some(UnmatchedTabDropAction::MoveSourceWindow)
        ) && move_single_tab_source_window(source_ui, source_window, screen_x, screen_y, state);
    let detached = matches!(
        unmatched_action,
        Some(UnmatchedTabDropAction::DetachToNewWindow)
    ) && detach_tab_into_new_window(
        source_ui,
        source_window,
        screen_x,
        screen_y,
        senders,
        confirmations,
        state,
    );
    let finished = if valid_source {
        state.lock().is_ok_and(|mut app| app.finish_tab_drag(true))
    } else if moved || moved_source_window || detached {
        false
    } else {
        state.lock().is_ok_and(|mut app| app.cancel_tab_drag())
    };
    project_native_insertion_indicator(None, state);
    source_ui.invoke_clear_tab_drag();
    if !moved && !detached {
        refresh_window_ui(source_ui, state, source_window);
    }
    finished || moved || moved_source_window || detached
}

fn move_single_tab_source_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    screen_x: i32,
    screen_y: i32,
    state: &SharedSessions,
) -> bool {
    let scale = source_ui.window().scale_factor();
    let Some((grab_x, grab_y)) = state.lock().ok().and_then(|app| {
        let drag = app
            .tab_drag
            .filter(|drag| drag.window_id == source_window)?;
        Some((drag.press_x, drag.press_y))
    }) else {
        return false;
    };
    let outer = source_ui.window().position();
    let Ok((client_left, client_top, _, _)) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(source_ui))
    else {
        return false;
    };
    let (x, y) = single_tab_window_drop_position(
        screen_x,
        screen_y,
        grab_x,
        grab_y,
        scale,
        client_left - outer.x,
        client_top - outer.y,
    );
    let committed = state
        .lock()
        .is_ok_and(|mut app| app.commit_drag_source_window_move(source_window, x, y));
    if !committed {
        return false;
    }
    if platform::windows::move_window(native_window_handle(source_ui), x, y).is_err() {
        if let Ok(mut app) = state.lock()
            && let Some(window) = app.windows.get_mut(&source_window)
        {
            window.placement.x = outer.x;
            window.placement.y = outer.y;
        }
        return false;
    }
    true
}

fn single_tab_window_drop_position(
    screen_x: i32,
    screen_y: i32,
    grab_x: f32,
    grab_y: f32,
    scale: f32,
    client_offset_x: i32,
    client_offset_y: i32,
) -> (i32, i32) {
    (
        screen_x - (grab_x * scale).round() as i32 - client_offset_x,
        screen_y - (grab_y * scale).round() as i32 - client_offset_y,
    )
}

fn drag_paths_for_pressed_entry(app: &AppState, entry_id: EntryId) -> Vec<PathBuf> {
    let tab = app.active();
    if tab.selected.contains(&entry_id) {
        return selected_paths(app);
    }
    tab.visible_entry(entry_id)
        .map(|entry| vec![entry.path.clone()])
        .unwrap_or_default()
}

fn selected_paths(app: &AppState) -> Vec<PathBuf> {
    app.active()
        .selected
        .iter()
        .filter_map(|id| {
            app.active()
                .visible_entry(*id)
                .map(|entry| entry.path.clone())
        })
        .collect()
}
fn selected_paths_for_tab(tab: &TabSession) -> Vec<PathBuf> {
    tab.selected
        .iter()
        .filter_map(|id| tab.visible_entry(*id).map(|entry| entry.path.clone()))
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
struct FileHitGeometry {
    list_left: f32,
    list_top: f32,
    viewport_x: f32,
    viewport_y: f32,
    viewport_width: f32,
    columns_width: f32,
}

impl FileHitGeometry {
    fn details_range(self) -> (f32, f32) {
        (
            16.0_f32.max(16.0 + self.viewport_x),
            self.viewport_width
                .min(16.0 + self.viewport_x + self.columns_width),
        )
    }

    fn details_contains(self, window_x: f32) -> bool {
        let local_x = window_x - self.list_left;
        let (left, right) = self.details_range();
        right > left && local_x >= left && local_x < right
    }
}

fn directory_entry_at_visual_point(
    app: &AppState,
    tab: &TabSession,
    view_mode: ViewMode,
    grid_columns: usize,
    local_x: f32,
    content_y: f32,
) -> Option<EntryId> {
    if content_y < 0.0 || local_x < 16.0 {
        return None;
    }
    let entries = tab.visible_entries();
    let groups = directory_group_projections(app, tab, entries);
    if file_layout_geometry(view_mode).grid {
        let projection = IconProjection::from_groups(
            &groups,
            grid_columns,
            32,
            file_row_height(view_mode) as u64,
        );
        let location = projection.offsets.locate(content_y as u64)?;
        let IconVisualRow::Entries { entries, .. } = projection.rows.get(location.row_index)?
        else {
            return None;
        };
        let geometry = file_layout_geometry(view_mode);
        let column = ((local_x - 16.0) / (geometry.card_width + 8.0).max(1.0)).floor() as usize;
        entries.get(column).copied()
    } else {
        let projection =
            ListProjection::from_groups(&groups, 32, file_row_height(view_mode) as u64);
        let location = projection.offsets.locate(content_y as u64)?;
        match projection.rows.get(location.row_index)? {
            ListVisualRow::Entry { entry_id } => Some(*entry_id),
            ListVisualRow::GroupHeader { .. } => None,
        }
    }
}
fn context_target_at(
    state: &SharedSessions,
    window_x: f32,
    window_y: f32,
    geometry: FileHitGeometry,
    search_scroll_y: f32,
    grid_columns: usize,
) -> (Option<EntryId>, bool) {
    let app = state.lock().expect("app state mutex is not poisoned");
    if window_y < geometry.list_top {
        return (None, true);
    }
    let active = app.active();
    let view_mode = app.active_view_mode();
    let local_x = window_x - geometry.list_left;
    if view_mode == ViewMode::Details && !geometry.details_contains(window_x) {
        return (None, true);
    }
    if view_mode != ViewMode::Details
        && (local_x < 16.0 || local_x >= geometry.viewport_width - 16.0)
    {
        return (None, true);
    }
    let content_y = window_y - geometry.list_top + (-geometry.viewport_y).max(0.0);
    let local_row = (content_y / file_row_height(view_mode)).floor() as usize;
    let local_index = if file_layout_geometry(view_mode).grid {
        let local_x = window_x - geometry.list_left;
        if local_x < 16.0 || local_x >= geometry.viewport_width - 16.0 {
            return (None, true);
        }
        let column = ((local_x - 16.0)
            / (file_layout_geometry(view_mode).card_width + 8.0).max(1.0))
        .floor() as usize;
        local_row
            .saturating_mul(grid_columns.max(1))
            .saturating_add(column.min(grid_columns.max(1) - 1))
    } else {
        local_row
    };
    let entry = if active.page_source == PageSource::Search {
        let window = search_window_for_scroll(
            search_scroll_y,
            active.search_total.unwrap_or(0),
            view_mode,
            grid_columns,
        );
        window
            .start
            .checked_add(local_index as u32)
            .and_then(|index| index.checked_add(1))
            .and_then(|id| active.visible_entry(EntryId(id)))
            .map(|entry| entry.id)
    } else {
        directory_entry_at_visual_point(
            &app,
            active,
            view_mode,
            grid_columns,
            window_x - geometry.list_left,
            content_y,
        )
    };
    (entry, entry.is_none())
}

fn enqueue_operation(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    origin_tab: TabId,
    kind: FileOperationKind,
    mut items: Vec<OperationItem>,
) {
    if items.is_empty() {
        return;
    }
    for item in &mut items {
        if let Some(source) = item.source.take() {
            item.source =
                Some(platform::windows::network::network_drive_to_unc(&source).unwrap_or(source));
        }
        if let Some(destination) = item.destination.take() {
            item.destination = Some(
                platform::windows::network::network_drive_to_unc(&destination)
                    .unwrap_or(destination),
            );
        }
    }
    let resource = operation_resource(&items);
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.tab(origin_tab).is_none() {
            return;
        }
        app.operations
            .submit(resource, kind, Some(origin_tab), items);
        if app.operations.active_id(resource).is_some() {
            return;
        }
        app.operations
            .start_next(resource)
            .ok()
            .flatten()
            .and_then(|id| {
                let _ = app.operations.mark_running(id);
                app.operations.task(id).map(|task| FileOperationRequest {
                    id,
                    kind: task.kind,
                    resource: task.resource,
                    items: task.items.clone(),
                    cancellation: task.cancellation.clone(),
                })
            })
    };
    if let Some(request) = request {
        let _ = sender.send(request);
    }
}

fn operation_resource(items: &[OperationItem]) -> OperationResource {
    if items.iter().any(|item| {
        item.source
            .as_deref()
            .is_some_and(crate::network::is_unc_path)
            || item
                .destination
                .as_deref()
                .is_some_and(crate::network::is_unc_path)
    }) {
        OperationResource::Network
    } else {
        OperationResource::Local
    }
}

fn create_default_folder(state: &WindowSessions, sender: &mpsc::Sender<FileOperationRequest>) {
    let target = state.lock().ok().and_then(|app| {
        let tab_id = app.window(state.window_id)?.active_tab;
        let name = match app.language {
            Language::Chinese => "新建文件夹",
            Language::English => "New folder",
        };
        app.tab(tab_id)
            .and_then(TabSession::visible_path)
            .map(|parent| (tab_id, parent.join(name)))
    });
    if let Some((tab_id, path)) = target {
        enqueue_operation(
            &state.shared,
            sender,
            tab_id,
            FileOperationKind::CreateFolder,
            vec![OperationItem::pending(None, Some(path))],
        );
    }
}
fn request_clipboard_write(
    state: &WindowSessions,
    sender: &mpsc::Sender<ClipboardRequest>,
    cut: bool,
) {
    let paths = state
        .lock()
        .map(|app| {
            app.window(state.window_id)
                .and_then(|window| window.tabs.get(&window.active_tab))
                .map(selected_paths_for_tab)
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if !paths.is_empty() {
        let _ = sender.send(ClipboardRequest::Write { paths, cut });
    }
}

fn request_clipboard_paste(state: &WindowSessions, sender: &mpsc::Sender<ClipboardRequest>) {
    let target = state.lock().ok().and_then(|app| {
        let tab_id = app.window(state.window_id)?.active_tab;
        app.tab(tab_id)
            .and_then(TabSession::visible_path)
            .map(|path| (tab_id, path.to_path_buf()))
    });
    if let Some((origin_tab, target)) = target {
        let _ = sender.send(ClipboardRequest::ReadPaste { origin_tab, target });
    }
}
fn set_view_mode(state: &WindowSessions, mode: ViewMode) {
    if let Ok(mut app) = state.lock() {
        let tab_id = app.active_window_state().active_tab;
        if app.active().page_source == PageSource::Search {
            app.search_view.view_mode = mode;
        } else if let Some(path) = app.active().visible_path().map(Path::to_path_buf) {
            app.update_directory_preference(path, |preference| preference.view_mode = mode);
        }
        app.thumbnail_requests
            .retain(|(request_tab, _, _, _)| *request_tab != tab_id);
    }
}

fn set_sort_field(state: &WindowSessions, field: SortField) {
    if let Ok(mut app) = state.lock() {
        if app.active().page_source == PageSource::Search {
            let direction = app.search_view.sort_direction;
            app.search_view.sort_field = field;
            for window in app.windows.values_mut() {
                for tab in window
                    .tabs
                    .values_mut()
                    .filter(|tab| tab.page_source == PageSource::Search)
                {
                    tab.search_sort_field = field;
                    tab.search_sort_direction = direction;
                }
            }
        } else if let Some(path) = app.active().visible_path().map(Path::to_path_buf) {
            let direction = app.directory_preference(&path).sort_direction;
            app.update_directory_preference(path.clone(), |preference| {
                preference.sort_field = field
            });
            for window in app.windows.values_mut() {
                for tab in window.tabs.values_mut().filter(|tab| {
                    tab.page_source == PageSource::Directory
                        && tab.visible_path() == Some(path.as_path())
                }) {
                    tab.sort_field = field;
                    tab.sort_direction = direction;
                    tab.resort_entries();
                }
            }
        }
    }
}

fn set_sort_direction(state: &WindowSessions, direction: SortDirection) {
    if let Ok(mut app) = state.lock() {
        if app.active().page_source == PageSource::Search {
            app.search_view.sort_direction = direction;
            for window in app.windows.values_mut() {
                for tab in window
                    .tabs
                    .values_mut()
                    .filter(|tab| tab.page_source == PageSource::Search)
                {
                    tab.search_sort_direction = direction;
                }
            }
        } else if let Some(path) = app.active().visible_path().map(Path::to_path_buf) {
            app.update_directory_preference(path.clone(), |preference| {
                preference.sort_direction = direction
            });
            for window in app.windows.values_mut() {
                for tab in window.tabs.values_mut().filter(|tab| {
                    tab.page_source == PageSource::Directory
                        && tab.visible_path() == Some(path.as_path())
                }) {
                    tab.sort_direction = direction;
                    tab.resort_entries();
                }
            }
        }
    }
}

fn set_group(state: &WindowSessions, field: GroupField, direction: Option<SortDirection>) -> bool {
    let Ok(mut app) = state.lock() else {
        return false;
    };
    if app.active().page_source == PageSource::Search {
        return false;
    }
    let Some(path) = app.active().visible_path().map(Path::to_path_buf) else {
        return false;
    };
    app.update_directory_preference(path, |preference| {
        preference.group_field = field;
        if let Some(direction) = direction {
            preference.group_direction = direction;
        }
    });
    true
}

fn apply_group_command(state: &WindowSessions, command: i32) -> bool {
    match command {
        command if (CMD_GROUP_BASE..CMD_GROUP_BASE + 6).contains(&command) => {
            GroupField::from_storage_code((command - CMD_GROUP_BASE) as u8)
                .is_some_and(|field| set_group(state, field, None))
        }
        CMD_GROUP_ASC | CMD_GROUP_DESC => {
            let field = state
                .lock()
                .ok()
                .and_then(|app| {
                    app.active()
                        .visible_path()
                        .map(|path| app.directory_preference(path).group_field)
                })
                .unwrap_or(GroupField::None);
            set_group(
                state,
                field,
                Some(if command == CMD_GROUP_ASC {
                    SortDirection::Ascending
                } else {
                    SortDirection::Descending
                }),
            )
        }
        _ => false,
    }
}

fn fitted_column_width(app: &AppState, kind: ColumnKind) -> u32 {
    let header = match (app.language, kind) {
        (Language::Chinese, ColumnKind::Name) => "名称",
        (Language::English, ColumnKind::Name) => "Name",
        (Language::Chinese, ColumnKind::Kind) => "类型",
        (Language::English, ColumnKind::Kind) => "Type",
        (Language::Chinese, ColumnKind::Size) => "大小",
        (Language::English, ColumnKind::Size) => "Size",
        (Language::Chinese, ColumnKind::Modified) => "修改时间",
        (Language::English, ColumnKind::Modified) => "Date modified",
        (Language::Chinese, ColumnKind::Created) => "创建时间",
        (Language::English, ColumnKind::Created) => "Date created",
    };
    let texts = Texts::new(app.language);
    let max_chars = app
        .active()
        .visible_entries()
        .iter()
        .map(|entry| match kind {
            ColumnKind::Name => entry.display_name.chars().count(),
            ColumnKind::Kind => entry
                .path
                .extension()
                .map(|value| value.to_string_lossy().chars().count() + 5)
                .unwrap_or(6),
            ColumnKind::Size => texts.size(entry.size_bytes).chars().count(),
            ColumnKind::Modified => texts.modified(entry.modified).chars().count(),
            ColumnKind::Created => texts.modified(entry.created).chars().count(),
        })
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(8);
    let padding = if kind == ColumnKind::Name { 54 } else { 28 };
    (max_chars as u32 * 8 + padding).clamp(
        crate::domain::MIN_COLUMN_WIDTH,
        crate::domain::MAX_COLUMN_WIDTH,
    )
}
fn begin_rename_ui(weak: &slint::Weak<AppWindow>, state: &WindowSessions) {
    let target = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.active().selected.len() != 1 {
            return;
        }
        let id = app.active().selected[0];
        let entry = app.active().visible_entry(id).cloned();
        let name = entry.as_ref().map(|entry| {
            if entry.kind == crate::domain::EntryKind::File {
                entry
                    .path
                    .file_stem()
                    .unwrap_or(&entry.original_name)
                    .to_string_lossy()
                    .into_owned()
            } else {
                entry.display_name.clone()
            }
        });
        let extension = entry
            .filter(|entry| entry.kind == crate::domain::EntryKind::File)
            .and_then(|entry| entry.path.extension().map(std::ffi::OsStr::to_os_string));
        let tab_id = app.active_window_state().active_tab;
        app.rename_targets
            .insert(state.window_id, (tab_id, id, extension));
        name.map(|name| (id, name))
    };
    if let Some((id, name)) = target
        && let Some(ui) = weak.upgrade()
    {
        ui.set_rename_entry_id(id.0 as i32);
        ui.set_rename_input(name.into());
        ui.set_rename_editing(true);
    }
}

fn rename_validation_message(
    language: Language,
    error: crate::fs::file_operations::NameValidationError,
) -> String {
    use crate::fs::file_operations::NameValidationError;
    match (language, error) {
        (Language::Chinese, NameValidationError::Empty) => "名称不能为空。".to_owned(),
        (Language::English, NameValidationError::Empty) => "The name cannot be empty.".to_owned(),
        (Language::Chinese, NameValidationError::DotName) => "不能使用这个名称。".to_owned(),
        (Language::English, NameValidationError::DotName) => "This name cannot be used.".to_owned(),
        (Language::Chinese, NameValidationError::InvalidCharacter(character)) => {
            format!("名称不能包含字符“{character}”。")
        }
        (Language::English, NameValidationError::InvalidCharacter(character)) => {
            format!("The name cannot contain '{character}'.")
        }
        (Language::Chinese, NameValidationError::TrailingSpaceOrDot) => {
            "名称不能以空格或句点结尾。".to_owned()
        }
        (Language::English, NameValidationError::TrailingSpaceOrDot) => {
            "The name cannot end with a space or period.".to_owned()
        }
        (Language::Chinese, NameValidationError::ReservedName) => {
            "这是 Windows 保留名称，不能使用。".to_owned()
        }
        (Language::English, NameValidationError::ReservedName) => {
            "This name is reserved by Windows.".to_owned()
        }
    }
}

fn submit_rename(
    state: &WindowSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    name: &str,
) -> Result<(), String> {
    let target = {
        let app = state.lock().expect("app state mutex is not poisoned");
        crate::fs::file_operations::validate_name(std::ffi::OsStr::new(name))
            .map_err(|error| rename_validation_message(app.language, error))?;
        app.rename_targets
            .get(&state.window_id)
            .and_then(|(tab_id, id, extension)| {
                app.window(state.window_id)?
                    .tabs
                    .get(tab_id)?
                    .visible_entry(*id)
                    .and_then(|entry| {
                        entry.path.parent().map(|parent| {
                            let mut new_name = std::ffi::OsString::from(name);
                            if let Some(extension) = extension.as_ref() {
                                new_name.push(".");
                                new_name.push(extension);
                            }
                            (
                                *tab_id,
                                OperationItem::pending(
                                    Some(entry.path.clone()),
                                    Some(parent.join(new_name)),
                                ),
                            )
                        })
                    })
            })
    };
    let (tab_id, item) =
        target.ok_or_else(|| "Rename target is no longer available.".to_owned())?;
    enqueue_operation(
        &state.shared,
        sender,
        tab_id,
        FileOperationKind::Rename,
        vec![item],
    );
    Ok(())
}

fn should_fast_remove(path: &Path) -> bool {
    if crate::network::is_unc_path(path) {
        return false;
    }
    let protected = path.parent().is_none()
        || std::env::var_os("USERPROFILE").is_some_and(|home| Path::new(&home) == path)
        || std::env::current_dir().is_ok_and(|workspace| workspace == path);
    !protected
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && std::fs::read_dir(path)
            .ok()
            .and_then(|mut entries| entries.nth(999))
            .is_some()
}

fn pending_permanent_delete(state: &WindowSessions) -> Option<(TabId, Vec<OperationItem>)> {
    state.lock().ok().and_then(|app| {
        let window = app.window(state.window_id)?;
        let tab = window.tabs.get(&window.active_tab)?;
        let items = selected_paths_for_tab(tab)
            .into_iter()
            .map(|path| OperationItem::pending(Some(path), None))
            .collect::<Vec<_>>();
        (!items.is_empty()).then_some((tab.id, items))
    })
}
fn submit_delete_items(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    origin_tab: TabId,
    permanent: bool,
    items: Vec<OperationItem>,
) {
    let kind = if permanent {
        FileOperationKind::PermanentDelete
    } else {
        FileOperationKind::RecycleDelete
    };
    enqueue_operation(state, sender, origin_tab, kind, items);
}

fn submit_delete(
    state: &WindowSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    permanent: bool,
) {
    if let Some((origin_tab, items)) = pending_permanent_delete(state) {
        submit_delete_items(&state.shared, sender, origin_tab, permanent, items);
    }
}

const SHELL_CONTEXT_COMMAND_BASE: i32 = 100_000;
const CMD_REFRESH: i32 = 20;
const CMD_VIEW_BASE: i32 = 100;
const CMD_SORT_BASE: i32 = 120;
const CMD_SORT_ASC: i32 = 130;
const CMD_SORT_DESC: i32 = 131;
const CMD_GROUP_BASE: i32 = 140;
const CMD_GROUP_ASC: i32 = 150;
const CMD_GROUP_DESC: i32 = 151;
const CMD_COLUMN_FIT: i32 = 160;
const CMD_COLUMNS_FIT: i32 = 161;
const CMD_COLUMN_TOGGLE_BASE: i32 = 170;
const CMD_ADD_NETWORK_LOCATION: i32 = 190;
const CMD_NETWORK_LOCATION_OPEN: i32 = 191;
const CMD_NETWORK_LOCATION_COPY_ADDRESS: i32 = 192;
const CMD_NETWORK_LOCATION_REMOVE: i32 = 193;
const CMD_NETWORK_LOCATION_MOVE_UP: i32 = 194;
const CMD_NETWORK_LOCATION_MOVE_DOWN: i32 = 195;
const CMD_NETWORK_LOCATION_OPEN_NEW_TAB: i32 = 196;
const CMD_NETWORK_LOCATION_MANAGE_CREDENTIALS: i32 = 197;
const CMD_NETWORK_LOCATION_RENAME: i32 = 198;
const CMD_QUICK_ACCESS_PIN: i32 = 199;
const CMD_QUICK_ACCESS_UNPIN: i32 = 200;
const NODE_VIEW: i32 = 10_001;
const NODE_SORT: i32 = 10_002;
const NODE_GROUP: i32 = 10_003;
const NODE_COLUMNS: i32 = 10_004;
const QUICK_MENU_PLACEHOLDER_ROWS: usize = 3;
const QUICK_MENU_SNAPSHOT_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct QuickMenuKey {
    window_id: WindowId,
    tab_id: TabId,
    navigation_request: RequestId,
    paths: Vec<PathBuf>,
    folder: Option<PathBuf>,
}

#[derive(Clone)]
struct QuickMenuIdentity {
    session_id: u64,
    request_id: u64,
    key: QuickMenuKey,
    ready: bool,
}

#[derive(Clone)]
struct QuickMenuSnapshot {
    rows: Vec<ContextCommandRow>,
    captured_at: Instant,
}

#[derive(Default)]
struct QuickMenuState {
    next_request: u64,
    identity: Option<QuickMenuIdentity>,
    snapshots: HashMap<QuickMenuKey, QuickMenuSnapshot>,
    all_rows: Vec<ContextCommandRow>,
    built_in_rows: Vec<ContextCommandRow>,
    submenu_rows: Vec<ContextCommandRow>,
    submenu_history: Vec<(u64, Vec<ContextCommandRow>)>,
    submenu_tokens: HashMap<i32, u64>,
    preloaded_submenu_rows: HashMap<u64, Vec<ContextCommandRow>>,
    loaded_submenu_rows: HashMap<u64, Vec<ContextCommandRow>>,
    built_in_submenu_rows: HashMap<i32, Vec<ContextCommandRow>>,
    active_column: Option<ColumnKind>,
    active_network_location: Option<u64>,
    active_quick_access_path: Option<PathBuf>,
    next_submenu_node: i32,
    active_submenu_token: Option<u64>,
    active_submenu_request: u64,
}

type SharedQuickMenu = Arc<Mutex<QuickMenuState>>;

fn selected_network_location_path(
    state: &WindowSessions,
    menu: &SharedQuickMenu,
) -> Option<PathBuf> {
    let id = menu.lock().ok()?.active_network_location?;
    let app = state.lock().ok()?;
    app.imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .find(|location| location.id == id)
        .and_then(|location| match &location.target {
            NetworkTarget::WindowsPath(path) => Some(path.clone()),
            NetworkTarget::ShellItemId(_) => None,
        })
}

fn selected_network_location_target(
    state: &WindowSessions,
    menu: &SharedQuickMenu,
) -> Option<NetworkTarget> {
    let id = menu.lock().ok()?.active_network_location?;
    let app = state.lock().ok()?;
    app.imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .find(|location| location.id == id)
        .map(|location| location.target.clone())
}

fn quick_menu_separator() -> ContextCommandRow {
    ContextCommandRow {
        id: -1,
        node_id: 0,
        label: "".into(),
        search_text: "".into(),
        hint: "".into(),
        enabled: false,
        separator: true,
        shell: true,
        checked: false,
        default: false,
        submenu: false,
        loading: false,
        placeholder: false,
        icon_kind: 0,
    }
}

fn quick_menu_placeholder() -> ContextCommandRow {
    ContextCommandRow {
        id: -1,
        node_id: 0,
        label: "".into(),
        search_text: "".into(),
        hint: "".into(),
        enabled: false,
        separator: false,
        shell: true,
        checked: false,
        default: false,
        submenu: false,
        loading: true,
        placeholder: true,
        icon_kind: 0,
    }
}

fn pending_shell_rows(rows: &[ContextCommandRow]) -> Vec<ContextCommandRow> {
    rows.iter()
        .cloned()
        .map(|mut row| {
            if !row.separator {
                row.enabled = false;
                row.loading = true;
            }
            row
        })
        .collect()
}

fn compose_quick_menu_rows(
    built_in_rows: &[ContextCommandRow],
    shell_rows: &[ContextCommandRow],
) -> Vec<ContextCommandRow> {
    let mut rows = built_in_rows.to_vec();
    if !rows.is_empty() && !shell_rows.is_empty() {
        rows.push(quick_menu_separator());
    }
    rows.extend(shell_rows.iter().cloned());
    filtered_context_rows(&rows, "")
}

fn project_cached_shell_rows(
    session_ready: bool,
    snapshot: Option<&QuickMenuSnapshot>,
) -> (Vec<ContextCommandRow>, bool, bool, bool) {
    let snapshot =
        snapshot.filter(|snapshot| snapshot.captured_at.elapsed() <= QUICK_MENU_SNAPSHOT_TTL);
    let cache_hit = snapshot.is_some();
    let session_hit = session_ready && cache_hit;
    let rows = match (session_hit, snapshot) {
        (true, Some(snapshot)) => snapshot.rows.clone(),
        (false, Some(snapshot)) => pending_shell_rows(&snapshot.rows),
        (_, None) => (0..QUICK_MENU_PLACEHOLDER_ROWS)
            .map(|_| quick_menu_placeholder())
            .collect(),
    };
    (rows, !session_hit, cache_hit, session_hit)
}

fn quick_menu_key(
    state: &WindowSessions,
    background: bool,
) -> Option<(
    QuickMenuKey,
    platform::windows::context_menu::ShellMenuLoadTarget,
)> {
    let app = state.lock().ok()?;
    let tab = app.active();
    let paths = if background {
        Vec::new()
    } else {
        selected_paths(&app)
    };
    let folder = tab.visible_path().map(Path::to_path_buf);
    if background && folder.is_none() || !background && paths.is_empty() {
        return None;
    }
    let key = QuickMenuKey {
        window_id: state.window_id,
        tab_id: tab.id,
        navigation_request: tab.latest_request,
        paths: paths.clone(),
        folder: folder.clone(),
    };
    let target = if background {
        platform::windows::context_menu::ShellMenuLoadTarget::Background(folder?)
    } else {
        platform::windows::context_menu::ShellMenuLoadTarget::Paths(paths)
    };
    Some((key, target))
}

fn quick_menu_key_is_current(state: &SharedSessions, key: &QuickMenuKey) -> bool {
    state.lock().ok().is_some_and(|app| {
        app.window_for_tab(key.tab_id) == Some(key.window_id)
            && app.tab(key.tab_id).is_some_and(|tab| {
                tab.latest_request == key.navigation_request
                    && tab.visible_path().map(Path::to_path_buf) == key.folder
                    && if key.paths.is_empty() {
                        tab.selected.is_empty()
                    } else {
                        key.paths
                            == tab
                                .selected
                                .iter()
                                .filter_map(|id| {
                                    tab.visible_entry(*id).map(|entry| entry.path.clone())
                                })
                                .collect::<Vec<_>>()
                    }
            })
    })
}
fn context_row_matches(row: &ContextCommandRow, query: &str) -> bool {
    if row.separator {
        return false;
    }
    let query = query.trim().to_lowercase();
    query.is_empty()
        || row.label.to_lowercase().contains(&query)
        || row.search_text.to_lowercase().contains(&query)
}

fn filtered_context_rows(rows: &[ContextCommandRow], query: &str) -> Vec<ContextCommandRow> {
    let mut result = Vec::new();
    let mut pending_separator = false;
    for row in rows {
        if row.separator {
            pending_separator = !result.is_empty();
            continue;
        }
        if !context_row_matches(row, query) {
            continue;
        }
        if pending_separator && !result.is_empty() {
            result.push(ContextCommandRow {
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
            });
        }
        pending_separator = false;
        result.push(row.clone());
    }
    result
}

fn first_enabled_context_index(rows: &[ContextCommandRow]) -> i32 {
    rows.iter()
        .position(|row| row.enabled && !row.separator)
        .map_or(-1, |index| index as i32)
}

fn next_enabled_context_index(rows: &[ContextCommandRow], current: i32, direction: i32) -> i32 {
    let enabled = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| (row.enabled && !row.separator).then_some(index))
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        return -1;
    }
    let position = enabled
        .iter()
        .position(|index| *index == current.max(0) as usize)
        .unwrap_or(0);
    let next = if direction < 0 {
        position.checked_sub(1).unwrap_or(enabled.len() - 1)
    } else {
        (position + 1) % enabled.len()
    };
    enabled[next] as i32
}

fn shell_menu_item_row(
    item: platform::windows::context_menu::ClassicMenuItem,
    node_id: i32,
) -> Option<ContextCommandRow> {
    use platform::windows::context_menu::ClassicMenuItemKind;
    if item
        .verb
        .as_deref()
        .is_some_and(|verb| matches!(verb, "cut" | "copy" | "paste" | "delete" | "rename"))
    {
        return None;
    }
    let separator = matches!(&item.kind, ClassicMenuItemKind::Separator);
    let submenu = matches!(&item.kind, ClassicMenuItemKind::Submenu { .. });
    Some(ContextCommandRow {
        id: item
            .command_id
            .map_or(-1, |id| SHELL_CONTEXT_COMMAND_BASE + id as i32),
        node_id,
        label: item.title.into(),
        search_text: item.verb.unwrap_or_default().into(),
        hint: "".into(),
        enabled: item.enabled && !separator && (submenu || item.command_id.is_some()),
        separator,
        shell: true,
        checked: item.checked,
        default: item.default,
        submenu,
        loading: false,
        placeholder: false,
        icon_kind: 0,
    })
}

fn project_shell_menu_items(
    menu: &mut QuickMenuState,
    items: Vec<platform::windows::context_menu::ClassicMenuItem>,
) -> Vec<ContextCommandRow> {
    use platform::windows::context_menu::ClassicMenuItemKind;
    items
        .into_iter()
        .filter_map(|item| {
            let node_id = if let ClassicMenuItemKind::Submenu { token, items } = &item.kind {
                menu.next_submenu_node = menu.next_submenu_node.saturating_add(1).max(1);
                menu.submenu_tokens.insert(menu.next_submenu_node, *token);
                if !items.is_empty() {
                    let rows = items
                        .iter()
                        .cloned()
                        .filter_map(|item| shell_menu_item_row(item, 0))
                        .collect();
                    menu.preloaded_submenu_rows.insert(*token, rows);
                }
                menu.next_submenu_node
            } else {
                0
            };
            shell_menu_item_row(item, node_id)
        })
        .collect()
}

fn project_context_submenu(ui: &AppWindow, menu: &SharedQuickMenu) {
    let rows = menu
        .lock()
        .map(|menu| menu.submenu_rows.clone())
        .unwrap_or_default();
    ui.set_context_submenu_active_index(first_enabled_context_index(&rows));
    ui.set_context_submenu_content_height(context_menu_content_height(&rows));
    ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
    if let Some(window_id) = window_id_for_ui(ui) {
        update_open_submenu_projection(window_id);
    }
}

fn context_menu_content_height(rows: &[ContextCommandRow]) -> f32 {
    rows.iter()
        .map(|row| if row.separator { 9.0 } else { 28.0 })
        .sum()
}

fn cached_submenu_rows(menu: &QuickMenuState, token: u64) -> Option<Vec<ContextCommandRow>> {
    menu.loaded_submenu_rows
        .get(&token)
        .or_else(|| menu.preloaded_submenu_rows.get(&token))
        .cloned()
}
fn submenu_result_matches(
    menu: &QuickMenuState,
    session_id: u64,
    request_id: u64,
    submenu_request_id: u64,
    token: u64,
) -> bool {
    menu.identity.as_ref().is_some_and(|identity| {
        identity.session_id == session_id && identity.request_id == request_id
    }) && menu.active_submenu_request == submenu_request_id
        && menu.active_submenu_token == Some(token)
}

fn submenu_request_is_duplicate(
    submenu_is_open: bool,
    active_token: Option<u64>,
    requested_token: u64,
) -> bool {
    submenu_is_open && active_token == Some(requested_token)
}

fn wire_context_submenu_hover(ui: &AppWindow) -> Rc<slint::Timer> {
    let timer = Rc::new(slint::Timer::default());
    let timer_for_hover = timer.clone();
    let weak = ui.as_weak();
    ui.on_hover_context_submenu(move |encoded_index| {
        timer_for_hover.stop();
        if encoded_index == i32::MIN + 1 {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_close_context_submenu();
            }
            return;
        }
        let weak = weak.clone();
        timer_for_hover.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(250),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let window_id = window_id_for_ui(&ui);
                    trace_quick_menu(
                        "quick_menu_hover_timer_fired",
                        format!(
                            "window={:?} encoded_index={} menu_open={}",
                            window_id.map(|window_id| window_id.0),
                            encoded_index,
                            ui.get_context_menu_open(),
                        ),
                    );
                    ui.invoke_open_context_submenu(encoded_index);
                    if let Some(window_id) = window_id {
                        let parent_depth = (encoded_index < 0).then(|| {
                            WINDOW_RUNTIMES.with_borrow(|runtimes| {
                                runtimes
                                    .get(&window_id)
                                    .and_then(|runtime| {
                                        runtime
                                            .quick_menu_popup
                                            .session
                                            .branches()
                                            .len()
                                            .checked_sub(2)
                                    })
                                    .unwrap_or(0)
                            })
                        });
                        open_quick_submenu_popup(
                            window_id,
                            ui.get_context_submenu_anchor_y(),
                            parent_depth,
                        );
                    }
                }
            },
        );
    });
    let timer_for_cancel = timer.clone();
    ui.on_cancel_context_submenu_hover(move || timer_for_cancel.stop());
    timer
}

fn wire_quick_menu_prewarm(
    ui: &AppWindow,
    state: WindowSessions,
    menu: SharedQuickMenu,
    worker: platform::windows::context_menu::ShellMenuWorker,
) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(350),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_context_menu_open() {
                return;
            }
            let background = state
                .lock()
                .map(|app| app.active().selected.is_empty())
                .unwrap_or(true);
            let Some((key, _)) = quick_menu_key(&state, background) else {
                return;
            };
            let already_loaded = menu.lock().ok().is_some_and(|menu| {
                menu.identity.as_ref().is_some_and(|identity| {
                    identity.key == key
                        && (!identity.ready
                            || menu.snapshots.get(&key).is_some_and(|snapshot| {
                                snapshot.captured_at.elapsed() <= QUICK_MENU_SNAPSHOT_TTL
                            }))
                })
            });
            if !already_loaded {
                begin_shell_menu_load(&ui, &state, &menu, &worker, background);
            }
        },
    );
    timer
}
fn project_filtered_context_menu(ui: &AppWindow, menu: &SharedQuickMenu, query: &str) {
    let rows = menu
        .lock()
        .map(|menu| filtered_context_rows(&menu.all_rows, query))
        .unwrap_or_default();
    ui.set_context_active_index(first_enabled_context_index(&rows));
    ui.set_context_menu_content_height(context_menu_content_height(&rows));
    ui.set_context_commands(ModelRc::new(VecModel::from(rows)));
    if let Some(window_id) = window_id_for_ui(ui) {
        update_root_popup_projection(window_id);
    }
}

fn begin_shell_menu_load(
    ui: &AppWindow,
    state: &WindowSessions,
    menu: &SharedQuickMenu,
    worker: &platform::windows::context_menu::ShellMenuWorker,
    background: bool,
) {
    let Some((key, target)) = quick_menu_key(state, background) else {
        ui.set_context_shell_loading(false);
        return;
    };
    let identity = {
        let mut menu = menu.lock().expect("quick menu mutex is not poisoned");
        if let Some(identity) = menu.identity.as_ref()
            && identity.key == key
        {
            let snapshot_is_fresh = menu
                .snapshots
                .get(&key)
                .is_some_and(|snapshot| snapshot.captured_at.elapsed() <= QUICK_MENU_SNAPSHOT_TTL);
            if !identity.ready || snapshot_is_fresh {
                ui.set_context_shell_loading(!identity.ready);
                return;
            }
        }
        menu.next_request = menu.next_request.wrapping_add(1).max(1);
        let request_id = menu.next_request;
        let session_id = (u64::from(key.window_id.0) << 32) | request_id;
        let identity = QuickMenuIdentity {
            session_id,
            request_id,
            key,
            ready: false,
        };
        menu.identity = Some(identity.clone());
        identity
    };
    if worker
        .send(platform::windows::context_menu::ShellMenuCommand::Load {
            session_id: identity.session_id,
            request_id: identity.request_id,
            target,
            include_extended_verbs: false,
            owner_window: native_window_handle(ui),
        })
        .is_err()
    {
        ui.set_context_shell_loading(false);
    } else {
        eprintln!(
            "{{\"event\":\"shell_menu_load_started\",\"session\":{},\"request\":{},\"window\":{},\"tab\":{},\"navigation_request\":{}}}",
            identity.session_id,
            identity.request_id,
            identity.key.window_id.0,
            identity.key.tab_id.0,
            identity.key.navigation_request.0,
        );
    }
}
fn quick_menu_row(
    id: i32,
    node_id: i32,
    label: &str,
    enabled: bool,
    checked: bool,
    submenu: bool,
) -> ContextCommandRow {
    ContextCommandRow {
        id,
        node_id,
        label: label.into(),
        enabled,
        separator: false,
        search_text: "".into(),
        hint: "".into(),
        shell: false,
        checked,
        default: false,
        submenu,
        loading: false,
        placeholder: false,
        icon_kind: built_in_menu_icon_kind(id, node_id),
    }
}

fn built_in_menu_icon_kind(id: i32, node_id: i32) -> i32 {
    match node_id {
        NODE_VIEW => 1,
        NODE_SORT => 2,
        NODE_GROUP => 3,
        NODE_COLUMNS => 6,
        _ => match id {
            CMD_REFRESH => 4,
            1 => 5,
            _ => 0,
        },
    }
}
fn built_in_context_rows(
    app: &AppState,
    can_paste: bool,
    background: bool,
) -> (Vec<ContextCommandRow>, HashMap<i32, Vec<ContextCommandRow>>) {
    let language = app.language;
    let selected = app.active().selected.len();
    let zh = |chinese: &'static str, english: &'static str| {
        if language == Language::Chinese {
            chinese
        } else {
            english
        }
    };
    let mut submenus = HashMap::new();
    let mut rows = Vec::new();
    if background {
        let view = app.active_view_mode();
        let (sort_field, sort_direction, group_field, group_direction) =
            if app.active().page_source == PageSource::Search {
                (
                    app.search_view.sort_field,
                    app.search_view.sort_direction,
                    GroupField::None,
                    SortDirection::Ascending,
                )
            } else {
                let preference = app
                    .active()
                    .visible_path()
                    .map(|path| app.directory_preference(path))
                    .unwrap_or(app.default_directory_view);
                (
                    preference.sort_field,
                    preference.sort_direction,
                    preference.group_field,
                    preference.group_direction,
                )
            };
        rows.extend([
            quick_menu_row(-1, NODE_VIEW, zh("查看", "View"), true, false, true),
            quick_menu_row(-1, NODE_SORT, zh("排序方式", "Sort by"), true, false, true),
            quick_menu_row(
                -1,
                NODE_GROUP,
                zh("分组依据", "Group by"),
                app.active().page_source != PageSource::Search,
                false,
                true,
            ),
            quick_menu_row(CMD_REFRESH, 0, zh("刷新", "Refresh"), true, false, false),
            quick_menu_separator(),
        ]);
        let view_labels = [
            (ViewMode::ExtraLargeIcons, "超大图标", "Extra large icons"),
            (ViewMode::LargeIcons, "大图标", "Large icons"),
            (ViewMode::MediumIcons, "中等图标", "Medium icons"),
            (ViewMode::SmallIcons, "小图标", "Small icons"),
            (ViewMode::List, "列表", "List"),
            (ViewMode::Details, "详细信息", "Details"),
            (ViewMode::Tiles, "平铺", "Tiles"),
            (ViewMode::Content, "内容", "Content"),
        ];
        submenus.insert(
            NODE_VIEW,
            view_labels
                .into_iter()
                .map(|(mode, chinese, english)| {
                    quick_menu_row(
                        CMD_VIEW_BASE + i32::from(mode.storage_code()),
                        0,
                        zh(chinese, english),
                        true,
                        view == mode,
                        false,
                    )
                })
                .collect(),
        );
        let sort_labels = [
            (SortField::Name, "名称", "Name"),
            (SortField::Modified, "修改时间", "Date modified"),
            (SortField::Created, "创建时间", "Date created"),
            (SortField::Kind, "类型", "Type"),
            (SortField::Size, "大小", "Size"),
        ];
        let mut sort_rows = sort_labels
            .into_iter()
            .map(|(field, chinese, english)| {
                quick_menu_row(
                    CMD_SORT_BASE + i32::from(field.storage_code()),
                    0,
                    zh(chinese, english),
                    true,
                    sort_field == field,
                    false,
                )
            })
            .collect::<Vec<_>>();
        sort_rows.push(quick_menu_separator());
        sort_rows.push(quick_menu_row(
            CMD_SORT_ASC,
            0,
            zh("升序", "Ascending"),
            true,
            sort_direction == SortDirection::Ascending,
            false,
        ));
        sort_rows.push(quick_menu_row(
            CMD_SORT_DESC,
            0,
            zh("降序", "Descending"),
            true,
            sort_direction == SortDirection::Descending,
            false,
        ));
        submenus.insert(NODE_SORT, sort_rows);
        let group_labels = [
            (GroupField::None, "无", "None"),
            (GroupField::Name, "名称", "Name"),
            (GroupField::Modified, "修改日期", "Date modified"),
            (GroupField::Created, "创建日期", "Date created"),
            (GroupField::Kind, "类型", "Type"),
            (GroupField::Size, "大小", "Size"),
        ];
        let enabled = app.active().page_source != PageSource::Search;
        let mut group_rows = group_labels
            .into_iter()
            .map(|(field, chinese, english)| {
                quick_menu_row(
                    CMD_GROUP_BASE + i32::from(field.storage_code()),
                    0,
                    zh(chinese, english),
                    enabled,
                    group_field == field,
                    false,
                )
            })
            .collect::<Vec<_>>();
        group_rows.push(quick_menu_separator());
        group_rows.push(quick_menu_row(
            CMD_GROUP_ASC,
            0,
            zh("分组升序", "Group ascending"),
            enabled,
            group_direction == SortDirection::Ascending,
            false,
        ));
        group_rows.push(quick_menu_row(
            CMD_GROUP_DESC,
            0,
            zh("分组降序", "Group descending"),
            enabled,
            group_direction == SortDirection::Descending,
            false,
        ));
        submenus.insert(NODE_GROUP, group_rows);
        rows.push(quick_menu_row(
            1,
            0,
            zh("新建文件夹", "New folder"),
            true,
            false,
            false,
        ));
        if app
            .active()
            .visible_path()
            .is_some_and(crate::network::is_unc_path)
        {
            rows.push(quick_menu_row(
                CMD_ADD_NETWORK_LOCATION,
                0,
                zh("添加到网络位置", "Add to network locations"),
                true,
                false,
                false,
            ));
        }
    } else {
        let selected_path = if selected == 1 {
            app.active()
                .selected
                .first()
                .and_then(|id| app.active().visible_entry(*id))
                .filter(|entry| entry.kind == crate::domain::EntryKind::Directory)
                .map(|entry| entry.path.clone())
        } else {
            None
        };
        if let Some(path) = selected_path {
            let pinned = platform::windows::quick_access::contains(&app.sidebar, &path);
            rows.push(quick_menu_row(
                if pinned {
                    CMD_QUICK_ACCESS_UNPIN
                } else {
                    CMD_QUICK_ACCESS_PIN
                },
                0,
                if pinned {
                    zh("从快速访问取消固定", "Unpin from Quick access")
                } else {
                    zh("固定到快速访问", "Pin to Quick access")
                },
                !app.quick_access_pending.contains(&path),
                false,
                false,
            ));
        }
        rows.push(quick_menu_row(
            2,
            0,
            zh("复制", "Copy"),
            selected > 0,
            false,
            false,
        ));
        rows.push(quick_menu_row(
            3,
            0,
            zh("剪切", "Cut"),
            selected > 0,
            false,
            false,
        ));
    }
    rows.push(quick_menu_row(
        4,
        0,
        zh("粘贴", "Paste"),
        can_paste,
        false,
        false,
    ));
    if !background {
        rows.push(quick_menu_row(
            5,
            0,
            zh("重命名", "Rename"),
            selected == 1,
            false,
            false,
        ));
        rows.push(quick_menu_row(
            6,
            0,
            zh("删除", "Delete"),
            selected > 0,
            false,
            false,
        ));
        rows.push(quick_menu_row(
            7,
            0,
            zh("永久删除", "Delete permanently"),
            selected > 0,
            false,
            false,
        ));
    }
    (rows, submenus)
}
fn project_context_menu(
    ui: &AppWindow,
    state: &WindowSessions,
    menu: &SharedQuickMenu,
    background: bool,
) {
    // A new pointer target must never inherit executable Shell command identities.
    if ui.get_context_menu_open() {
        ui.invoke_dismiss_context_menu();
    }
    let (built_in_rows, built_in_submenus) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        built_in_context_rows(&app, app.clipboard_has_files, background)
    };
    let key = quick_menu_key(state, background).map(|(key, _)| key);
    let (loading, cache_hit, session_hit) = menu
        .lock()
        .map(|mut menu| {
            let session_ready = key.as_ref().is_some_and(|key| {
                menu.identity
                    .as_ref()
                    .is_some_and(|identity| &identity.key == key && identity.ready)
            });
            let snapshot = key.as_ref().and_then(|key| menu.snapshots.get(key));
            let (shell_rows, loading, cache_hit, session_hit) =
                project_cached_shell_rows(session_ready, snapshot);
            menu.built_in_rows = built_in_rows.clone();
            menu.all_rows = compose_quick_menu_rows(&built_in_rows, &shell_rows);
            menu.submenu_rows.clear();
            menu.submenu_history.clear();
            if !session_ready {
                menu.submenu_tokens.clear();
                menu.preloaded_submenu_rows.clear();
                menu.loaded_submenu_rows.clear();
                menu.next_submenu_node = 0;
            }
            menu.built_in_submenu_rows = built_in_submenus.clone();
            menu.active_submenu_token = None;
            (loading, cache_hit, session_hit)
        })
        .unwrap_or_default();
    ui.set_context_search("".into());
    ui.set_context_shell_loading(loading);
    ui.set_context_shell_elapsed_ms(0);
    ui.set_context_submenu_open(false);
    ui.set_context_submenu_parent_open(false);
    ui.set_context_submenu_loading(false);
    ui.set_context_submenu_commands(ModelRc::new(
        VecModel::from(Vec::<ContextCommandRow>::new()),
    ));
    project_filtered_context_menu(ui, menu, "");
    ui.set_context_menu_open(true);
    if let Some(window_id) = window_id_for_ui(ui) {
        update_root_popup_projection(window_id);
        open_quick_menu_popup(
            window_id,
            ui.get_context_menu_anchor_x(),
            ui.get_context_menu_anchor_y(),
        );
    }
    eprintln!(
        "{{\"event\":\"quick_menu_projected\",\"background\":{},\"cache_hit\":{},\"session_hit\":{}}}",
        background, cache_hit, session_hit
    );
}
fn native_window_handle(ui: &AppWindow) -> isize {
    component_window_handle(ui)
}

fn component_window_handle<T: slint::ComponentHandle>(ui: &T) -> isize {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    ui.window()
        .with_winit_window(|window| {
            window
                .window_handle()
                .ok()
                .and_then(|handle| match handle.as_raw() {
                    RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

fn window_id_for_ui(ui: &AppWindow) -> Option<WindowId> {
    let hwnd = native_window_handle(ui);
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .find_map(|(id, runtime)| (native_window_handle(&runtime.ui) == hwnd).then_some(*id))
    })
}

fn trace_quick_menu(event: &str, detail: impl AsRef<str>) {
    platform::windows::window_trace::log_diagnostic(event, detail.as_ref());
}
fn popup_rows(rows: &[ContextCommandRow]) -> Vec<PopupCommandRow> {
    rows.iter()
        .map(|row| PopupCommandRow {
            label: row.label.clone(),
            hint: row.hint.clone(),
            enabled: row.enabled,
            separator: row.separator,
            checked: row.checked,
            default: row.default,
            submenu: row.submenu,
            loading: row.loading,
            placeholder: row.placeholder,
            icon_kind: row.icon_kind,
        })
        .collect()
}

fn create_quick_menu_popup(
    ui: &AppWindow,
    state: &WindowSessions,
) -> Result<QuickMenuPopupRuntime, slint::PlatformError> {
    let owner_hwnd = native_window_handle(ui);
    platform::windows::quick_menu_window::prepare_window(owner_hwnd);
    let root = match QuickMenuWindow::new() {
        Ok(root) => root,
        Err(error) => {
            platform::windows::quick_menu_window::cancel_prepared_window();
            return Err(error);
        }
    };
    root.set_dark_theme(state.lock().is_ok_and(|app| app.dark_theme()));
    Ok(QuickMenuPopupRuntime {
        root,
        branches: Vec::new(),
        session: Default::default(),
        owner_hwnd,
        client_anchor: crate::quick_menu_popup::PhysicalPoint::new(0, 0),
        work_area: crate::quick_menu_popup::PhysicalRect::new(0, 0, 1, 1),
        root_rect: None,
        next_generation: 0,
        next_branch: 0,
        shown_once: false,
        presentation: PopupPresentation::Hidden,
        cloak_generation: None,
    })
}

fn root_popup_height_for_content(content_height: f32, loading: bool, scale: f32) -> i32 {
    let loading_height = if loading { 20.0 } else { 0.0 };
    ((42.0 + content_height + loading_height) * scale)
        .ceil()
        .max(42.0) as i32
}

fn root_popup_height(ui: &AppWindow, scale: f32) -> i32 {
    root_popup_height_for_content(
        ui.get_context_menu_content_height(),
        ui.get_context_shell_loading(),
        scale,
    )
}

fn submenu_popup_height(ui: &AppWindow, scale: f32) -> i32 {
    ((8.0
        + ui.get_context_submenu_content_height()
        + if ui.get_context_submenu_loading() {
            20.0
        } else {
            0.0
        })
        * scale)
        .ceil()
        .max(28.0) as i32
}

fn context_row_anchor(rows: &[ContextCommandRow], index: i32, header_height: f32) -> f32 {
    if index < 0 {
        return header_height;
    }
    header_height
        + rows
            .iter()
            .take(index as usize)
            .map(|row| if row.separator { 9.0 } else { 28.0 })
            .sum::<f32>()
}

fn update_root_popup_projection(window_id: WindowId) {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let Some(runtime) = runtimes.get(&window_id) else {
            return;
        };
        let ui = &runtime.ui;
        let popup = &runtime.quick_menu_popup.root;
        let rows = (0..ui.get_context_commands().row_count())
            .filter_map(|index| ui.get_context_commands().row_data(index))
            .collect::<Vec<_>>();
        popup.set_rows(ModelRc::new(VecModel::from(popup_rows(&rows))));
        popup.set_content_height(ui.get_context_menu_content_height());
        popup.set_loading(ui.get_context_shell_loading());
        popup.set_search(ui.get_context_search());
        popup.set_active_index(ui.get_context_active_index());
        popup.set_dark_theme(ui.get_dark_theme());
        popup.set_search_text(ui.get_text_context_search());
        popup.set_loading_text(ui.get_text_context_loading());
        popup.set_empty_text(ui.get_text_context_empty());
    });
    resize_quick_menu_root_and_reposition_submenus(window_id);
}

fn submenu_slot_is_current(
    session: &crate::quick_menu_popup::QuickMenuPopupSession,
    depth: usize,
    event: Option<crate::quick_menu_popup::MenuEventIdentity>,
) -> bool {
    event.is_some_and(|event| {
        session.matches_event(event)
            && session.branches().get(depth + 1).map(|branch| branch.id) == Some(event.branch)
    })
}

fn replace_submenu_slot_rows_if_current(
    session: &crate::quick_menu_popup::QuickMenuPopupSession,
    depth: usize,
    event: Option<crate::quick_menu_popup::MenuEventIdentity>,
    slot_rows: &mut Vec<ContextCommandRow>,
    projected_rows: &[ContextCommandRow],
) -> bool {
    let current = submenu_slot_is_current(session, depth, event);
    if current {
        *slot_rows = projected_rows.to_vec();
    }
    current
}

fn submenu_projection_needs_presentation(
    presentation: PopupPresentation,
    cloak_event_pending: bool,
    loading: bool,
) -> bool {
    presentation == PopupPresentation::ShownCloaked && !cloak_event_pending && !loading
}

fn present_loaded_submenu_popup(
    window_id: WindowId,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        finish_submenu_popup_presentation(window_id, depth, event);
    });
}
fn update_open_submenu_projection(window_id: WindowId) {
    let projection = WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let runtime = runtimes.get_mut(&window_id)?;
        let ui = &runtime.ui;
        let rows = (0..ui.get_context_submenu_commands().row_count())
            .filter_map(|index| ui.get_context_submenu_commands().row_data(index))
            .collect::<Vec<_>>();
        let content_height = ui.get_context_submenu_content_height();
        let loading = ui.get_context_submenu_loading();
        let active_index = ui.get_context_submenu_active_index();
        let popup = &mut runtime.quick_menu_popup;
        let depth = popup.session.branches().len().checked_sub(2)?;
        let submenu = popup.branches.get_mut(depth)?;
        if !replace_submenu_slot_rows_if_current(
            &popup.session,
            depth,
            submenu.event,
            &mut submenu.rows,
            &rows,
        ) {
            return None;
        }
        let present_after_update = submenu_projection_needs_presentation(
            submenu.presentation,
            submenu.cloak_event.is_some(),
            loading,
        );
        Some((
            submenu.window.clone_strong(),
            submenu.event,
            depth,
            rows,
            content_height,
            loading,
            active_index,
            popup.work_area.height,
            present_after_update,
        ))
    });
    let Some((
        submenu,
        event,
        depth,
        rows,
        content_height,
        loading,
        active_index,
        work_area_height,
        present_after_update,
    )) = projection
    else {
        trace_quick_menu(
            "quick_menu_submenu_projection_skipped",
            format!("window={}", window_id.0),
        );
        return;
    };
    trace_quick_menu(
        "quick_menu_submenu_projected",
        format!(
            "window={} depth={} branch={:?} rows={} loading={} presentation_needed={}",
            window_id.0,
            depth,
            event.map(|event| event.branch.0),
            rows.len(),
            loading,
            present_after_update,
        ),
    );
    submenu.set_rows(ModelRc::new(VecModel::from(popup_rows(&rows))));
    submenu.set_content_height(content_height);
    submenu.set_loading(loading);
    submenu.set_active_index(active_index);
    let scale = submenu.window().scale_factor();
    let height = ((8.0 + content_height + if loading { 20.0 } else { 0.0 }) * scale)
        .ceil()
        .max(28.0) as i32;
    submenu.set_window_height(height.min(work_area_height.max(1)) as f32 / scale);
    reposition_quick_submenu_popups(window_id);
    if present_after_update && let Some(event) = event {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id)
                && current_slot_matches(&runtime.quick_menu_popup, depth, event)
                && let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth)
            {
                slot.cloak_event = Some(event);
            }
        });
        trace_quick_menu(
            "quick_menu_submenu_presentation_scheduled",
            format!(
                "window={} depth={} branch={}",
                window_id.0, depth, event.branch.0,
            ),
        );
        submenu.window().request_redraw();
        present_loaded_submenu_popup(window_id, depth, event);
    }
}

fn open_quick_menu_popup(window_id: WindowId, client_x: f32, client_y: f32) {
    let started_at = Instant::now();
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else { return };
        let popup = &mut runtime.quick_menu_popup;
        let ui = &runtime.ui;
        let owner_hwnd = native_window_handle(ui);
        if owner_hwnd == 0 {
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"owner_unavailable\"}}");
            return;
        }
        popup.owner_hwnd = owner_hwnd;
        close_quick_submenu_windows(popup);
        popup.next_branch = popup.next_branch.wrapping_add(1).max(1);
        let (tab_id, request_id) = runtime.sessions.lock().ok().map(|app| {
            let tab = app.window(window_id).expect("popup owner window exists").active_tab;
            (tab, app.tab(tab).expect("popup owner tab exists").latest_request)
        }).unwrap_or((TabId(0), RequestId(0)));
        let shell_generation = runtime
            ._quick_menu
            .lock()
            .ok()
            .and_then(|menu| menu.identity.as_ref().map(|identity| identity.request_id));
        popup.next_generation = shell_generation.unwrap_or_else(|| {
            popup.next_generation.wrapping_add(1).max(1)
        });
        let session = crate::quick_menu_popup::MenuSessionId {
            owner_window: window_id,
            tab_id,
            request_id,
            generation: popup.next_generation,
        };
        let root_branch = crate::quick_menu_popup::MenuBranchId(popup.next_branch);
        let scale = ui.window().scale_factor();
        let client = crate::quick_menu_popup::PhysicalPoint::new(
            (client_x * scale).round() as i32,
            (client_y * scale).round() as i32,
        );
        popup.client_anchor = client;
        let Ok(anchor) = platform::windows::quick_menu_window::client_point_to_screen(popup.owner_hwnd, client) else {
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"client_to_screen\"}}");
            return;
        };
        let Ok(work_area) = platform::windows::quick_menu_window::work_area_for_point(anchor) else {
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"work_area\"}}");
            return;
        };
        popup.work_area = work_area;
        let placement = crate::quick_menu_popup::place_root_popup(
            anchor,
            crate::quick_menu_popup::PhysicalSize::new(
                (320.0 * scale).ceil() as i32,
                root_popup_height(ui, scale),
            ),
            work_area,
        );
        popup.root_rect = Some(placement.rect);
        popup.root.set_window_height(placement.rect.height as f32 / scale);
        popup.root.window().set_position(slint::PhysicalPosition::new(placement.rect.x, placement.rect.y));
        let root_hwnd = component_window_handle(&popup.root);
        let owner_attached = platform::windows::quick_menu_window::attach_owner(
            root_hwnd,
            popup.owner_hwnd,
        )
        .is_ok();
        let cloaked = owner_attached
            && platform::windows::quick_menu_window::set_cloaked(root_hwnd, true).is_ok();
        if cloaked {
            popup.presentation = PopupPresentation::Cloaked;
        }
        let shown = cloaked && popup.root.show().is_ok();
        if !shown {
            let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
            let _ = popup.root.hide();
            popup.presentation = PopupPresentation::Hidden;
            popup.cloak_generation = None;
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"show_or_owner\"}}");
            return;
        }
        popup.presentation = PopupPresentation::ShownCloaked;
        popup.cloak_generation = Some(popup.next_generation);
        popup.session.open_root(session, root_branch);
        popup.root.window().request_redraw();
        let first_show = !popup.shown_once;
        popup.shown_once = true;
        eprintln!(
            "{{\"event\":\"quick_menu_popup_opened\",\"window\":{},\"generation\":{},\"first_show\":{},\"elapsed_us\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"horizontal_flipped\":{},\"vertical_flipped\":{},\"height_limited\":{}}}",
            window_id.0, popup.next_generation, first_show, started_at.elapsed().as_micros(),
            placement.rect.x, placement.rect.y,
            placement.rect.width, placement.rect.height, placement.horizontal_flipped,
            placement.vertical_flipped, placement.height_limited,
        );
    });
}

fn resize_quick_menu_root_and_reposition_submenus(window_id: WindowId) {
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        let Some(root_rect) = popup.root_rect else {
            return;
        };
        let scale = popup.root.window().scale_factor();
        let stable_rect = crate::quick_menu_popup::stable_root_size(
            root_rect,
            root_popup_height(&runtime.ui, scale),
            popup.work_area,
        );
        popup.root_rect = Some(stable_rect);
        popup
            .root
            .set_window_height(stable_rect.height as f32 / scale);
    });
    reposition_quick_submenu_popups(window_id);
}

fn reposition_quick_menu_root_and_submenus(window_id: WindowId) {
    let started_at = Instant::now();
    let root = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        if !popup.session.is_open() {
            return None;
        }
        let scale = popup.root.window().scale_factor();
        let anchor = platform::windows::quick_menu_window::client_point_to_screen(
            popup.owner_hwnd,
            popup.client_anchor,
        )
        .ok()?;
        let work_area = platform::windows::quick_menu_window::work_area_for_point(anchor).ok()?;
        let placement = crate::quick_menu_popup::place_root_popup(
            anchor,
            crate::quick_menu_popup::PhysicalSize::new(
                (320.0 * scale).ceil() as i32,
                root_popup_height(&runtime.ui, scale),
            ),
            work_area,
        );
        Some((placement, scale, work_area))
    });
    let Some((placement, scale, work_area)) = root else {
        return;
    };
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        popup.work_area = work_area;
        popup.root_rect = Some(placement.rect);
        popup
            .root
            .set_window_height(placement.rect.height as f32 / scale);
        popup
            .root
            .window()
            .set_position(slint::PhysicalPosition::new(
                placement.rect.x,
                placement.rect.y,
            ));
    });
    reposition_quick_submenu_popups(window_id);
    eprintln!(
        "{{\"event\":\"quick_menu_popup_repositioned\",\"window\":{},\"elapsed_us\":{},\"x\":{},\"y\":{}}}",
        window_id.0,
        started_at.elapsed().as_micros(),
        placement.rect.x,
        placement.rect.y,
    );
}
fn reposition_quick_submenu_popups(window_id: WindowId) {
    let started_at = Instant::now();
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        let Some(root_rect) = popup.root_rect else {
            return;
        };
        let work_area = popup.work_area;
        let mut parent_scale = popup.root.window().scale_factor();
        let requests = popup
            .branches
            .iter()
            .filter(|slot| slot.event.is_some())
            .map(|branch| {
                let branch_scale = branch.window.window().scale_factor();
                let request = crate::quick_menu_popup::SubmenuPlacementRequest {
                    anchor_y: (branch.anchor_y * parent_scale).round() as i32,
                    desired_size: crate::quick_menu_popup::PhysicalSize::new(
                        (280.0 * branch_scale).ceil() as i32,
                        (branch.window.get_window_height() * branch_scale).ceil() as i32,
                    ),
                };
                parent_scale = branch_scale;
                request
            })
            .collect::<Vec<_>>();
        let placements =
            crate::quick_menu_popup::place_submenu_chain(root_rect, &requests, work_area);
        for (branch, placement) in popup
            .branches
            .iter()
            .filter(|slot| slot.event.is_some())
            .zip(placements)
        {
            branch.window.window().set_position(slint::PhysicalPosition::new(
                placement.rect.x,
                placement.rect.y,
            ));
        }
        eprintln!(
            "{{\"event\":\"quick_menu_submenus_repositioned\",\"window\":{},\"elapsed_us\":{},\"root_x\":{},\"root_y\":{}}}",
            window_id.0,
            started_at.elapsed().as_micros(),
            root_rect.x,
            root_rect.y,
        );
    });
}

fn hide_quick_submenu_slots_from(popup: &mut QuickMenuPopupRuntime, depth: usize) {
    for slot in popup.branches.iter_mut().skip(depth) {
        let hwnd = component_window_handle(&slot.window);
        let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
        let _ = slot.window.hide();
        slot.event = None;
        slot.rows.clear();
        slot.cloak_event = None;
        slot.presentation = PopupPresentation::Hidden;
    }
}

fn close_quick_submenu_windows(popup: &mut QuickMenuPopupRuntime) {
    hide_quick_submenu_slots_from(popup, 0);
}

fn quick_menu_popup_owns_foreground(popup: &QuickMenuPopupRuntime) -> bool {
    let handles = std::iter::once(component_window_handle(&popup.root))
        .chain(
            popup
                .branches
                .iter()
                .filter(|slot| slot.event.is_some())
                .map(|branch| component_window_handle(&branch.window)),
        )
        .collect::<Vec<_>>();
    platform::windows::quick_menu_window::foreground_belongs_to(popup.owner_hwnd, &handles)
}

fn close_quick_menu_popup(window_id: WindowId, restore_focus: bool) {
    let started_at = Instant::now();
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        let foreground_owned = restore_focus && quick_menu_popup_owns_foreground(popup);
        let was_open = popup.session.close_all();
        close_quick_submenu_windows(popup);
        let root_hwnd = component_window_handle(&popup.root);
        let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
        let _ = popup.root.hide();
        popup.presentation = PopupPresentation::Hidden;
        popup.cloak_generation = None;
        popup.root_rect = None;
        let focus_restored = was_open && foreground_owned;
        if focus_restored {
            platform::windows::quick_menu_window::focus_window(popup.owner_hwnd);
        }
        if was_open {
            eprintln!(
                "{{\"event\":\"quick_menu_popup_closed\",\"window\":{},\"elapsed_us\":{},\"focus_restored\":{}}}",
                window_id.0,
                started_at.elapsed().as_micros(),
                focus_restored,
            );
        }
    });
}

fn open_quick_submenu_popup(window_id: WindowId, anchor_y: f32, parent_depth: Option<usize>) {
    open_quick_submenu_popup_attempt(window_id, anchor_y, parent_depth, 2);
}

fn open_quick_submenu_popup_attempt(
    window_id: WindowId,
    anchor_y: f32,
    parent_depth: Option<usize>,
    retries_remaining: u8,
) {
    let started_at = Instant::now();
    let mut retry_reason = None;
    let opened = WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let runtime = runtimes.get_mut(&window_id)?;
        let popup = &mut runtime.quick_menu_popup;
        let ui = &runtime.ui;
        if !popup.session.is_open() || !ui.get_context_submenu_open() {
            return None;
        }
        if popup.owner_hwnd == 0 || native_window_handle(ui) != popup.owner_hwnd {
            return None;
        }

        let depth = parent_depth.map_or(0, |depth| depth + 1);
        let session = popup.session.identity()?;
        let parent_branch = popup.session.branches().get(depth)?.id;
        let parent_event = crate::quick_menu_popup::MenuEventIdentity {
            session,
            branch: parent_branch,
        };
        if !popup.session.close_to_branch(parent_event) {
            return None;
        }
        let parent_rect = if depth == 0 {
            let position = popup.root.window().position();
            let size = popup.root.window().size();
            crate::quick_menu_popup::PhysicalRect::new(
                position.x,
                position.y,
                size.width as i32,
                size.height as i32,
            )
        } else {
            let parent = popup.branches.get(depth - 1)?;
            let position = parent.window.window().position();
            let size = parent.window.window().size();
            crate::quick_menu_popup::PhysicalRect::new(
                position.x,
                position.y,
                size.width as i32,
                size.height as i32,
            )
        };

        popup.next_branch = popup.next_branch.wrapping_add(1).max(1);
        let new_branch = crate::quick_menu_popup::MenuBranchId(popup.next_branch);
        if !popup.session.push_branch(parent_event, new_branch) {
            return None;
        }
        let event = crate::quick_menu_popup::MenuEventIdentity {
            session,
            branch: new_branch,
        };
        hide_quick_submenu_slots_from(popup, depth + 1);

        let scale = if depth == 0 {
            popup.root.window().scale_factor()
        } else {
            popup.branches[depth - 1].window.window().scale_factor()
        };
        let rows = (0..ui.get_context_submenu_commands().row_count())
            .filter_map(|index| ui.get_context_submenu_commands().row_data(index))
            .collect::<Vec<_>>();
        let effective_anchor_y = if anchor_y > 0.0 {
            anchor_y
        } else {
            context_row_anchor(&rows, ui.get_context_submenu_active_index(), 12.0)
        };
        let placement = crate::quick_menu_popup::place_submenu_popup_with_margins(
            crate::quick_menu_popup::PhysicalRect::new(
                parent_rect.x,
                parent_rect
                    .y
                    .saturating_add((effective_anchor_y * scale).round() as i32),
                parent_rect.width,
                parent_rect.height,
            ),
            crate::quick_menu_popup::PhysicalSize::new(
                (280.0 * scale).ceil() as i32,
                submenu_popup_height(ui, scale),
            ),
            popup.work_area,
            0,
            0,
        );

        if depth == popup.branches.len() {
            platform::windows::quick_menu_window::prepare_window(popup.owner_hwnd);
            let window = match QuickSubmenuWindow::new() {
                Ok(window) => window,
                Err(error) => {
                    platform::windows::quick_menu_window::cancel_prepared_window();
                    retry_reason = Some(format!("create_window:{error}"));
                    return None;
                }
            };
            wire_submenu_popup_callbacks(&window, window_id, depth);
            popup.branches.push(QuickSubmenuPopupRuntime {
                window,
                event: None,
                rows: Vec::new(),
                active_index: -1,
                anchor_y: 0.0,
                presentation: PopupPresentation::Hidden,
                cloak_event: None,
            });
        }
        let slot = popup.branches.get_mut(depth)?;
        let already_visible = slot.event.is_some();
        slot.event = Some(event);
        slot.rows = rows.clone();
        slot.anchor_y = effective_anchor_y;
        slot.cloak_event = None;
        slot.window
            .set_rows(ModelRc::new(VecModel::from(popup_rows(&rows))));
        slot.window
            .set_content_height(ui.get_context_submenu_content_height());
        slot.window.set_loading(ui.get_context_submenu_loading());
        slot.window
            .set_active_index(ui.get_context_submenu_active_index());
        slot.window.set_dark_theme(ui.get_dark_theme());
        slot.window.set_loading_text(ui.get_text_context_loading());
        slot.window.set_empty_text(ui.get_text_context_empty());
        slot.window
            .set_window_height(placement.rect.height as f32 / scale);
        slot.window
            .window()
            .set_position(slint::PhysicalPosition::new(
                placement.rect.x,
                placement.rect.y,
            ));

        let hwnd = component_window_handle(&slot.window);
        if let Err(error) =
            platform::windows::quick_menu_window::attach_owner(hwnd, popup.owner_hwnd)
        {
            retry_reason = Some(format!("attach_owner:{error}"));
            hide_quick_submenu_slots_from(popup, depth);
            let _ = popup.session.close_branch_and_descendants(event);
            return None;
        }
        let loading = ui.get_context_submenu_loading();
        if already_visible {
            if slot.presentation == PopupPresentation::Presented {
                slot.cloak_event = None;
            } else if loading {
                let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, true);
                slot.presentation = PopupPresentation::ShownCloaked;
                slot.cloak_event = None;
            } else {
                slot.presentation = PopupPresentation::ShownCloaked;
                slot.cloak_event = Some(event);
            }
            slot.window.window().request_redraw();
        } else {
            if let Err(error) = platform::windows::quick_menu_window::set_cloaked(hwnd, true) {
                retry_reason = Some(format!("cloak:{error}"));
                hide_quick_submenu_slots_from(popup, depth);
                let _ = popup.session.close_branch_and_descendants(event);
                return None;
            }
            slot.presentation = PopupPresentation::Cloaked;
            if let Err(error) = slot.window.show() {
                retry_reason = Some(format!("show:{error}"));
                let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
                hide_quick_submenu_slots_from(popup, depth);
                let _ = popup.session.close_branch_and_descendants(event);
                return None;
            }
            slot.presentation = PopupPresentation::ShownCloaked;
            slot.cloak_event = (!loading).then_some(event);
            slot.window.window().request_redraw();
        }
        Some((
            depth,
            event,
            already_visible,
            placement.horizontal_flipped,
            placement.height_limited,
        ))
    });

    let Some((depth, event, reused, horizontal_flipped, height_limited)) = opened else {
        if let Some(reason) = retry_reason {
            trace_quick_menu(
                "quick_menu_submenu_open_retry",
                format!(
                    "window={} parent_depth={:?} anchor_y={} retries_remaining={} reason={}",
                    window_id.0, parent_depth, anchor_y, retries_remaining, reason,
                ),
            );
            if retries_remaining > 0 {
                slint::Timer::single_shot(Duration::from_millis(16), move || {
                    open_quick_submenu_popup_attempt(
                        window_id,
                        anchor_y,
                        parent_depth,
                        retries_remaining - 1,
                    );
                });
                return;
            }
        }
        trace_quick_menu(
            "quick_menu_submenu_open_failed",
            format!(
                "window={} parent_depth={:?} anchor_y={}",
                window_id.0, parent_depth, anchor_y,
            ),
        );
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_close_context_submenu();
            }
        });
        return;
    };
    trace_quick_menu(
        "quick_menu_submenu_opened",
        format!(
            "window={} depth={} branch={} reused={} elapsed_us={} horizontal_flipped={} height_limited={}",
            window_id.0,
            depth,
            event.branch.0,
            reused,
            started_at.elapsed().as_micros(),
            horizontal_flipped,
            height_limited,
        ),
    );
    eprintln!(
        "{{\"event\":\"quick_menu_submenu_opened\",\"window\":{},\"depth\":{},\"branch\":{},\"reused\":{},\"elapsed_us\":{},\"horizontal_flipped\":{},\"height_limited\":{}}}",
        window_id.0,
        depth,
        event.branch.0,
        reused,
        started_at.elapsed().as_micros(),
        horizontal_flipped,
        height_limited,
    );
}

fn current_submenu_event(
    window_id: WindowId,
    depth: usize,
) -> Option<crate::quick_menu_popup::MenuEventIdentity> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        let event = popup.branches.get(depth)?.event?;
        (popup.session.matches_event(event)
            && popup
                .session
                .branches()
                .get(depth + 1)
                .map(|branch| branch.id)
                == Some(event.branch))
        .then_some(event)
    })
}

fn activate_submenu_slot_row(window_id: WindowId, depth: usize, index: i32) -> bool {
    if index < 0 {
        return false;
    }
    let row = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        let slot = popup.branches.get(depth)?;
        slot.event?;
        slot.rows.get(index as usize).cloned()
    });
    let Some(row) = row else {
        return false;
    };
    if !row.enabled || row.separator || row.loading || row.placeholder {
        return false;
    }
    if row.submenu {
        let can_open = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes.get(&window_id).is_some_and(|runtime| {
                depth + 2 >= runtime.quick_menu_popup.session.branches().len()
            })
        });
        if !can_open {
            return false;
        }
        if let Some(ui) = window_ui(window_id) {
            ui.invoke_cancel_context_submenu_hover();
            ui.set_context_submenu_active_index(index);
            ui.invoke_open_context_submenu(-index - 1);
        }
    } else {
        let event = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            let popup = &runtimes.get(&window_id)?.quick_menu_popup;
            let event = popup.branches.get(depth)?.event?;
            submenu_slot_is_current(&popup.session, depth, Some(event)).then_some(event)
        });
        let Some(event) = event else {
            return false;
        };
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id) {
                hide_quick_submenu_slots_from(&mut runtime.quick_menu_popup, depth + 1);
                let _ = runtime.quick_menu_popup.session.close_to_branch(event);
            }
        });
        if let Some(ui) = window_ui(window_id) {
            ui.set_context_menu_open(false);
            ui.invoke_invoke_context_command(row.id);
            if !row.shell {
                ui.invoke_dismiss_context_menu();
            }
        }
        let menu_closed = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .get(&window_id)
                .is_none_or(|runtime| !runtime.ui.get_context_menu_open())
        });
        if menu_closed {
            close_quick_menu_popup(window_id, true);
        }
    }
    row.submenu
}

fn open_submenu_slot_row(window_id: WindowId, depth: usize, index: i32) -> bool {
    if index < 0 {
        return false;
    }
    let prepared = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        if depth + 2 < popup.session.branches().len() {
            return None;
        }
        let row = popup.branches.get(depth)?.rows.get(index as usize)?.clone();
        (row.enabled && row.submenu && row.node_id > 0).then_some((
            row.node_id,
            popup.branches.get(depth)?.rows.clone(),
            popup.branches.get(depth)?.anchor_y,
        ))
    });
    let Some((node_id, parent_rows, parent_anchor_y)) = prepared else {
        return false;
    };
    let ui = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let known = runtime
            ._quick_menu
            .lock()
            .ok()
            .is_some_and(|menu| menu.submenu_tokens.contains_key(&node_id));
        known.then(|| runtime.ui.clone_strong())
    });
    let Some(ui) = ui else {
        return false;
    };
    let parent_height = context_menu_content_height(&parent_rows);
    ui.set_context_submenu_parent_commands(ModelRc::new(VecModel::from(parent_rows)));
    ui.set_context_submenu_parent_content_height(parent_height);
    ui.set_context_submenu_parent_anchor_y(parent_anchor_y);
    ui.set_context_submenu_active_index(index);
    ui.invoke_open_context_submenu(-index - 1);
    true
}

fn close_submenu_slot_and_descendants(
    window_id: WindowId,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        if current_slot_matches(popup, depth, event) {
            let foreground_owned = quick_menu_popup_owns_foreground(popup);
            hide_quick_submenu_slots_from(popup, depth);
            let _ = popup.session.close_branch_and_descendants(event);
            let focus_target = depth
                .checked_sub(1)
                .and_then(|parent_depth| popup.branches.get(parent_depth))
                .and_then(|slot| slot.event.map(|_| component_window_handle(&slot.window)))
                .unwrap_or_else(|| component_window_handle(&popup.root));
            if foreground_owned {
                platform::windows::quick_menu_window::focus_window(focus_target);
            }
        }
    });
}

fn current_slot_matches(
    popup: &QuickMenuPopupRuntime,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) -> bool {
    popup.branches.get(depth).and_then(|slot| slot.event) == Some(event)
        && popup.session.matches_event(event)
}

fn finish_submenu_popup_presentation(
    window_id: WindowId,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    let hwnd = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        let slot = popup.branches.get(depth)?;
        (slot.cloak_event == Some(event)
            && slot.presentation == PopupPresentation::ShownCloaked
            && current_slot_matches(popup, depth, event))
        .then(|| component_window_handle(&slot.window))
    });
    let Some(hwnd) = hwnd else {
        trace_quick_menu(
            "quick_menu_submenu_presentation_skipped",
            format!(
                "window={} depth={} branch={} reason=state_mismatch",
                window_id.0, depth, event.branch.0,
            ),
        );
        return;
    };
    let flush_result = platform::windows::quick_menu_window::flush_compositor();
    let uncloak_result = flush_result
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|_| {
            platform::windows::quick_menu_window::set_cloaked(hwnd, false)
                .map_err(|error| error.to_string())
        });
    let presented = uncloak_result.is_ok();
    trace_quick_menu(
        "quick_menu_submenu_presentation_finished",
        format!(
            "window={} depth={} branch={} hwnd={} presented={} error={:?}",
            window_id.0,
            depth,
            event.branch.0,
            hwnd,
            presented,
            uncloak_result.err(),
        ),
    );
    if presented {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id)
                && current_slot_matches(&runtime.quick_menu_popup, depth, event)
                && let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth)
            {
                slot.presentation = PopupPresentation::Presented;
                slot.cloak_event = None;
            }
        });
    } else {
        let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
        close_submenu_slot_and_descendants(window_id, depth, event);
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_close_context_submenu();
            }
        });
    }
}

fn wire_submenu_popup_callbacks(submenu: &QuickSubmenuWindow, window_id: WindowId, depth: usize) {
    let weak = submenu.as_weak();
    submenu.on_move(move |index| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        let Some(submenu) = weak.upgrade() else {
            return;
        };
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id) {
                let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth) else {
                    return;
                };
                slot.active_index = if index == -1 || index == 1 {
                    next_enabled_context_index(&slot.rows, slot.active_index, index)
                } else {
                    index
                };
                submenu.set_active_index(slot.active_index);
                if depth + 2 == runtime.quick_menu_popup.session.branches().len() {
                    runtime
                        .ui
                        .set_context_submenu_active_index(slot.active_index);
                }
            }
        });
    });
    submenu.on_activate(move |index| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        let _ = activate_submenu_slot_row(window_id, depth, index);
    });
    submenu.on_open_submenu(move |index, anchor_y| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        if let Some(ui) = window_ui(window_id) {
            ui.invoke_cancel_context_submenu_hover();
        }
        if open_submenu_slot_row(window_id, depth, index) {
            open_quick_submenu_popup(window_id, anchor_y, Some(depth));
        }
    });
    submenu.on_hover(move |index, anchor_y| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        if index >= 0 {
            WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
                if let Some(runtime) = runtimes.get_mut(&window_id)
                    && let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth)
                {
                    slot.active_index = index;
                    slot.window.set_active_index(index);
                }
            });
            let timer = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                runtimes
                    .get(&window_id)
                    .map(|runtime| runtime._quick_submenu_timer.clone())
            });
            if let Some(timer) = timer {
                timer.stop();
                let expected_event = current_submenu_event(window_id, depth);
                timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(250),
                    move || {
                        let target_is_current = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                            runtimes.get(&window_id).is_some_and(|runtime| {
                                runtime
                                    .quick_menu_popup
                                    .branches
                                    .get(depth)
                                    .is_some_and(|slot| {
                                        slot.event == expected_event && slot.active_index == index
                                    })
                            })
                        });
                        if target_is_current && open_submenu_slot_row(window_id, depth, index) {
                            open_quick_submenu_popup(window_id, anchor_y, Some(depth));
                        }
                    },
                );
            }
        } else {
            WINDOW_RUNTIMES.with_borrow(|runtimes| {
                if let Some(runtime) = runtimes.get(&window_id) {
                    runtime.ui.invoke_cancel_context_submenu_hover();
                }
            });
        }
    });
    submenu.on_cancel_hover(move || {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_cancel_context_submenu_hover();
            }
        });
    });
    submenu.on_dismiss(move || {
        let Some(event) = current_submenu_event(window_id, depth) else {
            return;
        };
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_close_context_submenu();
            }
        });
        close_submenu_slot_and_descendants(window_id, depth, event);
    });
    submenu.window().on_winit_window_event(move |_, event| {
        if matches!(event, winit::event::WindowEvent::RedrawRequested)
            && let Some(event_identity) = current_submenu_event(window_id, depth)
        {
            slint::Timer::single_shot(Duration::from_millis(16), move || {
                finish_submenu_popup_presentation(window_id, depth, event_identity);
            });
        }
        if matches!(event, winit::event::WindowEvent::Focused(false))
            && let Some(event_identity) = current_submenu_event(window_id, depth)
        {
            slint::Timer::single_shot(Duration::from_millis(1), move || {
                if current_submenu_event(window_id, depth) == Some(event_identity)
                    && !quick_menu_popup_has_focus(window_id)
                {
                    dismiss_quick_menu_session(window_id, false);
                }
            });
        }
        if current_submenu_event(window_id, depth).is_some()
            && matches!(event, winit::event::WindowEvent::ScaleFactorChanged { .. })
        {
            reposition_quick_submenu_popups(window_id);
        }
        if matches!(event, winit::event::WindowEvent::Destroyed)
            && let Some(event_identity) = current_submenu_event(window_id, depth)
        {
            close_submenu_slot_and_descendants(window_id, depth, event_identity);
        }
        EventResult::Propagate
    });
}
fn quick_menu_event_is_current(
    window_id: WindowId,
    event: crate::quick_menu_popup::MenuEventIdentity,
) -> bool {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .get(&window_id)
            .is_some_and(|runtime| runtime.quick_menu_popup.session.matches_event(event))
    })
}

fn wire_root_popup_callbacks(root: &QuickMenuWindow, window_id: WindowId) {
    let weak = root.as_weak();
    root.on_filter(move |query| {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id) {
                close_quick_submenu_windows(&mut runtime.quick_menu_popup);
                if let (Some(session), Some(root_branch)) = (
                    runtime.quick_menu_popup.session.identity(),
                    runtime
                        .quick_menu_popup
                        .session
                        .branches()
                        .first()
                        .map(|branch| branch.id),
                ) {
                    let _ = runtime.quick_menu_popup.session.close_to_branch(
                        crate::quick_menu_popup::MenuEventIdentity {
                            session,
                            branch: root_branch,
                        },
                    );
                }
            }
        });
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_filter_context_menu(query.clone());
                if let Some(root) = weak.upgrade() {
                    root.set_rows(ModelRc::new(VecModel::from(popup_rows(
                        &(0..runtime.ui.get_context_commands().row_count())
                            .filter_map(|index| runtime.ui.get_context_commands().row_data(index))
                            .collect::<Vec<_>>(),
                    ))));
                    root.set_content_height(runtime.ui.get_context_menu_content_height());
                    root.set_active_index(runtime.ui.get_context_active_index());
                }
            }
        });
    });
    let weak = root.as_weak();
    root.on_move(move |index| {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                if index == -1 || index == 1 {
                    runtime.ui.invoke_move_context_selection(index);
                } else {
                    runtime.ui.set_context_active_index(index);
                }
                if let Some(root) = weak.upgrade() {
                    root.set_active_index(runtime.ui.get_context_active_index());
                }
            }
        });
    });
    root.on_activate(move |index| {
        if let Some(ui) = window_ui(window_id) {
            ui.set_context_active_index(index);
            ui.invoke_activate_context_selection();
        }
        let menu_closed = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .get(&window_id)
                .is_none_or(|runtime| !runtime.ui.get_context_menu_open())
        });
        if menu_closed {
            close_quick_menu_popup(window_id, true);
        }
    });
    root.on_open_submenu(move |index, anchor_y| {
        if let Some(ui) = window_ui(window_id) {
            ui.invoke_cancel_context_submenu_hover();
            ui.set_context_active_index(index);
            ui.invoke_open_context_submenu(index);
        }
        open_quick_submenu_popup(window_id, anchor_y, None);
    });
    root.on_hover(move |index, anchor_y| {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                let row = (index >= 0)
                    .then(|| runtime.ui.get_context_commands().row_data(index as usize))
                    .flatten();
                trace_quick_menu(
                    "quick_menu_root_hover",
                    format!(
                        "window={} index={} anchor_y={} label={:?} submenu={} node={} enabled={}",
                        window_id.0,
                        index,
                        anchor_y,
                        row.as_ref().map(|row| row.label.as_str()),
                        row.as_ref().is_some_and(|row| row.submenu),
                        row.as_ref().map_or(0, |row| row.node_id),
                        row.as_ref().is_some_and(|row| row.enabled),
                    ),
                );
                runtime.ui.set_context_active_index(index.max(0));
                runtime.ui.set_context_submenu_anchor_y(anchor_y);
                runtime.ui.invoke_hover_context_submenu(index);
            }
        });
    });
    root.on_cancel_hover(move || {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_cancel_context_submenu_hover();
            }
        });
    });
    root.on_dismiss(move || dismiss_quick_menu_session(window_id, true));
    root.window().on_close_requested(move || {
        dismiss_quick_menu_session(window_id, true);
        slint::CloseRequestResponse::HideWindow
    });
    root.window().on_winit_window_event(move |_, event| {
        if matches!(event, winit::event::WindowEvent::RedrawRequested)
            && let Some(event_identity) = quick_menu_root_event(window_id)
        {
            slint::Timer::single_shot(Duration::from_millis(16), move || {
                finish_root_popup_presentation(window_id, event_identity);
            });
        }
        if matches!(event, winit::event::WindowEvent::Focused(false))
            && let Some(event_identity) = quick_menu_root_event(window_id)
        {
            slint::Timer::single_shot(Duration::from_millis(1), move || {
                if quick_menu_event_is_current(window_id, event_identity)
                    && !quick_menu_popup_has_focus(window_id)
                    && !window_belongs_to_quick_menu_owner(window_id)
                {
                    dismiss_quick_menu_session(window_id, false);
                }
            });
        }
        if matches!(event, winit::event::WindowEvent::ScaleFactorChanged { .. }) {
            reposition_quick_menu_root_and_submenus(window_id);
        }
        EventResult::Propagate
    });
}

fn window_belongs_to_quick_menu_owner(window_id: WindowId) -> bool {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let Some(runtime) = runtimes.get(&window_id) else {
            return false;
        };
        platform::windows::quick_menu_window::foreground_belongs_to(
            runtime.quick_menu_popup.owner_hwnd,
            &[native_window_handle(&runtime.ui)],
        )
    })
}

fn finish_root_popup_presentation(
    window_id: WindowId,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    let hwnd = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        (popup.cloak_generation == Some(event.session.generation)
            && popup.presentation == PopupPresentation::ShownCloaked
            && popup.session.matches_event(event))
        .then(|| component_window_handle(&popup.root))
    });
    let Some(hwnd) = hwnd else {
        return;
    };
    let presented = platform::windows::quick_menu_window::flush_compositor().is_ok()
        && platform::windows::quick_menu_window::set_cloaked(hwnd, false).is_ok();
    if presented {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id)
                && runtime.quick_menu_popup.session.matches_event(event)
            {
                runtime.quick_menu_popup.presentation = PopupPresentation::Presented;
                runtime.quick_menu_popup.cloak_generation = None;
            }
        });
    } else {
        let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
        dismiss_quick_menu_session(window_id, false);
    }
}
fn quick_menu_root_event(
    window_id: WindowId,
) -> Option<crate::quick_menu_popup::MenuEventIdentity> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        Some(crate::quick_menu_popup::MenuEventIdentity {
            session: popup.session.identity()?,
            branch: popup.session.branches().first()?.id,
        })
    })
}

fn quick_menu_popup_has_focus(window_id: WindowId) -> bool {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let Some(runtime) = runtimes.get(&window_id) else {
            return false;
        };
        let handles = std::iter::once(component_window_handle(&runtime.quick_menu_popup.root))
            .chain(
                runtime
                    .quick_menu_popup
                    .branches
                    .iter()
                    .filter(|slot| slot.event.is_some())
                    .map(|branch| component_window_handle(&branch.window)),
            )
            .collect::<Vec<_>>();
        platform::windows::quick_menu_window::foreground_belongs_to(
            runtime.quick_menu_popup.owner_hwnd,
            &handles,
        )
    })
}

fn dismiss_quick_menu_session(window_id: WindowId, restore_focus: bool) {
    let ui = window_ui(window_id);
    close_quick_menu_popup(window_id, restore_focus);
    if let Some(ui) = ui {
        if ui.get_context_menu_open() {
            ui.set_context_menu_open(false);
        }
        ui.invoke_dismiss_context_menu();
    }
}
fn wire_internal_drag_drop(
    ui: &AppWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    state: WindowSessions,
) {
    #[derive(Debug)]
    struct InternalDrag {
        entry_id: EntryId,
        origin_tab: TabId,
        paths: Vec<PathBuf>,
        source_directories: Vec<PathBuf>,
        start_x: f32,
        start_y: f32,
        outbound_started: bool,
        right_button: bool,
    }

    let drag = Arc::new(Mutex::new(None::<InternalDrag>));

    let state_for_begin = state.clone();
    let drag_for_begin = drag.clone();
    ui.on_begin_internal_drag(move |entry_id, x, y, _control, _shift, right_button| {
        let app = state_for_begin
            .lock()
            .expect("app state mutex is not poisoned");
        let id = EntryId(entry_id as u32);
        let origin_tab = app.active_window_state().active_tab;
        let paths = drag_paths_for_pressed_entry(&app, id);
        let source_directories = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect();
        if let Ok(mut drag) = drag_for_begin.lock() {
            *drag = Some(InternalDrag {
                entry_id: id,
                origin_tab,
                paths,
                source_directories,
                start_x: x,
                start_y: y,
                outbound_started: false,
                right_button,
            });
        }
    });

    let weak_for_update = ui.as_weak();
    let state_for_update = state.clone();
    let drag_for_update = drag.clone();
    let directory_for_update = directory_sender.clone();
    let network_directory_for_update = network_directory_sender.clone();
    ui.on_update_internal_drag(move |x, y| {
        let Some(ui) = weak_for_update.upgrade() else {
            return;
        };
        let outbound = drag_for_update.lock().ok().and_then(|mut drag| {
            let drag = drag.as_mut()?;
            let distance = ((x - drag.start_x).powi(2) + (y - drag.start_y).powi(2)).sqrt();
            if !should_release_internal_pointer_grab(distance, drag.outbound_started) {
                return None;
            }
            drag.outbound_started = true;
            Some((
                drag.paths.clone(),
                drag.source_directories.clone(),
                drag.right_button,
            ))
        });
        let Some((paths, source_directories, right_button)) = outbound else {
            return;
        };
        // OLE must own pointer routing before the window can receive its own native drop callbacks.
        ui.invoke_release_internal_drag_pointer();
        ui.set_drop_hover_entry_id(-1);
        eprintln!("drag-drop: threshold reached, entering DoDragDrop right_button={right_button}");
        let outbound_result = platform::windows::drag_drop::begin_outbound_drag(
            &paths,
            platform::windows::drag_drop::DropEffect::Move,
        );
        eprintln!("drag-drop: DoDragDrop returned result={outbound_result:?}");
        match outbound_result {
            Ok(result) if should_refresh_outbound_drag_source(result) => {
                // External targets own the operation; refresh only the source views and never infer item removal.
                refresh_affected_tabs(
                    &directory_for_update,
                    &network_directory_for_update,
                    &state_for_update,
                    &source_directories,
                );
            }
            Ok(_) => {}
            Err(error) => {
                if let Ok(mut app) = state_for_update.lock() {
                    app.operation_errors
                        .push(format!("outbound drag failed: {error}"));
                }
            }
        }
    });
    let weak_for_end = ui.as_weak();
    let state_for_end = state.clone();
    let drag_for_end = drag.clone();
    ui.on_end_internal_drag(move |x, y, control, shift, right_button| {
        let Some(ui) = weak_for_end.upgrade() else {
            return;
        };
        ui.set_drop_hover_entry_id(-1);
        let Some(drag) = drag_for_end.lock().ok().and_then(|mut drag| drag.take()) else {
            return;
        };
        if drag.outbound_started {
            return;
        }
        let distance = ((x - drag.start_x).powi(2) + (y - drag.start_y).powi(2)).sqrt();
        if distance < 4.0 {
            if right_button {
                ui.invoke_show_entry_menu(drag.entry_id.0 as i32, x, y);
            }
            return;
        }
        let target = state_for_end.lock().ok().and_then(|app| {
            internal_drag_target(
                &app,
                x,
                y,
                FileHitGeometry {
                    list_left: ui.get_file_list_left(),
                    list_top: ui.get_file_list_top(),
                    viewport_x: ui.get_file_viewport_x(),
                    viewport_y: ui.get_file_viewport_y(),
                    viewport_width: ui.get_file_viewport_width(),
                    columns_width: ui.get_details_hit_width(),
                },
                ui.get_search_scroll_y(),
                ui.get_grid_column_count().max(1) as usize,
            )
            .map(|(_, path)| path)
        });
        let Some(target) = target else {
            return;
        };
        let key_state = (if control { 8 } else { 0 })
            | (if shift { 4 } else { 0 })
            | (if right_button { 2 } else { 0 });
        let (effect, reason) =
            platform::windows::drag_drop::negotiate_effect(&drag.paths, Some(&target), key_state);
        if reason.is_some() {
            return;
        }
        let intent = platform::windows::drag_drop::DropIntent {
            paths: drag.paths,
            target: platform::windows::drag_drop::DropTarget::Directory(target),
            effect,
            right_button,
            screen_x: x.round() as i32,
            screen_y: y.round() as i32,
            allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                | platform::windows::drag_drop::ALLOW_MOVE
                | platform::windows::drag_drop::ALLOW_LINK,
        };
        if right_button {
            ui.set_drop_can_copy(true);
            ui.set_drop_can_move(true);
            ui.set_drop_can_link(true);
            if let Ok(mut app) = state_for_end.lock() {
                app.pending_right_drops
                    .insert(state_for_end.window_id, (drag.origin_tab, intent));
            }
            ui.invoke_show_drop_menu(x, y);
        } else {
            let origin_tab = state_for_end.lock().ok().and_then(|app| {
                app.window(state_for_end.window_id)
                    .map(|window| window.active_tab)
            });
            let Some(origin_tab) = origin_tab else {
                return;
            };
            dispatch_drop_operation(
                intent,
                origin_tab,
                state_for_end.shared.clone(),
                operation_sender.clone(),
            );
        }
    });
}

fn wire_address_drag(ui: &AppWindow, state: WindowSessions) {
    let start = Arc::new(Mutex::new(None::<(f32, f32, PathBuf)>));
    let state_for_begin = state.clone();
    let start_for_begin = start.clone();
    ui.on_begin_address_drag(move |x, y| {
        let path = state_for_begin.lock().ok().and_then(|app| {
            let tab = app.active();
            (tab.kind == TabKind::Files
                && tab.page_source == PageSource::Directory
                && tab.load_state == LoadState::Complete)
                .then(|| tab.current_path.clone())
                .flatten()
        });
        if let Some(path) = path
            && let Ok(mut drag) = start_for_begin.lock()
        {
            *drag = Some((x, y, path));
        }
    });
    let weak = ui.as_weak();
    let start_for_update = start.clone();
    let shared = state.shared.clone();
    ui.on_update_address_drag(move |x, y| {
        let path = start_for_update.lock().ok().and_then(|mut drag| {
            let (start_x, start_y, path) = drag.as_ref()?.clone();
            if ((x - start_x).powi(2) + (y - start_y).powi(2)).sqrt() < 4.0 {
                return None;
            }
            *drag = None;
            Some(path)
        });
        let Some(path) = path else { return };
        if let Some(ui) = weak.upgrade() {
            ui.invoke_release_internal_drag_pointer();
        }
        match platform::windows::drag_drop::begin_outbound_drag(
            &[path],
            platform::windows::drag_drop::DropEffect::Link,
        ) {
            Ok(_) => {}
            Err(error) => {
                if let Ok(mut app) = shared.lock() {
                    app.operation_errors
                        .push(format!("address drag failed: {error}"));
                }
            }
        }
    });
    ui.on_end_address_drag(move || {
        if let Ok(mut drag) = start.lock() {
            *drag = None;
        }
    });
}

const RECTANGLE_SELECTION_THRESHOLD: f32 = 5.0;
const RECTANGLE_SELECTION_EDGE: f32 = 20.0;
const RECTANGLE_SELECTION_MAX_SCROLL: f32 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectionRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl SelectionRect {
    fn from_points(start_x: f32, start_y: f32, end_x: f32, end_y: f32) -> Self {
        Self {
            left: start_x.min(end_x),
            top: start_y.min(end_y),
            right: start_x.max(end_x),
            bottom: start_y.max(end_y),
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.left <= other.right
            && self.right >= other.left
            && self.top <= other.bottom
            && self.bottom >= other.top
    }
}

#[derive(Debug)]
struct RectangleSelectionGesture {
    tab_id: TabId,
    request_id: RequestId,
    start_x: f32,
    start_content_y: f32,
    pointer_x: f32,
    pointer_y: f32,
    snapshot: Arc<HashSet<EntryId>>,
    snapshot_order: Vec<EntryId>,
    focused: Option<EntryId>,
    anchor: Option<EntryId>,
    mode: RectangleSelectionMode,
    active: bool,
    dirty: bool,
    committed: bool,
    last_hits: HashSet<EntryId>,
}

fn rectangle_selection_started(start_x: f32, start_y: f32, x: f32, y: f32) -> bool {
    (x - start_x).hypot(y - start_y) >= RECTANGLE_SELECTION_THRESHOLD
}

fn rectangle_selection_scroll_delta(pointer_y: f32, top: f32, bottom: f32) -> f32 {
    if pointer_y < top + RECTANGLE_SELECTION_EDGE {
        (top + RECTANGLE_SELECTION_EDGE - pointer_y).min(RECTANGLE_SELECTION_MAX_SCROLL)
    } else if pointer_y > bottom - RECTANGLE_SELECTION_EDGE {
        -(pointer_y - (bottom - RECTANGLE_SELECTION_EDGE)).min(RECTANGLE_SELECTION_MAX_SCROLL)
    } else {
        0.0
    }
}

fn rectangle_selection_hits(
    tab: &TabSession,
    view_mode: ViewMode,
    grid_columns: usize,
    viewport_width: f32,
    details_viewport_x: f32,
    details_columns_width: f32,
    rect: SelectionRect,
) -> HashSet<EntryId> {
    let geometry = file_layout_geometry(view_mode);
    let (row_height, card_width, card_height, gap) = (
        geometry.row_height,
        geometry.card_width,
        geometry.card_height,
        if geometry.grid { 8.0 } else { 0.0 },
    );
    let slot_count = if tab.page_source == PageSource::Search {
        tab.search_total.unwrap_or(0) as usize
    } else {
        tab.visible_entries().len()
    };
    if slot_count == 0 || rect.bottom < 0.0 || rect.right < 0.0 {
        return HashSet::new();
    }
    let entry_at = |slot: usize| {
        if tab.page_source == PageSource::Search {
            u32::try_from(slot)
                .ok()
                .and_then(|slot| slot.checked_add(1))
                .and_then(|id| tab.visible_entry(EntryId(id)))
        } else {
            tab.visible_entries().get(slot)
        }
    };
    let grid_content_left = 16.0;
    let candidate_start = |position: f32, extent: f32| {
        ((position.max(0.0) / extent).floor() as usize).saturating_sub(1)
    };
    let mut hits = HashSet::new();
    if !geometry.grid {
        let (item_left, item_right) = if view_mode == ViewMode::Details {
            FileHitGeometry {
                viewport_x: details_viewport_x,
                viewport_width,
                columns_width: details_columns_width,
                ..FileHitGeometry::default()
            }
            .details_range()
        } else {
            (16.0, (viewport_width - 16.0).max(16.0))
        };
        if item_right <= item_left || rect.right < item_left || rect.left >= item_right {
            return hits;
        }
        let first = candidate_start(rect.top, row_height);
        let last = ((rect.bottom.max(0.0) / row_height).floor() as usize)
            .min(slot_count.saturating_sub(1));
        for slot in first..=last {
            let Some(entry) = entry_at(slot) else {
                continue;
            };
            let item = SelectionRect {
                left: item_left,
                top: slot as f32 * row_height,
                right: item_right,
                bottom: (slot + 1) as f32 * row_height,
            };
            if rect.intersects(item) {
                hits.insert(entry.id);
            }
        }
    } else {
        let columns = grid_columns.max(1);
        let column_extent = card_width + gap;
        let first_column =
            candidate_start(rect.left - grid_content_left, column_extent).min(columns - 1);
        let last_column = (((rect.right - grid_content_left).max(0.0) / column_extent).floor()
            as usize)
            .min(columns - 1);
        let first_row = candidate_start(rect.top, row_height);
        let last_row = ((rect.bottom.max(0.0) / row_height).floor() as usize)
            .min(slot_count.saturating_sub(1) / columns);
        for row in first_row..=last_row {
            for column in first_column..=last_column {
                let slot = row * columns + column;
                if slot >= slot_count {
                    break;
                }
                let Some(entry) = entry_at(slot) else {
                    continue;
                };
                let left = grid_content_left + column as f32 * column_extent;
                let top = row as f32 * row_height;
                let item = SelectionRect {
                    left,
                    top,
                    right: left + card_width,
                    bottom: top + card_height,
                };
                if rect.intersects(item) {
                    hits.insert(entry.id);
                }
            }
        }
    }
    hits
}

#[allow(clippy::too_many_arguments)]
fn rectangle_selection_hits_for_app(
    app: &AppState,
    tab_id: TabId,
    view_mode: ViewMode,
    grid_columns: usize,
    viewport_width: f32,
    details_viewport_x: f32,
    details_columns_width: f32,
    rect: SelectionRect,
) -> HashSet<EntryId> {
    let Some(tab) = app.tab(tab_id) else {
        return HashSet::new();
    };
    let group_field = tab
        .visible_path()
        .map(|path| app.directory_preference(path).group_field)
        .unwrap_or(GroupField::None);
    if tab.page_source == PageSource::Search || group_field == GroupField::None {
        return rectangle_selection_hits(
            tab,
            view_mode,
            grid_columns,
            viewport_width,
            details_viewport_x,
            details_columns_width,
            rect,
        );
    }
    let groups = directory_group_projections(app, tab, tab.visible_entries());
    let geometry = file_layout_geometry(view_mode);
    let mut hits = HashSet::new();
    if geometry.grid {
        let projection =
            IconProjection::from_groups(&groups, grid_columns, 32, geometry.row_height as u64);
        for entry in tab.visible_entries() {
            let Some(position) = projection.entry_position(entry.id) else {
                continue;
            };
            let Some(top) = projection.offsets.row_start(position.row_index) else {
                continue;
            };
            let left = 16.0 + position.column_index as f32 * (geometry.card_width + 8.0);
            if rect.intersects(SelectionRect {
                left,
                top: top as f32,
                right: left + geometry.card_width,
                bottom: top as f32 + geometry.card_height,
            }) {
                hits.insert(entry.id);
            }
        }
    } else {
        let (left, right) = if view_mode == ViewMode::Details {
            FileHitGeometry {
                viewport_x: details_viewport_x,
                viewport_width,
                columns_width: details_columns_width,
                ..FileHitGeometry::default()
            }
            .details_range()
        } else {
            (16.0, (viewport_width - 16.0).max(16.0))
        };
        if right <= left {
            return hits;
        }
        let projection = ListProjection::from_groups(&groups, 32, geometry.row_height as u64);
        for entry in tab.visible_entries() {
            let Some(position) = projection.entry_position(entry.id) else {
                continue;
            };
            let Some(top) = projection.offsets.row_start(position) else {
                continue;
            };
            if rect.intersects(SelectionRect {
                left,
                top: top as f32,
                right,
                bottom: top as f32 + geometry.row_height,
            }) {
                hits.insert(entry.id);
            }
        }
    }
    hits
}
fn selection_mode(control: bool, shift: bool) -> RectangleSelectionMode {
    if control {
        RectangleSelectionMode::Toggle
    } else if shift {
        RectangleSelectionMode::Extend
    } else {
        RectangleSelectionMode::Replace
    }
}

fn finish_rectangle_selection(
    ui: &AppWindow,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
    timer: &slint::Timer,
) {
    timer.stop();
    let had_gesture = gesture
        .lock()
        .is_ok_and(|mut gesture| gesture.take().is_some());
    if had_gesture || ui.get_rectangle_selection_visible() {
        ui.invoke_clear_rectangle_selection();
    }
}

fn restore_rectangle_selection_snapshot(
    tab: &mut TabSession,
    snapshot: &[EntryId],
    focused: Option<EntryId>,
    anchor: Option<EntryId>,
) {
    tab.selected = snapshot
        .iter()
        .copied()
        .filter(|id| tab.visible_entry(*id).is_some())
        .collect();
    tab.focused = focused.filter(|id| tab.visible_entry(*id).is_some());
    tab.selection_anchor = anchor.filter(|id| tab.visible_entry(*id).is_some());
}

fn cancel_rectangle_selection(
    ui: &AppWindow,
    state: &WindowSessions,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
    timer: &slint::Timer,
) {
    let snapshot = gesture.lock().ok().and_then(|gesture| {
        gesture.as_ref().map(|gesture| {
            (
                gesture.tab_id,
                gesture.request_id,
                gesture.snapshot_order.clone(),
                gesture.focused,
                gesture.anchor,
            )
        })
    });
    let Some((tab_id, request_id, snapshot, focused, anchor)) = snapshot else {
        finish_rectangle_selection(ui, gesture, timer);
        return;
    };
    let update = {
        let Ok(mut app) = state.lock() else {
            finish_rectangle_selection(ui, gesture, timer);
            return;
        };
        if app.active_window_state().active_tab != tab_id {
            None
        } else {
            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            if tab.latest_request != request_id {
                None
            } else {
                let before = selection_projection_ids(tab);
                restore_rectangle_selection_snapshot(tab, snapshot.as_ref(), focused, anchor);
                let mut changed = before;
                changed.extend(selection_projection_ids(tab));
                Some((tab_id, changed))
            }
        }
    };
    if let Some((tab_id, changed)) = update {
        update_file_rows(ui, state, tab_id, &changed);
        update_selection_status(ui, state);
    }
    finish_rectangle_selection(ui, gesture, timer);
}

fn project_rectangle_selection_visual(
    ui: &AppWindow,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
) -> bool {
    let (start_x, start_content_y, pointer_x, pointer_y) = {
        let Ok(mut gesture) = gesture.lock() else {
            return false;
        };
        let Some(gesture) = gesture.as_mut() else {
            return false;
        };
        if !gesture.active
            && !rectangle_selection_started(
                gesture.start_x,
                gesture.start_content_y + ui.get_file_viewport_y(),
                gesture.pointer_x,
                gesture.pointer_y,
            )
        {
            return false;
        }
        gesture.active = true;
        (
            gesture.start_x,
            gesture.start_content_y,
            gesture.pointer_x,
            gesture.pointer_y,
        )
    };
    let current_content_y = pointer_y - ui.get_file_viewport_y();
    let rect = SelectionRect::from_points(start_x, start_content_y, pointer_x, current_content_y);
    let viewport_top = ui.get_rectangle_viewport_top();
    let viewport_bottom = viewport_top + ui.get_file_viewport_height();
    let visual_top = (rect.top + ui.get_file_viewport_y()).max(viewport_top);
    let visual_bottom = (rect.bottom + ui.get_file_viewport_y()).min(viewport_bottom);
    ui.invoke_show_rectangle_selection(
        rect.left.max(0.0),
        visual_top,
        (rect.right.min(ui.get_file_viewport_width()) - rect.left.max(0.0)).max(0.0),
        (visual_bottom - visual_top).max(0.0),
    );
    true
}

fn update_rectangle_selection(
    ui: &AppWindow,
    state: &WindowSessions,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
    timer: &slint::Timer,
) -> bool {
    let (
        tab_id,
        request_id,
        snapshot,
        mode,
        start_x,
        start_content_y,
        pointer_x,
        pointer_y,
        previous_hits,
        committed,
    ) = {
        let Ok(mut gesture) = gesture.lock() else {
            return false;
        };
        let Some(gesture) = gesture.as_mut() else {
            return false;
        };
        if !gesture.active || !gesture.dirty {
            return false;
        }
        gesture.dirty = false;
        (
            gesture.tab_id,
            gesture.request_id,
            gesture.snapshot.clone(),
            gesture.mode,
            gesture.start_x,
            gesture.start_content_y,
            gesture.pointer_x,
            gesture.pointer_y,
            gesture.last_hits.clone(),
            gesture.committed,
        )
    };
    let current_content_y = pointer_y - ui.get_file_viewport_y();
    let rect = SelectionRect::from_points(start_x, start_content_y, pointer_x, current_content_y);
    let update = {
        let Ok(mut app) = state.lock() else {
            return false;
        };
        if app.active_window_state().active_tab != tab_id {
            None
        } else {
            let view_mode = app.active_view_mode();
            let grid_columns = ui.get_grid_column_count().max(1) as usize;
            let viewport_width = ui.get_file_viewport_width();

            let hits = rectangle_selection_hits_for_app(
                &app,
                tab_id,
                view_mode,
                grid_columns,
                viewport_width,
                ui.get_file_viewport_x(),
                ui.get_details_hit_width(),
                rect,
            );
            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            if tab.latest_request != request_id {
                None
            } else {
                let previous_focus = tab.focused;
                if committed && previous_hits == hits {
                    return false;
                }
                tab.apply_rectangle_selection(snapshot.as_ref(), &hits, mode);
                let mut changed = if committed {
                    previous_hits
                        .symmetric_difference(&hits)
                        .copied()
                        .collect::<HashSet<_>>()
                } else {
                    match mode {
                        RectangleSelectionMode::Replace => snapshot.union(&hits).copied().collect(),
                        RectangleSelectionMode::Extend => {
                            hits.difference(snapshot.as_ref()).copied().collect()
                        }
                        RectangleSelectionMode::Toggle => hits.clone(),
                    }
                };
                changed.extend(previous_focus);
                changed.extend(tab.focused);
                Some((tab_id, changed, hits))
            }
        }
    };
    let Some(update) = update else {
        finish_rectangle_selection(ui, gesture, timer);
        return false;
    };
    if let Ok(mut gesture) = gesture.lock()
        && let Some(gesture) = gesture.as_mut()
    {
        gesture.last_hits = update.2;
        gesture.committed = true;
    }
    update_file_rows(ui, state, update.0, &update.1);
    update_selection_status(ui, state);
    true
}

fn wire_rectangle_selection(ui: &AppWindow, state: WindowSessions) -> Rc<slint::Timer> {
    let gesture = Arc::new(Mutex::new(None::<RectangleSelectionGesture>));
    let timer = Rc::new(slint::Timer::default());
    let gesture_for_begin = gesture.clone();
    let timer_for_begin = timer.clone();
    let state_for_begin = state.clone();
    let weak_for_begin = ui.as_weak();
    ui.on_begin_rectangle_selection(move |x, y, control, shift| {
        let Some(ui) = weak_for_begin.upgrade() else {
            return;
        };
        let Ok(app) = state_for_begin.lock() else {
            return;
        };
        let tab = app.active();
        if tab.kind != TabKind::Files {
            return;
        }
        if let Ok(mut gesture) = gesture_for_begin.lock() {
            *gesture = Some(RectangleSelectionGesture {
                tab_id: tab.id,
                request_id: tab.latest_request,
                start_x: x,
                start_content_y: y - ui.get_file_viewport_y(),
                pointer_x: x,
                pointer_y: y,
                snapshot: Arc::new(tab.selected.iter().copied().collect()),
                snapshot_order: tab.selected.clone(),
                focused: tab.focused,
                anchor: tab.selection_anchor,
                mode: selection_mode(control, shift),
                active: false,
                dirty: false,
                committed: false,
                last_hits: HashSet::new(),
            });
            timer_for_begin.restart();
        }
    });

    let gesture_for_update = gesture.clone();
    let weak_for_update = ui.as_weak();
    ui.on_update_rectangle_selection(move |x, y| {
        let Some(ui) = weak_for_update.upgrade() else {
            return;
        };
        if let Ok(mut gesture) = gesture_for_update.lock()
            && let Some(gesture) = gesture.as_mut()
        {
            gesture.pointer_x = x;
            gesture.pointer_y = y;
            gesture.dirty = true;
        }
        project_rectangle_selection_visual(&ui, &gesture_for_update);
    });

    let gesture_for_end = gesture.clone();
    let timer_for_end = timer.clone();
    let state_for_end = state.clone();
    let weak_for_end = ui.as_weak();
    ui.on_end_rectangle_selection(move || {
        let Some(ui) = weak_for_end.upgrade() else {
            return;
        };
        let active = gesture_for_end
            .lock()
            .ok()
            .and_then(|gesture| gesture.as_ref().map(|gesture| gesture.active))
            .unwrap_or(false);
        if active {
            if let Ok(mut gesture) = gesture_for_end.lock()
                && let Some(gesture) = gesture.as_mut()
            {
                gesture.dirty = true;
            }
            project_rectangle_selection_visual(&ui, &gesture_for_end);
            update_rectangle_selection(&ui, &state_for_end, &gesture_for_end, &timer_for_end);
        } else {
            let update = mutate_active_selection(&state_for_end, TabSession::clear_selection);
            if let Some((tab_id, changed)) = update {
                update_file_rows(&ui, &state_for_end, tab_id, &changed);
                update_selection_summary(&ui, &state_for_end);
            }
        }
        finish_rectangle_selection(&ui, &gesture_for_end, &timer_for_end);
    });

    let gesture_for_cancel = gesture.clone();
    let timer_for_cancel = timer.clone();
    let state_for_cancel = state.clone();
    let weak_for_cancel = ui.as_weak();
    ui.on_cancel_rectangle_selection(move || {
        if let Some(ui) = weak_for_cancel.upgrade() {
            cancel_rectangle_selection(
                &ui,
                &state_for_cancel,
                &gesture_for_cancel,
                &timer_for_cancel,
            );
        }
    });

    let gesture_for_timer = gesture.clone();
    let timer_for_tick = timer.clone();
    let state_for_timer = state.clone();
    let weak_for_timer = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let Some(ui) = weak_for_timer.upgrade() else {
                return;
            };
            let state = gesture_for_timer.lock().ok().and_then(|gesture| {
                gesture
                    .as_ref()
                    .map(|gesture| (gesture.active, gesture.dirty, gesture.pointer_y))
            });
            let Some((active, dirty, pointer_y)) = state else {
                return;
            };
            let mut viewport_changed = false;
            if active {
                let top = ui.get_rectangle_viewport_top();
                let bottom = top + ui.get_file_viewport_height();
                let delta = rectangle_selection_scroll_delta(pointer_y, top, bottom);
                if delta != 0.0 && ui.get_search_results_mode() {
                    ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
                    viewport_changed = true;
                } else {
                    let mode = view_mode_from_ui(ui.get_view_mode());
                    let maximum =
                        projected_scroll_maximum(&ui, mode, ui.get_file_viewport_height());
                    let viewport = (ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0);
                    if viewport != ui.get_file_viewport_y() {
                        ui.set_file_viewport_y(viewport);
                        viewport_changed = true;
                    }
                }
                if viewport_changed {
                    if let Ok(mut gesture) = gesture_for_timer.lock()
                        && let Some(gesture) = gesture.as_mut()
                    {
                        gesture.dirty = true;
                    }
                    project_rectangle_selection_visual(&ui, &gesture_for_timer);
                }
            }
            if active && (dirty || viewport_changed) {
                update_rectangle_selection(
                    &ui,
                    &state_for_timer,
                    &gesture_for_timer,
                    &timer_for_tick,
                );
            }
        },
    );
    timer.stop();
    timer
}

fn update_network_discovery(
    state: &WindowSessions,
    sender: &mpsc::SyncSender<NetworkDiscoveryRequest>,
    expanded: bool,
) {
    let window_id = state.window_id;
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let coordinator = app.network_discovery.entry(window_id).or_default();
        if expanded {
            let (request_id, cancel) = coordinator.begin();
            Some(NetworkDiscoveryRequest::Discover {
                window_id,
                request_id,
                cancel,
            })
        } else {
            coordinator.cancel_current();
            None
        }
    };
    if let Some(request) = request
        && sender.try_send(request).is_err()
        && let Ok(mut app) = state.lock()
    {
        if let Some(coordinator) = app.network_discovery.get_mut(&window_id) {
            coordinator.cancel_current();
        }
        app.network_discovery_errors
            .insert(window_id, "Windows 网络发现正忙，请稍后重试".to_owned());
    }
    refresh_all_windows(state);
}

fn network_discovery_needed(app: &AppState, window_id: WindowId) -> bool {
    app.network_discovery
        .get(&window_id)
        .is_none_or(|discovery| discovery.devices().is_empty())
}
#[allow(clippy::too_many_arguments)]
fn wire_callbacks(
    ui: &AppWindow,
    network_login_ui: slint::Weak<NetworkLoginWindow>,
    network_login_state: Arc<Mutex<NetworkLoginCoordinator>>,
    network_location_rename_ui: slint::Weak<NetworkLocationRenameWindow>,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    sender: mpsc::Sender<DirectoryRequest>,
    network_sender: mpsc::SyncSender<DirectoryRequest>,
    network_discovery_sender: mpsc::SyncSender<NetworkDiscoveryRequest>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    clipboard_sender: mpsc::Sender<ClipboardRequest>,
    everything_sender: mpsc::Sender<EverythingRequest>,
    icon_sender: mpsc::Sender<IconRequest>,
    shell_menu_worker: platform::windows::context_menu::ShellMenuWorker,
    quick_menu: SharedQuickMenu,
    state: WindowSessions,
) {
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
    let weak = ui.as_weak();
    let sender_for_path = sender.clone();
    let network_sender_for_path = network_sender.clone();
    let state_for_path = state.clone();
    let everything_for_accept = everything_sender.clone();
    ui.on_navigate_path(move |path| {
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
        let sender_for_validation = sender_for_path.clone();
        let network_sender_for_validation = network_sender_for_path.clone();
        let everything_for_validation = everything_for_accept.clone();
        thread::spawn(move || {
            let target =
                platform::windows::network::network_drive_to_unc(&target).unwrap_or(target);
            if crate::network::is_unc_path(&target) || target.is_dir() {
                let _ = slint::invoke_from_event_loop(move || {
                    submit_path_navigation(
                        &sender_for_validation,
                        &network_sender_for_validation,
                        &state_for_validation,
                        tab_id,
                        target,
                        NavigationKind::Normal,
                    );
                });
            } else {
                let _ = slint::invoke_from_event_loop(move || {
                    submit_search(
                        &everything_for_validation,
                        &state_for_validation,
                        None,
                        tab_id,
                        query,
                    );
                });
            }
        });
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_path);
        }
    });

    let state_for_folder_range = state.clone();
    let everything_for_folder_range = everything_sender.clone();
    ui.on_request_folder_size_range(move |viewport_y, viewport_height| {
        let target = state_for_folder_range.lock().ok().and_then(|app| {
            let tab = app.active();
            (tab.page_source == PageSource::Directory).then_some((tab.id, tab.latest_request))
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
        }
    });
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
            refresh_ui(&ui, &state_for_search_edit);
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
            let state = state_for_changed.clone();
            let sender = everything_for_changed.clone();
            let weak = weak_for_changed.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(160));
                let still_current = state.lock().ok().is_some_and(|app| {
                    app.active_window_state()
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|tab| {
                            tab.address_input == input && tab.address_mode == AddressMode::Smart
                        })
                });
                if still_current {
                    let query = state
                        .lock()
                        .ok()
                        .and_then(|app| {
                            app.active_window_state()
                                .tabs
                                .get(&tab_id)
                                .map(|tab| tab.search_query.clone())
                        })
                        .unwrap_or(input);
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        submit_search(&sender, &state, Some(&ui), tab_id, query);
                    });
                }
            });
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
        ui.set_file_viewport_y(search_window_viewport_y(index, window, view_mode, columns));
        submit_search_page(
            &everything_for_next_search_page,
            &state_for_next_search_page,
            tab_id,
            index,
        );
        refresh_tab_window(&state_for_next_search_page.shared, tab_id);
    });
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
                app.active().visible_entry(entry_id).map(|entry| {
                    (
                        app.active_window_state().active_tab,
                        entry
                            .open_target
                            .clone()
                            .unwrap_or_else(|| entry.path.clone()),
                        entry.kind == crate::domain::EntryKind::Directory,
                    )
                })
            })
        };
        if let Some((tab_id, target, is_directory)) = target {
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
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_entry);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_breadcrumb = sender.clone();
    let network_sender_for_breadcrumb = network_sender.clone();
    let state_for_breadcrumb = state.clone();
    ui.on_navigate_breadcrumb(move |index| {
        let target = {
            let app = state_for_breadcrumb
                .lock()
                .expect("app state mutex is not poisoned");
            usize::try_from(index)
                .ok()
                .and_then(|index| app.active().breadcrumb_paths().get(index).cloned())
                .map(|(_, path)| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            if crate::network::is_unc_server_root(&path) {
                platform::windows::network::record_runtime_event(
                    "network_device_navigation_submitted",
                );
            }
            submit_path_navigation(
                &sender_for_breadcrumb,
                &network_sender_for_breadcrumb,
                &state_for_breadcrumb,
                tab_id,
                path,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_breadcrumb);
            }
        }
    });
    let weak = ui.as_weak();
    let sender_for_sidebar = sender.clone();
    let network_sender_for_sidebar = network_sender.clone();
    let state_for_sidebar = state.clone();
    ui.on_navigate_sidebar(move |index| {
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
    let sender_for_network_location = sender.clone();
    let network_sender_for_network_location = network_sender.clone();
    let state_for_network_location = state.clone();
    ui.on_navigate_network_location(move |stable_id| {
        let Some(id) = stable_id.as_str().parse::<u64>().ok() else {
            return;
        };
        let target = state_for_network_location.lock().ok().and_then(|app| {
            app.imported_network_locations
                .iter()
                .chain(app.network_locations.iter())
                .find(|location| location.id == id)
                .map(|location| {
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

    let weak = ui.as_weak();
    let sender_for_activate_entry = sender.clone();
    let network_sender_for_activate_entry = network_sender.clone();
    let state_for_activate_entry = state.clone();
    let last_click = std::rc::Rc::new(Cell::new(None::<(TabId, RequestId, EntryId, Instant)>));
    ui.on_activate_entry(move |entry_id, toggle, extend| {
        let entry_id = EntryId(entry_id as u32);
        let now = Instant::now();
        let (tab_id, request_id) = {
            let app = state_for_activate_entry
                .lock()
                .expect("app state mutex is not poisoned");
            (
                app.active_window_state().active_tab,
                app.active().latest_request,
            )
        };
        let double_click_interval = platform::double_click_interval();
        let should_open = last_click.get().is_some_and(
            |(previous_tab, previous_request, previous_id, previous_time)| {
                previous_tab == tab_id
                    && previous_request == request_id
                    && previous_id == entry_id
                    && now.saturating_duration_since(previous_time) <= double_click_interval
            },
        );
        last_click.set((!should_open).then_some((tab_id, request_id, entry_id, now)));

        if should_open {
            let target = {
                let app = state_for_activate_entry
                    .lock()
                    .expect("app state mutex is not poisoned");
                app.active().visible_entry(entry_id).map(|entry| {
                    (
                        app.active_window_state().active_tab,
                        entry
                            .open_target
                            .clone()
                            .unwrap_or_else(|| entry.path.clone()),
                        entry.kind == crate::domain::EntryKind::Directory,
                    )
                })
            };
            if let Some((tab_id, target, is_directory)) = target {
                if is_directory {
                    submit_path_navigation(
                        &sender_for_activate_entry,
                        &network_sender_for_activate_entry,
                        &state_for_activate_entry,
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
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_activate_entry);
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
            let previous_selected = tab.selected.clone();
            let previous_focused = tab.focused;
            tab.select_entry(entry_id, toggle, extend);
            previous_selected
                .into_iter()
                .chain(tab.selected.iter().copied())
                .chain(previous_focused)
                .chain(tab.focused)
                .collect::<std::collections::HashSet<_>>()
        };
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_activate_entry, tab_id, &changed_rows);
            update_selection_summary(&ui, &state_for_activate_entry);
        }
    });
    let weak = ui.as_weak();
    let state_for_select = state.clone();
    ui.on_select_entry(move |entry_id, toggle, extend| {
        let (tab_id, changed_rows) = {
            let mut app = state_for_select
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) else {
                return;
            };
            let previous_selected = tab.selected.clone();
            let previous_focused = tab.focused;
            tab.select_entry(EntryId(entry_id as u32), toggle, extend);
            let changed = previous_selected
                .into_iter()
                .chain(tab.selected.iter().copied())
                .chain(previous_focused)
                .chain(tab.focused)
                .collect::<std::collections::HashSet<_>>();
            (tab_id, changed)
        };
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_select, tab_id, &changed_rows);
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
                    let request_id = tab.latest_request;
                    folder_queries = with_folder_scheduler(tab, |scheduler, entries| {
                        scheduler.begin_complete_sort(request_id, entries)
                    });
                }
                None
            }
        } else {
            None
        };
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
            refresh_ui(&ui, &state_for_sort);
        }
    });

    let weak = ui.as_weak();
    let state_for_view = state.clone();
    let icon_for_view = icon_sender.clone();
    ui.on_change_view_mode(move |mode| {
        if let Some(ui) = weak.upgrade()
            && ui.get_column_dragging()
        {
            ui.invoke_cancel_column_drag();
        }
        let mode = view_mode_from_ui(mode);
        let mut preserved_search_index = None;
        if let Ok(mut app) = state_for_view.lock() {
            let path = app.active().visible_path().map(Path::to_path_buf);
            if app.active().page_source == PageSource::Search {
                let previous_mode = app.search_view.view_mode;
                let total = app.active().search_total.unwrap_or(0);
                preserved_search_index = weak.upgrade().map(|ui| {
                    search_result_index_at_scroll(
                        ui.get_search_scroll_y(),
                        total,
                        previous_mode,
                        ui.get_grid_column_count().max(1) as usize,
                    )
                });
            }
            let tab_id = app.active_window_state().active_tab;
            if app.active().page_source == PageSource::Search {
                app.search_view.view_mode = mode;
            } else if let Some(path) = path {
                app.update_directory_preference(path, |preference| preference.view_mode = mode);
            }
            app.thumbnail_requests
                .retain(|(request_tab, _, _, _)| *request_tab != tab_id);
        }
        if let Some(ui) = weak.upgrade() {
            if let Some(index) = preserved_search_index {
                ui.set_search_scroll_y(search_scroll_for_index(
                    index,
                    mode,
                    ui.get_grid_column_count().max(1) as usize,
                ));
            }
            refresh_ui(&ui, &state_for_view);
            if preserved_search_index.is_none() {
                ui.set_file_viewport_y(0.0);
            }
            request_grid_thumbnails(
                &ui,
                &state_for_view.shared,
                state_for_view.window_id,
                &icon_for_view,
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
                refresh_ui(&ui, &state_for_columns);
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

    let weak = ui.as_weak();
    let sender_for_new = sender.clone();
    let network_sender_for_new = network_sender.clone();
    let state_for_new = state.clone();
    ui.on_new_tab(move || {
        let reload = {
            let mut app = state_for_new
                .lock()
                .expect("app state mutex is not poisoned");
            if app.duplicate_active_tab().is_some() {
                None
            } else {
                let path = app
                    .active()
                    .current_path
                    .clone()
                    .unwrap_or_else(initial_path);
                let tab_id = app.create_tab(path.clone());
                Some((tab_id, path))
            }
        };
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_new);
        }
        if let Some((tab_id, path)) = reload {
            submit_path_navigation(
                &sender_for_new,
                &network_sender_for_new,
                &state_for_new,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_new);
            }
        }
    });

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
        if app.active_window_state().tabs.contains_key(&target) {
            app.close_tab(target);
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_close);
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
        if let Some((tab_id, path)) = restored {
            submit_path_navigation(
                &sender_for_restore,
                &network_sender_for_restore,
                &state_for_restore,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_restore);
        }
    });

    let weak = ui.as_weak();
    let state_for_activate = state.clone();
    ui.on_activate_tab(move |tab_id| {
        if let Some(ui) = weak.upgrade()
            && ui.get_column_dragging()
        {
            ui.invoke_cancel_column_drag();
        }
        let id = TabId(tab_id as u32);
        let mut app = state_for_activate
            .lock()
            .expect("app state mutex is not poisoned");
        if app.active_window_state().tabs.contains_key(&id) {
            app.active_window_state_mut().active_tab = id;
        }
        drop(app);
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
    let senders_for_tab_drag = WorkerSenders {
        directory: sender.clone(),
        network_directory: network_sender.clone(),
        network_discovery: network_discovery_sender.clone(),
        operation: operation_sender.clone(),
        clipboard: clipboard_sender.clone(),
        shell_menu: shell_menu_worker.clone(),
        everything: everything_sender.clone(),
        icon: icon_sender.clone(),
        network_login: network_login_ui.clone(),
        network_login_state: network_login_state.clone(),
        network_location_rename: network_location_rename_ui.clone(),
    };
    let confirmations_for_tab_drag = ConfirmationWindows::new(delete_ui, conflict_ui, exit_ui);
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
            submit_path_navigation(
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
            submit_path_navigation(
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
                &tab.back_history
            } else {
                &tab.forward_history
            };
            let index = usize::try_from(index).ok();
            index
                .and_then(|index| stack.iter().rev().nth(index))
                .cloned()
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_path_navigation(
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
    let sender_for_refresh = sender.clone();
    let network_sender_for_refresh = network_sender.clone();
    let state_for_refresh = state.clone();
    ui.on_refresh(move || {
        let target = {
            let app = state_for_refresh
                .lock()
                .expect("app state mutex is not poisoned");
            if matches!(
                app.active().load_state,
                LoadState::Loading | LoadState::Partial
            ) {
                None
            } else {
                app.active()
                    .requested_path
                    .clone()
                    .or_else(|| app.active().current_path.clone())
                    .map(|path| (app.active_window_state().active_tab, path))
            }
        };
        if let Some((tab_id, path)) = target {
            submit_path_navigation(
                &sender_for_refresh,
                &network_sender_for_refresh,
                &state_for_refresh,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
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
                    app.active().requested_path.clone().map(|path| {
                        (
                            app.active_window,
                            app.active_window_state().active_tab,
                            app.active().latest_request,
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
                if let Some(login) = network_login_for_access.upgrade() {
                    configure_network_login_window(&login, &state_for_access, &target);
                    login.set_username("".into());
                    login.set_password("".into());
                    login.set_remember(false);
                    login.set_conflict(false);
                    login.set_busy(false);
                    show_network_login_window(&login);
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

    let weak = ui.as_weak();
    let delete_weak_for_language = delete_ui.as_weak();
    let conflict_weak_for_language = conflict_ui.as_weak();
    let exit_weak_for_language = exit_ui.as_weak();
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
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_language);
        }
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
        if let Some(ui) = weak_for_visibility.upgrade() {
            refresh_ui(&ui, &state_for_visibility);
        }
    });
    let weak = ui.as_weak();
    let delete_weak_for_theme = delete_ui.as_weak();
    let conflict_weak_for_theme = conflict_ui.as_weak();
    let exit_weak_for_theme = exit_ui.as_weak();
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
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_theme);
        }
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
        app.everything_config.executable_path =
            (!path.is_empty()).then(|| PathBuf::from(path.as_str()));
        app.everything_config.instance_name = instance.to_string();
        app.everything_config.verified_version = None;
        app.everything_generation = app.everything_generation.saturating_add(1);
        app.everything_status.clear();
        app.everything_busy = false;
        app.everything_folder_sizes_indexed = None;
        let config = app.everything_config.clone();
        drop(app);
        let _ = everything_for_config.send(EverythingRequest::Configure(config));
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_everything_config);
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
            refresh_ui(&ui, &state_for_test);
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
            refresh_ui(&ui, &state_for_start);
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
            refresh_ui(&ui, &state_for_discover);
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
            refresh_ui(&ui, &state_for_picker);
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
                .everything_status = error.to_string();
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_download);
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
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_settings);
        }
    });

    let context_anchor = Arc::new(Mutex::new((false, 240_i32, 180_i32)));
    let weak = ui.as_weak();
    let state_for_entry_menu = state.clone();
    let anchor_for_entry = context_anchor.clone();
    let clipboard_for_entry = clipboard_sender.clone();
    let quick_menu_for_entry = quick_menu.clone();
    let shell_menu_for_entry = shell_menu_worker.clone();
    ui.on_show_entry_menu(move |entry_id, x, y| {
        *anchor_for_entry.lock().expect("context anchor mutex") =
            (false, x.round() as i32, y.round() as i32);
        let _ = clipboard_for_entry.send(ClipboardRequest::CheckAvailability);
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
            quick_access_path = app
                .active()
                .visible_entry(id)
                .map(|entry| entry.path.clone());
        }
        drop(app);
        if let Ok(mut menu) = quick_menu_for_entry.lock() {
            menu.active_quick_access_path = quick_access_path;
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_context_menu_anchor_x(x);
            ui.set_context_menu_anchor_y(y);
            project_context_menu(&ui, &state_for_entry_menu, &quick_menu_for_entry, false);
            begin_shell_menu_load(
                &ui,
                &state_for_entry_menu,
                &quick_menu_for_entry,
                &shell_menu_for_entry,
                false,
            );
        }
    });

    let weak = ui.as_weak();
    let state_for_network_location_menu = state.clone();
    let quick_menu_for_network_location = quick_menu.clone();
    ui.on_show_network_location_menu(move |stable_id, x, y| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(location_id) = stable_id.as_str().parse::<u64>().ok() else {
            return;
        };
        let selected = state_for_network_location_menu.lock().ok().and_then(|app| {
            app.imported_network_locations
                .iter()
                .chain(app.network_locations.iter())
                .find(|location| location.id == location_id)
                .map(|location| {
                    (
                        location.id,
                        location.source == NetworkLocationSource::AsterOwned,
                        matches!(location.target, NetworkTarget::WindowsPath(_)),
                        app.language,
                    )
                })
        });
        let Some((location_id, owned, navigable, language)) = selected else {
            return;
        };
        let zh = |chinese: &'static str, english: &'static str| {
            if language == Language::Chinese {
                chinese
            } else {
                english
            }
        };
        let mut rows = vec![
            quick_menu_row(
                CMD_NETWORK_LOCATION_OPEN,
                0,
                zh("打开", "Open"),
                true,
                false,
                false,
            ),
            quick_menu_row(
                CMD_NETWORK_LOCATION_OPEN_NEW_TAB,
                0,
                zh("在新标签页中打开", "Open in new tab"),
                navigable,
                false,
                false,
            ),
            quick_menu_row(
                CMD_NETWORK_LOCATION_COPY_ADDRESS,
                0,
                zh("复制地址", "Copy address"),
                navigable,
                false,
                false,
            ),
            quick_menu_row(
                CMD_NETWORK_LOCATION_MANAGE_CREDENTIALS,
                0,
                zh("管理 Windows 凭据", "Manage Windows credentials"),
                true,
                false,
                false,
            ),
        ];
        if owned {
            rows.push(quick_menu_separator());
            rows.push(quick_menu_row(
                CMD_NETWORK_LOCATION_RENAME,
                0,
                zh("重命名", "Rename"),
                true,
                false,
                false,
            ));
            rows.push(quick_menu_row(
                CMD_NETWORK_LOCATION_MOVE_UP,
                0,
                zh("上移", "Move up"),
                true,
                false,
                false,
            ));
            rows.push(quick_menu_row(
                CMD_NETWORK_LOCATION_MOVE_DOWN,
                0,
                zh("下移", "Move down"),
                true,
                false,
                false,
            ));
            rows.push(quick_menu_row(
                CMD_NETWORK_LOCATION_REMOVE,
                0,
                zh("从网络位置移除", "Remove from network locations"),
                true,
                false,
                false,
            ));
        }
        if let Ok(mut menu) = quick_menu_for_network_location.lock() {
            menu.identity = None;
            menu.active_network_location = Some(location_id);
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
        project_filtered_context_menu(&ui, &quick_menu_for_network_location, "");
        ui.set_context_menu_open(true);
        if let Some(window_id) = window_id_for_ui(&ui) {
            open_quick_menu_popup(window_id, x, y);
        }
    });

    let weak = ui.as_weak();
    let state_for_quick_access_menu = state.clone();
    let quick_menu_for_quick_access = quick_menu.clone();
    ui.on_show_quick_access_menu(move |stable_id, x, y| {
        let Some(ui) = weak.upgrade() else { return };
        let path = PathBuf::from(stable_id.as_str());
        let selected = state_for_quick_access_menu.lock().ok().and_then(|app| {
            app.sidebar
                .iter()
                .find(|location| {
                    location.kind == KnownLocationKind::Pinned
                        && platform::windows::quick_access::paths_equal(&location.path, &path)
                })
                .map(|location| {
                    (
                        location.path.clone(),
                        app.language,
                        app.quick_access_pending.contains(&location.path),
                    )
                })
        });
        let Some((path, language, pending)) = selected else {
            return;
        };
        let label = match language {
            Language::Chinese => "从快速访问取消固定",
            Language::English => "Unpin from Quick access",
        };
        let rows = vec![quick_menu_row(
            CMD_QUICK_ACCESS_UNPIN,
            0,
            label,
            !pending,
            false,
            false,
        )];
        if let Ok(mut menu) = quick_menu_for_quick_access.lock() {
            menu.identity = None;
            menu.active_quick_access_path = Some(path);
            menu.built_in_rows = rows.clone();
            menu.all_rows = rows;
        }
        ui.set_context_menu_anchor_x(x);
        ui.set_context_menu_anchor_y(y);
        ui.set_context_search("".into());
        ui.set_context_shell_loading(false);
        project_filtered_context_menu(&ui, &quick_menu_for_quick_access, "");
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
    let clipboard_for_background = clipboard_sender.clone();
    let quick_menu_for_background = quick_menu.clone();
    let shell_menu_for_background = shell_menu_worker.clone();
    ui.on_show_background_menu(move |x, y| {
        *anchor_for_background.lock().expect("context anchor mutex") =
            (true, x.round() as i32, y.round() as i32);
        let _ = clipboard_for_background.send(ClipboardRequest::CheckAvailability);
        if let Ok(mut app) = state_for_background_menu.lock() {
            let tab_id = app.active_window_state().active_tab;
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
                tab.clear_selection();
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_context_menu_anchor_x(x);
            ui.set_context_menu_anchor_y(y);
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
    });

    let weak = ui.as_weak();
    let state_for_reopen_menu = state.clone();
    let anchor_for_reopen = context_anchor.clone();
    let clipboard_for_reopen = clipboard_sender.clone();
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
        let _ = clipboard_for_reopen.send(ClipboardRequest::CheckAvailability);
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

    let weak = ui.as_weak();
    let quick_menu_for_filter = quick_menu.clone();
    ui.on_filter_context_menu(move |query| {
        if let Some(ui) = weak.upgrade() {
            ui.invoke_cancel_context_submenu_hover();
            if let Ok(mut menu) = quick_menu_for_filter.lock() {
                menu.submenu_rows.clear();
                menu.submenu_history.clear();
                menu.active_submenu_token = None;
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
                ui.set_context_submenu_anchor_y(ui.get_context_active_anchor_y());
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
            ui.set_context_submenu_anchor_y(ui.get_context_submenu_active_anchor_y());
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
        ui.set_context_submenu_content_height(0.0);
        ui.set_context_submenu_commands(ModelRc::new(VecModel::from(
            Vec::<ContextCommandRow>::new(),
        )));
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
        let identity = quick_menu_for_dismiss
            .lock()
            .ok()
            .and_then(|mut menu| menu.identity.take());
        if let Some(identity) = identity {
            let _ = shell_menu_for_dismiss.send(
                platform::windows::context_menu::ShellMenuCommand::Close {
                    session_id: identity.session_id,
                    request_id: identity.request_id,
                },
            );
        }
    });

    let weak = ui.as_weak();
    let state_for_context_command = state.clone();
    let sender_for_context_command = operation_sender.clone();
    let delete_weak_for_context = delete_ui.as_weak();
    let clipboard_for_context = clipboard_sender.clone();
    let quick_menu_for_command = quick_menu.clone();
    let shell_menu_for_command = shell_menu_worker.clone();
    let everything_for_context_command = everything_sender.clone();
    ui.on_invoke_context_command(move |command| {
        match command {
            1 => create_default_folder(&state_for_context_command, &sender_for_context_command),
            2 => request_clipboard_write(&state_for_context_command, &clipboard_for_context, false),
            3 => request_clipboard_write(&state_for_context_command, &clipboard_for_context, true),
            4 => request_clipboard_paste(&state_for_context_command, &clipboard_for_context),
            5 => begin_rename_ui(&weak, &state_for_context_command),
            6 => submit_delete(
                &state_for_context_command,
                &sender_for_context_command,
                false,
            ),
            CMD_REFRESH => {
                if let Some(ui) = weak.upgrade() {
                    ui.invoke_refresh();
                }
            }
            CMD_QUICK_ACCESS_PIN | CMD_QUICK_ACCESS_UNPIN => {
                let path = quick_menu_for_command
                    .lock()
                    .ok()
                    .and_then(|menu| menu.active_quick_access_path.clone());
                if let Some(path) = path {
                    submit_quick_access_change(
                        state_for_context_command.shared.clone(),
                        path,
                        command == CMD_QUICK_ACCESS_PIN,
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
                        Ok(_) => app.network_locations = catalog.locations().to_vec(),
                        Err(crate::network::NetworkLocationCatalogError::DuplicateTarget) => {}
                        Err(error) => app
                            .operation_errors
                            .push(format!("Failed to add network location: {error:?}")),
                    }
                }
            }
            CMD_NETWORK_LOCATION_OPEN => {
                let target = selected_network_location_target(
                    &state_for_context_command,
                    &quick_menu_for_command,
                );
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
                let path = selected_network_location_path(
                    &state_for_context_command,
                    &quick_menu_for_command,
                );
                if let Some(path) = path {
                    let tab_id = state_for_context_command
                        .lock()
                        .ok()
                        .map(|mut app| app.create_tab(path.clone()));
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
                let selected = quick_menu_for_command
                    .lock()
                    .ok()
                    .and_then(|menu| menu.active_network_location)
                    .and_then(|id| {
                        state_for_context_command.lock().ok().and_then(|app| {
                            app.network_locations
                                .iter()
                                .find(|location| location.id == id)
                                .map(|location| {
                                    (
                                        id,
                                        location.display_name.clone(),
                                        app.language,
                                        app.dark_theme(),
                                    )
                                })
                        })
                    });
                if let (Some((id, name, language, dark_theme)), Some(window)) =
                    (selected, network_location_rename_ui.upgrade())
                {
                    configure_network_location_rename_window(
                        &window, id, &name, language, dark_theme,
                    );
                    show_network_location_rename_window(&window);
                }
            }
            CMD_NETWORK_LOCATION_COPY_ADDRESS => {
                if let Some(path) = selected_network_location_path(
                    &state_for_context_command,
                    &quick_menu_for_command,
                ) {
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
                        Ok(_) => app.network_locations = catalog.locations().to_vec(),
                        Err(error) => app
                            .operation_errors
                            .push(format!("Failed to manage network location: {error:?}")),
                    }
                }
                refresh_all_windows(&state_for_context_command.shared);
            }
            command if (CMD_VIEW_BASE..CMD_VIEW_BASE + 8).contains(&command) => {
                if let Some(mode) = ViewMode::from_storage_code((command - CMD_VIEW_BASE) as u8) {
                    set_view_mode(&state_for_context_command, mode);
                    refresh_all_windows(&state_for_context_command.shared);
                    if let Some(ui) = weak.upgrade() {
                        request_grid_thumbnails(
                            &ui,
                            &state_for_context_command.shared,
                            state_for_context_command.window_id,
                            &icon_sender,
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
                    refresh_all_windows(&state_for_context_command.shared);
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
                refresh_all_windows(&state_for_context_command.shared);
            }
            command
                if (CMD_GROUP_BASE..CMD_GROUP_BASE + 6).contains(&command)
                    || matches!(command, CMD_GROUP_ASC | CMD_GROUP_DESC) =>
            {
                if apply_group_command(&state_for_context_command, command) {
                    refresh_all_windows(&state_for_context_command.shared);
                }
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
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_context_command);
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
                    if let Some(ui) = weak.upgrade() {
                        refresh_ui(&ui, &state_for_context_command);
                    }
                }
            }
            7 => {
                if delete_weak_for_context
                    .upgrade()
                    .is_some_and(|window| window.window().is_visible())
                {
                    if let (Some(ui), Some(delete_ui)) =
                        (weak.upgrade(), delete_weak_for_context.upgrade())
                    {
                        show_confirmation_window(&ui, None, &delete_ui);
                    }
                    return;
                }
                let pending = pending_permanent_delete(&state_for_context_command);
                if let Ok(mut app) = state_for_context_command.lock() {
                    app.pending_permanent_delete = pending;
                }
                if let (Some(ui), Some(delete_ui)) =
                    (weak.upgrade(), delete_weak_for_context.upgrade())
                {
                    show_confirmation_window(&ui, None, &delete_ui);
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
                let current = state_for_context_command.lock().ok().and_then(|app| {
                    let tab = app.tab(identity.key.tab_id)?;
                    Some((
                        tab.latest_request,
                        selected_paths(&app),
                        tab.visible_path().map(Path::to_path_buf),
                    ))
                });
                if current.is_none_or(|(request, paths, folder)| {
                    request != identity.key.navigation_request
                        || paths != identity.key.paths
                        || folder != identity.key.folder
                }) {
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
                    let _ = shell_menu_for_command.send(
                        platform::windows::context_menu::ShellMenuCommand::Invoke {
                            session_id: identity.session_id,
                            request_id: identity.request_id,
                            command_id,
                            owner_window: native_window_handle(&ui),
                            screen_x: screen.x,
                            screen_y: screen.y,
                        },
                    );
                }
            }
            _ => {}
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_context_command);
        }
    });

    let state_for_copy = state.clone();
    let clipboard_for_copy = clipboard_sender.clone();
    ui.on_copy_selection(move |cut| {
        request_clipboard_write(&state_for_copy, &clipboard_for_copy, cut);
    });
    let state_for_paste = state.clone();
    let clipboard_for_paste = clipboard_sender;
    ui.on_paste_files(move || {
        request_clipboard_paste(&state_for_paste, &clipboard_for_paste);
    });

    let weak = ui.as_weak();
    let state_for_rename = state.clone();
    ui.on_begin_rename(move || {
        begin_rename_ui(&weak, &state_for_rename);
    });
    let weak = ui.as_weak();
    let state_for_commit_rename = state.clone();
    let sender_for_rename = operation_sender.clone();
    ui.on_commit_rename(move |name| {
        if let Err(message) =
            submit_rename(&state_for_commit_rename, &sender_for_rename, name.as_str())
        {
            if let Ok(mut app) = state_for_commit_rename.lock() {
                app.operation_errors.push(message);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_rename_submitting(false);
                ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
                refresh_ui(&ui, &state_for_commit_rename);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_cancel_rename = state.clone();
    ui.on_cancel_rename(move || {
        if let Ok(mut app) = state_for_cancel_rename.lock() {
            app.rename_targets
                .remove(&state_for_cancel_rename.window_id);
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_rename_submitting(false);
            ui.set_rename_editing(false);
            refresh_ui(&ui, &state_for_cancel_rename);
        }
    });

    let weak = ui.as_weak();
    let state_for_delete = state.clone();
    let sender_for_delete = operation_sender.clone();
    let delete_weak = delete_ui.as_weak();
    ui.on_request_delete(move |permanent| {
        if permanent {
            if delete_weak
                .upgrade()
                .is_some_and(|window| window.window().is_visible())
            {
                if let (Some(ui), Some(delete_ui)) = (weak.upgrade(), delete_weak.upgrade()) {
                    show_confirmation_window(&ui, None, &delete_ui);
                }
                return;
            }
            let pending = pending_permanent_delete(&state_for_delete);
            if let Ok(mut app) = state_for_delete.lock() {
                app.pending_permanent_delete = pending;
            }
            if let (Some(ui), Some(delete_ui)) = (weak.upgrade(), delete_weak.upgrade()) {
                show_confirmation_window(&ui, None, &delete_ui);
            }
        } else {
            submit_delete(&state_for_delete, &sender_for_delete, false);
        }
    });
    let weak = ui.as_weak();
    let state_for_cancel_operation = state.clone();
    ui.on_cancel_operation(move |id| {
        if let Ok(mut app) = state_for_cancel_operation.lock() {
            let operation_id = OperationId(id as u64);
            let _ = app.operations.cancel(operation_id);
            if let Some(response) = app.conflict_responses.remove(&operation_id) {
                let _ = response.send(crate::domain::file_operations::ConflictDecision {
                    action: crate::domain::file_operations::ConflictAction::Skip,
                    apply_to_all: false,
                });
            }
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_cancel_operation);
        }
    });
    let state_for_retry = state.clone();
    let sender_for_retry = operation_sender.clone();
    ui.on_retry_operation(move |id| {
        if let Some(request) = prepare_retry(&state_for_retry, OperationId(id as u64)) {
            let _ = sender_for_retry.send(request);
        }
    });
    let weak = ui.as_weak();
    let state_for_close = state.clone();
    let exit_weak = exit_ui.as_weak();
    ui.on_request_close(move || {
        let action = state_for_close
            .lock()
            .map(|app| app.request_window_close(state_for_close.window_id))
            .unwrap_or(WindowCloseAction::Ignore);
        match action {
            WindowCloseAction::ConfirmApplicationExit => {
                if let (Some(ui), Some(exit_ui)) = (weak.upgrade(), exit_weak.upgrade()) {
                    show_confirmation_window(&ui, None, &exit_ui);
                }
            }
            WindowCloseAction::CloseWindow => {
                if let Ok(mut app) = state_for_close.lock() {
                    app.cancel_tab_drag_for_window(state_for_close.window_id);
                    let _ = app.close_window(state_for_close.window_id);
                }
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.hide();
                }
                remove_window_runtime(state_for_close.window_id);
            }
            WindowCloseAction::ExitApplication => {
                platform::windows::network::record_runtime_event("event_loop_exit_requested");
                platform::windows::drag_drop::begin_shutdown_current();
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.hide();
                }
            }
            WindowCloseAction::Ignore => {}
        }
    });
}
fn should_close_context_menu(event: &winit::event::WindowEvent) -> bool {
    matches!(
        event,
        winit::event::WindowEvent::Focused(false)
            | winit::event::WindowEvent::Occluded(true)
            | winit::event::WindowEvent::Destroyed
    )
}

fn keyboard_shortcuts_suppressed(rename_editing: bool, context_menu_open: bool) -> bool {
    rename_editing || context_menu_open
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideNavigation {
    Back,
    Forward,
}

fn is_side_navigation_mouse_button(button: winit::event::MouseButton) -> bool {
    matches!(
        button,
        winit::event::MouseButton::Back | winit::event::MouseButton::Forward
    )
}
fn side_navigation_for_mouse_button(
    state: winit::event::ElementState,
    button: winit::event::MouseButton,
) -> Option<SideNavigation> {
    if state != winit::event::ElementState::Released {
        return None;
    }
    match button {
        winit::event::MouseButton::Back => Some(SideNavigation::Back),
        winit::event::MouseButton::Forward => Some(SideNavigation::Forward),
        _ => None,
    }
}
fn wire_mouse_navigation(
    ui: &AppWindow,
    confirmations: ConfirmationWindows,
    state: WindowSessions,
    window_id: WindowId,
    senders: WorkerSenders,
    shared_state: SharedSessions,
) {
    use winit::{
        event::{ElementState, WindowEvent},
        keyboard::{Key, ModifiersState, NamedKey},
    };

    let weak = ui.as_weak();
    let exit_weak = confirmations.exit.clone();
    let modifiers = Cell::new(ModifiersState::empty());
    let ime_composing = Cell::new(false);
    let type_select = Rc::new(RefCell::new(TypeSelectState::default()));
    let cursor_position = Cell::new(winit::dpi::PhysicalPosition::new(0.0, 0.0));
    let ctrl_wheel_accumulator = Cell::new(0.0_f32);
    ui.window().on_winit_window_event(move |_, event| {
        if matches!(event, WindowEvent::CloseRequested) {
            if weak
                .upgrade()
                .is_some_and(|ui| platform::windows::has_pointer_capture(native_window_handle(&ui)))
            {
                platform::windows::release_pointer_capture();
            }
            let action = state
                .lock()
                .map(|app| app.request_window_close(window_id))
                .unwrap_or(WindowCloseAction::Ignore);
            match action {
                WindowCloseAction::ConfirmApplicationExit => {
                    if let Some(ui) = weak.upgrade()
                        && let Some(exit_ui) = exit_weak.upgrade()
                    {
                        show_confirmation_window(&ui, None, &exit_ui);
                    }
                    return EventResult::PreventDefault;
                }
                WindowCloseAction::CloseWindow => {
                    dismiss_quick_menu_session(window_id, false);
                    if let Ok(mut app) = state.lock() {
                        app.cancel_tab_drag_for_window(window_id);
                        let _ = app.close_window(window_id);
                    }
                    project_native_insertion_indicator(None, &state);
                    remove_window_runtime(window_id);
                    return EventResult::Propagate;
                }
                WindowCloseAction::ExitApplication => {
                    dismiss_quick_menu_session(window_id, false);
                    platform::windows::network::record_runtime_event("event_loop_exit_requested");
                    platform::windows::drag_drop::begin_shutdown_current();
                    return EventResult::Propagate;
                }
                WindowCloseAction::Ignore => return EventResult::PreventDefault,
            }
        }
        if matches!(event, WindowEvent::Focused(true))
            && let Some(ui) = weak.upgrade()
        {
            if ui.get_context_menu_open() {
                // The owner receives activation before the underlying control's mouse event.
                // Retire the popup here so the same click can reach that control.
                dismiss_quick_menu_session(window_id, false);
            } else {
                reload_quick_access(ui.as_weak(), shared_state.clone());
            }
        }
        if should_close_context_menu(event) {
            if weak
                .upgrade()
                .is_some_and(|ui| platform::windows::has_pointer_capture(native_window_handle(&ui)))
            {
                platform::windows::release_pointer_capture();
            }
            if let Some(ui) = weak.upgrade() {
                if ui.get_rectangle_selection_pointer_active() {
                    ui.invoke_cancel_rectangle_selection();
                }
                if ui.invoke_has_column_drag() {
                    ui.invoke_cancel_column_drag_from_window();
                }
                let popup_handles = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                    runtimes
                        .get(&window_id)
                        .map(|runtime| {
                            std::iter::once(component_window_handle(&runtime.quick_menu_popup.root))
                                .chain(
                                    runtime
                                        .quick_menu_popup
                                        .branches
                                        .iter()
                                        .filter(|slot| slot.event.is_some())
                                        .map(|branch| component_window_handle(&branch.window)),
                                )
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                });
                let focus_entered_owned_popup = matches!(event, WindowEvent::Focused(false))
                    && platform::windows::quick_menu_window::foreground_belongs_to(
                        native_window_handle(&ui),
                        &popup_handles,
                    );
                if ui.get_context_menu_open() && !focus_entered_owned_popup {
                    ui.set_context_menu_open(false);
                    ui.invoke_dismiss_context_menu();
                    close_quick_menu_popup(window_id, false);
                }
                ui.set_drop_menu_open(false);
                if state
                    .lock()
                    .is_ok_and(|app| app.tab_drag.is_some_and(|drag| drag.window_id == window_id))
                {
                    ui.invoke_cancel_tab_drag();
                }
            }
            if let Ok(mut app) = state.lock() {
                app.pending_right_drops.remove(&window_id);
            }
            project_native_insertion_indicator(None, &state);
            return EventResult::Propagate;
        }
        if let WindowEvent::ModifiersChanged(changed) = event {
            modifiers.set(changed.state());
            return EventResult::Propagate;
        }
        if let WindowEvent::Ime(ime) = event {
            match ime {
                winit::event::Ime::Preedit(value, _) => {
                    ime_composing.set(!value.is_empty());
                }
                winit::event::Ime::Commit(_) | winit::event::Ime::Disabled => {
                    ime_composing.set(false);
                }
                winit::event::Ime::Enabled => {}
            }
            type_select.borrow_mut().clear();
            return EventResult::Propagate;
        }
        let Some(ui) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        if matches!(
            event,
            WindowEvent::Focused(false) | WindowEvent::Occluded(true)
        ) {
            ime_composing.set(false);
            type_select.borrow_mut().clear();
        }
        if let WindowEvent::Resized(size) = event {
            platform::windows::window_trace::log_request(
                native_window_handle(&ui),
                "winit-resized",
            );
            ui.set_window_width(size.width as f32 / ui.window().scale_factor());
            if view_mode_from_ui(ui.get_view_mode()).uses_grid_layout() {
                request_grid_thumbnails(&ui, &shared_state, window_id, &senders.icon);
            }
            return EventResult::Propagate;
        }
        if matches!(
            event,
            WindowEvent::ScaleFactorChanged { .. } | WindowEvent::Moved(_)
        ) {
            reposition_quick_menu_root_and_submenus(window_id);
            return EventResult::Propagate;
        }
        if matches!(event, WindowEvent::Focused(false)) {
            platform::windows::window_trace::log_request(
                native_window_handle(&ui),
                "winit-focused-false",
            );
        }
        if matches!(
            event,
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && matches!(event.logical_key, Key::Named(NamedKey::Escape))
        ) {
            let type_select_active = type_select.borrow_mut().clear();
            let mut cancelled = false;
            if ui.get_rectangle_selection_pointer_active() {
                ui.invoke_cancel_rectangle_selection();
                cancelled = true;
            }
            if ui.get_drop_menu_open() {
                ui.set_drop_menu_open(false);
                if let Ok(mut app) = state.lock() {
                    app.pending_right_drops.remove(&window_id);
                }
                cancelled = true;
            }
            if state.lock().is_ok_and(|app| app.tab_drag.is_some()) {
                ui.invoke_cancel_tab_drag();
                cancelled = true;
            }
            if state.lock().is_ok_and(|app| app.column_drag.is_some()) {
                ui.invoke_cancel_column_drag_from_window();
                cancelled = true;
            }
            return if cancelled || type_select_active {
                EventResult::PreventDefault
            } else {
                EventResult::Propagate
            };
        }
        if matches!(event, WindowEvent::KeyboardInput { .. })
            && keyboard_shortcuts_suppressed(ui.get_rename_editing(), ui.get_context_menu_open())
        {
            type_select.borrow_mut().clear();
            return EventResult::Propagate;
        }
        if matches!(
            event,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            }
        ) {
            type_select.borrow_mut().clear();
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                cursor_position.set(*position);
                EventResult::Propagate
            }
            WindowEvent::MouseWheel { delta, .. } if !ui.get_active_is_settings() => {
                let logical = cursor_position
                    .get()
                    .to_logical::<f32>(f64::from(ui.window().scale_factor()));
                if !pointer_targets_file_area(
                    logical.x,
                    logical.y,
                    ui.get_file_list_left(),
                    ui.get_file_list_top(),
                    ui.get_file_viewport_width(),
                    ui.get_file_viewport_height(),
                ) {
                    ctrl_wheel_accumulator.set(0.0);
                    return EventResult::Propagate;
                }
                let view_mode = view_mode_from_ui(ui.get_view_mode());
                let control = modifiers.get().control_key();
                if control {
                    if ui.get_context_menu_open()
                        || ui.get_rename_editing()
                        || ui.get_rectangle_selection_pointer_active()
                        || ui.get_drop_menu_open()
                        || state
                            .lock()
                            .is_ok_and(|app| app.tab_drag.is_some() || app.column_drag.is_some())
                    {
                        ctrl_wheel_accumulator.set(0.0);
                        return EventResult::Propagate;
                    }
                    let mut accumulated = ctrl_wheel_accumulator.get();
                    let step = ctrl_wheel_step(delta, ui.window().scale_factor(), &mut accumulated);
                    ctrl_wheel_accumulator.set(accumulated);
                    if let Some(toward_larger) = step {
                        let next = view_mode.step_ctrl_wheel(toward_larger);
                        if next != view_mode {
                            let viewport = anchored_viewport(
                                ui.get_file_viewport_y(),
                                logical.y - ui.get_file_list_top(),
                                view_mode,
                                next,
                                ui.get_grid_column_count().max(1) as usize,
                                state
                                    .lock()
                                    .ok()
                                    .map(|app| app.active().visible_entries().len())
                                    .unwrap_or(0),
                                ui.get_file_viewport_height(),
                            );
                            set_view_mode(&state, next);
                            refresh_all_windows(&shared_state);
                            ui.set_file_viewport_y(viewport);
                            request_grid_thumbnails(&ui, &shared_state, window_id, &senders.icon);
                        }
                    }
                    return EventResult::PreventDefault;
                }
                ctrl_wheel_accumulator.set(0.0);
                let delta = logical_scroll_delta(delta, view_mode, ui.window().scale_factor());
                if ui.get_search_results_mode() {
                    ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
                } else {
                    let maximum =
                        projected_scroll_maximum(&ui, view_mode, ui.get_file_viewport_height());
                    let viewport = (ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0);
                    ui.set_file_viewport_y(viewport);
                }
                if view_mode_from_ui(ui.get_view_mode()).uses_grid_layout() {
                    request_grid_thumbnails(&ui, &shared_state, window_id, &senders.icon);
                }
                EventResult::PreventDefault
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let modifiers = modifiers.get();
                let control = modifiers.control_key();
                let alt = modifiers.alt_key();
                let shift = modifiers.shift_key();
                let super_key = modifiers.super_key();
                let editing_address = ui.get_address_editing();
                let settings_active = ui.get_active_is_settings();
                let type_select_active = type_select.borrow().is_active();
                let character = match &event.logical_key {
                    Key::Character(value) => Some(value.as_str()),
                    _ => None,
                };
                if editing_address || settings_active {
                    type_select.borrow_mut().clear();
                }
                let handled = match &event.logical_key {
                    Key::Named(NamedKey::ArrowLeft) if alt && !control && !shift => {
                        if ui.get_can_navigate_back() {
                            ui.invoke_navigate_back();
                        }
                        true
                    }
                    Key::Named(NamedKey::ArrowRight) if alt && !control && !shift => {
                        if ui.get_can_navigate_forward() {
                            ui.invoke_navigate_forward();
                        }
                        true
                    }
                    Key::Named(NamedKey::ArrowUp) if alt && !control && !shift => {
                        if ui.get_can_navigate_up() {
                            ui.invoke_navigate_up();
                        }
                        true
                    }
                    Key::Named(NamedKey::F5) if !control && !alt && !shift => {
                        if ui.get_can_refresh() {
                            ui.invoke_refresh();
                        }
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("r")) =>
                    {
                        if ui.get_can_refresh() {
                            ui.invoke_refresh();
                        }
                        true
                    }
                    _ if ((control
                        && !alt
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("l")))
                        || (alt
                            && !control
                            && character.is_some_and(|value| value.eq_ignore_ascii_case("d"))))
                        && !shift
                        && !settings_active =>
                    {
                        ui.invoke_begin_search();
                        true
                    }
                    _ if !settings_active
                        && !editing_address
                        && control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("a")) =>
                    {
                        ui.invoke_select_all();
                        true
                    }
                    Key::Named(NamedKey::Space)
                        if !settings_active && !editing_address && control && !alt && !shift =>
                    {
                        ui.invoke_toggle_focused();
                        true
                    }
                    Key::Named(NamedKey::ArrowUp)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_move_focus(-1, shift);
                        true
                    }
                    Key::Named(NamedKey::ArrowDown)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_move_focus(1, shift);
                        true
                    }
                    Key::Named(NamedKey::Home)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_focus_boundary(false, shift);
                        true
                    }
                    Key::Named(NamedKey::End)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_focus_boundary(true, shift);
                        true
                    }
                    Key::Named(NamedKey::Enter)
                        if !settings_active && !editing_address && !control && !alt && !shift =>
                    {
                        ui.invoke_open_entry(-1);
                        true
                    }
                    Key::Named(NamedKey::Escape)
                        if !settings_active && !editing_address && !control && !alt && !shift =>
                    {
                        if ui.get_drop_menu_open() {
                            ui.set_drop_menu_open(false);
                            if let Ok(mut app) = state.lock() {
                                app.pending_right_drops.remove(&window_id);
                            }
                        } else if !type_select_active {
                            ui.invoke_clear_selection();
                        }
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("c")) =>
                    {
                        ui.invoke_copy_selection(false);
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("x")) =>
                    {
                        ui.invoke_copy_selection(true);
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("v")) =>
                    {
                        ui.invoke_paste_files();
                        true
                    }
                    Key::Named(NamedKey::F2)
                        if !control && !alt && !shift && !settings_active && !editing_address =>
                    {
                        ui.invoke_begin_rename();
                        true
                    }
                    Key::Named(NamedKey::Delete)
                        if !control && !alt && !settings_active && !editing_address =>
                    {
                        ui.invoke_request_delete(shift);
                        true
                    }
                    Key::Named(NamedKey::F10)
                        if shift && !control && !alt && !settings_active && !editing_address =>
                    {
                        ui.invoke_show_keyboard_context_menu();
                        true
                    }
                    Key::Named(NamedKey::ContextMenu)
                        if !control && !alt && !settings_active && !editing_address =>
                    {
                        ui.invoke_show_keyboard_context_menu();
                        true
                    }
                    _ if ui.get_file_list_keyboard_target()
                        && !ui.get_rectangle_selection_pointer_active()
                        && !control
                        && !alt
                        && !shift
                        && !super_key
                        && !ime_composing.get()
                        && event.text.as_ref().is_some_and(|value| {
                            let mut chars = value.chars();
                            chars
                                .next()
                                .is_some_and(|character| character.is_alphanumeric())
                                && chars.next().is_none()
                        }) =>
                    {
                        let typed = event.text.as_ref().and_then(|value| value.chars().next());
                        let target = typed.and_then(|typed| {
                            let app = state.shared.lock().ok()?;
                            let window = app.window(window_id)?;
                            let tab = window.tabs.get(&window.active_tab)?;
                            (tab.kind == TabKind::Files).then_some(())?;
                            let context = type_select_context(&app, tab);
                            let projection = type_select_projection(&app, tab);
                            let focused = tab.focused;
                            drop(app);
                            type_select.borrow_mut().select(
                                context,
                                Instant::now(),
                                typed,
                                &projection,
                                focused,
                            )
                        });
                        if let Some(entry_id) = target {
                            let update = mutate_window_selection(&state, window_id, |tab| {
                                tab.select_entry(entry_id, false, false);
                            });
                            if let Some((tab_id, changed)) = update {
                                update_file_rows(&ui, &state, tab_id, &changed);
                                update_selection_summary(&ui, &state);
                                let request_id = state
                                    .lock()
                                    .ok()
                                    .and_then(|app| app.tab(tab_id).map(|tab| tab.latest_request));
                                if let Some(request_id) = request_id {
                                    reveal_entry(&ui, &state.shared, tab_id, request_id, entry_id);
                                }
                            }
                        }
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("t")) =>
                    {
                        ui.invoke_new_tab();
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("w")) =>
                    {
                        if ui.get_can_close_tab() {
                            ui.invoke_close_tab(-1);
                        }
                        true
                    }
                    _ => false,
                };
                if handled {
                    EventResult::PreventDefault
                } else {
                    EventResult::Propagate
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } if is_side_navigation_mouse_button(*button) => EventResult::PreventDefault,
            WindowEvent::MouseInput { state, button, .. }
                if side_navigation_for_mouse_button(*state, *button)
                    == Some(SideNavigation::Back) =>
            {
                if ui.get_can_navigate_back() {
                    let weak = ui.as_weak();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_navigate_back();
                        }
                    });
                }
                EventResult::PreventDefault
            }
            WindowEvent::MouseInput { state, button, .. }
                if side_navigation_for_mouse_button(*state, *button)
                    == Some(SideNavigation::Forward) =>
            {
                if ui.get_can_navigate_forward() {
                    let weak = ui.as_weak();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_navigate_forward();
                        }
                    });
                }
                EventResult::PreventDefault
            }

            _ => EventResult::Propagate,
        }
    });
}

fn should_release_internal_pointer_grab(distance: f32, outbound_started: bool) -> bool {
    distance >= 4.0 && !outbound_started
}

fn should_start_native_tab_drag(became_dragging: bool, native_started: bool) -> bool {
    became_dragging && !native_started
}
fn should_refresh_outbound_drag_source(
    result: platform::windows::drag_drop::OutboundDropResult,
) -> bool {
    result.dropped && result.effect == platform::windows::drag_drop::DropEffect::Move
}

fn drop_requires_choice(intent: &platform::windows::drag_drop::DropIntent) -> bool {
    intent.right_button
}
fn selected_right_drop(
    mut intent: platform::windows::drag_drop::DropIntent,
    choice: i32,
) -> Result<Option<platform::windows::drag_drop::DropIntent>, String> {
    let (effect, allowed, key_state) = match choice {
        0 => return Ok(None),
        1 => (
            platform::windows::drag_drop::DropEffect::Copy,
            platform::windows::drag_drop::ALLOW_COPY,
            8,
        ),
        2 => (
            platform::windows::drag_drop::DropEffect::Move,
            platform::windows::drag_drop::ALLOW_MOVE,
            4,
        ),
        3 => (
            platform::windows::drag_drop::DropEffect::Link,
            platform::windows::drag_drop::ALLOW_LINK,
            32,
        ),
        _ => return Err("无效的拖放菜单选择".to_owned()),
    };
    if intent.allowed_effects & allowed == 0 {
        return Err("拖放来源不允许所选操作".to_owned());
    }
    let validation_key_state = key_state | if intent.right_button { 2 } else { 0 };
    let (validated, reason) = platform::windows::drag_drop::negotiate_target_effect(
        &intent.paths,
        Some(&intent.target),
        validation_key_state,
    );
    if reason.is_some() || validated != effect {
        return Err("所选拖放操作未通过路径保护".to_owned());
    }
    intent.effect = effect;
    intent.right_button = false;
    Ok(Some(intent))
}

fn dispatch_drop_operation(
    intent: platform::windows::drag_drop::DropIntent,
    origin_tab: TabId,
    state: SharedSessions,
    operation_sender: mpsc::Sender<FileOperationRequest>,
) {
    thread::spawn(move || match prepare_drop_operation(intent) {
        Ok(PreparedDrop::Operation(kind, items)) => {
            let _ = slint::invoke_from_event_loop(move || {
                enqueue_operation(&state, &operation_sender, origin_tab, kind, items);
            });
        }
        Ok(PreparedDrop::Shortcuts(shortcuts)) => {
            let result = create_drop_shortcuts(shortcuts);
            let _ = slint::invoke_from_event_loop(move || {
                if let Err(error) = result
                    && let Ok(mut app) = state.lock()
                {
                    app.operation_errors.push(error);
                }
            });
        }
        Err(error) => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Ok(mut app) = state.lock() {
                    app.operation_errors.push(error);
                }
            });
        }
    });
}

fn submit_quick_access_change(state: SharedSessions, path: PathBuf, pin: bool) {
    let pending_path = path.clone();
    {
        let Ok(mut app) = state.lock() else { return };
        if !app.quick_access_pending.insert(pending_path.clone()) {
            return;
        }
        app.quick_access_generation = app.quick_access_generation.wrapping_add(1).max(1);
    }
    thread::spawn(move || {
        let result = if pin {
            platform::windows::quick_access::pin(&path)
        } else {
            platform::windows::quick_access::unpin(&path)
        };
        let refreshed = platform::known_locations();
        let state_for_ui = state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut app) = state_for_ui.lock() {
                app.quick_access_pending.remove(&pending_path);
                app.sidebar = refreshed;
                if let Err(ref error) = result {
                    app.operation_errors
                        .push(format!("quick access operation failed: {error}"));
                }
                eprintln!(
                    "{{\"event\":\"quick_access_operation_finished\",\"pin\":{pin},\"path\":{:?},\"result\":{:?}}}",
                    path, result
                );
            }
            refresh_all_windows(&state_for_ui);
        });
    });
}
fn wire_native_drag_drop(
    ui: &AppWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: WindowSessions,
) -> slint::Timer {
    use std::cell::Cell;

    let window_id = state.window_id;
    let target = Arc::new(Mutex::new(
        platform::windows::drag_drop::DropTargetSnapshot::default(),
    ));
    let (intent_sender, intent_receiver) = mpsc::channel();
    let target_for_refresh = target.clone();
    let target_for_registration = target.clone();
    let intent_for_registration = intent_sender.clone();
    let state_for_target = state.clone();
    let weak_for_target = ui.as_weak();
    let last_sequence = Cell::new(0_u64);
    let registered_hwnd = Cell::new(0_isize);
    let target_timer = slint::Timer::default();
    target_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let hwnd = weak_for_target
                .upgrade()
                .map(|ui| native_window_handle(&ui))
                .unwrap_or_default();
            if platform::windows::drag_drop::registration_action(registered_hwnd.get(), hwnd)
                == platform::windows::drag_drop::RegistrationAction::Replace
            {
                let previous = registered_hwnd.replace(0);
                if previous != 0 {
                    platform::windows::drag_drop::revoke(previous);
                }
                match platform::windows::drag_drop::register_current(
                    hwnd,
                    target_for_registration.clone(),
                    intent_for_registration.clone(),
                ) {
                    Ok(()) => {
                        registered_hwnd.set(hwnd);
                        let state_for_tabs = state_for_target.shared.clone();
                        platform::windows::drag_drop::set_tab_target_handler(
                            hwnd,
                            Box::new(move |event| {
                                project_native_tab_target(window_id, event, &state_for_tabs);
                            }),
                        );
                    }
                    Err(error) => {
                        eprintln!("failed to register native drag-drop target hwnd={hwnd}: {error}")
                    }
                }
            }
            let snapshot = weak_for_target
                .upgrade()
                .and_then(|ui| {
                    state_for_target
                        .lock()
                        .ok()
                        .and_then(|app| drop_target_snapshot(&app, &ui))
                })
                .unwrap_or_default();
            if let Ok(mut target) = target_for_refresh.lock() {
                *target = snapshot.clone();
            }
            if let Some(ui) = weak_for_target.upgrade() {
                let drag = platform::windows::drag_drop::current_state(native_window_handle(&ui));
                if drag.event_sequence != last_sequence.get() {
                    last_sequence.set(drag.event_sequence);
                    let hovered = drag
                        .target
                        .as_deref()
                        .and_then(|path| {
                            snapshot
                                .folder_rows
                                .iter()
                                .find(|row| row.path == Path::new(path))
                        })
                        .and_then(|row| {
                            state_for_target.lock().ok().and_then(|app| {
                                app.active()
                                    .visible_entries()
                                    .iter()
                                    .find(|entry| entry.path == row.path)
                                    .map(|entry| entry.id.0 as i32)
                            })
                        })
                        .unwrap_or(-1);
                    ui.set_drop_hover_entry_id(hovered);
                    auto_scroll_drag_edge(&ui, &drag);
                }
            }
        },
    );

    let state_for_choice = state.clone();
    let operation_for_choice = operation_sender.clone();
    ui.on_choose_drop_effect(move |choice| {
        let pending = state_for_choice
            .lock()
            .ok()
            .and_then(|mut app| app.pending_right_drops.remove(&state_for_choice.window_id));
        let Some((origin_tab, intent)) = pending else {
            return;
        };
        match selected_right_drop(intent, choice) {
            Ok(Some(intent)) => dispatch_drop_operation(
                intent,
                origin_tab,
                state_for_choice.shared.clone(),
                operation_for_choice.clone(),
            ),
            Ok(None) => {}
            Err(error) => {
                if let Ok(mut app) = state_for_choice.lock() {
                    app.operation_errors.push(error);
                }
            }
        }
    });

    let weak_for_intents = ui.as_weak();
    thread::spawn(move || {
        while let Ok(intent) = intent_receiver.recv() {
            eprintln!(
                "drag-drop: intent received right_button={}",
                intent.right_button
            );
            if matches!(
                intent.target,
                platform::windows::drag_drop::DropTarget::QuickAccessPin
            ) {
                if intent.paths.len() == 1 {
                    submit_quick_access_change(state.shared.clone(), intent.paths[0].clone(), true);
                } else if let Ok(mut app) = state.lock() {
                    let language = app.language;
                    app.operation_errors.push(
                        match language {
                            Language::Chinese => "这里只能固定一个文件夹",
                            Language::English => "Only one folder can be pinned here",
                        }
                        .to_owned(),
                    );
                }
            } else if drop_requires_choice(&intent) {
                let state_for_ui = state.clone();
                let weak_for_ui = weak_for_intents.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_for_ui.upgrade() else {
                        return;
                    };
                    let Ok((client_left, client_top, _, _)) =
                        platform::windows::drag_drop::client_screen_rect(native_window_handle(&ui))
                    else {
                        return;
                    };
                    let scale = ui.window().scale_factor();
                    let x = (intent.screen_x - client_left) as f32 / scale;
                    let y = (intent.screen_y - client_top) as f32 / scale;
                    ui.set_drop_can_copy(
                        intent.allowed_effects & platform::windows::drag_drop::ALLOW_COPY != 0,
                    );
                    ui.set_drop_can_move(
                        intent.allowed_effects & platform::windows::drag_drop::ALLOW_MOVE != 0,
                    );
                    ui.set_drop_can_link(
                        intent.allowed_effects & platform::windows::drag_drop::ALLOW_LINK != 0,
                    );
                    let pending_saved = state_for_ui.lock().is_ok_and(|mut app| {
                        let Some(origin_tab) = app
                            .window(state_for_ui.window_id)
                            .map(|window| window.active_tab)
                        else {
                            return false;
                        };
                        app.pending_right_drops
                            .insert(state_for_ui.window_id, (origin_tab, intent));
                        true
                    });
                    if !pending_saved {
                        return;
                    }
                    eprintln!("drag-drop: showing right-drop menu x={x} y={y}");
                    ui.invoke_show_drop_menu(x, y);
                });
            } else {
                let origin_tab = state
                    .lock()
                    .ok()
                    .and_then(|app| app.window(state.window_id).map(|window| window.active_tab));
                if let Some(origin_tab) = origin_tab {
                    dispatch_drop_operation(
                        intent,
                        origin_tab,
                        state.shared.clone(),
                        operation_sender.clone(),
                    );
                }
            }
        }
    });
    target_timer
}

fn auto_scroll_drag_edge(ui: &AppWindow, drag: &platform::windows::drag_drop::DragDropState) {
    if drag.lifecycle != platform::windows::drag_drop::DragDropLifecycle::Dragging {
        return;
    }
    let Some(cursor_y) = drag.cursor_y else {
        return;
    };
    let Ok((_, client_top, _, _)) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(ui))
    else {
        return;
    };
    let scale = ui.window().scale_factor();
    let list_top = client_top + (ui.get_file_list_top() * scale).round() as i32;
    let bottom = client_top
        + ((ui.get_file_list_top() + ui.get_file_viewport_height()) * scale).round() as i32;
    let edge = (32.0 * scale).round() as i32;
    let delta = if cursor_y < list_top + edge {
        24.0
    } else if cursor_y > bottom - edge {
        -24.0
    } else {
        0.0
    };
    if delta == 0.0 {
        return;
    }
    if ui.get_search_results_mode() {
        ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
        return;
    }
    let view_mode = view_mode_from_ui(ui.get_view_mode());
    let maximum = projected_scroll_maximum(ui, view_mode, ui.get_file_viewport_height());
    ui.set_file_viewport_y((ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0));
}

fn drop_target_snapshot(
    app: &AppState,
    ui: &AppWindow,
) -> Option<platform::windows::drag_drop::DropTargetSnapshot> {
    let tab = app.active();
    if tab.kind != TabKind::Files || tab.load_state != LoadState::Complete {
        return None;
    }
    let current = tab.visible_path()?.to_path_buf();
    let hwnd = native_window_handle(ui);
    let (left, top, _right, bottom) =
        platform::windows::drag_drop::client_screen_rect(hwnd).ok()?;
    let scale = ui.window().scale_factor();
    let list_top = (ui.get_file_list_top() * scale).round() as i32;
    let list_left = (ui.get_file_list_left() * scale).round() as i32;
    let viewport = (-ui.get_file_viewport_y() * scale).max(0.0);
    let view_mode = view_mode_from_ui(ui.get_view_mode());
    let row_height = file_row_height(view_mode) * scale;
    let (target_left, target_right) = if view_mode == ViewMode::Details {
        let geometry = FileHitGeometry {
            viewport_x: ui.get_file_viewport_x(),
            viewport_width: ui.get_file_viewport_width(),
            columns_width: ui.get_details_hit_width(),
            ..FileHitGeometry::default()
        };
        let (target_left, target_right) = geometry.details_range();
        (
            left + list_left + (target_left * scale).round() as i32,
            left + list_left + (target_right * scale).round() as i32,
        )
    } else {
        (
            left + list_left,
            left + list_left + (ui.get_file_viewport_width() * scale).round() as i32,
        )
    };
    let groups = directory_group_projections(app, tab, tab.visible_entries());
    let list_projection = (!view_mode.uses_grid_layout())
        .then(|| ListProjection::from_groups(&groups, 32, file_row_height(view_mode) as u64));
    let icon_projection = view_mode.uses_grid_layout().then(|| {
        IconProjection::from_groups(
            &groups,
            ui.get_grid_column_count().max(1) as usize,
            32,
            file_row_height(view_mode) as u64,
        )
    });
    let folder_rows = tab
        .visible_entries()
        .iter()
        .filter(|entry| entry.kind == crate::domain::EntryKind::Directory)
        .filter_map(|entry| {
            let (visual_row, column) = if let Some(projection) = icon_projection.as_ref() {
                let position = projection.entry_position(entry.id)?;
                (position.row_index, position.column_index)
            } else {
                (list_projection.as_ref()?.entry_position(entry.id)?, 0)
            };
            let row_start = if let Some(projection) = icon_projection.as_ref() {
                projection.offsets.row_start(visual_row)? as f32
            } else {
                list_projection.as_ref()?.offsets.row_start(visual_row)? as f32
            };
            let row_top = top + list_top + (row_start * scale - viewport).round() as i32;
            let row_bottom = row_top + row_height.round() as i32;
            let card = file_layout_geometry(view_mode);
            let item_left = if card.grid {
                left + list_left
                    + (16.0 * scale).round() as i32
                    + (column as f32 * (card.card_width + 8.0) * scale).round() as i32
            } else {
                target_left
            };
            let item_right = if card.grid {
                item_left + (card.card_width * scale).round() as i32
            } else {
                target_right
            };
            (item_right > item_left && row_bottom > top + list_top && row_top < bottom).then(|| {
                platform::windows::drag_drop::FolderDropTarget {
                    left: item_left,
                    top: row_top,
                    right: item_right,
                    bottom: row_bottom.min(bottom),
                    path: entry.path.clone(),
                }
            })
        })
        .collect();
    Some(platform::windows::drag_drop::DropTargetSnapshot {
        current: Some(current),
        folder_rows,
        quick_access_pin: {
            let left_px = left + (ui.get_quick_access_drop_left() * scale).round() as i32;
            let top_px = top + (ui.get_quick_access_drop_top() * scale).round() as i32;
            let width_px = (ui.get_quick_access_drop_width() * scale).round() as i32;
            let height_px = (ui.get_quick_access_drop_height() * scale).round() as i32;
            (width_px > 0 && height_px > 0).then_some(
                platform::windows::drag_drop::DropTargetRect {
                    left: left_px,
                    top: top_px,
                    right: left_px + width_px,
                    bottom: top_px + height_px,
                },
            )
        },
    })
}

fn internal_drag_target(
    app: &AppState,
    x: f32,
    y: f32,
    geometry: FileHitGeometry,
    search_scroll_y: f32,
    grid_columns: usize,
) -> Option<(EntryId, PathBuf)> {
    let tab = app.active();
    let view_mode = app.active_view_mode();
    let local_x = x - geometry.list_left;
    if view_mode == ViewMode::Details && !geometry.details_contains(x) {
        return None;
    }
    if view_mode != ViewMode::Details
        && (local_x < 16.0 || local_x >= geometry.viewport_width - 16.0)
    {
        return None;
    }
    let content_y = y - geometry.list_top + (-geometry.viewport_y).max(0.0);
    let local_row = (content_y / file_row_height(view_mode)).floor() as usize;
    let local_index = if file_layout_geometry(view_mode).grid {
        let column = ((local_x - 16.0)
            / (file_layout_geometry(view_mode).card_width + 8.0).max(1.0))
        .floor() as usize;
        local_row
            .saturating_mul(grid_columns.max(1))
            .saturating_add(column.min(grid_columns.max(1) - 1))
    } else {
        local_row
    };
    let entry = if tab.page_source == PageSource::Search {
        let window = search_window_for_scroll(
            search_scroll_y,
            tab.search_total.unwrap_or(0),
            view_mode,
            grid_columns,
        );
        let id = window
            .start
            .checked_add(local_index as u32)?
            .checked_add(1)?;
        tab.visible_entry(EntryId(id))?
    } else {
        let id = directory_entry_at_visual_point(
            app,
            tab,
            view_mode,
            grid_columns,
            x - geometry.list_left,
            content_y,
        )?;
        tab.visible_entry(id)?
    };
    (entry.kind == crate::domain::EntryKind::Directory).then(|| (entry.id, entry.path.clone()))
}

fn create_drop_shortcuts(shortcuts: Vec<(PathBuf, PathBuf)>) -> Result<(), String> {
    for (source, destination) in shortcuts {
        platform::windows::shortcut::create_shortcut(&source, &destination)
            .map_err(|error| format!("创建快捷方式失败：{error}"))?;
    }
    Ok(())
}
enum PreparedDrop {
    Operation(FileOperationKind, Vec<OperationItem>),
    Shortcuts(Vec<(PathBuf, PathBuf)>),
}

fn prepare_drop_operation(
    intent: platform::windows::drag_drop::DropIntent,
) -> Result<PreparedDrop, String> {
    let platform::windows::drag_drop::DropTarget::Directory(target) = intent.target else {
        return Err("快速访问固定不能进入文件任务".to_owned());
    };
    let target = platform::windows::network::network_drive_to_unc(&target).unwrap_or(target);
    let target_metadata =
        std::fs::metadata(&target).map_err(|error| format!("拖放目标不可用：{error}"))?;
    if !target_metadata.is_dir() {
        return Err("拖放目标不是文件夹".to_owned());
    }
    let kind = match intent.effect {
        platform::windows::drag_drop::DropEffect::Copy => Some(FileOperationKind::Copy),
        platform::windows::drag_drop::DropEffect::Move => Some(FileOperationKind::Move),
        platform::windows::drag_drop::DropEffect::Link => None,
        platform::windows::drag_drop::DropEffect::None => {
            return Err("拖放效果不受支持".to_owned());
        }
    };
    let mut items = Vec::with_capacity(intent.paths.len());
    for source in intent.paths {
        let source = platform::windows::network::network_drive_to_unc(&source).unwrap_or(source);
        let metadata = std::fs::symlink_metadata(&source)
            .map_err(|error| format!("拖放来源不可用：{error}"))?;
        let _ = metadata.file_type();
        let name = source
            .file_name()
            .ok_or_else(|| "拖放来源没有可用名称".to_owned())?;
        let destination = target.join(name);
        if target.starts_with(&source)
            || (source == destination
                && intent.effect == platform::windows::drag_drop::DropEffect::Move)
        {
            return Err("不能把项目移动到自身或把项目拖放到自身子目录".to_owned());
        }
        items.push(OperationItem::pending(Some(source), Some(destination)));
    }
    if items.is_empty() {
        return Err("拖放未包含文件系统项目".to_owned());
    }
    if let Some(kind) = kind {
        Ok(PreparedDrop::Operation(kind, items))
    } else {
        let mut reserved = std::collections::HashSet::new();
        let mut shortcuts = Vec::with_capacity(items.len());
        for source in items.into_iter().filter_map(|item| item.source) {
            let mut destination =
                platform::windows::shortcut::shortcut_destination(&target, &source)
                    .map_err(|error| error.to_string())?;
            if reserved.contains(&destination) {
                let stem = source
                    .file_stem()
                    .or_else(|| source.file_name())
                    .ok_or_else(|| "拖放来源没有可用名称".to_owned())?;
                for index in 2_u64.. {
                    let mut name = stem.to_os_string();
                    name.push(format!(" ({index})"));
                    let mut candidate = target.join(name);
                    candidate.set_extension("lnk");
                    if !reserved.contains(&candidate) && !candidate.exists() {
                        destination = candidate;
                        break;
                    }
                }
            }
            reserved.insert(destination.clone());
            shortcuts.push((source, destination));
        }
        Ok(PreparedDrop::Shortcuts(shortcuts))
    }
}

fn wire_window_trace(ui: &AppWindow) {
    let path = if let Some(path) = platform::windows::window_trace::requested_path() {
        path
    } else if cfg!(debug_assertions) {
        let path = platform::windows::window_trace::default_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        path
    } else {
        return;
    };
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        if let Err(error) = platform::windows::window_trace::install(hwnd, &path) {
            eprintln!(
                "failed to install window trace at {}: {error}",
                path.display()
            );
        }
    });
}
fn wire_window_controls(ui: &AppWindow) {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    #[cfg(windows)]
    ui.window().with_winit_window(|window| {
        window.set_corner_preference(CornerPreference::Round);
        window.set_undecorated_shadow(true);
    });

    wire_window_trace(ui);

    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        platform::windows::window_trace::log_request(hwnd, "move-request");
        ui.window().with_winit_window(|window| {
            let _ = window.drag_window();
        });
    });

    let weak = ui.as_weak();
    ui.on_drag_window_after_menu_dismiss(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        platform::windows::window_trace::log_request(hwnd, "move-request-after-menu-dismiss");
        let _ = platform::windows::begin_window_drag(hwnd);
    });
}

fn configure_confirmation_window(ui: &ConfirmationWindow) {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    ui.window().on_close_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_safe_cancel();
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|window| {
                let _ = window.drag_window();
            });
        }
    });
    ui.window().with_winit_window(|window| {
        window.set_resizable(false);
        window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
        #[cfg(windows)]
        {
            window.set_corner_preference(CornerPreference::Round);
            window.set_undecorated_shadow(true);
        }
    });
}

fn wire_confirmation_windows(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: WindowSessions,
) {
    for confirmation in [delete_ui, conflict_ui, exit_ui] {
        configure_confirmation_window(confirmation);
    }

    let delete_weak = delete_ui.as_weak();
    let state_for_delete = state.clone();
    delete_ui.on_safe_cancel(move || {
        let demo_mode = delete_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if !demo_mode && let Ok(mut app) = state_for_delete.lock() {
            app.pending_permanent_delete = None;
        }
        if let Some(ui) = delete_weak.upgrade() {
            let _ = ui.hide();
        }
    });
    let delete_weak = delete_ui.as_weak();
    let state_for_delete = state.clone();
    let sender_for_delete = operation_sender.clone();
    delete_ui.on_primary_action(move || {
        if delete_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
            if let Some(ui) = delete_weak.upgrade() {
                let _ = ui.hide();
            }
            return;
        }
        let pending = state_for_delete
            .lock()
            .ok()
            .and_then(|mut app| app.pending_permanent_delete.take());
        if let Some(ui) = delete_weak.upgrade() {
            let _ = ui.hide();
        }
        if let Some((origin_tab, items)) = pending {
            submit_delete_items(
                &state_for_delete,
                &sender_for_delete,
                origin_tab,
                true,
                items,
            );
        }
    });

    let resolve_conflict = |operation_id: &str, action, apply_to_all, state: &SharedSessions| {
        let decision = crate::domain::file_operations::ConflictDecision {
            action,
            apply_to_all,
        };
        if let Ok(mut app) = state.lock()
            && let Ok(operation_id) = operation_id.parse::<u64>()
        {
            let id = OperationId(operation_id);
            if let Some(task) = app.operations.task_mut(id) {
                let _ = task.resolve_conflict(decision);
            }
            if let Some(response) = app.conflict_responses.remove(&id) {
                let _ = response.send(decision);
            }
        }
    };
    let conflict_weak = conflict_ui.as_weak();
    let state_for_conflict = state.clone();
    conflict_ui.on_safe_cancel(move || {
        if conflict_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
            if let Some(ui) = conflict_weak.upgrade() {
                let _ = ui.hide();
            }
            return;
        }
        let (operation_id, apply_to_all) = conflict_weak
            .upgrade()
            .map(|ui| (ui.get_operation_id(), ui.get_apply_all()))
            .unwrap_or_default();
        resolve_conflict(
            operation_id.as_str(),
            crate::domain::file_operations::ConflictAction::Skip,
            apply_to_all,
            &state_for_conflict,
        );
        if let Some(conflict_ui) = conflict_weak.upgrade() {
            conflict_ui.set_apply_all(false);
            conflict_ui.set_operation_id("".into());
            let _ = conflict_ui.hide();
        }
        refresh_all_windows(&state_for_conflict);
    });
    for (action_index, action) in [
        crate::domain::file_operations::ConflictAction::Replace,
        crate::domain::file_operations::ConflictAction::Skip,
        crate::domain::file_operations::ConflictAction::KeepBoth,
    ]
    .into_iter()
    .enumerate()
    {
        let conflict_weak = conflict_ui.as_weak();
        let state_for_conflict = state.clone();
        let callback = move || {
            if conflict_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
                if let Some(ui) = conflict_weak.upgrade() {
                    let _ = ui.hide();
                }
                return;
            }
            let (operation_id, apply_to_all) = conflict_weak
                .upgrade()
                .map(|ui| (ui.get_operation_id(), ui.get_apply_all()))
                .unwrap_or_default();
            resolve_conflict(
                operation_id.as_str(),
                action,
                apply_to_all,
                &state_for_conflict,
            );
            if let Some(conflict_ui) = conflict_weak.upgrade() {
                conflict_ui.set_apply_all(false);
                conflict_ui.set_operation_id("".into());
                let _ = conflict_ui.hide();
            }
            refresh_all_windows(&state_for_conflict);
        };
        match action_index {
            0 => conflict_ui.on_primary_action(callback),
            1 => conflict_ui.on_secondary_action(callback),
            _ => conflict_ui.on_tertiary_action(callback),
        }
    }

    let exit_weak = exit_ui.as_weak();
    let state_for_safe_cancel = state.clone();
    exit_ui.on_safe_cancel(move || {
        let demo_mode = exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if !demo_mode {
            open_task_center_on_live_window(&state_for_safe_cancel);
        }
    });
    let exit_weak = exit_ui.as_weak();
    let trace_path = platform::windows::window_trace::active_path()
        .map(Path::to_path_buf)
        .unwrap_or_else(platform::windows::window_trace::default_path);
    ui.set_window_trace_active(platform::windows::window_trace::is_active());
    ui.set_window_trace_status(trace_path.to_string_lossy().into_owned().into());
    let trace_weak = ui.as_weak();
    ui.on_start_window_trace(move || {
        let Some(ui) = trace_weak.upgrade() else {
            return;
        };
        if platform::windows::window_trace::is_active() {
            return;
        }
        let path = platform::windows::window_trace::default_path();
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            ui.set_window_trace_status(error.to_string().into());
            return;
        }
        match platform::windows::window_trace::install(native_window_handle(&ui), &path) {
            Ok(()) => {
                ui.set_window_trace_active(true);
                ui.set_window_trace_status(path.to_string_lossy().into_owned().into());
            }
            Err(error) => ui.set_window_trace_status(error.to_string().into()),
        }
    });
    let operation_weak = operation_ui.as_weak();
    let conflict_weak_for_exit = conflict_ui.as_weak();
    let delete_weak_for_exit = delete_ui.as_weak();
    let state_for_exit = state.clone();
    exit_ui.on_primary_action(move || {
        if exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
            if let Some(ui) = exit_weak.upgrade() {
                let _ = ui.hide();
            }
            return;
        }
        if let Ok(mut app) = state_for_exit.lock() {
            app.exit_after_cancel = true;
            let ids = app
                .operations
                .iter()
                .filter(|task| task.state.is_active())
                .map(|task| task.id)
                .collect::<Vec<_>>();
            for id in ids {
                let _ = app.operations.cancel(id);
                if let Some(response) = app.conflict_responses.remove(&id) {
                    let _ = response.send(crate::domain::file_operations::ConflictDecision {
                        action: crate::domain::file_operations::ConflictAction::Skip,
                        apply_to_all: false,
                    });
                }
            }
        }
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if let Some(conflict_ui) = conflict_weak_for_exit.upgrade() {
            conflict_ui.set_operation_id("".into());
            conflict_ui.set_apply_all(false);
            let _ = conflict_ui.hide();
        }
        if let Some(delete_ui) = delete_weak_for_exit.upgrade() {
            let _ = delete_ui.hide();
        }
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
        if !state_for_exit
            .lock()
            .is_ok_and(|app| app.operations.has_active_tasks())
        {
            hide_all_app_windows();
        }
    });
    let exit_weak = exit_ui.as_weak();
    let state_for_wait = state.clone();
    exit_ui.on_secondary_action(move || {
        let demo_mode = exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if !demo_mode {
            open_task_center_on_live_window(&state_for_wait);
        }
    });
}

fn wire_debug_showcase(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    state: SharedSessions,
) {
    ui.set_debug_tools_enabled(cfg!(debug_assertions));
    if !cfg!(debug_assertions) {
        return;
    }
    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let delete_weak = delete_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    ui.on_show_debug_window(move |kind| {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        match kind {
            0 => {
                if let Some(window) = delete_weak.upgrade() {
                    show_confirmation_window(&ui, None, &window);
                    window.set_demo_mode(true);
                }
            }
            1 => {
                if let Some(window) = conflict_weak.upgrade() {
                    window.set_operation_id("debug-showcase".into());
                    window.set_source_text(r"C:\示例\一段很长的源文件名.txt".into());
                    window.set_destination_text(r"D:\目标\一段很长的目标文件名.txt".into());
                    window.set_apply_all(false);
                    show_confirmation_window(&ui, operation_weak.upgrade().as_ref(), &window);
                    window.set_demo_mode(true);
                }
            }
            2 => {
                if let Some(window) = exit_weak.upgrade() {
                    show_confirmation_window(&ui, None, &window);
                    window.set_demo_mode(true);
                }
            }
            3 => {
                if let Some(window) = operation_weak.upgrade() {
                    refresh_debug_operation_window(&window, &state);
                    position_operation_window_next_to_main(&ui, &window);
                    let _ = window.show();
                }
            }
            _ => {}
        }
    });
}

fn show_confirmation_window(
    ui: &AppWindow,
    operation_ui: Option<&OperationWindow>,
    confirmation_ui: &ConfirmationWindow,
) {
    confirmation_ui.set_demo_mode(false);
    if confirmation_ui.window().is_visible() {
        confirmation_ui
            .window()
            .with_winit_window(|window| window.focus_window());
        return;
    }
    position_window_centered(ui, operation_ui, confirmation_ui);
    let _ = confirmation_ui.show();
    confirmation_ui
        .window()
        .with_winit_window(|window| window.focus_window());
}

fn show_network_login_window(ui: &NetworkLoginWindow) {
    if ui.window().is_visible() {
        ui.window()
            .with_winit_window(|window| window.focus_window());
        return;
    }
    let _ = ui.show();
    ui.window()
        .with_winit_window(|window| window.focus_window());
}

fn show_network_location_rename_window(ui: &NetworkLocationRenameWindow) {
    if ui.window().is_visible() {
        ui.window()
            .with_winit_window(|window| window.focus_window());
        return;
    }
    let _ = ui.show();
    ui.window()
        .with_winit_window(|window| window.focus_window());
}

fn configure_network_location_rename_window(
    ui: &NetworkLocationRenameWindow,
    id: u64,
    name: &str,
    language: Language,
    dark_theme: bool,
) {
    ui.set_location_id(id.to_string().into());
    ui.set_name(name.into());
    ui.set_dark_theme(dark_theme);
    match language {
        Language::Chinese => {
            ui.set_title_text("重命名网络位置".into());
            ui.set_detail_text("输入新的显示名称。".into());
            ui.set_cancel_text("取消".into());
            ui.set_save_text("保存".into());
            ui.set_close_text("关闭".into());
        }
        Language::English => {
            ui.set_title_text("Rename network location".into());
            ui.set_detail_text("Enter a new display name.".into());
            ui.set_cancel_text("Cancel".into());
            ui.set_save_text("Save".into());
            ui.set_close_text("Close".into());
        }
    }
}

fn wire_network_location_rename_window(ui: &NetworkLocationRenameWindow, state: SharedSessions) {
    ui.window().on_close_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_safe_cancel();
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let weak = ui.as_weak();
    ui.on_safe_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_location_id("".into());
            let _ = ui.hide();
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            let _ = ui.window().with_winit_window(|window| window.drag_window());
        }
    });
    let weak = ui.as_weak();
    ui.on_save(move |name| {
        let id = weak
            .upgrade()
            .and_then(|ui| ui.get_location_id().as_str().parse::<u64>().ok());
        let Some(id) = id else { return };
        if let Ok(mut app) = state.lock() {
            let mut catalog = NetworkLocationCatalog::new(app.network_locations.clone());
            match catalog.rename(id, name.as_str()) {
                Ok(()) => app.network_locations = catalog.locations().to_vec(),
                Err(error) => app
                    .operation_errors
                    .push(format!("Failed to rename network location: {error:?}")),
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_location_id("".into());
            let _ = ui.hide();
        }
        refresh_all_windows(&state);
    });
}

fn configure_network_login_window(ui: &NetworkLoginWindow, state: &SharedSessions, target: &Path) {
    let language = state
        .lock()
        .map(|app| app.language)
        .unwrap_or(Language::Chinese);
    ui.set_target_text(display_path(target).into());
    match language {
        Language::Chinese => {
            ui.set_title_text("登录网络位置".into());
            ui.set_detail_text("输入此网络位置使用的 Windows 凭据。".into());
            ui.set_user_label("用户名".into());
            ui.set_password_label("密码".into());
            ui.set_remember_label("记住凭据（由 Windows 管理）".into());
            ui.set_conflict_text(
                "Windows 已使用另一账号连接此服务器。断开会影响 Explorer 和其他软件。".into(),
            );
            ui.set_cancel_text("取消".into());
            ui.set_login_text("登录".into());
            ui.set_reconnect_text("断开并重新登录".into());
            ui.set_close_text("关闭".into());
        }
        Language::English => {
            ui.set_title_text("Sign in to network location".into());
            ui.set_detail_text("Enter the Windows credentials for this network location.".into());
            ui.set_user_label("Username".into());
            ui.set_password_label("Password".into());
            ui.set_remember_label("Remember credentials (managed by Windows)".into());
            ui.set_conflict_text(
                "Windows is already connected to this server with another account. Disconnecting can affect Explorer and other apps."
                    .into(),
            );
            ui.set_cancel_text("Cancel".into());
            ui.set_login_text("Sign in".into());
            ui.set_reconnect_text("Disconnect and sign in".into());
            ui.set_close_text("Close".into());
        }
    }
}

fn network_auth_error_text(
    error: platform::windows::network::NetworkAuthError,
    language: Language,
) -> String {
    use platform::windows::network::NetworkAuthErrorKind;

    let message = match (language, error.kind) {
        (Language::Chinese, NetworkAuthErrorKind::AccessDenied) => "Windows 拒绝了访问",
        (Language::Chinese, NetworkAuthErrorKind::LogonFailure) => "用户名或密码不正确",
        (Language::Chinese, NetworkAuthErrorKind::CredentialConflict) => {
            "Windows 已使用另一账号连接此服务器"
        }
        (Language::Chinese, NetworkAuthErrorKind::BadPath) => "网络路径不存在",
        (Language::Chinese, NetworkAuthErrorKind::Unavailable) => "网络位置暂时不可用",
        (Language::Chinese, NetworkAuthErrorKind::Other) => "无法登录网络位置",
        (Language::English, NetworkAuthErrorKind::AccessDenied) => "Windows denied access",
        (Language::English, NetworkAuthErrorKind::LogonFailure) => {
            "The username or password is incorrect"
        }
        (Language::English, NetworkAuthErrorKind::CredentialConflict) => {
            "Windows is connected to this server with another account"
        }
        (Language::English, NetworkAuthErrorKind::BadPath) => "The network path does not exist",
        (Language::English, NetworkAuthErrorKind::Unavailable) => {
            "The network location is unavailable"
        }
        (Language::English, NetworkAuthErrorKind::Other) => {
            "Unable to sign in to the network location"
        }
    };
    format!("{message}（Windows {}）", error.code)
}

fn wire_network_login_window(
    owner: &AppWindow,
    login: &NetworkLoginWindow,
    local_sender: mpsc::Sender<DirectoryRequest>,
    network_sender: mpsc::SyncSender<DirectoryRequest>,
    state: SharedSessions,
    login_state: Arc<Mutex<NetworkLoginCoordinator>>,
) {
    let login_weak = login.as_weak();
    let login_state_for_cancel = login_state.clone();
    login.on_safe_cancel(move || {
        if let Ok(mut coordinator) = login_state_for_cancel.lock() {
            coordinator.cancel();
        }
        if let Some(login) = login_weak.upgrade() {
            login.set_password("".into());
            login.set_conflict(false);
            login.set_busy(false);
            let _ = login.hide();
        }
    });
    let owner_weak = owner.as_weak();
    let login_weak = login.as_weak();
    login.on_drag_window(move || {
        if let Some(login) = login_weak.upgrade() {
            let _ = login
                .window()
                .with_winit_window(|window| window.drag_window());
        }
    });
    let login_weak = login.as_weak();
    login.on_login(move |username, password, remember, disconnect_first| {
        let session = login_state
            .lock()
            .ok()
            .and_then(|coordinator| coordinator.current.clone());
        let Some(session) = session else {
            return;
        };
        if let Some(login) = login_weak.upgrade() {
            login.set_busy(true);
        }
        let state = state.clone();
        let local_sender = local_sender.clone();
        let network_sender = network_sender.clone();
        let login_weak = login_weak.clone();
        let owner_weak = owner_weak.clone();
        let login_state = login_state.clone();
        let username = username.to_string();
        let mut password = password.to_string();
        let auth_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let auth_done_for_monitor = auth_done.clone();
        let state_for_monitor = state.clone();
        let login_state_for_monitor = login_state.clone();
        let session_for_monitor = session.clone();
        thread::spawn(move || {
            while !auth_done_for_monitor.load(std::sync::atomic::Ordering::Acquire) {
                let valid = state_for_monitor.lock().ok().is_some_and(|app| {
                    app.window_for_tab(session_for_monitor.tab_id)
                        == Some(session_for_monitor.window_id)
                        && app.tab(session_for_monitor.tab_id).is_some_and(|tab| {
                            tab.latest_request == session_for_monitor.failed_request_id
                                && tab.requested_path.as_deref()
                                    == Some(session_for_monitor.target.as_path())
                        })
                });
                if !valid {
                    if let Ok(mut coordinator) = login_state_for_monitor.lock() {
                        coordinator.cancel_generation(session_for_monitor.generation);
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        thread::spawn(move || {
            let result = (|| {
                if disconnect_first {
                    platform::windows::network::isolated_force_disconnect_network_share(
                        &session.target,
                        &session.cancel,
                    )?;
                }
                platform::windows::network::isolated_connect_network_share(
                    &session.target,
                    Some(&username),
                    Some(&password),
                    remember,
                    &session.cancel,
                )
            })();
            auth_done.store(true, std::sync::atomic::Ordering::Release);
            unsafe { password.as_bytes_mut() }.fill(0);
            let _ = slint::invoke_from_event_loop(move || {
                let still_current = login_state
                    .lock()
                    .ok()
                    .is_some_and(|coordinator| coordinator.is_current(session.generation))
                    && state.lock().ok().is_some_and(|app| {
                        app.window_for_tab(session.tab_id) == Some(session.window_id)
                            && app.tab(session.tab_id).is_some_and(|tab| {
                                tab.latest_request == session.failed_request_id
                                    && tab.requested_path.as_deref()
                                        == Some(session.target.as_path())
                            })
                    });
                if !still_current {
                    let superseded = login_state.lock().ok().is_some_and(|coordinator| {
                        coordinator
                            .current
                            .as_ref()
                            .is_some_and(|current| current.generation != session.generation)
                    });
                    if !superseded && let Some(login) = login_weak.upgrade() {
                        login.set_password("".into());
                        login.set_busy(false);
                        let _ = login.hide();
                    }
                    return;
                }
                match result {
                    Ok(_) => {
                        if let Ok(mut coordinator) = login_state.lock() {
                            coordinator.finish(session.generation);
                        }
                        if let Some(login) = login_weak.upgrade() {
                            login.set_password("".into());
                            login.set_busy(false);
                            login.set_conflict(false);
                            let _ = login.hide();
                        }
                        submit_path_navigation(
                            &local_sender,
                            &network_sender,
                            &state,
                            session.tab_id,
                            session.target,
                            NavigationKind::Refresh,
                        );
                        if let Some(target_ui) =
                            window_ui(session.window_id).or_else(|| owner_weak.upgrade())
                        {
                            refresh_ui(
                                &target_ui,
                                &WindowSessions::new(state.clone(), session.window_id),
                            );
                        }
                    }
                    Err(error)
                        if error.kind
                            == platform::windows::network::NetworkAuthErrorKind::CredentialConflict =>
                    {
                        if let Some(login) = login_weak.upgrade() {
                            login.set_busy(false);
                            login.set_conflict(true);
                        }
                    }
                    Err(error) => {
                        if let Some(login) = login_weak.upgrade() {
                            login.set_busy(false);
                            let language = state
                                .lock()
                                .map(|app| app.language)
                                .unwrap_or(Language::Chinese);
                            login.set_detail_text(network_auth_error_text(error, language).into());
                        }
                    }
                }
            });
        });
    });
}

fn position_window_centered(
    ui: &AppWindow,
    operation_ui: Option<&OperationWindow>,
    target_ui: &ConfirmationWindow,
) {
    let target_size = target_ui.window().size();
    let operation_visible = operation_ui.is_some_and(|window| window.window().is_visible());
    let mut target = None;
    let mut calculate = |source: &winit::window::Window| {
        let Some(monitor) = source.current_monitor() else {
            return;
        };
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let source_position = source.outer_position().unwrap_or(monitor_position);
        let source_size = source.outer_size();
        let centered_x =
            source_position.x + (source_size.width as i32 - target_size.width as i32) / 2;
        let centered_y =
            source_position.y + (source_size.height as i32 - target_size.height as i32) / 2;
        let max_x = monitor_position.x + monitor_size.width as i32 - target_size.width as i32;
        let max_y = monitor_position.y + monitor_size.height as i32 - target_size.height as i32;
        target = Some(slint::PhysicalPosition::new(
            centered_x.clamp(monitor_position.x, max_x.max(monitor_position.x)),
            centered_y.clamp(monitor_position.y, max_y.max(monitor_position.y)),
        ));
    };
    if operation_visible {
        operation_ui
            .expect("visible operation window exists")
            .window()
            .with_winit_window(&mut calculate);
    } else {
        ui.window().with_winit_window(calculate);
    }
    if let Some(position) = target {
        target_ui.window().set_position(position);
    }
}

fn wire_operation_window(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: SharedSessions,
) -> slint::Timer {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    let operation_weak = operation_ui.as_weak();
    operation_ui.on_request_hide(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
    });
    let operation_weak = operation_ui.as_weak();
    operation_ui.window().on_close_requested(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_drag_window(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            operation_ui.window().with_winit_window(|window| {
                let _ = window.drag_window();
            });
        }
    });
    #[cfg(windows)]
    operation_ui.window().with_winit_window(|window| {
        window.set_corner_preference(CornerPreference::Round);
        window.set_undecorated_shadow(true);
        window.set_resizable(false);
    });

    let state_for_cancel = state.clone();
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_cancel_operation(move |id| {
        if let Ok(mut app) = state_for_cancel.lock() {
            let id = OperationId(id as u64);
            if let Err(error) = app.operations.cancel(id) {
                app.operation_errors.push(format!(
                    "unable to cancel operation {} from {:?} to {:?}",
                    id.0, error.from, error.to
                ));
            }
            if let Some(response) = app.conflict_responses.remove(&id) {
                let _ = response.send(crate::domain::file_operations::ConflictDecision {
                    action: crate::domain::file_operations::ConflictAction::Skip,
                    apply_to_all: false,
                });
            }
        }
        refresh_all_windows(&state_for_cancel);
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
    });

    let state_for_pause = state.clone();
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_toggle_pause_operation(move |id| {
        if let Ok(mut app) = state_for_pause.lock() {
            let id = OperationId(id as u64);
            if let Err(error) = app.operations.toggle_pause(id) {
                app.operation_errors.push(format!(
                    "unable to pause or resume operation {} from {:?}",
                    id.0, error.from
                ));
            }
        }
        refresh_all_windows(&state_for_pause);
        if let Some(operation_ui) = operation_weak.upgrade() {
            refresh_operation_window(&operation_ui, &state_for_pause);
        }
    });

    let state_for_retry = state.clone();
    let sender_for_retry = operation_sender.clone();
    operation_ui.on_retry_operation(move |id| {
        if let Some(request) = prepare_retry(&state_for_retry, OperationId(id as u64)) {
            let _ = sender_for_retry.send(request);
        }
    });

    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let state_for_open = state.clone();
    ui.on_open_operation_window(move || {
        if let (Some(ui), Some(operation_ui)) = (ui_weak.upgrade(), operation_weak.upgrade()) {
            refresh_operation_window(&operation_ui, &state_for_open);
            position_operation_window_next_to_main(&ui, &operation_ui);
            let _ = operation_ui.show();
        }
    });

    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let state_for_auto_open = state.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(operation_ui) = operation_weak.upgrade() else {
                return;
            };
            let removed = state_for_auto_open
                .lock()
                .map(|mut app| app.operations.prune_transient(Duration::ZERO))
                .unwrap_or_default();
            if removed > 0 {
                refresh_all_windows(&state_for_auto_open);
                refresh_operation_window(&operation_ui, &state_for_auto_open);
            }
            let should_open = state_for_auto_open
                .lock()
                .is_ok_and(|app| should_auto_open_operation_window(&app));
            if should_open && !operation_ui.window().is_visible() {
                refresh_operation_window(&operation_ui, &state_for_auto_open);
                if let Some(ui) = ui_weak.upgrade() {
                    position_operation_window_next_to_main(&ui, &operation_ui);
                }
                let _ = operation_ui.show();
            }
        },
    );
    timer
}

fn should_auto_open_operation_window(app: &AppState) -> bool {
    app.operations.iter().any(|task| {
        matches!(
            task.state,
            OperationState::Preflight | OperationState::Running | OperationState::Paused
        ) && task.cancellation.active_elapsed(task.started_at) >= Duration::from_millis(800)
    })
}

fn position_operation_window_next_to_main(ui: &AppWindow, operation_ui: &OperationWindow) {
    let mut target = None;
    ui.window().with_winit_window(|main| {
        let Some(monitor) = main.current_monitor() else {
            return;
        };
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let operation_size = operation_ui.window().size();
        let main_position = main.outer_position().unwrap_or(monitor_position);
        let main_size = main.outer_size();
        let centered_x =
            main_position.x + (main_size.width as i32 - operation_size.width as i32) / 2;
        let centered_y =
            main_position.y + (main_size.height as i32 - operation_size.height as i32) / 2;
        let max_x = monitor_position.x + monitor_size.width as i32 - operation_size.width as i32;
        let max_y = monitor_position.y + monitor_size.height as i32 - operation_size.height as i32;
        target = Some(slint::PhysicalPosition::new(
            centered_x.clamp(monitor_position.x, max_x.max(monitor_position.x)),
            centered_y.clamp(monitor_position.y, max_y.max(monitor_position.y)),
        ));
    });
    if let Some(position) = target {
        operation_ui.window().set_position(position);
    }
}

fn refresh_operation_window(ui: &OperationWindow, state: &SharedSessions) {
    let Ok(mut app) = state.lock() else {
        return;
    };
    app.operations.prune_transient(Duration::ZERO);
    ui.set_operations(ModelRc::new(VecModel::from(operation_rows(&app))));
    ui.set_dark_theme(app.dark_theme());
    let (title, cancel, retry, pause, resume, empty, minimize, close) = match app.language {
        Language::Chinese => (
            "文件操作",
            "取消",
            "重试",
            "暂停",
            "继续",
            "没有文件操作",
            "最小化",
            "关闭",
        ),
        Language::English => (
            "File operations",
            "Cancel",
            "Retry",
            "Pause",
            "Resume",
            "No file operations",
            "Minimize",
            "Close",
        ),
    };
    ui.set_text_file_operations(title.into());
    ui.set_text_cancel_operation(cancel.into());
    ui.set_text_retry_operation(retry.into());
    ui.set_text_pause_operation(pause.into());
    ui.set_text_resume_operation(resume.into());
    ui.set_text_no_operations(empty.into());
    ui.set_text_window_minimize(minimize.into());
    ui.set_text_window_close(close.into());
}

fn refresh_debug_operation_window(ui: &OperationWindow, state: &SharedSessions) {
    refresh_operation_window(ui, state);
    let language = state
        .lock()
        .map(|app| app.language)
        .unwrap_or(Language::Chinese);
    let (title, transferred, speed, eta) = match language {
        Language::Chinese => ("正在复制示例文件", "384 MB / 1.0 GB", "48 MB/s", "约 14 秒"),
        Language::English => (
            "Copying sample files",
            "384 MB / 1.0 GB",
            "48 MB/s",
            "About 14 seconds",
        ),
    };
    ui.set_operations(ModelRc::new(VecModel::from(vec![OperationRow {
        id: -1,
        title: title.into(),
        percent: "38%".into(),
        file_progress: "12 / 32".into(),
        transferred: transferred.into(),
        speed: speed.into(),
        eta: eta.into(),
        current_item: "Example document with a long name.pdf".into(),
        source: r"C:\Users\Example\Documents".into(),
        destination: r"D:\Backup\Documents".into(),
        progress: 0.38,
        state: operation_state_index(OperationState::Running),
        paused: false,
        can_pause: false,
        can_cancel: false,
        can_retry: false,
    }])));
}

fn refresh_confirmation_windows(
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    state: &SharedSessions,
) {
    let Ok(app) = state.lock() else {
        return;
    };
    let (
        cancel,
        close,
        delete_title,
        delete_detail,
        delete,
        exit_title,
        exit_detail,
        cancel_exit,
        wait,
        conflict_title,
        conflict_detail,
        replace,
        skip,
        keep_both,
        apply_all,
        source_label,
        destination_label,
    ) = match app.language {
        Language::Chinese => (
            "取消",
            "关闭",
            "永久删除所选项目？",
            "此操作无法撤销。",
            "删除",
            "文件操作仍在进行",
            "等待任务完成，或取消任务后退出。",
            "取消任务并退出",
            "等待完成",
            "目标位置已有同名项目",
            "请选择如何处理当前冲突。",
            "替换",
            "跳过",
            "保留两者",
            "对这类冲突全部应用",
            "来源：",
            "目标：",
        ),
        Language::English => (
            "Cancel",
            "Close",
            "Permanently delete selected items?",
            "This action cannot be undone.",
            "Delete",
            "File operations are still running",
            "Wait for them to finish, or cancel the tasks before exiting.",
            "Cancel tasks and exit",
            "Wait",
            "An item with the same name already exists",
            "Choose how to handle this conflict.",
            "Replace",
            "Skip",
            "Keep both",
            "Apply to all conflicts of this type",
            "From: ",
            "To: ",
        ),
    };
    for window in [delete_ui, conflict_ui, exit_ui] {
        window.set_demo_mode(false);
        window.set_dark_theme(app.dark_theme());
        window.set_close_text(close.into());
    }
    delete_ui.set_kind(0);
    delete_ui.set_title_text(delete_title.into());
    delete_ui.set_detail_text(delete_detail.into());
    delete_ui.set_cancel_text(cancel.into());
    delete_ui.set_primary_text(delete.into());
    conflict_ui.set_kind(1);
    conflict_ui.set_title_text(conflict_title.into());
    conflict_ui.set_detail_text(conflict_detail.into());
    conflict_ui.set_cancel_text(cancel.into());
    conflict_ui.set_primary_text(replace.into());
    conflict_ui.set_secondary_text(skip.into());
    conflict_ui.set_tertiary_text(keep_both.into());
    conflict_ui.set_apply_all_text(apply_all.into());
    conflict_ui.set_source_label(source_label.into());
    conflict_ui.set_destination_label(destination_label.into());
    exit_ui.set_kind(2);
    exit_ui.set_title_text(exit_title.into());
    exit_ui.set_detail_text(exit_detail.into());
    exit_ui.set_primary_text(cancel_exit.into());
    exit_ui.set_secondary_text(wait.into());
}
fn scan_cleanup_diagnostics(_ui: &AppWindow, state: SharedSessions) {
    let roots = state
        .lock()
        .map(|app| app.stable_paths())
        .unwrap_or_default();
    thread::spawn(move || {
        let pending = roots
            .into_iter()
            .map(|path| path.join(".asterfiles-cleanup"))
            .find(|path| {
                std::fs::read_dir(path)
                    .ok()
                    .and_then(|mut entries| entries.next())
                    .is_some()
            });
        if let Some(path) = pending {
            let state_for_ui = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Ok(mut app) = state_for_ui.lock() {
                    app.operation_errors.push(format!(
                        "Pending cleanup requires attention: {}",
                        display_path(&path)
                    ));
                }
                refresh_all_windows(&state_for_ui);
            });
        }
    });
}

fn spawn_clipboard_worker() -> (
    mpsc::Sender<ClipboardRequest>,
    mpsc::Receiver<ClipboardEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            let event = match request {
                ClipboardRequest::Write { paths, cut } => ClipboardEvent::Written {
                    result: platform::windows::clipboard::write_file_list(
                        &paths,
                        if cut {
                            platform::windows::clipboard::ClipboardOperation::Move
                        } else {
                            platform::windows::clipboard::ClipboardOperation::Copy
                        },
                    )
                    .map_err(|error| error.to_string()),
                    paths,
                    cut,
                },
                ClipboardRequest::CheckAvailability => ClipboardEvent::Availability(
                    platform::windows::clipboard::read_file_list()
                        .map(|clipboard| clipboard.is_some())
                        .map_err(|error| error.to_string()),
                ),
                ClipboardRequest::ReadPaste { origin_tab, target } => ClipboardEvent::Paste {
                    origin_tab,
                    result: platform::windows::clipboard::read_file_list()
                        .map(|clipboard| {
                            clipboard.map(|clipboard| {
                                let target =
                                    platform::windows::network::network_drive_to_unc(&target)
                                        .unwrap_or(target);
                                let kind = match clipboard.operation {
                                    platform::windows::clipboard::ClipboardOperation::Copy => {
                                        FileOperationKind::Copy
                                    }
                                    platform::windows::clipboard::ClipboardOperation::Move => {
                                        FileOperationKind::Move
                                    }
                                };
                                let items = clipboard
                                    .paths
                                    .into_iter()
                                    .filter_map(|source| {
                                        let source =
                                            platform::windows::network::network_drive_to_unc(
                                                &source,
                                            )
                                            .unwrap_or(source);
                                        source.file_name().map(|name| {
                                            OperationItem::pending(
                                                Some(source.clone()),
                                                Some(target.join(name)),
                                            )
                                        })
                                    })
                                    .collect();
                                (kind, items)
                            })
                        })
                        .map_err(|error| error.to_string()),
                },
            };
            let _ = event_sender.send(event);
        }
    });
    (request_sender, event_receiver)
}

fn quick_menu_for_session(
    session_id: u64,
    request_id: u64,
) -> Option<(SharedQuickMenu, AppWindow)> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let window_id = WindowId((session_id >> 32) as u32);
        let runtime = runtimes.get(&window_id)?;
        let matches = runtime._quick_menu.lock().ok().is_some_and(|menu| {
            menu.identity.as_ref().is_some_and(|identity| {
                identity.session_id == session_id && identity.request_id == request_id
            })
        });
        matches.then(|| (runtime._quick_menu.clone(), runtime.ui.clone_strong()))
    })
}

fn start_shell_menu_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<platform::windows::context_menu::ShellMenuEvent>,
    worker: platform::windows::context_menu::ShellMenuWorker,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    _quick_menu: SharedQuickMenu,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let weak = weak.clone();
            let worker = worker.clone();
            let directory_sender = directory_sender.clone();
            let network_directory_sender = network_directory_sender.clone();
            let state = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let (session_id, request_id) = match &event {
                    platform::windows::context_menu::ShellMenuEvent::Loaded {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::SubmenuLoaded {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::SubmenuError {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::Invoked {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::Error {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::Closed {
                        session_id,
                        request_id,
                    } => (*session_id, *request_id),
                };
                let Some((menu_state, ui)) = quick_menu_for_session(session_id, request_id)
                    .or_else(|| {
                        weak.upgrade()
                            .map(|ui| (Arc::new(Mutex::new(QuickMenuState::default())), ui))
                    })
                else {
                    return;
                };
                match event {
                    platform::windows::context_menu::ShellMenuEvent::Loaded {
                        items,
                        elapsed_ms,
                        ..
                    } => {
                        let identity = menu_state
                            .lock()
                            .ok()
                            .and_then(|menu| menu.identity.clone());
                        let accepted = identity.as_ref().is_some_and(|identity| {
                            quick_menu_key_is_current(&state, &identity.key)
                        });
                        if !accepted {
                            let _ = worker.send(
                                platform::windows::context_menu::ShellMenuCommand::Close {
                                    session_id,
                                    request_id,
                                },
                            );
                            eprintln!(
                                "{{\"event\":\"shell_menu_stale\",\"session\":{session_id},\"request\":{request_id},\"phase\":\"load\"}}"
                            );
                            return;
                        }
                        if let Ok(mut menu) = menu_state.lock() {
                            menu.submenu_tokens.clear();
                            menu.preloaded_submenu_rows.clear();
                            menu.loaded_submenu_rows.clear();
                            menu.next_submenu_node = 0;
                            let projected = project_shell_menu_items(&mut menu, items);
                            if let Some(identity) = identity.as_ref() {
                                menu.snapshots.insert(
                                    identity.key.clone(),
                                    QuickMenuSnapshot {
                                        rows: projected.clone(),
                                        captured_at: Instant::now(),
                                    },
                                );
                                if let Some(current) = menu.identity.as_mut()
                                    && current.session_id == session_id
                                    && current.request_id == request_id
                                {
                                    current.ready = true;
                                }
                            }
                            menu.all_rows =
                                compose_quick_menu_rows(&menu.built_in_rows, &projected);
                        }
                        if ui.get_context_menu_open() {
                            ui.set_context_shell_loading(false);
                            ui.set_context_shell_elapsed_ms(elapsed_ms.min(i32::MAX as u128) as i32);
                            project_filtered_context_menu(
                                &ui,
                                &menu_state,
                                ui.get_context_search().as_str(),
                            );
                        }
                        eprintln!(
                            "{{\"event\":\"shell_menu_loaded\",\"session\":{session_id},\"request\":{request_id},\"elapsed_ms\":{elapsed_ms}}}"
                        );
                    }
                    platform::windows::context_menu::ShellMenuEvent::SubmenuLoaded {
                        submenu_request_id,
                        token,
                        items,
                        elapsed_ms,
                        ..
                    } => {
                        let item_count = items.len();
                        let accepted = menu_state.lock().ok().is_some_and(|menu| {
                            submenu_result_matches(
                                &menu,
                                session_id,
                                request_id,
                                submenu_request_id,
                                token,
                            ) && menu.identity.as_ref().is_some_and(|identity| {
                                quick_menu_key_is_current(&state, &identity.key)
                            })
                        });
                        trace_quick_menu(
                            "quick_menu_submenu_result_received",
                            format!(
                                "session={} request={} submenu_request={} token={} items={} elapsed_ms={} accepted={}",
                                session_id,
                                request_id,
                                submenu_request_id,
                                token,
                                item_count,
                                elapsed_ms,
                                accepted,
                            ),
                        );
                        if !accepted {
                            return;
                        }
                        if let Ok(mut menu) = menu_state.lock() {
                            let rows = project_shell_menu_items(&mut menu, items);
                            if !rows.is_empty() {
                                menu.loaded_submenu_rows.insert(token, rows.clone());
                            }
                            menu.submenu_rows = rows;
                        }
                        ui.set_context_submenu_loading(false);
                        project_context_submenu(&ui, &menu_state);
                        eprintln!(
                            "{{\"event\":\"shell_submenu_loaded\",\"session\":{session_id},\"request\":{request_id},\"elapsed_ms\":{elapsed_ms}}}"
                        );
                    }
                    platform::windows::context_menu::ShellMenuEvent::SubmenuError {
                        submenu_request_id,
                        token,
                        message,
                        elapsed_ms,
                        ..
                    } => {
                        let accepted = menu_state.lock().ok().is_some_and(|menu| {
                            submenu_result_matches(
                                &menu,
                                session_id,
                                request_id,
                                submenu_request_id,
                                token,
                            ) && menu.identity.as_ref().is_some_and(|identity| {
                                quick_menu_key_is_current(&state, &identity.key)
                            })
                        });
                        if accepted {
                            ui.set_context_submenu_loading(false);
                            if let Ok(mut app) = state.lock() {
                                app.operation_errors.push(format!(
                                    "Shell submenu failed after {elapsed_ms} ms: {message}"
                                ));
                            }
                        }
                    }
                    platform::windows::context_menu::ShellMenuEvent::Invoked {
                        invocation,
                        elapsed_ms,
                        ..
                    } => {
                        let identity = menu_state
                            .lock()
                            .ok()
                            .and_then(|menu| menu.identity.clone());
                        if !identity.as_ref().is_some_and(|identity| {
                            quick_menu_key_is_current(&state, &identity.key)
                        }) {
                            let _ = worker.send(
                                platform::windows::context_menu::ShellMenuCommand::Close {
                                    session_id,
                                    request_id,
                                },
                            );
                            return;
                        }
                        if let platform::windows::context_menu::ClassicMenuInvocation::BuiltIn {
                            verb,
                        } = invocation
                        {
                            match verb.as_str() {
                                "copy" => ui.invoke_copy_selection(false),
                                "cut" => ui.invoke_copy_selection(true),
                                "paste" => ui.invoke_paste_files(),
                                "delete" => ui.invoke_request_delete(false),
                                "rename" => ui.invoke_begin_rename(),
                                _ => {}
                            }
                        } else if let Some(folder) =
                            identity.and_then(|identity| identity.key.folder)
                        {
                            refresh_affected_tabs(
                                &directory_sender,
                                &network_directory_sender,
                                &state,
                                &[folder],
                            );
                        }
                        let _ =
                            worker.send(platform::windows::context_menu::ShellMenuCommand::Close {
                                session_id,
                                request_id,
                            });
                        eprintln!(
                            "{{\"event\":\"shell_menu_invoked\",\"session\":{session_id},\"request\":{request_id},\"elapsed_ms\":{elapsed_ms}}}"
                        );
                    }
                    platform::windows::context_menu::ShellMenuEvent::Error {
                        operation,
                        message,
                        elapsed_ms,
                        ..
                    } => {
                        let current = menu_state.lock().ok().is_some_and(|menu| {
                            menu.identity.as_ref().is_some_and(|identity| {
                                identity.session_id == session_id
                                    && identity.request_id == request_id
                            })
                        });
                        if current {
                            ui.set_context_shell_loading(false);
                            if let Ok(mut menu) = menu_state.lock() {
                                menu.identity = None;
                            }
                            if let Ok(mut app) = state.lock() {
                                app.operation_errors.push(format!(
                                    "Shell menu {operation} failed after {elapsed_ms} ms: {message}"
                                ));
                            }
                        } else {
                            eprintln!(
                                "{{\"event\":\"shell_menu_stale_error\",\"session\":{session_id},\"request\":{request_id},\"operation\":\"{operation}\",\"elapsed_ms\":{elapsed_ms}}}"
                            );
                        }
                        let _ =
                            worker.send(platform::windows::context_menu::ShellMenuCommand::Close {
                                session_id,
                                request_id,
                            });
                    }
                    platform::windows::context_menu::ShellMenuEvent::Closed { .. } => {
                        if let Ok(mut menu) = menu_state.lock()
                            && menu.identity.as_ref().is_some_and(|identity| {
                                identity.session_id == session_id
                                    && identity.request_id == request_id
                            })
                        {
                            menu.identity = None;
                        }
                    }
                }
            });
        }
    });
}
fn start_clipboard_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<ClipboardEvent>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let weak = weak.clone();
            let state = state.clone();
            let operation_sender = operation_sender.clone();
            let directory_sender = directory_sender.clone();
            let network_directory_sender = network_directory_sender.clone();
            let _ = slint::invoke_from_event_loop(move || {
                match event {
                    ClipboardEvent::Written {
                        result: Err(error), ..
                    }
                    | ClipboardEvent::Paste {
                        result: Err(error), ..
                    }
                    | ClipboardEvent::Availability(Err(error)) => {
                        if let Ok(mut app) = state.lock() {
                            app.operation_errors.push(error);
                        }
                    }
                    ClipboardEvent::Paste {
                        origin_tab,
                        result: Ok(Some((kind, items))),
                    } => enqueue_operation(&state, &operation_sender, origin_tab, kind, items),
                    ClipboardEvent::Written {
                        result: Ok(()),
                        paths,
                        cut,
                    } => {
                        let generation = if let Ok(mut app) = state.lock() {
                            app.cut_generation = app.cut_generation.wrapping_add(1);
                            app.cut_paths = if cut { paths.clone() } else { Vec::new() };
                            app.cut_generation
                        } else {
                            0
                        };
                        if cut && generation != 0 {
                            monitor_external_cut(
                                paths,
                                generation,
                                directory_sender.clone(),
                                network_directory_sender.clone(),
                                state.clone(),
                            );
                        }
                    }
                    ClipboardEvent::Availability(Ok(available)) => {
                        if let Ok(mut app) = state.lock() {
                            app.clipboard_has_files = available;
                        }
                    }
                    ClipboardEvent::Paste {
                        result: Ok(None), ..
                    } => {}
                }
                if weak.upgrade().is_some() {
                    refresh_all_windows(&state);
                }
            });
        }
    });
}

fn monitor_external_cut(
    mut paths: Vec<PathBuf>,
    generation: u64,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    state: SharedSessions,
) {
    thread::spawn(move || {
        let parents = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        for _ in 0..1_200 {
            thread::sleep(Duration::from_millis(250));
            if state
                .lock()
                .map_or(true, |app| app.cut_generation != generation)
            {
                return;
            }
            let remaining = existing_paths(&paths);
            if remaining.len() == paths.len() {
                continue;
            }
            let remaining_for_ui = remaining.clone();
            let parents_for_ui = parents.clone();
            let directory_sender_for_ui = directory_sender.clone();
            let network_directory_sender_for_ui = network_directory_sender.clone();
            let state_for_ui = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let current = state_for_ui.lock().is_ok_and(|mut app| {
                    if app.cut_generation != generation {
                        return false;
                    }
                    app.cut_paths = remaining_for_ui;
                    true
                });
                if current {
                    refresh_affected_tabs(
                        &directory_sender_for_ui,
                        &network_directory_sender_for_ui,
                        &state_for_ui,
                        &parents_for_ui,
                    );
                }
            });
            if remaining.is_empty() {
                return;
            }
            paths = remaining;
        }
    });
}

fn completed_target_for_item(
    item: &OperationItem,
    completed_paths: &[PathBuf],
    destination_was_existing_directory: bool,
) -> Option<PathBuf> {
    let destination = item.destination.as_deref()?;
    if destination_was_existing_directory {
        return None;
    }
    let parent = destination.parent()?;
    completed_paths
        .iter()
        .rfind(|path| {
            path.parent() == Some(parent) && item.source.as_deref() != Some(path.as_path())
        })
        .cloned()
}

fn queue_completed_focus(app: &mut AppState, targets: &[PathBuf]) {
    let directories = targets
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<std::collections::HashSet<_>>();
    for directory in directories {
        let paths = targets
            .iter()
            .filter(|path| path.parent() == Some(directory.as_path()))
            .cloned()
            .collect::<Vec<_>>();
        let matching_tabs = app
            .windows
            .values()
            .flat_map(|window| window.tabs.values())
            .filter(|tab| tab.visible_path() == Some(directory.as_path()))
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        for tab_id in matching_tabs {
            app.focus_after_refresh.insert(
                tab_id,
                PendingFocus {
                    directory: directory.clone(),
                    request_id: None,
                    paths: paths.clone(),
                },
            );
        }
    }
}

fn existing_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok())
        .cloned()
        .collect()
}
fn spawn_file_operation_worker() -> (
    mpsc::Sender<FileOperationRequest>,
    mpsc::Receiver<FileOperationEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<FileOperationRequest>();
    let (local_sender, local_receiver) = mpsc::channel::<FileOperationRequest>();
    let (network_sender, network_receiver) = mpsc::channel::<FileOperationRequest>();
    let (event_sender, event_receiver) = mpsc::channel::<FileOperationEvent>();
    let conflict_gate = Arc::new(Mutex::new(()));
    let dispatcher_event_sender = event_sender.clone();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            let sender = match request.resource {
                OperationResource::Local => &local_sender,
                OperationResource::Network => &network_sender,
            };
            if sender.send(request).is_err() {
                break;
            }
        }
    });
    run_file_operation_worker(local_receiver, event_sender.clone(), conflict_gate.clone());
    run_file_operation_worker(network_receiver, dispatcher_event_sender, conflict_gate);
    (request_sender, event_receiver)
}

fn run_file_operation_worker(
    receiver: mpsc::Receiver<FileOperationRequest>,
    event_sender: mpsc::Sender<FileOperationEvent>,
    conflict_gate: Arc<Mutex<()>>,
) {
    thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            execute_file_operation_request(request, &event_sender, &conflict_gate);
        }
    });
}

fn execute_file_operation_request(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
    conflict_gate: &Arc<Mutex<()>>,
) {
    let mut succeeded = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut affected = Vec::new();
    let mut indexed_states = Vec::new();
    let mut completed_targets = Vec::new();
    let started = Instant::now();
    let totals = (request.resource == OperationResource::Local)
        .then(|| {
            request
                .items
                .iter()
                .filter(|item| item.state == ItemState::Pending)
                .filter_map(|item| item.source.as_deref())
                .try_fold((0_u64, 0_usize), |(bytes, files), path| {
                    tree_totals(path, &request.cancellation).map(|(next_bytes, next_files)| {
                        (
                            bytes.saturating_add(next_bytes),
                            files.saturating_add(next_files),
                        )
                    })
                })
        })
        .flatten();
    let (total_bytes, total_files) = totals
        .map(|(bytes, files)| (Some(bytes), Some(files)))
        .unwrap_or((None, None));
    let _ = event_sender.send(FileOperationEvent::Progress {
        id: request.id,
        completed_items: 0,
        completed_files: 0,
        total_files,
        processed_bytes: 0,
        total_bytes,
        current_item: request
            .items
            .first()
            .and_then(|item| item.source.clone().or_else(|| item.destination.clone()))
            .unwrap_or_default(),
        started,
    });
    let mut processed_bytes = 0_u64;
    let mut processed_files = 0_usize;
    let mut conflict_defaults = HashMap::new();
    for (item_index, item) in request.items.iter().enumerate() {
        if item.state != ItemState::Pending {
            continue;
        }
        if request.cancellation.is_cancelled() {
            indexed_states.push((item_index, ItemState::Cancelled, None));
            continue;
        }
        let destination_was_existing_directory = request.resource == OperationResource::Local
            && item.source != item.destination
            && item
                .source
                .as_deref()
                .and_then(|path| std::fs::symlink_metadata(path).ok())
                .is_some_and(|metadata| metadata.file_type().is_dir())
            && item
                .destination
                .as_deref()
                .and_then(|path| std::fs::symlink_metadata(path).ok())
                .is_some_and(|metadata| metadata.file_type().is_dir());
        let outcome = execute_file_operation_item(
            request.id,
            request.kind,
            item,
            &request.cancellation,
            event_sender,
            item_index,
            processed_bytes,
            processed_files,
            total_bytes,
            total_files,
            started,
            &mut conflict_defaults,
            conflict_gate,
        );
        match outcome {
            Ok(report) => {
                processed_bytes = processed_bytes.saturating_add(report.bytes);
                processed_files = processed_files.saturating_add(report.files);
                if report.skipped.is_empty()
                    && let Some(target) = completed_target_for_item(
                        item,
                        &report.completed_paths,
                        destination_was_existing_directory,
                    )
                    && !completed_targets.contains(&target)
                {
                    completed_targets.push(target);
                }
                let current_item = item
                    .source
                    .clone()
                    .or_else(|| item.destination.clone())
                    .unwrap_or_default();
                let _ = event_sender.send(FileOperationEvent::Progress {
                    id: request.id,
                    completed_items: item_index + 1,
                    completed_files: processed_files,
                    total_files,
                    processed_bytes,
                    total_bytes,
                    current_item,
                    started,
                });
                let identity = item
                    .destination
                    .clone()
                    .or_else(|| item.source.clone())
                    .unwrap_or_default();
                if report.skipped.is_empty() {
                    succeeded.push(identity);
                    indexed_states.push((item_index, ItemState::Succeeded, None));
                } else {
                    skipped.extend(report.skipped);
                    indexed_states.push((item_index, ItemState::Skipped, None));
                }
                for directory in report.affected_directories {
                    if !affected.contains(&directory) {
                        affected.push(directory);
                    }
                }
            }
            Err(message) => {
                let identity = item
                    .source
                    .clone()
                    .or_else(|| item.destination.clone())
                    .unwrap_or_default();
                if request.cancellation.is_cancelled() {
                    indexed_states.push((item_index, ItemState::Cancelled, None));
                } else {
                    failed.push((identity, message.clone()));
                    indexed_states.push((item_index, ItemState::Failed, Some(message)));
                }
            }
        }
    }
    let _ = event_sender.send(FileOperationEvent::Finished {
        id: request.id,
        result: OperationResult {
            succeeded,
            skipped,
            failed,
            affected_directories: affected,
        },
        item_states: indexed_states,
        completed_targets,
    });
}
fn tree_totals(
    path: &Path,
    cancellation: &crate::domain::file_operations::CancellationToken,
) -> Option<(u64, usize)> {
    cancellation.wait_if_paused();
    if cancellation.is_cancelled() {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return Some((0, 1));
    }
    if metadata.is_file() {
        return Some((metadata.len(), 1));
    }
    let mut total_bytes = 0_u64;
    let mut total_files = 0_usize;
    for entry in std::fs::read_dir(path).ok()? {
        let (bytes, files) = tree_totals(&entry.ok()?.path(), cancellation)?;
        total_bytes = total_bytes.saturating_add(bytes);
        total_files = total_files.saturating_add(files);
    }
    Some((total_bytes, total_files))
}

fn file_snapshot(path: &Path) -> crate::domain::file_operations::FileSnapshot {
    let metadata = std::fs::symlink_metadata(path).ok();
    crate::domain::file_operations::FileSnapshot {
        path: path.to_path_buf(),
        is_directory: metadata
            .as_ref()
            .is_some_and(|value| value.file_type().is_dir()),
        size_bytes: metadata
            .as_ref()
            .filter(|value| value.is_file())
            .map(|value| value.len()),
        modified: metadata.and_then(|value| value.modified().ok()),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_file_operation_item(
    id: OperationId,
    kind: FileOperationKind,
    item: &OperationItem,
    cancel: &crate::domain::file_operations::CancellationToken,
    events: &mpsc::Sender<FileOperationEvent>,
    completed_items: usize,
    base_processed_bytes: u64,
    base_processed_files: usize,
    operation_total_bytes: Option<u64>,
    operation_total_files: Option<usize>,
    started: Instant,
    conflict_defaults: &mut HashMap<
        crate::domain::file_operations::ConflictCategory,
        crate::domain::file_operations::ConflictAction,
    >,
    conflict_gate: &Arc<Mutex<()>>,
) -> Result<crate::fs::file_operations::FileOperationReport, String> {
    if [item.source.as_deref(), item.destination.as_deref()]
        .into_iter()
        .flatten()
        .any(crate::network::is_unc_path)
        && matches!(
            kind,
            FileOperationKind::CreateFolder | FileOperationKind::Rename
        )
    {
        let isolated_kind = match kind {
            FileOperationKind::CreateFolder => {
                platform::windows::network::IsolatedFileMutationKind::CreateFolder
            }
            FileOperationKind::Rename => {
                platform::windows::network::IsolatedFileMutationKind::Rename
            }
            _ => unreachable!(),
        };
        let result = platform::windows::network::isolated_file_mutation(
            isolated_kind,
            item.source.as_deref(),
            item.destination.as_deref(),
            cancel.cancellation_flag(),
        )
        .map_err(|error| error.to_string())?;
        return Ok(crate::fs::file_operations::FileOperationReport {
            files: usize::from(kind != FileOperationKind::CreateFolder),
            directories: usize::from(kind == FileOperationKind::CreateFolder),
            bytes: 0,
            skipped: Vec::new(),
            affected_directories: result.affected_directories,
            cleanup_pending: None,
            completed_paths: result.completed_path.into_iter().collect(),
        });
    }
    let replace = &mut |category, source: &Path, destination: &Path| {
        let _conflict_gate = conflict_gate
            .lock()
            .expect("conflict gate mutex is not poisoned");
        if let Some(action) = conflict_defaults.get(&category).copied() {
            return action;
        }
        let (response_sender, response_receiver) = mpsc::channel();
        let conflict = crate::domain::file_operations::OperationConflict {
            category,
            source: file_snapshot(source),
            destination: file_snapshot(destination),
        };
        if events
            .send(FileOperationEvent::Conflict {
                id,
                conflict,
                response: response_sender,
            })
            .is_err()
        {
            return crate::domain::file_operations::ConflictAction::Skip;
        }
        match response_receiver.recv() {
            Ok(decision) => {
                if decision.apply_to_all {
                    conflict_defaults.insert(category, decision.action);
                }
                decision.action
            }
            Err(_) => crate::domain::file_operations::ConflictAction::Skip,
        }
    };
    match kind {
        FileOperationKind::CreateFolder => {
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let parent = destination.parent().ok_or("missing parent")?;
            let name = destination.file_name().ok_or("missing name")?;
            crate::fs::file_operations::create_folder(parent, name)
                .map(|path| {
                    let mut report = crate::fs::file_operations::FileOperationReport {
                        files: 0,
                        directories: 1,
                        bytes: 0,
                        skipped: vec![],
                        affected_directories: vec![],
                        cleanup_pending: None,
                        completed_paths: vec![path.clone()],
                    };
                    if let Some(parent) = path.parent() {
                        report.affected_directories.push(parent.to_path_buf());
                    }
                    report
                })
                .map_err(|error| format!("{error:?}"))
        }
        FileOperationKind::Rename => {
            let source = item.source.as_ref().ok_or("missing source")?;
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let name = destination.file_name().ok_or("missing name")?;
            crate::fs::file_operations::rename_path(source, name)
                .map(|path| {
                    let mut report = crate::fs::file_operations::FileOperationReport {
                        files: 1,
                        directories: 0,
                        bytes: 0,
                        skipped: vec![],
                        affected_directories: vec![],
                        cleanup_pending: None,
                        completed_paths: vec![path.clone()],
                    };
                    if let Some(parent) = path.parent() {
                        report.affected_directories.push(parent.to_path_buf());
                    }
                    report
                })
                .map_err(|error| format!("{error:?}"))
        }
        FileOperationKind::Copy | FileOperationKind::Move => {
            let source = item.source.as_ref().ok_or("missing source")?;
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let mut processed = base_processed_bytes;
            let mut completed_files = base_processed_files;
            let mut progress = |bytes, file_completed, current: &Path| {
                processed = processed.saturating_add(bytes);
                if file_completed {
                    completed_files = completed_files.saturating_add(1);
                }
                let _ = events.send(FileOperationEvent::Progress {
                    id,
                    completed_items,
                    completed_files,
                    total_files: operation_total_files,
                    processed_bytes: processed,
                    total_bytes: operation_total_bytes,
                    current_item: current.to_path_buf(),
                    started,
                });
            };
            let mut root_destination_reported = false;
            let result = if kind == FileOperationKind::Copy {
                crate::fs::file_operations::copy_path_with_progress(
                    source,
                    destination,
                    cancel,
                    replace,
                    &mut progress,
                    &mut |path| {
                        if !root_destination_reported {
                            root_destination_reported = true;
                            let _ = events.send(FileOperationEvent::DestinationCreated {
                                id,
                                path: path.to_path_buf(),
                            });
                        }
                    },
                )
            } else {
                crate::fs::file_operations::move_path_with_progress(
                    source,
                    destination,
                    cancel,
                    replace,
                    &mut progress,
                )
            };
            result.map_err(|error| format!("{error:?}"))
        }
        FileOperationKind::RecycleDelete => {
            let path = item.source.as_ref().ok_or("missing source")?.clone();
            let result = platform::windows::file_operation::recycle(&[path]);
            let first = result
                .items
                .into_iter()
                .next()
                .ok_or("missing recycle result")?;
            first
                .result
                .map(|_| crate::fs::file_operations::FileOperationReport {
                    files: 1,
                    directories: 0,
                    bytes: 0,
                    skipped: vec![],
                    affected_directories: first
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .into_iter()
                        .collect(),
                    cleanup_pending: None,
                    completed_paths: vec![],
                })
        }
        FileOperationKind::PermanentDelete => {
            let path = item.source.as_ref().ok_or("missing source")?;
            if should_fast_remove(path) {
                let parent = path.parent().ok_or("missing parent")?;
                let report = crate::fs::file_operations::fast_remove(
                    path,
                    &parent.join(".asterfiles-cleanup"),
                    cancel,
                )
                .map_err(|error| format!("{error:?}"))?;
                if let Some(pending) = report.cleanup_pending.as_ref()
                    && let Err(error) = crate::fs::file_operations::clean_pending(pending, cancel)
                {
                    let _ = std::fs::rename(pending, path);
                    return Err(format!(
                        "cleanup pending at {}: {error:?}",
                        display_path(pending)
                    ));
                }
                Ok(report)
            } else {
                crate::fs::file_operations::permanently_delete(path, cancel)
                    .map_err(|error| format!("{error:?}"))
            }
        }
        FileOperationKind::FastRemove => {
            let path = item.source.as_ref().ok_or("missing source")?;
            let parent = path.parent().ok_or("missing parent")?;
            let report = crate::fs::file_operations::fast_remove(
                path,
                &parent.join(".asterfiles-cleanup"),
                cancel,
            )
            .map_err(|error| format!("{error:?}"))?;
            if let Some(pending) = report.cleanup_pending.as_ref()
                && let Err(error) = crate::fs::file_operations::clean_pending(pending, cancel)
            {
                let _ = std::fs::rename(pending, path);
                return Err(format!(
                    "cleanup pending at {}: {error:?}",
                    display_path(pending)
                ));
            }
            Ok(report)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_file_operation_event_pump(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    receiver: mpsc::Receiver<FileOperationEvent>,
    sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    let operation_weak = operation_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let delete_weak = delete_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let weak = weak.clone();
            let operation_weak = operation_weak.clone();
            let conflict_weak = conflict_weak.clone();
            let delete_weak = delete_weak.clone();
            let exit_weak = exit_weak.clone();
            let sender = sender.clone();
            let directory_sender = directory_sender.clone();
            let network_directory_sender = network_directory_sender.clone();
            let state = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let event_operation_id = match &event {
                    FileOperationEvent::DestinationCreated { id, .. }
                    | FileOperationEvent::Progress { id, .. }
                    | FileOperationEvent::Conflict { id, .. }
                    | FileOperationEvent::Finished { id, .. } => *id,
                };
                match event {
                    FileOperationEvent::DestinationCreated { path, .. } => {
                        let parent = path.parent().map(Path::to_path_buf);
                        if let Some(parent) = parent {
                            refresh_affected_tabs(
                                &directory_sender,
                                &network_directory_sender,
                                &state,
                                &[parent],
                            );
                        }
                    }
                    FileOperationEvent::Progress {
                        id,
                        completed_items,
                        completed_files,
                        total_files,
                        processed_bytes,
                        total_bytes,
                        current_item,
                        started,
                    } => {
                        if let Ok(mut app) = state.lock()
                            && let Some(task) = app.operations.task_mut(id)
                        {
                            task.progress.completed_items = completed_items;
                            task.progress.completed_files = completed_files;
                            task.progress.total_files = total_files;
                            task.progress.processed_bytes = processed_bytes;
                            task.progress.total_bytes = total_bytes;
                            task.progress.current_item = Some(current_item);
                            let _elapsed = started.elapsed();
                        }
                    }
                    FileOperationEvent::Conflict {
                        id,
                        conflict,
                        response,
                    } => {
                        let source = display_path(&conflict.source.path);
                        let destination = display_path(&conflict.destination.path);
                        if let Ok(mut app) = state.lock() {
                            if let Some(task) = app.operations.task_mut(id) {
                                let _ = task.set_conflict(conflict);
                            }
                            app.conflict_responses.insert(id, response);
                        }
                        if let (Some(ui), Some(conflict_ui)) =
                            (weak.upgrade(), conflict_weak.upgrade())
                        {
                            conflict_ui.set_source_text(source.into());
                            conflict_ui.set_destination_text(destination.into());
                            conflict_ui.set_operation_id(id.0.to_string().into());
                            conflict_ui.set_apply_all(false);
                            show_confirmation_window(
                                &ui,
                                operation_weak.upgrade().as_ref(),
                                &conflict_ui,
                            );
                        }
                    }
                    FileOperationEvent::Finished {
                        id,
                        result,
                        item_states,
                        completed_targets,
                    } => {
                        let (affected, next) = {
                            let mut app = state.lock().expect("app state mutex is not poisoned");
                            if let Some(task) = app.operations.task_mut(id) {
                                for (index, status, error) in item_states {
                                    if let Some(item) = task.items.get_mut(index) {
                                        item.state = status;
                                        item.error = error;
                                    }
                                }
                            }
                            let cancelled = app
                                .operations
                                .task(id)
                                .is_some_and(|task| task.cancellation.is_cancelled());
                            let terminal = if cancelled
                                && result.succeeded.is_empty()
                                && result.failed.is_empty()
                            {
                                OperationState::Cancelled
                            } else if cancelled
                                || (!result.failed.is_empty() && !result.succeeded.is_empty())
                            {
                                OperationState::PartiallyCompleted
                            } else if result.failed.is_empty() {
                                OperationState::Completed
                            } else {
                                OperationState::Failed
                            };
                            let affected = result.affected_directories.clone();
                            let resource = app
                                .operations
                                .task(id)
                                .map(|task| task.resource)
                                .unwrap_or(OperationResource::Local);
                            app.conflict_responses.remove(&id);
                            queue_completed_focus(&mut app, &completed_targets);
                            let _ = app.operations.finish(id, terminal, result);
                            if cancelled {
                                app.operations.remove_terminal(id);
                            }
                            let next = app.operations.start_next(resource).ok().flatten().and_then(
                                |next_id| {
                                    let _ = app.operations.mark_running(next_id);
                                    app.operations
                                        .task(next_id)
                                        .map(|task| FileOperationRequest {
                                            id: next_id,
                                            kind: task.kind,
                                            resource: task.resource,
                                            items: task.items.clone(),
                                            cancellation: task.cancellation.clone(),
                                        })
                                },
                            );
                            (affected, next)
                        };
                        if let Some(request) = next {
                            let _ = sender.send(request);
                        }
                        refresh_affected_tabs(
                            &directory_sender,
                            &network_directory_sender,
                            &state,
                            &affected,
                        );
                    }
                }
                let rename_update = state.lock().ok().and_then(|app| {
                    let task = app.operations.task(event_operation_id)?;
                    (task.kind == FileOperationKind::Rename
                        && task.state != OperationState::Running)
                        .then(|| {
                            (
                                task.origin_tab
                                    .and_then(|tab_id| app.window_for_tab(tab_id)),
                                task.state == OperationState::Completed,
                                task.items.iter().find_map(|item| item.error.clone()),
                            )
                        })
                });
                if let Some((Some(window_id), completed, error)) = rename_update
                    && let Some(ui) = window_ui(window_id)
                {
                    if completed {
                        if let Ok(mut app) = state.lock() {
                            app.rename_targets.remove(&window_id);
                        }
                        ui.set_rename_submitting(false);
                        ui.set_rename_editing(false);
                    } else if let Some(message) = error {
                        if let Ok(mut app) = state.lock() {
                            app.operation_errors.push(message);
                        }
                        ui.set_rename_submitting(false);
                        ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
                    }
                }
                refresh_all_windows(&state);
                if state
                    .lock()
                    .is_ok_and(|app| app.exit_after_cancel && !app.operations.has_active_tasks())
                {
                    if let Some(operation_ui) = operation_weak.upgrade() {
                        let _ = operation_ui.hide();
                    }
                    if let Some(delete_ui) = delete_weak.upgrade() {
                        let _ = delete_ui.hide();
                    }
                    if let Some(conflict_ui) = conflict_weak.upgrade() {
                        let _ = conflict_ui.hide();
                    }
                    if let Some(exit_ui) = exit_weak.upgrade() {
                        let _ = exit_ui.hide();
                    }
                    hide_all_app_windows();
                }
                if let Some(operation_ui) = operation_weak.upgrade() {
                    refresh_operation_window(&operation_ui, &state);
                    let has_attention = state.lock().is_ok_and(|app| {
                        app.operations.iter().any(|task| {
                            matches!(
                                task.state,
                                OperationState::Failed
                                    | OperationState::PartiallyCompleted
                                    | OperationState::WaitingConflict
                            )
                        })
                    });
                    let has_active = state
                        .lock()
                        .is_ok_and(|app| app.operations.has_active_tasks());
                    let has_rows = state
                        .lock()
                        .is_ok_and(|app| app.operations.iter().next().is_some());
                    if !has_active && !has_attention && !has_rows {
                        let _ = operation_ui.hide();
                    }
                }
            });
        }
    });
}
fn start_directory_watchers(
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: SharedSessions,
) -> slint::Timer {
    use std::{cell::RefCell, rc::Rc};

    let (event_sender, event_receiver) = mpsc::channel();
    let failed_roots = Arc::new(Mutex::new(std::collections::HashSet::<PathBuf>::new()));
    let failed_for_timer = failed_roots.clone();
    let watchers = Rc::new(RefCell::new(HashMap::<
        PathBuf,
        platform::windows::directory_watch::DirectoryWatch,
    >::new()));
    let watchers_for_timer = watchers.clone();
    let state_for_timer = state.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(500),
        move || {
            let roots = state_for_timer
                .lock()
                .map(|app| watched_roots(&app))
                .unwrap_or_default();
            let mut watchers = watchers_for_timer.borrow_mut();
            if let Ok(mut failed) = failed_for_timer.lock() {
                for root in failed.drain() {
                    watchers.remove(&root);
                }
            }
            watchers.retain(|root, _| roots.contains(root));
            for root in roots {
                if watchers.contains_key(&root) {
                    continue;
                }
                if let Ok(watcher) = platform::windows::directory_watch::DirectoryWatch::start(
                    &root,
                    event_sender.clone(),
                ) {
                    watchers.insert(root, watcher);
                }
            }
        },
    );
    thread::spawn(move || {
        while let Ok(first) = event_receiver.recv() {
            if let platform::windows::directory_watch::DirectoryWatchEvent::Error {
                root,
                message,
            } = &first
            {
                eprintln!("directory watch failed for {}: {message}", root.display());
                if let Ok(mut failed) = failed_roots.lock() {
                    failed.insert(root.clone());
                }
            }
            let mut roots = std::collections::HashSet::from([watch_event_root(first)]);
            thread::sleep(Duration::from_millis(120));
            while let Ok(next) = event_receiver.try_recv() {
                roots.insert(watch_event_root(next));
            }
            let roots = roots.into_iter().collect::<Vec<_>>();
            let sender_for_ui = directory_sender.clone();
            let state_for_ui = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                // Network paths are excluded from watched_roots, so this refresh cannot reach SMB.
                for root in &roots {
                    let targets = {
                        let app = state_for_ui
                            .lock()
                            .expect("app state mutex is not poisoned");
                        app.windows
                            .values()
                            .flat_map(|window| window.tabs.values())
                            .filter_map(|tab| {
                                (tab.visible_path() == Some(root.as_path()))
                                    .then_some((tab.id, root.clone()))
                            })
                            .collect::<Vec<_>>()
                    };
                    for (tab, path) in targets {
                        submit_navigation(
                            &sender_for_ui,
                            &state_for_ui,
                            tab,
                            path,
                            NavigationKind::Refresh,
                        );
                    }
                }
            });
        }
    });
    timer
}
fn watched_roots(app: &AppState) -> std::collections::HashSet<PathBuf> {
    app.windows
        .values()
        .flat_map(|window| window.tabs.values())
        .filter_map(|tab| {
            tab.visible_path()
                .filter(|path| !crate::network::is_unc_path(path))
                .map(Path::to_path_buf)
        })
        .collect()
}

fn watch_event_root(event: platform::windows::directory_watch::DirectoryWatchEvent) -> PathBuf {
    use platform::windows::directory_watch::DirectoryWatchEvent;
    match event {
        DirectoryWatchEvent::Changes { root, .. }
        | DirectoryWatchEvent::Overflow { root }
        | DirectoryWatchEvent::Error { root, .. } => root,
    }
}
fn refresh_affected_tabs(
    sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    directories: &[PathBuf],
) {
    let targets = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.windows
            .values()
            .flat_map(|window| window.tabs.values())
            .filter_map(|tab| {
                tab.visible_path()
                    .filter(|path| directories.iter().any(|directory| directory == *path))
                    .map(|path| (tab.id, path.to_path_buf()))
            })
            .collect::<Vec<_>>()
    };
    for (tab, path) in targets {
        let pending = state
            .lock()
            .ok()
            .and_then(|mut app| app.focus_after_refresh.remove(&tab))
            .filter(|pending| pending.directory == path);
        if submit_path_navigation(
            sender,
            network_sender,
            state,
            tab,
            path.clone(),
            NavigationKind::Refresh,
        ) && let Some(mut pending) = pending
            && let Ok(mut app) = state.lock()
        {
            pending.request_id = app.tab(tab).map(|session| session.latest_request);
            app.focus_after_refresh.insert(tab, pending);
        }
    }
}

fn prepare_retry(state: &SharedSessions, id: OperationId) -> Option<FileOperationRequest> {
    let mut app = state.lock().ok()?;
    let resource = app.operations.task(id)?.resource;
    if !app.operations.retry(id) {
        return None;
    }
    let started = app.operations.start_next(resource).ok().flatten()?;
    app.operations.mark_running(started).ok()?;
    let task = app.operations.task(started)?;
    Some(FileOperationRequest {
        id: started,
        kind: task.kind,
        resource: task.resource,
        items: task.items.clone(),
        cancellation: task.cancellation.clone(),
    })
}
fn spawn_directory_workers(
    worker_count: usize,
    network_worker_count: usize,
) -> (
    mpsc::Sender<DirectoryRequest>,
    mpsc::SyncSender<DirectoryRequest>,
    mpsc::Receiver<DirectoryEvent>,
) {
    let network_worker_count = network_worker_count.max(1);
    let (request_sender, request_receiver) = mpsc::channel::<DirectoryRequest>();
    let (network_request_sender, network_request_receiver) =
        mpsc::sync_channel::<DirectoryRequest>(network_worker_count.saturating_mul(32));
    let request_receiver = Arc::new(Mutex::new(request_receiver));
    let (event_sender, event_receiver) = mpsc::channel::<DirectoryEvent>();
    for _ in 0..worker_count {
        let requests = request_receiver.clone();
        let events = event_sender.clone();
        thread::spawn(move || {
            loop {
                let request = requests
                    .lock()
                    .expect("directory request receiver mutex is not poisoned")
                    .recv();
                let Ok(request) = request else {
                    break;
                };
                run_directory_request(request, &events);
            }
        });
    }
    spawn_network_directory_scheduler(
        network_worker_count,
        network_request_receiver,
        event_sender.clone(),
    );
    (request_sender, network_request_sender, event_receiver)
}

fn spawn_network_directory_scheduler(
    worker_count: usize,
    requests: mpsc::Receiver<DirectoryRequest>,
    events: mpsc::Sender<DirectoryEvent>,
) {
    let (work_sender, work_receiver) = mpsc::channel::<(NetworkExecutionKey, DirectoryRequest)>();
    let (completion_sender, completion_receiver) = mpsc::channel::<NetworkDirectoryCompletion>();
    let work_receiver = Arc::new(Mutex::new(work_receiver));
    for _ in 0..worker_count {
        let work_receiver = work_receiver.clone();
        let completion_sender = completion_sender.clone();
        let events = events.clone();
        thread::spawn(move || {
            loop {
                let work = work_receiver
                    .lock()
                    .expect("network directory work receiver mutex is not poisoned")
                    .recv();
                let Ok((key, request)) = work else {
                    break;
                };
                let slow_cancel = request.cancel.clone();
                let slow_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let slow_done_for_timer = slow_done.clone();
                let slow_events = events.clone();
                let slow_tab_id = request.tab_id;
                let slow_request_id = request.request_id;
                thread::spawn(move || {
                    let started = Instant::now();
                    while started.elapsed() < Duration::from_secs(2) {
                        if slow_cancel.load(std::sync::atomic::Ordering::Acquire)
                            || slow_done_for_timer.load(std::sync::atomic::Ordering::Acquire)
                        {
                            return;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    let _ = slow_events.send(DirectoryEvent::Slow {
                        tab_id: slow_tab_id,
                        request_id: slow_request_id,
                    });
                });
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_directory_request(request, &events);
                }));
                slow_done.store(true, std::sync::atomic::Ordering::Release);
                if outcome.is_err() {
                    let _ = events.send(DirectoryEvent::Failed {
                        tab_id: slow_tab_id,
                        request_id: slow_request_id,
                        kind: io::ErrorKind::Other,
                        message: "network directory worker failed unexpectedly".to_owned(),
                    });
                }
                let _ = completion_sender.send(NetworkDirectoryCompletion { key });
            }
        });
    }
    thread::spawn(move || {
        let mut scheduler = NetworkDirectoryScheduler::default();
        let mut input_open = true;
        while input_open || !scheduler.pending.is_empty() || !scheduler.active.is_empty() {
            while let Ok(completion) = completion_receiver.try_recv() {
                scheduler.complete(&completion.key);
            }
            for request in scheduler.take_cancelled() {
                let _ = events.send(DirectoryEvent::Cancelled {
                    tab_id: request.tab_id,
                    request_id: request.request_id,
                });
            }
            while scheduler.active.len() < worker_count {
                let Some(work) = scheduler.next_ready() else {
                    break;
                };
                if work_sender.send(work).is_err() {
                    return;
                }
            }
            if scheduler.active.len() >= worker_count || !scheduler.pending.is_empty() {
                match completion_receiver.recv_timeout(Duration::from_millis(20)) {
                    Ok(completion) => scheduler.complete(&completion.key),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            } else if input_open {
                match requests.recv_timeout(Duration::from_millis(20)) {
                    Ok(request) => {
                        if let Some(key) = NetworkExecutionKey::from_unc(&request.path) {
                            scheduler.push(key, request);
                        } else {
                            let _ = events.send(DirectoryEvent::Failed {
                                tab_id: request.tab_id,
                                request_id: request.request_id,
                                kind: io::ErrorKind::InvalidInput,
                                message: "network directory request requires a UNC path".to_owned(),
                            });
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => input_open = false,
                }
            }
            while let Ok(request) = requests.try_recv() {
                if let Some(key) = NetworkExecutionKey::from_unc(&request.path) {
                    scheduler.push(key, request);
                } else {
                    let _ = events.send(DirectoryEvent::Failed {
                        tab_id: request.tab_id,
                        request_id: request.request_id,
                        kind: io::ErrorKind::InvalidInput,
                        message: "network directory request requires a UNC path".to_owned(),
                    });
                }
            }
        }
    });
}

fn read_network_root_batches(
    request: &DirectoryRequest,
    events: &mpsc::Sender<DirectoryEvent>,
) -> io::Result<ReadOutcome> {
    use std::sync::atomic::Ordering;

    if request.cancel.load(Ordering::Acquire) {
        return Ok(ReadOutcome::Cancelled);
    }
    let items = platform::windows::network::isolated_network_root(&request.path, &request.cancel)?;
    if request.cancel.load(Ordering::Acquire) {
        return Ok(ReadOutcome::Cancelled);
    }
    for (batch_index, items) in items.chunks(32).enumerate() {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(ReadOutcome::Cancelled);
        }
        let entries = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = batch_index
                    .saturating_mul(32)
                    .saturating_add(index)
                    .saturating_add(1)
                    .min(u32::MAX as usize) as u32;
                FileEntry {
                    id: EntryId(id),
                    original_name: crate::network::unc_leaf_name(&item.target)
                        .unwrap_or_else(|| item.label.clone().into()),
                    display_name: item.label.clone(),
                    name_highlights: Vec::new(),
                    path: item.target.clone(),
                    kind: crate::domain::EntryKind::Directory,
                    open_target: None,
                    parent_display: display_path(&request.path),
                    size_bytes: None,
                    folder_size: FolderSizeState::NotIndexed,
                    modified: None,
                    created: None,
                }
            })
            .collect();
        let _ = events.send(DirectoryEvent::Batch {
            tab_id: request.tab_id,
            request_id: request.request_id,
            entries,
        });
    }
    Ok(ReadOutcome::Complete { skipped: 0 })
}

fn read_network_directory_batches(
    request: &DirectoryRequest,
    events: &mpsc::Sender<DirectoryEvent>,
) -> io::Result<ReadOutcome> {
    use std::sync::atomic::Ordering;

    if request.cancel.load(Ordering::Acquire) {
        return Ok(ReadOutcome::Cancelled);
    }
    let (entries, skipped) = platform::windows::network::isolated_directory(
        &request.path,
        request.visibility,
        &request.cancel,
    )?;
    if request.cancel.load(Ordering::Acquire) {
        return Ok(ReadOutcome::Cancelled);
    }
    let first = entries.len().min(32);
    if first > 0 {
        let _ = events.send(DirectoryEvent::Batch {
            tab_id: request.tab_id,
            request_id: request.request_id,
            entries: entries[..first].to_vec(),
        });
    }
    for batch in entries[first..].chunks(256) {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(ReadOutcome::Cancelled);
        }
        let _ = events.send(DirectoryEvent::Batch {
            tab_id: request.tab_id,
            request_id: request.request_id,
            entries: batch.to_vec(),
        });
    }
    Ok(ReadOutcome::Complete { skipped })
}
fn run_directory_request(request: DirectoryRequest, events: &mpsc::Sender<DirectoryEvent>) {
    if crate::network::is_unc_server_root(&request.path) {
        platform::windows::network::record_runtime_event("network_root_request_started");
    }
    let result = if crate::network::is_unc_server_root(&request.path) {
        read_network_root_batches(&request, events)
    } else if crate::network::is_unc_path(&request.path) {
        read_network_directory_batches(&request, events)
    } else {
        read_directory_batches_filtered(
            &request.path,
            &request.cancel,
            request.visibility,
            |entries| {
                let _ = events.send(DirectoryEvent::Batch {
                    tab_id: request.tab_id,
                    request_id: request.request_id,
                    entries,
                });
            },
        )
    };
    if crate::network::is_unc_server_root(&request.path) {
        platform::windows::network::record_runtime_event(match &result {
            Ok(ReadOutcome::Complete { .. }) => "network_root_request_completed",
            Ok(ReadOutcome::Cancelled) => "network_root_request_cancelled",
            Err(_) => "network_root_request_failed",
        });
    }
    let event = match result {
        Ok(ReadOutcome::Complete { skipped }) => DirectoryEvent::Finished {
            tab_id: request.tab_id,
            request_id: request.request_id,
            path: request.path,
            skipped,
        },
        Ok(ReadOutcome::Cancelled) => DirectoryEvent::Cancelled {
            tab_id: request.tab_id,
            request_id: request.request_id,
        },
        Err(error) => DirectoryEvent::Failed {
            tab_id: request.tab_id,
            request_id: request.request_id,
            kind: error.kind(),
            message: error.to_string(),
        },
    };
    let _ = events.send(event);
}

fn start_event_pump(
    ui: &AppWindow,
    receiver: Arc<Mutex<mpsc::Receiver<DirectoryEvent>>>,
    icon_sender: mpsc::Sender<IconRequest>,
    everything_sender: mpsc::Sender<EverythingRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        loop {
            let event = receiver
                .lock()
                .expect("directory event receiver mutex is not poisoned")
                .recv();
            let Ok(event) = event else {
                break;
            };
            let state = state.clone();
            let icon_sender = icon_sender.clone();
            let everything_sender = everything_sender.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    let routed_tab = Some(event.request_identity().0);
                    let batch = match &event {
                        DirectoryEvent::Batch {
                            tab_id, request_id, ..
                        } => Some((*tab_id, *request_id)),
                        _ => None,
                    };
                    let finished = match &event {
                        DirectoryEvent::Finished {
                            tab_id, request_id, ..
                        } => Some((*tab_id, *request_id)),
                        _ => None,
                    };
                    let network_tab = routed_tab.is_some_and(|tab_id| {
                        state.lock().ok().is_some_and(|app| {
                            app.tab(tab_id)
                                .and_then(TabSession::visible_path)
                                .is_some_and(crate::network::is_unc_path)
                        })
                    });
                    let network_root_finished = finished.is_some_and(|(tab_id, request_id)| {
                        state.lock().ok().is_some_and(|app| {
                            app.tab(tab_id).is_some_and(|tab| {
                                tab.latest_request == request_id
                                    && tab
                                        .requested_path
                                        .as_deref()
                                        .is_some_and(crate::network::is_unc_server_root)
                            })
                        })
                    });
                    if network_root_finished {
                        platform::windows::network::record_runtime_event(
                            "network_root_event_apply_started",
                        );
                    }
                    let icon_requests = apply_event(&state, event);
                    if network_root_finished {
                        platform::windows::network::record_runtime_event(
                            "network_root_event_apply_completed",
                        );
                    }
                    if !network_tab {
                        for request in icon_requests {
                            let _ = icon_sender.send(request);
                        }
                    }
                    if let Some((tab_id, request_id)) = batch {
                        if let Some(window_id) =
                            state.lock().ok().and_then(|app| app.window_for_tab(tab_id))
                            && let Some(target_ui) = window_ui(window_id)
                        {
                            append_active_file_rows(
                                &target_ui,
                                &WindowSessions::new(state.clone(), window_id),
                                tab_id,
                                request_id,
                            );
                        } else {
                            append_active_file_rows(&ui, &state, tab_id, request_id);
                        }
                        if !network_tab {
                            defer_grid_thumbnails(state.clone(), tab_id, icon_sender.clone());
                        }
                    } else {
                        if let Some((tab_id, request_id)) = finished.filter(|_| !network_tab) {
                            let viewport = state
                                .lock()
                                .ok()
                                .and_then(|app| app.window_for_tab(tab_id))
                                .and_then(window_ui)
                                .map(|ui| (ui.get_file_viewport_y(), ui.get_file_viewport_height()))
                                .unwrap_or((0.0, 640.0));
                            submit_visible_folder_sizes(
                                &everything_sender,
                                &state,
                                tab_id,
                                request_id,
                                viewport.0,
                                viewport.1,
                            );
                        }
                        if let Some(tab_id) = routed_tab {
                            refresh_tab_window(&state, tab_id);
                        } else {
                            refresh_all_windows(&state);
                        }
                        if let Some((tab_id, request_id)) = finished {
                            reveal_focused_entry(&ui, &state, tab_id, request_id);
                        }
                        if network_root_finished {
                            platform::windows::network::record_runtime_event(
                                "network_root_ui_refresh_completed",
                            );
                        }
                    }
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn reveal_scroll_target(
    current: f32,
    entry_top: f32,
    entry_extent: f32,
    visible_height: f32,
    maximum: f32,
) -> f32 {
    let row_top = entry_top + current;
    let row_bottom = row_top + entry_extent;
    let viewport = if row_top < 0.0 {
        -entry_top
    } else if row_bottom > visible_height {
        visible_height - entry_top - entry_extent
    } else {
        current
    };
    viewport.clamp(-maximum, 0.0)
}
fn reveal_entry(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    entry_id: EntryId,
) {
    let snapshot = state.lock().ok().and_then(|app| {
        let window_id = app.window_for_tab(tab_id)?;
        let window = app.window(window_id)?;
        let tab = window.tabs.get(&tab_id)?;
        (window.active_tab == tab_id && tab.latest_request == request_id).then(|| {
            let entries = directory_display_entries(tab);
            (
                app.view_mode_for_tab(tab_id)
                    .unwrap_or(app.default_directory_view.view_mode),
                tab.page_source,
                tab.search_total,
                entries.iter().position(|entry| entry.id == entry_id),
                directory_group_projections(&app, tab, entries),
            )
        })
    });
    let Some((view_mode, page_source, search_total, entry_index, groups)) = snapshot else {
        return;
    };
    let columns = ui.get_grid_column_count().max(1) as usize;
    if page_source == PageSource::Search {
        let index = entry_id.0.saturating_sub(1);
        let maximum = search_logical_maximum(
            search_total.unwrap_or(0),
            view_mode,
            columns,
            ui.get_file_viewport_height(),
        );
        let logical_scroll =
            search_scroll_for_index(index, view_mode, columns).clamp(-maximum, 0.0);
        ui.set_search_scroll_y(logical_scroll);
        let window = search_window_for_index(index, search_total.unwrap_or(0), columns);
        ui.set_file_viewport_y(search_window_viewport_y(index, window, view_mode, columns));
        refresh_tab_window(state, tab_id);
        return;
    }

    let geometry = file_layout_geometry(view_mode);
    let entry_top = if geometry.grid {
        let projection =
            IconProjection::from_groups(&groups, columns, 32, geometry.row_height as u64);
        projection
            .entry_position(entry_id)
            .and_then(|position| projection.offsets.row_start(position.row_index))
            .map(|value| value as f32)
    } else {
        let projection = ListProjection::from_groups(&groups, 32, geometry.row_height as u64);
        projection
            .entry_position(entry_id)
            .and_then(|position| projection.offsets.row_start(position))
            .map(|value| value as f32)
    }
    .or_else(|| entry_index.map(|index| index as f32 * geometry.row_height));
    let Some(entry_top) = entry_top else {
        return;
    };
    let visible_height = ui.get_file_viewport_height().max(geometry.row_height);
    let maximum = projected_scroll_maximum(ui, view_mode, visible_height);
    ui.set_file_viewport_y(reveal_scroll_target(
        ui.get_file_viewport_y(),
        entry_top,
        geometry.row_height,
        visible_height,
        maximum,
    ));
}

fn reveal_focused_entry(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    const ROW_HEIGHT: f32 = 40.0;
    let index = state.lock().ok().and_then(|app| {
        (app.window_for_tab(tab_id) == Some(app.active_window)
            && app.active_window_state().active_tab == tab_id)
            .then(|| app.tab(tab_id))
            .flatten()
            .filter(|tab| tab.latest_request == request_id)
            .and_then(|tab| tab.focused.and_then(|id| tab.visible_entry_index(id)))
    });
    let Some(index) = index else {
        return;
    };
    let window_height = ui.window().size().height as f32 / ui.window().scale_factor();
    let visible_height = (window_height - ui.get_file_list_top() - 30.0).max(ROW_HEIGHT);
    let current = ui.get_file_viewport_y();
    let row_top = index as f32 * ROW_HEIGHT + current;
    let row_bottom = row_top + ROW_HEIGHT;
    let viewport = if row_top < 0.0 {
        -(index as f32 * ROW_HEIGHT)
    } else if row_bottom > visible_height {
        visible_height - (index as f32 + 1.0) * ROW_HEIGHT
    } else {
        current
    };
    let maximum = (ui.get_files().row_count() as f32 * ROW_HEIGHT - visible_height).max(0.0);
    ui.set_file_viewport_y(viewport.clamp(-maximum, 0.0));
}

fn apply_event(state: &SharedSessions, event: DirectoryEvent) -> Vec<IconRequest> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let mut icon_requests = Vec::new();
    match event {
        DirectoryEvent::Batch {
            tab_id,
            request_id,
            entries,
        } => {
            let accepted = app.tab(tab_id).is_some_and(|tab| tab.accepts(request_id));
            if accepted {
                let local_entries = entries
                    .iter()
                    .filter(|entry| !crate::network::is_unc_path(&entry.path));
                if !app
                    .view_mode_for_tab(tab_id)
                    .is_some_and(ViewMode::uses_grid_layout)
                {
                    icon_requests.extend(
                        local_entries
                            .filter(|entry| !app.icon_cache.contains_key(&entry.path))
                            .map(|entry| IconRequest {
                                tab_id,
                                request_id,
                                target: IconTarget::Entry(entry.id),
                                path: entry.path.clone(),
                                thumbnail: false,
                                requested_px: 0,
                            }),
                    );
                }
                app.tab_mut(tab_id)
                    .expect("accepted tab exists")
                    .append_pending(entries);
            }
        }
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path,
            skipped,
        } => {
            let accepted = app.tab(tab_id).is_some_and(|tab| tab.accepts(request_id));
            let focus = accepted
                .then(|| app.focus_after_refresh.get(&tab_id).cloned())
                .flatten()
                .filter(|pending| {
                    pending.request_id == Some(request_id) && pending.directory == path
                });
            if accepted {
                let consumed_focus = focus.is_some();
                let preference = app.directory_preference(&path);
                let location_path = {
                    let tab = app.tab_mut(tab_id).expect("accepted tab exists");
                    tab.sort_field = preference.sort_field;
                    tab.sort_direction = preference.sort_direction;
                    tab.sort_pending();
                    tab.commit_pending();
                    tab.commit_path(path);
                    if let Some(focus) = focus.as_ref() {
                        let ids = focus
                            .paths
                            .iter()
                            .filter_map(|target| {
                                tab.entries
                                    .iter()
                                    .find(|entry| entry.path == *target)
                                    .map(|entry| entry.id)
                            })
                            .collect::<Vec<_>>();
                        if let Some(focused) = ids.last().copied() {
                            tab.selected = ids;
                            tab.focused = Some(focused);
                            tab.selection_anchor = Some(focused);
                        }
                    }
                    tab.error = (skipped > 0).then(|| skipped.to_string());
                    tab.current_path.clone().expect("committed path exists")
                };
                if consumed_focus {
                    app.focus_after_refresh.remove(&tab_id);
                }
                if !crate::network::is_unc_path(&location_path) {
                    icon_requests.push(IconRequest {
                        tab_id,
                        request_id,
                        target: IconTarget::Location,
                        path: location_path,
                        thumbnail: false,
                        requested_px: 0,
                    });
                }
            }
        }
        DirectoryEvent::Cancelled { tab_id, request_id } => {
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.latest_request == request_id
            {
                tab.discard_pending();
                tab.load_state = LoadState::Cancelled;
            }
        }
        DirectoryEvent::Failed {
            tab_id,
            request_id,
            kind,
            message,
        } => {
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.accepts(request_id)
            {
                tab.discard_pending();
                tab.load_state = classify_error(kind);
                tab.error = Some(message);
            }
        }
        DirectoryEvent::Slow { tab_id, request_id } => {
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.accepts(request_id)
                && tab.load_state == LoadState::Loading
            {
                tab.error = Some("network_slow".to_owned());
            }
        }
    }
    icon_requests
}

fn platform_everything_config(
    config: &crate::domain::EverythingConfig,
) -> Option<platform::windows::everything::PlatformEverythingConfig> {
    Some(platform::windows::everything::PlatformEverythingConfig {
        executable_path: config.executable_path.clone()?,
        instance_name: config.instance_name.clone(),
        allow_start: config.allow_launch,
    })
}

#[derive(Debug)]
struct GroupedSearchPage {
    items: Vec<platform::windows::everything::EverythingSearchItem>,
    total: u32,
    file_total: u32,
    response_offsets_valid: bool,
}

fn search_grouped_page(
    client: &platform::windows::everything::EverythingClient,
    scope: (Option<PathBuf>, bool),
    query: String,
    sort: platform::windows::everything::EverythingSort,
    offset: u32,
    limit: u32,
    timeout: Duration,
) -> Result<GroupedSearchPage, platform::windows::everything::EverythingError> {
    use platform::windows::everything::{EverythingItemKind, EverythingSearchRequest};
    let (scope, recursive) = scope;

    let mut files = EverythingSearchRequest::new(query.clone(), scope.clone());
    files.recursive = recursive;
    files.item_kind = EverythingItemKind::Files;
    files.sort = sort;
    files.offset = offset;
    files.max_results = limit;
    let file_page = client.search(&files, timeout)?;
    let file_offset_valid = file_page.offset == files.offset;

    let mut folders = EverythingSearchRequest::new(query, scope);
    folders.recursive = recursive;
    folders.item_kind = EverythingItemKind::Folders;
    folders.sort = sort;
    folders.offset = offset.saturating_sub(file_page.total);
    folders.max_results = limit.saturating_sub(file_page.items.len() as u32);
    let folder_page = client.search(&folders, timeout)?;
    let folder_offset_valid = folder_page.offset == folders.offset;

    let total = file_page.total.saturating_add(folder_page.total);
    let file_total = file_page.total;
    let mut items = file_page.items;
    if offset >= file_page.total || items.len() < limit as usize {
        items.extend(folder_page.items);
    }
    items.truncate(limit as usize);
    Ok(GroupedSearchPage {
        items,
        total,
        file_total,
        response_offsets_valid: file_offset_valid && folder_offset_valid,
    })
}

fn everything_status(
    client: Option<&platform::windows::everything::EverythingClient>,
    timeout: Duration,
) -> Result<
    platform::windows::everything::EverythingStatus,
    platform::windows::everything::EverythingError,
> {
    let status = client
        .ok_or(platform::windows::everything::EverythingError::NotConfigured)?
        .status(timeout)?;
    if status.database_loaded {
        Ok(status)
    } else {
        Err(platform::windows::everything::EverythingError::DatabaseNotLoaded)
    }
}

fn wait_for_everything_status(
    client: &platform::windows::everything::EverythingClient,
) -> Result<
    platform::windows::everything::EverythingStatus,
    platform::windows::everything::EverythingError,
> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match client.status(Duration::from_millis(750)) {
            Ok(status) if status.database_loaded => return Ok(status),
            Ok(_) if Instant::now() >= deadline => {
                return Err(platform::windows::everything::EverythingError::DatabaseNotLoaded);
            }
            Ok(_) => thread::sleep(Duration::from_millis(250)),
            Err(error @ platform::windows::everything::EverythingError::UnsupportedVersion(_))
            | Err(
                error @ platform::windows::everything::EverythingError::UnsupportedArchitecture,
            )
            | Err(error @ platform::windows::everything::EverythingError::InvalidExecutable(_)) => {
                return Err(error);
            }
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }
}
fn spawn_everything_worker(
    config: crate::domain::EverythingConfig,
    state: SharedSessions,
) -> (
    mpsc::Sender<EverythingRequest>,
    mpsc::Receiver<EverythingEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<EverythingRequest>();
    let (event_sender, event_receiver) = mpsc::channel::<EverythingEvent>();
    let (folder_sender, folder_receiver) =
        mpsc::sync_channel::<FolderSizeWork>(FOLDER_SIZE_QUEUE_CAPACITY);
    let folder_events = event_sender.clone();
    let folder_config = config.clone();
    let folder_state = state.clone();
    thread::spawn(move || {
        let mut client = platform_everything_config(&folder_config)
            .and_then(|value| platform::windows::everything::EverythingClient::new(value).ok());
        while let Ok(first) = folder_receiver.recv() {
            let mut batch = vec![first];
            while batch.len() < FOLDER_SIZE_QUEUE_CAPACITY {
                match folder_receiver.try_recv() {
                    Ok(work) => batch.push(work),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                }
            }
            batch.sort_by_key(|work| match work {
                FolderSizeWork::Query { tab_id, .. } => {
                    let active = folder_state.lock().ok().is_some_and(|app| {
                        app.window_for_tab(*tab_id) == Some(app.active_window)
                            && app.active_window_state().active_tab == *tab_id
                    });
                    !active
                }
                FolderSizeWork::Configure(_) => false,
            });
            for work in batch {
                match work {
                    FolderSizeWork::Query { tab_id, query } => {
                        let valid = folder_state.lock().ok().is_some_and(|app| {
                            app.tab(tab_id).is_some_and(|tab| {
                                tab.accepts_page(query.request_id, PageSource::Directory)
                                    && tab.folder_sizes.accepts(&query)
                            })
                        });
                        if !valid {
                            continue;
                        }
                        let state =
                            client
                                .as_ref()
                                .map_or(FolderSizeState::Disconnected, |client| {
                                    folder_size_state(
                                        client.folder_size(&query.key.path, Duration::from_secs(2)),
                                    )
                                });
                        let _ = folder_events.send(EverythingEvent::FolderSize {
                            tab_id,
                            query,
                            state,
                        });
                    }
                    FolderSizeWork::Configure(config) => {
                        client = platform_everything_config(&config).and_then(|value| {
                            platform::windows::everything::EverythingClient::new(value).ok()
                        });
                    }
                }
            }
        }
    });
    thread::spawn(move || {
        let mut client = platform_everything_config(&config)
            .and_then(|value| platform::windows::everything::EverythingClient::new(value).ok());
        while let Ok(request) = request_receiver.recv() {
            match request {
                EverythingRequest::Search {
                    tab_id,
                    request_id,
                    scope,
                    depth,
                    query,
                    sort,
                    offset,
                    cancel,
                } => {
                    let Some(client) = client.as_ref() else {
                        let _ = event_sender.send(EverythingEvent::SearchFailed {
                            tab_id,
                            request_id,
                            offset,
                            error: platform::windows::everything::EverythingError::NotConfigured,
                        });
                        continue;
                    };
                    let scope = match scope {
                        SearchScope::Global => None,
                        SearchScope::Directory(path) => Some(path),
                    };
                    let recursive = depth == SearchDepth::Recursive;
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        let _ = event_sender.send(EverythingEvent::SearchSkipped {
                            tab_id,
                            request_id,
                            offset,
                        });
                        continue;
                    }
                    let page_result = search_grouped_page(
                        client,
                        (scope, recursive),
                        query,
                        sort,
                        offset,
                        SEARCH_PAGE_LIMIT,
                        Duration::from_secs(3),
                    );
                    match page_result {
                        Ok(page) => {
                            let GroupedSearchPage {
                                items,
                                total,
                                file_total,
                                response_offsets_valid,
                            } = page;
                            let entries = items
                                .into_iter()
                                .enumerate()
                                .map(|(index, item)| FileEntry {
                                    id: EntryId(
                                        offset.saturating_add(index as u32).saturating_add(1),
                                    ),
                                    display_name: item.name.to_string_lossy().into_owned(),
                                    name_highlights: item
                                        .name_highlights
                                        .into_iter()
                                        .map(|segment| NameHighlightSegment {
                                            text: segment.text,
                                            highlighted: segment.highlighted,
                                        })
                                        .collect(),
                                    original_name: item.name,
                                    path: item.path,
                                    kind: if item.is_directory {
                                        crate::domain::EntryKind::Directory
                                    } else {
                                        crate::domain::EntryKind::File
                                    },
                                    open_target: None,
                                    parent_display: item
                                        .parent
                                        .as_os_str()
                                        .to_string_lossy()
                                        .into_owned(),
                                    size_bytes: item.size,
                                    folder_size: item
                                        .size
                                        .map(FolderSizeState::Value)
                                        .unwrap_or(FolderSizeState::NotIndexed),
                                    modified: item.modified,
                                    created: item.created,
                                })
                                .collect::<Vec<_>>();
                            if !cancel.load(std::sync::atomic::Ordering::Acquire) {
                                let response_valid = response_offsets_valid
                                    && !(offset < total && entries.is_empty());
                                let _ = event_sender.send(EverythingEvent::SearchPage {
                                    tab_id,
                                    request_id,
                                    offset,
                                    entries,
                                    total,
                                    file_total,
                                    response_valid,
                                });
                            } else {
                                let _ = event_sender.send(EverythingEvent::SearchSkipped {
                                    tab_id,
                                    request_id,
                                    offset,
                                });
                            }
                        }
                        Err(error) => {
                            let _ = event_sender.send(EverythingEvent::SearchFailed {
                                tab_id,
                                request_id,
                                offset,
                                error,
                            });
                        }
                    }
                }
                EverythingRequest::FolderSize { tab_id, query } => {
                    match folder_sender.try_send(FolderSizeWork::Query {
                        tab_id,
                        query: query.clone(),
                    }) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(_)) => {
                            let _ = event_sender.send(EverythingEvent::FolderSize {
                                tab_id,
                                query,
                                state: FolderSizeState::TimedOut,
                            });
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            let _ = event_sender.send(EverythingEvent::FolderSize {
                                tab_id,
                                query,
                                state: FolderSizeState::Disconnected,
                            });
                        }
                    }
                }
                EverythingRequest::Configure(config) => {
                    let _ = folder_sender.send(FolderSizeWork::Configure(config.clone()));
                    client = platform_everything_config(&config).and_then(|value| {
                        platform::windows::everything::EverythingClient::new(value).ok()
                    });
                }
                EverythingRequest::Discover(generation) => {
                    let installations = platform::windows::everything::EverythingClient::discover();
                    let usable = |item: &&platform::windows::everything::EverythingInstallation| {
                        item.executable_path.is_file()
                    };
                    let installation = installations
                        .iter()
                        .filter(usable)
                        .find(|item| item.running && item.instance_name == "1.5a")
                        .or_else(|| {
                            installations
                                .iter()
                                .filter(usable)
                                .find(|item| item.running)
                        })
                        .or_else(|| installations.iter().find(|item| usable(item)))
                        .cloned();
                    let Some(installation) = installation else {
                        let _ = event_sender.send(EverythingEvent::Status {
                            generation,
                            result: Err(
                                platform::windows::everything::EverythingError::NotConfigured,
                            ),
                        });
                        continue;
                    };
                    let configured = crate::domain::EverythingConfig {
                        executable_path: (!installation.executable_path.as_os_str().is_empty())
                            .then_some(installation.executable_path),
                        instance_name: installation.instance_name,
                        verified_version: None,
                        allow_launch: true,
                    };
                    client = platform_everything_config(&configured).and_then(|value| {
                        platform::windows::everything::EverythingClient::new(value).ok()
                    });
                    match everything_status(client.as_ref(), Duration::from_secs(2)) {
                        Ok(status) => {
                            let _ =
                                folder_sender.send(FolderSizeWork::Configure(configured.clone()));
                            let _ = event_sender.send(EverythingEvent::Discovered {
                                generation,
                                config: configured,
                                status,
                            });
                        }
                        Err(error) => {
                            let _ = event_sender.send(EverythingEvent::Status {
                                generation,
                                result: Err(error),
                            });
                        }
                    }
                }
                EverythingRequest::TestConnection(generation) => {
                    let result = everything_status(client.as_ref(), Duration::from_secs(2));
                    let _ = event_sender.send(EverythingEvent::Status { generation, result });
                }
                EverythingRequest::Start(generation) => {
                    let result = client
                        .as_ref()
                        .ok_or(platform::windows::everything::EverythingError::NotConfigured)
                        .and_then(|client| {
                            client.start()?;
                            wait_for_everything_status(client)
                        });
                    let _ = event_sender.send(EverythingEvent::Status { generation, result });
                }
                EverythingRequest::PickExecutable {
                    generation,
                    owner_window,
                } => {
                    let picker_events = event_sender.clone();
                    thread::spawn(move || {
                        let result =
                            platform::windows::everything_file_picker::pick_everything_executable(
                                owner_window,
                            );
                        let _ = picker_events
                            .send(EverythingEvent::ExecutablePicked { generation, result });
                    });
                }
            }
        }
    });
    (request_sender, event_receiver)
}

fn folder_size_state(
    result: Result<
        platform::windows::everything::EverythingFolderSize,
        platform::windows::everything::EverythingError,
    >,
) -> FolderSizeState {
    use platform::windows::everything::{EverythingError, EverythingFolderSize};

    match result {
        Ok(EverythingFolderSize::Indexed(value)) => FolderSizeState::Value(value),
        Ok(EverythingFolderSize::NotIndexed) => FolderSizeState::NotIndexed,
        Err(EverythingError::Timeout) => FolderSizeState::TimedOut,
        Err(
            EverythingError::NotConfigured
            | EverythingError::NotRunning(_)
            | EverythingError::FolderSizePipeUnavailable(_)
            | EverythingError::FolderSizeDisconnected,
        ) => FolderSizeState::Disconnected,
        Err(EverythingError::FolderSizeRejected(404)) => FolderSizeState::NotFound,
        Err(EverythingError::FolderSizeRejected(_)) => FolderSizeState::ProtocolError,
        Err(EverythingError::Protocol(message)) if protocol_reports_not_found(&message) => {
            FolderSizeState::NotFound
        }
        Err(EverythingError::Protocol(_) | EverythingError::QueryRejected) => {
            FolderSizeState::ProtocolError
        }
        Err(_) => FolderSizeState::ProtocolError,
    }
}

fn protocol_reports_not_found(message: &str) -> bool {
    message
        .split(|character: char| !character.is_ascii_digit())
        .any(|part| part == "404")
}
fn with_folder_scheduler<T>(
    tab: &mut TabSession,
    action: impl FnOnce(
        &mut crate::domain::folder_size_scheduler::FolderSizeScheduler,
        &mut Vec<FileEntry>,
    ) -> T,
) -> T {
    let mut scheduler = std::mem::take(&mut tab.folder_sizes);
    let result = action(&mut scheduler, Arc::make_mut(&mut tab.entries));
    tab.folder_sizes = scheduler;
    result
}

fn cancel_folder_sizes(tab: &mut TabSession) {
    let request_id = tab.latest_request;
    with_folder_scheduler(tab, |scheduler, entries| {
        for entry in entries {
            if entry.folder_size == FolderSizeState::Querying {
                entry.folder_size = FolderSizeState::Unknown;
            }
        }
        scheduler.cancel(request_id);
    });
}

fn start_folder_size_query(app: &mut AppState, tab_id: TabId, query: &FolderSizeQuery) -> bool {
    let Some(tab) = app.tab_mut(tab_id) else {
        return false;
    };
    tab.accepts_page(query.request_id, PageSource::Directory) && tab.folder_sizes.start(query)
}

fn apply_folder_size_event(
    app: &mut AppState,
    tab_id: TabId,
    query: &FolderSizeQuery,
    state: FolderSizeState,
) -> FolderSizeCommit {
    let Some(tab) = app.tab_mut(tab_id) else {
        return FolderSizeCommit::Ignored;
    };
    if !tab.accepts_page(query.request_id, PageSource::Directory) {
        return FolderSizeCommit::Ignored;
    }
    with_folder_scheduler(tab, |scheduler, entries| {
        scheduler.complete(query, state, entries)
    })
}

#[cfg(test)]
fn apply_search_page_event(
    app: &mut AppState,
    tab_id: TabId,
    request_id: RequestId,
    offset: u32,
    entries: Vec<FileEntry>,
    total: u32,
    file_total: u32,
) -> Option<()> {
    let tab = app.tab_mut(tab_id)?;
    if !tab.accepts_page(request_id, PageSource::Search) {
        return None;
    }
    tab.finish_search_page_request(offset);
    tab.merge_search_page(offset, entries, total, file_total, SEARCH_PAGE_LIMIT);
    Some(())
}
fn start_everything_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<EverythingEvent>,
    search_sender: mpsc::Sender<EverythingRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            let sender_for_search_consistency = search_sender.clone();
            if weak.upgrade_in_event_loop(move |ui| {
            let routed_tab = match &event {
                EverythingEvent::SearchPage { tab_id, .. }
                | EverythingEvent::SearchFailed { tab_id, .. }
                | EverythingEvent::SearchSkipped { tab_id, .. } => Some(*tab_id),
                EverythingEvent::FolderSize { .. }
                | EverythingEvent::Status { .. }
                | EverythingEvent::Discovered { .. }
                | EverythingEvent::ExecutablePicked { .. } => None,
            };
            let mut folder_size_update = None;
            let mut app = state.lock().expect("app state mutex is not poisoned");
            match event {
                EverythingEvent::SearchPage { tab_id, request_id, offset, entries, total, file_total, response_valid } => if let Some(tab) = app.tab_mut(tab_id) && tab.accepts_page(request_id, PageSource::Search) {
                    if !response_valid {
                        let retry = tab.retry_unexpected_search_page(offset);
                        let retry_request = retry
                            .then(|| tab.search_cancel_token())
                            .flatten()
                            .map(|cancel| EverythingRequest::Search {
                                tab_id,
                                request_id,
                                scope: tab.search_scope.clone(),
                                depth: tab.search_depth,
                                query: tab.search_query.clone(),
                                sort: everything_sort(
                                    tab.search_sort_field,
                                    tab.search_sort_direction,
                                ),
                                offset,
                                cancel,
                            });
                        eprintln!(
                            "everything search page rejected tab={} request={} offset={} count={} total={} retry={}",
                            tab_id.0,
                            request_id.0,
                            offset,
                            entries.len(),
                            total,
                            retry
                        );
                        if !retry {
                            tab.error = Some(format!(
                                "Everything returned an invalid page at offset {offset}"
                            ));
                        }
                        drop(app);
                        if let Some(request) = retry_request {
                            let _ = sender_for_search_consistency.send(request);
                        }
                        return;
                    }
                    let pending_offset = tab.finish_search_page_request(offset);
                    if tab.search_file_total.is_some_and(|known| known != file_total) {
                        let scope = tab.search_scope.clone();
                        let depth = tab.search_depth;
                        let query = tab.search_query.clone();
                        let sort = everything_sort(tab.search_sort_field, tab.search_sort_direction);
                        let (request_id, cancel) = tab.begin_search(scope.clone(), query.clone());
                        drop(app);
                        let _ = sender_for_search_consistency.send(EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope,
                            depth,
                            query,
                            sort,
                            offset: 0,
                            cancel,
                        });
                        return;
                    }
                    tab.merge_search_page(
                        offset,
                        entries,
                        total,
                        file_total,
                        SEARCH_PAGE_LIMIT,
                    );
                    let pending_request = pending_offset.and_then(|offset| {
                        tab.search_cancel_token().map(|cancel| EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope: tab.search_scope.clone(),
                            depth: tab.search_depth,
                            query: tab.search_query.clone(),
                            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
                            offset,
                            cancel,
                        })
                    });
                    let active = app.window_for_tab(tab_id) == Some(app.active_window)
                        && app.active_window_state_mut().active_tab == tab_id;
                    drop(app);
                    if let Some(request) = pending_request {
                        let _ = sender_for_search_consistency.send(request);
                    }
                    if active {
                        project_search_page(&ui, &state, tab_id, request_id, offset, total);

                    }
                    return;
                },
                EverythingEvent::SearchFailed { tab_id, request_id, offset, error } => if let Some(tab) = app.tab_mut(tab_id) && tab.accepts_page(request_id, PageSource::Search) {
                    let pending_offset = tab.finish_search_page_request(offset);
                    let pending_request = pending_offset.and_then(|offset| {
                        tab.search_cancel_token().map(|cancel| EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope: tab.search_scope.clone(),
                            depth: tab.search_depth,
                            query: tab.search_query.clone(),
                            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
                            offset,
                            cancel,
                        })
                    });
                    tab.discard_pending();
                    tab.search_state = match error {
                        platform::windows::everything::EverythingError::NotConfigured
                        | platform::windows::everything::EverythingError::InvalidExecutable(_) => SearchState::NotConfigured,
                        platform::windows::everything::EverythingError::NotRunning(_) => SearchState::Disconnected,
                        platform::windows::everything::EverythingError::UnsupportedVersion(_) => SearchState::UnsupportedVersion,
                        platform::windows::everything::EverythingError::UnsupportedArchitecture => SearchState::UnsupportedArchitecture,
                        platform::windows::everything::EverythingError::DatabaseNotLoaded => SearchState::NotIndexed,
                        platform::windows::everything::EverythingError::QueryRejected => SearchState::SyntaxError,
                        platform::windows::everything::EverythingError::Timeout => SearchState::TimedOut,
                        _ => SearchState::Failed,
                    };
                    tab.error = Some(error.to_string());
                    if let Some(request) = pending_request {
                        let _ = sender_for_search_consistency.send(request);
                    }
                },
                EverythingEvent::SearchSkipped { tab_id, request_id, offset } => {
                    if let Some(tab) = app.tab_mut(tab_id)
                        && tab.accepts_page(request_id, PageSource::Search)
                        && let Some(offset) = tab.finish_search_page_request(offset)
                        && let Some(cancel) = tab.search_cancel_token()
                    {
                        let _ = sender_for_search_consistency.send(EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope: tab.search_scope.clone(),
                            depth: tab.search_depth,
                            query: tab.search_query.clone(),
                            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
                            offset,
                            cancel,
                        });
                    }
                    return;
                },
                EverythingEvent::FolderSize { tab_id, query, state: size } => {
                    let commit = apply_folder_size_event(&mut app, tab_id, &query, size);
                    let next = app
                        .tab_mut(tab_id)
                        .map(|tab| {
                            with_folder_scheduler(tab, |scheduler, entries| match commit {
                                FolderSizeCommit::Staged => scheduler.next_complete_queries(entries),
                                FolderSizeCommit::Visible(_) => scheduler.next_visible_queries(entries),
                                FolderSizeCommit::Ignored | FolderSizeCommit::CompleteSort => Vec::new(),
                            })
                        })
                        .unwrap_or_default();
                    for query in next {
                        if start_folder_size_query(&mut app, tab_id, &query) {
                            let _ = sender_for_search_consistency.send(
                                EverythingRequest::FolderSize { tab_id, query }
                            );
                        }
                    }
                    match commit {
                        FolderSizeCommit::Visible(entry_id) => {
                            folder_size_update = Some((tab_id, HashSet::from([entry_id])));
                        }
                        FolderSizeCommit::CompleteSort => {
                            if let Some(tab) = app.tab_mut(tab_id) {
                                tab.resort_entries();
                                folder_size_update = Some((
                                    tab_id,
                                    tab.entries.iter().map(|entry| entry.id).collect(),
                                ));
                            }
                        }
                        FolderSizeCommit::Staged => {
                            folder_size_update = Some((tab_id, HashSet::new()));
                        }
                        FolderSizeCommit::Ignored => {}
                    }
                }
                EverythingEvent::Status { generation, result }
                    if generation == app.everything_generation =>
                {
                    app.everything_busy = false;
                    match result {
                        Ok(status) => {
                            app.everything_status =
                                everything_connected_status(app.language, &status);
                            app.everything_folder_sizes_indexed = Some(status.folder_size_indexed);
                            app.everything_config.verified_version =
                                Some(status.version.to_string());
                        }
                        Err(error) => {
                            app.everything_status = everything_error_text(app.language, &error);
                            app.everything_folder_sizes_indexed = None;
                        }
                    }
                }
                EverythingEvent::Discovered { generation, config, status }
                    if generation == app.everything_generation =>
                {
                    app.everything_busy = false;
                    apply_everything_discovery(&mut app, generation, config, &status);
                }
                EverythingEvent::ExecutablePicked { generation, result }
                    if generation == app.everything_generation =>
                {
                    app.everything_busy = false;
                    match result {
                        Ok(Some(path)) => {
                            app.everything_config.executable_path = Some(path);
                            app.everything_config.verified_version = None;
                            app.everything_generation = app.everything_generation.saturating_add(1);
                            app.everything_status.clear();
                            app.everything_folder_sizes_indexed = None;
                            let _ = sender_for_search_consistency.send(EverythingRequest::Configure(
                                app.everything_config.clone(),
                            ));
                        }
                        Ok(None) => {}
                        Err(_) => {
                            app.everything_status = everything_picker_error_text(app.language).to_owned();
                        }
                    }
                }
                EverythingEvent::Status { .. }
                | EverythingEvent::Discovered { .. }
                | EverythingEvent::ExecutablePicked { .. } => {}
            }
            drop(app);
            if let Some((tab_id, changed)) = folder_size_update {
                if let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id))
                    && let Some(target_ui) = window_ui(window_id)
                {
                    update_file_rows(&target_ui, &state, tab_id, &changed);
                    update_tab_status(&target_ui, &state, tab_id);
                }
                return;
            }
            if let Some(tab_id) = routed_tab {
                refresh_tab_window(&state, tab_id);
            } else {
                refresh_all_windows(&state);
            }
        }).is_err() { break; }
        }
    });
}

fn empty_file_row() -> FileRow {
    FileRow {
        id: 0,
        loaded: false,
        group_header: false,
        group_label: "".into(),
        group_count: 0,
        name: "".into(),
        name_segments: ModelRc::new(VecModel::default()),
        kind: "".into(),
        parent_path: "".into(),
        size: "".into(),
        modified: "".into(),
        created: "".into(),
        is_directory: false,
        selected: false,
        focused: false,
        cut: false,
        icon: Image::default(),
    }
}

fn group_header_file_row(label: &str, entry_count: usize) -> FileRow {
    let mut row = empty_file_row();
    row.loaded = true;
    row.group_header = true;
    row.group_label = label.into();
    row.group_count = entry_count.min(i32::MAX as usize) as i32;
    row
}
fn project_search_page(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    offset: u32,
    total: u32,
) {
    let should_refresh = state.lock().ok().is_some_and(|app| {
        app.window_for_tab(tab_id) == Some(app.active_window)
            && app.active_window_state().active_tab == tab_id
            && app.tab(tab_id).is_some_and(|tab| {
                tab.latest_request == request_id && tab.page_source == PageSource::Search
            })
    });
    if !should_refresh {
        return;
    }
    if offset == 0 {
        ui.set_file_viewport_y(0.0);
        ui.set_search_scroll_y(0.0);
    }
    refresh_tab_window(state, tab_id);
    ui.set_page_state(if total == 0 { 3 } else { 4 });
}

fn everything_sort(
    field: SortField,
    direction: SortDirection,
) -> platform::windows::everything::EverythingSort {
    use platform::windows::everything::EverythingSort;
    match (field, direction) {
        (SortField::Name, SortDirection::Ascending) => EverythingSort::NameAscending,
        (SortField::Name, SortDirection::Descending) => EverythingSort::NameDescending,
        (SortField::Kind, SortDirection::Ascending) => EverythingSort::ExtensionAscending,
        (SortField::Kind, SortDirection::Descending) => EverythingSort::ExtensionDescending,
        (SortField::Size, SortDirection::Ascending) => EverythingSort::SizeAscending,
        (SortField::Size, SortDirection::Descending) => EverythingSort::SizeDescending,
        (SortField::Modified, SortDirection::Ascending) => EverythingSort::ModifiedAscending,
        (SortField::Modified, SortDirection::Descending) => EverythingSort::ModifiedDescending,
        (SortField::Created, SortDirection::Ascending) => EverythingSort::CreatedAscending,
        (SortField::Created, SortDirection::Descending) => EverythingSort::CreatedDescending,
    }
}
fn submit_search(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    ui: Option<&AppWindow>,
    tab_id: TabId,
    query: String,
) {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.cancel_column_drag();
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        let scope = tab.search_scope.clone();
        let sort = everything_sort(tab.search_sort_field, tab.search_sort_direction);
        let (request_id, cancel) = tab.begin_search(scope.clone(), query.clone());
        EverythingRequest::Search {
            tab_id,
            request_id,
            scope,
            depth: tab.search_depth,
            query,
            sort,
            offset: 0,
            cancel,
        }
    };
    if let Some(ui) = ui {
        ui.set_file_viewport_y(0.0);
        ui.set_search_scroll_y(0.0);
        refresh_tab_window(state, tab_id);
    }
    let _ = sender.send(request);
}

fn submit_search_page(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    offset: u32,
) {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        let page = offset / SEARCH_PAGE_LIMIT * SEARCH_PAGE_LIMIT;
        let previous = page.saturating_sub(SEARCH_PAGE_LIMIT);
        let next = page.saturating_add(SEARCH_PAGE_LIMIT);
        let Some(offset) = tab.queue_search_pages(&[page, previous, next], SEARCH_PAGE_LIMIT)
        else {
            return;
        };
        let Some(cancel) = tab.search_cancel_token() else {
            return;
        };
        EverythingRequest::Search {
            tab_id,
            request_id: tab.latest_request,
            scope: tab.search_scope.clone(),
            depth: tab.search_depth,
            query: tab.search_query.clone(),
            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
            offset,
            cancel,
        }
    };
    let _ = sender.send(request);
}
fn submit_folder_size_queries(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    queries: Vec<FolderSizeQuery>,
) {
    for query in queries {
        let started = state
            .lock()
            .ok()
            .is_some_and(|mut app| start_folder_size_query(&mut app, tab_id, &query));
        if !started {
            continue;
        }
        if sender
            .send(EverythingRequest::FolderSize {
                tab_id,
                query: query.clone(),
            })
            .is_err()
            && let Ok(mut app) = state.lock()
            && let Some(tab) = app.tab_mut(tab_id)
        {
            with_folder_scheduler(tab, |scheduler, entries| {
                scheduler.reject(&query, entries);
            });
        }
    }
}

fn submit_visible_folder_sizes(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    viewport_y: f32,
    viewport_height: f32,
) {
    const ROW_HEIGHT: f32 = 40.0;
    let queries = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.everything_folder_sizes_indexed == Some(false) {
            return;
        }
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        if tab.sort_field == SortField::Size {
            with_folder_scheduler(tab, |scheduler, entries| {
                scheduler.begin_complete_sort(request_id, entries)
            })
        } else {
            let first_row = ((-viewport_y).max(0.0) / ROW_HEIGHT).floor() as usize;
            let visible_rows = (viewport_height.max(ROW_HEIGHT) / ROW_HEIGHT).ceil() as usize + 1;
            with_folder_scheduler(tab, |scheduler, entries| {
                scheduler.visible_queries(request_id, entries, first_row, visible_rows)
            })
        }
    };
    submit_folder_size_queries(sender, state, tab_id, queries);
}
fn spawn_icon_workers(
    worker_count: usize,
    state: SharedSessions,
) -> (mpsc::Sender<IconRequest>, mpsc::Receiver<IconEvent>) {
    let (request_sender, request_receiver) = mpsc::channel::<IconRequest>();
    let request_receiver = Arc::new(Mutex::new(request_receiver));
    let (event_sender, event_receiver) = mpsc::channel::<IconEvent>();
    for _ in 0..worker_count {
        let requests = request_receiver.clone();
        let events = event_sender.clone();
        let state = state.clone();
        thread::spawn(move || {
            let _shell_apartment = platform::windows_shell_icons::initialize_shell_worker().ok();
            loop {
                let request = requests
                    .lock()
                    .expect("icon request receiver mutex is not poisoned")
                    .recv();
                let Ok(request) = request else {
                    break;
                };
                let is_current = state.lock().ok().is_some_and(|app| {
                    app.tab(request.tab_id)
                        .is_some_and(|tab| tab.latest_request == request.request_id)
                        && (!request.thumbnail
                            || app.thumbnail_requests.contains(&(
                                request.tab_id,
                                request.request_id,
                                request.path.clone(),
                                request.requested_px,
                            )))
                });
                if !is_current {
                    continue;
                }
                let cached = state.lock().ok().and_then(|app| {
                    if request.thumbnail {
                        app.thumbnail_cache
                            .get(&(request.path.clone(), request.requested_px))
                            .or_else(|| {
                                app.large_icon_cache
                                    .get(&(request.path.clone(), request.requested_px))
                            })
                            .cloned()
                    } else {
                        app.icon_cache.get(&request.path).cloned()
                    }
                });
                let (icon, actual_thumbnail) = if request.thumbnail {
                    if let Some(cached) = cached {
                        let actual = state.lock().is_ok_and(|app| {
                            app.thumbnail_cache
                                .contains_key(&(request.path.clone(), request.requested_px))
                        });
                        (Some(cached), actual)
                    } else if let Some(thumbnail) =
                        platform::windows_shell_icons::shell_thumbnail_rgba(
                            &request.path,
                            request.requested_px,
                            true,
                        )
                        .ok()
                        .filter(|thumbnail| {
                            thumbnail.image.width.max(thumbnail.image.height)
                                >= request.requested_px
                        })
                        .or_else(|| {
                            platform::windows_shell_icons::shell_thumbnail_rgba(
                                &request.path,
                                request.requested_px,
                                false,
                            )
                            .ok()
                        })
                    {
                        let _source = thumbnail.source;
                        (Some(thumbnail.image), true)
                    } else {
                        (
                            platform::windows_shell_icons::shell_large_icon_rgba(
                                &request.path,
                                request.requested_px,
                            )
                            .ok(),
                            false,
                        )
                    }
                } else {
                    (
                        cached.or_else(|| {
                            platform::windows_shell_icons::shell_icon_rgba(&request.path).ok()
                        }),
                        false,
                    )
                };
                if let Some(icon) = icon {
                    let _ = events.send(IconEvent {
                        tab_id: request.tab_id,
                        request_id: request.request_id,
                        target: request.target,
                        path: request.path,
                        icon,
                        actual_thumbnail,
                        requested_px: request.requested_px,
                    });
                } else if request.thumbnail
                    && let Ok(mut app) = state.lock()
                {
                    app.thumbnail_requests.remove(&(
                        request.tab_id,
                        request.request_id,
                        request.path,
                        request.requested_px,
                    ));
                }
            }
        });
    }
    (request_sender, event_receiver)
}

fn start_sidebar_icon_loader(ui: &AppWindow, state: SharedSessions) {
    let locations = state
        .lock()
        .expect("app state mutex is not poisoned")
        .sidebar
        .iter()
        .map(|location| location.path.clone())
        .collect::<Vec<_>>();
    let weak = ui.as_weak();
    thread::spawn(move || {
        let _shell_apartment = platform::windows_shell_icons::initialize_shell_worker().ok();
        for path in locations {
            if crate::network::is_unc_path(&path) {
                continue;
            }
            let Ok(icon) = platform::windows_shell_icons::shell_icon_rgba(&path) else {
                continue;
            };
            let state = state.clone();
            if weak
                .upgrade_in_event_loop(move |_ui| {
                    state
                        .lock()
                        .expect("app state mutex is not poisoned")
                        .sidebar_icons
                        .insert(path, icon);
                    refresh_all_windows(&state);
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn start_sidebar_loader(ui: &AppWindow, state: SharedSessions) {
    reload_quick_access(ui.as_weak(), state);
}

fn reload_quick_access(weak: slint::Weak<AppWindow>, state: SharedSessions) {
    let generation = {
        let Ok(mut app) = state.lock() else { return };
        app.quick_access_generation = app.quick_access_generation.wrapping_add(1).max(1);
        app.quick_access_generation
    };
    thread::spawn(move || {
        let locations = platform::known_locations();
        let state_for_ui = state.clone();
        let weak_for_icons = weak.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Ok(mut app) = state_for_ui.lock()
                && app.quick_access_generation == generation
            {
                app.sidebar = locations;
            }
            refresh_all_windows(&state_for_ui);
            if let Some(owner) = weak_for_icons.upgrade() {
                start_sidebar_icon_loader(&owner, state_for_ui.clone());
            } else {
                start_sidebar_icon_loader(&ui, state_for_ui.clone());
            }
        });
    });
}

fn start_network_location_loader(ui: &AppWindow, state: SharedSessions) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        let imported = platform::windows::network::enumerate_network_locations()
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(index, location)| NetworkLocation {
                id: stable_network_location_id(&location.shell_path),
                source: NetworkLocationSource::WindowsImported,
                display_name: location.label,
                sort_order: index as u32,
                target: location
                    .target
                    .map(NetworkTarget::WindowsPath)
                    .or_else(|| location.shell_identity.map(NetworkTarget::ShellItemId))
                    .expect("Windows network location retains an executable identity"),
            })
            .collect::<Vec<_>>();
        let state_for_ui = state.clone();
        let _ = weak.upgrade_in_event_loop(move |_ui| {
            if let Ok(mut app) = state_for_ui.lock() {
                app.imported_network_locations = imported;
            }
            refresh_all_windows(&state_for_ui);
        });
    });
}

fn stable_network_location_id(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn spawn_network_discovery_worker() -> (
    mpsc::SyncSender<NetworkDiscoveryRequest>,
    mpsc::Receiver<NetworkDiscoveryEvent>,
) {
    let (request_sender, request_receiver) = mpsc::sync_channel(1);
    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            run_network_discovery(request, &event_sender);
        }
    });
    (request_sender, event_receiver)
}

fn run_network_discovery(
    request: NetworkDiscoveryRequest,
    event_sender: &mpsc::Sender<NetworkDiscoveryEvent>,
) {
    let NetworkDiscoveryRequest::Discover {
        window_id,
        request_id,
        cancel,
    } = request;
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let result = platform::windows::network::isolated_network_devices(&cancel);
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    match result {
        Ok(devices) => {
            for batch in devices.chunks(16) {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let devices = batch
                    .iter()
                    .map(|device| NetworkDeviceTarget {
                        id: crate::network::network_device_id(&device.target),
                        display_name: device.label.clone(),
                        shell_identity: None,
                        unc_path: crate::network::is_unc_path(&device.target)
                            .then(|| device.target.clone()),
                    })
                    .collect();
                let _ = event_sender.send(NetworkDiscoveryEvent::Batch {
                    window_id,
                    request_id,
                    devices,
                });
            }
            let _ = event_sender.send(NetworkDiscoveryEvent::Finished {
                window_id,
                request_id,
            });
        }
        Err(error) => {
            if error.kind() == io::ErrorKind::TimedOut
                && let Ok(devices) =
                    platform::windows::network::network_devices_from_imported_locations()
                && !devices.is_empty()
            {
                let devices = devices
                    .into_iter()
                    .map(|device| NetworkDeviceTarget {
                        id: crate::network::network_device_id(&device.target),
                        display_name: device.label,
                        shell_identity: None,
                        unc_path: Some(device.target),
                    })
                    .collect();
                let _ = event_sender.send(NetworkDiscoveryEvent::Batch {
                    window_id,
                    request_id,
                    devices,
                });
                let _ = event_sender.send(NetworkDiscoveryEvent::Finished {
                    window_id,
                    request_id,
                });
                return;
            }
            let _ = event_sender.send(NetworkDiscoveryEvent::Failed {
                window_id,
                request_id,
                error: crate::network::classify_network_error(&error),
            });
        }
    }
}

fn network_error_text(error: crate::network::NetworkErrorKind, language: Language) -> &'static str {
    use crate::network::NetworkErrorKind;
    match (language, error) {
        (Language::Chinese, NetworkErrorKind::PermissionDenied) => {
            "无权访问 Windows 网络设备，点击重试"
        }
        (Language::Chinese, NetworkErrorKind::TimedOut) => "获取网络设备超时，点击重试",
        (Language::Chinese, NetworkErrorKind::Disconnected) => "网络不可用，点击重试",
        (Language::Chinese, _) => "无法获取 Windows 网络设备，点击重试",
        (Language::English, NetworkErrorKind::PermissionDenied) => {
            "Windows network access was denied. Click to retry"
        }
        (Language::English, NetworkErrorKind::TimedOut) => {
            "Network discovery timed out. Click to retry"
        }
        (Language::English, NetworkErrorKind::Disconnected) => {
            "The network is unavailable. Click to retry"
        }
        (Language::English, _) => "Unable to discover Windows network devices. Click to retry",
    }
}
fn start_network_discovery_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<NetworkDiscoveryEvent>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            let _ = weak.upgrade_in_event_loop(move |_ui| {
                let mut error_message = None;
                if let Ok(mut app) = state.lock() {
                    match event {
                        NetworkDiscoveryEvent::Batch {
                            window_id,
                            request_id,
                            devices,
                        } => {
                            let accepted = app
                                .network_discovery
                                .get_mut(&window_id)
                                .is_some_and(|coordinator| coordinator.append(request_id, devices));
                            if accepted {
                                app.network_discovery_errors.remove(&window_id);
                            }
                        }
                        NetworkDiscoveryEvent::Finished {
                            window_id,
                            request_id,
                        } => {
                            let accepted = app
                                .network_discovery
                                .get_mut(&window_id)
                                .is_some_and(|coordinator| coordinator.finish(request_id));
                            if accepted {
                                app.network_discovery_errors.remove(&window_id);
                                if let Some(devices) = app
                                    .network_discovery
                                    .get(&window_id)
                                    .filter(|discovery| !discovery.devices().is_empty())
                                    .map(|discovery| discovery.devices().to_vec())
                                {
                                    for other_window in
                                        app.windows.keys().copied().collect::<Vec<_>>()
                                    {
                                        if other_window != window_id {
                                            app.network_discovery.insert(
                                                other_window,
                                                DiscoveryCoordinator::with_devices(devices.clone()),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        NetworkDiscoveryEvent::Failed {
                            window_id,
                            request_id,
                            error,
                        } => {
                            if let Some(coordinator) = app.network_discovery.get_mut(&window_id)
                                && coordinator.fail(request_id, error)
                            {
                                let message = network_error_text(error, app.language).to_owned();
                                app.network_discovery_errors.insert(window_id, message);
                                error_message = Some((window_id, error));
                            }
                        }
                    }
                }
                if let Some((window_id, kind)) = error_message {
                    eprintln!(
                        "{{\"event\":\"network_discovery_failed\",\"window\":{},\"kind\":{:?}}}",
                        window_id.0, kind
                    );
                }
                refresh_all_windows(&state);
            });
        }
    });
}
fn start_icon_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<IconEvent>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    if let Some(update) = apply_icon_event(&state, event) {
                        if let Some(window_id) = state
                            .lock()
                            .ok()
                            .and_then(|app| app.window_for_tab(update.tab_id))
                            && let Some(target_ui) = window_ui(window_id)
                        {
                            update_icon_row(
                                &target_ui,
                                &WindowSessions::new(state.clone(), window_id),
                                update,
                            );
                        } else {
                            update_icon_row(&ui, &state, update);
                        }
                    }
                })
                .is_err()
            {
                break;
            }
        }
    });
}

#[derive(Debug, Clone, Copy)]
struct IconUpdate {
    tab_id: TabId,
    entry_id: Option<EntryId>,
}

fn insert_bounded_image(
    cache: &mut HashMap<(PathBuf, u32), platform::windows_shell_icons::ShellIconRgba>,
    order: &mut VecDeque<(PathBuf, u32)>,
    key: (PathBuf, u32),
    image: platform::windows_shell_icons::ShellIconRgba,
    capacity: usize,
) {
    if let std::collections::hash_map::Entry::Occupied(mut existing) = cache.entry(key.clone()) {
        order.retain(|candidate| candidate != &key);
        order.push_back(key);
        existing.insert(image);
        return;
    }
    while cache.len() >= capacity {
        let Some(oldest) = order.pop_front() else {
            cache.clear();
            break;
        };
        cache.remove(&oldest);
    }
    order.push_back(key.clone());
    cache.insert(key, image);
}

fn apply_icon_event(state: &SharedSessions, event: IconEvent) -> Option<IconUpdate> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    if event.requested_px > 0 {
        app.thumbnail_requests.remove(&(
            event.tab_id,
            event.request_id,
            event.path.clone(),
            event.requested_px,
        ));
    }
    if !icon_event_is_current(&app, &event) {
        return None;
    }
    let entry_id = match event.target {
        IconTarget::Entry(entry_id) => {
            if event.requested_px == 0 {
                app.icons.insert(
                    (event.tab_id, event.request_id, entry_id),
                    event.icon.clone(),
                );
            }
            Some(entry_id)
        }
        IconTarget::Location => None,
    };
    if event.actual_thumbnail {
        let app = &mut *app;
        insert_bounded_image(
            &mut app.thumbnail_cache,
            &mut app.thumbnail_cache_order,
            (event.path.clone(), event.requested_px),
            event.icon.clone(),
            THUMBNAIL_CACHE_CAPACITY,
        );
    } else if event.requested_px > 0 {
        let app = &mut *app;
        insert_bounded_image(
            &mut app.large_icon_cache,
            &mut app.large_icon_cache_order,
            (event.path.clone(), event.requested_px),
            event.icon.clone(),
            LARGE_ICON_CACHE_CAPACITY,
        );
    } else {
        app.icon_cache.insert(event.path, event.icon);
    }
    Some(IconUpdate {
        tab_id: event.tab_id,
        entry_id,
    })
}

fn icon_event_is_current(app: &AppState, event: &IconEvent) -> bool {
    app.tab(event.tab_id).is_some_and(|tab| {
        tab.latest_request == event.request_id
            && match event.target {
                IconTarget::Entry(entry_id) => tab
                    .visible_entry(entry_id)
                    .is_some_and(|entry| entry.path == event.path),
                IconTarget::Location => tab.visible_path() == Some(event.path.as_path()),
            }
    })
}

fn classify_error(kind: io::ErrorKind) -> LoadState {
    match kind {
        io::ErrorKind::NotFound => LoadState::NotFound,
        io::ErrorKind::PermissionDenied => LoadState::PermissionDenied,
        io::ErrorKind::NotConnected
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::TimedOut => LoadState::Disconnected,
        _ => LoadState::Failed,
    }
}

fn should_rebuild_projection(projected_entries: usize, pending_entries: usize) -> bool {
    projected_entries == 0
        || pending_entries.saturating_sub(projected_entries) >= REBUILT_PROJECTION_BATCH_SIZE
}

fn append_active_file_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    use slint::Model;
    let app = state.lock().expect("app state mutex is not poisoned");
    if app.active_window_state().active_tab != tab_id {
        return;
    }
    let tab = app.active();
    if tab.latest_request != request_id || tab.load_state != LoadState::Partial {
        return;
    }
    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        drop(app);
        refresh_tab_window(state, tab_id);
        return;
    };
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != request_id.0 as i32
    {
        drop(app);
        refresh_tab_window(state, tab_id);
        return;
    }
    let texts = Texts::new(app.language);
    let grouped = tab
        .visible_path()
        .map(|path| app.directory_preference(path).group_field != GroupField::None)
        .unwrap_or(false);
    let grid_layout = app.active_view_mode().uses_grid_layout();
    if grouped || grid_layout {
        let projected_entries = if grid_layout {
            ui.get_grid_rows()
                .iter()
                .filter(|row| !row.group_header)
                .map(|row| row.entries.row_count())
                .sum()
        } else {
            (0..model.row_count())
                .filter_map(|index| model.row_data(index))
                .filter(|row| row.loaded && !row.group_header)
                .count()
        };
        let pending_entries = tab.pending_entries.len();
        let should_refresh = should_rebuild_projection(projected_entries, pending_entries);
        drop(app);
        if should_refresh {
            refresh_tab_window(state, tab_id);
        } else {
            update_tab_status(ui, state, tab_id);
        }
        return;
    }
    let start = model.row_count();
    if start > tab.pending_entries.len() {
        drop(app);
        refresh_tab_window(state, tab_id);
        return;
    }
    model.extend(
        tab.pending_entries[start..]
            .iter()
            .map(|entry| file_row(entry, tab, texts, &app, None)),
    );
    ui.set_status_text(status_text(tab, texts).into());
}

fn update_icon_row(ui: &AppWindow, state: &SharedSessions, update: IconUpdate) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window_id) = app.window_for_tab(update.tab_id) else {
        return;
    };
    let Some(window) = app.window(window_id) else {
        return;
    };
    if window.active_tab != update.tab_id {
        return;
    }
    let Some(tab) = window.tabs.get(&update.tab_id) else {
        return;
    };
    let Some(entry_id) = update.entry_id else {
        let path = tab.visible_path();
        ui.set_current_location_icon(
            path.and_then(|path| app.icon_cache.get(path))
                .map(shell_icon_image)
                .unwrap_or_default(),
        );
        return;
    };
    drop(app);
    update_file_rows(ui, state, update.tab_id, &HashSet::from([entry_id]));
}

fn selection_projection_ids(tab: &TabSession) -> HashSet<EntryId> {
    tab.selected.iter().copied().chain(tab.focused).collect()
}

fn mutate_active_selection(
    state: &WindowSessions,
    mutate: impl FnOnce(&mut TabSession),
) -> Option<(TabId, HashSet<EntryId>)> {
    let mut app = state.lock().ok()?;
    let tab_id = app.active_window_state().active_tab;
    let tab = app.active_window_state_mut().tabs.get_mut(&tab_id)?;
    let before = selection_projection_ids(tab);
    mutate(tab);
    let mut changed = before;
    changed.extend(selection_projection_ids(tab));
    Some((tab_id, changed))
}
fn mutate_window_selection(
    state: &SharedSessions,
    window_id: WindowId,
    mutate: impl FnOnce(&mut TabSession),
) -> Option<(TabId, HashSet<EntryId>)> {
    let mut app = state.lock().ok()?;
    let window = app.windows.get_mut(&window_id)?;
    let tab_id = window.active_tab;
    let tab = window.tabs.get_mut(&tab_id)?;
    let before = selection_projection_ids(tab);
    mutate(tab);
    let mut changed = before;
    changed.extend(selection_projection_ids(tab));
    Some((tab_id, changed))
}

fn update_tab_status(ui: &AppWindow, state: &SharedSessions, tab_id: TabId) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window) = app
        .window_for_tab(tab_id)
        .and_then(|window_id| app.window(window_id))
    else {
        return;
    };
    let Some(tab) = window.tabs.get(&tab_id) else {
        return;
    };
    if window.active_tab == tab_id
        && ui.get_projected_file_tab_id() == tab_id.0 as i32
        && ui.get_projected_file_request_id() == tab.latest_request.0 as i32
    {
        ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
    }
}
fn update_file_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    changed: &HashSet<EntryId>,
) {
    use slint::Model;

    if changed.is_empty() {
        return;
    }
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window_id) = app.window_for_tab(tab_id) else {
        return;
    };
    let Some(window) = app.window(window_id) else {
        return;
    };
    if window.active_tab != tab_id {
        return;
    }
    let Some(tab) = window.tabs.get(&tab_id) else {
        return;
    };
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != tab.latest_request.0 as i32
    {
        return;
    }

    let texts = Texts::new(app.language);
    let Some(view_mode) = app.view_mode_for_tab(tab_id) else {
        return;
    };
    let grid_requested_px = view_mode
        .uses_grid_layout()
        .then(|| grid_thumbnail_request_px(view_mode, ui.window().scale_factor()));
    let rows = changed
        .iter()
        .filter_map(|id| tab.visible_entry(*id))
        .map(|entry| {
            (
                entry.id,
                file_row(entry, tab, texts, &app, grid_requested_px),
            )
        })
        .collect::<HashMap<_, _>>();
    if rows.is_empty() {
        return;
    }

    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        return;
    };
    for index in 0..model.row_count() {
        let Some(existing) = model.row_data(index) else {
            continue;
        };
        if let Some(updated) = rows.get(&EntryId(existing.id as u32)) {
            model.set_row_data(index, updated.clone());
        }
    }

    let grid_model = ui.get_grid_rows();
    if let Some(grid_model) = grid_model.as_any().downcast_ref::<VecModel<GridRow>>() {
        for row_index in 0..grid_model.row_count() {
            let Some(grid_row) = grid_model.row_data(row_index) else {
                continue;
            };
            let Some(entries) = grid_row
                .entries
                .as_any()
                .downcast_ref::<VecModel<FileRow>>()
            else {
                continue;
            };
            for entry_index in 0..entries.row_count() {
                let Some(existing) = entries.row_data(entry_index) else {
                    continue;
                };
                if let Some(updated) = rows.get(&EntryId(existing.id as u32)) {
                    entries.set_row_data(entry_index, updated.clone());
                }
            }
        }
    }
}

fn update_selection_status(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let tab = app.active();
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_context_menu_has_entry(!tab.selected.is_empty() || tab.focused.is_some());
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
}

fn project_column_layout(ui: &AppWindow, tab: &TabSession, app: &AppState) {
    let normal = tab
        .visible_path()
        .map(|path| app.directory_preference(path).columns)
        .unwrap_or(app.default_directory_view.columns);
    let search = app.search_view.columns;
    ui.set_normal_name_width(normal.widths[ColumnKind::Name.storage_code() as usize] as f32);
    ui.set_normal_kind_width(normal.widths[ColumnKind::Kind.storage_code() as usize] as f32);
    ui.set_normal_size_width(normal.widths[ColumnKind::Size.storage_code() as usize] as f32);
    ui.set_normal_modified_width(
        normal.widths[ColumnKind::Modified.storage_code() as usize] as f32,
    );
    ui.set_normal_created_width(normal.widths[ColumnKind::Created.storage_code() as usize] as f32);
    ui.set_search_name_width(search.widths[ColumnKind::Name.storage_code() as usize] as f32);
    ui.set_search_parent_width(search.widths[ColumnKind::Kind.storage_code() as usize] as f32);
    ui.set_search_size_width(search.widths[ColumnKind::Size.storage_code() as usize] as f32);
    ui.set_search_modified_width(
        search.widths[ColumnKind::Modified.storage_code() as usize] as f32,
    );
    ui.set_search_created_width(search.widths[ColumnKind::Created.storage_code() as usize] as f32);
    let layout = if tab.page_source == PageSource::Search {
        search
    } else {
        normal
    };
    let label = |kind: ColumnKind| match (app.language, kind, tab.page_source) {
        (Language::Chinese, ColumnKind::Name, _) => "名称",
        (Language::English, ColumnKind::Name, _) => "Name",
        (Language::Chinese, ColumnKind::Kind, PageSource::Search) => "父目录",
        (Language::English, ColumnKind::Kind, PageSource::Search) => "Parent path",
        (Language::Chinese, ColumnKind::Kind, _) => "类型",
        (Language::English, ColumnKind::Kind, _) => "Type",
        (Language::Chinese, ColumnKind::Size, _) => "大小",
        (Language::English, ColumnKind::Size, _) => "Size",
        (Language::Chinese, ColumnKind::Modified, _) => "修改时间",
        (Language::English, ColumnKind::Modified, _) => "Date modified",
        (Language::Chinese, ColumnKind::Created, _) => "创建时间",
        (Language::English, ColumnKind::Created, _) => "Date created",
    };
    ui.set_columns(ModelRc::new(VecModel::from(
        layout
            .order
            .iter()
            .map(|kind| {
                let code = kind.storage_code() as usize;
                ColumnRow {
                    kind: code as i32,
                    label: label(*kind).into(),
                    visible: layout.visible[code],
                    min_width: 64.0,
                    content_left: if *kind == ColumnKind::Name { 10.0 } else { 8.0 },
                    content_right: 8.0,
                    icon_slot: if *kind == ColumnKind::Name { 25.0 } else { 0.0 },
                }
            })
            .collect::<Vec<_>>(),
    )));
}
fn update_selection_summary(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let tab = app.active();

    project_column_layout(ui, tab, &app);
    let operation_rows = operation_rows(&app);
    ui.set_operations(ModelRc::new(VecModel::from(operation_rows)));
    ui.set_operation_error(
        app.operation_errors
            .last()
            .cloned()
            .unwrap_or_default()
            .into(),
    );
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_context_menu_has_entry(!tab.selected.is_empty() || tab.focused.is_some());
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
}

fn operation_rows(app: &AppState) -> Vec<OperationRow> {
    app.operations
        .iter()
        .map(|task| {
            let completed = task
                .items
                .iter()
                .filter(|item| {
                    matches!(
                        item.state,
                        ItemState::Succeeded
                            | ItemState::Skipped
                            | ItemState::Failed
                            | ItemState::Cancelled
                    )
                })
                .count();
            let title = match (app.language, task.kind) {
                (Language::Chinese, FileOperationKind::CreateFolder) => "正在新建文件夹",
                (Language::Chinese, FileOperationKind::Rename) => "正在重命名",
                (Language::Chinese, FileOperationKind::Copy) => "正在复制",
                (Language::Chinese, FileOperationKind::Move) => "正在移动",
                (Language::Chinese, FileOperationKind::RecycleDelete) => "正在移到回收站",
                (Language::Chinese, FileOperationKind::PermanentDelete) => "正在删除",
                (Language::Chinese, FileOperationKind::FastRemove) => "正在删除",
                (Language::English, FileOperationKind::CreateFolder) => "Creating folder",
                (Language::English, FileOperationKind::Rename) => "Renaming",
                (Language::English, FileOperationKind::Copy) => "Copying",
                (Language::English, FileOperationKind::Move) => "Moving",
                (Language::English, FileOperationKind::RecycleDelete) => "Recycling",
                (Language::English, FileOperationKind::PermanentDelete) => "Deleting",
                (Language::English, FileOperationKind::FastRemove) => "Deleting",
            };
            let progress = task
                .progress
                .total_bytes
                .filter(|total| *total > 0)
                .map(|total| task.progress.processed_bytes as f32 / total as f32)
                .unwrap_or_else(|| {
                    if task.items.is_empty() {
                        0.0
                    } else {
                        completed as f32 / task.items.len() as f32
                    }
                });
            let (transferred, speed_text, eta_text) = task
                .progress
                .total_bytes
                .filter(|total| *total > 0)
                .map(|total| {
                    let elapsed = task
                        .cancellation
                        .active_elapsed(task.started_at)
                        .as_secs_f64()
                        .max(0.001);
                    let speed = task.progress.processed_bytes as f64 / elapsed;
                    let remaining = total.saturating_sub(task.progress.processed_bytes);
                    let eta = if speed > 0.0 {
                        remaining as f64 / speed
                    } else {
                        0.0
                    };
                    (
                        format!(
                            "{:.1} / {:.1} MB",
                            task.progress.processed_bytes as f64 / 1_048_576.0,
                            total as f64 / 1_048_576.0
                        ),
                        if task.state == OperationState::Paused {
                            match app.language {
                                Language::Chinese => "已暂停".to_owned(),
                                Language::English => "Paused".to_owned(),
                            }
                        } else {
                            format!("{:.1} MB/s", speed / 1_048_576.0)
                        },
                        match (app.language, task.state == OperationState::Paused) {
                            (_, true) => "—".to_owned(),
                            (language, false) => format_remaining_time(language, eta),
                        },
                    )
                })
                .unwrap_or_else(|| {
                    (
                        format!("{completed}/{}", task.items.len()),
                        String::new(),
                        String::new(),
                    )
                });
            let source = task
                .items
                .first()
                .and_then(|item| item.source.as_deref())
                .map(display_path)
                .unwrap_or_default();
            let destination = task
                .items
                .first()
                .and_then(|item| item.destination.as_deref())
                .and_then(Path::parent)
                .map(display_path)
                .unwrap_or_default();
            OperationRow {
                id: task.id.0 as i32,
                title: title.into(),
                percent: format!("{:.0}%", (progress * 100.0).clamp(0.0, 100.0)).into(),
                file_progress: task
                    .progress
                    .total_files
                    .map(|total| format!("{} / {total}", task.progress.completed_files.min(total)))
                    .unwrap_or_else(|| match app.language {
                        Language::Chinese => "正在准备…".to_owned(),
                        Language::English => "Preparing…".to_owned(),
                    })
                    .into(),
                transferred: transferred.into(),
                speed: speed_text.into(),
                eta: eta_text.into(),
                source: source.into(),
                destination: destination.into(),
                paused: task.state == OperationState::Paused,
                current_item: task
                    .progress
                    .current_item
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
                progress,
                state: operation_state_index(task.state),
                can_cancel: matches!(
                    task.state,
                    OperationState::Queued
                        | OperationState::Preflight
                        | OperationState::Running
                        | OperationState::Paused
                        | OperationState::WaitingConflict
                ),
                can_pause: matches!(task.state, OperationState::Running | OperationState::Paused),
                can_retry: matches!(
                    task.state,
                    OperationState::Failed | OperationState::PartiallyCompleted
                ) && task.items.iter().any(|item| {
                    matches!(
                        item.state,
                        ItemState::Failed | ItemState::Cancelled | ItemState::Pending
                    )
                }),
            }
        })
        .collect()
}

fn format_remaining_time(language: Language, seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    if seconds < 60 {
        return match language {
            Language::Chinese => format!("预计剩余 {seconds} 秒"),
            Language::English => format!("About {seconds} seconds remaining"),
        };
    }
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    match language {
        Language::Chinese => format!("预计剩余 {minutes} 分 {seconds} 秒"),
        Language::English => format!("About {minutes}min {seconds}s remaining"),
    }
}
fn refresh_window_ui(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    if !state
        .lock()
        .is_ok_and(|app| app.windows.contains_key(&window_id))
    {
        return;
    }
    refresh_ui_inner(ui, state, window_id);
}

fn refresh_ui(ui: &AppWindow, state: &WindowSessions) {
    refresh_window_ui(ui, &state.shared, state.window_id);
}

fn local_utc_offset_seconds() -> i32 {
    use windows_sys::Win32::System::Time::{
        GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
    };
    let mut information = TIME_ZONE_INFORMATION::default();
    let state = unsafe { GetTimeZoneInformation(&mut information) };
    if state == TIME_ZONE_ID_INVALID {
        return 0;
    }
    let seasonal_bias = match state {
        1 => information.StandardBias,
        2 => information.DaylightBias,
        _ => 0,
    };
    -(information.Bias.saturating_add(seasonal_bias)).saturating_mul(60)
}

fn type_select_context(app: &AppState, tab: &TabSession) -> TypeSelectContext {
    let (group_field, group_direction) = if tab.page_source == PageSource::Search {
        (GroupField::None, SortDirection::Ascending)
    } else {
        tab.visible_path()
            .map(|path| {
                let preference = app.directory_preference(path);
                (preference.group_field, preference.group_direction)
            })
            .unwrap_or((GroupField::None, SortDirection::Ascending))
    };
    TypeSelectContext {
        tab_id: tab.id,
        request_id: tab.latest_request,
        view_mode: app
            .view_mode_for_tab(tab.id)
            .unwrap_or(app.default_directory_view.view_mode),
        page_source: tab.page_source,
        group_field,
        group_direction,
        sort_field: if tab.page_source == PageSource::Search {
            tab.search_sort_field
        } else {
            tab.sort_field
        },
        sort_direction: if tab.page_source == PageSource::Search {
            tab.search_sort_direction
        } else {
            tab.sort_direction
        },
    }
}

fn type_select_projection(app: &AppState, tab: &TabSession) -> Vec<(EntryId, String)> {
    let entries = directory_display_entries(tab);
    let by_id = entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<HashMap<_, _>>();
    directory_group_projections(app, tab, entries)
        .into_iter()
        .flat_map(|group| group.entries)
        .filter_map(|id| by_id.get(&id).map(|entry| (id, entry.display_name.clone())))
        .collect()
}

fn directory_group_projections(
    app: &AppState,
    tab: &TabSession,
    entries: &[FileEntry],
) -> Vec<group_projection::GroupProjection> {
    let preference = tab
        .visible_path()
        .map(|path| app.directory_preference(path))
        .unwrap_or(app.default_directory_view);
    group_projection::project_groups(
        entries,
        preference.group_field,
        preference.group_direction,
        GroupProjectionContext {
            language: app.language,
            now: std::time::SystemTime::now(),
            utc_offset_seconds: local_utc_offset_seconds(),
        },
    )
}

fn projected_directory_rows(
    entries: &[FileEntry],
    tab: &TabSession,
    texts: Texts,
    app: &AppState,
) -> Vec<FileRow> {
    let groups = directory_group_projections(app, tab, entries);
    let by_id = entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<HashMap<_, _>>();
    ListProjection::from_groups(&groups, 32, file_row_height(app.active_view_mode()) as u64)
        .rows
        .into_iter()
        .filter_map(|visual| match visual {
            ListVisualRow::GroupHeader {
                label, entry_count, ..
            } => Some(group_header_file_row(&label, entry_count)),
            ListVisualRow::Entry { entry_id } => by_id
                .get(&entry_id)
                .map(|entry| file_row(entry, tab, texts, app, None)),
        })
        .collect()
}

fn projected_directory_grid_rows(
    entries: &[FileEntry],
    tab: &TabSession,
    texts: Texts,
    app: &AppState,
    columns: usize,
    requested_px: u32,
) -> Vec<GridRow> {
    let groups = directory_group_projections(app, tab, entries);
    let by_id = entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<HashMap<_, _>>();
    IconProjection::from_groups(
        &groups,
        columns,
        32,
        file_row_height(app.active_view_mode()) as u64,
    )
    .rows
    .into_iter()
    .map(|visual| match visual {
        IconVisualRow::GroupHeader {
            label, entry_count, ..
        } => GridRow {
            group_header: true,
            group_label: label.into(),
            group_count: entry_count.min(i32::MAX as usize) as i32,
            entries: ModelRc::new(VecModel::default()),
        },
        IconVisualRow::Entries { entries, .. } => GridRow {
            group_header: false,
            group_label: "".into(),
            group_count: 0,
            entries: ModelRc::new(VecModel::from(
                entries
                    .into_iter()
                    .filter_map(|id| {
                        by_id
                            .get(&id)
                            .map(|entry| file_row(entry, tab, texts, app, Some(requested_px)))
                    })
                    .collect::<Vec<_>>(),
            )),
        },
    })
    .collect()
}
fn directory_display_entries(tab: &TabSession) -> &[FileEntry] {
    if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) {
        &tab.pending_entries
    } else if tab.has_failed_location() {
        &[]
    } else {
        &tab.entries
    }
}

fn refresh_ui_inner(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let texts = Texts::new(app.language);
    let Some(window) = app.window(window_id) else {
        return;
    };
    let Some(tab) = window.tabs.get(&window.active_tab) else {
        return;
    };
    if let Some(window_id) = window_id_for_ui(ui) {
        let request_id = tab.latest_request;
        let invalidated = WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            runtimes.get_mut(&window_id).is_some_and(|runtime| {
                runtime
                    .quick_menu_popup
                    .session
                    .invalidate_request(window_id, tab.id, request_id)
            })
        });
        if invalidated {
            let _ = ui;
            drop(app);
            dismiss_quick_menu_session(window_id, false);
            return;
        }
    }
    let active_is_settings = tab.kind == TabKind::Settings;
    ui.set_active_is_settings(active_is_settings);
    let view_mode = app
        .view_mode_for_tab(tab.id)
        .unwrap_or(app.default_directory_view.view_mode);
    ui.set_view_mode(view_mode_to_ui(view_mode));
    let projected_tab_id = tab.id.0 as i32;
    let projected_request_id = tab.latest_request.0 as i32;
    if ui.get_projected_file_tab_id() != projected_tab_id
        || ui.get_projected_file_request_id() != projected_request_id
    {
        if ui.get_rectangle_selection_pointer_active() {
            ui.invoke_cancel_rectangle_selection();
        }
        ui.set_file_viewport_y(0.0);
        ui.set_search_scroll_y(0.0);
        ui.set_projected_file_tab_id(projected_tab_id);
        ui.set_projected_file_request_id(projected_request_id);
    }
    let display_entries = directory_display_entries(tab);
    let geometry = file_layout_geometry(view_mode);
    let grid_columns = (((ui.window().size().width as f32 / ui.window().scale_factor()) - 292.0)
        / (geometry.card_width + 8.0).max(1.0))
    .floor()
    .max(1.0) as usize;
    let total = tab.search_total.unwrap_or(tab.entries.len() as u32);
    let search_index =
        search_result_index_at_scroll(ui.get_search_scroll_y(), total, view_mode, grid_columns);
    let search_window = search_window_for_index(search_index, total, grid_columns);
    if tab.page_source == PageSource::Search {
        ui.set_file_viewport_y(search_window_viewport_y(
            search_index,
            search_window,
            view_mode,
            grid_columns,
        ));
    }
    let (file_rows, grid_rows) = if view_mode.uses_grid_layout() {
        let requested_px = grid_thumbnail_request_px(view_mode, ui.window().scale_factor());
        let grid_rows = if tab.page_source == PageSource::Search {
            search_window_rows(tab, &app, search_window, Some(requested_px))
                .chunks(grid_columns)
                .map(|entries| GridRow {
                    group_header: false,
                    group_label: "".into(),
                    group_count: 0,
                    entries: ModelRc::new(VecModel::from(entries.to_vec())),
                })
                .collect::<Vec<_>>()
        } else {
            projected_directory_grid_rows(
                display_entries,
                tab,
                texts,
                &app,
                grid_columns,
                requested_px,
            )
        };
        (Vec::new(), grid_rows)
    } else {
        let file_rows = if tab.page_source == PageSource::Search {
            search_window_rows(tab, &app, search_window, None)
        } else {
            projected_directory_rows(display_entries, tab, texts, &app)
        };
        (file_rows, Vec::new())
    };
    ui.set_files(ModelRc::new(VecModel::from(file_rows)));
    ui.set_grid_column_count(grid_columns as i32);
    ui.set_grid_rows(ModelRc::new(VecModel::from(grid_rows)));
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
    ui.set_window_width(ui.window().size().width as f32 / ui.window().scale_factor());
    let visible_path = tab.visible_path().map(display_path).unwrap_or_default();
    let address_input = if tab.address_editing {
        tab.address_input.clone()
    } else {
        visible_path.clone()
    };
    ui.set_current_path(visible_path.into());
    let current_location_path = tab.visible_path();
    ui.set_current_location_icon(
        current_location_path
            .and_then(|path| app.icon_cache.get(path))
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

    ui.set_status_text(status_text(tab, texts).into());
    let network_permission = tab.load_state == LoadState::PermissionDenied
        && tab
            .requested_path
            .as_deref()
            .is_some_and(crate::network::is_unc_path);
    let (error_page_title, error_page_description) = if network_permission {
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
                    tab.visible_path()
                        .and_then(Path::file_name)
                        .map(|name| name.to_string_lossy().into_owned())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| {
                            display_path(tab.visible_path().unwrap_or(Path::new("C:\\")))
                        })
                }
                .into(),
                active: tab.id == window.active_tab,
                loading: matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
                icon: tab
                    .visible_path()
                    .and_then(|path| app.icon_cache.get(path))
                    .map(shell_icon_image)
                    .unwrap_or_default(),
                is_drive: tab.visible_path().is_some_and(is_drive_root),
                is_settings: tab.kind == TabKind::Settings,
            })
            .collect::<Vec<_>>(),
    )));
    let breadcrumb_paths = tab.breadcrumb_paths();
    ui.set_breadcrumbs(ModelRc::new(VecModel::from(
        breadcrumb_paths
            .iter()
            .enumerate()
            .map(|(index, (label, _))| BreadcrumbRow {
                index: index as i32,
                label: label.clone().into(),
                current: index + 1 == breadcrumb_paths.len(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_back_history(ModelRc::new(VecModel::from(
        tab.back_history
            .iter()
            .rev()
            .enumerate()
            .map(|(index, path)| HistoryRow {
                index: index as i32,
                label: display_path(path).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_forward_history(ModelRc::new(VecModel::from(
        tab.forward_history
            .iter()
            .rev()
            .enumerate()
            .map(|(index, path)| HistoryRow {
                index: index as i32,
                label: display_path(path).into(),
            })
            .collect::<Vec<_>>(),
    )));
    let mut sidebar_rows = app
        .sidebar
        .iter()
        .enumerate()
        .map(|(index, location)| SidebarRow {
            index: index as i32,
            stable_id: "".into(),
            label: match (app.language, location.kind) {
                (Language::Chinese, KnownLocationKind::Home) => "主页",
                (Language::English, KnownLocationKind::Home) => "Home",
                (_, KnownLocationKind::Pinned | KnownLocationKind::Drive) => {
                    location.label.as_str()
                }
            }
            .into(),
            icon_kind: match location.kind {
                KnownLocationKind::Home => 0,
                KnownLocationKind::Drive => 7,
                KnownLocationKind::Pinned => 3,
            },
            is_drive: location.kind == KnownLocationKind::Drive,
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
        })
        .collect::<Vec<_>>();
    for (row, location) in sidebar_rows.iter_mut().zip(app.sidebar.iter()) {
        if location.kind == KnownLocationKind::Pinned {
            row.stable_id = location
                .path
                .as_os_str()
                .to_string_lossy()
                .into_owned()
                .into();
            row.source_kind = 3;
        }
    }
    let mut network_row_index = app.sidebar.len();
    let mut locations = app
        .imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .collect::<Vec<_>>();
    locations.sort_by_key(|location| location.sort_order);
    for location in locations {
        let index = network_row_index;
        network_row_index += 1;
        sidebar_rows.push(SidebarRow {
            index: index as i32,
            stable_id: location.id.to_string().into(),
            label: location.display_name.clone().into(),
            icon_kind: 7,
            group_kind: 2,
            source_kind: if location.source == NetworkLocationSource::WindowsImported {
                1
            } else {
                2
            },
            is_drive: false,
            icon: app
                .sidebar_icons
                .get(match &location.target {
                    NetworkTarget::WindowsPath(path) | NetworkTarget::ShellItemId(path) => path,
                })
                .map(shell_icon_image)
                .unwrap_or_default(),
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
                stable_id: "".into(),
                label: device.display_name.clone().into(),
                icon_kind: 7,
                group_kind: 3,
                source_kind: 0,
                is_drive: false,
                icon: Image::default(),
            });
        }
    }
    ui.set_sidebar_items(ModelRc::new(VecModel::from(sidebar_rows)));

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
    ui.set_selected_count(tab.selected.len() as i32);
    project_column_layout(ui, tab, &app);
    let (sort_field, sort_direction) = if tab.page_source == PageSource::Search {
        (tab.search_sort_field, tab.search_sort_direction)
    } else {
        (tab.sort_field, tab.sort_direction)
    };
    ui.set_sort_field(match sort_field {
        SortField::Name => 0,
        SortField::Kind => 1,
        SortField::Size => 2,
        SortField::Modified => 3,
        SortField::Created => 4,
    });
    ui.set_sort_descending(sort_direction == crate::domain::SortDirection::Descending);
    let page_projection = agent_debug::page_projection(tab.load_state, tab.entries.is_empty());
    ui.set_page_state(if tab.page_source == PageSource::Search {
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
    } else {
        page_projection.index
    });
    let show_request_access = page_projection
        .visible_page_operations
        .contains(&agent_debug::PageOperation::RequestWindowsAccess);
    ui.set_show_request_access(show_request_access);
    if show_request_access
        && tab
            .requested_path
            .as_deref()
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
        !active_is_settings && (tab.has_failed_location() || !tab.back_history.is_empty()),
    );
    ui.set_can_navigate_forward(!active_is_settings && !tab.forward_history.is_empty());
    ui.set_can_navigate_up(
        !active_is_settings && tab.visible_path().and_then(Path::parent).is_some(),
    );
    ui.set_can_refresh(
        !active_is_settings && !matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
    );
    ui.set_can_close_tab(window.tab_order.len() > 1);
    ui.set_can_restore_tab(!window.closed_tabs.is_empty());
    ui.set_language_mode(match app.language {
        Language::Chinese => 0,
        Language::English => 1,
    });
    ui.set_show_hidden_files(app.file_visibility.show_hidden);
    ui.set_show_system_files(app.file_visibility.show_system);
    ui.set_everything_path(
        app.everything_config
            .executable_path
            .as_deref()
            .map(display_path)
            .unwrap_or_default()
            .into(),
    );
    ui.set_everything_instance(app.everything_config.instance_name.clone().into());
    ui.set_everything_status(app.everything_status.clone().into());
    ui.set_everything_busy(app.everything_busy);
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

fn operation_state_index(state: OperationState) -> i32 {
    match state {
        OperationState::Queued => 0,
        OperationState::Preflight => 1,
        OperationState::Running => 2,
        OperationState::Paused => 9,
        OperationState::WaitingConflict => 3,
        OperationState::Cancelling => 4,
        OperationState::Completed => 5,
        OperationState::Cancelled => 6,
        OperationState::PartiallyCompleted => 7,
        OperationState::Failed => 8,
    }
}
fn file_row(
    entry: &FileEntry,
    tab: &TabSession,
    texts: Texts,
    app: &AppState,
    grid_requested_px: Option<u32>,
) -> FileRow {
    debug_assert_eq!(
        if crate::network::is_unc_path(&entry.path) {
            crate::network::unc_leaf_name(&entry.path)
        } else {
            entry.path.file_name().map(ToOwned::to_owned)
        }
        .as_deref(),
        Some(entry.original_name.as_os_str()),
        "entry identity must retain its original file name"
    );
    let name_segments = if entry.name_highlights.is_empty() {
        vec![HighlightSegment {
            text: entry.display_name.clone().into(),
            highlighted: false,
        }]
    } else {
        entry
            .name_highlights
            .iter()
            .map(|segment| HighlightSegment {
                text: segment.text.clone().into(),
                highlighted: segment.highlighted,
            })
            .collect::<Vec<_>>()
    };
    let grid_image = grid_requested_px.and_then(|requested_px| {
        app.thumbnail_cache
            .get(&(entry.path.clone(), requested_px))
            .or_else(|| {
                app.large_icon_cache
                    .get(&(entry.path.clone(), requested_px))
            })
    });
    let image = grid_image.or_else(|| {
        app.icons
            .get(&(tab.id, tab.latest_request, entry.id))
            .or_else(|| app.icon_cache.get(&entry.path))
    });
    FileRow {
        id: entry.id.0 as i32,
        loaded: true,
        group_header: false,
        group_label: "".into(),
        group_count: 0,
        name: entry.display_name.clone().into(),
        name_segments: ModelRc::new(VecModel::from(name_segments)),
        kind: if entry.kind == crate::domain::EntryKind::File {
            entry
                .path
                .extension()
                .map(|extension| extension.to_string_lossy().to_uppercase())
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{extension} {}", texts.kind(entry.kind)))
                .unwrap_or_else(|| texts.kind(entry.kind).to_owned())
                .into()
        } else {
            texts.kind(entry.kind).into()
        },
        parent_path: entry.parent_display.clone().into(),
        size: if entry.kind == crate::domain::EntryKind::Directory {
            texts.folder_size(entry.folder_size).into()
        } else {
            texts.size(entry.size_bytes).into()
        },
        modified: texts.modified(entry.modified).into(),
        created: texts.modified(entry.created).into(),
        is_directory: entry.kind == crate::domain::EntryKind::Directory,
        selected: tab.selected.contains(&entry.id),
        focused: tab.focused == Some(entry.id),
        cut: app.cut_paths.contains(&entry.path),
        icon: image.map(shell_icon_image).unwrap_or_default(),
    }
}

fn shell_icon_image(icon: &platform::windows_shell_icons::ShellIconRgba) -> Image {
    let buffer =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&icon.pixels, icon.width, icon.height);
    Image::from_rgba8(buffer)
}

fn status_text(tab: &TabSession, texts: Texts) -> String {
    if tab.error.as_deref() == Some("network_slow")
        && matches!(tab.load_state, LoadState::Loading | LoadState::Partial)
    {
        return match texts.language {
            Language::Chinese => "网络连接较慢，仍在等待…".to_owned(),
            Language::English => "The network connection is slow. Still waiting…".to_owned(),
        };
    }
    if let Some(progress) = tab.folder_sizes.progress()
        && progress.completed < progress.total
    {
        return match texts.language {
            Language::Chinese => format!(
                "正在获取文件夹大小（已完成 {}/{}）",
                progress.completed, progress.total
            ),
            Language::English => format!(
                "Getting folder sizes ({}/{})",
                progress.completed, progress.total
            ),
        };
    }
    if tab.page_source == PageSource::Search {
        return match tab.search_total {
            Some(total) if total as usize > tab.entries.len() => match texts.language {
                Language::Chinese => format!("已加载 {} 项，共 {total} 项", tab.entries.len()),
                Language::English => format!("Loaded {} of {total} items", tab.entries.len()),
            },
            Some(total) => texts.items(total as usize, 0),
            None => texts.search_state(tab.search_state).to_owned(),
        };
    }
    if !tab.selected.is_empty() {
        return match texts.language {
            Language::Chinese => format!("已选择 {} 项", tab.selected.len()),
            Language::English => format!("{} selected", tab.selected.len()),
        };
    }
    match tab.load_state {
        LoadState::Complete => texts.items(
            tab.entries.len(),
            tab.error
                .as_deref()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        ),
        state => texts.state(state).to_owned(),
    }
}

fn error_page_text(state: LoadState, texts: Texts) -> (&'static str, &'static str) {
    match (texts.language, state) {
        (Language::Chinese, LoadState::PermissionDenied) => (
            "无权访问此文件夹",
            "你当前没有访问此文件夹的权限。可以使用 Windows 请求访问权限。",
        ),
        (Language::English, LoadState::PermissionDenied) => (
            "Access denied",
            "You don't currently have permission to access this folder. Use Windows to request access.",
        ),
        (Language::Chinese, LoadState::NotFound) => {
            ("找不到该位置", "该位置可能已被移动、重命名或删除。")
        }
        (Language::English, LoadState::NotFound) => (
            "Location not found",
            "This location may have been moved, renamed, or deleted.",
        ),
        (Language::Chinese, LoadState::Disconnected) => {
            ("位置已断开", "请检查磁盘或网络连接，然后重试。")
        }
        (Language::English, LoadState::Disconnected) => (
            "Location disconnected",
            "Check the drive or network connection, then try again.",
        ),
        (Language::Chinese, LoadState::Cancelled) => ("加载已取消", "你可以重试或返回上一个位置。"),
        (Language::English, LoadState::Cancelled) => (
            "Loading cancelled",
            "Try again or return to the previous location.",
        ),
        (Language::Chinese, _) => ("无法打开该位置", "读取此位置时发生错误。你可以重试或返回。"),
        (Language::English, _) => (
            "Unable to open location",
            "An error occurred while reading this location. Try again or go back.",
        ),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EverythingOperation {
    Discover,
    Pick,
    Start,
    Test,
}

fn begin_everything_operation(
    state: &SharedSessions,
    operation: EverythingOperation,
) -> Option<u64> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    if app.everything_busy {
        return None;
    }
    app.everything_busy = true;
    app.everything_status = everything_progress_text(app.language, operation);
    Some(app.everything_generation)
}

fn everything_progress_text(language: Language, operation: EverythingOperation) -> String {
    match (language, operation) {
        (Language::Chinese, EverythingOperation::Discover) => "正在自动发现 Everything…",
        (Language::Chinese, EverythingOperation::Pick) => "正在选择 Everything 程序…",
        (Language::Chinese, EverythingOperation::Start) => "正在启动 Everything…",
        (Language::Chinese, EverythingOperation::Test) => "正在测试连接…",
        (Language::English, EverythingOperation::Discover) => "Detecting Everything…",
        (Language::English, EverythingOperation::Pick) => "Choosing the Everything program…",
        (Language::English, EverythingOperation::Start) => "Starting Everything…",
        (Language::English, EverythingOperation::Test) => "Testing connection…",
    }
    .to_owned()
}

fn everything_picker_error_text(language: Language) -> &'static str {
    match language {
        Language::Chinese => "无法打开程序选择器。请手动输入 Everything64.exe 的路径。",
        Language::English => {
            "Unable to open the program picker. Enter the path to Everything64.exe manually."
        }
    }
}

fn everything_error_text(
    language: Language,
    error: &platform::windows::everything::EverythingError,
) -> String {
    use platform::windows::everything::EverythingError;
    match (language, error) {
        (Language::Chinese, EverythingError::NotConfigured) => {
            "尚未配置 Everything。请选择 Everything64.exe，或使用自动发现。"
        }
        (Language::Chinese, EverythingError::InvalidExecutable(_)) => {
            "程序路径无效。请选择 Everything 1.5 x64 的 Everything64.exe。"
        }
        (Language::Chinese, EverythingError::NotRunning(_)) => {
            "该 Everything 实例未运行。请启动后重试，并确认实例名一致。"
        }
        (Language::Chinese, EverythingError::Timeout) => {
            "连接 Everything 超时。请确认程序已完成启动，且实例名一致。"
        }
        (Language::Chinese, EverythingError::UnsupportedVersion(_)) => {
            "Everything 版本不受支持。请安装 Everything 1.5 x64。"
        }
        (Language::Chinese, EverythingError::UnsupportedArchitecture) => {
            "Everything 架构不受支持。请使用 Everything 1.5 x64。"
        }
        (Language::Chinese, EverythingError::DatabaseNotLoaded) => {
            "Everything 数据库尚未加载。请等待加载完成后重新测试。"
        }
        (Language::Chinese, EverythingError::StartFailed(_)) => {
            "启动 Everything 失败。请检查程序路径，或手动启动后重试。"
        }
        (Language::Chinese, EverythingError::FolderSizePipeUnavailable(_)
            | EverythingError::FolderSizeDisconnected
            | EverythingError::FolderSizeRejected(_)) => {
                "Everything 文件夹大小索引不可用。请在 Everything 中启用文件夹大小索引，并确认该目录已收录。"
            }
        (Language::Chinese, _) => {
            "无法连接 Everything。请检查程序路径、实例名和运行状态后重试。"
        }
        (Language::English, EverythingError::NotConfigured) => {
            "Everything is not configured. Choose Everything64.exe or use Auto-detect."
        }
        (Language::English, EverythingError::InvalidExecutable(_)) => {
            "The program path is invalid. Choose Everything64.exe from Everything 1.5 x64."
        }
        (Language::English, EverythingError::NotRunning(_)) => {
            "The configured Everything instance is not running. Start it and confirm the instance name matches."
        }
        (Language::English, EverythingError::Timeout) => {
            "The Everything connection timed out. Wait for startup to finish and confirm the instance name."
        }
        (Language::English, EverythingError::UnsupportedVersion(_)) => {
            "This Everything version is unsupported. Install Everything 1.5 x64."
        }
        (Language::English, EverythingError::UnsupportedArchitecture) => {
            "This Everything architecture is unsupported. Use Everything 1.5 x64."
        }
        (Language::English, EverythingError::DatabaseNotLoaded) => {
            "The Everything database is not loaded yet. Wait for it to finish, then test again."
        }
        (Language::English, EverythingError::StartFailed(_)) => {
            "Everything could not be started. Check the program path or start it manually, then retry."
        }
        (Language::English, EverythingError::FolderSizePipeUnavailable(_)
            | EverythingError::FolderSizeDisconnected
            | EverythingError::FolderSizeRejected(_)) => {
                "Everything folder-size indexing is unavailable. Enable folder-size indexing and include this folder."
            }
        (Language::English, _) => {
            "Unable to connect to Everything. Check the program path, instance name, and running state."
        }
    }
    .to_owned()
}
fn everything_connected_status(
    language: Language,
    status: &platform::windows::everything::EverythingStatus,
) -> String {
    let database = match (language, status.database_loaded) {
        (Language::Chinese, true) => "数据库已加载",
        (Language::Chinese, false) => "数据库未加载",
        (Language::English, true) => "Database loaded",
        (Language::English, false) => "Database not loaded",
    };
    let folder_size_status = match (language, status.folder_size_indexed) {
        (Language::Chinese, true) => "文件夹大小索引已启用",
        (Language::Chinese, false) => "文件夹大小索引未启用",
        (Language::English, true) => "Folder-size indexing enabled",
        (Language::English, false) => "Folder-size indexing disabled",
    };
    let instance = if status.instance_name.is_empty() {
        match language {
            Language::Chinese => "默认实例",
            Language::English => "Default instance",
        }
        .to_owned()
    } else {
        status.instance_name.clone()
    };
    match language {
        Language::Chinese => format!(
            "连接成功 · 版本 {} · 实例 {} · {database} · {folder_size_status}",
            status.version, instance
        ),
        Language::English => format!(
            "Connected · Version {} · Instance {} · {database} · {folder_size_status}",
            status.version, instance
        ),
    }
}

fn apply_everything_discovery(
    app: &mut AppState,
    generation: u64,
    mut config: crate::domain::EverythingConfig,
    status: &platform::windows::everything::EverythingStatus,
) -> bool {
    if generation != app.everything_generation {
        return false;
    }
    config.verified_version = Some(status.version.to_string());
    app.everything_config = config;
    app.everything_status = everything_connected_status(app.language, status);
    app.everything_folder_sizes_indexed = Some(status.folder_size_indexed);
    true
}

fn search_error_page_text(state: SearchState, texts: Texts) -> (&'static str, &'static str) {
    match (texts.language, state) {
        (Language::Chinese, SearchState::NotConfigured) => (
            "需要安装并配置 Everything",
            "AsterFiles 搜索依赖 Everything 1.5 x64。请先安装 Everything，再到设置中确认程序路径和实例。",
        ),
        (Language::English, SearchState::NotConfigured) => (
            "Install and configure Everything",
            "AsterFiles search requires Everything 1.5 x64. Install Everything, then confirm its program path and instance in Settings.",
        ),
        (Language::Chinese, SearchState::Disconnected) => (
            "无法连接 Everything",
            "Everything 可能尚未运行，或程序路径与实例配置不正确。请启动 Everything，或前往设置检查配置。",
        ),
        (Language::English, SearchState::Disconnected) => (
            "Can't connect to Everything",
            "Everything may not be running, or its program path and instance may be incorrect. Start Everything or check its settings.",
        ),
        (Language::Chinese, SearchState::NotIndexed) => (
            "Everything 索引尚未就绪",
            "请等待 Everything 完成索引，并在 Everything 设置中确认索引功能已启用。",
        ),
        (Language::English, SearchState::NotIndexed) => (
            "Everything index isn't ready",
            "Wait for Everything to finish indexing and confirm indexing is enabled in Everything settings.",
        ),
        (Language::Chinese, SearchState::UnsupportedVersion) => (
            "Everything 版本不受支持",
            "AsterFiles 需要 Everything 1.5。请安装兼容版本，并前往设置重新发现或检查配置。",
        ),
        (Language::English, SearchState::UnsupportedVersion) => (
            "Everything version isn't supported",
            "AsterFiles requires Everything 1.5. Install a compatible version, then auto-detect it or check the configuration in Settings.",
        ),
        (Language::Chinese, SearchState::UnsupportedArchitecture) => (
            "需要 Everything x64",
            "当前 Everything 位数不兼容。请安装 Everything 1.5 x64，并前往设置重新配置。",
        ),
        (Language::English, SearchState::UnsupportedArchitecture) => (
            "Everything x64 is required",
            "The installed Everything architecture is incompatible. Install Everything 1.5 x64, then update the configuration in Settings.",
        ),
        (Language::Chinese, SearchState::SyntaxError) => {
            ("搜索表达式有误", "请检查搜索内容后重试。")
        }
        (Language::English, SearchState::SyntaxError) => (
            "Invalid search expression",
            "Check the search text and try again.",
        ),
        (Language::Chinese, SearchState::TimedOut) => {
            ("搜索超时", "Everything 未及时响应，请稍后重试。")
        }
        (Language::English, SearchState::TimedOut) => (
            "Search timed out",
            "Everything did not respond in time. Try again.",
        ),
        (Language::Chinese, _) => ("搜索失败", "Everything 搜索发生错误，请重试。"),
        (Language::English, _) => ("Search failed", "Everything search failed. Try again."),
    }
}
fn apply_ui_texts(ui: &AppWindow, language: Language) {
    let (
        go,
        back,
        forward,
        up,
        refresh,
        home,
        desktop,
        downloads,
        documents,
        pictures,
        music,
        videos,
        quick_access,
        drives,
        name,
        kind,
        modified,
        size,
        request_access,
        menu,
        settings,
        theme,
        theme_system,
        theme_light,
        theme_dark,
        language_label,
        chinese,
        english,
        show_hidden_files,
        show_system_files,
        settings_general,
        settings_appearance,
        settings_developer,
        settings_about,
        version,
        ui_showcase,
        ui_showcase_detail,
        show_delete_confirmation,
        show_conflict_confirmation,
        show_exit_confirmation,
        show_operation_window,
        window_trace_title,
        window_trace_detail,
        window_trace_start,
        window_trace_active,
        window_trace_path,
        loading,
        empty_folder,
        no_search_results,
        new_tab,
        close_tab,
        all_tabs,
        minimize,
        restore,
        maximize,
        close,
        address,
        cancel_edit,
        search_recursive,
        search_current,
        search_global_disabled,
        drop_copy,
        drop_move,
        drop_link,
        drop_cancel,
    ) = match language {
        Language::Chinese => (
            "前往",
            "后退",
            "前进",
            "向上",
            "刷新",
            "主页",
            "桌面",
            "下载",
            "文档",
            "图片",
            "音乐",
            "视频",
            "快速访问",
            "磁盘",
            "名称",
            "类型",
            "修改时间",
            "大小",
            "使用 Windows 请求访问权限",
            "菜单",
            "设置",
            "主题",
            "跟随系统",
            "浅色",
            "深色",
            "语言",
            "中文",
            "English",
            "显示隐藏文件",
            "显示系统文件",
            "常规",
            "外观",
            "开发工具",
            "关于",
            "版本",
            "UI 陈列室",
            "直接打开难以触发的窗口和状态；演示操作不会修改文件或任务。",
            "永久删除确认",
            "文件冲突",
            "退出任务确认",
            "文件进度窗口",
            "窗口交互诊断",
            "只记录窗口移动与缩放消息，不改变窗口行为。问题复现后关闭应用并把日志交给开发者。",
            "开始记录",
            "正在记录",
            "日志位置",
            "正在加载…",
            "此文件夹为空",
            "没有搜索结果",
            "新建标签",
            "关闭标签",
            "全部标签",
            "最小化",
            "还原",
            "最大化",
            "关闭",
            "路径",
            "取消编辑",
            "搜索当前文件夹及子文件夹",
            "仅搜索当前文件夹",
            "全局搜索不支持切换范围",
            "复制到此处",
            "移动到此处",
            "在此处创建快捷方式",
            "取消",
        ),
        Language::English => (
            "Go",
            "Back",
            "Forward",
            "Up",
            "Refresh",
            "Home",
            "Desktop",
            "Downloads",
            "Documents",
            "Pictures",
            "Music",
            "Videos",
            "Quick access",
            "Drives",
            "Name",
            "Type",
            "Modified",
            "Size",
            "Request access with Windows",
            "Menu",
            "Settings",
            "Theme",
            "Use system setting",
            "Light",
            "Dark",
            "Language",
            "中文",
            "English",
            "Show hidden files",
            "Show system files",
            "General",
            "Appearance",
            "Developer tools",
            "About",
            "Version",
            "UI showcase",
            "Open hard-to-reach windows and states directly. Demo actions do not change files or tasks.",
            "Delete confirmation",
            "File conflict",
            "Exit confirmation",
            "File progress window",
            "Window interaction diagnostics",
            "Records move and resize messages without changing window behavior. After reproducing the issue, close the app and send the log to the developer.",
            "Start recording",
            "Recording",
            "Log location",
            "Loading…",
            "This folder is empty",
            "No search results",
            "New tab",
            "Close tab",
            "All tabs",
            "Minimize",
            "Restore",
            "Maximize",
            "Close",
            "Path",
            "Cancel editing",
            "Search current folder and subfolders",
            "Search current folder only",
            "Search depth is unavailable for global search",
            "Copy here",
            "Move here",
            "Create shortcut here",
            "Cancel",
        ),
    };
    ui.set_text_go(go.into());
    ui.set_text_back(back.into());
    ui.set_text_forward(forward.into());
    ui.set_text_up(up.into());
    ui.set_text_refresh(refresh.into());
    ui.set_text_home(home.into());
    ui.set_text_desktop(desktop.into());
    ui.set_text_downloads(downloads.into());
    ui.set_text_documents(documents.into());
    ui.set_text_pictures(pictures.into());
    ui.set_text_music(music.into());
    ui.set_text_videos(videos.into());
    ui.set_text_quick_access(quick_access.into());
    ui.set_text_drives(drives.into());
    match language {
        Language::Chinese => {
            ui.set_text_network_locations("网络位置".into());
            ui.set_text_network("网络".into());
            ui.set_text_network_discovery_loading("正在发现网络设备…".into());
            ui.set_text_network_discovery_empty("未发现网络设备".into());
            ui.set_text_network_discovery_error("网络设备发现失败".into());
        }
        Language::English => {
            ui.set_text_network_locations("Network locations".into());
            ui.set_text_network("Network".into());
            ui.set_text_network_discovery_loading("Discovering network devices…".into());
            ui.set_text_network_discovery_empty("No network devices found".into());
            ui.set_text_network_discovery_error("Network discovery failed".into());
        }
    }
    ui.set_text_name(name.into());
    ui.set_text_type(kind.into());
    ui.set_text_modified(modified.into());
    ui.set_text_size(size.into());
    ui.set_text_request_access(request_access.into());
    ui.set_text_menu(menu.into());
    ui.set_text_settings(settings.into());
    ui.set_text_settings_title(settings.into());
    ui.set_text_theme(theme.into());
    ui.set_text_theme_system(theme_system.into());
    ui.set_text_theme_light(theme_light.into());
    ui.set_text_theme_dark(theme_dark.into());
    ui.set_text_language(language_label.into());
    ui.set_text_language_chinese(chinese.into());
    ui.set_text_language_english(english.into());
    ui.set_text_show_hidden_files(show_hidden_files.into());
    ui.set_text_show_system_files(show_system_files.into());
    ui.set_text_settings_general(settings_general.into());
    ui.set_text_settings_appearance(settings_appearance.into());
    ui.set_text_settings_developer(settings_developer.into());
    ui.set_text_settings_about(settings_about.into());
    ui.set_text_version(version.into());
    let (
        everything_title,
        everything_connection,
        everything_path,
        select_everything_program,
        everything_instance,
        everything_instance_help,
        everything_actions,
        test_connection,
        start_everything,
        everything_help,
        everything_help_install,
        everything_help_dependencies,
        everything_help_independent,
        everything_help_settings,
        discover_everything,
        download_everything,
        everything_settings,
    ) = match language {
        Language::Chinese => (
            "Everything 集成配置",
            "连接配置",
            "程序路径",
            "选择程序",
            "Everything 实例名",
            "“1.5a”是实例名，不是版本号。通常保持默认；仅在自定义或同时运行多个实例时修改，并与运行中的实例一致。",
            "连接状态与操作",
            "测试连接",
            "启动 Everything",
            "使用说明",
            "• 需要安装 Everything 1.5 x64 并保持后台运行，数据库须完成加载。",
            "• 地址栏快速搜索、搜索结果索引信息和普通目录文件夹大小依赖 Everything；文件夹大小索引需启用，目录也必须已收录。",
            "• 普通目录浏览和 UNC 网络路径不依赖 Everything；未索引位置不会回退为递归扫描。",
            "• 修改程序路径或实例名后请重新测试；AsterFiles 不会自动修改 Everything 的索引设置。",
            "自动发现",
            "下载 Everything",
            "前往 Everything 设置",
        ),
        Language::English => (
            "Everything integration setup",
            "Connection settings",
            "Program path",
            "Choose program",
            "Everything instance name",
            "“1.5a” is an instance name, not a version. Keep the default unless you use a custom or multiple instance, and match the running instance.",
            "Connection status and actions",
            "Test connection",
            "Start Everything",
            "How it works",
            "• Install Everything 1.5 x64, keep it running in the background, and wait for its database to finish loading.",
            "• Address-bar search, indexed result details, and local folder sizes depend on Everything. Folder-size indexing must be enabled and the folder included.",
            "• Normal folder browsing and UNC navigation do not depend on Everything. Unindexed locations never fall back to slow recursive scanning.",
            "• Retest after changing the path or instance. AsterFiles never changes Everything index settings automatically.",
            "Auto-detect",
            "Download Everything",
            "Open Everything settings",
        ),
    };
    ui.set_text_everything_title(everything_title.into());
    ui.set_text_everything_connection(everything_connection.into());
    ui.set_text_everything_path(everything_path.into());
    ui.set_text_select_everything_program(select_everything_program.into());
    ui.set_text_everything_instance(everything_instance.into());
    ui.set_text_everything_instance_help(everything_instance_help.into());
    ui.set_text_everything_actions(everything_actions.into());
    ui.set_text_test_connection(test_connection.into());
    ui.set_text_start_everything(start_everything.into());
    ui.set_text_everything_help(everything_help.into());
    ui.set_text_everything_help_install(everything_help_install.into());
    ui.set_text_everything_help_dependencies(everything_help_dependencies.into());
    ui.set_text_everything_help_independent(everything_help_independent.into());
    ui.set_text_everything_help_settings(everything_help_settings.into());
    ui.set_text_discover_everything(discover_everything.into());
    ui.set_text_download_everything(download_everything.into());
    ui.set_text_everything_settings(everything_settings.into());
    ui.set_text_ui_showcase(ui_showcase.into());
    ui.set_text_ui_showcase_detail(ui_showcase_detail.into());
    ui.set_text_show_delete_confirmation(show_delete_confirmation.into());
    ui.set_text_show_conflict_confirmation(show_conflict_confirmation.into());
    ui.set_text_show_exit_confirmation(show_exit_confirmation.into());
    ui.set_text_show_operation_window(show_operation_window.into());
    ui.set_text_window_trace_title(window_trace_title.into());
    ui.set_text_window_trace_detail(window_trace_detail.into());
    ui.set_text_window_trace_start(window_trace_start.into());
    ui.set_text_window_trace_active(window_trace_active.into());
    ui.set_text_window_trace_path(window_trace_path.into());
    ui.set_text_loading(loading.into());
    ui.set_text_empty_folder(empty_folder.into());
    ui.set_text_no_search_results(no_search_results.into());
    ui.set_text_new_tab(new_tab.into());
    ui.set_text_close_tab(close_tab.into());
    ui.set_text_all_tabs(all_tabs.into());
    ui.set_text_window_minimize(minimize.into());
    ui.set_text_window_restore(restore.into());
    ui.set_text_window_maximize(maximize.into());
    ui.set_text_window_close(close.into());
    ui.set_text_address(address.into());
    ui.set_text_cancel_edit(cancel_edit.into());
    ui.set_text_search_recursive(search_recursive.into());
    ui.set_text_search_current(search_current.into());
    ui.set_text_search_global_disabled(search_global_disabled.into());
    ui.set_text_drop_copy(drop_copy.into());
    ui.set_text_drop_move(drop_move.into());
    ui.set_text_drop_link(drop_link.into());
    ui.set_text_drop_cancel(drop_cancel.into());
    let (context_search, context_loading, context_empty) = match language {
        Language::Chinese => ("搜索命令", "正在加载 Windows 菜单…", "没有匹配的命令"),
        Language::English => (
            "Search commands",
            "Loading Windows menu…",
            "No matching commands",
        ),
    };
    ui.set_text_context_search(context_search.into());
    ui.set_text_context_loading(context_loading.into());
    ui.set_text_context_empty(context_empty.into());
    let (file_operations, task_count, cancel_operation, retry_operation) = match language {
        Language::Chinese => ("文件操作", "个任务", "取消", "重试"),
        Language::English => ("File operations", "tasks", "Cancel", "Retry"),
    };
    ui.set_text_file_operations(file_operations.into());
    ui.set_text_task_count(task_count.into());
    ui.set_text_cancel_operation(cancel_operation.into());
    ui.set_text_retry_operation(retry_operation.into());
}

fn display_path(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
}

fn network_location_default_name(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| crate::network::unc_host_key(path))
        .unwrap_or_else(|| display_path(path))
}

fn is_drive_root(path: &Path) -> bool {
    let value = path.as_os_str().to_string_lossy();
    value.len() == 3 && value.as_bytes()[1] == b':' && matches!(value.as_bytes()[2], b'\\' | b'/')
}

fn initial_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("C:\\").to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn type_select_test_context(window: u32, request: u64) -> TypeSelectContext {
        TypeSelectContext {
            tab_id: TabId(window),
            request_id: RequestId(request),
            view_mode: ViewMode::Details,
            page_source: PageSource::Directory,
            group_field: GroupField::None,
            group_direction: SortDirection::Ascending,
            sort_field: SortField::Name,
            sort_direction: SortDirection::Ascending,
        }
    }

    fn type_select_rows(names: &[&str]) -> Vec<(EntryId, String)> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| (EntryId(index as u32 + 1), (*name).to_owned()))
            .collect()
    }

    #[test]
    fn issue_20_type_select_handles_prefix_cycle_wrap_case_and_digits() {
        let rows = type_select_rows(&["Alpha", "alpine", "Beta", "7-Zip", "apricot"]);
        let context = type_select_test_context(1, 1);
        let start = Instant::now();
        let mut state = TypeSelectState::default();

        assert_eq!(
            state.select(context, start, 'a', &rows, None),
            Some(EntryId(1))
        );
        assert_eq!(
            state.select(
                context,
                start + Duration::from_millis(100),
                'l',
                &rows,
                Some(EntryId(1))
            ),
            Some(EntryId(1))
        );
        assert_eq!(
            state.select(
                context,
                start + Duration::from_millis(200),
                'x',
                &rows,
                Some(EntryId(1))
            ),
            None
        );

        state.clear();
        assert_eq!(
            state.select(context, start, 'a', &rows, Some(EntryId(1))),
            Some(EntryId(2))
        );
        assert_eq!(
            state.select(
                context,
                start + Duration::from_millis(100),
                'a',
                &rows,
                Some(EntryId(2))
            ),
            Some(EntryId(5))
        );
        assert_eq!(
            state.select(
                context,
                start + Duration::from_millis(200),
                'a',
                &rows,
                Some(EntryId(5))
            ),
            Some(EntryId(1))
        );
        state.clear();
        assert_eq!(
            state.select(context, start, '7', &rows, None),
            Some(EntryId(4))
        );
    }

    #[test]
    fn issue_20_type_select_timeout_context_and_no_match_are_isolated() {
        let rows = type_select_rows(&["alpha", "beta", "bravo"]);
        let first = type_select_test_context(1, 7);
        let second = type_select_test_context(2, 7);
        let start = Instant::now();
        let mut state = TypeSelectState::default();

        assert_eq!(
            state.select(first, start, 'b', &rows, None),
            Some(EntryId(2))
        );
        assert_eq!(
            state.select(
                first,
                start + Duration::from_millis(100),
                'z',
                &rows,
                Some(EntryId(2))
            ),
            None
        );
        assert_eq!(state.buffer, "bz");
        assert_eq!(
            state.select(
                first,
                start + Duration::from_millis(1_100),
                'b',
                &rows,
                Some(EntryId(2))
            ),
            Some(EntryId(3))
        );
        assert_eq!(state.buffer, "b");
        assert_eq!(
            state.select(
                second,
                start + TYPE_SELECT_TIMEOUT,
                'a',
                &rows,
                Some(EntryId(3))
            ),
            Some(EntryId(1))
        );
        assert_eq!(state.context, Some(second));
        assert!(state.clear());
        assert!(!state.clear());
    }

    #[test]
    fn issue_20_projection_uses_current_group_order_and_loaded_entries_only() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\group")], 0, [0, 1, 2, 3]);
        let tab_id = app.active_window_state().active_tab;
        {
            let tab = app.tab_mut(tab_id).unwrap();
            let mut alpha = focus_entry(1, r"C:\group\alpha.txt");
            alpha.kind = crate::domain::EntryKind::File;
            let mut folder = focus_entry(2, r"C:\group\folder");
            folder.kind = crate::domain::EntryKind::Directory;
            tab.replace_entries(vec![alpha, folder]);
        }
        app.update_directory_preference(PathBuf::from(r"C:\group"), |preference| {
            preference.group_field = GroupField::Kind;
            preference.group_direction = SortDirection::Descending;
        });
        let preference = app.directory_preference(Path::new(r"C:\group"));
        assert_eq!(preference.group_field, GroupField::Kind);
        assert_eq!(preference.group_direction, SortDirection::Descending);
        let projection = type_select_projection(&app, app.tab(tab_id).unwrap());
        let expected = directory_group_projections(
            &app,
            app.tab(tab_id).unwrap(),
            app.tab(tab_id).unwrap().visible_entries(),
        )
        .into_iter()
        .flat_map(|group| group.entries)
        .collect::<Vec<_>>();
        assert_eq!(
            projection.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            expected
        );

        let tab = app.tab_mut(tab_id).unwrap();
        tab.page_source = PageSource::Search;
        tab.load_state = LoadState::Complete;
        tab.search_total = Some(4);
        tab.replace_entries(vec![
            focus_entry(1, r"C:\search\alpha.txt"),
            focus_entry(4, r"C:\search\delta.txt"),
        ]);
        let projection = type_select_projection(&app, app.tab(tab_id).unwrap());
        assert_eq!(
            projection.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![EntryId(1), EntryId(4)]
        );
        assert_eq!(app.active().latest_request, RequestId(0));
    }

    #[test]
    fn issue_20_selection_mutation_targets_the_owning_window() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window = app.register_window(
            vec![PathBuf::from(r"C:\two")],
            0,
            session_store::WindowPlacement {
                x: 100,
                y: 100,
                width: 800,
                height: 600,
            },
        );
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        app.window_mut(first_window)
            .unwrap()
            .tabs
            .get_mut(&first_tab)
            .unwrap()
            .replace_entries(vec![focus_entry(1, r"C:\one\alpha.txt")]);
        app.window_mut(second_window)
            .unwrap()
            .tabs
            .get_mut(&second_tab)
            .unwrap()
            .replace_entries(vec![focus_entry(2, r"C:\two\beta.txt")]);
        let shared = Arc::new(Mutex::new(app));

        let updated = mutate_window_selection(&shared, first_window, |tab| {
            tab.select_entry(EntryId(1), false, false);
        });
        assert_eq!(updated.as_ref().map(|(id, _)| *id), Some(first_tab));
        let app = shared.lock().unwrap();
        assert_eq!(
            app.window(first_window).unwrap().active().selected,
            vec![EntryId(1)]
        );
        assert!(
            app.window(second_window)
                .unwrap()
                .active()
                .selected
                .is_empty()
        );
    }
    #[test]
    fn issue_20_reveal_scrolls_only_when_the_match_is_outside_the_viewport() {
        assert_eq!(
            reveal_scroll_target(-80.0, 100.0, 40.0, 160.0, 400.0),
            -80.0
        );
        assert_eq!(reveal_scroll_target(0.0, 240.0, 40.0, 160.0, 400.0), -120.0);
        assert_eq!(
            reveal_scroll_target(-200.0, 80.0, 40.0, 160.0, 400.0),
            -80.0
        );
    }
    #[test]
    fn issue_20_window_keyboard_route_excludes_editors_ime_and_shortcuts() {
        let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"));
        for marker in [
            "WindowEvent::Ime(ime)",
            "ui.get_file_list_keyboard_target()",
            "!editing_address",
            "!settings_active",
            "!control",
            "!alt",
            "!super_key",
            "!ime_composing.get()",
            "event.text.as_ref().is_some_and",
        ] {
            assert!(source.contains(marker), "missing keyboard guard: {marker}");
        }
    }

    #[test]
    fn issue_49_active_tab_uses_one_continuous_contour() {
        let ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
        let shape = ui
            .split_once("component ActiveTabShape inherits Rectangle {")
            .expect("active tab shape exists")
            .1
            .split_once("component IconButton inherits Rectangle {")
            .expect("active tab shape has a component boundary")
            .0;

        assert_eq!(shape.matches("Path {").count(), 1);
        assert_eq!(shape.matches("Rectangle {").count(), 0);
        assert_eq!(shape.matches("MoveTo {").count(), 1);
        assert_eq!(shape.matches("CubicTo {").count(), 4);
        assert_eq!(shape.matches("Close { }").count(), 1);
        assert!(shape.contains("x: root.width / 1px; y: root.height / 1px;"));
    }
    #[test]
    fn issue_43_file_name_font_size_has_one_semantic_source() {
        let ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
        let semantic_font_size = "font-size: VisualStyle.file-name-font-size;";

        assert_eq!(
            ui.matches("out property <length> file-name-font-size: 12px;")
                .count(),
            1
        );
        assert_eq!(ui.matches(semantic_font_size).count(), 5);
        let details_and_list_name = ui
            .split_once("for segment in entry.name-segments: Text {")
            .expect("details and list must render segmented file names")
            .1;
        assert!(details_and_list_name.contains(semantic_font_size));
        assert_eq!(
            ui.matches("text: entry.name; color: VisualStyle.c24252a; font-size: VisualStyle.file-name-font-size;")
                .count(),
            2
        );
        let rename_editor = ui
            .split_once("rename-editor := TextInput {")
            .expect("rename editor must exist")
            .1;
        assert!(rename_editor.contains(semantic_font_size));
        assert!(!ui.contains("text: entry.name; color: VisualStyle.c24252a; font-size: 14px;"));
    }
    #[test]
    fn issue_14_everything_status_text_covers_progress_success_and_failures() {
        use platform::windows::everything::{EverythingError, EverythingStatus, EverythingVersion};

        assert_eq!(
            everything_progress_text(Language::Chinese, EverythingOperation::Test),
            "正在测试连接…"
        );
        assert_eq!(
            everything_progress_text(Language::English, EverythingOperation::Start),
            "Starting Everything…"
        );
        let connected = everything_connected_status(
            Language::Chinese,
            &EverythingStatus {
                version: EverythingVersion {
                    major: 1,
                    minor: 5,
                    revision: 0,
                    build: 1400,
                },
                instance_name: "1.5a".into(),
                database_loaded: true,
                folder_size_indexed: false,
            },
        );
        assert!(connected.contains("版本 1.5.0.1400"));
        assert!(connected.contains("实例 1.5a"));
        assert!(connected.contains("数据库已加载"));
        assert!(connected.contains("文件夹大小索引未启用"));
        assert!(
            everything_error_text(Language::Chinese, &EverythingError::Timeout).contains("超时")
        );
        assert!(
            everything_error_text(
                Language::English,
                &EverythingError::InvalidExecutable(PathBuf::from("missing.exe"))
            )
            .contains("invalid")
        );
    }

    #[test]
    fn issue_14_everything_page_is_fully_localized_and_wired() {
        let ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));

        for marker in [
            "text-everything-title",
            "text-select-everything-program",
            "text-everything-instance-help",
            "text-everything-help-dependencies",
            "callback select-everything-program();",
            "enabled: !root.everything-busy",
            "maximum: max(0px, settings-content.preferred-height - settings-scroll.height);",
            "settings-scroll.viewport-y = max(-maximum, min(0px, settings-scroll.viewport-y + event.delta-y));",
        ] {
            assert!(
                ui.contains(marker),
                "missing Everything page marker: {marker}"
            );
        }
    }
    #[test]
    fn network_directory_scheduler_serializes_one_host_and_allows_another() {
        let mut scheduler = NetworkDirectoryScheduler::default();
        let server = NetworkExecutionKey::from_unc(Path::new(r"\\server\one")).unwrap();
        let other = NetworkExecutionKey::from_unc(Path::new(r"\\other\one")).unwrap();
        scheduler.push(server.clone(), network_directory_request(r"\\server\one"));
        scheduler.push(server.clone(), network_directory_request(r"\\server\two"));
        scheduler.push(other.clone(), network_directory_request(r"\\other\one"));

        let (first_key, _) = scheduler.next_ready().unwrap();
        assert_eq!(first_key, server);
        let (second_key, _) = scheduler.next_ready().unwrap();
        assert_eq!(second_key, other);
        assert!(scheduler.next_ready().is_none());

        scheduler.complete(&first_key);
        let (third_key, _) = scheduler.next_ready().unwrap();
        assert_eq!(third_key, server);
    }

    #[test]
    fn network_directory_scheduler_discards_cancelled_request() {
        let mut scheduler = NetworkDirectoryScheduler::default();
        let key = NetworkExecutionKey::from_unc(Path::new(r"\\server\one")).unwrap();
        let request = network_directory_request(r"\\server\one");
        request
            .cancel
            .store(true, std::sync::atomic::Ordering::Release);
        scheduler.push(key, request);
        let cancelled = scheduler.take_cancelled();
        assert_eq!(cancelled.len(), 1);
        assert!(scheduler.next_ready().is_none());
    }

    #[test]
    fn network_login_coordinator_replaces_and_cancels_previous_request() {
        let mut coordinator = NetworkLoginCoordinator::default();
        let first = coordinator.begin(
            WindowId(1),
            TabId(1),
            RequestId(1),
            PathBuf::from(r"\\server\one"),
        );
        let second = coordinator.begin(
            WindowId(2),
            TabId(2),
            RequestId(2),
            PathBuf::from(r"\\server\two"),
        );

        assert!(first.cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(!coordinator.is_current(first.generation));
        assert!(coordinator.is_current(second.generation));
        coordinator.cancel_generation(first.generation);
        assert!(!second.cancel.load(std::sync::atomic::Ordering::Acquire));
        coordinator.cancel_generation(second.generation);
        assert!(second.cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(coordinator.current.is_none());
    }

    fn test_image(size: u32) -> platform::windows_shell_icons::ShellIconRgba {
        platform::windows_shell_icons::ShellIconRgba {
            width: size,
            height: size,
            pixels: vec![0; size as usize * size as usize * 4],
        }
    }

    #[test]
    fn issue_18_navigation_uses_pending_entries_before_the_first_thumbnail() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\old")], 0, [0, 1, 2, 3]);
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.begin_navigation(PathBuf::from(r"C:\new"), NavigationKind::Normal);
        assert!(directory_display_entries(tab).is_empty());
        tab.append_pending(vec![focus_entry(1, r"C:\new\photo.png")]);
        assert_eq!(directory_display_entries(tab).len(), 1);
        assert!(app.icons.is_empty());
        assert!(app.thumbnail_cache.is_empty());
    }
    #[test]
    fn issue_18_active_projection_avoids_building_the_hidden_view_model() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
        app.update_directory_preference(PathBuf::from(r"C:\grid"), |preference| {
            preference.view_mode = ViewMode::MediumIcons
        });
        let tab_id = app.active_window_state().active_tab;
        app.tab_mut(tab_id)
            .unwrap()
            .replace_entries(vec![focus_entry(1, r"C:\grid\photo.png")]);
        app.tab_mut(tab_id).unwrap().load_state = LoadState::Complete;
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
        let ui = headless_file_view();

        refresh_ui(&ui, &state);

        assert_eq!(ui.get_files().row_count(), 0);
        assert_eq!(
            ui.get_grid_rows()
                .iter()
                .map(|row| row.entries.row_count())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn issue_18_thumbnail_sizes_use_files_style_standard_buckets() {
        assert_eq!(grid_thumbnail_request_px(ViewMode::MediumIcons, 1.0), 128);
        assert_eq!(grid_thumbnail_request_px(ViewMode::LargeIcons, 1.5), 256);
        assert_eq!(
            grid_thumbnail_request_px(ViewMode::ExtraLargeIcons, 1.5),
            256
        );
    }

    #[test]
    fn issue_18_grid_batches_skip_eager_icons_but_details_keep_them() {
        let mut grid = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
        grid.update_directory_preference(PathBuf::from(r"C:\grid"), |preference| {
            preference.view_mode = ViewMode::ExtraLargeIcons
        });
        let tab = grid.tab_mut(TabId(1)).unwrap();
        tab.latest_request = RequestId(4);
        tab.load_state = LoadState::Loading;
        let grid = Arc::new(Mutex::new(grid));
        let grid_requests = apply_event(
            &grid,
            DirectoryEvent::Batch {
                tab_id: TabId(1),
                request_id: RequestId(4),
                entries: vec![focus_entry(1, r"C:\grid\photo.png")],
            },
        );
        assert!(grid_requests.is_empty());

        let mut details =
            AppState::new_for_test(vec![PathBuf::from(r"C:\details")], 0, [0, 1, 2, 3]);
        let tab = details.tab_mut(TabId(1)).unwrap();
        tab.latest_request = RequestId(5);
        tab.load_state = LoadState::Loading;
        let details = Arc::new(Mutex::new(details));
        let details_requests = apply_event(
            &details,
            DirectoryEvent::Batch {
                tab_id: TabId(1),
                request_id: RequestId(5),
                entries: vec![focus_entry(1, r"C:\details\photo.png")],
            },
        );
        assert_eq!(details_requests.len(), 1);
        assert!(!details_requests[0].thumbnail);
    }
    #[test]
    fn issue_18_rebuilt_projection_is_coalesced_for_large_directories() {
        assert!(should_rebuild_projection(0, 32));
        assert!(!should_rebuild_projection(32, 255));
        assert!(should_rebuild_projection(32, 288));
    }

    #[test]
    fn issue_18_thumbnail_cache_evicts_the_oldest_entry() {
        let mut cache = HashMap::new();
        let mut order = VecDeque::new();
        insert_bounded_image(
            &mut cache,
            &mut order,
            (PathBuf::from("first.png"), 256),
            test_image(1),
            2,
        );
        insert_bounded_image(
            &mut cache,
            &mut order,
            (PathBuf::from("second.png"), 256),
            test_image(1),
            2,
        );
        insert_bounded_image(
            &mut cache,
            &mut order,
            (PathBuf::from("third.png"), 256),
            test_image(1),
            2,
        );

        assert!(!cache.contains_key(&(PathBuf::from("first.png"), 256)));
        assert!(cache.contains_key(&(PathBuf::from("second.png"), 256)));
        assert!(cache.contains_key(&(PathBuf::from("third.png"), 256)));
    }
    fn context_test_row(
        id: i32,
        label: &str,
        search_text: &str,
        separator: bool,
    ) -> ContextCommandRow {
        ContextCommandRow {
            id,
            node_id: 0,
            label: label.into(),
            search_text: search_text.into(),
            hint: "".into(),
            enabled: !separator,
            separator,
            shell: id >= SHELL_CONTEXT_COMMAND_BASE,
            checked: false,
            default: false,
            submenu: false,
            loading: false,
            placeholder: false,
            icon_kind: 0,
        }
    }

    #[test]
    fn issue_18_builtin_menu_rows_expose_only_their_own_icons() {
        let app = AppState::new_for_test(vec![PathBuf::from(r"C:\menu")], 0, [0, 1, 2, 3]);
        let (rows, submenus) = built_in_context_rows(&app, false, true);
        assert_eq!(rows[0].icon_kind, 1);
        assert_eq!(rows[1].icon_kind, 2);
        assert_eq!(rows[2].icon_kind, 3);
        assert_eq!(rows[3].icon_kind, 4);
        assert_eq!(rows[5].icon_kind, 5);
        assert!(
            submenus
                .values()
                .flatten()
                .filter(|row| row.checked)
                .all(|row| row.icon_kind == 0)
        );

        let shell = context_test_row(SHELL_CONTEXT_COMMAND_BASE + 1, "Shell", "", false);
        assert_eq!(shell.icon_kind, 0);
    }
    #[test]
    fn quick_menu_composes_built_ins_before_single_shell_separator() {
        let built_ins = vec![
            context_test_row(1, "New folder", "", false),
            context_test_row(4, "Paste", "", false),
        ];
        let shell = vec![
            context_test_row(SHELL_CONTEXT_COMMAND_BASE + 1, "Open terminal", "", false),
            context_test_row(SHELL_CONTEXT_COMMAND_BASE + 2, "Properties", "", false),
        ];
        let rows = compose_quick_menu_rows(&built_ins, &shell);
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [
                1,
                4,
                -1,
                SHELL_CONTEXT_COMMAND_BASE + 1,
                SHELL_CONTEXT_COMMAND_BASE + 2
            ]
        );
        assert_eq!(rows.iter().filter(|row| row.separator).count(), 1);
        assert!(rows[..2].iter().all(|row| !row.shell));
        assert!(rows[3..].iter().all(|row| row.shell));
    }

    #[test]
    fn quick_menu_shell_projection_preserves_hmenu_order() {
        use platform::windows::context_menu::{ClassicMenuItem, ClassicMenuItemKind};
        let shell_item = |command_id, title: &str| ClassicMenuItem {
            command_id: Some(command_id),
            title: title.to_owned(),
            verb: Some(format!("verb-{command_id}")),
            enabled: true,
            checked: false,
            default: false,
            kind: ClassicMenuItemKind::Command,
        };
        let mut menu = QuickMenuState::default();
        let rows = project_shell_menu_items(
            &mut menu,
            vec![
                shell_item(31, "First"),
                shell_item(7, "Second"),
                shell_item(42, "Third"),
            ],
        );

        assert_eq!(
            rows.iter()
                .map(|row| row.label.as_str())
                .collect::<Vec<_>>(),
            ["First", "Second", "Third"]
        );
        assert_eq!(
            rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [
                SHELL_CONTEXT_COMMAND_BASE + 31,
                SHELL_CONTEXT_COMMAND_BASE + 7,
                SHELL_CONTEXT_COMMAND_BASE + 42,
            ]
        );
    }

    #[test]
    fn quick_menu_loaded_content_replaces_placeholder_height() {
        let placeholder_height = context_menu_content_height(
            &(0..QUICK_MENU_PLACEHOLDER_ROWS)
                .map(|_| quick_menu_placeholder())
                .collect::<Vec<_>>(),
        );
        let loaded_rows = (0..12)
            .map(|index| {
                context_test_row(
                    SHELL_CONTEXT_COMMAND_BASE + index,
                    &format!("Shell {index}"),
                    "",
                    false,
                )
            })
            .collect::<Vec<_>>();
        let loaded_height = context_menu_content_height(&loaded_rows);

        assert!(loaded_height > placeholder_height);
        assert!(
            root_popup_height_for_content(loaded_height, false, 1.0)
                > root_popup_height_for_content(placeholder_height, true, 1.0)
        );
    }
    #[test]
    fn quick_menu_placeholder_and_pending_snapshot_are_not_interactive() {
        let placeholder = quick_menu_placeholder();
        assert!(placeholder.placeholder && placeholder.loading && !placeholder.enabled);
        let pending = pending_shell_rows(&[
            context_test_row(SHELL_CONTEXT_COMMAND_BASE + 1, "Properties", "", false),
            quick_menu_separator(),
        ]);
        assert!(!pending[0].enabled && pending[0].loading && !pending[0].placeholder);
        assert!(pending[1].separator && !pending[1].enabled && !pending[1].loading);
        assert_eq!(first_enabled_context_index(&pending), -1);
    }

    #[test]
    fn quick_menu_inflight_prewarm_projects_placeholders_until_snapshot_arrives() {
        let (rows, loading, cache_hit, session_hit) = project_cached_shell_rows(true, None);
        assert!(loading && !cache_hit && !session_hit);
        assert_eq!(rows.len(), QUICK_MENU_PLACEHOLDER_ROWS);
        assert!(
            rows.iter()
                .all(|row| row.placeholder && row.loading && !row.enabled)
        );
    }

    #[test]
    fn quick_menu_expired_snapshot_projects_placeholders() {
        let snapshot = QuickMenuSnapshot {
            rows: vec![context_test_row(
                SHELL_CONTEXT_COMMAND_BASE + 1,
                "Old",
                "",
                false,
            )],
            captured_at: Instant::now() - QUICK_MENU_SNAPSHOT_TTL - Duration::from_millis(1),
        };
        let (rows, loading, cache_hit, session_hit) =
            project_cached_shell_rows(true, Some(&snapshot));
        assert!(loading && !cache_hit && !session_hit);
        assert_eq!(rows.len(), QUICK_MENU_PLACEHOLDER_ROWS);
        assert!(rows.iter().all(|row| row.placeholder));
    }
    #[test]
    fn quick_menu_snapshots_are_isolated_by_full_context_key() {
        let first = QuickMenuKey {
            window_id: WindowId(1),
            tab_id: TabId(2),
            navigation_request: RequestId(3),
            paths: vec![PathBuf::from(r"C:\first.txt")],
            folder: Some(PathBuf::from(r"C:\")),
        };
        let second = QuickMenuKey {
            paths: vec![PathBuf::from(r"C:\second.txt")],
            ..first.clone()
        };
        let mut snapshots = HashMap::new();
        snapshots.insert(
            first.clone(),
            QuickMenuSnapshot {
                rows: vec![context_test_row(
                    SHELL_CONTEXT_COMMAND_BASE + 1,
                    "First",
                    "",
                    false,
                )],
                captured_at: Instant::now(),
            },
        );
        assert!(snapshots.contains_key(&first));
        assert!(!snapshots.contains_key(&second));
    }
    #[test]
    fn quick_menu_filter_matches_case_chinese_and_command_verb_without_shell_work() {
        let rows = vec![
            context_test_row(1, "Copy", "copy", false),
            context_test_row(-1, "", "", true),
            context_test_row(
                SHELL_CONTEXT_COMMAND_BASE + 42,
                "在终端中打开",
                "openinterminal",
                false,
            ),
        ];
        assert_eq!(
            filtered_context_rows(&rows, "COPY")
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(
            filtered_context_rows(&rows, "终端")
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [SHELL_CONTEXT_COMMAND_BASE + 42]
        );
        assert_eq!(
            filtered_context_rows(&rows, "openinterminal")
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [SHELL_CONTEXT_COMMAND_BASE + 42]
        );
        assert!(filtered_context_rows(&rows, "missing").is_empty());
    }

    #[test]
    fn quick_menu_filter_removes_leading_trailing_and_duplicate_separators() {
        let rows = vec![
            context_test_row(-1, "", "", true),
            context_test_row(1, "Copy", "copy", false),
            context_test_row(-1, "", "", true),
            context_test_row(-1, "", "", true),
            context_test_row(2, "Paste", "paste", false),
            context_test_row(-1, "", "", true),
        ];
        let result = filtered_context_rows(&rows, "");
        assert_eq!(result.len(), 3);
        assert!(!result[0].separator);
        assert!(result[1].separator);
        assert!(!result[2].separator);
    }

    #[test]
    fn quick_menu_content_height_uses_compact_separator_height() {
        let rows = vec![
            context_test_row(1, "Open", "open", false),
            context_test_row(-1, "", "", true),
            context_test_row(2, "PowerShell 7", "powershell", false),
        ];

        assert_eq!(context_menu_content_height(&rows), 65.0);
    }

    #[test]
    fn loaded_submenu_requests_presentation_after_hidden_loading_state() {
        assert!(!submenu_projection_needs_presentation(
            PopupPresentation::ShownCloaked,
            false,
            true,
        ));
        assert!(submenu_projection_needs_presentation(
            PopupPresentation::ShownCloaked,
            false,
            false,
        ));
        assert!(!submenu_projection_needs_presentation(
            PopupPresentation::ShownCloaked,
            true,
            false,
        ));
        assert!(!submenu_projection_needs_presentation(
            PopupPresentation::Presented,
            false,
            false,
        ));
    }
    #[test]
    fn parent_submenu_branch_remains_current_while_a_descendant_is_open() {
        let active = crate::quick_menu_popup::MenuSessionId {
            owner_window: WindowId(1),
            tab_id: TabId(2),
            request_id: RequestId(3),
            generation: 4,
        };
        let root = crate::quick_menu_popup::MenuBranchId(10);
        let parent = crate::quick_menu_popup::MenuBranchId(11);
        let descendant = crate::quick_menu_popup::MenuBranchId(12);
        let mut session = crate::quick_menu_popup::QuickMenuPopupSession::default();
        session.open_root(active, root);
        assert!(session.push_branch(
            crate::quick_menu_popup::MenuEventIdentity {
                session: active,
                branch: root,
            },
            parent,
        ));
        let parent_event = crate::quick_menu_popup::MenuEventIdentity {
            session: active,
            branch: parent,
        };
        assert!(session.push_branch(parent_event, descendant));

        assert!(submenu_slot_is_current(&session, 0, Some(parent_event)));
        assert!(session.close_to_branch(parent_event));
        assert_eq!(session.branches().len(), 2);
    }
    #[test]
    fn loaded_submenu_rows_replace_the_visible_slot_snapshot() {
        let active = crate::quick_menu_popup::MenuSessionId {
            owner_window: WindowId(1),
            tab_id: TabId(2),
            request_id: RequestId(3),
            generation: 4,
        };
        let root = crate::quick_menu_popup::MenuBranchId(10);
        let child = crate::quick_menu_popup::MenuBranchId(11);
        let mut session = crate::quick_menu_popup::QuickMenuPopupSession::default();
        session.open_root(active, root);
        assert!(session.push_branch(
            crate::quick_menu_popup::MenuEventIdentity {
                session: active,
                branch: root,
            },
            child,
        ));
        let event = crate::quick_menu_popup::MenuEventIdentity {
            session: active,
            branch: child,
        };
        let mut slot_rows = Vec::new();
        let loaded = vec![context_test_row(
            SHELL_CONTEXT_COMMAND_BASE + 42,
            "Loaded child",
            "loaded",
            false,
        )];

        assert!(replace_submenu_slot_rows_if_current(
            &session,
            0,
            Some(event),
            &mut slot_rows,
            &loaded,
        ));
        assert_eq!(slot_rows, loaded);
    }

    #[test]
    fn stale_submenu_rows_do_not_replace_the_visible_slot_snapshot() {
        let active = crate::quick_menu_popup::MenuSessionId {
            owner_window: WindowId(1),
            tab_id: TabId(2),
            request_id: RequestId(3),
            generation: 4,
        };
        let root = crate::quick_menu_popup::MenuBranchId(10);
        let current_child = crate::quick_menu_popup::MenuBranchId(12);
        let mut session = crate::quick_menu_popup::QuickMenuPopupSession::default();
        session.open_root(active, root);
        assert!(session.push_branch(
            crate::quick_menu_popup::MenuEventIdentity {
                session: active,
                branch: root,
            },
            current_child,
        ));
        let mut slot_rows = vec![context_test_row(1, "Current", "current", false)];
        let stale = vec![context_test_row(2, "Stale", "stale", false)];

        assert!(!replace_submenu_slot_rows_if_current(
            &session,
            0,
            Some(crate::quick_menu_popup::MenuEventIdentity {
                session: active,
                branch: crate::quick_menu_popup::MenuBranchId(11),
            }),
            &mut slot_rows,
            &stale,
        ));
        assert_eq!(slot_rows[0].label.as_str(), "Current");
    }
    #[test]
    fn quick_menu_ignores_duplicate_request_for_visible_submenu() {
        assert!(submenu_request_is_duplicate(true, Some(7), 7));
        assert!(!submenu_request_is_duplicate(false, Some(7), 7));
        assert!(!submenu_request_is_duplicate(true, Some(7), 8));
    }

    #[test]
    fn quick_menu_navigation_wraps_and_skips_disabled_and_separators() {
        let mut disabled = context_test_row(2, "Paste", "paste", false);
        disabled.enabled = false;
        let rows = vec![
            context_test_row(-1, "", "", true),
            context_test_row(1, "Copy", "copy", false),
            disabled,
            context_test_row(3, "Rename", "rename", false),
        ];
        assert_eq!(first_enabled_context_index(&rows), 1);
        assert_eq!(next_enabled_context_index(&rows, 1, 1), 3);
        assert_eq!(next_enabled_context_index(&rows, 3, 1), 1);
        assert_eq!(next_enabled_context_index(&rows, 1, -1), 3);
    }

    #[test]
    fn quick_menu_projects_preloaded_submenu_rows_by_original_token() {
        use platform::windows::context_menu::{ClassicMenuItem, ClassicMenuItemKind};
        let mut menu = QuickMenuState::default();
        let rows = project_shell_menu_items(
            &mut menu,
            vec![ClassicMenuItem {
                command_id: Some(31),
                title: "PowerShell 7".to_owned(),
                verb: Some("powershell7x64".to_owned()),
                enabled: true,
                checked: false,
                default: false,
                kind: ClassicMenuItemKind::Submenu {
                    token: 2,
                    items: vec![ClassicMenuItem {
                        command_id: Some(32_770),
                        title: "Open here".to_owned(),
                        verb: None,
                        enabled: true,
                        checked: false,
                        default: false,
                        kind: ClassicMenuItemKind::Command,
                    }],
                },
            }],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(menu.submenu_tokens.get(&rows[0].node_id), Some(&2));
        let children = menu
            .preloaded_submenu_rows
            .get(&2)
            .expect("preloaded PowerShell rows");
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].label.as_str(), "Open here");
        assert_eq!(children[0].id, SHELL_CONTEXT_COMMAND_BASE + 32_770);
    }
    #[test]
    fn quick_menu_submenu_result_accepts_only_the_current_shell_identity() {
        let mut menu = QuickMenuState {
            identity: Some(QuickMenuIdentity {
                session_id: 41,
                request_id: 7,
                key: QuickMenuKey {
                    window_id: WindowId(1),
                    tab_id: TabId(2),
                    navigation_request: RequestId(3),
                    paths: vec![PathBuf::from(r"C:\selected.txt")],
                    folder: None,
                },
                ready: true,
            }),
            active_submenu_token: Some(99),
            active_submenu_request: 12,
            ..QuickMenuState::default()
        };

        assert!(submenu_result_matches(&menu, 41, 7, 12, 99));
        assert!(!submenu_result_matches(&menu, 41, 7, 11, 99));
        assert!(!submenu_result_matches(&menu, 41, 7, 12, 98));
        assert!(!submenu_result_matches(&menu, 41, 6, 12, 99));
        assert!(!submenu_result_matches(&menu, 40, 7, 12, 99));

        menu.active_submenu_token = None;
        assert!(!submenu_result_matches(&menu, 41, 7, 12, 99));
    }
    #[test]
    fn quick_menu_loaded_submenu_cache_reuses_nonempty_results() {
        let mut menu = QuickMenuState::default();
        menu.loaded_submenu_rows.insert(
            8,
            vec![context_test_row(
                SHELL_CONTEXT_COMMAND_BASE + 42,
                "Cached child",
                "cached",
                false,
            )],
        );

        assert_eq!(cached_submenu_rows(&menu, 7), None);
        assert_eq!(
            cached_submenu_rows(&menu, 8).map(|rows| rows.len()),
            Some(1)
        );
        assert_eq!(
            cached_submenu_rows(&menu, 8).expect("cached rows")[0].id,
            SHELL_CONTEXT_COMMAND_BASE + 42
        );
    }
    #[test]
    fn quick_menu_submenu_projection_keeps_token_and_leaf_command_id() {
        use platform::windows::context_menu::{ClassicMenuItem, ClassicMenuItemKind};
        let submenu = shell_menu_item_row(
            ClassicMenuItem {
                command_id: None,
                title: "NanaZip".to_owned(),
                verb: None,
                enabled: true,
                checked: false,
                default: false,
                kind: ClassicMenuItemKind::Submenu {
                    token: 9,
                    items: Vec::new(),
                },
            },
            9,
        )
        .unwrap();
        assert!(submenu.enabled && submenu.submenu);
        assert_eq!(submenu.node_id, 9);
        assert_eq!(submenu.id, -1);

        let leaf = shell_menu_item_row(
            ClassicMenuItem {
                command_id: Some(42),
                title: "Extract here".to_owned(),
                verb: Some("extract".to_owned()),
                enabled: true,
                checked: true,
                default: true,
                kind: ClassicMenuItemKind::Command,
            },
            0,
        )
        .unwrap();
        assert_eq!(leaf.id, SHELL_CONTEXT_COMMAND_BASE + 42);
        assert!(leaf.checked && leaf.default && !leaf.submenu);
    }

    #[test]
    fn search_window_keeps_slint_model_bounded_at_deep_offsets() {
        assert_eq!(
            search_window_for_index(65_536, 133_796, 1),
            SearchWindow {
                start: 65_280,
                len: SEARCH_WINDOW_ITEM_LIMIT,
            }
        );
        assert_eq!(
            search_window_for_index(133_795, 133_796, 1),
            SearchWindow {
                start: 133_376,
                len: 420,
            }
        );
        assert_eq!(SEARCH_WINDOW_ITEM_LIMIT, 768);
    }

    #[test]
    fn search_window_preserves_position_inside_the_loaded_window() {
        let window = search_window_for_index(65_536, 133_796, 1);
        assert_eq!(
            search_window_viewport_y(65_536, window, ViewMode::Details, 1),
            -256.0 * 40.0
        );
        let grid_window = search_window_for_index(65_536, 133_796, 4);
        assert_eq!(grid_window.start % 4, 0);
        assert_eq!(
            search_window_viewport_y(65_536, grid_window, ViewMode::MediumIcons, 4),
            -64.0 * 148.0
        );
    }
    #[test]
    fn logical_search_scroll_maps_each_view_mode_to_absolute_result_index() {
        assert_eq!(
            search_result_index_at_scroll(-65_536.0 * 40.0, 133_796, ViewMode::Details, 1),
            65_536
        );
        assert_eq!(
            search_result_index_at_scroll(-65_536.0 * 34.0, 133_796, ViewMode::List, 1),
            65_536
        );
        assert_eq!(
            search_result_index_at_scroll(-16_384.0 * 148.0, 133_796, ViewMode::MediumIcons, 4),
            65_536
        );
        assert_eq!(
            search_result_index_at_scroll(-f32::MAX, 133_796, ViewMode::Details, 1),
            133_795
        );
    }
    #[test]
    fn tab_drop_routes_physical_screen_cursor_at_each_dpi() {
        assert_eq!(
            screen_to_client_physical(800, 400, 640, 250),
            (160.0, 150.0)
        );
        assert_eq!(grid_thumbnail_request_px(ViewMode::MediumIcons, 1.0), 128);
        assert_eq!(grid_thumbnail_request_px(ViewMode::LargeIcons, 1.5), 256);
        assert_eq!(
            grid_thumbnail_request_px(ViewMode::ExtraLargeIcons, 1.5),
            256
        );
    }

    #[test]
    fn grid_rows_never_reuse_a_thumbnail_smaller_than_the_active_view() {
        let path = PathBuf::from(r"C:\grid\photo.png");
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
        app.update_directory_preference(PathBuf::from(r"C:\grid"), |preference| {
            preference.view_mode = ViewMode::LargeIcons
        });
        app.thumbnail_cache.insert(
            (path.clone(), 100),
            platform::windows_shell_icons::ShellIconRgba {
                width: 100,
                height: 100,
                pixels: vec![0; 100 * 100 * 4],
            },
        );
        let tab = app.active();
        let row = file_row(
            &focus_entry(1, path.to_str().unwrap()),
            tab,
            Texts::new(Language::Chinese),
            &app,
            Some(148),
        );
        assert_eq!(row.icon.size().width, 0);

        app.thumbnail_cache.insert(
            (path, 148),
            platform::windows_shell_icons::ShellIconRgba {
                width: 148,
                height: 148,
                pixels: vec![0; 148 * 148 * 4],
            },
        );
        let tab = app.active();
        let row = file_row(
            &focus_entry(1, r"C:\grid\photo.png"),
            tab,
            Texts::new(Language::Chinese),
            &app,
            Some(148),
        );
        assert_eq!(row.icon.size().width, 148);
    }
    #[test]
    fn grid_directory_batch_keeps_icon_loading_separate_from_thumbnail_planning() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
        app.update_directory_preference(PathBuf::from(r"C:\grid"), |preference| {
            preference.view_mode = ViewMode::MediumIcons
        });
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.latest_request = RequestId(4);
        tab.load_state = LoadState::Loading;
        let state = Arc::new(Mutex::new(app));

        let requests = apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: TabId(1),
                request_id: RequestId(4),
                entries: vec![focus_entry(1, r"C:\grid\photo.png")],
            },
        );

        assert!(requests.is_empty());
        assert_eq!(
            grid_thumbnail_request_rows(&vec![148.0; 125], -296.0, 444.0, 296.0),
            (0..=7).collect::<Vec<_>>()
        );
        assert_eq!(
            grid_thumbnail_request_rows(&vec![148.0; 125], -1480.0, 296.0, 296.0),
            (7..=14).collect::<Vec<_>>()
        );
        assert_eq!(
            grid_thumbnail_request_rows(
                &[32.0, 220.0, 220.0, 32.0, 220.0, 220.0, 220.0],
                -500.0,
                220.0,
                0.0,
            ),
            vec![3, 4]
        );
    }

    #[test]
    fn network_directory_events_never_request_shell_icons() {
        let path = PathBuf::from(r"\\server\share");
        let mut app = AppState::new_for_test(vec![path.clone()], 0, [0, 1, 2, 3]);
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.latest_request = RequestId(4);
        tab.load_state = LoadState::Loading;
        let state = Arc::new(Mutex::new(app));

        let batch = apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: TabId(1),
                request_id: RequestId(4),
                entries: vec![focus_entry(1, r"\\server\share\folder")],
            },
        );
        let finished = apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: TabId(1),
                request_id: RequestId(4),
                path,
                skipped: 0,
            },
        );

        assert!(batch.is_empty());
        assert!(finished.is_empty());
    }

    #[test]
    fn operation_window_waits_until_conflict_is_resolved_and_runtime_reaches_threshold() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        let id = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from("a")),
                Some(PathBuf::from("b")),
            )],
        );
        app.operations.start_next(OperationResource::Local).unwrap();
        app.operations.mark_running(id).unwrap();
        {
            let task = app.operations.task_mut(id).unwrap();
            task.started_at = Instant::now() - Duration::from_millis(900);
            let snapshot = crate::domain::file_operations::FileSnapshot {
                path: PathBuf::from("a"),
                is_directory: false,
                size_bytes: Some(1),
                modified: None,
            };
            task.set_conflict(crate::domain::file_operations::OperationConflict {
                category: crate::domain::file_operations::ConflictCategory::ExistingFile,
                source: snapshot.clone(),
                destination: snapshot,
            })
            .unwrap();
        }
        assert!(!should_auto_open_operation_window(&app));

        app.operations
            .task_mut(id)
            .unwrap()
            .resolve_conflict(crate::domain::file_operations::ConflictDecision {
                action: crate::domain::file_operations::ConflictAction::Replace,
                apply_to_all: false,
            })
            .unwrap();
        assert!(should_auto_open_operation_window(&app));
    }

    #[test]
    fn remaining_time_uses_minutes_only_when_needed() {
        assert_eq!(
            format_remaining_time(Language::English, 42.0),
            "About 42 seconds remaining"
        );
        assert_eq!(
            format_remaining_time(Language::English, 102.0),
            "About 1min 42s remaining"
        );
        assert_eq!(
            format_remaining_time(Language::Chinese, 102.0),
            "预计剩余 1 分 42 秒"
        );
    }

    #[test]
    fn reorders_columns_to_normalized_slots_and_clamps_edges() {
        let mut order = [
            ColumnKind::Name,
            ColumnKind::Kind,
            ColumnKind::Size,
            ColumnKind::Modified,
            ColumnKind::Created,
        ];
        assert!(reorder_column_to_slot(&mut order, ColumnKind::Kind, 0));
        assert_eq!(order[0], ColumnKind::Kind);
        assert!(reorder_column_to_slot(&mut order, ColumnKind::Kind, 8));
        assert_eq!(order[4], ColumnKind::Kind);
        assert!(!reorder_column_to_slot(&mut order, ColumnKind::Size, 1));
        assert_eq!(normalized_column_slot(1, 0, 5), 0);
        assert_eq!(normalized_column_slot(1, 2, 5), 1);
        assert_eq!(normalized_column_slot(1, 5, 5), 4);
    }

    #[test]
    fn column_insertion_slot_uses_real_widths_midpoints_and_viewport_offset() {
        let order = [
            ColumnKind::Name,
            ColumnKind::Kind,
            ColumnKind::Size,
            ColumnKind::Modified,
            ColumnKind::Created,
        ];
        let widths = [100, 240, 80, 160, 120];
        let geometry = ColumnHeaderGeometry {
            x: 100.0,
            y: 0.0,
            width: 580.0,
            viewport_x: 0.0,
        };
        assert_eq!(
            column_insertion_slot(99.0, 20.0, geometry, &order, &widths),
            None
        );
        assert_eq!(
            column_insertion_slot(149.9, 20.0, geometry, &order, &widths),
            Some(0)
        );
        assert_eq!(
            column_insertion_slot(150.0, 20.0, geometry, &order, &widths),
            Some(1)
        );
        assert_eq!(
            column_insertion_slot(319.9, 20.0, geometry, &order, &widths),
            Some(1)
        );
        assert_eq!(
            column_insertion_slot(320.0, 20.0, geometry, &order, &widths),
            Some(2)
        );
        assert_eq!(
            column_insertion_slot(460.0, 20.0, geometry, &order, &widths),
            Some(2)
        );
        assert_eq!(
            column_insertion_slot(680.0, 20.0, geometry, &order, &widths),
            Some(4)
        );
        let scrolled_geometry = ColumnHeaderGeometry {
            width: 300.0,
            viewport_x: -220.0,
            ..geometry
        };
        assert_eq!(
            column_insertion_slot(100.0, 20.0, scrolled_geometry, &order, &widths),
            Some(2)
        );
        assert_eq!(
            column_insertion_slot(401.0, 20.0, scrolled_geometry, &order, &widths),
            None
        );
        assert_eq!(
            column_insertion_slot(320.0, 39.0, geometry, &order, &widths),
            None
        );
    }

    #[test]
    fn column_drag_threshold_cancel_and_source_isolation_preserve_state() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let widths = app.active_column_layout().widths;
        let request = app.active().latest_request;
        let sort = app.active().sort_field;
        assert!(app.begin_column_drag(app.active_window, 1, 100.0, 20.0));
        assert_eq!(
            app.update_column_drag(
                100.0 + COLUMN_DRAG_THRESHOLD - 0.1,
                20.0,
                0.0,
                0.0,
                800.0,
                0.0,
            ),
            None
        );
        assert!(matches!(
            app.column_drag.unwrap().phase,
            ColumnDragPhase::Pressed
        ));
        assert_eq!(
            app.update_column_drag(100.0 + COLUMN_DRAG_THRESHOLD, 20.0, 0.0, 0.0, 800.0, 0.0,),
            Some(0)
        );
        assert!(matches!(
            app.column_drag.unwrap().phase,
            ColumnDragPhase::Dragging { .. }
        ));
        assert!(app.cancel_column_drag());
        assert_eq!(
            app.default_directory_view.columns.order,
            [
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Created
            ]
        );

        assert!(app.begin_column_drag(app.active_window, 1, 100.0, 20.0));
        assert_eq!(
            app.update_column_drag(799.0, 20.0, 0.0, 0.0, 800.0, -200.0),
            Some(4)
        );
        assert!(app.cancel_column_drag());
        assert_eq!(
            app.default_directory_view.columns.order,
            [
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Created
            ]
        );
        assert_eq!(app.active_column_layout().widths, widths);
        assert_eq!(app.active().latest_request, request);
        assert_eq!(app.active().sort_field, sort);

        let tab_id = app.active_window_state().active_tab;
        app.tab_mut(tab_id).unwrap().page_source = PageSource::Search;
        assert!(app.begin_column_drag(app.active_window, 0, 10.0, 20.0));
        assert_eq!(
            app.update_column_drag(799.0, 20.0, 0.0, 0.0, 800.0, -300.0),
            Some(5)
        );
        assert!(app.finish_column_drag(true));
        assert_eq!(
            app.default_directory_view.columns.order,
            [
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Created
            ]
        );
        assert_eq!(
            app.search_view.columns.order,
            [
                ColumnKind::Modified,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Created,
                ColumnKind::Name
            ]
        );
    }

    #[test]
    fn column_drag_invalid_release_and_source_change_cancel_without_commit() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        assert!(app.begin_column_drag(app.active_window, 2, 100.0, 20.0));
        assert_eq!(
            app.update_column_drag(0.0, 20.0, 0.0, 0.0, 800.0, 0.0),
            Some(0)
        );
        assert!(!app.finish_column_drag(false));
        assert_eq!(
            app.default_directory_view.columns.order,
            [
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Created
            ]
        );

        assert!(app.begin_column_drag(app.active_window, 2, 100.0, 20.0));
        assert_eq!(
            app.update_column_drag(0.0, 80.0, 0.0, 0.0, 800.0, 0.0),
            None
        );
        assert!(!app.finish_column_drag(true));
        assert_eq!(
            app.default_directory_view.columns.order,
            [
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Created
            ]
        );

        assert!(app.begin_column_drag(app.active_window, 2, 100.0, 20.0));
        let tab_id = app.active_window_state().active_tab;
        app.tab_mut(tab_id).unwrap().page_source = PageSource::Search;
        assert_eq!(
            app.update_column_drag(0.0, 20.0, 0.0, 0.0, 800.0, 0.0),
            None
        );
        assert!(app.column_drag.is_none());
        assert_eq!(
            app.default_directory_view.columns.order,
            [
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Created
            ]
        );
        assert_eq!(app.search_view.columns.order, ColumnLayout::default().order);
    }

    #[test]
    fn column_reorder_commit_isolated_by_page_source() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        assert!(app.commit_column_reorder(PageSource::Directory, 0, 4));
        assert_eq!(
            app.directory_preference(Path::new("a")).columns.order,
            [
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Name,
                ColumnKind::Created
            ]
        );
        assert_eq!(app.search_view.columns.order, ColumnLayout::default().order);
        assert!(app.commit_column_reorder(PageSource::Search, 3, 0));
        assert_eq!(
            app.directory_preference(Path::new("a")).columns.order,
            [
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Modified,
                ColumnKind::Name,
                ColumnKind::Created
            ]
        );
        assert_eq!(
            app.search_view.columns.order,
            [
                ColumnKind::Modified,
                ColumnKind::Name,
                ColumnKind::Kind,
                ColumnKind::Size,
                ColumnKind::Created
            ]
        );
        assert!(!app.commit_column_reorder(PageSource::Search, 3, 1));
    }

    fn test_window_placement(x: i32) -> session_store::WindowPlacement {
        session_store::WindowPlacement {
            x,
            y: 80,
            width: 1180,
            height: 760,
        }
    }

    #[test]
    fn window_registry_allocates_global_tab_ids_without_reuse() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_extra = app.create_tab(PathBuf::from("two"));
        let second_window =
            app.register_window(vec![PathBuf::from("three")], 0, test_window_placement(160));
        let second_first = app.window(second_window).unwrap().active_tab;

        assert_eq!(
            app.window(first_window).unwrap().tab_order,
            [TabId(1), first_extra]
        );
        assert_eq!(second_first, TabId(3));
        app.active_window = second_window;
        assert_eq!(app.close_tab(second_first), None);
        let second_extra = app.create_tab(PathBuf::from("four"));
        assert_eq!(second_extra, TabId(4));
    }

    #[test]
    fn window_state_keeps_tabs_history_and_placement_isolated() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let closed = app.create_tab(PathBuf::from("closed"));
        app.close_tab(closed).unwrap();
        let second_window = app.register_window(
            vec![PathBuf::from("second"), PathBuf::from("active")],
            1,
            test_window_placement(240),
        );

        let first = app.window(first_window).unwrap();
        let second = app.window(second_window).unwrap();
        assert_eq!(first.closed_tabs, [PathBuf::from("closed")]);
        assert!(second.closed_tabs.is_empty());
        assert_eq!(first.active_tab, TabId(1));
        assert_eq!(second.active_tab, TabId(4));
        assert_eq!(first.placement.x, 80);
        assert_eq!(second.placement.x, 240);
    }

    #[test]
    fn closing_one_window_cancels_only_its_tabs_and_keeps_shared_operation() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        let (_, first_cancel) = app
            .window_mut(first_window)
            .unwrap()
            .tabs
            .get_mut(&first_tab)
            .unwrap()
            .begin_navigation(PathBuf::from("one/new"), NavigationKind::Normal);
        let (_, second_cancel) = app
            .window_mut(second_window)
            .unwrap()
            .tabs
            .get_mut(&second_tab)
            .unwrap()
            .begin_navigation(PathBuf::from("two/new"), NavigationKind::Normal);
        let operation = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            Some(first_tab),
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );

        assert_eq!(
            app.close_window(first_window),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(first_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(!second_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(app.window(second_window).is_some());
        assert!(app.operations.task(operation).is_some());
        assert_eq!(
            app.close_window(second_window),
            Some(WindowCloseDecision::ExitApplication)
        );
    }

    #[test]
    fn closing_window_cancels_and_removes_network_discovery() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first = app.active_window;
        let second = app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let (request, cancel) = app.network_discovery.entry(first).or_default().begin();
        assert_eq!(request, DiscoveryRequestId(1));
        app.network_discovery_errors
            .insert(first, "temporary".to_owned());

        assert_eq!(
            app.close_window(first),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(!app.network_discovery.contains_key(&first));
        assert!(!app.network_discovery_errors.contains_key(&first));
        assert!(app.window(second).is_some());
    }
    #[test]
    fn file_operations_use_network_resource_when_either_endpoint_is_unc() {
        let local = OperationItem::pending(
            Some(PathBuf::from(r"C:\source.txt")),
            Some(PathBuf::from(r"D:\target.txt")),
        );
        let to_network = OperationItem::pending(
            Some(PathBuf::from(r"C:\source.txt")),
            Some(PathBuf::from(r"\\server\share\target.txt")),
        );
        let from_network = OperationItem::pending(
            Some(PathBuf::from(r"\\server\share\source.txt")),
            Some(PathBuf::from(r"C:\target.txt")),
        );

        assert_eq!(operation_resource(&[local]), OperationResource::Local);
        assert_eq!(
            operation_resource(&[to_network]),
            OperationResource::Network
        );
        assert_eq!(
            operation_resource(&[from_network]),
            OperationResource::Network
        );
    }
    #[test]
    fn network_location_name_prefers_share_or_host() {
        assert_eq!(
            network_location_default_name(Path::new(r"\\server\share\folder")),
            "folder"
        );
        assert_eq!(
            network_location_default_name(Path::new(r"\\server")),
            "server"
        );
    }
    #[test]
    fn network_sidebar_index_skips_shell_only_location_without_shifting_device() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
        app.imported_network_locations = vec![
            NetworkLocation {
                id: 1,
                source: NetworkLocationSource::WindowsImported,
                display_name: "Path location".into(),
                sort_order: 0,
                target: NetworkTarget::WindowsPath(PathBuf::from(r"\\server\share")),
            },
            NetworkLocation {
                id: 2,
                source: NetworkLocationSource::WindowsImported,
                display_name: "Shell only".into(),
                sort_order: 1,
                target: NetworkTarget::ShellItemId(PathBuf::from("shell:::virtual")),
            },
        ];
        let (request_id, _) = app
            .network_discovery
            .entry(app.active_window)
            .or_default()
            .begin();
        app.network_discovery
            .get_mut(&app.active_window)
            .unwrap()
            .append(
                request_id,
                [NetworkDeviceTarget {
                    id: crate::network::network_device_id(Path::new(r"\\LiuYanghomeNAS")),
                    display_name: "LiuYanghomeNAS".into(),
                    shell_identity: None,
                    unc_path: Some(PathBuf::from(r"\\LiuYanghomeNAS")),
                }],
            );

        assert_eq!(
            sidebar_navigation_target(&app, app.sidebar.len() + 2),
            Some(PathBuf::from(r"\\LiuYanghomeNAS"))
        );
    }
    #[test]
    fn cached_network_devices_skip_automatic_rediscovery() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
        let window_id = app.active_window;
        assert!(network_discovery_needed(&app, window_id));
        app.network_discovery.insert(
            window_id,
            DiscoveryCoordinator::with_devices(vec![NetworkDeviceTarget {
                id: crate::network::network_device_id(Path::new(r"\\LiuYanghomeNAS")),
                display_name: "LiuYanghomeNAS".into(),
                shell_identity: None,
                unc_path: Some(PathBuf::from(r"\\LiuYanghomeNAS")),
            }]),
        );
        assert!(!network_discovery_needed(&app, window_id));
    }

    #[test]
    fn outbound_copy_does_not_refresh_source_but_move_does() {
        use platform::windows::drag_drop::{DropEffect, OutboundDropResult};

        let result = |effect, dropped| OutboundDropResult {
            effect,
            dropped,
            performed_effect_reported: true,
        };
        assert!(!should_refresh_outbound_drag_source(result(
            DropEffect::Copy,
            true
        )));
        assert!(!should_refresh_outbound_drag_source(result(
            DropEffect::Link,
            true
        )));
        assert!(!should_refresh_outbound_drag_source(result(
            DropEffect::Move,
            false
        )));
        assert!(should_refresh_outbound_drag_source(result(
            DropEffect::Move,
            true
        )));
    }
    #[test]
    fn directory_events_route_to_non_active_windows_by_global_tab_id() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        for tab_id in [first_tab, second_tab] {
            let tab = app.tab_mut(tab_id).unwrap();
            tab.latest_request = RequestId(7);
            tab.load_state = LoadState::Loading;
        }
        let state = Arc::new(Mutex::new(app));
        let entry = focus_entry(1, "two/item.txt");

        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: second_tab,
                request_id: RequestId(7),
                entries: vec![entry],
            },
        );
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: second_tab,
                request_id: RequestId(7),
                path: PathBuf::from("two"),
                skipped: 0,
            },
        );
        let app = state.lock().unwrap();
        assert!(app.tab(first_tab).unwrap().entries.is_empty());
        assert_eq!(app.tab(second_tab).unwrap().entries.len(), 1);
        assert_eq!(app.tab(second_tab).unwrap().load_state, LoadState::Complete);
    }

    #[test]
    fn search_and_icon_events_route_to_non_active_windows() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let second_tab = app.window(second_window).unwrap().active_tab;
        let tab = app.tab_mut(second_tab).unwrap();
        let (request_id, _) = tab.begin_search(SearchScope::Global, "item".into());
        let search_entry = focus_entry(1, "two/item.txt");
        assert!(
            apply_search_page_event(
                &mut app,
                second_tab,
                request_id,
                0,
                vec![search_entry.clone()],
                1,
                1,
            )
            .is_some()
        );
        let event = IconEvent {
            tab_id: second_tab,
            request_id,
            target: IconTarget::Entry(EntryId(1)),
            path: search_entry.path,
            icon: platform::windows_shell_icons::ShellIconRgba {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0, 0],
            },
            actual_thumbnail: false,
            requested_px: 0,
        };
        assert!(icon_event_is_current(&app, &event));
    }

    #[test]
    fn closed_window_rejects_late_results_while_other_window_accepts_them() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        for tab_id in [first_tab, second_tab] {
            let tab = app.tab_mut(tab_id).unwrap();
            tab.latest_request = RequestId(9);
            tab.load_state = LoadState::Loading;
        }
        app.close_window(first_window).unwrap();
        let state = Arc::new(Mutex::new(app));
        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: first_tab,
                request_id: RequestId(9),
                entries: vec![focus_entry(1, "one/late.txt")],
            },
        );
        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: second_tab,
                request_id: RequestId(9),
                entries: vec![focus_entry(1, "two/current.txt")],
            },
        );
        let app = state.lock().unwrap();
        assert!(app.tab(first_tab).is_none());
        assert_eq!(app.tab(second_tab).unwrap().pending_entries.len(), 1);
    }

    #[test]
    fn close_decision_only_confirms_exit_for_last_window_with_active_tasks() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let operation = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );
        app.operations.start_next(OperationResource::Local).unwrap();
        app.operations.mark_running(operation).unwrap();
        assert_eq!(
            app.request_window_close(first_window),
            WindowCloseAction::ConfirmApplicationExit
        );
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        assert_eq!(
            app.request_window_close(first_window),
            WindowCloseAction::CloseWindow
        );
        app.close_window(first_window).unwrap();
        assert_eq!(
            app.request_window_close(second_window),
            WindowCloseAction::ConfirmApplicationExit
        );
        assert!(app.operations.task(operation).is_some());
    }

    #[test]
    fn last_window_without_tasks_keeps_state_until_session_snapshot_is_read() {
        let app = AppState::new_for_test(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            1,
            [0, 1, 2, 3],
        );
        let window = app.active_window;

        assert_eq!(
            app.request_window_close(window),
            WindowCloseAction::ExitApplication
        );
        assert_eq!(
            app.stable_paths(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(app.stable_active_path_index(), 1);
        assert!(app.window(window).is_some());
    }

    fn begin_drag_at(app: &mut AppState, index: usize) -> (WindowId, TabId) {
        let window_id = app.active_window;
        let tab_id = app.active_window_state().tab_order[index];
        assert!(app.begin_tab_drag(window_id, tab_id, index, 100.0, 20.0));
        (window_id, tab_id)
    }

    #[test]
    fn tab_drag_threshold_and_invalid_release_preserve_order() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let original = app.active_window_state().tab_order.clone();
        begin_drag_at(&mut app, 1);
        assert_eq!(
            app.update_tab_drag(104.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            None
        );
        assert!(!app.finish_tab_drag(true));
        assert_eq!(app.active_window_state().tab_order, original);

        begin_drag_at(&mut app, 1);
        assert_eq!(
            app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            Some(2)
        );
        assert!(!app.finish_tab_drag(false));
        assert_eq!(app.active_window_state().tab_order, original);
    }

    #[test]
    fn single_file_tab_window_can_begin_drag() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let window = app.active_window;
        let tab = app.active_window_state().active_tab;

        assert!(app.begin_tab_drag(window, tab, 0, 100.0, 20.0));
        assert!(app.drag_source_is_single_file_tab_window(window));
        assert_eq!(app.active_window_state().tab_order, [tab]);
    }

    #[test]
    fn tab_drag_reorders_only_ids_and_preserves_request_identity() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            1,
            [0, 1, 2, 3],
        );
        let active = app.active_window_state().active_tab;
        let tab = app.tab_mut(active).unwrap();
        let (request_id, cancel) =
            tab.begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        let (_, moved) = begin_drag_at(&mut app, 0);
        assert_eq!(
            app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            Some(2)
        );
        assert!(app.finish_tab_drag(true));
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(2), TabId(3), moved]
        );
        assert_eq!(app.active_window_state().active_tab, active);
        assert_eq!(app.tab(active).unwrap().latest_request, request_id);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn detached_tab_commits_only_after_destination_is_reserved() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source_window = app.active_window;
        let source_order = app.active_window_state().tab_order.clone();
        let (_, tab_id) = begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        assert!(
            app.detach_dragged_tab_to_window(source_window, test_window_placement(160))
                .is_none()
        );
        assert_eq!(app.window(source_window).unwrap().tab_order, source_order);
        assert_eq!(app.window_for_tab(tab_id), Some(source_window));

        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();
        assert_eq!(outcome.tab_id, tab_id);
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(app.window(destination).unwrap().tab_order, [tab_id]);
        assert_eq!(app.window(source_window).unwrap().tab_order, [TabId(2)]);
    }

    #[test]
    fn detaching_pending_tab_cancels_old_request_and_requires_restart() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let tab_id = app.active_window_state().active_tab;
        let (_, token) = app
            .tab_mut(tab_id)
            .unwrap()
            .begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(token.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            outcome.restart,
            Some(DetachedTabRestart::Directory(PathBuf::from("pending")))
        );
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
    }

    #[test]
    fn single_tab_cross_window_move_closes_source_and_keeps_identity() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let destination = app.register_window(
            vec![PathBuf::from("c"), PathBuf::from("d")],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().active_tab;
        let request = app.tab(tab_id).unwrap().latest_request;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        let outcome = app.move_dragged_tab_to_window(destination, 1).unwrap();

        assert!(outcome.source_window_closed);
        assert!(app.window(source).is_none());
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(
            app.window(destination).unwrap().tab_order,
            [TabId(2), tab_id, TabId(3)]
        );
        assert_eq!(app.tab(tab_id).unwrap().latest_request, request);
    }

    #[test]
    fn cross_window_move_inserts_at_target_and_keeps_single_owner() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let destination = app.register_window(
            vec![PathBuf::from("c"), PathBuf::from("d")],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().tab_order[0];
        let request = app.tab(tab_id).unwrap().latest_request;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        let outcome = app.move_dragged_tab_to_window(destination, 1).unwrap();

        assert_eq!(app.window(source).unwrap().tab_order, [TabId(2)]);
        assert_eq!(
            app.window(destination).unwrap().tab_order,
            [TabId(3), tab_id, TabId(4)]
        );
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(app.window(destination).unwrap().active_tab, tab_id);
        assert_eq!(app.tab(tab_id).unwrap().latest_request, request);
        assert!(!outcome.source_window_closed);
    }

    #[test]
    fn cross_window_move_failure_and_cancel_leave_source_untouched() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let tab_id = app.window(source).unwrap().active_tab;
        app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        assert!(app.move_dragged_tab_to_window(WindowId(999), 0).is_none());
        assert!(app.cancel_tab_drag());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert_eq!(app.window(source).unwrap().tab_order, [tab_id, TabId(2)]);
    }

    #[test]
    fn closing_target_window_does_not_cancel_source_drag() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let target = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let tab_id = app.window(source).unwrap().active_tab;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        assert!(
            app.update_tab_drag(108.0, 20.0, 47.0, 540.0, 0.0, 178.0)
                .is_some()
        );

        assert!(!app.cancel_tab_drag_for_window(target));
        assert!(app.tab_drag.is_some_and(|drag| drag.window_id == source));
        assert_eq!(
            app.close_window(target),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(app.tab_drag.is_some());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
    }

    #[test]
    fn closing_source_window_cancels_its_drag() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let target = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let tab_id = app.window(source).unwrap().active_tab;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        assert!(app.cancel_tab_drag_for_window(source));
        assert!(app.tab_drag.is_none());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert!(app.window(target).is_some());
    }

    #[test]
    fn physical_client_coordinates_convert_once_across_dpi_boundaries() {
        assert_eq!(
            physical_client_to_logical(1250.0, 350.0, 1000, 200, 1.0),
            (250.0, 150.0)
        );
        assert_eq!(
            physical_client_to_logical(1250.0, 350.0, 1000, 200, 1.5),
            (500.0 / 3.0, 100.0)
        );
        assert_eq!(
            physical_client_to_logical(900.0, 260.0, 750, 200, 1.0),
            (150.0, 60.0)
        );
    }

    #[test]
    fn external_insertion_slot_uses_midpoints_scroll_and_bounds() {
        assert_eq!(
            external_tab_insertion_slot(46.0, 47.0, 300.0, 0.0, 80.0, 3),
            None
        );
        assert_eq!(
            external_tab_insertion_slot(47.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(0)
        );
        assert_eq!(
            external_tab_insertion_slot(87.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(1)
        );
        assert_eq!(
            external_tab_insertion_slot(132.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(1)
        );
        assert_eq!(
            external_tab_insertion_slot(300.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(3)
        );
        assert_eq!(
            external_tab_insertion_slot(52.0, 47.0, 300.0, -100.0, 80.0, 3),
            Some(1)
        );
        assert_eq!(
            external_tab_insertion_slot(348.0, 47.0, 300.0, 0.0, 80.0, 3),
            None
        );
    }

    #[test]
    fn single_tab_unmatched_drop_moves_original_window_without_detaching() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let tab_id = app.active_window_state().active_tab;
        {
            let tab = app.tab_mut(tab_id).unwrap();
            tab.current_path = Some(PathBuf::from("a/current"));
            tab.back_history = vec![PathBuf::from("a/previous")];
            tab.forward_history = vec![PathBuf::from("a/next")];
            tab.replace_entries(vec![focus_entry(7, "a/current/item.txt")]);
            tab.selected = vec![EntryId(7)];
            tab.focused = Some(EntryId(7));
            tab.selection_anchor = Some(EntryId(7));
            tab.sort_field = SortField::Size;
            tab.sort_direction = SortDirection::Descending;
            tab.search_query = "needle".to_owned();
            tab.search_state = SearchState::Complete;
            tab.load_state = LoadState::Complete;
        }
        let request = app.tab(tab_id).unwrap().latest_request;
        let before_entries = app.tab(tab_id).unwrap().entries.clone();
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        app.update_tab_drag(140.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        assert_eq!(
            unmatched_tab_drop_action(true, false),
            Some(UnmatchedTabDropAction::MoveSourceWindow)
        );
        assert!(app.commit_drag_source_window_move(source, 320, 240));
        assert_eq!(app.windows.len(), 1);
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert_eq!(app.window(source).unwrap().placement.x, 320);
        assert_eq!(app.window(source).unwrap().placement.y, 240);
        let tab = app.tab(tab_id).unwrap();
        assert_eq!(tab.latest_request, request);
        assert_eq!(tab.current_path.as_deref(), Some(Path::new("a/current")));
        assert_eq!(tab.back_history, [PathBuf::from("a/previous")]);
        assert_eq!(tab.forward_history, [PathBuf::from("a/next")]);
        assert_eq!(tab.selected, [EntryId(7)]);
        assert_eq!(tab.focused, Some(EntryId(7)));
        assert_eq!(tab.selection_anchor, Some(EntryId(7)));
        assert_eq!(tab.sort_field, SortField::Size);
        assert_eq!(tab.sort_direction, SortDirection::Descending);
        assert_eq!(tab.search_query, "needle");
        assert_eq!(tab.search_state, SearchState::Complete);
        assert_eq!(tab.load_state, LoadState::Complete);
        assert!(Arc::ptr_eq(&tab.entries, &before_entries));
        assert!(app.tab_drag.is_none());
    }

    #[test]
    fn settings_tab_keeps_single_file_tab_on_multi_tab_detach_route() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let file_tab = app.active_window_state().active_tab;
        app.open_settings();
        assert!(app.begin_tab_drag(source, file_tab, 0, 100.0, 20.0));
        app.update_tab_drag(140.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        assert!(!app.drag_source_is_single_file_tab_window(source));
        assert_eq!(
            unmatched_tab_drop_action(false, false),
            Some(UnmatchedTabDropAction::DetachToNewWindow)
        );
    }
    #[test]
    fn unmatched_drop_routing_preserves_multi_tab_detach_and_rejects_known_windows() {
        assert_eq!(
            unmatched_tab_drop_action(false, false),
            Some(UnmatchedTabDropAction::DetachToNewWindow)
        );
        assert_eq!(unmatched_tab_drop_action(false, true), None);
        assert_eq!(
            unmatched_tab_drop_action(true, false),
            Some(UnmatchedTabDropAction::MoveSourceWindow)
        );
        assert_eq!(
            unmatched_tab_drop_action(true, true),
            Some(UnmatchedTabDropAction::MoveSourceWindow)
        );
    }

    #[test]
    fn single_tab_window_drop_keeps_grab_offset_across_dpi() {
        assert_eq!(
            single_tab_window_drop_position(800, 500, 100.0, 20.0, 1.0, 8, 8),
            (692, 472)
        );
        assert_eq!(
            single_tab_window_drop_position(800, 500, 100.0, 20.0, 1.25, 8, 8),
            (667, 467)
        );
        assert_eq!(
            single_tab_window_drop_position(800, 500, 100.0, 20.0, 1.5, 8, 8),
            (642, 462)
        );
    }

    #[test]
    fn settings_window_stays_open_when_its_file_tab_is_detached() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let file_tab = app.active_window_state().active_tab;
        let settings = app.open_settings();
        assert!(app.begin_tab_drag(source, file_tab, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(!outcome.source_window_closed);
        assert_eq!(app.window(source).unwrap().tab_order, [settings]);
        assert_eq!(app.window(source).unwrap().active_tab, settings);
    }

    #[test]
    fn cancelled_or_below_threshold_detach_restores_source() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let (_, tab_id) = begin_drag_at(&mut app, 0);
        app.update_tab_drag(103.0, 20.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        assert!(
            app.detach_dragged_tab_to_window(destination, test_window_placement(160))
                .is_none()
        );
        app.cancel_tab_drag();
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert!(app.window(destination).is_none());
    }

    #[test]
    fn file_commands_keep_the_triggering_window_and_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\WindowA")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_window = app.register_window(
            vec![PathBuf::from(r"C:\WindowB")],
            0,
            test_window_placement(160),
        );
        let second_tab = app.window(second_window).unwrap().active_tab;
        {
            let tab = app.tab_mut(first_tab).unwrap();
            tab.replace_entries(vec![focus_entry(1, r"C:\WindowA\first.txt")]);
            tab.selected = vec![EntryId(1)];
        }
        {
            let tab = app.tab_mut(second_tab).unwrap();
            tab.replace_entries(vec![focus_entry(2, r"C:\WindowB\second.txt")]);
            tab.selected = vec![EntryId(2)];
        }
        app.active_window = second_window;
        let shared = Arc::new(Mutex::new(app));
        let first = WindowSessions::new(shared.clone(), first_window);
        let (sender, _receiver) = mpsc::channel();

        submit_delete(&first, &sender, false);
        create_default_folder(&first, &sender);

        let app = shared.lock().unwrap();
        let tasks = app.operations.iter().collect::<Vec<_>>();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].origin_tab, Some(first_tab));
        assert_eq!(
            tasks[0].items[0].source.as_deref(),
            Some(Path::new(r"C:\WindowA\first.txt"))
        );
        assert_eq!(tasks[1].origin_tab, Some(first_tab));
        assert_eq!(
            tasks[1].items[0].destination.as_deref(),
            Some(Path::new(r"C:\WindowA\新建文件夹"))
        );
        assert_ne!(tasks[0].origin_tab, Some(second_tab));
    }

    #[test]
    fn permanent_delete_snapshot_keeps_the_triggering_window_and_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\WindowA")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_tab = app.window(first_window).unwrap().active_tab;
        app.tab_mut(first_tab)
            .unwrap()
            .replace_entries(vec![focus_entry(1, r"C:\WindowA\first.txt")]);
        app.tab_mut(first_tab).unwrap().selected = vec![EntryId(1)];
        let second_window = app.register_window(
            vec![PathBuf::from(r"C:\WindowB")],
            0,
            test_window_placement(160),
        );
        app.active_window = first_window;
        let shared = Arc::new(Mutex::new(app));
        let second = WindowSessions::new(shared, second_window);

        assert!(pending_permanent_delete(&second).is_none());

        let mut app = second.lock().unwrap();
        let second_tab = app.window(second_window).unwrap().active_tab;
        app.tab_mut(second_tab)
            .unwrap()
            .replace_entries(vec![focus_entry(2, r"C:\WindowB\second.txt")]);
        app.tab_mut(second_tab).unwrap().selected = vec![EntryId(2)];
        drop(app);

        let (origin_tab, items) = pending_permanent_delete(&second).unwrap();
        assert_eq!(origin_tab, second_tab);
        assert_eq!(
            items[0].source.as_deref(),
            Some(Path::new(r"C:\WindowB\second.txt"))
        );
    }
    #[test]
    fn delayed_paste_keeps_its_original_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\WindowA")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_window = app.register_window(
            vec![PathBuf::from(r"C:\WindowB")],
            0,
            test_window_placement(160),
        );
        app.active_window = second_window;
        let shared = Arc::new(Mutex::new(app));
        let (sender, _receiver) = mpsc::channel();
        let event = ClipboardEvent::Paste {
            origin_tab: first_tab,
            result: Ok(Some((
                FileOperationKind::Copy,
                vec![OperationItem::pending(
                    Some(PathBuf::from(r"C:\source.txt")),
                    Some(PathBuf::from(r"C:\WindowA\source.txt")),
                )],
            ))),
        };

        if let ClipboardEvent::Paste {
            origin_tab,
            result: Ok(Some((kind, items))),
        } = event
        {
            enqueue_operation(&shared, &sender, origin_tab, kind, items);
        }

        let app = shared.lock().unwrap();
        let task = app.operations.iter().next().unwrap();
        assert_eq!(task.origin_tab, Some(first_tab));
        assert_eq!(app.window_for_tab(first_tab), Some(first_window));
    }
    #[test]
    fn refreshing_each_window_does_not_change_the_active_window() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let first = app.active_window;
        let second = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let state = Arc::new(Mutex::new(app));
        let first_ui = headless_file_view();
        let second_ui = AppWindow::new().expect("second headless window should initialize");
        second_ui
            .window()
            .set_size(slint::LogicalSize::new(1_180.0, 760.0));

        refresh_window_ui(&first_ui, &state, first);
        refresh_window_ui(&second_ui, &state, second);

        assert_eq!(state.lock().unwrap().active_window, first);
        assert_eq!(first_ui.get_tabs().row_count(), 1);
        assert_eq!(second_ui.get_tabs().row_count(), 1);
        assert_ne!(
            first_ui.get_tabs().row_data(0).unwrap().title,
            second_ui.get_tabs().row_data(0).unwrap().title
        );
    }
    #[test]
    fn background_window_projection_does_not_change_active_window() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let first = app.active_window;
        let second = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let state = Arc::new(Mutex::new(app));

        let projected = WindowSessions::new(state.clone(), second);
        let guard = projected.peek().unwrap();
        assert_eq!(guard.active_window, first);
        drop(guard);
        assert_eq!(state.lock().unwrap().active_window, first);
    }

    #[test]
    fn partial_search_detach_cancels_and_requests_restart() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let tab_id = app.active_window_state().active_tab;
        let token = {
            let tab = app.tab_mut(tab_id).unwrap();
            let (_, token) = tab.begin_search(
                SearchScope::Directory(PathBuf::from("a")),
                "needle".to_owned(),
            );
            tab.search_state = SearchState::Partial;
            token
        };
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(token.load(std::sync::atomic::Ordering::Acquire));
        assert!(matches!(
            outcome.restart,
            Some(DetachedTabRestart::Search { query, .. }) if query == "needle"
        ));
    }

    #[test]
    fn settings_tab_is_fixed_and_bounds_the_file_tab_range() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let settings = app.open_settings();
        let third = app.create_tab(PathBuf::from("c"));
        assert!(!app.begin_tab_drag(app.active_window, settings, 2, 100.0, 20.0));
        assert!(app.begin_tab_drag(app.active_window, third, 3, 600.0, 20.0));
        assert_eq!(
            app.update_tab_drag(50.0, 20.0, 47.0, 720.0, 0.0, 178.0),
            Some(3)
        );
        assert!(!app.finish_tab_drag(true));
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(1), TabId(2), settings, third]
        );
    }

    #[test]
    fn tab_drag_slot_accounts_for_overflow_viewport_offset() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        begin_drag_at(&mut app, 2);
        assert_eq!(
            app.update_tab_drag(52.0, 20.0, 47.0, 240.0, -100.0, 80.0),
            Some(1)
        );
        app.cancel_tab_drag();
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(1), TabId(2), TabId(3)]
        );
    }

    #[test]
    fn tab_insertion_slot_uses_midpoints_gaps_ranges_and_bounds() {
        assert_eq!(tab_insertion_slot(46.0, 47.0, 300.0, 0.0, 80.0, 0..3), None);
        assert_eq!(
            tab_insertion_slot(48.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(0)
        );
        assert_eq!(
            tab_insertion_slot(87.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(0)
        );
        assert_eq!(
            tab_insertion_slot(88.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(130.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(340.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(2)
        );
        assert_eq!(
            tab_insertion_slot(348.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            None
        );
        assert_eq!(
            tab_insertion_slot(52.0, 47.0, 240.0, -100.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(305.0, 47.0, 400.0, 0.0, 80.0, 3..5),
            Some(3)
        );
        assert_eq!(
            tab_insertion_slot(390.0, 47.0, 400.0, 0.0, 80.0, 3..5),
            Some(4)
        );
    }

    #[test]
    fn astf8_round_trip_uses_reordered_paths_and_active_file_index() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            1,
            [0, 1, 2, 3],
        );
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0);
        app.finish_tab_drag(true);
        let temporary =
            std::env::temp_dir().join(format!("asterfiles-tab-order-{}.bin", std::process::id()));
        let session = session_store::SessionState::new(
            test_window_placement(80),
            app.stable_active_path_index(),
            app.stable_paths(),
        )
        .unwrap();
        session_store::save(&temporary, &session).unwrap();
        let restored = session_store::load(&temporary).unwrap();
        std::fs::remove_file(temporary).unwrap();
        assert_eq!(
            restored.windows[0].tab_paths,
            [PathBuf::from("b"), PathBuf::from("c"), PathBuf::from("a")]
        );
        assert_eq!(restored.windows[0].active_tab, 0);
    }
    #[test]
    fn complete_tab_duplication_shares_entries_without_reloading() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        let source = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        source.latest_request = RequestId(7);
        source.load_state = LoadState::Complete;
        source.replace_entries(vec![FileEntry {
            id: EntryId(1),
            original_name: "file.txt".into(),
            display_name: "file.txt".into(),
            name_highlights: Vec::new(),
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: "same".into(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
            created: None,
        }]);
        let source_entries = source.entries.clone();

        let duplicate = app.duplicate_active_tab().expect("complete tab duplicates");
        let duplicated = app.active_window_state().tabs.get(&duplicate).unwrap();

        assert!(Arc::ptr_eq(&source_entries, &duplicated.entries));
        assert_eq!(duplicated.latest_request, RequestId(7));
        assert_eq!(duplicated.load_state, LoadState::Complete);
    }
    #[test]
    fn closing_a_tab_preserves_another_session() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        assert_eq!(app.active_window_state().active_tab, second);
        assert_eq!(app.close_tab(TabId(2)), Some(TabId(1)));
        assert_eq!(app.active().current_path, Some(PathBuf::from("one")));
    }

    #[test]
    fn closing_an_inactive_tab_keeps_the_active_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        let third = app.create_tab(PathBuf::from("three"));
        assert_eq!(app.active_window_state().active_tab, third);

        assert_eq!(app.close_tab(second), Some(third));
        assert_eq!(app.active_window_state().active_tab, third);
        assert_eq!(app.active().current_path, Some(PathBuf::from("three")));
        assert!(!app.active_window_state().tabs.contains_key(&second));
    }

    #[test]
    fn settings_tab_is_singleton_and_does_not_restore() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);

        let settings = app.open_settings();
        assert_eq!(app.open_settings(), settings);
        assert_eq!(app.active_window_state().tab_order.len(), 2);
        assert_eq!(app.active().kind, TabKind::Settings);

        assert_eq!(app.close_tab(settings), Some(TabId(1)));
        assert!(app.active_window_state().closed_tabs.is_empty());
        assert!(app.restore_closed().is_none());
    }

    #[test]
    fn settings_tab_is_excluded_from_saved_paths_and_active_index() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            0,
            [0, 1, 2, 3],
        );

        app.open_settings();
        assert_eq!(
            app.stable_paths(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(app.stable_active_path_index(), 1);
    }

    #[test]
    fn last_file_tab_cannot_be_closed_while_settings_is_open() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        app.open_settings();

        assert_eq!(app.close_tab(TabId(1)), None);
        assert!(app.active_window_state().tabs.contains_key(&TabId(1)));
    }

    #[test]
    fn settings_tab_never_submits_directory_navigation() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("one")],
            0,
            [0, 1, 2, 3],
        )));
        let settings = state
            .lock()
            .expect("app state mutex is not poisoned")
            .open_settings();
        let (sender, receiver) = mpsc::channel();

        assert!(!submit_navigation(
            &sender,
            &state,
            settings,
            PathBuf::from("ignored"),
            NavigationKind::Refresh,
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn same_path_normal_navigation_is_ignored_without_new_request() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("same")],
            0,
            [0, 1, 2, 3],
        )));
        let (sender, receiver) = mpsc::channel();

        assert!(!submit_navigation(
            &sender,
            &state,
            TabId(1),
            PathBuf::from("same"),
            NavigationKind::Normal,
        ));
        assert!(receiver.try_recv().is_err());
        let app = state.lock().expect("app state mutex is not poisoned");
        assert_eq!(app.active().latest_request, RequestId(0));
        assert!(app.active().back_history.is_empty());
    }

    #[test]
    fn group_menu_command_updates_the_window_directory_and_projects_headers() {
        let shared = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from(r"C:\group")],
            0,
            [0, 1, 2, 3],
        )));
        {
            let mut app = shared.lock().unwrap();
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap();
            let mut folder = focus_entry(1, r"C:\group\folder");
            folder.kind = crate::domain::EntryKind::Directory;
            tab.replace_entries(vec![
                folder,
                focus_entry(2, r"C:\group\model.obj"),
                focus_entry(3, r"C:\group\material.mtl"),
            ]);
        }
        let window_id = shared.lock().unwrap().active_window;
        let state = WindowSessions::new(shared.clone(), window_id);

        assert!(apply_group_command(
            &state,
            CMD_GROUP_BASE + i32::from(GroupField::Kind.storage_code()),
        ));

        {
            let app = shared.lock().unwrap();
            assert_eq!(
                app.directory_preference(Path::new(r"C:\group")).group_field,
                GroupField::Kind
            );
            let tab = app.active();
            let rows = projected_directory_rows(
                tab.visible_entries(),
                tab,
                Texts::new(Language::Chinese),
                &app,
            );
            let labels = rows
                .iter()
                .filter(|row| row.group_header)
                .map(|row| row.group_label.to_string())
                .collect::<Vec<_>>();
            assert_eq!(labels, ["文件夹", "MTL 文件", "OBJ 文件"]);
            assert_eq!(rows.iter().filter(|row| !row.group_header).count(), 3);
        }

        let ui = headless_file_view();
        refresh_ui(&ui, &state);
        let labels = ui
            .get_files()
            .iter()
            .filter(|row| row.group_header)
            .map(|row| row.group_label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(labels, ["文件夹", "MTL 文件", "OBJ 文件"]);
    }
    #[test]
    fn directory_view_modes_are_shared_by_raw_path_without_touching_requests() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("A"), PathBuf::from("B")],
            0,
            [0, 1, 2, 3],
        );
        let tab_a = app.active_window_state().tab_order[0];
        let request = app.tab(tab_a).unwrap().latest_request;
        app.update_directory_preference(PathBuf::from("A"), |preference| {
            preference.view_mode = ViewMode::MediumIcons
        });
        app.update_directory_preference(PathBuf::from("B"), |preference| {
            preference.view_mode = ViewMode::List
        });
        assert_eq!(
            app.directory_preference(Path::new("A")).view_mode,
            ViewMode::MediumIcons
        );
        assert_eq!(
            app.directory_preference(Path::new("B")).view_mode,
            ViewMode::List
        );
        assert_eq!(app.tab(tab_a).unwrap().latest_request, request);
    }

    #[test]
    fn grouped_search_page_matches_everything_files_first_setting() {
        let executable = PathBuf::from(r"C:\Program Files\Everything 1.5a\Everything64.exe");
        if !executable.is_file() {
            return;
        }
        let client = crate::platform::windows::everything::EverythingClient::new(
            crate::platform::windows::everything::PlatformEverythingConfig {
                executable_path: executable,
                instance_name: "1.5a".into(),
                allow_start: false,
            },
        )
        .unwrap();
        let GroupedSearchPage {
            items,
            total,
            file_total,
            response_offsets_valid,
        } = search_grouped_page(
            &client,
            (None, true),
            ".md".into(),
            crate::platform::windows::everything::EverythingSort::NameAscending,
            0,
            30,
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(response_offsets_valid);
        assert_eq!(items.len(), 30);
        assert!(total > items.len() as u32);
        assert!(file_total <= total);
        assert!(items.iter().all(|item| !item.is_directory));
    }
    #[test]
    fn search_sort_state_is_independent_and_defaults_to_everything_session_direction() {
        let mut tab = TabSession::new(TabId(1));
        assert_eq!(tab.sort_field, SortField::Name);
        assert_eq!(tab.sort_direction, SortDirection::Ascending);
        assert_eq!(tab.search_sort_field, SortField::Name);
        assert_eq!(tab.search_sort_direction, SortDirection::Descending);
        tab.set_search_sort(SortField::Name);
        assert_eq!(tab.search_sort_direction, SortDirection::Ascending);
        assert_eq!(tab.sort_direction, SortDirection::Ascending);
        tab.set_sort(SortField::Name);
        assert_eq!(tab.sort_direction, SortDirection::Descending);
        assert_eq!(tab.search_sort_direction, SortDirection::Ascending);
    }
    #[test]
    fn search_sort_arrow_maps_to_query2_service_sort() {
        use crate::platform::windows::everything::EverythingSort;
        assert_eq!(
            everything_sort(SortField::Name, SortDirection::Ascending),
            EverythingSort::NameAscending
        );
        assert_eq!(
            everything_sort(SortField::Name, SortDirection::Descending),
            EverythingSort::NameDescending
        );
        assert_eq!(
            everything_sort(SortField::Size, SortDirection::Ascending),
            EverythingSort::SizeAscending
        );
        assert_eq!(
            everything_sort(SortField::Modified, SortDirection::Descending),
            EverythingSort::ModifiedDescending
        );
    }
    #[test]
    fn root_path_has_no_parent_navigation_target() {
        assert!(Path::new(r"C:\").parent().is_none());
        assert!(is_drive_root(Path::new(r"C:\")));
        assert!(!is_drive_root(Path::new(r"C:\Users")));
    }

    #[test]
    fn icon_events_require_matching_tab_request_entry_and_path() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.latest_request = RequestId(7);
        tab.replace_entries(vec![FileEntry {
            id: EntryId(3),
            original_name: "file.txt".into(),
            display_name: "file.txt".into(),
            name_highlights: Vec::new(),
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: "same".into(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
            created: None,
        }]);
        let event = IconEvent {
            tab_id: TabId(1),
            request_id: RequestId(7),
            target: IconTarget::Entry(EntryId(3)),
            path: PathBuf::from("same/file.txt"),
            icon: platform::windows_shell_icons::ShellIconRgba {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0, 0],
            },
            actual_thumbnail: false,
            requested_px: 0,
        };
        assert!(icon_event_is_current(&app, &event));

        let location_event = IconEvent {
            tab_id: TabId(1),
            request_id: RequestId(7),
            target: IconTarget::Location,
            path: PathBuf::from("same"),
            icon: event.icon.clone(),
            actual_thumbnail: false,
            requested_px: 0,
        };
        assert!(icon_event_is_current(&app, &location_event));

        let mut stale_request = event;
        stale_request.request_id = RequestId(6);
        assert!(!icon_event_is_current(&app, &stale_request));
        stale_request.request_id = RequestId(7);
        stale_request.path = PathBuf::from("same/other.txt");
        assert!(!icon_event_is_current(&app, &stale_request));
        app.active_window_state_mut().tabs.remove(&TabId(1));
        assert!(!icon_event_is_current(&app, &stale_request));
    }

    #[test]
    fn folder_size_errors_keep_distinct_states() {
        use platform::windows::everything::{EverythingError, EverythingFolderSize};

        assert_eq!(
            folder_size_state(Ok(EverythingFolderSize::Indexed(0))),
            FolderSizeState::Value(0)
        );
        assert_eq!(
            folder_size_state(Ok(EverythingFolderSize::NotIndexed)),
            FolderSizeState::NotIndexed
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::FolderSizeRejected(404))),
            FolderSizeState::NotFound
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Timeout)),
            FolderSizeState::TimedOut
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::FolderSizeDisconnected)),
            FolderSizeState::Disconnected
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Protocol("response code 404".into()))),
            FolderSizeState::NotFound
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Protocol("bad response".into()))),
            FolderSizeState::ProtocolError
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Windows(5))),
            FolderSizeState::ProtocolError
        );
    }

    #[test]
    fn stale_folder_size_event_cannot_update_reused_entry_id() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.latest_request = RequestId(8);
        tab.page_source = PageSource::Directory;
        tab.replace_entries(vec![FileEntry {
            id: EntryId(1),
            path: PathBuf::from(r"C:\current\same-id"),
            original_name: "same-id".into(),
            display_name: "same-id".into(),
            name_highlights: Vec::new(),
            kind: crate::domain::EntryKind::Directory,
            open_target: None,
            parent_display: r"C:\current".into(),
            size_bytes: None,
            folder_size: FolderSizeState::Querying,
            modified: None,
            created: None,
        }]);

        let queries = {
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap();
            tab.entries = Arc::new(vec![{
                let mut entry = tab.entries[0].clone();
                entry.folder_size = FolderSizeState::Unknown;
                entry
            }]);
            with_folder_scheduler(tab, |scheduler, entries| {
                scheduler.visible_queries(RequestId(8), entries, 0, 1)
            })
        };
        let current = queries[0].clone();
        assert!(start_folder_size_query(&mut app, TabId(1), &current));
        let mut stale_request = current.clone();
        stale_request.request_id = RequestId(7);
        assert_eq!(
            apply_folder_size_event(
                &mut app,
                TabId(1),
                &stale_request,
                FolderSizeState::Value(12),
            ),
            FolderSizeCommit::Ignored
        );
        let mut stale_path = current.clone();
        stale_path.key.path = PathBuf::from(r"C:\old\same-id");
        assert_eq!(
            apply_folder_size_event(&mut app, TabId(1), &stale_path, FolderSizeState::Value(12)),
            FolderSizeCommit::Ignored
        );
        assert_eq!(
            app.active_window_state().tabs[&TabId(1)].entries[0].folder_size,
            FolderSizeState::Querying
        );
        assert_eq!(
            apply_folder_size_event(&mut app, TabId(1), &current, FolderSizeState::Value(0)),
            FolderSizeCommit::Visible(EntryId(1))
        );
        assert_eq!(
            app.active_window_state().tabs[&TabId(1)].entries[0].folder_size,
            FolderSizeState::Value(0)
        );
    }

    fn focus_entry(id: u32, path: &str) -> FileEntry {
        FileEntry {
            id: EntryId(id),
            original_name: Path::new(path).file_name().unwrap().to_os_string(),
            display_name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            name_highlights: Vec::new(),
            path: PathBuf::from(path),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: String::new(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
            created: None,
        }
    }

    #[test]
    fn pressing_an_unselected_row_for_drag_does_not_mutate_selection() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\test")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.replace_entries(vec![
            focus_entry(1, r"C:\test\one.txt"),
            focus_entry(2, r"C:\test\two.txt"),
            focus_entry(3, r"C:\test\three.txt"),
        ]);
        tab.select_entry(EntryId(1), false, false);

        assert_eq!(
            drag_paths_for_pressed_entry(&app, EntryId(2)),
            vec![PathBuf::from(r"C:\test\two.txt")]
        );
        let tab = &app.active_window_state().tabs[&TabId(1)];
        assert_eq!(tab.selected, vec![EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
        assert_eq!(tab.selection_anchor, Some(EntryId(1)));
    }

    #[test]
    fn pressing_a_selected_row_for_drag_uses_the_existing_multi_selection() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\test")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.replace_entries(vec![
            focus_entry(1, r"C:\test\one.txt"),
            focus_entry(2, r"C:\test\two.txt"),
        ]);
        tab.select_entry(EntryId(1), false, false);
        tab.select_entry(EntryId(2), true, false);

        assert_eq!(
            drag_paths_for_pressed_entry(&app, EntryId(2)),
            vec![
                PathBuf::from(r"C:\test\one.txt"),
                PathBuf::from(r"C:\test\two.txt"),
            ]
        );
    }
    #[test]
    fn completed_focus_selects_all_targets_in_every_matching_tab() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from(r"C:\target"), PathBuf::from(r"C:\target")],
            0,
            [0, 1, 2, 3],
        );
        let targets = vec![
            PathBuf::from(r"C:\target\first.txt"),
            PathBuf::from(r"C:\target\second.txt"),
        ];
        queue_completed_focus(&mut app, &targets);
        for tab_id in [TabId(1), TabId(2)] {
            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            let (_, cancel) =
                tab.begin_navigation(PathBuf::from(r"C:\target"), NavigationKind::Refresh);
            assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
            let request_id = tab.latest_request;
            tab.pending_entries = vec![
                focus_entry(1, r"C:\target\first.txt"),
                focus_entry(2, r"C:\target\second.txt"),
            ];
            app.focus_after_refresh.get_mut(&tab_id).unwrap().request_id = Some(request_id);
        }
        let state = Arc::new(Mutex::new(app));
        for tab_id in [TabId(1), TabId(2)] {
            apply_event(
                &state,
                DirectoryEvent::Finished {
                    tab_id,
                    request_id: RequestId(1),
                    path: PathBuf::from(r"C:\target"),
                    skipped: 0,
                },
            );
        }
        let app = state.lock().unwrap();
        for tab_id in [TabId(1), TabId(2)] {
            let tab = &app.active_window_state().tabs[&tab_id];
            assert_eq!(tab.selected, vec![EntryId(1), EntryId(2)]);
            assert_eq!(tab.focused, Some(EntryId(2)));
        }
    }

    #[test]
    fn stale_refresh_does_not_consume_completed_focus() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\target")], 0, [0, 1, 2, 3]);
        queue_completed_focus(&mut app, &[PathBuf::from(r"C:\target\item.txt")]);
        app.focus_after_refresh
            .get_mut(&TabId(1))
            .unwrap()
            .request_id = Some(RequestId(8));
        let state = Arc::new(Mutex::new(app));
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: TabId(1),
                request_id: RequestId(7),
                path: PathBuf::from(r"C:\target"),
                skipped: 0,
            },
        );
        assert!(
            state
                .lock()
                .unwrap()
                .focus_after_refresh
                .contains_key(&TabId(1))
        );
    }

    #[test]
    fn final_target_uses_keep_both_path_and_excludes_existing_merge_root() {
        let item = OperationItem::pending(
            Some(PathBuf::from(r"C:\source\name.txt")),
            Some(PathBuf::from(r"C:\target\name.txt")),
        );
        let kept = PathBuf::from(r"C:\target\name (2).txt");
        assert_eq!(
            completed_target_for_item(&item, std::slice::from_ref(&kept), false),
            Some(kept)
        );
        assert_eq!(
            completed_target_for_item(
                &OperationItem::pending(
                    Some(PathBuf::from(r"C:\source\folder")),
                    Some(PathBuf::from(r"C:\target\folder")),
                ),
                &[PathBuf::from(r"C:\target\folder")],
                true,
            ),
            None
        );
    }

    #[test]
    fn rename_validation_keeps_invalid_names_out_of_operation_queue() {
        assert!(crate::fs::file_operations::validate_name(std::ffi::OsStr::new("")).is_err());
        assert!(
            crate::fs::file_operations::validate_name(std::ffi::OsStr::new("bad?.txt")).is_err()
        );
        assert!(
            crate::fs::file_operations::validate_name(std::ffi::OsStr::new("valid.txt")).is_ok()
        );
        assert!(!keyboard_shortcuts_suppressed(false, false));
        assert!(keyboard_shortcuts_suppressed(true, false));
    }
    #[test]
    fn permission_page_has_actionable_copy_in_both_languages() {
        for language in [Language::Chinese, Language::English] {
            let (title, description) =
                error_page_text(LoadState::PermissionDenied, Texts::new(language));
            assert!(!title.is_empty());
            assert!(!description.is_empty());
            assert!(description.contains("Windows"));
        }
    }
    #[test]
    fn everything_search_errors_explain_installation_and_configuration() {
        let (title, description) =
            search_error_page_text(SearchState::NotConfigured, Texts::new(Language::Chinese));
        assert!(title.contains("安装"));
        assert!(description.contains("Everything 1.5 x64"));
        assert!(description.contains("设置"));

        let (title, description) =
            search_error_page_text(SearchState::Disconnected, Texts::new(Language::English));
        assert!(title.contains("Everything"));
        assert!(description.contains("running"));
        assert!(description.contains("settings"));

        let (title, description) = search_error_page_text(
            SearchState::UnsupportedArchitecture,
            Texts::new(Language::Chinese),
        );
        assert!(title.contains("x64"));
        assert!(description.contains("重新配置"));
    }
    #[test]
    fn late_everything_discovery_does_not_overwrite_newer_configuration() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        app.everything_generation = 2;
        app.everything_config.executable_path = Some(PathBuf::from("new.exe"));
        let discovered = crate::domain::EverythingConfig {
            executable_path: Some(PathBuf::from("old.exe")),
            instance_name: "old".into(),
            verified_version: None,
            allow_launch: true,
        };
        let status = platform::windows::everything::EverythingStatus {
            version: platform::windows::everything::EverythingVersion {
                major: 1,
                minor: 5,
                revision: 0,
                build: 1,
            },
            instance_name: "old".into(),
            database_loaded: true,
            folder_size_indexed: true,
        };

        assert!(!apply_everything_discovery(
            &mut app, 1, discovered, &status
        ));
        assert_eq!(
            app.everything_config.executable_path,
            Some(PathBuf::from("new.exe"))
        );
    }
    #[test]
    fn context_menu_closes_on_window_deactivation() {
        assert!(should_close_context_menu(
            &winit::event::WindowEvent::Focused(false)
        ));
        assert!(should_close_context_menu(
            &winit::event::WindowEvent::Occluded(true)
        ));
        assert!(!should_close_context_menu(
            &winit::event::WindowEvent::Focused(true)
        ));
    }

    #[test]
    fn rename_editor_owns_keyboard_input_before_window_shortcuts() {
        assert!(keyboard_shortcuts_suppressed(true, false));
        assert!(!keyboard_shortcuts_suppressed(false, false));
    }

    #[test]
    fn details_horizontal_hit_boundary_respects_margins_and_scroll() {
        let geometry = |list_left, viewport_x, viewport_width, columns_width| FileHitGeometry {
            list_left,
            list_top: 0.0,
            viewport_x,
            viewport_y: 0.0,
            viewport_width,
            columns_width,
        };
        assert!(!geometry(0.0, 0.0, 600.0, 400.0).details_contains(15.9));
        assert!(geometry(0.0, 0.0, 600.0, 400.0).details_contains(16.0));
        assert!(geometry(0.0, 0.0, 600.0, 400.0).details_contains(415.9));
        assert!(!geometry(0.0, 0.0, 600.0, 400.0).details_contains(416.0));

        assert!(!geometry(20.0, 0.0, 600.0, 400.0).details_contains(35.9));
        assert!(geometry(20.0, 0.0, 600.0, 400.0).details_contains(36.0));
        assert!(geometry(0.0, 0.0, 340.0, 400.0).details_contains(339.9));
        assert!(!geometry(0.0, 0.0, 340.0, 400.0).details_contains(340.0));

        assert!(!geometry(0.0, -120.0, 340.0, 400.0).details_contains(15.9));
        assert!(geometry(0.0, -120.0, 340.0, 400.0).details_contains(16.0));
        assert!(geometry(0.0, -120.0, 340.0, 400.0).details_contains(295.9));
        assert!(!geometry(0.0, -120.0, 340.0, 400.0).details_contains(296.0));
    }
    fn test_hit_geometry(list_top: f32, viewport_y: f32) -> FileHitGeometry {
        FileHitGeometry {
            list_left: 0.0,
            list_top,
            viewport_x: 0.0,
            viewport_y,
            viewport_width: 600.0,
            columns_width: 400.0,
        }
    }
    #[test]
    fn repeated_context_menu_hit_test_switches_between_entry_and_background() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("C:/test")],
            0,
            [0, 1, 2, 3],
        )));
        {
            let mut app = state.lock().unwrap();
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap();
            tab.replace_entries(vec![FileEntry {
                id: EntryId(1),
                path: PathBuf::from("C:/test/item.txt"),
                original_name: std::ffi::OsString::from("item.txt"),
                display_name: "item.txt".into(),
                name_highlights: Vec::new(),
                kind: crate::domain::EntryKind::File,
                open_target: None,
                parent_display: "C:/test".into(),
                size_bytes: Some(1),
                folder_size: crate::domain::FolderSizeState::Unknown,
                modified: None,
                created: None,
            }]);
        }
        assert_eq!(
            context_target_at(&state, 16.0, 170.0, test_hit_geometry(166.0, 0.0), 0.0, 1),
            (Some(EntryId(1)), false)
        );
        assert_eq!(
            context_target_at(&state, 416.0, 170.0, test_hit_geometry(166.0, 0.0), 0.0, 1,),
            (None, true)
        );
        assert_eq!(
            context_target_at(&state, 16.0, 250.0, test_hit_geometry(166.0, 0.0), 0.0, 1,),
            (None, true)
        );
    }

    #[test]
    fn non_details_context_menu_gutters_are_background() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("C:/test")],
            0,
            [0, 1, 2, 3],
        )));
        {
            let mut app = state.lock().unwrap();
            app.update_directory_preference(PathBuf::from("C:/test"), |preference| {
                preference.view_mode = ViewMode::List
            });
            app.active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap()
                .replace_entries(vec![focus_entry(1, r"C:\test\item.txt")]);
        }

        assert_eq!(
            context_target_at(&state, 15.9, 4.0, test_hit_geometry(0.0, 0.0), 0.0, 1),
            (None, true)
        );
        assert_eq!(
            context_target_at(&state, 16.0, 4.0, test_hit_geometry(0.0, 0.0), 0.0, 1),
            (Some(EntryId(1)), false)
        );
        assert_eq!(
            context_target_at(&state, 584.0, 4.0, test_hit_geometry(0.0, 0.0), 0.0, 1),
            (None, true)
        );
    }

    #[test]
    fn context_menu_hit_test_accounts_for_negative_viewport_offset() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("C:/test")],
            0,
            [0, 1, 2, 3],
        )));
        {
            let mut app = state.lock().unwrap();
            app.active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap()
                .replace_entries(
                    (1..=4)
                        .map(|id| FileEntry {
                            id: EntryId(id),
                            original_name: format!("{id}.txt").into(),
                            display_name: format!("{id}.txt"),
                            name_highlights: Vec::new(),
                            path: PathBuf::from(format!(r"C:\test\{id}.txt")),
                            kind: crate::domain::EntryKind::File,
                            open_target: None,
                            parent_display: "C:/test".into(),
                            size_bytes: Some(1),
                            folder_size: crate::domain::FolderSizeState::Unknown,
                            modified: None,
                            created: None,
                        })
                        .collect(),
                );
        }
        assert_eq!(
            context_target_at(&state, 16.0, 170.0, test_hit_geometry(166.0, -80.0), 0.0, 1),
            (Some(EntryId(3)), false)
        );
    }

    #[test]
    fn watched_roots_deduplicate_tabs_showing_the_same_directory() {
        let app = AppState::new_for_test(
            vec![PathBuf::from(r"C:\One"), PathBuf::from(r"C:\One")],
            0,
            [0, 1, 2, 3],
        );
        assert_eq!(
            watched_roots(&app),
            std::collections::HashSet::from([PathBuf::from(r"C:\One")])
        );
    }
    #[test]
    fn side_navigation_consumes_both_phases_and_routes_only_release() {
        use winit::event::{ElementState, MouseButton};

        assert!(is_side_navigation_mouse_button(MouseButton::Back));
        assert!(is_side_navigation_mouse_button(MouseButton::Forward));
        assert!(!is_side_navigation_mouse_button(MouseButton::Left));
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Pressed, MouseButton::Back),
            None
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Pressed, MouseButton::Forward),
            None
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Released, MouseButton::Back),
            Some(SideNavigation::Back)
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Released, MouseButton::Forward),
            Some(SideNavigation::Forward)
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Released, MouseButton::Left),
            None
        );
    }
    #[test]
    fn internal_drag_targets_only_directory_rows_with_viewport_offset() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        app.active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap()
            .replace_entries(vec![
                FileEntry {
                    id: EntryId(1),
                    original_name: "file.txt".into(),
                    display_name: "file.txt".into(),
                    name_highlights: Vec::new(),
                    path: PathBuf::from(r"C:\test\file.txt"),
                    kind: crate::domain::EntryKind::File,
                    open_target: None,
                    parent_display: String::new(),
                    size_bytes: Some(1),
                    folder_size: crate::domain::FolderSizeState::Unknown,
                    modified: None,
                    created: None,
                },
                FileEntry {
                    id: EntryId(2),
                    original_name: "folder".into(),
                    display_name: "folder".into(),
                    name_highlights: Vec::new(),
                    path: PathBuf::from(r"C:\test\folder"),
                    kind: crate::domain::EntryKind::Directory,
                    open_target: None,
                    parent_display: String::new(),
                    size_bytes: None,
                    folder_size: crate::domain::FolderSizeState::Unknown,
                    modified: None,
                    created: None,
                },
            ]);

        assert_eq!(
            internal_drag_target(&app, 16.0, 20.0, test_hit_geometry(0.0, 0.0), 0.0, 1),
            None
        );
        assert_eq!(
            internal_drag_target(&app, 416.0, 20.0, test_hit_geometry(0.0, -40.0), 0.0, 1,),
            None
        );
        assert_eq!(
            internal_drag_target(&app, 16.0, 20.0, test_hit_geometry(0.0, -40.0), 0.0, 1),
            Some((EntryId(2), PathBuf::from(r"C:\test\folder")))
        );
    }

    #[test]
    fn drop_preflight_preserves_paths_and_builds_operation_items() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-drop-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        let source_parent = temporary.join("来源");
        let target = temporary.join("目标");
        std::fs::create_dir_all(&source_parent).unwrap();
        std::fs::create_dir(&target).unwrap();
        let source = source_parent.join("文件😀.txt");
        std::fs::write(&source, b"drag").unwrap();

        let PreparedDrop::Operation(kind, items) =
            prepare_drop_operation(platform::windows::drag_drop::DropIntent {
                paths: vec![source.clone()],
                target: platform::windows::drag_drop::DropTarget::Directory(target.clone()),
                effect: platform::windows::drag_drop::DropEffect::Copy,
                right_button: false,
                screen_x: 0,
                screen_y: 0,
                allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                    | platform::windows::drag_drop::ALLOW_MOVE
                    | platform::windows::drag_drop::ALLOW_LINK,
            })
            .unwrap()
        else {
            panic!("copy drag must create a file operation");
        };

        assert_eq!(kind, FileOperationKind::Copy);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source.as_deref(), Some(source.as_path()));
        assert_eq!(
            items[0].destination.as_deref(),
            Some(target.join("文件😀.txt").as_path())
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn link_drop_preflight_uses_numbered_shortcut_destination() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-link-drop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        let target = temporary.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let source = temporary.join("Report.txt");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(target.join("Report.lnk"), b"existing").unwrap();

        let PreparedDrop::Shortcuts(shortcuts) =
            prepare_drop_operation(platform::windows::drag_drop::DropIntent {
                paths: vec![source.clone()],
                target: platform::windows::drag_drop::DropTarget::Directory(target.clone()),
                effect: platform::windows::drag_drop::DropEffect::Link,
                right_button: false,
                screen_x: 0,
                screen_y: 0,
                allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                    | platform::windows::drag_drop::ALLOW_MOVE
                    | platform::windows::drag_drop::ALLOW_LINK,
            })
            .unwrap()
        else {
            panic!("link drag must prepare shortcuts");
        };
        assert_eq!(shortcuts, vec![(source, target.join("Report (2).lnk"))]);
        std::fs::remove_dir_all(temporary).unwrap();
    }
    #[test]
    fn link_drop_reserves_distinct_names_for_same_named_sources() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-link-batch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        let left = temporary.join("left");
        let right = temporary.join("right");
        let target = temporary.join("target");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let first = left.join("Report.txt");
        let second = right.join("Report.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let PreparedDrop::Shortcuts(shortcuts) =
            prepare_drop_operation(platform::windows::drag_drop::DropIntent {
                paths: vec![first.clone(), second.clone()],
                target: platform::windows::drag_drop::DropTarget::Directory(target.clone()),
                effect: platform::windows::drag_drop::DropEffect::Link,
                right_button: false,
                screen_x: 0,
                screen_y: 0,
                allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                    | platform::windows::drag_drop::ALLOW_MOVE
                    | platform::windows::drag_drop::ALLOW_LINK,
            })
            .unwrap()
        else {
            panic!("link drag must prepare shortcuts");
        };
        assert_eq!(
            shortcuts,
            vec![
                (first, target.join("Report.lnk")),
                (second, target.join("Report (2).lnk")),
            ]
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }
    #[test]
    fn internal_drag_releases_pointer_grab_once_before_ole() {
        assert!(!should_release_internal_pointer_grab(3.9, false));
        assert!(should_release_internal_pointer_grab(4.0, false));
        assert!(!should_release_internal_pointer_grab(8.0, true));
    }

    #[test]
    fn rectangle_selection_starts_only_after_threshold() {
        assert!(!rectangle_selection_started(10.0, 10.0, 13.0, 13.0));
        assert!(rectangle_selection_started(10.0, 10.0, 13.0, 14.0));
    }

    #[test]
    fn cancelling_rectangle_selection_restores_the_starting_snapshot() {
        let mut tab = TabSession::new(TabId(1));
        tab.replace_entries(
            (1..=3)
                .map(|id| focus_entry(id, &format!(r"C:\test\{id}.txt")))
                .collect(),
        );
        let snapshot = HashSet::from([EntryId(1)]);
        tab.apply_rectangle_selection(
            &snapshot,
            &HashSet::from([EntryId(2)]),
            RectangleSelectionMode::Replace,
        );
        assert_eq!(tab.selected, vec![EntryId(2)]);

        restore_rectangle_selection_snapshot(
            &mut tab,
            &snapshot.iter().copied().collect::<Vec<_>>(),
            Some(EntryId(1)),
            Some(EntryId(1)),
        );

        assert_eq!(tab.selected, vec![EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
        assert_eq!(tab.selection_anchor, Some(EntryId(1)));
    }

    #[test]
    fn rectangle_selection_intersects_in_all_directions() {
        let item = SelectionRect {
            left: 20.0,
            top: 20.0,
            right: 40.0,
            bottom: 40.0,
        };
        for rect in [
            SelectionRect::from_points(0.0, 0.0, 25.0, 25.0),
            SelectionRect::from_points(25.0, 0.0, 0.0, 25.0),
            SelectionRect::from_points(0.0, 25.0, 25.0, 0.0),
            SelectionRect::from_points(25.0, 25.0, 0.0, 0.0),
        ] {
            assert!(rect.intersects(item));
        }
        assert!(!SelectionRect::from_points(0.0, 0.0, 19.0, 19.0).intersects(item));
    }

    #[test]
    fn rectangle_selection_hits_detail_list_and_grid_geometry() {
        let mut tab = TabSession::new(TabId(1));
        tab.replace_entries(
            (1..=6)
                .map(|id| focus_entry(id, &format!(r"C:\test\{id}.txt")))
                .collect(),
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(0.0, 39.0, 20.0, 41.0),
            ),
            HashSet::from([EntryId(1), EntryId(2)])
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::List,
                1,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(16.0, 35.0, 20.0, 67.0),
            ),
            HashSet::from([EntryId(2)])
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::MediumIcons,
                3,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(161.0, 145.0, 171.0, 155.0),
            ),
            HashSet::from([EntryId(5)])
        );
    }

    #[test]
    fn rectangle_selection_leaves_sixteen_pixel_background_gutters() {
        let mut tab = TabSession::new(TabId(1));
        tab.replace_entries(vec![focus_entry(1, r"C:\test\one.txt")]);

        assert!(
            rectangle_selection_hits(
                &tab,
                ViewMode::List,
                1,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(0.0, 0.0, 15.0, 33.0),
            )
            .is_empty()
        );
        assert!(
            rectangle_selection_hits(
                &tab,
                ViewMode::MediumIcons,
                3,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(0.0, 0.0, 15.0, 140.0),
            )
            .is_empty()
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::MediumIcons,
                3,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(16.0, 0.0, 30.0, 140.0),
            ),
            HashSet::from([EntryId(1)])
        );
    }
    #[test]
    fn rectangle_selection_details_hits_only_visible_column_region() {
        let mut tab = TabSession::new(TabId(1));
        tab.replace_entries(vec![focus_entry(1, r"C:\test\1.txt")]);

        assert!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(416.0, 0.0, 580.0, 40.0),
            )
            .is_empty()
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                340.0,
                -120.0,
                400.0,
                SelectionRect::from_points(295.0, 0.0, 310.0, 40.0),
            ),
            HashSet::from([EntryId(1)])
        );
        assert!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                340.0,
                -120.0,
                400.0,
                SelectionRect::from_points(296.0, 0.0, 324.0, 40.0),
            )
            .is_empty()
        );
    }
    #[test]
    fn rectangle_selection_search_geometry_uses_sparse_result_identity() {
        let mut tab = TabSession::new(TabId(1));
        tab.begin_search(SearchScope::Global, ".txt".into());
        tab.merge_search_page(
            256,
            vec![focus_entry(257, r"C:\test\257.txt")],
            1_000,
            1_000,
            256,
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(0.0, 256.0 * 40.0, 20.0, 257.0 * 40.0),
            ),
            HashSet::from([EntryId(257)])
        );
        assert!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                0.0,
                400.0,
                SelectionRect::from_points(0.0, 0.0, 20.0, 40.0),
            )
            .is_empty()
        );
    }

    fn test_grid_rows() -> ModelRc<GridRow> {
        ModelRc::new(VecModel::from(
            (0..12)
                .map(|_| GridRow {
                    group_header: false,
                    group_label: "".into(),
                    group_count: 0,
                    entries: ModelRc::new(VecModel::from(vec![empty_file_row(); 6])),
                })
                .collect::<Vec<_>>(),
        ))
    }

    fn headless_file_view() -> AppWindow {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().expect("headless app window should initialize");
        ui.window()
            .set_size(slint::LogicalSize::new(1_180.0, 760.0));
        ui.set_files(ModelRc::new(VecModel::from(vec![empty_file_row(); 67])));
        ui.set_grid_column_count(6);
        ui.set_grid_rows(test_grid_rows());
        ui.show().expect("headless app window should show");
        update_test_layout(&ui);
        ui
    }

    fn update_test_layout(ui: &AppWindow) {
        use i_slint_backend_testing::ElementRoot;

        ui.window().request_redraw();
        let _ = ui.root_element().query_descendants().find_all();
    }

    #[test]
    fn light_theme_uses_navigation_background_for_tab_address_sidebar_corner_and_settings() {
        let ui = headless_file_view();
        ui.set_dark_theme(false);
        let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
        assert_eq!(
            source
                .matches("VisualStyle.navigation-surface-background")
                .count(),
            6
        );
        assert!(source.contains("shape-color: VisualStyle.navigation-surface-background;"));
        assert!(source.contains("fill: VisualStyle.navigation-surface-background;"));
        update_test_layout(&ui);

        assert_eq!(
            VisualStyle::get(&ui).get_navigation_surface_background(),
            Color::from_argb_u8(0xff, 0xe9, 0xe9, 0xe9)
        );

        VisualStyle::get(&ui).set_dark_theme(true);
        update_test_layout(&ui);
        assert_eq!(
            VisualStyle::get(&ui).get_navigation_surface_background(),
            Color::from_argb_u8(0xff, 0x32, 0x34, 0x37)
        );
    }
    #[test]
    fn inactive_grid_model_updates_do_not_rewind_the_details_viewport() {
        let ui = headless_file_view();
        ui.set_view_mode(0);
        ui.set_file_viewport_y(-1_500.0);

        for _ in 0..5 {
            ui.set_files(ModelRc::new(VecModel::from(vec![empty_file_row(); 67])));
            ui.set_grid_rows(test_grid_rows());
            update_test_layout(&ui);
        }

        assert_eq!(ui.get_file_viewport_y(), -1_500.0);
    }

    #[test]
    fn switching_file_view_modes_resets_and_routes_only_the_active_viewport() {
        let ui = headless_file_view();
        ui.set_file_viewport_y(-1_000.0);
        update_test_layout(&ui);

        ui.set_file_viewport_y(0.0);
        ui.set_view_mode(2);
        update_test_layout(&ui);
        assert_eq!(ui.get_file_viewport_y(), 0.0);
        ui.set_file_viewport_y(-500.0);
        update_test_layout(&ui);

        ui.set_file_viewport_y(0.0);
        ui.set_view_mode(0);
        update_test_layout(&ui);
        assert_eq!(ui.get_file_viewport_y(), 0.0);
    }

    #[test]
    fn short_window_keeps_file_status_bar_above_the_bottom_edge() {
        use i_slint_backend_testing::ElementRoot;

        let ui = headless_file_view();
        ui.window()
            .set_size(slint::LogicalSize::new(1_180.0, 520.0));
        ui.set_quick_access_expanded(true);
        ui.set_drives_expanded(true);
        ui.set_network_locations_expanded(true);
        ui.set_network_expanded(true);
        ui.set_sidebar_items(ModelRc::new(VecModel::from(
            (0..48)
                .map(|index| SidebarRow {
                    index,
                    stable_id: index.to_string().into(),
                    label: format!("Sidebar {index}").into(),
                    icon_kind: 0,
                    group_kind: index % 4,
                    source_kind: 0,
                    is_drive: false,
                    icon: Image::default(),
                })
                .collect::<Vec<_>>(),
        )));
        update_test_layout(&ui);

        let status = ui
            .root_element()
            .query_descendants()
            .match_id("AppWindow::status-bar")
            .find_all()
            .into_iter()
            .next()
            .expect("file status bar exists");
        let position = status.absolute_position();
        let size = status.size();
        assert_eq!(size.height, 30.0);
        assert!(position.y + size.height <= 520.0);
    }

    #[test]
    fn overflowing_sidebar_uses_its_own_scroll_view() {
        use i_slint_backend_testing::ElementRoot;

        let ui = headless_file_view();
        ui.window()
            .set_size(slint::LogicalSize::new(1_180.0, 520.0));
        ui.set_quick_access_expanded(true);
        ui.set_drives_expanded(true);
        ui.set_network_locations_expanded(true);
        ui.set_network_expanded(true);
        ui.set_sidebar_items(ModelRc::new(VecModel::from(
            (0..48)
                .map(|index| SidebarRow {
                    index,
                    stable_id: index.to_string().into(),
                    label: format!("Sidebar {index}").into(),
                    icon_kind: 0,
                    group_kind: index % 4,
                    source_kind: 0,
                    is_drive: false,
                    icon: Image::default(),
                })
                .collect::<Vec<_>>(),
        )));
        update_test_layout(&ui);

        let scroll = ui
            .root_element()
            .query_descendants()
            .match_id("AppWindow::sidebar-scroll")
            .find_all()
            .into_iter()
            .next()
            .expect("sidebar scroll view exists");
        let content = ui
            .root_element()
            .query_descendants()
            .match_id("AppWindow::sidebar-content")
            .find_all()
            .into_iter()
            .next()
            .expect("sidebar content exists");
        assert!(content.size().height > scroll.size().height);
    }

    #[test]
    fn sidebar_wheel_scrolls_sidebar_without_moving_file_list() {
        use i_slint_backend_testing::ElementRoot;

        let ui = headless_file_view();
        ui.window()
            .set_size(slint::LogicalSize::new(1_180.0, 520.0));
        ui.set_quick_access_expanded(true);
        ui.set_drives_expanded(true);
        ui.set_network_locations_expanded(true);
        ui.set_network_expanded(true);
        ui.set_sidebar_items(ModelRc::new(VecModel::from(
            (0..48)
                .map(|index| SidebarRow {
                    index,
                    stable_id: index.to_string().into(),
                    label: format!("Sidebar {index}").into(),
                    icon_kind: 0,
                    group_kind: index % 4,
                    source_kind: 0,
                    is_drive: false,
                    icon: Image::default(),
                })
                .collect::<Vec<_>>(),
        )));
        ui.set_file_viewport_y(-200.0);
        update_test_layout(&ui);

        let scroll = ui
            .root_element()
            .query_descendants()
            .match_id("AppWindow::sidebar-scroll")
            .find_all()
            .into_iter()
            .next()
            .expect("sidebar scroll view exists");
        scroll.scroll(0.0, -120.0);
        update_test_layout(&ui);

        assert!(ui.get_sidebar_viewport_y() < 0.0);
        assert_eq!(ui.get_file_viewport_y(), -200.0);
    }

    #[test]
    fn native_wheel_routing_uses_the_complete_file_rectangle() {
        let bounds = (218.0, 154.0, 934.0, 516.0);
        assert!(!pointer_targets_file_area(
            120.0, 300.0, bounds.0, bounds.1, bounds.2, bounds.3
        ));
        assert!(!pointer_targets_file_area(
            500.0, 120.0, bounds.0, bounds.1, bounds.2, bounds.3
        ));
        assert!(!pointer_targets_file_area(
            500.0, 700.0, bounds.0, bounds.1, bounds.2, bounds.3
        ));
        assert!(pointer_targets_file_area(
            500.0, 300.0, bounds.0, bounds.1, bounds.2, bounds.3
        ));
    }

    #[test]
    fn scroll_delta_uses_active_row_height_and_logical_dpi_units() {
        for (mode, expected) in [
            (ViewMode::Details, -120.0),
            (ViewMode::List, -102.0),
            (ViewMode::MediumIcons, -444.0),
        ] {
            assert_eq!(
                logical_scroll_delta(&MouseScrollDelta::LineDelta(0.0, -1.0), mode, 1.0),
                expected
            );
        }

        let pixels = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, -150.0));
        for (scale, expected) in [(1.0, -150.0), (1.25, -120.0), (1.5, -100.0)] {
            assert_eq!(
                logical_scroll_delta(&pixels, ViewMode::Details, scale),
                expected
            );
        }
    }
    #[test]
    fn ctrl_wheel_steps_once_accumulates_pixels_and_clamps_views() {
        let mut accumulated = 0.0;
        assert_eq!(
            ctrl_wheel_step(
                &MouseScrollDelta::LineDelta(0.0, 1.0),
                1.0,
                &mut accumulated
            ),
            Some(true)
        );
        assert_eq!(
            ViewMode::ExtraLargeIcons.step_ctrl_wheel(true),
            ViewMode::ExtraLargeIcons
        );
        assert_eq!(ViewMode::Content.step_ctrl_wheel(false), ViewMode::Content);

        let pixels = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, 60.0));
        assert_eq!(ctrl_wheel_step(&pixels, 1.0, &mut accumulated), None);
        assert_eq!(ctrl_wheel_step(&pixels, 1.0, &mut accumulated), Some(true));
        let reverse = MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, -80.0));
        assert_eq!(
            ctrl_wheel_step(&reverse, 1.0, &mut accumulated),
            Some(false)
        );
    }

    #[test]
    fn ctrl_wheel_anchor_preserves_visible_item_without_resetting_to_top() {
        let viewport = anchored_viewport(
            -400.0,
            100.0,
            ViewMode::Details,
            ViewMode::MediumIcons,
            4,
            100,
            500.0,
        );
        assert!(viewport < 0.0);
        assert!(viewport >= -file_scroll_maximum(100, ViewMode::MediumIcons, 4, 500.0));
    }

    #[test]
    fn file_scroll_geometry_matches_every_view_mode_and_grid_remainder() {
        assert_eq!(
            file_scroll_maximum(67, ViewMode::Details, 6, 590.0),
            2_090.0
        );
        assert_eq!(file_scroll_maximum(67, ViewMode::List, 6, 590.0), 1_688.0);
        assert_eq!(
            file_scroll_maximum(67, ViewMode::MediumIcons, 6, 590.0),
            1_186.0
        );
        assert_eq!(
            file_scroll_maximum(66, ViewMode::MediumIcons, 6, 590.0),
            1_038.0
        );
        assert_eq!(file_scroll_maximum(2, ViewMode::MediumIcons, 6, 590.0), 0.0);
        assert_eq!(
            file_scroll_maximum(67, ViewMode::Details, 6, 590.5),
            2_089.5
        );
    }
    #[test]
    fn rectangle_selection_auto_scroll_uses_edge_distance_and_clamps_speed() {
        assert_eq!(rectangle_selection_scroll_delta(110.0, 100.0, 500.0), 10.0);
        assert_eq!(rectangle_selection_scroll_delta(490.0, 100.0, 500.0), -10.0);
        assert_eq!(rectangle_selection_scroll_delta(0.0, 100.0, 500.0), 40.0);
        assert_eq!(rectangle_selection_scroll_delta(600.0, 100.0, 500.0), -40.0);
        assert_eq!(rectangle_selection_scroll_delta(250.0, 100.0, 500.0), 0.0);
        assert_eq!(
            file_scroll_maximum(100, ViewMode::Details, 1, 400.0),
            3_600.0
        );
        assert_eq!(file_scroll_maximum(100, ViewMode::List, 1, 400.0), 3_000.0);
        assert_eq!(
            file_scroll_maximum(10, ViewMode::MediumIcons, 3, 400.0),
            192.0
        );
        assert_eq!(file_scroll_maximum(2, ViewMode::MediumIcons, 3, 400.0), 0.0);
    }

    #[test]
    fn native_tab_drag_starts_once_after_the_threshold_transition() {
        assert!(!should_start_native_tab_drag(false, false));
        assert!(should_start_native_tab_drag(true, false));
        assert!(!should_start_native_tab_drag(true, true));
    }
    fn right_drop_intent(
        paths: Vec<PathBuf>,
        target: PathBuf,
        allowed_effects: u32,
    ) -> platform::windows::drag_drop::DropIntent {
        platform::windows::drag_drop::DropIntent {
            paths,
            target: platform::windows::drag_drop::DropTarget::Directory(target),
            effect: platform::windows::drag_drop::DropEffect::Move,
            right_button: true,
            screen_x: 25,
            screen_y: 40,
            allowed_effects,
        }
    }

    #[test]
    fn right_drop_waits_for_a_menu_choice_and_cancel_does_not_prepare_work() {
        let intent = right_drop_intent(
            vec![PathBuf::from(r"C:\Source\item.txt")],
            PathBuf::from(r"C:\Target"),
            platform::windows::drag_drop::ALLOW_COPY,
        );
        assert!(drop_requires_choice(&intent));
        assert_eq!(selected_right_drop(intent, 0).unwrap(), None);
    }

    #[test]
    fn right_drop_choices_select_copy_move_and_link() {
        let allowed = platform::windows::drag_drop::ALLOW_COPY
            | platform::windows::drag_drop::ALLOW_MOVE
            | platform::windows::drag_drop::ALLOW_LINK;
        for (choice, effect) in [
            (1, platform::windows::drag_drop::DropEffect::Copy),
            (2, platform::windows::drag_drop::DropEffect::Move),
            (3, platform::windows::drag_drop::DropEffect::Link),
        ] {
            let selected = selected_right_drop(
                right_drop_intent(
                    vec![PathBuf::from(r"C:\Source\item.txt")],
                    PathBuf::from(r"C:\Target"),
                    allowed,
                ),
                choice,
            )
            .unwrap()
            .unwrap();
            assert_eq!(selected.effect, effect);
            assert!(!selected.right_button);
        }
    }

    #[test]
    fn right_drop_rejects_disallowed_effects_and_protected_paths() {
        let copy_only = right_drop_intent(
            vec![PathBuf::from(r"C:\Source\item.txt")],
            PathBuf::from(r"C:\Target"),
            platform::windows::drag_drop::ALLOW_COPY,
        );
        assert!(selected_right_drop(copy_only, 2).is_err());

        let descendant = right_drop_intent(
            vec![PathBuf::from(r"C:\Source")],
            PathBuf::from(r"C:\Source\Child"),
            platform::windows::drag_drop::ALLOW_COPY
                | platform::windows::drag_drop::ALLOW_MOVE
                | platform::windows::drag_drop::ALLOW_LINK,
        );
        assert!(selected_right_drop(descendant, 1).is_err());

        let same_location = right_drop_intent(
            vec![PathBuf::from(r"C:\Target\item.txt")],
            PathBuf::from(r"C:\Target"),
            platform::windows::drag_drop::ALLOW_COPY
                | platform::windows::drag_drop::ALLOW_MOVE
                | platform::windows::drag_drop::ALLOW_LINK,
        );
        assert_eq!(
            selected_right_drop(same_location.clone(), 1)
                .unwrap()
                .unwrap()
                .effect,
            platform::windows::drag_drop::DropEffect::Copy
        );
        assert!(selected_right_drop(same_location.clone(), 2).is_err());
        assert_eq!(
            selected_right_drop(same_location, 3)
                .unwrap()
                .unwrap()
                .effect,
            platform::windows::drag_drop::DropEffect::Link
        );
    }

    #[test]
    fn same_folder_drop_preflight_keeps_copy_and_link_executable() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-same-folder-drop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        std::fs::create_dir_all(&temporary).unwrap();
        let source = temporary.join("Report.txt");
        std::fs::write(&source, b"source").unwrap();
        let allowed =
            platform::windows::drag_drop::ALLOW_COPY | platform::windows::drag_drop::ALLOW_LINK;

        let copy = selected_right_drop(
            right_drop_intent(vec![source.clone()], temporary.clone(), allowed),
            1,
        )
        .unwrap()
        .unwrap();
        let PreparedDrop::Operation(FileOperationKind::Copy, copy_items) =
            prepare_drop_operation(copy).unwrap()
        else {
            panic!("same-folder copy must remain executable");
        };
        assert_eq!(copy_items[0].source.as_deref(), Some(source.as_path()));
        assert_eq!(copy_items[0].destination.as_deref(), Some(source.as_path()));

        let link = selected_right_drop(
            right_drop_intent(vec![source.clone()], temporary.clone(), allowed),
            3,
        )
        .unwrap()
        .unwrap();
        let PreparedDrop::Shortcuts(shortcuts) = prepare_drop_operation(link).unwrap() else {
            panic!("same-folder link must remain executable");
        };
        assert_eq!(shortcuts[0].0, source);
        assert_eq!(shortcuts[0].1, temporary.join("Report.lnk"));
        create_drop_shortcuts(shortcuts).unwrap();
        assert!(temporary.join("Report.lnk").is_file());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn external_cut_tracking_keeps_only_sources_still_on_disk() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-cut-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        std::fs::create_dir(&temporary).unwrap();
        let moved = temporary.join("moved.txt");
        let remaining = temporary.join("remaining.txt");
        std::fs::write(&moved, b"moved").unwrap();
        std::fs::write(&remaining, b"remaining").unwrap();
        std::fs::remove_file(&moved).unwrap();
        assert_eq!(existing_paths(&[moved, remaining.clone()]), vec![remaining]);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn error_kinds_have_distinct_page_states() {
        assert_eq!(classify_error(io::ErrorKind::NotFound), LoadState::NotFound);
        assert_eq!(
            classify_error(io::ErrorKind::PermissionDenied),
            LoadState::PermissionDenied
        );
        assert_eq!(
            classify_error(io::ErrorKind::TimedOut),
            LoadState::Disconnected
        );
    }
}
