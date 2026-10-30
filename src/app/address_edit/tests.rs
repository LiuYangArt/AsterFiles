use super::*;
use i_slint_backend_testing::{ElementHandle, ElementRoot};
use slint::platform::{Key, WindowEvent};

struct Fixture {
    ui: AppWindow,
    state: WindowSessions,
    workers: actions::ActionWorkers,
    local: mpsc::Receiver<DirectoryRequest>,
    network: mpsc::Receiver<DirectoryRequest>,
    everything: mpsc::Sender<EverythingRequest>,
    searches: mpsc::Receiver<EverythingRequest>,
}

impl Fixture {
    fn new(path: &str) -> Self {
        i_slint_backend_testing::init_no_event_loop();
        let mut app = AppState::new_for_test(vec![PathBuf::from(path)], 0, [0, 1, 2, 3]);
        app.tab_mut(TabId(1)).unwrap().load_state = LoadState::Complete;
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        wire_dismissal(&ui, &state);
        let (directory, local) = mpsc::channel();
        let (network_directory, network) = mpsc::sync_channel(4);
        let (operation, _) = mpsc::channel();
        let (everything, searches) = mpsc::channel();
        refresh_ui(&ui, &state);
        ui.show().unwrap();
        Self {
            ui,
            state,
            workers: actions::ActionWorkers {
                directory,
                network_directory,
                operation,
            },
            local,
            network,
            everything,
            searches,
        }
    }

    fn edit(&self) {
        self.state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_smart_address_edit();
        refresh_ui(&self.ui, &self.state);
        self.ui.invoke_focus_address_editor();
        self.layout();
        assert!(self.ui.get_address_editing());
    }

    fn layout(&self) {
        let _ = self.ui.root_element().query_descendants().find_all();
        slint::platform::update_timers_and_animations();
    }

    fn element(&self, id: &str) -> ElementHandle {
        self.ui
            .root_element()
            .query_descendants()
            .match_id(id)
            .find_all()
            .into_iter()
            .next()
            .unwrap()
    }

    fn enter(&self, directory: Option<&str>, query: &str) {
        let state = self.state.clone();
        let workers = self.workers.clone();
        let everything = self.everything.clone();
        let weak = self.ui.as_weak();
        let directory = directory.map(PathBuf::from);
        let query = query.to_owned();
        // Use the compiled editor's Enter binding; only background classification is injected.
        self.ui.on_navigate_path(move |_| {
            finish_validation(
                &weak.upgrade().unwrap(),
                &state,
                &workers,
                &everything,
                actions::target(&state).unwrap(),
                directory.clone(),
                query.clone(),
            );
        });
        self.ui.window().dispatch_event(WindowEvent::KeyPressed {
            text: Key::Return.into(),
        });
        self.ui.window().dispatch_event(WindowEvent::KeyReleased {
            text: Key::Return.into(),
        });
        self.layout();
    }
}

#[test]
fn issue_177_enter_same_local_directory_projects_breadcrumbs_without_reload() {
    let f = Fixture::new(r"C:\same");
    f.edit();
    let request = f.state.lock().unwrap().active().latest_request();
    f.enter(Some(r"C:\same"), "");
    assert!(!f.ui.get_address_editing());
    assert!(f.local.try_recv().is_err());
    assert!(f.network.try_recv().is_err());
    let app = f.state.lock().unwrap();
    assert_eq!(app.active().latest_request(), request);
    assert!(app.active().back_history().is_empty());
}

#[test]
fn issue_177_enter_different_directory_exits_before_background_completion() {
    let f = Fixture::new(r"C:\same");
    f.edit();
    f.enter(Some(r"C:\other"), "");
    assert!(!f.ui.get_address_editing());
    let request = f.local.try_recv().unwrap();
    assert_eq!(request.path, PathBuf::from(r"C:\other"));
    assert!(!request.cancel.load(Ordering::Acquire));
    assert_eq!(
        f.state.lock().unwrap().active().load_state,
        LoadState::Loading
    );
}

#[test]
fn issue_177_enter_same_unc_directory_does_not_start_network_work() {
    let f = Fixture::new(r"\\server\share\same");
    f.edit();
    f.enter(Some(r"\\server\share\same"), "");
    assert!(!f.ui.get_address_editing());
    assert!(f.network.try_recv().is_err());
    assert!(f.local.try_recv().is_err());
}

#[test]
fn issue_177_enter_invalid_path_keeps_existing_search_behavior() {
    let f = Fixture::new(r"C:\same");
    f.edit();
    f.enter(None, r"C:\missing");
    assert!(f.ui.get_address_editing());
    assert!(f.ui.get_search_results_mode());
    assert!(
        matches!(f.searches.try_recv().unwrap(), EverythingRequest::Search { query, .. } if query == r"C:\missing")
    );
    assert!(f.local.try_recv().is_err());
}

