//! #178: scrollbar grabs end at the window boundary, not at the next in-window click.
use super::*;
use i_slint_backend_testing::{ElementHandle, ElementRoot};
use slint::platform::{PointerEventButton, WindowEvent};
use winit::event::{DeviceId, WindowEvent as Native};

struct Fixture {
    ui: AppWindow,
}

impl Fixture {
    fn new(mode: ViewMode, group: GroupField) -> Self {
        let root = PathBuf::from(r"C:\issue-178-memory");
        let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
        app.update_directory_preference(root.clone(), |view| {
            view.view_mode = mode;
            view.group_field = group;
        });
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.load_state = LoadState::Complete;
        tab.replace_entries(
            (1..=160)
                .map(|id| {
                    let name = format!("file-{id:03}.txt");
                    FileEntry {
                        id: EntryId(id),
                        original_name: name.clone().into(),
                        display_name: name.clone(),
                        name_highlights: Vec::new(),
                        path: root.join(name),
                        kind: crate::domain::EntryKind::File,
                        open_target: None,
                        library_source_index: None,
                        parent_display: String::new(),
                        size_bytes: Some(u64::from(id)),
                        folder_size: FolderSizeState::NotIndexed,
                        modified: None,
                        created: None,
                        webdav: None,
                    }
                })
                .collect(),
        );
        tab.resort_entries();
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        refresh_ui(&ui, &state);
        ui.show().expect("testing backend has no native window");
        ui.window().request_redraw();
        Self { ui }
    }

    fn element(&self, id: &str) -> ElementHandle {
        self.ui.window().request_redraw();
        self.ui
            .root_element()
            .query_descendants()
            .match_id(id)
            .find_all()
            .into_iter()
            .find(|item| item.size().width > 0.0 && item.size().height > 0.0)
            .unwrap_or_else(|| panic!("visible scrollbar element: {id}"))
    }

    fn thumb_position(&self) -> slint::LogicalPosition {
        let thumb = self.element("LightScrollBar::thumb");
        let origin = thumb.absolute_position();
        slint::LogicalPosition::new(
            origin.x + thumb.size().width / 2.0,
            origin.y + thumb.size().height / 2.0,
        )
    }

    fn move_to(&self, position: slint::LogicalPosition) {
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerMoved { position });
    }

    fn press(&self, position: slint::LogicalPosition) {
        self.move_to(position);
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerPressed {
                position,
                button: PointerEventButton::Left,
            });
        assert!(
            self.ui.get_scrollbar_pointer_active(),
            "press owns scrollbar grab"
        );
    }

    fn release(&self, position: slint::LogicalPosition) {
        self.ui
            .window()
            .dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
        assert!(
            !self.ui.get_scrollbar_pointer_active(),
            "release clears scrollbar grab"
        );
    }

    fn begin_drag(&self) -> slint::LogicalPosition {
        let position = self.thumb_position();
        let before = self.ui.get_file_viewport_y();
        self.press(position);
        let moved = slint::LogicalPosition::new(position.x, position.y + 35.0);
        self.move_to(moved);
        assert!(
            self.ui.get_file_viewport_y() < before,
            "in-window drag scrolls"
        );
        moved
    }

    fn native(&self, event: Native) {
        // The production winit filter runs before Slint's own pressed-state filtering.
        prepare_scrollbar_event(&self.ui, &event);
    }
}

fn cursor_left() -> Native {
    Native::CursorLeft {
        device_id: DeviceId::dummy(),
    }
}

fn cursor_entered() -> Native {
    Native::CursorEntered {
        device_id: DeviceId::dummy(),
    }
}

