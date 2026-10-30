use super::file_operation_worker::{
    completed_target_for_item, execute_cleanup_request, is_fast_remove_candidate,
};
use super::test_support::{FixtureCleanup, begin_drag_at, test_window_placement};
use super::*;
use crate::domain::file_operations::OperationResult;

fn rect(x: i32, y: i32, width: u32, height: u32) -> WindowRect {
    WindowRect {
        position: (x, y),
        size: (width, height),
    }
}

fn advance_test_navigation(tab: &mut TabSession, request: RequestId) {
    let location = tab.visible_location().cloned().expect("test location");
    while tab.latest_request().0 < request.0 {
        tab.begin_navigation(location.clone(), NavigationKind::Refresh);
    }
    assert_eq!(tab.latest_request(), request);
}

fn complete_test_navigation(
    tab: &mut TabSession,
    location: NavigationLocation,
    kind: NavigationKind,
) {
    let entries = tab.visible_entries().to_vec();
    let preference = DirectoryViewPreference {
        sort_field: tab.sort_field,
        sort_direction: tab.sort_direction,
        ..Default::default()
    };
    let (request, _) = tab.begin_navigation(location.clone(), kind);
    assert!(tab.accept_directory_batch(request, entries, preference));
    assert!(tab.finish_directory_navigation(request, location, preference, None));
}

#[test]
fn issue_90_centered_window_position_follows_the_anchor_monitor() {
    // 主窗口位于左侧带负坐标的显示器时，弹窗必须留在同一显示器内。
    assert_eq!(
        centered_window_position(
            rect(-1900, 100, 1200, 800),
            (460, 210),
            rect(-1920, 0, 1920, 1080)
        ),
        (-1530, 395)
    );
    assert_eq!(
        centered_window_position(
            rect(100, 100, 1000, 700),
            (520, 286),
            rect(0, 0, 2560, 1440)
        ),
        (340, 307)
    );
}

#[test]
fn issue_90_centered_window_position_clamps_into_the_monitor() {
    // 锚点窗口贴边或部分越界时，弹窗仍不得越出显示器。
    assert_eq!(
        centered_window_position(rect(0, 0, 400, 300), (460, 210), rect(0, 0, 1920, 1080)),
        (0, 45)
    );
    assert_eq!(
        centered_window_position(
            rect(1880, 1040, 400, 300),
            (460, 210),
            rect(0, 0, 1920, 1080)
        ),
        (1460, 870)
    );
}

#[test]
fn issue_90_centered_window_position_never_escapes_a_small_monitor() {
    // 弹窗比锚点所在显示器还大时退化到显示器原点，不产生可见区域外的负坐标。
    assert_eq!(
        centered_window_position(rect(0, 0, 400, 300), (520, 286), rect(0, 0, 400, 300)),
        (0, 7)
    );
    assert_eq!(
        centered_window_position(
            rect(-1920, 0, 300, 200),
            (520, 286),
            rect(-1920, 0, 400, 300)
        ),
        (-1920, 0)
    );
}

#[test]
fn issue_86_new_tab_policy_and_explicit_targets_are_separate() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\One")], 0, [0, 1, 2, 3]);
    app.new_tab_opens_home = true;
    let home = app.create_tab(NavigationLocation::Home);
    assert_eq!(
        app.tab(home).unwrap().current_location(),
        &Some(NavigationLocation::Home)
    );

    let explicit = app.create_tab(NavigationLocation::Directory(PathBuf::from(r"D:\Target")));
    assert_eq!(
        app.tab(explicit).unwrap().current_location(),
        &Some(NavigationLocation::Directory(PathBuf::from(r"D:\Target")))
    );

    app.new_tab_opens_home = false;
    app.tab_mut(explicit).unwrap().load_state = LoadState::Complete;
    let duplicate = app.duplicate_active_tab().unwrap();
    assert_eq!(
        app.tab(duplicate).unwrap().current_location(),
        &Some(NavigationLocation::Directory(PathBuf::from(r"D:\Target")))
    );

    let home_page = HomePageState {
        selected: Some("home:0:0".to_owned()),
        quick_access_expanded: false,
        ..HomePageState::expanded()
    };
    app.active_window_state_mut().active_tab = home;
    app.home_pages.insert(home, home_page.clone());
    let duplicated_home = app.duplicate_active_tab().unwrap();
    assert_eq!(
        app.home_pages.get(&duplicated_home).unwrap().selected,
        home_page.selected
    );
    assert!(!app.home_pages[&duplicated_home].quick_access_expanded);
}

#[test]
fn issue_86_home_sources_ignore_stale_results_and_do_not_overwrite_each_other() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\One")], 0, [0, 1, 2, 3]);
    app.quick_access_generation = 2;
    app.drive_list_generation = 4;
    let pinned = KnownLocation {
        kind: KnownLocationKind::Pinned,
        label: "Pinned".to_owned(),
        path: PathBuf::from(r"C:\Pinned"),
        drive_root: None,
    };
    let drive = KnownLocation {
        kind: KnownLocationKind::Drive,
        label: "C:".to_owned(),
        path: PathBuf::from(r"C:\"),
        drive_root: Some(PathBuf::from(r"C:\")),
    };
    let second_drive = KnownLocation {
        kind: KnownLocationKind::Drive,
        label: "Z:".to_owned(),
        path: PathBuf::from(r"Z:\"),
        drive_root: Some(PathBuf::from(r"Z:\")),
    };
    app.sidebar = vec![drive.clone()];

    assert!(!replace_quick_access_projection(
        &mut app,
        1,
        vec![pinned.clone()]
    ));
    assert_eq!(app.sidebar, vec![drive.clone()]);
    assert!(replace_quick_access_projection(
        &mut app,
        2,
        vec![pinned.clone()]
    ));
    assert_eq!(app.sidebar, vec![pinned.clone(), drive.clone()]);

    assert!(!replace_drive_projection(
        &mut app,
        3,
        vec![second_drive.clone()]
    ));
    assert_eq!(app.sidebar, vec![pinned.clone(), drive]);
    assert!(replace_drive_projection(
        &mut app,
        4,
        vec![second_drive.clone()]
    ));
    assert_eq!(app.sidebar, vec![pinned, second_drive]);

    app.home_pages.insert(
        TabId(1),
        HomePageState {
            selected: Some("home:0:0".to_owned()),
            quick_access_expanded: false,
            ..HomePageState::expanded()
        },
    );
    app.home_pages.insert(TabId(2), HomePageState::expanded());
    assert_ne!(
        app.home_pages[&TabId(1)].quick_access_expanded,
        app.home_pages[&TabId(2)].quick_access_expanded
    );
}

#[test]
fn issue_86_home_layout_switches_between_default_and_narrow_columns() {
    let ui = AppWindow::new().unwrap();
    ui.set_home_content_width(home_content_width_for_window(1693.0));
    assert_eq!(ui.get_home_column_count(), 5);
    ui.set_home_content_width(home_content_width_for_window(1180.0));
    assert_eq!(ui.get_home_column_count(), 3);
    ui.set_home_content_width(home_content_width_for_window(950.0));
    assert_eq!(ui.get_home_column_count(), 2);
    ui.set_home_content_width(home_content_width_for_window(820.0));
    assert_eq!(ui.get_home_column_count(), 2);
    ui.set_home_content_width(home_content_width_for_window(520.0));
    assert_eq!(ui.get_home_column_count(), 1);
    assert_eq!(ui.get_home_card_width(), 264.0);
    let source = include_str!("../../ui/app-window.slint");
    assert!(source.contains("wrap: root.item.source-kind == 2 ? word-wrap : no-wrap"));
    assert!(source.contains("visible: root.item.source-kind != 2"));
    assert!(!source.contains("height: home-scroll.viewport-height"));
}

#[test]
fn home_wheel_scrolls_overflow_without_moving_the_file_list() {
    let ui = headless_file_view();
    ui.window()
        .set_size(slint::LogicalSize::new(1_180.0, 520.0));
    ui.set_active_is_home(true);
    ui.set_home_content_width(home_content_width_for_window(1_180.0));
    ui.set_home_quick_access_expanded(true);
    ui.set_file_viewport_y(-200.0);
    ui.set_home_quick_access_items(ModelRc::new(VecModel::from(Vec::new())));
    update_test_layout(&ui);
    let content_y = || {
        named_element(&ui, "AppWindow::home-content")
            .expect("home content")
            .absolute_position()
            .y
    };
    let scroll_home = || {
        named_element(&ui, "AppWindow::home-scroll")
            .expect("home scroll")
            .scroll(0.0, -160.0);
        update_test_layout(&ui);
    };
    let fitted = content_y();
    scroll_home();
    assert_eq!(content_y(), fitted);

    let item = |index: i32| HomeItemRow {
        stable_id: index.to_string().into(),
        label: format!("Home {index}").into(),
        detail: "detail".into(),
        icon: Image::default(),
        progress: 0.0,
        state: -1,
        status: "".into(),
        selected: false,
        focused: false,
        source_kind: 0,
        menu_group: 0,
        menu_stable_id: "".into(),
    };
    let rows = (0..24)
        .map(item)
        .collect::<Vec<_>>()
        .chunks(2)
        .map(|entries| HomeCardRow {
            entries: ModelRc::new(VecModel::from(entries.to_vec())),
        })
        .collect::<Vec<_>>();
    ui.set_home_quick_access_items(ModelRc::new(VecModel::from(rows)));
    update_test_layout(&ui);

    let before = content_y();
    scroll_home();
    let after = content_y();
    assert!(
        after < before - 100.0,
        "home page should follow the wheel: {before} -> {after}"
    );
    assert_eq!(ui.get_file_viewport_y(), -200.0);
}

#[test]
fn issue_86_home_projection_preserves_raw_targets_and_network_location_empty_state() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\One")], 0, [0, 1, 2, 3]);
    app.sidebar = vec![KnownLocation {
        kind: KnownLocationKind::Pinned,
        label: "Display only".to_owned(),
        path: PathBuf::from(r"C:\Raw\Target"),
        drive_root: None,
    }];
    assert_eq!(
        home_item_target(&app, "home:0:0"),
        Some(PathBuf::from(r"C:\Raw\Target"))
    );
    assert!(sorted_network_locations(&app).is_empty());
    app.network_locations = vec![NetworkLocation {
        id: 86,
        source: NetworkLocationSource::AsterOwned,
        display_name: "Network location".to_owned(),
        sort_order: 0,
        target: NetworkTarget::WindowsPath(PathBuf::from(r"\\server\share")),
        shell_path: None,
    }];
    assert_eq!(
        home_item_target(&app, "home:2:86"),
        Some(PathBuf::from(r"\\server\share"))
    );
    assert_eq!(
        home_item_network_target(&app, "home:2:86"),
        Some(NetworkTarget::WindowsPath(PathBuf::from(r"\\server\share")))
    );
    app.network_locations.push(NetworkLocation {
        id: 157,
        source: NetworkLocationSource::AsterOwned,
        display_name: "OpenList".to_owned(),
        sort_order: 1,
        target: NetworkTarget::WebDav(crate::network::WebDavConnection {
            connection_id: 157,
            revision: 1,
            https: false,
            host: "192.168.50.45".to_owned(),
            port: 5244,
            root: "/dav/mount".to_owned(),
            username: "reader".to_owned(),
        }),
        shell_path: None,
    });
    assert_eq!(home_item_target(&app, "home:2:157"), None);
    match home_item_network_target(&app, "home:2:157") {
        Some(NetworkTarget::WebDav(connection)) => {
            assert_eq!(connection.host, "192.168.50.45");
            assert_eq!(connection.root, "/dav/mount");
        }
        other => panic!("home card lost the WebDAV target: {other:?}"),
    }
    assert_eq!(home_item_target(&app, "Display only"), None);
}

fn recycle_task_for_test(state: OperationState) -> (AppState, OperationId) {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::RecycleDelete,
        None,
        vec![
            OperationItem::pending(Some(PathBuf::from("C:/test/one")), None),
            OperationItem::pending(Some(PathBuf::from("C:/test/two")), None),
        ],
    );
    app.operations.start_next(OperationResource::Local).unwrap();
    if state == OperationState::Running {
        app.operations.mark_running(id).unwrap();
    }
    (app, id)
}

#[test]
fn external_startup_paths_replace_restored_session_and_activate_last_path() {
    let restored = vec![NavigationLocation::Directory(PathBuf::from(r"C:\Restored"))];
    let external = vec![
        ClassifiedExternalLaunch::Directory(PathBuf::from(r"C:\Folder With Spaces")),
        ClassifiedExternalLaunch::Directory(PathBuf::from(r"C:\中文")),
    ];

    let (locations, active, reveals) = startup_locations(restored, 0, external);

    assert_eq!(active, 1);
    assert_eq!(
        locations,
        vec![
            NavigationLocation::Directory(PathBuf::from(r"C:\Folder With Spaces")),
            NavigationLocation::Directory(PathBuf::from(r"C:\中文")),
        ]
    );
    assert_eq!(reveals, [None, None]);
}

#[test]
fn issue_118_startup_shell_select_overlays_the_matching_folder_tab() {
    let parent = PathBuf::from(r"D:\Depot");
    let target = parent.join("file.cpp");
    let tabs = vec![
        (
            TabId(1),
            NavigationLocation::Directory(PathBuf::from(r"C:\Other")),
        ),
        (TabId(2), NavigationLocation::Directory(parent.clone())),
    ];
    let mut reveals = vec![None, None];
    assert!(overlay_startup_shell_select(
        &tabs,
        &mut reveals,
        target.clone()
    ));
    assert_eq!(reveals, [None, Some(target)]);
}

#[test]
fn issue_118_external_startup_files_open_the_parent_and_keep_reveal_targets() {
    let restored = vec![NavigationLocation::Directory(PathBuf::from(r"C:\Restored"))];
    let file = PathBuf::from(r"D:\Downloads\file.zip");
    let parent = PathBuf::from(r"D:\Downloads");
    let folder = PathBuf::from(r"C:\JustAFolder");
    let (locations, active, reveals) = startup_locations(
        restored,
        0,
        vec![
            ClassifiedExternalLaunch::Reveal {
                parent: parent.clone(),
                target: file.clone(),
            },
            ClassifiedExternalLaunch::Directory(folder.clone()),
        ],
    );

    assert_eq!(active, 1);
    assert_eq!(
        locations,
        vec![
            NavigationLocation::Directory(parent),
            NavigationLocation::Directory(folder),
        ]
    );
    assert_eq!(reveals, [Some(file), None]);
}

#[test]
fn issue_118_external_file_navigation_registers_reveal_before_worker_delivery() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let app = AppState::new_for_test(vec![parent.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    let window_id = app.active_window;
    let state = Arc::new(Mutex::new(app));
    let (local_sender, local_receiver) = mpsc::channel();
    let (network_sender, _network_receiver) = mpsc::sync_channel(1);

    assert!(submit_path_navigation_with_reveal(
        &local_sender,
        &network_sender,
        &state,
        tab_id,
        parent.clone(),
        NavigationKind::Refresh,
        Some(target.clone()),
    ));
    let request = local_receiver.try_recv().expect("directory request queued");
    let app = state.lock().unwrap();
    let pending = app.focus_after_refresh.get(&tab_id).unwrap();
    assert_eq!(pending.request_id, Some(request.request_id));
    assert_eq!(pending.directory, parent);
    assert_eq!(pending.paths, vec![target]);
    assert_eq!(pending.action, PendingFocusAction::Reveal { window_id });
}

#[test]
fn issue_118_late_shell_select_reveals_the_already_open_folder_tab() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let mut app = AppState::new_for_test(vec![parent.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    {
        let tab = app.tab_mut(tab_id).unwrap();
        tab.replace_entries(vec![focus_entry(1, target.to_str().unwrap())]);
        tab.load_state = LoadState::Complete;
    }
    let state = Arc::new(Mutex::new(app));

    let context = external_reveal_context(&state, tab_id).unwrap();
    apply_late_shell_select_reveal(&state, &context, target);

    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.selected, vec![EntryId(1)]);
    assert_eq!(tab.focused, Some(EntryId(1)));
    assert!(!app.focus_after_refresh.contains_key(&tab_id));
}

#[test]
fn issue_118_late_shell_select_queues_reveal_while_the_folder_is_loading() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let mut app = AppState::new_for_test(vec![parent.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    let window_id = app.active_window;
    let request_id = {
        let tab = app.tab_mut(tab_id).unwrap();
        let (request_id, _) =
            tab.begin_directory_navigation(parent.clone(), NavigationKind::Refresh);
        request_id
    };
    let state = Arc::new(Mutex::new(app));

    let context = external_reveal_context(&state, tab_id).unwrap();
    apply_late_shell_select_reveal(&state, &context, target.clone());

    let app = state.lock().unwrap();
    let pending = app.focus_after_refresh.get(&tab_id).unwrap();
    assert_eq!(pending.request_id, Some(request_id));
    assert_eq!(pending.directory, parent);
    assert_eq!(pending.paths, vec![target]);
    assert_eq!(pending.action, PendingFocusAction::Reveal { window_id });
}

#[test]
fn issue_122_late_shell_select_stays_with_the_original_tab() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let mut app = AppState::new_for_test(vec![parent.clone(), parent.clone()], 0, [0, 1, 2, 3]);
    let original = app.active_window_state().tab_order[0];
    let other = app.active_window_state().tab_order[1];
    for tab_id in [original, other] {
        let tab = app.tab_mut(tab_id).unwrap();
        tab.replace_entries(vec![focus_entry(1, target.to_str().unwrap())]);
        tab.load_state = LoadState::Complete;
    }
    app.active_window_state_mut().active_tab = other;
    let state = Arc::new(Mutex::new(app));
    let context = external_reveal_context(&state, original).unwrap();

    apply_late_shell_select_reveal(&state, &context, target);

    let app = state.lock().unwrap();
    assert_eq!(app.tab(original).unwrap().selected, vec![EntryId(1)]);
    assert!(app.tab(other).unwrap().selected.is_empty());
    assert_eq!(app.active_window_state().active_tab, other);
    assert_eq!(app.active_window_state().tab_order.len(), 2);
}

#[test]
fn issue_122_late_shell_select_does_not_reopen_a_closed_tab() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let app = AppState::new_for_test(vec![parent.clone(), parent], 0, [0, 1, 2, 3]);
    let original = app.active_window_state().active_tab;
    let state = Arc::new(Mutex::new(app));
    let context = external_reveal_context(&state, original).unwrap();
    state.lock().unwrap().close_tab(original).unwrap();

    apply_late_shell_select_reveal(&state, &context, target);

    let app = state.lock().unwrap();
    assert!(app.tab(original).is_none());
    assert_eq!(app.active_window_state().tab_order.len(), 1);
    assert!(app.focus_after_refresh.is_empty());
}

#[test]
fn issue_122_late_shell_select_rejects_an_old_navigation_to_the_same_folder() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let app = AppState::new_for_test(vec![parent.clone()], 0, [0, 1, 2, 3]);
    let original = app.active_window_state().active_tab;
    let state = Arc::new(Mutex::new(app));
    let context = external_reveal_context(&state, original).unwrap();
    state
        .lock()
        .unwrap()
        .tab_mut(original)
        .unwrap()
        .begin_directory_navigation(parent, NavigationKind::Refresh);

    apply_late_shell_select_reveal(&state, &context, target);

    let app = state.lock().unwrap();
    assert!(app.tab(original).unwrap().selected.is_empty());
    assert!(app.focus_after_refresh.is_empty());
}

#[test]
fn issue_118_late_shell_select_scrolls_the_focused_file_into_view() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"));
    let function = source
        .split("fn apply_late_shell_select_reveal(")
        .nth(1)
        .expect("late select function")
        .split("\nfn open_classified_external_launches(")
        .next()
        .expect("late select function boundary");
    assert!(
        function.contains("reveal_entry("),
        "P4V SelectItem after the folder is visible must reuse reveal_entry to scroll"
    );
}

#[test]
fn issue_118_classified_external_file_opens_a_new_tab_on_the_parent() {
    let parent = PathBuf::from(r"D:\Downloads");
    let target = parent.join("file.zip");
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\Existing")], 0, [0, 1, 2, 3]);
    let existing = app.active_window_state().active_tab;
    let state = Arc::new(Mutex::new(app));
    let (local_sender, local_receiver) = mpsc::channel();
    let (network_sender, _network_receiver) = mpsc::sync_channel(1);

    open_classified_external_launches(
        &state,
        &local_sender,
        &network_sender,
        vec![ClassifiedExternalLaunch::Reveal {
            parent: parent.clone(),
            target: target.clone(),
        }],
        None,
    );

    let request = local_receiver.try_recv().expect("parent directory queued");
    let app = state.lock().unwrap();
    let tab_id = app.active_window_state().active_tab;
    assert_ne!(tab_id, existing);
    assert_eq!(request.path, parent);
    let pending = app.focus_after_refresh.get(&tab_id).unwrap();
    assert_eq!(pending.paths, vec![target]);
    assert_eq!(pending.directory, parent);
}

#[test]
fn issue_118_external_reveal_selects_the_complete_original_path() {
    let parent = PathBuf::from(r"D:\second");
    let target = parent.join("same.txt");
    let mut app = AppState::new_for_test(vec![parent.clone()], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let tab_id = app.active_window_state().active_tab;
    let request_id = {
        let tab = app.tab_mut(tab_id).unwrap();
        tab.begin_directory_navigation(parent.clone(), NavigationKind::Refresh)
            .0
    };
    app.focus_after_refresh.insert(
        tab_id,
        PendingFocus {
            directory: parent.clone(),
            request_id: Some(request_id),
            paths: vec![target.clone()],
            action: PendingFocusAction::Reveal { window_id },
        },
    );
    let state = Arc::new(Mutex::new(app));
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id,
            request_id,
            entries: vec![
                focus_entry(1, r"D:\first\same.txt"),
                focus_entry(2, r"D:\second\same.txt"),
            ],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path: parent,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );

    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.selected, vec![EntryId(2)]);
    assert_eq!(tab.focused, Some(EntryId(2)));
    assert_eq!(tab.selection_anchor, Some(EntryId(2)));
    assert!(!app.focus_after_refresh.contains_key(&tab_id));
}

#[test]
fn external_paths_open_as_new_tabs_in_the_active_window() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\Existing")], 0, [0, 1, 2, 3]);
    let existing = app.active_window_state().active_tab;
    let first = app.create_tab(NavigationLocation::Directory(PathBuf::from(r"C:\One")));
    let second = app.create_tab(NavigationLocation::Directory(PathBuf::from(r"C:\Two")));

    assert_ne!(existing, first);
    assert_ne!(first, second);
    assert_eq!(app.active_window_state().active_tab, second);
    assert_eq!(
        app.active_window_state().tab_order,
        [existing, first, second]
    );
}

#[test]
fn issue_81_recycle_rows_prioritize_discovery_progress() {
    let (mut preparing, preparing_id) = recycle_task_for_test(OperationState::Preflight);
    let task = preparing.operations.task_mut(preparing_id).unwrap();
    task.progress.prepared_items = 1;
    task.progress.current_item = Some(PathBuf::from("C:/test/one"));
    let row = operation_rows(&preparing).remove(0);
    assert_eq!(row.status.as_str(), "正在准备");
    assert_eq!(row.file_progress.as_str(), "正在准备 · 1 / 2 个所选项目");
    assert!(!row.progress_known);
    assert!(row.percent.is_empty());
    assert!(!row.can_pause);

    let (mut executing, executing_id) = recycle_task_for_test(OperationState::Running);
    let task = executing.operations.task_mut(executing_id).unwrap();
    task.progress.discovered_files = 22_998;
    task.progress.discovered_bytes = 8 * 1_073_741_824;
    let row = operation_rows(&executing).remove(0);
    assert_eq!(row.file_progress.as_str(), "已发现 22,998 项（8.0 GB）");
    assert_eq!(row.speed.as_str(), "正在发现项目");
    assert!(row.transferred.is_empty());
    assert!(!row.progress_known);
    assert!(row.percent.is_empty());

    let task = executing.operations.task_mut(executing_id).unwrap();
    task.progress.scanning_complete = true;
    let row = operation_rows(&executing).remove(0);
    assert_eq!(
        row.file_progress.as_str(),
        "已发现 22,998 项（8.0 GB） · 正在移到回收站"
    );
    assert_eq!(row.speed.as_str(), "已完成内容统计");
    assert!(row.percent.is_empty());
    assert!(!row.progress_known);
}

