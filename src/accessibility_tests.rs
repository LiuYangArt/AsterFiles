use crate::app::{AppWindow, FileRow, GridRow, PopupCommandRow, QuickMenuWindow, TabRow};
use i_slint_backend_testing::{ElementHandle, ElementRoot};
use slint::{ComponentHandle, ModelRc, VecModel};

fn by_id(root: &impl ElementRoot, id: &str) -> ElementHandle {
    let id = id.to_owned();
    root.root_element()
        .query_descendants()
        .match_predicate(move |element| element.accessible_id().as_deref() == Some(id.as_str()))
        .find_first()
        .expect("semantic identity exists in the in-memory component")
}

#[test]
fn issue_111_toolbar_tabs_and_files_expose_stable_semantics() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_accessibility_window_id("window/7".into());
    ui.set_accessibility_context("window/7/tab/11/request/3".into());
    ui.set_can_refresh(false);
    let refresh = by_id(&ui, "navigation-refresh");
    assert_eq!(refresh.accessible_enabled(), Some(false));
    ui.set_can_refresh(true);
    assert_eq!(refresh.accessible_enabled(), Some(true));
    ui.set_tabs(ModelRc::new(VecModel::from(vec![TabRow {
        id: 11,
        title: "中文目录".into(),
        active: true,
        ..Default::default()
    }])));
    let tab = by_id(&ui, "window/7/tab/11");
    assert_eq!(tab.accessible_item_selected(), Some(true));
    assert_eq!(tab.accessible_label().as_deref(), Some("中文目录"));
    let entry = FileRow {
        id: 4,
        loaded: true,
        selected: true,
        name: "同名文件.txt".into(),
        ..Default::default()
    };
    ui.set_page_state(2);
    ui.set_files(ModelRc::new(VecModel::from(vec![entry.clone()])));
    let id = "window/7/tab/11/request/3/entry/4";
    let list_entry = by_id(&ui, id);
    assert_eq!(list_entry.accessible_item_selected(), Some(true));
    assert_eq!(
        list_entry.accessible_label().as_deref(),
        Some("同名文件.txt")
    );
    ui.set_view_mode(2);
    ui.set_grid_rows(ModelRc::new(VecModel::from(vec![GridRow {
        entries: ModelRc::new(VecModel::from(vec![entry])),
        ..Default::default()
    }])));
    let grid_entry = by_id(&ui, id);
    assert_eq!(grid_entry.accessible_item_selected(), Some(true));
    assert_eq!(grid_entry.accessible_label(), list_entry.accessible_label());
}

#[test]
fn issue_111_menu_placeholders_are_not_actions() {
    i_slint_backend_testing::init_no_event_loop();
    let menu = QuickMenuWindow::new().unwrap();
    menu.set_content_height(80.0);
    menu.set_rows(ModelRc::new(VecModel::from(vec![
        PopupCommandRow {
            stable_id: "app-command/rename".into(),
            label: "重命名".into(),
            enabled: false,
            ..Default::default()
        },
        PopupCommandRow {
            stable_id: "app-command/view".into(),
            label: "视图".into(),
            enabled: true,
            checkable: true,
            checked: true,
            submenu: true,
            expanded: true,
            ..Default::default()
        },
        PopupCommandRow {
            stable_id: "separator".into(),
            separator: true,
            ..Default::default()
        },
        PopupCommandRow {
            stable_id: "placeholder".into(),
            loading: true,
            placeholder: true,
            ..Default::default()
        },
    ])));
    let action = by_id(&menu, "app-command/rename");
    assert_eq!(action.accessible_enabled(), Some(false));
    let view = by_id(&menu, "app-command/view");
    assert_eq!(view.accessible_checkable(), Some(true));
    assert_eq!(view.accessible_checked(), Some(true));
    assert_eq!(view.accessible_expandable(), Some(true));
    assert_eq!(view.accessible_expanded(), Some(true));
    for id in ["separator", "placeholder"] {
        assert!(
            menu.root_element()
                .query_descendants()
                .match_predicate(move |element| element.accessible_id().as_deref() == Some(id))
                .find_all()
                .is_empty()
        );
    }
}

