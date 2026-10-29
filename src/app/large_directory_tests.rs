//! #175: a 100k-entry local directory must load, rectangle-select and scroll without UI-thread
//! work proportional to the entry count on every batch, pointer move or viewport change.
use super::directory_loading::read_source;
use super::*;
use crate::domain::EntryKind;
use std::ffi::OsString;
use std::fmt::Write as _;

const LARGE: u32 = 100_000;
const SYNTHETIC_DIRECTORY: &str = r"C:\issue-175";
const RECT_STEPS: u32 = 120;
const AUTOSCROLL_TICKS: u32 = 40;
const SCROLL_STEPS: u32 = 240;
const WHEEL_STEPS: u32 = 60;
// Limits leave room for slower CI runners. Before the fix a local run took 7918 ms to load,
// 43 ms per viewport change, 12 ms per row patch, 39 ms per grouped pointer move and 67 ms per
// grouped autoscroll tick (artifacts/perf/issue-175/before-fix.json). After the fix the same
// run took 961 ms, 0.2 ms, 0.002 ms, 0.002 ms and 9 ms; the final full projection (~215 ms)
// remains until #162/#165.
const FIRST_ROWS_LIMIT_MS: f64 = 1000.0;
const LOAD_COMPLETE_LIMIT_MS: f64 = 4000.0;
const BATCH_UI_PEAK_LIMIT_MS: f64 = 100.0;
const POINTER_MOVE_PEAK_LIMIT_MS: f64 = 16.0;
const AUTOSCROLL_TICK_PEAK_LIMIT_MS: f64 = 60.0;
const VIEWPORT_CHANGE_PEAK_LIMIT_MS: f64 = 16.0;
const ROW_PATCH_PEAK_LIMIT_MS: f64 = 8.0;

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

#[derive(Default, Clone, Copy)]
struct Stats {
    total: Duration,
    peak: Duration,
    count: u32,
}

impl Stats {
    fn add(&mut self, elapsed: Duration) {
        self.total += elapsed;
        self.peak = self.peak.max(elapsed);
        self.count += 1;
    }

    fn time<T>(&mut self, action: impl FnOnce() -> T) -> T {
        let started = Instant::now();
        let result = action();
        self.add(started.elapsed());
        result
    }

    fn mean_ms(self) -> f64 {
        millis(self.total) / f64::from(self.count.max(1))
    }

    fn json(self) -> String {
        format!(
            "{{\"count\": {}, \"total_ms\": {:.1}, \"mean_ms\": {:.3}, \"peak_ms\": {:.1}}}",
            self.count,
            millis(self.total),
            self.mean_ms(),
            millis(self.peak)
        )
    }
}

/// Uses the Agent-prepared `%TEMP%\asterfiles-161` (100k empty files) and fills any gap, so a
/// fresh CI runner measures the same directory shape the user reported.
fn local_fixture() -> PathBuf {
    let directory = std::env::var_os("ASTERFILES_ISSUE_175_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("asterfiles-161"));
    let existing = std::fs::read_dir(&directory).map_or(0, |entries| entries.count());
    if existing < LARGE as usize {
        std::fs::create_dir_all(&directory).unwrap();
        for id in 1..=LARGE {
            let path = directory.join(format!("item-{id:06}.txt"));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("cannot create {path:?}: {error}"),
            }
        }
    }
    directory
}

fn synthetic_entry(id: u32, directory_every: u32) -> FileEntry {
    let directory = directory_every != 0 && id.is_multiple_of(directory_every);
    let name = if directory {
        format!("folder-{id:06}")
    } else {
        format!("item-{id:06}.txt")
    };
    FileEntry {
        id: EntryId(id),
        original_name: OsString::from(&name),
        path: PathBuf::from(SYNTHETIC_DIRECTORY).join(&name),
        display_name: name,
        name_highlights: Vec::new(),
        kind: if directory {
            EntryKind::Directory
        } else {
            EntryKind::File
        },
        open_target: None,
        library_source_index: None,
        parent_display: SYNTHETIC_DIRECTORY.to_owned(),
        size_bytes: (!directory).then_some(u64::from(id)),
        folder_size: if directory {
            FolderSizeState::Unknown
        } else {
            FolderSizeState::NotIndexed
        },
        modified: None,
        created: None,
        webdav: None,
    }
}