fn test_library(
    identity: &str,
    display_name: &str,
    sources: Vec<Option<PathBuf>>,
    default_save_path: Option<PathBuf>,
) -> platform::windows::libraries::WindowsLibrary {
    platform::windows::libraries::WindowsLibrary {
        id: platform::windows::libraries::LibraryId::new(identity.into()),
        definition_path: Some(PathBuf::from(format!(
            r"C:\Libraries\{display_name}.library-ms"
        ))),
        display_name: display_name.into(),
        pinned: true,
        sort_order: 0,
        sources: sources
            .into_iter()
            .enumerate()
            .map(
                |(index, path)| platform::windows::libraries::LibrarySource {
                    shell_identity: format!("shell:source:{index}").into(),
                    display_name: format!("Source {index}").into(),
                    path,
                },
            )
            .collect(),
        default_save_path,
    }
}

#[test]
fn issue_104_library_navigation_keeps_unavailable_source_slots_and_cancels_previous_request() {
    let first = PathBuf::from(r"C:\First");
    let second = PathBuf::from(r"D:\Second");
    let definition = test_library(
        "shell:library:sparse",
        "Sparse",
        vec![None, Some(first.clone()), None, Some(second.clone())],
        None,
    );
    let library = LibraryLocationId::new("shell:library:sparse".into(), "Sparse".into());
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
    app.libraries.push(definition);
    let state = Arc::new(Mutex::new(app));
    let (sender, receiver) = mpsc::channel();
    assert!(submit_library_navigation(
        &sender,
        &state,
        TabId(1),
        library.clone(),
        NavigationKind::Normal
    ));
    let first_request = receiver.recv().unwrap();
    assert_eq!(
        first_request.library_sources,
        Some(vec![(1, first), (3, second)])
    );
    assert_eq!(first_request.unavailable_library_sources, 2);
    assert!(submit_library_navigation(
        &sender,
        &state,
        TabId(1),
        library,
        NavigationKind::Refresh
    ));
    let refreshed = receiver.recv().unwrap();
    assert!(first_request.cancelled());
    assert!(!refreshed.cancelled());
    assert_eq!(refreshed.unavailable_library_sources, 2);
}

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

fn library_projection_entry(id: u32, source_index: usize, path: &str) -> FileEntry {
    let path = PathBuf::from(path);
    let original_name = path.file_name().unwrap().to_owned();
    FileEntry {
        id: EntryId(id),
        display_name: original_name.to_string_lossy().into_owned(),
        name_highlights: Vec::new(),
        original_name,
        path: path.clone(),
        kind: crate::domain::EntryKind::File,
        open_target: None,
        library_source_index: Some(source_index),
        parent_display: path.parent().map(display_path).unwrap_or_default(),
        size_bytes: Some(1),
        folder_size: FolderSizeState::Unknown,
        modified: None,
        created: None,
        webdav: None,
    }
}

#[test]
fn library_projection_preserves_shell_source_order_and_real_paths() {
    let mut library = test_library(
        "shell:library:grouped",
        "Grouped",
        vec![
            Some(PathBuf::from(r"C:\First")),
            Some(PathBuf::from(r"D:\Second")),
        ],
        None,
    );
    library.sources[0].display_name = "Same name".into();
    library.sources[1].display_name = "Same name".into();
    let entries = vec![
        library_projection_entry(1, 0, r"C:\First\one.txt"),
        library_projection_entry(2, 1, r"D:\Second\two.txt"),
        library_projection_entry(3, 1, r"D:\Second\three.txt"),
    ];

    let groups = library_source_group_projections(&library, &entries);

    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].label, "Same name");
    assert_eq!(groups[0].detail, r"C:\First");
    assert_eq!(groups[0].entries, [EntryId(1)]);
    assert_eq!(groups[1].detail, r"D:\Second");
    assert_eq!(groups[1].entries, [EntryId(2), EntryId(3)]);
    assert_ne!(groups[0].key, groups[1].key);
    assert!(groups.iter().all(|group| group.header_visible));
}

#[test]
fn library_projection_keeps_empty_sources_during_incremental_loading() {
    let library = test_library(
        "shell:library:incremental",
        "Incremental",
        vec![
            Some(PathBuf::from(r"C:\First")),
            Some(PathBuf::from(r"D:\Second")),
        ],
        None,
    );
    let first_batch = vec![library_projection_entry(1, 1, r"D:\Second\one.txt")];
    let mut second_batch = first_batch.clone();
    second_batch.push(library_projection_entry(2, 1, r"D:\Second\two.txt"));

    let first = library_source_group_projections(&library, &first_batch);
    let second = library_source_group_projections(&library, &second_batch);

    assert!(first[0].entries.is_empty());
    assert_eq!(first[1].entries, [EntryId(1)]);
    assert_eq!(second.len(), 2);
    assert_eq!(second[1].entries, [EntryId(1), EntryId(2)]);
    assert_eq!(first[1].key, second[1].key);
}

#[test]
fn library_projection_uses_source_identity_instead_of_path_spelling() {
    let library = test_library(
        "shell:library:duplicate-path",
        "Duplicate path",
        vec![
            Some(PathBuf::from(r"C:\Same")),
            Some(PathBuf::from(r"C:\Same")),
        ],
        None,
    );
    let entries = vec![
        library_projection_entry(1, 0, r"c:\same\first.txt"),
        library_projection_entry(2, 1, r"C:\SAME\second.txt"),
    ];

    let groups = library_source_group_projections(&library, &entries);
    let projected = groups
        .iter()
        .flat_map(|group| group.entries.iter().copied())
        .collect::<Vec<_>>();

    assert_eq!(groups[0].entries, [EntryId(1)]);
    assert_eq!(groups[1].entries, [EntryId(2)]);
    assert_eq!(projected, [EntryId(1), EntryId(2)]);
}

#[test]
fn grid_thumbnail_range_accounts_for_library_group_header_height() {
    let extents = [48.0, 140.0, 48.0, 140.0];
    assert_eq!(
        grid_thumbnail_request_rows(&extents, -188.0, 140.0, 0.0),
        vec![1, 2, 3]
    );
}

#[test]
fn library_write_target_uses_shell_default_save_location() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\before")], 0, [0, 1, 2, 3]);
    let library = test_library(
        "shell:library:documents",
        "Documents",
        vec![
            Some(PathBuf::from(r"C:\first")),
            Some(PathBuf::from(r"D:\second")),
        ],
        Some(PathBuf::from(r"D:\second")),
    );
    app.libraries.push(library);
    let active_tab = app.active_window_state().active_tab;
    complete_test_navigation(
        app.tab_mut(active_tab).unwrap(),
        NavigationLocation::Library(LibraryLocationId::new(
            "shell:library:documents".into(),
            "Documents".to_owned(),
        )),
        NavigationKind::Normal,
    );

    assert_eq!(
        active_write_target(&app, app.active_window).unwrap().1,
        PathBuf::from(r"D:\second")
    );
    app.libraries[0].default_save_path = None;
    assert_eq!(
        active_write_target(&app, app.active_window).unwrap_err(),
        Texts::new(app.language).library_no_default_save_location()
    );
}

#[test]
fn stale_library_events_do_not_replace_newer_navigation() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\before")], 0, [0, 1, 2, 3]);
    let active_tab = app.active_window_state().active_tab;
    let tab = app
        .active_window_state_mut()
        .tabs
        .get_mut(&active_tab)
        .unwrap();
    let first = LibraryLocationId::new("library:first".into(), "First".to_owned());
    let second = LibraryLocationId::new("library:second".into(), "Second".to_owned());
    let (stale_request, _) = tab.begin_navigation(
        NavigationLocation::Library(first.clone()),
        NavigationKind::Normal,
    );
    let (current_request, _) = tab.begin_navigation(
        NavigationLocation::Library(second.clone()),
        NavigationKind::Normal,
    );
    let state = Arc::new(Mutex::new(app));
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id: stale_request,
            entries: vec![focus_entry(1, r"C:\first\late.txt")],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id: TabId(1),
            request_id: stale_request,
            path: PathBuf::new(),
            skipped: 0,
            source_failures: 0,
            library: Some(first),
        },
    );
    let app = state.lock().unwrap();
    let tab = app.active();
    assert_eq!(tab.latest_request(), current_request);
    assert_eq!(
        tab.requested_location(),
        &Some(NavigationLocation::Library(second))
    );
    assert!(tab.pending_entries().is_empty());
    assert_eq!(tab.load_state, LoadState::Loading);
}
#[test]
fn issue_28_quick_search_defaults_off_and_starts_in_current_directory() {
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\work")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);

    assert_eq!(begin_file_list_quick_search(&state, 'a'), None);
    {
        let mut app = state.lock().unwrap();
        app.file_list_quick_search = true;
        let tab = app.active();
        assert_eq!(tab.page_source, PageSource::Directory);
        assert!(!tab.address_editing);
    }

    let (tab_id, input) = begin_file_list_quick_search(&state, '7').unwrap();
    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.search_depth, SearchDepth::CurrentFolder);
    assert_eq!(
        tab.search_scope,
        SearchScope::Directory(PathBuf::from(r"C:\work"))
    );
    assert_eq!(tab.search_query, "7");
    assert_eq!(tab.address_input, input);
    assert!(tab.address_editing);
    assert_eq!(tab.address_mode, AddressMode::Smart);
}

#[test]
fn issue_28_quick_search_preserves_directory_selection_when_cancelled() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\work")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let tab_id = app.active_window_state().active_tab;
    let entry = focus_entry(1, r"C:\work\alpha.txt");
    {
        let tab = app.tab_mut(tab_id).unwrap();
        tab.replace_entries(vec![entry]);
        tab.select_entry(EntryId(1), false, false);
    }
    app.file_list_quick_search = true;
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
    begin_file_list_quick_search(&state, 'a').unwrap();
    {
        let mut app = state.lock().unwrap();
        let tab = app.tab_mut(tab_id).unwrap();
        tab.begin_search(tab.search_scope.clone(), tab.search_query.clone());
        tab.cancel_address_edit();
        assert_eq!(tab.page_source, PageSource::Directory);
        assert_eq!(tab.selected, vec![EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
    }
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
    assert_eq!(app.active().latest_request(), RequestId(0));
}

#[test]
fn issue_20_selection_mutation_targets_the_owning_window() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\one")], 0, [0, 1, 2, 3]);
    let first_window = app.active_window;
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from(r"C:\two"))],
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
fn issue_20_keyboard_shortcuts_are_suppressed_only_by_editing_or_context_menu() {
    assert!(!keyboard_shortcuts_suppressed(false, false));
    assert!(keyboard_shortcuts_suppressed(true, false));
    assert!(keyboard_shortcuts_suppressed(false, true));
    assert!(keyboard_shortcuts_suppressed(true, true));
}