#[test]
fn issue_146_every_layout_has_exactly_one_rename_editor() {
    use crate::app::{ColumnRow, PositionedGridRow};

    i_slint_backend_testing::init_no_event_loop();
    for mode in 0..8 {
        for grouped in [false, true] {
            let ui = AppWindow::new().unwrap();
            ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
            ui.set_page_state(2);
            ui.set_accessibility_context("rename-test".into());
            ui.set_view_mode(mode);
            ui.set_grouped_grid_enabled(grouped);
            ui.set_layout_row_height(160.0);
            ui.set_layout_card_width(200.0);
            ui.set_layout_icon_size(64.0);
            ui.set_columns(ModelRc::new(VecModel::from(vec![ColumnRow {
                kind: 0,
                visible: true,
                ..Default::default()
            }])));
            let entry = FileRow {
                id: 4,
                loaded: true,
                selected: true,
                name: "报告.txt".into(),
                ..Default::default()
            };
            ui.set_files(ModelRc::new(VecModel::from(vec![entry.clone()])));
            let row = GridRow {
                entries: ModelRc::new(VecModel::from(vec![entry])),
                ..Default::default()
            };
            ui.set_grid_rows(ModelRc::new(VecModel::from(vec![row.clone()])));
            ui.set_grouped_grid_visible_rows(ModelRc::new(VecModel::from(vec![
                PositionedGridRow {
                    row,
                    offset: 0.0,
                    extent: 160.0,
                },
            ])));
            ui.set_rename_entry_id(4);
            ui.set_rename_input("报告.txt".into());
            ui.set_rename_selection_end(6);
            ui.set_rename_editing(true);
            let editors = ui
                .root_element()
                .query_descendants()
                .match_predicate(|element| {
                    element.accessible_id().as_deref() == Some("rename-test/entry/4/rename")
                })
                .find_all();
            assert_eq!(editors.len(), 1, "mode {mode}, grouped {grouped}");
            assert_eq!(editors[0].accessible_label().as_deref(), Some("报告.txt"));
            assert_eq!(editors[0].accessible_enabled(), Some(true));
            ui.set_rename_submitting(true);
            assert_eq!(editors[0].accessible_enabled(), Some(false));
            ui.set_rename_editing(false);
            assert!(
                ui.root_element()
                    .query_descendants()
                    .match_predicate(|element| {
                        element.accessible_id().as_deref() == Some("rename-test/entry/4/rename")
                    })
                    .find_all()
                    .is_empty()
            );
        }
    }
}

fn issue_154_grid_editor(grouped: bool, name: &str) -> AppWindow {
    use crate::app::PositionedGridRow;
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.set_page_state(2);
    ui.set_accessibility_context("window/1/tab/1/request/1".into());
    ui.set_view_mode(2);
    ui.set_grouped_grid_enabled(grouped);
    ui.set_layout_row_height(140.0);
    ui.set_layout_card_width(150.0);
    ui.set_layout_icon_size(64.0);
    let entry = FileRow {
        id: 4,
        loaded: true,
        selected: true,
        name: name.into(),
        ..Default::default()
    };
    ui.set_files(ModelRc::new(VecModel::from(vec![entry.clone()])));
    let row = GridRow {
        entries: ModelRc::new(VecModel::from(vec![entry])),
        ..Default::default()
    };
    ui.set_grid_rows(ModelRc::new(VecModel::from(vec![row.clone()])));
    ui.set_grouped_grid_visible_rows(ModelRc::new(VecModel::from(vec![PositionedGridRow {
        row,
        offset: 0.0,
        extent: 140.0,
    }])));
    ui.set_rename_entry_id(4);
    ui.set_rename_input(name.into());
    ui.set_rename_selection_end(name.len() as i32);
    ui.set_rename_editing(true);
    ui
}

#[test]
fn issue_154_grid_editor_wraps_without_widening_and_keeps_full_name() {
    i_slint_backend_testing::init_no_event_loop();
    for grouped in [false, true] {
        let ui = issue_154_grid_editor(grouped, "short.png");
        let id = "window/1/tab/1/request/1/entry/4/rename";
        let editor = by_id(&ui, id);
        let short_size = editor.size();
        let long_name =
            "T_HoverTank_DecalText_very_long_filename_with_no_spaces_".repeat(4) + ".png";
        ui.set_rename_input(long_name.clone().into());
        let size = editor.size();
        assert!(
            size.height > short_size.height,
            "grouped {grouped}: {short_size:?} -> {size:?}"
        );
        assert_eq!(size.width, short_size.width);
        assert_eq!(
            editor.accessible_value().as_deref(),
            Some(long_name.as_str())
        );
        let position = editor.absolute_position();
        assert!(position.x >= 0.0 && position.x + size.width <= 1180.0);
        assert!(position.y >= ui.get_file_list_top());
        assert!(position.y + size.height <= 760.0 - 28.0);
        ui.set_rename_editing(false);
        assert!(
            ui.root_element()
                .query_descendants()
                .match_predicate(move |element| { element.accessible_id().as_deref() == Some(id) })
                .find_all()
                .is_empty()
        );
    }
}

