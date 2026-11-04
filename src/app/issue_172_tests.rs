//! #172: region ownership and one-shot localized text projection.
use super::*;
use i_slint_backend_testing::ElementRoot;

#[test]
fn issue_172_components_keep_region_implementations_out_of_app_window() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("ui");
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path
            .extension()
            .is_some_and(|extension| extension == "slint")
        {
            let source = std::fs::read_to_string(&path).unwrap();
            let lines = source.lines().count();
            assert!(lines <= 800, "{} has {lines} lines", path.display());
        }
    }

    let window = include_str!("../../ui/app-window.slint");
    for component in [
        "TabBar",
        "NavigationBar",
        "Sidebar",
        "FileView",
        "SettingsPage",
    ] {
        assert!(window.contains(component), "missing region: {component}");
    }
    for implementation in [
        "for tab[index] in root.tabs",
        "settings-scroll :=",
        "for entry[index] in root.files",
        "sidebar-scroll :=",
        "component HomeCard",
    ] {
        assert!(
            !window.contains(implementation),
            "region leaked into AppWindow: {implementation}"
        );
    }
    assert!(include_str!("../../ui/file-view.slint").contains("HomePage"));
}

#[test]
fn issue_172_ui_texts_use_one_global_record_without_window_forwarding() {
    for source in [
        include_str!("../../ui/app-window.slint"),
        include_str!("../../ui/tab-bar.slint"),
        include_str!("../../ui/address-bar.slint"),
        include_str!("../../ui/sidebar.slint"),
        include_str!("../../ui/file-view.slint"),
        include_str!("../../ui/home-page.slint"),
        include_str!("../../ui/settings-page.slint"),
    ] {
        assert!(!source.contains("property <string> text-"));
        assert!(!source.contains("root.text-"));
    }
    let application = include_str!("../app.rs");
    let projection = application
        .split_once("fn apply_ui_texts(")
        .unwrap()
        .1
        .split_once("fn display_path(")
        .unwrap()
        .0;
    assert_eq!(projection.matches(".set_values(").count(), 1);
    assert!(!projection.contains(".set_text_"));
}

#[test]
fn issue_172_language_switch_updates_real_region_controls_in_place() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    for (language, expected) in [
        (
            Language::Chinese,
            ["后退", "刷新", "新建标签", "关闭", "快速访问", "磁盘"],
        ),
        (
            Language::English,
            [
                "Back",
                "Refresh",
                "New tab",
                "Close",
                "Quick access",
                "Drives",
            ],
        ),
        (
            Language::Chinese,
            ["后退", "刷新", "新建标签", "关闭", "快速访问", "磁盘"],
        ),
    ] {
        apply_ui_texts(&ui, language);
        for (id, label) in [
            "navigation-back",
            "navigation-refresh",
            "tab-new",
            "window-close",
            "sidebar-group-quick-access",
            "sidebar-group-drives",
        ]
        .into_iter()
        .zip(expected)
        {
            let element = ui
                .root_element()
                .query_descendants()
                .match_predicate(move |element| element.accessible_id().as_deref() == Some(id))
                .find_first()
                .unwrap_or_else(|| panic!("missing control: {id}"));
            assert_eq!(
                element.accessible_label().as_deref(),
                Some(label),
                "{id}: {language:?}"
            );
        }
    }
}

fn issue_172_update_layout(ui: &AppWindow) {
    ui.window().request_redraw();
    let _ = ui.root_element().query_descendants().find_all();
}

#[test]
fn issue_172_settings_values_survive_conditional_region_creation() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    issue_172_update_layout(&ui);
    for (theme, language, quick_search) in [(2, 1, true), (1, 0, false)] {
        ui.set_active_is_settings(false);
        issue_172_update_layout(&ui);
        ui.set_theme_mode(theme);
        ui.set_language_mode(language);
        ui.set_file_list_quick_search(quick_search);
        ui.set_active_is_settings(true);
        issue_172_update_layout(&ui);
        assert_eq!(ui.get_theme_mode(), theme);
        assert_eq!(ui.get_language_mode(), language);
        assert_eq!(ui.get_file_list_quick_search(), quick_search);
        assert!(
            !ui.root_element()
                .query_descendants()
                .match_id("SettingsPage::settings-scroll")
                .find_all()
                .is_empty()
        );
    }
}

#[test]
fn issue_172_file_models_written_on_settings_page_reach_recreated_file_view() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.set_accessibility_context("issue-172".into());
    for mode in [0, 2] {
        ui.set_active_is_settings(true);
        issue_172_update_layout(&ui);
        let entry = FileRow {
            id: 172,
            loaded: true,
            name: "retained.txt".into(),
            ..Default::default()
        };
        let row = GridRow {
            entries: ModelRc::new(VecModel::from(vec![entry.clone()])),
            ..Default::default()
        };
        ui.set_files(ModelRc::new(VecModel::from(vec![entry])));
        ui.set_grid_rows(ModelRc::new(VecModel::from(vec![row.clone()])));
        ui.set_grouped_grid_enabled(mode == 2);
        ui.set_grouped_grid_visible_rows(ModelRc::new(VecModel::from(vec![PositionedGridRow {
            row,
            offset: 0.0,
            extent: 140.0,
        }])));
        ui.set_layout_row_height(140.0);
        ui.set_layout_card_width(150.0);
        ui.set_layout_icon_size(64.0);
        ui.set_view_mode(mode);
        ui.set_page_state(2);
        ui.set_active_is_settings(false);
        issue_172_update_layout(&ui);
        assert_eq!(ui.get_files().row_count(), 1);
        assert_eq!(ui.get_grid_rows().row_count(), 1);
        assert_eq!(ui.get_grouped_grid_visible_rows().row_count(), 1);
        assert_eq!(ui.get_grouped_grid_enabled(), mode == 2);
        let entry = ui
            .root_element()
            .query_descendants()
            .match_predicate(|element| {
                element.accessible_id().as_deref() == Some("issue-172/entry/172")
            })
            .find_first()
            .expect("recreated file region receives the stored model");
        assert_eq!(entry.accessible_label().as_deref(), Some("retained.txt"));
    }
}