fn synthetic_fixture(group: GroupField, directory_every: u32) -> (WindowSessions, AppWindow) {
    let mut app = AppState::new_for_test(vec![PathBuf::from(SYNTHETIC_DIRECTORY)], 0, [0, 1, 2, 3]);
    app.update_directory_preference(PathBuf::from(SYNTHETIC_DIRECTORY), |view| {
        view.view_mode = ViewMode::Details;
        view.group_field = group;
        view.sort_field = SortField::Name;
        view.sort_direction = SortDirection::Ascending;
    });
    let tab = app.tab_mut(TabId(1)).unwrap();
    tab.load_state = LoadState::Complete;
    tab.replace_entries(
        (1..=LARGE)
            .map(|id| synthetic_entry(id, directory_every))
            .collect(),
    );
    tab.resort_entries();
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), WindowId(1));
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &state);
    (state, ui)
}

struct Channels {
    icon_receiver: mpsc::Receiver<IconRequest>,
    scheduler: ThumbnailScheduler,
    everything_sender: mpsc::Sender<EverythingRequest>,
    everything_receiver: mpsc::Receiver<EverythingRequest>,
    shortcut_sender: mpsc::SyncSender<ShortcutRequest>,
    shortcut_receiver: mpsc::Receiver<ShortcutRequest>,
}

impl Channels {
    fn new() -> Self {
        let (icon_sender, icon_receiver) = mpsc::channel();
        let (everything_sender, everything_receiver) = mpsc::channel();
        let (shortcut_sender, shortcut_receiver) = mpsc::sync_channel(SHORTCUT_QUEUE_CAPACITY);
        Self {
            icon_receiver,
            scheduler: ThumbnailScheduler::new(icon_sender),
            everything_sender,
            everything_receiver,
            shortcut_sender,
            shortcut_receiver,
        }
    }

    /// Drains what the background workers would receive and returns planned thumbnail keys.
    fn drain(&self) -> (usize, usize, usize, Vec<ThumbnailKey>) {
        let icons = self.icon_receiver.try_iter().count();
        let folders = self.everything_receiver.try_iter().count();
        let shortcuts = self.shortcut_receiver.try_iter().count();
        let mut thumbnails = Vec::new();
        let (lock, _) = &*self.scheduler.shared;
        let mut scheduler = lock.lock().unwrap();
        while let Some((_, key)) = scheduler.plans.next_any() {
            scheduler.plans.finish(&key);
            thumbnails.push(key);
        }
        (icons, folders, shortcuts, thumbnails)
    }
}

/// The work `on_request_folder_size_range` performs for each `viewport-y` change.
fn viewport_changed(
    ui: &AppWindow,
    state: &WindowSessions,
    channels: &Channels,
    timing: &mut [Stats; 3],
) {
    let (tab_id, request_id) = {
        let app = state.lock().unwrap();
        (app.active().id, app.active().latest_request())
    };
    let viewport_y = ui.get_file_viewport_y();
    let viewport_height = ui.get_file_viewport_height();
    timing[0].time(|| {
        submit_visible_folder_sizes(
            &channels.everything_sender,
            &state.shared,
            tab_id,
            request_id,
            viewport_y,
            viewport_height,
        )
    });
    timing[1].time(|| {
        request_visible_file_images(ui, &state.shared, state.window_id, &channels.scheduler)
    });
    timing[2].time(|| {
        submit_visible_shortcuts(
            &channels.shortcut_sender,
            &state.shared,
            ui,
            tab_id,
            request_id,
        )
    });
}

struct LoadProfile {
    transport_first_ms: f64,
    transport_total_ms: f64,
    transport_batches: u32,
    first_rows_ms: f64,
    complete_ms: f64,
    waiting: Stats,
    merge: Stats,
    append_projection: Stats,
    batch_visible_work: Stats,
    batch_ui_total: Stats,
    finish_merge_ms: f64,
    final_projection_ms: f64,
    finish_visible_work_ms: f64,
    icon_requests: usize,
}