#[test]
fn issue_147_type_select_rejects_non_file_targets_ime_modifiers_and_invalid_text() {
    assert!(file_list_type_select_allowed(
        true,
        false,
        [false; 4],
        false,
        Some("a")
    ));
    assert!(file_list_type_select_allowed(
        true,
        false,
        [false; 4],
        false,
        Some("中")
    ));
    assert!(!file_list_type_select_allowed(
        false,
        false,
        [false; 4],
        false,
        Some("a")
    ));
    assert!(!file_list_type_select_allowed(
        true,
        true,
        [false; 4],
        false,
        Some("a")
    ));
    assert!(!file_list_type_select_allowed(
        true,
        false,
        [false; 4],
        true,
        Some("a")
    ));
    for index in 0..4 {
        let mut modifiers = [false; 4];
        modifiers[index] = true;
        assert!(!file_list_type_select_allowed(
            true,
            false,
            modifiers,
            false,
            Some("a")
        ));
    }
    for text in [None, Some(""), Some("ab"), Some(" "), Some("/"), Some("\n")] {
        assert!(!file_list_type_select_allowed(
            true, false, [false; 4], false, text
        ));
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
fn issue_67_active_and_hovered_tabs_share_one_corner_radius() {
    let ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    let radius = "VisualStyle.tab-corner-radius";

    assert_eq!(
        ui.matches("out property <length> tab-corner-radius: 7px;")
            .count(),
        1
    );
    assert_eq!(
        ui.matches("border-radius: VisualStyle.tab-corner-radius;")
            .count(),
        1
    );
    assert_eq!(
        ui.matches("corner-radius: VisualStyle.tab-corner-radius;")
            .count(),
        1
    );

    let tab_visual = ui
        .split_once("for tab[index] in root.tabs: Rectangle {")
        .expect("tab visual exists")
        .1
        .split_once("tab-touch := TouchArea {")
        .expect("tab visual has a touch boundary")
        .0;
    assert!(tab_visual.contains(&format!("border-radius: {radius};")));
    assert!(tab_visual.contains(&format!("corner-radius: {radius};")));

    let active_shape = ui
        .split_once("component ActiveTabShape inherits Rectangle {")
        .expect("active tab shape exists")
        .1
        .split_once("component IconButton inherits Rectangle {")
        .expect("active tab shape has a component boundary")
        .0;
    assert!(active_shape.contains("in property <length> corner-radius;"));
    assert!(active_shape.contains("root.corner-radius / 1px"));
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
    assert_eq!(ui.matches(semantic_font_size).count(), 6);
    let details_and_list_name = ui
        .split_once("for segment in entry.name-segments: Text {")
        .expect("details and list must render segmented file names")
        .1;
    assert!(details_and_list_name.contains(semantic_font_size));
    assert_eq!(
        ui.matches("text: entry.name; color: VisualStyle.c24252a; font-size: VisualStyle.file-name-font-size;")
            .count(),
        3
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
    assert!(everything_error_text(Language::Chinese, &EverythingError::Timeout).contains("超时"));
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
fn sidebar_visibility_menu_has_five_independent_localized_toggles() {
    let mut visibility = session_store::SidebarVisibility::default();
    assert!(visibility.toggle(3));
    let chinese = sidebar_visibility_rows(visibility, Language::Chinese);
    assert_eq!(chinese.len(), 5);
    assert_eq!(
        chinese
            .iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        ["快速访问", "库", "磁盘", "网络位置", "网络"]
    );
    assert_eq!(
        chinese.iter().map(|row| row.checked).collect::<Vec<_>>(),
        [true, false, true, false, true]
    );
    let english = sidebar_visibility_rows(visibility, Language::English);
    assert_eq!(
        english
            .iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Quick access",
            "Libraries",
            "Drives",
            "Network locations",
            "Network"
        ]
    );
}

#[test]
fn sidebar_visibility_commands_toggle_only_the_selected_category() {
    let mut visibility = session_store::SidebarVisibility::default();
    for offset in 0..5 {
        let before = visibility;
        assert!(toggle_sidebar_visibility(
            &mut visibility,
            CMD_SIDEBAR_VISIBILITY_BASE + offset
        ));
        let before_bits: [bool; 5] = std::array::from_fn(|index| before.is_visible(index));
        let after_bits: [bool; 5] = std::array::from_fn(|index| visibility.is_visible(index));
        for index in 0..5 {
            assert_eq!(
                after_bits[index],
                if index == offset as usize {
                    !before_bits[index]
                } else {
                    before_bits[index]
                }
            );
        }
    }
    assert!(!toggle_sidebar_visibility(&mut visibility, 999));
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

fn test_image(size: u32) -> platform::windows::shell_icons::ShellIconRgba {
    platform::windows::shell_icons::ShellIconRgba {
        width: size,
        height: size,
        pixels: vec![0; size as usize * size as usize * 4].into(),
    }
}

#[test]
fn issue_18_navigation_uses_pending_entries_before_the_first_thumbnail() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\old")], 0, [0, 1, 2, 3]);
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.begin_directory_navigation(PathBuf::from(r"C:\new"), NavigationKind::Normal);
    assert!(directory_display_entries(tab).is_empty());
    tab.accept_directory_batch(
        tab.latest_request(),
        vec![focus_entry(1, r"C:\new\photo.png")],
        DirectoryViewPreference::default(),
    );
    assert_eq!(directory_display_entries(tab).len(), 1);
    assert!(app.icons.is_empty());
    assert!(app.image_cache.thumbnails.is_empty());
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
    assert_eq!(grid_thumbnail_request_px(ViewMode::MediumIcons, 1.0), 48);
    assert_eq!(grid_thumbnail_request_px(ViewMode::LargeIcons, 1.5), 192);
    assert_eq!(
        grid_thumbnail_request_px(ViewMode::ExtraLargeIcons, 1.5),
        384
    );
}

#[test]
fn issue_97_image_requests_use_finite_dpi_buckets() {
    for (size, scale, expected) in [
        (16, 1.0, 16),
        (48, 1.25, 64),
        (72, 1.0, 96),
        (72, 1.5, 128),
        (104, 2.0, 256),
        (256, 1.5, 384),
        (256, 2.0, 512),
    ] {
        assert_eq!(
            grid_thumbnail_request_px(ViewMode::Icons(size), scale),
            expected
        );
    }
    let buckets = (16..=256)
        .flat_map(|size| {
            [1.0, 1.25, 1.5, 2.0]
                .map(|scale| grid_thumbnail_request_px(ViewMode::Icons(size), scale))
        })
        .collect::<HashSet<_>>();
    assert!(buckets.len() <= 12);
}

#[test]
fn issue_88_rust_geometry_matches_the_slint_view_geometry() {
    for (mode, row_height, card_width, card_height) in [
        (ViewMode::Details, 32.0, 0.0, 32.0),
        (ViewMode::List, 30.0, 0.0, 30.0),
        (ViewMode::SmallIcons, 32.0, 180.0, 24.0),
        (ViewMode::MediumIcons, 124.0, 88.0, 116.0),
        (ViewMode::LargeIcons, 172.0, 112.0, 164.0),
        (ViewMode::ExtraLargeIcons, 332.0, 272.0, 324.0),
        (ViewMode::Tiles, 78.0, 292.0, 70.0),
        (ViewMode::Content, 68.0, 0.0, 68.0),
    ] {
        let geometry = file_layout_geometry(mode);
        assert_eq!(geometry.row_height, row_height, "{mode:?}");
        assert_eq!(geometry.card_width, card_width, "{mode:?}");
        assert_eq!(geometry.card_height, card_height, "{mode:?}");
    }
}

#[test]
fn issue_116_grid_columns_use_content_width_instead_of_window_minus_292() {
    assert_eq!(navigation_sidebar_width(1180.0), 218.0);
    assert_eq!(navigation_sidebar_width(950.0), 184.0);
    assert_eq!(navigation_sidebar_width(800.0), 0.0);
    assert_eq!(estimated_file_viewport_width(1180.0), 934.0);
    for (width, card_width, columns) in [(1180.0, 88.0, 9), (900.0, 88.0, 6), (1180.0, 292.0, 3)] {
        assert_eq!(
            grid_column_count_for_width(estimated_file_viewport_width(width), card_width),
            columns,
            "width={width} card={card_width}"
        );
    }
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(source.contains("width: parent.width - 28px;"));
    assert!(source.contains("padding-left: 16px;"));
    assert!(source.contains("padding-right: 16px;"));
    assert!(source.contains("spacing: 8px;"));
    assert!(source.contains("width: root.compact ? 184px : 218px;"));
}

#[test]
fn issue_18_grid_batches_skip_eager_icons_but_details_keep_them() {
    let mut grid = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
    grid.update_directory_preference(PathBuf::from(r"C:\grid"), |preference| {
        preference.view_mode = ViewMode::ExtraLargeIcons
    });
    let tab = grid.tab_mut(TabId(1)).unwrap();
    advance_test_navigation(tab, RequestId(4));
    tab.load_state = LoadState::Loading;
    let grid = Arc::new(Mutex::new(grid));
    let DirectoryEventChange {
        icon_requests: grid_requests,
        ..
    } = apply_event(
        &grid,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id: RequestId(4),
            entries: vec![focus_entry(1, r"C:\grid\photo.png")],
        },
    );
    assert!(grid_requests.is_empty());

    let mut details = AppState::new_for_test(vec![PathBuf::from(r"C:\details")], 0, [0, 1, 2, 3]);
    let tab = details.tab_mut(TabId(1)).unwrap();
    advance_test_navigation(tab, RequestId(5));
    tab.load_state = LoadState::Loading;
    let details = Arc::new(Mutex::new(details));
    let DirectoryEventChange {
        icon_requests: details_requests,
        ..
    } = apply_event(
        &details,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
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
    let mut cache = icon_cache::BoundedImages::new(2);
    cache.insert((PathBuf::from("first.png"), 256), test_image(1));
    cache.insert((PathBuf::from("second.png"), 256), test_image(1));
    assert!(cache.get(&(PathBuf::from("first.png"), 256)).is_some());
    cache.insert((PathBuf::from("third.png"), 256), test_image(1));

    assert!(!cache.contains_key(&(PathBuf::from("first.png"), 256)));
    assert!(cache.contains_key(&(PathBuf::from("second.png"), 256)));
    assert!(cache.contains_key(&(PathBuf::from("third.png"), 256)));

    cache.insert((PathBuf::from("second.png"), 256), test_image(2));
    assert_eq!(cache.len(), 2);
    assert_eq!(
        cache
            .get(&(PathBuf::from("second.png"), 256))
            .unwrap()
            .width,
        2
    );
    cache.insert((PathBuf::from("fourth.png"), 256), test_image(1));
    assert!(!cache.contains_key(&(PathBuf::from("third.png"), 256)));
    assert!(cache.contains_key(&(PathBuf::from("second.png"), 256)));
    assert!(cache.contains_key(&(PathBuf::from("fourth.png"), 256)));
}
#[test]
fn issue_88_thumbnail_retry_is_delayed_and_bounded() {
    let now = Instant::now();
    assert!(thumbnail_retry_due(None, now));
    assert!(!thumbnail_retry_due(Some(&(1, now)), now));
    assert!(thumbnail_retry_due(
        Some(&(1, now - THUMBNAIL_RETRY_DELAY)),
        now
    ));
    assert!(!thumbnail_retry_due(
        Some(&(THUMBNAIL_MAX_ATTEMPTS, now - THUMBNAIL_RETRY_DELAY)),
        now
    ));
}
fn context_test_row(id: i32, label: &str, search_text: &str, separator: bool) -> ContextCommandRow {
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
fn issue_111_shell_popup_actions_have_distinct_stable_identities() {
    let first = context_test_row(SHELL_CONTEXT_COMMAND_BASE + 1, "Open", "open", false);
    let second = context_test_row(SHELL_CONTEXT_COMMAND_BASE + 2, "Copy", "copy", false);
    let mut submenu = first.clone();
    submenu.submenu = true;
    submenu.node_id = first.id;
    let rows = popup_rows(&[first.clone(), second, submenu]);

    assert_ne!(rows[0].stable_id, rows[1].stable_id);
    assert_ne!(rows[0].stable_id, rows[2].stable_id);
    assert_eq!(
        rows[2].stable_id.as_str(),
        format!("shell-session/node/{}", first.id)
    );
    let mut translated = first.clone();
    translated.label = "打开".into();
    assert_eq!(popup_rows(&[translated])[0].stable_id, rows[0].stable_id);
}
#[test]
fn issue_18_builtin_menu_rows_expose_only_their_own_icons() {
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\menu")], 0, [0, 1, 2, 3]);
    let (rows, submenus) = built_in_context_rows(&app, false, true);
    assert_eq!(rows[0].icon_kind, 1);
    assert_eq!(rows[1].icon_kind, 2);
    assert_eq!(rows[2].icon_kind, 3);
    assert_eq!(rows[3].icon_kind, 4);
    assert_eq!(
        rows.iter()
            .find(|row| row.node_id == NODE_CREATE)
            .unwrap()
            .icon_kind,
        0
    );
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
fn shell_test_item(
    command_id: Option<u32>,
    title: &str,
    verb: Option<&str>,
    enabled: bool,
    kind: platform::windows::context_menu::ClassicMenuItemKind,
) -> platform::windows::context_menu::ClassicMenuItem {
    platform::windows::context_menu::ClassicMenuItem {
        command_id,
        title: title.to_owned(),
        verb: verb.map(str::to_owned),
        enabled,
        checked: false,
        default: false,
        kind,
    }
}

#[test]
fn issue_57_background_menu_has_one_builtin_new_submenu() {
    for language in [Language::Chinese, Language::English] {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\menu")], 0, [0, 1, 2, 3]);
        app.language = language;
        let (rows, submenus) = built_in_context_rows(&app, false, true);
        let new_rows = rows
            .iter()
            .filter(|row| row.node_id == NODE_CREATE)
            .collect::<Vec<_>>();
        assert_eq!(new_rows.len(), 1);
        assert_eq!(new_rows[0].label.as_str(), Texts::new(language).new_menu());
        assert!(new_rows[0].submenu && !new_rows[0].shell);
        assert!(!rows.iter().any(|row| row.id == 1));
        assert!(!submenus.contains_key(&NODE_CREATE));
    }
}

#[test]
fn issue_57_merges_new_by_verb_and_preserves_shell_templates() {
    use platform::windows::context_menu::ClassicMenuItemKind;
    let new_children = vec![
        shell_test_item(
            Some(33),
            "任意文件夹标题",
            Some("newfolder"),
            true,
            ClassicMenuItemKind::Command,
        ),
        shell_test_item(
            Some(34),
            "任意快捷方式标题",
            Some("newlink"),
            false,
            ClassicMenuItemKind::Command,
        ),
        shell_test_item(None, "", None, false, ClassicMenuItemKind::Separator),
        shell_test_item(
            Some(40),
            "第三方文件夹模板",
            Some(".txt"),
            true,
            ClassicMenuItemKind::Command,
        ),
    ];
    let mut menu = QuickMenuState {
        identity: Some(QuickMenuIdentity {
            session_id: 1,
            request_id: 1,
            key: QuickMenuKey {
                window_id: WindowId(1),
                tab_id: TabId(1),
                navigation_request: RequestId(1),
                scope: QuickMenuScope::Content {
                    paths: Vec::new(),
                    location: Some(
                        platform::windows::context_menu::ShellMenuBackgroundTarget::FileSystem(
                            PathBuf::from(r"C:\menu"),
                        ),
                    ),
                },
            },
            ready: true,
        }),
        language: Some(Language::Chinese),
        ..QuickMenuState::default()
    };
    let root = project_shell_menu_items(
        &mut menu,
        vec![
            shell_test_item(
                Some(10),
                "Before",
                Some("before"),
                true,
                ClassicMenuItemKind::Command,
            ),
            shell_test_item(
                Some(33),
                "任意语言",
                Some("new"),
                true,
                ClassicMenuItemKind::Submenu {
                    token: 7,
                    items: new_children,
                },
            ),
            shell_test_item(
                Some(50),
                "After",
                Some("after"),
                true,
                ClassicMenuItemKind::Command,
            ),
        ],
        None,
    );
    assert_eq!(
        root.iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        ["Before", "After"]
    );
    assert_eq!(menu.submenu_tokens.get(&NODE_CREATE), Some(&7));
    let children = menu.preloaded_submenu_rows.get(&7).unwrap();
    assert_eq!(
        children
            .iter()
            .map(|row| row.label.as_str())
            .collect::<Vec<_>>(),
        ["文件夹", "任意快捷方式标题", "", "第三方文件夹模板"]
    );
    assert_eq!(children[0].id, 1);
    assert!(!children[0].shell);
    assert_eq!(children[1].id, SHELL_CONTEXT_COMMAND_BASE + 34);
    assert!(!children[1].enabled);
    assert_eq!(children[3].id, SHELL_CONTEXT_COMMAND_BASE + 40);
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
        None,
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
fn issue_68_root_and_submenus_share_the_backdrop_setting() {
    let menu_ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/quick-menu.slint"));
    assert_eq!(
        menu_ui
            .matches("in property <bool> backdrop-enabled: true;")
            .count(),
        2
    );

    let app_ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(app_ui.contains("in property <bool> quick-menu-backdrop: true;"));
    assert!(app_ui.contains("in property <float> quick-menu-backdrop-opacity: 85;"));
    assert!(app_ui.contains("callback change-quick-menu-backdrop(bool);"));
    assert!(app_ui.contains("callback change-quick-menu-backdrop-opacity(float);"));
}

#[test]
fn issue_75_root_menu_height_includes_layout_spacing() {
    let content_height = 55.0;
    assert_eq!(
        root_popup_height_for_content(content_height, false, 1.0),
        96
    );

    let menu_ui = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/quick-menu.slint"));
    assert!(menu_ui.contains(
        "window-height: max(41px, 41px + root.content-height + (root.loading ? 20px : 0px));"
    ));
    assert!(menu_ui.contains("padding: 4px; spacing: 1px;"));
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
fn root_popup_waits_for_shell_before_presentation() {
    assert!(!root_popup_presentation_ready(
        PopupPresentation::ShownCloaked,
        true,
    ));
    assert!(root_popup_presentation_ready(
        PopupPresentation::ShownCloaked,
        false,
    ));
    assert!(!root_popup_presentation_ready(
        PopupPresentation::Presented,
        false,
    ));
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
fn issue_16_drive_projection_covers_loading_ready_warning_and_unavailable() {
    let loading = DriveCapacityState::loading(1);
    assert_eq!(drive_projection(Language::Chinese, Some(&loading)).1, 0);

    let mut ready = DriveCapacityState::loading(2);
    ready.apply(Ok(platform::windows::drives::DriveCapacity {
        total: 1_000,
        available: 250,
    }));
    let (progress, state, status) = drive_projection(Language::English, Some(&ready));
    assert!((progress - 0.75).abs() < f32::EPSILON);
    assert_eq!(state, 1);
    assert_eq!(status, "Drive usage normal");

    ready.apply(Ok(platform::windows::drives::DriveCapacity {
        total: 1_000,
        available: 99,
    }));
    assert_eq!(drive_projection(Language::Chinese, Some(&ready)).1, 2);

    ready.apply(Err(io::Error::other("unavailable")));
    assert_eq!(drive_projection(Language::English, Some(&ready)).1, 3);
}

#[test]
fn issue_16_affected_drive_refresh_keeps_only_unique_local_roots() {
    let roots = affected_drive_roots(&[
        PathBuf::from(r"c:\one"),
        PathBuf::from(r"C:\two"),
        PathBuf::from(r"D:\three"),
        PathBuf::from(r"\\server\share\four"),
    ]);
    assert_eq!(roots.len(), 2);
    assert!(roots.contains(Path::new(r"C:\")));
    assert!(roots.contains(Path::new(r"D:\")));
}

#[test]
fn issue_16_stale_drive_result_identity_requires_root_and_generation() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
    let root = PathBuf::from(r"C:\");
    app.sidebar = vec![KnownLocation {
        kind: KnownLocationKind::Drive,
        label: "C:".to_owned(),
        path: root.clone(),
        drive_root: Some(root.clone()),
    }];
    app.drive_capacity
        .insert(root.clone(), DriveCapacityState::loading(7));
    assert!(
        app.drive_capacity
            .get(&root)
            .is_some_and(|state| state.request_generation == 7)
    );
    assert!(
        app.drive_capacity
            .get(&root)
            .is_none_or(|state| state.request_generation != 6)
    );
    app.sidebar.clear();
    assert!(
        !app.sidebar
            .iter()
            .any(|location| location.drive_root.as_ref() == Some(&root))
    );
}
#[test]
fn issue_50_sidebar_targets_use_model_identity_and_generation() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
    app.quick_access_generation = 7;
    app.sidebar = vec![KnownLocation {
        kind: KnownLocationKind::Drive,
        label: "Display only".to_owned(),
        path: PathBuf::from(r"Z:\"),
        drive_root: Some(PathBuf::from(r"Z:\")),
    }];
    let target = sidebar_menu_target(&app, WindowId(1), 1, "location:0").expect("drive target");
    assert_eq!(target.generation, 7);
    assert_eq!(
        target.target,
        platform::windows::context_menu::ShellMenuItemTarget::FileSystem(PathBuf::from(r"Z:\"))
    );
    assert!(sidebar_menu_target(&app, WindowId(1), 1, "Display only").is_none());
}

#[test]
fn issue_50_sidebar_key_rejects_refresh_navigation_and_other_window() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
    app.quick_access_generation = 4;
    app.sidebar = vec![KnownLocation {
        kind: KnownLocationKind::Pinned,
        label: "Pinned".to_owned(),
        path: PathBuf::from(r"C:\Pinned"),
        drive_root: None,
    }];
    let shared = Arc::new(Mutex::new(app));
    let resolved =
        sidebar_menu_target(&shared.lock().unwrap(), WindowId(1), 0, "location:0").unwrap();
    let key = QuickMenuKey {
        window_id: WindowId(1),
        tab_id: TabId(1),
        navigation_request: shared
            .lock()
            .unwrap()
            .tab(TabId(1))
            .unwrap()
            .latest_request(),
        scope: QuickMenuScope::Sidebar {
            group: 0,
            stable_id: resolved.stable_id,
            generation: resolved.generation,
            target: resolved.target,
        },
    };
    assert!(quick_menu_key_is_current(&shared, &key));
    shared.lock().unwrap().quick_access_generation += 1;
    assert!(!quick_menu_key_is_current(&shared, &key));
    shared.lock().unwrap().quick_access_generation = 4;
    shared
        .lock()
        .unwrap()
        .tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"C:\current"), NavigationKind::Refresh);
    assert!(!quick_menu_key_is_current(&shared, &key));
    let mut other = key.clone();
    other.window_id = WindowId(2);
    assert!(!quick_menu_key_is_current(&shared, &other));
}
#[test]
fn issue_50_library_and_shell_network_preserve_namespace_identity() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
    let library_identity =
        std::ffi::OsString::from(r"::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\项目.library-ms");
    app.library_generation = 9;
    app.libraries.push(test_library(
        library_identity.to_string_lossy().as_ref(),
        "Display library",
        vec![Some(PathBuf::from(r"D:\Data"))],
        Some(PathBuf::from(r"D:\Data")),
    ));
    let library = sidebar_menu_target(
        &app,
        WindowId(1),
        4,
        &library_stable_id(library_identity.as_os_str()),
    )
    .unwrap();
    assert!(
        matches!(library.target, platform::windows::context_menu::ShellMenuItemTarget::Namespace { shell_identity } if shell_identity == library_identity)
    );
    let shell_identity = PathBuf::from(r"::{208D2C60-3AEA-1069-A2D7-08002B30309D}\服务器");
    app.network_location_generation = 5;
    app.imported_network_locations = vec![NetworkLocation {
        id: 42,
        source: NetworkLocationSource::WindowsImported,
        display_name: "Display network".to_owned(),
        sort_order: 0,
        target: NetworkTarget::ShellItemId(shell_identity.clone()),
        shell_path: None,
    }];
    let network = sidebar_menu_target(&app, WindowId(1), 2, "network-location:42").unwrap();
    assert!(
        matches!(network.target, platform::windows::context_menu::ShellMenuItemTarget::Namespace { shell_identity: identity } if identity == shell_identity.as_os_str())
    );
}

#[test]
fn issue_50_current_sidebar_target_rejects_removed_node() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
    app.quick_access_generation = 1;
    app.sidebar = vec![KnownLocation {
        kind: KnownLocationKind::Pinned,
        label: "Pinned".to_owned(),
        path: PathBuf::from(r"C:\Pinned"),
        drive_root: None,
    }];
    let shared = Arc::new(Mutex::new(app));
    let state = WindowSessions::new(shared.clone(), WindowId(1));
    let resolved =
        sidebar_menu_target(&shared.lock().unwrap(), WindowId(1), 0, "location:0").unwrap();
    let key = QuickMenuKey {
        window_id: WindowId(1),
        tab_id: TabId(1),
        navigation_request: shared
            .lock()
            .unwrap()
            .tab(TabId(1))
            .unwrap()
            .latest_request(),
        scope: QuickMenuScope::Sidebar {
            group: 0,
            stable_id: resolved.stable_id.clone(),
            generation: resolved.generation,
            target: resolved.target.clone(),
        },
    };
    let menu = Arc::new(Mutex::new(QuickMenuState {
        identity: Some(QuickMenuIdentity {
            session_id: 1,
            request_id: 1,
            key,
            ready: true,
        }),
        active_sidebar_target: Some(resolved),
        ..QuickMenuState::default()
    }));
    assert!(current_sidebar_menu_target(&state, &menu).is_some());
    shared.lock().unwrap().sidebar.clear();
    assert!(current_sidebar_menu_target(&state, &menu).is_none());
}
#[test]
fn issue_69_library_background_uses_original_shell_identity() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\before")], 0, [0, 1, 2, 3]);
    let identity =
        std::ffi::OsString::from(r"::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\项目.library-ms");
    let library = LibraryLocationId::new(identity.clone(), "项目".to_owned());
    complete_test_navigation(
        app.tab_mut(TabId(1)).unwrap(),
        NavigationLocation::Library(library),
        NavigationKind::Normal,
    );
    app.libraries.push(test_library(
        identity.to_string_lossy().as_ref(),
        "项目",
        vec![
            Some(PathBuf::from(r"C:\First")),
            Some(PathBuf::from(r"D:\Second")),
        ],
        Some(PathBuf::from(r"D:\Second")),
    ));
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));

    let (key, target) = quick_menu_key(&state, true).expect("library background target");
    let expected = platform::windows::context_menu::ShellMenuBackgroundTarget::Namespace {
        shell_identity: identity,
        working_directory: Some(PathBuf::from(r"D:\Second")),
    };
    assert!(
        matches!(&key.scope, QuickMenuScope::Content { location: Some(candidate), paths } if candidate == &expected && paths.is_empty())
    );
    assert!(matches!(
        target,
        platform::windows::context_menu::ShellMenuLoadTarget::Background(target)
            if target == expected
    ));
}

#[test]
fn issue_69_selection_menu_keeps_its_visible_location_identity() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\folder")], 0, [0, 1, 2, 3]);
    app.tab_mut(TabId(1))
        .unwrap()
        .replace_entries(vec![focus_entry(1, r"C:\folder\selected.txt")]);
    app.tab_mut(TabId(1)).unwrap().selected = vec![EntryId(1)].into();
    let shared = Arc::new(Mutex::new(app));
    let state = WindowSessions::new(shared.clone(), WindowId(1));

    let (key, target) = quick_menu_key(&state, false).expect("selection target");
    assert!(
        matches!(&key.scope, QuickMenuScope::Content { location: Some(platform::windows::context_menu::ShellMenuBackgroundTarget::FileSystem(path)), paths } if path == &PathBuf::from(r"C:\folder") && paths == &[PathBuf::from(r"C:\folder\selected.txt")])
    );
    assert!(matches!(
        target,
        platform::windows::context_menu::ShellMenuLoadTarget::Paths(paths)
            if paths == [PathBuf::from(r"C:\folder\selected.txt")]
    ));
    assert!(quick_menu_key_is_current(&shared, &key));
}
#[test]
fn issue_69_library_powershell_reuses_the_default_save_location() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\before")], 0, [0, 1, 2, 3]);
    let identity = std::ffi::OsString::from("shell:library:multi-source");
    complete_test_navigation(
        app.tab_mut(TabId(1)).unwrap(),
        NavigationLocation::Library(LibraryLocationId::new(
            identity.clone(),
            "Multi source".to_owned(),
        )),
        NavigationKind::Normal,
    );
    app.libraries.push(test_library(
        identity.to_string_lossy().as_ref(),
        "Multi source",
        vec![
            Some(PathBuf::from(r"C:\First")),
            Some(PathBuf::from(r"D:\Second")),
        ],
        Some(PathBuf::from(r"D:\Second")),
    ));

    assert_eq!(
        quick_menu_location_target(&app, app.active()),
        Some(
            platform::windows::context_menu::ShellMenuBackgroundTarget::Namespace {
                shell_identity: identity,
                working_directory: Some(PathBuf::from(r"D:\Second")),
            }
        )
    );
}
#[test]
fn issue_69_library_background_key_rejects_refresh_and_location_changes() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\before")], 0, [0, 1, 2, 3]);
    let library = LibraryLocationId::new("shell:library:one".into(), "One".to_owned());
    complete_test_navigation(
        app.tab_mut(TabId(1)).unwrap(),
        NavigationLocation::Library(library.clone()),
        NavigationKind::Normal,
    );
    let shared = Arc::new(Mutex::new(app));
    let state = WindowSessions::new(shared.clone(), WindowId(1));
    let (key, _) = quick_menu_key(&state, true).expect("library background target");
    assert!(quick_menu_key_is_current(&shared, &key));

    shared
        .lock()
        .unwrap()
        .tab_mut(TabId(1))
        .unwrap()
        .begin_navigation(
            NavigationLocation::Library(library.clone()),
            NavigationKind::Refresh,
        );
    assert!(!quick_menu_key_is_current(&shared, &key));
    {
        let mut app = shared.lock().unwrap();
        let tab = app.tab_mut(TabId(1)).unwrap();
        *tab = TabSession::new_at(tab.id, NavigationLocation::Library(library));
        complete_test_navigation(
            tab,
            NavigationLocation::Library(LibraryLocationId::new(
                "shell:library:two".into(),
                "Two".to_owned(),
            )),
            NavigationKind::Normal,
        );
    }
    assert!(!quick_menu_key_is_current(&shared, &key));
}

#[test]
fn issue_69_empty_shell_menu_load_is_a_successful_terminal_state() {
    let key = QuickMenuKey {
        window_id: WindowId(1),
        tab_id: TabId(2),
        navigation_request: RequestId(3),
        scope: QuickMenuScope::Content {
            paths: Vec::new(),
            location: Some(
                platform::windows::context_menu::ShellMenuBackgroundTarget::Namespace {
                    shell_identity: "shell:library:one".into(),
                    working_directory: None,
                },
            ),
        },
    };
    let mut menu = QuickMenuState {
        identity: Some(QuickMenuIdentity {
            session_id: 41,
            request_id: 7,
            key,
            ready: false,
        }),
        all_rows: (0..QUICK_MENU_PLACEHOLDER_ROWS)
            .map(|_| quick_menu_placeholder())
            .collect(),
        built_in_rows: vec![context_test_row(1, "Refresh", "refresh", false)],
        ..QuickMenuState::default()
    };

    assert!(apply_loaded_shell_menu(&mut menu, 41, 7, Vec::new()));
    assert!(
        menu.identity
            .as_ref()
            .is_some_and(|identity| identity.ready)
    );
    assert_eq!(menu.all_rows, menu.built_in_rows);
    assert!(
        menu.all_rows
            .iter()
            .all(|row| !row.placeholder && !row.loading)
    );
    assert!(!apply_loaded_shell_menu(&mut menu, 41, 8, Vec::new()));
}
#[test]
fn issue_69_shell_menu_failure_is_terminal_and_retryable() {
    let key = QuickMenuKey {
        window_id: WindowId(1),
        tab_id: TabId(2),
        navigation_request: RequestId(3),
        scope: QuickMenuScope::Content {
            paths: Vec::new(),
            location: Some(
                platform::windows::context_menu::ShellMenuBackgroundTarget::Namespace {
                    shell_identity: "shell:library:one".into(),
                    working_directory: None,
                },
            ),
        },
    };
    let mut menu = QuickMenuState {
        identity: Some(QuickMenuIdentity {
            session_id: 41,
            request_id: 7,
            key,
            ready: false,
        }),
        all_rows: (0..QUICK_MENU_PLACEHOLDER_ROWS)
            .map(|_| quick_menu_placeholder())
            .collect(),
        built_in_rows: vec![context_test_row(1, "Refresh", "refresh", false)],
        ..QuickMenuState::default()
    };

    assert!(clear_current_shell_menu_failure(&mut menu, 41, 7));
    assert!(menu.identity.is_none());
    assert_eq!(menu.all_rows, menu.built_in_rows);
    assert!(
        menu.all_rows
            .iter()
            .all(|row| !row.placeholder && !row.loading)
    );
    assert!(!clear_current_shell_menu_failure(&mut menu, 41, 7));
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
    let (rows, loading, cache_hit, session_hit) = project_cached_shell_rows(true, Some(&snapshot));
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
        scope: QuickMenuScope::Content {
            paths: vec![PathBuf::from(r"C:\first.txt")],
            location: Some(
                platform::windows::context_menu::ShellMenuBackgroundTarget::FileSystem(
                    PathBuf::from(r"C:\"),
                ),
            ),
        },
    };
    let mut second = first.clone();
    if let QuickMenuScope::Content { paths, .. } = &mut second.scope {
        *paths = vec![PathBuf::from(r"C:\second.txt")];
    }
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
fn issue_72_filter_synchronizes_search_before_projection() {
    let source = include_str!("callbacks/context_menu.rs");
    let filter = source
        .split_once("ui.on_filter_context_menu(move |query| {")
        .expect("context menu filter exists")
        .1
        .split_once("ui.on_move_context_selection")
        .expect("context menu filter has a callback boundary")
        .0;
    let synchronize = filter
        .find("ui.set_context_search(query.clone());")
        .expect("filter stores the current search");
    let project = filter
        .find("project_filtered_context_menu(&ui, &quick_menu_for_filter, query.as_str());")
        .expect("filter projects matching rows");

    assert!(synchronize < project);
}

#[test]
fn issue_72_search_placeholder_stays_concise() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless app window should initialize");

    apply_ui_texts(&ui, Language::Chinese);
    assert_eq!(ui.get_text_context_search().as_str(), "搜索命令");

    apply_ui_texts(&ui, Language::English);
    assert_eq!(ui.get_text_context_search().as_str(), "Search commands");
}
#[test]
fn quick_menu_filter_matches_text_pinyin_initials_and_verb_without_shell_work() {
    let paste = ContextCommandRow {
        id: 1,
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
        icon_kind: 5,
    };
    let terminal = ContextCommandRow {
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
    };
    let rows = vec![
        context_test_row(3, "Copy", "copy", false),
        context_test_row(-1, "", "", true),
        paste.clone(),
        terminal.clone(),
        context_test_row(4, "拼音 Search", "", false),
    ];

    for query in ["粘贴", "zhantie", "zt", "ZHANTIE", "paste"] {
        assert_eq!(
            filtered_context_rows(&rows, query).as_slice(),
            std::slice::from_ref(&paste)
        );
    }
    for query in ["终端", "zhongduan", "zd", "openinterminal"] {
        assert_eq!(
            filtered_context_rows(&rows, query).as_slice(),
            std::slice::from_ref(&terminal)
        );
    }
    assert_eq!(
        filtered_context_rows(&rows, "COPY")
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [3]
    );
    assert_eq!(
        filtered_context_rows(&rows, "pinyin search")
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [4]
    );
    assert!(filtered_context_rows(&rows, "zhantiex").is_empty());
    assert!(filtered_context_rows(&rows, "missing").is_empty());
}
#[test]
fn quick_menu_filter_accepts_each_single_keyboard_character() {
    let rows = [context_test_row(1, "粘贴 Copy [测试]", "paste", false)];
    for character in ' '..='~' {
        let query = character.to_string();
        let _ = filtered_context_rows(&rows, &query);
    }
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

    assert_eq!(context_menu_content_height(&rows), 55.0);
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
        None,
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
                scope: QuickMenuScope::Content {
                    paths: vec![PathBuf::from(r"C:\selected.txt")],
                    location: None,
                },
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
        true,
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
        true,
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
        search_window_viewport_y(-65_536.0 * 32.0, window, ViewMode::Details, 1),
        -256.0 * 32.0
    );
    let grid_window = search_window_for_index(65_536, 133_796, 4);
    assert_eq!(grid_window.start % 4, 0);
    assert_eq!(
        search_window_viewport_y(
            -16_384.0 * 124.0 - 17.5,
            grid_window,
            ViewMode::MediumIcons,
            4
        ),
        -64.0 * 124.0 - 17.5
    );
}
#[test]
fn logical_search_scroll_maps_each_view_mode_to_absolute_result_index() {
    assert_eq!(
        search_result_index_at_scroll(-65_536.0 * 32.0, 133_796, ViewMode::Details, 1),
        65_536
    );
    assert_eq!(
        search_result_index_at_scroll(-65_536.0 * 30.0, 133_796, ViewMode::List, 1),
        65_536
    );
    assert_eq!(
        search_result_index_at_scroll(-16_384.0 * 124.0, 133_796, ViewMode::MediumIcons, 4),
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
    assert_eq!(grid_thumbnail_request_px(ViewMode::MediumIcons, 1.0), 48);
    assert_eq!(grid_thumbnail_request_px(ViewMode::LargeIcons, 1.5), 192);
    assert_eq!(
        grid_thumbnail_request_px(ViewMode::ExtraLargeIcons, 1.5),
        384
    );
}

#[test]
fn issue_99_switch_to_list_dispatches_missing_icons_once_and_uses_completed_cache() {
    i_slint_backend_testing::init_no_event_loop();
    {
        let mode = ViewMode::List;
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        let state = issue_98_state();
        let (sender, receiver) = mpsc::channel();
        let scheduler = ThumbnailScheduler::new(sender);
        let sessions = WindowSessions::new(state.clone(), WindowId(1));
        {
            let mut app = state.lock().unwrap();
            app.tab_mut(TabId(1)).unwrap().load_state = LoadState::Complete;
            app.update_directory_preference(PathBuf::from(r"C:\grid"), |view| {
                view.view_mode = ViewMode::MediumIcons;
            });
        }
        refresh_ui(&ui, &sessions);
        request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
        assert!(receiver.try_recv().is_err());
        assert!(apply_icon_event(&state, issue_98_icon_event(r"C:\grid\model.mtl")).is_some());

        set_view_mode(&sessions, mode);
        refresh_ui(&ui, &sessions);
        request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
        let request = receiver
            .try_recv()
            .expect("switching to a row view must request its ordinary icon");
        assert!(!request.thumbnail);
        assert_eq!(request.requested_px, 0);
        assert_eq!(request.target, IconTarget::Entry(EntryId(1)));
        request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
        assert!(receiver.try_recv().is_err());

        let mut completed = issue_98_icon_event(r"C:\grid\model.mtl");
        completed.path = request.path;
        completed.requested_px = 0;
        completed.type_key = request.type_key;
        completed.icon = test_image(32);
        assert!(apply_icon_event(&state, completed).is_some());
        assert!(state.lock().unwrap().ordinary_icon_requests.is_empty());
        request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
        assert!(receiver.try_recv().is_err());
        let app = state.lock().unwrap();
        let tab = app.tab(TabId(1)).unwrap();
        assert_eq!(
            file_row(
                tab.visible_entry(EntryId(1)).unwrap(),
                tab,
                Texts::new(Language::Chinese),
                &app,
                None
            )
            .icon
            .size()
            .width,
            32
        );
    }
}

#[test]
fn issue_99_grouped_search_list_requests_only_loaded_nearby_rows_and_scrolls() {
    issue_99_check_grouped_viewport(PageSource::Search);
}

#[test]
fn issue_99_grouped_directory_list_requests_only_nearby_rows_and_scrolls() {
    issue_99_check_grouped_viewport(PageSource::Directory);
}

fn issue_99_check_grouped_viewport(source: PageSource) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    let state = issue_98_state();
    let entries = (1..=1000)
        .map(|id| focus_entry(id, &format!(r"C:\grid\app{id:04}.exe")))
        .collect::<Vec<_>>();
    {
        let mut app = state.lock().unwrap();
        app.search_view.view_mode = ViewMode::List;
        app.update_directory_preference(PathBuf::from(r"C:\grid"), |view| {
            view.view_mode = ViewMode::List;
            view.group_field = GroupField::Kind;
        });
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.page_source = source;
        tab.load_state = LoadState::Complete;
        tab.replace_entries(entries.clone());
    }
    let unloaded_id = if source == PageSource::Search {
        let mut rows = vec![group_header_file_row("apps", "group detail", 1000)];
        rows.extend(entries.iter().map(|entry| FileRow {
            id: entry.id.0 as i32,
            loaded: true,
            ..empty_file_row()
        }));
        rows[2].loaded = false;
        ui.set_files(ModelRc::new(VecModel::from(rows)));
        Some(2)
    } else {
        refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
        None
    };
    let rows = ui.get_files();
    assert_eq!(rows.row_count(), 1001);
    assert!(rows.row_data(0).unwrap().group_header);
    assert_eq!(rows.row_data(1).unwrap().id, 1);
    let header_height =
        group_header_height_for_detail(rows.row_data(0).unwrap().group_detail.as_str()) as f32;
    let row_height = file_layout_geometry(ViewMode::List).row_height;
    let height = ui.get_file_viewport_height().max(row_height);
    // The expected identities come from fixed fixture geometry, including a header
    // and one missing search row, rather than the image planner's range helper.
    let expected_at = |top: f32| -> Vec<u32> {
        (1..=1000)
            .filter(|id| {
                let bottom = header_height + *id as f32 * row_height;
                let start = bottom - row_height;
                Some(*id) != unloaded_id
                    && bottom >= (top - height).max(0.0)
                    && start <= top + 2.0 * height
            })
            .collect()
    };
    let (sender, receiver) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(sender);
    ui.set_file_viewport_y(0.0);
    request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
    let first = receiver.try_iter().collect::<Vec<_>>();
    let ids = |requests: &[IconRequest]| {
        requests
            .iter()
            .map(|request| match request.target {
                IconTarget::Entry(EntryId(id)) => id,
                IconTarget::Location => panic!("row requests must retain an entry identity"),
            })
            .collect::<Vec<_>>()
    };
    let first_ids = ids(&first);
    assert_eq!(first_ids, expected_at(0.0));
    assert!(first_ids.len() > 1 && first_ids.len() < 1000);
    assert!(
        first
            .iter()
            .all(|request| !request.thumbnail && request.type_key.is_none())
    );

    let scroll = height * 4.0 + header_height;
    ui.set_file_viewport_y(-scroll);
    request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
    let scrolled = receiver.try_iter().collect::<Vec<_>>();
    assert_eq!(ids(&scrolled), expected_at(scroll));
    assert!(!scrolled.is_empty());
    assert!(ids(&scrolled).iter().all(|id| !first_ids.contains(id)));
    request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
    assert!(receiver.try_recv().is_err());
}

#[test]
fn issue_99_ordinary_path_requests_are_deduplicated_and_stale_work_is_cleared() {
    let state = issue_98_state();
    let entry = focus_entry(1, r"C:\grid\app.exe");
    let request = {
        let mut app = state.lock().unwrap();
        app.tab_mut(TabId(1))
            .unwrap()
            .replace_entries(vec![entry.clone()]);
        let request = ordinary_icon_request(&mut app, TabId(1), RequestId(12), &entry).unwrap();
        assert!(ordinary_icon_request(&mut app, TabId(1), RequestId(12), &entry).is_none());
        advance_test_navigation(app.tab_mut(TabId(1)).unwrap(), RequestId(13));
        request
    };
    let (sender, _) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(sender);
    let (events, receiver) = mpsc::sync_channel(THUMBNAIL_QUEUE_CAPACITY);
    run_icon_request(request, None, &state, &events, &scheduler);
    assert!(state.lock().unwrap().ordinary_icon_requests.is_empty());
    assert!(receiver.try_recv().is_err());
    let mut app = state.lock().unwrap();
    app.image_cache
        .ordinary
        .insert(entry.path.clone(), test_image(32));
    assert!(ordinary_icon_request(&mut app, TabId(1), RequestId(13), &entry).is_none());
}

fn issue_98_icon_event(path: &str) -> IconEvent {
    IconEvent {
        tab_id: TabId(1),
        request_id: RequestId(12),
        target: IconTarget::Entry(EntryId(1)),
        path: PathBuf::from(path),
        icon: test_image(128),
        actual_thumbnail: false,
        requested_px: 128,
        type_key: None,
        thumbnail_failed: false,
        thumbnail_plan: None,
    }
}

fn issue_98_state() -> SharedSessions {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
    let tab = app.tab_mut(TabId(1)).unwrap();
    advance_test_navigation(tab, RequestId(12));
    tab.replace_entries(vec![focus_entry(1, r"C:\grid\model.mtl")]);
    for extension in ["png", "mtl"] {
        app.thumbnail_types.insert(
            platform::windows::shell_icons::ShellTypeIconKey::Extension(extension.into()),
            true,
        );
    }
    Arc::new(Mutex::new(app))
}

#[test]
fn issue_155_completed_rename_refresh_requests_new_path_and_restores_thumbnail() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    let state = issue_98_state();
    let sessions = WindowSessions::new(state.clone(), WindowId(1));
    let (sender, _) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(sender);
    let old_path = PathBuf::from(r"C:\grid\before.png");
    let new_path = PathBuf::from(r"C:\grid\after.png");
    {
        let mut app = state.lock().unwrap();
        app.update_directory_preference(PathBuf::from(r"C:\grid"), |view| {
            view.view_mode = ViewMode::MediumIcons
        });
        app.image_cache
            .thumbnails
            .insert((old_path, 48), test_image(48));
        let tab = app.tab_mut(TabId(1)).unwrap();
        advance_test_navigation(tab, RequestId(13));
        tab.load_state = LoadState::Loading;
    }
    assert!(
        apply_event(
            &state,
            DirectoryEvent::Batch {
                acknowledgement: mpsc::channel().0,
                tab_id: TabId(1),
                request_id: RequestId(13),
                entries: vec![focus_entry(8, new_path.to_str().unwrap())],
            }
        )
        .accepted
    );
    assert!(
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: TabId(1),
                request_id: RequestId(13),
                path: PathBuf::from(r"C:\grid"),
                skipped: 0,
                source_failures: 0,
                library: None,
            }
        )
        .accepted
    );
    refresh_ui(&ui, &sessions);
    let first = || {
        ui.get_grid_rows()
            .row_data(0)
            .unwrap()
            .entries
            .row_data(0)
            .unwrap()
    };
    assert_eq!(first().icon.size().width, 0);
    request_completed_file_images(&ui, &state, TabId(1), &scheduler);
    let key = scheduler
        .shared
        .0
        .lock()
        .unwrap()
        .plans
        .next_any()
        .unwrap()
        .1;
    assert_eq!(key.path, new_path);
    assert_eq!(key.request_id, RequestId(13));
    assert_eq!(key.entry_id, EntryId(8));
    let stale = IconEvent {
        request_id: RequestId(12),
        ..issue_98_icon_event(r"C:\grid\before.png")
    };
    assert!(apply_icon_event(&state, stale).is_none());
    let event = IconEvent {
        tab_id: key.tab_id,
        request_id: key.request_id,
        target: IconTarget::Entry(key.entry_id),
        path: key.path,
        icon: test_image(key.requested_px),
        actual_thumbnail: true,
        requested_px: key.requested_px,
        type_key: None,
        thumbnail_failed: false,
        thumbnail_plan: None,
    };
    update_icon_row(&ui, &state, apply_icon_event(&state, event).unwrap());
    assert_eq!(first().thumbnail_state, 2);
    assert_eq!(first().icon.size().width, 48);
}

#[test]
fn issue_156_content_and_details_request_and_project_thumbnails() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in [ViewMode::Content, ViewMode::Details] {
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        let state = issue_98_state();
        let sessions = WindowSessions::new(state.clone(), WindowId(1));
        {
            let mut app = state.lock().unwrap();
            app.update_directory_preference(PathBuf::from(r"C:\grid"), |view| {
                view.view_mode = mode;
                view.group_field = GroupField::Kind;
            });
            let tab = app.tab_mut(TabId(1)).unwrap();
            tab.load_state = LoadState::Complete;
            tab.replace_entries(
                (1..=1000)
                    .map(|id| focus_entry(id, &format!(r"C:\grid\image{id:04}.png")))
                    .collect(),
            );
        }
        refresh_ui(&ui, &sessions);
        let (residents, requests) = list_icon_plan(&ui, &state, WindowId(1));
        assert!(requests.len() > 1 && requests.len() < 1000);
        let requested_px = file_thumbnail_request_px(mode, ui.window().scale_factor()).unwrap();
        assert!(
            requests
                .iter()
                .all(|request| request.thumbnail && request.requested_px == requested_px)
        );
        assert_eq!(residents.len(), requests.len());
        let request = &requests[0];
        let event = IconEvent {
            tab_id: request.tab_id,
            request_id: request.request_id,
            target: request.target,
            path: request.path.clone(),
            icon: test_image(requested_px),
            actual_thumbnail: true,
            requested_px,
            type_key: None,
            thumbnail_failed: false,
            thumbnail_plan: None,
        };
        update_icon_row(&ui, &state, apply_icon_event(&state, event).unwrap());
        let first = || ui.get_files().iter().find(|row| row.id == 1).unwrap();
        assert_eq!(first().thumbnail_state, 2);
        assert_eq!(first().icon.size().width, requested_px);
        refresh_ui(&ui, &sessions);
        assert_eq!(
            first().icon.size().width,
            requested_px,
            "full projection must retain cached thumbnails"
        );
        assert!(
            list_icon_plan(&ui, &state, WindowId(1))
                .1
                .iter()
                .all(|request| request.target != IconTarget::Entry(EntryId(1)))
        );
        ui.set_file_viewport_y(-800.0);
        let (scrolled, _) = list_icon_plan(&ui, &state, WindowId(1));
        assert!(!scrolled.contains(&EntryId(1)));
    }
}

#[test]
fn issue_156_unc_uses_type_icons_and_unsupported_files_use_shell_icon() {
    let state = issue_98_state();
    let mut app = state.lock().unwrap();
    let unc = focus_entry(1, r"\\server\share\photo.png");
    let request = thumbnail_request(&mut app, TabId(1), RequestId(12), &unc, 48).unwrap();
    assert!(!request.thumbnail);
    assert!(request.type_key.is_some());
    let entry = app
        .tab(TabId(1))
        .unwrap()
        .visible_entry(EntryId(1))
        .unwrap()
        .clone();
    app.image_cache
        .grid_icons
        .insert((entry.path.clone(), 48), test_image(48));
    assert!(thumbnail_request(&mut app, TabId(1), RequestId(12), &entry, 48).is_none());
    let tab = app.tab(TabId(1)).unwrap();
    let row = file_row(&entry, tab, Texts::new(Language::Chinese), &app, Some(48));
    assert_eq!(row.thumbnail_state, 4);
    assert_eq!(row.icon.size().width, 48);
}

#[test]
fn issue_98_system_icon_is_terminal_cached_and_thumbnail_still_wins() {
    let state = issue_98_state();
    let event = issue_98_icon_event(r"C:\grid\model.mtl");
    let mut failed = event.clone();
    failed.thumbnail_failed = true;
    assert!(apply_icon_event(&state, failed).is_some());
    assert!(apply_icon_event(&state, event.clone()).is_some());
    {
        let app = state.lock().unwrap();
        assert!(app.image_cache.thumbnails.is_empty());
        assert!(app.thumbnail_failures.is_empty());
        assert_eq!(app.image_cache.grid_icons.len(), 1);
        let tab = app.active();
        let entry = tab.visible_entry(EntryId(1)).unwrap();
        let row = file_row(entry, tab, Texts::new(Language::Chinese), &app, Some(128));
        assert_eq!(row.thumbnail_state, 4);
        assert_eq!(row.icon.size().width, 128);
        let larger = file_row(entry, tab, Texts::new(Language::Chinese), &app, Some(256));
        assert_eq!(larger.icon.size().width, 0);
        let other = focus_entry(2, r"C:\grid\other.mtl");
        assert_eq!(
            file_row(&other, tab, Texts::new(Language::Chinese), &app, Some(128))
                .icon
                .size()
                .width,
            0
        );
    }
    let (sender, receiver) = mpsc::sync_channel(THUMBNAIL_QUEUE_CAPACITY);
    let (requests, _) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(requests);
    // A cache hit must not access the nonexistent fixture path or report thumbnail failure.
    run_icon_request(
        IconRequest {
            tab_id: event.tab_id,
            request_id: event.request_id,
            target: event.target,
            path: event.path.clone(),
            thumbnail: true,
            requested_px: 128,
            type_key: None,
        },
        None,
        &state,
        &sender,
        &scheduler,
    );
    let cached = receiver.try_recv().unwrap();
    assert!(!cached.actual_thumbnail);
    assert!(!cached.thumbnail_failed);
    assert_eq!(cached.icon.width, 128);
    let mut thumbnail = event;
    thumbnail.actual_thumbnail = true;
    thumbnail.icon = test_image(160);
    assert!(apply_icon_event(&state, thumbnail).is_some());
    let app = state.lock().unwrap();
    let tab = app.active();
    let row = file_row(
        tab.visible_entry(EntryId(1)).unwrap(),
        tab,
        Texts::new(Language::Chinese),
        &app,
        Some(128),
    );
    assert_eq!(row.thumbnail_state, 2);
    assert_eq!(row.icon.size().width, 160);
}

#[test]
fn issue_98_failed_and_stale_results_cannot_populate_image_caches() {
    let state = issue_98_state();
    let event = issue_98_icon_event(r"C:\grid\model.mtl");
    let mut stale = event.clone();
    stale.request_id = RequestId(11);
    assert!(apply_icon_event(&state, stale).is_none());
    let mut wrong_path = event.clone();
    wrong_path.path = PathBuf::from(r"C:\grid\other.mtl");
    assert!(apply_icon_event(&state, wrong_path).is_none());
    let mut failed = event.clone();
    failed.thumbnail_failed = true;
    failed.icon = test_image(0);
    assert!(apply_icon_event(&state, failed).is_some());
    {
        let mut app = state.lock().unwrap();
        assert!(app.image_cache.grid_icons.is_empty());
        assert!(app.image_cache.thumbnails.is_empty());
        let tab = app.active();
        let row = file_row(
            tab.visible_entry(EntryId(1)).unwrap(),
            tab,
            Texts::new(Language::Chinese),
            &app,
            Some(128),
        );
        assert_eq!(row.thumbnail_state, 3);
        assert_eq!(row.icon.size().width, 0);
        app.active_window_state_mut().tabs.remove(&TabId(1));
    }
    assert!(apply_icon_event(&state, event).is_none());
    assert!(state.lock().unwrap().image_cache.grid_icons.is_empty());
}

#[test]
fn grid_rows_never_reuse_a_thumbnail_smaller_than_the_active_view() {
    let path = PathBuf::from(r"C:\grid\photo.png");
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
    app.update_directory_preference(PathBuf::from(r"C:\grid"), |preference| {
        preference.view_mode = ViewMode::LargeIcons
    });
    app.image_cache.thumbnails.insert(
        (path.clone(), 100),
        platform::windows::shell_icons::ShellIconRgba {
            width: 100,
            height: 100,
            pixels: vec![0; 100 * 100 * 4].into(),
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

    app.image_cache.thumbnails.insert(
        (path, 148),
        platform::windows::shell_icons::ShellIconRgba {
            width: 148,
            height: 148,
            pixels: vec![0; 148 * 148 * 4].into(),
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
    advance_test_navigation(tab, RequestId(4));
    tab.load_state = LoadState::Loading;
    let state = Arc::new(Mutex::new(app));

    let DirectoryEventChange {
        icon_requests: requests,
        ..
    } = apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id: RequestId(4),
            entries: vec![focus_entry(1, r"C:\grid\photo.png")],
        },
    );

    assert!(requests.is_empty());
    assert_eq!(
        grid_thumbnail_request_rows(&vec![132.0; 125], -264.0, 396.0, 264.0),
        (0..=7).collect::<Vec<_>>()
    );
    assert_eq!(
        grid_thumbnail_request_rows(&vec![132.0; 125], -1320.0, 264.0, 264.0),
        (7..=14).collect::<Vec<_>>()
    );
    assert_eq!(
        grid_thumbnail_request_rows(
            &[32.0, 204.0, 204.0, 32.0, 204.0, 204.0, 204.0],
            -468.0,
            204.0,
            0.0,
        ),
        vec![3, 4]
    );
}

#[test]
fn issue_65_network_entries_request_shared_type_icons_without_thumbnails() {
    let path = PathBuf::from(r"\\server\share");
    let mut app = AppState::new_for_test(vec![path.clone()], 0, [0, 1, 2, 3]);
    let tab = app.tab_mut(TabId(1)).unwrap();
    advance_test_navigation(tab, RequestId(4));
    tab.load_state = LoadState::Loading;
    let state = Arc::new(Mutex::new(app));

    let DirectoryEventChange {
        icon_requests: batch,
        ..
    } = apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id: RequestId(4),
            entries: vec![
                focus_entry(1, r"\\server\share\model.blend"),
                focus_entry(2, r"\\server\share\mesh.FBX"),
                focus_entry(3, r"\\server\share\surface.obj"),
                focus_entry(4, r"\\server\share\README"),
            ],
        },
    );
    let DirectoryEventChange {
        icon_requests: finished,
        ..
    } = apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id: TabId(1),
            request_id: RequestId(4),
            path,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );

    assert_eq!(batch.len(), 4);
    assert!(batch.iter().all(|request| !request.thumbnail));
    assert!(batch.iter().all(|request| request.type_key.is_some()));
    assert_eq!(
        batch[0].type_key,
        entry_type_icon_key(
            Path::new(r"C:\local\model.BLEND"),
            crate::domain::EntryKind::File,
        )
    );
    assert_eq!(
        batch[3].type_key,
        Some(platform::windows::shell_icons::ShellTypeIconKey::GenericFile)
    );
    assert!(
        batch
            .iter()
            .all(|request| !crate::network::is_unc_path(&request.path))
    );
    assert!(finished.is_empty());
}

#[test]
fn issue_65_network_type_requests_are_deduplicated_per_extension() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"\\server\share")], 0, [0, 1, 2, 3]);
    let tab = app.tab_mut(TabId(1)).unwrap();
    advance_test_navigation(tab, RequestId(8));
    tab.load_state = LoadState::Loading;
    let state = Arc::new(Mutex::new(app));

    let DirectoryEventChange {
        icon_requests: requests,
        ..
    } = apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id: RequestId(8),
            entries: vec![
                focus_entry(1, r"\\server\share\one.obj"),
                focus_entry(2, r"\\server\share\two.OBJ"),
            ],
        },
    );

    assert_eq!(requests.len(), 1);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .tab(TabId(1))
            .unwrap()
            .pending_entries()
            .len(),
        2
    );
}

