use super::*;
use i_slint_backend_testing::ElementRoot;
use slint::platform::{Key, PointerEventButton, WindowEvent};

#[derive(Debug, PartialEq)]
enum InputObservation {
    Down(i32),
    Activate(i32, bool, bool),
    Cancel,
    Menu(i32),
}

struct InputFixture {
    ui: AppWindow,
    state: WindowSessions,
    gesture: Rc<RefCell<FileDragGesture>>,
    observations: Rc<RefCell<Vec<InputObservation>>>,
    drag_starts: Rc<Cell<usize>>,
    touch_id: &'static str,
}

impl InputFixture {
    fn new(mode: ViewMode) -> Self {
        Self::grouped(mode, crate::domain::GroupField::None)
    }

    fn grouped(mode: ViewMode, group: crate::domain::GroupField) -> Self {
        let root = PathBuf::from(r"C:\issue-133-memory");
        let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
        app.update_directory_preference(root.clone(), |preference| {
            preference.view_mode = mode;
            preference.group_field = group;
        });
        let tab_id = app.active().id;
        let tab = app.tab_mut(tab_id).unwrap();
        let (request, _) = tab.begin_directory_navigation(root.clone(), NavigationKind::Normal);
        assert!(
            tab.accept_directory_batch(
                request,
                (1..=3)
                    .map(|id| FileEntry {
                        id: EntryId(id),
                        original_name: format!("folder{id}").into(),
                        display_name: format!("folder{id}"),
                        name_highlights: Vec::new(),
                        path: root.join(format!("folder{id}")),
                        kind: crate::domain::EntryKind::Directory,
                        open_target: None,
                        library_source_index: None,
                        parent_display: String::new(),
                        size_bytes: None,
                        folder_size: FolderSizeState::Unknown,
                        modified: None,
                        created: None,
                        webdav: None,
                    })
                    .collect(),
                DirectoryViewPreference::default(),
            )
        );
        assert!(tab.finish_directory_navigation(
            request,
            NavigationLocation::Directory(root),
            DirectoryViewPreference::default(),
            None
        ));
        let window_id = app.active_window;
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
        let ui = AppWindow::new().expect("in-memory backend");
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        refresh_ui(&ui, &state);
        let gesture = Rc::new(RefCell::new(FileDragGesture::default()));
        let observations = Rc::new(RefCell::new(Vec::new()));
        // Production wiring owns the right-press handlers; left press and cancel are observed below.
        wire_file_drag_gesture(&ui, state.clone(), gesture.clone());
        let drag_starts = Rc::new(Cell::new(0));
        let started_state = state.clone();
        let started_gesture = gesture.clone();
        let started = drag_starts.clone();
        let started_ui = ui.as_weak();
        ui.on_update_internal_drag(move |x, y| {
            let drag = started_gesture.borrow_mut().take_native_start(
                &started_state.lock().unwrap(),
                x,
                y,
            );
            if drag.is_none() {
                return false;
            }
            started.set(started.get() + 1);
            // OLE itself is not run headlessly; the pointer handoff around it is.
            run_outbound_ole(&started_ui.upgrade().unwrap(), || ());
            true
        });

        let down_state = state.clone();
        let down_gesture = gesture.clone();
        let down_observations = observations.clone();
        ui.on_begin_internal_drag(move |id| {
            down_observations
                .borrow_mut()
                .push(InputObservation::Down(id));
            down_gesture
                .borrow_mut()
                .bind_entry(&down_state.lock().unwrap(), EntryId(id as u32));
        });
        let cancelled_gesture = gesture.clone();
        let cancelled_observations = observations.clone();
        ui.on_cancel_internal_drag(move || {
            cancelled_observations
                .borrow_mut()
                .push(InputObservation::Cancel);
            cancelled_gesture.borrow_mut().cancel("slint_release");
        });
        let menu_observations = observations.clone();
        ui.on_show_entry_menu(move |id, _, _| {
            menu_observations
                .borrow_mut()
                .push(InputObservation::Menu(id));
        });
        let selected_state = state.clone();
        let selected_observations = observations.clone();
        let weak = ui.as_weak();
        // Exercise real TouchAreas and selection projection without constructing Shell workers.
        // Native activation and directory opening remain user acceptance checks.
        ui.on_activate_entry(move |id, toggle, extend| {
            selected_observations
                .borrow_mut()
                .push(InputObservation::Activate(id, toggle, extend));
            let changed = {
                let mut app = selected_state.lock().unwrap();
                let tab = app.tab_mut(tab_id).unwrap();
                mutate_selection(tab, |tab| {
                    tab.select_entry(EntryId(id as u32), toggle, extend)
                })
            };
            if let Some(ui) = weak.upgrade() {
                update_file_rows(&ui, &selected_state, tab_id, &changed);
                update_selection_summary(&ui, &selected_state);
            }
        });
        ui.show().expect("testing backend creates no native window");
        ui.window().request_redraw();
        let touch_id = match (mode, group) {
            (ViewMode::Details | ViewMode::List, _) => "AppWindow::mouse",
            (_, crate::domain::GroupField::None) => "AppWindow::card-touch",
            _ => "AppWindow::grouped-card-touch",
        };
        Self {
            ui,
            state,
            gesture,
            observations,
            drag_starts,
            touch_id,
        }
    }