#[test]
fn issue_177_outside_click_uses_actual_bounds_and_does_not_cancel_loading() {
    let f = Fixture::new(r"C:\same");
    let cancel = f
        .state
        .lock()
        .unwrap()
        .tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"C:\loading"), NavigationKind::Normal)
        .1;
    for position in [(10.0, 200.0), (700.0, 300.0), (1170.0, 700.0), (10.0, 10.0)] {
        f.edit();
        let bounds = f.element("AppWindow::address-bar").absolute_position();
        let size = f.element("AppWindow::address-bar").size();
        f.ui.invoke_dismiss_address_edit_outside(
            bounds.x + size.width / 2.0,
            bounds.y + size.height / 2.0,
        );
        assert!(f.ui.get_address_editing(), "inside click must keep editing");
        f.ui.invoke_dismiss_address_edit_outside(position.0, position.1);
        assert!(!f.ui.get_address_editing());
        assert!(!cancel.load(Ordering::Acquire));
        assert_eq!(
            f.state.lock().unwrap().active().load_state,
            LoadState::Loading
        );
    }
}

#[test]
fn issue_177_focus_loss_exits_editing_without_discarding_search_results() {
    let f = Fixture::new(r"C:\same");
    f.edit();
    submit_search(&f.everything, &f.state, None, TabId(1), "needle".to_owned());
    refresh_ui(&f.ui, &f.state);
    let request = f.state.lock().unwrap().active().latest_request();
    // Tab transfers real Slint focus from the editor to the window's next focus target.
    f.ui.set_search_depth_enabled(false);
    f.ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Tab.into(),
    });
    f.ui.window().dispatch_event(WindowEvent::KeyReleased {
        text: Key::Tab.into(),
    });
    f.layout();
    assert!(!f.ui.get_address_editing());
    let app = f.state.lock().unwrap();
    let tab = app.active();
    assert_eq!(tab.page_source, PageSource::Search);
    assert_eq!(tab.latest_request(), request);
    assert_eq!(tab.search_query, "needle");
    assert!(!tab.search_cancel_token().unwrap().load(Ordering::Acquire));
}

#[test]
fn issue_177_escape_keeps_existing_cancel_behavior() {
    let f = Fixture::new(r"C:\same");
    let state = f.state.clone();
    let weak = f.ui.as_weak();
    f.ui.on_cancel_address_edit(move || {
        state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .cancel_address_edit();
        refresh_ui(&weak.upgrade().unwrap(), &state);
    });
    f.edit();
    submit_search(&f.everything, &f.state, None, TabId(1), "needle".to_owned());
    refresh_ui(&f.ui, &f.state);
    let cancel = f
        .state
        .lock()
        .unwrap()
        .active()
        .search_cancel_token()
        .unwrap();
    f.ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    f.layout();
    assert!(!f.ui.get_address_editing());
    assert!(cancel.load(Ordering::Acquire));
    assert_eq!(
        f.state.lock().unwrap().active().page_source,
        PageSource::Directory
    );
    assert_eq!(
        f.state.lock().unwrap().active().address_mode,
        AddressMode::Normal
    );
}

#[test]
fn issue_177_late_validation_cannot_reopen_a_dismissed_editor() {
    let f = Fixture::new(r"C:\same");
    f.edit();
    let target = actions::target(&f.state).unwrap();
    f.ui.invoke_dismiss_address_edit();
    finish_validation(
        &f.ui,
        &f.state,
        &f.workers,
        &f.everything,
        target,
        None,
        "late".to_owned(),
    );
    assert!(!f.ui.get_address_editing());
    assert!(f.searches.try_recv().is_err());
}

#[test]
fn issue_177_reopening_dismissed_search_preserves_query_scope_and_depth() {
    let f = Fixture::new(r"C:\same");
    f.edit();
    {
        let mut app = f.state.lock().unwrap();
        app.tab_mut(TabId(1)).unwrap().search_depth = SearchDepth::CurrentFolder;
    }
    submit_search(&f.everything, &f.state, None, TabId(1), "needle".to_owned());
    refresh_ui(&f.ui, &f.state);
    f.ui.invoke_dismiss_address_edit();
    f.edit();
    let app = f.state.lock().unwrap();
    assert_eq!(app.active().search_query, "needle");
    assert_eq!(app.active().search_depth, SearchDepth::CurrentFolder);
    assert_eq!(
        app.active().search_scope,
        SearchScope::Directory(PathBuf::from(r"C:\same"))
    );
    assert!(f.ui.get_address_input().contains("needle"));
}

#[test]
fn issue_177_enter_same_loading_directory_keeps_request_alive() {
    let f = Fixture::new(r"C:\same");
    let (request, cancel) = f
        .state
        .lock()
        .unwrap()
        .tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"C:\loading"), NavigationKind::Normal);
    f.edit();
    f.enter(Some(r"C:\loading"), "");
    assert!(!f.ui.get_address_editing());
    assert!(!cancel.load(Ordering::Acquire));
    assert_eq!(f.state.lock().unwrap().active().latest_request(), request);
    assert!(f.local.try_recv().is_err());
}

