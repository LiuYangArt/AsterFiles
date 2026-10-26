//! #161: selection membership must stay constant-time so whole-directory selections in a
//! 100k-entry directory do not stall the UI thread while rows are patched or rebuilt.
use super::*;
use crate::domain::EntryKind;
use std::ffi::OsString;

const DIRECTORY: &str = r"C:\issue-161";
const LARGE: u32 = 100_000;
const SCALE_SIZES: [u32; 3] = [25_000, 50_000, LARGE];
const CTRL_TOGGLES: u32 = 100;
// Limits leave room for slower CI runners. Before the fix a local run took 1187 ms to select all,
// 1346 ms for one Ctrl-click inside a full selection and grew 10.5x for 4x entries
// (artifacts/perf/issue-161/before-fix.json); the remaining cost is linear row projection.
const SELECT_ALL_LIMIT_MS: f64 = 1000.0;
const FULL_REDRAW_LIMIT_MS: f64 = 1000.0;
const RANGE_LIMIT_MS: f64 = 1000.0;
const CTRL_TOGGLE_PEAK_LIMIT_MS: f64 = 250.0;
const SELECT_ALL_GROWTH_LIMIT: f64 = 8.0;

fn entry(id: u32) -> FileEntry {
    let name = format!("item-{id:06}.txt");
    FileEntry {
        id: EntryId(id),
        original_name: OsString::from(&name),
        path: PathBuf::from(DIRECTORY).join(&name),
        display_name: name,
        name_highlights: Vec::new(),
        kind: EntryKind::File,
        open_target: None,
        library_source_index: None,
        parent_display: DIRECTORY.to_owned(),
        size_bytes: Some(u64::from(id)),
        folder_size: FolderSizeState::NotIndexed,
        modified: None,
        created: None,
        webdav: None,
    }
}

fn fixture(count: u32) -> (WindowSessions, AppWindow) {
    let mut app = AppState::new_for_test(vec![PathBuf::from(DIRECTORY)], 0, [0, 1, 2, 3]);
    app.update_directory_preference(PathBuf::from(DIRECTORY), |view| {
        view.view_mode = ViewMode::Details;
        view.group_field = GroupField::None;
        view.sort_field = SortField::Name;
        view.sort_direction = SortDirection::Ascending;
    });
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.load_state = LoadState::Complete;
    tab.replace_entries((1..=count).map(entry).collect());
    tab.resort_entries();
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &state);
    assert_eq!(ui.get_files().row_count(), count as usize);
    (state, ui)
}

/// Mirrors the selection callbacks: mutate under the lock, then patch only the changed rows.
fn timed_selection(
    ui: &AppWindow,
    state: &WindowSessions,
    mutate: impl FnOnce(&mut TabSession),
) -> Duration {
    let started = Instant::now();
    let (tab_id, changed) = mutate_active_selection(state, mutate).unwrap();
    update_file_rows(ui, state, tab_id, &changed);
    update_selection_summary(ui, state);
    started.elapsed()
}

fn selected_rows(ui: &AppWindow) -> usize {
    ui.get_files().iter().filter(|row| row.selected).count()
}