#[test]
fn issue_65_type_icon_events_keep_request_and_entry_identity() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"\\server\share")], 0, [0, 1, 2, 3]);
    let tab = app.tab_mut(TabId(1)).unwrap();
    advance_test_navigation(tab, RequestId(12));
    tab.replace_entries(vec![focus_entry(1, r"\\server\share\model.obj")]);
    let key = entry_type_icon_key(
        Path::new(r"\\server\share\model.obj"),
        crate::domain::EntryKind::File,
    )
    .unwrap();
    let event = IconEvent {
        tab_id: TabId(1),
        request_id: RequestId(12),
        target: IconTarget::Entry(EntryId(1)),
        path: key.query_path(),
        icon: platform::windows::shell_icons::ShellIconRgba {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 0].into(),
        },
        actual_thumbnail: false,
        requested_px: 0,
        type_key: Some(key),
        thumbnail_failed: false,
        thumbnail_plan: None,
    };

    assert!(icon_event_is_current(&app, &event));
    let mut stale_request = event.clone();
    stale_request.request_id = RequestId(11);
    assert!(!icon_event_is_current(&app, &stale_request));
    let mut wrong_entry = event;
    wrong_entry.target = IconTarget::Entry(EntryId(2));
    assert!(!icon_event_is_current(&app, &wrong_entry));
    app.active_window_state_mut().tabs.remove(&TabId(1));
    assert!(!icon_event_is_current(&app, &stale_request));
}

#[test]
fn issue_135_fast_failures_notify_once_and_new_execution_can_notify_again() {
    for terminal_state in [OperationState::Failed, OperationState::PartiallyCompleted] {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        let id = app.operations.submit(
            OperationResource::Network,
            FileOperationKind::Copy,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from(r"\\server\share\file")),
                Some(PathBuf::from("C:/test/file")),
            )],
        );
        let mut shown = HashSet::new();
        assert!(pending_operation_window_notifications(&app, &mut shown).is_empty());
        app.operations
            .start_next(OperationResource::Network)
            .unwrap();
        app.operations.mark_running(id).unwrap();
        assert!(pending_operation_window_notifications(&app, &mut shown).is_empty());
        app.operations.task_mut(id).unwrap().state = terminal_state;

        let pending = pending_operation_window_notifications(&app, &mut shown);
        assert_eq!(pending.len(), 1);
        shown.extend(pending);
        assert!(pending_operation_window_notifications(&app, &mut shown).is_empty());

        // A retry can fail between timer ticks, so the execution round must distinguish it.
        app.operations.task_mut(id).unwrap().execution_round += 1;
        let pending = pending_operation_window_notifications(&app, &mut shown);
        assert_eq!(pending.len(), 1);
        assert!(shown.is_empty());
        shown.extend(pending);
        assert!(pending_operation_window_notifications(&app, &mut shown).is_empty());
    }
}