#[test]
fn issue_178_leave_ends_drag_with_or_without_outside_release_in_all_views() {
    i_slint_backend_testing::init_no_event_loop();
    for mode in ViewMode::CTRL_WHEEL_ORDER {
        for group in [GroupField::None, GroupField::Kind] {
            for outside_release in [false, true] {
                let fixture = Fixture::new(mode, group);
                let position = fixture.begin_drag();
                let stopped = fixture.ui.get_file_viewport_y();
                fixture.native(cursor_left());
                assert!(
                    !fixture.ui.get_scrollbar_pointer_active(),
                    "{mode:?}/{group:?}"
                );
                fixture.move_to(slint::LogicalPosition::new(1200.0, position.y + 60.0));
                assert_eq!(fixture.ui.get_file_viewport_y(), stopped, "outside move");
                if outside_release {
                    // A release outside may never be delivered to this window.
                    // Deliberately omit the Slint up event, then verify re-entry anyway.
                    fixture.native(Native::MouseInput {
                        device_id: DeviceId::dummy(),
                        state: winit::event::ElementState::Released,
                        button: winit::event::MouseButton::Left,
                    });
                }
                fixture.native(cursor_entered());
                fixture.move_to(slint::LogicalPosition::new(position.x, position.y + 80.0));
                assert_eq!(
                    fixture.ui.get_file_viewport_y(),
                    stopped,
                    "re-entry cannot resume drag"
                );
                fixture.release(position);
                let moved = fixture.begin_drag();
                fixture.release(moved);
                let stopped = fixture.ui.get_file_viewport_y();
                fixture.move_to(slint::LogicalPosition::new(moved.x, moved.y + 30.0));
                assert_eq!(
                    fixture.ui.get_file_viewport_y(),
                    stopped,
                    "ordinary release"
                );
            }
        }
    }
}

#[test]
fn issue_178_focus_occlusion_close_and_explicit_cancel_release_grab() {
    i_slint_backend_testing::init_no_event_loop();
    for event in [
        Native::Focused(false),
        Native::Occluded(true),
        Native::CloseRequested,
        Native::Destroyed,
    ] {
        let fixture = Fixture::new(ViewMode::Details, GroupField::None);
        let position = fixture.begin_drag();
        let stopped = fixture.ui.get_file_viewport_y();
        fixture.native(event);
        fixture.native(Native::Focused(true));
        fixture.move_to(slint::LogicalPosition::new(position.x, position.y + 60.0));
        assert!(!fixture.ui.get_scrollbar_pointer_active());
        assert_eq!(fixture.ui.get_file_viewport_y(), stopped);
        fixture.release(position);
        let moved = fixture.begin_drag();
        fixture.release(moved);
    }
    let fixture = Fixture::new(ViewMode::Details, GroupField::None);
    let position = fixture.begin_drag();
    assert!(
        cancel_scrollbar_pointer(&fixture.ui),
        "Escape consumes an active scrollbar drag"
    );
    assert!(
        !cancel_scrollbar_pointer(&fixture.ui),
        "cancel is idempotent"
    );
    let stopped = fixture.ui.get_file_viewport_y();
    fixture.move_to(slint::LogicalPosition::new(position.x, position.y + 60.0));
    assert_eq!(fixture.ui.get_file_viewport_y(), stopped);
    fixture.release(position);
    let moved = fixture.begin_drag();
    fixture
        .ui
        .window()
        .dispatch_event(WindowEvent::PointerExited);
    assert!(!fixture.ui.get_scrollbar_pointer_active());
    let stopped = fixture.ui.get_file_viewport_y();
    fixture.move_to(slint::LogicalPosition::new(moved.x, moved.y + 60.0));
    assert_eq!(fixture.ui.get_file_viewport_y(), stopped);
    // A fresh down works even if the old outside up was never delivered.
    let moved = fixture.begin_drag();
    fixture.release(moved);
}

