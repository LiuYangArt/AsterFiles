use super::*;
use i_slint_backend_testing::ElementRoot;
use slint::platform::{PointerEventButton, WindowEvent};

fn memory_app() -> AppState {
    let mut app = AppState::new_for_test(
        vec![
            PathBuf::from("C:/issue-93-memory/source"),
            PathBuf::from("C:/issue-93-memory/other"),
        ],
        0,
        [0, 1, 2, 3],
    );
    let id = app.active().id;
    let tab = app.tab_mut(id).unwrap();
    tab.load_state = LoadState::Complete;
    tab.latest_request = RequestId(10);
    tab.entries = Arc::new(vec![FileEntry {
        id: EntryId(7),
        original_name: "Bridge".into(),
        display_name: "Bridge".into(),
        name_highlights: Vec::new(),
        path: PathBuf::from("C:/issue-93-memory/source/Bridge"),
        kind: crate::domain::EntryKind::Directory,
        open_target: None,
        library_source_index: None,
        parent_display: "C:/issue-93-memory/source".into(),
        size_bytes: None,
        folder_size: FolderSizeState::Unknown,
        modified: None,
        created: None,
    }]);
    tab.entry_indices.insert(EntryId(7), 0);
    app
}

fn armed(app: &AppState) -> FileDragGesture {
    let mut gesture = FileDragGesture::default();
    gesture.arm(20.0, 20.0);
    gesture.bind_entry(app, EntryId(7));
    gesture
}

#[test]
fn issue_93_slint_entry_binding_without_native_press_cannot_authorize_drag() {
    let app = memory_app();
    let mut gesture = FileDragGesture::default();
    for _ in 0..3 {
        gesture.bind_entry(&app, EntryId(7));
        assert!(gesture.take_native_start(&app, 900.0, 900.0).is_none());
    }
}

#[test]
fn issue_93_repeated_clicks_and_relocated_release_cannot_authorize_drag() {
    let app = memory_app();
    let mut gesture = FileDragGesture::default();
    for _ in 0..3 {
        gesture.arm(20.0, 20.0);
        gesture.bind_entry(&app, EntryId(7));
        assert!(gesture.take_native_start(&app, 20.0, 20.0).is_none());
        gesture.cancel("pointer_released");
        assert!(gesture.take_native_start(&app, 800.0, 900.0).is_none());
    }
}

#[test]
fn issue_93_valid_native_drag_authorizes_original_paths_exactly_once() {
    let app = memory_app();
    let mut gesture = armed(&app);
    assert!(gesture.take_native_start(&app, 22.0, 22.0).is_none());
    let drag = gesture
        .take_native_start(&app, 24.0, 20.0)
        .expect("native move crossed threshold");
    assert_eq!(drag.origin_tab, app.active().id);
    assert_eq!(drag.request_id, RequestId(10));
    assert_eq!(
        drag.entries,
        vec![(
            EntryId(7),
            PathBuf::from("C:/issue-93-memory/source/Bridge")
        )]
    );
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
    gesture.bind_entry(&app, EntryId(7));
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
}

#[test]
fn issue_93_cancel_and_navigation_invalidate_pending_drag() {
    let mut app = memory_app();
    let mut gesture = armed(&app);
    gesture.cancel("focus_lost");
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());

    let mut gesture = armed(&app);
    let source = app.active().id;
    app.tab_mut(source).unwrap().begin_navigation(
        NavigationLocation::Directory(PathBuf::from("C:/issue-93-memory/source/Bridge")),
        NavigationKind::Normal,
    );
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
    assert!(
        gesture.pending.is_none(),
        "rejected navigation identity must not become valid on back"
    );
}

#[test]
fn issue_93_switching_or_closing_source_tab_rejects_late_pointer_move() {
    let mut app = memory_app();
    let source = app.active().id;
    let other = app.active_window_state().tab_order[1];
    let mut gesture = armed(&app);
    app.active_window_state_mut().active_tab = other;
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
    app.active_window_state_mut().active_tab = source;
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());

    let mut gesture = armed(&app);
    app.close_tab(source).expect("second file tab survives");
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
}

#[test]
fn issue_93_reused_entry_id_cannot_authorize_a_different_original_path() {
    let mut app = memory_app();
    let mut gesture = armed(&app);
    let source = app.active().id;
    Arc::make_mut(&mut app.tab_mut(source).unwrap().entries)[0].path =
        PathBuf::from("C:/issue-93-memory/source/Whitebox");
    assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
    assert!(gesture.pending.is_none());
}

#[test]
fn issue_93_nonfinite_pointer_values_never_authorize_drag() {
    let app = memory_app();
    for (x, y) in [(f32::NAN, 20.0), (20.0, f32::INFINITY)] {
        let mut gesture = FileDragGesture::default();
        gesture.arm(x, y);
        gesture.bind_entry(&app, EntryId(7));
        assert!(gesture.take_native_start(&app, 100.0, 100.0).is_none());
        let mut gesture = armed(&app);
        assert!(gesture.take_native_start(&app, x, y).is_none());
    }
}