#[test]
fn issue_135_failure_after_dismissed_progress_notifies_but_success_does_not() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("C:/test/a")),
            Some(PathBuf::from("C:/test/b")),
        )],
    );
    app.operations.start_next(OperationResource::Local).unwrap();
    app.operations.mark_running(id).unwrap();
    let task = app.operations.task_mut(id).unwrap();
    task.started_at = Instant::now() - Duration::from_secs(1);
    let mut shown = HashSet::new();
    shown.extend(pending_operation_window_notifications(
        &app,
        &mut HashSet::new(),
    ));
    assert!(pending_operation_window_notifications(&app, &mut shown).is_empty());

    app.operations.task_mut(id).unwrap().state = OperationState::Failed;
    let pending = pending_operation_window_notifications(&app, &mut shown);
    assert_eq!(pending.len(), 1);
    shown.extend(pending);
    assert!(pending_operation_window_notifications(&app, &mut shown).is_empty());

    for state in [OperationState::Completed, OperationState::Cancelled] {
        app.operations.task_mut(id).unwrap().state = state;
        assert!(pending_operation_window_notifications(&app, &mut HashSet::new()).is_empty());
    }
    let task = app.operations.task_mut(id).unwrap();
    task.kind = FileOperationKind::PermanentDelete;
    task.state = OperationState::Failed;
    assert!(pending_operation_window_notifications(&app, &mut HashSet::new()).is_empty());
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
    assert!(operation_window_notifications(&app).is_empty());

    app.operations
        .task_mut(id)
        .unwrap()
        .resolve_conflict(crate::domain::file_operations::ConflictDecision {
            action: crate::domain::file_operations::ConflictAction::Replace,
            apply_to_all: false,
        })
        .unwrap();
    assert!(!operation_window_notifications(&app).is_empty());
}

#[test]
fn issue_82_permanent_delete_never_auto_opens_operation_window() {
    for stage in [
        PermanentDeleteStage::RemovingOriginal,
        PermanentDeleteStage::SourceRemoved,
        PermanentDeleteStage::ReleasingSpace,
    ] {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        let id = app.operations.submit(
            OperationResource::Local,
            FileOperationKind::PermanentDelete,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from("C:/test/large")),
                None,
            )],
        );
        app.operations.start_next(OperationResource::Local).unwrap();
        app.operations.mark_running(id).unwrap();
        let task = app.operations.task_mut(id).unwrap();
        task.started_at = Instant::now() - Duration::from_secs(5);
        task.set_permanent_delete_stage(stage);

        assert!(operation_window_notifications(&app).is_empty());
        assert_eq!(operation_rows(&app).len(), 1);
    }
}

#[test]
fn issue_82_silent_cleanup_does_not_hide_other_slow_operations() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let cleanup_id = app.operations.submit(
        OperationResource::Cleanup,
        FileOperationKind::PermanentDelete,
        None,
        vec![OperationItem::cleanup_pending(
            PathBuf::from(r"C:\test\large"),
            PathBuf::from(r"C:\test\.asterfiles-cleanup\task\payload-0"),
            PathBuf::from(r"C:\records\task\record.afcleanup"),
        )],
    );
    app.operations
        .start_next(OperationResource::Cleanup)
        .unwrap();
    app.operations.mark_running(cleanup_id).unwrap();
    let cleanup = app.operations.task_mut(cleanup_id).unwrap();
    cleanup.started_at = Instant::now() - Duration::from_secs(5);
    cleanup.set_permanent_delete_stage(PermanentDeleteStage::ReleasingSpace);

    let copy_id = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("C:/test/source")),
            Some(PathBuf::from(r"\\server\share\target")),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    app.operations.mark_running(copy_id).unwrap();
    app.operations.task_mut(copy_id).unwrap().started_at = Instant::now() - Duration::from_secs(5);

    assert!(!operation_window_notifications(&app).is_empty());
    assert_eq!(operation_rows(&app).len(), 2);
}
#[test]
fn issue_82_restart_recovery_serializes_multiple_records() {
    let temp = std::env::temp_dir().join(format!(
        "asterfiles-issue-82-restart-queue-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _fixture_cleanup = FixtureCleanup(temp.clone());
    let records = temp.join("records");
    for name in ["first", "second"] {
        let source = temp.join(name);
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("content.txt"), b"content").unwrap();
        crate::fs::file_operations::create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &crate::domain::file_operations::CancellationToken::new(),
        )
        .unwrap();
    }
    let discovered = crate::fs::file_operations::discover_cleanup_tasks(&records).unwrap();
    let mut manager = OperationManager::new();

    let requests = recovered_cleanup_requests(&mut manager, discovered);

    assert_eq!(requests.len(), 1);
    let running = requests[0].id;
    let queued = manager
        .iter()
        .find(|task| task.id != running)
        .expect("second recovered record remains queued");
    assert_eq!(queued.state, OperationState::Queued);
    assert_eq!(manager.queue_position(queued.id), Some(1));
    let queued_id = queued.id;
    manager
        .finish(
            running,
            OperationState::Completed,
            OperationResult {
                succeeded: Vec::new(),
                skipped: Vec::new(),
                failed: Vec::new(),
                affected_directories: Vec::new(),
            },
        )
        .unwrap();
    assert_eq!(
        manager.start_next(OperationResource::Cleanup).unwrap(),
        Some(queued_id)
    );
}
#[test]
fn issue_82_restart_recovery_discards_completed_record() {
    let temp = std::env::temp_dir().join(format!(
        "asterfiles-issue-82-restart-complete-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _fixture_cleanup = FixtureCleanup(temp.clone());
    let records = temp.join("records");
    let source = temp.join("source");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("content.txt"), b"content").unwrap();
    let record = crate::fs::file_operations::create_cleanup_task(
        std::slice::from_ref(&source),
        &records,
        &crate::domain::file_operations::CancellationToken::new(),
    )
    .unwrap();
    let mut completed = crate::fs::file_operations::retry_cleanup_task(
        &records,
        &record.task_id,
        &crate::domain::file_operations::CancellationToken::new(),
        &mut |_| {},
    )
    .unwrap();
    let task_root = records.join(&completed.task_id);
    std::fs::create_dir_all(&task_root).unwrap();
    completed.record_path = task_root.join("record-after-cleanup.afcleanup");
    std::fs::write(&completed.record_path, b"completed").unwrap();
    let mut manager = OperationManager::new();

    let requests = recovered_cleanup_requests(&mut manager, vec![completed]);

    assert!(requests.is_empty());
    assert!(manager.iter().next().is_none());
    assert!(!task_root.exists());
}
#[test]
fn issue_82_restart_recovery_runs_cleanup_without_creating_failure_rows() {
    let temp = std::env::temp_dir().join(format!(
        "asterfiles-issue-82-restart-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _fixture_cleanup = FixtureCleanup(temp.clone());
    let records = temp.join("records");
    let sources = [temp.join("first"), temp.join("second")];
    for source in &sources {
        std::fs::create_dir_all(source).unwrap();
        std::fs::write(source.join("content.txt"), b"content").unwrap();
    }
    crate::fs::file_operations::create_cleanup_task(
        &sources,
        &records,
        &crate::domain::file_operations::CancellationToken::new(),
    )
    .unwrap();
    let discovered = crate::fs::file_operations::discover_cleanup_tasks(&records).unwrap();
    let mut manager = OperationManager::new();

    let requests = recovered_cleanup_requests(&mut manager, discovered);

    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].resource, OperationResource::Cleanup);
    assert_eq!(requests[0].items.len(), 2);
    let task = manager.task(requests[0].id).unwrap();
    assert_eq!(task.state, OperationState::Running);
    assert!(
        task.items
            .iter()
            .all(|item| { item.state == ItemState::Pending && item.error.is_none() })
    );
    assert!(task.result.is_none());
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    app.operations = manager;
    assert!(operation_rows(&app).is_empty());

    let (sender, receiver) = mpsc::channel();
    execute_cleanup_request(requests.into_iter().next().unwrap(), &sender);
    assert!(matches!(
        receiver.try_iter().last(),
        Some(FileOperationEvent::Finished { result, .. }) if result.failed.is_empty()
    ));
    assert!(
        crate::fs::file_operations::discover_cleanup_tasks(&records)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn issue_82_task_center_distinguishes_source_removal_from_space_release() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Cleanup,
        FileOperationKind::PermanentDelete,
        None,
        vec![OperationItem::cleanup_pending(
            PathBuf::from(r"C:\test\large"),
            PathBuf::from(r"C:\test\.asterfiles-cleanup\task\payload-0"),
            PathBuf::from(r"C:\records\task\record.afcleanup"),
        )],
    );
    app.operations
        .start_next(OperationResource::Cleanup)
        .unwrap();
    app.operations.mark_running(id).unwrap();
    app.operations
        .task_mut(id)
        .unwrap()
        .set_permanent_delete_stage(PermanentDeleteStage::ReleasingSpace);

    let row = operation_rows(&app).remove(0);
    assert_eq!(row.status.as_str(), "已从原位置移除，正在释放空间");
    assert!(row.destination_line.as_str().starts_with("遗留位置："));
    assert_eq!(row.file_progress.as_str(), "已处理 0 项");
    assert!(!row.can_pause);
    assert!(!row.can_cancel);
}

#[test]
fn issue_82_cleanup_completion_receipt_is_visible_without_controls() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Cleanup,
        FileOperationKind::PermanentDelete,
        None,
        vec![OperationItem::cleanup_pending(
            PathBuf::from(r"C:\test\large"),
            PathBuf::from(r"C:\test\.asterfiles-cleanup\task\payload-0"),
            PathBuf::from(r"C:\records\task\record.afcleanup"),
        )],
    );
    app.operations
        .start_next(OperationResource::Cleanup)
        .unwrap();
    app.operations.mark_running(id).unwrap();
    app.operations
        .finish(
            id,
            OperationState::Completed,
            OperationResult {
                succeeded: vec![PathBuf::from(r"C:\test\large")],
                skipped: vec![],
                failed: vec![],
                affected_directories: vec![],
            },
        )
        .unwrap();

    let row = operation_rows(&app).remove(0);
    assert_eq!(row.status.as_str(), "空间已释放");
    assert_eq!(row.file_progress.as_str(), "空间已释放");
    assert!(row.destination_line.is_empty());
    assert!(row.progress_known);
    assert_eq!(row.progress, 1.0);
    assert!(!row.can_pause);
    assert!(!row.can_cancel);
    assert_eq!(
        app.operations
            .prune_transient_with_retained_cleanup(Duration::ZERO, Duration::from_secs(10),),
        0
    );
}
#[test]
fn issue_82_fast_remove_candidates_exclude_files_links_and_protected_roots() {
    let temp = std::env::temp_dir().join(format!(
        "asterfiles-issue-82-candidate-{}",
        std::process::id()
    ));
    let _fixture_cleanup = FixtureCleanup(temp.clone());
    std::fs::create_dir_all(&temp).unwrap();
    let directory = temp.join("directory");
    let file = temp.join("file.txt");
    std::fs::create_dir(&directory).unwrap();
    std::fs::write(&file, b"x").unwrap();
    assert!(is_fast_remove_candidate(&directory));
    assert!(!is_fast_remove_candidate(&file));
    assert!(!is_fast_remove_candidate(Path::new(r"C:\")));
    assert!(!is_fast_remove_candidate(Path::new(
        r"\\server\share\folder"
    )));
}
#[test]
fn issue_76_operation_window_closes_only_after_successful_rows_are_cleared() {
    for (kind, item_count) in [
        (FileOperationKind::RecycleDelete, 1),
        (FileOperationKind::RecycleDelete, 3),
        (FileOperationKind::PermanentDelete, 1),
        (FileOperationKind::PermanentDelete, 3),
    ] {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        let items = (0..item_count)
            .map(|index| {
                OperationItem::pending(Some(PathBuf::from(format!("C:/test/item-{index}"))), None)
            })
            .collect::<Vec<_>>();
        let id = app
            .operations
            .submit(OperationResource::Local, kind, None, items);
        app.operations.start_next(OperationResource::Local).unwrap();
        app.operations.mark_running(id).unwrap();

        assert!(!should_close_operation_window(&app));

        app.operations
            .finish(
                id,
                OperationState::Completed,
                OperationResult {
                    succeeded: vec![],
                    skipped: vec![],
                    failed: vec![],
                    affected_directories: vec![],
                },
            )
            .unwrap();
        assert!(operation_rows(&app).is_empty());
        assert_eq!(app.operations.prune_transient(Duration::ZERO), 1);
        assert!(should_close_operation_window(&app));
    }
}

#[test]
fn issue_111_conflict_window_tracks_its_task_after_cancellation() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let waiting = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::Copy,
        None,
        vec![],
    );
    app.operations.task_mut(waiting).unwrap().state = OperationState::WaitingConflict;
    let other = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![],
    );
    app.operations.cancel(other).unwrap();
    let waiting_id = waiting.0.to_string();
    assert!(!should_close_conflict_window(&waiting_id, &app));
    assert!(should_close_conflict_window(&other.0.to_string(), &app));

    app.operations.cancel(waiting).unwrap();
    assert!(should_close_conflict_window(&waiting_id, &app));
    app.operations.task_mut(waiting).unwrap().state = OperationState::Cancelled;
    assert!(should_close_conflict_window(&waiting_id, &app));
    assert!(should_close_conflict_window("999", &app));
    assert!(!should_close_conflict_window("", &app));
}

#[test]
fn issue_76_operation_window_keeps_active_and_attention_rows() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::RecycleDelete,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("C:/test/running")),
            None,
        )],
    );
    assert!(!should_close_operation_window(&app));
    app.operations.start_next(OperationResource::Local).unwrap();
    app.operations.mark_running(id).unwrap();
    assert!(!should_close_operation_window(&app));
    app.operations.task_mut(id).unwrap().items[0].state = ItemState::Failed;
    app.operations
        .finish(
            id,
            OperationState::Failed,
            OperationResult {
                succeeded: vec![],
                skipped: vec![],
                failed: vec![(PathBuf::from("C:/test/running"), "locked".into())],
                affected_directories: vec![],
            },
        )
        .unwrap();

    assert_eq!(app.operations.prune_transient(Duration::ZERO), 0);
    assert!(!operation_rows(&app).is_empty());
    assert!(!should_close_operation_window(&app));
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
fn issue_62_operation_rows_prioritize_live_work_and_localize_states() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let failed = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("old")),
            Some(PathBuf::from("target")),
        )],
    );
    app.operations.start_next(OperationResource::Local).unwrap();
    app.operations.mark_running(failed).unwrap();
    app.operations.task_mut(failed).unwrap().items[0].state = ItemState::Failed;
    app.operations.task_mut(failed).unwrap().items[0].error = Some("locked".to_owned());
    app.operations
        .finish(
            failed,
            OperationState::Failed,
            OperationResult {
                succeeded: vec![],
                skipped: vec![],
                failed: vec![(PathBuf::from("old"), "locked".to_owned())],
                affected_directories: vec![],
            },
        )
        .unwrap();
    let running = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("new")),
            Some(PathBuf::from("remote")),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    app.operations.mark_running(running).unwrap();
    let queued = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Move,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("later")),
            Some(PathBuf::from("remote")),
        )],
    );

    let rows = operation_rows(&app);
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![running.0 as i32, queued.0 as i32, failed.0 as i32]
    );
    assert_eq!(rows[0].status.as_str(), "正在运行");
    assert_eq!(rows[1].queue.as_str(), "队列第 1 项");
    assert!(rows[2].failure_summary.as_str().contains("locked"));
    assert!(rows[2].can_retry);

    app.language = Language::English;
    let english = operation_rows(&app);
    assert_eq!(english[0].status.as_str(), "Running");
    assert_eq!(english[1].queue.as_str(), "Queue position 1");
}

#[test]
fn issue_136_pause_ack_refreshes_only_when_displayed_state_changes() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("a")),
            Some(PathBuf::from("b")),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    app.operations.mark_running(id).unwrap();
    assert!(!operation_pause_ack_changed(
        &app,
        [(id, false)].into_iter()
    ));

    app.operations.set_paused(id, true).unwrap();
    assert!(!operation_pause_ack_changed(&app, [(id, true)].into_iter()));
    app.operations
        .task(id)
        .unwrap()
        .cancellation
        .acknowledge_pause();
    assert!(operation_pause_ack_changed(&app, [(id, true)].into_iter()));
    assert!(!operation_pause_ack_changed(
        &app,
        [(id, false)].into_iter()
    ));
    assert!(!operation_pause_ack_changed(&app, [].into_iter()));

    app.operations.set_paused(id, false).unwrap();
    assert!(!operation_pause_ack_changed(
        &app,
        [(id, false)].into_iter()
    ));
}

#[test]
fn issue_136_copy_projection_reports_transfer_before_total_and_pause_ack() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("a")),
            Some(PathBuf::from("b")),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    let preparing = operation_rows(&app);
    assert_eq!(preparing[0].file_progress.as_str(), "正在准备");
    app.operations.mark_running(id).unwrap();
    let task = app.operations.task_mut(id).unwrap();
    task.progress.processed_bytes = 2 * 1_048_576;
    task.progress.recent_speed_bps = Some(1_048_576);
    let row = operation_rows(&app).remove(0);
    assert!(!row.progress_known);
    assert_eq!(row.transferred.as_str(), "已复制 2.0 MB");
    assert_eq!(row.speed.as_str(), "1.0 MB/s");
    assert_eq!(row.eta.as_str(), "总大小统计中");
    assert_eq!(row.file_progress.as_str(), "正在复制 · 已完成 0 个文件");

    app.language = Language::English;
    let row = operation_rows(&app).remove(0);
    assert_eq!(row.transferred.as_str(), "2.0 MB copied");
    assert_eq!(row.eta.as_str(), "Calculating total size");
    assert_eq!(row.file_progress.as_str(), "Copying · 0 files completed");

    app.language = Language::Chinese;
    let task = app.operations.task_mut(id).unwrap();
    task.state = OperationState::Paused;
    task.cancellation.pause();
    let row = operation_rows(&app).remove(0);
    assert!(row.pause_pending);
    assert_eq!(row.status.as_str(), "正在暂停");
    assert_eq!(row.speed.as_str(), "正在暂停");
    app.operations
        .task(id)
        .unwrap()
        .cancellation
        .acknowledge_pause();
    let row = operation_rows(&app).remove(0);
    assert!(!row.pause_pending);
    assert_eq!(row.status.as_str(), "已暂停");
    assert_eq!(row.speed.as_str(), "已暂停");

    let task = app.operations.task_mut(id).unwrap();
    task.state = OperationState::Running;
    task.cancellation.resume();
    task.progress.scanning_complete = true;
    task.progress.total_bytes = Some(10 * 1_048_576);
    task.progress.total_files = Some(3);
    let row = operation_rows(&app).remove(0);
    assert!(row.progress_known);
    assert_eq!(row.percent.as_str(), "20%");
    assert_eq!(row.speed.as_str(), "1.0 MB/s");
    assert_eq!(row.eta.as_str(), "预计剩余 8 秒");
    assert_eq!(row.file_progress.as_str(), "0 / 3");
}

#[test]
fn issue_137_network_wait_projection_preserves_pause_priority() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some("source".into()),
            Some("target".into()),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    app.operations.mark_running(id).unwrap();
    app.operations
        .task_mut(id)
        .unwrap()
        .progress
        .recent_speed_bps = Some(0);
    let row = operation_rows(&app).remove(0);
    assert_eq!(row.status.as_str(), "等待网络响应");
    assert_eq!(row.speed.as_str(), "等待网络响应");
    assert_eq!(row.eta.as_str(), "—");
    app.language = Language::English;
    assert_eq!(
        operation_rows(&app)[0].status.as_str(),
        "Waiting for network response"
    );
    app.language = Language::Chinese;
    app.operations.set_paused(id, true).unwrap();
    assert_eq!(operation_rows(&app)[0].status.as_str(), "正在暂停");
    app.operations
        .task(id)
        .unwrap()
        .cancellation
        .acknowledge_pause();
    assert_eq!(operation_rows(&app)[0].status.as_str(), "已暂停");
    app.operations.set_paused(id, false).unwrap();
    let task = app.operations.task_mut(id).unwrap();
    task.progress.recent_speed_bps = Some(0);
    task.resource = OperationResource::Local;
    assert_ne!(operation_rows(&app)[0].status.as_str(), "等待网络响应");
}

#[test]
fn issue_137_recovery_projection_preserves_pause_priority() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Network,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some("source".into()),
            Some("target".into()),
        )],
    );
    app.operations
        .start_next(OperationResource::Network)
        .unwrap();
    app.operations.mark_running(id).unwrap();
    let task = app.operations.task_mut(id).unwrap();
    task.progress.processed_bytes = 4_096;
    task.progress.total_bytes = Some(8_192);
    task.progress.total_files = Some(1);
    task.progress.scanning_complete = true;
    task.progress.recent_speed_bps = Some(0);
    task.progress.copy_recovering = true;
    let row = operation_rows(&app).remove(0);
    assert_eq!(row.status.as_str(), "正在自动恢复");
    assert_eq!(row.speed.as_str(), "正在自动恢复");
    assert_eq!(row.eta.as_str(), "—");
    assert_eq!(row.percent.as_str(), "50%");
    assert_eq!(
        app.operations.task(id).unwrap().progress.processed_bytes,
        4_096
    );
    app.language = Language::English;
    assert_eq!(
        operation_rows(&app)[0].status.as_str(),
        "Automatically recovering"
    );
    app.language = Language::Chinese;
    app.operations.set_paused(id, true).unwrap();
    let row = operation_rows(&app).remove(0);
    assert_eq!(row.status.as_str(), "正在暂停");
    assert_eq!(row.speed.as_str(), "正在暂停");
    app.operations
        .task(id)
        .unwrap()
        .cancellation
        .acknowledge_pause();
    let row = operation_rows(&app).remove(0);
    assert_eq!(row.status.as_str(), "已暂停");
    assert_eq!(row.speed.as_str(), "已暂停");
    app.operations.set_paused(id, false).unwrap();
    assert_eq!(operation_rows(&app)[0].status.as_str(), "正在自动恢复");
    let task = app.operations.task_mut(id).unwrap();
    task.progress.copy_recovering = false;
    task.progress.recent_speed_bps = Some(0);
    assert_eq!(operation_rows(&app)[0].status.as_str(), "等待网络响应");
    let task = app.operations.task_mut(id).unwrap();
    task.progress.copy_recovering = true;
    task.state = OperationState::Completed;
    assert!(operation_rows(&app).is_empty());
    let task = app.operations.task_mut(id).unwrap();
    task.state = OperationState::Running;
    task.resource = OperationResource::Local;
    assert_ne!(operation_rows(&app)[0].status.as_str(), "正在自动恢复");
}

#[test]
fn issue_62_running_eta_waits_for_recent_speed() {
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
    let task = app.operations.task_mut(id).unwrap();
    task.progress.scanning_complete = true;
    task.progress.total_bytes = Some(10_000);
    task.progress.processed_bytes = 2_000;
    let estimating = operation_rows(&app);
    assert_eq!(estimating[0].eta.as_str(), "正在估算");
    app.operations
        .task_mut(id)
        .unwrap()
        .progress
        .recent_speed_bps = Some(2_000);
    let estimated = operation_rows(&app);
    assert_eq!(estimated[0].eta.as_str(), "预计剩余 4 秒");
}

#[test]
fn issue_82_operation_window_has_one_hide_control() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(source.contains("close-requested => { root.close(); }"));
    assert!(!source.contains("show-minimize"));
    assert!(!source.contains("minimize-requested"));
    assert!(!source.contains("close-requested => { if (root.operations.length"));
    assert!(
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"))
            .contains("slint::CloseRequestResponse::HideWindow")
    );
}

#[test]
fn issue_62_showcase_covers_every_state_and_preserves_path_details() {
    for language in [Language::Chinese, Language::English] {
        let rows = debug_operation_rows(language);
        assert_eq!(rows.len(), 12);
        assert_eq!(
            rows.iter()
                .map(|row| row.state)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            10
        );
        assert!(rows.iter().all(|row| !row.source_line.is_empty()));
        assert!(rows.iter().all(|row| !row.destination_line.is_empty()));
        assert!(rows.iter().all(|row| !row.current_item_line.is_empty()));
        assert!(rows.iter().any(|row| row.can_pause));
        assert!(rows.iter().any(|row| row.can_cancel));
        assert!(rows.iter().any(|row| row.can_retry));
    }
}