impl Fixture {
    fn prepare_click_targets(&self) {
        let mut app = self.state.lock().unwrap();
        app.sidebar = vec![KnownLocation {
            kind: KnownLocationKind::Pinned,
            label: "Pinned".to_owned(),
            path: PathBuf::from(r"C:\pinned"),
            drive_root: None,
        }];
        app.update_directory_preference(PathBuf::from(r"C:\same"), |view| {
            view.view_mode = ViewMode::Details;
        });
        app.tab_mut(TabId(1))
            .unwrap()
            .replace_entries(vec![FileEntry {
                id: EntryId(1),
                original_name: "file.txt".into(),
                path: PathBuf::from(r"C:\same\file.txt"),
                display_name: "file.txt".to_owned(),
                name_highlights: Vec::new(),
                kind: crate::domain::EntryKind::File,
                open_target: None,
                library_source_index: None,
                parent_display: String::new(),
                size_bytes: Some(0),
                folder_size: FolderSizeState::NotIndexed,
                modified: None,
                created: None,
                webdav: None,
            }]);
    }

    fn native_left_click(&self, position: slint::LogicalPosition) {
        use winit::event::{DeviceId, ElementState, MouseButton, WindowEvent as Native};
        let event = Native::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        };
        // Exercise the production gate that returns early for every left press.
        // Only the native menu cleanup is omitted; the click still reaches Slint.
        assert!(prepare_context_menu_event(
            &self.ui,
            &event,
            winit::dpi::LogicalPosition::new(position.x, position.y)
                .to_physical(f64::from(self.ui.window().scale_factor())),
        ));
        assert!(
            !self.ui.get_address_editing(),
            "native left press must dismiss before menu cleanup returns"
        );
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: slint::platform::PointerEventButton::Left,
            });
        self.layout();
    }
}

#[test]
fn issue_177_native_left_list_click_exits_and_selects_on_first_click() {
    let f = Fixture::new(r"C:\same");
    f.prepare_click_targets();
    let state = f.state.clone();
    let clicks = Rc::new(Cell::new(0));
    let observed = clicks.clone();
    f.ui.on_activate_entry(move |id, toggle, extend| {
        observed.set(observed.get() + 1);
        state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .select_entry(EntryId(id as u32), toggle, extend);
    });
    f.edit();
    let bounds = f.element("AppWindow::mouse").absolute_position();
    f.native_left_click(slint::LogicalPosition::new(
        bounds.x + 10.0,
        bounds.y + 10.0,
    ));
    assert_eq!(clicks.get(), 1);
    assert!(
        f.state
            .lock()
            .unwrap()
            .active()
            .selected
            .contains(&EntryId(1))
    );
}

#[test]
fn issue_177_native_left_sidebar_click_exits_and_activates_first_click() {
    let f = Fixture::new(r"C:\same");
    f.prepare_click_targets();
    let destination = Rc::new(Cell::new(-2));
    let observed = destination.clone();
    f.ui.on_navigate_sidebar(move |index| observed.set(index));
    f.edit();
    let bounds = f.element("AppWindow::side-touch").absolute_position();
    f.native_left_click(slint::LogicalPosition::new(
        bounds.x + 10.0,
        bounds.y + 10.0,
    ));
    assert_eq!(destination.get(), -1, "first sidebar row opens Home");
}

#[test]
fn issue_177_native_left_blank_click_exits_and_keeps_rectangle_gesture() {
    let f = Fixture::new(r"C:\same");
    f.prepare_click_targets();
    let gestures = Rc::new(Cell::new(0));
    let observed = gestures.clone();
    f.ui.on_begin_rectangle_selection(move |_, _, _, _| observed.set(observed.get() + 1));
    f.edit();
    let background = f.element("AppWindow::background-touch");
    let bounds = background.absolute_position();
    f.native_left_click(slint::LogicalPosition::new(
        bounds.x + 20.0,
        bounds.y + background.size().height - 20.0,
    ));
    assert_eq!(gestures.get(), 1);
    assert!(!f.ui.get_rectangle_selection_pointer_active());
}

#[test]
fn issue_177_native_inside_press_and_outside_release_keep_editor_open() {
    use winit::event::{DeviceId, ElementState, MouseButton, WindowEvent as Native};
    let f = Fixture::new(r"C:\same");
    f.edit();
    let bar = f.element("AppWindow::address-bar");
    let origin = bar.absolute_position();
    let point =
        winit::dpi::LogicalPosition::new(origin.x + bar.size().width / 2.0, origin.y + 10.0)
            .to_physical(f64::from(f.ui.window().scale_factor()));
    assert!(prepare_context_menu_event(
        &f.ui,
        &Native::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Pressed,
            button: MouseButton::Left,
        },
        point
    ));
    assert!(f.ui.get_address_editing());
    assert!(!prepare_context_menu_event(
        &f.ui,
        &Native::MouseInput {
            device_id: DeviceId::dummy(),
            state: ElementState::Released,
            button: MouseButton::Left,
        },
        winit::dpi::PhysicalPosition::new(10.0, 200.0)
    ));
    assert!(
        f.ui.get_address_editing(),
        "selecting address text must not dismiss on release outside"
    );
}
