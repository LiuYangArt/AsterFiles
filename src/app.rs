use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    io,
    ops::Deref,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use slint::{
    Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{
    agent_debug::{self, AgentScenario},
    domain::{
        AddressMode, EntryId, FileEntry, FolderSizeState, LoadState, NameHighlightSegment,
        NavigationKind, PageSource, RectangleSelectionMode, RequestId, SearchDepth, SearchScope,
        SearchState, SortDirection, SortField, TabId, TabKind, TabSession, ViewMode,
        file_operations::{
            FileOperationKind, ItemState, OperationId, OperationItem, OperationManager,
            OperationResult, OperationState,
        },
    },
    fs::{ReadOutcome, read_directory_batches_filtered},
    i18n::{Language, Texts},
    platform::{self, KnownLocation, KnownLocationKind},
    session_store,
};

slint::include_modules!();

const WORKER_COUNT: usize = 4;
const ICON_WORKER_COUNT: usize = 2;

pub fn export_multi_window_state_layering(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from(r"C:\AgentScenarios\WindowA")],
        0,
        [0, 1, 2, 3],
        [0, 1, 2, 3],
        session_store::DEFAULT_COLUMN_WIDTHS,
        session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let first_window = app.active_window;
    let second_window = app.register_window(
        vec![PathBuf::from(r"C:\AgentScenarios\WindowB")],
        0,
        session_store::WindowPlacement {
            x: 160,
            y: 120,
            width: 1180,
            height: 760,
        },
    );
    let first_tab = app
        .window(first_window)
        .expect("first window exists")
        .active_tab;
    let second_tab = app
        .window(second_window)
        .expect("second window exists")
        .active_tab;
    let (_, first_cancel) = app
        .tab_mut(first_tab)
        .expect("first tab exists")
        .begin_navigation(
            PathBuf::from(r"C:\AgentScenarios\WindowA\Pending"),
            NavigationKind::Normal,
        );
    let operation = app.operations.submit(
        FileOperationKind::Copy,
        Some(first_tab),
        vec![OperationItem::pending(
            Some(PathBuf::from(r"C:\AgentScenarios\source.txt")),
            Some(PathBuf::from(r"C:\AgentScenarios\target.txt")),
        )],
    );
    let close_decision = app
        .close_window(first_window)
        .expect("first window can close");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"multi-window-state-layering\",\n",
            "  \"scope\": \"pure_state_no_second_native_window\",\n",
            "  \"window_ids\": [{}, {}],\n",
            "  \"tab_ids\": [{}, {}],\n",
            "  \"tab_ids_globally_unique\": {},\n",
            "  \"closed_window_request_cancelled\": {},\n",
            "  \"remaining_window_registered\": {},\n",
            "  \"shared_operation_survived\": {},\n",
            "  \"close_decision\": \"{}\"\n",
            "}}\n"
        ),
        first_window.0,
        second_window.0,
        first_tab.0,
        second_tab.0,
        first_tab != second_tab,
        first_cancel.load(std::sync::atomic::Ordering::Acquire),
        app.window(second_window).is_some(),
        app.operations.task(operation).is_some(),
        match close_decision {
            WindowCloseDecision::KeepRunning => "keep_running",
            WindowCloseDecision::ExitApplication => "exit_application",
        },
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

pub fn export_tab_reorder_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
        1,
        [0, 1, 2, 3],
        [0, 1, 2, 3],
        session_store::DEFAULT_COLUMN_WIDTHS,
        session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let window = app.active_window;
    let active = app.active_window_state().active_tab;
    let source = app.active_window_state().tab_order[0];
    let request_id = {
        let tab = app.tab_mut(source).expect("source tab exists");
        let (request_id, _) =
            tab.begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        request_id
    };
    app.begin_tab_drag(window, source, 0, 100.0, 20.0);
    let threshold_preserved = app
        .update_tab_drag(104.0, 20.0, 47.0, 540.0, 0.0, 178.0)
        .is_none()
        && app.active_window_state().tab_order == [TabId(1), TabId(2), TabId(3)];
    app.cancel_tab_drag();
    app.begin_tab_drag(window, source, 0, 100.0, 20.0);
    let target_slot = app
        .update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0)
        .expect("drag crosses threshold");
    let reordered = app.finish_tab_drag(true);
    let paths = app
        .stable_paths()
        .iter()
        .map(|value| format!("\"{}\"", value.display()))
        .collect::<Vec<_>>()
        .join(", ");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"tab-reorder\",\n",
            "  \"scope\": \"pure_state_no_native_pointer_capture\",\n",
            "  \"threshold_preserved_order\": {},\n",
            "  \"target_slot\": {},\n",
            "  \"reordered\": {},\n",
            "  \"tab_order\": [{}, {}, {}],\n",
            "  \"active_tab_unchanged\": {},\n",
            "  \"request_id_unchanged\": {},\n",
            "  \"session_paths\": [{}]\n",
            "}}\n"
        ),
        threshold_preserved,
        target_slot,
        reordered,
        app.active_window_state().tab_order[0].0,
        app.active_window_state().tab_order[1].0,
        app.active_window_state().tab_order[2].0,
        app.active_window_state().active_tab == active,
        app.tab(source)
            .is_some_and(|tab| tab.latest_request == request_id),
        paths,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

pub fn export_tab_detach_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from("a"), PathBuf::from("b")],
        0,
        [0, 1, 2, 3],
        [0, 1, 2, 3],
        session_store::DEFAULT_COLUMN_WIDTHS,
        session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let source_window = app.active_window;
    let tab_id = app.active_window_state().active_tab;
    let old_request = {
        let tab = app.tab_mut(tab_id).expect("detached tab exists");
        tab.begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh)
            .0
    };
    app.begin_tab_drag(source_window, tab_id, 0, 100.0, 20.0);
    app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
    let destination_window = app.reserve_window_id();
    let outcome = app
        .detach_dragged_tab_to_window(
            destination_window,
            session_store::WindowPlacement {
                x: 220,
                y: 180,
                width: 1180,
                height: 760,
            },
        )
        .expect("state transaction commits after destination readiness");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"tab-detach\",\n",
            "  \"scope\": \"state_transaction_native_window_visual_pending_manual_acceptance\",\n",
            "  \"source_window_id\": {},\n",
            "  \"destination_window_id\": {},\n",
            "  \"tab_id\": {},\n",
            "  \"single_owner_after_commit\": {},\n",
            "  \"source_order_after\": [{}],\n",
            "  \"destination_order\": [{}],\n",
            "  \"pending_request_cancelled\": {},\n",
            "  \"destination_request_restart_required\": {},\n",
            "  \"old_request_id\": {},\n",
            "  \"astf7_unchanged\": true\n",
            "}}\n"
        ),
        source_window.0,
        destination_window.0,
        tab_id.0,
        app.window_for_tab(tab_id) == Some(destination_window),
        app.window(source_window)
            .map(|window| window
                .tab_order
                .iter()
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(", "))
            .unwrap_or_default(),
        tab_id.0,
        matches!(outcome.restart, Some(DetachedTabRestart::Directory(_))),
        outcome.restart.is_some(),
        old_request.0,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

pub fn export_tab_cross_window_state(path: &Path) -> io::Result<()> {
    let mut app = AppState::new(
        vec![PathBuf::from("source-a"), PathBuf::from("source-b")],
        0,
        [0, 1, 2, 3],
        [0, 1, 2, 3],
        session_store::DEFAULT_COLUMN_WIDTHS,
        session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
        crate::domain::EverythingConfig::default(),
        session_store::ThemeMode::System,
        Language::Chinese,
        false,
    );
    let source_window = app.active_window;
    let destination = app.register_window(
        vec![PathBuf::from("target-a"), PathBuf::from("target-b")],
        0,
        session_store::WindowPlacement {
            x: 240,
            y: 120,
            width: 1180,
            height: 760,
        },
    );
    let tab_id = app.window(source_window).unwrap().tab_order[0];
    let request_before = app.tab(tab_id).unwrap().latest_request;
    app.begin_tab_drag(source_window, tab_id, 0, 100.0, 20.0);
    app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
    let outcome = app
        .move_dragged_tab_to_window(destination, 1)
        .expect("cross-window move commits");
    let destination_order = app
        .window(destination)
        .unwrap()
        .tab_order
        .iter()
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let state = format!(
        concat!(
            "{{\n",
            "  \"schema_version\": 1,\n",
            "  \"scenario\": \"tab-cross-window\",\n",
            "  \"scope\": \"pure_state_no_native_pointer_or_visual_validation\",\n",
            "  \"source_window\": {},\n",
            "  \"destination_window\": {},\n",
            "  \"tab_id\": {},\n",
            "  \"destination_order\": [{}],\n",
            "  \"inserted_at_target_slot\": {},\n",
            "  \"single_owner_after_commit\": {},\n",
            "  \"active_in_destination\": {},\n",
            "  \"request_identity_preserved_for_complete_tab\": {},\n",
            "  \"source_window_closed\": {},\n",
            "  \"astf7_unchanged\": true\n",
            "}}\n"
        ),
        source_window.0,
        destination.0,
        tab_id.0,
        destination_order,
        app.window(destination).unwrap().tab_order[1] == tab_id,
        app.window_for_tab(tab_id) == Some(destination),
        app.window(destination).unwrap().active_tab == tab_id,
        app.tab(tab_id).unwrap().latest_request == request_before,
        outcome.source_window_closed,
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, state)
}

type SharedSessions = Arc<Mutex<AppState>>;

#[derive(Clone)]
struct WorkerSenders {
    directory: mpsc::Sender<DirectoryRequest>,
    operation: mpsc::Sender<FileOperationRequest>,
    clipboard: mpsc::Sender<ClipboardRequest>,
    everything: mpsc::Sender<EverythingRequest>,
    icon: mpsc::Sender<IconRequest>,
}

#[derive(Clone)]
struct ConfirmationWindows {
    delete: slint::Weak<ConfirmationWindow>,
    conflict: slint::Weak<ConfirmationWindow>,
    exit: slint::Weak<ConfirmationWindow>,
}

impl ConfirmationWindows {
    fn new(
        delete: &ConfirmationWindow,
        conflict: &ConfirmationWindow,
        exit: &ConfirmationWindow,
    ) -> Self {
        Self {
            delete: delete.as_weak(),
            conflict: conflict.as_weak(),
            exit: exit.as_weak(),
        }
    }
}

#[derive(Clone)]
struct WindowSessions {
    shared: SharedSessions,
    window_id: WindowId,
}

impl WindowSessions {
    fn new(shared: SharedSessions, window_id: WindowId) -> Self {
        Self { shared, window_id }
    }

    fn lock(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, AppState>> {
        let mut app = self.shared.lock()?;
        if app.windows.contains_key(&self.window_id) {
            app.active_window = self.window_id;
        }
        Ok(app)
    }

    #[cfg(test)]
    fn peek(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, AppState>> {
        self.shared.lock()
    }
}

impl Deref for WindowSessions {
    type Target = SharedSessions;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

struct WindowRuntime {
    ui: AppWindow,
    _native_drop_timer: slint::Timer,
    _rectangle_selection_timer: Rc<slint::Timer>,
}

thread_local! {
    static WINDOW_RUNTIMES: RefCell<HashMap<WindowId, WindowRuntime>> = RefCell::new(HashMap::new());
}

fn native_tab_drag_image(
    ui: &AppWindow,
    state: &SharedSessions,
) -> Option<platform::windows::drag_drop::TabDragImage> {
    let scale = ui.window().scale_factor();
    let app = state.lock().ok()?;
    let drag = app.tab_drag?;
    let tab = app.tab(drag.tab_id)?;
    let icon = tab
        .visible_path()
        .and_then(|path| app.icon_cache.get(path))
        .map(|icon| (icon.width, icon.height, icon.pixels.clone()));
    let title = tab
        .visible_path()
        .and_then(Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| display_path(tab.visible_path().unwrap_or(Path::new("C:\\"))));
    Some(platform::windows::drag_drop::TabDragImage {
        title,
        icon,
        width_px: (ui.get_tab_current_width().max(80.0) * scale).round() as u32,
        height_px: (34.0 * scale).round() as u32,
        grab_x_px: ((drag.press_x
            - 47.0
            - ui.get_tab_viewport_x()
            - drag.source_index as f32 * (ui.get_tab_current_width() + 5.0))
            * scale)
            .round() as i32,
        dark: app.dark_theme(),
        active: app
            .window(drag.window_id)
            .is_some_and(|window| window.active_tab == drag.tab_id),
    })
}

fn screen_to_client_physical(screen_x: i32, screen_y: i32, left: i32, top: i32) -> (f64, f64) {
    (f64::from(screen_x - left), f64::from(screen_y - top))
}

fn grid_thumbnail_request_px(scale: f32) -> u32 {
    (100.0 * scale).round().max(64.0) as u32
}

fn grid_thumbnail_request_indices(
    entry_count: usize,
    columns: usize,
    viewport_y: f32,
    visible_height: f32,
) -> Vec<usize> {
    if entry_count == 0 {
        return Vec::new();
    }
    let columns = columns.max(1);
    let first_row = ((-viewport_y).max(0.0) / 148.0).floor() as usize;
    let visible_rows = (visible_height.max(148.0) / 148.0).ceil() as usize + 1;
    let first = first_row.saturating_sub(2) * columns;
    let last = ((first_row + visible_rows + 2) * columns).min(entry_count);
    (first..last).collect()
}

fn grid_thumbnail_requests(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
) -> Vec<IconRequest> {
    let requested_px = grid_thumbnail_request_px(ui.window().scale_factor());
    let columns = ui.get_grid_column_count().max(1) as usize;
    let visible_height = ui.window().size().height as f32 / ui.window().scale_factor()
        - ui.get_file_list_top()
        - 30.0;
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let Some(window) = app.window(window_id) else {
        return Vec::new();
    };
    let Some(tab) = window.tabs.get(&window.active_tab) else {
        return Vec::new();
    };
    if tab.kind != TabKind::Files
        || tab
            .visible_path()
            .is_none_or(|path| app.directory_view_modes.get(path) != Some(&ViewMode::Grid))
    {
        return Vec::new();
    }
    let entries = tab.visible_entries();
    let search_window = (tab.page_source == PageSource::Search).then(|| {
        search_window_for_scroll(
            ui.get_search_scroll_y(),
            tab.search_total.unwrap_or(0),
            ViewMode::Grid,
            columns,
        )
    });
    let projected_count = search_window.map_or(entries.len(), |window| window.len);
    let indices = grid_thumbnail_request_indices(
        projected_count,
        columns,
        ui.get_file_viewport_y() / 1.0,
        visible_height,
    );
    let requests = indices
        .into_iter()
        .filter_map(|index| {
            let entry = if let Some(window) = search_window {
                let id = window.start.checked_add(index as u32)?.checked_add(1)?;
                tab.visible_entry(EntryId(id))?
            } else {
                entries.get(index)?
            };
            (!app
                .thumbnail_cache
                .contains_key(&(entry.path.clone(), requested_px))
                && !app
                    .large_icon_cache
                    .contains_key(&(entry.path.clone(), requested_px))
                && !app.thumbnail_requests.contains(&(
                    tab.id,
                    tab.latest_request,
                    entry.path.clone(),
                    requested_px,
                )))
            .then(|| IconRequest {
                tab_id: tab.id,
                request_id: tab.latest_request,
                target: IconTarget::Entry(entry.id),
                path: entry.path.clone(),
                thumbnail: true,
                requested_px,
            })
        })
        .collect::<Vec<_>>();
    for request in &requests {
        app.thumbnail_requests.insert((
            request.tab_id,
            request.request_id,
            request.path.clone(),
            request.requested_px,
        ));
    }
    requests
}

fn request_grid_thumbnails(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    sender: &mpsc::Sender<IconRequest>,
) {
    for request in grid_thumbnail_requests(ui, state, window_id) {
        let _ = sender.send(request);
    }
}

fn refresh_all_windows(state: &SharedSessions) {
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .map(|(id, runtime)| (*id, runtime.ui.clone_strong()))
            .collect::<Vec<_>>()
    });
    for (window_id, ui) in windows {
        if state
            .lock()
            .is_ok_and(|app| app.window(window_id).is_some())
        {
            refresh_window_ui(&ui, state, window_id);
        }
    }
}

fn refresh_tab_window(state: &SharedSessions, tab_id: TabId) {
    let window_id = state.lock().ok().and_then(|app| app.window_for_tab(tab_id));
    if let Some(window_id) = window_id
        && let Some(ui) = window_ui(window_id)
    {
        refresh_window_ui(&ui, state, window_id);
    }
}

fn window_ui(window_id: WindowId) -> Option<AppWindow> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .get(&window_id)
            .map(|runtime| runtime.ui.clone_strong())
    })
}

fn cross_window_drop_target(
    source_window: WindowId,
    screen_x: i32,
    screen_y: i32,
    state: &SharedSessions,
) -> Option<(WindowId, usize)> {
    let screen_x = f64::from(screen_x);
    let screen_y = f64::from(screen_y);
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes.iter().find_map(|(window_id, runtime)| {
            if *window_id == source_window {
                return None;
            }
            let (target_left, target_top, target_right, target_bottom) =
                platform::windows::drag_drop::client_screen_rect(native_window_handle(&runtime.ui))
                    .ok()?;
            if screen_x < f64::from(target_left)
                || screen_x >= f64::from(target_right)
                || screen_y < f64::from(target_top)
                || screen_y >= f64::from(target_bottom)
            {
                return None;
            }
            let (x, y) = physical_client_to_logical(
                screen_x,
                screen_y,
                target_left,
                target_top,
                runtime.ui.window().scale_factor(),
            );
            if !(0.0..=96.0).contains(&y) {
                return None;
            }
            let file_boundary = state.lock().ok().and_then(|app| {
                let window = app.window(*window_id)?;
                Some(
                    window
                        .tab_order
                        .iter()
                        .position(|id| {
                            window
                                .tabs
                                .get(id)
                                .is_some_and(|tab| tab.kind == TabKind::Settings)
                        })
                        .unwrap_or(window.tab_order.len()),
                )
            })?;
            let slot = external_tab_insertion_slot(
                x,
                47.0,
                runtime.ui.get_tab_strip_width(),
                runtime.ui.get_tab_viewport_x(),
                runtime.ui.get_tab_current_width(),
                file_boundary,
            )
            .or_else(|| (x >= 0.0 && x <= runtime.ui.get_window_width()).then_some(file_boundary));
            slot.map(|slot| (*window_id, slot))
        })
    })
}

fn source_tab_drop_is_valid(
    source_ui: &AppWindow,
    screen_x: i32,
    screen_y: i32,
) -> Option<(bool, winit::dpi::PhysicalPosition<f64>)> {
    let (left, top, right, _) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(source_ui)).ok()?;
    let (client_x, client_y) = screen_to_client_physical(screen_x, screen_y, left, top);
    let logical = winit::dpi::PhysicalPosition::new(client_x, client_y)
        .to_logical::<f32>(f64::from(source_ui.window().scale_factor()));
    let strip_x = 47.0;
    let strip_right = strip_x + source_ui.get_tab_strip_width();
    let valid = screen_x >= left
        && screen_x < right
        && (0.0..=46.0).contains(&logical.y)
        && (strip_x..=strip_right).contains(&logical.x);
    Some((valid, winit::dpi::PhysicalPosition::new(client_x, client_y)))
}

fn physical_client_to_logical(
    screen_x: f64,
    screen_y: f64,
    client_left: i32,
    client_top: i32,
    scale: f32,
) -> (f32, f32) {
    (
        ((screen_x - f64::from(client_left)) / f64::from(scale)) as f32,
        ((screen_y - f64::from(client_top)) / f64::from(scale)) as f32,
    )
}

fn insertion_indicator_screen_rect(
    ui: &AppWindow,
    insertion_index: usize,
) -> Option<(i32, i32, i32, i32)> {
    let (left, top, _, _) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(ui)).ok()?;
    let scale = ui.window().scale_factor();
    let strip_width = ui.get_tab_strip_width();
    let logical_x = (47.0
        + ui.get_tab_viewport_x()
        + if insertion_index == 0 {
            0.0
        } else {
            insertion_index as f32 * (ui.get_tab_current_width() + 5.0) - 5.0
        })
    .clamp(47.0, 47.0 + strip_width - 5.0);
    Some((
        left + (logical_x * scale).round() as i32,
        top + (12.0 * scale).round() as i32,
        (5.0 * scale).round().max(1.0) as i32,
        (34.0 * scale).round().max(1.0) as i32,
    ))
}

fn project_native_insertion_indicator(target: Option<(WindowId, usize)>, state: &SharedSessions) {
    let dark_theme = state.lock().ok().is_some_and(|app| app.dark_theme());
    let projected = target.and_then(|(window_id, insertion_index)| {
        let ui = window_ui(window_id)?;
        let (x, y, width, height) = insertion_indicator_screen_rect(&ui, insertion_index)?;
        platform::windows::tab_insertion_indicator::show(x, y, width, height, dark_theme).ok()?;
        Some(())
    });
    if projected.is_none() {
        platform::windows::tab_insertion_indicator::hide();
    }
}

fn project_native_tab_target(
    window_id: WindowId,
    event: platform::windows::drag_drop::TabTargetEvent,
    state: &SharedSessions,
) {
    let Some(ui) = window_ui(window_id) else {
        return;
    };
    let source_hover = matches!(
        event,
        platform::windows::drag_drop::TabTargetEvent::Hover { .. }
    ) && state
        .lock()
        .ok()
        .and_then(|app| app.tab_drag.map(|drag| drag.window_id))
        == Some(window_id);
    let target = match event {
        platform::windows::drag_drop::TabTargetEvent::Hover { screen_x, screen_y } => {
            let source_window = state
                .lock()
                .ok()
                .and_then(|app| app.tab_drag.map(|drag| drag.window_id));
            source_window.and_then(|source_window| {
                if source_window == window_id {
                    let (left, top, _, _) =
                        platform::windows::drag_drop::client_screen_rect(native_window_handle(&ui))
                            .ok()?;
                    let (x, y) = physical_client_to_logical(
                        f64::from(screen_x),
                        f64::from(screen_y),
                        left,
                        top,
                        ui.window().scale_factor(),
                    );
                    let insertion = state.lock().ok().and_then(|mut app| {
                        app.update_tab_drag(
                            x,
                            y,
                            47.0,
                            ui.get_tab_strip_width(),
                            ui.get_tab_viewport_x(),
                            ui.get_tab_current_width(),
                        )
                    });
                    project_native_insertion_indicator(
                        insertion.map(|index| (window_id, index)),
                        state,
                    );
                    None
                } else {
                    cross_window_drop_target(source_window, screen_x, screen_y, state)
                }
            })
        }
        platform::windows::drag_drop::TabTargetEvent::Leave
        | platform::windows::drag_drop::TabTargetEvent::Drop { .. } => None,
    };
    if !source_hover {
        project_native_insertion_indicator(target, state);
    }
}

fn remove_window_runtime(window_id: WindowId) {
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        if let Some(runtime) = runtimes.remove(&window_id) {
            platform::windows::drag_drop::revoke(native_window_handle(&runtime.ui));
        }
    });
}

fn clear_window_runtimes() {
    platform::windows::tab_insertion_indicator::destroy();
    WINDOW_RUNTIMES.with_borrow_mut(HashMap::clear);
}

fn hide_all_app_windows() {
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .values()
            .map(|runtime| runtime.ui.clone_strong())
            .collect::<Vec<_>>()
    });
    for window in windows {
        let _ = window.hide();
    }
}

fn open_task_center_on_live_window(state: &SharedSessions) {
    let preferred = state.lock().ok().map(|app| app.active_window);
    let target = preferred.and_then(window_ui).or_else(|| {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .values()
                .next()
                .map(|runtime| runtime.ui.clone_strong())
        })
    });
    if let Some(ui) = target {
        ui.set_task_center_open(true);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct WindowId(u32);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFocus {
    directory: PathBuf,
    request_id: Option<RequestId>,
    paths: Vec<PathBuf>,
}

#[derive(Debug)]
struct WindowState {
    tabs: HashMap<TabId, TabSession>,
    tab_order: Vec<TabId>,
    active_tab: TabId,
    closed_tabs: VecDeque<PathBuf>,
    placement: session_store::WindowPlacement,
}

fn movable_tab_range(window: &WindowState, source_index: usize) -> Option<std::ops::Range<usize>> {
    window.tab_order.get(source_index)?;
    let start = window.tab_order[..source_index]
        .iter()
        .rposition(|id| {
            window
                .tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        })
        .map_or(0, |index| index + 1);
    let end = window.tab_order[source_index + 1..]
        .iter()
        .position(|id| {
            window
                .tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        })
        .map_or(window.tab_order.len(), |index| source_index + 1 + index);
    Some(start..end)
}

fn tab_insertion_slot(
    pointer_x: f32,
    strip_x: f32,
    strip_width: f32,
    viewport_x: f32,
    tab_width: f32,
    range: std::ops::Range<usize>,
) -> Option<usize> {
    if pointer_x < strip_x || pointer_x > strip_x + strip_width || range.is_empty() {
        return None;
    }
    let pitch = tab_width + 5.0;
    let range_x = pointer_x - strip_x - viewport_x - range.start as f32 * pitch;
    let remaining = range.len().saturating_sub(1);
    if range_x <= tab_width / 2.0 {
        return Some(range.start);
    }
    for slot in 1..remaining {
        let midpoint = slot as f32 * pitch + tab_width / 2.0;
        if range_x < midpoint {
            return Some(range.start + slot);
        }
    }
    Some(range.start + remaining)
}

fn external_tab_insertion_slot(
    pointer_x: f32,
    strip_x: f32,
    strip_width: f32,
    viewport_x: f32,
    tab_width: f32,
    tab_count: usize,
) -> Option<usize> {
    if pointer_x < strip_x || pointer_x > strip_x + strip_width {
        return None;
    }
    if tab_count == 0 {
        return Some(0);
    }
    let pitch = tab_width + 5.0;
    let content_x = pointer_x - strip_x - viewport_x;
    for index in 0..tab_count {
        if content_x < index as f32 * pitch + tab_width / 2.0 {
            return Some(index);
        }
    }
    Some(tab_count)
}

const TAB_DRAG_THRESHOLD: f32 = 6.0;

#[derive(Debug, Clone, Copy, PartialEq)]
enum TabDragPhase {
    Pressed,
    Dragging { insertion_index: Option<usize> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DetachedTabRestart {
    Directory(PathBuf),
    Search {
        scope: SearchScope,
        depth: SearchDepth,
        query: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DetachedTabOutcome {
    source_window: WindowId,
    destination_window: WindowId,
    tab_id: TabId,
    source_index: usize,
    source_placement: session_store::WindowPlacement,
    source_closed_tabs: VecDeque<PathBuf>,
    source_active_tab: TabId,
    source_window_closed: bool,
    restart: Option<DetachedTabRestart>,
}

fn detached_tab_restart(tab: &mut TabSession) -> Option<DetachedTabRestart> {
    match tab.page_source {
        PageSource::Search
            if matches!(
                tab.search_state,
                SearchState::Searching | SearchState::Partial
            ) =>
        {
            let restart = DetachedTabRestart::Search {
                scope: tab.search_scope.clone(),
                depth: tab.search_depth,
                query: tab.search_query.clone(),
            };
            tab.cancel_pending();
            Some(restart)
        }
        _ if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) => {
            let path = tab.visible_path().map(Path::to_path_buf);
            tab.cancel_pending();
            path.map(DetachedTabRestart::Directory)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct TabDragSession {
    window_id: WindowId,
    tab_id: TabId,
    source_index: usize,
    press_x: f32,
    press_y: f32,
    phase: TabDragPhase,
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCloseDecision {
    KeepRunning,
    ExitApplication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowCloseAction {
    CloseWindow,
    ExitApplication,
    ConfirmApplicationExit,
    Ignore,
}

#[derive(Debug)]
struct AppState {
    windows: HashMap<WindowId, WindowState>,
    active_window: WindowId,
    #[cfg_attr(not(test), allow(dead_code))]
    next_window_id: u32,
    next_tab_id: u32,
    language: Language,
    theme_mode: session_store::ThemeMode,
    file_visibility: crate::domain::FileVisibility,
    system_dark_theme: bool,
    icons: HashMap<(TabId, RequestId, EntryId), platform::windows_shell_icons::ShellIconRgba>,
    icon_cache: HashMap<PathBuf, platform::windows_shell_icons::ShellIconRgba>,
    sidebar_icons: HashMap<PathBuf, platform::windows_shell_icons::ShellIconRgba>,
    thumbnail_cache: HashMap<(PathBuf, u32), platform::windows_shell_icons::ShellIconRgba>,
    large_icon_cache: HashMap<(PathBuf, u32), platform::windows_shell_icons::ShellIconRgba>,
    thumbnail_requests: std::collections::HashSet<(TabId, RequestId, PathBuf, u32)>,
    sidebar: Vec<KnownLocation>,
    directory_view_modes: HashMap<PathBuf, crate::domain::ViewMode>,
    column_order: [u8; 4],
    column_widths: session_store::ColumnWidths,
    operations: OperationManager,
    operation_errors: Vec<String>,
    rename_target: Option<(TabId, EntryId)>,
    rename_extension: Option<std::ffi::OsString>,
    focus_after_refresh: HashMap<TabId, PendingFocus>,
    pending_permanent_delete: Vec<OperationItem>,
    exit_after_cancel: bool,
    clipboard_has_files: bool,
    cut_paths: Vec<PathBuf>,
    cut_generation: u64,
    conflict_responses:
        HashMap<OperationId, mpsc::Sender<crate::domain::file_operations::ConflictDecision>>,
    search_column_order: [u8; 4],
    search_column_widths: session_store::SearchColumnWidths,
    everything_config: crate::domain::EverythingConfig,
    everything_status: String,
    everything_folder_sizes_indexed: Option<bool>,
    pending_right_drop: Option<platform::windows::drag_drop::DropIntent>,
    tab_drag: Option<TabDragSession>,
}

impl AppState {
    fn active_window_state(&self) -> &WindowState {
        self.windows
            .get(&self.active_window)
            .expect("active window state exists")
    }

    fn active_window_state_mut(&mut self) -> &mut WindowState {
        self.windows
            .get_mut(&self.active_window)
            .expect("active window state exists")
    }
    fn tab(&self, tab_id: TabId) -> Option<&TabSession> {
        self.windows
            .values()
            .find_map(|window| window.tabs.get(&tab_id))
    }

    fn tab_mut(&mut self, tab_id: TabId) -> Option<&mut TabSession> {
        self.windows
            .values_mut()
            .find_map(|window| window.tabs.get_mut(&tab_id))
    }

    fn window_for_tab(&self, tab_id: TabId) -> Option<WindowId> {
        self.windows
            .iter()
            .find_map(|(id, window)| window.tabs.contains_key(&tab_id).then_some(*id))
    }

    #[cfg(test)]
    fn new_for_test(
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        column_order: [u8; 4],
    ) -> Self {
        Self::new(
            initial_paths,
            active_index,
            column_order,
            [0, 1, 2, 3],
            session_store::DEFAULT_COLUMN_WIDTHS,
            session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
            crate::domain::EverythingConfig::default(),
            session_store::ThemeMode::System,
            Language::Chinese,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        column_order: [u8; 4],
        search_column_order: [u8; 4],
        column_widths: session_store::ColumnWidths,
        search_column_widths: session_store::SearchColumnWidths,
        everything_config: crate::domain::EverythingConfig,
        theme_mode: session_store::ThemeMode,
        language: Language,
        system_dark_theme: bool,
    ) -> Self {
        let initial_paths = if initial_paths.is_empty() {
            vec![initial_path()]
        } else {
            initial_paths
        };
        let active_window = WindowId(1);
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        let mut next_tab_id = 1;
        for path in initial_paths {
            let id = TabId(next_tab_id);
            next_tab_id += 1;
            let mut tab = TabSession::new(id);
            tab.current_path = Some(path);
            tabs.insert(id, tab);
            tab_order.push(id);
        }
        let active_tab = tab_order[active_index.min(tab_order.len() - 1)];
        let window = WindowState {
            tabs,
            tab_order,
            active_tab,
            closed_tabs: VecDeque::new(),
            placement: session_store::WindowPlacement {
                x: 80,
                y: 80,
                width: 1180,
                height: 760,
            },
        };
        Self {
            windows: HashMap::from([(active_window, window)]),
            active_window,
            next_window_id: 2,
            next_tab_id,
            language,
            theme_mode,
            file_visibility: crate::domain::FileVisibility::default(),
            system_dark_theme,
            icons: HashMap::new(),
            icon_cache: HashMap::new(),
            sidebar_icons: HashMap::new(),
            thumbnail_cache: HashMap::new(),
            large_icon_cache: HashMap::new(),
            thumbnail_requests: std::collections::HashSet::new(),
            sidebar: Vec::new(),
            directory_view_modes: HashMap::new(),
            column_order,
            column_widths,
            operations: OperationManager::new(),
            operation_errors: Vec::new(),
            rename_target: None,
            rename_extension: None,
            focus_after_refresh: HashMap::new(),
            pending_permanent_delete: Vec::new(),
            exit_after_cancel: false,
            clipboard_has_files: false,
            cut_paths: Vec::new(),
            cut_generation: 0,
            conflict_responses: HashMap::new(),
            search_column_order,
            search_column_widths,
            everything_config,
            everything_status: String::new(),
            everything_folder_sizes_indexed: None,
            pending_right_drop: None,
            tab_drag: None,
        }
    }

    fn allocate_tab_id(&mut self) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id = self
            .next_tab_id
            .checked_add(1)
            .expect("tab identity space is exhausted");
        id
    }

    #[cfg_attr(not(test), allow(dead_code))]
    fn register_window(
        &mut self,
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        placement: session_store::WindowPlacement,
    ) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window identity space is exhausted");
        let paths = if initial_paths.is_empty() {
            vec![initial_path()]
        } else {
            initial_paths
        };
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        for path in paths {
            let tab_id = self.allocate_tab_id();
            let mut tab = TabSession::new(tab_id);
            tab.current_path = Some(path);
            tabs.insert(tab_id, tab);
            tab_order.push(tab_id);
        }
        let active_tab = tab_order[active_index.min(tab_order.len() - 1)];
        self.windows.insert(
            id,
            WindowState {
                tabs,
                tab_order,
                active_tab,
                closed_tabs: VecDeque::new(),
                placement,
            },
        );
        id
    }

    fn reserve_window_id(&mut self) -> WindowId {
        let id = WindowId(self.next_window_id);
        self.next_window_id = self
            .next_window_id
            .checked_add(1)
            .expect("window identity space is exhausted");
        id
    }

    fn detach_dragged_tab_to_window(
        &mut self,
        destination_window: WindowId,
        placement: session_store::WindowPlacement,
    ) -> Option<DetachedTabOutcome> {
        let drag = self.tab_drag?;
        if !matches!(drag.phase, TabDragPhase::Dragging { .. })
            || self.windows.contains_key(&destination_window)
        {
            return None;
        }
        let source = self.windows.get(&drag.window_id)?;
        let source_placement = source.placement;
        let source_closed_tabs = source.closed_tabs.clone();
        let source_active_tab = source.active_tab;
        if source.tab_order.get(drag.source_index) != Some(&drag.tab_id)
            || source
                .tabs
                .get(&drag.tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return None;
        }

        let mut tab = self
            .windows
            .get_mut(&drag.window_id)?
            .tabs
            .remove(&drag.tab_id)?;
        self.windows
            .get_mut(&drag.window_id)?
            .tab_order
            .remove(drag.source_index);
        let restart = detached_tab_restart(&mut tab);
        self.windows.insert(
            destination_window,
            WindowState {
                tabs: HashMap::from([(drag.tab_id, tab)]),
                tab_order: vec![drag.tab_id],
                active_tab: drag.tab_id,
                closed_tabs: VecDeque::new(),
                placement,
            },
        );

        let source_has_tabs = self
            .windows
            .get(&drag.window_id)
            .is_some_and(|window| !window.tab_order.is_empty());
        let source_window_closed = if source_has_tabs {
            let source = self.windows.get_mut(&drag.window_id)?;
            if source.active_tab == drag.tab_id {
                source.active_tab = *source.tab_order.first()?;
            }
            false
        } else {
            self.windows.remove(&drag.window_id);
            true
        };
        self.active_window = destination_window;
        self.tab_drag = None;
        Some(DetachedTabOutcome {
            source_window: drag.window_id,
            destination_window,
            tab_id: drag.tab_id,
            source_index: drag.source_index,
            source_placement,
            source_closed_tabs,
            source_active_tab,
            source_window_closed,
            restart,
        })
    }

    fn move_dragged_tab_to_window(
        &mut self,
        destination_window: WindowId,
        insertion_index: usize,
    ) -> Option<DetachedTabOutcome> {
        let drag = self.tab_drag?;
        if drag.window_id == destination_window || !self.windows.contains_key(&destination_window) {
            return None;
        }
        let source = self.windows.get(&drag.window_id)?;
        if source.tab_order.get(drag.source_index) != Some(&drag.tab_id)
            || source
                .tabs
                .get(&drag.tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return None;
        }
        let destination = self.windows.get(&destination_window)?;
        let movable_end = destination
            .tab_order
            .iter()
            .position(|id| {
                destination
                    .tabs
                    .get(id)
                    .is_some_and(|tab| tab.kind == TabKind::Settings)
            })
            .unwrap_or(destination.tab_order.len());
        let insertion_index = insertion_index.min(movable_end);

        let source_placement = source.placement;
        let source_closed_tabs = source.closed_tabs.clone();
        let source_active_tab = source.active_tab;
        let mut tab = self
            .windows
            .get_mut(&drag.window_id)?
            .tabs
            .remove(&drag.tab_id)?;
        self.windows
            .get_mut(&drag.window_id)?
            .tab_order
            .remove(drag.source_index);
        let restart = detached_tab_restart(&mut tab);
        let destination = self.windows.get_mut(&destination_window)?;
        destination.tabs.insert(drag.tab_id, tab);
        destination.tab_order.insert(insertion_index, drag.tab_id);
        destination.active_tab = drag.tab_id;

        let source_has_tabs = self
            .windows
            .get(&drag.window_id)
            .is_some_and(|window| !window.tab_order.is_empty());
        let source_window_closed = if source_has_tabs {
            let source = self.windows.get_mut(&drag.window_id)?;
            if source.active_tab == drag.tab_id {
                source.active_tab = *source.tab_order.first()?;
            }
            false
        } else {
            self.windows.remove(&drag.window_id);
            true
        };
        self.active_window = destination_window;
        self.tab_drag = None;
        Some(DetachedTabOutcome {
            source_window: drag.window_id,
            destination_window,
            tab_id: drag.tab_id,
            source_index: drag.source_index,
            source_placement,
            source_closed_tabs,
            source_active_tab,
            source_window_closed,
            restart,
        })
    }

    fn window(&self, id: WindowId) -> Option<&WindowState> {
        self.windows.get(&id)
    }

    #[cfg(test)]
    fn window_mut(&mut self, id: WindowId) -> Option<&mut WindowState> {
        self.windows.get_mut(&id)
    }

    fn close_window(&mut self, id: WindowId) -> Option<WindowCloseDecision> {
        let mut window = self.windows.remove(&id)?;
        for tab in window.tabs.values_mut() {
            tab.cancel_pending();
        }
        self.icons
            .retain(|(tab_id, _, _), _| !window.tabs.contains_key(tab_id));
        self.focus_after_refresh
            .retain(|tab_id, _| !window.tabs.contains_key(tab_id));
        self.rename_target = self
            .rename_target
            .filter(|(tab_id, _)| !window.tabs.contains_key(tab_id));
        if self.windows.is_empty() {
            return Some(WindowCloseDecision::ExitApplication);
        }
        if self.active_window == id {
            self.active_window = *self.windows.keys().min_by_key(|id| id.0)?;
        }
        Some(WindowCloseDecision::KeepRunning)
    }

    fn request_window_close(&self, id: WindowId) -> WindowCloseAction {
        if !self.windows.contains_key(&id) {
            return WindowCloseAction::Ignore;
        }
        if self.windows.len() > 1 {
            WindowCloseAction::CloseWindow
        } else if self.operations.has_active_tasks() {
            WindowCloseAction::ConfirmApplicationExit
        } else {
            WindowCloseAction::ExitApplication
        }
    }

    fn begin_tab_drag(
        &mut self,
        window_id: WindowId,
        tab_id: TabId,
        source_index: usize,
        press_x: f32,
        press_y: f32,
    ) -> bool {
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        if window.tab_order.len() <= 1
            || window.tab_order.get(source_index) != Some(&tab_id)
            || window
                .tabs
                .get(&tab_id)
                .is_none_or(|tab| tab.kind != TabKind::Files)
        {
            return false;
        }
        self.tab_drag = Some(TabDragSession {
            window_id,
            tab_id,
            source_index,
            press_x,
            press_y,
            phase: TabDragPhase::Pressed,
        });
        true
    }

    fn update_tab_drag(
        &mut self,
        pointer_x: f32,
        pointer_y: f32,
        strip_x: f32,
        strip_width: f32,
        viewport_x: f32,
        tab_width: f32,
    ) -> Option<usize> {
        let mut drag = self.tab_drag?;
        if matches!(drag.phase, TabDragPhase::Pressed)
            && (pointer_x - drag.press_x).hypot(pointer_y - drag.press_y) < TAB_DRAG_THRESHOLD
        {
            return None;
        }
        let window = self.windows.get(&drag.window_id)?;
        let range = movable_tab_range(window, drag.source_index)?;
        let insertion_index = tab_insertion_slot(
            pointer_x,
            strip_x,
            strip_width,
            viewport_x,
            tab_width,
            range,
        );
        drag.phase = TabDragPhase::Dragging { insertion_index };
        self.tab_drag = Some(drag);
        ((0.0..=46.0).contains(&pointer_y))
            .then_some(insertion_index)
            .flatten()
    }

    fn finish_tab_drag(&mut self, valid_release: bool) -> bool {
        let Some(drag) = self.tab_drag.take() else {
            return false;
        };
        let TabDragPhase::Dragging {
            insertion_index: Some(insertion_index),
        } = drag.phase
        else {
            return false;
        };
        if !valid_release {
            return false;
        }
        let Some(window) = self.windows.get_mut(&drag.window_id) else {
            return false;
        };
        if window.tab_order.get(drag.source_index) != Some(&drag.tab_id) {
            return false;
        }
        let moved = window.tab_order.remove(drag.source_index);
        let target = insertion_index.min(window.tab_order.len());
        window.tab_order.insert(target, moved);
        target != drag.source_index
    }

    fn cancel_tab_drag(&mut self) -> bool {
        self.tab_drag.take().is_some()
    }

    fn cancel_tab_drag_for_window(&mut self, window_id: WindowId) -> bool {
        if self
            .tab_drag
            .is_some_and(|drag| drag.window_id == window_id)
        {
            self.cancel_tab_drag()
        } else {
            false
        }
    }

    fn duplicate_active_tab(&mut self) -> Option<TabId> {
        let source_id = self.active_window_state().active_tab;
        let id = self.allocate_tab_id();
        let tab = {
            let source = self.active_window_state().tabs.get(&source_id)?;
            if source.kind != TabKind::Files || source.load_state != LoadState::Complete {
                return None;
            }
            TabSession::duplicate_complete(id, source)
        };
        let window = self.active_window_state_mut();
        window.tabs.insert(id, tab);
        window.tab_order.push(id);
        window.active_tab = id;
        Some(id)
    }
    fn create_tab(&mut self, path: PathBuf) -> TabId {
        let id = self.allocate_tab_id();
        let mut tab = TabSession::new(id);
        tab.current_path = Some(path);
        let window = self.active_window_state_mut();
        window.tabs.insert(id, tab);
        window.tab_order.push(id);
        window.active_tab = id;
        id
    }

    fn open_settings(&mut self) -> TabId {
        if let Some(id) = self
            .active_window_state()
            .tab_order
            .iter()
            .copied()
            .find(|id| {
                self.active_window_state()
                    .tabs
                    .get(id)
                    .is_some_and(|tab| tab.kind == TabKind::Settings)
            })
        {
            self.active_window_state_mut().active_tab = id;
            return id;
        }
        let id = self.allocate_tab_id();
        let window = self.active_window_state_mut();
        window.tabs.insert(id, TabSession::new_settings(id));
        window.tab_order.push(id);
        window.active_tab = id;
        id
    }

    fn close_tab(&mut self, closing: TabId) -> Option<TabId> {
        if self.active_window_state().tab_order.len() == 1 {
            return None;
        }
        let closing_kind = self.active_window_state().tabs.get(&closing)?.kind;
        if closing_kind == TabKind::Files
            && self
                .active_window_state()
                .tabs
                .values()
                .filter(|tab| tab.kind == TabKind::Files)
                .count()
                == 1
        {
            return None;
        }
        let index = self
            .active_window_state()
            .tab_order
            .iter()
            .position(|id| *id == closing)?;
        let closing_was_active = closing == self.active_window_state().active_tab;
        let removed = self.active_window_state_mut().tabs.remove(&closing);
        if let Some(mut tab) = removed {
            tab.cancel_pending();
            self.icons.retain(|(tab_id, _, _), _| *tab_id != closing);
            if tab.kind == TabKind::Files
                && let Some(path) = tab.current_path.take()
            {
                let window = self.active_window_state_mut();
                window.closed_tabs.push_front(path);
                window.closed_tabs.truncate(10);
            }
        }
        self.active_window_state_mut().tab_order.remove(index);
        if closing_was_active {
            let window = self.active_window_state_mut();
            window.active_tab = window.tab_order[index.min(window.tab_order.len() - 1)];
        }
        Some(self.active_window_state().active_tab)
    }

    fn restore_closed(&mut self) -> Option<(TabId, PathBuf)> {
        let path = self.active_window_state_mut().closed_tabs.pop_front()?;
        let tab_id = self.create_tab(path.clone());
        Some((tab_id, path))
    }

    fn active(&self) -> &TabSession {
        self.active_window_state().active()
    }

    fn stable_paths(&self) -> Vec<PathBuf> {
        self.active_window_state().stable_paths()
    }

    #[cfg(test)]
    fn stable_active_path_index(&self) -> usize {
        self.active_window_state().stable_active_path_index()
    }

    fn dark_theme(&self) -> bool {
        match self.theme_mode {
            session_store::ThemeMode::System => self.system_dark_theme,
            session_store::ThemeMode::Light => false,
            session_store::ThemeMode::Dark => true,
        }
    }
}

impl WindowState {
    fn active(&self) -> &TabSession {
        self.tabs
            .get(&self.active_tab)
            .expect("active tab session exists")
    }

    fn stable_paths(&self) -> Vec<PathBuf> {
        self.tab_order
            .iter()
            .filter_map(|id| self.tabs.get(id))
            .filter_map(|tab| tab.current_path.clone())
            .collect()
    }

    fn stable_active_path_index(&self) -> usize {
        let mut file_index = 0;
        for id in &self.tab_order {
            let Some(tab) = self.tabs.get(id) else {
                continue;
            };
            if tab.kind != TabKind::Files {
                continue;
            }
            if *id == self.active_tab {
                return file_index;
            }
            file_index += 1;
        }
        file_index.saturating_sub(1)
    }
}

fn reorder_column(order: &mut [u8; 4], kind: u8, offset: i32) -> bool {
    let Some(from) = order.iter().position(|candidate| *candidate == kind) else {
        return false;
    };
    let target = (from as i32 + offset).clamp(0, order.len() as i32 - 1) as usize;
    if from == target {
        return false;
    }
    let moved = order[from];
    if from < target {
        order.copy_within(from + 1..=target, from);
    } else {
        order.copy_within(target..from, target + 1);
    }
    order[target] = moved;
    true
}
#[derive(Debug)]
struct DirectoryRequest {
    tab_id: TabId,
    request_id: RequestId,
    path: PathBuf,
    visibility: crate::domain::FileVisibility,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Debug)]
enum DirectoryEvent {
    Batch {
        tab_id: TabId,
        request_id: RequestId,
        entries: Vec<FileEntry>,
    },
    Finished {
        tab_id: TabId,
        request_id: RequestId,
        path: PathBuf,
        skipped: usize,
    },
    Cancelled {
        tab_id: TabId,
        request_id: RequestId,
    },
    Failed {
        tab_id: TabId,
        request_id: RequestId,
        kind: io::ErrorKind,
        message: String,
    },
}

const SEARCH_PAGE_LIMIT: u32 = 256;
const SEARCH_WINDOW_PAGE_COUNT: u32 = 3;
const SEARCH_WINDOW_ITEM_LIMIT: usize = (SEARCH_PAGE_LIMIT * SEARCH_WINDOW_PAGE_COUNT) as usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchWindow {
    start: u32,
    len: usize,
}

fn search_row_height(view_mode: ViewMode) -> f32 {
    match view_mode {
        ViewMode::Details => 40.0,
        ViewMode::List => 34.0,
        ViewMode::Grid => 148.0,
    }
}

fn search_logical_maximum(
    total: u32,
    view_mode: ViewMode,
    columns: usize,
    visible_height: f32,
) -> f32 {
    let rows = match view_mode {
        ViewMode::Grid => (total as usize).div_ceil(columns.max(1)),
        ViewMode::Details | ViewMode::List => total as usize,
    };
    (rows as f32 * search_row_height(view_mode) - visible_height).max(0.0)
}

fn search_result_index_at_scroll(
    scroll_y: f32,
    total: u32,
    view_mode: ViewMode,
    columns: usize,
) -> u32 {
    if total == 0 {
        return 0;
    }
    let row = ((-scroll_y).max(0.0) / search_row_height(view_mode)).floor() as usize;
    let index = match view_mode {
        ViewMode::Grid => row.saturating_mul(columns.max(1)),
        ViewMode::Details | ViewMode::List => row,
    };
    index.min(total.saturating_sub(1) as usize) as u32
}

fn search_window_for_index(index: u32, total: u32, columns: usize) -> SearchWindow {
    if total == 0 {
        return SearchWindow { start: 0, len: 0 };
    }
    let page = index / SEARCH_PAGE_LIMIT * SEARCH_PAGE_LIMIT;
    let start = page.saturating_sub(SEARCH_PAGE_LIMIT);
    let columns = columns.max(1) as u32;
    let start = start / columns * columns;
    SearchWindow {
        start,
        len: total
            .saturating_sub(start)
            .min(SEARCH_WINDOW_ITEM_LIMIT as u32) as usize,
    }
}
fn search_window_for_scroll(
    scroll_y: f32,
    total: u32,
    view_mode: ViewMode,
    columns: usize,
) -> SearchWindow {
    search_window_for_index(
        search_result_index_at_scroll(scroll_y, total, view_mode, columns),
        total,
        columns,
    )
}

fn search_window_local_index(entry_id: EntryId, window_start: u32) -> Option<usize> {
    entry_id
        .0
        .checked_sub(window_start.saturating_add(1))
        .map(|index| index as usize)
}

fn search_window_viewport_y(
    index: u32,
    window: SearchWindow,
    view_mode: ViewMode,
    columns: usize,
) -> f32 {
    let local = index.saturating_sub(window.start) as usize;
    let row = match view_mode {
        ViewMode::Grid => local / columns.max(1),
        ViewMode::Details | ViewMode::List => local,
    };
    -(row as f32 * search_row_height(view_mode))
}
fn search_scroll_for_index(index: u32, view_mode: ViewMode, columns: usize) -> f32 {
    let row = match view_mode {
        ViewMode::Grid => index as usize / columns.max(1),
        ViewMode::Details | ViewMode::List => index as usize,
    };
    -(row as f32 * search_row_height(view_mode))
}

fn search_window_rows(tab: &TabSession, app: &AppState, window: SearchWindow) -> Vec<FileRow> {
    let texts = Texts::new(app.language);
    (0..window.len)
        .map(|local| {
            let result_index = window.start.saturating_add(local as u32);
            tab.visible_entry(EntryId(result_index.saturating_add(1)))
                .map(|entry| file_row(entry, tab, texts, app))
                .unwrap_or_else(empty_file_row)
        })
        .collect()
}

#[derive(Debug)]
enum EverythingRequest {
    Search {
        tab_id: TabId,
        request_id: RequestId,
        scope: SearchScope,
        depth: SearchDepth,
        query: String,
        sort: platform::windows::everything::EverythingSort,
        offset: u32,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
    FolderSize {
        tab_id: TabId,
        request_id: RequestId,
        entry_id: EntryId,
        path: PathBuf,
    },
    Configure(crate::domain::EverythingConfig),
    TestConnection,
    Start,
}

#[derive(Debug)]
enum EverythingEvent {
    SearchPage {
        tab_id: TabId,
        request_id: RequestId,
        offset: u32,
        entries: Vec<FileEntry>,
        total: u32,
        file_total: u32,
        response_valid: bool,
    },
    SearchFailed {
        tab_id: TabId,
        request_id: RequestId,
        offset: u32,
        error: platform::windows::everything::EverythingError,
    },
    SearchSkipped {
        tab_id: TabId,
        request_id: RequestId,
        offset: u32,
    },
    FolderSize {
        tab_id: TabId,
        request_id: RequestId,
        entry_id: EntryId,
        path: PathBuf,
        state: FolderSizeState,
    },
    Status(
        Result<
            platform::windows::everything::EverythingStatus,
            platform::windows::everything::EverythingError,
        >,
    ),
}

enum FolderSizeWork {
    Query {
        tab_id: TabId,
        request_id: RequestId,
        entry_id: EntryId,
        path: PathBuf,
    },
    Configure(crate::domain::EverythingConfig),
}
struct IconRequest {
    tab_id: TabId,
    request_id: RequestId,
    target: IconTarget,
    path: PathBuf,
    thumbnail: bool,
    requested_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconTarget {
    Entry(EntryId),
    Location,
}

#[derive(Debug)]
struct FileOperationRequest {
    id: OperationId,
    kind: FileOperationKind,
    items: Vec<OperationItem>,
    cancellation: crate::domain::file_operations::CancellationToken,
}

#[derive(Debug)]
#[allow(dead_code)]
enum FileOperationEvent {
    DestinationCreated {
        id: OperationId,
        path: PathBuf,
    },
    Progress {
        id: OperationId,
        completed_items: usize,
        completed_files: usize,
        total_files: Option<usize>,
        processed_bytes: u64,
        total_bytes: Option<u64>,
        current_item: PathBuf,
        started: Instant,
    },
    Conflict {
        id: OperationId,
        conflict: crate::domain::file_operations::OperationConflict,
        response: mpsc::Sender<crate::domain::file_operations::ConflictDecision>,
    },
    Finished {
        id: OperationId,
        result: OperationResult,
        item_states: Vec<(usize, ItemState, Option<String>)>,
        completed_targets: Vec<PathBuf>,
    },
}

#[derive(Debug)]
enum ClipboardRequest {
    Write { paths: Vec<PathBuf>, cut: bool },
    ReadPaste { target: PathBuf },
    CheckAvailability,
}
#[derive(Debug)]
enum ClipboardEvent {
    Written {
        result: Result<(), String>,
        paths: Vec<PathBuf>,
        cut: bool,
    },
    Paste(Result<Option<(FileOperationKind, Vec<OperationItem>)>, String>),
    Availability(Result<bool, String>),
}
struct IconEvent {
    tab_id: TabId,
    request_id: RequestId,
    target: IconTarget,
    path: PathBuf,
    icon: platform::windows_shell_icons::ShellIconRgba,
    actual_thumbnail: bool,
    requested_px: u32,
}

pub fn run(scenario: Option<AgentScenario>) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let operation_ui = OperationWindow::new()?;
    let delete_ui = ConfirmationWindow::new()?;
    let conflict_ui = ConfirmationWindow::new()?;
    let exit_ui = ConfirmationWindow::new()?;
    let delete_weak = delete_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    let restored = scenario
        .is_none()
        .then(|| session_store::default_path().and_then(|path| session_store::load(&path).ok()))
        .flatten();
    let default_window = session_store::WindowPlacement {
        x: 80,
        y: 80,
        width: 1180,
        height: 760,
    };
    let (
        restored_paths,
        active_index,
        window,
        additional_windows,
        column_order,
        search_column_order,
        column_widths,
        search_column_widths,
        everything_config,
        theme_mode,
        language,
        file_visibility,
    ) = restored
        .filter(|session| {
            session
                .windows
                .first()
                .is_some_and(|window| !window.tab_paths.is_empty())
        })
        .map(|session| {
            let mut windows = session.windows;
            let first = windows.remove(0);
            (
                first.tab_paths,
                first.active_tab,
                first.placement,
                windows,
                session.column_order,
                session.search_column_order,
                session.column_widths,
                session.search_column_widths,
                session.everything,
                session.theme_mode,
                session.language,
                session.file_visibility,
            )
        })
        .unwrap_or_else(|| {
            (
                vec![initial_path()],
                0,
                default_window,
                Vec::new(),
                [0, 1, 2, 3],
                [0, 1, 2, 3],
                session_store::DEFAULT_COLUMN_WIDTHS,
                session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
                crate::domain::EverythingConfig::default(),
                session_store::ThemeMode::System,
                Language::Chinese,
                crate::domain::FileVisibility::default(),
            )
        });
    ui.window()
        .set_position(slint::PhysicalPosition::new(window.x, window.y));
    ui.window().set_size(slint::LogicalSize::new(
        window.width as f32,
        window.height as f32,
    ));
    let state = Arc::new(Mutex::new(AppState::new(
        restored_paths,
        active_index,
        column_order,
        search_column_order,
        column_widths,
        search_column_widths,
        everything_config,
        theme_mode,
        language,
        platform::system_uses_dark_theme(),
    )));
    if let Ok(mut app) = state.lock() {
        app.file_visibility = file_visibility;
        app.active_window_state_mut().placement = window;
        for restored in &additional_windows {
            if !restored.tab_paths.is_empty() {
                app.register_window(
                    restored.tab_paths.clone(),
                    restored.active_tab,
                    restored.placement,
                );
            }
        }
        let _ = app.window(app.active_window);
    }
    if let Some(scenario) = scenario {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let active_tab = app.active_window_state().active_tab;
        agent_debug::apply_scenario(
            app.active_window_state_mut()
                .tabs
                .get_mut(&active_tab)
                .expect("active tab session exists"),
            scenario,
        );
    }
    let (request_sender, event_receiver) = spawn_directory_workers(WORKER_COUNT);
    let event_receiver = Arc::new(Mutex::new(event_receiver));
    {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.everything_config.executable_path.is_none()
            && let Some(installation) = platform::windows::everything::EverythingClient::discover()
                .into_iter()
                .find(|item| item.instance_name == "1.5a")
                .or_else(|| {
                    platform::windows::everything::EverythingClient::discover()
                        .into_iter()
                        .next()
                })
        {
            if !installation.executable_path.as_os_str().is_empty() {
                app.everything_config.executable_path = Some(installation.executable_path);
            }
            if !installation.instance_name.is_empty() {
                app.everything_config.instance_name = installation.instance_name;
            }
        }
    }
    let everything_config = state
        .lock()
        .expect("app state mutex is not poisoned")
        .everything_config
        .clone();
    let (everything_sender, everything_receiver) = spawn_everything_worker(everything_config);
    let (icon_sender, icon_receiver) = spawn_icon_workers(ICON_WORKER_COUNT, state.clone());
    let (operation_sender, operation_receiver) = spawn_file_operation_worker();
    let (clipboard_sender, clipboard_receiver) = spawn_clipboard_worker();

    let senders = WorkerSenders {
        directory: request_sender.clone(),
        operation: operation_sender.clone(),
        clipboard: clipboard_sender.clone(),
        everything: everything_sender.clone(),
        icon: icon_sender.clone(),
    };
    let initial_window_id = state
        .lock()
        .expect("app state mutex is not poisoned")
        .active_window;
    let scoped_state = WindowSessions::new(state.clone(), initial_window_id);
    wire_callbacks(
        &ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        request_sender.clone(),
        operation_sender.clone(),
        clipboard_sender,
        everything_sender.clone(),
        icon_sender.clone(),
        scoped_state.clone(),
    );
    wire_internal_drag_drop(
        &ui,
        operation_sender.clone(),
        request_sender.clone(),
        scoped_state.clone(),
    );
    let rectangle_selection_timer = wire_rectangle_selection(&ui, scoped_state.clone());
    wire_mouse_navigation(
        &ui,
        ConfirmationWindows::new(&delete_ui, &conflict_ui, &exit_ui),
        scoped_state.clone(),
        initial_window_id,
        senders.clone(),
        state.clone(),
    );
    wire_window_controls(&ui);
    wire_confirmation_windows(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        operation_sender.clone(),
        state.clone(),
    );
    wire_debug_showcase(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        state.clone(),
    );
    let _operation_timer =
        wire_operation_window(&ui, &operation_ui, operation_sender.clone(), state.clone());
    start_event_pump(
        &ui,
        event_receiver,
        icon_sender,
        everything_sender.clone(),
        state.clone(),
    );
    start_everything_event_pump(
        &ui,
        everything_receiver,
        everything_sender.clone(),
        state.clone(),
    );
    let _ = everything_sender.send(EverythingRequest::TestConnection);
    start_icon_event_pump(&ui, icon_receiver, state.clone());
    start_file_operation_event_pump(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        operation_receiver,
        operation_sender.clone(),
        request_sender.clone(),
        state.clone(),
    );
    start_clipboard_event_pump(
        &ui,
        clipboard_receiver,
        operation_sender.clone(),
        request_sender.clone(),
        state.clone(),
    );
    scan_cleanup_diagnostics(&ui, state.clone());
    start_sidebar_loader(&ui, state.clone());
    refresh_window_ui(&ui, &state, initial_window_id);
    refresh_operation_window(&operation_ui, &state);
    refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state);
    let initial_tabs = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.active_window_state()
            .tab_order
            .iter()
            .filter_map(|id| {
                app.active_window_state()
                    .tabs
                    .get(id)
                    .and_then(|tab| tab.current_path.clone())
                    .map(|path| (*id, path))
            })
            .collect::<Vec<_>>()
    };
    if scenario.is_none() {
        for (tab_id, path) in initial_tabs {
            submit_navigation(
                &request_sender,
                &state,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
        }
    }

    let drag_drop_target_timer =
        wire_native_drag_drop(&ui, operation_sender.clone(), scoped_state.clone());
    let directory_watch_timer = start_directory_watchers(request_sender.clone(), state.clone());
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        runtimes.insert(
            initial_window_id,
            WindowRuntime {
                ui: ui.clone_strong(),
                _native_drop_timer: drag_drop_target_timer,
                _rectangle_selection_timer: rectangle_selection_timer,
            },
        );
    });
    let restored_window_ids = state
        .lock()
        .map(|app| {
            app.windows
                .keys()
                .copied()
                .filter(|id| *id != initial_window_id)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for window_id in restored_window_ids {
        install_app_window(
            AppWindow::new()?,
            window_id,
            &delete_ui,
            &conflict_ui,
            &exit_ui,
            &senders,
            state.clone(),
        )?;
    }
    ui.show()?;
    let result = slint::run_event_loop();
    drop(directory_watch_timer);
    platform::windows::drag_drop::revoke_current();
    for weak in [delete_weak, conflict_weak, exit_weak] {
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
    }
    let live_placements = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .map(|(id, runtime)| {
                let position = runtime.ui.window().position();
                let size = runtime.ui.window().size();
                let scale = runtime.ui.window().scale_factor();
                (
                    *id,
                    session_store::WindowPlacement {
                        x: position.x,
                        y: position.y,
                        width: (size.width as f32 / scale).round() as u32,
                        height: (size.height as f32 / scale).round() as u32,
                    },
                )
            })
            .collect::<HashMap<_, _>>()
    });
    let (
        windows,
        column_order,
        search_column_order,
        column_widths,
        search_column_widths,
        everything_config,
        theme_mode,
        language,
        file_visibility,
    ) = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        for (id, placement) in &live_placements {
            if let Some(window) = app.windows.get_mut(id) {
                window.placement = *placement;
            }
        }
        let mut window_ids = app.windows.keys().copied().collect::<Vec<_>>();
        window_ids.sort_by_key(|id| id.0);
        let windows = window_ids
            .into_iter()
            .filter_map(|id| {
                let window = app.windows.get(&id)?;
                let paths = window.stable_paths();
                (!paths.is_empty()).then_some(session_store::WindowSessionState {
                    placement: window.placement,
                    active_tab: window.stable_active_path_index(),
                    tab_paths: paths,
                })
            })
            .collect::<Vec<_>>();
        (
            windows,
            app.column_order,
            app.search_column_order,
            app.column_widths,
            app.search_column_widths,
            app.everything_config.clone(),
            app.theme_mode,
            app.language,
            app.file_visibility,
        )
    };
    if scenario.is_none()
        && let Some(path) = session_store::default_path()
        && let Ok(session) = session_store::SessionState::with_windows_and_settings(
            windows,
            column_order,
            search_column_order,
            column_widths,
            search_column_widths,
            theme_mode,
            language,
            everything_config,
            file_visibility,
        )
    {
        let _ = session_store::save(&path, &session);
    }
    let _ = state.lock().ok().and_then(|mut app| {
        let active_window = app.active_window;
        app.close_window(active_window)
    });
    clear_window_runtimes();
    result
}

fn submit_navigation(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
) -> bool {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
        app.thumbnail_requests
            .retain(|(request_tab, _, _, _)| *request_tab != tab_id);
        app.focus_after_refresh.remove(&tab_id);
        let Some(tab) = app.tab_mut(tab_id) else {
            return false;
        };
        if tab.kind != TabKind::Files {
            return false;
        }
        if kind == NavigationKind::Normal && tab.current_path.as_ref() == Some(&path) {
            tab.cancel_address_edit();
            return false;
        }
        let (request_id, cancel) = tab.begin_navigation(path.clone(), kind);
        DirectoryRequest {
            tab_id,
            request_id,
            path,
            visibility: app.file_visibility,
            cancel,
        }
    };
    sender.send(request).is_ok()
}

fn restart_detached_tab(
    outcome: &DetachedTabOutcome,
    senders: &WorkerSenders,
    state: &SharedSessions,
) {
    match &outcome.restart {
        Some(DetachedTabRestart::Directory(path)) => {
            submit_navigation(
                &senders.directory,
                state,
                outcome.tab_id,
                path.clone(),
                NavigationKind::Refresh,
            );
        }
        Some(DetachedTabRestart::Search {
            scope,
            depth,
            query,
        }) => {
            if let Ok(mut app) = state.lock()
                && let Some(tab) = app.tab_mut(outcome.tab_id)
            {
                tab.search_scope = scope.clone();
                tab.search_depth = *depth;
            }
            submit_search(
                &senders.everything,
                state,
                None,
                outcome.tab_id,
                query.clone(),
            );
        }
        None => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn install_app_window(
    ui: AppWindow,
    window_id: WindowId,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    senders: &WorkerSenders,
    state: SharedSessions,
) -> Result<(), slint::PlatformError> {
    let placement = state
        .lock()
        .ok()
        .and_then(|app| app.window(window_id).map(|window| window.placement))
        .ok_or(slint::PlatformError::NoPlatform)?;
    install_app_window_at(
        ui,
        window_id,
        placement,
        true,
        delete_ui,
        conflict_ui,
        exit_ui,
        senders,
        state,
    )
}

#[allow(clippy::too_many_arguments)]
fn install_app_window_at(
    ui: AppWindow,
    window_id: WindowId,
    placement: session_store::WindowPlacement,
    refresh_before_show: bool,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    senders: &WorkerSenders,
    state: SharedSessions,
) -> Result<(), slint::PlatformError> {
    let scoped = WindowSessions::new(state.clone(), window_id);
    ui.window()
        .set_position(slint::PhysicalPosition::new(placement.x, placement.y));
    ui.window().set_size(slint::LogicalSize::new(
        placement.width as f32,
        placement.height as f32,
    ));
    wire_callbacks(
        &ui,
        delete_ui,
        conflict_ui,
        exit_ui,
        senders.directory.clone(),
        senders.operation.clone(),
        senders.clipboard.clone(),
        senders.everything.clone(),
        senders.icon.clone(),
        scoped.clone(),
    );
    wire_internal_drag_drop(
        &ui,
        senders.operation.clone(),
        senders.directory.clone(),
        scoped.clone(),
    );
    let rectangle_selection_timer = wire_rectangle_selection(&ui, scoped.clone());
    wire_mouse_navigation(
        &ui,
        ConfirmationWindows::new(delete_ui, conflict_ui, exit_ui),
        scoped.clone(),
        window_id,
        senders.clone(),
        state.clone(),
    );
    wire_window_controls(&ui);
    let native_drop_timer = wire_native_drag_drop(&ui, senders.operation.clone(), scoped.clone());
    if refresh_before_show {
        refresh_ui(&ui, &scoped);
    }
    ui.show()?;
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        runtimes.insert(
            window_id,
            WindowRuntime {
                ui,
                _native_drop_timer: native_drop_timer,
                _rectangle_selection_timer: rectangle_selection_timer,
            },
        );
    });
    Ok(())
}

fn detach_tab_into_new_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    screen_x: i32,
    screen_y: i32,
    senders: &WorkerSenders,
    confirmations: &ConfirmationWindows,
    state: &SharedSessions,
) -> bool {
    let destination = {
        let Ok(mut app) = state.lock() else {
            return false;
        };
        let Some(drag) = app.tab_drag else {
            return false;
        };
        if drag.window_id != source_window || !matches!(drag.phase, TabDragPhase::Dragging { .. }) {
            return false;
        }
        app.reserve_window_id()
    };
    let scale = source_ui.window().scale_factor();
    let placement = session_store::WindowPlacement {
        x: screen_x - (178.0 * scale / 2.0).round() as i32,
        y: screen_y - (20.0 * scale).round() as i32,
        width: 1180,
        height: 760,
    };
    let candidate = match AppWindow::new() {
        Ok(candidate) => candidate,
        Err(_) => return false,
    };
    if install_app_window_at(
        candidate,
        destination,
        placement,
        false,
        &confirmations
            .delete
            .upgrade()
            .expect("shared delete window exists"),
        &confirmations
            .conflict
            .upgrade()
            .expect("shared conflict window exists"),
        &confirmations
            .exit
            .upgrade()
            .expect("shared exit window exists"),
        senders,
        state.clone(),
    )
    .is_err()
    {
        return false;
    }
    let outcome = {
        let Ok(mut app) = state.lock() else {
            remove_window_runtime(destination);
            return false;
        };
        app.detach_dragged_tab_to_window(destination, placement)
    };
    let Some(outcome) = outcome else {
        if let Some(destination_ui) = window_ui(destination) {
            let _ = destination_ui.hide();
        }
        remove_window_runtime(destination);
        return false;
    };
    if let Some(destination_ui) = window_ui(destination) {
        refresh_window_ui(&destination_ui, state, destination);
    }
    restart_detached_tab(&outcome, senders, state);
    if outcome.source_window_closed {
        let _ = source_ui.hide();
        remove_window_runtime(source_window);
    } else if let Some(source) = window_ui(source_window) {
        refresh_ui(&source, &WindowSessions::new(state.clone(), source_window));
        source.invoke_clear_tab_drag();
    }
    true
}

fn move_tab_into_existing_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    destination_window: WindowId,
    insertion_index: usize,
    senders: &WorkerSenders,
    state: &SharedSessions,
) -> bool {
    if window_ui(destination_window).is_none() {
        return false;
    }
    let outcome = state
        .lock()
        .ok()
        .and_then(|mut app| app.move_dragged_tab_to_window(destination_window, insertion_index));
    let Some(outcome) = outcome else {
        return false;
    };
    restart_detached_tab(&outcome, senders, state);
    if let Some(destination) = window_ui(destination_window) {
        refresh_window_ui(&destination, state, destination_window);
    }
    if outcome.source_window_closed {
        let _ = source_ui.hide();
        remove_window_runtime(source_window);
    } else {
        refresh_window_ui(source_ui, state, source_window);
        source_ui.invoke_clear_tab_drag();
    }
    true
}

fn finish_native_tab_drag(
    source_ui: &AppWindow,
    source_window: WindowId,
    drop_point: Option<platform::windows::drag_drop::TabDropPoint>,
    senders: &WorkerSenders,
    confirmations: &ConfirmationWindows,
    state: &SharedSessions,
) -> bool {
    let dragging = state.lock().is_ok_and(|app| {
        app.tab_drag.is_some_and(|drag| {
            drag.window_id == source_window && matches!(drag.phase, TabDragPhase::Dragging { .. })
        })
    });
    if !dragging {
        return false;
    }
    let Some(drop_point) = drop_point else {
        if let Ok(mut app) = state.lock() {
            app.cancel_tab_drag();
        }
        project_native_insertion_indicator(None, state);
        source_ui.invoke_clear_tab_drag();
        refresh_window_ui(source_ui, state, source_window);
        return false;
    };
    let screen_x = drop_point.screen_x;
    let screen_y = drop_point.screen_y;
    let target_window = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes.iter().find_map(|(window_id, runtime)| {
            (native_window_handle(&runtime.ui) == drop_point.target_hwnd).then_some(*window_id)
        })
    });
    let cross_target = target_window
        .filter(|window_id| *window_id != source_window)
        .and_then(|_| cross_window_drop_target(source_window, screen_x, screen_y, state));
    let moved = cross_target.is_some_and(|(destination, insertion)| {
        move_tab_into_existing_window(
            source_ui,
            source_window,
            destination,
            insertion,
            senders,
            state,
        )
    });
    let (valid_source, _) = source_tab_drop_is_valid(source_ui, screen_x, screen_y)
        .unwrap_or((false, winit::dpi::PhysicalPosition::new(0.0, 0.0)));
    let detached = !moved
        && cross_target.is_none()
        && target_window.is_none()
        && !valid_source
        && detach_tab_into_new_window(
            source_ui,
            source_window,
            screen_x,
            screen_y,
            senders,
            confirmations,
            state,
        );
    let finished = if valid_source {
        state.lock().is_ok_and(|mut app| app.finish_tab_drag(true))
    } else if moved || detached {
        false
    } else {
        state.lock().is_ok_and(|mut app| app.cancel_tab_drag())
    };
    project_native_insertion_indicator(None, state);
    source_ui.invoke_clear_tab_drag();
    if !moved && !detached {
        refresh_window_ui(source_ui, state, source_window);
    }
    finished || moved || detached
}

fn drag_paths_for_pressed_entry(app: &AppState, entry_id: EntryId) -> Vec<PathBuf> {
    let tab = app.active();
    if tab.selected.contains(&entry_id) {
        return selected_paths(app);
    }
    tab.visible_entry(entry_id)
        .map(|entry| vec![entry.path.clone()])
        .unwrap_or_default()
}

fn selected_paths(app: &AppState) -> Vec<PathBuf> {
    app.active()
        .selected
        .iter()
        .filter_map(|id| {
            app.active()
                .visible_entry(*id)
                .map(|entry| entry.path.clone())
        })
        .collect()
}

fn context_target_at(
    state: &SharedSessions,
    window_x: f32,
    window_y: f32,
    list_top: f32,
    viewport_y: f32,
    search_scroll_y: f32,
    grid_columns: usize,
) -> (Option<EntryId>, bool) {
    let app = state.lock().expect("app state mutex is not poisoned");
    if window_y < list_top {
        return (None, true);
    }
    let active = app.active();
    let view_mode = active
        .visible_path()
        .and_then(|path| app.directory_view_modes.get(path))
        .copied()
        .unwrap_or(ViewMode::Details);
    let local_row = ((window_y - list_top + (-viewport_y).max(0.0)) / search_row_height(view_mode))
        .floor() as usize;
    let local_index = match view_mode {
        ViewMode::Grid => {
            let column = (window_x.max(0.0) / 148.0).floor() as usize;
            local_row
                .saturating_mul(grid_columns.max(1))
                .saturating_add(column.min(grid_columns.max(1) - 1))
        }
        ViewMode::Details | ViewMode::List => local_row,
    };
    let entry = if active.page_source == PageSource::Search {
        let window = search_window_for_scroll(
            search_scroll_y,
            active.search_total.unwrap_or(0),
            view_mode,
            grid_columns,
        );
        window
            .start
            .checked_add(local_index as u32)
            .and_then(|index| index.checked_add(1))
            .and_then(|id| active.visible_entry(EntryId(id)))
            .map(|entry| entry.id)
    } else {
        active
            .visible_entries()
            .get(local_index)
            .map(|entry| entry.id)
    };
    (entry, entry.is_none())
}

fn enqueue_operation(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    kind: FileOperationKind,
    items: Vec<OperationItem>,
) {
    if items.is_empty() {
        return;
    }
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let tab = app.active_window_state().active_tab;
        app.operations.submit(kind, Some(tab), items);
        if app.operations.active_id().is_some() {
            return;
        }
        app.operations.start_next().ok().flatten().and_then(|id| {
            let _ = app.operations.mark_running(id);
            app.operations.task(id).map(|task| FileOperationRequest {
                id,
                kind: task.kind,
                items: task.items.clone(),
                cancellation: task.cancellation.clone(),
            })
        })
    };
    if let Some(request) = request {
        let _ = sender.send(request);
    }
}

fn create_default_folder(state: &SharedSessions, sender: &mpsc::Sender<FileOperationRequest>) {
    let destination = state.lock().ok().and_then(|app| {
        let name = match app.language {
            Language::Chinese => "新建文件夹",
            Language::English => "New folder",
        };
        app.active().visible_path().map(|parent| parent.join(name))
    });
    if let Some(path) = destination {
        enqueue_operation(
            state,
            sender,
            FileOperationKind::CreateFolder,
            vec![OperationItem::pending(None, Some(path))],
        );
    }
}
fn request_clipboard_write(
    state: &SharedSessions,
    sender: &mpsc::Sender<ClipboardRequest>,
    cut: bool,
) {
    let paths = state
        .lock()
        .map(|app| selected_paths(&app))
        .unwrap_or_default();
    if !paths.is_empty() {
        let _ = sender.send(ClipboardRequest::Write { paths, cut });
    }
}

fn request_clipboard_paste(state: &SharedSessions, sender: &mpsc::Sender<ClipboardRequest>) {
    let target = state
        .lock()
        .ok()
        .and_then(|app| app.active().visible_path().map(Path::to_path_buf));
    if let Some(target) = target {
        let _ = sender.send(ClipboardRequest::ReadPaste { target });
    }
}
fn begin_rename_ui(weak: &slint::Weak<AppWindow>, state: &SharedSessions) {
    let target = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.active().selected.len() != 1 {
            return;
        }
        let id = app.active().selected[0];
        let entry = app.active().visible_entry(id).cloned();
        let name = entry.as_ref().map(|entry| {
            if entry.kind == crate::domain::EntryKind::File {
                entry
                    .path
                    .file_stem()
                    .unwrap_or(&entry.original_name)
                    .to_string_lossy()
                    .into_owned()
            } else {
                entry.display_name.clone()
            }
        });
        app.rename_extension = entry
            .filter(|entry| entry.kind == crate::domain::EntryKind::File)
            .and_then(|entry| entry.path.extension().map(std::ffi::OsStr::to_os_string));
        app.rename_target = Some((app.active_window_state().active_tab, id));
        name.map(|name| (id, name))
    };
    if let Some((id, name)) = target
        && let Some(ui) = weak.upgrade()
    {
        ui.set_rename_entry_id(id.0 as i32);
        ui.set_rename_input(name.into());
        ui.set_rename_editing(true);
    }
}

fn rename_validation_message(
    language: Language,
    error: crate::fs::file_operations::NameValidationError,
) -> String {
    use crate::fs::file_operations::NameValidationError;
    match (language, error) {
        (Language::Chinese, NameValidationError::Empty) => "名称不能为空。".to_owned(),
        (Language::English, NameValidationError::Empty) => "The name cannot be empty.".to_owned(),
        (Language::Chinese, NameValidationError::DotName) => "不能使用这个名称。".to_owned(),
        (Language::English, NameValidationError::DotName) => "This name cannot be used.".to_owned(),
        (Language::Chinese, NameValidationError::InvalidCharacter(character)) => {
            format!("名称不能包含字符“{character}”。")
        }
        (Language::English, NameValidationError::InvalidCharacter(character)) => {
            format!("The name cannot contain '{character}'.")
        }
        (Language::Chinese, NameValidationError::TrailingSpaceOrDot) => {
            "名称不能以空格或句点结尾。".to_owned()
        }
        (Language::English, NameValidationError::TrailingSpaceOrDot) => {
            "The name cannot end with a space or period.".to_owned()
        }
        (Language::Chinese, NameValidationError::ReservedName) => {
            "这是 Windows 保留名称，不能使用。".to_owned()
        }
        (Language::English, NameValidationError::ReservedName) => {
            "This name is reserved by Windows.".to_owned()
        }
    }
}

fn submit_rename(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    name: &str,
) -> Result<(), String> {
    let item = {
        let app = state.lock().expect("app state mutex is not poisoned");
        crate::fs::file_operations::validate_name(std::ffi::OsStr::new(name))
            .map_err(|error| rename_validation_message(app.language, error))?;
        let target = app.rename_target;
        target
            .and_then(|(tab_id, id)| {
                app.active_window_state()
                    .tabs
                    .get(&tab_id)?
                    .visible_entry(id)
            })
            .and_then(|entry| {
                entry.path.parent().map(|parent| {
                    let mut new_name = std::ffi::OsString::from(name);
                    if let Some(extension) = app.rename_extension.as_ref() {
                        new_name.push(".");
                        new_name.push(extension);
                    }
                    OperationItem::pending(Some(entry.path.clone()), Some(parent.join(new_name)))
                })
            })
    };
    let Some(item) = item else {
        return Err("Rename target is no longer available.".to_owned());
    };
    enqueue_operation(state, sender, FileOperationKind::Rename, vec![item]);
    Ok(())
}

fn should_fast_remove(path: &Path) -> bool {
    let protected = path.parent().is_none()
        || std::env::var_os("USERPROFILE").is_some_and(|home| Path::new(&home) == path)
        || std::env::current_dir().is_ok_and(|workspace| workspace == path);
    !protected
        && std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
        && std::fs::read_dir(path)
            .ok()
            .and_then(|mut entries| entries.nth(999))
            .is_some()
}

fn selected_delete_items(state: &SharedSessions) -> Vec<OperationItem> {
    state
        .lock()
        .map(|app| {
            selected_paths(&app)
                .into_iter()
                .map(|path| OperationItem::pending(Some(path), None))
                .collect()
        })
        .unwrap_or_default()
}

fn submit_delete_items(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    permanent: bool,
    items: Vec<OperationItem>,
) {
    if permanent {
        enqueue_operation(state, sender, FileOperationKind::PermanentDelete, items);
    } else {
        enqueue_operation(state, sender, FileOperationKind::RecycleDelete, items);
    }
}

fn submit_delete(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    permanent: bool,
) {
    submit_delete_items(state, sender, permanent, selected_delete_items(state));
}

fn project_context_menu(ui: &AppWindow, state: &SharedSessions, background: bool) {
    let (language, selected, can_paste) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        (
            app.language,
            app.active().selected.len(),
            app.clipboard_has_files,
        )
    };
    let label = |zh: &'static str, en: &'static str| -> &'static str {
        if language == Language::Chinese {
            zh
        } else {
            en
        }
    };
    let mut rows = Vec::new();
    if background {
        rows.push(ContextCommandRow {
            id: 1,
            label: label("新建文件夹", "New folder").into(),
            enabled: true,
            separator: false,
        });
    }
    if !background {
        rows.push(ContextCommandRow {
            id: 2,
            label: label("复制", "Copy").into(),
            enabled: selected > 0,
            separator: false,
        });
        rows.push(ContextCommandRow {
            id: 3,
            label: label("剪切", "Cut").into(),
            enabled: selected > 0,
            separator: false,
        });
    }
    rows.push(ContextCommandRow {
        id: 4,
        label: label("粘贴", "Paste").into(),
        enabled: can_paste,
        separator: false,
    });
    if !background {
        rows.push(ContextCommandRow {
            id: 5,
            label: label("重命名", "Rename").into(),
            enabled: selected == 1,
            separator: false,
        });
        rows.push(ContextCommandRow {
            id: 6,
            label: label("删除", "Delete").into(),
            enabled: selected > 0,
            separator: false,
        });
        rows.push(ContextCommandRow {
            id: 7,
            label: label("永久删除", "Delete permanently").into(),
            enabled: selected > 0,
            separator: false,
        });
    }
    rows.push(ContextCommandRow {
        id: 8,
        label: label("显示完整经典菜单", "Show full classic menu").into(),
        enabled: background || selected > 0,
        separator: false,
    });
    ui.set_context_commands(ModelRc::new(VecModel::from(rows)));
    ui.set_context_menu_on_background(background);
    ui.set_context_menu_open(true);
}

fn show_classic_menu(
    weak: slint::Weak<AppWindow>,
    state: &SharedSessions,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    background: bool,
    owner_window: isize,
    screen_x: i32,
    screen_y: i32,
) {
    let (paths, folder) = state
        .lock()
        .map(|app| {
            (
                selected_paths(&app),
                app.active().visible_path().map(Path::to_path_buf),
            )
        })
        .unwrap_or_default();
    if background && folder.is_none() || !background && paths.is_empty() {
        return;
    }
    let state = state.clone();
    thread::spawn(move || {
        let session = if background {
            platform::windows::context_menu::ClassicMenuSession::for_background_with_owner(
                folder.as_deref().expect("background folder"),
                true,
                owner_window,
            )
        } else {
            platform::windows::context_menu::ClassicMenuSession::for_paths_with_owner(
                &paths,
                true,
                owner_window,
            )
        };
        let affected_folders = if background {
            folder.iter().cloned().collect::<Vec<_>>()
        } else {
            let mut parents = paths
                .iter()
                .filter_map(|path| path.parent().map(Path::to_path_buf))
                .collect::<Vec<_>>();
            parents.sort();
            parents.dedup();
            parents
        };
        match session.and_then(|session| {
            let _ = session.items()?;
            session.show_native_and_invoke(owner_window, screen_x, screen_y)
        }) {
            Ok(Some(platform::windows::context_menu::ClassicMenuInvocation::BuiltIn { verb })) => {
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        match verb.as_str() {
                            "copy" => ui.invoke_copy_selection(false),
                            "cut" => ui.invoke_copy_selection(true),
                            "paste" => ui.invoke_paste_files(),
                            "delete" => ui.invoke_request_delete(false),
                            "rename" => ui.invoke_begin_rename(),
                            _ => {}
                        }
                    }
                });
            }
            Ok(_) => {
                if !affected_folders.is_empty() {
                    let state_for_refresh = state.clone();
                    thread::spawn(move || {
                        thread::sleep(Duration::from_millis(200));
                        let _ = slint::invoke_from_event_loop(move || {
                            refresh_affected_tabs(
                                &directory_sender,
                                &state_for_refresh,
                                &affected_folders,
                            );
                        });
                    });
                }
            }
            Err(error) => {
                let state_for_error = state.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Ok(mut app) = state_for_error.lock() {
                        app.operation_errors
                            .push(format!("Classic menu failed: {error}"));
                    }
                    refresh_all_windows(&state_for_error);
                });
            }
        }
    });
}

fn native_window_handle(ui: &AppWindow) -> isize {
    component_window_handle(ui)
}

fn component_window_handle<T: slint::ComponentHandle>(ui: &T) -> isize {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    ui.window()
        .with_winit_window(|window| {
            window
                .window_handle()
                .ok()
                .and_then(|handle| match handle.as_raw() {
                    RawWindowHandle::Win32(handle) => Some(handle.hwnd.get()),
                    _ => None,
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}
fn wire_internal_drag_drop(
    ui: &AppWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: WindowSessions,
) {
    #[derive(Debug)]
    struct InternalDrag {
        entry_id: EntryId,
        paths: Vec<PathBuf>,
        source_directories: Vec<PathBuf>,
        start_x: f32,
        start_y: f32,
        outbound_started: bool,
        right_button: bool,
    }

    let drag = Arc::new(Mutex::new(None::<InternalDrag>));

    let state_for_begin = state.clone();
    let drag_for_begin = drag.clone();
    ui.on_begin_internal_drag(move |entry_id, x, y, _control, _shift, right_button| {
        let app = state_for_begin
            .lock()
            .expect("app state mutex is not poisoned");
        let id = EntryId(entry_id as u32);
        let paths = drag_paths_for_pressed_entry(&app, id);
        let source_directories = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect();
        if let Ok(mut drag) = drag_for_begin.lock() {
            *drag = Some(InternalDrag {
                entry_id: id,
                paths,
                source_directories,
                start_x: x,
                start_y: y,
                outbound_started: false,
                right_button,
            });
        }
    });

    let weak_for_update = ui.as_weak();
    let state_for_update = state.clone();
    let drag_for_update = drag.clone();
    let directory_for_update = directory_sender.clone();
    ui.on_update_internal_drag(move |x, y| {
        let Some(ui) = weak_for_update.upgrade() else {
            return;
        };
        let outbound = drag_for_update.lock().ok().and_then(|mut drag| {
            let drag = drag.as_mut()?;
            let distance = ((x - drag.start_x).powi(2) + (y - drag.start_y).powi(2)).sqrt();
            if !should_release_internal_pointer_grab(distance, drag.outbound_started) {
                return None;
            }
            drag.outbound_started = true;
            Some((
                drag.paths.clone(),
                drag.source_directories.clone(),
                drag.right_button,
            ))
        });
        let Some((paths, source_directories, right_button)) = outbound else {
            return;
        };
        // OLE must own pointer routing before the window can receive its own native drop callbacks.
        ui.invoke_release_internal_drag_pointer();
        ui.set_drop_hover_entry_id(-1);
        eprintln!("drag-drop: threshold reached, entering DoDragDrop right_button={right_button}");
        let outbound_result = platform::windows::drag_drop::begin_outbound_drag(
            &paths,
            platform::windows::drag_drop::DropEffect::Move,
        );
        eprintln!("drag-drop: DoDragDrop returned result={outbound_result:?}");
        match outbound_result {
            Ok(result) if result.dropped => {
                // External targets own the operation; refresh only the source views and never infer item removal.
                refresh_affected_tabs(
                    &directory_for_update,
                    &state_for_update,
                    &source_directories,
                );
            }
            Ok(_) => {}
            Err(error) => {
                if let Ok(mut app) = state_for_update.lock() {
                    app.operation_errors
                        .push(format!("outbound drag failed: {error}"));
                }
            }
        }
    });
    let weak_for_end = ui.as_weak();
    let state_for_end = state.clone();
    let drag_for_end = drag.clone();
    ui.on_end_internal_drag(move |x, y, control, shift, right_button| {
        let Some(ui) = weak_for_end.upgrade() else {
            return;
        };
        ui.set_drop_hover_entry_id(-1);
        let Some(drag) = drag_for_end.lock().ok().and_then(|mut drag| drag.take()) else {
            return;
        };
        if drag.outbound_started {
            return;
        }
        let distance = ((x - drag.start_x).powi(2) + (y - drag.start_y).powi(2)).sqrt();
        if distance < 4.0 {
            if right_button {
                ui.invoke_show_entry_menu(drag.entry_id.0 as i32, x, y);
            }
            return;
        }
        let target = state_for_end.lock().ok().and_then(|app| {
            internal_drag_target(
                &app,
                y,
                ui.get_file_list_top(),
                ui.get_file_viewport_y(),
                ui.get_search_scroll_y(),
                ui.get_grid_column_count().max(1) as usize,
            )
            .map(|(_, path)| path)
        });
        let Some(target) = target else {
            return;
        };
        let key_state = (if control { 8 } else { 0 })
            | (if shift { 4 } else { 0 })
            | (if right_button { 2 } else { 0 });
        let (effect, reason) =
            platform::windows::drag_drop::negotiate_effect(&drag.paths, Some(&target), key_state);
        if reason.is_some() {
            return;
        }
        let intent = platform::windows::drag_drop::DropIntent {
            paths: drag.paths,
            target,
            effect,
            right_button,
            screen_x: x.round() as i32,
            screen_y: y.round() as i32,
            allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                | platform::windows::drag_drop::ALLOW_MOVE
                | platform::windows::drag_drop::ALLOW_LINK,
        };
        if right_button {
            ui.set_drop_can_copy(true);
            ui.set_drop_can_move(true);
            ui.set_drop_can_link(true);
            if let Ok(mut app) = state_for_end.lock() {
                app.pending_right_drop = Some(intent);
            }
            ui.invoke_show_drop_menu(x, y);
        } else {
            dispatch_drop_operation(
                intent,
                state_for_end.shared.clone(),
                operation_sender.clone(),
            );
        }
    });
}

const RECTANGLE_SELECTION_THRESHOLD: f32 = 5.0;
const RECTANGLE_SELECTION_EDGE: f32 = 20.0;
const RECTANGLE_SELECTION_MAX_SCROLL: f32 = 40.0;

#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectionRect {
    left: f32,
    top: f32,
    right: f32,
    bottom: f32,
}

impl SelectionRect {
    fn from_points(start_x: f32, start_y: f32, end_x: f32, end_y: f32) -> Self {
        Self {
            left: start_x.min(end_x),
            top: start_y.min(end_y),
            right: start_x.max(end_x),
            bottom: start_y.max(end_y),
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.left <= other.right
            && self.right >= other.left
            && self.top <= other.bottom
            && self.bottom >= other.top
    }
}

#[derive(Debug)]
struct RectangleSelectionGesture {
    tab_id: TabId,
    request_id: RequestId,
    start_x: f32,
    start_content_y: f32,
    pointer_x: f32,
    pointer_y: f32,
    snapshot: Arc<HashSet<EntryId>>,
    snapshot_order: Vec<EntryId>,
    focused: Option<EntryId>,
    anchor: Option<EntryId>,
    mode: RectangleSelectionMode,
    active: bool,
    dirty: bool,
    committed: bool,
    last_hits: HashSet<EntryId>,
}

fn rectangle_selection_started(start_x: f32, start_y: f32, x: f32, y: f32) -> bool {
    (x - start_x).hypot(y - start_y) >= RECTANGLE_SELECTION_THRESHOLD
}

fn rectangle_selection_scroll_delta(pointer_y: f32, top: f32, bottom: f32) -> f32 {
    if pointer_y < top + RECTANGLE_SELECTION_EDGE {
        (top + RECTANGLE_SELECTION_EDGE - pointer_y).min(RECTANGLE_SELECTION_MAX_SCROLL)
    } else if pointer_y > bottom - RECTANGLE_SELECTION_EDGE {
        -(pointer_y - (bottom - RECTANGLE_SELECTION_EDGE)).min(RECTANGLE_SELECTION_MAX_SCROLL)
    } else {
        0.0
    }
}

fn rectangle_selection_scroll_maximum(
    item_count: usize,
    view_mode: ViewMode,
    grid_columns: usize,
    visible_height: f32,
) -> f32 {
    let content_height = match view_mode {
        ViewMode::Details => item_count as f32 * 40.0,
        ViewMode::List => item_count as f32 * 34.0,
        ViewMode::Grid => item_count.div_ceil(grid_columns.max(1)) as f32 * 148.0,
    };
    (content_height - visible_height).max(0.0)
}

fn rectangle_selection_hits(
    tab: &TabSession,
    view_mode: ViewMode,
    grid_columns: usize,
    viewport_width: f32,
    rect: SelectionRect,
) -> HashSet<EntryId> {
    let (row_height, card_width, card_height, gap) = match view_mode {
        ViewMode::Details => (40.0, 0.0, 0.0, 0.0),
        ViewMode::List => (34.0, 0.0, 0.0, 0.0),
        ViewMode::Grid => (148.0, 140.0, 140.0, 8.0),
    };
    let slot_count = if tab.page_source == PageSource::Search {
        tab.search_total.unwrap_or(0) as usize
    } else {
        tab.visible_entries().len()
    };
    if slot_count == 0 || rect.bottom < 0.0 || rect.right < 0.0 {
        return HashSet::new();
    }
    let entry_at = |slot: usize| {
        if tab.page_source == PageSource::Search {
            u32::try_from(slot)
                .ok()
                .and_then(|slot| slot.checked_add(1))
                .and_then(|id| tab.visible_entry(EntryId(id)))
        } else {
            tab.visible_entries().get(slot)
        }
    };
    let candidate_start = |position: f32, extent: f32| {
        ((position.max(0.0) / extent).floor() as usize).saturating_sub(1)
    };
    let mut hits = HashSet::new();
    match view_mode {
        ViewMode::Details | ViewMode::List => {
            if rect.left > viewport_width {
                return hits;
            }
            let first = candidate_start(rect.top, row_height);
            let last = ((rect.bottom.max(0.0) / row_height).floor() as usize)
                .min(slot_count.saturating_sub(1));
            for slot in first..=last {
                let Some(entry) = entry_at(slot) else {
                    continue;
                };
                let item = SelectionRect {
                    left: 0.0,
                    top: slot as f32 * row_height,
                    right: viewport_width,
                    bottom: (slot + 1) as f32 * row_height,
                };
                if rect.intersects(item) {
                    hits.insert(entry.id);
                }
            }
        }
        ViewMode::Grid => {
            let columns = grid_columns.max(1);
            let column_extent = card_width + gap;
            let first_column = candidate_start(rect.left, column_extent).min(columns - 1);
            let last_column =
                ((rect.right.max(0.0) / column_extent).floor() as usize).min(columns - 1);
            let first_row = candidate_start(rect.top, row_height);
            let last_row = ((rect.bottom.max(0.0) / row_height).floor() as usize)
                .min(slot_count.saturating_sub(1) / columns);
            for row in first_row..=last_row {
                for column in first_column..=last_column {
                    let slot = row * columns + column;
                    if slot >= slot_count {
                        break;
                    }
                    let Some(entry) = entry_at(slot) else {
                        continue;
                    };
                    let left = column as f32 * column_extent;
                    let top = row as f32 * row_height;
                    let item = SelectionRect {
                        left,
                        top,
                        right: left + card_width,
                        bottom: top + card_height,
                    };
                    if rect.intersects(item) {
                        hits.insert(entry.id);
                    }
                }
            }
        }
    }
    hits
}

fn selection_mode(control: bool, shift: bool) -> RectangleSelectionMode {
    if control {
        RectangleSelectionMode::Toggle
    } else if shift {
        RectangleSelectionMode::Extend
    } else {
        RectangleSelectionMode::Replace
    }
}

fn finish_rectangle_selection(
    ui: &AppWindow,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
    timer: &slint::Timer,
) {
    timer.stop();
    let had_gesture = gesture
        .lock()
        .is_ok_and(|mut gesture| gesture.take().is_some());
    if had_gesture || ui.get_rectangle_selection_visible() {
        ui.invoke_clear_rectangle_selection();
    }
}

fn restore_rectangle_selection_snapshot(
    tab: &mut TabSession,
    snapshot: &[EntryId],
    focused: Option<EntryId>,
    anchor: Option<EntryId>,
) {
    tab.selected = snapshot
        .iter()
        .copied()
        .filter(|id| tab.visible_entry(*id).is_some())
        .collect();
    tab.focused = focused.filter(|id| tab.visible_entry(*id).is_some());
    tab.selection_anchor = anchor.filter(|id| tab.visible_entry(*id).is_some());
}

fn cancel_rectangle_selection(
    ui: &AppWindow,
    state: &WindowSessions,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
    timer: &slint::Timer,
) {
    let snapshot = gesture.lock().ok().and_then(|gesture| {
        gesture.as_ref().map(|gesture| {
            (
                gesture.tab_id,
                gesture.request_id,
                gesture.snapshot_order.clone(),
                gesture.focused,
                gesture.anchor,
            )
        })
    });
    let Some((tab_id, request_id, snapshot, focused, anchor)) = snapshot else {
        finish_rectangle_selection(ui, gesture, timer);
        return;
    };
    let update = {
        let Ok(mut app) = state.lock() else {
            finish_rectangle_selection(ui, gesture, timer);
            return;
        };
        if app.active_window_state().active_tab != tab_id {
            None
        } else {
            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            if tab.latest_request != request_id {
                None
            } else {
                let before = selection_projection_ids(tab);
                restore_rectangle_selection_snapshot(tab, snapshot.as_ref(), focused, anchor);
                let mut changed = before;
                changed.extend(selection_projection_ids(tab));
                Some((tab_id, changed))
            }
        }
    };
    if let Some((tab_id, changed)) = update {
        update_file_rows(ui, state, tab_id, &changed);
        update_selection_status(ui, state);
    }
    finish_rectangle_selection(ui, gesture, timer);
}

fn project_rectangle_selection_visual(
    ui: &AppWindow,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
) -> bool {
    let (start_x, start_content_y, pointer_x, pointer_y) = {
        let Ok(mut gesture) = gesture.lock() else {
            return false;
        };
        let Some(gesture) = gesture.as_mut() else {
            return false;
        };
        if !gesture.active
            && !rectangle_selection_started(
                gesture.start_x,
                gesture.start_content_y + ui.get_file_viewport_y(),
                gesture.pointer_x,
                gesture.pointer_y,
            )
        {
            return false;
        }
        gesture.active = true;
        (
            gesture.start_x,
            gesture.start_content_y,
            gesture.pointer_x,
            gesture.pointer_y,
        )
    };
    let current_content_y = pointer_y - ui.get_file_viewport_y();
    let rect = SelectionRect::from_points(start_x, start_content_y, pointer_x, current_content_y);
    let viewport_top = ui.get_rectangle_viewport_top();
    let viewport_bottom = viewport_top + ui.get_file_viewport_height();
    let visual_top = (rect.top + ui.get_file_viewport_y()).max(viewport_top);
    let visual_bottom = (rect.bottom + ui.get_file_viewport_y()).min(viewport_bottom);
    ui.invoke_show_rectangle_selection(
        rect.left.max(0.0),
        visual_top,
        (rect.right.min(ui.get_file_viewport_width()) - rect.left.max(0.0)).max(0.0),
        (visual_bottom - visual_top).max(0.0),
    );
    true
}

fn update_rectangle_selection(
    ui: &AppWindow,
    state: &WindowSessions,
    gesture: &Arc<Mutex<Option<RectangleSelectionGesture>>>,
    timer: &slint::Timer,
) -> bool {
    let (
        tab_id,
        request_id,
        snapshot,
        mode,
        start_x,
        start_content_y,
        pointer_x,
        pointer_y,
        previous_hits,
        committed,
    ) = {
        let Ok(mut gesture) = gesture.lock() else {
            return false;
        };
        let Some(gesture) = gesture.as_mut() else {
            return false;
        };
        if !gesture.active || !gesture.dirty {
            return false;
        }
        gesture.dirty = false;
        (
            gesture.tab_id,
            gesture.request_id,
            gesture.snapshot.clone(),
            gesture.mode,
            gesture.start_x,
            gesture.start_content_y,
            gesture.pointer_x,
            gesture.pointer_y,
            gesture.last_hits.clone(),
            gesture.committed,
        )
    };
    let current_content_y = pointer_y - ui.get_file_viewport_y();
    let rect = SelectionRect::from_points(start_x, start_content_y, pointer_x, current_content_y);
    let update = {
        let Ok(mut app) = state.lock() else {
            return false;
        };
        if app.active_window_state().active_tab != tab_id {
            None
        } else {
            let view_mode = app
                .active()
                .visible_path()
                .and_then(|path| app.directory_view_modes.get(path))
                .copied()
                .unwrap_or(ViewMode::Details);
            let grid_columns = ui.get_grid_column_count().max(1) as usize;
            let viewport_width = ui.get_file_viewport_width();

            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            if tab.latest_request != request_id {
                None
            } else {
                let previous_focus = tab.focused;
                let hits =
                    rectangle_selection_hits(tab, view_mode, grid_columns, viewport_width, rect);
                if committed && previous_hits == hits {
                    return false;
                }
                tab.apply_rectangle_selection(snapshot.as_ref(), &hits, mode);
                let mut changed = if committed {
                    previous_hits
                        .symmetric_difference(&hits)
                        .copied()
                        .collect::<HashSet<_>>()
                } else {
                    match mode {
                        RectangleSelectionMode::Replace => snapshot.union(&hits).copied().collect(),
                        RectangleSelectionMode::Extend => {
                            hits.difference(snapshot.as_ref()).copied().collect()
                        }
                        RectangleSelectionMode::Toggle => hits.clone(),
                    }
                };
                changed.extend(previous_focus);
                changed.extend(tab.focused);
                Some((tab_id, changed, hits))
            }
        }
    };
    let Some(update) = update else {
        finish_rectangle_selection(ui, gesture, timer);
        return false;
    };
    if let Ok(mut gesture) = gesture.lock()
        && let Some(gesture) = gesture.as_mut()
    {
        gesture.last_hits = update.2;
        gesture.committed = true;
    }
    update_file_rows(ui, state, update.0, &update.1);
    update_selection_status(ui, state);
    true
}

fn wire_rectangle_selection(ui: &AppWindow, state: WindowSessions) -> Rc<slint::Timer> {
    let gesture = Arc::new(Mutex::new(None::<RectangleSelectionGesture>));
    let timer = Rc::new(slint::Timer::default());
    let gesture_for_begin = gesture.clone();
    let timer_for_begin = timer.clone();
    let state_for_begin = state.clone();
    let weak_for_begin = ui.as_weak();
    ui.on_begin_rectangle_selection(move |x, y, control, shift| {
        let Some(ui) = weak_for_begin.upgrade() else {
            return;
        };
        let Ok(app) = state_for_begin.lock() else {
            return;
        };
        let tab = app.active();
        if tab.kind != TabKind::Files {
            return;
        }
        if let Ok(mut gesture) = gesture_for_begin.lock() {
            *gesture = Some(RectangleSelectionGesture {
                tab_id: tab.id,
                request_id: tab.latest_request,
                start_x: x,
                start_content_y: y - ui.get_file_viewport_y(),
                pointer_x: x,
                pointer_y: y,
                snapshot: Arc::new(tab.selected.iter().copied().collect()),
                snapshot_order: tab.selected.clone(),
                focused: tab.focused,
                anchor: tab.selection_anchor,
                mode: selection_mode(control, shift),
                active: false,
                dirty: false,
                committed: false,
                last_hits: HashSet::new(),
            });
            timer_for_begin.restart();
        }
    });

    let gesture_for_update = gesture.clone();
    let weak_for_update = ui.as_weak();
    ui.on_update_rectangle_selection(move |x, y| {
        let Some(ui) = weak_for_update.upgrade() else {
            return;
        };
        if let Ok(mut gesture) = gesture_for_update.lock()
            && let Some(gesture) = gesture.as_mut()
        {
            gesture.pointer_x = x;
            gesture.pointer_y = y;
            gesture.dirty = true;
        }
        project_rectangle_selection_visual(&ui, &gesture_for_update);
    });

    let gesture_for_end = gesture.clone();
    let timer_for_end = timer.clone();
    let state_for_end = state.clone();
    let weak_for_end = ui.as_weak();
    ui.on_end_rectangle_selection(move || {
        let Some(ui) = weak_for_end.upgrade() else {
            return;
        };
        let active = gesture_for_end
            .lock()
            .ok()
            .and_then(|gesture| gesture.as_ref().map(|gesture| gesture.active))
            .unwrap_or(false);
        if active {
            if let Ok(mut gesture) = gesture_for_end.lock()
                && let Some(gesture) = gesture.as_mut()
            {
                gesture.dirty = true;
            }
            project_rectangle_selection_visual(&ui, &gesture_for_end);
            update_rectangle_selection(&ui, &state_for_end, &gesture_for_end, &timer_for_end);
        } else {
            let update = mutate_active_selection(&state_for_end, TabSession::clear_selection);
            if let Some((tab_id, changed)) = update {
                update_file_rows(&ui, &state_for_end, tab_id, &changed);
                update_selection_summary(&ui, &state_for_end);
            }
        }
        finish_rectangle_selection(&ui, &gesture_for_end, &timer_for_end);
    });

    let gesture_for_cancel = gesture.clone();
    let timer_for_cancel = timer.clone();
    let state_for_cancel = state.clone();
    let weak_for_cancel = ui.as_weak();
    ui.on_cancel_rectangle_selection(move || {
        if let Some(ui) = weak_for_cancel.upgrade() {
            cancel_rectangle_selection(
                &ui,
                &state_for_cancel,
                &gesture_for_cancel,
                &timer_for_cancel,
            );
        }
    });

    let gesture_for_timer = gesture.clone();
    let timer_for_tick = timer.clone();
    let state_for_timer = state.clone();
    let weak_for_timer = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(16),
        move || {
            let Some(ui) = weak_for_timer.upgrade() else {
                return;
            };
            let state = gesture_for_timer.lock().ok().and_then(|gesture| {
                gesture
                    .as_ref()
                    .map(|gesture| (gesture.active, gesture.dirty, gesture.pointer_y))
            });
            let Some((active, dirty, pointer_y)) = state else {
                return;
            };
            let mut viewport_changed = false;
            if active {
                let top = ui.get_rectangle_viewport_top();
                let bottom = top + ui.get_file_viewport_height();
                let delta = rectangle_selection_scroll_delta(pointer_y, top, bottom);
                if delta != 0.0 && ui.get_search_results_mode() {
                    ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
                    viewport_changed = true;
                } else {
                    let maximum = rectangle_selection_scroll_maximum(
                        ui.get_files().row_count(),
                        match ui.get_view_mode() {
                            1 => ViewMode::List,
                            2 => ViewMode::Grid,
                            _ => ViewMode::Details,
                        },
                        ui.get_grid_column_count().max(1) as usize,
                        ui.get_file_viewport_height(),
                    );
                    let viewport = (ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0);
                    if viewport != ui.get_file_viewport_y() {
                        ui.set_file_viewport_y(viewport);
                        viewport_changed = true;
                    }
                }
                if viewport_changed {
                    if let Ok(mut gesture) = gesture_for_timer.lock()
                        && let Some(gesture) = gesture.as_mut()
                    {
                        gesture.dirty = true;
                    }
                    project_rectangle_selection_visual(&ui, &gesture_for_timer);
                }
            }
            if active && (dirty || viewport_changed) {
                update_rectangle_selection(
                    &ui,
                    &state_for_timer,
                    &gesture_for_timer,
                    &timer_for_tick,
                );
            }
        },
    );
    timer.stop();
    timer
}

#[allow(clippy::too_many_arguments)]
fn wire_callbacks(
    ui: &AppWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    sender: mpsc::Sender<DirectoryRequest>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    clipboard_sender: mpsc::Sender<ClipboardRequest>,
    everything_sender: mpsc::Sender<EverythingRequest>,
    icon_sender: mpsc::Sender<IconRequest>,
    state: WindowSessions,
) {
    let weak = ui.as_weak();
    let sender_for_path = sender.clone();
    let state_for_path = state.clone();
    let everything_for_accept = everything_sender.clone();
    ui.on_navigate_path(move |path| {
        let input = path.to_string();
        let (tab_id, mode) = {
            let app = state_for_path
                .lock()
                .expect("app state mutex is not poisoned");
            (
                app.active_window_state().active_tab,
                app.active().address_mode,
            )
        };
        let query = if mode == AddressMode::Smart {
            let mut app = state_for_path
                .lock()
                .expect("app state mutex is not poisoned");
            app.active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .map(|tab| {
                    tab.update_address_input(input.clone());
                    tab.search_query.clone()
                })
                .unwrap_or_else(|| input.clone())
        } else {
            input.clone()
        };
        let target = platform::windows::address_path::normalize_address_path(path.as_str());
        let state_for_validation = state_for_path.clone();
        let sender_for_validation = sender_for_path.clone();
        let everything_for_validation = everything_for_accept.clone();
        thread::spawn(move || {
            if target.is_dir() {
                let _ = slint::invoke_from_event_loop(move || {
                    submit_navigation(
                        &sender_for_validation,
                        &state_for_validation,
                        tab_id,
                        target,
                        NavigationKind::Normal,
                    );
                });
            } else {
                let _ = slint::invoke_from_event_loop(move || {
                    submit_search(
                        &everything_for_validation,
                        &state_for_validation,
                        None,
                        tab_id,
                        query,
                    );
                });
            }
        });
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_path);
        }
    });

    let weak = ui.as_weak();
    let state_for_search_edit = state.clone();
    ui.on_begin_search(move || {
        let mut app = state_for_search_edit
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
            tab.begin_smart_address_edit();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_search_edit);
            ui.invoke_focus_address_editor();
        }
    });

    let state_for_changed = state.clone();
    let everything_for_changed = everything_sender.clone();
    let weak_for_changed = ui.as_weak();
    ui.on_address_input_changed(move |value| {
        let input = value.to_string();
        let (tab_id, is_search) = {
            let mut app = state_for_changed
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .expect("active tab exists");
            tab.update_address_input(input.clone());
            (tab_id, tab.address_mode == AddressMode::Smart)
        };
        if is_search {
            let state = state_for_changed.clone();
            let sender = everything_for_changed.clone();
            let weak = weak_for_changed.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(160));
                let still_current = state.lock().ok().is_some_and(|app| {
                    app.active_window_state()
                        .tabs
                        .get(&tab_id)
                        .is_some_and(|tab| {
                            tab.address_input == input && tab.address_mode == AddressMode::Smart
                        })
                });
                if still_current {
                    let query = state
                        .lock()
                        .ok()
                        .and_then(|app| {
                            app.active_window_state()
                                .tabs
                                .get(&tab_id)
                                .map(|tab| tab.search_query.clone())
                        })
                        .unwrap_or(input);
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        submit_search(&sender, &state, Some(&ui), tab_id, query);
                    });
                }
            });
        }
    });

    let state_for_search_depth = state.clone();
    let everything_for_search_depth = everything_sender.clone();
    let weak_for_search_depth = ui.as_weak();
    ui.on_toggle_search_depth(move || {
        let (tab_id, query, changed) = {
            let mut app = state_for_search_depth
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .expect("active tab exists");
            let changed = tab.address_mode == AddressMode::Smart && tab.toggle_search_depth();
            (tab_id, tab.search_query.clone(), changed)
        };
        if changed {
            submit_search(
                &everything_for_search_depth,
                &state_for_search_depth,
                weak_for_search_depth.upgrade().as_ref(),
                tab_id,
                query,
            );
            if let Some(ui) = weak_for_search_depth.upgrade() {
                ui.invoke_focus_address_editor();
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_cancel_edit = state.clone();
    ui.on_cancel_address_edit(move || {
        let mut app = state_for_cancel_edit
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
            tab.cancel_address_edit();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_cancel_edit);
        }
    });

    let weak = ui.as_weak();
    let state_for_next_search_page = state.clone();
    let everything_for_next_search_page = everything_sender.clone();
    ui.on_request_search_position(move |requested_scroll| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let (tab_id, total, view_mode) = {
            let app = state_for_next_search_page
                .lock()
                .expect("app state mutex is not poisoned");
            let tab = app.active();
            let mode = tab
                .visible_path()
                .and_then(|path| app.directory_view_modes.get(path))
                .copied()
                .unwrap_or(ViewMode::Details);
            (
                app.active_window_state().active_tab,
                tab.search_total.unwrap_or(0),
                mode,
            )
        };
        let columns = ui.get_grid_column_count().max(1) as usize;
        let maximum =
            search_logical_maximum(total, view_mode, columns, ui.get_file_viewport_height());
        let scroll = requested_scroll.clamp(-maximum, 0.0);
        ui.set_search_scroll_y(scroll);
        let index = search_result_index_at_scroll(scroll, total, view_mode, columns);
        let window = search_window_for_index(index, total, columns);
        ui.set_file_viewport_y(search_window_viewport_y(index, window, view_mode, columns));
        submit_search_page(
            &everything_for_next_search_page,
            &state_for_next_search_page,
            tab_id,
            index,
        );
        refresh_tab_window(&state_for_next_search_page.shared, tab_id);
    });
    let weak = ui.as_weak();
    let sender_for_entry = sender.clone();
    let state_for_entry = state.clone();
    ui.on_open_entry(move |entry_id| {
        let target = {
            let app = state_for_entry
                .lock()
                .expect("app state mutex is not poisoned");
            let entry_id = if entry_id < 0 {
                app.active().focused
            } else {
                Some(EntryId(entry_id as u32))
            };
            entry_id.and_then(|entry_id| {
                app.active().visible_entry(entry_id).map(|entry| {
                    (
                        app.active_window_state().active_tab,
                        entry
                            .open_target
                            .clone()
                            .unwrap_or_else(|| entry.path.clone()),
                        entry.kind == crate::domain::EntryKind::Directory,
                    )
                })
            })
        };
        if let Some((tab_id, target, is_directory)) = target {
            if is_directory {
                submit_navigation(
                    &sender_for_entry,
                    &state_for_entry,
                    tab_id,
                    target,
                    NavigationKind::Normal,
                );
            } else {
                thread::spawn(move || {
                    if let Err(error) = platform::open_path(&target) {
                        eprintln!("unable to open file: {error}");
                    }
                });
            }
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_entry);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_breadcrumb = sender.clone();
    let state_for_breadcrumb = state.clone();
    ui.on_navigate_breadcrumb(move |index| {
        let target = {
            let app = state_for_breadcrumb
                .lock()
                .expect("app state mutex is not poisoned");
            usize::try_from(index)
                .ok()
                .and_then(|index| app.active().breadcrumb_paths().get(index).cloned())
                .map(|(_, path)| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_breadcrumb,
                &state_for_breadcrumb,
                tab_id,
                path,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_breadcrumb);
            }
        }
    });
    let weak = ui.as_weak();
    let sender_for_sidebar = sender.clone();
    let state_for_sidebar = state.clone();
    ui.on_navigate_sidebar(move |index| {
        let target = {
            let app = state_for_sidebar
                .lock()
                .expect("app state mutex is not poisoned");
            usize::try_from(index)
                .ok()
                .and_then(|index| app.sidebar.get(index))
                .map(|location| (app.active_window_state().active_tab, location.path.clone()))
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_sidebar,
                &state_for_sidebar,
                tab_id,
                path,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_sidebar);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_activate_entry = sender.clone();
    let state_for_activate_entry = state.clone();
    let last_click = std::rc::Rc::new(Cell::new(None::<(TabId, RequestId, EntryId, Instant)>));
    ui.on_activate_entry(move |entry_id, toggle, extend| {
        let entry_id = EntryId(entry_id as u32);
        let now = Instant::now();
        let (tab_id, request_id) = {
            let app = state_for_activate_entry
                .lock()
                .expect("app state mutex is not poisoned");
            (
                app.active_window_state().active_tab,
                app.active().latest_request,
            )
        };
        let double_click_interval = platform::double_click_interval();
        let should_open = last_click.get().is_some_and(
            |(previous_tab, previous_request, previous_id, previous_time)| {
                previous_tab == tab_id
                    && previous_request == request_id
                    && previous_id == entry_id
                    && now.saturating_duration_since(previous_time) <= double_click_interval
            },
        );
        last_click.set((!should_open).then_some((tab_id, request_id, entry_id, now)));

        if should_open {
            let target = {
                let app = state_for_activate_entry
                    .lock()
                    .expect("app state mutex is not poisoned");
                app.active().visible_entry(entry_id).map(|entry| {
                    (
                        app.active_window_state().active_tab,
                        entry
                            .open_target
                            .clone()
                            .unwrap_or_else(|| entry.path.clone()),
                        entry.kind == crate::domain::EntryKind::Directory,
                    )
                })
            };
            if let Some((tab_id, target, is_directory)) = target {
                if is_directory {
                    submit_navigation(
                        &sender_for_activate_entry,
                        &state_for_activate_entry,
                        tab_id,
                        target,
                        NavigationKind::Normal,
                    );
                } else {
                    thread::spawn(move || {
                        if let Err(error) = platform::open_path(&target) {
                            eprintln!("unable to open file: {error}");
                        }
                    });
                }
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_activate_entry);
                }
            }
            return;
        }

        let changed_rows = {
            let mut app = state_for_activate_entry
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) else {
                return;
            };
            let previous_selected = tab.selected.clone();
            let previous_focused = tab.focused;
            tab.select_entry(entry_id, toggle, extend);
            previous_selected
                .into_iter()
                .chain(tab.selected.iter().copied())
                .chain(previous_focused)
                .chain(tab.focused)
                .collect::<std::collections::HashSet<_>>()
        };
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_activate_entry, tab_id, &changed_rows);
            update_selection_summary(&ui, &state_for_activate_entry);
        }
    });
    let weak = ui.as_weak();
    let state_for_select = state.clone();
    ui.on_select_entry(move |entry_id, toggle, extend| {
        let (tab_id, changed_rows) = {
            let mut app = state_for_select
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) else {
                return;
            };
            let previous_selected = tab.selected.clone();
            let previous_focused = tab.focused;
            tab.select_entry(EntryId(entry_id as u32), toggle, extend);
            let changed = previous_selected
                .into_iter()
                .chain(tab.selected.iter().copied())
                .chain(previous_focused)
                .chain(tab.focused)
                .collect::<std::collections::HashSet<_>>();
            (tab_id, changed)
        };
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_select, tab_id, &changed_rows);
            update_selection_summary(&ui, &state_for_select);
        }
    });

    let weak = ui.as_weak();
    let state_for_clear = state.clone();
    ui.on_clear_selection(move || {
        let update = mutate_active_selection(&state_for_clear, TabSession::clear_selection);
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_clear, tab_id, &changed);
            update_selection_summary(&ui, &state_for_clear);
        }
    });

    let weak = ui.as_weak();
    let state_for_all = state.clone();
    ui.on_select_all(move || {
        let update = mutate_active_selection(&state_for_all, TabSession::select_all);
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_all, tab_id, &changed);
            update_selection_summary(&ui, &state_for_all);
        }
    });

    let weak = ui.as_weak();
    let state_for_focus = state.clone();
    ui.on_move_focus(move |delta, extend| {
        let update = mutate_active_selection(&state_for_focus, |tab| {
            tab.move_focus(delta as isize, extend);
        });
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_focus, tab_id, &changed);
            update_selection_summary(&ui, &state_for_focus);
        }
    });

    let weak = ui.as_weak();
    let state_for_boundary = state.clone();
    ui.on_focus_boundary(move |last, extend| {
        let update = mutate_active_selection(&state_for_boundary, |tab| {
            tab.focus_boundary(last, extend);
        });
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_boundary, tab_id, &changed);
            update_selection_summary(&ui, &state_for_boundary);
        }
    });

    let weak = ui.as_weak();
    let state_for_toggle = state.clone();
    ui.on_toggle_focused(move || {
        let update = mutate_active_selection(&state_for_toggle, TabSession::toggle_focused);
        if let (Some(ui), Some((tab_id, changed))) = (weak.upgrade(), update) {
            update_file_rows(&ui, &state_for_toggle, tab_id, &changed);
            update_selection_summary(&ui, &state_for_toggle);
        }
    });

    let weak = ui.as_weak();
    let state_for_sort = state.clone();
    let everything_for_sort = everything_sender.clone();
    ui.on_change_sort(move |field| {
        let field = match field {
            1 => SortField::Kind,
            2 => SortField::Size,
            3 => SortField::Modified,
            _ => SortField::Name,
        };
        let mut app = state_for_sort
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        let search_query = if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
            if tab.page_source == PageSource::Search {
                tab.set_search_sort(field);
                Some(tab.search_query.clone())
            } else {
                tab.set_sort(field);
                None
            }
        } else {
            None
        };
        drop(app);
        if let Some(query) = search_query
            && let Some(ui) = weak.upgrade()
        {
            submit_search(
                &everything_for_sort,
                &state_for_sort,
                Some(&ui),
                tab_id,
                query,
            );
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_sort);
        }
    });

    let weak = ui.as_weak();
    let state_for_view = state.clone();
    let icon_for_view = icon_sender.clone();
    ui.on_change_view_mode(move |mode| {
        let mode = match mode {
            1 => ViewMode::List,
            2 => ViewMode::Grid,
            _ => ViewMode::Details,
        };
        let mut preserved_search_index = None;
        if let Ok(mut app) = state_for_view.lock() {
            let path = app.active().visible_path().map(Path::to_path_buf);
            if app.active().page_source == PageSource::Search {
                let previous_mode = path
                    .as_deref()
                    .and_then(|path| app.directory_view_modes.get(path))
                    .copied()
                    .unwrap_or(ViewMode::Details);
                let total = app.active().search_total.unwrap_or(0);
                preserved_search_index = weak.upgrade().map(|ui| {
                    search_result_index_at_scroll(
                        ui.get_search_scroll_y(),
                        total,
                        previous_mode,
                        ui.get_grid_column_count().max(1) as usize,
                    )
                });
            }
            if let Some(path) = path {
                app.directory_view_modes.insert(path, mode);
            }
        }
        if let Some(ui) = weak.upgrade() {
            if let Some(index) = preserved_search_index {
                ui.set_search_scroll_y(search_scroll_for_index(
                    index,
                    mode,
                    ui.get_grid_column_count().max(1) as usize,
                ));
            }
            refresh_ui(&ui, &state_for_view);
            request_grid_thumbnails(
                &ui,
                &state_for_view.shared,
                state_for_view.window_id,
                &icon_for_view,
            );
        }
    });
    let weak = ui.as_weak();
    let state_for_columns = state.clone();
    ui.on_reorder_column(move |kind, offset| {
        let mut app = state_for_columns
            .lock()
            .expect("app state mutex is not poisoned");
        let search = app.active().page_source == PageSource::Search;
        if search {
            reorder_column(&mut app.search_column_order, kind as u8, offset);
        } else {
            reorder_column(&mut app.column_order, kind as u8, offset);
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_columns);
        }
    });
    let state_for_column_widths = state.clone();
    ui.on_resize_column(move |kind, width| {
        if !(0..4).contains(&kind) || !width.is_finite() {
            return;
        }
        let width = width.round().clamp(64.0, 4_096.0) as u32;
        let mut app = state_for_column_widths
            .lock()
            .expect("app state mutex is not poisoned");
        let search = app.active().page_source == PageSource::Search;
        if search {
            app.search_column_widths[kind as usize] = width;
        } else {
            app.column_widths[kind as usize] = width;
        }
        drop(app);
    });

    let weak = ui.as_weak();
    let sender_for_new = sender.clone();
    let state_for_new = state.clone();
    ui.on_new_tab(move || {
        let reload = {
            let mut app = state_for_new
                .lock()
                .expect("app state mutex is not poisoned");
            if app.duplicate_active_tab().is_some() {
                None
            } else {
                let path = app
                    .active()
                    .current_path
                    .clone()
                    .unwrap_or_else(initial_path);
                let tab_id = app.create_tab(path.clone());
                Some((tab_id, path))
            }
        };
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_new);
        }
        if let Some((tab_id, path)) = reload {
            submit_navigation(
                &sender_for_new,
                &state_for_new,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_new);
            }
        }
    });

    let weak = ui.as_weak();
    let state_for_close = state.clone();
    ui.on_close_tab(move |tab_id| {
        let mut app = state_for_close
            .lock()
            .expect("app state mutex is not poisoned");
        let target = if tab_id < 0 {
            app.active_window_state().active_tab
        } else {
            TabId(tab_id as u32)
        };
        if app.active_window_state().tabs.contains_key(&target) {
            app.close_tab(target);
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_close);
        }
    });

    let weak = ui.as_weak();
    let sender_for_restore = sender.clone();
    let state_for_restore = state.clone();
    ui.on_restore_tab(move || {
        let restored = state_for_restore
            .lock()
            .expect("app state mutex is not poisoned")
            .restore_closed();
        if let Some((tab_id, path)) = restored {
            submit_navigation(
                &sender_for_restore,
                &state_for_restore,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_restore);
        }
    });

    let weak = ui.as_weak();
    let state_for_activate = state.clone();
    ui.on_activate_tab(move |tab_id| {
        let id = TabId(tab_id as u32);
        let mut app = state_for_activate
            .lock()
            .expect("app state mutex is not poisoned");
        if app.active_window_state().tabs.contains_key(&id) {
            app.active_window_state_mut().active_tab = id;
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_activate);
        }
    });

    let state_for_tab_drag = state.clone();
    ui.on_begin_tab_drag(move |tab_id, source_index, press_x, press_y| {
        if let Ok(mut app) = state_for_tab_drag.lock() {
            let window_id = app.active_window;
            app.begin_tab_drag(
                window_id,
                TabId(tab_id as u32),
                source_index.max(0) as usize,
                press_x,
                press_y,
            );
        }
    });

    let weak = ui.as_weak();
    let state_for_tab_drag = state.clone();
    let senders_for_tab_drag = WorkerSenders {
        directory: sender.clone(),
        operation: operation_sender.clone(),
        clipboard: clipboard_sender.clone(),
        everything: everything_sender.clone(),
        icon: icon_sender.clone(),
    };
    let confirmations_for_tab_drag = ConfirmationWindows::new(delete_ui, conflict_ui, exit_ui);
    ui.on_update_tab_drag(move |x, y, strip_x, strip_width, viewport_x, tab_width| {
        let update = state_for_tab_drag.lock().ok().map(|mut app| {
            let before = app.tab_drag.map(|drag| drag.phase);
            let insertion = app.update_tab_drag(x, y, strip_x, strip_width, viewport_x, tab_width);
            let after = app.tab_drag.map(|drag| drag.phase);
            let became_dragging = !matches!(before, Some(TabDragPhase::Dragging { .. }))
                && matches!(after, Some(TabDragPhase::Dragging { .. }));
            (insertion, became_dragging)
        });
        let Some((insertion, became_dragging)) = update else {
            return -2;
        };
        if should_start_native_tab_drag(became_dragging, false)
            && let Some(ui) = weak.upgrade()
        {
            let hwnd = native_window_handle(&ui);
            let payload = state_for_tab_drag.lock().ok().and_then(|app| {
                app.tab_drag
                    .map(|drag| platform::windows::drag_drop::TabDragPayload {
                        process_id: std::process::id(),
                        source_hwnd: hwnd,
                        tab_id: drag.tab_id.0,
                    })
            });
            let image = native_tab_drag_image(&ui, &state_for_tab_drag.shared);
            ui.set_tab_dragging(true);
            ui.set_suppress_tab_click(true);
            // End Slint's gesture ownership before Windows starts the modal OLE drag loop.
            ui.invoke_release_tab_drag_pointer();
            let result = payload.zip(image).map(|(payload, image)| {
                platform::windows::drag_drop::begin_tab_drag(payload, &image)
            });
            let drop_point = result
                .as_ref()
                .and_then(|result| result.as_ref().ok())
                .and_then(|result| result.dropped)
                .or_else(|| {
                    result
                        .as_ref()
                        .and_then(|result| result.as_ref().ok())
                        .filter(|result| result.released_outside)
                        .and_then(|_| platform::windows::cursor_screen_position().ok())
                        .map(
                            |(screen_x, screen_y)| platform::windows::drag_drop::TabDropPoint {
                                target_hwnd: 0,
                                screen_x,
                                screen_y,
                            },
                        )
                });
            finish_native_tab_drag(
                &ui,
                state_for_tab_drag.window_id,
                drop_point,
                &senders_for_tab_drag,
                &confirmations_for_tab_drag,
                &state_for_tab_drag.shared,
            );
            return -2;
        }
        insertion.map(|index| index as i32).unwrap_or(-2)
    });

    let weak = ui.as_weak();
    let state_for_tab_drag = state.clone();
    ui.on_cancel_tab_drag(move || {
        if let Ok(mut app) = state_for_tab_drag.lock() {
            app.cancel_tab_drag();
        }
        if let Some(ui) = weak.upgrade() {
            ui.invoke_clear_tab_drag();
        }
        project_native_insertion_indicator(None, &state_for_tab_drag.shared);
    });

    let weak = ui.as_weak();
    let sender_for_back = sender.clone();
    let state_for_back = state.clone();
    ui.on_navigate_back(move || {
        let (restored, target) = {
            let mut app = state_for_back
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_window_state().active_tab;
            if app
                .active_window_state_mut()
                .tabs
                .get_mut(&tab_id)
                .is_some_and(TabSession::restore_successful_location)
            {
                (true, None)
            } else {
                (false, app.active().back_target().map(|path| (tab_id, path)))
            }
        };
        let navigated = target.is_some();
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_back,
                &state_for_back,
                tab_id,
                path,
                NavigationKind::Back,
            );
        }
        if (restored || navigated)
            && let Some(ui) = weak.upgrade()
        {
            refresh_ui(&ui, &state_for_back);
        }
    });

    let weak = ui.as_weak();
    let sender_for_forward = sender.clone();
    let state_for_forward = state.clone();
    ui.on_navigate_forward(move || {
        let target = {
            let app = state_for_forward
                .lock()
                .expect("app state mutex is not poisoned");
            app.active()
                .forward_target()
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_forward,
                &state_for_forward,
                tab_id,
                path,
                NavigationKind::Forward,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_forward);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_history = sender.clone();
    let state_for_history = state.clone();
    ui.on_navigate_history(move |is_back, index| {
        let target = {
            let app = state_for_history
                .lock()
                .expect("app state mutex is not poisoned");
            let tab = app.active();
            let stack = if is_back {
                &tab.back_history
            } else {
                &tab.forward_history
            };
            let index = usize::try_from(index).ok();
            index
                .and_then(|index| stack.iter().rev().nth(index))
                .cloned()
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_history,
                &state_for_history,
                tab_id,
                path,
                if is_back {
                    NavigationKind::Back
                } else {
                    NavigationKind::Forward
                },
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_history);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_up = sender.clone();
    let state_for_up = state.clone();
    ui.on_navigate_up(move || {
        let target = {
            let app = state_for_up
                .lock()
                .expect("app state mutex is not poisoned");
            app.active()
                .visible_path()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .map(|path| (app.active_window_state().active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_up,
                &state_for_up,
                tab_id,
                path,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_up);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_refresh = sender.clone();
    let state_for_refresh = state.clone();
    ui.on_refresh(move || {
        let target = {
            let app = state_for_refresh
                .lock()
                .expect("app state mutex is not poisoned");
            if matches!(
                app.active().load_state,
                LoadState::Loading | LoadState::Partial
            ) {
                None
            } else {
                app.active()
                    .requested_path
                    .clone()
                    .or_else(|| app.active().current_path.clone())
                    .map(|path| (app.active_window_state().active_tab, path))
            }
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_refresh,
                &state_for_refresh,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_refresh);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_access = state.clone();
    ui.on_request_folder_access(move || {
        let target = {
            let app = state_for_access
                .lock()
                .expect("app state mutex is not poisoned");
            (app.active().load_state == LoadState::PermissionDenied)
                .then(|| app.active().requested_path.clone())
                .flatten()
        };
        if let Some(target) = target {
            thread::spawn(move || {
                if let Err(error) = platform::request_folder_access(&target) {
                    eprintln!("unable to request folder access through Windows: {error}");
                }
            });
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_access);
        }
    });

    let weak = ui.as_weak();
    let delete_weak_for_language = delete_ui.as_weak();
    let conflict_weak_for_language = conflict_ui.as_weak();
    let exit_weak_for_language = exit_ui.as_weak();
    let state_for_language = state.clone();
    ui.on_change_language(move |language| {
        let mut app = state_for_language
            .lock()
            .expect("app state mutex is not poisoned");
        app.language = if language == 1 {
            Language::English
        } else {
            Language::Chinese
        };
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_language);
        }
        if let (Some(delete_ui), Some(conflict_ui), Some(exit_ui)) = (
            delete_weak_for_language.upgrade(),
            conflict_weak_for_language.upgrade(),
            exit_weak_for_language.upgrade(),
        ) {
            refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state_for_language);
        }
    });

    let weak_for_visibility = ui.as_weak();
    let state_for_visibility = state.clone();
    let sender_for_visibility = sender.clone();
    ui.on_change_file_visibility(move |show_hidden, show_system| {
        let targets = {
            let mut app = state_for_visibility
                .lock()
                .expect("app state mutex is not poisoned");
            app.file_visibility = crate::domain::FileVisibility {
                show_hidden,
                show_system,
            };
            app.active_window_state()
                .tabs
                .values()
                .filter_map(|tab| tab.visible_path().map(|path| (tab.id, path.to_path_buf())))
                .collect::<Vec<_>>()
        };
        for (tab_id, path) in targets {
            submit_navigation(
                &sender_for_visibility,
                &state_for_visibility,
                tab_id,
                path,
                NavigationKind::Refresh,
            );
        }
        if let Some(ui) = weak_for_visibility.upgrade() {
            refresh_ui(&ui, &state_for_visibility);
        }
    });
    let weak = ui.as_weak();
    let delete_weak_for_theme = delete_ui.as_weak();
    let conflict_weak_for_theme = conflict_ui.as_weak();
    let exit_weak_for_theme = exit_ui.as_weak();
    let state_for_theme = state.clone();
    ui.on_change_theme(move |theme| {
        let mut app = state_for_theme
            .lock()
            .expect("app state mutex is not poisoned");
        app.theme_mode = match theme {
            1 => session_store::ThemeMode::Light,
            2 => session_store::ThemeMode::Dark,
            _ => session_store::ThemeMode::System,
        };
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_theme);
        }
        if let (Some(delete_ui), Some(conflict_ui), Some(exit_ui)) = (
            delete_weak_for_theme.upgrade(),
            conflict_weak_for_theme.upgrade(),
            exit_weak_for_theme.upgrade(),
        ) {
            refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state_for_theme);
        }
    });

    let state_for_everything_config = state.clone();
    let everything_for_config = everything_sender.clone();
    ui.on_update_everything_config(move |path, instance| {
        let mut app = state_for_everything_config
            .lock()
            .expect("app state mutex is not poisoned");
        app.everything_config.executable_path =
            (!path.is_empty()).then(|| PathBuf::from(path.as_str()));
        app.everything_config.instance_name = instance.to_string();
        app.everything_config.verified_version = None;
        let config = app.everything_config.clone();
        drop(app);
        let _ = everything_for_config.send(EverythingRequest::Configure(config));
    });
    let everything_for_test = everything_sender.clone();
    ui.on_test_everything_connection(move || {
        let _ = everything_for_test.send(EverythingRequest::TestConnection);
    });
    let everything_for_start = everything_sender.clone();
    ui.on_start_everything(move || {
        let _ = everything_for_start.send(EverythingRequest::Start);
    });
    let weak = ui.as_weak();
    let state_for_settings = state.clone();
    ui.on_open_settings(move || {
        state_for_settings
            .lock()
            .expect("app state mutex is not poisoned")
            .open_settings();
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_settings);
        }
    });

    let context_anchor = Arc::new(Mutex::new((false, 240_i32, 180_i32)));
    let weak = ui.as_weak();
    let state_for_entry_menu = state.clone();
    let anchor_for_entry = context_anchor.clone();
    let clipboard_for_entry = clipboard_sender.clone();
    ui.on_show_entry_menu(move |entry_id, x, y| {
        *anchor_for_entry.lock().expect("context anchor mutex") =
            (false, x.round() as i32, y.round() as i32);
        let _ = clipboard_for_entry.send(ClipboardRequest::CheckAvailability);
        let mut app = state_for_entry_menu
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_window_state().active_tab;
        if entry_id >= 0 {
            let id = EntryId(entry_id as u32);
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id)
                && !tab.selected.contains(&id)
            {
                tab.select_entry(id, false, false);
            }
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            project_context_menu(&ui, &state_for_entry_menu, false);
        }
    });

    let weak = ui.as_weak();
    let state_for_background_menu = state.clone();
    let anchor_for_background = context_anchor.clone();
    let clipboard_for_background = clipboard_sender.clone();
    ui.on_show_background_menu(move |x, y| {
        *anchor_for_background.lock().expect("context anchor mutex") =
            (true, x.round() as i32, y.round() as i32);
        let _ = clipboard_for_background.send(ClipboardRequest::CheckAvailability);
        if let Ok(mut app) = state_for_background_menu.lock() {
            let tab_id = app.active_window_state().active_tab;
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
                tab.clear_selection();
            }
        }
        if let Some(ui) = weak.upgrade() {
            project_context_menu(&ui, &state_for_background_menu, true);
        }
    });

    let weak = ui.as_weak();
    let state_for_reopen_menu = state.clone();
    let anchor_for_reopen = context_anchor.clone();
    let clipboard_for_reopen = clipboard_sender.clone();
    ui.on_reopen_context_menu(move |x, y| {
        let ui = weak.upgrade();
        let (entry_id, background) = context_target_at(
            &state_for_reopen_menu,
            x,
            y,
            ui.as_ref().map_or(0.0, |ui| ui.get_file_list_top()),
            ui.as_ref().map_or(0.0, |ui| ui.get_file_viewport_y()),
            ui.as_ref().map_or(0.0, |ui| ui.get_search_scroll_y()),
            ui.as_ref()
                .map_or(1, |ui| ui.get_grid_column_count().max(1) as usize),
        );
        *anchor_for_reopen.lock().expect("context anchor mutex") =
            (background, x.round() as i32, y.round() as i32);
        let _ = clipboard_for_reopen.send(ClipboardRequest::CheckAvailability);
        if let Ok(mut app) = state_for_reopen_menu.lock() {
            let tab_id = app.active_window_state().active_tab;
            if let Some(tab) = app.active_window_state_mut().tabs.get_mut(&tab_id) {
                if let Some(id) = entry_id {
                    if !tab.selected.contains(&id) {
                        tab.select_entry(id, false, false);
                    }
                } else {
                    tab.clear_selection();
                }
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_context_menu_anchor_x(x);
            ui.set_context_menu_anchor_y(y);
            project_context_menu(&ui, &state_for_reopen_menu, background);
        }
    });

    let weak = ui.as_weak();
    let state_for_context_command = state.clone();
    let sender_for_context_command = operation_sender.clone();
    let delete_weak_for_context = delete_ui.as_weak();
    let clipboard_for_context = clipboard_sender.clone();
    ui.on_invoke_context_command(move |command| {
        match command {
            1 => create_default_folder(&state_for_context_command, &sender_for_context_command),
            2 => request_clipboard_write(&state_for_context_command, &clipboard_for_context, false),
            3 => request_clipboard_write(&state_for_context_command, &clipboard_for_context, true),
            4 => request_clipboard_paste(&state_for_context_command, &clipboard_for_context),
            5 => begin_rename_ui(&weak, &state_for_context_command),
            6 => submit_delete(
                &state_for_context_command,
                &sender_for_context_command,
                false,
            ),
            7 => {
                if delete_weak_for_context
                    .upgrade()
                    .is_some_and(|window| window.window().is_visible())
                {
                    if let (Some(ui), Some(delete_ui)) =
                        (weak.upgrade(), delete_weak_for_context.upgrade())
                    {
                        show_confirmation_window(&ui, None, &delete_ui);
                    }
                    return;
                }
                if let Ok(mut app) = state_for_context_command.lock() {
                    app.pending_permanent_delete = selected_paths(&app)
                        .into_iter()
                        .map(|path| OperationItem::pending(Some(path), None))
                        .collect();
                }
                if let (Some(ui), Some(delete_ui)) =
                    (weak.upgrade(), delete_weak_for_context.upgrade())
                {
                    show_confirmation_window(&ui, None, &delete_ui);
                }
            }
            8 => {
                let (background, x, y) = *context_anchor.lock().expect("context anchor mutex");
                if let Some(ui) = weak.upgrade() {
                    let origin = ui.window().position();
                    let scale = ui.window().scale_factor();
                    show_classic_menu(
                        weak.clone(),
                        &state_for_context_command,
                        sender.clone(),
                        background,
                        native_window_handle(&ui),
                        origin.x + (x as f32 * scale).round() as i32,
                        origin.y + (y as f32 * scale).round() as i32,
                    );
                }
            }
            _ => {}
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_context_command);
        }
    });

    let state_for_copy = state.clone();
    let clipboard_for_copy = clipboard_sender.clone();
    ui.on_copy_selection(move |cut| {
        request_clipboard_write(&state_for_copy, &clipboard_for_copy, cut);
    });
    let state_for_paste = state.clone();
    let clipboard_for_paste = clipboard_sender;
    ui.on_paste_files(move || {
        request_clipboard_paste(&state_for_paste, &clipboard_for_paste);
    });

    let weak = ui.as_weak();
    let state_for_rename = state.clone();
    ui.on_begin_rename(move || {
        begin_rename_ui(&weak, &state_for_rename);
    });
    let weak = ui.as_weak();
    let state_for_commit_rename = state.clone();
    let sender_for_rename = operation_sender.clone();
    ui.on_commit_rename(move |name| {
        if let Err(message) =
            submit_rename(&state_for_commit_rename, &sender_for_rename, name.as_str())
        {
            if let Ok(mut app) = state_for_commit_rename.lock() {
                app.operation_errors.push(message);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_rename_submitting(false);
                ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
                refresh_ui(&ui, &state_for_commit_rename);
            }
        }
    });
    let weak = ui.as_weak();
    let state_for_cancel_rename = state.clone();
    ui.on_cancel_rename(move || {
        if let Ok(mut app) = state_for_cancel_rename.lock() {
            app.rename_target = None;
            app.rename_extension = None;
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_rename_submitting(false);
            ui.set_rename_editing(false);
            refresh_ui(&ui, &state_for_cancel_rename);
        }
    });

    let weak = ui.as_weak();
    let state_for_delete = state.clone();
    let sender_for_delete = operation_sender.clone();
    let delete_weak = delete_ui.as_weak();
    ui.on_request_delete(move |permanent| {
        if permanent {
            if delete_weak
                .upgrade()
                .is_some_and(|window| window.window().is_visible())
            {
                if let (Some(ui), Some(delete_ui)) = (weak.upgrade(), delete_weak.upgrade()) {
                    show_confirmation_window(&ui, None, &delete_ui);
                }
                return;
            }
            if let Ok(mut app) = state_for_delete.lock() {
                app.pending_permanent_delete = selected_paths(&app)
                    .into_iter()
                    .map(|path| OperationItem::pending(Some(path), None))
                    .collect();
            }
            if let (Some(ui), Some(delete_ui)) = (weak.upgrade(), delete_weak.upgrade()) {
                show_confirmation_window(&ui, None, &delete_ui);
            }
        } else {
            submit_delete(&state_for_delete, &sender_for_delete, false);
        }
    });
    let weak = ui.as_weak();
    let state_for_cancel_operation = state.clone();
    ui.on_cancel_operation(move |id| {
        if let Ok(mut app) = state_for_cancel_operation.lock() {
            let operation_id = OperationId(id as u64);
            let _ = app.operations.cancel(operation_id);
            if let Some(response) = app.conflict_responses.remove(&operation_id) {
                let _ = response.send(crate::domain::file_operations::ConflictDecision {
                    action: crate::domain::file_operations::ConflictAction::Skip,
                    apply_to_all: false,
                });
            }
        }
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_cancel_operation);
        }
    });
    let state_for_retry = state.clone();
    let sender_for_retry = operation_sender.clone();
    ui.on_retry_operation(move |id| {
        if let Some(request) = prepare_retry(&state_for_retry, OperationId(id as u64)) {
            let _ = sender_for_retry.send(request);
        }
    });
    let weak = ui.as_weak();
    let state_for_close = state.clone();
    let exit_weak = exit_ui.as_weak();
    ui.on_request_close(move || {
        let action = state_for_close
            .lock()
            .map(|app| app.request_window_close(state_for_close.window_id))
            .unwrap_or(WindowCloseAction::Ignore);
        match action {
            WindowCloseAction::ConfirmApplicationExit => {
                if let (Some(ui), Some(exit_ui)) = (weak.upgrade(), exit_weak.upgrade()) {
                    show_confirmation_window(&ui, None, &exit_ui);
                }
            }
            WindowCloseAction::CloseWindow => {
                if let Ok(mut app) = state_for_close.lock() {
                    app.cancel_tab_drag_for_window(state_for_close.window_id);
                    let _ = app.close_window(state_for_close.window_id);
                }
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.hide();
                }
                remove_window_runtime(state_for_close.window_id);
            }
            WindowCloseAction::ExitApplication => {
                if let Some(ui) = weak.upgrade() {
                    let _ = ui.hide();
                }
            }
            WindowCloseAction::Ignore => {}
        }
    });
}
fn should_close_context_menu(event: &winit::event::WindowEvent) -> bool {
    matches!(
        event,
        winit::event::WindowEvent::Focused(false)
            | winit::event::WindowEvent::Occluded(true)
            | winit::event::WindowEvent::Destroyed
    )
}

fn keyboard_shortcuts_suppressed(rename_editing: bool) -> bool {
    rename_editing
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SideNavigation {
    Back,
    Forward,
}

fn is_side_navigation_mouse_button(button: winit::event::MouseButton) -> bool {
    matches!(
        button,
        winit::event::MouseButton::Back | winit::event::MouseButton::Forward
    )
}
fn side_navigation_for_mouse_button(
    state: winit::event::ElementState,
    button: winit::event::MouseButton,
) -> Option<SideNavigation> {
    if state != winit::event::ElementState::Released {
        return None;
    }
    match button {
        winit::event::MouseButton::Back => Some(SideNavigation::Back),
        winit::event::MouseButton::Forward => Some(SideNavigation::Forward),
        _ => None,
    }
}
fn wire_mouse_navigation(
    ui: &AppWindow,
    confirmations: ConfirmationWindows,
    state: WindowSessions,
    window_id: WindowId,
    senders: WorkerSenders,
    shared_state: SharedSessions,
) {
    use winit::{
        event::{ElementState, MouseScrollDelta, WindowEvent},
        keyboard::{Key, ModifiersState, NamedKey},
    };

    let weak = ui.as_weak();
    let exit_weak = confirmations.exit.clone();
    let modifiers = Cell::new(ModifiersState::empty());
    let cursor_position = Cell::new(winit::dpi::PhysicalPosition::new(0.0, 0.0));
    ui.window().on_winit_window_event(move |_, event| {
        if matches!(event, WindowEvent::CloseRequested) {
            if weak
                .upgrade()
                .is_some_and(|ui| platform::windows::has_pointer_capture(native_window_handle(&ui)))
            {
                platform::windows::release_pointer_capture();
            }
            let action = state
                .lock()
                .map(|app| app.request_window_close(window_id))
                .unwrap_or(WindowCloseAction::Ignore);
            match action {
                WindowCloseAction::ConfirmApplicationExit => {
                    if let Some(ui) = weak.upgrade()
                        && let Some(exit_ui) = exit_weak.upgrade()
                    {
                        show_confirmation_window(&ui, None, &exit_ui);
                    }
                    return EventResult::PreventDefault;
                }
                WindowCloseAction::CloseWindow => {
                    if let Ok(mut app) = state.lock() {
                        app.cancel_tab_drag_for_window(window_id);
                        let _ = app.close_window(window_id);
                    }
                    project_native_insertion_indicator(None, &state);
                    remove_window_runtime(window_id);
                    return EventResult::Propagate;
                }
                WindowCloseAction::ExitApplication => return EventResult::Propagate,
                WindowCloseAction::Ignore => return EventResult::PreventDefault,
            }
        }
        if should_close_context_menu(event) {
            if weak
                .upgrade()
                .is_some_and(|ui| platform::windows::has_pointer_capture(native_window_handle(&ui)))
            {
                platform::windows::release_pointer_capture();
            }
            if let Some(ui) = weak.upgrade() {
                if ui.get_rectangle_selection_pointer_active() {
                    ui.invoke_cancel_rectangle_selection();
                }
                ui.set_context_menu_open(false);
                ui.set_drop_menu_open(false);
                if state
                    .lock()
                    .is_ok_and(|app| app.tab_drag.is_some_and(|drag| drag.window_id == window_id))
                {
                    ui.invoke_cancel_tab_drag();
                }
            }
            if let Ok(mut app) = state.lock() {
                app.pending_right_drop = None;
            }
            project_native_insertion_indicator(None, &state);
            return EventResult::Propagate;
        }
        if let WindowEvent::ModifiersChanged(changed) = event {
            modifiers.set(changed.state());
            return EventResult::Propagate;
        }
        let Some(ui) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        if let WindowEvent::Resized(size) = event {
            platform::windows::window_trace::log_request(
                native_window_handle(&ui),
                "winit-resized",
            );
            ui.set_window_width(size.width as f32 / ui.window().scale_factor());
            if ui.get_view_mode() == 2 {
                request_grid_thumbnails(&ui, &shared_state, window_id, &senders.icon);
            }
            return EventResult::Propagate;
        }
        if matches!(event, WindowEvent::Focused(false)) {
            platform::windows::window_trace::log_request(
                native_window_handle(&ui),
                "winit-focused-false",
            );
        }
        if matches!(
            event,
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && matches!(event.logical_key, Key::Named(NamedKey::Escape))
        ) {
            let mut cancelled = false;
            if ui.get_rectangle_selection_pointer_active() {
                ui.invoke_cancel_rectangle_selection();
                cancelled = true;
            }
            if ui.get_drop_menu_open() {
                ui.set_drop_menu_open(false);
                if let Ok(mut app) = state.lock() {
                    app.pending_right_drop = None;
                }
                cancelled = true;
            }
            if state.lock().is_ok_and(|app| app.tab_drag.is_some()) {
                ui.invoke_cancel_tab_drag();
                cancelled = true;
            }
            return if cancelled {
                EventResult::PreventDefault
            } else {
                EventResult::Propagate
            };
        }
        if matches!(event, WindowEvent::KeyboardInput { .. })
            && keyboard_shortcuts_suppressed(ui.get_rename_editing())
        {
            return EventResult::Propagate;
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                cursor_position.set(*position);
                EventResult::Propagate
            }
            WindowEvent::MouseWheel { delta, .. } if !ui.get_active_is_settings() => {
                let logical = cursor_position
                    .get()
                    .to_logical::<f32>(f64::from(ui.window().scale_factor()));
                if logical.y < ui.get_file_list_top() {
                    return EventResult::Propagate;
                }
                let delta = match delta {
                    MouseScrollDelta::LineDelta(_, y) => *y * 40.0 * 3.0,
                    MouseScrollDelta::PixelDelta(position) => position.y as f32,
                };
                let window_height = ui.window().size().height as f32 / ui.window().scale_factor();
                if logical.y >= window_height - 30.0 {
                    return EventResult::Propagate;
                }
                if ui.get_search_results_mode() {
                    ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
                } else {
                    let visible_height = (window_height - ui.get_file_list_top() - 30.0).max(0.0);
                    let maximum =
                        (ui.get_files().row_count() as f32 * 40.0 - visible_height).max(0.0);
                    let viewport = (ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0);
                    ui.set_file_viewport_y(viewport);
                }
                if ui.get_view_mode() == 2 {
                    request_grid_thumbnails(&ui, &shared_state, window_id, &senders.icon);
                }
                EventResult::PreventDefault
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let modifiers = modifiers.get();
                let control = modifiers.control_key();
                let alt = modifiers.alt_key();
                let shift = modifiers.shift_key();
                let editing_address = ui.get_address_editing();
                let settings_active = ui.get_active_is_settings();
                let character = match &event.logical_key {
                    Key::Character(value) => Some(value.as_str()),
                    _ => None,
                };
                let handled = match &event.logical_key {
                    Key::Named(NamedKey::ArrowLeft) if alt && !control && !shift => {
                        if ui.get_can_navigate_back() {
                            ui.invoke_navigate_back();
                        }
                        true
                    }
                    Key::Named(NamedKey::ArrowRight) if alt && !control && !shift => {
                        if ui.get_can_navigate_forward() {
                            ui.invoke_navigate_forward();
                        }
                        true
                    }
                    Key::Named(NamedKey::ArrowUp) if alt && !control && !shift => {
                        if ui.get_can_navigate_up() {
                            ui.invoke_navigate_up();
                        }
                        true
                    }
                    Key::Named(NamedKey::F5) if !control && !alt && !shift => {
                        if ui.get_can_refresh() {
                            ui.invoke_refresh();
                        }
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("r")) =>
                    {
                        if ui.get_can_refresh() {
                            ui.invoke_refresh();
                        }
                        true
                    }
                    _ if ((control
                        && !alt
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("l")))
                        || (alt
                            && !control
                            && character.is_some_and(|value| value.eq_ignore_ascii_case("d"))))
                        && !shift
                        && !settings_active =>
                    {
                        ui.invoke_begin_search();
                        true
                    }
                    _ if !settings_active
                        && !editing_address
                        && control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("a")) =>
                    {
                        ui.invoke_select_all();
                        true
                    }
                    Key::Named(NamedKey::Space)
                        if !settings_active && !editing_address && control && !alt && !shift =>
                    {
                        ui.invoke_toggle_focused();
                        true
                    }
                    Key::Named(NamedKey::ArrowUp)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_move_focus(-1, shift);
                        true
                    }
                    Key::Named(NamedKey::ArrowDown)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_move_focus(1, shift);
                        true
                    }
                    Key::Named(NamedKey::Home)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_focus_boundary(false, shift);
                        true
                    }
                    Key::Named(NamedKey::End)
                        if !settings_active && !editing_address && !control && !alt =>
                    {
                        ui.invoke_focus_boundary(true, shift);
                        true
                    }
                    Key::Named(NamedKey::Enter)
                        if !settings_active && !editing_address && !control && !alt && !shift =>
                    {
                        ui.invoke_open_entry(-1);
                        true
                    }
                    Key::Named(NamedKey::Escape)
                        if !settings_active && !editing_address && !control && !alt && !shift =>
                    {
                        if ui.get_drop_menu_open() {
                            ui.set_drop_menu_open(false);
                            if let Ok(mut app) = state.lock() {
                                app.pending_right_drop = None;
                            }
                        } else {
                            ui.invoke_clear_selection();
                        }
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("c")) =>
                    {
                        ui.invoke_copy_selection(false);
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("x")) =>
                    {
                        ui.invoke_copy_selection(true);
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("v")) =>
                    {
                        ui.invoke_paste_files();
                        true
                    }
                    Key::Named(NamedKey::F2)
                        if !control && !alt && !shift && !settings_active && !editing_address =>
                    {
                        ui.invoke_begin_rename();
                        true
                    }
                    Key::Named(NamedKey::Delete)
                        if !control && !alt && !settings_active && !editing_address =>
                    {
                        ui.invoke_request_delete(shift);
                        true
                    }
                    Key::Named(NamedKey::F10)
                        if shift && !control && !alt && !settings_active && !editing_address =>
                    {
                        ui.invoke_show_keyboard_context_menu();
                        true
                    }
                    Key::Named(NamedKey::ContextMenu)
                        if !control && !alt && !settings_active && !editing_address =>
                    {
                        ui.invoke_show_keyboard_context_menu();
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("t")) =>
                    {
                        ui.invoke_new_tab();
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("w")) =>
                    {
                        if ui.get_can_close_tab() {
                            ui.invoke_close_tab(-1);
                        }
                        true
                    }
                    _ => false,
                };
                if handled {
                    EventResult::PreventDefault
                } else {
                    EventResult::Propagate
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button,
                ..
            } if is_side_navigation_mouse_button(*button) => EventResult::PreventDefault,
            WindowEvent::MouseInput { state, button, .. }
                if side_navigation_for_mouse_button(*state, *button)
                    == Some(SideNavigation::Back) =>
            {
                if ui.get_can_navigate_back() {
                    let weak = ui.as_weak();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_navigate_back();
                        }
                    });
                }
                EventResult::PreventDefault
            }
            WindowEvent::MouseInput { state, button, .. }
                if side_navigation_for_mouse_button(*state, *button)
                    == Some(SideNavigation::Forward) =>
            {
                if ui.get_can_navigate_forward() {
                    let weak = ui.as_weak();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_navigate_forward();
                        }
                    });
                }
                EventResult::PreventDefault
            }

            _ => EventResult::Propagate,
        }
    });
}

fn should_release_internal_pointer_grab(distance: f32, outbound_started: bool) -> bool {
    distance >= 4.0 && !outbound_started
}

fn should_start_native_tab_drag(became_dragging: bool, native_started: bool) -> bool {
    became_dragging && !native_started
}
fn drop_requires_choice(intent: &platform::windows::drag_drop::DropIntent) -> bool {
    intent.right_button
}
fn selected_right_drop(
    mut intent: platform::windows::drag_drop::DropIntent,
    choice: i32,
) -> Result<Option<platform::windows::drag_drop::DropIntent>, String> {
    let (effect, allowed, key_state) = match choice {
        0 => return Ok(None),
        1 => (
            platform::windows::drag_drop::DropEffect::Copy,
            platform::windows::drag_drop::ALLOW_COPY,
            8,
        ),
        2 => (
            platform::windows::drag_drop::DropEffect::Move,
            platform::windows::drag_drop::ALLOW_MOVE,
            4,
        ),
        3 => (
            platform::windows::drag_drop::DropEffect::Link,
            platform::windows::drag_drop::ALLOW_LINK,
            32,
        ),
        _ => return Err("无效的拖放菜单选择".to_owned()),
    };
    if intent.allowed_effects & allowed == 0 {
        return Err("拖放来源不允许所选操作".to_owned());
    }
    let validation_key_state = key_state | if intent.right_button { 2 } else { 0 };
    let (validated, reason) = platform::windows::drag_drop::negotiate_effect(
        &intent.paths,
        Some(&intent.target),
        validation_key_state,
    );
    if reason.is_some() || validated != effect {
        return Err("所选拖放操作未通过路径保护".to_owned());
    }
    intent.effect = effect;
    intent.right_button = false;
    Ok(Some(intent))
}

fn dispatch_drop_operation(
    intent: platform::windows::drag_drop::DropIntent,
    state: SharedSessions,
    operation_sender: mpsc::Sender<FileOperationRequest>,
) {
    thread::spawn(move || match prepare_drop_operation(intent) {
        Ok(PreparedDrop::Operation(kind, items)) => {
            let _ = slint::invoke_from_event_loop(move || {
                enqueue_operation(&state, &operation_sender, kind, items);
            });
        }
        Ok(PreparedDrop::Shortcuts(shortcuts)) => {
            let result = create_drop_shortcuts(shortcuts);
            let _ = slint::invoke_from_event_loop(move || {
                if let Err(error) = result
                    && let Ok(mut app) = state.lock()
                {
                    app.operation_errors.push(error);
                }
            });
        }
        Err(error) => {
            let _ = slint::invoke_from_event_loop(move || {
                if let Ok(mut app) = state.lock() {
                    app.operation_errors.push(error);
                }
            });
        }
    });
}
fn wire_native_drag_drop(
    ui: &AppWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: WindowSessions,
) -> slint::Timer {
    use std::cell::Cell;

    let window_id = state.window_id;
    let target = Arc::new(Mutex::new(
        platform::windows::drag_drop::DropTargetSnapshot::default(),
    ));
    let (intent_sender, intent_receiver) = mpsc::channel();
    let target_for_refresh = target.clone();
    let target_for_registration = target.clone();
    let intent_for_registration = intent_sender.clone();
    let state_for_target = state.clone();
    let weak_for_target = ui.as_weak();
    let last_sequence = Cell::new(0_u64);
    let registered_hwnd = Cell::new(0_isize);
    let target_timer = slint::Timer::default();
    target_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(100),
        move || {
            let hwnd = weak_for_target
                .upgrade()
                .map(|ui| native_window_handle(&ui))
                .unwrap_or_default();
            if platform::windows::drag_drop::registration_action(registered_hwnd.get(), hwnd)
                == platform::windows::drag_drop::RegistrationAction::Replace
            {
                let previous = registered_hwnd.replace(0);
                if previous != 0 {
                    platform::windows::drag_drop::revoke(previous);
                }
                match platform::windows::drag_drop::register_current(
                    hwnd,
                    target_for_registration.clone(),
                    intent_for_registration.clone(),
                ) {
                    Ok(()) => {
                        registered_hwnd.set(hwnd);
                        let state_for_tabs = state_for_target.shared.clone();
                        platform::windows::drag_drop::set_tab_target_handler(
                            hwnd,
                            Box::new(move |event| {
                                project_native_tab_target(window_id, event, &state_for_tabs);
                            }),
                        );
                    }
                    Err(error) => {
                        eprintln!("failed to register native drag-drop target hwnd={hwnd}: {error}")
                    }
                }
            }
            let snapshot = weak_for_target
                .upgrade()
                .and_then(|ui| {
                    state_for_target
                        .lock()
                        .ok()
                        .and_then(|app| drop_target_snapshot(&app, &ui))
                })
                .unwrap_or_default();
            if let Ok(mut target) = target_for_refresh.lock() {
                *target = snapshot.clone();
            }
            if let Some(ui) = weak_for_target.upgrade() {
                let drag = platform::windows::drag_drop::current_state(native_window_handle(&ui));
                if drag.event_sequence != last_sequence.get() {
                    last_sequence.set(drag.event_sequence);
                    let hovered = drag
                        .target
                        .as_deref()
                        .and_then(|path| {
                            snapshot
                                .folder_rows
                                .iter()
                                .find(|row| row.path == Path::new(path))
                        })
                        .and_then(|row| {
                            state_for_target.lock().ok().and_then(|app| {
                                app.active()
                                    .visible_entries()
                                    .iter()
                                    .find(|entry| entry.path == row.path)
                                    .map(|entry| entry.id.0 as i32)
                            })
                        })
                        .unwrap_or(-1);
                    ui.set_drop_hover_entry_id(hovered);
                    auto_scroll_drag_edge(&ui, &drag);
                }
            }
        },
    );

    let state_for_choice = state.clone();
    let operation_for_choice = operation_sender.clone();
    ui.on_choose_drop_effect(move |choice| {
        let pending = state_for_choice
            .lock()
            .ok()
            .and_then(|mut app| app.pending_right_drop.take());
        let Some(intent) = pending else {
            return;
        };
        match selected_right_drop(intent, choice) {
            Ok(Some(intent)) => dispatch_drop_operation(
                intent,
                state_for_choice.shared.clone(),
                operation_for_choice.clone(),
            ),
            Ok(None) => {}
            Err(error) => {
                if let Ok(mut app) = state_for_choice.lock() {
                    app.operation_errors.push(error);
                }
            }
        }
    });

    let weak_for_intents = ui.as_weak();
    thread::spawn(move || {
        while let Ok(intent) = intent_receiver.recv() {
            eprintln!(
                "drag-drop: intent received right_button={}",
                intent.right_button
            );
            if drop_requires_choice(&intent) {
                let state_for_ui = state.clone();
                let weak_for_ui = weak_for_intents.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_for_ui.upgrade() else {
                        return;
                    };
                    let Ok((client_left, client_top, _, _)) =
                        platform::windows::drag_drop::client_screen_rect(native_window_handle(&ui))
                    else {
                        return;
                    };
                    let scale = ui.window().scale_factor();
                    let x = (intent.screen_x - client_left) as f32 / scale;
                    let y = (intent.screen_y - client_top) as f32 / scale;
                    ui.set_drop_can_copy(
                        intent.allowed_effects & platform::windows::drag_drop::ALLOW_COPY != 0,
                    );
                    ui.set_drop_can_move(
                        intent.allowed_effects & platform::windows::drag_drop::ALLOW_MOVE != 0,
                    );
                    ui.set_drop_can_link(
                        intent.allowed_effects & platform::windows::drag_drop::ALLOW_LINK != 0,
                    );
                    if let Ok(mut app) = state_for_ui.lock() {
                        app.pending_right_drop = Some(intent);
                    }
                    eprintln!("drag-drop: showing right-drop menu x={x} y={y}");
                    ui.invoke_show_drop_menu(x, y);
                });
            } else {
                dispatch_drop_operation(intent, state.shared.clone(), operation_sender.clone());
            }
        }
    });
    target_timer
}

fn auto_scroll_drag_edge(ui: &AppWindow, drag: &platform::windows::drag_drop::DragDropState) {
    if drag.lifecycle != platform::windows::drag_drop::DragDropLifecycle::Dragging {
        return;
    }
    let Some(cursor_y) = drag.cursor_y else {
        return;
    };
    let Ok((_, client_top, _, client_bottom)) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(ui))
    else {
        return;
    };
    let scale = ui.window().scale_factor();
    let list_top = client_top + (ui.get_file_list_top() * scale).round() as i32;
    let bottom = client_bottom - (30.0 * scale).round() as i32;
    let edge = (32.0 * scale).round() as i32;
    let delta = if cursor_y < list_top + edge {
        24.0
    } else if cursor_y > bottom - edge {
        -24.0
    } else {
        0.0
    };
    if delta == 0.0 {
        return;
    }
    if ui.get_search_results_mode() {
        ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
        return;
    }
    let visible_height = ((bottom - list_top).max(0) as f32 / scale).max(0.0);
    let maximum = (ui.get_files().row_count() as f32 * 40.0 - visible_height).max(0.0);
    ui.set_file_viewport_y((ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0));
}

fn drop_target_snapshot(
    app: &AppState,
    ui: &AppWindow,
) -> Option<platform::windows::drag_drop::DropTargetSnapshot> {
    let tab = app.active();
    if tab.kind != TabKind::Files || tab.load_state != LoadState::Complete {
        return None;
    }
    let current = tab.visible_path()?.to_path_buf();
    let hwnd = native_window_handle(ui);
    let (left, top, right, bottom) = platform::windows::drag_drop::client_screen_rect(hwnd).ok()?;
    let scale = ui.window().scale_factor();
    let list_top = (ui.get_file_list_top() * scale).round() as i32;
    let viewport = (-ui.get_file_viewport_y() * scale).max(0.0);
    let row_height = 40.0 * scale;
    let folder_rows = tab
        .visible_entries()
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry.kind == crate::domain::EntryKind::Directory)
        .filter_map(|(index, entry)| {
            let row_top = top + list_top + (index as f32 * row_height - viewport).round() as i32;
            let row_bottom = row_top + row_height.round() as i32;
            (row_bottom > top + list_top && row_top < bottom).then(|| {
                platform::windows::drag_drop::FolderDropTarget {
                    left,
                    top: row_top,
                    right,
                    bottom: row_bottom.min(bottom),
                    path: entry.path.clone(),
                }
            })
        })
        .collect();
    Some(platform::windows::drag_drop::DropTargetSnapshot {
        current: Some(current),
        folder_rows,
    })
}

fn internal_drag_target(
    app: &AppState,
    y: f32,
    list_top: f32,
    viewport_y: f32,
    search_scroll_y: f32,
    grid_columns: usize,
) -> Option<(EntryId, PathBuf)> {
    let tab = app.active();
    let view_mode = tab
        .visible_path()
        .and_then(|path| app.directory_view_modes.get(path))
        .copied()
        .unwrap_or(ViewMode::Details);
    let local_row =
        ((y - list_top + (-viewport_y).max(0.0)) / search_row_height(view_mode)).floor() as usize;
    let local_index = match view_mode {
        ViewMode::Grid => local_row.saturating_mul(grid_columns.max(1)),
        ViewMode::Details | ViewMode::List => local_row,
    };
    let entry = if tab.page_source == PageSource::Search {
        let window = search_window_for_scroll(
            search_scroll_y,
            tab.search_total.unwrap_or(0),
            view_mode,
            grid_columns,
        );
        let id = window
            .start
            .checked_add(local_index as u32)?
            .checked_add(1)?;
        tab.visible_entry(EntryId(id))?
    } else {
        tab.visible_entries().get(local_index)?
    };
    (entry.kind == crate::domain::EntryKind::Directory).then(|| (entry.id, entry.path.clone()))
}

fn create_drop_shortcuts(shortcuts: Vec<(PathBuf, PathBuf)>) -> Result<(), String> {
    for (source, destination) in shortcuts {
        platform::windows::shortcut::create_shortcut(&source, &destination)
            .map_err(|error| format!("创建快捷方式失败：{error}"))?;
    }
    Ok(())
}
enum PreparedDrop {
    Operation(FileOperationKind, Vec<OperationItem>),
    Shortcuts(Vec<(PathBuf, PathBuf)>),
}

fn prepare_drop_operation(
    intent: platform::windows::drag_drop::DropIntent,
) -> Result<PreparedDrop, String> {
    let target_metadata =
        std::fs::metadata(&intent.target).map_err(|error| format!("拖放目标不可用：{error}"))?;
    if !target_metadata.is_dir() {
        return Err("拖放目标不是文件夹".to_owned());
    }
    let kind = match intent.effect {
        platform::windows::drag_drop::DropEffect::Copy => Some(FileOperationKind::Copy),
        platform::windows::drag_drop::DropEffect::Move => Some(FileOperationKind::Move),
        platform::windows::drag_drop::DropEffect::Link => None,
        platform::windows::drag_drop::DropEffect::None => {
            return Err("拖放效果不受支持".to_owned());
        }
    };
    let mut items = Vec::with_capacity(intent.paths.len());
    for source in intent.paths {
        let metadata = std::fs::symlink_metadata(&source)
            .map_err(|error| format!("拖放来源不可用：{error}"))?;
        let _ = metadata.file_type();
        let name = source
            .file_name()
            .ok_or_else(|| "拖放来源没有可用名称".to_owned())?;
        let destination = intent.target.join(name);
        if intent.target.starts_with(&source)
            || (source == destination
                && intent.effect == platform::windows::drag_drop::DropEffect::Move)
        {
            return Err("不能把项目移动到自身或把项目拖放到自身子目录".to_owned());
        }
        items.push(OperationItem::pending(Some(source), Some(destination)));
    }
    if items.is_empty() {
        return Err("拖放未包含文件系统项目".to_owned());
    }
    if let Some(kind) = kind {
        Ok(PreparedDrop::Operation(kind, items))
    } else {
        let mut reserved = std::collections::HashSet::new();
        let mut shortcuts = Vec::with_capacity(items.len());
        for source in items.into_iter().filter_map(|item| item.source) {
            let mut destination =
                platform::windows::shortcut::shortcut_destination(&intent.target, &source)
                    .map_err(|error| error.to_string())?;
            if reserved.contains(&destination) {
                let stem = source
                    .file_stem()
                    .or_else(|| source.file_name())
                    .ok_or_else(|| "拖放来源没有可用名称".to_owned())?;
                for index in 2_u64.. {
                    let mut name = stem.to_os_string();
                    name.push(format!(" ({index})"));
                    let mut candidate = intent.target.join(name);
                    candidate.set_extension("lnk");
                    if !reserved.contains(&candidate) && !candidate.exists() {
                        destination = candidate;
                        break;
                    }
                }
            }
            reserved.insert(destination.clone());
            shortcuts.push((source, destination));
        }
        Ok(PreparedDrop::Shortcuts(shortcuts))
    }
}

fn wire_window_trace(ui: &AppWindow) {
    let Some(path) = platform::windows::window_trace::requested_path() else {
        return;
    };
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        if let Err(error) = platform::windows::window_trace::install(hwnd, &path) {
            eprintln!(
                "failed to install window trace at {}: {error}",
                path.display()
            );
        }
    });
}
fn wire_window_controls(ui: &AppWindow) {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    #[cfg(windows)]
    ui.window().with_winit_window(|window| {
        window.set_corner_preference(CornerPreference::Round);
        window.set_undecorated_shadow(true);
    });

    wire_window_trace(ui);

    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        platform::windows::window_trace::log_request(hwnd, "move-request");
        ui.window().with_winit_window(|window| {
            let _ = window.drag_window();
        });
    });

    let weak = ui.as_weak();
    ui.on_drag_window_after_menu_dismiss(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        platform::windows::window_trace::log_request(hwnd, "move-request-after-menu-dismiss");
        let _ = platform::windows::begin_window_drag(hwnd);
    });
}

fn configure_confirmation_window(ui: &ConfirmationWindow) {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    ui.window().on_close_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_safe_cancel();
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            ui.window().with_winit_window(|window| {
                let _ = window.drag_window();
            });
        }
    });
    ui.window().with_winit_window(|window| {
        window.set_resizable(false);
        window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
        #[cfg(windows)]
        {
            window.set_corner_preference(CornerPreference::Round);
            window.set_undecorated_shadow(true);
        }
    });
}

fn wire_confirmation_windows(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: SharedSessions,
) {
    for confirmation in [delete_ui, conflict_ui, exit_ui] {
        configure_confirmation_window(confirmation);
    }

    let delete_weak = delete_ui.as_weak();
    let state_for_delete = state.clone();
    delete_ui.on_safe_cancel(move || {
        let demo_mode = delete_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if !demo_mode && let Ok(mut app) = state_for_delete.lock() {
            app.pending_permanent_delete.clear();
        }
        if let Some(ui) = delete_weak.upgrade() {
            let _ = ui.hide();
        }
    });
    let delete_weak = delete_ui.as_weak();
    let state_for_delete = state.clone();
    let sender_for_delete = operation_sender.clone();
    delete_ui.on_primary_action(move || {
        if delete_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
            if let Some(ui) = delete_weak.upgrade() {
                let _ = ui.hide();
            }
            return;
        }
        let items = state_for_delete
            .lock()
            .map(|mut app| std::mem::take(&mut app.pending_permanent_delete))
            .unwrap_or_default();
        if let Some(ui) = delete_weak.upgrade() {
            let _ = ui.hide();
        }
        submit_delete_items(&state_for_delete, &sender_for_delete, true, items);
    });

    let resolve_conflict = |operation_id: &str, action, apply_to_all, state: &SharedSessions| {
        let decision = crate::domain::file_operations::ConflictDecision {
            action,
            apply_to_all,
        };
        if let Ok(mut app) = state.lock()
            && let Ok(operation_id) = operation_id.parse::<u64>()
        {
            let id = OperationId(operation_id);
            if let Some(task) = app.operations.task_mut(id) {
                let _ = task.resolve_conflict(decision);
            }
            if let Some(response) = app.conflict_responses.remove(&id) {
                let _ = response.send(decision);
            }
        }
    };
    let conflict_weak = conflict_ui.as_weak();
    let state_for_conflict = state.clone();
    conflict_ui.on_safe_cancel(move || {
        if conflict_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
            if let Some(ui) = conflict_weak.upgrade() {
                let _ = ui.hide();
            }
            return;
        }
        let (operation_id, apply_to_all) = conflict_weak
            .upgrade()
            .map(|ui| (ui.get_operation_id(), ui.get_apply_all()))
            .unwrap_or_default();
        resolve_conflict(
            operation_id.as_str(),
            crate::domain::file_operations::ConflictAction::Skip,
            apply_to_all,
            &state_for_conflict,
        );
        if let Some(conflict_ui) = conflict_weak.upgrade() {
            conflict_ui.set_apply_all(false);
            conflict_ui.set_operation_id("".into());
            let _ = conflict_ui.hide();
        }
        refresh_all_windows(&state_for_conflict);
    });
    for (action_index, action) in [
        crate::domain::file_operations::ConflictAction::Replace,
        crate::domain::file_operations::ConflictAction::Skip,
        crate::domain::file_operations::ConflictAction::KeepBoth,
    ]
    .into_iter()
    .enumerate()
    {
        let conflict_weak = conflict_ui.as_weak();
        let state_for_conflict = state.clone();
        let callback = move || {
            if conflict_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
                if let Some(ui) = conflict_weak.upgrade() {
                    let _ = ui.hide();
                }
                return;
            }
            let (operation_id, apply_to_all) = conflict_weak
                .upgrade()
                .map(|ui| (ui.get_operation_id(), ui.get_apply_all()))
                .unwrap_or_default();
            resolve_conflict(
                operation_id.as_str(),
                action,
                apply_to_all,
                &state_for_conflict,
            );
            if let Some(conflict_ui) = conflict_weak.upgrade() {
                conflict_ui.set_apply_all(false);
                conflict_ui.set_operation_id("".into());
                let _ = conflict_ui.hide();
            }
            refresh_all_windows(&state_for_conflict);
        };
        match action_index {
            0 => conflict_ui.on_primary_action(callback),
            1 => conflict_ui.on_secondary_action(callback),
            _ => conflict_ui.on_tertiary_action(callback),
        }
    }

    let exit_weak = exit_ui.as_weak();
    let state_for_safe_cancel = state.clone();
    exit_ui.on_safe_cancel(move || {
        let demo_mode = exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if !demo_mode {
            open_task_center_on_live_window(&state_for_safe_cancel);
        }
    });
    let exit_weak = exit_ui.as_weak();
    let trace_path = platform::windows::window_trace::active_path()
        .map(Path::to_path_buf)
        .unwrap_or_else(platform::windows::window_trace::default_path);
    ui.set_window_trace_active(platform::windows::window_trace::is_active());
    ui.set_window_trace_status(trace_path.to_string_lossy().into_owned().into());
    let trace_weak = ui.as_weak();
    ui.on_start_window_trace(move || {
        let Some(ui) = trace_weak.upgrade() else {
            return;
        };
        if platform::windows::window_trace::is_active() {
            return;
        }
        let path = platform::windows::window_trace::default_path();
        if let Some(parent) = path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            ui.set_window_trace_status(error.to_string().into());
            return;
        }
        match platform::windows::window_trace::install(native_window_handle(&ui), &path) {
            Ok(()) => {
                ui.set_window_trace_active(true);
                ui.set_window_trace_status(path.to_string_lossy().into_owned().into());
            }
            Err(error) => ui.set_window_trace_status(error.to_string().into()),
        }
    });
    let operation_weak = operation_ui.as_weak();
    let conflict_weak_for_exit = conflict_ui.as_weak();
    let delete_weak_for_exit = delete_ui.as_weak();
    let state_for_exit = state.clone();
    exit_ui.on_primary_action(move || {
        if exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode()) {
            if let Some(ui) = exit_weak.upgrade() {
                let _ = ui.hide();
            }
            return;
        }
        if let Ok(mut app) = state_for_exit.lock() {
            app.exit_after_cancel = true;
            let ids = app
                .operations
                .iter()
                .filter(|task| task.state.is_active())
                .map(|task| task.id)
                .collect::<Vec<_>>();
            for id in ids {
                let _ = app.operations.cancel(id);
                if let Some(response) = app.conflict_responses.remove(&id) {
                    let _ = response.send(crate::domain::file_operations::ConflictDecision {
                        action: crate::domain::file_operations::ConflictAction::Skip,
                        apply_to_all: false,
                    });
                }
            }
        }
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if let Some(conflict_ui) = conflict_weak_for_exit.upgrade() {
            conflict_ui.set_operation_id("".into());
            conflict_ui.set_apply_all(false);
            let _ = conflict_ui.hide();
        }
        if let Some(delete_ui) = delete_weak_for_exit.upgrade() {
            let _ = delete_ui.hide();
        }
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
        if !state_for_exit
            .lock()
            .is_ok_and(|app| app.operations.has_active_tasks())
        {
            hide_all_app_windows();
        }
    });
    let exit_weak = exit_ui.as_weak();
    let state_for_wait = state.clone();
    exit_ui.on_secondary_action(move || {
        let demo_mode = exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if !demo_mode {
            open_task_center_on_live_window(&state_for_wait);
        }
    });
}

fn wire_debug_showcase(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    state: SharedSessions,
) {
    ui.set_debug_tools_enabled(cfg!(debug_assertions));
    if !cfg!(debug_assertions) {
        return;
    }
    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let delete_weak = delete_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    ui.on_show_debug_window(move |kind| {
        let Some(ui) = ui_weak.upgrade() else {
            return;
        };
        match kind {
            0 => {
                if let Some(window) = delete_weak.upgrade() {
                    show_confirmation_window(&ui, None, &window);
                    window.set_demo_mode(true);
                }
            }
            1 => {
                if let Some(window) = conflict_weak.upgrade() {
                    window.set_operation_id("debug-showcase".into());
                    window.set_source_text(r"C:\示例\一段很长的源文件名.txt".into());
                    window.set_destination_text(r"D:\目标\一段很长的目标文件名.txt".into());
                    window.set_apply_all(false);
                    show_confirmation_window(&ui, operation_weak.upgrade().as_ref(), &window);
                    window.set_demo_mode(true);
                }
            }
            2 => {
                if let Some(window) = exit_weak.upgrade() {
                    show_confirmation_window(&ui, None, &window);
                    window.set_demo_mode(true);
                }
            }
            3 => {
                if let Some(window) = operation_weak.upgrade() {
                    refresh_debug_operation_window(&window, &state);
                    position_operation_window_next_to_main(&ui, &window);
                    let _ = window.show();
                }
            }
            _ => {}
        }
    });
}

fn show_confirmation_window(
    ui: &AppWindow,
    operation_ui: Option<&OperationWindow>,
    confirmation_ui: &ConfirmationWindow,
) {
    confirmation_ui.set_demo_mode(false);
    if confirmation_ui.window().is_visible() {
        confirmation_ui
            .window()
            .with_winit_window(|window| window.focus_window());
        return;
    }
    position_window_centered(ui, operation_ui, confirmation_ui);
    let _ = confirmation_ui.show();
    confirmation_ui
        .window()
        .with_winit_window(|window| window.focus_window());
}

fn position_window_centered(
    ui: &AppWindow,
    operation_ui: Option<&OperationWindow>,
    target_ui: &ConfirmationWindow,
) {
    let target_size = target_ui.window().size();
    let operation_visible = operation_ui.is_some_and(|window| window.window().is_visible());
    let mut target = None;
    let mut calculate = |source: &winit::window::Window| {
        let Some(monitor) = source.current_monitor() else {
            return;
        };
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let source_position = source.outer_position().unwrap_or(monitor_position);
        let source_size = source.outer_size();
        let centered_x =
            source_position.x + (source_size.width as i32 - target_size.width as i32) / 2;
        let centered_y =
            source_position.y + (source_size.height as i32 - target_size.height as i32) / 2;
        let max_x = monitor_position.x + monitor_size.width as i32 - target_size.width as i32;
        let max_y = monitor_position.y + monitor_size.height as i32 - target_size.height as i32;
        target = Some(slint::PhysicalPosition::new(
            centered_x.clamp(monitor_position.x, max_x.max(monitor_position.x)),
            centered_y.clamp(monitor_position.y, max_y.max(monitor_position.y)),
        ));
    };
    if operation_visible {
        operation_ui
            .expect("visible operation window exists")
            .window()
            .with_winit_window(&mut calculate);
    } else {
        ui.window().with_winit_window(calculate);
    }
    if let Some(position) = target {
        target_ui.window().set_position(position);
    }
}

fn wire_operation_window(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: SharedSessions,
) -> slint::Timer {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    let operation_weak = operation_ui.as_weak();
    operation_ui.on_request_hide(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
    });
    let operation_weak = operation_ui.as_weak();
    operation_ui.window().on_close_requested(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
        slint::CloseRequestResponse::KeepWindowShown
    });
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_drag_window(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            operation_ui.window().with_winit_window(|window| {
                let _ = window.drag_window();
            });
        }
    });
    #[cfg(windows)]
    operation_ui.window().with_winit_window(|window| {
        window.set_corner_preference(CornerPreference::Round);
        window.set_undecorated_shadow(true);
        window.set_resizable(false);
    });

    let state_for_cancel = state.clone();
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_cancel_operation(move |id| {
        if let Ok(mut app) = state_for_cancel.lock() {
            let id = OperationId(id as u64);
            if let Err(error) = app.operations.cancel(id) {
                app.operation_errors.push(format!(
                    "unable to cancel operation {} from {:?} to {:?}",
                    id.0, error.from, error.to
                ));
            }
            if let Some(response) = app.conflict_responses.remove(&id) {
                let _ = response.send(crate::domain::file_operations::ConflictDecision {
                    action: crate::domain::file_operations::ConflictAction::Skip,
                    apply_to_all: false,
                });
            }
        }
        refresh_all_windows(&state_for_cancel);
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
    });

    let state_for_pause = state.clone();
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_toggle_pause_operation(move |id| {
        if let Ok(mut app) = state_for_pause.lock() {
            let id = OperationId(id as u64);
            if let Err(error) = app.operations.toggle_pause(id) {
                app.operation_errors.push(format!(
                    "unable to pause or resume operation {} from {:?}",
                    id.0, error.from
                ));
            }
        }
        refresh_all_windows(&state_for_pause);
        if let Some(operation_ui) = operation_weak.upgrade() {
            refresh_operation_window(&operation_ui, &state_for_pause);
        }
    });

    let state_for_retry = state.clone();
    let sender_for_retry = operation_sender.clone();
    operation_ui.on_retry_operation(move |id| {
        if let Some(request) = prepare_retry(&state_for_retry, OperationId(id as u64)) {
            let _ = sender_for_retry.send(request);
        }
    });

    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let state_for_open = state.clone();
    ui.on_open_operation_window(move || {
        if let (Some(ui), Some(operation_ui)) = (ui_weak.upgrade(), operation_weak.upgrade()) {
            refresh_operation_window(&operation_ui, &state_for_open);
            position_operation_window_next_to_main(&ui, &operation_ui);
            let _ = operation_ui.show();
        }
    });

    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let state_for_auto_open = state.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(operation_ui) = operation_weak.upgrade() else {
                return;
            };
            let removed = state_for_auto_open
                .lock()
                .map(|mut app| app.operations.prune_transient(Duration::ZERO))
                .unwrap_or_default();
            if removed > 0 {
                refresh_all_windows(&state_for_auto_open);
                refresh_operation_window(&operation_ui, &state_for_auto_open);
            }
            let should_open = state_for_auto_open
                .lock()
                .is_ok_and(|app| should_auto_open_operation_window(&app));
            if should_open && !operation_ui.window().is_visible() {
                refresh_operation_window(&operation_ui, &state_for_auto_open);
                if let Some(ui) = ui_weak.upgrade() {
                    position_operation_window_next_to_main(&ui, &operation_ui);
                }
                let _ = operation_ui.show();
            }
        },
    );
    timer
}

fn should_auto_open_operation_window(app: &AppState) -> bool {
    app.operations.iter().any(|task| {
        matches!(
            task.state,
            OperationState::Preflight | OperationState::Running | OperationState::Paused
        ) && task.cancellation.active_elapsed(task.started_at) >= Duration::from_millis(800)
    })
}

fn position_operation_window_next_to_main(ui: &AppWindow, operation_ui: &OperationWindow) {
    let mut target = None;
    ui.window().with_winit_window(|main| {
        let Some(monitor) = main.current_monitor() else {
            return;
        };
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let operation_size = operation_ui.window().size();
        let main_position = main.outer_position().unwrap_or(monitor_position);
        let main_size = main.outer_size();
        let centered_x =
            main_position.x + (main_size.width as i32 - operation_size.width as i32) / 2;
        let centered_y =
            main_position.y + (main_size.height as i32 - operation_size.height as i32) / 2;
        let max_x = monitor_position.x + monitor_size.width as i32 - operation_size.width as i32;
        let max_y = monitor_position.y + monitor_size.height as i32 - operation_size.height as i32;
        target = Some(slint::PhysicalPosition::new(
            centered_x.clamp(monitor_position.x, max_x.max(monitor_position.x)),
            centered_y.clamp(monitor_position.y, max_y.max(monitor_position.y)),
        ));
    });
    if let Some(position) = target {
        operation_ui.window().set_position(position);
    }
}

fn refresh_operation_window(ui: &OperationWindow, state: &SharedSessions) {
    let Ok(mut app) = state.lock() else {
        return;
    };
    app.operations.prune_transient(Duration::ZERO);
    ui.set_operations(ModelRc::new(VecModel::from(operation_rows(&app))));
    ui.set_dark_theme(app.dark_theme());
    let (title, cancel, retry, pause, resume, empty, minimize, close) = match app.language {
        Language::Chinese => (
            "文件操作",
            "取消",
            "重试",
            "暂停",
            "继续",
            "没有文件操作",
            "最小化",
            "关闭",
        ),
        Language::English => (
            "File operations",
            "Cancel",
            "Retry",
            "Pause",
            "Resume",
            "No file operations",
            "Minimize",
            "Close",
        ),
    };
    ui.set_text_file_operations(title.into());
    ui.set_text_cancel_operation(cancel.into());
    ui.set_text_retry_operation(retry.into());
    ui.set_text_pause_operation(pause.into());
    ui.set_text_resume_operation(resume.into());
    ui.set_text_no_operations(empty.into());
    ui.set_text_window_minimize(minimize.into());
    ui.set_text_window_close(close.into());
}

fn refresh_debug_operation_window(ui: &OperationWindow, state: &SharedSessions) {
    refresh_operation_window(ui, state);
    let language = state
        .lock()
        .map(|app| app.language)
        .unwrap_or(Language::Chinese);
    let (title, transferred, speed, eta) = match language {
        Language::Chinese => ("正在复制示例文件", "384 MB / 1.0 GB", "48 MB/s", "约 14 秒"),
        Language::English => (
            "Copying sample files",
            "384 MB / 1.0 GB",
            "48 MB/s",
            "About 14 seconds",
        ),
    };
    ui.set_operations(ModelRc::new(VecModel::from(vec![OperationRow {
        id: -1,
        title: title.into(),
        percent: "38%".into(),
        file_progress: "12 / 32".into(),
        transferred: transferred.into(),
        speed: speed.into(),
        eta: eta.into(),
        current_item: "Example document with a long name.pdf".into(),
        source: r"C:\Users\Example\Documents".into(),
        destination: r"D:\Backup\Documents".into(),
        progress: 0.38,
        state: operation_state_index(OperationState::Running),
        paused: false,
        can_pause: false,
        can_cancel: false,
        can_retry: false,
    }])));
}

fn refresh_confirmation_windows(
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    state: &SharedSessions,
) {
    let Ok(app) = state.lock() else {
        return;
    };
    let (
        cancel,
        close,
        delete_title,
        delete_detail,
        delete,
        exit_title,
        exit_detail,
        cancel_exit,
        wait,
        conflict_title,
        conflict_detail,
        replace,
        skip,
        keep_both,
        apply_all,
        source_label,
        destination_label,
    ) = match app.language {
        Language::Chinese => (
            "取消",
            "关闭",
            "永久删除所选项目？",
            "此操作无法撤销。",
            "删除",
            "文件操作仍在进行",
            "等待任务完成，或取消任务后退出。",
            "取消任务并退出",
            "等待完成",
            "目标位置已有同名项目",
            "请选择如何处理当前冲突。",
            "替换",
            "跳过",
            "保留两者",
            "对这类冲突全部应用",
            "来源：",
            "目标：",
        ),
        Language::English => (
            "Cancel",
            "Close",
            "Permanently delete selected items?",
            "This action cannot be undone.",
            "Delete",
            "File operations are still running",
            "Wait for them to finish, or cancel the tasks before exiting.",
            "Cancel tasks and exit",
            "Wait",
            "An item with the same name already exists",
            "Choose how to handle this conflict.",
            "Replace",
            "Skip",
            "Keep both",
            "Apply to all conflicts of this type",
            "From: ",
            "To: ",
        ),
    };
    for window in [delete_ui, conflict_ui, exit_ui] {
        window.set_demo_mode(false);
        window.set_dark_theme(app.dark_theme());
        window.set_close_text(close.into());
    }
    delete_ui.set_kind(0);
    delete_ui.set_title_text(delete_title.into());
    delete_ui.set_detail_text(delete_detail.into());
    delete_ui.set_cancel_text(cancel.into());
    delete_ui.set_primary_text(delete.into());
    conflict_ui.set_kind(1);
    conflict_ui.set_title_text(conflict_title.into());
    conflict_ui.set_detail_text(conflict_detail.into());
    conflict_ui.set_cancel_text(cancel.into());
    conflict_ui.set_primary_text(replace.into());
    conflict_ui.set_secondary_text(skip.into());
    conflict_ui.set_tertiary_text(keep_both.into());
    conflict_ui.set_apply_all_text(apply_all.into());
    conflict_ui.set_source_label(source_label.into());
    conflict_ui.set_destination_label(destination_label.into());
    exit_ui.set_kind(2);
    exit_ui.set_title_text(exit_title.into());
    exit_ui.set_detail_text(exit_detail.into());
    exit_ui.set_primary_text(cancel_exit.into());
    exit_ui.set_secondary_text(wait.into());
}
fn scan_cleanup_diagnostics(_ui: &AppWindow, state: SharedSessions) {
    let roots = state
        .lock()
        .map(|app| app.stable_paths())
        .unwrap_or_default();
    thread::spawn(move || {
        let pending = roots
            .into_iter()
            .map(|path| path.join(".asterfiles-cleanup"))
            .find(|path| {
                std::fs::read_dir(path)
                    .ok()
                    .and_then(|mut entries| entries.next())
                    .is_some()
            });
        if let Some(path) = pending {
            let state_for_ui = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Ok(mut app) = state_for_ui.lock() {
                    app.operation_errors.push(format!(
                        "Pending cleanup requires attention: {}",
                        display_path(&path)
                    ));
                }
                refresh_all_windows(&state_for_ui);
            });
        }
    });
}

fn spawn_clipboard_worker() -> (
    mpsc::Sender<ClipboardRequest>,
    mpsc::Receiver<ClipboardEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel();
    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            let event = match request {
                ClipboardRequest::Write { paths, cut } => ClipboardEvent::Written {
                    result: platform::windows::clipboard::write_file_list(
                        &paths,
                        if cut {
                            platform::windows::clipboard::ClipboardOperation::Move
                        } else {
                            platform::windows::clipboard::ClipboardOperation::Copy
                        },
                    )
                    .map_err(|error| error.to_string()),
                    paths,
                    cut,
                },
                ClipboardRequest::CheckAvailability => ClipboardEvent::Availability(
                    platform::windows::clipboard::read_file_list()
                        .map(|clipboard| clipboard.is_some())
                        .map_err(|error| error.to_string()),
                ),
                ClipboardRequest::ReadPaste { target } => ClipboardEvent::Paste(
                    platform::windows::clipboard::read_file_list()
                        .map(|clipboard| {
                            clipboard.map(|clipboard| {
                                let kind = match clipboard.operation {
                                    platform::windows::clipboard::ClipboardOperation::Copy => {
                                        FileOperationKind::Copy
                                    }
                                    platform::windows::clipboard::ClipboardOperation::Move => {
                                        FileOperationKind::Move
                                    }
                                };
                                let items = clipboard
                                    .paths
                                    .into_iter()
                                    .filter_map(|source| {
                                        source.file_name().map(|name| {
                                            OperationItem::pending(
                                                Some(source.clone()),
                                                Some(target.join(name)),
                                            )
                                        })
                                    })
                                    .collect();
                                (kind, items)
                            })
                        })
                        .map_err(|error| error.to_string()),
                ),
            };
            let _ = event_sender.send(event);
        }
    });
    (request_sender, event_receiver)
}

fn start_clipboard_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<ClipboardEvent>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let weak = weak.clone();
            let state = state.clone();
            let operation_sender = operation_sender.clone();
            let directory_sender = directory_sender.clone();
            let _ = slint::invoke_from_event_loop(move || {
                match event {
                    ClipboardEvent::Written {
                        result: Err(error), ..
                    }
                    | ClipboardEvent::Paste(Err(error))
                    | ClipboardEvent::Availability(Err(error)) => {
                        if let Ok(mut app) = state.lock() {
                            app.operation_errors.push(error);
                        }
                    }
                    ClipboardEvent::Paste(Ok(Some((kind, items)))) => {
                        enqueue_operation(&state, &operation_sender, kind, items)
                    }
                    ClipboardEvent::Written {
                        result: Ok(()),
                        paths,
                        cut,
                    } => {
                        let generation = if let Ok(mut app) = state.lock() {
                            app.cut_generation = app.cut_generation.wrapping_add(1);
                            app.cut_paths = if cut { paths.clone() } else { Vec::new() };
                            app.cut_generation
                        } else {
                            0
                        };
                        if cut && generation != 0 {
                            monitor_external_cut(
                                paths,
                                generation,
                                directory_sender.clone(),
                                state.clone(),
                            );
                        }
                    }
                    ClipboardEvent::Availability(Ok(available)) => {
                        if let Ok(mut app) = state.lock() {
                            app.clipboard_has_files = available;
                        }
                    }
                    ClipboardEvent::Paste(Ok(None)) => {}
                }
                if let Some(ui) = weak.upgrade() {
                    refresh_all_windows(&state);
                    if ui.get_context_menu_open() {
                        let background = ui.get_context_menu_on_background();
                        project_context_menu(&ui, &state, background);
                    }
                }
            });
        }
    });
}

fn monitor_external_cut(
    mut paths: Vec<PathBuf>,
    generation: u64,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: SharedSessions,
) {
    thread::spawn(move || {
        let parents = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        for _ in 0..1_200 {
            thread::sleep(Duration::from_millis(250));
            if state
                .lock()
                .map_or(true, |app| app.cut_generation != generation)
            {
                return;
            }
            let remaining = existing_paths(&paths);
            if remaining.len() == paths.len() {
                continue;
            }
            let remaining_for_ui = remaining.clone();
            let parents_for_ui = parents.clone();
            let directory_sender_for_ui = directory_sender.clone();
            let state_for_ui = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let current = state_for_ui.lock().is_ok_and(|mut app| {
                    if app.cut_generation != generation {
                        return false;
                    }
                    app.cut_paths = remaining_for_ui;
                    true
                });
                if current {
                    refresh_affected_tabs(&directory_sender_for_ui, &state_for_ui, &parents_for_ui);
                }
            });
            if remaining.is_empty() {
                return;
            }
            paths = remaining;
        }
    });
}

fn completed_target_for_item(
    item: &OperationItem,
    completed_paths: &[PathBuf],
    destination_was_existing_directory: bool,
) -> Option<PathBuf> {
    let destination = item.destination.as_deref()?;
    if destination_was_existing_directory {
        return None;
    }
    let parent = destination.parent()?;
    completed_paths
        .iter()
        .rfind(|path| {
            path.parent() == Some(parent) && item.source.as_deref() != Some(path.as_path())
        })
        .cloned()
}

fn queue_completed_focus(app: &mut AppState, targets: &[PathBuf]) {
    let directories = targets
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<std::collections::HashSet<_>>();
    for directory in directories {
        let paths = targets
            .iter()
            .filter(|path| path.parent() == Some(directory.as_path()))
            .cloned()
            .collect::<Vec<_>>();
        let matching_tabs = app
            .windows
            .values()
            .flat_map(|window| window.tabs.values())
            .filter(|tab| tab.visible_path() == Some(directory.as_path()))
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        for tab_id in matching_tabs {
            app.focus_after_refresh.insert(
                tab_id,
                PendingFocus {
                    directory: directory.clone(),
                    request_id: None,
                    paths: paths.clone(),
                },
            );
        }
    }
}

fn existing_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .filter(|path| std::fs::symlink_metadata(path).is_ok())
        .cloned()
        .collect()
}
fn spawn_file_operation_worker() -> (
    mpsc::Sender<FileOperationRequest>,
    mpsc::Receiver<FileOperationEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<FileOperationRequest>();
    let (event_sender, event_receiver) = mpsc::channel::<FileOperationEvent>();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            let mut succeeded = Vec::new();
            let mut skipped = Vec::new();
            let mut failed = Vec::new();
            let mut affected = Vec::new();
            let mut indexed_states = Vec::new();
            let mut completed_targets = Vec::new();
            let started = Instant::now();
            let totals = request
                .items
                .iter()
                .filter(|item| item.state == ItemState::Pending)
                .filter_map(|item| item.source.as_deref())
                .try_fold((0_u64, 0_usize), |(bytes, files), path| {
                    tree_totals(path, &request.cancellation).map(|(next_bytes, next_files)| {
                        (
                            bytes.saturating_add(next_bytes),
                            files.saturating_add(next_files),
                        )
                    })
                });
            let (total_bytes, total_files) = totals
                .map(|(bytes, files)| (Some(bytes), Some(files)))
                .unwrap_or((None, None));
            let _ = event_sender.send(FileOperationEvent::Progress {
                id: request.id,
                completed_items: 0,
                completed_files: 0,
                total_files,
                processed_bytes: 0,
                total_bytes,
                current_item: request
                    .items
                    .first()
                    .and_then(|item| item.source.clone().or_else(|| item.destination.clone()))
                    .unwrap_or_default(),
                started,
            });
            let mut processed_bytes = 0_u64;
            let mut processed_files = 0_usize;
            let mut conflict_defaults = HashMap::new();
            for (item_index, item) in request.items.iter().enumerate() {
                if item.state != ItemState::Pending {
                    continue;
                }
                if request.cancellation.is_cancelled() {
                    indexed_states.push((item_index, ItemState::Cancelled, None));
                    continue;
                }
                let destination_was_existing_directory = item.source != item.destination
                    && item
                        .source
                        .as_deref()
                        .and_then(|path| std::fs::symlink_metadata(path).ok())
                        .is_some_and(|metadata| metadata.file_type().is_dir())
                    && item
                        .destination
                        .as_deref()
                        .and_then(|path| std::fs::symlink_metadata(path).ok())
                        .is_some_and(|metadata| metadata.file_type().is_dir());
                let outcome = execute_file_operation_item(
                    request.id,
                    request.kind,
                    item,
                    &request.cancellation,
                    &event_sender,
                    item_index,
                    processed_bytes,
                    processed_files,
                    total_bytes,
                    total_files,
                    started,
                    &mut conflict_defaults,
                );
                match outcome {
                    Ok(report) => {
                        processed_bytes = processed_bytes.saturating_add(report.bytes);
                        processed_files = processed_files.saturating_add(report.files);
                        if report.skipped.is_empty()
                            && let Some(target) = completed_target_for_item(
                                item,
                                &report.completed_paths,
                                destination_was_existing_directory,
                            )
                            && !completed_targets.contains(&target)
                        {
                            completed_targets.push(target);
                        }
                        let current_item = item
                            .source
                            .clone()
                            .or_else(|| item.destination.clone())
                            .unwrap_or_default();
                        let _ = event_sender.send(FileOperationEvent::Progress {
                            id: request.id,
                            completed_items: item_index + 1,
                            completed_files: processed_files,
                            total_files,
                            processed_bytes,
                            total_bytes,
                            current_item,
                            started,
                        });
                        let identity = item
                            .destination
                            .clone()
                            .or_else(|| item.source.clone())
                            .unwrap_or_default();
                        if report.skipped.is_empty() {
                            succeeded.push(identity);
                            indexed_states.push((item_index, ItemState::Succeeded, None));
                        } else {
                            skipped.extend(report.skipped);
                            indexed_states.push((item_index, ItemState::Skipped, None));
                        }
                        for directory in report.affected_directories {
                            if !affected.contains(&directory) {
                                affected.push(directory);
                            }
                        }
                    }
                    Err(message) => {
                        let identity = item
                            .source
                            .clone()
                            .or_else(|| item.destination.clone())
                            .unwrap_or_default();
                        if request.cancellation.is_cancelled() {
                            indexed_states.push((item_index, ItemState::Cancelled, None));
                        } else {
                            failed.push((identity, message.clone()));
                            indexed_states.push((item_index, ItemState::Failed, Some(message)));
                        }
                    }
                }
            }
            let _ = event_sender.send(FileOperationEvent::Finished {
                id: request.id,
                result: OperationResult {
                    succeeded,
                    skipped,
                    failed,
                    affected_directories: affected,
                },
                item_states: indexed_states,
                completed_targets,
            });
        }
    });
    (request_sender, event_receiver)
}

fn tree_totals(
    path: &Path,
    cancellation: &crate::domain::file_operations::CancellationToken,
) -> Option<(u64, usize)> {
    cancellation.wait_if_paused();
    if cancellation.is_cancelled() {
        return None;
    }
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if metadata.file_type().is_symlink() {
        return Some((0, 1));
    }
    if metadata.is_file() {
        return Some((metadata.len(), 1));
    }
    let mut total_bytes = 0_u64;
    let mut total_files = 0_usize;
    for entry in std::fs::read_dir(path).ok()? {
        let (bytes, files) = tree_totals(&entry.ok()?.path(), cancellation)?;
        total_bytes = total_bytes.saturating_add(bytes);
        total_files = total_files.saturating_add(files);
    }
    Some((total_bytes, total_files))
}

fn file_snapshot(path: &Path) -> crate::domain::file_operations::FileSnapshot {
    let metadata = std::fs::symlink_metadata(path).ok();
    crate::domain::file_operations::FileSnapshot {
        path: path.to_path_buf(),
        is_directory: metadata
            .as_ref()
            .is_some_and(|value| value.file_type().is_dir()),
        size_bytes: metadata
            .as_ref()
            .filter(|value| value.is_file())
            .map(|value| value.len()),
        modified: metadata.and_then(|value| value.modified().ok()),
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_file_operation_item(
    id: OperationId,
    kind: FileOperationKind,
    item: &OperationItem,
    cancel: &crate::domain::file_operations::CancellationToken,
    events: &mpsc::Sender<FileOperationEvent>,
    completed_items: usize,
    base_processed_bytes: u64,
    base_processed_files: usize,
    operation_total_bytes: Option<u64>,
    operation_total_files: Option<usize>,
    started: Instant,
    conflict_defaults: &mut HashMap<
        crate::domain::file_operations::ConflictCategory,
        crate::domain::file_operations::ConflictAction,
    >,
) -> Result<crate::fs::file_operations::FileOperationReport, String> {
    let replace = &mut |category, source: &Path, destination: &Path| {
        if let Some(action) = conflict_defaults.get(&category).copied() {
            return action;
        }
        let (response_sender, response_receiver) = mpsc::channel();
        let conflict = crate::domain::file_operations::OperationConflict {
            category,
            source: file_snapshot(source),
            destination: file_snapshot(destination),
        };
        if events
            .send(FileOperationEvent::Conflict {
                id,
                conflict,
                response: response_sender,
            })
            .is_err()
        {
            return crate::domain::file_operations::ConflictAction::Skip;
        }
        match response_receiver.recv() {
            Ok(decision) => {
                if decision.apply_to_all {
                    conflict_defaults.insert(category, decision.action);
                }
                decision.action
            }
            Err(_) => crate::domain::file_operations::ConflictAction::Skip,
        }
    };
    match kind {
        FileOperationKind::CreateFolder => {
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let parent = destination.parent().ok_or("missing parent")?;
            let name = destination.file_name().ok_or("missing name")?;
            crate::fs::file_operations::create_folder(parent, name)
                .map(|path| {
                    let mut report = crate::fs::file_operations::FileOperationReport {
                        files: 0,
                        directories: 1,
                        bytes: 0,
                        skipped: vec![],
                        affected_directories: vec![],
                        cleanup_pending: None,
                        completed_paths: vec![path.clone()],
                    };
                    if let Some(parent) = path.parent() {
                        report.affected_directories.push(parent.to_path_buf());
                    }
                    report
                })
                .map_err(|error| format!("{error:?}"))
        }
        FileOperationKind::Rename => {
            let source = item.source.as_ref().ok_or("missing source")?;
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let name = destination.file_name().ok_or("missing name")?;
            crate::fs::file_operations::rename_path(source, name)
                .map(|path| {
                    let mut report = crate::fs::file_operations::FileOperationReport {
                        files: 1,
                        directories: 0,
                        bytes: 0,
                        skipped: vec![],
                        affected_directories: vec![],
                        cleanup_pending: None,
                        completed_paths: vec![path.clone()],
                    };
                    if let Some(parent) = path.parent() {
                        report.affected_directories.push(parent.to_path_buf());
                    }
                    report
                })
                .map_err(|error| format!("{error:?}"))
        }
        FileOperationKind::Copy | FileOperationKind::Move => {
            let source = item.source.as_ref().ok_or("missing source")?;
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let mut processed = base_processed_bytes;
            let mut completed_files = base_processed_files;
            let mut progress = |bytes, file_completed, current: &Path| {
                processed = processed.saturating_add(bytes);
                if file_completed {
                    completed_files = completed_files.saturating_add(1);
                }
                let _ = events.send(FileOperationEvent::Progress {
                    id,
                    completed_items,
                    completed_files,
                    total_files: operation_total_files,
                    processed_bytes: processed,
                    total_bytes: operation_total_bytes,
                    current_item: current.to_path_buf(),
                    started,
                });
            };
            let mut root_destination_reported = false;
            let result = if kind == FileOperationKind::Copy {
                crate::fs::file_operations::copy_path_with_progress(
                    source,
                    destination,
                    cancel,
                    replace,
                    &mut progress,
                    &mut |path| {
                        if !root_destination_reported {
                            root_destination_reported = true;
                            let _ = events.send(FileOperationEvent::DestinationCreated {
                                id,
                                path: path.to_path_buf(),
                            });
                        }
                    },
                )
            } else {
                crate::fs::file_operations::move_path_with_progress(
                    source,
                    destination,
                    cancel,
                    replace,
                    &mut progress,
                )
            };
            result.map_err(|error| format!("{error:?}"))
        }
        FileOperationKind::RecycleDelete => {
            let path = item.source.as_ref().ok_or("missing source")?.clone();
            let result = platform::windows::file_operation::recycle(&[path]);
            let first = result
                .items
                .into_iter()
                .next()
                .ok_or("missing recycle result")?;
            first
                .result
                .map(|_| crate::fs::file_operations::FileOperationReport {
                    files: 1,
                    directories: 0,
                    bytes: 0,
                    skipped: vec![],
                    affected_directories: first
                        .path
                        .parent()
                        .map(Path::to_path_buf)
                        .into_iter()
                        .collect(),
                    cleanup_pending: None,
                    completed_paths: vec![],
                })
        }
        FileOperationKind::PermanentDelete => {
            let path = item.source.as_ref().ok_or("missing source")?;
            if should_fast_remove(path) {
                let parent = path.parent().ok_or("missing parent")?;
                let report = crate::fs::file_operations::fast_remove(
                    path,
                    &parent.join(".asterfiles-cleanup"),
                    cancel,
                )
                .map_err(|error| format!("{error:?}"))?;
                if let Some(pending) = report.cleanup_pending.as_ref()
                    && let Err(error) = crate::fs::file_operations::clean_pending(pending, cancel)
                {
                    let _ = std::fs::rename(pending, path);
                    return Err(format!(
                        "cleanup pending at {}: {error:?}",
                        display_path(pending)
                    ));
                }
                Ok(report)
            } else {
                crate::fs::file_operations::permanently_delete(path, cancel)
                    .map_err(|error| format!("{error:?}"))
            }
        }
        FileOperationKind::FastRemove => {
            let path = item.source.as_ref().ok_or("missing source")?;
            let parent = path.parent().ok_or("missing parent")?;
            let report = crate::fs::file_operations::fast_remove(
                path,
                &parent.join(".asterfiles-cleanup"),
                cancel,
            )
            .map_err(|error| format!("{error:?}"))?;
            if let Some(pending) = report.cleanup_pending.as_ref()
                && let Err(error) = crate::fs::file_operations::clean_pending(pending, cancel)
            {
                let _ = std::fs::rename(pending, path);
                return Err(format!(
                    "cleanup pending at {}: {error:?}",
                    display_path(pending)
                ));
            }
            Ok(report)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn start_file_operation_event_pump(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    receiver: mpsc::Receiver<FileOperationEvent>,
    sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    let operation_weak = operation_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let delete_weak = delete_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let weak = weak.clone();
            let operation_weak = operation_weak.clone();
            let conflict_weak = conflict_weak.clone();
            let delete_weak = delete_weak.clone();
            let exit_weak = exit_weak.clone();
            let sender = sender.clone();
            let directory_sender = directory_sender.clone();
            let state = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let event_operation_id = match &event {
                    FileOperationEvent::DestinationCreated { id, .. }
                    | FileOperationEvent::Progress { id, .. }
                    | FileOperationEvent::Conflict { id, .. }
                    | FileOperationEvent::Finished { id, .. } => *id,
                };
                match event {
                    FileOperationEvent::DestinationCreated { path, .. } => {
                        let parent = path.parent().map(Path::to_path_buf);
                        if let Some(parent) = parent {
                            refresh_affected_tabs(&directory_sender, &state, &[parent]);
                        }
                    }
                    FileOperationEvent::Progress {
                        id,
                        completed_items,
                        completed_files,
                        total_files,
                        processed_bytes,
                        total_bytes,
                        current_item,
                        started,
                    } => {
                        if let Ok(mut app) = state.lock()
                            && let Some(task) = app.operations.task_mut(id)
                        {
                            task.progress.completed_items = completed_items;
                            task.progress.completed_files = completed_files;
                            task.progress.total_files = total_files;
                            task.progress.processed_bytes = processed_bytes;
                            task.progress.total_bytes = total_bytes;
                            task.progress.current_item = Some(current_item);
                            let _elapsed = started.elapsed();
                        }
                    }
                    FileOperationEvent::Conflict {
                        id,
                        conflict,
                        response,
                    } => {
                        let source = display_path(&conflict.source.path);
                        let destination = display_path(&conflict.destination.path);
                        if let Ok(mut app) = state.lock() {
                            if let Some(task) = app.operations.task_mut(id) {
                                let _ = task.set_conflict(conflict);
                            }
                            app.conflict_responses.insert(id, response);
                        }
                        if let (Some(ui), Some(conflict_ui)) =
                            (weak.upgrade(), conflict_weak.upgrade())
                        {
                            conflict_ui.set_source_text(source.into());
                            conflict_ui.set_destination_text(destination.into());
                            conflict_ui.set_operation_id(id.0.to_string().into());
                            conflict_ui.set_apply_all(false);
                            show_confirmation_window(
                                &ui,
                                operation_weak.upgrade().as_ref(),
                                &conflict_ui,
                            );
                        }
                    }
                    FileOperationEvent::Finished {
                        id,
                        result,
                        item_states,
                        completed_targets,
                    } => {
                        let (affected, next) =
                            {
                                let mut app =
                                    state.lock().expect("app state mutex is not poisoned");
                                if let Some(task) = app.operations.task_mut(id) {
                                    for (index, status, error) in item_states {
                                        if let Some(item) = task.items.get_mut(index) {
                                            item.state = status;
                                            item.error = error;
                                        }
                                    }
                                }
                                let cancelled = app
                                    .operations
                                    .task(id)
                                    .is_some_and(|task| task.cancellation.is_cancelled());
                                let terminal = if cancelled
                                    && result.succeeded.is_empty()
                                    && result.failed.is_empty()
                                {
                                    OperationState::Cancelled
                                } else if cancelled
                                    || (!result.failed.is_empty() && !result.succeeded.is_empty())
                                {
                                    OperationState::PartiallyCompleted
                                } else if result.failed.is_empty() {
                                    OperationState::Completed
                                } else {
                                    OperationState::Failed
                                };
                                let affected = result.affected_directories.clone();
                                app.conflict_responses.remove(&id);
                                queue_completed_focus(&mut app, &completed_targets);
                                let _ = app.operations.finish(id, terminal, result);
                                if cancelled {
                                    app.operations.remove_terminal(id);
                                }
                                let next = app.operations.start_next().ok().flatten().and_then(
                                    |next_id| {
                                        let _ = app.operations.mark_running(next_id);
                                        app.operations.task(next_id).map(|task| {
                                            FileOperationRequest {
                                                id: next_id,
                                                kind: task.kind,
                                                items: task.items.clone(),
                                                cancellation: task.cancellation.clone(),
                                            }
                                        })
                                    },
                                );
                                (affected, next)
                            };
                        if let Some(request) = next {
                            let _ = sender.send(request);
                        }
                        refresh_affected_tabs(&directory_sender, &state, &affected);
                    }
                }
                if let Some(ui) = weak.upgrade() {
                    let close_editor = state
                        .lock()
                        .ok()
                        .and_then(|app| app.operations.task(event_operation_id).cloned())
                        .is_some_and(|task| {
                            matches!(
                                task.kind,
                                FileOperationKind::CreateFolder | FileOperationKind::Rename
                            ) && task.state == OperationState::Completed
                        });
                    if close_editor {
                        if let Ok(mut app) = state.lock() {
                            app.rename_target = None;
                            app.rename_extension = None;
                        }
                        ui.set_rename_submitting(false);
                        ui.set_rename_editing(false);
                    } else {
                        let failed_rename = state
                            .lock()
                            .ok()
                            .and_then(|app| app.operations.task(event_operation_id).cloned())
                            .filter(|task| task.kind == FileOperationKind::Rename)
                            .filter(|task| task.state != OperationState::Running)
                            .and_then(|task| task.items.into_iter().find_map(|item| item.error));
                        if let Some(message) = failed_rename {
                            if let Ok(mut app) = state.lock() {
                                app.operation_errors.push(message);
                            }
                            ui.set_rename_submitting(false);
                            ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
                        }
                    }
                    refresh_all_windows(&state);
                    if state.lock().is_ok_and(|app| {
                        app.exit_after_cancel && !app.operations.has_active_tasks()
                    }) {
                        if let Some(operation_ui) = operation_weak.upgrade() {
                            let _ = operation_ui.hide();
                        }
                        if let Some(delete_ui) = delete_weak.upgrade() {
                            let _ = delete_ui.hide();
                        }
                        if let Some(conflict_ui) = conflict_weak.upgrade() {
                            let _ = conflict_ui.hide();
                        }
                        if let Some(exit_ui) = exit_weak.upgrade() {
                            let _ = exit_ui.hide();
                        }
                        hide_all_app_windows();
                    }
                }
                if let Some(operation_ui) = operation_weak.upgrade() {
                    refresh_operation_window(&operation_ui, &state);
                    let has_attention = state.lock().is_ok_and(|app| {
                        app.operations.iter().any(|task| {
                            matches!(
                                task.state,
                                OperationState::Failed
                                    | OperationState::PartiallyCompleted
                                    | OperationState::WaitingConflict
                            )
                        })
                    });
                    let has_active = state
                        .lock()
                        .is_ok_and(|app| app.operations.has_active_tasks());
                    let has_rows = state
                        .lock()
                        .is_ok_and(|app| app.operations.iter().next().is_some());
                    if !has_active && !has_attention && !has_rows {
                        let _ = operation_ui.hide();
                    }
                }
            });
        }
    });
}
fn start_directory_watchers(
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: SharedSessions,
) -> slint::Timer {
    use std::{cell::RefCell, rc::Rc};

    let (event_sender, event_receiver) = mpsc::channel();
    let failed_roots = Arc::new(Mutex::new(std::collections::HashSet::<PathBuf>::new()));
    let failed_for_timer = failed_roots.clone();
    let watchers = Rc::new(RefCell::new(HashMap::<
        PathBuf,
        platform::windows::directory_watch::DirectoryWatch,
    >::new()));
    let watchers_for_timer = watchers.clone();
    let state_for_timer = state.clone();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(500),
        move || {
            let roots = state_for_timer
                .lock()
                .map(|app| watched_roots(&app))
                .unwrap_or_default();
            let mut watchers = watchers_for_timer.borrow_mut();
            if let Ok(mut failed) = failed_for_timer.lock() {
                for root in failed.drain() {
                    watchers.remove(&root);
                }
            }
            watchers.retain(|root, _| roots.contains(root));
            for root in roots {
                if watchers.contains_key(&root) {
                    continue;
                }
                if let Ok(watcher) = platform::windows::directory_watch::DirectoryWatch::start(
                    &root,
                    event_sender.clone(),
                ) {
                    watchers.insert(root, watcher);
                }
            }
        },
    );
    thread::spawn(move || {
        while let Ok(first) = event_receiver.recv() {
            if let platform::windows::directory_watch::DirectoryWatchEvent::Error {
                root,
                message,
            } = &first
            {
                eprintln!("directory watch failed for {}: {message}", root.display());
                if let Ok(mut failed) = failed_roots.lock() {
                    failed.insert(root.clone());
                }
            }
            let mut roots = std::collections::HashSet::from([watch_event_root(first)]);
            thread::sleep(Duration::from_millis(120));
            while let Ok(next) = event_receiver.try_recv() {
                roots.insert(watch_event_root(next));
            }
            let roots = roots.into_iter().collect::<Vec<_>>();
            let sender_for_ui = directory_sender.clone();
            let state_for_ui = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                refresh_affected_tabs(&sender_for_ui, &state_for_ui, &roots);
            });
        }
    });
    timer
}
fn watched_roots(app: &AppState) -> std::collections::HashSet<PathBuf> {
    app.windows
        .values()
        .flat_map(|window| window.tabs.values())
        .filter_map(|tab| tab.visible_path().map(Path::to_path_buf))
        .collect()
}

fn watch_event_root(event: platform::windows::directory_watch::DirectoryWatchEvent) -> PathBuf {
    use platform::windows::directory_watch::DirectoryWatchEvent;
    match event {
        DirectoryWatchEvent::Changes { root, .. }
        | DirectoryWatchEvent::Overflow { root }
        | DirectoryWatchEvent::Error { root, .. } => root,
    }
}
fn refresh_affected_tabs(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    directories: &[PathBuf],
) {
    let targets = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.windows
            .values()
            .flat_map(|window| window.tabs.values())
            .filter_map(|tab| {
                tab.visible_path()
                    .filter(|path| directories.iter().any(|directory| directory == *path))
                    .map(|path| (tab.id, path.to_path_buf()))
            })
            .collect::<Vec<_>>()
    };
    for (tab, path) in targets {
        let pending = state
            .lock()
            .ok()
            .and_then(|mut app| app.focus_after_refresh.remove(&tab))
            .filter(|pending| pending.directory == path);
        if submit_navigation(sender, state, tab, path.clone(), NavigationKind::Refresh)
            && let Some(mut pending) = pending
            && let Ok(mut app) = state.lock()
        {
            pending.request_id = app.tab(tab).map(|session| session.latest_request);
            app.focus_after_refresh.insert(tab, pending);
        }
    }
}

fn prepare_retry(state: &SharedSessions, id: OperationId) -> Option<FileOperationRequest> {
    let mut app = state.lock().ok()?;
    if !app.operations.retry(id) {
        return None;
    }
    let started = app.operations.start_next().ok().flatten()?;
    app.operations.mark_running(started).ok()?;
    let task = app.operations.task(started)?;
    Some(FileOperationRequest {
        id: started,
        kind: task.kind,
        items: task.items.clone(),
        cancellation: task.cancellation.clone(),
    })
}
fn spawn_directory_workers(
    worker_count: usize,
) -> (
    mpsc::Sender<DirectoryRequest>,
    mpsc::Receiver<DirectoryEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<DirectoryRequest>();
    let request_receiver = Arc::new(Mutex::new(request_receiver));
    let (event_sender, event_receiver) = mpsc::channel::<DirectoryEvent>();
    for _ in 0..worker_count {
        let requests = request_receiver.clone();
        let events = event_sender.clone();
        thread::spawn(move || {
            loop {
                let request = requests
                    .lock()
                    .expect("directory request receiver mutex is not poisoned")
                    .recv();
                let Ok(request) = request else {
                    break;
                };
                run_directory_request(request, &events);
            }
        });
    }
    (request_sender, event_receiver)
}

fn run_directory_request(request: DirectoryRequest, events: &mpsc::Sender<DirectoryEvent>) {
    let result = read_directory_batches_filtered(
        &request.path,
        &request.cancel,
        request.visibility,
        |entries| {
            let _ = events.send(DirectoryEvent::Batch {
                tab_id: request.tab_id,
                request_id: request.request_id,
                entries,
            });
        },
    );
    let event = match result {
        Ok(ReadOutcome::Complete { skipped }) => DirectoryEvent::Finished {
            tab_id: request.tab_id,
            request_id: request.request_id,
            path: request.path,
            skipped,
        },
        Ok(ReadOutcome::Cancelled) => DirectoryEvent::Cancelled {
            tab_id: request.tab_id,
            request_id: request.request_id,
        },
        Err(error) => DirectoryEvent::Failed {
            tab_id: request.tab_id,
            request_id: request.request_id,
            kind: error.kind(),
            message: error.to_string(),
        },
    };
    let _ = events.send(event);
}

fn start_event_pump(
    ui: &AppWindow,
    receiver: Arc<Mutex<mpsc::Receiver<DirectoryEvent>>>,
    icon_sender: mpsc::Sender<IconRequest>,
    everything_sender: mpsc::Sender<EverythingRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        loop {
            let event = receiver
                .lock()
                .expect("directory event receiver mutex is not poisoned")
                .recv();
            let Ok(event) = event else {
                break;
            };
            let state = state.clone();
            let icon_sender = icon_sender.clone();
            let everything_sender = everything_sender.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    let routed_tab = match &event {
                        DirectoryEvent::Batch { tab_id, .. }
                        | DirectoryEvent::Finished { tab_id, .. }
                        | DirectoryEvent::Failed { tab_id, .. }
                        | DirectoryEvent::Cancelled { tab_id, .. } => Some(*tab_id),
                    };
                    let batch = match &event {
                        DirectoryEvent::Batch {
                            tab_id, request_id, ..
                        } => Some((*tab_id, *request_id)),
                        _ => None,
                    };
                    let finished = match &event {
                        DirectoryEvent::Finished {
                            tab_id, request_id, ..
                        } => Some((*tab_id, *request_id)),
                        _ => None,
                    };
                    let icon_requests = apply_event(&state, event);
                    for request in icon_requests {
                        let _ = icon_sender.send(request);
                    }
                    if let Some((tab_id, request_id)) = batch {
                        if let Some(window_id) =
                            state.lock().ok().and_then(|app| app.window_for_tab(tab_id))
                            && let Some(target_ui) = window_ui(window_id)
                        {
                            append_active_file_rows(
                                &target_ui,
                                &WindowSessions::new(state.clone(), window_id),
                                tab_id,
                                request_id,
                            );
                        } else {
                            append_active_file_rows(&ui, &state, tab_id, request_id);
                        }
                        if let Some(window_id) =
                            state.lock().ok().and_then(|app| app.window_for_tab(tab_id))
                            && let Some(target_ui) = window_ui(window_id)
                        {
                            request_grid_thumbnails(&target_ui, &state, window_id, &icon_sender);
                        }
                    } else {
                        if let Some((tab_id, request_id)) = finished {
                            submit_folder_sizes(&everything_sender, &state, tab_id, request_id);
                        }
                        if let Some(tab_id) = routed_tab {
                            refresh_tab_window(&state, tab_id);
                        } else {
                            refresh_all_windows(&state);
                        }
                        if let Some((tab_id, request_id)) = finished {
                            reveal_focused_entry(&ui, &state, tab_id, request_id);
                        }
                    }
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn reveal_focused_entry(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    const ROW_HEIGHT: f32 = 40.0;
    let index = state.lock().ok().and_then(|app| {
        (app.window_for_tab(tab_id) == Some(app.active_window)
            && app.active_window_state().active_tab == tab_id)
            .then(|| app.tab(tab_id))
            .flatten()
            .filter(|tab| tab.latest_request == request_id)
            .and_then(|tab| tab.focused.and_then(|id| tab.visible_entry_index(id)))
    });
    let Some(index) = index else {
        return;
    };
    let window_height = ui.window().size().height as f32 / ui.window().scale_factor();
    let visible_height = (window_height - ui.get_file_list_top() - 30.0).max(ROW_HEIGHT);
    let current = ui.get_file_viewport_y();
    let row_top = index as f32 * ROW_HEIGHT + current;
    let row_bottom = row_top + ROW_HEIGHT;
    let viewport = if row_top < 0.0 {
        -(index as f32 * ROW_HEIGHT)
    } else if row_bottom > visible_height {
        visible_height - (index as f32 + 1.0) * ROW_HEIGHT
    } else {
        current
    };
    let maximum = (ui.get_files().row_count() as f32 * ROW_HEIGHT - visible_height).max(0.0);
    ui.set_file_viewport_y(viewport.clamp(-maximum, 0.0));
}

fn apply_event(state: &SharedSessions, event: DirectoryEvent) -> Vec<IconRequest> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let mut icon_requests = Vec::new();
    match event {
        DirectoryEvent::Batch {
            tab_id,
            request_id,
            entries,
        } => {
            let accepted = app.tab(tab_id).is_some_and(|tab| tab.accepts(request_id));
            if accepted {
                icon_requests.extend(
                    entries
                        .iter()
                        .filter(|entry| !app.icon_cache.contains_key(&entry.path))
                        .map(|entry| IconRequest {
                            tab_id,
                            request_id,
                            target: IconTarget::Entry(entry.id),
                            path: entry.path.clone(),
                            thumbnail: false,
                            requested_px: 0,
                        }),
                );
                app.tab_mut(tab_id)
                    .expect("accepted tab exists")
                    .append_pending(entries);
            }
        }
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path,
            skipped,
        } => {
            let accepted = app.tab(tab_id).is_some_and(|tab| tab.accepts(request_id));
            let focus = accepted
                .then(|| app.focus_after_refresh.get(&tab_id).cloned())
                .flatten()
                .filter(|pending| {
                    pending.request_id == Some(request_id) && pending.directory == path
                });
            if accepted {
                let consumed_focus = focus.is_some();
                let location_path = {
                    let tab = app.tab_mut(tab_id).expect("accepted tab exists");
                    tab.sort_pending();
                    tab.commit_pending();
                    tab.commit_path(path);
                    if let Some(focus) = focus.as_ref() {
                        let ids = focus
                            .paths
                            .iter()
                            .filter_map(|target| {
                                tab.entries
                                    .iter()
                                    .find(|entry| entry.path == *target)
                                    .map(|entry| entry.id)
                            })
                            .collect::<Vec<_>>();
                        if let Some(focused) = ids.last().copied() {
                            tab.selected = ids;
                            tab.focused = Some(focused);
                            tab.selection_anchor = Some(focused);
                        }
                    }
                    tab.error = (skipped > 0).then(|| skipped.to_string());
                    tab.current_path.clone().expect("committed path exists")
                };
                if consumed_focus {
                    app.focus_after_refresh.remove(&tab_id);
                }
                icon_requests.push(IconRequest {
                    tab_id,
                    request_id,
                    target: IconTarget::Location,
                    path: location_path,
                    thumbnail: false,
                    requested_px: 0,
                });
            }
        }
        DirectoryEvent::Cancelled { tab_id, request_id } => {
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.latest_request == request_id
            {
                tab.discard_pending();
                tab.load_state = LoadState::Cancelled;
            }
        }
        DirectoryEvent::Failed {
            tab_id,
            request_id,
            kind,
            message,
        } => {
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.accepts(request_id)
            {
                tab.discard_pending();
                tab.load_state = classify_error(kind);
                tab.error = Some(message);
            }
        }
    }
    icon_requests
}

fn platform_everything_config(
    config: &crate::domain::EverythingConfig,
) -> Option<platform::windows::everything::PlatformEverythingConfig> {
    Some(platform::windows::everything::PlatformEverythingConfig {
        executable_path: config.executable_path.clone()?,
        instance_name: config.instance_name.clone(),
        allow_start: config.allow_launch,
    })
}

#[derive(Debug)]
struct GroupedSearchPage {
    items: Vec<platform::windows::everything::EverythingSearchItem>,
    total: u32,
    file_total: u32,
    response_offsets_valid: bool,
}

fn search_grouped_page(
    client: &platform::windows::everything::EverythingClient,
    scope: (Option<PathBuf>, bool),
    query: String,
    sort: platform::windows::everything::EverythingSort,
    offset: u32,
    limit: u32,
    timeout: Duration,
) -> Result<GroupedSearchPage, platform::windows::everything::EverythingError> {
    use platform::windows::everything::{EverythingItemKind, EverythingSearchRequest};
    let (scope, recursive) = scope;

    let mut files = EverythingSearchRequest::new(query.clone(), scope.clone());
    files.recursive = recursive;
    files.item_kind = EverythingItemKind::Files;
    files.sort = sort;
    files.offset = offset;
    files.max_results = limit;
    let file_page = client.search(&files, timeout)?;
    let file_offset_valid = file_page.offset == files.offset;

    let mut folders = EverythingSearchRequest::new(query, scope);
    folders.recursive = recursive;
    folders.item_kind = EverythingItemKind::Folders;
    folders.sort = sort;
    folders.offset = offset.saturating_sub(file_page.total);
    folders.max_results = limit.saturating_sub(file_page.items.len() as u32);
    let folder_page = client.search(&folders, timeout)?;
    let folder_offset_valid = folder_page.offset == folders.offset;

    let total = file_page.total.saturating_add(folder_page.total);
    let file_total = file_page.total;
    let mut items = file_page.items;
    if offset >= file_page.total || items.len() < limit as usize {
        items.extend(folder_page.items);
    }
    items.truncate(limit as usize);
    Ok(GroupedSearchPage {
        items,
        total,
        file_total,
        response_offsets_valid: file_offset_valid && folder_offset_valid,
    })
}
fn spawn_everything_worker(
    config: crate::domain::EverythingConfig,
) -> (
    mpsc::Sender<EverythingRequest>,
    mpsc::Receiver<EverythingEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<EverythingRequest>();
    let (event_sender, event_receiver) = mpsc::channel::<EverythingEvent>();
    let (folder_sender, folder_receiver) = mpsc::channel::<FolderSizeWork>();
    let folder_events = event_sender.clone();
    let folder_config = config.clone();
    thread::spawn(move || {
        let mut client = platform_everything_config(&folder_config)
            .and_then(|value| platform::windows::everything::EverythingClient::new(value).ok());
        while let Ok(work) = folder_receiver.recv() {
            match work {
                FolderSizeWork::Query {
                    tab_id,
                    request_id,
                    entry_id,
                    path,
                } => {
                    let state = client
                        .as_ref()
                        .map_or(FolderSizeState::Disconnected, |client| {
                            folder_size_state(client.folder_size(&path, Duration::from_secs(2)))
                        });
                    let _ = folder_events.send(EverythingEvent::FolderSize {
                        tab_id,
                        request_id,
                        entry_id,
                        path,
                        state,
                    });
                }
                FolderSizeWork::Configure(config) => {
                    client = platform_everything_config(&config).and_then(|value| {
                        platform::windows::everything::EverythingClient::new(value).ok()
                    });
                }
            }
        }
    });
    thread::spawn(move || {
        let mut client = platform_everything_config(&config)
            .and_then(|value| platform::windows::everything::EverythingClient::new(value).ok());
        while let Ok(request) = request_receiver.recv() {
            match request {
                EverythingRequest::Search {
                    tab_id,
                    request_id,
                    scope,
                    depth,
                    query,
                    sort,
                    offset,
                    cancel,
                } => {
                    let Some(client) = client.as_ref() else {
                        let _ = event_sender.send(EverythingEvent::SearchFailed {
                            tab_id,
                            request_id,
                            offset,
                            error: platform::windows::everything::EverythingError::NotConfigured,
                        });
                        continue;
                    };
                    let scope = match scope {
                        SearchScope::Global => None,
                        SearchScope::Directory(path) => Some(path),
                    };
                    let recursive = depth == SearchDepth::Recursive;
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        let _ = event_sender.send(EverythingEvent::SearchSkipped {
                            tab_id,
                            request_id,
                            offset,
                        });
                        continue;
                    }
                    let page_result = search_grouped_page(
                        client,
                        (scope, recursive),
                        query,
                        sort,
                        offset,
                        SEARCH_PAGE_LIMIT,
                        Duration::from_secs(3),
                    );
                    match page_result {
                        Ok(page) => {
                            let GroupedSearchPage {
                                items,
                                total,
                                file_total,
                                response_offsets_valid,
                            } = page;
                            let entries = items
                                .into_iter()
                                .enumerate()
                                .map(|(index, item)| FileEntry {
                                    id: EntryId(
                                        offset.saturating_add(index as u32).saturating_add(1),
                                    ),
                                    display_name: item.name.to_string_lossy().into_owned(),
                                    name_highlights: item
                                        .name_highlights
                                        .into_iter()
                                        .map(|segment| NameHighlightSegment {
                                            text: segment.text,
                                            highlighted: segment.highlighted,
                                        })
                                        .collect(),
                                    original_name: item.name,
                                    path: item.path,
                                    kind: if item.is_directory {
                                        crate::domain::EntryKind::Directory
                                    } else {
                                        crate::domain::EntryKind::File
                                    },
                                    open_target: None,
                                    parent_display: item
                                        .parent
                                        .as_os_str()
                                        .to_string_lossy()
                                        .into_owned(),
                                    size_bytes: item.size,
                                    folder_size: item
                                        .size
                                        .map(FolderSizeState::Value)
                                        .unwrap_or(FolderSizeState::NotIndexed),
                                    modified: item.modified,
                                })
                                .collect::<Vec<_>>();
                            if !cancel.load(std::sync::atomic::Ordering::Acquire) {
                                let response_valid = response_offsets_valid
                                    && !(offset < total && entries.is_empty());
                                let _ = event_sender.send(EverythingEvent::SearchPage {
                                    tab_id,
                                    request_id,
                                    offset,
                                    entries,
                                    total,
                                    file_total,
                                    response_valid,
                                });
                            } else {
                                let _ = event_sender.send(EverythingEvent::SearchSkipped {
                                    tab_id,
                                    request_id,
                                    offset,
                                });
                            }
                        }
                        Err(error) => {
                            let _ = event_sender.send(EverythingEvent::SearchFailed {
                                tab_id,
                                request_id,
                                offset,
                                error,
                            });
                        }
                    }
                }
                EverythingRequest::FolderSize {
                    tab_id,
                    request_id,
                    entry_id,
                    path,
                } => {
                    let _ = folder_sender.send(FolderSizeWork::Query {
                        tab_id,
                        request_id,
                        entry_id,
                        path,
                    });
                }
                EverythingRequest::Configure(config) => {
                    let _ = folder_sender.send(FolderSizeWork::Configure(config.clone()));
                    client = platform_everything_config(&config).and_then(|value| {
                        platform::windows::everything::EverythingClient::new(value).ok()
                    });
                }
                EverythingRequest::TestConnection => {
                    let result = client
                        .as_ref()
                        .ok_or(platform::windows::everything::EverythingError::NotConfigured)
                        .and_then(|client| client.status(Duration::from_secs(2)));
                    let _ = event_sender.send(EverythingEvent::Status(result));
                }
                EverythingRequest::Start => {
                    let result = client
                        .as_ref()
                        .ok_or(platform::windows::everything::EverythingError::NotConfigured)
                        .and_then(|client| client.start())
                        .and_then(|_| client.as_ref().unwrap().status(Duration::from_secs(3)));
                    let _ = event_sender.send(EverythingEvent::Status(result));
                }
            }
        }
    });
    (request_sender, event_receiver)
}

fn folder_size_state(
    result: Result<
        platform::windows::everything::EverythingFolderSize,
        platform::windows::everything::EverythingError,
    >,
) -> FolderSizeState {
    use platform::windows::everything::{EverythingError, EverythingFolderSize};

    match result {
        Ok(EverythingFolderSize::Indexed(value)) => FolderSizeState::Value(value),
        Ok(EverythingFolderSize::NotIndexed) => FolderSizeState::NotIndexed,
        Err(EverythingError::Timeout) => FolderSizeState::TimedOut,
        Err(
            EverythingError::NotConfigured
            | EverythingError::NotRunning(_)
            | EverythingError::FolderSizePipeUnavailable(_)
            | EverythingError::FolderSizeDisconnected,
        ) => FolderSizeState::Disconnected,
        Err(EverythingError::FolderSizeRejected(404)) => FolderSizeState::NotFound,
        Err(EverythingError::FolderSizeRejected(_)) => FolderSizeState::ProtocolError,
        Err(EverythingError::Protocol(message)) if protocol_reports_not_found(&message) => {
            FolderSizeState::NotFound
        }
        Err(EverythingError::Protocol(_) | EverythingError::QueryRejected) => {
            FolderSizeState::ProtocolError
        }
        Err(_) => FolderSizeState::ProtocolError,
    }
}

fn protocol_reports_not_found(message: &str) -> bool {
    message
        .split(|character: char| !character.is_ascii_digit())
        .any(|part| part == "404")
}
fn apply_folder_size_event(
    app: &mut AppState,
    tab_id: TabId,
    request_id: RequestId,
    entry_id: EntryId,
    path: &Path,
    state: FolderSizeState,
) -> bool {
    let Some(tab) = app.tab_mut(tab_id) else {
        return false;
    };
    if !tab.accepts_page(request_id, PageSource::Directory) {
        return false;
    }
    let Some(index) = tab.entry_indices.get(&entry_id).copied() else {
        return false;
    };
    let updated = {
        let Some(entry) = Arc::make_mut(&mut tab.entries).get_mut(index) else {
            return false;
        };
        entry.set_folder_size(entry_id, path, state)
    };
    if updated && tab.sort_field == SortField::Size {
        tab.resort_entries();
    }
    updated
}

#[cfg(test)]
fn apply_search_page_event(
    app: &mut AppState,
    tab_id: TabId,
    request_id: RequestId,
    offset: u32,
    entries: Vec<FileEntry>,
    total: u32,
    file_total: u32,
) -> Option<()> {
    let tab = app.tab_mut(tab_id)?;
    if !tab.accepts_page(request_id, PageSource::Search) {
        return None;
    }
    tab.finish_search_page_request(offset);
    tab.merge_search_page(offset, entries, total, file_total, SEARCH_PAGE_LIMIT);
    Some(())
}
fn start_everything_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<EverythingEvent>,
    search_sender: mpsc::Sender<EverythingRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            let sender_for_search_consistency = search_sender.clone();
            if weak.upgrade_in_event_loop(move |ui| {
            let routed_tab = match &event {
                EverythingEvent::SearchPage { tab_id, .. }
                | EverythingEvent::SearchFailed { tab_id, .. }
                | EverythingEvent::SearchSkipped { tab_id, .. } => Some(*tab_id),
                EverythingEvent::FolderSize { .. } | EverythingEvent::Status(_) => None,
            };
            let mut folder_size_update = None;
            let mut app = state.lock().expect("app state mutex is not poisoned");
            match event {
                EverythingEvent::SearchPage { tab_id, request_id, offset, entries, total, file_total, response_valid } => if let Some(tab) = app.tab_mut(tab_id) && tab.accepts_page(request_id, PageSource::Search) {
                    if !response_valid {
                        let retry = tab.retry_unexpected_search_page(offset);
                        let retry_request = retry
                            .then(|| tab.search_cancel_token())
                            .flatten()
                            .map(|cancel| EverythingRequest::Search {
                                tab_id,
                                request_id,
                                scope: tab.search_scope.clone(),
                                depth: tab.search_depth,
                                query: tab.search_query.clone(),
                                sort: everything_sort(
                                    tab.search_sort_field,
                                    tab.search_sort_direction,
                                ),
                                offset,
                                cancel,
                            });
                        eprintln!(
                            "everything search page rejected tab={} request={} offset={} count={} total={} retry={}",
                            tab_id.0,
                            request_id.0,
                            offset,
                            entries.len(),
                            total,
                            retry
                        );
                        if !retry {
                            tab.error = Some(format!(
                                "Everything returned an invalid page at offset {offset}"
                            ));
                        }
                        drop(app);
                        if let Some(request) = retry_request {
                            let _ = sender_for_search_consistency.send(request);
                        }
                        return;
                    }
                    let pending_offset = tab.finish_search_page_request(offset);
                    if tab.search_file_total.is_some_and(|known| known != file_total) {
                        let scope = tab.search_scope.clone();
                        let depth = tab.search_depth;
                        let query = tab.search_query.clone();
                        let sort = everything_sort(tab.search_sort_field, tab.search_sort_direction);
                        let (request_id, cancel) = tab.begin_search(scope.clone(), query.clone());
                        drop(app);
                        let _ = sender_for_search_consistency.send(EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope,
                            depth,
                            query,
                            sort,
                            offset: 0,
                            cancel,
                        });
                        return;
                    }
                    tab.merge_search_page(
                        offset,
                        entries,
                        total,
                        file_total,
                        SEARCH_PAGE_LIMIT,
                    );
                    let pending_request = pending_offset.and_then(|offset| {
                        tab.search_cancel_token().map(|cancel| EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope: tab.search_scope.clone(),
                            depth: tab.search_depth,
                            query: tab.search_query.clone(),
                            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
                            offset,
                            cancel,
                        })
                    });
                    let active = app.window_for_tab(tab_id) == Some(app.active_window)
                        && app.active_window_state_mut().active_tab == tab_id;
                    drop(app);
                    if let Some(request) = pending_request {
                        let _ = sender_for_search_consistency.send(request);
                    }
                    if active {
                        project_search_page(&ui, &state, tab_id, request_id, offset, total);

                    }
                    return;
                },
                EverythingEvent::SearchFailed { tab_id, request_id, offset, error } => if let Some(tab) = app.tab_mut(tab_id) && tab.accepts_page(request_id, PageSource::Search) {
                    let pending_offset = tab.finish_search_page_request(offset);
                    let pending_request = pending_offset.and_then(|offset| {
                        tab.search_cancel_token().map(|cancel| EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope: tab.search_scope.clone(),
                            depth: tab.search_depth,
                            query: tab.search_query.clone(),
                            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
                            offset,
                            cancel,
                        })
                    });
                    tab.discard_pending();
                    tab.search_state = match error { platform::windows::everything::EverythingError::NotConfigured => SearchState::NotConfigured, platform::windows::everything::EverythingError::NotRunning(_) => SearchState::Disconnected, platform::windows::everything::EverythingError::Timeout => SearchState::TimedOut, _ => SearchState::Failed };
                    tab.error = Some(error.to_string());
                    if let Some(request) = pending_request {
                        let _ = sender_for_search_consistency.send(request);
                    }
                },
                EverythingEvent::SearchSkipped { tab_id, request_id, offset } => {
                    if let Some(tab) = app.tab_mut(tab_id)
                        && tab.accepts_page(request_id, PageSource::Search)
                        && let Some(offset) = tab.finish_search_page_request(offset)
                        && let Some(cancel) = tab.search_cancel_token()
                    {
                        let _ = sender_for_search_consistency.send(EverythingRequest::Search {
                            tab_id,
                            request_id,
                            scope: tab.search_scope.clone(),
                            depth: tab.search_depth,
                            query: tab.search_query.clone(),
                            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
                            offset,
                            cancel,
                        });
                    }
                    return;
                },
                EverythingEvent::FolderSize { tab_id, request_id, entry_id, path, state: size } => {
                    let size_sort = app
                        .tab(tab_id)
                        .is_some_and(|tab| tab.sort_field == SortField::Size);
                    if apply_folder_size_event(
                        &mut app,
                        tab_id,
                        request_id,
                        entry_id,
                        &path,
                        size,
                    ) {
                        let changed = if size_sort {
                            app.tab(tab_id)
                                .map(|tab| tab.entries.iter().map(|entry| entry.id).collect())
                                .unwrap_or_default()
                        } else {
                            HashSet::from([entry_id])
                        };
                        folder_size_update = Some((tab_id, changed));
                    }
                }
                EverythingEvent::Status(result) => match result {
                    Ok(status) => { app.everything_status = format!("Everything {} · {}", status.version, if status.folder_size_indexed { "文件夹大小已索引" } else { "文件夹大小未索引" }); app.everything_folder_sizes_indexed = Some(status.folder_size_indexed); app.everything_config.verified_version = Some(status.version.to_string()); }
                    Err(error) => { app.everything_status = error.to_string(); app.everything_folder_sizes_indexed = None; }
                },
            }
            drop(app);
            if let Some((tab_id, changed)) = folder_size_update {
                if let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id))
                    && let Some(target_ui) = window_ui(window_id)
                {
                    update_file_rows(&target_ui, &state, tab_id, &changed);
                }
                return;
            }
            if let Some(tab_id) = routed_tab {
                refresh_tab_window(&state, tab_id);
            } else {
                refresh_all_windows(&state);
            }
        }).is_err() { break; }
        }
    });
}

fn empty_file_row() -> FileRow {
    FileRow {
        id: 0,
        loaded: false,
        name: "".into(),
        name_segments: ModelRc::new(VecModel::default()),
        kind: "".into(),
        parent_path: "".into(),
        size: "".into(),
        modified: "".into(),
        is_directory: false,
        selected: false,
        focused: false,
        cut: false,
        icon: Image::default(),
    }
}

fn project_search_page(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    offset: u32,
    total: u32,
) {
    let should_refresh = state.lock().ok().is_some_and(|app| {
        app.window_for_tab(tab_id) == Some(app.active_window)
            && app.active_window_state().active_tab == tab_id
            && app.tab(tab_id).is_some_and(|tab| {
                tab.latest_request == request_id && tab.page_source == PageSource::Search
            })
    });
    if !should_refresh {
        return;
    }
    if offset == 0 {
        ui.set_file_viewport_y(0.0);
        ui.set_search_scroll_y(0.0);
    }
    refresh_tab_window(state, tab_id);
    ui.set_page_state(if total == 0 { 3 } else { 4 });
}

fn everything_sort(
    field: SortField,
    direction: SortDirection,
) -> platform::windows::everything::EverythingSort {
    use platform::windows::everything::EverythingSort;
    match (field, direction) {
        (SortField::Name, SortDirection::Ascending) => EverythingSort::NameAscending,
        (SortField::Name, SortDirection::Descending) => EverythingSort::NameDescending,
        (SortField::Kind, SortDirection::Ascending) => EverythingSort::ExtensionAscending,
        (SortField::Kind, SortDirection::Descending) => EverythingSort::ExtensionDescending,
        (SortField::Size, SortDirection::Ascending) => EverythingSort::SizeAscending,
        (SortField::Size, SortDirection::Descending) => EverythingSort::SizeDescending,
        (SortField::Modified, SortDirection::Ascending) => EverythingSort::ModifiedAscending,
        (SortField::Modified, SortDirection::Descending) => EverythingSort::ModifiedDescending,
    }
}
fn submit_search(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    ui: Option<&AppWindow>,
    tab_id: TabId,
    query: String,
) {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        let scope = tab.search_scope.clone();
        let sort = everything_sort(tab.search_sort_field, tab.search_sort_direction);
        let (request_id, cancel) = tab.begin_search(scope.clone(), query.clone());
        EverythingRequest::Search {
            tab_id,
            request_id,
            scope,
            depth: tab.search_depth,
            query,
            sort,
            offset: 0,
            cancel,
        }
    };
    if let Some(ui) = ui {
        ui.set_file_viewport_y(0.0);
        ui.set_search_scroll_y(0.0);
        refresh_tab_window(state, tab_id);
    }
    let _ = sender.send(request);
}

fn submit_search_page(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    offset: u32,
) {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        let page = offset / SEARCH_PAGE_LIMIT * SEARCH_PAGE_LIMIT;
        let previous = page.saturating_sub(SEARCH_PAGE_LIMIT);
        let next = page.saturating_add(SEARCH_PAGE_LIMIT);
        let Some(offset) = tab.queue_search_pages(&[page, previous, next], SEARCH_PAGE_LIMIT)
        else {
            return;
        };
        let Some(cancel) = tab.search_cancel_token() else {
            return;
        };
        EverythingRequest::Search {
            tab_id,
            request_id: tab.latest_request,
            scope: tab.search_scope.clone(),
            depth: tab.search_depth,
            query: tab.search_query.clone(),
            sort: everything_sort(tab.search_sort_field, tab.search_sort_direction),
            offset,
            cancel,
        }
    };
    let _ = sender.send(request);
}
fn submit_folder_sizes(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    let requests = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.everything_folder_sizes_indexed == Some(false) {
            return;
        }
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        Arc::make_mut(&mut tab.entries)
            .iter_mut()
            .filter(|entry| entry.kind == crate::domain::EntryKind::Directory)
            .take(48)
            .map(|entry| {
                entry.folder_size = FolderSizeState::Querying;
                (entry.id, entry.path.clone())
            })
            .collect::<Vec<_>>()
    };
    for (entry_id, path) in requests {
        let _ = sender.send(EverythingRequest::FolderSize {
            tab_id,
            request_id,
            entry_id,
            path,
        });
    }
}
fn spawn_icon_workers(
    worker_count: usize,
    state: SharedSessions,
) -> (mpsc::Sender<IconRequest>, mpsc::Receiver<IconEvent>) {
    let (request_sender, request_receiver) = mpsc::channel::<IconRequest>();
    let request_receiver = Arc::new(Mutex::new(request_receiver));
    let (event_sender, event_receiver) = mpsc::channel::<IconEvent>();
    for _ in 0..worker_count {
        let requests = request_receiver.clone();
        let events = event_sender.clone();
        let state = state.clone();
        thread::spawn(move || {
            loop {
                let request = requests
                    .lock()
                    .expect("icon request receiver mutex is not poisoned")
                    .recv();
                let Ok(request) = request else {
                    break;
                };
                let is_current = state
                    .lock()
                    .ok()
                    .and_then(|app| {
                        app.tab(request.tab_id)
                            .map(|tab| tab.latest_request == request.request_id)
                    })
                    .unwrap_or(false);
                if !is_current {
                    continue;
                }
                let cached = state.lock().ok().and_then(|app| {
                    if request.thumbnail {
                        app.thumbnail_cache
                            .get(&(request.path.clone(), request.requested_px))
                            .or_else(|| {
                                app.large_icon_cache
                                    .get(&(request.path.clone(), request.requested_px))
                            })
                            .cloned()
                    } else {
                        app.icon_cache.get(&request.path).cloned()
                    }
                });
                let (icon, actual_thumbnail) = if request.thumbnail {
                    if let Some(cached) = cached {
                        let actual = state.lock().is_ok_and(|app| {
                            app.thumbnail_cache
                                .contains_key(&(request.path.clone(), request.requested_px))
                        });
                        (Some(cached), actual)
                    } else if let Ok(thumbnail) =
                        platform::windows_shell_icons::shell_thumbnail_rgba(
                            &request.path,
                            request.requested_px,
                            true,
                        )
                        .or_else(|_| {
                            platform::windows_shell_icons::shell_thumbnail_rgba(
                                &request.path,
                                request.requested_px,
                                false,
                            )
                        })
                    {
                        let _source = thumbnail.source;
                        (Some(thumbnail.image), true)
                    } else {
                        (
                            platform::windows_shell_icons::shell_large_icon_rgba(
                                &request.path,
                                request.requested_px,
                            )
                            .ok(),
                            false,
                        )
                    }
                } else {
                    (
                        cached.or_else(|| {
                            platform::windows_shell_icons::shell_icon_rgba(&request.path).ok()
                        }),
                        false,
                    )
                };
                if let Some(icon) = icon {
                    let _ = events.send(IconEvent {
                        tab_id: request.tab_id,
                        request_id: request.request_id,
                        target: request.target,
                        path: request.path,
                        icon,
                        actual_thumbnail,
                        requested_px: request.requested_px,
                    });
                } else if request.thumbnail
                    && let Ok(mut app) = state.lock()
                {
                    app.thumbnail_requests.remove(&(
                        request.tab_id,
                        request.request_id,
                        request.path,
                        request.requested_px,
                    ));
                }
            }
        });
    }
    (request_sender, event_receiver)
}

fn start_sidebar_icon_loader(ui: &AppWindow, state: SharedSessions) {
    let locations = state
        .lock()
        .expect("app state mutex is not poisoned")
        .sidebar
        .iter()
        .map(|location| location.path.clone())
        .collect::<Vec<_>>();
    let weak = ui.as_weak();
    thread::spawn(move || {
        for path in locations {
            let Ok(icon) = platform::windows_shell_icons::shell_icon_rgba(&path) else {
                continue;
            };
            let state = state.clone();
            if weak
                .upgrade_in_event_loop(move |_ui| {
                    state
                        .lock()
                        .expect("app state mutex is not poisoned")
                        .sidebar_icons
                        .insert(path, icon);
                    refresh_all_windows(&state);
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn start_sidebar_loader(ui: &AppWindow, state: SharedSessions) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        let locations = platform::known_locations();
        let state_for_ui = state.clone();
        let weak_for_icons = weak.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            if let Ok(mut app) = state_for_ui.lock() {
                app.sidebar = locations;
            }
            refresh_all_windows(&state_for_ui);
            if let Some(owner) = weak_for_icons.upgrade() {
                start_sidebar_icon_loader(&owner, state_for_ui.clone());
            } else {
                start_sidebar_icon_loader(&ui, state_for_ui.clone());
            }
        });
    });
}

fn start_icon_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<IconEvent>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    if let Some(update) = apply_icon_event(&state, event) {
                        if let Some(window_id) = state
                            .lock()
                            .ok()
                            .and_then(|app| app.window_for_tab(update.tab_id))
                            && let Some(target_ui) = window_ui(window_id)
                        {
                            update_icon_row(
                                &target_ui,
                                &WindowSessions::new(state.clone(), window_id),
                                update,
                            );
                        } else {
                            update_icon_row(&ui, &state, update);
                        }
                    }
                })
                .is_err()
            {
                break;
            }
        }
    });
}

#[derive(Debug, Clone, Copy)]
struct IconUpdate {
    tab_id: TabId,
    entry_id: Option<EntryId>,
}

fn apply_icon_event(state: &SharedSessions, event: IconEvent) -> Option<IconUpdate> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    if event.requested_px > 0 {
        app.thumbnail_requests.remove(&(
            event.tab_id,
            event.request_id,
            event.path.clone(),
            event.requested_px,
        ));
    }
    if !icon_event_is_current(&app, &event) {
        return None;
    }
    let entry_id = match event.target {
        IconTarget::Entry(entry_id) => {
            app.icons.insert(
                (event.tab_id, event.request_id, entry_id),
                event.icon.clone(),
            );
            Some(entry_id)
        }
        IconTarget::Location => None,
    };
    if event.actual_thumbnail {
        app.thumbnail_cache
            .insert((event.path.clone(), event.requested_px), event.icon.clone());
    } else if event.requested_px > 0 {
        app.large_icon_cache
            .insert((event.path.clone(), event.requested_px), event.icon.clone());
    } else {
        app.icon_cache.insert(event.path, event.icon);
    }
    Some(IconUpdate {
        tab_id: event.tab_id,
        entry_id,
    })
}

fn icon_event_is_current(app: &AppState, event: &IconEvent) -> bool {
    app.tab(event.tab_id).is_some_and(|tab| {
        tab.latest_request == event.request_id
            && match event.target {
                IconTarget::Entry(entry_id) => tab
                    .visible_entry(entry_id)
                    .is_some_and(|entry| entry.path == event.path),
                IconTarget::Location => tab.visible_path() == Some(event.path.as_path()),
            }
    })
}

fn classify_error(kind: io::ErrorKind) -> LoadState {
    match kind {
        io::ErrorKind::NotFound => LoadState::NotFound,
        io::ErrorKind::PermissionDenied => LoadState::PermissionDenied,
        io::ErrorKind::NotConnected
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::TimedOut => LoadState::Disconnected,
        _ => LoadState::Failed,
    }
}

fn append_active_file_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    use slint::Model;
    let app = state.lock().expect("app state mutex is not poisoned");
    if app.active_window_state().active_tab != tab_id {
        return;
    }
    let tab = app.active();
    if tab.latest_request != request_id || tab.load_state != LoadState::Partial {
        return;
    }
    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        drop(app);
        refresh_tab_window(state, tab_id);
        return;
    };
    let start = model.row_count();
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != request_id.0 as i32
        || start > tab.pending_entries.len()
    {
        drop(app);
        refresh_tab_window(state, tab_id);
        return;
    }
    let texts = Texts::new(app.language);
    model.extend(
        tab.pending_entries[start..]
            .iter()
            .map(|entry| file_row(entry, tab, texts, &app)),
    );
    ui.set_status_text(status_text(tab, texts).into());
}

fn update_icon_row(ui: &AppWindow, state: &SharedSessions, update: IconUpdate) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window_id) = app.window_for_tab(update.tab_id) else {
        return;
    };
    let Some(window) = app.window(window_id) else {
        return;
    };
    if window.active_tab != update.tab_id {
        return;
    }
    let Some(tab) = window.tabs.get(&update.tab_id) else {
        return;
    };
    let Some(entry_id) = update.entry_id else {
        let path = tab.visible_path();
        ui.set_current_location_icon(
            path.and_then(|path| app.icon_cache.get(path))
                .map(shell_icon_image)
                .unwrap_or_default(),
        );
        return;
    };
    drop(app);
    update_file_rows(ui, state, update.tab_id, &HashSet::from([entry_id]));
}

fn selection_projection_ids(tab: &TabSession) -> HashSet<EntryId> {
    tab.selected.iter().copied().chain(tab.focused).collect()
}

fn mutate_active_selection(
    state: &WindowSessions,
    mutate: impl FnOnce(&mut TabSession),
) -> Option<(TabId, HashSet<EntryId>)> {
    let mut app = state.lock().ok()?;
    let tab_id = app.active_window_state().active_tab;
    let tab = app.active_window_state_mut().tabs.get_mut(&tab_id)?;
    let before = selection_projection_ids(tab);
    mutate(tab);
    let mut changed = before;
    changed.extend(selection_projection_ids(tab));
    Some((tab_id, changed))
}
fn update_file_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    changed: &HashSet<EntryId>,
) {
    use slint::Model;

    if changed.is_empty() {
        return;
    }
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window_id) = app.window_for_tab(tab_id) else {
        return;
    };
    let Some(window) = app.window(window_id) else {
        return;
    };
    if window.active_tab != tab_id {
        return;
    }
    let Some(tab) = window.tabs.get(&tab_id) else {
        return;
    };
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != tab.latest_request.0 as i32
    {
        return;
    }

    let texts = Texts::new(app.language);
    let rows = changed
        .iter()
        .filter_map(|id| tab.visible_entry(*id))
        .map(|entry| (entry.id, file_row(entry, tab, texts, &app)))
        .collect::<HashMap<_, _>>();
    if rows.is_empty() {
        return;
    }

    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        return;
    };
    let window_start = if tab.page_source == PageSource::Search {
        let total = tab.search_total.unwrap_or(tab.entries.len() as u32);
        let view_mode = tab
            .visible_path()
            .and_then(|path| app.directory_view_modes.get(path))
            .copied()
            .unwrap_or(ViewMode::Details);

        Some(
            search_window_for_scroll(
                ui.get_search_scroll_y(),
                total,
                view_mode,
                ui.get_grid_column_count().max(1) as usize,
            )
            .start,
        )
    } else {
        None
    };
    for (id, row) in &rows {
        let index = window_start
            .and_then(|start| search_window_local_index(*id, start))
            .or_else(|| tab.visible_entry_index(*id));
        if let Some(index) = index
            && index < model.row_count()
        {
            model.set_row_data(index, row.clone());
        }
    }

    let grid_model = ui.get_grid_rows();
    if let Some(grid_model) = grid_model.as_any().downcast_ref::<VecModel<GridRow>>() {
        let columns = ui.get_grid_column_count().max(1) as usize;
        for (id, updated) in &rows {
            let index = window_start
                .and_then(|start| id.0.checked_sub(start.saturating_add(1)))
                .map(|index| index as usize)
                .or_else(|| tab.visible_entry_index(*id));
            let Some(index) = index else {
                continue;
            };
            let row_index = index / columns;
            let entry_index = index % columns;
            let Some(grid_row) = grid_model.row_data(row_index) else {
                continue;
            };
            let Some(entries) = grid_row
                .entries
                .as_any()
                .downcast_ref::<VecModel<FileRow>>()
            else {
                continue;
            };
            if entry_index < entries.row_count() {
                entries.set_row_data(entry_index, updated.clone());
            }
        }
    }
}

fn update_selection_status(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let tab = app.active();
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_context_menu_has_entry(!tab.selected.is_empty() || tab.focused.is_some());
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
}

fn update_selection_summary(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let tab = app.active();

    ui.set_normal_name_width(app.column_widths[0] as f32);
    ui.set_normal_kind_width(app.column_widths[1] as f32);
    ui.set_normal_size_width(app.column_widths[2] as f32);
    ui.set_normal_modified_width(app.column_widths[3] as f32);
    ui.set_search_name_width(app.search_column_widths[0] as f32);
    ui.set_search_parent_width(app.search_column_widths[1] as f32);
    ui.set_search_size_width(app.search_column_widths[2] as f32);
    ui.set_search_modified_width(app.search_column_widths[3] as f32);
    ui.set_columns(ModelRc::new(VecModel::from(
        (if tab.page_source == PageSource::Search {
            app.search_column_order
        } else {
            app.column_order
        })
        .iter()
        .map(|kind| ColumnRow {
            kind: i32::from(*kind),

            min_width: 64.0,
            content_left: if *kind == 0 { 10.0 } else { 8.0 },
            content_right: 8.0,
            icon_slot: if *kind == 0 { 25.0 } else { 0.0 },
        })
        .collect::<Vec<_>>(),
    )));
    let operation_rows = operation_rows(&app);
    ui.set_operations(ModelRc::new(VecModel::from(operation_rows)));
    ui.set_operation_error(
        app.operation_errors
            .last()
            .cloned()
            .unwrap_or_default()
            .into(),
    );
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_context_menu_has_entry(!tab.selected.is_empty() || tab.focused.is_some());
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
}

fn operation_rows(app: &AppState) -> Vec<OperationRow> {
    app.operations
        .iter()
        .map(|task| {
            let completed = task
                .items
                .iter()
                .filter(|item| {
                    matches!(
                        item.state,
                        ItemState::Succeeded
                            | ItemState::Skipped
                            | ItemState::Failed
                            | ItemState::Cancelled
                    )
                })
                .count();
            let title = match (app.language, task.kind) {
                (Language::Chinese, FileOperationKind::CreateFolder) => "正在新建文件夹",
                (Language::Chinese, FileOperationKind::Rename) => "正在重命名",
                (Language::Chinese, FileOperationKind::Copy) => "正在复制",
                (Language::Chinese, FileOperationKind::Move) => "正在移动",
                (Language::Chinese, FileOperationKind::RecycleDelete) => "正在移到回收站",
                (Language::Chinese, FileOperationKind::PermanentDelete) => "正在删除",
                (Language::Chinese, FileOperationKind::FastRemove) => "正在删除",
                (Language::English, FileOperationKind::CreateFolder) => "Creating folder",
                (Language::English, FileOperationKind::Rename) => "Renaming",
                (Language::English, FileOperationKind::Copy) => "Copying",
                (Language::English, FileOperationKind::Move) => "Moving",
                (Language::English, FileOperationKind::RecycleDelete) => "Recycling",
                (Language::English, FileOperationKind::PermanentDelete) => "Deleting",
                (Language::English, FileOperationKind::FastRemove) => "Deleting",
            };
            let progress = task
                .progress
                .total_bytes
                .filter(|total| *total > 0)
                .map(|total| task.progress.processed_bytes as f32 / total as f32)
                .unwrap_or_else(|| {
                    if task.items.is_empty() {
                        0.0
                    } else {
                        completed as f32 / task.items.len() as f32
                    }
                });
            let (transferred, speed_text, eta_text) = task
                .progress
                .total_bytes
                .filter(|total| *total > 0)
                .map(|total| {
                    let elapsed = task
                        .cancellation
                        .active_elapsed(task.started_at)
                        .as_secs_f64()
                        .max(0.001);
                    let speed = task.progress.processed_bytes as f64 / elapsed;
                    let remaining = total.saturating_sub(task.progress.processed_bytes);
                    let eta = if speed > 0.0 {
                        remaining as f64 / speed
                    } else {
                        0.0
                    };
                    (
                        format!(
                            "{:.1} / {:.1} MB",
                            task.progress.processed_bytes as f64 / 1_048_576.0,
                            total as f64 / 1_048_576.0
                        ),
                        if task.state == OperationState::Paused {
                            match app.language {
                                Language::Chinese => "已暂停".to_owned(),
                                Language::English => "Paused".to_owned(),
                            }
                        } else {
                            format!("{:.1} MB/s", speed / 1_048_576.0)
                        },
                        match (app.language, task.state == OperationState::Paused) {
                            (_, true) => "—".to_owned(),
                            (language, false) => format_remaining_time(language, eta),
                        },
                    )
                })
                .unwrap_or_else(|| {
                    (
                        format!("{completed}/{}", task.items.len()),
                        String::new(),
                        String::new(),
                    )
                });
            let source = task
                .items
                .first()
                .and_then(|item| item.source.as_deref())
                .map(display_path)
                .unwrap_or_default();
            let destination = task
                .items
                .first()
                .and_then(|item| item.destination.as_deref())
                .and_then(Path::parent)
                .map(display_path)
                .unwrap_or_default();
            OperationRow {
                id: task.id.0 as i32,
                title: title.into(),
                percent: format!("{:.0}%", (progress * 100.0).clamp(0.0, 100.0)).into(),
                file_progress: task
                    .progress
                    .total_files
                    .map(|total| format!("{} / {total}", task.progress.completed_files.min(total)))
                    .unwrap_or_else(|| match app.language {
                        Language::Chinese => "正在准备…".to_owned(),
                        Language::English => "Preparing…".to_owned(),
                    })
                    .into(),
                transferred: transferred.into(),
                speed: speed_text.into(),
                eta: eta_text.into(),
                source: source.into(),
                destination: destination.into(),
                paused: task.state == OperationState::Paused,
                current_item: task
                    .progress
                    .current_item
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .into(),
                progress,
                state: operation_state_index(task.state),
                can_cancel: matches!(
                    task.state,
                    OperationState::Queued
                        | OperationState::Preflight
                        | OperationState::Running
                        | OperationState::Paused
                        | OperationState::WaitingConflict
                ),
                can_pause: matches!(task.state, OperationState::Running | OperationState::Paused),
                can_retry: matches!(
                    task.state,
                    OperationState::Failed | OperationState::PartiallyCompleted
                ) && task.items.iter().any(|item| {
                    matches!(
                        item.state,
                        ItemState::Failed | ItemState::Cancelled | ItemState::Pending
                    )
                }),
            }
        })
        .collect()
}

fn format_remaining_time(language: Language, seconds: f64) -> String {
    let seconds = seconds.max(0.0).round() as u64;
    if seconds < 60 {
        return match language {
            Language::Chinese => format!("预计剩余 {seconds} 秒"),
            Language::English => format!("About {seconds} seconds remaining"),
        };
    }
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    match language {
        Language::Chinese => format!("预计剩余 {minutes} 分 {seconds} 秒"),
        Language::English => format!("About {minutes}min {seconds}s remaining"),
    }
}
fn refresh_window_ui(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    let previous = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if !app.windows.contains_key(&window_id) {
            return;
        }
        let previous = app.active_window;
        app.active_window = window_id;
        previous
    };
    refresh_ui_inner(ui, state);
    if let Ok(mut app) = state.lock()
        && app.windows.contains_key(&previous)
    {
        app.active_window = previous;
    }
}

fn refresh_ui(ui: &AppWindow, state: &WindowSessions) {
    refresh_window_ui(ui, &state.shared, state.window_id);
}

fn refresh_ui_inner(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let texts = Texts::new(app.language);
    let tab = app.active();
    let active_is_settings = tab.kind == TabKind::Settings;
    ui.set_active_is_settings(active_is_settings);
    let view_mode = tab
        .visible_path()
        .and_then(|path| app.directory_view_modes.get(path))
        .copied()
        .unwrap_or(ViewMode::Details);
    ui.set_view_mode(match view_mode {
        ViewMode::Details => 0,
        ViewMode::List => 1,
        ViewMode::Grid => 2,
    });
    let projected_tab_id = tab.id.0 as i32;
    let projected_request_id = tab.latest_request.0 as i32;
    if ui.get_projected_file_tab_id() != projected_tab_id
        || ui.get_projected_file_request_id() != projected_request_id
    {
        if ui.get_rectangle_selection_pointer_active() {
            ui.invoke_cancel_rectangle_selection();
        }
        ui.set_file_viewport_y(0.0);
        ui.set_search_scroll_y(0.0);
        ui.set_projected_file_tab_id(projected_tab_id);
        ui.set_projected_file_request_id(projected_request_id);
    }
    let display_entries = if matches!(tab.load_state, LoadState::Partial) {
        &tab.pending_entries
    } else if tab.has_failed_location() {
        &[] as &[FileEntry]
    } else {
        &tab.entries
    };
    let directory_rows = display_entries
        .iter()
        .map(|entry| file_row(entry, tab, texts, &app))
        .collect::<Vec<_>>();
    let grid_columns = (((ui.window().size().width as f32 / ui.window().scale_factor()) - 260.0)
        / 148.0)
        .floor()
        .max(1.0) as usize;
    let total = tab.search_total.unwrap_or(tab.entries.len() as u32);
    let search_index =
        search_result_index_at_scroll(ui.get_search_scroll_y(), total, view_mode, grid_columns);
    let search_window = search_window_for_index(search_index, total, grid_columns);
    if tab.page_source == PageSource::Search {
        ui.set_file_viewport_y(search_window_viewport_y(
            search_index,
            search_window,
            view_mode,
            grid_columns,
        ));
    }
    let file_rows = if tab.page_source == PageSource::Search {
        search_window_rows(tab, &app, search_window)
    } else {
        directory_rows
    };
    let grid_rows = file_rows
        .chunks(grid_columns)
        .map(|entries| GridRow {
            entries: ModelRc::new(VecModel::from(entries.to_vec())),
        })
        .collect::<Vec<_>>();
    ui.set_files(ModelRc::new(VecModel::from(file_rows)));
    ui.set_grid_column_count(grid_columns as i32);
    ui.set_grid_rows(ModelRc::new(VecModel::from(grid_rows)));
    ui.set_search_total_items(total.min(i32::MAX as u32) as i32);
    ui.set_window_width(ui.window().size().width as f32 / ui.window().scale_factor());
    let visible_path = tab.visible_path().map(display_path).unwrap_or_default();
    let address_input = if tab.address_editing {
        tab.address_input.clone()
    } else {
        visible_path.clone()
    };
    ui.set_current_path(visible_path.into());
    let current_location_path = tab.visible_path();
    ui.set_current_location_icon(
        current_location_path
            .and_then(|path| app.icon_cache.get(path))
            .map(shell_icon_image)
            .unwrap_or_default(),
    );
    ui.set_current_location_is_drive(current_location_path.is_some_and(is_drive_root));
    ui.set_address_input(address_input.into());
    ui.set_address_editing(tab.address_editing);
    ui.set_search_mode(tab.address_mode == AddressMode::Smart);
    ui.set_search_results_mode(tab.page_source == PageSource::Search);
    ui.set_search_depth_enabled(matches!(tab.search_scope, SearchScope::Directory(_)));
    ui.set_search_recursive(tab.search_depth == SearchDepth::Recursive);

    ui.set_status_text(status_text(tab, texts).into());
    let (error_page_title, error_page_description) = error_page_text(tab.load_state, texts);
    ui.set_error_page_title(error_page_title.into());
    ui.set_error_page_description(error_page_description.into());
    ui.set_active_tab_index(
        app.active_window_state()
            .tab_order
            .iter()
            .position(|id| *id == app.active_window_state().active_tab)
            .unwrap_or(0) as i32,
    );
    ui.set_tabs(ModelRc::new(VecModel::from(
        app.active_window_state()
            .tab_order
            .iter()
            .filter_map(|id| app.active_window_state().tabs.get(id))
            .map(|tab| TabRow {
                id: tab.id.0 as i32,
                title: if tab.kind == TabKind::Settings {
                    texts.settings().to_owned()
                } else {
                    tab.visible_path()
                        .and_then(Path::file_name)
                        .map(|name| name.to_string_lossy().into_owned())
                        .filter(|name| !name.is_empty())
                        .unwrap_or_else(|| {
                            display_path(tab.visible_path().unwrap_or(Path::new("C:\\")))
                        })
                }
                .into(),
                active: tab.id == app.active_window_state().active_tab,
                loading: matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
                icon: tab
                    .visible_path()
                    .and_then(|path| app.icon_cache.get(path))
                    .map(shell_icon_image)
                    .unwrap_or_default(),
                is_drive: tab.visible_path().is_some_and(is_drive_root),
                is_settings: tab.kind == TabKind::Settings,
            })
            .collect::<Vec<_>>(),
    )));
    let breadcrumb_paths = tab.breadcrumb_paths();
    ui.set_breadcrumbs(ModelRc::new(VecModel::from(
        breadcrumb_paths
            .iter()
            .enumerate()
            .map(|(index, (label, _))| BreadcrumbRow {
                index: index as i32,
                label: label.clone().into(),
                current: index + 1 == breadcrumb_paths.len(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_back_history(ModelRc::new(VecModel::from(
        tab.back_history
            .iter()
            .rev()
            .enumerate()
            .map(|(index, path)| HistoryRow {
                index: index as i32,
                label: display_path(path).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_forward_history(ModelRc::new(VecModel::from(
        tab.forward_history
            .iter()
            .rev()
            .enumerate()
            .map(|(index, path)| HistoryRow {
                index: index as i32,
                label: display_path(path).into(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_sidebar_items(ModelRc::new(VecModel::from(
        app.sidebar
            .iter()
            .enumerate()
            .map(|(index, location)| SidebarRow {
                index: index as i32,
                label: match (app.language, location.kind) {
                    (Language::Chinese, KnownLocationKind::Home) => "主页",
                    (Language::English, KnownLocationKind::Home) => "Home",
                    (_, KnownLocationKind::Pinned) => location.label.as_str(),
                    (_, KnownLocationKind::Drive) => location.label.as_str(),
                }
                .into(),
                icon_kind: match location.kind {
                    KnownLocationKind::Home => 0,
                    KnownLocationKind::Drive => 7,
                    KnownLocationKind::Pinned => 3,
                },
                is_drive: location.kind == KnownLocationKind::Drive,
                icon: app
                    .sidebar_icons
                    .get(&location.path)
                    .map(shell_icon_image)
                    .unwrap_or_default(),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_normal_name_width(app.column_widths[0] as f32);
    ui.set_normal_kind_width(app.column_widths[1] as f32);
    ui.set_normal_size_width(app.column_widths[2] as f32);
    ui.set_normal_modified_width(app.column_widths[3] as f32);
    ui.set_search_name_width(app.search_column_widths[0] as f32);
    ui.set_search_parent_width(app.search_column_widths[1] as f32);
    ui.set_search_size_width(app.search_column_widths[2] as f32);
    ui.set_search_modified_width(app.search_column_widths[3] as f32);
    ui.set_columns(ModelRc::new(VecModel::from(
        (if tab.page_source == PageSource::Search {
            app.search_column_order
        } else {
            app.column_order
        })
        .iter()
        .map(|kind| ColumnRow {
            kind: i32::from(*kind),

            min_width: 64.0,
            content_left: if *kind == 0 { 10.0 } else { 8.0 },
            content_right: 8.0,
            icon_slot: if *kind == 0 { 25.0 } else { 0.0 },
        })
        .collect::<Vec<_>>(),
    )));
    let (sort_field, sort_direction) = if tab.page_source == PageSource::Search {
        (tab.search_sort_field, tab.search_sort_direction)
    } else {
        (tab.sort_field, tab.sort_direction)
    };
    ui.set_sort_field(match sort_field {
        SortField::Name => 0,
        SortField::Kind => 1,
        SortField::Size => 2,
        SortField::Modified => 3,
    });
    ui.set_sort_descending(sort_direction == crate::domain::SortDirection::Descending);
    let page_projection = agent_debug::page_projection(tab.load_state, tab.entries.is_empty());
    ui.set_page_state(if tab.page_source == PageSource::Search {
        match tab.search_state {
            SearchState::Searching if tab.entries.is_empty() => 1,
            SearchState::NoResults => 3,
            SearchState::NotConfigured
            | SearchState::Disconnected
            | SearchState::NotIndexed
            | SearchState::SyntaxError
            | SearchState::TimedOut
            | SearchState::Failed => 9,
            _ => 4,
        }
    } else {
        page_projection.index
    });
    ui.set_show_request_access(
        page_projection
            .visible_page_operations
            .contains(&agent_debug::PageOperation::RequestWindowsAccess),
    );
    ui.set_can_navigate_back(
        !active_is_settings && (tab.has_failed_location() || !tab.back_history.is_empty()),
    );
    ui.set_can_navigate_forward(!active_is_settings && !tab.forward_history.is_empty());
    ui.set_can_navigate_up(
        !active_is_settings && tab.visible_path().and_then(Path::parent).is_some(),
    );
    ui.set_can_refresh(
        !active_is_settings && !matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
    );
    ui.set_can_close_tab(app.active_window_state().tab_order.len() > 1);
    ui.set_can_restore_tab(!app.active_window_state().closed_tabs.is_empty());
    ui.set_language_mode(match app.language {
        Language::Chinese => 0,
        Language::English => 1,
    });
    ui.set_show_hidden_files(app.file_visibility.show_hidden);
    ui.set_show_system_files(app.file_visibility.show_system);
    ui.set_everything_path(
        app.everything_config
            .executable_path
            .as_deref()
            .map(display_path)
            .unwrap_or_default()
            .into(),
    );
    ui.set_everything_instance(app.everything_config.instance_name.clone().into());
    ui.set_everything_status(app.everything_status.clone().into());
    ui.set_theme_mode(match app.theme_mode {
        session_store::ThemeMode::System => 0,
        session_store::ThemeMode::Light => 1,
        session_store::ThemeMode::Dark => 2,
    });
    ui.set_dark_theme(app.dark_theme());
    apply_ui_texts(ui, app.language);
}

fn operation_state_index(state: OperationState) -> i32 {
    match state {
        OperationState::Queued => 0,
        OperationState::Preflight => 1,
        OperationState::Running => 2,
        OperationState::Paused => 9,
        OperationState::WaitingConflict => 3,
        OperationState::Cancelling => 4,
        OperationState::Completed => 5,
        OperationState::Cancelled => 6,
        OperationState::PartiallyCompleted => 7,
        OperationState::Failed => 8,
    }
}
fn file_row(entry: &FileEntry, tab: &TabSession, texts: Texts, app: &AppState) -> FileRow {
    debug_assert_eq!(
        entry.path.file_name(),
        Some(entry.original_name.as_os_str()),
        "entry identity must retain its original file name"
    );
    let name_segments = if entry.name_highlights.is_empty() {
        vec![HighlightSegment {
            text: entry.display_name.clone().into(),
            highlighted: false,
        }]
    } else {
        entry
            .name_highlights
            .iter()
            .map(|segment| HighlightSegment {
                text: segment.text.clone().into(),
                highlighted: segment.highlighted,
            })
            .collect::<Vec<_>>()
    };
    let grid_image = tab
        .visible_path()
        .is_some_and(|path| app.directory_view_modes.get(path) == Some(&ViewMode::Grid))
        .then(|| {
            let thumbnail = app
                .thumbnail_cache
                .iter()
                .filter(|((path, _), _)| path == &entry.path)
                .max_by_key(|((_, requested_px), image)| {
                    (*requested_px, image.width.min(image.height))
                })
                .map(|(_, image)| image);
            thumbnail.or_else(|| {
                app.large_icon_cache
                    .iter()
                    .filter(|((path, _), _)| path == &entry.path)
                    .max_by_key(|((_, requested_px), image)| {
                        (*requested_px, image.width.min(image.height))
                    })
                    .map(|(_, image)| image)
            })
        })
        .flatten();
    let image = grid_image.or_else(|| {
        app.icons
            .get(&(tab.id, tab.latest_request, entry.id))
            .or_else(|| app.icon_cache.get(&entry.path))
    });
    FileRow {
        id: entry.id.0 as i32,
        loaded: true,
        name: entry.display_name.clone().into(),
        name_segments: ModelRc::new(VecModel::from(name_segments)),
        kind: if entry.kind == crate::domain::EntryKind::File {
            entry
                .path
                .extension()
                .map(|extension| extension.to_string_lossy().to_uppercase())
                .filter(|extension| !extension.is_empty())
                .map(|extension| format!("{extension} {}", texts.kind(entry.kind)))
                .unwrap_or_else(|| texts.kind(entry.kind).to_owned())
                .into()
        } else {
            texts.kind(entry.kind).into()
        },
        parent_path: entry.parent_display.clone().into(),
        size: if entry.kind == crate::domain::EntryKind::Directory {
            texts.folder_size(entry.folder_size).into()
        } else {
            texts.size(entry.size_bytes).into()
        },
        modified: texts.modified(entry.modified).into(),
        is_directory: entry.kind == crate::domain::EntryKind::Directory,
        selected: tab.selected.contains(&entry.id),
        focused: tab.focused == Some(entry.id),
        cut: app.cut_paths.contains(&entry.path),
        icon: image.map(shell_icon_image).unwrap_or_default(),
    }
}

fn shell_icon_image(icon: &platform::windows_shell_icons::ShellIconRgba) -> Image {
    let buffer =
        SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(&icon.pixels, icon.width, icon.height);
    Image::from_rgba8(buffer)
}

fn status_text(tab: &TabSession, texts: Texts) -> String {
    if tab.page_source == PageSource::Search {
        return match tab.search_total {
            Some(total) if total as usize > tab.entries.len() => match texts.language {
                Language::Chinese => format!("已加载 {} 项，共 {total} 项", tab.entries.len()),
                Language::English => format!("Loaded {} of {total} items", tab.entries.len()),
            },
            Some(total) => texts.items(total as usize, 0),
            None => texts.search_state(tab.search_state).to_owned(),
        };
    }
    if !tab.selected.is_empty() {
        return match texts.language {
            Language::Chinese => format!("已选择 {} 项", tab.selected.len()),
            Language::English => format!("{} selected", tab.selected.len()),
        };
    }
    match tab.load_state {
        LoadState::Complete => texts.items(
            tab.entries.len(),
            tab.error
                .as_deref()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        ),
        state => texts.state(state).to_owned(),
    }
}

fn error_page_text(state: LoadState, texts: Texts) -> (&'static str, &'static str) {
    match (texts.language, state) {
        (Language::Chinese, LoadState::PermissionDenied) => (
            "无权访问此文件夹",
            "你当前没有访问此文件夹的权限。可以使用 Windows 请求访问权限。",
        ),
        (Language::English, LoadState::PermissionDenied) => (
            "Access denied",
            "You don't currently have permission to access this folder. Use Windows to request access.",
        ),
        (Language::Chinese, LoadState::NotFound) => {
            ("找不到该位置", "该位置可能已被移动、重命名或删除。")
        }
        (Language::English, LoadState::NotFound) => (
            "Location not found",
            "This location may have been moved, renamed, or deleted.",
        ),
        (Language::Chinese, LoadState::Disconnected) => {
            ("位置已断开", "请检查磁盘或网络连接，然后重试。")
        }
        (Language::English, LoadState::Disconnected) => (
            "Location disconnected",
            "Check the drive or network connection, then try again.",
        ),
        (Language::Chinese, LoadState::Cancelled) => ("加载已取消", "你可以重试或返回上一个位置。"),
        (Language::English, LoadState::Cancelled) => (
            "Loading cancelled",
            "Try again or return to the previous location.",
        ),
        (Language::Chinese, _) => ("无法打开该位置", "读取此位置时发生错误。你可以重试或返回。"),
        (Language::English, _) => (
            "Unable to open location",
            "An error occurred while reading this location. Try again or go back.",
        ),
    }
}
fn apply_ui_texts(ui: &AppWindow, language: Language) {
    let (
        go,
        back,
        forward,
        up,
        refresh,
        home,
        desktop,
        downloads,
        documents,
        pictures,
        music,
        videos,
        quick_access,
        drives,
        name,
        kind,
        modified,
        size,
        request_access,
        menu,
        settings,
        theme,
        theme_system,
        theme_light,
        theme_dark,
        language_label,
        chinese,
        english,
        show_hidden_files,
        show_system_files,
        settings_general,
        settings_appearance,
        settings_developer,
        ui_showcase,
        ui_showcase_detail,
        show_delete_confirmation,
        show_conflict_confirmation,
        show_exit_confirmation,
        show_operation_window,
        window_trace_title,
        window_trace_detail,
        window_trace_start,
        window_trace_active,
        window_trace_path,
        loading,
        empty_folder,
        no_search_results,
        new_tab,
        close_tab,
        all_tabs,
        minimize,
        restore,
        maximize,
        close,
        address,
        cancel_edit,
        search_recursive,
        search_current,
        search_global_disabled,
        drop_copy,
        drop_move,
        drop_link,
        drop_cancel,
    ) = match language {
        Language::Chinese => (
            "前往",
            "后退",
            "前进",
            "向上",
            "刷新",
            "主页",
            "桌面",
            "下载",
            "文档",
            "图片",
            "音乐",
            "视频",
            "快速访问",
            "磁盘",
            "名称",
            "类型",
            "修改时间",
            "大小",
            "使用 Windows 请求访问权限",
            "菜单",
            "设置",
            "主题",
            "跟随系统",
            "浅色",
            "深色",
            "语言",
            "中文",
            "English",
            "显示隐藏文件",
            "显示系统文件",
            "常规",
            "外观",
            "开发工具",
            "UI 陈列室",
            "直接打开难以触发的窗口和状态；演示操作不会修改文件或任务。",
            "永久删除确认",
            "文件冲突",
            "退出任务确认",
            "文件进度窗口",
            "窗口交互诊断",
            "只记录窗口移动与缩放消息，不改变窗口行为。问题复现后关闭应用并把日志交给开发者。",
            "开始记录",
            "正在记录",
            "日志位置",
            "正在加载…",
            "此文件夹为空",
            "没有搜索结果",
            "新建标签",
            "关闭标签",
            "全部标签",
            "最小化",
            "还原",
            "最大化",
            "关闭",
            "路径",
            "取消编辑",
            "搜索当前文件夹及子文件夹",
            "仅搜索当前文件夹",
            "全局搜索不支持切换范围",
            "复制到此处",
            "移动到此处",
            "在此处创建快捷方式",
            "取消",
        ),
        Language::English => (
            "Go",
            "Back",
            "Forward",
            "Up",
            "Refresh",
            "Home",
            "Desktop",
            "Downloads",
            "Documents",
            "Pictures",
            "Music",
            "Videos",
            "Quick access",
            "Drives",
            "Name",
            "Type",
            "Modified",
            "Size",
            "Request access with Windows",
            "Menu",
            "Settings",
            "Theme",
            "Use system setting",
            "Light",
            "Dark",
            "Language",
            "中文",
            "English",
            "Show hidden files",
            "Show system files",
            "General",
            "Appearance",
            "Developer tools",
            "UI showcase",
            "Open hard-to-reach windows and states directly. Demo actions do not change files or tasks.",
            "Delete confirmation",
            "File conflict",
            "Exit confirmation",
            "File progress window",
            "Window interaction diagnostics",
            "Records move and resize messages without changing window behavior. After reproducing the issue, close the app and send the log to the developer.",
            "Start recording",
            "Recording",
            "Log location",
            "Loading…",
            "This folder is empty",
            "No search results",
            "New tab",
            "Close tab",
            "All tabs",
            "Minimize",
            "Restore",
            "Maximize",
            "Close",
            "Path",
            "Cancel editing",
            "Search current folder and subfolders",
            "Search current folder only",
            "Search depth is unavailable for global search",
            "Copy here",
            "Move here",
            "Create shortcut here",
            "Cancel",
        ),
    };
    ui.set_text_go(go.into());
    ui.set_text_back(back.into());
    ui.set_text_forward(forward.into());
    ui.set_text_up(up.into());
    ui.set_text_refresh(refresh.into());
    ui.set_text_home(home.into());
    ui.set_text_desktop(desktop.into());
    ui.set_text_downloads(downloads.into());
    ui.set_text_documents(documents.into());
    ui.set_text_pictures(pictures.into());
    ui.set_text_music(music.into());
    ui.set_text_videos(videos.into());
    ui.set_text_quick_access(quick_access.into());
    ui.set_text_drives(drives.into());
    ui.set_text_name(name.into());
    ui.set_text_type(kind.into());
    ui.set_text_modified(modified.into());
    ui.set_text_size(size.into());
    ui.set_text_request_access(request_access.into());
    ui.set_text_menu(menu.into());
    ui.set_text_settings(settings.into());
    ui.set_text_settings_title(settings.into());
    ui.set_text_theme(theme.into());
    ui.set_text_theme_system(theme_system.into());
    ui.set_text_theme_light(theme_light.into());
    ui.set_text_theme_dark(theme_dark.into());
    ui.set_text_language(language_label.into());
    ui.set_text_language_chinese(chinese.into());
    ui.set_text_language_english(english.into());
    ui.set_text_show_hidden_files(show_hidden_files.into());
    ui.set_text_show_system_files(show_system_files.into());
    ui.set_text_settings_general(settings_general.into());
    ui.set_text_settings_appearance(settings_appearance.into());
    ui.set_text_settings_developer(settings_developer.into());
    ui.set_text_ui_showcase(ui_showcase.into());
    ui.set_text_ui_showcase_detail(ui_showcase_detail.into());
    ui.set_text_show_delete_confirmation(show_delete_confirmation.into());
    ui.set_text_show_conflict_confirmation(show_conflict_confirmation.into());
    ui.set_text_show_exit_confirmation(show_exit_confirmation.into());
    ui.set_text_show_operation_window(show_operation_window.into());
    ui.set_text_window_trace_title(window_trace_title.into());
    ui.set_text_window_trace_detail(window_trace_detail.into());
    ui.set_text_window_trace_start(window_trace_start.into());
    ui.set_text_window_trace_active(window_trace_active.into());
    ui.set_text_window_trace_path(window_trace_path.into());
    ui.set_text_loading(loading.into());
    ui.set_text_empty_folder(empty_folder.into());
    ui.set_text_no_search_results(no_search_results.into());
    ui.set_text_new_tab(new_tab.into());
    ui.set_text_close_tab(close_tab.into());
    ui.set_text_all_tabs(all_tabs.into());
    ui.set_text_window_minimize(minimize.into());
    ui.set_text_window_restore(restore.into());
    ui.set_text_window_maximize(maximize.into());
    ui.set_text_window_close(close.into());
    ui.set_text_address(address.into());
    ui.set_text_cancel_edit(cancel_edit.into());
    ui.set_text_search_recursive(search_recursive.into());
    ui.set_text_search_current(search_current.into());
    ui.set_text_search_global_disabled(search_global_disabled.into());
    ui.set_text_drop_copy(drop_copy.into());
    ui.set_text_drop_move(drop_move.into());
    ui.set_text_drop_link(drop_link.into());
    ui.set_text_drop_cancel(drop_cancel.into());
    let (file_operations, task_count, cancel_operation, retry_operation) = match language {
        Language::Chinese => ("文件操作", "个任务", "取消", "重试"),
        Language::English => ("File operations", "tasks", "Cancel", "Retry"),
    };
    ui.set_text_file_operations(file_operations.into());
    ui.set_text_task_count(task_count.into());
    ui.set_text_cancel_operation(cancel_operation.into());
    ui.set_text_retry_operation(retry_operation.into());
}

fn display_path(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
}

fn is_drive_root(path: &Path) -> bool {
    let value = path.as_os_str().to_string_lossy();
    value.len() == 3 && value.as_bytes()[1] == b':' && matches!(value.as_bytes()[2], b'\\' | b'/')
}

fn initial_path() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new("C:\\").to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_window_keeps_slint_model_bounded_at_deep_offsets() {
        assert_eq!(
            search_window_for_index(65_536, 133_796, 1),
            SearchWindow {
                start: 65_280,
                len: SEARCH_WINDOW_ITEM_LIMIT,
            }
        );
        assert_eq!(
            search_window_for_index(133_795, 133_796, 1),
            SearchWindow {
                start: 133_376,
                len: 420,
            }
        );
        assert_eq!(SEARCH_WINDOW_ITEM_LIMIT, 768);
    }

    #[test]
    fn search_window_preserves_position_inside_the_loaded_window() {
        let window = search_window_for_index(65_536, 133_796, 1);
        assert_eq!(
            search_window_viewport_y(65_536, window, ViewMode::Details, 1),
            -256.0 * 40.0
        );
        let grid_window = search_window_for_index(65_536, 133_796, 4);
        assert_eq!(grid_window.start % 4, 0);
        assert_eq!(
            search_window_viewport_y(65_536, grid_window, ViewMode::Grid, 4),
            -64.0 * 148.0
        );
    }
    #[test]
    fn logical_search_scroll_maps_each_view_mode_to_absolute_result_index() {
        assert_eq!(
            search_result_index_at_scroll(-65_536.0 * 40.0, 133_796, ViewMode::Details, 1),
            65_536
        );
        assert_eq!(
            search_result_index_at_scroll(-65_536.0 * 34.0, 133_796, ViewMode::List, 1),
            65_536
        );
        assert_eq!(
            search_result_index_at_scroll(-16_384.0 * 148.0, 133_796, ViewMode::Grid, 4),
            65_536
        );
        assert_eq!(
            search_result_index_at_scroll(-f32::MAX, 133_796, ViewMode::Details, 1),
            133_795
        );
    }
    #[test]
    fn tab_drop_routes_physical_screen_cursor_at_each_dpi() {
        assert_eq!(
            screen_to_client_physical(800, 400, 640, 250),
            (160.0, 150.0)
        );
        assert_eq!(grid_thumbnail_request_px(1.0), 100);
        assert_eq!(grid_thumbnail_request_px(1.5), 150);
    }

    #[test]
    fn grid_directory_batch_keeps_icon_loading_separate_from_thumbnail_planning() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\grid")], 0, [0, 1, 2, 3]);
        app.directory_view_modes
            .insert(PathBuf::from(r"C:\grid"), ViewMode::Grid);
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.latest_request = RequestId(4);
        tab.load_state = LoadState::Loading;
        let state = Arc::new(Mutex::new(app));

        let requests = apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: TabId(1),
                request_id: RequestId(4),
                entries: vec![focus_entry(1, r"C:\grid\photo.png")],
            },
        );

        assert_eq!(requests.len(), 1);
        assert!(!requests[0].thumbnail);
        assert_eq!(requests[0].requested_px, 0);
        assert_eq!(
            grid_thumbnail_request_indices(500, 4, -296.0, 444.0),
            (0..32).collect::<Vec<_>>()
        );
        assert_eq!(
            grid_thumbnail_request_indices(500, 4, -1480.0, 296.0),
            (32..60).collect::<Vec<_>>()
        );
    }

    #[test]
    fn operation_window_waits_until_conflict_is_resolved_and_runtime_reaches_threshold() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        let id = app.operations.submit(
            FileOperationKind::Copy,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from("a")),
                Some(PathBuf::from("b")),
            )],
        );
        app.operations.start_next().unwrap();
        app.operations.mark_running(id).unwrap();
        {
            let task = app.operations.task_mut(id).unwrap();
            task.started_at = Instant::now() - Duration::from_millis(900);
            let snapshot = crate::domain::file_operations::FileSnapshot {
                path: PathBuf::from("a"),
                is_directory: false,
                size_bytes: Some(1),
                modified: None,
            };
            task.set_conflict(crate::domain::file_operations::OperationConflict {
                category: crate::domain::file_operations::ConflictCategory::ExistingFile,
                source: snapshot.clone(),
                destination: snapshot,
            })
            .unwrap();
        }
        assert!(!should_auto_open_operation_window(&app));

        app.operations
            .task_mut(id)
            .unwrap()
            .resolve_conflict(crate::domain::file_operations::ConflictDecision {
                action: crate::domain::file_operations::ConflictAction::Replace,
                apply_to_all: false,
            })
            .unwrap();
        assert!(should_auto_open_operation_window(&app));
    }

    #[test]
    fn remaining_time_uses_minutes_only_when_needed() {
        assert_eq!(
            format_remaining_time(Language::English, 42.0),
            "About 42 seconds remaining"
        );
        assert_eq!(
            format_remaining_time(Language::English, 102.0),
            "About 1min 42s remaining"
        );
        assert_eq!(
            format_remaining_time(Language::Chinese, 102.0),
            "预计剩余 1 分 42 秒"
        );
    }

    #[test]
    fn reorders_columns_in_both_directions_and_clamps_edges() {
        let mut order = [0, 1, 2, 3];
        assert!(reorder_column(&mut order, 1, -3));
        assert_eq!(order, [1, 0, 2, 3]);
        assert!(reorder_column(&mut order, 1, 8));
        assert_eq!(order, [0, 2, 3, 1]);
        assert!(!reorder_column(&mut order, 9, -1));
        assert!(!reorder_column(&mut order, 2, 0));
    }

    fn test_window_placement(x: i32) -> session_store::WindowPlacement {
        session_store::WindowPlacement {
            x,
            y: 80,
            width: 1180,
            height: 760,
        }
    }

    #[test]
    fn window_registry_allocates_global_tab_ids_without_reuse() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let first_extra = app.create_tab(PathBuf::from("two"));
        let second_window =
            app.register_window(vec![PathBuf::from("three")], 0, test_window_placement(160));
        let second_first = app.window(second_window).unwrap().active_tab;

        assert_eq!(
            app.window(first_window).unwrap().tab_order,
            [TabId(1), first_extra]
        );
        assert_eq!(second_first, TabId(3));
        app.active_window = second_window;
        assert_eq!(app.close_tab(second_first), None);
        let second_extra = app.create_tab(PathBuf::from("four"));
        assert_eq!(second_extra, TabId(4));
    }

    #[test]
    fn window_state_keeps_tabs_history_and_placement_isolated() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let closed = app.create_tab(PathBuf::from("closed"));
        app.close_tab(closed).unwrap();
        let second_window = app.register_window(
            vec![PathBuf::from("second"), PathBuf::from("active")],
            1,
            test_window_placement(240),
        );

        let first = app.window(first_window).unwrap();
        let second = app.window(second_window).unwrap();
        assert_eq!(first.closed_tabs, [PathBuf::from("closed")]);
        assert!(second.closed_tabs.is_empty());
        assert_eq!(first.active_tab, TabId(1));
        assert_eq!(second.active_tab, TabId(4));
        assert_eq!(first.placement.x, 80);
        assert_eq!(second.placement.x, 240);
    }

    #[test]
    fn closing_one_window_cancels_only_its_tabs_and_keeps_shared_operation() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        let (_, first_cancel) = app
            .window_mut(first_window)
            .unwrap()
            .tabs
            .get_mut(&first_tab)
            .unwrap()
            .begin_navigation(PathBuf::from("one/new"), NavigationKind::Normal);
        let (_, second_cancel) = app
            .window_mut(second_window)
            .unwrap()
            .tabs
            .get_mut(&second_tab)
            .unwrap()
            .begin_navigation(PathBuf::from("two/new"), NavigationKind::Normal);
        let operation = app.operations.submit(
            FileOperationKind::Copy,
            Some(first_tab),
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );

        assert_eq!(
            app.close_window(first_window),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(first_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(!second_cancel.load(std::sync::atomic::Ordering::Acquire));
        assert!(app.window(second_window).is_some());
        assert!(app.operations.task(operation).is_some());
        assert_eq!(
            app.close_window(second_window),
            Some(WindowCloseDecision::ExitApplication)
        );
    }

    #[test]
    fn directory_events_route_to_non_active_windows_by_global_tab_id() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        for tab_id in [first_tab, second_tab] {
            let tab = app.tab_mut(tab_id).unwrap();
            tab.latest_request = RequestId(7);
            tab.load_state = LoadState::Loading;
        }
        let state = Arc::new(Mutex::new(app));
        let entry = focus_entry(1, "two/item.txt");

        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: second_tab,
                request_id: RequestId(7),
                entries: vec![entry],
            },
        );
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: second_tab,
                request_id: RequestId(7),
                path: PathBuf::from("two"),
                skipped: 0,
            },
        );
        let app = state.lock().unwrap();
        assert!(app.tab(first_tab).unwrap().entries.is_empty());
        assert_eq!(app.tab(second_tab).unwrap().entries.len(), 1);
        assert_eq!(app.tab(second_tab).unwrap().load_state, LoadState::Complete);
    }

    #[test]
    fn search_and_icon_events_route_to_non_active_windows() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let second_tab = app.window(second_window).unwrap().active_tab;
        let tab = app.tab_mut(second_tab).unwrap();
        let (request_id, _) = tab.begin_search(SearchScope::Global, "item".into());
        let search_entry = focus_entry(1, "two/item.txt");
        assert!(
            apply_search_page_event(
                &mut app,
                second_tab,
                request_id,
                0,
                vec![search_entry.clone()],
                1,
                1,
            )
            .is_some()
        );
        let event = IconEvent {
            tab_id: second_tab,
            request_id,
            target: IconTarget::Entry(EntryId(1)),
            path: search_entry.path,
            icon: platform::windows_shell_icons::ShellIconRgba {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0, 0],
            },
            actual_thumbnail: false,
            requested_px: 0,
        };
        assert!(icon_event_is_current(&app, &event));
    }

    #[test]
    fn closed_window_rejects_late_results_while_other_window_accepts_them() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        let first_tab = app.window(first_window).unwrap().active_tab;
        let second_tab = app.window(second_window).unwrap().active_tab;
        for tab_id in [first_tab, second_tab] {
            let tab = app.tab_mut(tab_id).unwrap();
            tab.latest_request = RequestId(9);
            tab.load_state = LoadState::Loading;
        }
        app.close_window(first_window).unwrap();
        let state = Arc::new(Mutex::new(app));
        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: first_tab,
                request_id: RequestId(9),
                entries: vec![focus_entry(1, "one/late.txt")],
            },
        );
        apply_event(
            &state,
            DirectoryEvent::Batch {
                tab_id: second_tab,
                request_id: RequestId(9),
                entries: vec![focus_entry(1, "two/current.txt")],
            },
        );
        let app = state.lock().unwrap();
        assert!(app.tab(first_tab).is_none());
        assert_eq!(app.tab(second_tab).unwrap().pending_entries.len(), 1);
    }

    #[test]
    fn close_decision_only_confirms_exit_for_last_window_with_active_tasks() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let first_window = app.active_window;
        let operation = app.operations.submit(
            FileOperationKind::Copy,
            None,
            vec![OperationItem::pending(
                Some(PathBuf::from("source")),
                Some(PathBuf::from("target")),
            )],
        );
        app.operations.start_next().unwrap();
        app.operations.mark_running(operation).unwrap();
        assert_eq!(
            app.request_window_close(first_window),
            WindowCloseAction::ConfirmApplicationExit
        );
        let second_window =
            app.register_window(vec![PathBuf::from("two")], 0, test_window_placement(160));
        assert_eq!(
            app.request_window_close(first_window),
            WindowCloseAction::CloseWindow
        );
        app.close_window(first_window).unwrap();
        assert_eq!(
            app.request_window_close(second_window),
            WindowCloseAction::ConfirmApplicationExit
        );
        assert!(app.operations.task(operation).is_some());
    }

    #[test]
    fn last_window_without_tasks_keeps_state_until_session_snapshot_is_read() {
        let app = AppState::new_for_test(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            1,
            [0, 1, 2, 3],
        );
        let window = app.active_window;

        assert_eq!(
            app.request_window_close(window),
            WindowCloseAction::ExitApplication
        );
        assert_eq!(
            app.stable_paths(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(app.stable_active_path_index(), 1);
        assert!(app.window(window).is_some());
    }

    fn begin_drag_at(app: &mut AppState, index: usize) -> (WindowId, TabId) {
        let window_id = app.active_window;
        let tab_id = app.active_window_state().tab_order[index];
        assert!(app.begin_tab_drag(window_id, tab_id, index, 100.0, 20.0));
        (window_id, tab_id)
    }

    #[test]
    fn tab_drag_threshold_and_invalid_release_preserve_order() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let original = app.active_window_state().tab_order.clone();
        begin_drag_at(&mut app, 1);
        assert_eq!(
            app.update_tab_drag(104.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            None
        );
        assert!(!app.finish_tab_drag(true));
        assert_eq!(app.active_window_state().tab_order, original);

        begin_drag_at(&mut app, 1);
        assert_eq!(
            app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            Some(2)
        );
        assert!(!app.finish_tab_drag(false));
        assert_eq!(app.active_window_state().tab_order, original);
    }

    #[test]
    fn single_tab_window_rejects_tab_drag() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let window = app.active_window;
        let tab = app.active_window_state().active_tab;

        assert!(!app.begin_tab_drag(window, tab, 0, 100.0, 20.0));
        assert!(app.tab_drag.is_none());
        assert_eq!(app.active_window_state().tab_order, [tab]);
    }

    #[test]
    fn tab_drag_reorders_only_ids_and_preserves_request_identity() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            1,
            [0, 1, 2, 3],
        );
        let active = app.active_window_state().active_tab;
        let tab = app.tab_mut(active).unwrap();
        let (request_id, cancel) =
            tab.begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        let (_, moved) = begin_drag_at(&mut app, 0);
        assert_eq!(
            app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0),
            Some(2)
        );
        assert!(app.finish_tab_drag(true));
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(2), TabId(3), moved]
        );
        assert_eq!(app.active_window_state().active_tab, active);
        assert_eq!(app.tab(active).unwrap().latest_request, request_id);
        assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn detached_tab_commits_only_after_destination_is_reserved() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source_window = app.active_window;
        let source_order = app.active_window_state().tab_order.clone();
        let (_, tab_id) = begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        assert!(
            app.detach_dragged_tab_to_window(source_window, test_window_placement(160))
                .is_none()
        );
        assert_eq!(app.window(source_window).unwrap().tab_order, source_order);
        assert_eq!(app.window_for_tab(tab_id), Some(source_window));

        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();
        assert_eq!(outcome.tab_id, tab_id);
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(app.window(destination).unwrap().tab_order, [tab_id]);
        assert_eq!(app.window(source_window).unwrap().tab_order, [TabId(2)]);
    }

    #[test]
    fn detaching_pending_tab_cancels_old_request_and_requires_restart() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let tab_id = app.active_window_state().active_tab;
        let (_, token) = app
            .tab_mut(tab_id)
            .unwrap()
            .begin_navigation(PathBuf::from("pending"), NavigationKind::Refresh);
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(token.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            outcome.restart,
            Some(DetachedTabRestart::Directory(PathBuf::from("pending")))
        );
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
    }

    #[test]
    fn cross_window_move_inserts_at_target_and_keeps_single_owner() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let destination = app.register_window(
            vec![PathBuf::from("c"), PathBuf::from("d")],
            0,
            test_window_placement(160),
        );
        let tab_id = app.window(source).unwrap().tab_order[0];
        let request = app.tab(tab_id).unwrap().latest_request;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);

        let outcome = app.move_dragged_tab_to_window(destination, 1).unwrap();

        assert_eq!(app.window(source).unwrap().tab_order, [TabId(2)]);
        assert_eq!(
            app.window(destination).unwrap().tab_order,
            [TabId(3), tab_id, TabId(4)]
        );
        assert_eq!(app.window_for_tab(tab_id), Some(destination));
        assert_eq!(app.window(destination).unwrap().active_tab, tab_id);
        assert_eq!(app.tab(tab_id).unwrap().latest_request, request);
        assert!(!outcome.source_window_closed);
    }

    #[test]
    fn cross_window_move_failure_and_cancel_leave_source_untouched() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let tab_id = app.window(source).unwrap().active_tab;
        app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        assert!(app.move_dragged_tab_to_window(WindowId(999), 0).is_none());
        assert!(app.cancel_tab_drag());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert_eq!(app.window(source).unwrap().tab_order, [tab_id, TabId(2)]);
    }

    #[test]
    fn closing_target_window_does_not_cancel_source_drag() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let target = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let tab_id = app.window(source).unwrap().active_tab;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        assert!(
            app.update_tab_drag(108.0, 20.0, 47.0, 540.0, 0.0, 178.0)
                .is_some()
        );

        assert!(!app.cancel_tab_drag_for_window(target));
        assert!(app.tab_drag.is_some_and(|drag| drag.window_id == source));
        assert_eq!(
            app.close_window(target),
            Some(WindowCloseDecision::KeepRunning)
        );
        assert!(app.tab_drag.is_some());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
    }

    #[test]
    fn closing_source_window_cancels_its_drag() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let target = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let tab_id = app.window(source).unwrap().active_tab;
        assert!(app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        assert!(app.cancel_tab_drag_for_window(source));
        assert!(app.tab_drag.is_none());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert!(app.window(target).is_some());
    }

    #[test]
    fn physical_client_coordinates_convert_once_across_dpi_boundaries() {
        assert_eq!(
            physical_client_to_logical(1250.0, 350.0, 1000, 200, 1.0),
            (250.0, 150.0)
        );
        assert_eq!(
            physical_client_to_logical(1250.0, 350.0, 1000, 200, 1.5),
            (500.0 / 3.0, 100.0)
        );
        assert_eq!(
            physical_client_to_logical(900.0, 260.0, 750, 200, 1.0),
            (150.0, 60.0)
        );
    }

    #[test]
    fn external_insertion_slot_uses_midpoints_scroll_and_bounds() {
        assert_eq!(
            external_tab_insertion_slot(46.0, 47.0, 300.0, 0.0, 80.0, 3),
            None
        );
        assert_eq!(
            external_tab_insertion_slot(47.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(0)
        );
        assert_eq!(
            external_tab_insertion_slot(87.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(1)
        );
        assert_eq!(
            external_tab_insertion_slot(132.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(1)
        );
        assert_eq!(
            external_tab_insertion_slot(300.0, 47.0, 300.0, 0.0, 80.0, 3),
            Some(3)
        );
        assert_eq!(
            external_tab_insertion_slot(52.0, 47.0, 300.0, -100.0, 80.0, 3),
            Some(1)
        );
        assert_eq!(
            external_tab_insertion_slot(348.0, 47.0, 300.0, 0.0, 80.0, 3),
            None
        );
    }

    #[test]
    fn single_tab_cannot_detach_into_an_equivalent_new_window() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let tab_id = app.active_window_state().active_tab;
        assert!(!app.begin_tab_drag(source, tab_id, 0, 100.0, 20.0));
        let destination = app.reserve_window_id();
        assert!(
            app.detach_dragged_tab_to_window(destination, test_window_placement(160))
                .is_none()
        );
        assert!(app.window(source).is_some());
        assert_eq!(app.window_for_tab(tab_id), Some(source));
    }

    #[test]
    fn settings_window_stays_open_when_its_only_file_tab_is_detached() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let source = app.active_window;
        let file_tab = app.active_window_state().active_tab;
        let settings = app.open_settings();
        assert!(app.begin_tab_drag(source, file_tab, 0, 100.0, 20.0));
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(!outcome.source_window_closed);
        assert_eq!(app.window(source).unwrap().tab_order, [settings]);
        assert_eq!(app.window(source).unwrap().active_tab, settings);
    }

    #[test]
    fn cancelled_or_below_threshold_detach_restores_source() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let source = app.active_window;
        let (_, tab_id) = begin_drag_at(&mut app, 0);
        app.update_tab_drag(103.0, 20.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        assert!(
            app.detach_dragged_tab_to_window(destination, test_window_placement(160))
                .is_none()
        );
        app.cancel_tab_drag();
        assert_eq!(app.window_for_tab(tab_id), Some(source));
        assert!(app.window(destination).is_none());
    }

    #[test]
    fn background_window_projection_does_not_change_active_window() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("a")], 0, [0, 1, 2, 3]);
        let first = app.active_window;
        let second = app.register_window(vec![PathBuf::from("b")], 0, test_window_placement(160));
        let state = Arc::new(Mutex::new(app));

        let projected = WindowSessions::new(state.clone(), second);
        let guard = projected.peek().unwrap();
        assert_eq!(guard.active_window, first);
        drop(guard);
        assert_eq!(state.lock().unwrap().active_window, first);
    }

    #[test]
    fn partial_search_detach_cancels_and_requests_restart() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let tab_id = app.active_window_state().active_tab;
        let token = {
            let tab = app.tab_mut(tab_id).unwrap();
            let (_, token) = tab.begin_search(
                SearchScope::Directory(PathBuf::from("a")),
                "needle".to_owned(),
            );
            tab.search_state = SearchState::Partial;
            token
        };
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(100.0, 80.0, 47.0, 540.0, 0.0, 178.0);
        let destination = app.reserve_window_id();
        let outcome = app
            .detach_dragged_tab_to_window(destination, test_window_placement(160))
            .unwrap();

        assert!(token.load(std::sync::atomic::Ordering::Acquire));
        assert!(matches!(
            outcome.restart,
            Some(DetachedTabRestart::Search { query, .. }) if query == "needle"
        ));
    }

    #[test]
    fn settings_tab_is_fixed_and_bounds_the_file_tab_range() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b")],
            0,
            [0, 1, 2, 3],
        );
        let settings = app.open_settings();
        let third = app.create_tab(PathBuf::from("c"));
        assert!(!app.begin_tab_drag(app.active_window, settings, 2, 100.0, 20.0));
        assert!(app.begin_tab_drag(app.active_window, third, 3, 600.0, 20.0));
        assert_eq!(
            app.update_tab_drag(50.0, 20.0, 47.0, 720.0, 0.0, 178.0),
            Some(3)
        );
        assert!(!app.finish_tab_drag(true));
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(1), TabId(2), settings, third]
        );
    }

    #[test]
    fn tab_drag_slot_accounts_for_overflow_viewport_offset() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            0,
            [0, 1, 2, 3],
        );
        begin_drag_at(&mut app, 2);
        assert_eq!(
            app.update_tab_drag(52.0, 20.0, 47.0, 240.0, -100.0, 80.0),
            Some(1)
        );
        app.cancel_tab_drag();
        assert_eq!(
            app.active_window_state().tab_order,
            [TabId(1), TabId(2), TabId(3)]
        );
    }

    #[test]
    fn tab_insertion_slot_uses_midpoints_gaps_ranges_and_bounds() {
        assert_eq!(tab_insertion_slot(46.0, 47.0, 300.0, 0.0, 80.0, 0..3), None);
        assert_eq!(
            tab_insertion_slot(48.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(0)
        );
        assert_eq!(
            tab_insertion_slot(87.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(0)
        );
        assert_eq!(
            tab_insertion_slot(88.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(130.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(340.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            Some(2)
        );
        assert_eq!(
            tab_insertion_slot(348.0, 47.0, 300.0, 0.0, 80.0, 0..3),
            None
        );
        assert_eq!(
            tab_insertion_slot(52.0, 47.0, 240.0, -100.0, 80.0, 0..3),
            Some(1)
        );
        assert_eq!(
            tab_insertion_slot(305.0, 47.0, 400.0, 0.0, 80.0, 3..5),
            Some(3)
        );
        assert_eq!(
            tab_insertion_slot(390.0, 47.0, 400.0, 0.0, 80.0, 3..5),
            Some(4)
        );
    }

    #[test]
    fn astf8_round_trip_uses_reordered_paths_and_active_file_index() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            1,
            [0, 1, 2, 3],
        );
        begin_drag_at(&mut app, 0);
        app.update_tab_drag(500.0, 20.0, 47.0, 540.0, 0.0, 178.0);
        app.finish_tab_drag(true);
        let temporary =
            std::env::temp_dir().join(format!("asterfiles-tab-order-{}.bin", std::process::id()));
        let session = session_store::SessionState::new(
            test_window_placement(80),
            app.stable_active_path_index(),
            app.stable_paths(),
            [0, 1, 2, 3],
        )
        .unwrap();
        session_store::save(&temporary, &session).unwrap();
        let restored = session_store::load(&temporary).unwrap();
        std::fs::remove_file(temporary).unwrap();
        assert_eq!(
            restored.windows[0].tab_paths,
            [PathBuf::from("b"), PathBuf::from("c"), PathBuf::from("a")]
        );
        assert_eq!(restored.windows[0].active_tab, 0);
    }
    #[test]
    fn complete_tab_duplication_shares_entries_without_reloading() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        let source = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        source.latest_request = RequestId(7);
        source.load_state = LoadState::Complete;
        source.replace_entries(vec![FileEntry {
            id: EntryId(1),
            original_name: "file.txt".into(),
            display_name: "file.txt".into(),
            name_highlights: Vec::new(),
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: "same".into(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
        }]);
        let source_entries = source.entries.clone();

        let duplicate = app.duplicate_active_tab().expect("complete tab duplicates");
        let duplicated = app.active_window_state().tabs.get(&duplicate).unwrap();

        assert!(Arc::ptr_eq(&source_entries, &duplicated.entries));
        assert_eq!(duplicated.latest_request, RequestId(7));
        assert_eq!(duplicated.load_state, LoadState::Complete);
    }
    #[test]
    fn closing_a_tab_preserves_another_session() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        assert_eq!(app.active_window_state().active_tab, second);
        assert_eq!(app.close_tab(TabId(2)), Some(TabId(1)));
        assert_eq!(app.active().current_path, Some(PathBuf::from("one")));
    }

    #[test]
    fn closing_an_inactive_tab_keeps_the_active_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        let third = app.create_tab(PathBuf::from("three"));
        assert_eq!(app.active_window_state().active_tab, third);

        assert_eq!(app.close_tab(second), Some(third));
        assert_eq!(app.active_window_state().active_tab, third);
        assert_eq!(app.active().current_path, Some(PathBuf::from("three")));
        assert!(!app.active_window_state().tabs.contains_key(&second));
    }

    #[test]
    fn settings_tab_is_singleton_and_does_not_restore() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);

        let settings = app.open_settings();
        assert_eq!(app.open_settings(), settings);
        assert_eq!(app.active_window_state().tab_order.len(), 2);
        assert_eq!(app.active().kind, TabKind::Settings);

        assert_eq!(app.close_tab(settings), Some(TabId(1)));
        assert!(app.active_window_state().closed_tabs.is_empty());
        assert!(app.restore_closed().is_none());
    }

    #[test]
    fn settings_tab_is_excluded_from_saved_paths_and_active_index() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("one"), PathBuf::from("two")],
            0,
            [0, 1, 2, 3],
        );

        app.open_settings();
        assert_eq!(
            app.stable_paths(),
            [PathBuf::from("one"), PathBuf::from("two")]
        );
        assert_eq!(app.stable_active_path_index(), 1);
    }

    #[test]
    fn last_file_tab_cannot_be_closed_while_settings_is_open() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        app.open_settings();

        assert_eq!(app.close_tab(TabId(1)), None);
        assert!(app.active_window_state().tabs.contains_key(&TabId(1)));
    }

    #[test]
    fn settings_tab_never_submits_directory_navigation() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("one")],
            0,
            [0, 1, 2, 3],
        )));
        let settings = state
            .lock()
            .expect("app state mutex is not poisoned")
            .open_settings();
        let (sender, receiver) = mpsc::channel();

        assert!(!submit_navigation(
            &sender,
            &state,
            settings,
            PathBuf::from("ignored"),
            NavigationKind::Refresh,
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn same_path_normal_navigation_is_ignored_without_new_request() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("same")],
            0,
            [0, 1, 2, 3],
        )));
        let (sender, receiver) = mpsc::channel();

        assert!(!submit_navigation(
            &sender,
            &state,
            TabId(1),
            PathBuf::from("same"),
            NavigationKind::Normal,
        ));
        assert!(receiver.try_recv().is_err());
        let app = state.lock().expect("app state mutex is not poisoned");
        assert_eq!(app.active().latest_request, RequestId(0));
        assert!(app.active().back_history.is_empty());
    }

    #[test]
    fn directory_view_modes_are_shared_by_raw_path_without_touching_requests() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from("A"), PathBuf::from("B")],
            0,
            [0, 1, 2, 3],
        );
        let tab_a = app.active_window_state().tab_order[0];
        let request = app.tab(tab_a).unwrap().latest_request;
        app.directory_view_modes
            .insert(PathBuf::from("A"), ViewMode::Grid);
        app.directory_view_modes
            .insert(PathBuf::from("B"), ViewMode::List);
        assert_eq!(
            app.directory_view_modes.get(Path::new("A")),
            Some(&ViewMode::Grid)
        );
        assert_eq!(
            app.directory_view_modes.get(Path::new("B")),
            Some(&ViewMode::List)
        );
        assert_eq!(app.tab(tab_a).unwrap().latest_request, request);
    }

    #[test]
    fn grouped_search_page_matches_everything_files_first_setting() {
        let executable = PathBuf::from(r"C:\Program Files\Everything 1.5a\Everything64.exe");
        if !executable.is_file() {
            return;
        }
        let client = crate::platform::windows::everything::EverythingClient::new(
            crate::platform::windows::everything::PlatformEverythingConfig {
                executable_path: executable,
                instance_name: "1.5a".into(),
                allow_start: false,
            },
        )
        .unwrap();
        let GroupedSearchPage {
            items,
            total,
            file_total,
            response_offsets_valid,
        } = search_grouped_page(
            &client,
            (None, true),
            ".md".into(),
            crate::platform::windows::everything::EverythingSort::NameAscending,
            0,
            30,
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(response_offsets_valid);
        assert_eq!(items.len(), 30);
        assert!(total > items.len() as u32);
        assert!(file_total <= total);
        assert!(items.iter().all(|item| !item.is_directory));
    }
    #[test]
    fn search_sort_state_is_independent_and_defaults_to_everything_session_direction() {
        let mut tab = TabSession::new(TabId(1));
        assert_eq!(tab.sort_field, SortField::Name);
        assert_eq!(tab.sort_direction, SortDirection::Ascending);
        assert_eq!(tab.search_sort_field, SortField::Name);
        assert_eq!(tab.search_sort_direction, SortDirection::Descending);
        tab.set_search_sort(SortField::Name);
        assert_eq!(tab.search_sort_direction, SortDirection::Ascending);
        assert_eq!(tab.sort_direction, SortDirection::Ascending);
        tab.set_sort(SortField::Name);
        assert_eq!(tab.sort_direction, SortDirection::Descending);
        assert_eq!(tab.search_sort_direction, SortDirection::Ascending);
    }
    #[test]
    fn search_sort_arrow_maps_to_query2_service_sort() {
        use crate::platform::windows::everything::EverythingSort;
        assert_eq!(
            everything_sort(SortField::Name, SortDirection::Ascending),
            EverythingSort::NameAscending
        );
        assert_eq!(
            everything_sort(SortField::Name, SortDirection::Descending),
            EverythingSort::NameDescending
        );
        assert_eq!(
            everything_sort(SortField::Size, SortDirection::Ascending),
            EverythingSort::SizeAscending
        );
        assert_eq!(
            everything_sort(SortField::Modified, SortDirection::Descending),
            EverythingSort::ModifiedDescending
        );
    }
    #[test]
    fn root_path_has_no_parent_navigation_target() {
        assert!(Path::new(r"C:\").parent().is_none());
        assert!(is_drive_root(Path::new(r"C:\")));
        assert!(!is_drive_root(Path::new(r"C:\Users")));
    }

    #[test]
    fn icon_events_require_matching_tab_request_entry_and_path() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.latest_request = RequestId(7);
        tab.replace_entries(vec![FileEntry {
            id: EntryId(3),
            original_name: "file.txt".into(),
            display_name: "file.txt".into(),
            name_highlights: Vec::new(),
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: "same".into(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
        }]);
        let event = IconEvent {
            tab_id: TabId(1),
            request_id: RequestId(7),
            target: IconTarget::Entry(EntryId(3)),
            path: PathBuf::from("same/file.txt"),
            icon: platform::windows_shell_icons::ShellIconRgba {
                width: 1,
                height: 1,
                pixels: vec![0, 0, 0, 0],
            },
            actual_thumbnail: false,
            requested_px: 0,
        };
        assert!(icon_event_is_current(&app, &event));

        let location_event = IconEvent {
            tab_id: TabId(1),
            request_id: RequestId(7),
            target: IconTarget::Location,
            path: PathBuf::from("same"),
            icon: event.icon.clone(),
            actual_thumbnail: false,
            requested_px: 0,
        };
        assert!(icon_event_is_current(&app, &location_event));

        let mut stale_request = event;
        stale_request.request_id = RequestId(6);
        assert!(!icon_event_is_current(&app, &stale_request));
        stale_request.request_id = RequestId(7);
        stale_request.path = PathBuf::from("same/other.txt");
        assert!(!icon_event_is_current(&app, &stale_request));
        app.active_window_state_mut().tabs.remove(&TabId(1));
        assert!(!icon_event_is_current(&app, &stale_request));
    }

    #[test]
    fn folder_size_errors_keep_distinct_states() {
        use platform::windows::everything::{EverythingError, EverythingFolderSize};

        assert_eq!(
            folder_size_state(Ok(EverythingFolderSize::Indexed(0))),
            FolderSizeState::Value(0)
        );
        assert_eq!(
            folder_size_state(Ok(EverythingFolderSize::NotIndexed)),
            FolderSizeState::NotIndexed
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::FolderSizeRejected(404))),
            FolderSizeState::NotFound
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Timeout)),
            FolderSizeState::TimedOut
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::FolderSizeDisconnected)),
            FolderSizeState::Disconnected
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Protocol("response code 404".into()))),
            FolderSizeState::NotFound
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Protocol("bad response".into()))),
            FolderSizeState::ProtocolError
        );
        assert_eq!(
            folder_size_state(Err(EverythingError::Windows(5))),
            FolderSizeState::ProtocolError
        );
    }

    #[test]
    fn stale_folder_size_event_cannot_update_reused_entry_id() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\current")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.latest_request = RequestId(8);
        tab.page_source = PageSource::Directory;
        tab.replace_entries(vec![FileEntry {
            id: EntryId(1),
            path: PathBuf::from(r"C:\current\same-id"),
            original_name: "same-id".into(),
            display_name: "same-id".into(),
            name_highlights: Vec::new(),
            kind: crate::domain::EntryKind::Directory,
            open_target: None,
            parent_display: r"C:\current".into(),
            size_bytes: None,
            folder_size: FolderSizeState::Querying,
            modified: None,
        }]);

        assert!(!apply_folder_size_event(
            &mut app,
            TabId(1),
            RequestId(7),
            EntryId(1),
            Path::new(r"C:\old\same-id"),
            FolderSizeState::Value(12),
        ));
        assert!(!apply_folder_size_event(
            &mut app,
            TabId(1),
            RequestId(8),
            EntryId(1),
            Path::new(r"C:\old\same-id"),
            FolderSizeState::Value(12),
        ));
        assert_eq!(
            app.active_window_state().tabs[&TabId(1)].entries[0].folder_size,
            FolderSizeState::Querying
        );
        assert!(apply_folder_size_event(
            &mut app,
            TabId(1),
            RequestId(8),
            EntryId(1),
            Path::new(r"C:\current\same-id"),
            FolderSizeState::Value(0),
        ));
        assert_eq!(
            app.active_window_state().tabs[&TabId(1)].entries[0].folder_size,
            FolderSizeState::Value(0)
        );
    }

    fn focus_entry(id: u32, path: &str) -> FileEntry {
        FileEntry {
            id: EntryId(id),
            original_name: Path::new(path).file_name().unwrap().to_os_string(),
            display_name: Path::new(path)
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            name_highlights: Vec::new(),
            path: PathBuf::from(path),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: String::new(),
            size_bytes: Some(1),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: None,
        }
    }

    #[test]
    fn pressing_an_unselected_row_for_drag_does_not_mutate_selection() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\test")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.replace_entries(vec![
            focus_entry(1, r"C:\test\one.txt"),
            focus_entry(2, r"C:\test\two.txt"),
            focus_entry(3, r"C:\test\three.txt"),
        ]);
        tab.select_entry(EntryId(1), false, false);

        assert_eq!(
            drag_paths_for_pressed_entry(&app, EntryId(2)),
            vec![PathBuf::from(r"C:\test\two.txt")]
        );
        let tab = &app.active_window_state().tabs[&TabId(1)];
        assert_eq!(tab.selected, vec![EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
        assert_eq!(tab.selection_anchor, Some(EntryId(1)));
    }

    #[test]
    fn pressing_a_selected_row_for_drag_uses_the_existing_multi_selection() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\test")], 0, [0, 1, 2, 3]);
        let tab = app
            .active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap();
        tab.replace_entries(vec![
            focus_entry(1, r"C:\test\one.txt"),
            focus_entry(2, r"C:\test\two.txt"),
        ]);
        tab.select_entry(EntryId(1), false, false);
        tab.select_entry(EntryId(2), true, false);

        assert_eq!(
            drag_paths_for_pressed_entry(&app, EntryId(2)),
            vec![
                PathBuf::from(r"C:\test\one.txt"),
                PathBuf::from(r"C:\test\two.txt"),
            ]
        );
    }
    #[test]
    fn completed_focus_selects_all_targets_in_every_matching_tab() {
        let mut app = AppState::new_for_test(
            vec![PathBuf::from(r"C:\target"), PathBuf::from(r"C:\target")],
            0,
            [0, 1, 2, 3],
        );
        let targets = vec![
            PathBuf::from(r"C:\target\first.txt"),
            PathBuf::from(r"C:\target\second.txt"),
        ];
        queue_completed_focus(&mut app, &targets);
        for tab_id in [TabId(1), TabId(2)] {
            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            let (_, cancel) =
                tab.begin_navigation(PathBuf::from(r"C:\target"), NavigationKind::Refresh);
            assert!(!cancel.load(std::sync::atomic::Ordering::Acquire));
            let request_id = tab.latest_request;
            tab.pending_entries = vec![
                focus_entry(1, r"C:\target\first.txt"),
                focus_entry(2, r"C:\target\second.txt"),
            ];
            app.focus_after_refresh.get_mut(&tab_id).unwrap().request_id = Some(request_id);
        }
        let state = Arc::new(Mutex::new(app));
        for tab_id in [TabId(1), TabId(2)] {
            apply_event(
                &state,
                DirectoryEvent::Finished {
                    tab_id,
                    request_id: RequestId(1),
                    path: PathBuf::from(r"C:\target"),
                    skipped: 0,
                },
            );
        }
        let app = state.lock().unwrap();
        for tab_id in [TabId(1), TabId(2)] {
            let tab = &app.active_window_state().tabs[&tab_id];
            assert_eq!(tab.selected, vec![EntryId(1), EntryId(2)]);
            assert_eq!(tab.focused, Some(EntryId(2)));
        }
    }

    #[test]
    fn stale_refresh_does_not_consume_completed_focus() {
        let mut app = AppState::new_for_test(vec![PathBuf::from(r"C:\target")], 0, [0, 1, 2, 3]);
        queue_completed_focus(&mut app, &[PathBuf::from(r"C:\target\item.txt")]);
        app.focus_after_refresh
            .get_mut(&TabId(1))
            .unwrap()
            .request_id = Some(RequestId(8));
        let state = Arc::new(Mutex::new(app));
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: TabId(1),
                request_id: RequestId(7),
                path: PathBuf::from(r"C:\target"),
                skipped: 0,
            },
        );
        assert!(
            state
                .lock()
                .unwrap()
                .focus_after_refresh
                .contains_key(&TabId(1))
        );
    }

    #[test]
    fn final_target_uses_keep_both_path_and_excludes_existing_merge_root() {
        let item = OperationItem::pending(
            Some(PathBuf::from(r"C:\source\name.txt")),
            Some(PathBuf::from(r"C:\target\name.txt")),
        );
        let kept = PathBuf::from(r"C:\target\name (2).txt");
        assert_eq!(
            completed_target_for_item(&item, std::slice::from_ref(&kept), false),
            Some(kept)
        );
        assert_eq!(
            completed_target_for_item(
                &OperationItem::pending(
                    Some(PathBuf::from(r"C:\source\folder")),
                    Some(PathBuf::from(r"C:\target\folder")),
                ),
                &[PathBuf::from(r"C:\target\folder")],
                true,
            ),
            None
        );
    }

    #[test]
    fn rename_validation_keeps_invalid_names_out_of_operation_queue() {
        assert!(crate::fs::file_operations::validate_name(std::ffi::OsStr::new("")).is_err());
        assert!(
            crate::fs::file_operations::validate_name(std::ffi::OsStr::new("bad?.txt")).is_err()
        );
        assert!(
            crate::fs::file_operations::validate_name(std::ffi::OsStr::new("valid.txt")).is_ok()
        );
        assert!(!keyboard_shortcuts_suppressed(false));
        assert!(keyboard_shortcuts_suppressed(true));
    }
    #[test]
    fn permission_page_has_actionable_copy_in_both_languages() {
        for language in [Language::Chinese, Language::English] {
            let (title, description) =
                error_page_text(LoadState::PermissionDenied, Texts::new(language));
            assert!(!title.is_empty());
            assert!(!description.is_empty());
            assert!(description.contains("Windows"));
        }
    }
    #[test]
    fn context_menu_closes_on_window_deactivation() {
        assert!(should_close_context_menu(
            &winit::event::WindowEvent::Focused(false)
        ));
        assert!(should_close_context_menu(
            &winit::event::WindowEvent::Occluded(true)
        ));
        assert!(!should_close_context_menu(
            &winit::event::WindowEvent::Focused(true)
        ));
    }

    #[test]
    fn rename_editor_owns_keyboard_input_before_window_shortcuts() {
        assert!(keyboard_shortcuts_suppressed(true));
        assert!(!keyboard_shortcuts_suppressed(false));
    }

    #[test]
    fn repeated_context_menu_hit_test_switches_between_entry_and_background() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("C:/test")],
            0,
            [0, 1, 2, 3],
        )));
        {
            let mut app = state.lock().unwrap();
            let tab = app
                .active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap();
            tab.replace_entries(vec![FileEntry {
                id: EntryId(1),
                path: PathBuf::from("C:/test/item.txt"),
                original_name: std::ffi::OsString::from("item.txt"),
                display_name: "item.txt".into(),
                name_highlights: Vec::new(),
                kind: crate::domain::EntryKind::File,
                open_target: None,
                parent_display: "C:/test".into(),
                size_bytes: Some(1),
                folder_size: crate::domain::FolderSizeState::Unknown,
                modified: None,
            }]);
        }
        assert_eq!(
            context_target_at(&state, 0.0, 170.0, 166.0, 0.0, 0.0, 1),
            (Some(EntryId(1)), false)
        );
        assert_eq!(
            context_target_at(&state, 0.0, 250.0, 166.0, 0.0, 0.0, 1),
            (None, true)
        );
    }

    #[test]
    fn context_menu_hit_test_accounts_for_negative_viewport_offset() {
        let state = Arc::new(Mutex::new(AppState::new_for_test(
            vec![PathBuf::from("C:/test")],
            0,
            [0, 1, 2, 3],
        )));
        {
            let mut app = state.lock().unwrap();
            app.active_window_state_mut()
                .tabs
                .get_mut(&TabId(1))
                .unwrap()
                .replace_entries(
                    (1..=4)
                        .map(|id| FileEntry {
                            id: EntryId(id),
                            original_name: format!("{id}.txt").into(),
                            display_name: format!("{id}.txt"),
                            name_highlights: Vec::new(),
                            path: PathBuf::from(format!(r"C:\test\{id}.txt")),
                            kind: crate::domain::EntryKind::File,
                            open_target: None,
                            parent_display: "C:/test".into(),
                            size_bytes: Some(1),
                            folder_size: crate::domain::FolderSizeState::Unknown,
                            modified: None,
                        })
                        .collect(),
                );
        }
        assert_eq!(
            context_target_at(&state, 0.0, 170.0, 166.0, -80.0, 0.0, 1),
            (Some(EntryId(3)), false)
        );
    }

    #[test]
    fn watched_roots_deduplicate_tabs_showing_the_same_directory() {
        let app = AppState::new_for_test(
            vec![PathBuf::from(r"C:\One"), PathBuf::from(r"C:\One")],
            0,
            [0, 1, 2, 3],
        );
        assert_eq!(
            watched_roots(&app),
            std::collections::HashSet::from([PathBuf::from(r"C:\One")])
        );
    }
    #[test]
    fn side_navigation_consumes_both_phases_and_routes_only_release() {
        use winit::event::{ElementState, MouseButton};

        assert!(is_side_navigation_mouse_button(MouseButton::Back));
        assert!(is_side_navigation_mouse_button(MouseButton::Forward));
        assert!(!is_side_navigation_mouse_button(MouseButton::Left));
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Pressed, MouseButton::Back),
            None
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Pressed, MouseButton::Forward),
            None
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Released, MouseButton::Back),
            Some(SideNavigation::Back)
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Released, MouseButton::Forward),
            Some(SideNavigation::Forward)
        );
        assert_eq!(
            side_navigation_for_mouse_button(ElementState::Released, MouseButton::Left),
            None
        );
    }
    #[test]
    fn internal_drag_targets_only_directory_rows_with_viewport_offset() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("C:/test")], 0, [0, 1, 2, 3]);
        app.active_window_state_mut()
            .tabs
            .get_mut(&TabId(1))
            .unwrap()
            .replace_entries(vec![
                FileEntry {
                    id: EntryId(1),
                    original_name: "file.txt".into(),
                    display_name: "file.txt".into(),
                    name_highlights: Vec::new(),
                    path: PathBuf::from(r"C:\test\file.txt"),
                    kind: crate::domain::EntryKind::File,
                    open_target: None,
                    parent_display: String::new(),
                    size_bytes: Some(1),
                    folder_size: crate::domain::FolderSizeState::Unknown,
                    modified: None,
                },
                FileEntry {
                    id: EntryId(2),
                    original_name: "folder".into(),
                    display_name: "folder".into(),
                    name_highlights: Vec::new(),
                    path: PathBuf::from(r"C:\test\folder"),
                    kind: crate::domain::EntryKind::Directory,
                    open_target: None,
                    parent_display: String::new(),
                    size_bytes: None,
                    folder_size: crate::domain::FolderSizeState::Unknown,
                    modified: None,
                },
            ]);

        assert_eq!(internal_drag_target(&app, 20.0, 0.0, 0.0, 0.0, 1), None);
        assert_eq!(
            internal_drag_target(&app, 20.0, 0.0, -40.0, 0.0, 1),
            Some((EntryId(2), PathBuf::from(r"C:\test\folder")))
        );
    }

    #[test]
    fn drop_preflight_preserves_paths_and_builds_operation_items() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-drop-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        let source_parent = temporary.join("来源");
        let target = temporary.join("目标");
        std::fs::create_dir_all(&source_parent).unwrap();
        std::fs::create_dir(&target).unwrap();
        let source = source_parent.join("文件😀.txt");
        std::fs::write(&source, b"drag").unwrap();

        let PreparedDrop::Operation(kind, items) =
            prepare_drop_operation(platform::windows::drag_drop::DropIntent {
                paths: vec![source.clone()],
                target: target.clone(),
                effect: platform::windows::drag_drop::DropEffect::Copy,
                right_button: false,
                screen_x: 0,
                screen_y: 0,
                allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                    | platform::windows::drag_drop::ALLOW_MOVE
                    | platform::windows::drag_drop::ALLOW_LINK,
            })
            .unwrap()
        else {
            panic!("copy drag must create a file operation");
        };

        assert_eq!(kind, FileOperationKind::Copy);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source.as_deref(), Some(source.as_path()));
        assert_eq!(
            items[0].destination.as_deref(),
            Some(target.join("文件😀.txt").as_path())
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn link_drop_preflight_uses_numbered_shortcut_destination() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-link-drop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        let target = temporary.join("target");
        std::fs::create_dir_all(&target).unwrap();
        let source = temporary.join("Report.txt");
        std::fs::write(&source, b"source").unwrap();
        std::fs::write(target.join("Report.lnk"), b"existing").unwrap();

        let PreparedDrop::Shortcuts(shortcuts) =
            prepare_drop_operation(platform::windows::drag_drop::DropIntent {
                paths: vec![source.clone()],
                target: target.clone(),
                effect: platform::windows::drag_drop::DropEffect::Link,
                right_button: false,
                screen_x: 0,
                screen_y: 0,
                allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                    | platform::windows::drag_drop::ALLOW_MOVE
                    | platform::windows::drag_drop::ALLOW_LINK,
            })
            .unwrap()
        else {
            panic!("link drag must prepare shortcuts");
        };
        assert_eq!(shortcuts, vec![(source, target.join("Report (2).lnk"))]);
        std::fs::remove_dir_all(temporary).unwrap();
    }
    #[test]
    fn link_drop_reserves_distinct_names_for_same_named_sources() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-link-batch-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        let left = temporary.join("left");
        let right = temporary.join("right");
        let target = temporary.join("target");
        std::fs::create_dir_all(&left).unwrap();
        std::fs::create_dir_all(&right).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        let first = left.join("Report.txt");
        let second = right.join("Report.txt");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();

        let PreparedDrop::Shortcuts(shortcuts) =
            prepare_drop_operation(platform::windows::drag_drop::DropIntent {
                paths: vec![first.clone(), second.clone()],
                target: target.clone(),
                effect: platform::windows::drag_drop::DropEffect::Link,
                right_button: false,
                screen_x: 0,
                screen_y: 0,
                allowed_effects: platform::windows::drag_drop::ALLOW_COPY
                    | platform::windows::drag_drop::ALLOW_MOVE
                    | platform::windows::drag_drop::ALLOW_LINK,
            })
            .unwrap()
        else {
            panic!("link drag must prepare shortcuts");
        };
        assert_eq!(
            shortcuts,
            vec![
                (first, target.join("Report.lnk")),
                (second, target.join("Report (2).lnk")),
            ]
        );
        std::fs::remove_dir_all(temporary).unwrap();
    }
    #[test]
    fn internal_drag_releases_pointer_grab_once_before_ole() {
        assert!(!should_release_internal_pointer_grab(3.9, false));
        assert!(should_release_internal_pointer_grab(4.0, false));
        assert!(!should_release_internal_pointer_grab(8.0, true));
    }

    #[test]
    fn rectangle_selection_starts_only_after_threshold() {
        assert!(!rectangle_selection_started(10.0, 10.0, 13.0, 13.0));
        assert!(rectangle_selection_started(10.0, 10.0, 13.0, 14.0));
    }

    #[test]
    fn cancelling_rectangle_selection_restores_the_starting_snapshot() {
        let mut tab = TabSession::new(TabId(1));
        tab.replace_entries(
            (1..=3)
                .map(|id| focus_entry(id, &format!(r"C:\test\{id}.txt")))
                .collect(),
        );
        let snapshot = HashSet::from([EntryId(1)]);
        tab.apply_rectangle_selection(
            &snapshot,
            &HashSet::from([EntryId(2)]),
            RectangleSelectionMode::Replace,
        );
        assert_eq!(tab.selected, vec![EntryId(2)]);

        restore_rectangle_selection_snapshot(
            &mut tab,
            &snapshot.iter().copied().collect::<Vec<_>>(),
            Some(EntryId(1)),
            Some(EntryId(1)),
        );

        assert_eq!(tab.selected, vec![EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
        assert_eq!(tab.selection_anchor, Some(EntryId(1)));
    }

    #[test]
    fn rectangle_selection_intersects_in_all_directions() {
        let item = SelectionRect {
            left: 20.0,
            top: 20.0,
            right: 40.0,
            bottom: 40.0,
        };
        for rect in [
            SelectionRect::from_points(0.0, 0.0, 25.0, 25.0),
            SelectionRect::from_points(25.0, 0.0, 0.0, 25.0),
            SelectionRect::from_points(0.0, 25.0, 25.0, 0.0),
            SelectionRect::from_points(25.0, 25.0, 0.0, 0.0),
        ] {
            assert!(rect.intersects(item));
        }
        assert!(!SelectionRect::from_points(0.0, 0.0, 19.0, 19.0).intersects(item));
    }

    #[test]
    fn rectangle_selection_hits_detail_list_and_grid_geometry() {
        let mut tab = TabSession::new(TabId(1));
        tab.replace_entries(
            (1..=6)
                .map(|id| focus_entry(id, &format!(r"C:\test\{id}.txt")))
                .collect(),
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                SelectionRect::from_points(0.0, 39.0, 20.0, 41.0),
            ),
            HashSet::from([EntryId(1), EntryId(2)])
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::List,
                1,
                600.0,
                SelectionRect::from_points(0.0, 35.0, 20.0, 67.0),
            ),
            HashSet::from([EntryId(2)])
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Grid,
                3,
                600.0,
                SelectionRect::from_points(145.0, 145.0, 155.0, 155.0),
            ),
            HashSet::from([EntryId(5)])
        );
    }

    #[test]
    fn rectangle_selection_search_geometry_uses_sparse_result_identity() {
        let mut tab = TabSession::new(TabId(1));
        tab.begin_search(SearchScope::Global, ".txt".into());
        tab.merge_search_page(
            256,
            vec![focus_entry(257, r"C:\test\257.txt")],
            1_000,
            1_000,
            256,
        );
        assert_eq!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                SelectionRect::from_points(0.0, 256.0 * 40.0, 20.0, 257.0 * 40.0),
            ),
            HashSet::from([EntryId(257)])
        );
        assert!(
            rectangle_selection_hits(
                &tab,
                ViewMode::Details,
                1,
                600.0,
                SelectionRect::from_points(0.0, 0.0, 20.0, 40.0),
            )
            .is_empty()
        );
    }

    #[test]
    fn rectangle_selection_auto_scroll_uses_edge_distance_and_clamps_speed() {
        assert_eq!(rectangle_selection_scroll_delta(110.0, 100.0, 500.0), 10.0);
        assert_eq!(rectangle_selection_scroll_delta(490.0, 100.0, 500.0), -10.0);
        assert_eq!(rectangle_selection_scroll_delta(0.0, 100.0, 500.0), 40.0);
        assert_eq!(rectangle_selection_scroll_delta(600.0, 100.0, 500.0), -40.0);
        assert_eq!(rectangle_selection_scroll_delta(250.0, 100.0, 500.0), 0.0);
        assert_eq!(
            rectangle_selection_scroll_maximum(100, ViewMode::Details, 1, 400.0),
            3_600.0
        );
        assert_eq!(
            rectangle_selection_scroll_maximum(100, ViewMode::List, 1, 400.0),
            3_000.0
        );
        assert_eq!(
            rectangle_selection_scroll_maximum(10, ViewMode::Grid, 3, 400.0),
            192.0
        );
        assert_eq!(
            rectangle_selection_scroll_maximum(2, ViewMode::Grid, 3, 400.0),
            0.0
        );
    }

    #[test]
    fn native_tab_drag_starts_once_after_the_threshold_transition() {
        assert!(!should_start_native_tab_drag(false, false));
        assert!(should_start_native_tab_drag(true, false));
        assert!(!should_start_native_tab_drag(true, true));
    }
    fn right_drop_intent(
        paths: Vec<PathBuf>,
        target: PathBuf,
        allowed_effects: u32,
    ) -> platform::windows::drag_drop::DropIntent {
        platform::windows::drag_drop::DropIntent {
            paths,
            target,
            effect: platform::windows::drag_drop::DropEffect::Move,
            right_button: true,
            screen_x: 25,
            screen_y: 40,
            allowed_effects,
        }
    }

    #[test]
    fn right_drop_waits_for_a_menu_choice_and_cancel_does_not_prepare_work() {
        let intent = right_drop_intent(
            vec![PathBuf::from(r"C:\Source\item.txt")],
            PathBuf::from(r"C:\Target"),
            platform::windows::drag_drop::ALLOW_COPY,
        );
        assert!(drop_requires_choice(&intent));
        assert_eq!(selected_right_drop(intent, 0).unwrap(), None);
    }

    #[test]
    fn right_drop_choices_select_copy_move_and_link() {
        let allowed = platform::windows::drag_drop::ALLOW_COPY
            | platform::windows::drag_drop::ALLOW_MOVE
            | platform::windows::drag_drop::ALLOW_LINK;
        for (choice, effect) in [
            (1, platform::windows::drag_drop::DropEffect::Copy),
            (2, platform::windows::drag_drop::DropEffect::Move),
            (3, platform::windows::drag_drop::DropEffect::Link),
        ] {
            let selected = selected_right_drop(
                right_drop_intent(
                    vec![PathBuf::from(r"C:\Source\item.txt")],
                    PathBuf::from(r"C:\Target"),
                    allowed,
                ),
                choice,
            )
            .unwrap()
            .unwrap();
            assert_eq!(selected.effect, effect);
            assert!(!selected.right_button);
        }
    }

    #[test]
    fn right_drop_rejects_disallowed_effects_and_protected_paths() {
        let copy_only = right_drop_intent(
            vec![PathBuf::from(r"C:\Source\item.txt")],
            PathBuf::from(r"C:\Target"),
            platform::windows::drag_drop::ALLOW_COPY,
        );
        assert!(selected_right_drop(copy_only, 2).is_err());

        let descendant = right_drop_intent(
            vec![PathBuf::from(r"C:\Source")],
            PathBuf::from(r"C:\Source\Child"),
            platform::windows::drag_drop::ALLOW_COPY
                | platform::windows::drag_drop::ALLOW_MOVE
                | platform::windows::drag_drop::ALLOW_LINK,
        );
        assert!(selected_right_drop(descendant, 1).is_err());

        let same_location = right_drop_intent(
            vec![PathBuf::from(r"C:\Target\item.txt")],
            PathBuf::from(r"C:\Target"),
            platform::windows::drag_drop::ALLOW_COPY
                | platform::windows::drag_drop::ALLOW_MOVE
                | platform::windows::drag_drop::ALLOW_LINK,
        );
        assert_eq!(
            selected_right_drop(same_location.clone(), 1)
                .unwrap()
                .unwrap()
                .effect,
            platform::windows::drag_drop::DropEffect::Copy
        );
        assert!(selected_right_drop(same_location.clone(), 2).is_err());
        assert_eq!(
            selected_right_drop(same_location, 3)
                .unwrap()
                .unwrap()
                .effect,
            platform::windows::drag_drop::DropEffect::Link
        );
    }

    #[test]
    fn same_folder_drop_preflight_keeps_copy_and_link_executable() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-same-folder-drop-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        std::fs::create_dir_all(&temporary).unwrap();
        let source = temporary.join("Report.txt");
        std::fs::write(&source, b"source").unwrap();
        let allowed =
            platform::windows::drag_drop::ALLOW_COPY | platform::windows::drag_drop::ALLOW_LINK;

        let copy = selected_right_drop(
            right_drop_intent(vec![source.clone()], temporary.clone(), allowed),
            1,
        )
        .unwrap()
        .unwrap();
        let PreparedDrop::Operation(FileOperationKind::Copy, copy_items) =
            prepare_drop_operation(copy).unwrap()
        else {
            panic!("same-folder copy must remain executable");
        };
        assert_eq!(copy_items[0].source.as_deref(), Some(source.as_path()));
        assert_eq!(copy_items[0].destination.as_deref(), Some(source.as_path()));

        let link = selected_right_drop(
            right_drop_intent(vec![source.clone()], temporary.clone(), allowed),
            3,
        )
        .unwrap()
        .unwrap();
        let PreparedDrop::Shortcuts(shortcuts) = prepare_drop_operation(link).unwrap() else {
            panic!("same-folder link must remain executable");
        };
        assert_eq!(shortcuts[0].0, source);
        assert_eq!(shortcuts[0].1, temporary.join("Report.lnk"));
        create_drop_shortcuts(shortcuts).unwrap();
        assert!(temporary.join("Report.lnk").is_file());
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn external_cut_tracking_keeps_only_sources_still_on_disk() {
        let temporary = std::env::temp_dir().join(format!(
            "asterfiles-cut-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&temporary);
        std::fs::create_dir(&temporary).unwrap();
        let moved = temporary.join("moved.txt");
        let remaining = temporary.join("remaining.txt");
        std::fs::write(&moved, b"moved").unwrap();
        std::fs::write(&remaining, b"remaining").unwrap();
        std::fs::remove_file(&moved).unwrap();
        assert_eq!(existing_paths(&[moved, remaining.clone()]), vec![remaining]);
        std::fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn error_kinds_have_distinct_page_states() {
        assert_eq!(classify_error(io::ErrorKind::NotFound), LoadState::NotFound);
        assert_eq!(
            classify_error(io::ErrorKind::PermissionDenied),
            LoadState::PermissionDenied
        );
        assert_eq!(
            classify_error(io::ErrorKind::TimedOut),
            LoadState::Disconnected
        );
    }
}