fn profile_local_load(directory: &Path) -> LoadProfile {
    let visibility = crate::domain::FileVisibility::default();

    // Helper process enumeration and frame transport alone, with an instant consumer.
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let mut transport_first = None;
    let mut transport_batches = 0;
    let mut transported = 0;
    read_source(directory, visibility, &cancel, &mut |entries| {
        transport_first.get_or_insert_with(|| started.elapsed());
        transport_batches += 1;
        transported += entries.len();
        Ok(())
    })
    .unwrap();
    let transport_total = started.elapsed();
    assert_eq!(transported, LARGE as usize);

    let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\")], 0, [0, 1, 2, 3]);
    app.update_directory_preference(directory.to_path_buf(), |view| {
        view.view_mode = ViewMode::Details;
        view.group_field = GroupField::None;
        view.sort_field = SortField::Name;
        view.sort_direction = SortDirection::Ascending;
    });
    let window_id = app.active_window;
    let (request_id, _) = app
        .tab_mut(TabId(1))
        .unwrap()
        .begin_directory_navigation(directory.to_path_buf(), NavigationKind::Normal);
    let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
    let ui = AppWindow::new().unwrap();
    ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
    refresh_ui(&ui, &state);
    let channels = Channels::new();

    // Mirrors the production pump: the producer waits for `apply_event` to acknowledge each
    // batch, while the UI thread consumes one event at a time.
    let (events, receiver) = mpsc::sync_channel::<DirectoryEvent>(1);
    let path = directory.to_path_buf();
    let started = Instant::now();
    let worker = thread::spawn(move || {
        let cancel = AtomicBool::new(false);
        let skipped = read_source(&path, visibility, &cancel, &mut |entries| {
            let (acknowledgement, applied) = mpsc::channel();
            events
                .send(DirectoryEvent::Batch {
                    tab_id: TabId(1),
                    request_id,
                    entries,
                    acknowledgement,
                })
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
            applied
                .recv()
                .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
        })
        .unwrap();
        events
            .send(DirectoryEvent::Finished {
                tab_id: TabId(1),
                request_id,
                path,
                skipped,
                source_failures: 0,
                library: None,
            })
            .unwrap();
    });

    let mut waiting = Stats::default();
    let mut merge = Stats::default();
    let mut append_projection = Stats::default();
    let mut batch_visible_work = Stats::default();
    let mut batch_ui_total = Stats::default();
    let mut icon_requests = 0;
    let mut first_rows = None;
    let (finish_merge, final_projection, finish_visible_work, complete) = loop {
        let event = waiting.time(|| receiver.recv().unwrap());
        let batch_started = Instant::now();
        match event {
            event @ DirectoryEvent::Batch { .. } => {
                let change = merge.time(|| apply_event(&state.shared, event));
                assert!(change.accepted);
                icon_requests += change.icon_requests.len();
                append_projection
                    .time(|| append_active_file_rows(&ui, &state.shared, TabId(1), request_id));
                if first_rows.is_none() && ui.get_files().row_count() > 0 {
                    first_rows = Some(started.elapsed());
                }
                batch_visible_work.time(|| {
                    request_visible_file_images(&ui, &state.shared, window_id, &channels.scheduler);
                    submit_visible_shortcuts(
                        &channels.shortcut_sender,
                        &state.shared,
                        &ui,
                        TabId(1),
                        request_id,
                    );
                });
                batch_ui_total.add(batch_started.elapsed());
            }
            event @ DirectoryEvent::Finished { .. } => {
                let mut finish = Stats::default();
                let change = finish.time(|| apply_event(&state.shared, event));
                assert!(change.accepted);
                {
                    // Appended rows must already equal the committed order before the rebuild.
                    let app = state.lock().unwrap();
                    let entries = &app.active().entries;
                    assert_eq!(ui.get_files().row_count(), entries.len());
                    assert!(
                        ui.get_files()
                            .iter()
                            .zip(entries.iter())
                            .all(|(row, entry)| row.id == entry.id.0 as i32)
                    );
                }
                let mut projection = Stats::default();
                projection.time(|| refresh_ui(&ui, &state));
                let mut visible = Stats::default();
                visible.time(|| {
                    submit_visible_folder_sizes(
                        &channels.everything_sender,
                        &state.shared,
                        TabId(1),
                        request_id,
                        ui.get_file_viewport_y(),
                        ui.get_file_viewport_height(),
                    );
                    request_completed_file_images(
                        &ui,
                        &state.shared,
                        TabId(1),
                        &channels.scheduler,
                    );
                });
                break (finish, projection, visible, started.elapsed());
            }
            _ => panic!("unexpected directory event"),
        }
    };
    worker.join().unwrap();
    {
        let app = state.lock().unwrap();
        let tab = app.active();
        assert_eq!(tab.load_state, LoadState::Complete);
        assert_eq!(tab.entries.len(), LARGE as usize);
        assert!(
            ui.get_files()
                .iter()
                .zip(tab.entries.iter())
                .all(|(row, entry)| row.id == entry.id.0 as i32)
        );
    }
    assert_eq!(ui.get_files().row_count(), LARGE as usize);
    icon_requests += channels.drain().0;
    LoadProfile {
        transport_first_ms: millis(transport_first.unwrap()),
        transport_total_ms: millis(transport_total),
        transport_batches,
        first_rows_ms: millis(first_rows.unwrap()),
        complete_ms: millis(complete),
        waiting,
        merge,
        append_projection,
        batch_visible_work,
        batch_ui_total,
        finish_merge_ms: millis(finish_merge.total),
        final_projection_ms: millis(final_projection.total),
        finish_visible_work_ms: millis(finish_visible_work.total),
        icon_requests,
    }
}