    fn position(&self, row: usize) -> slint::LogicalPosition {
        let items = self
            .ui
            .root_element()
            .query_descendants()
            .match_id(self.touch_id)
            .find_all();
        let item = items
            .iter()
            .filter(|item| item.size().width > 0.0 && item.size().height > 0.0)
            .nth(row)
            .expect("requested real file-list TouchArea is visible");
        let position = item.absolute_position();
        slint::LogicalPosition::new(position.x + 10.0, position.y + 10.0)
    }

    fn press(&self, row: usize, button: PointerEventButton) -> slint::LogicalPosition {
        let position = self.position(row);
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        match button {
            PointerEventButton::Left => self.gesture.borrow_mut().arm(position.x, position.y),
            PointerEventButton::Right => self
                .ui
                .invoke_arm_internal_right_drag(position.x, position.y),
            _ => {}
        }
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        position
    }

    fn release(&self, position: slint::LogicalPosition, button: PointerEventButton) {
        // winit hands the native release to the window hook before Slint sees it.
        if button == PointerEventButton::Right {
            self.ui.invoke_release_internal_right_drag();
        }
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
    }

    fn click(&self, row: usize) {
        let position = self.press(row, PointerEventButton::Left);
        self.release(position, PointerEventButton::Left);
    }

    fn move_to(&self, position: slint::LogicalPosition) {
        // Mirrors the winit hook: a move that starts OLE is not replayed into Slint.
        if !self.ui.invoke_update_internal_drag(position.x, position.y) {
            self.ui
                .window()
                .dispatch_event(WindowEvent::PointerMoved { position });
        }
    }

    fn selected(&self) -> Vec<EntryId> {
        self.state.lock().unwrap().active().selected.to_vec()
    }
}

#[test]
fn issue_133_first_click_pair_after_reactivation_reaches_the_same_folder() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in [ViewMode::Details, ViewMode::List] {
        let fixture = InputFixture::new(mode);
        let model = fixture.ui.get_files();
        let request_id = fixture.state.lock().unwrap().active().latest_request();
        fixture
            .ui
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(false));
        fixture
            .ui
            .window()
            .dispatch_event(WindowEvent::WindowActiveChanged(true));
        fixture.click(0);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(50));
        fixture.click(0);
        assert_eq!(
            *fixture.observations.borrow(),
            vec![
                InputObservation::Down(1),
                InputObservation::Activate(1, false, false),
                InputObservation::Cancel,
                InputObservation::Down(1),
                InputObservation::Activate(1, false, false),
                InputObservation::Cancel,
            ]
        );
        assert_eq!(fixture.selected(), vec![EntryId(1)]);
        assert_eq!(
            fixture.state.lock().unwrap().active().latest_request(),
            request_id
        );
        assert!(std::ptr::eq(
            model.as_any(),
            fixture.ui.get_files().as_any()
        ));
        assert!(fixture.gesture.borrow().pending.is_none());
    }
}

#[test]
fn issue_133_pointer_down_keeps_selection_and_release_uses_latched_modifiers() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = InputFixture::new(ViewMode::List);
    fixture.click(0);
    fixture.ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Control.into(),
    });
    let position = fixture.press(2, PointerEventButton::Left);
    assert_eq!(fixture.selected(), vec![EntryId(1)], "press only arms drag");
    assert!(fixture.gesture.borrow().pending.is_some());
    fixture
        .ui
        .window()
        .dispatch_event(WindowEvent::KeyReleased {
            text: Key::Control.into(),
        });
    fixture.release(position, PointerEventButton::Left);
    assert_eq!(fixture.selected(), vec![EntryId(1), EntryId(3)]);
    assert!(fixture.gesture.borrow().pending.is_none());
    fixture.ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Shift.into(),
    });
    fixture.click(1);
    fixture
        .ui
        .window()
        .dispatch_event(WindowEvent::KeyReleased {
            text: Key::Shift.into(),
        });
    assert_eq!(fixture.selected(), vec![EntryId(2), EntryId(3)]);
    assert!(
        fixture
            .observations
            .borrow()
            .contains(&InputObservation::Activate(3, true, false))
    );
    assert!(
        fixture
            .observations
            .borrow()
            .contains(&InputObservation::Activate(2, false, true))
    );
}