#[test]
fn issue_147_showcase_exposes_demo_notice_rows_and_window_size() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = OperationWindow::new().unwrap();
    let state = Arc::new(Mutex::new(AppState::new_for_test(
        vec![PathBuf::from("C:/test")],
        0,
        [0, 1, 2, 3],
    )));
    for (language, notice) in [
        (
            Language::Chinese,
            "陈列室示例：共 12 行，含真实窗口不会保留的完成/取消状态，请滚动查看",
        ),
        (
            Language::English,
            "Showcase: 12 rows, including completed/cancelled states not retained in real use",
        ),
    ] {
        state.lock().unwrap().language = language;
        refresh_debug_operation_window(&ui, &state);
        assert!(ui.get_demo_mode());
        assert_eq!(ui.get_demo_notice(), notice);
        assert_eq!(ui.get_operations().row_count(), 12);
        let size = ui.window().size().to_logical(ui.window().scale_factor());
        assert_eq!(size, slint::LogicalSize::new(580.0, 650.0));
    }
}

#[test]
fn issue_62_real_operation_window_height_tracks_visible_rows() {
    assert_eq!(operation_window_height(0), 278.0);
    assert_eq!(operation_window_height(1), 278.0);
    assert_eq!(operation_window_height(2), 508.0);
    assert_eq!(operation_window_height(3), 650.0);
    assert_eq!(operation_window_height(12), 650.0);

    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(source.contains("visible: index + 1 < root.operations.length"));
}

#[test]
fn issue_62_progress_updates_model_until_cancel_then_ignores_late_events() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
    let id = app.operations.submit(
        OperationResource::Local,
        FileOperationKind::Copy,
        None,
        vec![OperationItem::pending(
            Some(PathBuf::from("source")),
            Some(PathBuf::from("target")),
        )],
    );
    app.operations.start_next(OperationResource::Local).unwrap();
    app.operations.mark_running(id).unwrap();

    apply_progress_event_for_test(&mut app, id, 4_096, 12);
    assert_eq!(
        app.operations.task(id).unwrap().progress.processed_bytes,
        4_096
    );
    assert_eq!(
        app.operations.task(id).unwrap().progress.discovered_files,
        12
    );

    app.operations.cancel(id).unwrap();
    apply_progress_event_for_test(&mut app, id, 8_192, 24);
    assert_eq!(
        app.operations.task(id).unwrap().progress.processed_bytes,
        4_096
    );
    assert_eq!(
        app.operations.task(id).unwrap().progress.discovered_files,
        12
    );
}

#[test]
fn issue_62_operation_rows_use_flat_single_layer_style() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(!source.contains("background: operation.prominent"));
    assert!(!source.contains(
        "border-color: VisualStyle.ce0e2e7;\n                    accessible-role: groupbox"
    ));
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
    let request = app.active().latest_request();
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
    assert_eq!(app.active().latest_request(), request);
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

#[test]
fn explorer_network_location_mutation_warning_is_explicit_in_both_languages() {
    assert_eq!(
        explorer_network_location_warning(Language::Chinese),
        "此更改会同步影响 Explorer。"
    );
    assert_eq!(
        explorer_network_location_warning(Language::English),
        "This change also affects Explorer."
    );
}
#[test]
fn explorer_network_location_snapshot_keeps_shell_identity_and_stable_id() {
    let shell_path =
        PathBuf::from(r"C:\Users\tester\AppData\Roaming\Microsoft\Windows\Network Shortcuts\NAS");
    let imported = imported_network_locations(vec![
        platform::windows::network::NetworkLocation {
            label: "家庭 NAS".into(),
            target: Some(PathBuf::from(r"\\server\share")),
            shell_path: shell_path.clone(),
            shell_identity: Some(PathBuf::from("shell:::network-location")),
        },
        platform::windows::network::NetworkLocation {
            label: "Shell only".into(),
            target: None,
            shell_path: PathBuf::from(r"C:\Network Shortcuts\Virtual"),
            shell_identity: Some(PathBuf::from("shell:::virtual-network-location")),
        },
    ]);

    assert_eq!(imported.len(), 2);
    assert_eq!(imported[0].id, stable_network_location_id(&shell_path));
    assert_eq!(imported[0].display_name, "家庭 NAS");
    assert_eq!(
        imported[0].target,
        NetworkTarget::WindowsPath(PathBuf::from(r"\\server\share"))
    );
    assert_eq!(
        imported[1].target,
        NetworkTarget::ShellItemId(PathBuf::from("shell:::virtual-network-location"))
    );
}

#[test]
fn explorer_network_location_snapshot_drops_items_without_executable_identity() {
    let imported = imported_network_locations(vec![platform::windows::network::NetworkLocation {
        label: "Broken".into(),
        target: None,
        shell_path: PathBuf::from(r"C:\Network Shortcuts\Broken"),
        shell_identity: None,
    }]);

    assert!(imported.is_empty());
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
fn issue_73_network_location_sidebar_identity_resolves_to_navigation_target() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
    let target = NetworkTarget::WindowsPath(PathBuf::from(r"\\server\share"));
    app.network_locations = vec![NetworkLocation {
        id: 73,
        source: NetworkLocationSource::AsterOwned,
        display_name: "NAS".into(),
        sort_order: 0,
        target: target.clone(),
        shell_path: None,
    }];

    assert_eq!(
        network_location_from_stable_id(&app, "network-location:73")
            .map(|location| &location.target),
        Some(&target)
    );
    assert!(network_location_from_stable_id(&app, "73").is_none());
    assert!(network_location_from_stable_id(&app, "network-location:invalid").is_none());
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
            shell_path: None,
        },
        NetworkLocation {
            id: 2,
            source: NetworkLocationSource::WindowsImported,
            display_name: "Shell only".into(),
            sort_order: 1,
            target: NetworkTarget::ShellItemId(PathBuf::from("shell:::virtual")),
            shell_path: None,
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
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from("two"))],
        0,
        test_window_placement(160),
    );
    let first_tab = app.window(first_window).unwrap().active_tab;
    let second_tab = app.window(second_window).unwrap().active_tab;
    for tab_id in [first_tab, second_tab] {
        let tab = app.tab_mut(tab_id).unwrap();
        advance_test_navigation(tab, RequestId(7));
        tab.load_state = LoadState::Loading;
    }
    let state = Arc::new(Mutex::new(app));
    let entry = focus_entry(1, "two/item.txt");

    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
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
            source_failures: 0,
            library: None,
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
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from("two"))],
        0,
        test_window_placement(160),
    );
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
        icon: platform::windows::shell_icons::ShellIconRgba {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 0].into(),
        },
        actual_thumbnail: false,
        requested_px: 0,
        type_key: None,
        thumbnail_failed: false,
        thumbnail_plan: None,
    };
    assert!(icon_event_is_current(&app, &event));
}

#[test]
fn closed_window_rejects_late_results_while_other_window_accepts_them() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
    let first_window = app.active_window;
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from("two"))],
        0,
        test_window_placement(160),
    );
    let first_tab = app.window(first_window).unwrap().active_tab;
    let second_tab = app.window(second_window).unwrap().active_tab;
    for tab_id in [first_tab, second_tab] {
        let tab = app.tab_mut(tab_id).unwrap();
        advance_test_navigation(tab, RequestId(9));
        tab.load_state = LoadState::Loading;
    }
    app.close_window(first_window).unwrap();
    let state = Arc::new(Mutex::new(app));
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: first_tab,
            request_id: RequestId(9),
            entries: vec![focus_entry(1, "one/late.txt")],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: second_tab,
            request_id: RequestId(9),
            entries: vec![focus_entry(1, "two/current.txt")],
        },
    );
    let app = state.lock().unwrap();
    assert!(app.tab(first_tab).is_none());
    assert_eq!(app.tab(second_tab).unwrap().pending_entries().len(), 1);
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
        *tab = TabSession::new_at(
            tab.id,
            NavigationLocation::Directory(PathBuf::from("a/previous")),
        );
        complete_test_navigation(
            tab,
            NavigationLocation::Directory(PathBuf::from("a/current")),
            NavigationKind::Normal,
        );
        complete_test_navigation(
            tab,
            NavigationLocation::Directory(PathBuf::from("a/next")),
            NavigationKind::Normal,
        );
        complete_test_navigation(
            tab,
            NavigationLocation::Directory(PathBuf::from("a/current")),
            NavigationKind::Back,
        );
        tab.replace_entries(vec![focus_entry(7, "a/current/item.txt")]);
        tab.selected = vec![EntryId(7)].into();
        tab.focused = Some(EntryId(7));
        tab.selection_anchor = Some(EntryId(7));
        tab.sort_field = SortField::Size;
        tab.sort_direction = SortDirection::Descending;
        tab.search_query = "needle".to_owned();
        tab.search_state = SearchState::Complete;
        tab.load_state = LoadState::Complete;
    }
    let request = app.tab(tab_id).unwrap().latest_request();
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
    assert_eq!(tab.latest_request(), request);
    assert_eq!(
        tab.current_location()
            .as_ref()
            .and_then(NavigationLocation::directory_path),
        Some(Path::new("a/current"))
    );
    assert_eq!(*tab.back_history(), [PathBuf::from("a/previous")]);
    assert_eq!(*tab.forward_history(), [PathBuf::from("a/next")]);
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
        vec![NavigationLocation::Directory(PathBuf::from(r"C:\WindowB"))],
        0,
        test_window_placement(160),
    );
    let second_tab = app.window(second_window).unwrap().active_tab;
    {
        let tab = app.tab_mut(first_tab).unwrap();
        tab.replace_entries(vec![focus_entry(1, r"C:\WindowA\first.txt")]);
        tab.selected = vec![EntryId(1)].into();
    }
    {
        let tab = app.tab_mut(second_tab).unwrap();
        tab.replace_entries(vec![focus_entry(2, r"C:\WindowB\second.txt")]);
        tab.selected = vec![EntryId(2)].into();
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
    app.tab_mut(first_tab).unwrap().selected = vec![EntryId(1)].into();
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from(r"C:\WindowB"))],
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
    app.tab_mut(second_tab).unwrap().selected = vec![EntryId(2)].into();
    drop(app);

    let (origin_tab, items) = pending_permanent_delete(&second).unwrap();
    assert_eq!(origin_tab, second_tab);
    assert_eq!(
        items[0].source.as_deref(),
        Some(Path::new(r"C:\WindowB\second.txt"))
    );
}

#[test]
fn issue_82_delete_shortcut_preserves_permanent_intent() {
    assert_eq!(
        delete_shortcut_request(true, false, false, false, false, false),
        Some(false)
    );
    assert_eq!(
        delete_shortcut_request(true, false, false, false, false, true),
        Some(true)
    );
    assert_eq!(
        delete_shortcut_request(true, true, false, false, false, true),
        None
    );
    assert_eq!(
        delete_shortcut_request(true, false, false, false, true, true),
        None
    );
    assert_eq!(
        delete_shortcut_request(false, false, false, false, false, true),
        None
    );
}

#[test]
fn issue_82_context_delete_preserves_shift_intent() {
    assert_eq!(context_delete_request(6, false), Some(false));
    assert_eq!(context_delete_request(6, true), Some(true));
    assert_eq!(context_delete_request(CMD_REFRESH, true), None);
}

#[test]
fn issue_103_exit_cancels_all_windows_without_losing_session_paths() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\One")], 0, [0, 1, 2, 3]);
    app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from(r"C:\Two"))],
        0,
        test_window_placement(240),
    );
    let mut tokens = Vec::new();
    for window in app.windows.values_mut() {
        for tab in window.tabs.values_mut() {
            let path = tab.visible_path().unwrap().to_path_buf();
            let (_, cancel) = tab.begin_directory_navigation(path, NavigationKind::Refresh);
            tokens.push(cancel);
        }
    }
    for window_id in app.windows.keys().copied().collect::<Vec<_>>() {
        let (_, cancel) = app.network_discovery.entry(window_id).or_default().begin();
        tokens.push(cancel);
    }
    app.cancel_directory_work_for_exit();
    assert_eq!(app.windows.len(), 2);
    assert_eq!(tokens.len(), 4);
    assert!(
        tokens
            .iter()
            .all(|cancel| cancel.load(std::sync::atomic::Ordering::Acquire))
    );
    assert!(
        app.windows
            .values()
            .flat_map(|window| window.tabs.values())
            .all(|tab| tab.visible_path().is_some() && !tab.accepts(tab.latest_request()))
    );
}

#[test]
fn issue_150_event_pump_waits_until_callback_is_consumed_or_dropped() {
    let (queued, callbacks) = mpsc::channel();
    let (finished, completion) = mpsc::channel();
    let worker = thread::spawn(move || {
        for _ in 0..2 {
            assert!(deliver_directory_event_to_ui(
                |guard| { queued.send(guard).map_err(|_| ()) },
                || true
            ));
        }
        finished.send(()).unwrap();
    });
    let first = callbacks.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(callbacks.recv_timeout(Duration::from_millis(50)).is_err());
    drop(first);
    let second = callbacks.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(completion.recv_timeout(Duration::from_millis(50)).is_err());
    drop(second);
    completion.recv_timeout(Duration::from_secs(2)).unwrap();
    worker.join().unwrap();
    assert!(!deliver_directory_event_to_ui(
        |_guard| Err::<(), _>(()),
        || true
    ));
    let (queued, callbacks) = mpsc::channel();
    let started = Instant::now();
    assert!(!deliver_directory_event_to_ui(
        |guard| queued.send(guard).map_err(|_| ()),
        || false,
    ));
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(callbacks.recv().unwrap());
}

#[test]
fn issue_150_cancelled_and_replaced_batches_are_acknowledged_without_model_changes() {
    for cancelled in [false, true] {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
        let (request_id, _) = app
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(PathBuf::from(r"C:\first"), NavigationKind::Normal);
        if cancelled {
            app.tab_mut(TabId(1)).unwrap().cancel_pending();
        } else {
            app.tab_mut(TabId(1))
                .unwrap()
                .begin_directory_navigation(PathBuf::from(r"C:\second"), NavigationKind::Normal);
        }
        let state = Arc::new(Mutex::new(app));
        let (acknowledgement, consumed) = mpsc::channel();
        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: TabId(1),
                request_id,
                entries: vec![focus_entry(1, r"C:\first\late")],
                acknowledgement,
            },
        );
        consumed.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(state.lock().unwrap().active().pending_entries().is_empty());
    }
}

#[test]
fn issue_103_network_backpressure_waits_for_application_and_preserves_local_loading() {
    use super::directory_loading::{
        deliver_directory_batch, network_directory_request, run_directory_request,
    };

    let request = network_directory_request(r"\\server\share");
    let (sender, receiver) = mpsc::sync_channel(32);
    let worker_sender = sender.clone();
    let worker = thread::spawn(move || {
        for id in 1..=2 {
            deliver_directory_batch(
                &request,
                &worker_sender,
                vec![focus_entry(id, r"\\server\share\item")],
            )?;
        }
        Ok::<(), io::Error>(())
    });
    let first = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(receiver.recv_timeout(Duration::from_millis(60)).is_err());

    let local_path =
        std::env::temp_dir().join(format!("asterfiles-issue-103-local-{}", std::process::id()));
    std::fs::create_dir_all(&local_path).unwrap();
    let mut local = network_directory_request("");
    local.path = local_path.clone();
    local.tab_id = TabId(2);
    run_directory_request(local, &sender);
    loop {
        let event = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(event.request_identity().0, TabId(2));
        if matches!(event, DirectoryEvent::Finished { .. }) {
            break;
        }
    }
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
    advance_test_navigation(app.tab_mut(TabId(1)).unwrap(), RequestId(2));
    let state = Arc::new(Mutex::new(app));
    apply_event(&state, first);
    let second = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
    apply_event(&state, second);
    worker.join().unwrap().unwrap();
    std::fs::remove_dir(local_path).unwrap();
    assert!(state.lock().unwrap().active().pending_entries().is_empty());
}

#[test]
fn issue_103_network_batch_is_visible_before_completion_and_closed_tab_rejects_late_batch() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
    let (request_id, _) = app
        .tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"\\server\share"), NavigationKind::Normal);
    let state = Arc::new(Mutex::new(app));
    let (acknowledgement, applied) = mpsc::channel();
    apply_event(
        &state,
        DirectoryEvent::Batch {
            tab_id: TabId(1),
            request_id,
            entries: vec![focus_entry(1, r"\\server\share\first")],
            acknowledgement,
        },
    );
    applied.recv_timeout(Duration::from_secs(1)).unwrap();
    {
        let mut app = state.lock().unwrap();
        let tab = app.tab(TabId(1)).unwrap();
        assert_eq!(tab.load_state, LoadState::Partial);
        assert_eq!(tab.pending_entries().len(), 1);
        let window_id = app.active_window;
        app.close_window(window_id).unwrap();
    }
    let (acknowledgement, applied) = mpsc::channel();
    apply_event(
        &state,
        DirectoryEvent::Batch {
            tab_id: TabId(1),
            request_id,
            entries: vec![focus_entry(2, r"\\server\share\late")],
            acknowledgement,
        },
    );
    applied.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(state.lock().unwrap().tab(TabId(1)).is_none());
}

#[test]
fn issue_103_cancelled_navigation_rejects_late_batches_and_terminal_events() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\local")], 0, [0, 1, 2, 3]);
    let (request_id, cancel) = app
        .tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"\\server\share"), NavigationKind::Normal);
    app.tab_mut(TabId(1)).unwrap().cancel_pending();
    assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
    let state = Arc::new(Mutex::new(app));
    for event in [
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: TabId(1),
            request_id,
            entries: vec![focus_entry(1, r"\\server\share\late")],
        },
        DirectoryEvent::Finished {
            tab_id: TabId(1),
            request_id,
            path: PathBuf::from(r"\\server\share"),
            skipped: 0,
            source_failures: 0,
            library: None,
        },
        DirectoryEvent::Failed {
            tab_id: TabId(1),
            request_id,
            kind: io::ErrorKind::Other,
            message: "late failure".to_owned(),
        },
    ] {
        apply_event(&state, event);
        let app = state.lock().unwrap();
        assert_eq!(app.active().load_state, LoadState::Cancelled);
        assert!(app.active().pending_entries().is_empty());
        assert!(app.active().error.is_none());
    }
}

#[test]
fn issue_102_rejection_notice_is_localized_once_and_follows_origin_tab() {
    use super::file_operation_coordinator::self_containment_notice;
    use super::file_operation_worker::{ExecuteFileOperationError, SELF_CONTAINMENT_ERROR};

    for language in [Language::Chinese, Language::English] {
        for kind in [FileOperationKind::Copy, FileOperationKind::Move] {
            let mut app =
                AppState::new_for_test(vec![PathBuf::from(r"C:\Source")], 0, [0, 1, 2, 3]);
            app.language = language;
            let origin_window = app.active_window;
            let origin_tab = app.active_window_state().active_tab;
            let other_window = app.register_window(
                vec![NavigationLocation::Directory(PathBuf::from(r"C:\Other"))],
                0,
                test_window_placement(160),
            );
            app.active_window = other_window;
            let (_, error, _) = ExecuteFileOperationError::from_operation(
                crate::fs::file_operations::OperationError::SourceInsideDestination,
            )
            .into_item_result(false);
            let error = error.unwrap();
            let mut item = OperationItem::pending(Some(PathBuf::from(r"C:\Source")), None);
            item.state = ItemState::Failed;
            item.error = Some(error.clone());
            let id =
                app.operations
                    .submit(OperationResource::Local, kind, Some(origin_tab), vec![item]);
            let mut result = OperationResult {
                succeeded: vec![],
                skipped: vec![],
                failed: vec![
                    (PathBuf::from(r"C:\Source"), error.clone()),
                    (PathBuf::from(r"C:\Source2"), error),
                ],
                affected_directories: vec![],
            };
            let notice = self_containment_notice(&app, id, &result).unwrap();
            assert_eq!(notice.0, origin_window);
            assert_eq!(notice.2, Texts::new(language).self_containment_message());
            assert!(!notice.2.contains(SELF_CONTAINMENT_ERROR));
            assert!(
                operation_failure_summary(language, app.operations.task(id).unwrap())
                    .contains(notice.2)
            );
            let tab = app
                .window_mut(origin_window)
                .unwrap()
                .tabs
                .remove(&origin_tab)
                .unwrap();
            app.window_mut(other_window)
                .unwrap()
                .tabs
                .insert(origin_tab, tab);
            assert_eq!(
                self_containment_notice(&app, id, &result).unwrap().0,
                other_window
            );
            result.failed.clear();
            assert!(self_containment_notice(&app, id, &result).is_none());
            result
                .failed
                .push((PathBuf::new(), "some other error".to_owned()));
            assert!(self_containment_notice(&app, id, &result).is_none());
            result.failed[0].1 = SELF_CONTAINMENT_ERROR.to_owned();
            app.window_mut(other_window)
                .unwrap()
                .tabs
                .remove(&origin_tab);
            assert!(self_containment_notice(&app, id, &result).is_none());
        }
    }
}

#[test]
fn issue_102_drop_preparation_defers_copy_move_rejection_to_worker() {
    use platform::windows::drag_drop::{
        ALLOW_COPY, ALLOW_LINK, ALLOW_MOVE, DropEffect, DropIntent, DropTarget,
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("artifacts")
        .join(format!("issue-102-drop-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let _cleanup = FixtureCleanup(root.clone());
    let source = root.join("source");
    let target = source.join("child");
    std::fs::create_dir_all(&target).unwrap();
    for effect in [DropEffect::Copy, DropEffect::Move, DropEffect::Link] {
        let prepared = prepare_drop_operation(DropIntent {
            paths: vec![source.clone()],
            target: DropTarget::Directory(target.clone()),
            effect,
            right_button: false,
            screen_x: 0,
            screen_y: 0,
            allowed_effects: ALLOW_COPY | ALLOW_MOVE | ALLOW_LINK,
            staging_root: None,
        });
        if effect == DropEffect::Link {
            assert!(prepared.is_err());
        } else {
            let PreparedDrop::Operation(_, items) = prepared.unwrap() else {
                panic!("expected operation")
            };
            assert_eq!(items[0].source.as_ref(), Some(&source));
            assert_eq!(items[0].destination.as_ref(), Some(&target.join("source")));
            assert!(!target.join("source").exists());
        }
    }
}

#[test]
fn delayed_paste_keeps_its_original_tab() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\WindowA")], 0, [0, 1, 2, 3]);
    let first_window = app.active_window;
    let first_tab = app.window(first_window).unwrap().active_tab;
    let second_window = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from(r"C:\WindowB"))],
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
        let _ = enqueue_operation(&shared, &sender, origin_tab, kind, items, None);
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
    let second = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from("b"))],
        0,
        test_window_placement(160),
    );
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
    let second = app.register_window(
        vec![NavigationLocation::Directory(PathBuf::from("b"))],
        0,
        test_window_placement(160),
    );
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
        app.stable_active_location_index(),
        app.stable_locations(),
    )
    .unwrap();
    session_store::save(&temporary, &session).unwrap();
    let restored = session_store::load(&temporary).unwrap();
    std::fs::remove_file(temporary).unwrap();
    assert_eq!(
        restored.windows[0].tab_locations,
        [PathBuf::from("b"), PathBuf::from("c"), PathBuf::from("a")]
    );
    assert_eq!(restored.windows[0].active_tab, 0);
}

#[test]
fn issue_71_ctrl_w_routes_close_and_exit_independently_of_address_focus() {
    assert_eq!(ctrl_w_action(true), CtrlWAction::CloseActiveTab);
    assert_eq!(ctrl_w_action(false), CtrlWAction::RequestWindowClose);

    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"));
    let shortcut = source
        .split_once("value.eq_ignore_ascii_case(\"w\")")
        .expect("window Ctrl+W route exists")
        .1
        .split_once("_ => false")
        .expect("window Ctrl+W route has a match boundary")
        .0;
    assert!(shortcut.contains("ui.invoke_request_close()"));
    assert!(!shortcut.contains("editing_address"));
}

#[test]
fn issue_71_ctrl_n_uses_the_new_window_route() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/app.rs"));
    let shortcut = source
        .split_once("value.eq_ignore_ascii_case(\"n\")")
        .expect("window Ctrl+N route exists")
        .1
        .split_once("_ if control")
        .expect("window Ctrl+N route has a match boundary")
        .0;
    assert!(shortcut.contains("create_new_window("));
    assert!(!shortcut.contains("ui.invoke_new_tab()"));
}

#[test]
fn issue_71_new_window_uses_only_a_stable_active_directory() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
    let source = app.active_window;
    app.tab_mut(TabId(1)).unwrap().load_state = LoadState::Complete;
    assert_eq!(
        new_window_location(&app, source),
        NavigationLocation::Directory(PathBuf::from("one"))
    );

    app.tab_mut(TabId(1)).unwrap().load_state = LoadState::Failed;
    assert_eq!(
        new_window_location(&app, source),
        NavigationLocation::Directory(initial_path())
    );

    app.open_settings();
    assert_eq!(
        new_window_location(&app, source),
        NavigationLocation::Directory(initial_path())
    );
}