struct RectangleProfile {
    group: &'static str,
    hits: Stats,
    selection_update: Stats,
    patch_rows: Stats,
    pointer_move: Stats,
    autoscroll_tick: Stats,
    selected_after_drag: usize,
    selected_after_autoscroll: usize,
}

fn begin_gesture(
    state: &WindowSessions,
    x: f32,
    y: f32,
) -> Arc<Mutex<Option<RectangleSelectionGesture>>> {
    let app = state.lock().unwrap();
    let tab = app.active();
    Arc::new(Mutex::new(Some(RectangleSelectionGesture {
        tab_id: tab.id,
        request_id: tab.latest_request(),
        start_x: x,
        start_content_y: y,
        pointer_x: x,
        pointer_y: y,
        snapshot: Arc::new(HashSet::new()),
        snapshot_order: Vec::new(),
        focused: None,
        anchor: None,
        mode: RectangleSelectionMode::Replace,
        active: true,
        dirty: false,
        committed: false,
        last_hits: HashSet::new(),
    })))
}

fn move_pointer(gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>, x: f32, y: f32) {
    let mut gesture = gesture.lock().unwrap();
    let gesture = gesture.as_mut().unwrap();
    gesture.pointer_x = x;
    gesture.pointer_y = y;
    gesture.dirty = true;
}

fn assert_rows_match_selection(ui: &AppWindow, state: &WindowSessions, label: &str) {
    let app = state.lock().unwrap();
    let tab = app.active();
    for row in ui.get_files().iter().filter(|row| !row.group_header) {
        let id = EntryId(row.id as u32);
        assert_eq!(row.selected, tab.selected.contains(&id), "{label} {id:?}");
        assert_eq!(row.focused, tab.focused == Some(id), "{label} {id:?}");
    }
}

fn profile_rectangle(group: GroupField, label: &'static str) -> RectangleProfile {
    let (state, ui) = synthetic_fixture(group, 0);
    let timer = slint::Timer::default();
    let x = 120.0;
    let start_y = 40.0;
    let gesture = begin_gesture(&state, x, start_y);
    let mut hits = Stats::default();
    let mut selection_update = Stats::default();
    let mut selection_tab = TabSession::new_home(TabId(1));
    selection_tab.replace_entries(state.lock().unwrap().active().entries.as_ref().clone());
    let snapshot = HashSet::new();
    let mut patch_rows = Stats::default();
    let mut pointer_move = Stats::default();
    for step in 1..=RECT_STEPS {
        let pointer_y = start_y + step as f32 * 5.0;
        move_pointer(&gesture, x + step as f32, pointer_y);
        // Component costs at the same pointer position, measured outside the gesture.
        {
            let app = state.lock().unwrap();
            let rect = SelectionRect::from_points(x, start_y, x + step as f32, pointer_y);
            let files = ui.get_files();
            let hit = |shown_rows| {
                rectangle_selection_hits_for_app(
                    &app,
                    TabId(1),
                    ViewMode::Details,
                    1,
                    ui.get_file_viewport_width(),
                    ui.get_file_viewport_x(),
                    ui.get_details_hit_width(),
                    rect,
                    shown_rows,
                )
            };
            let indexed = hits.time(|| hit(files.as_any().downcast_ref::<VecModel<FileRow>>()));
            assert_eq!(
                indexed,
                hit(None),
                "row-indexed hits match the group projection"
            );
            selection_update.time(|| {
                selection_tab.apply_rectangle_selection(
                    &snapshot,
                    &indexed,
                    RectangleSelectionMode::Replace,
                )
            });
            assert_eq!(
                selection_tab
                    .selected
                    .iter()
                    .copied()
                    .collect::<HashSet<_>>(),
                indexed
            );
        }
        let focused = state.lock().unwrap().active().focused;
        if let Some(focused) = focused {
            patch_rows
                .time(|| update_file_rows(&ui, &state.shared, TabId(1), &HashSet::from([focused])));
        }
        pointer_move.time(|| update_rectangle_selection(&ui, &state, &gesture, &timer));
    }
    let selected_after_drag = state.lock().unwrap().active().selected.len();

    assert_rows_match_selection(&ui, &state, label);

    // Holding the pointer below the list auto-scrolls; every tick grows the rectangle.
    let mut autoscroll_tick = Stats::default();
    let row_height = file_row_height(ViewMode::Details);
    for tick in 1..=AUTOSCROLL_TICKS {
        let pointer_y = ui.get_file_viewport_height() + 20.0;
        move_pointer(&gesture, x + 200.0, pointer_y);
        autoscroll_tick.time(|| {
            let mode = projected_view_mode(&ui);
            let maximum = projected_scroll_maximum(&ui, mode, ui.get_file_viewport_height());
            let viewport = (-(tick as f32) * 500.0 * row_height).clamp(-maximum, 0.0);
            ui.set_file_viewport_y(viewport);
            update_rectangle_selection(&ui, &state, &gesture, &timer)
        });
    }
    let selected_after_autoscroll = state.lock().unwrap().active().selected.len();
    assert_rows_match_selection(&ui, &state, label);
    RectangleProfile {
        group: label,
        hits,
        selection_update,
        patch_rows,
        pointer_move,
        autoscroll_tick,
        selected_after_drag,
        selected_after_autoscroll,
    }
}