#[test]
fn issue_172_sidebar_values_written_while_hidden_survive_recreation() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.set_window_width(1180.0);
    issue_172_update_layout(&ui);
    for expanded in [false, true] {
        ui.set_window_width(820.0);
        issue_172_update_layout(&ui);
        assert!(
            ui.root_element()
                .query_descendants()
                .match_id("Sidebar::sidebar-scroll")
                .find_all()
                .is_empty()
        );
        ui.set_quick_access_expanded(expanded);
        ui.set_sidebar_items(ModelRc::new(VecModel::from(vec![SidebarRow {
            stable_id: "retained".into(),
            label: "Retained location".into(),
            group_kind: 0,
            ..Default::default()
        }])));
        ui.set_window_width(1180.0);
        issue_172_update_layout(&ui);
        assert_eq!(ui.get_quick_access_expanded(), expanded);
        assert_eq!(ui.get_sidebar_items().row_count(), 1);
        let header = ui
            .root_element()
            .query_descendants()
            .match_predicate(|element| {
                element.accessible_id().as_deref() == Some("sidebar-group-quick-access")
            })
            .find_first()
            .expect("sidebar recreated");
        assert_eq!(header.accessible_expanded(), Some(expanded));
        if expanded {
            let row = ui
                .root_element()
                .query_descendants()
                .match_predicate(|element| {
                    element.accessible_id().as_deref() == Some("sidebar/0/retained")
                })
                .find_first()
                .expect("sidebar receives the stored model");
            assert_eq!(row.accessible_label().as_deref(), Some("Retained location"));
        }
    }
}

#[test]
fn issue_172_access_button_switches_network_and_local_text_in_both_languages() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.set_page_state(7);
    ui.set_show_request_access(true);
    for (language, local, network) in [
        (
            Language::Chinese,
            "使用 Windows 请求访问权限",
            "登录网络位置",
        ),
        (
            Language::English,
            "Request access with Windows",
            "Sign in to network location",
        ),
        (
            Language::Chinese,
            "使用 Windows 请求访问权限",
            "登录网络位置",
        ),
    ] {
        apply_ui_texts(&ui, language);
        for is_network in [true, false] {
            ui.set_request_access_is_network(is_network);
            issue_172_update_layout(&ui);
            let (expected, stale) = if is_network {
                (network, local)
            } else {
                (local, network)
            };
            let buttons = ui
                .root_element()
                .query_descendants()
                .match_accessible_role(i_slint_backend_testing::AccessibleRole::Button)
                .match_predicate(move |element| {
                    element.accessible_label().as_deref() == Some(expected)
                })
                .find_all();
            assert_eq!(
                buttons.len(),
                1,
                "{language:?}, network={is_network}: correct access button"
            );
            assert!(
                ui.root_element()
                    .query_descendants()
                    .match_predicate(
                        move |element| element.accessible_label().as_deref() == Some(stale)
                    )
                    .find_all()
                    .is_empty(),
                "{language:?}, network={is_network}: stale access text"
            );
        }
    }
}

#[test]
fn issue_172_region_creation_preserves_computed_geometry_and_column_defaults() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    ui.set_tabs(ModelRc::new(VecModel::from(vec![TabRow {
        id: 1,
        title: "First".into(),
        active: true,
        ..Default::default()
    }])));
    issue_172_update_layout(&ui);
    assert_eq!(ui.get_tab_current_width(), 178.0);
    assert_eq!(ui.get_tab_strip_width(), 178.0);
    ui.set_tabs(ModelRc::new(VecModel::from(vec![
        TabRow {
            id: 1,
            title: "First".into(),
            active: true,
            ..Default::default()
        },
        TabRow {
            id: 2,
            title: "Second".into(),
            ..Default::default()
        },
    ])));
    ui.set_columns(ModelRc::new(VecModel::from(
        (0..5)
            .map(|kind| ColumnRow {
                kind,
                visible: true,
                ..Default::default()
            })
            .collect::<Vec<_>>(),
    )));
    for (settings, home) in [
        (false, false),
        (true, false),
        (false, false),
        (false, true),
        (false, false),
    ] {
        ui.set_active_is_settings(settings);
        ui.set_active_is_home(home);
        issue_172_update_layout(&ui);
        assert_eq!(ui.get_tab_current_width(), 178.0);
        assert_eq!(ui.get_tab_strip_width(), 361.0);
        assert_eq!(ui.get_normal_name_width(), 480.0);
        assert_eq!(ui.get_normal_kind_width(), 160.0);
        assert_eq!(ui.get_normal_size_width(), 120.0);
        assert_eq!(ui.get_normal_modified_width(), 200.0);
        assert_eq!(ui.get_normal_created_width(), 200.0);
        assert_eq!(ui.get_details_hit_width(), 1160.0);
        assert_eq!(ui.get_home_card_width(), 264.0);
    }
}