#[test]
fn issue_154_grid_editor_rejects_stale_context_and_cancels_before_scroll() {
    use std::{cell::Cell, rc::Rc};
    i_slint_backend_testing::init_no_event_loop();
    for grouped in [false, true] {
        let ui = issue_154_grid_editor(grouped, "same-entry-id.png");
        let _editor = by_id(&ui, "window/1/tab/1/request/1/entry/4/rename");
        ui.set_accessibility_context("window/1/tab/2/request/9".into());
        assert!(
            ui.root_element()
                .query_descendants()
                .match_predicate(|element| {
                    element
                        .accessible_id()
                        .is_some_and(|id| id.ends_with("/rename"))
                })
                .find_all()
                .is_empty()
        );
        ui.set_rename_editing(false);
        assert!(
            ui.root_element()
                .query_descendants()
                .match_predicate(|element| {
                    element
                        .accessible_id()
                        .is_some_and(|id| id.ends_with("/rename"))
                })
                .find_all()
                .is_empty()
        );
        slint::platform::update_timers_and_animations();
        ui.set_rename_editing(true);
        let _editor = by_id(&ui, "window/1/tab/2/request/9/entry/4/rename");
        let cancelled = Rc::new(Cell::new(0));
        let count = cancelled.clone();
        let weak = ui.as_weak();
        ui.on_cancel_rename(move || {
            count.set(count.get() + 1);
            weak.unwrap().set_rename_editing(false);
        });
        ui.set_file_viewport_y(-80.0);
        slint::platform::update_timers_and_animations();
        assert_eq!(cancelled.get(), 1);
        assert!(!ui.get_rename_editing());
    }
}
#[test]
fn issue_154_rename_enter_commits_without_newline_and_escape_cancels() {
    use slint::platform::{Key, WindowEvent};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };
    i_slint_backend_testing::init_no_event_loop();
    for grouped in [false, true] {
        for key in [Key::Return, Key::Escape] {
            let name = "T_HoverTank_DecalText_long_filename_without_spaces.png";
            let ui = issue_154_grid_editor(grouped, name);

            let committed = Rc::new(RefCell::new(Vec::<String>::new()));
            let committed_result = committed.clone();
            ui.on_commit_rename(move |name| committed_result.borrow_mut().push(name.to_string()));
            let cancelled = Rc::new(Cell::new(0));
            let cancelled_result = cancelled.clone();
            ui.on_cancel_rename(move || cancelled_result.set(cancelled_result.get() + 1));
            let _editor = by_id(&ui, "window/1/tab/1/request/1/entry/4/rename");
            assert!(
                committed.borrow().is_empty(),
                "mounting the editor must not submit"
            );
            assert_eq!(cancelled.get(), 0);
            let text: slint::SharedString = key.into();
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text });
            assert_eq!(ui.get_rename_input().as_str(), name);
            if key == Key::Return {
                assert_eq!(committed.borrow().as_slice(), [name]);
                assert_eq!(cancelled.get(), 0);
            } else {
                assert!(committed.borrow().is_empty());
                assert_eq!(cancelled.get(), 1);
            }
        }
    }
}

#[test]
fn issue_154_single_line_cursor_scrolls_to_end_and_returns_home() {
    use slint::platform::{Key, WindowEvent};
    i_slint_backend_testing::init_no_event_loop();
    for grouped in [false, true] {
        let name = "T_HoverTank_DecalText_very_long_filename_with_no_spaces.png";
        let ui = issue_154_grid_editor(grouped, name);
        ui.set_view_mode(3);
        let editor = by_id(&ui, "window/1/tab/1/request/1/entry/4/rename");
        let send_key = |key: Key| {
            let text: slint::SharedString = key.into();
            ui.window()
                .dispatch_event(WindowEvent::KeyPressed { text: text.clone() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text });
        };
        send_key(Key::Home);
        let home = editor.absolute_position();
        let width = editor.size().width;
        send_key(Key::End);
        let end = editor.absolute_position();
        assert!(
            end.x < home.x,
            "long filename must scroll left to reveal its end: {home:?} -> {end:?}"
        );
        assert!(
            end.x + width <= home.x + 150.0,
            "text end must remain inside the edit frame"
        );
        send_key(Key::Home);
        assert_eq!(editor.absolute_position().x, home.x);
        assert_eq!(ui.get_rename_input().as_str(), name);
    }
}
