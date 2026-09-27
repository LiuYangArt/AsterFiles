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
}

impl InputFixture {
    fn new(mode: ViewMode) -> Self {
        let root = PathBuf::from(r"C:\issue-133-memory");
        let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
        app.update_directory_preference(root.clone(), |preference| {
            preference.view_mode = mode;
        });
        let tab_id = app.active().id;
        let tab = app.tab_mut(tab_id).unwrap();
        tab.load_state = LoadState::Complete;
        tab.latest_request = RequestId(10);
        tab.replace_entries(
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
                })
                .collect(),
        );
        let window_id = app.active_window;
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
        let ui = AppWindow::new().expect("in-memory backend");
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        refresh_ui(&ui, &state);
        let gesture = Rc::new(RefCell::new(FileDragGesture::default()));
        let observations = Rc::new(RefCell::new(Vec::new()));

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
                let mut changed = selection_projection_ids(tab);
                tab.select_entry(EntryId(id as u32), toggle, extend);
                changed.extend(selection_projection_ids(tab));
                changed
            };
            if let Some(ui) = weak.upgrade() {
                update_file_rows(&ui, &selected_state, tab_id, &changed);
                update_selection_summary(&ui, &selected_state);
            }
        });
        ui.show().expect("testing backend creates no native window");
        ui.window().request_redraw();
        Self {
            ui,
            state,
            gesture,
            observations,
        }
    }

    fn position(&self, row: usize) -> slint::LogicalPosition {
        let items = self
            .ui
            .root_element()
            .query_descendants()
            .match_id("AppWindow::mouse")
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
        if button == PointerEventButton::Left {
            self.gesture.borrow_mut().arm(position.x, position.y);
        }
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed { position, button });
        position
    }

    fn release(&self, position: slint::LogicalPosition, button: PointerEventButton) {
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased { position, button });
    }

    fn click(&self, row: usize) {
        let position = self.press(row, PointerEventButton::Left);
        self.release(position, PointerEventButton::Left);
    }

    fn selected(&self) -> Vec<EntryId> {
        self.state.lock().unwrap().active().selected.clone()
    }
}

#[test]
fn issue_133_first_click_pair_after_reactivation_reaches_the_same_folder() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in [ViewMode::Details, ViewMode::List] {
        let fixture = InputFixture::new(mode);
        let model = fixture.ui.get_files();
        let request_id = fixture.state.lock().unwrap().active().latest_request;
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
            fixture.state.lock().unwrap().active().latest_request,
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