#[test]
fn issue_71_closing_active_tab_selects_right_neighbor_and_rejects_late_results() {
    let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
    let closing = app.create_tab(PathBuf::from("two"));
    let right = app.create_tab(PathBuf::from("three"));
    app.active_window_state_mut().active_tab = closing;
    let (request_id, cancel) = app
        .active_window_state_mut()
        .tabs
        .get_mut(&closing)
        .unwrap()
        .begin_directory_navigation(PathBuf::from("two/pending"), NavigationKind::Refresh);

    assert_eq!(app.close_tab(closing), Some(right));
    assert!(cancel.load(std::sync::atomic::Ordering::Acquire));
    assert_eq!(app.active_window_state().tab_order, [TabId(1), right]);

    let state = Arc::new(Mutex::new(app));
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id: closing,
            request_id,
            entries: vec![focus_entry(1, "two/late.txt")],
        },
    );
    let app = state.lock().unwrap();
    assert!(app.tab(closing).is_none());
    assert!(app.tab(right).unwrap().pending_entries().is_empty());
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
        None,
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
        None,
    ));
    assert!(receiver.try_recv().is_err());
    let app = state.lock().expect("app state mutex is not poisoned");
    assert_eq!(app.active().latest_request(), RequestId(0));
    assert!(app.active().back_history().is_empty());
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
            None,
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
    let request = app.tab(tab_a).unwrap().latest_request();
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
    assert_eq!(app.tab(tab_a).unwrap().latest_request(), request);
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
        everything_sort(SortField::Kind, SortDirection::Ascending),
        EverythingSort::PathAscending
    );
    assert_eq!(
        everything_sort(SortField::Kind, SortDirection::Descending),
        EverythingSort::PathDescending
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
    advance_test_navigation(tab, RequestId(7));
    tab.replace_entries(vec![FileEntry {
        id: EntryId(3),
        original_name: "file.txt".into(),
        display_name: "file.txt".into(),
        name_highlights: Vec::new(),
        path: PathBuf::from("same/file.txt"),
        kind: crate::domain::EntryKind::File,
        open_target: None,
        library_source_index: None,
        parent_display: "same".into(),
        size_bytes: Some(1),
        folder_size: crate::domain::FolderSizeState::Unknown,
        modified: None,
        created: None,
        webdav: None,
    }]);
    let event = IconEvent {
        tab_id: TabId(1),
        request_id: RequestId(7),
        target: IconTarget::Entry(EntryId(3)),
        path: PathBuf::from("same/file.txt"),
        icon: platform::windows::shell_icons::ShellIconRgba {
            width: 1,
            height: 1,
            pixels: vec![0, 0, 0, 0].into(),
        },
        actual_thumbnail: false,
        requested_px: 0,
        type_key: None,
        thumbnail_failed: false,
        thumbnail_plan: None,
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
        type_key: None,
        thumbnail_failed: false,
        thumbnail_plan: None,
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
    advance_test_navigation(tab, RequestId(8));
    tab.page_source = PageSource::Directory;
    tab.replace_entries(vec![FileEntry {
        id: EntryId(1),
        path: PathBuf::from(r"C:\current\same-id"),
        original_name: "same-id".into(),
        display_name: "same-id".into(),
        name_highlights: Vec::new(),
        kind: crate::domain::EntryKind::Directory,
        open_target: None,
        library_source_index: None,
        parent_display: r"C:\current".into(),
        size_bytes: None,
        folder_size: FolderSizeState::Querying,
        modified: None,
        created: None,
        webdav: None,
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
        library_source_index: None,
        parent_display: String::new(),
        size_bytes: Some(1),
        folder_size: crate::domain::FolderSizeState::Unknown,
        modified: None,
        created: None,
        webdav: None,
    }
}

#[test]
fn issue_59_menu_is_first_and_only_enabled_for_one_search_result() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\search")], 0, [0, 1, 2, 3]);
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.page_source = PageSource::Search;
    tab.replace_entries(vec![focus_entry(257, r"D:\one\same.txt")]);
    tab.selected = vec![EntryId(257)].into();

    let (rows, _) = built_in_context_rows(&app, false, false);
    assert_eq!(rows[0].id, CMD_OPEN_FILE_LOCATION);
    assert_eq!(rows[0].label.as_str(), "打开所在位置");
    assert!(rows[0].enabled);
    assert!(rows[1].separator);

    app.tab_mut(TabId(1)).unwrap().selected.clear();
    let (rows, _) = built_in_context_rows(&app, false, false);
    assert!(!rows[0].enabled);
    app.tab_mut(TabId(1)).unwrap().page_source = PageSource::Directory;
    let (rows, _) = built_in_context_rows(&app, false, false);
    assert!(rows.iter().all(|row| row.id != CMD_OPEN_FILE_LOCATION));

    app.language = Language::English;
    app.tab_mut(TabId(1)).unwrap().page_source = PageSource::Search;
    app.tab_mut(TabId(1)).unwrap().selected = vec![EntryId(257)].into();
    let (rows, _) = built_in_context_rows(&app, false, false);
    assert_eq!(rows[0].label.as_str(), "Open file location");
}

#[test]
fn issue_59_reveal_request_is_registered_before_worker_delivery() {
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\search")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let origin_tab_id = app.active_window_state().active_tab;
    let state = Arc::new(Mutex::new(app));
    let (local_sender, local_receiver) = mpsc::channel();
    let (network_sender, _network_receiver) = mpsc::sync_channel(1);
    let target = PathBuf::from(r"D:\target\item.txt");
    let parent = target.parent().unwrap().to_path_buf();

    let tab_id = submit_reveal_navigation(
        &local_sender,
        &network_sender,
        &state,
        window_id,
        origin_tab_id,
        parent.clone(),
        target.clone(),
    )
    .unwrap();
    let request = local_receiver.try_recv().expect("directory request queued");
    let app = state.lock().unwrap();
    let pending = app.focus_after_refresh.get(&tab_id).unwrap();
    assert_eq!(pending.request_id, Some(request.request_id));
    assert_eq!(pending.directory, parent);
    assert_eq!(pending.paths, vec![target]);
    assert_eq!(app.window(window_id).unwrap().active_tab, tab_id);
}
#[test]
fn issue_59_reveal_matches_the_complete_original_path() {
    let target = PathBuf::from(r"D:\second\same.txt");
    let parent = target.parent().unwrap().to_path_buf();
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\search")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let tab_id = app
        .create_tab_in_window(window_id, NavigationLocation::Directory(parent.clone()))
        .unwrap();
    let request_id = {
        let tab = app.tab_mut(tab_id).unwrap();
        tab.begin_directory_navigation(parent.clone(), NavigationKind::Refresh)
            .0
    };
    app.focus_after_refresh.insert(
        tab_id,
        PendingFocus {
            directory: parent.clone(),
            request_id: Some(request_id),
            paths: vec![target.clone()],
            action: PendingFocusAction::Reveal { window_id },
        },
    );
    let state = Arc::new(Mutex::new(app));
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id,
            request_id,
            entries: vec![
                focus_entry(1, r"D:\first\same.txt"),
                focus_entry(2, r"D:\second\same.txt"),
            ],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path: parent,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );

    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.selected, vec![EntryId(2)]);
    assert_eq!(tab.focused, Some(EntryId(2)));
    assert_eq!(tab.selection_anchor, Some(EntryId(2)));
    assert!(!app.focus_after_refresh.contains_key(&tab_id));
}