struct ScrollProfile {
    fixture: &'static str,
    folder_sizes: Stats,
    images: Stats,
    shortcuts: Stats,
    handler: Stats,
    icon_result_patch: Stats,
    wheel_step: Stats,
    icon_requests: usize,
    folder_size_requests: usize,
    thumbnail_requests: usize,
    shortcut_requests: usize,
    requests_peak: usize,
    requests_limit: usize,
}

fn profile_scroll(group: GroupField, directory_every: u32, label: &'static str) -> ScrollProfile {
    let (state, ui) = synthetic_fixture(group, directory_every);
    let channels = Channels::new();
    let mut timing = [Stats::default(); 3];
    let mut handler = Stats::default();
    let mut icon_result_patch = Stats::default();
    let (mut icon_requests, mut folder_size_requests) = (0, 0);
    let (mut thumbnail_requests, mut shortcut_requests) = (0, 0);
    let maximum = projected_scroll_maximum(&ui, ViewMode::Details, ui.get_file_viewport_height());
    let requests_limit = 3
        * (ui
            .get_file_viewport_height()
            .max(file_row_height(ViewMode::Details))
            / file_row_height(ViewMode::Details))
        .ceil() as usize
        + 6;
    let requests_limit =
        3 * requests_limit + crate::domain::folder_size_scheduler::FOLDER_SIZE_SUBMIT_LIMIT;
    let mut requests_peak = 0;
    // A scrollbar drag from the top to the bottom of the list.
    for step in 1..=SCROLL_STEPS {
        ui.set_file_viewport_y(-maximum * step as f32 / SCROLL_STEPS as f32);
        handler.time(|| viewport_changed(&ui, &state, &channels, &mut timing));
        let (icons, folders, shortcuts, thumbnails) = channels.drain();
        let visible_limit =
            (requests_limit - crate::domain::folder_size_scheduler::FOLDER_SIZE_SUBMIT_LIMIT) / 3;
        assert!(
            thumbnails.len() <= visible_limit,
            "{label}: thumbnail request burst"
        );
        assert!(icons <= visible_limit, "{label}: icon request burst");
        assert!(
            shortcuts <= visible_limit,
            "{label}: shortcut request burst"
        );
        assert!(
            folders <= crate::domain::folder_size_scheduler::FOLDER_SIZE_SUBMIT_LIMIT,
            "{label}: folder-size request burst"
        );
        let requests = icons + folders + shortcuts + thumbnails.len();
        requests_peak = requests_peak.max(requests);
        assert!(
            requests <= requests_limit,
            "{label}: {requests} requests exceed visible-range budget {requests_limit}"
        );
        icon_requests += icons;
        folder_size_requests += folders;
        shortcut_requests += shortcuts;
        thumbnail_requests += thumbnails.len();
        // Each completed thumbnail patches its row on the UI thread.
        for key in thumbnails.iter().take(4) {
            icon_result_patch.time(|| {
                update_icon_row(
                    &ui,
                    &state.shared,
                    IconUpdate {
                        tab_id: key.tab_id,
                        entry_id: Some(key.entry_id),
                        type_key: None,
                    },
                )
            });
        }
    }
    let mut wheel_step = Stats::default();
    for _ in 0..WHEEL_STEPS {
        wheel_step.time(|| apply_file_scroll_delta(&ui, 96.0));
    }
    let [folder_sizes, images, shortcuts] = timing;
    ScrollProfile {
        fixture: label,
        folder_sizes,
        images,
        shortcuts,
        handler,
        icon_result_patch,
        wheel_step,
        icon_requests,
        folder_size_requests,
        thumbnail_requests,
        shortcut_requests,
        requests_peak,
        requests_limit,
    }
}

