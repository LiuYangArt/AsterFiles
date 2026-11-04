use super::*;
use slint::{ComponentHandle, Global};

pub(super) fn project<C: ComponentHandle>(ui: &C, system: Option<u32>)
where
    for<'a> VisualStyle<'a>: Global<'a, C>,
{
    let palette = |dark| {
        let value = accent_palette::Palette::derive(system, dark);
        AccentPalette {
            accent: Color::from_argb_encoded(value.accent),
            selection: Color::from_argb_encoded(value.selection),
            emphasis: Color::from_argb_encoded(value.emphasis),
            hover: Color::from_argb_encoded(value.hover),
            rectangle_fill: Color::from_argb_encoded(value.rectangle_fill),
            rectangle_border: Color::from_argb_encoded(value.rectangle_border),
        }
    };
    let style = ui.global::<VisualStyle>();
    style.set_light_accent_palette(palette(false));
    style.set_dark_accent_palette(palette(true));
}

pub(super) fn start(
    receiver: mpsc::Receiver<Option<u32>>,
    state: SharedSessions,
    operation: &OperationWindow,
    confirmations: ConfirmationWindows,
    login: &NetworkLoginWindow,
    rename: &NetworkLocationRenameWindow,
    location: &WebDavLocationWindow,
) -> slint::Timer {
    let operation = operation.as_weak();
    let confirmations = [
        confirmations.delete,
        confirmations.conflict,
        confirmations.exit,
    ];
    let login = login.as_weak();
    let rename = rename.as_weak();
    let location = location.as_weak();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let Some(accent) = receiver.try_iter().last() else {
                return;
            };
            {
                let mut app = state.lock().expect("app state mutex is not poisoned");
                if app.system_accent == accent {
                    return;
                }
                app.system_accent = accent;
            }
            refresh_all_windows(&state, RefreshRegions::SETTINGS);
            if let Some(ui) = operation.upgrade() {
                project(&ui, accent);
            }
            for ui in confirmations.iter().filter_map(slint::Weak::upgrade) {
                project(&ui, accent);
            }
            if let Some(ui) = login.upgrade() {
                project(&ui, accent);
            }
            if let Some(ui) = rename.upgrade() {
                project(&ui, accent);
            }
            if let Some(ui) = location.upgrade() {
                project(&ui, accent);
            }
            let app = state.lock().expect("app state mutex is not poisoned");
            platform::windows::tab_insertion_indicator::set_accent(
                accent_palette::Palette::derive(accent, app.dark_theme()).accent,
            );
        },
    );
    timer
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_173_projection_updates_both_themes_and_preserves_unrelated_tokens() {
        i_slint_backend_testing::init_no_event_loop();
        let ui = AppWindow::new().unwrap();
        let other = AppWindow::new().unwrap();
        let style = ui.global::<VisualStyle>();
        for color in [Some(0xff00_78d4), Some(0xffc4_2b1c), None] {
            project(&ui, color);
            for dark in [false, true] {
                ui.set_dark_theme(dark);
                slint::platform::update_timers_and_animations();
                let expected = accent_palette::Palette::derive(color, dark);
                assert_eq!(style.get_accent().as_argb_encoded(), expected.accent);
                assert_eq!(style.get_selection().as_argb_encoded(), expected.selection);
                assert_eq!(
                    style.get_selection_emphasis().as_argb_encoded(),
                    expected.emphasis
                );
                assert_eq!(style.get_accent_hover().as_argb_encoded(), expected.hover);
                assert_eq!(
                    style.get_rectangle_selection_fill().as_argb_encoded(),
                    expected.rectangle_fill
                );
                assert_eq!(
                    style.get_rectangle_selection_border().as_argb_encoded(),
                    expected.rectangle_border
                );
                assert_eq!(style.get_drive_progress_fill(), style.get_accent());
                assert_eq!(ui.get_insertion_indicator_color(), style.get_accent());
                assert_eq!(
                    style.get_surface().as_argb_encoded(),
                    if dark { 0xff25_2525 } else { 0xffff_ffff }
                );
                assert_eq!(
                    style.get_text_primary().as_argb_encoded(),
                    if dark { 0xfff2_f2f2 } else { 0xff30_333a }
                );
            }
            // Each newly projected window receives the current state, not the previous window's globals.
            project(&other, color);
            other.set_dark_theme(true);
            slint::platform::update_timers_and_animations();
            assert_eq!(
                other.global::<VisualStyle>().get_accent(),
                style.get_accent()
            );
        }
    }

    #[test]
    fn issue_173_notifications_update_state_and_all_standalone_dialogs_without_ui() {
        i_slint_backend_testing::init_no_event_loop();
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from(r"C:\issue-173")],
            0,
            [0, 1, 2, 3],
        )));
        let operation = OperationWindow::new().unwrap();
        let delete = ConfirmationWindow::new().unwrap();
        let conflict = ConfirmationWindow::new().unwrap();
        let exit = ConfirmationWindow::new().unwrap();
        let login = NetworkLoginWindow::new().unwrap();
        let rename = NetworkLocationRenameWindow::new().unwrap();
        let location = WebDavLocationWindow::new().unwrap();
        let (sender, receiver) = mpsc::channel();
        let _timer = start(
            receiver,
            state.clone(),
            &operation,
            ConfirmationWindows::new(&delete, &conflict, &exit),
            &login,
            &rename,
            &location,
        );
        for accent in [Some(0xff00_78d4), Some(0xffc4_2b1c), None] {
            sender.send(Some(0xff00_ff00)).unwrap();
            sender.send(accent).unwrap();
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(100));
            slint::platform::update_timers_and_animations();
            assert_eq!(state.lock().unwrap().system_accent, accent);
            let expected =
                Color::from_argb_encoded(accent_palette::Palette::derive(accent, false).accent);
            assert_eq!(operation.global::<VisualStyle>().get_accent(), expected);
            for ui in [&delete, &conflict, &exit] {
                assert_eq!(ui.global::<VisualStyle>().get_accent(), expected);
            }
            assert_eq!(login.global::<VisualStyle>().get_accent(), expected);
            assert_eq!(rename.global::<VisualStyle>().get_accent(), expected);
            assert_eq!(location.global::<VisualStyle>().get_accent(), expected);
        }
    }
}