#[test]
fn issue_133_right_click_on_selection_does_not_activate_or_arm_drag() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = InputFixture::new(ViewMode::List);
    fixture.click(0);
    fixture.observations.borrow_mut().clear();
    let position = fixture.press(0, PointerEventButton::Right);
    fixture.release(position, PointerEventButton::Right);
    assert_eq!(
        *fixture.observations.borrow(),
        vec![InputObservation::Menu(1)]
    );
    assert_eq!(fixture.selected(), vec![EntryId(1)]);
    assert!(fixture.gesture.borrow().pending.is_none());
}

fn offset(position: slint::LogicalPosition, dx: f32) -> slint::LogicalPosition {
    slint::LogicalPosition::new(position.x + dx, position.y)
}

fn issue_160_fixtures() -> Vec<(&'static str, InputFixture)> {
    vec![
        ("details", InputFixture::new(ViewMode::Details)),
        ("list", InputFixture::new(ViewMode::List)),
        ("grid", InputFixture::new(ViewMode::MediumIcons)),
        (
            "grouped grid",
            InputFixture::grouped(ViewMode::MediumIcons, crate::domain::GroupField::Kind),
        ),
    ]
}

#[test]
fn issue_160_short_right_click_opens_menu_on_release_without_drag() {
    i_slint_backend_testing::init_no_event_loop();
    for (view, fixture) in issue_160_fixtures() {
        fixture.click(0);
        fixture.observations.borrow_mut().clear();
        let position = fixture.press(1, PointerEventButton::Right);
        assert!(
            fixture.observations.borrow().is_empty(),
            "{view}: press only arms the right drag candidate"
        );
        assert!(fixture.gesture.borrow().pending.is_some(), "{view}");
        fixture.move_to(offset(position, 3.0));
        assert_eq!(fixture.drag_starts.get(), 0, "{view}: below threshold");
        fixture.release(offset(position, 3.0), PointerEventButton::Right);
        assert_eq!(
            *fixture.observations.borrow(),
            vec![InputObservation::Menu(2)],
            "{view}"
        );
        assert_eq!(fixture.selected(), vec![EntryId(1)], "{view}");
        assert!(fixture.gesture.borrow().pending.is_none(), "{view}");
        assert!(fixture.gesture.borrow().entry_menu.is_none(), "{view}");
        fixture.move_to(offset(position, 300.0));
        assert_eq!(
            fixture.drag_starts.get(),
            0,
            "{view}: late move after release"
        );
    }
}

#[test]
fn issue_160_right_drag_past_threshold_starts_once_and_suppresses_menu() {
    i_slint_backend_testing::init_no_event_loop();
    for (view, fixture) in issue_160_fixtures() {
        fixture.observations.borrow_mut().clear();
        let position = fixture.press(0, PointerEventButton::Right);
        fixture.move_to(offset(position, 2.0));
        assert_eq!(fixture.drag_starts.get(), 0, "{view}");
        fixture.move_to(offset(position, 12.0));
        assert_eq!(fixture.drag_starts.get(), 1, "{view}: right drag starts");
        fixture.move_to(offset(position, 40.0));
        assert_eq!(
            fixture.drag_starts.get(),
            1,
            "{view}: one gesture, one drag"
        );
        fixture.release(offset(position, 40.0), PointerEventButton::Right);
        assert!(
            fixture.observations.borrow().is_empty(),
            "{view}: a started drag never opens the entry menu: {:?}",
            fixture.observations.borrow()
        );
        assert!(
            fixture.selected().is_empty(),
            "{view}: drag keeps selection"
        );
    }
}