/// Header heights differ from the row height here, so any offset error shows up.
#[test]
fn issue_175_row_index_geometry_matches_a_full_walk() {
    i_slint_backend_testing::init_no_event_loop();
    const ROW: f32 = 30.0;
    let entry_row = |id: i32| FileRow {
        id,
        loaded: true,
        ..empty_file_row()
    };
    let mut rows = vec![group_header_file_row("a", "C:/a", 10)];
    rows.extend((1..=10).map(entry_row));
    rows.push(group_header_file_row("b", "", 5));
    rows.extend((11..=15).map(entry_row));
    rows.push(group_header_file_row("c", "C:/c", 3));
    rows.extend((16..=18).map(entry_row));
    let header_height =
        |row: &FileRow| group_header_height_for_detail(row.group_detail.as_str()) as f32;
    let heights = rows
        .iter()
        .map(|row| {
            if row.group_header {
                header_height(row)
            } else {
                ROW
            }
        })
        .collect::<Vec<_>>();
    let tops = heights
        .iter()
        .scan(0.0, |top, height| {
            let current = *top;
            *top += height;
            Some(current)
        })
        .collect::<Vec<_>>();
    let total = heights.iter().sum::<f32>();
    let layout = file_row_index::RowLayout::build(&rows, true, header_height);
    let model = Rc::new(VecModel::from(rows));
    file_row_index::install(&model, layout);

    assert_eq!(file_row_index::extent(&model, ROW), Some(total));
    for (index, top) in tops.iter().enumerate() {
        assert_eq!(file_row_index::top_of(&model, ROW, index), Some(*top));
    }
    let mut top = -40.0_f32;
    while top <= total + 40.0 {
        for span in [0.0, 1.0, 29.0, 47.0, 95.0] {
            let bottom = top + span;
            // The walk `list_icon_plan` used before #175.
            let expected = tops
                .iter()
                .zip(&heights)
                .enumerate()
                .filter(|(_, (row_top, height))| {
                    **row_top <= bottom && **row_top + **height >= top.max(0.0)
                })
                .map(|(index, _)| index)
                .collect::<Vec<_>>();
            let actual = file_row_index::rows_between(&model, ROW, top, bottom)
                .unwrap()
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "top={top} bottom={bottom}");
        }
        top += 0.5;
    }

    let tab = TabSession::new_home(TabId(1));
    let located = file_row_index::locate(&model, &tab, [EntryId(1), EntryId(15), EntryId(99)]);
    assert_eq!(located, vec![(EntryId(1), 1), (EntryId(15), 16)]);
    // A model changed behind the index falls back to a scan instead of patching wrong rows.
    model.insert(0, entry_row(99));
    assert_eq!(file_row_index::extent(&model, ROW), None);
    let mut located = file_row_index::locate(&model, &tab, [EntryId(1), EntryId(99)]);
    located.sort_by_key(|(_, row)| *row);
    assert_eq!(located, vec![(EntryId(99), 0), (EntryId(1), 2)]);
}

