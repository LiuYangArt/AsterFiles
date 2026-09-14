use crate::app::{AppWindow, FileRow, GridRow, PopupCommandRow, QuickMenuWindow, TabRow};
use i_slint_backend_testing::{ElementHandle, ElementRoot};
use slint::{ModelRc, VecModel};

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