#[test]
fn issue_178_cancel_does_not_swallow_other_pointer_gestures_or_wheel() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = Fixture::new(ViewMode::Details, GroupField::None);
    let position = fixture.begin_drag();
    let before = fixture.ui.get_file_viewport_y();
    for event in [
        cursor_entered(),
        Native::Focused(true),
        Native::Occluded(false),
    ] {
        fixture.native(event);
        assert!(fixture.ui.get_scrollbar_pointer_active());
    }
    fixture.move_to(slint::LogicalPosition::new(position.x, position.y + 30.0));
    assert!(
        fixture.ui.get_file_viewport_y() < before,
        "normal events keep in-window drag"
    );
    fixture.native(cursor_left());
    let stopped = fixture.ui.get_file_viewport_y();
    // File-list wheels are consumed by the native hook, not by Slint's scroll delivery.
    let delta = logical_scroll_delta(
        &MouseScrollDelta::LineDelta(0.0, -1.0),
        ViewMode::Details,
        fixture.ui.window().scale_factor(),
    );
    apply_file_scroll_delta(&fixture.ui, delta);
    assert!(
        fixture.ui.get_file_viewport_y() < stopped,
        "wheel still works after cancellation"
    );

    let clicks = Rc::new(Cell::new(0));
    let clicked = clicks.clone();
    fixture
        .ui
        .on_activate_entry(move |_, _, _| clicked.set(clicked.get() + 1));
    let position = slint::LogicalPosition::new(
        fixture.ui.get_file_list_left() + 50.0,
        fixture.ui.get_file_list_top() + 20.0,
    );
    fixture.move_to(position);
    fixture
        .ui
        .window()
        .dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
    assert!(!fixture.ui.get_scrollbar_pointer_active());
    assert!(
        !cancel_scrollbar_pointer(&fixture.ui),
        "non-scrollbar press is untouched"
    );
    fixture.release(position);
    assert_eq!(clicks.get(), 1, "file click retains its grab");
}

#[test]
fn issue_178_track_horizontal_and_settings_use_the_same_release_rule() {
    i_slint_backend_testing::init_no_event_loop();
    let fixture = Fixture::new(ViewMode::Details, GroupField::None);
    let track = fixture.element("LightScrollBar::scroll-touch");
    let origin = track.absolute_position();
    let position =
        slint::LogicalPosition::new(origin.x + 5.0, origin.y + track.size().height - 12.0);
    fixture.press(position);
    let page = fixture.ui.get_file_viewport_y();
    assert!(page < 0.0, "track click pages down");
    fixture.move_to(slint::LogicalPosition::new(position.x, position.y - 50.0));
    assert_eq!(
        fixture.ui.get_file_viewport_y(),
        page,
        "track click is not thumb drag"
    );
    fixture.native(cursor_left());
    assert!(!fixture.ui.get_scrollbar_pointer_active());
    fixture.release(position);

    let track = fixture.element("AppWindow::horizontal-scroll-touch");
    let origin = track.absolute_position();
    let position =
        slint::LogicalPosition::new(origin.x + 30.0, origin.y + track.size().height / 2.0);
    fixture.press(position);
    fixture.move_to(slint::LogicalPosition::new(position.x + 60.0, position.y));
    assert!(fixture.ui.get_file_viewport_x() < 0.0);
    fixture.native(cursor_left());
    let stopped = fixture.ui.get_file_viewport_x();
    fixture.move_to(slint::LogicalPosition::new(position.x + 120.0, position.y));
    assert_eq!(fixture.ui.get_file_viewport_x(), stopped);
    fixture.release(position);

    fixture.ui.set_active_is_settings(true);
    fixture
        .ui
        .window()
        .set_size(slint::LogicalSize::new(1180.0, 600.0));
    let position = fixture.thumb_position();
    fixture.press(position);
    fixture.move_to(slint::LogicalPosition::new(position.x, position.y + 30.0));
    fixture.native(cursor_left());
    assert!(
        !fixture.ui.get_scrollbar_pointer_active(),
        "settings uses shared component"
    );
    let stopped = fixture.thumb_position();
    fixture.move_to(slint::LogicalPosition::new(position.x, position.y + 90.0));
    assert_eq!(fixture.thumb_position().y, stopped.y);
    fixture.release(position);
}