#[test]
fn issue_59_folder_results_open_their_parent_instead_of_the_folder() {
    let target = PathBuf::from(r"D:\parent\folder");
    let parent = target.parent().unwrap().to_path_buf();
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\search")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let origin_tab_id = app.active_window_state().active_tab;
    let state = Arc::new(Mutex::new(app));
    let (local_sender, local_receiver) = mpsc::channel();
    let (network_sender, _network_receiver) = mpsc::sync_channel(1);
    let tab_id = submit_reveal_navigation(
        &local_sender,
        &network_sender,
        &state,
        window_id,
        origin_tab_id,
        parent.clone(),
        target.clone(),
    )
    .unwrap();
    let request = local_receiver.try_recv().unwrap();
    assert_eq!(request.path, parent);
    let mut folder = focus_entry(9, r"D:\parent\folder");
    folder.kind = crate::domain::EntryKind::Directory;
    apply_event(
        &state,
        DirectoryEvent::Batch {
            acknowledgement: mpsc::channel().0,
            tab_id,
            request_id: request.request_id,
            entries: vec![folder],
        },
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id: request.request_id,
            path: request.path,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert_eq!(tab.visible_path(), Some(parent.as_path()));
    assert_eq!(tab.selected, vec![EntryId(9)]);
}

#[test]
fn issue_59_missing_and_stale_targets_do_not_select_or_steal_focus() {
    let parent = PathBuf::from(r"D:\target");
    let target = parent.join("deleted.txt");
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\search")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let tab_id = app
        .create_tab_in_window(window_id, NavigationLocation::Directory(parent.clone()))
        .unwrap();
    let request_id = {
        let tab = app.tab_mut(tab_id).unwrap();
        tab.begin_directory_navigation(parent.clone(), NavigationKind::Refresh)
            .0
    };
    app.focus_after_refresh.insert(
        tab_id,
        PendingFocus {
            directory: parent.clone(),
            request_id: Some(request_id),
            paths: vec![target],
            action: PendingFocusAction::Reveal { window_id },
        },
    );
    let state = Arc::new(Mutex::new(app));
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id: RequestId(request_id.0.saturating_sub(1)),
            path: parent.clone(),
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    assert!(
        state
            .lock()
            .unwrap()
            .focus_after_refresh
            .contains_key(&tab_id)
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path: parent,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    let app = state.lock().unwrap();
    let tab = app.tab(tab_id).unwrap();
    assert!(tab.selected.is_empty());
    assert!(tab.focused.is_none());
    assert_eq!(
        status_text(tab, Texts::new(Language::Chinese)),
        "目标已被移动、重命名或删除"
    );
    assert!(!app.focus_after_refresh.contains_key(&tab_id));
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

    let mut gesture = FileDragGesture::default();
    gesture.arm(0.0, 0.0);
    gesture.bind_entry(&app, EntryId(2));
    let drag = gesture.take_native_start(&app, 5.0, 0.0).unwrap();
    assert_eq!(
        drag.entries,
        vec![DraggedEntry::Local(
            EntryId(2),
            PathBuf::from(r"C:\test\two.txt")
        )]
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

    let mut gesture = FileDragGesture::default();
    gesture.arm(0.0, 0.0);
    gesture.bind_entry(&app, EntryId(2));
    let drag = gesture.take_native_start(&app, 5.0, 0.0).unwrap();
    assert_eq!(
        drag.entries,
        vec![
            DraggedEntry::Local(EntryId(1), PathBuf::from(r"C:\test\one.txt")),
            DraggedEntry::Local(EntryId(2), PathBuf::from(r"C:\test\two.txt")),
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
            tab.begin_directory_navigation(PathBuf::from(r"C:\target"), NavigationKind::Refresh);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
        let request_id = tab.latest_request();
        tab.accept_directory_batch(
            request_id,
            vec![
                focus_entry(1, r"C:\target\first.txt"),
                focus_entry(2, r"C:\target\second.txt"),
            ],
            DirectoryViewPreference::default(),
        );
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
                source_failures: 0,
                library: None,
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
            source_failures: 0,
            library: None,
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
fn create_folder_focus_is_scoped_to_origin_tab_and_requests_rename() {
    let mut app = AppState::new_for_test(
        vec![PathBuf::from(r"C:\target"), PathBuf::from(r"C:\target")],
        0,
        [0, 1, 2, 3],
    );
    queue_completed_rename(&mut app, TabId(1), PathBuf::from(r"C:\target\New folder"));
    assert_eq!(app.focus_after_refresh.len(), 1);
    let pending = app.focus_after_refresh.get(&TabId(1)).unwrap();
    assert_eq!(
        pending.action,
        PendingFocusAction::Rename {
            window_id: app.active_window
        }
    );
    assert!(!app.focus_after_refresh.contains_key(&TabId(2)));
    app.active_window_state_mut().active_tab = TabId(2);
    assert!(!pending_focus_is_valid(
        &app,
        TabId(1),
        app.focus_after_refresh.get(&TabId(1)).unwrap()
    ));
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
fn issue_146_rename_targets_one_selected_entry_in_every_view_mode() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in 0..8 {
        let ui = AppWindow::new().unwrap();
        ui.set_view_mode(mode);
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\target")], 0, [0, 1, 2, 3]);
        let window_id = app.active_window;
        let tab_id = app.active_window_state().active_tab;
        let tab = app.tab_mut(tab_id).unwrap();
        tab.replace_entries(vec![
            focus_entry(1, r"C:\target\报告.txt"),
            focus_entry(2, r"C:\target\other.txt"),
        ]);
        tab.load_state = LoadState::Complete;
        let shared = Arc::new(Mutex::new(app));
        let state = WindowSessions::new(shared.clone(), window_id);
        for selected in [vec![], vec![EntryId(1), EntryId(2)]] {
            shared.lock().unwrap().tab_mut(tab_id).unwrap().selected = selected.into();
            begin_rename_ui(&ui.as_weak(), &state);
            assert!(!ui.get_rename_editing(), "mode {mode}");
            assert!(shared.lock().unwrap().rename_targets.is_empty());
        }
        shared.lock().unwrap().tab_mut(tab_id).unwrap().selected = vec![EntryId(1)].into();
        begin_rename_ui(&ui.as_weak(), &state);
        assert!(ui.get_rename_editing(), "mode {mode}");
        assert_eq!(ui.get_rename_entry_id(), 1);
        assert_eq!(ui.get_rename_input().as_str(), "报告.txt");
        assert_eq!(ui.get_rename_selection_end(), 6);
        let app = shared.lock().unwrap();
        let target = app.rename_targets.get(&window_id).unwrap();
        assert_eq!(target.entry_id, EntryId(1));
        assert_eq!(target.tab.tab_id, tab_id);
        assert_eq!(
            target.tab.request_id,
            app.tab(tab_id).unwrap().latest_request()
        );
    }
}

#[test]
fn issue_154_rename_is_dismissed_when_target_disappears_or_request_changes() {
    i_slint_backend_testing::init_no_event_loop();
    for navigation in [false, true] {
        let ui = AppWindow::new().unwrap();
        let state = issue_98_state();
        let sessions = WindowSessions::new(state.clone(), WindowId(1));
        state.lock().unwrap().tab_mut(TabId(1)).unwrap().selected = vec![EntryId(1)].into();
        begin_rename_ui(&ui.as_weak(), &sessions);
        dismiss_stale_rename(&ui, &state, WindowId(1));
        assert!(ui.get_rename_editing());
        {
            let mut app = state.lock().unwrap();
            let tab = app.tab_mut(TabId(1)).unwrap();
            if navigation {
                advance_test_navigation(tab, RequestId(13));
            } else {
                tab.replace_entries(Vec::new());
            }
        }
        dismiss_stale_rename(&ui, &state, WindowId(1));
        assert!(!ui.get_rename_editing());
        assert!(!ui.get_rename_submitting());
        assert!(state.lock().unwrap().rename_targets.is_empty());
    }
}
#[test]
fn rename_validation_keeps_invalid_names_out_of_operation_queue() {
    assert!(crate::fs::file_operations::validate_name(std::ffi::OsStr::new("")).is_err());
    assert!(crate::fs::file_operations::validate_name(std::ffi::OsStr::new("bad?.txt")).is_err());
    assert!(crate::fs::file_operations::validate_name(std::ffi::OsStr::new("valid.txt")).is_ok());
    assert!(!keyboard_shortcuts_suppressed(false, false));
    assert!(keyboard_shortcuts_suppressed(true, false));
}
#[test]
fn rename_input_keeps_full_name_and_selects_the_windows_stem() {
    let cases = [
        ("aaa.txt", crate::domain::EntryKind::File, "aaa.txt", 3),
        (
            "archive.tar.gz",
            crate::domain::EntryKind::File,
            "archive.tar.gz",
            11,
        ),
        (
            ".gitignore",
            crate::domain::EntryKind::File,
            ".gitignore",
            10,
        ),
        ("README", crate::domain::EntryKind::File, "README", 6),
        (
            "folder.name",
            crate::domain::EntryKind::Directory,
            "folder.name",
            11,
        ),
        ("报告.txt", crate::domain::EntryKind::File, "报告.txt", 6),
    ];
    for (name, kind, expected_input, expected_selection_end) in cases {
        let mut entry = focus_entry(1, &format!(r"C:\target\{name}"));
        entry.kind = kind;
        assert_eq!(
            rename_input_for_entry(&entry),
            (expected_input.to_owned(), expected_selection_end)
        );
    }
}
#[test]
fn rename_submission_uses_the_complete_edited_name_for_local_and_unc_paths() {
    for (directory, source, expected) in [
        (r"C:\target", r"C:\target\old.txt", r"C:\target\new.md"),
        (
            r"\\server\share",
            r"\\server\share\old.txt",
            r"\\server\share\new.md",
        ),
    ] {
        let mut app = AppState::new_for_test(vec![PathBuf::from(directory)], 0, [0, 1, 2, 3]);
        let window_id = app.active_window;
        let tab_id = app.active_window_state().active_tab;
        let tab = app.tab_mut(tab_id).unwrap();
        tab.replace_entries(vec![focus_entry(1, source)]);
        tab.load_state = LoadState::Complete;
        tab.selected = vec![EntryId(1)].into();
        let request_id = app.tab(tab_id).unwrap().latest_request();
        app.rename_targets.insert(
            window_id,
            actions::EntryTarget {
                tab: actions::ActionTarget {
                    window_id,
                    tab_id,
                    request_id,
                },
                entry_id: EntryId(1),
            },
        );
        let shared = Arc::new(Mutex::new(app));
        let state = WindowSessions::new(shared.clone(), window_id);
        let (sender, receiver) = mpsc::channel();

        submit_rename(&state, &sender, "new.md").unwrap();

        let request = receiver.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(
            request.items[0].destination.as_deref(),
            Some(Path::new(expected))
        );
    }
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
    app.everything.generation = 2;
    app.everything.config.executable_path = Some(PathBuf::from("new.exe"));
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
        app.everything.config.executable_path,
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
            library_source_index: None,
            parent_display: "C:/test".into(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
            created: None,
            webdav: None,
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
                        library_source_index: None,
                        parent_display: "C:/test".into(),
                        size_bytes: Some(1),
                        folder_size: crate::domain::FolderSizeState::Unknown,
                        modified: None,
                        created: None,
                        webdav: None,
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
            staging_root: None,
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
            staging_root: None,
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
            staging_root: None,
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
            SelectionRect::from_points(0.0, 31.0, 20.0, 33.0),
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
            SelectionRect::from_points(16.0, 31.0, 20.0, 59.0),
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
            SelectionRect::from_points(161.0, 129.0, 171.0, 139.0),
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
            SelectionRect::from_points(0.0, 256.0 * 32.0, 20.0, 257.0 * 32.0),
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
            SelectionRect::from_points(0.0, 0.0, 20.0, 32.0),
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
                group_detail: "".into(),
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

fn named_element(ui: &AppWindow, id: &str) -> Option<i_slint_backend_testing::ElementHandle> {
    use i_slint_backend_testing::ElementRoot;

    ui.root_element()
        .query_descendants()
        .match_id(id)
        .find_all()
        .into_iter()
        .next()
}

#[test]
fn issue_83_ctrl_z_routes_only_from_the_file_list_focus_domain() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(source.contains("callback request-undo();"));
    assert!(source.contains(
        "KeyBinding { keys: @keys(Control + Z); enabled: root.file-list-keyboard-target; activated => { root.request-undo(); } }"
    ));
    assert!(source.contains("&& !root.address-editing"));
    assert!(source.contains("&& !root.rename-editing"));
    assert!(source.contains("&& !root.context-menu-open"));
    assert!(source.contains("!root.active-is-settings"));
    assert!(!source.contains("root.menu-open"));
    assert!(source.contains("&& !root.tabs-menu-open"));
    assert!(source.contains("&& !root.drop-menu-open"));
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
        7
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
fn issue_176_refresh_projection_keeps_rows_selection_and_scroll() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    let path = PathBuf::from(r"C:\files");
    let mut app = AppState::new_for_test(vec![path.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    let window_id = app.active_window;
    let tab = app.tab_mut(tab_id).unwrap();
    tab.replace_entries(
        (1..=200)
            .map(|id| focus_entry(id, &format!(r"C:\files\file{id:03}.txt")))
            .collect(),
    );
    tab.load_state = LoadState::Complete;
    tab.select_entry(EntryId(80), false, false);
    let state = Arc::new(Mutex::new(app));
    refresh_window_ui(&ui, &state, window_id);
    ui.set_file_viewport_y(-1500.0);
    let (sender, receiver) = mpsc::channel();
    assert!(submit_navigation(
        &sender,
        &state,
        tab_id,
        path.clone(),
        NavigationKind::BackgroundRefresh,
        None
    ));
    let request = receiver.try_recv().unwrap();
    assert_eq!(ui.get_file_viewport_y(), -1500.0);
    assert_eq!(ui.get_files().row_count(), 200);
    assert!(!ui.get_loading_hint_visible());
    assert!(!ui.get_status_text().contains("Loading"));
    apply_event(
        &state,
        DirectoryEvent::Batch {
            tab_id,
            request_id: request.request_id,
            entries: (1..=200)
                .map(|id| focus_entry(201 - id, &format!(r"C:\files\file{id:03}.txt")))
                .collect(),
            acknowledgement: mpsc::channel().0,
        },
    );
    append_active_file_rows(&ui, &state, tab_id, request.request_id);
    assert_eq!(
        ui.get_projected_file_request_id(),
        request.request_id.0 as i32
    );
    let (_, changed) = mutate_window_selection(&state, window_id, |tab| {
        tab.select_entry(EntryId(81), false, false)
    })
    .unwrap();
    update_file_rows(&ui, &state, tab_id, &changed);
    assert!(
        ui.get_files()
            .iter()
            .any(|row| row.id == 81 && row.selected)
    );
    assert_eq!(ui.get_file_viewport_y(), -1500.0);
    assert_eq!(ui.get_files().row_count(), 200);
    assert_eq!(
        state
            .lock()
            .unwrap()
            .active()
            .visible_entry(EntryId(80))
            .unwrap()
            .path,
        path.join("file080.txt")
    );
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id: request.request_id,
            path: path.clone(),
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    refresh_window_ui(&ui, &state, window_id);
    assert_eq!(ui.get_file_viewport_y(), -1500.0);
    assert_eq!(state.lock().unwrap().active().selected, vec![EntryId(120)]);
    // A shorter snapshot clamps the old viewport; navigating away still starts at the top.
    assert!(submit_navigation(
        &sender,
        &state,
        tab_id,
        path.clone(),
        NavigationKind::BackgroundRefresh,
        None
    ));
    let request = receiver.try_recv().unwrap();
    apply_event(
        &state,
        DirectoryEvent::Finished {
            tab_id,
            request_id: request.request_id,
            path,
            skipped: 0,
            source_failures: 0,
            library: None,
        },
    );
    refresh_window_ui(&ui, &state, window_id);
    assert_eq!(ui.get_file_viewport_y(), 0.0);
    ui.set_file_viewport_y(-200.0);
    assert!(submit_navigation(
        &sender,
        &state,
        tab_id,
        PathBuf::from(r"C:\other"),
        NavigationKind::Normal,
        None
    ));
    refresh_window_ui(&ui, &state, window_id);
    assert_eq!(ui.get_file_viewport_y(), 0.0);
}

#[test]
fn issue_125_open_with_completion_keeps_list_position_and_selection() {
    use platform::windows::context_menu::{ClassicMenuInvocation, ShellMenuBackgroundTarget};

    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("headless model");
    let path = PathBuf::from(r"C:\files");
    let mut app = AppState::new_for_test(vec![path.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    let window_id = app.active_window;
    let tab = app.tab_mut(tab_id).unwrap();
    tab.replace_entries(
        (1..=200)
            .map(|id| focus_entry(id, &format!(r"C:\files\file{id:03}.txt")))
            .collect(),
    );
    tab.load_state = LoadState::Complete;
    tab.select_entry(EntryId(80), false, false);
    let request_id = tab.latest_request();
    let state = Arc::new(Mutex::new(app));
    refresh_window_ui(&ui, &state, window_id);
    ui.set_file_viewport_y(-1_500.0);
    let target = ShellMenuBackgroundTarget::FileSystem(path.clone());
    let (local, requests) = mpsc::channel();
    let (network, network_requests) = mpsc::sync_channel(1);

    for verb in [None, Some("openwith"), Some("openas"), Some("program")] {
        let invocation = ClassicMenuInvocation::Shell {
            verb: verb.map(str::to_owned),
            refresh_directory: false,
        };
        if let Some(location) =
            shell_menu_invocation_refresh_target(&state, tab_id, Some(&target), &invocation)
        {
            submit_location_navigation(
                &local,
                &network,
                &state,
                tab_id,
                location,
                NavigationKind::Refresh,
            );
        }
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        assert!(matches!(
            network_requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        refresh_window_ui(&ui, &state, window_id);
        assert_eq!(ui.get_file_viewport_y(), -1_500.0);
        let app = state.lock().unwrap();
        let tab = app.tab(tab_id).unwrap();
        assert_eq!(tab.latest_request(), request_id);
        assert_eq!(tab.selected, vec![EntryId(80)]);
        assert_eq!(tab.focused, Some(EntryId(80)));
        assert_eq!(tab.entries.len(), 200);
    }
    let changed = ClassicMenuInvocation::Shell {
        verb: None,
        refresh_directory: true,
    };
    assert_eq!(
        shell_menu_invocation_refresh_target(&state, tab_id, Some(&target), &changed),
        Some(NavigationLocation::Directory(path))
    );
    let network_target = ShellMenuBackgroundTarget::FileSystem(PathBuf::from(r"\\nas\share"));
    assert_eq!(
        shell_menu_invocation_refresh_target(&state, tab_id, Some(&network_target), &changed),
        Some(NavigationLocation::Directory(PathBuf::from(r"\\nas\share")))
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
fn issue_113_home_fills_content_to_window_bottom() {
    let ui = headless_file_view();
    ui.set_page_state(3);
    ui.set_active_is_home(true);
    update_test_layout(&ui);
    let home = named_element(&ui, "AppWindow::home-area").expect("home area exists");
    let top = home.absolute_position().y;
    let bottom = top + home.size().height;
    let window_height = ui
        .window()
        .size()
        .to_logical(ui.window().scale_factor())
        .height;
    assert!(top > 80.0 && top < 130.0);
    assert!((bottom - window_height).abs() <= 1.0);
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
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
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
fn issue_115_content_area_does_not_publish_window_height_as_max() {
    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(!source.contains("max-height: root.height"));
    assert!(!source.contains("Rectangle { min-height: 0px; }"));
    assert!(source.contains("content-pane := Rectangle"));
    assert!(source.contains("if !root.active-is-home: content-area := Rectangle"));
    assert!(source.contains("if !root.active-is-home: status-bar := Rectangle"));
    assert!(source.contains("if root.active-is-home: home-area := Rectangle"));
}

#[test]
fn issue_115_switching_home_keeps_window_height_and_fills_bottom() {
    let ui = headless_file_view();
    ui.window()
        .set_size(slint::LogicalSize::new(1_180.0, 960.0));
    update_test_layout(&ui);
    let scale = ui.window().scale_factor();
    let height = || ui.window().size().to_logical(scale).height;
    assert!((height() - 960.0).abs() < 0.5);

    ui.set_active_is_home(true);
    update_test_layout(&ui);
    assert!((height() - 960.0).abs() < 0.5);
    let home = named_element(&ui, "AppWindow::home-area").expect("home area exists");
    let home_top = home.absolute_position().y;
    assert!(home_top > 80.0 && home_top < 130.0);
    assert!((home_top + home.size().height - 960.0).abs() <= 1.0);
    assert!(named_element(&ui, "AppWindow::content-area").is_none());
    assert!(named_element(&ui, "AppWindow::status-bar").is_none());

    ui.set_active_is_home(false);
    update_test_layout(&ui);
    assert!((height() - 960.0).abs() < 0.5);
    assert!(named_element(&ui, "AppWindow::home-area").is_none());
    let files = named_element(&ui, "AppWindow::content-area").expect("file content area exists");
    assert!(files.absolute_position().y > 80.0 && files.absolute_position().y < 180.0);
    let status = named_element(&ui, "AppWindow::status-bar").expect("file status bar exists");
    assert_eq!(status.size().height, 30.0);
    assert!((status.absolute_position().y + status.size().height - 960.0).abs() <= 1.0);
}

#[test]
fn issue_130_sidebar_survives_scale_changes_before_resize_is_committed() {
    use slint::platform::WindowEvent;

    let ui = headless_file_view();
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\dpi")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let tab_id = app.active_window_state().active_tab;
    let tab = app.tab_mut(tab_id).unwrap();
    tab.replace_entries(vec![focus_entry(1, r"C:\dpi\selected.txt")]);
    tab.load_state = LoadState::Complete;
    tab.select_entry(EntryId(1), false, false);
    let request_id = tab.latest_request();
    let state = Arc::new(Mutex::new(app));
    refresh_window_ui(&ui, &state, window_id);
    update_test_layout(&ui);
    let expected_columns = ui.get_grid_column_count();
    let expected_home_width = ui.get_home_content_width();

    for _ in 0..3 {
        for scale_factor in [1.5, 1.0, 1.25, 1.5, 1.25, 1.0] {
            ui.window()
                .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
            // Background model refreshes can land between the scale and resize events.
            refresh_window_ui(&ui, &state, window_id);
            reflow_after_window_resize(&ui, &state, window_id);
            update_test_layout(&ui);
            assert_eq!(ui.get_window_width(), 1180.0);
            assert_eq!(ui.get_home_content_width(), expected_home_width);
            assert_eq!(ui.get_grid_column_count(), expected_columns);
            assert_eq!(
                named_element(&ui, "AppWindow::sidebar-scroll")
                    .unwrap()
                    .size()
                    .width,
                218.0,
            );

            ui.window().dispatch_event(WindowEvent::Resized {
                size: slint::LogicalSize::new(1180.0, 760.0),
            });
            reflow_after_window_resize(&ui, &state, window_id);
            update_test_layout(&ui);
            assert_eq!(ui.get_window_width(), 1180.0);
            assert_eq!(
                named_element(&ui, "AppWindow::sidebar-scroll")
                    .unwrap()
                    .size()
                    .width,
                218.0,
            );
            let app = state.lock().unwrap();
            let tab = app.tab(tab_id).unwrap();
            assert_eq!(tab.latest_request(), request_id);
            assert_eq!(tab.selected, vec![EntryId(1)]);
        }
    }
}

#[test]
fn issue_130_sidebar_still_follows_real_logical_resize_at_each_scale() {
    use slint::platform::WindowEvent;

    let ui = headless_file_view();
    let app = AppState::new_for_test(vec![PathBuf::from(r"C:\dpi")], 0, [0, 1, 2, 3]);
    let window_id = app.active_window;
    let state = Arc::new(Mutex::new(app));
    for scale_factor in [1.0, 1.25, 1.5] {
        ui.window()
            .dispatch_event(WindowEvent::ScaleFactorChanged { scale_factor });
        for (width, sidebar_width) in [
            (1180.0, Some(218.0)),
            (999.0, Some(184.0)),
            (899.0, None),
            (900.0, Some(184.0)),
            (1000.0, Some(218.0)),
            (1400.0, Some(218.0)),
        ] {
            ui.window().dispatch_event(WindowEvent::Resized {
                size: slint::LogicalSize::new(width, 760.0),
            });
            reflow_after_window_resize(&ui, &state, window_id);
            update_test_layout(&ui);
            assert_eq!(ui.get_window_width(), width);
            assert_eq!(
                named_element(&ui, "AppWindow::sidebar-scroll").map(|item| item.size().width),
                sidebar_width,
                "logical width {width} at scale {scale_factor}",
            );
        }
    }
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
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
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
fn issue_16_drive_row_is_compact_and_other_rows_keep_their_height() {
    use i_slint_backend_testing::ElementRoot;

    let ui = headless_file_view();
    ui.set_quick_access_expanded(true);
    ui.set_drives_expanded(true);
    ui.set_sidebar_items(ModelRc::new(VecModel::from(vec![
        SidebarRow {
            index: 0,
            stable_id: "location:0".into(),
            label: "Home".into(),
            icon_kind: 0,
            group_kind: 0,
            source_kind: 0,
            is_drive: false,
            drive_progress: 0.0,
            drive_state: 3,
            drive_status: "".into(),
            icon: Image::default(),
        },
        SidebarRow {
            index: 1,
            stable_id: "location:1".into(),
            label: "System (C:)".into(),
            icon_kind: 7,
            group_kind: 1,
            source_kind: 0,
            is_drive: true,
            drive_progress: 0.75,
            drive_state: 1,
            drive_status: "Drive usage normal".into(),
            icon: Image::default(),
        },
    ])));
    update_test_layout(&ui);

    let drive = ui
        .root_element()
        .query_descendants()
        .match_id("AppWindow::drive-row")
        .find_all()
        .into_iter()
        .find(|element| element.size().height > 0.0)
        .expect("visible drive row exists");
    assert_eq!(drive.size().height, 40.0);
    let quick_row = ui
        .root_element()
        .query_descendants()
        .match_type_name("Rectangle")
        .find_all()
        .into_iter()
        .find(|element| element.size().height == 30.0)
        .expect("regular sidebar row keeps compact height");
    assert_eq!(quick_row.size().height, 30.0);
}
#[test]
fn issue_70_sidebar_collapse_clamps_stale_scroll_position() {
    use i_slint_backend_testing::ElementRoot;

    let ui = headless_file_view();
    ui.window()
        .set_size(slint::LogicalSize::new(1_180.0, 520.0));
    ui.set_quick_access_expanded(true);
    ui.set_libraries_expanded(true);
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
                group_kind: index % 5,
                source_kind: 0,
                is_drive: false,
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
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
    scroll.scroll(0.0, -5_000.0);
    update_test_layout(&ui);
    let scrolled_to_bottom = ui.get_sidebar_viewport_y();
    assert!(scrolled_to_bottom < 0.0);

    let bottom_group_header = ui
        .root_element()
        .query_descendants()
        .match_type_name("SidebarGroupHeader")
        .find_all()
        .into_iter()
        .last()
        .expect("bottom sidebar group header exists");
    bottom_group_header.mock_single_click(slint::platform::PointerEventButton::Left);
    update_test_layout(&ui);

    assert!(ui.get_sidebar_viewport_y() > scrolled_to_bottom);
}
#[test]
fn issue_87_settings_stays_fixed_and_preserves_sidebar_background_hit_area() {
    use i_slint_backend_testing::ElementRoot;
    use std::{cell::Cell, rc::Rc};

    let ui = headless_file_view();
    ui.window()
        .set_size(slint::LogicalSize::new(1_180.0, 520.0));
    ui.set_quick_access_expanded(true);
    ui.set_libraries_expanded(true);
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
                group_kind: index % 5,
                source_kind: 0,
                is_drive: false,
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
                icon: Image::default(),
            })
            .collect::<Vec<_>>(),
    )));
    let open_settings_count = Rc::new(Cell::new(0));
    let open_settings_count_for_callback = open_settings_count.clone();
    ui.on_open_settings(move || {
        open_settings_count_for_callback.set(open_settings_count_for_callback.get() + 1);
    });
    let background_menu_count = Rc::new(Cell::new(0));
    let background_menu_count_for_callback = background_menu_count.clone();
    ui.on_show_sidebar_background_menu(move |_, _| {
        background_menu_count_for_callback.set(background_menu_count_for_callback.get() + 1);
    });
    update_test_layout(&ui);

    let find = |id: &str| {
        ui.root_element()
            .query_descendants()
            .match_id(id)
            .find_all()
            .into_iter()
            .next()
            .unwrap_or_else(|| panic!("missing element: {id}"))
    };
    let settings_row = find("AppWindow::settings-row");
    let settings_button = find("AppWindow::settings-button");
    let sidebar_scroll = find("AppWindow::sidebar-scroll");
    let initial_y = settings_row.absolute_position().y;

    assert_eq!(settings_row.size().height, 42.0);
    assert_eq!(settings_button.size().width, 34.0);
    assert_eq!(settings_button.size().height, 34.0);
    assert!(settings_button.size().width < settings_row.size().width);

    sidebar_scroll.scroll(0.0, -5_000.0);
    update_test_layout(&ui);
    assert_eq!(settings_row.absolute_position().y, initial_y);

    settings_button.mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(open_settings_count.get(), 1);
    assert_eq!(background_menu_count.get(), 0);

    settings_button.mock_single_click(slint::platform::PointerEventButton::Right);
    assert_eq!(open_settings_count.get(), 1);
    assert_eq!(background_menu_count.get(), 0);

    settings_row.mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(open_settings_count.get(), 1);
    assert_eq!(background_menu_count.get(), 0);

    settings_row.mock_single_click(slint::platform::PointerEventButton::Right);
    assert_eq!(background_menu_count.get(), 1);

    let source = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/ui/app-window.slint"));
    assert!(!source.contains("action-menu"));
    assert!(!source.contains("root.menu-open"));
    assert!(source.contains("settings-row := Rectangle"));
    assert!(source.contains("settings-button := Rectangle"));
    assert!(source.contains("icon-size: 22px;"));
    assert!(!source.contains("Text { text: root.text-settings;"));
}
#[test]
fn issue_74_hiding_all_categories_clamps_scroll_and_preserves_expansion() {
    let ui = headless_file_view();
    ui.window()
        .set_size(slint::LogicalSize::new(1_180.0, 520.0));
    ui.set_quick_access_expanded(false);
    ui.set_libraries_expanded(true);
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
                group_kind: index % 5,
                source_kind: 0,
                is_drive: false,
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
                icon: Image::default(),
            })
            .collect::<Vec<_>>(),
    )));
    update_test_layout(&ui);
    use i_slint_backend_testing::ElementRoot;
    let scroll = ui
        .root_element()
        .query_descendants()
        .match_id("AppWindow::sidebar-scroll")
        .find_all()
        .into_iter()
        .next()
        .expect("sidebar scroll view exists");
    scroll.scroll(0.0, -5_000.0);
    update_test_layout(&ui);
    assert!(ui.get_sidebar_viewport_y() < 0.0);
    ui.set_quick_access_visible(false);
    ui.set_libraries_visible(false);
    ui.set_drives_visible(false);
    ui.set_network_locations_visible(false);
    ui.set_network_visible(false);
    ui.set_sidebar_viewport_y(0.0);
    update_test_layout(&ui);

    assert_eq!(ui.get_sidebar_viewport_y(), 0.0);
    assert!(!ui.get_quick_access_expanded());
    assert!(ui.get_libraries_expanded());
    assert!(ui.get_drives_expanded());
    assert!(ui.get_network_locations_expanded());
    assert!(ui.get_network_expanded());

    ui.set_quick_access_visible(true);
    update_test_layout(&ui);
    assert!(!ui.get_quick_access_expanded());
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
                drive_progress: 0.0,
                drive_state: 3,
                drive_status: "".into(),
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
        (ViewMode::Details, -96.0),
        (ViewMode::List, -90.0),
        (ViewMode::MediumIcons, -372.0),
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
fn ctrl_wheel_steps_accumulate_fractional_lines_and_pixels() {
    let mut accumulated = 0.0;
    let lines = |y| MouseScrollDelta::LineDelta(0.0, y);
    assert_eq!(ctrl_wheel_step(&lines(0.25), 1.0, &mut accumulated), 0);
    assert_eq!(ctrl_wheel_step(&lines(0.75), 1.0, &mut accumulated), 1);
    assert_eq!(ctrl_wheel_step(&lines(3.0), 1.0, &mut accumulated), 3);
    let pixels = |y| MouseScrollDelta::PixelDelta(winit::dpi::PhysicalPosition::new(0.0, y));
    assert_eq!(ctrl_wheel_step(&pixels(60.0), 1.0, &mut accumulated), 0);
    assert_eq!(ctrl_wheel_step(&pixels(60.0), 1.0, &mut accumulated), 1);
    assert_eq!(ctrl_wheel_step(&pixels(-80.0), 1.0, &mut accumulated), -1);
    assert_eq!(ctrl_wheel_step(&pixels(320.0), 2.0, &mut accumulated), 2);
}

#[test]
fn file_scroll_geometry_matches_every_view_mode_and_grid_remainder() {
    assert_eq!(
        file_scroll_maximum(67, ViewMode::Details, 6, 590.0),
        1_554.0
    );
    assert_eq!(file_scroll_maximum(67, ViewMode::List, 6, 590.0), 1_420.0);
    assert_eq!(
        file_scroll_maximum(67, ViewMode::MediumIcons, 6, 590.0),
        898.0
    );
    assert_eq!(
        file_scroll_maximum(66, ViewMode::MediumIcons, 6, 590.0),
        774.0
    );
    assert_eq!(file_scroll_maximum(2, ViewMode::MediumIcons, 6, 590.0), 0.0);
    assert_eq!(
        file_scroll_maximum(67, ViewMode::Details, 6, 590.5),
        1_553.5
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
        2_800.0
    );
    assert_eq!(file_scroll_maximum(100, ViewMode::List, 1, 400.0), 2_600.0);
    assert_eq!(
        file_scroll_maximum(10, ViewMode::MediumIcons, 3, 400.0),
        96.0
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
        staging_root: None,
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
fn issue_83_move_undo_clears_only_successfully_restored_cut_paths() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    let restored_file = PathBuf::from(r"C:\source\restored.txt");
    let restored_folder = PathBuf::from(r"C:\source\restored-folder");
    let still_cut = PathBuf::from(r"C:\source\still-cut.txt");
    app.cut_paths = vec![
        restored_file.clone(),
        restored_folder.clone(),
        still_cut.clone(),
    ];

    let changed =
        clear_completed_cut_paths(&mut app, &[restored_file.clone(), restored_folder.clone()]);

    assert_eq!(changed, vec![restored_file, restored_folder]);
    assert_eq!(app.cut_paths, vec![still_cut]);
}

#[test]
fn issue_83_modified_copy_undo_explains_why_the_copy_was_kept() {
    let message = undo_failure_message(
        Language::Chinese,
        Some(UndoSourceKind::Copy),
        "item changed after the original operation",
    );

    assert!(message.contains("无法撤销复制"));
    assert!(message.contains("复制后的文件或文件夹已经被修改"));
    assert!(message.contains("没有删除"));
}
#[test]
fn clipboard_notifications_update_availability_and_clear_cut_once() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    app.cut_paths = vec![
        PathBuf::from(r"C:\source\one.txt"),
        PathBuf::from(r"C:\source\two.txt"),
    ];
    let same_cut = platform::windows::clipboard::ClipboardFileList {
        paths: app.cut_paths.clone(),
        operation: platform::windows::clipboard::ClipboardOperation::Move,
    };
    app.clipboard_has_files = true;
    app.clipboard_sequence = Some(10);
    assert!(apply_clipboard_snapshot(&mut app, 10, Some(same_cut)).is_empty());
    assert!(app.clipboard_has_files);
    assert_eq!(app.cut_paths.len(), 2);

    let refresh = apply_clipboard_snapshot(&mut app, 11, None);
    assert_eq!(refresh, vec![PathBuf::from(r"C:\source")]);
    assert!(!app.clipboard_has_files);
    assert!(app.cut_paths.is_empty());
    assert!(apply_clipboard_snapshot(&mut app, 11, None).is_empty());
}

#[test]
fn issue_85_pending_cut_cancellation_preserves_selection() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    let tab_id = app.active_window_state().active_tab;
    app.tab_mut(tab_id)
        .unwrap()
        .replace_entries(vec![focus_entry(1, r"C:\source\old.txt")]);
    app.tab_mut(tab_id)
        .unwrap()
        .select_entry(EntryId(1), false, false);
    app.cut_paths = vec![PathBuf::from(r"C:\source\old.txt")];
    app.clipboard_sequence = Some(10);
    let focused_before = app.active().focused;
    let selected_before = app.active().selected.clone();

    assert_eq!(
        pending_clipboard_cut(&app),
        Some((app.cut_paths.clone(), 10))
    );
    assert_eq!(app.active().focused, focused_before);
    assert_eq!(app.active().selected, selected_before);
}

#[test]
fn issue_85_copy_and_absent_sequence_are_not_cancellable_cuts() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    app.clipboard_has_files = true;
    app.clipboard_sequence = Some(10);
    assert_eq!(pending_clipboard_cut(&app), None);

    app.cut_paths = vec![PathBuf::from(r"C:\source\old.txt")];
    app.clipboard_sequence = None;
    assert_eq!(pending_clipboard_cut(&app), None);
}
#[test]
fn copied_files_clear_previous_cut_and_keep_paste_available() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    app.cut_paths = vec![PathBuf::from(r"C:\source\old.txt")];
    let focused_before = app.active().focused;
    let selected_before = app.active().selected.clone();
    let copied = platform::windows::clipboard::ClipboardFileList {
        paths: vec![PathBuf::from(r"D:\other\new.txt")],
        operation: platform::windows::clipboard::ClipboardOperation::Copy,
    };
    assert_eq!(
        apply_clipboard_snapshot(&mut app, 20, Some(copied)),
        vec![PathBuf::from(r"C:\source")]
    );
    assert!(app.clipboard_has_files);
    assert!(app.cut_paths.is_empty());
    assert_eq!(app.active().focused, focused_before);
    assert_eq!(app.active().selected, selected_before);
}

#[test]
fn stale_and_repeated_cut_notifications_preserve_cut_state() {
    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\source")], 0, [0, 1, 2, 3]);
    let paths = vec![PathBuf::from(r"C:\source\old.txt")];
    app.cut_paths = paths.clone();
    app.clipboard_has_files = true;
    app.clipboard_sequence = Some(30);
    let same_cut = platform::windows::clipboard::ClipboardFileList {
        paths: paths.clone(),
        operation: platform::windows::clipboard::ClipboardOperation::Move,
    };
    assert!(apply_clipboard_snapshot(&mut app, 29, Some(same_cut.clone())).is_empty());
    assert_eq!(app.cut_paths, paths);
    assert!(apply_clipboard_snapshot(&mut app, 31, Some(same_cut)).is_empty());
    assert_eq!(app.cut_paths, paths);
}

#[test]
fn unique_shell_created_path_is_detected_without_guessing_multiple_changes() {
    let baseline = HashSet::from([PathBuf::from(r"C:\target\existing.txt")]);
    let one = vec![
        focus_entry(1, r"C:\target\existing.txt"),
        focus_entry(2, r"C:\target\created.txt"),
    ];
    assert_eq!(
        detect_unique_created_path(&baseline, &one),
        Some(PathBuf::from(r"C:\target\created.txt"))
    );
    let mut many = one;
    many.push(focus_entry(3, r"C:\target\another.txt"));
    assert_eq!(detect_unique_created_path(&baseline, &many), None);
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

#[test]
fn issue_64_shortcut_event_rejects_stale_request_and_reused_entry_id() {
    let root = PathBuf::from(r"C:\links");
    let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
    let tab_id = app.active().id;
    let request_id = app.active().latest_request();
    let mut item = focus_entry(1, r"C:\links\docs.lnk");
    item.kind = crate::domain::EntryKind::File;
    app.active_window_state_mut()
        .tabs
        .get_mut(&tab_id)
        .unwrap()
        .replace_entries(vec![item]);
    let state = Arc::new(Mutex::new(app));
    let request = ShortcutRequest {
        tab_id,
        request_id,
        entry_id: EntryId(1),
        path: PathBuf::from(r"C:\links\stale.lnk"),
    };
    assert!(
        apply_shortcut_event(
            &state,
            ShortcutEvent {
                request,
                target: platform::ShortcutTarget {
                    path: PathBuf::from(r"C:\docs"),
                    is_directory: Some(true),
                },
            }
        )
        .is_none()
    );

    state
        .lock()
        .unwrap()
        .tab_mut(tab_id)
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"C:\links"), NavigationKind::Refresh);
    let request = ShortcutRequest {
        tab_id,
        request_id,
        entry_id: EntryId(1),
        path: PathBuf::from(r"C:\links\docs.lnk"),
    };
    assert!(
        apply_shortcut_event(
            &state,
            ShortcutEvent {
                request,
                target: platform::ShortcutTarget {
                    path: PathBuf::from(r"C:\docs"),
                    is_directory: Some(true),
                },
            }
        )
        .is_none()
    );
}
