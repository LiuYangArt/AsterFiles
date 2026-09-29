use super::{
    AppWindow, BreadcrumbRow, Language, NavigationLocation, TabSession, navigation_display_name,
};
use slint::{ComponentHandle, Model, ModelRc, VecModel};

pub(super) fn wire_resize(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.on_breadcrumb_width_changed(move || {
        if let Some(ui) = weak.upgrade() {
            relayout(&ui);
        }
    });
}

pub(super) fn project(ui: &AppWindow, tab: &TabSession, language: Language) {
    let labels: Vec<String> = tab
        .breadcrumb_paths()
        .into_iter()
        .map(|(label, location)| match location {
            NavigationLocation::Home => navigation_display_name(&location, language),
            _ => label,
        })
        .collect();
    let source = ui.get_breadcrumb_source();
    let request = tab.latest_request.0.to_string();
    if ui.get_breadcrumb_tab_id() == tab.id.0 as i32
        && ui.get_breadcrumb_request_id() == request
        && source.row_count() == labels.len()
        && source
            .iter()
            .zip(&labels)
            .all(|(row, label)| row.label == *label)
    {
        return;
    }

    ui.invoke_close_breadcrumb_menu();
    ui.set_breadcrumb_tab_id(tab.id.0 as i32);
    ui.set_breadcrumb_request_id(request.into());
    let count = labels.len();
    let rows = labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| {
            let current = index + 1 == count;
            let natural_width = ui
                .invoke_measure_breadcrumb(label.clone().into(), current)
                .max(42.0);
            BreadcrumbRow {
                index: index as i32,
                label: label.into(),
                current,
                natural_width,
                ..Default::default()
            }
        })
        .collect::<Vec<_>>();
    ui.set_breadcrumb_source(ModelRc::new(VecModel::from(rows)));
    relayout(ui);
}

pub(super) fn relayout(ui: &AppWindow) {
    let source: Vec<_> = ui.get_breadcrumb_source().iter().collect();
    let widths: Vec<_> = source.iter().map(|row| row.natural_width).collect();
    let layout = super::breadcrumb_layout::layout(&widths, ui.get_breadcrumb_available_width());
    // A queued menu action must not be interpreted against another tab or projection.
    ui.invoke_close_breadcrumb_menu();
    let generation = ui.get_breadcrumb_generation().wrapping_add(1);
    ui.set_breadcrumb_generation(generation);
    let visible: Vec<_> = layout
        .visible
        .iter()
        .map(|&(index, offset, extent)| BreadcrumbRow {
            offset,
            extent,
            generation,
            ..source[index].clone()
        })
        .collect();
    let hidden: Vec<_> = layout
        .hidden
        .iter()
        .map(|&index| BreadcrumbRow {
            generation,
            ..source[index].clone()
        })
        .collect();
    ui.set_breadcrumbs(ModelRc::new(VecModel::from(visible)));
    ui.set_breadcrumb_overflow(ModelRc::new(VecModel::from(hidden)));
    ui.set_breadcrumb_overflow_x(layout.overflow_x);
    ui.set_breadcrumb_overflow_width(layout.overflow_width);
}