/// Batches that sort before existing rows are inserted at their final rows and stay indexed.
#[test]
fn issue_175_out_of_order_batches_keep_rows_in_pending_order() {
    i_slint_backend_testing::init_no_event_loop();
    for sort_direction in [SortDirection::Ascending, SortDirection::Descending] {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/")], 0, [0, 1, 2, 3]);
        app.update_directory_preference(PathBuf::from(SYNTHETIC_DIRECTORY), |view| {
            view.view_mode = ViewMode::Details;
            view.group_field = GroupField::None;
            view.sort_field = SortField::Name;
            view.sort_direction = sort_direction;
        });
        let window_id = app.active_window;
        let (request_id, _) = app
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(PathBuf::from(SYNTHETIC_DIRECTORY), NavigationKind::Normal);
        let state = WindowSessions::new(Arc::new(Mutex::new(app)), window_id);
        let ui = AppWindow::new().unwrap();
        ui.window().set_size(slint::LogicalSize::new(1180.0, 760.0));
        refresh_ui(&ui, &state);
        let named = |id: u32, rank: u32| {
            let mut entry = synthetic_entry(id, 0);
            let name = format!("item-{rank:06}.txt");
            entry.original_name = OsString::from(&name);
            entry.path = PathBuf::from(SYNTHETIC_DIRECTORY).join(&name);
            entry.display_name = name;
            entry
        };
        let batches = [
            (1..=200).map(|id| named(id, id * 3)).collect::<Vec<_>>(),
            // Interleaves before almost every existing row.
            (201..=400)
                .map(|id| named(id, (id - 200) * 3 - 1))
                .collect(),
            // A small batch in the middle, then a tail batch.
            (401..=410).map(|id| named(id, id - 300)).collect(),
            (411..=500).map(|id| named(id, 1000 + id)).collect(),
        ];
        for (step, entries) in batches.into_iter().enumerate() {
            apply_event(
                &state.shared,
                DirectoryEvent::Batch {
                    tab_id: TabId(1),
                    request_id,
                    entries,
                    acknowledgement: mpsc::channel().0,
                },
            );
            append_active_file_rows(&ui, &state.shared, TabId(1), request_id);
            let app = state.lock().unwrap();
            let expected = app
                .active()
                .visible_entries()
                .iter()
                .map(|entry| entry.id.0 as i32)
                .collect::<Vec<_>>();
            let actual = ui.get_files().iter().map(|row| row.id).collect::<Vec<_>>();
            assert_eq!(actual, expected, "batch {step}");
        }
        let files = ui.get_files();
        let model = files.as_any().downcast_ref::<VecModel<FileRow>>().unwrap();
        assert_eq!(file_row_index::projected_entries(model), Some(500));
        // Patches land on the selected rows only.
        let targets = [EntryId(7), EntryId(250), EntryId(405), EntryId(500)];
        let (tab_id, changed) = mutate_active_selection(&state, |tab| {
            for id in targets {
                tab.select_entry(id, true, false);
            }
        })
        .unwrap();
        update_file_rows(&ui, &state.shared, tab_id, &changed);
        let selected = ui
            .get_files()
            .iter()
            .filter(|row| row.selected)
            .map(|row| EntryId(row.id as u32))
            .collect::<HashSet<_>>();
        assert_eq!(selected, HashSet::from(targets));
    }
}