fn visible_id(state: &WindowSessions, index: usize) -> EntryId {
    state.peek().unwrap().active().visible_entries()[index].id
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[test]
fn issue_161_large_directory_selection_patches_rows_without_quadratic_lookups() {
    i_slint_backend_testing::init_no_event_loop();
    let mut select_all_scale = Vec::new();
    for count in SCALE_SIZES {
        let (state, ui) = fixture(count);
        let elapsed = timed_selection(&ui, &state, TabSession::select_all);
        assert_eq!(selected_rows(&ui), count as usize);
        select_all_scale.push((count, elapsed));
    }

    let (state, ui) = fixture(LARGE);
    let count = LARGE as usize;
    let first = visible_id(&state, 0);
    let middle = visible_id(&state, count / 2);
    let last = visible_id(&state, count - 1);

    let select_all = timed_selection(&ui, &state, TabSession::select_all);
    assert_eq!(selected_rows(&ui), count);
    {
        let app = state.peek().unwrap();
        let tab = app.active();
        assert_eq!(tab.selected.len(), count);
        assert_eq!(tab.selected.first(), Some(&first));
        assert_eq!(tab.selected.last(), Some(&last));
        assert_eq!(tab.focused, Some(first));
        assert_eq!(tab.selection_anchor, Some(first));
    }

    // A whole-window redraw rebuilds every row while everything is selected.
    let started = Instant::now();
    refresh_ui(&ui, &state);
    let refresh_all_selected = started.elapsed();
    assert_eq!(selected_rows(&ui), count);

    // Ctrl-click toggles spread over the directory: each removes one id from a full selection,
    // and the second pass appends it back after the remaining order.
    let toggled = (0..CTRL_TOGGLES)
        .map(|step| visible_id(&state, (step as usize * 997) % count))
        .collect::<Vec<_>>();
    let mut ctrl_toggle_total = Duration::ZERO;
    let mut ctrl_toggle_peak = Duration::ZERO;
    for pass in 0..2 {
        for id in &toggled {
            let elapsed = timed_selection(&ui, &state, |tab| tab.select_entry(*id, true, false));
            ctrl_toggle_total += elapsed;
            ctrl_toggle_peak = ctrl_toggle_peak.max(elapsed);
        }
        let expected = if pass == 0 {
            count - toggled.len()
        } else {
            count
        };
        assert_eq!(selected_rows(&ui), expected);
    }
    {
        let app = state.peek().unwrap();
        let tab = app.active();
        assert_eq!(&tab.selected[count - toggled.len()..], toggled.as_slice());
        assert_eq!(tab.focused, toggled.last().copied());
        assert_eq!(tab.selection_anchor, toggled.last().copied());
    }

    let clear_all = timed_selection(&ui, &state, TabSession::clear_selection);
    assert_eq!(selected_rows(&ui), 0);

    timed_selection(&ui, &state, |tab| tab.select_entry(first, false, false));
    let shift_range = timed_selection(&ui, &state, |tab| tab.select_entry(last, false, true));
    assert_eq!(selected_rows(&ui), count);
    {
        let app = state.peek().unwrap();
        let tab = app.active();
        let visible = tab
            .visible_entries()
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>();
        assert_eq!(tab.selected, visible);
        assert_eq!(tab.selection_anchor, Some(first));
        assert_eq!(tab.focused, Some(last));
    }

    // Ctrl+Shift extends the existing lower half with the upper half; existing ids keep their slot.
    timed_selection(&ui, &state, |tab| tab.select_entry(middle, false, false));
    timed_selection(&ui, &state, |tab| tab.select_entry(last, false, true));
    let ctrl_shift_extend = timed_selection(&ui, &state, |tab| tab.select_entry(first, true, true));
    assert_eq!(selected_rows(&ui), count);
    {
        let app = state.peek().unwrap();
        let tab = app.active();
        assert_eq!(tab.selected.len(), count);
        assert_eq!(tab.selected.first(), Some(&middle));
        assert_eq!(tab.selected[count - count / 2 - 1], last);
        assert_eq!(tab.selected[count - count / 2], first);
        assert_eq!(tab.selection_anchor, Some(middle));
        assert_eq!(tab.focused, Some(first));
    }

    let scale_json = select_all_scale
        .iter()
        .map(|(count, elapsed)| {
            format!("{{\"entries\": {count}, \"ms\": {:.1}}}", millis(*elapsed))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let growth = millis(select_all_scale[2].1) / millis(select_all_scale[0].1).max(0.001);
    let report = format!(
        concat!(
            "{{\n  \"schema_version\": 1,\n  \"issue\": 161,\n  \"entries\": {},\n",
            "  \"view_mode\": \"details\",\n  \"native_window\": false,\n",
            "  \"select_all_scale\": [{}],\n",
            "  \"select_all_growth_4x_entries\": {:.2},\n",
            "  \"select_all_and_patch_rows_ms\": {:.1},\n",
            "  \"refresh_ui_all_selected_ms\": {:.1},\n",
            "  \"ctrl_toggle_count\": {},\n",
            "  \"ctrl_toggle_total_ms\": {:.1},\n",
            "  \"ctrl_toggle_peak_ms\": {:.1},\n",
            "  \"clear_all_ms\": {:.1},\n",
            "  \"shift_range_all_ms\": {:.1},\n",
            "  \"ctrl_shift_extend_half_ms\": {:.1},\n",
            "  \"limits\": {{\"select_all_ms\": {}, \"full_redraw_ms\": {}, \"range_ms\": {}, ",
            "\"ctrl_toggle_peak_ms\": {}, \"select_all_growth_4x_entries\": {}}}\n}}\n"
        ),
        LARGE,
        scale_json,
        growth,
        millis(select_all),
        millis(refresh_all_selected),
        CTRL_TOGGLES * 2,
        millis(ctrl_toggle_total),
        millis(ctrl_toggle_peak),
        millis(clear_all),
        millis(shift_range),
        millis(ctrl_shift_extend),
        SELECT_ALL_LIMIT_MS,
        FULL_REDRAW_LIMIT_MS,
        RANGE_LIMIT_MS,
        CTRL_TOGGLE_PEAK_LIMIT_MS,
        SELECT_ALL_GROWTH_LIMIT,
    );
    let artifact = PathBuf::from("artifacts/perf/issue-161/latest.json");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, &report).unwrap();
    eprintln!("#161 {report}");

    assert!(millis(select_all) < SELECT_ALL_LIMIT_MS, "{report}");
    assert!(
        millis(refresh_all_selected) < FULL_REDRAW_LIMIT_MS,
        "{report}"
    );
    assert!(millis(shift_range) < RANGE_LIMIT_MS, "{report}");
    assert!(millis(ctrl_shift_extend) < RANGE_LIMIT_MS, "{report}");
    assert!(
        millis(ctrl_toggle_peak) < CTRL_TOGGLE_PEAK_LIMIT_MS,
        "{report}"
    );
    assert!(growth < SELECT_ALL_GROWTH_LIMIT, "{report}");
}