pub(super) fn resolve_target(
    ui: &AppWindow,
    tab: &TabSession,
    index: i32,
    generation: i32,
) -> Option<NavigationLocation> {
    if generation != ui.get_breadcrumb_generation()
        || tab.id.0 as i32 != ui.get_breadcrumb_tab_id()
        || ui.get_breadcrumb_request_id() != tab.latest_request.0.to_string()
    {
        return None;
    }
    usize::try_from(index)
        .ok()
        .and_then(|index| tab.breadcrumb_paths().get(index).cloned())
        .map(|(_, location)| location)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{NavigationKind, TabId};
    use std::path::PathBuf;

    fn tab(id: u32, path: &str) -> TabSession {
        let mut tab = TabSession::new(TabId(id));
        tab.current_location = Some(NavigationLocation::Directory(PathBuf::from(path)));
        tab
    }

    #[test]
    fn issue_145_projection_preserves_raw_targets_and_rejects_stale_actions() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        let mut first = tab(1, r"C:\中文\outer\middle\near\current");
        project(&ui, &first, Language::Chinese);
        let generation = ui.get_breadcrumb_generation();
        for (index, (_, location)) in first.breadcrumb_paths().iter().enumerate() {
            assert_eq!(
                resolve_target(&ui, &first, index as i32, generation),
                Some(location.clone())
            );
        }
        assert!(resolve_target(&ui, &first, -1, generation).is_none());
        let other = tab(2, r"D:\another\middle\near\current");
        assert!(resolve_target(&ui, &other, 1, generation).is_none());
        project(&ui, &other, Language::English);
        assert!(resolve_target(&ui, &other, 1, generation).is_none());
        first.begin_directory_navigation(PathBuf::from(r"C:\new"), NavigationKind::Normal);
        assert!(resolve_target(&ui, &first, 1, generation).is_none());
        project(&ui, &first, Language::Chinese);
        let generation = ui.get_breadcrumb_generation();
        first.begin_directory_navigation(PathBuf::from(r"C:\newer"), NavigationKind::Normal);
        assert!(resolve_target(&ui, &first, 1, generation).is_none());
    }

    #[test]
    fn issue_145_measurement_and_resize_keep_current_inside_address_bar() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        wire_resize(&ui);
        let path = format!(r"C:\a\b\c\d\e\f\g\h\{}", "long中文".repeat(60));
        let tab = tab(1, &path);
        project(&ui, &tab, Language::Chinese);
        let narrow = ui.invoke_measure_breadcrumb("iii".into(), false);
        let long = ui.invoke_measure_breadcrumb("iiiiiiiiiiii".into(), false);
        assert!(long > narrow);
        for width in [640.0, 900.0, 1280.0, 4000.0] {
            ui.window().set_size(slint::LogicalSize::new(width, 760.0));
            relayout(&ui);
            let available = ui.get_breadcrumb_available_width();
            assert!(available > 100.0, "width {width}: available {available}");
            let visible: Vec<_> = ui.get_breadcrumbs().iter().collect();
            assert_eq!(visible.first().unwrap().index, 0);
            assert!(visible.last().unwrap().current);
            for row in &visible {
                assert!(row.offset >= 0.0 && row.extent >= 0.0);
                assert!(row.offset + row.extent <= available + 0.01);
                assert!(row.natural_width > 16.0);
            }
            let mut indices: Vec<_> = visible
                .iter()
                .map(|row| row.index)
                .chain(ui.get_breadcrumb_overflow().iter().map(|row| row.index))
                .collect();
            indices.sort_unstable();
            assert_eq!(
                indices,
                (0..tab.breadcrumb_paths().len() as i32).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn issue_145_refresh_reuses_projection_and_virtual_unc_targets() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        let network = tab(3, r"\\server\共享\中文\folder");
        project(&ui, &network, Language::Chinese);
        let generation = ui.get_breadcrumb_generation();
        project(&ui, &network, Language::Chinese);
        assert_eq!(generation, ui.get_breadcrumb_generation());
        assert_eq!(
            resolve_target(&ui, &network, 0, generation),
            Some(network.breadcrumb_paths()[0].1.clone())
        );
        let mut home = TabSession::new(TabId(4));
        home.current_location = Some(NavigationLocation::Home);
        project(&ui, &home, Language::Chinese);
        assert_eq!(ui.get_breadcrumb_source().row_count(), 1);
        assert_eq!(
            resolve_target(&ui, &home, 0, ui.get_breadcrumb_generation()),
            Some(NavigationLocation::Home)
        );
        let chinese = ui.get_breadcrumb_source().row_data(0).unwrap().label;
        project(&ui, &home, Language::English);
        assert_ne!(
            chinese,
            ui.get_breadcrumb_source().row_data(0).unwrap().label
        );
    }
}