#[test]
fn issue_160_cancel_or_source_change_drops_right_drag_and_menu() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = InputFixture::new(ViewMode::List);
    let position = fixture.press(0, PointerEventButton::Right);
    // Escape, focus loss, wheel and tab switches all reach the gesture through this cancel.
    fixture.ui.invoke_cancel_internal_drag();
    fixture.move_to(offset(position, 30.0));
    fixture.release(offset(position, 30.0), PointerEventButton::Right);
    assert_eq!(fixture.drag_starts.get(), 0);
    assert!(
        fixture
            .observations
            .borrow()
            .iter()
            .all(|o| !matches!(o, InputObservation::Menu(_)))
    );

    fixture.observations.borrow_mut().clear();
    let position = fixture.press(0, PointerEventButton::Right);
    {
        let mut app = fixture.state.lock().unwrap();
        let id = app.active().id;
        let tab = app.tab_mut(id).unwrap();
        let location = tab.current_location().clone().unwrap();
        tab.begin_navigation(location, NavigationKind::Refresh);
    }
    fixture.move_to(offset(position, 30.0));
    assert_eq!(fixture.drag_starts.get(), 0, "refreshed source cannot drag");
    fixture.release(offset(position, 30.0), PointerEventButton::Right);
    assert!(
        fixture.observations.borrow().is_empty(),
        "stale entry id cannot open a menu: {:?}",
        fixture.observations.borrow()
    );

    let position = fixture.press(0, PointerEventButton::Right);
    {
        let mut app = fixture.state.lock().unwrap();
        app.open_settings();
    }
    fixture.release(position, PointerEventButton::Right);
    assert!(
        fixture.observations.borrow().is_empty(),
        "tab switch drops menu"
    );
}

#[test]
fn issue_160_slint_right_press_without_native_press_opens_menu_without_drag() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = InputFixture::new(ViewMode::Details);
    let position = fixture.position(0);
    fixture
        .ui
        .window()
        .dispatch_event(WindowEvent::PointerMoved { position });
    fixture
        .ui
        .window()
        .dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Right,
        });
    assert_eq!(
        *fixture.observations.borrow(),
        vec![InputObservation::Menu(1)]
    );
    fixture.move_to(offset(position, 30.0));
    assert_eq!(fixture.drag_starts.get(), 0);
    assert!(fixture.gesture.borrow().pending.is_none());
}

#[test]
fn issue_160_left_press_replaces_pending_right_press() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = InputFixture::new(ViewMode::List);
    fixture.press(0, PointerEventButton::Right);
    let position = fixture.press(1, PointerEventButton::Left);
    assert!(fixture.gesture.borrow().entry_menu.is_none());
    fixture.ui.invoke_release_internal_right_drag();
    assert!(
        fixture
            .observations
            .borrow()
            .iter()
            .all(|o| !matches!(o, InputObservation::Menu(_)))
    );
    fixture.move_to(offset(position, 30.0));
    assert_eq!(
        fixture.drag_starts.get(),
        0,
        "native release cancelled both"
    );
}

#[test]
fn issue_160_native_right_press_arms_and_release_resolves_menu() {
    use winit::event::{DeviceId, ElementState, MouseButton, WindowEvent as Native};
    let input = |state, button| Native::MouseInput {
        device_id: DeviceId::dummy(),
        state,
        button,
    };
    assert_eq!(
        native_file_drag_action(&input(ElementState::Pressed, MouseButton::Left)),
        Some(NativeFileDragAction::Arm(DragButton::Left))
    );
    assert_eq!(
        native_file_drag_action(&input(ElementState::Pressed, MouseButton::Right)),
        Some(NativeFileDragAction::Arm(DragButton::Right))
    );
    assert_eq!(
        native_file_drag_action(&input(ElementState::Released, MouseButton::Right)),
        Some(NativeFileDragAction::ReleaseRight)
    );
    for event in [
        input(ElementState::Released, MouseButton::Left),
        input(ElementState::Pressed, MouseButton::Middle),
        Native::Focused(false),
        Native::Occluded(true),
        Native::CursorLeft {
            device_id: DeviceId::dummy(),
        },
    ] {
        assert_eq!(
            native_file_drag_action(&event),
            Some(NativeFileDragAction::Cancel),
            "{event:?}"
        );
    }
    assert_eq!(native_file_drag_action(&Native::Focused(true)), None);
}

#[test]
fn issue_160_started_drag_hands_pointer_to_ole_without_source_tooltip() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = InputFixture::new(ViewMode::List);
    let elements = |f: &InputFixture| {
        f.ui.window().request_redraw();
        f.ui.root_element().query_descendants().find_all().len()
    };
    let baseline = elements(&fixture);
    let position = fixture.press(0, PointerEventButton::Right);
    fixture.move_to(offset(position, 12.0));
    assert_eq!(fixture.drag_starts.get(), 1);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
    assert_eq!(
        elements(&fixture),
        baseline,
        "source row tooltip must not open while OLE owns the pointer"
    );
    fixture.release(offset(position, 12.0), PointerEventButton::Right);
    assert!(
        fixture.observations.borrow().is_empty(),
        "a started drag never opens the entry menu"
    );

    // Control: the same hover without a drag does open the tooltip popup.
    fixture.move_to(offset(position, 1.0));
    fixture.move_to(offset(position, 2.0));
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(700));
    assert!(elements(&fixture) > baseline, "probe detects the tooltip");
}
