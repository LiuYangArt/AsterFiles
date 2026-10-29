use super::*;
use platform::windows::shell_icons::{ShellIconRgba, ShellTypeIconKey};

fn entry(id: u32, extension: &str, size: u64) -> FileEntry {
    let name = format!("item-{id:06}.{extension}");
    FileEntry {
        id: EntryId(id),
        original_name: name.clone().into(),
        display_name: name.clone(),
        path: PathBuf::from(r"C:\issue-175-icons").join(name),
        name_highlights: Vec::new(),
        kind: crate::domain::EntryKind::File,
        open_target: None,
        library_source_index: None,
        parent_display: r"C:\issue-175-icons".into(),
        size_bytes: Some(size),
        folder_size: FolderSizeState::NotIndexed,
        modified: None,
        created: None,
        webdav: None,
    }
}

fn fixture(count: u32, extension: &str, size: u64) -> (SharedSessions, AppWindow) {
    GRID_THUMBNAIL_RESIDENTS.with_borrow_mut(|windows| windows.clear());
    let directory = PathBuf::from(r"C:\issue-175-icons");
    let mut app = AppState::new_for_test(vec![directory.clone()], 0, [0, 1, 2, 3]);
    app.update_directory_preference(directory, |view| view.view_mode = ViewMode::Details);
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.load_state = LoadState::Complete;
    tab.replace_entries((1..=count).map(|id| entry(id, extension, size)).collect());
    let state = Arc::new(Mutex::new(app));
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &WindowSessions::new(state.clone(), WindowId(1)));
    (state, ui)
}

fn icon_event(request: IconRequest, value: u8) -> IconEvent {
    let size = request.requested_px.max(32);
    IconEvent {
        tab_id: request.tab_id,
        request_id: request.request_id,
        target: request.target,
        path: request.path,
        icon: ShellIconRgba {
            width: size,
            height: size,
            pixels: vec![value; (size * size * 4) as usize].into(),
        },
        actual_thumbnail: request.thumbnail,
        requested_px: request.requested_px,
        type_key: request.type_key,
        thumbnail_failed: false,
        thumbnail_plan: None,
    }
}

#[test]
fn issue_175_icons_real_shell_and_pixel_updates_stay_viewport_bounded() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(100_000, "txt", 0);
    let (sender, requests) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(sender);
    let conversions_before = file_images::conversion_count();
    request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
    let request = requests.try_recv().expect("one shared type image");
    assert!(!request.thumbnail);
    assert_eq!(
        request.requested_px,
        file_thumbnail_request_px(ViewMode::Details, ui.window().scale_factor()).unwrap()
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(scheduler.shared.0.lock().unwrap().plans.next_any(), None);
    let (sender, results) = mpsc::sync_channel(32);
    let started = Instant::now();
    thread::scope(|threads| {
        threads
            .spawn(|| {
                let _apartment = platform::windows::shell_icons::initialize_shell_worker().unwrap();
                run_icon_request(request, None, &state, &sender, &scheduler);
            })
            .join()
            .unwrap();
    });
    let extraction_ms = started.elapsed().as_secs_f64() * 1000.0;
    let started = Instant::now();
    let changed =
        file_images::present_batch(&ui, &state, &scheduler, vec![results.try_recv().unwrap()]);
    let result_patch_ms = started.elapsed().as_secs_f64() * 1000.0;
    let resident_count =
        GRID_THUMBNAIL_RESIDENTS.with_borrow(|windows| windows[&WindowId(1)].len());
    assert!(changed > 0 && changed <= resident_count);
    assert!(resident_count < 128);
    assert!(
        result_patch_ms < 16.0,
        "shared result patch {result_patch_ms} ms"
    );
    assert_eq!(file_images::conversion_count() - conversions_before, 1);
    let files = ui.get_files();
    let model = files.as_any().downcast_ref::<VecModel<FileRow>>().unwrap();
    let first = model.row_data(0).unwrap().icon.to_rgba8().unwrap();
    let second = model.row_data(1).unwrap().icon.to_rgba8().unwrap();
    assert_eq!(first.as_bytes().as_ptr(), second.as_bytes().as_ptr());
    assert_eq!(model.row_data(99_999).unwrap().icon.size().width, 0);

    let maximum = projected_scroll_maximum(&ui, ViewMode::Details, ui.get_file_viewport_height());
    let mut scroll_peak_ms = 0.0_f64;
    for step in 1..=240 {
        ui.set_file_viewport_y(-maximum * step as f32 / 240.0);
        let started = Instant::now();
        request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
        scroll_peak_ms = scroll_peak_ms.max(started.elapsed().as_secs_f64() * 1000.0);
        assert!(requests.try_recv().is_err());
        assert!(
            scheduler
                .shared
                .0
                .lock()
                .unwrap()
                .plans
                .next_any()
                .is_none()
        );
    }
    assert_eq!(file_images::conversion_count() - conversions_before, 1);
    assert!(
        scroll_peak_ms < 16.0,
        "image scroll peak {scroll_peak_ms} ms"
    );
    let final_image = model.row_data(99_999).unwrap().icon.to_rgba8().unwrap();
    assert_eq!(first.as_bytes().as_ptr(), final_image.as_bytes().as_ptr());
    let report = format!(
        concat!(
            "{{\n  \"issue\": 175, \"entries\": 100000, \"viewport_changes\": 240,\n",
            "  \"native_window\": false, \"real_shell\": true,\n",
            "  \"shared_icon_requests\": 1, \"per_file_thumbnail_requests\": 0,\n",
            "  \"ui_image_conversions\": 1, \"initial_rows_patched\": {},\n",
            "  \"initial_resident_rows\": {}, \"shell_extraction_ms\": {:.3},\n",
            "  \"result_patch_ms\": {:.3}, \"scroll_peak_ms\": {:.3},\n",
            "  \"limits\": {{\"result_patch_ms\": 16, \"scroll_peak_ms\": 16}}\n}}\n"
        ),
        changed, resident_count, extraction_ms, result_patch_ms, scroll_peak_ms
    );
    let path = Path::new("artifacts/perf/issue-175/icons-after-fix.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, &report).unwrap();
    eprintln!("#175 icons {report}");
}

#[test]
fn issue_175_icons_type_identity_size_navigation_and_preview_capabilities() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(50, "png", 200);
    let (sender, receiver) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(sender);
    request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
    let request = receiver.try_recv().unwrap();
    assert!(!request.thumbnail);
    let key = request.type_key.clone().unwrap();
    {
        let mut app = state.lock().unwrap();
        app.thumbnail_types.insert(key.clone(), true);
        let target = entry(2, "png", 200);
        assert!(
            thumbnail_request(&mut app, TabId(1), RequestId(9), &target, 48)
                .unwrap()
                .thumbnail
        );
        let no_provider = ShellTypeIconKey::Extension("txt".into());
        app.thumbnail_types.insert(no_provider.clone(), false);
        let first = entry(3, "txt", 100);
        let first_request =
            thumbnail_request(&mut app, TabId(1), RequestId(9), &first, 48).unwrap();
        assert!(!first_request.thumbnail);
        assert!(
            thumbnail_request(&mut app, TabId(2), RequestId(10), &entry(4, "txt", 100), 48)
                .is_none()
        );
        assert!(thumbnail_request(&mut app, TabId(2), RequestId(10), &first, 128).is_some());
        // Executables and shortcuts keep their per-item images even when empty.
        for extension in ["exe", "lnk", "ico", "url"] {
            let request = thumbnail_request(
                &mut app,
                TabId(1),
                RequestId(9),
                &entry(5, extension, 0),
                48,
            )
            .unwrap();
            assert!(request.thumbnail && request.type_key.is_none());
        }
    }
    let changed = file_images::present_batch(&ui, &state, &scheduler, vec![icon_event(request, 7)]);
    assert!(changed > 0);
    let planned = scheduler
        .shared
        .0
        .lock()
        .unwrap()
        .plans
        .next_any()
        .unwrap()
        .1;
    assert!(
        planned
            .path
            .extension()
            .is_some_and(|extension| extension == "png")
    );
    let ordinary = {
        let mut app = state.lock().unwrap();
        shared_request_after_navigation(&mut app)
    };
    assert!(apply_icon_event(&state, icon_event(ordinary, 8)).is_some());
    assert!(
        state
            .lock()
            .unwrap()
            .type_icon_cache
            .contains_key(&(ShellTypeIconKey::Extension("txt".into()), 256))
    );
}