fn set_probe_rows(ui: &AppWindow) {
    let mut row = empty_file_row();
    row.id = 7;
    row.loaded = true;
    row.name = "Bridge".into();
    row.is_directory = true;
    ui.set_files(ModelRc::new(VecModel::from(vec![row.clone()])));
    ui.set_grid_rows(ModelRc::new(VecModel::from(vec![GridRow {
        group_header: false,
        group_label: "".into(),
        group_detail: "".into(),
        group_count: 0,
        entries: ModelRc::new(VecModel::from(vec![row])),
    }])));
}

fn actual_file_view_release_cancels_after_model_rebuild(mode: i32, grouped: bool) {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().expect("in-memory testing backend");
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    project_file_geometry(&ui, view_mode_from_ui(mode));
    ui.set_view_mode(mode);
    ui.set_page_state(4);
    ui.set_active_is_home(false);
    ui.set_grid_column_count(1);
    let weak = ui.as_weak();
    ui.on_refresh_grouped_grid_viewport(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_grouped_grid_viewport(&ui);
        }
    });
    set_probe_rows(&ui);
    if grouped {
        rebuild_grouped_grid_layout(&ui, true);
    }
    let app = Rc::new(RefCell::new(memory_app()));
    let gesture = Rc::new(RefCell::new(FileDragGesture::default()));
    let events = Rc::new(RefCell::new(Vec::<&'static str>::new()));
    let starts = Rc::new(std::cell::Cell::new(0));

    let bound_app = app.clone();
    let bound_gesture = gesture.clone();
    let bound_events = events.clone();
    ui.on_begin_internal_drag(move |id| {
        assert_eq!(id, 7);
        bound_events.borrow_mut().push("down");
        bound_gesture
            .borrow_mut()
            .bind_entry(&bound_app.borrow(), EntryId(id as u32));
    });
    let cancelled_gesture = gesture.clone();
    let cancelled_events = events.clone();
    ui.on_cancel_internal_drag(move || {
        cancelled_events.borrow_mut().push("cancel");
        cancelled_gesture.borrow_mut().cancel("slint_release");
    });
    let updated_app = app.clone();
    let updated_gesture = gesture.clone();
    let native_starts = starts.clone();
    ui.on_update_internal_drag(move |x, y| {
        if updated_gesture
            .borrow_mut()
            .take_native_start(&updated_app.borrow(), x, y)
            .is_some()
        {
            native_starts.set(native_starts.get() + 1);
        }
    });
    let activated_app = app.clone();
    let activated_events = events.clone();
    let weak = ui.as_weak();
    ui.on_activate_entry(move |id, _, _| {
        assert_eq!(id, 7);
        activated_events.borrow_mut().push("clicked");
        let mut app = activated_app.borrow_mut();
        let source = app.active().id;
        app.tab_mut(source).unwrap().latest_request.0 += 1;
        if let Some(ui) = weak.upgrade() {
            set_probe_rows(&ui);
            if grouped {
                rebuild_grouped_grid_layout(&ui, true);
            }
        }
    });
    ui.show()
        .expect("testing backend realizes no native window");

    for _ in 0..2 {
        ui.window().request_redraw();
        let item = ui
            .root_element()
            .query_descendants()
            .match_id(if grouped {
                "AppWindow::grouped-card-touch"
            } else if mode == 2 {
                "AppWindow::card-touch"
            } else {
                "AppWindow::mouse"
            })
            .find_all()
            .into_iter()
            .find(|item| item.size().width > 0.0 && item.size().height > 0.0)
            .expect("actual file item TouchArea is laid out");
        let origin = item.absolute_position();
        let point = slint::LogicalPosition::new(origin.x + 10.0, origin.y + 10.0);
        events.borrow_mut().clear();
        ui.window()
            .dispatch_event(WindowEvent::PointerMoved { position: point });
        gesture.borrow_mut().arm(point.x, point.y);
        ui.window().dispatch_event(WindowEvent::PointerPressed {
            position: point,
            button: PointerEventButton::Left,
        });
        assert!(
            gesture.borrow().pending.is_some(),
            "actual Down binds the original entry"
        );
        ui.window().dispatch_event(WindowEvent::PointerReleased {
            position: point,
            button: PointerEventButton::Left,
        });
        assert_eq!(&*events.borrow(), &["down", "clicked", "cancel"]);
        assert!(
            gesture.borrow().pending.is_none(),
            "actual Up clears even when clicked replaced the model"
        );
        ui.invoke_update_internal_drag(point.x + 300.0, point.y + 300.0);
        assert_eq!(
            starts.get(),
            0,
            "late native move after click/navigation cannot start file drag"
        );
    }
}

#[test]
fn issue_93_actual_list_release_cancels_after_clicked_rebuilds_model() {
    actual_file_view_release_cancels_after_model_rebuild(1, false);
}

#[test]
fn issue_93_actual_grid_release_cancels_after_clicked_rebuilds_model() {
    actual_file_view_release_cancels_after_model_rebuild(2, false);
}

#[test]
fn issue_91_grouped_grid_release_cancels_after_clicked_rebuilds_model() {
    actual_file_view_release_cancels_after_model_rebuild(2, true);
}