#[test]
fn issue_175_large_directory_load_selection_and_scroll_profile() {
    i_slint_backend_testing::init_no_event_loop();
    let directory = local_fixture();
    let load = profile_local_load(&directory);
    let rectangles = [
        profile_rectangle(GroupField::None, "none"),
        profile_rectangle(GroupField::Kind, "kind"),
    ];
    let scrolls = [
        profile_scroll(GroupField::None, 0, "files"),
        profile_scroll(GroupField::None, 10, "files_with_10pct_folders"),
        profile_scroll(GroupField::Kind, 0, "files_grouped_by_kind"),
    ];

    let mut report = String::new();
    write!(
        report,
        concat!(
            "{{\n  \"schema_version\": 2,\n  \"issue\": 175,\n  \"entries\": {},\n",
            "  \"view_mode\": \"details\",\n  \"native_window\": false,\n",
            "  \"load\": {{\n",
            "    \"directory\": {:?},\n",
            "    \"transport_only\": {{\"first_batch_ms\": {:.1}, \"total_ms\": {:.1}, \"batches\": {}}},\n",
            "    \"first_rows_ms\": {:.1},\n    \"complete_ms\": {:.1},\n",
            "    \"ui_waiting_for_helper\": {},\n",
            "    \"batch_merge\": {},\n    \"batch_append_projection\": {},\n",
            "    \"batch_visible_work\": {},\n    \"batch_ui_total\": {},\n",
            "    \"finish_merge_ms\": {:.1},\n    \"final_projection_ms\": {:.1},\n",
            "    \"finish_visible_work_ms\": {:.1},\n    \"icon_requests\": {}\n  }},\n",
            "  \"rectangle_selection\": ["
        ),
        LARGE,
        directory.display().to_string(),
        load.transport_first_ms,
        load.transport_total_ms,
        load.transport_batches,
        load.first_rows_ms,
        load.complete_ms,
        load.waiting.json(),
        load.merge.json(),
        load.append_projection.json(),
        load.batch_visible_work.json(),
        load.batch_ui_total.json(),
        load.finish_merge_ms,
        load.final_projection_ms,
        load.finish_visible_work_ms,
        load.icon_requests,
    )
    .unwrap();
    for (index, profile) in rectangles.iter().enumerate() {
        write!(
            report,
            concat!(
                "{}\n    {{\"group\": \"{}\", \"hits\": {}, \"selection_update\": {}, \"patch_focus_row\": {}, ",
                "\"pointer_move\": {}, \"autoscroll_tick\": {}, ",
                "\"selected_after_drag\": {}, \"selected_after_autoscroll\": {}}}"
            ),
            if index == 0 { "" } else { "," },
            profile.group,
            profile.hits.json(),
            profile.selection_update.json(),
            profile.patch_rows.json(),
            profile.pointer_move.json(),
            profile.autoscroll_tick.json(),
            profile.selected_after_drag,
            profile.selected_after_autoscroll,
        )
        .unwrap();
    }
    report.push_str("\n  ],\n  \"scroll\": [");
    for (index, profile) in scrolls.iter().enumerate() {
        write!(
            report,
            concat!(
                "{}\n    {{\"fixture\": \"{}\", \"viewport_changes\": {}, \"handler\": {}, ",
                "\"folder_sizes\": {}, \"images\": {}, \"shortcuts\": {}, ",
                "\"icon_result_patch\": {}, \"wheel_step\": {}, ",
                "\"requests\": {{\"icons\": {}, \"thumbnails\": {}, \"folder_sizes\": {}, ",
                "\"shortcuts\": {}, \"peak_per_viewport\": {}, \"limit_per_viewport\": {}}}}}"
            ),
            if index == 0 { "" } else { "," },
            profile.fixture,
            SCROLL_STEPS,
            profile.handler.json(),
            profile.folder_sizes.json(),
            profile.images.json(),
            profile.shortcuts.json(),
            profile.icon_result_patch.json(),
            profile.wheel_step.json(),
            profile.icon_requests,
            profile.thumbnail_requests,
            profile.folder_size_requests,
            profile.shortcut_requests,
            profile.requests_peak,
            profile.requests_limit,
        )
        .unwrap();
    }
    write!(
        report,
        concat!(
            "\n  ],\n  \"limits\": {{\"first_rows_ms\": {}, \"load_complete_ms\": {}, ",
            "\"batch_ui_peak_ms\": {}, \"pointer_move_peak_ms\": {}, ",
            "\"autoscroll_tick_peak_ms\": {}, \"viewport_change_peak_ms\": {}, ",
            "\"row_patch_and_wheel_peak_ms\": {}}}\n}}\n"
        ),
        FIRST_ROWS_LIMIT_MS,
        LOAD_COMPLETE_LIMIT_MS,
        BATCH_UI_PEAK_LIMIT_MS,
        POINTER_MOVE_PEAK_LIMIT_MS,
        AUTOSCROLL_TICK_PEAK_LIMIT_MS,
        VIEWPORT_CHANGE_PEAK_LIMIT_MS,
        ROW_PATCH_PEAK_LIMIT_MS,
    )
    .unwrap();
    let artifact = PathBuf::from("artifacts/perf/issue-175/latest.json");
    std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
    std::fs::write(&artifact, &report).unwrap();
    eprintln!("#175 {report}");

    assert!(load.first_rows_ms < FIRST_ROWS_LIMIT_MS, "{report}");
    assert!(load.complete_ms < LOAD_COMPLETE_LIMIT_MS, "{report}");
    assert!(
        millis(load.batch_ui_total.peak) < BATCH_UI_PEAK_LIMIT_MS,
        "{report}"
    );
    for profile in &rectangles {
        assert!(profile.selected_after_drag > 0, "{report}");
        assert!(
            profile.selected_after_autoscroll > profile.selected_after_drag,
            "{report}"
        );
        assert!(
            millis(profile.pointer_move.peak) < POINTER_MOVE_PEAK_LIMIT_MS,
            "{report}"
        );
        assert!(
            millis(profile.autoscroll_tick.peak) < AUTOSCROLL_TICK_PEAK_LIMIT_MS,
            "{report}"
        );
    }
    for profile in &scrolls {
        assert!(
            millis(profile.handler.peak) < VIEWPORT_CHANGE_PEAK_LIMIT_MS,
            "{report}"
        );
        assert!(
            millis(profile.icon_result_patch.peak) < ROW_PATCH_PEAK_LIMIT_MS,
            "{report}"
        );
        assert!(
            millis(profile.wheel_step.peak) < ROW_PATCH_PEAK_LIMIT_MS,
            "{report}"
        );
    }
}