fn shared_request_after_navigation(app: &mut AppState) -> IconRequest {
    let request = thumbnail_request(app, TabId(1), RequestId(1), &entry(1, "txt", 0), 256).unwrap();
    app.tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(PathBuf::from(r"C:\other"), NavigationKind::Normal);
    request
}

#[test]
fn issue_175_icons_batch_updates_real_pixels_once_and_discards_stale_previews() {
    i_slint_backend_testing::init_no_event_loop();
    let (state, ui) = fixture(100, "png", 500);
    let (sender, receiver) = mpsc::channel();
    let scheduler = ThumbnailScheduler::new(sender);
    request_visible_file_images(&ui, &state, WindowId(1), &scheduler);
    let shared = receiver.try_recv().unwrap();
    let shared_key = shared.type_key.clone().unwrap();
    state
        .lock()
        .unwrap()
        .thumbnail_types
        .insert(shared_key, true);
    file_images::present_batch(&ui, &state, &scheduler, vec![icon_event(shared, 1)]);
    let planned = scheduler
        .shared
        .0
        .lock()
        .unwrap()
        .plans
        .next_any()
        .unwrap()
        .1;
    let request = || IconRequest {
        tab_id: planned.tab_id,
        request_id: planned.request_id,
        target: IconTarget::Entry(planned.entry_id),
        path: planned.path.clone(),
        thumbnail: true,
        requested_px: planned.requested_px,
        type_key: None,
    };
    let a = icon_event(request(), 2);
    let b = icon_event(request(), 3);
    let changed = file_images::present_batch(&ui, &state, &scheduler, vec![a, b]);
    assert_eq!(
        changed, 1,
        "multiple results for one entry produce one row notification"
    );
    let row = ui
        .get_files()
        .iter()
        .find(|row| row.id == planned.entry_id.0 as i32)
        .unwrap();
    assert_eq!(row.thumbnail_state, 2);
    assert_eq!(row.icon.to_rgba8().unwrap().as_bytes()[0], 3);
    let mut stale = icon_event(request(), 4);
    stale.request_id = RequestId(planned.request_id.0 + 1);
    assert_eq!(
        file_images::present_batch(&ui, &state, &scheduler, vec![stale]),
        0
    );
    // A type result uses the actual viewport residents even if the model is a sparse search page.
    let tail = EntryId(100);
    GRID_THUMBNAIL_RESIDENTS.with_borrow_mut(|windows| {
        windows.insert(WindowId(1), HashSet::from([tail]));
    });
    let app = state.lock().unwrap();
    assert_eq!(
        file_images::resident_ids(&ui, &app, WindowId(1), app.tab(TabId(1)).unwrap()),
        [tail]
    );
}
