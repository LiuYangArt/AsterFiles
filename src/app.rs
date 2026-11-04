use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, HashSet, VecDeque},
    io,
    ops::Deref,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use slint::{
    Color, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    winit_030::winit::event::MouseScrollDelta,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{
    agent_debug::{self, AgentScenario},
    domain::{
        AddressMode, ColumnKind, ColumnLayout, DirectoryViewPreference, EntryId, FileEntry,
        FolderSizeState, GroupField, LibraryLocationId, LoadState, MAX_DIRECTORY_VIEW_PREFERENCES,
        NameHighlightSegment, NavigationKind, NavigationLocation, PageSource,
        RectangleSelectionMode, RequestId, SearchDepth, SearchScope, SearchState,
        SearchViewPreference, Selection, SortDirection, SortField, TabId, TabKind, TabSession,
        ViewMode,
        file_operations::{
            FileOperationKind, ItemState, OperationId, OperationItem, OperationManager,
            OperationResource, OperationState, PermanentDeletePhase, PermanentDeleteStage,
            UndoEntry, UndoHistory, UndoItem, UndoSourceKind,
        },
        folder_size_scheduler::{FOLDER_SIZE_QUEUE_CAPACITY, FolderSizeCommit, FolderSizeQuery},
        thumbnail_scheduler::{THUMBNAIL_QUEUE_CAPACITY, ThumbnailKey, ThumbnailPlans},
    },
    group_projection::{
        self, GroupProjectionContext, IconProjection, IconVisualRow, ListProjection, ListVisualRow,
    },
    i18n::{Language, Texts},
    network::{
        DiscoveryCoordinator, DiscoveryRequestId, DiscoveryState, NetworkDeviceTarget,
        NetworkLocation, NetworkLocationCatalog, NetworkLocationSource, NetworkTarget,
    },
    platform::{self, KnownLocation, KnownLocationKind},
    session_store,
};

use crate::platform::windows::address_path::{
    ClassifiedExternalLaunch, ExternalLaunchPath, classify_external_launches,
    classify_external_launches_off_thread,
};

pub(crate) mod action_scenario;
mod actions;
mod address_edit;
mod agent_exports;
mod app_state;
mod breadcrumb;
mod breadcrumb_layout;
mod callbacks;
#[cfg(test)]
mod copy_interaction_tests;
#[cfg(test)]
mod copy_selection_tests;
mod directory_loading;
mod directory_model;
#[cfg(test)]
mod directory_model_tests;
mod directory_watch_refresh;
mod file_images;
#[cfg(test)]
mod file_images_tests;
mod file_operation_coordinator;
mod file_operation_worker;
mod file_row_index;
#[cfg(test)]
mod folder_size_sort_tests;
mod icon_cache;
#[cfg(test)]
mod issue_172_tests;
#[cfg(test)]
mod large_directory_tests;
#[path = "library_loading.rs"]
mod library_loading;
mod loading_hint;
#[cfg(test)]
mod network_sort_tests;
#[cfg(test)]
mod refresh_tests;
mod rename_updates;
#[cfg(test)]
mod scrollbar_tests;
#[cfg(test)]
mod selection_scale_tests;
#[cfg(test)]
mod test_support;
mod ui_refresh;
mod webdav_ui;
mod window_sessions;

pub use agent_exports::*;
use callbacks::wire_callbacks;
use directory_loading::{DirectoryEvent, DirectoryRequest, spawn_directory_workers};
use file_operation_coordinator::{
    OperationCompletion, enqueue_operation, finish_file_operation, mark_operation_running_if_ready,
    prepare_retry, register_operation_directories, release_operation_directories, request_undo,
};
use file_operation_worker::{
    FileOperationEvent, FileOperationRequest, cleanup_record_root, spawn_file_operation_worker,
    uses_local_recycle_batch,
};
use ui_refresh::{RefreshRegions, refresh_window_regions};
use window_sessions::external_tab_insertion_slot;

pub use asterfiles_ui::*;

const WORKER_COUNT: usize = 4;
const NETWORK_WORKER_COUNT: usize = 2;
const ICON_WORKER_COUNT: usize = 2;
const SHORTCUT_QUEUE_CAPACITY: usize = 128;
const DIRECTORY_EVENT_INTERVAL: Duration = Duration::from_millis(16);
const THUMBNAIL_RETRY_DELAY: Duration = Duration::from_secs(2);
const THUMBNAIL_MAX_ATTEMPTS: u8 = 2;
const TYPE_SELECT_TIMEOUT: Duration = Duration::from_millis(1_000);

fn entry_type_icon_key(
    path: &Path,
    kind: crate::domain::EntryKind,
) -> Option<platform::windows::shell_icons::ShellTypeIconKey> {
    let network = crate::network::is_unc_path(path);
    if kind == crate::domain::EntryKind::Directory && !network {
        return None;
    }
    let path_specific_local_file = !network
        && path.extension().is_some_and(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "exe" | "lnk" | "ico" | "url"
            )
        });
    (!path_specific_local_file).then(|| {
        platform::windows::shell_icons::ShellTypeIconKey::for_entry(
            path,
            kind == crate::domain::EntryKind::Directory,
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TypeSelectContext {
    tab_id: TabId,
    request_id: RequestId,
    view_mode: ViewMode,
    page_source: PageSource,
    group_field: GroupField,
    group_direction: SortDirection,
    sort_field: SortField,
    sort_direction: SortDirection,
}

#[derive(Debug, Default)]
struct TypeSelectState {
    buffer: String,
    last_input: Option<Instant>,
    context: Option<TypeSelectContext>,
}

impl TypeSelectState {
    fn clear(&mut self) -> bool {
        let changed =
            !self.buffer.is_empty() || self.last_input.is_some() || self.context.is_some();
        self.buffer.clear();
        self.last_input = None;
        self.context = None;
        changed
    }

    fn select(
        &mut self,
        context: TypeSelectContext,
        now: Instant,
        typed: char,
        entries: &[(EntryId, String)],
        focused: Option<EntryId>,
    ) -> Option<EntryId> {
        if self.context != Some(context) {
            self.clear();
        }
        self.context = Some(context);
        if entries.is_empty() {
            return None;
        }

        let typed = typed.to_lowercase().collect::<String>();
        let expired = self
            .last_input
            .is_none_or(|last| now.saturating_duration_since(last) >= TYPE_SELECT_TIMEOUT);
        let cycle = !expired && self.buffer == typed;
        let start_after_focus = cycle || expired;
        if expired || cycle || self.buffer.is_empty() {
            self.buffer.clone_from(&typed);
        } else {
            self.buffer.push_str(&typed);
        }
        self.last_input = Some(now);

        let focused_index =
            focused.and_then(|id| entries.iter().position(|(entry_id, _)| *entry_id == id));
        let start = match focused_index {
            Some(index) if start_after_focus => (index + 1) % entries.len(),
            Some(index) => index,
            None => 0,
        };
        (0..entries.len())
            .map(|offset| (start + offset) % entries.len())
            .find_map(|index| {
                entries[index]
                    .1
                    .to_lowercase()
                    .starts_with(&self.buffer)
                    .then_some(entries[index].0)
            })
    }

    fn is_active(&self) -> bool {
        !self.buffer.is_empty()
    }
}

type SharedSessions = Arc<Mutex<AppState>>;

#[derive(Clone)]
struct WorkerSenders {
    directory: mpsc::Sender<DirectoryRequest>,
    network_directory: mpsc::SyncSender<DirectoryRequest>,
    network_discovery: mpsc::SyncSender<NetworkDiscoveryRequest>,
    operation: mpsc::Sender<FileOperationRequest>,
    clipboard: mpsc::Sender<ClipboardRequest>,
    shell_menu: platform::windows::context_menu::ShellMenuWorker,
    everything: mpsc::Sender<EverythingRequest>,
    icon: mpsc::Sender<IconRequest>,
    thumbnail: ThumbnailScheduler,
    shortcut: mpsc::SyncSender<ShortcutRequest>,
    network_login: slint::Weak<NetworkLoginWindow>,
    network_login_state: Arc<Mutex<NetworkLoginCoordinator>>,
    network_location_rename: slint::Weak<NetworkLocationRenameWindow>,
    webdav_location: slint::Weak<WebDavLocationWindow>,
    operation_window: slint::Weak<OperationWindow>,
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
    sessions: WindowSessions,
    _native_drop_timer: slint::Timer,
    _rectangle_selection_timer: Rc<slint::Timer>,
    _quick_menu: SharedQuickMenu,
    _quick_submenu_timer: Rc<slint::Timer>,
    _quick_menu_prewarm_timer: slint::Timer,
    quick_menu_popup: QuickMenuPopupRuntime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PopupPresentation {
    Hidden,
    Cloaked,
    ShownCloaked,
    Presented,
}
struct QuickMenuPopupRuntime {
    root: QuickMenuWindow,
    branches: Vec<QuickSubmenuPopupRuntime>,
    session: crate::quick_menu_popup::QuickMenuPopupSession,
    owner_hwnd: isize,
    client_anchor: crate::quick_menu_popup::PhysicalPoint,
    work_area: crate::quick_menu_popup::PhysicalRect,
    root_rect: Option<crate::quick_menu_popup::PhysicalRect>,
    next_generation: u64,
    next_branch: u64,
    shown_once: bool,
    presentation: PopupPresentation,
    cloak_generation: Option<u64>,
}

struct QuickSubmenuPopupRuntime {
    parent_row_index: i32,
    window: QuickSubmenuWindow,
    event: Option<crate::quick_menu_popup::MenuEventIdentity>,
    rows: Vec<ContextCommandRow>,
    active_index: i32,
    anchor_y: f32,
    presentation: PopupPresentation,
    cloak_event: Option<crate::quick_menu_popup::MenuEventIdentity>,
}

thread_local! {
    static WINDOW_RUNTIMES: RefCell<HashMap<WindowId, WindowRuntime>> = RefCell::new(HashMap::new());
    static GRID_ENTRY_POSITIONS: RefCell<GridEntryPositions> = RefCell::new(HashMap::new());
    static GRID_THUMBNAIL_RESIDENTS: RefCell<HashMap<WindowId, HashSet<EntryId>>> = RefCell::new(HashMap::new());
}

type GridEntryPositions = HashMap<WindowId, HashMap<EntryId, (usize, usize)>>;

fn directory_rows(ui: &AppWindow) -> Option<Rc<directory_model::DirectoryRows>> {
    if let Some(rows) = ui
        .get_files()
        .as_any()
        .downcast_ref::<directory_model::DirectoryRows>()
    {
        return Some(rows.shared());
    }
    ui.get_grid_rows()
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>()
        .map(directory_model::DirectoryGrid::rows)
}

fn install_directory_models(
    ui: &AppWindow,
    app: &AppState,
    tab: &TabSession,
    mode: ViewMode,
    preserve: bool,
) {
    let entries = if tab.has_failed_location() && !tab.retains_directory_view() {
        Arc::new(Vec::new())
    } else {
        tab.visible_entries_snapshot()
    };
    let groups = directory_group_projections(app, tab, &entries);
    let rows = directory_model::DirectoryRows::new(
        entries,
        groups.clone(),
        tab,
        app,
        file_thumbnail_request_px(mode, ui.window().scale_factor()),
        file_row_height(mode),
    );
    let rows = if preserve && let Some(current) = directory_rows(ui) {
        current.replace_from(&rows);
        current
    } else {
        rows
    };
    if mode.uses_grid_layout() {
        if !preserve || ui.get_files().row_count() != 0 {
            ui.set_files(ModelRc::new(VecModel::default()));
        }
    } else if !preserve || !std::ptr::eq(ui.get_files().as_any(), rows.as_any()) {
        ui.set_files(ModelRc::from(rows.clone()));
    }
    let columns = grid_column_count_for_view(ui, mode);
    ui.set_grid_column_count(columns as i32);
    if mode.uses_grid_layout() {
        let next =
            directory_model::DirectoryGrid::new(rows, &groups, columns, file_row_height(mode));
        let current = ui.get_grid_rows();
        if preserve
            && let Some(grid) = current
                .as_any()
                .downcast_ref::<directory_model::DirectoryGrid>()
        {
            grid.replace_from(&next);
        } else {
            ui.set_grid_rows(ModelRc::from(next));
        }
    } else if !preserve || ui.get_grid_rows().row_count() != 0 {
        ui.set_grid_rows(ModelRc::new(VecModel::default()));
    }
    let grouped = mode.uses_grid_layout() && groups.iter().any(|group| group.header_visible);
    rebuild_grouped_grid_layout(ui, grouped);
    if let Some(window_id) = app.window_for_tab(tab.id) {
        rebuild_grid_entry_positions(ui, window_id);
    }
}

fn rebuild_grouped_grid_layout(ui: &AppWindow, enabled: bool) {
    if !enabled && ui.get_grouped_grid_layout().row_count() == 0 {
        ui.set_grouped_grid_enabled(false);
        ui.set_grouped_grid_extent(0.0);
        return;
    }

    let grid_rows = ui.get_grid_rows();
    if let Some(grid) = grid_rows
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>()
    {
        ui.set_grouped_grid_layout(ModelRc::from(grid.positioned()));
        ui.set_grouped_grid_extent(grid.extent());
        ui.set_grouped_grid_enabled(enabled);
        refresh_grouped_grid_viewport(ui);
        return;
    }

    let mut extent = 0.0;
    let layout = if enabled {
        let entry_height = file_row_height(projected_view_mode(ui));
        ui.get_grid_rows()
            .iter()
            .map(|row| {
                let height = if row.group_header {
                    group_header_height_for_detail(row.group_detail.as_str()) as f32
                } else {
                    entry_height
                };
                let placed = PositionedGridRow {
                    row,
                    offset: extent,
                    extent: height,
                };
                extent += height;
                placed
            })
            .collect()
    } else {
        Vec::new()
    };
    ui.set_grouped_grid_layout(ModelRc::new(VecModel::from(layout)));
    ui.set_grouped_grid_extent(extent);
    ui.set_grouped_grid_enabled(enabled);
    ui.set_grouped_grid_visible_rows(ModelRc::new(VecModel::default()));
    refresh_grouped_grid_viewport(ui);
}

fn refresh_grouped_grid_viewport(ui: &AppWindow) {
    if !ui.get_grouped_grid_enabled() {
        ui.set_grouped_grid_visible_rows(ModelRc::new(VecModel::default()));
        return;
    }
    let height = ui.get_file_viewport_height().max(0.0);
    let maximum = (ui.get_grouped_grid_extent() - height).max(0.0);
    let viewport = ui.get_file_viewport_y().clamp(-maximum, 0.0);
    if ui.get_file_viewport_y() != viewport {
        ui.set_file_viewport_y(viewport);
    }
    let layout = ui.get_grouped_grid_layout();
    let overscan = file_row_height(projected_view_mode(ui)) * 2.0;
    let start = (-viewport - overscan).max(0.0);
    let end = -viewport + height + overscan;
    let mut low = 0;
    let mut high = layout.row_count();
    while low < high {
        let middle = low + (high - low) / 2;
        let row = layout.row_data(middle).expect("layout row exists");
        if row.offset + row.extent <= start {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let rows = (low..layout.row_count())
        .filter_map(|index| layout.row_data(index))
        .take_while(|row| row.offset < end)
        .collect::<Vec<_>>();
    let current = ui.get_grouped_grid_visible_rows();
    if current.row_count() == rows.len()
        && current.iter().zip(&rows).all(|(old, new)| {
            old.offset == new.offset
                && old.extent == new.extent
                && old.row.group_label == new.row.group_label
                && old.row.group_count == new.row.group_count
                && std::ptr::eq(old.row.entries.as_any(), new.row.entries.as_any())
        })
    {
        return;
    }
    // Reuse entry models so thumbnail and selection updates cannot alter geometry.
    if let Some(model) = current
        .as_any()
        .downcast_ref::<VecModel<PositionedGridRow>>()
    {
        while model.row_count() > rows.len() {
            model.remove(model.row_count() - 1);
        }
        for (index, row) in rows.into_iter().enumerate() {
            if index < model.row_count() {
                model.set_row_data(index, row);
            } else {
                model.push(row);
            }
        }
    } else {
        ui.set_grouped_grid_visible_rows(ModelRc::new(VecModel::from(rows)));
    }
}

fn rebuild_grid_entry_positions(ui: &AppWindow, window_id: WindowId) {
    let rows = ui.get_grid_rows();
    if rows.as_any().is::<directory_model::DirectoryGrid>() {
        GRID_ENTRY_POSITIONS.with_borrow_mut(|windows| {
            windows.remove(&window_id);
        });
        return;
    }

    let positions = ui
        .get_grid_rows()
        .iter()
        .enumerate()
        .flat_map(|(row_index, row)| {
            row.entries
                .iter()
                .enumerate()
                .filter_map(move |(entry_index, entry)| {
                    (entry.id > 0).then_some((EntryId(entry.id as u32), (row_index, entry_index)))
                })
                .collect::<Vec<_>>()
        })
        .collect::<HashMap<_, _>>();
    GRID_ENTRY_POSITIONS.with_borrow_mut(|by_window| {
        by_window.insert(window_id, positions);
    });
}

fn update_grid_thumbnail_residents(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    wanted: &HashSet<EntryId>,
) {
    let removed = GRID_THUMBNAIL_RESIDENTS.with_borrow_mut(|by_window| {
        let resident = by_window.entry(window_id).or_default();
        let removed = resident.difference(wanted).copied().collect::<Vec<_>>();
        *resident = wanted.clone();
        removed
    });
    if let Some(rows) = directory_rows(ui) {
        for id in removed {
            rows.evict_image(id);
        }
        return;
    }
    let list = ui.get_files();
    if !removed.is_empty()
        && let Some(rows) = list.as_any().downcast_ref::<VecModel<FileRow>>()
    {
        for (_, index) in locate_list_rows(rows, state, window_id, removed.iter().copied()) {
            if let Some(mut row) = rows.row_data(index)
                && matches!(row.thumbnail_state, 2 | 4)
            {
                row.icon = Image::default();
                row.thumbnail_state = 1;
                rows.set_row_data(index, row);
            }
        }
    }
    let grid = ui.get_grid_rows();
    for entry_id in removed {
        let Some((row_index, entry_index)) = GRID_ENTRY_POSITIONS.with_borrow(|by_window| {
            by_window
                .get(&window_id)
                .and_then(|positions| positions.get(&entry_id).copied())
        }) else {
            continue;
        };
        let Some(row) = grid.row_data(row_index) else {
            continue;
        };
        let Some(entries) = row.entries.as_any().downcast_ref::<VecModel<FileRow>>() else {
            continue;
        };
        let Some(mut entry) = entries.row_data(entry_index) else {
            continue;
        };
        if entry.id == entry_id.0 as i32 {
            entry.icon = Image::default();
            if matches!(entry.thumbnail_state, 2 | 4) {
                entry.thumbnail_state = 1;
            }
            entries.set_row_data(entry_index, entry);
        }
    }
}

fn locate_list_rows(
    rows: &VecModel<FileRow>,
    state: &SharedSessions,
    window_id: WindowId,
    ids: impl IntoIterator<Item = EntryId>,
) -> Vec<(EntryId, usize)> {
    let app = state.lock().expect("app state mutex is not poisoned");
    app.window(window_id)
        .and_then(|window| window.tabs.get(&window.active_tab))
        .map(|tab| file_row_index::locate(rows, tab, ids))
        .unwrap_or_default()
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
        .and_then(|path| app.image_cache.ordinary.get(path))
        .map(|icon| (icon.width, icon.height, icon.pixels.to_vec()));
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

fn grid_thumbnail_request_px(view_mode: ViewMode, scale: f32) -> u32 {
    const STANDARD_SIZES: [u32; 12] = [16, 24, 32, 48, 64, 96, 128, 192, 256, 384, 512, 1024];
    let requested = (file_layout_geometry(view_mode).icon_size as f32 * scale)
        .round()
        .clamp(16.0, 1024.0) as u32;
    STANDARD_SIZES
        .into_iter()
        .find(|size| *size >= requested)
        .unwrap_or(1024)
}

fn grid_thumbnail_request_rows(
    row_extents: &[f32],
    viewport_y: f32,
    visible_height: f32,
    prefetch_extent: f32,
) -> Vec<usize> {
    let visible_top = (-viewport_y).max(0.0);
    let request_top = (visible_top - prefetch_extent).max(0.0);
    let request_bottom = visible_top + visible_height.max(0.0) + prefetch_extent;
    let mut row_top = 0.0;
    row_extents
        .iter()
        .enumerate()
        .filter_map(|(index, extent)| {
            let row_bottom = row_top + extent.max(0.0);
            let visible = row_bottom >= request_top && row_top <= request_bottom;
            row_top = row_bottom;
            visible.then_some(index)
        })
        .collect()
}

#[derive(Clone, PartialEq, Eq)]
enum ActivateTarget {
    Path(PathBuf),
    WebDav(String),
}

impl From<&FileEntry> for ActivateTarget {
    fn from(entry: &FileEntry) -> Self {
        if let Some(item) = &entry.webdav {
            Self::WebDav(item.path.clone())
        } else {
            Self::Path(entry.path())
        }
    }
}

fn ordinary_icon_request(
    app: &mut AppState,
    tab_id: TabId,
    request_id: RequestId,
    entry: &FileEntry,
) -> Option<IconRequest> {
    file_images::shared_icon_request(app, tab_id, request_id, entry, 0)
}

fn list_icon_plan(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
) -> (HashSet<EntryId>, Vec<IconRequest>) {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let Some(tab) = app
        .window(window_id)
        .and_then(|window| window.tabs.get(&window.active_tab))
    else {
        return (HashSet::new(), Vec::new());
    };
    let Some(view_mode) = app.view_mode_for_tab(tab.id) else {
        return (HashSet::new(), Vec::new());
    };
    if tab.kind != TabKind::Files || view_mode.uses_grid_layout() {
        return (HashSet::new(), Vec::new());
    }
    let row_height = file_layout_geometry(view_mode).row_height;
    let height = ui.get_file_viewport_height().max(row_height);
    let rows = ui.get_files();
    let tab_id = tab.id;
    let request_id = tab.latest_request();
    let visible_top = (-ui.get_file_viewport_y()).max(0.0);
    let request_top = (visible_top - height).max(0.0);
    let request_bottom = visible_top + height * 2.0;
    let mut entries = Vec::new();
    let mut add = |row: FileRow| {
        if row.loaded
            && !row.group_header
            && row.id > 0
            && let Some(entry) = tab.visible_entry(EntryId(row.id as u32))
        {
            entries.push(entry.clone());
        }
    };
    if let Some(model) = rows
        .as_any()
        .downcast_ref::<directory_model::DirectoryRows>()
    {
        for index in model.rows_between(request_top, request_bottom) {
            if let Some(id) = model.entry_id(index)
                && let Some(entry) = tab.visible_entry(id)
            {
                entries.push(entry.clone());
            }
        }
    } else {
        let indexed = rows
            .as_any()
            .downcast_ref::<VecModel<FileRow>>()
            .and_then(|model| {
                file_row_index::rows_between(model, row_height, request_top, request_bottom)
                    .map(|range| (model, range))
            });
        if let Some((model, range)) = indexed {
            range
                .filter_map(|index| model.row_data(index))
                .for_each(add);
        } else {
            let mut row_top = 0.0;
            for row in rows.iter() {
                if row_top > request_bottom {
                    break;
                }
                let row_bottom = row_top
                    + if row.group_header {
                        group_header_height_for_detail(row.group_detail.as_str()) as f32
                    } else {
                        row_height
                    };
                if row_bottom >= request_top {
                    add(row);
                }
                row_top = row_bottom;
            }
        }
    }
    let residents = entries.iter().map(|entry| entry.id).collect();
    let requested_px = file_thumbnail_request_px(view_mode, ui.window().scale_factor());
    let requests = entries
        .iter()
        .filter_map(|entry| {
            if let Some(px) = requested_px {
                thumbnail_request(&mut app, tab_id, request_id, entry, px)
            } else {
                ordinary_icon_request(&mut app, tab_id, request_id, entry)
            }
        })
        .collect();
    (residents, requests)
}

fn grid_thumbnail_plan(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
) -> (TabId, HashSet<EntryId>, Vec<IconRequest>) {
    let visible_height = ui.window().size().height as f32 / ui.window().scale_factor()
        - ui.get_file_list_top()
        - 30.0;
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let Some(window) = app.window(window_id) else {
        return (TabId(0), HashSet::new(), Vec::new());
    };
    let Some(tab) = window.tabs.get(&window.active_tab) else {
        return (TabId(0), HashSet::new(), Vec::new());
    };
    let Some(view_mode) = app.view_mode_for_tab(tab.id) else {
        return (TabId(0), HashSet::new(), Vec::new());
    };
    if tab.kind != TabKind::Files || !view_mode.uses_grid_layout() {
        return (tab.id, HashSet::new(), Vec::new());
    }
    let requested_px = grid_thumbnail_request_px(view_mode, ui.window().scale_factor());

    let row_height = file_layout_geometry(view_mode).row_height;
    let grid_rows = ui.get_grid_rows();
    let request_rows: Vec<usize> = if let Some(grid) = grid_rows
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>(
    ) {
        let top = (-ui.get_file_viewport_y()).max(0.0);
        grid.rows_between(
            (top - row_height * 2.0).max(0.0),
            top + visible_height + row_height * 2.0,
        )
        .collect()
    } else {
        grid_thumbnail_request_rows(
            &grid_rows
                .iter()
                .map(|row| {
                    if row.group_header {
                        group_header_height_for_detail(row.group_detail.as_str()) as f32
                    } else {
                        row_height
                    }
                })
                .collect::<Vec<_>>(),
            ui.get_file_viewport_y(),
            visible_height,
            row_height * 2.0,
        )
    };
    let entry_ids = request_rows
        .into_iter()
        .flat_map(|row_index| {
            grid_rows.row_data(row_index).map_or_else(Vec::new, |row| {
                row.entries
                    .iter()
                    .filter_map(|entry| (entry.id > 0).then_some(EntryId(entry.id as u32)))
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();
    let tab_id = tab.id;
    let request_id = tab.latest_request();
    let entries = entry_ids
        .into_iter()
        .filter_map(|entry_id| tab.visible_entry(entry_id).cloned())
        .collect::<Vec<_>>();
    let resident_ids = entries.iter().map(|entry| entry.id).collect::<HashSet<_>>();
    let mut requests = Vec::new();
    for entry in entries {
        if let Some(request) = thumbnail_request(&mut app, tab_id, request_id, &entry, requested_px)
        {
            requests.push(request);
        }
    }
    (tab_id, resident_ids, requests)
}

fn file_thumbnail_request_px(view_mode: ViewMode, scale: f32) -> Option<u32> {
    (view_mode.uses_grid_layout() || matches!(view_mode, ViewMode::Details | ViewMode::Content))
        .then(|| grid_thumbnail_request_px(view_mode, scale))
}

fn thumbnail_request(
    app: &mut AppState,
    tab_id: TabId,
    request_id: RequestId,
    entry: &FileEntry,
    requested_px: u32,
) -> Option<IconRequest> {
    let entry_path = entry.path();
    if crate::network::is_unc_path(&entry_path) {
        return ordinary_icon_request(app, tab_id, request_id, entry);
    }
    let key = (entry_path.clone(), requested_px);
    if app.image_cache.thumbnails.contains_key(&key)
        || app.image_cache.grid_icons.contains_key(&key)
        || !thumbnail_retry_due(
            app.thumbnail_failures
                .get(&(tab_id, request_id, entry_path.clone(), requested_px)),
            Instant::now(),
        )
    {
        return None;
    }
    if let Some(type_key) = entry_type_icon_key(&entry_path, entry.kind)
        && (entry.size_bytes == Some(0) || app.thumbnail_types.get(&type_key) != Some(&true))
    {
        return file_images::shared_icon_request(app, tab_id, request_id, entry, requested_px);
    }
    Some(IconRequest {
        tab_id,
        request_id,
        target: IconTarget::Entry(entry.id),
        path: entry_path,
        thumbnail: true,
        requested_px,
        type_key: None,
    })
}

fn thumbnail_retry_due(failure: Option<&(u8, Instant)>, now: Instant) -> bool {
    failure.is_none_or(|(attempts, failed_at)| {
        *attempts < THUMBNAIL_MAX_ATTEMPTS
            && now.saturating_duration_since(*failed_at) >= THUMBNAIL_RETRY_DELAY
    })
}

fn request_visible_file_images(
    ui: &AppWindow,
    state: &SharedSessions,
    window_id: WindowId,
    scheduler: &ThumbnailScheduler,
) {
    let remote = state.lock().ok().is_some_and(|app| {
        app.window(window_id)
            .and_then(|window| app.tab(window.active_tab))
            .is_some_and(|tab| {
                matches!(tab.visible_location(), Some(NavigationLocation::WebDav(_)))
            })
    });
    if remote {
        return;
    }
    let (tab_id, mut resident_ids, mut requests) = grid_thumbnail_plan(ui, state, window_id);
    let (list_residents, list_requests) = list_icon_plan(ui, state, window_id);
    resident_ids.extend(list_residents);
    requests.extend(list_requests);
    let entering = GRID_THUMBNAIL_RESIDENTS.with_borrow(|windows| {
        windows.get(&window_id).map_or_else(
            || resident_ids.clone(),
            |previous| resident_ids.difference(previous).copied().collect(),
        )
    });
    update_grid_thumbnail_residents(ui, state, window_id, &resident_ids);
    if tab_id.0 != 0 {
        file_images::patch_images(ui, state, tab_id, &entering);
        scheduler.replace(tab_id, requests);
    }
}

fn request_completed_file_images(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    scheduler: &ThumbnailScheduler,
) {
    let window_id = state.lock().ok().and_then(|app| app.window_for_tab(tab_id));
    if let Some(window_id) = window_id {
        // Final projection and focus can change both entry identities and the visible range.
        request_visible_file_images(ui, state, window_id, scheduler);
        defer_visible_file_images(state.clone(), tab_id, scheduler.clone());
    }
}

fn defer_visible_file_images(state: SharedSessions, tab_id: TabId, scheduler: ThumbnailScheduler) {
    slint::Timer::single_shot(DIRECTORY_EVENT_INTERVAL, move || {
        let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id)) else {
            return;
        };
        let Some(ui) = window_ui(window_id) else {
            return;
        };
        request_visible_file_images(&ui, &state, window_id, &scheduler);
    });
}

fn defer_thumbnail_retry(state: SharedSessions, tab_id: TabId, scheduler: ThumbnailScheduler) {
    slint::Timer::single_shot(THUMBNAIL_RETRY_DELAY, move || {
        let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id)) else {
            return;
        };
        let Some(ui) = window_ui(window_id) else {
            return;
        };
        request_visible_file_images(&ui, &state, window_id, &scheduler);
    });
}

fn refresh_all_windows(state: &SharedSessions, regions: RefreshRegions) {
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
            refresh_window_regions(&ui, state, window_id, regions);
        }
    }
}

fn refresh_view_windows(state: &WindowSessions, owner: Option<&AppWindow>) {
    let targets = {
        let Ok(app) = state.lock() else { return };
        let source = app.active();
        app.windows
            .iter()
            .filter_map(|(window_id, window)| {
                let tab = window.tabs.get(&window.active_tab)?;
                let affected = if source.page_source == PageSource::Search {
                    tab.page_source == PageSource::Search
                } else if let Some(path) = source.visible_path() {
                    tab.page_source == PageSource::Directory && tab.visible_path() == Some(path)
                } else {
                    tab.id == source.id
                };
                affected.then_some(*window_id)
            })
            .collect::<Vec<_>>()
    };
    for window_id in targets {
        let ui = if window_id == state.window_id {
            owner
                .map(|ui| ui.clone_strong())
                .or_else(|| window_ui(window_id))
        } else {
            window_ui(window_id)
        };
        if let Some(ui) = ui {
            refresh_window_regions(&ui, state, window_id, RefreshRegions::CONTENT);
        }
    }
}

fn refresh_operation_badges(state: &SharedSessions) {
    let rows = {
        let Ok(app) = state.lock() else {
            return;
        };
        operation_rows(&app)
    };
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .values()
            .map(|runtime| runtime.ui.clone_strong())
            .collect::<Vec<_>>()
    });
    for ui in windows {
        ui.set_operations(ModelRc::new(VecModel::from(rows.clone())));
    }
}

fn refresh_tab_window(state: &SharedSessions, tab_id: TabId) {
    let target = state.lock().ok().and_then(|app| {
        let window_id = app.window_for_tab(tab_id)?;
        Some((window_id, app.window(window_id)?.active_tab == tab_id))
    });
    if let Some((window_id, active)) = target
        && let Some(ui) = window_ui(window_id)
    {
        let regions = if active {
            RefreshRegions::CONTENT
        } else {
            RefreshRegions::TABS
        };
        refresh_window_regions(&ui, state, window_id, regions);
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
        (platform::windows::tab_insertion_indicator::INDICATOR_WIDTH * scale)
            .round()
            .max(1.0) as i32,
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
        if let Some(mut runtime) = runtimes.remove(&window_id) {
            runtime.quick_menu_popup.session.invalidate_owner(window_id);
            close_quick_submenu_windows(&mut runtime.quick_menu_popup);
            let root_hwnd = component_window_handle(&runtime.quick_menu_popup.root);
            let _ = runtime.quick_menu_popup.root.hide();
            let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
            platform::windows::drag_drop::revoke(native_window_handle(&runtime.ui));
        }
    });
}

fn clear_window_runtimes() {
    platform::windows::drag_drop::begin_shutdown_current();
    platform::windows::tab_insertion_indicator::destroy();
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .values()
            .map(|runtime| runtime.ui.clone_strong())
            .collect::<Vec<_>>()
    });
    for window in windows {
        window.invoke_dismiss_context_menu();
    }
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        for (window_id, runtime) in runtimes.iter_mut() {
            runtime
                .quick_menu_popup
                .session
                .invalidate_owner(*window_id);
            close_quick_submenu_windows(&mut runtime.quick_menu_popup);
            let root_hwnd = component_window_handle(&runtime.quick_menu_popup.root);
            let _ = runtime.quick_menu_popup.root.hide();
            let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
        }
        runtimes.clear();
    });
}

fn hide_all_app_windows() {
    platform::windows::drag_drop::begin_shutdown_current();
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
        ui.invoke_open_operation_window();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct WindowId(pub(crate) u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingFocusAction {
    Select,
    Reveal { window_id: WindowId },
    Rename { window_id: WindowId },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingFocus {
    directory: PathBuf,
    request_id: Option<RequestId>,
    paths: Vec<PathBuf>,
    action: PendingFocusAction,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingRenameUi {
    tab_id: TabId,
    request_id: RequestId,
    entry_id: EntryId,
    input: String,
    selection_end: usize,
}

#[derive(Debug, Clone)]
struct PendingShellCreate {
    window_id: WindowId,
    tab_id: TabId,
    directory: PathBuf,
    baseline: HashSet<PathBuf>,
    started_at: Instant,
}

#[derive(Debug)]
struct WindowState {
    tabs: HashMap<TabId, TabSession>,
    tab_order: Vec<TabId>,
    active_tab: TabId,
    closed_tabs: VecDeque<NavigationLocation>,
    placement: session_store::WindowPlacement,
}

const TAB_DRAG_THRESHOLD: f32 = 6.0;
const COLUMN_DRAG_THRESHOLD: f32 = TAB_DRAG_THRESHOLD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnDragPhase {
    Pressed,
    Dragging { insertion_slot: Option<usize> },
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnDragSession {
    window_id: WindowId,
    source: PageSource,
    kind: u8,
    press_x: f32,
    press_y: f32,
    phase: ColumnDragPhase,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ColumnHeaderGeometry {
    x: f32,
    y: f32,
    width: f32,
    viewport_x: f32,
}

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
    source_closed_tabs: VecDeque<NavigationLocation>,
    source_active_tab: TabId,
    source_window_closed: bool,
    restart: Option<DetachedTabRestart>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlWAction {
    CloseActiveTab,
    RequestWindowClose,
}

fn ctrl_w_action(can_close_tab: bool) -> CtrlWAction {
    if can_close_tab {
        CtrlWAction::CloseActiveTab
    } else {
        CtrlWAction::RequestWindowClose
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnmatchedTabDropAction {
    MoveSourceWindow,
    DetachToNewWindow,
}

fn unmatched_tab_drop_action(
    source_has_single_file_tab: bool,
    target_window_hit: bool,
) -> Option<UnmatchedTabDropAction> {
    if source_has_single_file_tab {
        Some(UnmatchedTabDropAction::MoveSourceWindow)
    } else if !target_window_hit {
        Some(UnmatchedTabDropAction::DetachToNewWindow)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DriveCapacityStatus {
    Loading,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone)]
struct DriveCapacityState {
    status: DriveCapacityStatus,
    total: u64,
    available: u64,
    last_updated: Option<Instant>,
    request_generation: u64,
}

impl DriveCapacityState {
    fn loading(request_generation: u64) -> Self {
        Self {
            status: DriveCapacityStatus::Loading,
            total: 0,
            available: 0,
            last_updated: None,
            request_generation,
        }
    }

    fn apply(&mut self, result: io::Result<platform::windows::drives::DriveCapacity>) {
        self.last_updated = Some(Instant::now());
        match result {
            Ok(capacity) => {
                self.status = DriveCapacityStatus::Ready;
                self.total = capacity.total;
                self.available = capacity.available;
            }
            Err(_) => {
                self.status = DriveCapacityStatus::Unavailable;
                self.total = 0;
                self.available = 0;
            }
        }
    }
}
#[derive(Debug, Clone, Default)]
struct HomePageState {
    selected: Option<String>,
    quick_access_expanded: bool,
    drives_expanded: bool,
    network_locations_expanded: bool,
}

impl HomePageState {
    fn expanded() -> Self {
        Self {
            selected: None,
            quick_access_expanded: true,
            drives_expanded: true,
            network_locations_expanded: true,
        }
    }
}

struct AppState {
    windows: HashMap<WindowId, WindowState>,
    active_window: WindowId,
    #[cfg_attr(not(test), allow(dead_code))]
    next_window_id: u32,
    next_tab_id: u32,
    language: Language,
    theme_mode: session_store::ThemeMode,
    file_visibility: crate::domain::FileVisibility,
    file_list_quick_search: bool,
    new_tab_opens_home: bool,
    quick_menu_backdrop: bool,
    quick_menu_backdrop_opacity: u8,
    sidebar_visibility: session_store::SidebarVisibility,
    system_dark_theme: bool,
    icons: HashMap<(TabId, RequestId, EntryId), platform::windows::shell_icons::ShellIconRgba>,
    image_cache: icon_cache::IconCache,
    type_icon_requests: HashSet<(platform::windows::shell_icons::ShellTypeIconKey, u32)>,
    thumbnail_types: HashMap<platform::windows::shell_icons::ShellTypeIconKey, bool>,
    ordinary_icon_requests: HashSet<(TabId, RequestId, PathBuf)>,
    sidebar_icons: HashMap<PathBuf, platform::windows::shell_icons::ShellIconRgba>,
    libraries: Vec<platform::windows::libraries::WindowsLibrary>,
    library_failures: Vec<platform::windows::libraries::LibraryFailure>,
    library_icons: HashMap<std::ffi::OsString, platform::windows::shell_icons::ShellIconRgba>,
    library_generation: u64,
    network_location_generation: u64,
    thumbnail_failures: HashMap<(TabId, RequestId, PathBuf, u32), (u8, Instant)>,
    shortcut_requests: HashSet<ShortcutRequest>,
    shortcut_completed: HashSet<ShortcutRequest>,
    sidebar: Vec<KnownLocation>,

    drive_capacity: HashMap<PathBuf, DriveCapacityState>,
    drive_generation: u64,
    quick_access_generation: u64,
    drive_list_generation: u64,
    quick_access_pending: HashSet<PathBuf>,
    home_pages: HashMap<TabId, HomePageState>,

    network_locations: Vec<NetworkLocation>,
    imported_network_locations: Vec<NetworkLocation>,
    network_discovery: HashMap<WindowId, DiscoveryCoordinator>,
    network_discovery_errors: HashMap<WindowId, String>,
    default_directory_view: DirectoryViewPreference,
    directory_views: HashMap<PathBuf, DirectoryViewPreference>,
    directory_view_lru: VecDeque<PathBuf>,
    search_view: SearchViewPreference,
    operations: OperationManager,
    undo_history: UndoHistory,
    operation_errors: Vec<String>,
    operation_notice: Option<(u64, String)>,
    operation_notice_generation: u64,
    rename_targets: HashMap<WindowId, actions::EntryTarget>,
    focus_after_refresh: HashMap<TabId, PendingFocus>,
    pending_rename_ui: HashMap<WindowId, PendingRenameUi>,
    active_operation_directories: HashMap<PathBuf, usize>,
    deferred_watch_directories: HashSet<PathBuf>,
    pending_shell_creates: HashMap<WindowId, PendingShellCreate>,
    pending_permanent_delete: Option<(TabId, Vec<OperationItem>)>,
    pending_imported_network_location_remove: Option<NetworkLocation>,
    exit_after_cancel: bool,
    clipboard_has_files: bool,
    cut_paths: Vec<PathBuf>,
    clipboard_sequence: Option<u32>,
    conflict_responses:
        HashMap<OperationId, mpsc::Sender<crate::domain::file_operations::ConflictDecision>>,

    everything: app_state::EverythingState,
    pending_right_drops: HashMap<WindowId, (TabId, platform::windows::drag_drop::DropIntent)>,
    integration: app_state::IntegrationState,
    tab_drag: Option<TabDragSession>,
    column_drag: Option<ColumnDragSession>,
}

impl AppState {
    fn directory_preference(&self, path: &Path) -> DirectoryViewPreference {
        self.directory_views
            .get(path)
            .copied()
            .unwrap_or(self.default_directory_view)
    }

    fn view_mode_for_tab(&self, tab_id: TabId) -> Option<ViewMode> {
        let tab = self.tab(tab_id)?;
        Some(if tab.page_source == PageSource::Search {
            self.search_view.view_mode
        } else {
            tab.visible_path()
                .map(|path| self.directory_preference(path).view_mode)
                .unwrap_or(self.default_directory_view.view_mode)
        })
    }

    fn active_view_mode(&self) -> ViewMode {
        self.view_mode_for_tab(self.active_window_state().active_tab)
            .unwrap_or(self.default_directory_view.view_mode)
    }

    fn active_column_layout(&self) -> ColumnLayout {
        if self.active().page_source == PageSource::Search {
            self.search_view.columns
        } else {
            self.active()
                .visible_path()
                .map(|path| self.directory_preference(path).columns)
                .unwrap_or(self.default_directory_view.columns)
        }
    }

    fn update_directory_preference(
        &mut self,
        path: PathBuf,
        update: impl FnOnce(&mut DirectoryViewPreference),
    ) {
        let preference = self
            .directory_views
            .entry(path.clone())
            .or_insert(self.default_directory_view);
        update(preference);
        self.directory_view_lru
            .retain(|candidate| candidate != &path);
        self.directory_view_lru.push_back(path);
        while self.directory_view_lru.len() > MAX_DIRECTORY_VIEW_PREFERENCES {
            if let Some(evicted) = self.directory_view_lru.pop_front() {
                self.directory_views.remove(&evicted);
            }
        }
    }

    fn update_active_column_layout(&mut self, update: impl FnOnce(&mut ColumnLayout)) {
        if self.active().page_source == PageSource::Search {
            update(&mut self.search_view.columns);
        } else if let Some(path) = self.active().visible_path().map(Path::to_path_buf) {
            self.update_directory_preference(path, |preference| update(&mut preference.columns));
        } else {
            update(&mut self.default_directory_view.columns);
        }
    }

    #[cfg(test)]
    fn new_for_test(
        initial_locations: Vec<PathBuf>,
        active_index: usize,
        _column_order: [u8; 4],
    ) -> Self {
        let initial_locations = initial_locations
            .into_iter()
            .map(NavigationLocation::Directory)
            .collect();
        let mut default_directory_view = DirectoryViewPreference::default();
        default_directory_view.columns.order = [
            ColumnKind::Name,
            ColumnKind::Kind,
            ColumnKind::Size,
            ColumnKind::Modified,
            ColumnKind::Created,
        ];
        Self::new(
            initial_locations,
            active_index,
            default_directory_view,
            SearchViewPreference::default(),
            HashMap::new(),
            crate::domain::EverythingConfig::default(),
            session_store::ThemeMode::System,
            Language::Chinese,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        initial_locations: Vec<NavigationLocation>,
        active_index: usize,
        default_directory_view: DirectoryViewPreference,
        search_view: SearchViewPreference,
        directory_views: HashMap<PathBuf, DirectoryViewPreference>,
        everything_config: crate::domain::EverythingConfig,
        theme_mode: session_store::ThemeMode,
        language: Language,
        system_dark_theme: bool,
    ) -> Self {
        let initial_locations = if initial_locations.is_empty() {
            vec![NavigationLocation::Home]
        } else {
            initial_locations
        };
        let active_window = WindowId(1);
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        let mut next_tab_id = 1;
        for location in initial_locations {
            let id = TabId(next_tab_id);
            next_tab_id += 1;
            let mut tab = TabSession::new_at(id, location.clone());
            let preference = directory_views
                .get(location.directory_path().unwrap_or(Path::new("")))
                .copied()
                .unwrap_or(default_directory_view);
            tab.sort_field = preference.sort_field;
            tab.sort_direction = preference.sort_direction;
            tab.search_sort_field = search_view.sort_field;
            tab.search_sort_direction = search_view.sort_direction;
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
            file_list_quick_search: false,
            new_tab_opens_home: true,
            quick_menu_backdrop: true,
            quick_menu_backdrop_opacity: session_store::DEFAULT_QUICK_MENU_BACKDROP_OPACITY,
            sidebar_visibility: session_store::SidebarVisibility::default(),
            system_dark_theme,
            icons: HashMap::new(),
            image_cache: icon_cache::IconCache::default(),
            type_icon_requests: HashSet::new(),
            thumbnail_types: HashMap::new(),
            ordinary_icon_requests: HashSet::new(),
            sidebar_icons: HashMap::new(),
            libraries: Vec::new(),
            library_failures: Vec::new(),
            library_icons: HashMap::new(),
            library_generation: 0,
            network_location_generation: 0,
            thumbnail_failures: HashMap::new(),
            shortcut_requests: HashSet::new(),
            shortcut_completed: HashSet::new(),
            sidebar: Vec::new(),

            drive_capacity: HashMap::new(),
            drive_generation: 0,
            quick_access_generation: 0,
            drive_list_generation: 0,
            quick_access_pending: HashSet::new(),
            home_pages: HashMap::new(),

            network_locations: Vec::new(),
            imported_network_locations: Vec::new(),
            network_discovery: HashMap::new(),
            network_discovery_errors: HashMap::new(),
            default_directory_view,
            directory_view_lru: directory_views.keys().cloned().collect(),
            directory_views,
            search_view,
            operations: OperationManager::new(),
            undo_history: UndoHistory::default(),
            operation_errors: Vec::new(),
            operation_notice: None,
            operation_notice_generation: 0,
            rename_targets: HashMap::new(),
            focus_after_refresh: HashMap::new(),
            pending_rename_ui: HashMap::new(),
            active_operation_directories: HashMap::new(),
            deferred_watch_directories: HashSet::new(),
            pending_shell_creates: HashMap::new(),
            pending_permanent_delete: None,
            pending_imported_network_location_remove: None,
            exit_after_cancel: false,
            clipboard_has_files: false,
            cut_paths: Vec::new(),
            clipboard_sequence: None,
            conflict_responses: HashMap::new(),

            everything: app_state::EverythingState {
                config: everything_config,
                ..Default::default()
            },
            pending_right_drops: HashMap::new(),
            integration: app_state::IntegrationState::default(),
            tab_drag: None,
            column_drag: None,
        }
    }

    fn begin_column_drag(
        &mut self,
        window_id: WindowId,
        kind: u8,
        press_x: f32,
        press_y: f32,
    ) -> bool {
        if kind >= ColumnKind::COUNT as u8 || !press_x.is_finite() || !press_y.is_finite() {
            return false;
        }
        let Some(window) = self.windows.get(&window_id) else {
            return false;
        };
        let source = window.active().page_source;
        let order = if source == PageSource::Search {
            self.search_view.columns.order
        } else {
            let path = window.active().visible_path().map(Path::to_path_buf);
            path.as_deref()
                .map(|path| self.directory_preference(path).columns.order)
                .unwrap_or(self.default_directory_view.columns.order)
        };
        let Some(column_kind) = ColumnKind::from_storage_code(kind) else {
            return false;
        };
        if !order.contains(&column_kind) {
            return false;
        }
        self.column_drag = Some(ColumnDragSession {
            window_id,
            source,
            kind,
            press_x,
            press_y,
            phase: ColumnDragPhase::Pressed,
        });
        true
    }

    fn update_column_drag(
        &mut self,
        pointer_x: f32,
        pointer_y: f32,
        header_x: f32,
        header_y: f32,
        header_width: f32,
        viewport_x: f32,
    ) -> Option<usize> {
        let mut drag = self.column_drag?;
        if !pointer_x.is_finite() || !pointer_y.is_finite() {
            return None;
        }
        if matches!(drag.phase, ColumnDragPhase::Pressed)
            && (pointer_x - drag.press_x).hypot(pointer_y - drag.press_y) < COLUMN_DRAG_THRESHOLD
        {
            return None;
        }
        let current_source = self.windows.get(&drag.window_id)?.active().page_source;
        if current_source != drag.source {
            self.column_drag = None;
            return None;
        }
        let layout = if drag.source == PageSource::Search {
            self.search_view.columns
        } else {
            let path = self.windows.get(&drag.window_id)?.active().visible_path()?;
            self.directory_preference(path).columns
        };
        let order = layout.order;
        let widths = layout.widths;
        let insertion_slot = column_insertion_slot(
            pointer_x,
            pointer_y,
            ColumnHeaderGeometry {
                x: header_x,
                y: header_y,
                width: header_width,
                viewport_x,
            },
            &order,
            &widths,
        );
        drag.phase = ColumnDragPhase::Dragging { insertion_slot };
        self.column_drag = Some(drag);
        insertion_slot
    }

    fn finish_column_drag(&mut self, valid_release: bool) -> bool {
        let Some(drag) = self.column_drag.take() else {
            return false;
        };
        let ColumnDragPhase::Dragging {
            insertion_slot: Some(insertion_slot),
        } = drag.phase
        else {
            return false;
        };
        if !valid_release
            || self
                .windows
                .get(&drag.window_id)
                .map(WindowState::active)
                .map(|tab| tab.page_source)
                != Some(drag.source)
        {
            return false;
        }
        self.commit_column_reorder(drag.source, drag.kind, insertion_slot)
    }

    fn commit_column_reorder(
        &mut self,
        source: PageSource,
        kind: u8,
        insertion_slot: usize,
    ) -> bool {
        let Some(kind) = ColumnKind::from_storage_code(kind) else {
            return false;
        };
        let mut changed = false;
        let mut reorder = |layout: &mut ColumnLayout| {
            let Some(source_index) = layout.order.iter().position(|candidate| *candidate == kind)
            else {
                return;
            };
            let target = normalized_column_slot(source_index, insertion_slot, layout.order.len());
            changed = reorder_column_to_slot(&mut layout.order, kind, target);
        };
        if source == PageSource::Search {
            reorder(&mut self.search_view.columns);
        } else if let Some(path) = self.active().visible_path().map(Path::to_path_buf) {
            self.update_directory_preference(path, |preference| reorder(&mut preference.columns));
        }
        changed
    }

    fn cancel_column_drag(&mut self) -> bool {
        self.column_drag.take().is_some()
    }

    fn dark_theme(&self) -> bool {
        match self.theme_mode {
            session_store::ThemeMode::System => self.system_dark_theme,
            session_store::ThemeMode::Light => false,
            session_store::ThemeMode::Dark => true,
        }
    }
}

fn reorder_column_to_slot(
    order: &mut [ColumnKind; 5],
    kind: ColumnKind,
    insertion_slot: usize,
) -> bool {
    let Some(from) = order.iter().position(|candidate| *candidate == kind) else {
        return false;
    };
    let target = insertion_slot.min(order.len() - 1);
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

fn column_insertion_slot(
    pointer_x: f32,
    pointer_y: f32,
    geometry: ColumnHeaderGeometry,
    order: &[ColumnKind; 5],
    widths: &[u32; 5],
) -> Option<usize> {
    if !pointer_x.is_finite()
        || !geometry.x.is_finite()
        || !geometry.width.is_finite()
        || !geometry.viewport_x.is_finite()
        || pointer_x < geometry.x
        || pointer_x > geometry.x + geometry.width
        || !(geometry.y..=geometry.y + 38.0).contains(&pointer_y)
    {
        return None;
    }
    let content_x = pointer_x - geometry.x - geometry.viewport_x;
    let mut left = 0.0;
    for (index, kind) in order.iter().enumerate() {
        let width = widths.get(kind.storage_code() as usize).copied()? as f32;
        if content_x < left + width / 2.0 {
            return Some(index);
        }
        left += width;
    }
    Some(order.len())
}

fn normalized_column_slot(source_index: usize, insertion_slot: usize, len: usize) -> usize {
    let slot = insertion_slot.min(len);
    if slot > source_index { slot - 1 } else { slot }
}
#[derive(Debug)]
enum NetworkDiscoveryRequest {
    Discover {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
        cancel: Arc<std::sync::atomic::AtomicBool>,
    },
}

#[derive(Debug)]
enum NetworkDiscoveryEvent {
    Batch {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
        devices: Vec<NetworkDeviceTarget>,
    },
    Finished {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
    },
    Failed {
        window_id: WindowId,
        request_id: DiscoveryRequestId,
        error: crate::network::NetworkErrorKind,
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

fn view_mode_from_ui(mode: i32) -> ViewMode {
    ViewMode::from_storage_code(mode.clamp(0, u8::MAX as i32) as u8).unwrap_or(ViewMode::Details)
}

fn projected_view_mode(ui: &AppWindow) -> ViewMode {
    match view_mode_from_ui(ui.get_view_mode()) {
        ViewMode::Icons(_) => ViewMode::Icons((ui.get_layout_icon_size() as u16).clamp(16, 256)),
        mode => mode,
    }
}

fn project_file_geometry(ui: &AppWindow, mode: ViewMode) {
    let geometry = file_layout_geometry(mode);
    ui.set_layout_row_height(geometry.row_height);
    ui.set_layout_card_width(geometry.card_width);
    ui.set_layout_icon_size(geometry.icon_size as f32);
}

fn view_mode_to_ui(mode: ViewMode) -> i32 {
    i32::from(mode.storage_code())
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct FileLayoutGeometry {
    row_height: f32,
    card_width: f32,
    card_height: f32,
    icon_size: u32,
    grid: bool,
}

// Match Slint file-grid chrome: sidebar breakpoints, reserved scrollbar, and row padding/gap.
const WINDOW_MINIMAL_WIDTH: f32 = 900.0;
const WINDOW_COMPACT_WIDTH: f32 = 1000.0;
const NAVIGATION_SIDEBAR_COMPACT_WIDTH: f32 = 184.0;
const NAVIGATION_SIDEBAR_FULL_WIDTH: f32 = 218.0;
const FILE_GRID_SCROLLBAR_RESERVE: f32 = 28.0;
const FILE_GRID_COLUMN_GAP: f32 = 8.0;
const FILE_GRID_HORIZONTAL_INSET: f32 = 24.0;
const FILE_GRID_VIEWPORT_MATCH_SLACK: f32 = 2.0;

fn logical_window_width(ui: &AppWindow) -> f32 {
    ui.get_layout_width()
}

fn navigation_sidebar_width(window_width: f32) -> f32 {
    if window_width < WINDOW_MINIMAL_WIDTH {
        0.0
    } else if window_width < WINDOW_COMPACT_WIDTH {
        NAVIGATION_SIDEBAR_COMPACT_WIDTH
    } else {
        NAVIGATION_SIDEBAR_FULL_WIDTH
    }
}

fn estimated_file_viewport_width(window_width: f32) -> f32 {
    (window_width - navigation_sidebar_width(window_width) - FILE_GRID_SCROLLBAR_RESERVE).max(0.0)
}

fn grid_available_width(ui: &AppWindow) -> f32 {
    let estimated = estimated_file_viewport_width(logical_window_width(ui));
    let measured = ui.get_file_viewport_width();
    if measured > 1.0 && (measured - estimated).abs() <= FILE_GRID_VIEWPORT_MATCH_SLACK {
        measured
    } else if estimated > 1.0 {
        estimated
    } else {
        measured.max(1.0)
    }
}

fn grid_column_count_for_width(available_width: f32, card_width: f32) -> usize {
    let slot = (card_width + FILE_GRID_COLUMN_GAP).max(1.0);
    ((available_width - FILE_GRID_HORIZONTAL_INSET) / slot)
        .floor()
        .max(1.0) as usize
}

fn grid_column_count_for_view(ui: &AppWindow, view_mode: ViewMode) -> usize {
    let geometry = file_layout_geometry(view_mode);
    if !geometry.grid {
        1
    } else {
        grid_column_count_for_width(grid_available_width(ui), geometry.card_width)
    }
}

fn file_layout_geometry(view_mode: ViewMode) -> FileLayoutGeometry {
    match view_mode {
        ViewMode::Details => FileLayoutGeometry {
            row_height: 32.0,
            card_width: 0.0,
            card_height: 32.0,
            icon_size: 32,
            grid: false,
        },
        ViewMode::List => FileLayoutGeometry {
            row_height: 30.0,
            card_width: 0.0,
            card_height: 30.0,
            icon_size: 32,
            grid: false,
        },
        ViewMode::Icons(size) => {
            let logical_size = f32::from(size);
            let compact = size <= 16;
            let card_height = if compact { 24.0 } else { logical_size + 68.0 };
            FileLayoutGeometry {
                row_height: card_height + 8.0,
                card_width: if compact {
                    180.0
                } else {
                    (logical_size + 16.0).max(88.0)
                },
                card_height,
                icon_size: u32::from(size),
                grid: true,
            }
        }
        ViewMode::Tiles => FileLayoutGeometry {
            row_height: 78.0,
            card_width: 292.0,
            card_height: 70.0,
            icon_size: 48,
            grid: true,
        },
        ViewMode::Content => FileLayoutGeometry {
            row_height: 68.0,
            card_width: 0.0,
            card_height: 68.0,
            icon_size: 32,
            grid: false,
        },
    }
}

const DEFAULT_GROUP_HEADER_HEIGHT: u64 = 32;
const LIBRARY_GROUP_HEADER_HEIGHT: u64 = 48;

fn group_header_height_for_detail(detail: &str) -> u64 {
    if detail.is_empty() {
        DEFAULT_GROUP_HEADER_HEIGHT
    } else {
        LIBRARY_GROUP_HEADER_HEIGHT
    }
}

fn group_header_height(groups: &[group_projection::GroupProjection]) -> u64 {
    groups
        .iter()
        .find(|group| group.header_visible)
        .map_or(DEFAULT_GROUP_HEADER_HEIGHT, |group| {
            group_header_height_for_detail(&group.detail)
        })
}

fn file_row_height(view_mode: ViewMode) -> f32 {
    file_layout_geometry(view_mode).row_height
}

fn projected_scroll_maximum(ui: &AppWindow, view_mode: ViewMode, visible_height: f32) -> f32 {
    if view_mode.uses_grid_layout() {
        let rows = ui.get_grid_rows();
        if let Some(grid) = rows
            .as_any()
            .downcast_ref::<directory_model::DirectoryGrid>()
        {
            return (grid.extent() - visible_height).max(0.0);
        }
    } else if let Some(rows) = directory_rows(ui) {
        return (rows.extent() - visible_height).max(0.0);
    }

    if ui.get_grouped_grid_enabled() && view_mode.uses_grid_layout() {
        return (ui.get_grouped_grid_extent() - visible_height).max(0.0);
    }
    if view_mode.uses_grid_layout() {
        let extent = ui
            .get_grid_rows()
            .iter()
            .map(|row| {
                if row.group_header {
                    group_header_height_for_detail(row.group_detail.as_str()) as f32
                } else {
                    file_row_height(view_mode)
                }
            })
            .sum::<f32>();
        (extent - visible_height).max(0.0)
    } else {
        let files = ui.get_files();
        if let Some(extent) = files
            .as_any()
            .downcast_ref::<VecModel<FileRow>>()
            .and_then(|model| file_row_index::extent(model, file_row_height(view_mode)))
        {
            return (extent - visible_height).max(0.0);
        }
        let extent = files
            .iter()
            .map(|row| {
                if row.group_header {
                    group_header_height_for_detail(row.group_detail.as_str()) as f32
                } else {
                    file_row_height(view_mode)
                }
            })
            .sum::<f32>();
        (extent - visible_height).max(0.0)
    }
}
fn apply_file_scroll_delta(ui: &AppWindow, delta: f32) {
    if ui.get_search_results_mode() {
        ui.invoke_request_search_position(ui.get_search_scroll_y() + delta);
        return;
    }
    let mode = projected_view_mode(ui);
    let maximum = projected_scroll_maximum(ui, mode, ui.get_file_viewport_height());
    ui.set_file_viewport_y((ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0));
    if ui.get_grouped_grid_enabled() {
        refresh_grouped_grid_viewport(ui);
    }
}

fn file_scroll_maximum(
    item_count: usize,
    view_mode: ViewMode,
    grid_columns: usize,
    visible_height: f32,
) -> f32 {
    let rows = if file_layout_geometry(view_mode).grid {
        item_count.div_ceil(grid_columns.max(1))
    } else {
        item_count
    };
    (rows as f32 * file_row_height(view_mode) - visible_height).max(0.0)
}

const CTRL_WHEEL_PIXEL_THRESHOLD: f32 = 80.0;

fn ctrl_wheel_step(delta: &MouseScrollDelta, scale_factor: f32, accumulator: &mut f32) -> i32 {
    let value = match delta {
        MouseScrollDelta::LineDelta(_, y) => *y,
        MouseScrollDelta::PixelDelta(position) => {
            position.y as f32 / scale_factor.max(f32::EPSILON) / CTRL_WHEEL_PIXEL_THRESHOLD
        }
    };
    if !value.is_finite() || value == 0.0 {
        return 0;
    }
    if accumulator.signum() != value.signum() {
        *accumulator = 0.0;
    }
    *accumulator += value;
    let steps = accumulator.trunc().clamp(-64.0, 64.0) as i32;
    *accumulator -= steps as f32;
    steps
}

#[derive(Debug, Clone, Copy)]
struct ZoomAnchor {
    entry_id: i32,
    screen_y: f32,
}

fn projected_entry_top(ui: &AppWindow, entry_id: i32) -> Option<f32> {
    let id = EntryId(entry_id as u32);
    let grid = ui.get_grid_rows();
    if let Some(grid) = grid
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>()
    {
        return grid.locate(id).map(|(row, _)| grid.top_of(row));
    }
    if let Some(rows) = directory_rows(ui) {
        return rows.locate(id).map(|row| rows.top_of(row));
    }

    let mode = projected_view_mode(ui);
    let mut top = 0.0;
    if mode.uses_grid_layout() {
        for row in ui.get_grid_rows().iter() {
            if row.entries.iter().any(|entry| entry.id == entry_id) {
                return Some(top);
            }
            top += if row.group_header {
                group_header_height_for_detail(row.group_detail.as_str()) as f32
            } else {
                file_row_height(mode)
            };
        }
    } else {
        for row in ui.get_files().iter() {
            if !row.group_header && row.id == entry_id {
                return Some(top);
            }
            top += if row.group_header {
                group_header_height_for_detail(row.group_detail.as_str()) as f32
            } else {
                file_row_height(mode)
            };
        }
    }
    None
}

fn anchor_distance(y: f32, top: f32, height: f32) -> f32 {
    (top - y).max(0.0).max(y - top - height)
}

fn capture_zoom_anchor(ui: &AppWindow, pointer_x: f32, pointer_y: f32) -> Option<ZoomAnchor> {
    let mode = projected_view_mode(ui);
    let geometry = file_layout_geometry(mode);
    let columns = ui.get_grid_column_count().max(1) as usize;
    let column = (((pointer_x - 16.0).max(0.0) / (geometry.card_width + 8.0)).floor() as usize)
        .min(columns - 1);
    if ui.get_search_results_mode() {
        let total = ui.get_search_total_items().max(0) as usize;
        if total == 0 {
            return None;
        }
        let row = ((-ui.get_search_scroll_y() + pointer_y).max(0.0) / geometry.row_height).floor()
            as usize;
        let index = if geometry.grid {
            row * columns + column
        } else {
            row
        }
        .min(total - 1);
        let top = -search_scroll_for_index(index as u32, mode, columns);
        return Some(ZoomAnchor {
            entry_id: (index + 1) as i32,
            screen_y: top + ui.get_search_scroll_y(),
        });
    }
    let content_y = -ui.get_file_viewport_y() + pointer_y;
    let grid = ui.get_grid_rows();
    if let Some(grid) = grid
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>()
    {
        let range = grid.rows_between(
            content_y - geometry.row_height.max(48.0),
            content_y + geometry.row_height.max(48.0),
        );
        return range
            .filter_map(|row| {
                let ids = grid.entry_ids(row);
                ids.get(column.min(ids.len().saturating_sub(1)))
                    .map(|id| (id.0 as i32, grid.top_of(row)))
            })
            .min_by(|a, b| {
                anchor_distance(content_y, a.1, geometry.row_height).total_cmp(&anchor_distance(
                    content_y,
                    b.1,
                    geometry.row_height,
                ))
            })
            .map(|(entry_id, top)| ZoomAnchor {
                entry_id,
                screen_y: top + ui.get_file_viewport_y(),
            });
    }
    if let Some(rows) = directory_rows(ui) {
        return rows
            .rows_between(
                content_y - geometry.row_height.max(48.0),
                content_y + geometry.row_height.max(48.0),
            )
            .filter_map(|row| rows.entry_id(row).map(|id| (id.0 as i32, rows.top_of(row))))
            .min_by(|a, b| {
                anchor_distance(content_y, a.1, geometry.row_height).total_cmp(&anchor_distance(
                    content_y,
                    b.1,
                    geometry.row_height,
                ))
            })
            .map(|(entry_id, top)| ZoomAnchor {
                entry_id,
                screen_y: top + ui.get_file_viewport_y(),
            });
    }

    let mut top = 0.0;
    let mut nearest = None;
    let mut distance = f32::MAX;
    let mut consider = |id, top: f32| {
        let d = if content_y < top {
            top - content_y
        } else {
            (content_y - top - geometry.row_height).max(0.0)
        };
        if id > 0 && d < distance {
            distance = d;
            nearest = Some(ZoomAnchor {
                entry_id: id,
                screen_y: top + ui.get_file_viewport_y(),
            });
        }
    };
    if geometry.grid {
        for row in ui.get_grid_rows().iter() {
            if row.group_header {
                top += group_header_height_for_detail(row.group_detail.as_str()) as f32;
                continue;
            }
            if let Some(entry) = row
                .entries
                .row_data(column.min(row.entries.row_count().saturating_sub(1)))
            {
                consider(entry.id, top);
            }
            top += geometry.row_height;
        }
    } else {
        for row in ui.get_files().iter() {
            if row.group_header {
                top += group_header_height_for_detail(row.group_detail.as_str()) as f32;
                continue;
            }
            consider(row.id, top);
            top += geometry.row_height;
        }
    }
    nearest
}

fn restore_zoom_anchor(ui: &AppWindow, anchor: Option<ZoomAnchor>) {
    let Some(anchor) = anchor else {
        return;
    };
    let mode = projected_view_mode(ui);
    if ui.get_search_results_mode() {
        let position = search_scroll_for_index(
            anchor.entry_id.saturating_sub(1) as u32,
            mode,
            ui.get_grid_column_count().max(1) as usize,
        ) + anchor.screen_y;
        ui.invoke_request_search_position(position);
    } else if let Some(top) = projected_entry_top(ui, anchor.entry_id) {
        let maximum = projected_scroll_maximum(ui, mode, ui.get_file_viewport_height());
        ui.set_file_viewport_y((anchor.screen_y - top).clamp(-maximum, 0.0));
        if ui.get_grouped_grid_enabled() {
            refresh_grouped_grid_viewport(ui);
        }
    }
}

fn logical_scroll_delta(delta: &MouseScrollDelta, view_mode: ViewMode, scale_factor: f32) -> f32 {
    match delta {
        MouseScrollDelta::LineDelta(_, y) => *y * file_row_height(view_mode) * 3.0,
        MouseScrollDelta::PixelDelta(position) => {
            position.y as f32 / scale_factor.max(f32::EPSILON)
        }
    }
}

fn pointer_targets_file_area(
    pointer_x: f32,
    pointer_y: f32,
    left: f32,
    top: f32,
    width: f32,
    height: f32,
) -> bool {
    pointer_x >= left && pointer_x <= left + width && pointer_y >= top && pointer_y <= top + height
}
fn search_logical_maximum(
    total: u32,
    view_mode: ViewMode,
    columns: usize,
    visible_height: f32,
) -> f32 {
    file_scroll_maximum(total as usize, view_mode, columns, visible_height)
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
    let row = ((-scroll_y).max(0.0) / file_row_height(view_mode)).floor() as usize;
    let index = if file_layout_geometry(view_mode).grid {
        row.saturating_mul(columns.max(1))
    } else {
        row
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

fn search_window_viewport_y(
    scroll_y: f32,
    window: SearchWindow,
    view_mode: ViewMode,
    columns: usize,
) -> f32 {
    scroll_y - search_scroll_for_index(window.start, view_mode, columns)
}
fn search_scroll_for_index(index: u32, view_mode: ViewMode, columns: usize) -> f32 {
    let row = if file_layout_geometry(view_mode).grid {
        index as usize / columns.max(1)
    } else {
        index as usize
    };
    -(row as f32 * file_row_height(view_mode))
}

fn search_window_rows(
    tab: &TabSession,
    app: &AppState,
    window: SearchWindow,
    grid_requested_px: Option<u32>,
) -> Vec<FileRow> {
    let texts = Texts::new(app.language);
    (0..window.len)
        .map(|local| {
            let result_index = window.start.saturating_add(local as u32);
            tab.visible_entry(EntryId(result_index.saturating_add(1)))
                .map(|entry| file_row(entry, tab, texts, app, grid_requested_px))
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
        query: FolderSizeQuery,
    },
    Configure(crate::domain::EverythingConfig),
    Discover(u64),
    TestConnection(u64),
    Start(u64),
    PickExecutable {
        generation: u64,
        owner_window: isize,
    },
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
        query: FolderSizeQuery,
        state: FolderSizeState,
    },
    Status {
        generation: u64,
        result: Result<
            platform::windows::everything::EverythingStatus,
            platform::windows::everything::EverythingError,
        >,
    },
    Discovered {
        generation: u64,
        config: crate::domain::EverythingConfig,
        status: platform::windows::everything::EverythingStatus,
    },
    ExecutablePicked {
        generation: u64,
        result: std::io::Result<Option<PathBuf>>,
    },
}

enum FolderSizeWork {
    Query {
        tab_id: TabId,
        query: FolderSizeQuery,
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
    type_key: Option<platform::windows::shell_icons::ShellTypeIconKey>,
}

#[derive(Clone)]
struct ThumbnailScheduler {
    shared: Arc<(Mutex<ThumbnailSchedulerState>, Condvar)>,
    icon_sender: mpsc::Sender<IconRequest>,
}

#[derive(Default)]
struct ThumbnailSchedulerState {
    plans: ThumbnailPlans,
}

impl ThumbnailScheduler {
    fn new(icon_sender: mpsc::Sender<IconRequest>) -> Self {
        Self {
            shared: Arc::default(),
            icon_sender,
        }
    }

    fn replace(&self, tab_id: TabId, requests: Vec<IconRequest>) {
        let mut thumbnails = Vec::with_capacity(requests.len());
        for request in requests {
            if request.thumbnail {
                thumbnails.push(request);
            } else {
                let _ = self.icon_sender.send(request);
            }
        }
        let (lock, wake) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("thumbnail scheduler mutex is not poisoned");
        state.plans.replace(
            tab_id,
            thumbnails.into_iter().filter_map(|request| {
                Some(ThumbnailKey {
                    tab_id: request.tab_id,
                    request_id: request.request_id,
                    entry_id: match request.target {
                        IconTarget::Entry(entry_id) => entry_id,
                        IconTarget::Location => return None,
                    },
                    path: request.path,
                    requested_px: request.requested_px,
                })
            }),
        );
        wake.notify_all();
    }

    fn recv(&self) -> (u64, ThumbnailKey) {
        let (lock, wake) = &*self.shared;
        let mut state = lock
            .lock()
            .expect("thumbnail scheduler mutex is not poisoned");
        loop {
            if let Some(request) = state.plans.next_any() {
                return request;
            }
            state = wake
                .wait(state)
                .expect("thumbnail scheduler mutex is not poisoned");
        }
    }

    fn finish(&self, key: &ThumbnailKey) -> bool {
        self.shared
            .0
            .lock()
            .expect("thumbnail scheduler mutex is not poisoned")
            .plans
            .finish(key)
    }

    fn accepts(&self, key: &ThumbnailKey) -> bool {
        self.shared
            .0
            .lock()
            .expect("thumbnail scheduler mutex is not poisoned")
            .plans
            .accepts(key)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShortcutRequest {
    tab_id: TabId,
    request_id: RequestId,
    entry_id: EntryId,
    path: PathBuf,
}

#[derive(Debug)]
struct ShortcutEvent {
    request: ShortcutRequest,
    target: platform::ShortcutTarget,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IconTarget {
    Entry(EntryId),
    Location,
}

#[cfg(test)]
fn apply_progress_event_for_test(
    app: &mut AppState,
    id: OperationId,
    processed_bytes: u64,
    discovered_files: usize,
) {
    if let Some(task) = app.operations.task_mut(id)
        && !task.cancellation.is_cancelled()
    {
        task.progress.processed_bytes = processed_bytes;
        task.progress.discovered_files = discovered_files;
    }
}

#[derive(Debug)]
enum ClipboardRequest {
    Write { paths: Vec<PathBuf>, cut: bool },
    CancelCut { paths: Vec<PathBuf>, sequence: u32 },
    ReadPaste { origin_tab: TabId, target: PathBuf },
    Changed(u32),
}
#[derive(Debug)]
enum ClipboardEvent {
    Written {
        result: Result<(), String>,
        paths: Vec<PathBuf>,
        cut: bool,
        sequence: u32,
    },
    Paste {
        origin_tab: TabId,
        result: Result<Option<(FileOperationKind, Vec<OperationItem>)>, String>,
    },
    Changed {
        sequence: u32,
        result: Result<Option<platform::windows::clipboard::ClipboardFileList>, String>,
    },
}

#[derive(Clone)]
struct NetworkLoginSession {
    generation: u64,
    window_id: WindowId,
    tab_id: TabId,
    failed_request_id: RequestId,
    target: PathBuf,
    cancel: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Default)]
struct NetworkLoginCoordinator {
    next_generation: u64,
    current: Option<NetworkLoginSession>,
}

impl NetworkLoginCoordinator {
    fn begin(
        &mut self,
        window_id: WindowId,
        tab_id: TabId,
        failed_request_id: RequestId,
        target: PathBuf,
    ) -> NetworkLoginSession {
        self.cancel();
        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let session = NetworkLoginSession {
            generation: self.next_generation,
            window_id,
            tab_id,
            failed_request_id,
            target,
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };
        self.current = Some(session.clone());
        session
    }

    fn cancel(&mut self) {
        if let Some(current) = self.current.take() {
            current
                .cancel
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    fn is_current(&self, generation: u64) -> bool {
        self.current
            .as_ref()
            .is_some_and(|current| current.generation == generation)
    }

    fn finish(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.current = None;
        }
    }

    fn cancel_generation(&mut self, generation: u64) {
        if self.is_current(generation) {
            self.cancel();
        }
    }
}

#[derive(Clone)]
struct IconEvent {
    tab_id: TabId,
    request_id: RequestId,
    target: IconTarget,
    path: PathBuf,
    icon: platform::windows::shell_icons::ShellIconRgba,
    actual_thumbnail: bool,
    requested_px: u32,
    type_key: Option<platform::windows::shell_icons::ShellTypeIconKey>,
    thumbnail_failed: bool,
    thumbnail_plan: Option<(u64, ThumbnailKey)>,
}

fn startup_locations(
    restored_locations: Vec<NavigationLocation>,
    active_index: usize,
    launches: Vec<ClassifiedExternalLaunch>,
) -> (Vec<NavigationLocation>, usize, Vec<Option<PathBuf>>) {
    if launches.is_empty() {
        return (restored_locations, active_index, Vec::new());
    }
    let reveals = launches
        .iter()
        .map(|launch| launch.reveal_target().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    let locations = launches
        .into_iter()
        .map(|launch| NavigationLocation::Directory(launch.directory().to_path_buf()))
        .collect::<Vec<_>>();
    let active_index = locations.len().saturating_sub(1);
    (locations, active_index, reveals)
}

fn overlay_startup_shell_select(
    tabs: &[(TabId, NavigationLocation)],
    reveals: &mut [Option<PathBuf>],
    target: PathBuf,
) -> bool {
    let Some(parent) = target.parent() else {
        return false;
    };
    for (index, (_, location)) in tabs.iter().enumerate() {
        if location.directory_path() == Some(parent)
            && let Some(slot) = reveals.get_mut(index)
        {
            *slot = Some(target);
            return true;
        }
    }
    false
}
fn restore_session_in_background(
    scenario: Option<AgentScenario>,
) -> Option<session_store::SessionState> {
    if scenario.is_some() {
        return None;
    }
    let path = session_store::default_path()?;
    let worker = thread::Builder::new()
        .name("session-load".into())
        .spawn(move || session_store::load_with_diagnostics(&path));
    match worker {
        Ok(worker) => match worker.join() {
            Ok(Ok(session)) => Some(session),
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => None,
            Ok(Err(error)) => {
                eprintln!("session restore failed: {error}");
                None
            }
            Err(_) => {
                eprintln!("session restore worker panicked");
                None
            }
        },
        Err(error) => {
            eprintln!("session restore worker could not start: {error}");
            None
        }
    }
}

fn persist_session_in_background(path: PathBuf, session: io::Result<session_store::SessionState>) {
    let worker = thread::Builder::new()
        .name("session-save".into())
        .spawn(move || match session {
            Ok(session) => session_store::save_with_diagnostics(&path, &session),
            Err(error) => {
                session_store::record_save_validation_failure(&path, &error);
                Err(error)
            }
        });
    match worker {
        Ok(worker) => match worker.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("session save failed: {error}"),
            Err(_) => eprintln!("session save worker panicked"),
        },
        Err(error) => eprintln!("session save worker could not start: {error}"),
    }
}
pub fn run(
    scenario: Option<AgentScenario>,
    initial_external_paths: Vec<ExternalLaunchPath>,
    external_path_receiver: Option<
        mpsc::Receiver<platform::windows::single_instance::ExternalOpenRequest>,
    >,
    mut late_shell_select: Option<mpsc::Receiver<Option<PathBuf>>>,
) -> Result<(), slint::PlatformError> {
    let restored = restore_session_in_background(scenario);
    let classified_external = classify_external_launches_off_thread(initial_external_paths);
    let ui = AppWindow::new()?;
    let network_login_ui = NetworkLoginWindow::new()?;
    let network_location_rename_ui = NetworkLocationRenameWindow::new()?;
    let webdav_location_ui = WebDavLocationWindow::new()?;
    let operation_ui = OperationWindow::new()?;
    let delete_ui = ConfirmationWindow::new()?;
    let conflict_ui = ConfirmationWindow::new()?;
    let exit_ui = ConfirmationWindow::new()?;
    let delete_weak = delete_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let exit_weak = exit_ui.as_weak();

    let default_window = session_store::WindowPlacement {
        x: 80,
        y: 80,
        width: 1180,
        height: 760,
    };
    let (
        restored_locations,
        active_index,
        window,
        additional_windows,
        default_directory_view,
        search_view,
        directory_views,
        everything_config,
        theme_mode,
        language,
        file_visibility,
        file_list_quick_search,
        new_tab_opens_home,
        quick_menu_backdrop,
        quick_menu_backdrop_opacity,
        sidebar_visibility,
        network_locations,
        network_devices,
    ) = restored
        .filter(|session| {
            session
                .windows
                .first()
                .is_some_and(|window| !window.tab_locations.is_empty())
        })
        .map(|session| {
            let mut windows = session.windows;
            let first = windows.remove(0);
            (
                first.tab_locations,
                first.active_tab,
                first.placement,
                windows,
                session.default_directory_view,
                session.search_view,
                session
                    .directory_views
                    .into_iter()
                    .collect::<HashMap<_, _>>(),
                session.everything,
                session.theme_mode,
                session.language,
                session.file_visibility,
                session.file_list_quick_search,
                session.new_tab_opens_home,
                session.quick_menu_backdrop,
                session.quick_menu_backdrop_opacity,
                session.sidebar_visibility,
                session.network_locations,
                session.network_devices,
            )
        })
        .unwrap_or_else(|| {
            (
                vec![NavigationLocation::Home],
                0,
                default_window,
                Vec::new(),
                DirectoryViewPreference::default(),
                SearchViewPreference::default(),
                HashMap::new(),
                crate::domain::EverythingConfig::default(),
                session_store::ThemeMode::System,
                Language::Chinese,
                crate::domain::FileVisibility::default(),
                false,
                true,
                true,
                session_store::DEFAULT_QUICK_MENU_BACKDROP_OPACITY,
                session_store::SidebarVisibility::default(),
                Vec::new(),
                Vec::new(),
            )
        });
    let additional_windows = if classified_external.is_empty() {
        additional_windows
    } else {
        Vec::new()
    };
    let (restored_locations, active_index, mut startup_reveals) =
        startup_locations(restored_locations, active_index, classified_external);
    ui.window()
        .set_position(slint::PhysicalPosition::new(window.x, window.y));
    ui.window().set_size(slint::LogicalSize::new(
        window.width as f32,
        window.height as f32,
    ));
    let restored_locations = restored_locations
        .into_iter()
        .map(normalize_restored_location)
        .collect();
    let additional_windows = additional_windows
        .into_iter()
        .map(|mut window| {
            window.tab_locations = window
                .tab_locations
                .into_iter()
                .map(normalize_restored_location)
                .collect();
            window
        })
        .collect::<Vec<_>>();
    let state = Arc::new(Mutex::new(AppState::new(
        restored_locations,
        active_index,
        default_directory_view,
        search_view,
        directory_views,
        everything_config,
        theme_mode,
        language,
        platform::system_uses_dark_theme(),
    )));
    let network_login = Arc::new(Mutex::new(NetworkLoginCoordinator::default()));
    if let Ok(mut app) = state.lock() {
        app.file_visibility = file_visibility;
        app.file_list_quick_search = file_list_quick_search;
        app.new_tab_opens_home = new_tab_opens_home;
        app.quick_menu_backdrop = quick_menu_backdrop;
        app.quick_menu_backdrop_opacity = quick_menu_backdrop_opacity;
        app.sidebar_visibility = sidebar_visibility;
        app.network_locations = network_locations;
        app.network_location_generation = app.network_location_generation.wrapping_add(1).max(1);
        if !network_devices.is_empty() {
            let active_window = app.active_window;
            app.network_discovery.insert(
                active_window,
                DiscoveryCoordinator::with_devices(network_devices.clone()),
            );
        }
        app.active_window_state_mut().placement = window;
        for restored in &additional_windows {
            if !restored.tab_locations.is_empty() {
                let window_id = app.register_window(
                    restored.tab_locations.clone(),
                    restored.active_tab,
                    restored.placement,
                );
                if !network_devices.is_empty() {
                    app.network_discovery.insert(
                        window_id,
                        DiscoveryCoordinator::with_devices(network_devices.clone()),
                    );
                }
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
    let (request_sender, network_request_sender, event_receiver) =
        spawn_directory_workers(WORKER_COUNT, NETWORK_WORKER_COUNT);
    let (network_discovery_sender, network_discovery_receiver) = spawn_network_discovery_worker();
    let event_receiver = Arc::new(Mutex::new(event_receiver));
    let everything_config = state
        .lock()
        .expect("app state mutex is not poisoned")
        .everything
        .config
        .clone();
    let should_discover_everything = everything_config.executable_path.is_none();
    let (everything_sender, everything_receiver) =
        spawn_everything_worker(everything_config, state.clone());
    let (icon_sender, thumbnail_scheduler, icon_receiver) =
        spawn_icon_workers(ICON_WORKER_COUNT, state.clone());
    let (shortcut_sender, shortcut_receiver) = spawn_shortcut_worker(state.clone());
    let (operation_sender, operation_receiver) = spawn_file_operation_worker();
    let (clipboard_sender, clipboard_receiver) = spawn_clipboard_worker();
    let (clipboard_listener, clipboard_notifications) =
        platform::windows::clipboard::ClipboardListener::start()
            .map_err(|error| slint::PlatformError::Other(error.to_string()))?;
    forward_clipboard_notifications(clipboard_notifications, clipboard_sender.clone());
    let (shell_menu_worker, shell_menu_receiver) =
        platform::windows::context_menu::ShellMenuWorker::spawn();
    let quick_menu = Arc::new(Mutex::new(QuickMenuState::default()));

    let senders = WorkerSenders {
        directory: request_sender.clone(),
        network_directory: network_request_sender.clone(),
        network_discovery: network_discovery_sender.clone(),
        operation: operation_sender.clone(),
        clipboard: clipboard_sender.clone(),
        shell_menu: shell_menu_worker.clone(),
        everything: everything_sender.clone(),
        icon: icon_sender.clone(),
        thumbnail: thumbnail_scheduler.clone(),
        shortcut: shortcut_sender.clone(),
        network_login: network_login_ui.as_weak(),
        network_login_state: network_login.clone(),
        network_location_rename: network_location_rename_ui.as_weak(),
        webdav_location: webdav_location_ui.as_weak(),
        operation_window: operation_ui.as_weak(),
    };
    let initial_window_id = state
        .lock()
        .expect("app state mutex is not poisoned")
        .active_window;

    let scoped_state = WindowSessions::new(state.clone(), initial_window_id);
    let quick_menu_popup = create_quick_menu_popup(&ui, &scoped_state)?;
    wire_root_popup_callbacks(&quick_menu_popup.root, initial_window_id);

    wire_callbacks(
        &ui,
        network_login_ui.as_weak(),
        network_login.clone(),
        network_location_rename_ui.as_weak(),
        webdav_location_ui.as_weak(),
        operation_ui.as_weak(),
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        request_sender.clone(),
        network_request_sender.clone(),
        network_discovery_sender.clone(),
        operation_sender.clone(),
        clipboard_sender,
        everything_sender.clone(),
        icon_sender.clone(),
        thumbnail_scheduler.clone(),
        shortcut_sender.clone(),
        shell_menu_worker.clone(),
        quick_menu.clone(),
        scoped_state.clone(),
    );
    let quick_submenu_timer = wire_context_submenu_hover(&ui);
    let quick_menu_prewarm_timer = wire_quick_menu_prewarm(
        &ui,
        scoped_state.clone(),
        quick_menu.clone(),
        shell_menu_worker.clone(),
    );
    wire_internal_drag_drop(
        &ui,
        request_sender.clone(),
        network_request_sender.clone(),
        scoped_state.clone(),
    );
    wire_address_drag(&ui, scoped_state.clone());
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
        scoped_state.clone(),
    );
    wire_network_login_window(
        &ui,
        &network_login_ui,
        request_sender.clone(),
        network_request_sender.clone(),
        state.clone(),
        network_login.clone(),
    );
    wire_network_location_rename_window(&network_location_rename_ui, state.clone());
    webdav_ui::wire_window(&webdav_location_ui, state.clone());
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
        thumbnail_scheduler.clone(),
        shortcut_sender.clone(),
        everything_sender.clone(),
        state.clone(),
    );
    start_everything_event_pump(
        &ui,
        everything_receiver,
        everything_sender.clone(),
        thumbnail_scheduler.clone(),
        shortcut_sender.clone(),
        state.clone(),
    );
    let _ = everything_sender.send(if should_discover_everything {
        EverythingRequest::Discover(0)
    } else {
        EverythingRequest::TestConnection(0)
    });
    start_icon_event_pump(
        &ui,
        icon_receiver,
        state.clone(),
        thumbnail_scheduler.clone(),
    );
    start_shortcut_event_pump(&ui, shortcut_receiver, state.clone());
    start_file_operation_event_pump(
        &ui,
        &operation_ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        operation_receiver,
        operation_sender.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        everything_sender.clone(),
        shortcut_sender.clone(),
        thumbnail_scheduler,
        state.clone(),
    );
    start_shell_menu_event_pump(
        &ui,
        shell_menu_receiver,
        shell_menu_worker.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        quick_menu.clone(),
        state.clone(),
    );
    start_clipboard_event_pump(
        clipboard_receiver,
        operation_sender.clone(),
        request_sender.clone(),
        network_request_sender.clone(),
        state.clone(),
    );
    scan_cleanup_diagnostics(operation_sender.clone(), state.clone());
    start_sidebar_loader(&ui, state.clone());
    start_library_loader(&ui, state.clone(), request_sender.clone());
    start_network_location_loader(&ui, state.clone());
    start_network_discovery_event_pump(&ui, network_discovery_receiver, state.clone());
    refresh_system_integration_state(state.clone());
    refresh_window_ui(&ui, &state, initial_window_id);
    refresh_operation_window(&operation_ui, &state);
    refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state);
    network_login_ui.set_dark_theme(state.lock().map(|app| app.dark_theme()).unwrap_or_default());
    let initial_tabs = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.active_window_state()
            .tab_order
            .iter()
            .filter_map(|id| {
                app.active_window_state()
                    .tabs
                    .get(id)
                    .and_then(|tab| tab.current_location().clone())
                    .map(|path| (*id, path))
            })
            .collect::<Vec<_>>()
    };
    if scenario.is_none() {
        if let Some(receiver) = late_shell_select.as_mut() {
            match receiver.try_recv() {
                Ok(Some(target)) => {
                    if overlay_startup_shell_select(
                        &initial_tabs,
                        &mut startup_reveals,
                        target.clone(),
                    ) {
                        late_shell_select = None;
                    } else {
                        let (sender, receiver) = mpsc::channel();
                        let _ = sender.send(Some(target));
                        late_shell_select = Some(receiver);
                    }
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => late_shell_select = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        for (index, (tab_id, location)) in initial_tabs.into_iter().enumerate() {
            match location {
                NavigationLocation::Directory(path) => {
                    submit_path_navigation_with_reveal(
                        &request_sender,
                        &network_request_sender,
                        &state,
                        tab_id,
                        path,
                        NavigationKind::Refresh,
                        startup_reveals.get(index).cloned().flatten(),
                    );
                }
                NavigationLocation::WebDav(place) => {
                    submit_webdav_navigation(
                        &request_sender,
                        &state,
                        tab_id,
                        place,
                        NavigationKind::Refresh,
                    );
                }
                NavigationLocation::Home | NavigationLocation::Library(_) => {}
            }
        }
        let restored_remote_tabs = state
            .lock()
            .map(|app| {
                app.windows
                    .iter()
                    .filter(|(id, _)| **id != initial_window_id)
                    .flat_map(|(_, window)| {
                        window.tab_order.iter().filter_map(|id| {
                            match window
                                .tabs
                                .get(id)
                                .and_then(|tab| tab.current_location().as_ref())
                            {
                                Some(NavigationLocation::WebDav(place)) => {
                                    Some((*id, place.clone()))
                                }
                                _ => None,
                            }
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for (tab_id, place) in restored_remote_tabs {
            submit_webdav_navigation(
                &request_sender,
                &state,
                tab_id,
                place,
                NavigationKind::Refresh,
            );
        }
    }

    if let Some(receiver) = external_path_receiver {
        start_external_path_pump(
            receiver,
            state.clone(),
            request_sender.clone(),
            network_request_sender.clone(),
        );
    }
    if let Some(receiver) = late_shell_select {
        let tab_id = state
            .lock()
            .ok()
            .map(|app| app.active_window_state().active_tab);
        if let Some(context) = tab_id.and_then(|tab_id| external_reveal_context(&state, tab_id)) {
            start_late_shell_select_pump(receiver, state.clone(), context);
        }
    }
    let drag_drop_target_timer =
        wire_native_drag_drop(&ui, operation_sender.clone(), scoped_state.clone());
    let directory_watch_timer = start_directory_watchers(request_sender.clone(), state.clone());
    let drive_capacity_timer = start_drive_capacity_timer(state.clone());
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        runtimes.insert(
            initial_window_id,
            WindowRuntime {
                ui: ui.clone_strong(),
                sessions: scoped_state.clone(),
                _native_drop_timer: drag_drop_target_timer,
                _rectangle_selection_timer: rectangle_selection_timer,
                _quick_menu: quick_menu.clone(),
                _quick_submenu_timer: quick_submenu_timer.clone(),
                _quick_menu_prewarm_timer: quick_menu_prewarm_timer,
                quick_menu_popup,
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
    platform::windows::network::record_runtime_event("event_loop_started");
    let result = slint::run_event_loop();
    platform::windows::network::record_runtime_event("event_loop_returned");
    if let Ok(mut app) = state.lock() {
        app.cancel_directory_work_for_exit();
    }
    if let Ok(mut coordinator) = network_login.lock() {
        coordinator.cancel();
    }
    drop(directory_watch_timer);
    drop(drive_capacity_timer);
    for weak in [delete_weak, conflict_weak, exit_weak] {
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
    }
    let _ = network_login_ui.hide();
    let _ = network_location_rename_ui.hide();
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
        default_directory_view,
        search_view,
        directory_views,
        everything_config,
        theme_mode,
        language,
        file_visibility,
        file_list_quick_search,
        new_tab_opens_home,
        quick_menu_backdrop,
        quick_menu_backdrop_opacity,
        sidebar_visibility,
        network_locations,
        network_devices,
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
                let paths = window.stable_locations();
                (!paths.is_empty()).then_some(session_store::WindowSessionState {
                    placement: window.placement,
                    active_tab: window.stable_active_location_index(),
                    tab_locations: paths,
                })
            })
            .collect::<Vec<_>>();
        let mut directory_views = app
            .directory_views
            .iter()
            .map(|(path, preference)| (path.clone(), *preference))
            .collect::<Vec<_>>();
        directory_views.sort_by_key(|(path, _)| {
            app.directory_view_lru
                .iter()
                .position(|candidate| candidate == path)
                .unwrap_or(usize::MAX)
        });
        (
            windows,
            app.default_directory_view,
            app.search_view,
            directory_views,
            app.everything.config.clone(),
            app.theme_mode,
            app.language,
            app.file_visibility,
            app.file_list_quick_search,
            app.new_tab_opens_home,
            app.quick_menu_backdrop,
            app.quick_menu_backdrop_opacity,
            app.sidebar_visibility,
            app.network_locations.clone(),
            app.network_discovery
                .values()
                .flat_map(|discovery| discovery.successful_devices().iter())
                .filter(|device| device.unc_path.is_some())
                .fold(Vec::new(), |mut devices, device| {
                    if !devices
                        .iter()
                        .any(|known: &NetworkDeviceTarget| known.id == device.id)
                    {
                        devices.push(device.clone());
                    }
                    devices
                }),
        )
    };
    if scenario.is_none()
        && let Some(path) = session_store::default_path()
    {
        persist_session_in_background(
            path,
            session_store::SessionState::with_windows_and_settings(
                windows,
                default_directory_view,
                search_view,
                directory_views,
                theme_mode,
                language,
                everything_config,
                file_visibility,
                file_list_quick_search,
                new_tab_opens_home,
                quick_menu_backdrop,
                quick_menu_backdrop_opacity,
                sidebar_visibility,
                network_locations,
                network_devices,
            ),
        );
    }
    clear_window_runtimes();
    clipboard_listener.shutdown();
    platform::windows::drag_drop::shutdown_current();
    let _ = state.lock().ok().and_then(|mut app| {
        let active_window = app.active_window;
        app.close_window(active_window)
    });
    result
}

fn start_external_path_pump(
    receiver: mpsc::Receiver<platform::windows::single_instance::ExternalOpenRequest>,
    state: SharedSessions,
    local_sender: mpsc::Sender<DirectoryRequest>,
    network_sender: mpsc::SyncSender<DirectoryRequest>,
) {
    thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            let paths = request.paths;
            let paths = if paths.is_empty() {
                vec![ExternalLaunchPath::open(initial_path())]
            } else {
                paths
            };
            let followup = request.selection;
            let launches = classify_external_launches(&paths);
            let state_for_ui = state.clone();
            let local_for_ui = local_sender.clone();
            let network_for_ui = network_sender.clone();
            let _ = slint::invoke_from_event_loop(move || {
                open_classified_external_launches(
                    &state_for_ui,
                    &local_for_ui,
                    &network_for_ui,
                    launches,
                    followup,
                );
            });
        }
    });
}

#[derive(Clone)]
struct ExternalRevealContext {
    tab_id: TabId,
    request_id: RequestId,
    directory: PathBuf,
}

fn external_reveal_context(state: &SharedSessions, tab_id: TabId) -> Option<ExternalRevealContext> {
    let app = state.lock().ok()?;
    let tab = app.tab(tab_id)?;
    Some(ExternalRevealContext {
        tab_id,
        request_id: tab.latest_request(),
        directory: tab.visible_path()?.to_path_buf(),
    })
}

fn start_late_shell_select_pump(
    receiver: mpsc::Receiver<Option<PathBuf>>,
    state: SharedSessions,
    context: ExternalRevealContext,
) {
    thread::spawn(move || {
        let Ok(Some(target)) = receiver.recv() else {
            return;
        };
        for _ in 0..100 {
            let state_for_ui = state.clone();
            let context = context.clone();
            let target = target.clone();
            if slint::invoke_from_event_loop(move || {
                apply_late_shell_select_reveal(&state_for_ui, &context, target);
            })
            .is_ok()
            {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    });
}

fn apply_late_shell_select_reveal(
    state: &SharedSessions,
    context: &ExternalRevealContext,
    target: PathBuf,
) {
    if target.parent() != Some(context.directory.as_path()) {
        return;
    }
    let mut changed = HashSet::new();
    let scroll_target = {
        let mut app = match state.lock() {
            Ok(app) => app,
            Err(_) => return,
        };
        let tab_id = context.tab_id;
        let request_id = context.request_id;
        let Some(tab) = app.tab(tab_id) else {
            return;
        };
        if !tab.accepts(request_id) || tab.visible_path() != Some(context.directory.as_path()) {
            return;
        }
        // A late Shell reply belongs to the original navigation, not today's active tab.
        let is_active = app
            .window_for_tab(tab_id)
            .and_then(|id| app.window(id))
            .is_some_and(|window| window.active_tab == tab_id);
        if tab.load_state != LoadState::Complete {
            queue_external_reveal(
                &mut app,
                tab_id,
                context.directory.clone(),
                request_id,
                target,
            );
            None
        } else {
            let language = app.language;
            let tab = app.tab_mut(tab_id).expect("matched tab exists");
            if let Some(entry) = tab
                .entries
                .iter()
                .find(|entry| entry.path.matches(&target))
                .cloned()
            {
                changed = mutate_selection(tab, |tab| tab.select_entry(entry.id, false, false));
                is_active.then_some((tab_id, request_id, entry.id))
            } else {
                tab.error = Some(format!(
                    "reveal_target_missing:{}",
                    Texts::new(language).reveal_target_missing()
                ));
                None
            }
        }
    };
    let window_id = state
        .lock()
        .ok()
        .and_then(|app| app.window_for_tab(context.tab_id));
    if let Some(window_id) = window_id
        && let Some(ui) = window_ui(window_id)
    {
        update_file_rows(&ui, state, context.tab_id, &changed);
        refresh_window_regions(&ui, state, window_id, RefreshRegions::STATUS);
    }
    if let Some((tab_id, request_id, entry_id)) = scroll_target {
        reveal_late_shell_select(state, tab_id, request_id, entry_id);
    }
}

fn reveal_late_shell_select(
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    entry_id: EntryId,
) {
    let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id)) else {
        return;
    };
    let Some(ui) = window_ui(window_id) else {
        return;
    };
    reveal_entry(&ui, state, tab_id, request_id, entry_id);
    ui.window().request_redraw();
    platform::windows::restore_and_focus_window(native_window_handle(&ui));
}

fn open_classified_external_launches(
    state: &SharedSessions,
    local_sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    launches: Vec<ClassifiedExternalLaunch>,
    mut followup: Option<mpsc::Receiver<Option<PathBuf>>>,
) {
    for launch in launches {
        let directory = launch.directory().to_path_buf();
        if crate::domain::folder_size_scheduler::is_internal_cleanup_path(&directory) {
            continue;
        }
        let reveal = launch.reveal_target().map(Path::to_path_buf);
        let tab_id = state
            .lock()
            .ok()
            .map(|mut app| app.create_tab(NavigationLocation::Directory(directory.clone())));
        let Some(tab_id) = tab_id else {
            continue;
        };
        submit_path_navigation_with_reveal(
            local_sender,
            network_sender,
            state,
            tab_id,
            directory,
            NavigationKind::Refresh,
            reveal,
        );
        if let Some(receiver) = followup.take()
            && let Some(context) = external_reveal_context(state, tab_id)
        {
            start_late_shell_select_pump(receiver, state.clone(), context);
        }
    }
    let active_window = state.lock().ok().map(|app| app.active_window);
    if let Some(window_id) = active_window
        && let Some(ui) = window_ui(window_id)
    {
        refresh_window_regions(&ui, state, window_id, RefreshRegions::CONTENT);
        ui.window().request_redraw();
        platform::windows::restore_and_focus_window(native_window_handle(&ui));
    }
}
fn fail_directory_dispatch(state: &SharedSessions, request: &DirectoryRequest) {
    if let Ok(mut app) = state.lock() {
        let accepted = app.tab_mut(request.tab_id).is_some_and(|tab| {
            tab.fail_directory_navigation(
                request.request_id,
                LoadState::Failed,
                "queue-unavailable".to_owned(),
            )
        });
        if accepted {
            app.focus_after_refresh.remove(&request.tab_id);
        }
    }
}
fn submit_navigation(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
    reveal: Option<PathBuf>,
) -> bool {
    if crate::domain::folder_size_scheduler::is_internal_cleanup_path(&path)
        || crate::fs::is_directory_work_path(&path)
    {
        return false;
    }
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.cancel_column_drag();
        if let Some(tab) = app.tab_mut(tab_id) {
            cancel_folder_sizes(tab);
        }
        app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
        app.thumbnail_failures
            .retain(|(request_tab, _, _, _), _| *request_tab != tab_id);
        app.ordinary_icon_requests
            .retain(|(request_tab, _, _)| *request_tab != tab_id);
        app.focus_after_refresh.remove(&tab_id);
        let visibility = app.file_visibility;
        let (request_id, cancel) = {
            let Some(tab) = app.tab_mut(tab_id) else {
                return false;
            };
            if tab.kind != TabKind::Files {
                return false;
            }
            if kind == NavigationKind::Normal && tab.visible_path() == Some(path.as_path()) {
                tab.cancel_address_edit();
                return false;
            }
            tab.begin_directory_navigation(path.clone(), kind)
        };
        if let Some(target) = reveal {
            queue_external_reveal(&mut app, tab_id, path.clone(), request_id, target);
        }
        DirectoryRequest {
            tab_id,
            request_id,
            path,
            library: None,
            library_sources: None,
            unavailable_library_sources: 0,
            visibility,
            cancel,
            webdav: None,
        }
    };
    match sender.send(request) {
        Ok(()) => true,
        Err(error) => {
            fail_directory_dispatch(state, &error.0);
            false
        }
    }
}

fn submit_network_navigation(
    sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
    reveal: Option<PathBuf>,
) -> bool {
    if crate::domain::folder_size_scheduler::is_internal_cleanup_path(&path)
        || crate::fs::is_directory_work_path(&path)
    {
        return false;
    }
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.cancel_column_drag();
        if let Some(tab) = app.tab_mut(tab_id) {
            cancel_folder_sizes(tab);
        }
        app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
        app.thumbnail_failures
            .retain(|(request_tab, _, _, _), _| *request_tab != tab_id);
        app.ordinary_icon_requests
            .retain(|(request_tab, _, _)| *request_tab != tab_id);
        app.focus_after_refresh.remove(&tab_id);
        let visibility = app.file_visibility;
        let (request_id, cancel) = {
            let Some(tab) = app.tab_mut(tab_id) else {
                return false;
            };
            if tab.kind != TabKind::Files {
                return false;
            }
            if kind == NavigationKind::Normal && tab.visible_path() == Some(path.as_path()) {
                tab.cancel_address_edit();
                return false;
            }
            tab.begin_directory_navigation(path.clone(), kind)
        };
        if let Some(target) = reveal {
            queue_external_reveal(&mut app, tab_id, path.clone(), request_id, target);
        }
        DirectoryRequest {
            tab_id,
            request_id,
            path,
            library: None,
            library_sources: None,
            unavailable_library_sources: 0,
            visibility,
            cancel,
            webdav: None,
        }
    };
    match sender.try_send(request) {
        Ok(()) => true,
        Err(mpsc::TrySendError::Full(request)) => {
            if let Ok(mut app) = state.lock() {
                let accepted = app.tab_mut(request.tab_id).is_some_and(|tab| {
                    tab.fail_directory_navigation(
                        request.request_id,
                        LoadState::Failed,
                        "network directory queue is busy".to_owned(),
                    )
                });
                if accepted {
                    app.focus_after_refresh.remove(&request.tab_id);
                }
            }
            false
        }
        Err(mpsc::TrySendError::Disconnected(request)) => {
            fail_directory_dispatch(state, &request);
            false
        }
    }
}

fn submit_reveal_navigation(
    local_sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    window_id: WindowId,
    origin_tab_id: TabId,
    parent: PathBuf,
    target: PathBuf,
) -> Option<TabId> {
    if crate::domain::folder_size_scheduler::is_internal_cleanup_path(&parent)
        || crate::domain::folder_size_scheduler::is_internal_cleanup_path(&target)
    {
        return None;
    }
    let (tab_id, request) = {
        let mut app = state.lock().ok()?;
        if app
            .window(window_id)
            .is_none_or(|window| window.active_tab != origin_tab_id)
        {
            return None;
        }
        let tab_id =
            app.create_tab_in_window(window_id, NavigationLocation::Directory(parent.clone()))?;
        let visibility = app.file_visibility;
        let (request_id, cancel) = app
            .tab_mut(tab_id)?
            .begin_directory_navigation(parent.clone(), NavigationKind::Refresh);
        app.focus_after_refresh.insert(
            tab_id,
            PendingFocus {
                directory: parent.clone(),
                request_id: Some(request_id),
                paths: vec![target],
                action: PendingFocusAction::Reveal { window_id },
            },
        );
        (
            tab_id,
            DirectoryRequest {
                tab_id,
                request_id,
                path: parent.clone(),
                library: None,
                library_sources: None,
                unavailable_library_sources: 0,
                visibility,
                cancel,
                webdav: None,
            },
        )
    };
    let request_id = request.request_id;
    let sent = if crate::network::is_unc_path(&parent) {
        network_sender.try_send(request).is_ok()
    } else {
        local_sender.send(request).is_ok()
    };
    if !sent && let Ok(mut app) = state.lock() {
        app.focus_after_refresh.remove(&tab_id);
        if let Some(tab) = app.tab_mut(tab_id) {
            tab.fail_directory_navigation(
                request_id,
                LoadState::Failed,
                "directory queue is unavailable".to_owned(),
            );
        }
    }
    Some(tab_id)
}
fn submit_path_navigation(
    local_sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
) -> bool {
    submit_path_navigation_with_reveal(
        local_sender,
        network_sender,
        state,
        tab_id,
        path,
        kind,
        None,
    )
}

fn submit_path_navigation_with_reveal(
    local_sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
    reveal: Option<PathBuf>,
) -> bool {
    if crate::network::is_unc_path(&path) {
        submit_network_navigation(network_sender, state, tab_id, path, kind, reveal)
    } else {
        submit_navigation(local_sender, state, tab_id, path, kind, reveal)
    }
}

fn queue_external_reveal(
    app: &mut AppState,
    tab_id: TabId,
    directory: PathBuf,
    request_id: RequestId,
    target: PathBuf,
) {
    let Some(window_id) = app.window_for_tab(tab_id) else {
        return;
    };
    app.focus_after_refresh.insert(
        tab_id,
        PendingFocus {
            directory,
            request_id: Some(request_id),
            paths: vec![target],
            action: PendingFocusAction::Reveal { window_id },
        },
    );
}

fn submit_library_navigation(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    library: LibraryLocationId,
    kind: NavigationKind,
) -> bool {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let Some(definition) = app
            .libraries
            .iter()
            .find(|candidate| candidate.id.as_os_str() == library.identity.as_os_str())
            .cloned()
        else {
            let message = Texts::new(app.language).library_unavailable().to_owned();
            if let Some(tab) = app.tab_mut(tab_id) {
                let (request_id, _) =
                    tab.begin_navigation(NavigationLocation::Library(library.clone()), kind);
                tab.fail_directory_navigation(request_id, LoadState::Failed, message);
            }
            return false;
        };
        app.cancel_column_drag();
        if let Some(tab) = app.tab_mut(tab_id) {
            cancel_folder_sizes(tab);
        }
        app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
        app.thumbnail_failures
            .retain(|(request_tab, _, _, _), _| *request_tab != tab_id);
        app.ordinary_icon_requests
            .retain(|(request_tab, _, _)| *request_tab != tab_id);
        app.focus_after_refresh.remove(&tab_id);
        let visibility = app.file_visibility;
        let Some(tab) = app.tab_mut(tab_id) else {
            return false;
        };
        if tab.kind != TabKind::Files {
            return false;
        }
        let location = NavigationLocation::Library(library.clone());
        if kind == NavigationKind::Normal && tab.visible_location() == Some(&location) {
            tab.cancel_address_edit();
            return false;
        }
        let (request_id, cancel) = tab.begin_navigation(location, kind);
        let library_sources = definition
            .sources
            .iter()
            .enumerate()
            .filter_map(|(index, source)| source.path.clone().map(|path| (index, path)))
            .collect::<Vec<_>>();
        DirectoryRequest {
            tab_id,
            request_id,
            path: PathBuf::new(),
            library: Some(library),
            unavailable_library_sources: definition.sources.len() - library_sources.len(),
            library_sources: Some(library_sources),
            visibility,
            cancel,
            webdav: None,
        }
    };
    match sender.send(request) {
        Ok(()) => true,
        Err(error) => {
            fail_directory_dispatch(state, &error.0);
            false
        }
    }
}
fn submit_location_navigation(
    local_sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    location: NavigationLocation,
    kind: NavigationKind,
) -> bool {
    match location {
        NavigationLocation::Home => state
            .lock()
            .ok()
            .and_then(|mut app| {
                let tab = app.tab_mut(tab_id)?;
                (tab.kind == TabKind::Files).then(|| tab.navigate_home(kind))
            })
            .is_some(),
        NavigationLocation::Directory(path) => {
            submit_path_navigation(local_sender, network_sender, state, tab_id, path, kind)
        }
        NavigationLocation::Library(library) => {
            submit_library_navigation(local_sender, state, tab_id, library, kind)
        }
        NavigationLocation::WebDav(place) => {
            submit_webdav_navigation(local_sender, state, tab_id, place, kind)
        }
    }
}

fn submit_webdav_navigation(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    place: crate::domain::WebDavPlace,
    kind: NavigationKind,
) -> bool {
    let visibility = state.lock().ok().map(|app| app.file_visibility);
    let Some(visibility) = visibility else {
        return false;
    };
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let connection = app
            .network_locations
            .iter()
            .find_map(|location| match &location.target {
                NetworkTarget::WebDav(connection)
                    if connection.connection_id == place.connection_id =>
                {
                    Some(connection.clone())
                }
                _ => None,
            });
        let Some(connection) = connection else {
            return false;
        };
        let root = connection.root.trim_end_matches('/');
        let path = if place.path.is_empty() {
            connection.root.clone()
        } else {
            place.path.clone()
        };
        let inside = path == connection.root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
        if !inside {
            return false;
        }
        let location = NavigationLocation::WebDav(crate::domain::WebDavPlace {
            connection_id: connection.connection_id,
            revision: connection.revision,
            root: connection.root.clone(),
            path,
        });
        let Some(tab) = app.tab_mut(tab_id) else {
            return false;
        };
        if tab.kind != TabKind::Files {
            return false;
        }
        let directory = match &location {
            NavigationLocation::WebDav(place) => place.path.clone(),
            _ => return false,
        };
        let (request_id, cancel) = tab.begin_navigation(location, kind);
        DirectoryRequest {
            tab_id,
            request_id,
            path: PathBuf::new(),
            library: None,
            library_sources: None,
            unavailable_library_sources: 0,
            visibility,
            cancel,
            webdav: Some(directory_loading::WebDavListTarget {
                connection_id: connection.connection_id,
                revision: connection.revision,
                https: connection.https,
                host: connection.host,
                port: connection.port,
                root: connection.root,
                username: connection.username,
                directory,
            }),
        }
    };
    sender.send(request).is_ok()
}

fn webdav_incomplete_text(language: Language, detail: &str) -> String {
    match language {
        Language::Chinese => format!("目录没有完整读取：{detail}"),
        Language::English => format!("The folder listing is incomplete: {detail}"),
    }
}
fn network_location_from_stable_id<'a>(
    app: &'a AppState,
    stable_id: &str,
) -> Option<&'a NetworkLocation> {
    let id = stable_id
        .strip_prefix("network-location:")?
        .parse::<u64>()
        .ok()?;
    app.imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .find(|location| location.id == id)
}
fn sidebar_navigation_target(app: &AppState, index: usize) -> Option<PathBuf> {
    if let Some(location) = app.sidebar.get(index) {
        return Some(location.path.clone());
    }
    let mut location_targets = app
        .imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .map(|location| match &location.target {
            NetworkTarget::WindowsPath(path) => (location.sort_order, Some(path.clone())),
            NetworkTarget::ShellItemId(_) | NetworkTarget::WebDav(_) => (location.sort_order, None),
        })
        .collect::<Vec<_>>();
    location_targets.sort_by_key(|(order, _)| *order);
    let offset = index.saturating_sub(app.sidebar.len());
    if offset < location_targets.len() {
        return location_targets
            .get(offset)
            .and_then(|(_, path)| path.clone());
    }
    app.network_discovery
        .get(&app.active_window)
        .and_then(|discovery| {
            discovery
                .devices()
                .iter()
                .filter_map(crate::network::device_root_target)
                .nth(offset - location_targets.len())
        })
}
fn restart_detached_tab(
    outcome: &DetachedTabOutcome,
    senders: &WorkerSenders,
    state: &SharedSessions,
) {
    match &outcome.restart {
        Some(DetachedTabRestart::Directory(path)) => {
            submit_path_navigation(
                &senders.directory,
                &senders.network_directory,
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
    let quick_menu = Arc::new(Mutex::new(QuickMenuState::default()));
    let quick_menu_popup = create_quick_menu_popup(&ui, &scoped)?;
    wire_root_popup_callbacks(&quick_menu_popup.root, window_id);
    ui.window()
        .set_position(slint::PhysicalPosition::new(placement.x, placement.y));
    ui.window().set_size(slint::LogicalSize::new(
        placement.width as f32,
        placement.height as f32,
    ));
    wire_callbacks(
        &ui,
        senders.network_login.clone(),
        senders.network_login_state.clone(),
        senders.network_location_rename.clone(),
        senders.webdav_location.clone(),
        senders.operation_window.clone(),
        delete_ui,
        conflict_ui,
        exit_ui,
        senders.directory.clone(),
        senders.network_directory.clone(),
        senders.network_discovery.clone(),
        senders.operation.clone(),
        senders.clipboard.clone(),
        senders.everything.clone(),
        senders.icon.clone(),
        senders.thumbnail.clone(),
        senders.shortcut.clone(),
        senders.shell_menu.clone(),
        quick_menu.clone(),
        scoped.clone(),
    );
    let quick_submenu_timer = wire_context_submenu_hover(&ui);
    let quick_menu_prewarm_timer = wire_quick_menu_prewarm(
        &ui,
        scoped.clone(),
        quick_menu.clone(),
        senders.shell_menu.clone(),
    );
    wire_internal_drag_drop(
        &ui,
        senders.directory.clone(),
        senders.network_directory.clone(),
        scoped.clone(),
    );
    wire_address_drag(&ui, scoped.clone());
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
                sessions: scoped.clone(),
                _native_drop_timer: native_drop_timer,
                _rectangle_selection_timer: rectangle_selection_timer,
                _quick_menu: quick_menu.clone(),
                _quick_submenu_timer: quick_submenu_timer.clone(),
                _quick_menu_prewarm_timer: quick_menu_prewarm_timer,
                quick_menu_popup,
            },
        );
    });
    Ok(())
}

fn new_window_location(app: &AppState, source_window: WindowId) -> NavigationLocation {
    app.window(source_window)
        .map(WindowState::active)
        .filter(|tab| tab.kind == TabKind::Files && tab.load_state == LoadState::Complete)
        .and_then(|tab| tab.current_location().as_ref())
        .and_then(NavigationLocation::directory_path)
        .map(|path| NavigationLocation::Directory(path.to_path_buf()))
        .unwrap_or_else(|| NavigationLocation::Directory(initial_path()))
}

fn create_new_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    senders: &WorkerSenders,
    confirmations: &ConfirmationWindows,
    state: &SharedSessions,
) -> bool {
    let candidate = match AppWindow::new() {
        Ok(candidate) => candidate,
        Err(_) => return false,
    };
    let position = source_ui.window().position();
    let size = source_ui.window().size();
    let scale = source_ui.window().scale_factor();
    let placement = session_store::WindowPlacement {
        x: position.x.saturating_add(24),
        y: position.y.saturating_add(24),
        width: (size.width as f32 / scale).round() as u32,
        height: (size.height as f32 / scale).round() as u32,
    };
    let delete_ui = confirmations.delete.upgrade();
    let conflict_ui = confirmations.conflict.upgrade();
    let exit_ui = confirmations.exit.upgrade();
    let (Some(delete_ui), Some(conflict_ui), Some(exit_ui)) = (delete_ui, conflict_ui, exit_ui)
    else {
        return false;
    };
    let (destination, tab_id, location) = {
        let Ok(mut app) = state.lock() else {
            return false;
        };
        let location = new_window_location(&app, source_window);
        let destination = app.register_window(vec![location.clone()], 0, placement);
        let tab_id = app
            .window(destination)
            .expect("new window is registered")
            .active_tab;
        (destination, tab_id, location)
    };
    let installed = install_app_window(
        candidate,
        destination,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        senders,
        state.clone(),
    )
    .is_ok();
    if !installed {
        remove_window_runtime(destination);
        if let Ok(mut app) = state.lock() {
            let _ = app.close_window(destination);
        }
        return false;
    }
    submit_location_navigation(
        &senders.directory,
        &senders.network_directory,
        state,
        tab_id,
        location,
        NavigationKind::Refresh,
    );
    true
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
        refresh_window_regions(source_ui, state, source_window, RefreshRegions::TABS);
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
    let source_has_single_file_tab = state
        .lock()
        .is_ok_and(|app| app.drag_source_is_single_file_tab_window(source_window));
    let unmatched_action = (!moved && cross_target.is_none() && !valid_source)
        .then(|| unmatched_tab_drop_action(source_has_single_file_tab, target_window.is_some()))
        .flatten();
    let moved_source_window =
        matches!(
            unmatched_action,
            Some(UnmatchedTabDropAction::MoveSourceWindow)
        ) && move_single_tab_source_window(source_ui, source_window, screen_x, screen_y, state);
    let detached = matches!(
        unmatched_action,
        Some(UnmatchedTabDropAction::DetachToNewWindow)
    ) && detach_tab_into_new_window(
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
    } else if moved || moved_source_window || detached {
        false
    } else {
        state.lock().is_ok_and(|mut app| app.cancel_tab_drag())
    };
    project_native_insertion_indicator(None, state);
    source_ui.invoke_clear_tab_drag();
    if !moved && !detached {
        refresh_window_regions(source_ui, state, source_window, RefreshRegions::TABS);
    }
    finished || moved || moved_source_window || detached
}

fn move_single_tab_source_window(
    source_ui: &AppWindow,
    source_window: WindowId,
    screen_x: i32,
    screen_y: i32,
    state: &SharedSessions,
) -> bool {
    let scale = source_ui.window().scale_factor();
    let Some((grab_x, grab_y)) = state.lock().ok().and_then(|app| {
        let drag = app
            .tab_drag
            .filter(|drag| drag.window_id == source_window)?;
        Some((drag.press_x, drag.press_y))
    }) else {
        return false;
    };
    let outer = source_ui.window().position();
    let Ok((client_left, client_top, _, _)) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(source_ui))
    else {
        return false;
    };
    let (x, y) = single_tab_window_drop_position(
        screen_x,
        screen_y,
        grab_x,
        grab_y,
        scale,
        client_left - outer.x,
        client_top - outer.y,
    );
    let committed = state
        .lock()
        .is_ok_and(|mut app| app.commit_drag_source_window_move(source_window, x, y));
    if !committed {
        return false;
    }
    if platform::windows::move_window(native_window_handle(source_ui), x, y).is_err() {
        if let Ok(mut app) = state.lock()
            && let Some(window) = app.windows.get_mut(&source_window)
        {
            window.placement.x = outer.x;
            window.placement.y = outer.y;
        }
        return false;
    }
    true
}

fn single_tab_window_drop_position(
    screen_x: i32,
    screen_y: i32,
    grab_x: f32,
    grab_y: f32,
    scale: f32,
    client_offset_x: i32,
    client_offset_y: i32,
) -> (i32, i32) {
    (
        screen_x - (grab_x * scale).round() as i32 - client_offset_x,
        screen_y - (grab_y * scale).round() as i32 - client_offset_y,
    )
}

fn selected_paths(app: &AppState) -> Vec<PathBuf> {
    app.active()
        .selected
        .iter()
        .filter_map(|id| app.active().visible_entry(*id).map(|entry| entry.path()))
        .collect()
}
fn selected_paths_for_tab(tab: &TabSession) -> Vec<PathBuf> {
    tab.selected
        .iter()
        .filter_map(|id| tab.visible_entry(*id).map(|entry| entry.path()))
        .collect()
}

#[derive(Debug, Clone, Copy, Default)]
struct FileHitGeometry {
    list_left: f32,
    list_top: f32,
    viewport_x: f32,
    viewport_y: f32,
    viewport_width: f32,
    columns_width: f32,
}

impl FileHitGeometry {
    fn details_range(self) -> (f32, f32) {
        (
            16.0_f32.max(16.0 + self.viewport_x),
            self.viewport_width
                .min(16.0 + self.viewport_x + self.columns_width),
        )
    }

    fn details_contains(self, window_x: f32) -> bool {
        let local_x = window_x - self.list_left;
        let (left, right) = self.details_range();
        right > left && local_x >= left && local_x < right
    }
}

fn directory_entry_at_visual_point(
    app: &AppState,
    tab: &TabSession,
    view_mode: ViewMode,
    grid_columns: usize,
    local_x: f32,
    content_y: f32,
) -> Option<EntryId> {
    if content_y < 0.0 || local_x < 16.0 {
        return None;
    }
    let entries = tab.visible_entries();
    let groups = directory_group_projections(app, tab, entries);
    if file_layout_geometry(view_mode).grid {
        let projection = IconProjection::from_groups(
            &groups,
            grid_columns,
            group_header_height(&groups),
            file_row_height(view_mode) as u64,
        );
        let location = projection.offsets.locate(content_y as u64)?;
        let IconVisualRow::Entries { entries, .. } = projection.rows.get(location.row_index)?
        else {
            return None;
        };
        let geometry = file_layout_geometry(view_mode);
        let column = ((local_x - 16.0) / (geometry.card_width + 8.0).max(1.0)).floor() as usize;
        entries.get(column).copied()
    } else {
        let projection = ListProjection::from_groups(
            &groups,
            group_header_height(&groups),
            file_row_height(view_mode) as u64,
        );
        let location = projection.offsets.locate(content_y as u64)?;
        match projection.rows.get(location.row_index)? {
            ListVisualRow::Entry { entry_id } => Some(*entry_id),
            ListVisualRow::GroupHeader { .. } => None,
        }
    }
}
fn context_target_at(
    state: &SharedSessions,
    window_x: f32,
    window_y: f32,
    geometry: FileHitGeometry,
    search_scroll_y: f32,
    grid_columns: usize,
) -> (Option<EntryId>, bool) {
    let app = state.lock().expect("app state mutex is not poisoned");
    if window_y < geometry.list_top {
        return (None, true);
    }
    let active = app.active();
    let view_mode = app.active_view_mode();
    let local_x = window_x - geometry.list_left;
    if view_mode == ViewMode::Details && !geometry.details_contains(window_x) {
        return (None, true);
    }
    if view_mode != ViewMode::Details
        && (local_x < 16.0 || local_x >= geometry.viewport_width - 16.0)
    {
        return (None, true);
    }
    let content_y = window_y - geometry.list_top + (-geometry.viewport_y).max(0.0);
    let local_row = (content_y / file_row_height(view_mode)).floor() as usize;
    let local_index = if file_layout_geometry(view_mode).grid {
        let local_x = window_x - geometry.list_left;
        if local_x < 16.0 || local_x >= geometry.viewport_width - 16.0 {
            return (None, true);
        }
        let column = ((local_x - 16.0)
            / (file_layout_geometry(view_mode).card_width + 8.0).max(1.0))
        .floor() as usize;
        local_row
            .saturating_mul(grid_columns.max(1))
            .saturating_add(column.min(grid_columns.max(1) - 1))
    } else {
        local_row
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
        directory_entry_at_visual_point(
            &app,
            active,
            view_mode,
            grid_columns,
            window_x - geometry.list_left,
            content_y,
        )
    };
    (entry, entry.is_none())
}

fn cancel_operations(state: &SharedSessions, ids: impl IntoIterator<Item = OperationId>) {
    let window_id = state.lock().ok().map(|app| app.active_window);
    if let Some(window_id) = window_id {
        for id in ids {
            let _ = actions::cancel(state, window_id, id);
        }
    }
    refresh_operation_badges(state);
}

fn active_write_target(app: &AppState, window_id: WindowId) -> Result<(TabId, PathBuf), String> {
    let window = app
        .window(window_id)
        .ok_or_else(|| "window is unavailable".to_owned())?;
    let tab_id = window.active_tab;
    let tab = app
        .tab(tab_id)
        .ok_or_else(|| "tab is unavailable".to_owned())?;
    match tab.visible_location() {
        Some(NavigationLocation::Home) => Err(match app.language {
            Language::Chinese => "Home 页面不能执行文件写入".to_owned(),
            Language::English => "File writes are unavailable on Home".to_owned(),
        }),
        Some(NavigationLocation::Directory(path)) => Ok((tab_id, path.clone())),
        Some(NavigationLocation::WebDav(_)) => Err(match app.language {
            Language::Chinese => "WebDAV 位置不能执行本地写入".to_owned(),
            Language::English => "Local writes are unavailable on a WebDAV location".to_owned(),
        }),
        Some(NavigationLocation::Library(library)) => app
            .libraries
            .iter()
            .find(|candidate| candidate.id.as_os_str() == library.identity.as_os_str())
            .and_then(|candidate| candidate.default_save_path.clone())
            .map(|path| (tab_id, path))
            .ok_or_else(|| {
                Texts::new(app.language)
                    .library_no_default_save_location()
                    .to_owned()
            }),
        None => Err(Texts::new(app.language).library_unavailable().to_owned()),
    }
}
fn create_default_folder(state: &WindowSessions, sender: &mpsc::Sender<FileOperationRequest>) {
    let target = state.lock().ok().and_then(|mut app| {
        let name = match app.language {
            Language::Chinese => "新建文件夹",
            Language::English => "New folder",
        };
        match active_write_target(&app, state.window_id) {
            Ok((tab_id, parent)) => Some((tab_id, parent.join(name))),
            Err(message) => {
                app.operation_errors.push(message);
                None
            }
        }
    });
    if let Some((tab_id, path)) = target {
        let _ = enqueue_operation(
            &state.shared,
            sender,
            tab_id,
            FileOperationKind::CreateFolder,
            vec![OperationItem::pending(None, Some(path))],
            None,
        );
    }
}
fn request_clipboard_write(
    state: &WindowSessions,
    sender: &mpsc::Sender<ClipboardRequest>,
    cut: bool,
) {
    let paths = state
        .lock()
        .map(|app| {
            app.window(state.window_id)
                .and_then(|window| window.tabs.get(&window.active_tab))
                .map(selected_paths_for_tab)
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if !paths.is_empty() {
        let _ = sender.send(ClipboardRequest::Write { paths, cut });
    }
}

fn pending_clipboard_cut(app: &AppState) -> Option<(Vec<PathBuf>, u32)> {
    (!app.cut_paths.is_empty()).then(|| {
        app.clipboard_sequence
            .map(|sequence| (app.cut_paths.clone(), sequence))
    })?
}

fn request_cancel_clipboard_cut(
    state: &WindowSessions,
    sender: &mpsc::Sender<ClipboardRequest>,
) -> bool {
    let pending = state
        .lock()
        .ok()
        .and_then(|app| pending_clipboard_cut(&app));
    if let Some((paths, sequence)) = pending {
        sender
            .send(ClipboardRequest::CancelCut { paths, sequence })
            .is_ok()
    } else {
        false
    }
}

fn request_clipboard_paste(state: &WindowSessions, sender: &mpsc::Sender<ClipboardRequest>) {
    let target =
        state
            .lock()
            .ok()
            .and_then(|mut app| match active_write_target(&app, state.window_id) {
                Ok(target) => Some(target),
                Err(message) => {
                    app.operation_errors.push(message);
                    None
                }
            });
    if let Some((origin_tab, target)) = target {
        let _ = sender.send(ClipboardRequest::ReadPaste { origin_tab, target });
    }
}
fn set_view_mode(state: &WindowSessions, mode: ViewMode) {
    if let Ok(mut app) = state.lock() {
        let tab_id = app.active_window_state().active_tab;
        if app.active().page_source == PageSource::Search {
            app.search_view.view_mode = mode;
        } else if let Some(path) = app.active().visible_path().map(Path::to_path_buf) {
            app.update_directory_preference(path, |preference| preference.view_mode = mode);
        }
        app.thumbnail_failures
            .retain(|(request_tab, _, _, _), _| *request_tab != tab_id);
    }
}

fn set_sort_field(state: &WindowSessions, field: SortField) {
    if let Ok(mut app) = state.lock() {
        if app.active().page_source == PageSource::Search {
            let direction = app.search_view.sort_direction;
            app.search_view.sort_field = field;
            for window in app.windows.values_mut() {
                for tab in window
                    .tabs
                    .values_mut()
                    .filter(|tab| tab.page_source == PageSource::Search)
                {
                    tab.search_sort_field = field;
                    tab.search_sort_direction = direction;
                }
            }
        } else if let Some(path) = app.active().visible_path().map(Path::to_path_buf) {
            let direction = app.directory_preference(&path).sort_direction;
            app.update_directory_preference(path.clone(), |preference| {
                preference.sort_field = field
            });
            for window in app.windows.values_mut() {
                for tab in window.tabs.values_mut().filter(|tab| {
                    tab.page_source == PageSource::Directory
                        && tab.visible_path() == Some(path.as_path())
                }) {
                    tab.sort_field = field;
                    tab.sort_direction = direction;
                    tab.resort_entries();
                }
            }
        } else if matches!(
            app.active().visible_location(),
            Some(NavigationLocation::Library(_))
        ) {
            let tab_id = app.active_window_state().active_tab;
            let tab = app.tab_mut(tab_id).expect("active tab exists");
            tab.sort_field = field;
            tab.resort_entries();
        }
    }
}

fn set_sort_direction(state: &WindowSessions, direction: SortDirection) {
    if let Ok(mut app) = state.lock() {
        if app.active().page_source == PageSource::Search {
            app.search_view.sort_direction = direction;
            for window in app.windows.values_mut() {
                for tab in window
                    .tabs
                    .values_mut()
                    .filter(|tab| tab.page_source == PageSource::Search)
                {
                    tab.search_sort_direction = direction;
                }
            }
        } else if let Some(path) = app.active().visible_path().map(Path::to_path_buf) {
            app.update_directory_preference(path.clone(), |preference| {
                preference.sort_direction = direction
            });
            for window in app.windows.values_mut() {
                for tab in window.tabs.values_mut().filter(|tab| {
                    tab.page_source == PageSource::Directory
                        && tab.visible_path() == Some(path.as_path())
                }) {
                    tab.sort_direction = direction;
                    tab.resort_entries();
                }
            }
        } else if matches!(
            app.active().visible_location(),
            Some(NavigationLocation::Library(_))
        ) {
            let tab_id = app.active_window_state().active_tab;
            let tab = app.tab_mut(tab_id).expect("active tab exists");
            tab.sort_direction = direction;
            tab.resort_entries();
        }
    }
}

fn set_group(state: &WindowSessions, field: GroupField, direction: Option<SortDirection>) -> bool {
    let Ok(mut app) = state.lock() else {
        return false;
    };
    if app.active().page_source == PageSource::Search {
        return false;
    }
    let Some(path) = app.active().visible_path().map(Path::to_path_buf) else {
        return false;
    };
    app.update_directory_preference(path, |preference| {
        preference.group_field = field;
        if let Some(direction) = direction {
            preference.group_direction = direction;
        }
    });
    true
}

fn apply_group_command(state: &WindowSessions, command: i32) -> bool {
    match command {
        command if (CMD_GROUP_BASE..CMD_GROUP_BASE + 6).contains(&command) => {
            GroupField::from_storage_code((command - CMD_GROUP_BASE) as u8)
                .is_some_and(|field| set_group(state, field, None))
        }
        CMD_GROUP_ASC | CMD_GROUP_DESC => {
            let field = state
                .lock()
                .ok()
                .and_then(|app| {
                    app.active()
                        .visible_path()
                        .map(|path| app.directory_preference(path).group_field)
                })
                .unwrap_or(GroupField::None);
            set_group(
                state,
                field,
                Some(if command == CMD_GROUP_ASC {
                    SortDirection::Ascending
                } else {
                    SortDirection::Descending
                }),
            )
        }
        _ => false,
    }
}

fn fitted_column_width(app: &AppState, kind: ColumnKind) -> u32 {
    let header = match (app.language, kind) {
        (Language::Chinese, ColumnKind::Name) => "名称",
        (Language::English, ColumnKind::Name) => "Name",
        (Language::Chinese, ColumnKind::Kind) => "类型",
        (Language::English, ColumnKind::Kind) => "Type",
        (Language::Chinese, ColumnKind::Size) => "大小",
        (Language::English, ColumnKind::Size) => "Size",
        (Language::Chinese, ColumnKind::Modified) => "修改时间",
        (Language::English, ColumnKind::Modified) => "Date modified",
        (Language::Chinese, ColumnKind::Created) => "创建时间",
        (Language::English, ColumnKind::Created) => "Date created",
    };
    let texts = Texts::new(app.language);
    let max_chars = app
        .active()
        .visible_entries()
        .iter()
        .map(|entry| match kind {
            ColumnKind::Name => entry.display_name.chars().count(),
            ColumnKind::Kind => entry
                .path
                .extension()
                .map(|value| value.to_string_lossy().chars().count() + 5)
                .unwrap_or(6),
            ColumnKind::Size => texts.size(entry.size_bytes).chars().count(),
            ColumnKind::Modified => texts.modified(entry.modified).chars().count(),
            ColumnKind::Created => texts.modified(entry.created).chars().count(),
        })
        .chain(std::iter::once(header.chars().count()))
        .max()
        .unwrap_or(8);
    let padding = if kind == ColumnKind::Name { 54 } else { 28 };
    (max_chars as u32 * 8 + padding).clamp(
        crate::domain::MIN_COLUMN_WIDTH,
        crate::domain::MAX_COLUMN_WIDTH,
    )
}
fn begin_rename_ui(weak: &slint::Weak<AppWindow>, state: &WindowSessions) {
    let target = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.active().selected.len() != 1 {
            return;
        }
        let id = app.active().selected[0];
        let entry = app.active().visible_entry(id).cloned();
        let rename = entry.as_ref().map(rename_input_for_entry);
        let tab_id = app.active_window_state().active_tab;
        let request_id = app.active().latest_request();
        app.rename_targets.insert(
            state.window_id,
            actions::EntryTarget {
                tab: actions::ActionTarget {
                    window_id: state.window_id,
                    tab_id,
                    request_id,
                },
                entry_id: id,
            },
        );
        rename.map(|(input, selection_end)| (id, input, selection_end))
    };
    if let Some((id, input, selection_end)) = target
        && let Some(ui) = weak.upgrade()
    {
        ui.set_rename_entry_id(id.0 as i32);
        ui.set_rename_input(input.into());
        ui.set_rename_selection_end(selection_end as i32);
        ui.set_rename_editing(true);
    }
}

fn rename_validation_message(
    language: Language,
    error: crate::domain::name_validation::NameValidationError,
) -> String {
    Texts::new(language).rename_validation(error)
}

fn submit_rename(
    state: &WindowSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    name: &str,
) -> Result<(), String> {
    let (target, language) = {
        let app = state
            .shared
            .lock()
            .expect("app state mutex is not poisoned");
        crate::domain::name_validation::validate_name(std::ffi::OsStr::new(name))
            .map_err(|error| rename_validation_message(app.language, error))?;
        (
            app.rename_targets.get(&state.window_id).copied(),
            app.language,
        )
    };
    let target = target.ok_or_else(|| {
        Texts::new(language)
            .action_error("entry-unavailable")
            .to_owned()
    })?;
    actions::rename(&state.shared, sender, target, name.into())
        .map(|_| ())
        .map_err(|error| Texts::new(language).action_error(error.code()).to_owned())
}

fn pending_permanent_delete(state: &WindowSessions) -> Option<(TabId, Vec<OperationItem>)> {
    state.lock().ok().and_then(|app| {
        let window = app.window(state.window_id)?;
        let tab = window.tabs.get(&window.active_tab)?;
        let items = selected_paths_for_tab(tab)
            .into_iter()
            .map(|path| OperationItem::pending(Some(path), None))
            .collect::<Vec<_>>();
        (!items.is_empty()).then_some((tab.id, items))
    })
}

fn request_permanent_delete(
    state: &WindowSessions,
    ui: &AppWindow,
    delete_ui: &ConfirmationWindow,
) {
    let pending = pending_permanent_delete(state);
    if pending.is_none() {
        return;
    }
    if let Ok(mut app) = state.lock() {
        app.pending_permanent_delete = pending;
    }
    platform::windows::window_trace::log_diagnostic(
        "permanent-delete-confirmation-requested",
        &format!("window={}", state.window_id.0),
    );
    show_confirmation_window(ui, None, delete_ui);
}

fn context_delete_request(command: i32, shift_down: bool) -> Option<bool> {
    (command == 6).then_some(shift_down)
}

fn submit_delete_items(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    origin_tab: TabId,
    permanent: bool,
    items: Vec<OperationItem>,
) {
    let kind = if permanent {
        FileOperationKind::PermanentDelete
    } else {
        FileOperationKind::RecycleDelete
    };
    let _ = enqueue_operation(state, sender, origin_tab, kind, items, None);
}

fn submit_delete(
    state: &WindowSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    permanent: bool,
) {
    if let Some((origin_tab, items)) = pending_permanent_delete(state) {
        submit_delete_items(&state.shared, sender, origin_tab, permanent, items);
    }
}

const SHELL_CONTEXT_COMMAND_BASE: i32 = 100_000;
const CMD_REFRESH: i32 = 20;
const CMD_VIEW_BASE: i32 = 100;
const CMD_SORT_BASE: i32 = 120;
const CMD_SORT_ASC: i32 = 130;
const CMD_SORT_DESC: i32 = 131;
const CMD_GROUP_BASE: i32 = 140;
const CMD_GROUP_ASC: i32 = 150;
const CMD_GROUP_DESC: i32 = 151;
const CMD_COLUMN_FIT: i32 = 160;
const CMD_COLUMNS_FIT: i32 = 161;
const CMD_COLUMN_TOGGLE_BASE: i32 = 170;
const CMD_ADD_NETWORK_LOCATION: i32 = 190;
const CMD_NETWORK_LOCATION_OPEN: i32 = 191;
const CMD_NETWORK_LOCATION_COPY_ADDRESS: i32 = 192;
const CMD_NETWORK_LOCATION_REMOVE: i32 = 193;
const CMD_NETWORK_LOCATION_MOVE_UP: i32 = 194;
const CMD_NETWORK_LOCATION_MOVE_DOWN: i32 = 195;
const CMD_NETWORK_LOCATION_OPEN_NEW_TAB: i32 = 196;
const CMD_NETWORK_LOCATION_MANAGE_CREDENTIALS: i32 = 197;
const CMD_NETWORK_LOCATION_RENAME: i32 = 198;
const CMD_QUICK_ACCESS_PIN: i32 = 199;
const CMD_QUICK_ACCESS_UNPIN: i32 = 200;
const CMD_SIDEBAR_VISIBILITY_BASE: i32 = 210;
const CMD_OPEN_FILE_LOCATION: i32 = 220;
const NODE_VIEW: i32 = 10_001;
const NODE_SORT: i32 = 10_002;
const NODE_GROUP: i32 = 10_003;
const NODE_COLUMNS: i32 = 10_004;
const NODE_CREATE: i32 = 10_005;
const QUICK_MENU_PLACEHOLDER_ROWS: usize = 3;
const QUICK_MENU_SNAPSHOT_TTL: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum QuickMenuScope {
    Content {
        paths: Vec<PathBuf>,
        location: Option<platform::windows::context_menu::ShellMenuBackgroundTarget>,
    },
    Sidebar {
        group: i32,
        stable_id: String,
        generation: u64,
        target: platform::windows::context_menu::ShellMenuItemTarget,
    },
    SidebarBackground,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct QuickMenuKey {
    window_id: WindowId,
    tab_id: TabId,
    navigation_request: RequestId,
    scope: QuickMenuScope,
}

#[derive(Clone)]
struct QuickMenuIdentity {
    session_id: u64,
    request_id: u64,
    key: QuickMenuKey,
    ready: bool,
}

#[derive(Clone)]
struct QuickMenuSnapshot {
    rows: Vec<ContextCommandRow>,
    captured_at: Instant,
}

#[derive(Default)]
struct QuickMenuState {
    next_request: u64,
    identity: Option<QuickMenuIdentity>,
    snapshots: HashMap<QuickMenuKey, QuickMenuSnapshot>,
    all_rows: Vec<ContextCommandRow>,
    built_in_rows: Vec<ContextCommandRow>,
    submenu_rows: Vec<ContextCommandRow>,
    submenu_history: Vec<(u64, Vec<ContextCommandRow>)>,
    submenu_tokens: HashMap<i32, u64>,
    create_submenu_token: Option<u64>,
    create_submenu_open: bool,
    built_in_key: Option<QuickMenuKey>,
    language: Option<Language>,
    preloaded_submenu_rows: HashMap<u64, Vec<ContextCommandRow>>,
    loaded_submenu_rows: HashMap<u64, Vec<ContextCommandRow>>,
    built_in_submenu_rows: HashMap<i32, Vec<ContextCommandRow>>,
    active_column: Option<ColumnKind>,
    active_network_location: Option<u64>,
    active_quick_access_path: Option<PathBuf>,
    active_sidebar_target: Option<SidebarMenuResolved>,
    next_submenu_node: i32,
    active_submenu_token: Option<u64>,
    active_submenu_request: u64,
    pending_shell_delete_permanent: bool,
}

type SharedQuickMenu = Arc<Mutex<QuickMenuState>>;

#[derive(Clone, Debug, PartialEq, Eq)]
enum SidebarDestination {
    Directory(PathBuf),
    Library(LibraryLocationId),
    External(PathBuf),
    WebDav(crate::domain::WebDavPlace),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct SidebarMenuResolved {
    group: i32,
    stable_id: String,
    generation: u64,
    target: platform::windows::context_menu::ShellMenuItemTarget,
    destination: SidebarDestination,
    quick_access_path: Option<PathBuf>,
    network_location_id: Option<u64>,
    network_location_source: Option<NetworkLocationSource>,
}

fn sidebar_menu_target(
    app: &AppState,
    window_id: WindowId,
    group: i32,
    stable_id: &str,
) -> Option<SidebarMenuResolved> {
    use platform::windows::context_menu::ShellMenuItemTarget;
    match group {
        0 | 1 => {
            let index = stable_id.strip_prefix("location:")?.parse::<usize>().ok()?;
            let location = app.sidebar.get(index)?;
            let expected_group = if location.kind == KnownLocationKind::Drive {
                1
            } else {
                0
            };
            (group == expected_group).then(|| SidebarMenuResolved {
                group,
                stable_id: stable_id.to_owned(),
                generation: app.quick_access_generation,
                target: ShellMenuItemTarget::FileSystem(location.path.clone()),
                destination: SidebarDestination::Directory(location.path.clone()),
                quick_access_path: (location.kind == KnownLocationKind::Pinned)
                    .then(|| location.path.clone()),
                network_location_id: None,
                network_location_source: None,
            })
        }
        2 => {
            let location = network_location_from_stable_id(app, stable_id)?;
            let (target, destination) = match &location.target {
                NetworkTarget::WindowsPath(path) => (
                    ShellMenuItemTarget::FileSystem(path.clone()),
                    SidebarDestination::Directory(path.clone()),
                ),
                NetworkTarget::ShellItemId(identity) => (
                    ShellMenuItemTarget::Namespace {
                        shell_identity: identity.as_os_str().to_owned(),
                    },
                    SidebarDestination::External(identity.clone()),
                ),
                NetworkTarget::WebDav(connection) => (
                    ShellMenuItemTarget::Namespace {
                        shell_identity: std::ffi::OsString::new(),
                    },
                    SidebarDestination::WebDav(crate::domain::WebDavPlace {
                        connection_id: connection.connection_id,
                        revision: connection.revision,
                        root: connection.root.clone(),
                        path: connection.root.clone(),
                    }),
                ),
            };
            Some(SidebarMenuResolved {
                group,
                stable_id: stable_id.to_owned(),
                generation: app.network_location_generation,
                target,
                destination,
                quick_access_path: None,
                network_location_id: Some(location.id),
                network_location_source: Some(location.source),
            })
        }
        3 => {
            let coordinator = app.network_discovery.get(&window_id)?;
            let index = stable_id
                .strip_prefix("network-device:")?
                .parse::<usize>()
                .ok()?;
            let device = coordinator.devices().get(index)?;
            let path = crate::network::device_root_target(device)?;
            Some(SidebarMenuResolved {
                group,
                stable_id: stable_id.to_owned(),
                generation: coordinator.generation().0,
                target: ShellMenuItemTarget::FileSystem(path.clone()),
                destination: SidebarDestination::Directory(path),
                quick_access_path: None,
                network_location_id: None,
                network_location_source: None,
            })
        }
        4 => {
            let library = app
                .libraries
                .iter()
                .find(|library| library_stable_id(library.id.as_os_str()) == stable_id)?;
            Some(SidebarMenuResolved {
                group,
                stable_id: stable_id.to_owned(),
                generation: app.library_generation,
                target: ShellMenuItemTarget::Namespace {
                    shell_identity: library.id.as_os_str().to_owned(),
                },
                destination: SidebarDestination::Library(LibraryLocationId::new(
                    library.id.as_os_str().to_owned(),
                    library.display_name.to_string_lossy().into_owned(),
                )),
                quick_access_path: None,
                network_location_id: None,
                network_location_source: None,
            })
        }
        _ => None,
    }
}
fn current_sidebar_menu_target(
    state: &WindowSessions,
    menu: &SharedQuickMenu,
) -> Option<SidebarMenuResolved> {
    let menu = menu.lock().ok()?;
    let identity = menu.identity.as_ref()?;
    matches!(identity.key.scope, QuickMenuScope::Sidebar { .. }).then_some(())?;
    quick_menu_key_is_current(&state.shared, &identity.key).then_some(())?;
    menu.active_sidebar_target.clone()
}
fn sidebar_context_rows(
    target: &SidebarMenuResolved,
    language: Language,
    quick_access_pending: bool,
) -> Vec<ContextCommandRow> {
    let text = |chinese: &'static str, english: &'static str| {
        if language == Language::Chinese {
            chinese
        } else {
            english
        }
    };
    let mut rows = vec![
        quick_menu_row(
            CMD_NETWORK_LOCATION_OPEN,
            0,
            text("打开", "Open"),
            true,
            false,
            false,
        ),
        quick_menu_row(
            CMD_NETWORK_LOCATION_OPEN_NEW_TAB,
            0,
            text("在新标签页中打开", "Open in new tab"),
            !matches!(target.destination, SidebarDestination::External(_)),
            false,
            false,
        ),
    ];
    if !matches!(target.destination, SidebarDestination::Library(_)) {
        rows.push(quick_menu_row(
            CMD_NETWORK_LOCATION_COPY_ADDRESS,
            0,
            text("复制路径", "Copy path"),
            true,
            false,
            false,
        ));
    }
    if target.quick_access_path.is_some() {
        rows.extend([
            quick_menu_separator(),
            quick_menu_row(
                CMD_QUICK_ACCESS_UNPIN,
                0,
                text("从快速访问取消固定", "Unpin from Quick access"),
                !quick_access_pending,
                false,
                false,
            ),
        ]);
    }
    if let Some(source) = target.network_location_source {
        rows.extend([
            quick_menu_separator(),
            quick_menu_row(
                CMD_NETWORK_LOCATION_RENAME,
                0,
                text("重命名", "Rename"),
                true,
                false,
                false,
            ),
        ]);
        if matches!(target.destination, SidebarDestination::WebDav(_)) {
            rows.push(quick_menu_row(
                webdav_ui::CMD_EDIT_WEBDAV_LOCATION,
                0,
                text("编辑连接", "Edit connection"),
                true,
                false,
                false,
            ));
        }
        rows.push(quick_menu_row(
            CMD_NETWORK_LOCATION_REMOVE,
            0,
            text("移除", "Remove"),
            true,
            false,
            false,
        ));
        if source == NetworkLocationSource::AsterOwned {
            rows.extend([
                quick_menu_row(
                    CMD_NETWORK_LOCATION_MOVE_UP,
                    0,
                    text("上移", "Move up"),
                    true,
                    false,
                    false,
                ),
                quick_menu_row(
                    CMD_NETWORK_LOCATION_MOVE_DOWN,
                    0,
                    text("下移", "Move down"),
                    true,
                    false,
                    false,
                ),
            ]);
        }
    }
    if target.network_location_id.is_some() {
        rows.extend([
            quick_menu_separator(),
            quick_menu_row(
                CMD_NETWORK_LOCATION_MANAGE_CREDENTIALS,
                0,
                text("管理 Windows 凭据", "Manage Windows credentials"),
                true,
                false,
                false,
            ),
        ]);
    }
    rows
}
fn sidebar_visibility_rows(
    visibility: session_store::SidebarVisibility,
    language: Language,
) -> Vec<ContextCommandRow> {
    let labels = Texts::new(language).sidebar_categories();
    labels
        .into_iter()
        .enumerate()
        .map(|(index, label)| {
            quick_menu_row(
                CMD_SIDEBAR_VISIBILITY_BASE + index as i32,
                0,
                label,
                true,
                visibility.is_visible(index),
                false,
            )
        })
        .collect()
}

fn toggle_sidebar_visibility(
    visibility: &mut session_store::SidebarVisibility,
    command: i32,
) -> bool {
    let Ok(index) = usize::try_from(command - CMD_SIDEBAR_VISIBILITY_BASE) else {
        return false;
    };
    visibility.toggle(index)
}

fn selected_network_location_path(
    state: &WindowSessions,
    menu: &SharedQuickMenu,
) -> Option<PathBuf> {
    let id = menu.lock().ok()?.active_network_location?;
    let app = state.lock().ok()?;
    app.imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .find(|location| location.id == id)
        .and_then(|location| match &location.target {
            NetworkTarget::WindowsPath(path) => Some(path.clone()),
            NetworkTarget::ShellItemId(_) | NetworkTarget::WebDav(_) => None,
        })
}

fn selected_network_location(
    state: &WindowSessions,
    menu: &SharedQuickMenu,
) -> Option<NetworkLocation> {
    let id = menu.lock().ok()?.active_network_location?;
    let app = state.lock().ok()?;
    app.imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .find(|location| location.id == id)
        .cloned()
}
fn selected_network_location_target(
    state: &WindowSessions,
    menu: &SharedQuickMenu,
) -> Option<NetworkTarget> {
    let id = menu.lock().ok()?.active_network_location?;
    let app = state.lock().ok()?;
    app.imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .find(|location| location.id == id)
        .map(|location| location.target.clone())
}

fn quick_menu_separator() -> ContextCommandRow {
    ContextCommandRow {
        id: -1,
        node_id: 0,
        label: "".into(),
        search_text: "".into(),
        hint: "".into(),
        enabled: false,
        separator: true,
        shell: true,
        checked: false,
        default: false,
        submenu: false,
        loading: false,
        placeholder: false,
        icon_kind: 0,
    }
}

fn quick_menu_placeholder() -> ContextCommandRow {
    ContextCommandRow {
        id: -1,
        node_id: 0,
        label: "".into(),
        search_text: "".into(),
        hint: "".into(),
        enabled: false,
        separator: false,
        shell: true,
        checked: false,
        default: false,
        submenu: false,
        loading: true,
        placeholder: true,
        icon_kind: 0,
    }
}

fn pending_shell_rows(rows: &[ContextCommandRow]) -> Vec<ContextCommandRow> {
    rows.iter()
        .cloned()
        .map(|mut row| {
            if !row.separator {
                row.enabled = false;
                row.loading = true;
            }
            row
        })
        .collect()
}

fn compose_quick_menu_rows(
    built_in_rows: &[ContextCommandRow],
    shell_rows: &[ContextCommandRow],
) -> Vec<ContextCommandRow> {
    let mut rows = built_in_rows.to_vec();
    if !rows.is_empty() && !shell_rows.is_empty() {
        rows.push(quick_menu_separator());
    }
    rows.extend(shell_rows.iter().cloned());
    filtered_context_rows(&rows, "")
}

fn project_cached_shell_rows(
    session_ready: bool,
    snapshot: Option<&QuickMenuSnapshot>,
) -> (Vec<ContextCommandRow>, bool, bool, bool) {
    let snapshot =
        snapshot.filter(|snapshot| snapshot.captured_at.elapsed() <= QUICK_MENU_SNAPSHOT_TTL);
    let cache_hit = snapshot.is_some();
    let session_hit = session_ready && cache_hit;
    let rows = match (session_hit, snapshot) {
        (true, Some(snapshot)) => snapshot.rows.clone(),
        (false, Some(snapshot)) => pending_shell_rows(&snapshot.rows),
        (_, None) => (0..QUICK_MENU_PLACEHOLDER_ROWS)
            .map(|_| quick_menu_placeholder())
            .collect(),
    };
    (rows, !session_hit, cache_hit, session_hit)
}

fn quick_menu_location_target(
    app: &AppState,
    tab: &TabSession,
) -> Option<platform::windows::context_menu::ShellMenuBackgroundTarget> {
    match tab.visible_location()? {
        NavigationLocation::Home => None,
        NavigationLocation::Directory(path) => Some(
            platform::windows::context_menu::ShellMenuBackgroundTarget::FileSystem(path.clone()),
        ),
        NavigationLocation::Library(library) => Some(
            platform::windows::context_menu::ShellMenuBackgroundTarget::Namespace {
                shell_identity: library.identity.clone(),
                working_directory: app
                    .libraries
                    .iter()
                    .find(|candidate| candidate.id.as_os_str() == library.identity.as_os_str())
                    .and_then(|candidate| candidate.default_save_path.clone()),
            },
        ),
        NavigationLocation::WebDav(_) => None,
    }
}

fn quick_menu_key(
    state: &WindowSessions,
    background: bool,
) -> Option<(
    QuickMenuKey,
    platform::windows::context_menu::ShellMenuLoadTarget,
)> {
    let app = state.lock().ok()?;
    let tab = app.active();
    let paths = if background {
        Vec::new()
    } else {
        selected_paths(&app)
    };
    let location = quick_menu_location_target(&app, tab);
    if background && location.is_none() || !background && paths.is_empty() {
        return None;
    }
    let key = QuickMenuKey {
        window_id: state.window_id,
        tab_id: tab.id,
        navigation_request: tab.latest_request(),
        scope: QuickMenuScope::Content {
            paths: paths.clone(),
            location: location.clone(),
        },
    };
    let target = if background {
        platform::windows::context_menu::ShellMenuLoadTarget::Background(location?)
    } else {
        platform::windows::context_menu::ShellMenuLoadTarget::Paths(paths)
    };
    Some((key, target))
}

fn apply_loaded_shell_menu(
    menu: &mut QuickMenuState,
    session_id: u64,
    request_id: u64,
    items: Vec<platform::windows::context_menu::ClassicMenuItem>,
) -> bool {
    let Some(key) = menu.identity.as_ref().and_then(|identity| {
        (identity.session_id == session_id && identity.request_id == request_id)
            .then(|| identity.key.clone())
    }) else {
        return false;
    };
    menu.submenu_tokens.clear();
    menu.create_submenu_token = None;
    menu.preloaded_submenu_rows.clear();
    menu.loaded_submenu_rows.clear();
    menu.next_submenu_node = 0;
    let projected = project_shell_menu_items(menu, items, None);
    menu.snapshots.insert(
        key,
        QuickMenuSnapshot {
            rows: projected.clone(),
            captured_at: Instant::now(),
        },
    );
    if let Some(current) = menu.identity.as_mut() {
        current.ready = true;
    }
    menu.all_rows = compose_quick_menu_rows(&menu.built_in_rows, &projected);
    true
}
fn clear_current_shell_menu_failure(
    menu: &mut QuickMenuState,
    session_id: u64,
    request_id: u64,
) -> bool {
    if !menu.identity.as_ref().is_some_and(|identity| {
        identity.session_id == session_id && identity.request_id == request_id
    }) {
        return false;
    }
    menu.identity = None;
    menu.all_rows = menu.built_in_rows.clone();
    menu.submenu_tokens.clear();
    menu.create_submenu_token = None;
    menu.preloaded_submenu_rows.clear();
    menu.loaded_submenu_rows.clear();
    true
}
fn quick_menu_key_is_current(state: &SharedSessions, key: &QuickMenuKey) -> bool {
    state.lock().ok().is_some_and(|app| {
        if app.window_for_tab(key.tab_id) != Some(key.window_id) {
            return false;
        }
        let Some(window) = app.window(key.window_id) else {
            return false;
        };
        if window.active_tab != key.tab_id {
            return false;
        }
        let Some(tab) = app.tab(key.tab_id) else {
            return false;
        };
        if tab.latest_request() != key.navigation_request {
            return false;
        }
        match &key.scope {
            QuickMenuScope::Content { paths, location } => {
                quick_menu_location_target(&app, tab) == *location
                    && if paths.is_empty() {
                        tab.selected.is_empty()
                    } else {
                        *paths == selected_paths_for_tab(tab)
                    }
            }
            QuickMenuScope::Sidebar {
                group,
                stable_id,
                generation,
                target,
            } => {
                sidebar_menu_target(&app, key.window_id, *group, stable_id).is_some_and(|current| {
                    current.generation == *generation && current.target == *target
                })
            }
            QuickMenuScope::SidebarBackground => true,
        }
    })
}
fn context_row_matches(
    row: &ContextCommandRow,
    matcher: &ib_pinyin::matcher::PinyinMatcher<'_>,
) -> bool {
    !row.separator
        && (matcher.is_match(row.label.as_str()) || matcher.is_match(row.search_text.as_str()))
}

fn filtered_context_rows(rows: &[ContextCommandRow], query: &str) -> Vec<ContextCommandRow> {
    let query = query.trim();
    let matcher = (!query.is_empty()).then(|| {
        ib_pinyin::matcher::PinyinMatcher::builder(query)
            .pinyin_case_insensitive(true)
            .build()
    });
    let mut result = Vec::new();
    let mut pending_separator = false;
    for row in rows {
        if row.separator {
            pending_separator = !result.is_empty();
            continue;
        }
        if let Some(matcher) = &matcher
            && !context_row_matches(row, matcher)
        {
            continue;
        }
        if pending_separator && !result.is_empty() {
            result.push(ContextCommandRow {
                id: -1,
                node_id: 0,
                label: "".into(),
                search_text: "".into(),
                hint: "".into(),
                enabled: false,
                separator: true,
                shell: false,
                checked: false,
                default: false,
                submenu: false,
                loading: false,
                placeholder: false,
                icon_kind: 0,
            });
        }
        pending_separator = false;
        result.push(row.clone());
    }
    result
}

fn first_enabled_context_index(rows: &[ContextCommandRow]) -> i32 {
    rows.iter()
        .position(|row| row.enabled && !row.separator)
        .map_or(-1, |index| index as i32)
}

fn next_enabled_context_index(rows: &[ContextCommandRow], current: i32, direction: i32) -> i32 {
    let enabled = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| (row.enabled && !row.separator).then_some(index))
        .collect::<Vec<_>>();
    if enabled.is_empty() {
        return -1;
    }
    let position = enabled
        .iter()
        .position(|index| *index == current.max(0) as usize)
        .unwrap_or(0);
    let next = if direction < 0 {
        position.checked_sub(1).unwrap_or(enabled.len() - 1)
    } else {
        (position + 1) % enabled.len()
    };
    enabled[next] as i32
}

fn shell_menu_item_row(
    item: platform::windows::context_menu::ClassicMenuItem,
    node_id: i32,
    hide_content_built_ins: bool,
) -> Option<ContextCommandRow> {
    use platform::windows::context_menu::ClassicMenuItemKind;
    if hide_content_built_ins
        && item
            .verb
            .as_deref()
            .is_some_and(|verb| matches!(verb, "cut" | "copy" | "paste" | "delete" | "rename"))
    {
        return None;
    }
    let separator = matches!(&item.kind, ClassicMenuItemKind::Separator);
    let submenu = matches!(&item.kind, ClassicMenuItemKind::Submenu { .. });
    Some(ContextCommandRow {
        id: item
            .command_id
            .map_or(-1, |id| SHELL_CONTEXT_COMMAND_BASE + id as i32),
        node_id,
        label: item.title.into(),
        search_text: item.verb.unwrap_or_default().into(),
        hint: "".into(),
        enabled: item.enabled && !separator && (submenu || item.command_id.is_some()),
        separator,
        shell: true,
        checked: item.checked,
        default: item.default,
        submenu,
        loading: false,
        placeholder: false,
        icon_kind: 0,
    })
}

fn merges_shell_new_menu(menu: &QuickMenuState) -> bool {
    menu.identity.as_ref().is_some_and(|identity| {
        matches!(
            &identity.key.scope,
            QuickMenuScope::Content {
                paths,
                location: Some(_),
            } if paths.is_empty()
        )
    })
}
fn project_shell_menu_items(
    menu: &mut QuickMenuState,
    items: Vec<platform::windows::context_menu::ClassicMenuItem>,
    parent_token: Option<u64>,
) -> Vec<ContextCommandRow> {
    use platform::windows::context_menu::ClassicMenuItemKind;
    let create_submenu = parent_token.is_some_and(|token| menu.create_submenu_token == Some(token));
    let mut projected = if create_submenu {
        vec![built_in_folder_row(
            menu.language.unwrap_or(Language::English),
        )]
    } else {
        Vec::new()
    };
    projected.extend(items.into_iter().filter_map(|item| {
        if parent_token.is_none()
            && merges_shell_new_menu(menu)
            && item
                .verb
                .as_deref()
                .is_some_and(|verb| verb.eq_ignore_ascii_case("new"))
        {
            if let ClassicMenuItemKind::Submenu { token, items } = &item.kind {
                menu.create_submenu_token = Some(*token);
                menu.submenu_tokens.insert(NODE_CREATE, *token);
                if !items.is_empty() {
                    let rows = project_shell_menu_items(menu, items.clone(), Some(*token));
                    menu.preloaded_submenu_rows.insert(*token, rows);
                }
            }
            return None;
        }
        if create_submenu
            && item
                .verb
                .as_deref()
                .is_some_and(|verb| verb.eq_ignore_ascii_case("newfolder"))
        {
            return None;
        }
        let node_id = if let ClassicMenuItemKind::Submenu { token, items } = &item.kind {
            menu.next_submenu_node = menu.next_submenu_node.saturating_add(1).max(1);
            menu.submenu_tokens.insert(menu.next_submenu_node, *token);
            if !items.is_empty() {
                let rows = project_shell_menu_items(menu, items.clone(), Some(*token));
                menu.preloaded_submenu_rows.insert(*token, rows);
            }
            menu.next_submenu_node
        } else {
            0
        };
        let hide_content_built_ins = menu
            .identity
            .as_ref()
            .is_none_or(|identity| matches!(identity.key.scope, QuickMenuScope::Content { .. }));
        shell_menu_item_row(item, node_id, hide_content_built_ins)
    }));
    filtered_context_rows(&projected, "")
}

fn project_context_submenu(ui: &AppWindow, menu: &SharedQuickMenu) {
    let rows = menu
        .lock()
        .map(|menu| menu.submenu_rows.clone())
        .unwrap_or_default();
    ui.set_context_submenu_active_index(first_enabled_context_index(&rows));
    ui.set_context_submenu_content_height(context_menu_content_height(&rows));
    ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
    if let Some(window_id) = window_id_for_ui(ui) {
        update_open_submenu_projection(window_id);
    }
}

fn context_menu_content_height(rows: &[ContextCommandRow]) -> f32 {
    rows.iter()
        .map(|row| if row.separator { 7.0 } else { 24.0 })
        .sum()
}

fn cached_submenu_rows(menu: &QuickMenuState, token: u64) -> Option<Vec<ContextCommandRow>> {
    let mut rows = menu
        .loaded_submenu_rows
        .get(&token)
        .or_else(|| menu.preloaded_submenu_rows.get(&token))?
        .clone();
    if menu.create_submenu_token == Some(token)
        && let Some(folder) = rows.first_mut().filter(|row| row.id == 1 && !row.shell)
    {
        folder.label = Texts::new(menu.language.unwrap_or(Language::English))
            .folder()
            .into();
    }
    Some(rows)
}
fn submenu_result_matches(
    menu: &QuickMenuState,
    session_id: u64,
    request_id: u64,
    submenu_request_id: u64,
    token: u64,
) -> bool {
    menu.identity.as_ref().is_some_and(|identity| {
        identity.session_id == session_id && identity.request_id == request_id
    }) && menu.active_submenu_request == submenu_request_id
        && menu.active_submenu_token == Some(token)
}

fn submenu_request_is_duplicate(
    submenu_is_open: bool,
    active_token: Option<u64>,
    requested_token: u64,
) -> bool {
    submenu_is_open && active_token == Some(requested_token)
}

fn wire_context_submenu_hover(ui: &AppWindow) -> Rc<slint::Timer> {
    let timer = Rc::new(slint::Timer::default());
    let timer_for_hover = timer.clone();
    let weak = ui.as_weak();
    ui.on_hover_context_submenu(move |encoded_index| {
        timer_for_hover.stop();
        if encoded_index == i32::MIN + 1 {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_close_context_submenu();
            }
            return;
        }
        let weak = weak.clone();
        timer_for_hover.start(
            slint::TimerMode::SingleShot,
            Duration::from_millis(250),
            move || {
                if let Some(ui) = weak.upgrade() {
                    let window_id = window_id_for_ui(&ui);
                    trace_quick_menu(
                        "quick_menu_hover_timer_fired",
                        format!(
                            "window={:?} encoded_index={} menu_open={}",
                            window_id.map(|window_id| window_id.0),
                            encoded_index,
                            ui.get_context_menu_open(),
                        ),
                    );
                    ui.invoke_open_context_submenu(encoded_index);
                    if let Some(window_id) = window_id {
                        let parent_depth = (encoded_index < 0).then(|| {
                            WINDOW_RUNTIMES.with_borrow(|runtimes| {
                                runtimes
                                    .get(&window_id)
                                    .and_then(|runtime| {
                                        runtime
                                            .quick_menu_popup
                                            .session
                                            .branches()
                                            .len()
                                            .checked_sub(2)
                                    })
                                    .unwrap_or(0)
                            })
                        });
                        open_quick_submenu_popup(
                            window_id,
                            ui.get_context_submenu_anchor_y(),
                            parent_depth,
                        );
                    }
                }
            },
        );
    });
    let timer_for_cancel = timer.clone();
    ui.on_cancel_context_submenu_hover(move || timer_for_cancel.stop());
    timer
}

fn wire_quick_menu_prewarm(
    ui: &AppWindow,
    state: WindowSessions,
    menu: SharedQuickMenu,
    worker: platform::windows::context_menu::ShellMenuWorker,
) -> slint::Timer {
    let timer = slint::Timer::default();
    let weak = ui.as_weak();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(350),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_context_menu_open() {
                return;
            }
            let background = state
                .lock()
                .map(|app| app.active().selected.is_empty())
                .unwrap_or(true);
            let Some((key, _)) = quick_menu_key(&state, background) else {
                return;
            };
            let already_loaded = menu.lock().ok().is_some_and(|menu| {
                menu.identity.as_ref().is_some_and(|identity| {
                    identity.key == key
                        && (!identity.ready
                            || menu.snapshots.get(&key).is_some_and(|snapshot| {
                                snapshot.captured_at.elapsed() <= QUICK_MENU_SNAPSHOT_TTL
                            }))
                })
            });
            if !already_loaded {
                begin_shell_menu_load(&ui, &state, &menu, &worker, background);
            }
        },
    );
    timer
}
fn project_filtered_context_menu(ui: &AppWindow, menu: &SharedQuickMenu, query: &str) {
    let rows = menu
        .lock()
        .map(|menu| filtered_context_rows(&menu.all_rows, query))
        .unwrap_or_default();
    ui.set_context_active_index(first_enabled_context_index(&rows));
    ui.set_context_menu_content_height(context_menu_content_height(&rows));
    ui.set_context_commands(ModelRc::new(VecModel::from(rows)));
    if let Some(window_id) = window_id_for_ui(ui) {
        update_root_popup_projection(window_id);
    }
}

fn shell_menu_invocation_refresh_target(
    state: &SharedSessions,
    tab_id: TabId,
    target: Option<&platform::windows::context_menu::ShellMenuBackgroundTarget>,
    invocation: &platform::windows::context_menu::ClassicMenuInvocation,
) -> Option<NavigationLocation> {
    if !matches!(
        invocation,
        platform::windows::context_menu::ClassicMenuInvocation::Shell {
            refresh_directory: true,
            ..
        }
    ) {
        return None;
    }
    match target? {
        platform::windows::context_menu::ShellMenuBackgroundTarget::FileSystem(path) => {
            Some(NavigationLocation::Directory(path.clone()))
        }
        platform::windows::context_menu::ShellMenuBackgroundTarget::Namespace { .. } => {
            state.lock().ok()?.tab(tab_id)?.visible_location().cloned()
        }
    }
}
fn begin_shell_menu_load(
    ui: &AppWindow,
    state: &WindowSessions,
    menu: &SharedQuickMenu,
    worker: &platform::windows::context_menu::ShellMenuWorker,
    background: bool,
) {
    let Some((key, target)) = quick_menu_key(state, background) else {
        ui.set_context_shell_loading(false);
        return;
    };
    let identity = {
        let mut menu = menu.lock().expect("quick menu mutex is not poisoned");
        if let Some(identity) = menu.identity.as_ref()
            && identity.key == key
        {
            let snapshot_is_fresh = menu
                .snapshots
                .get(&key)
                .is_some_and(|snapshot| snapshot.captured_at.elapsed() <= QUICK_MENU_SNAPSHOT_TTL);
            if !identity.ready || snapshot_is_fresh {
                ui.set_context_shell_loading(!identity.ready);
                return;
            }
        }
        menu.next_request = menu.next_request.wrapping_add(1).max(1);
        let request_id = menu.next_request;
        let session_id = (u64::from(key.window_id.0) << 32) | request_id;
        let identity = QuickMenuIdentity {
            session_id,
            request_id,
            key,
            ready: false,
        };
        menu.identity = Some(identity.clone());
        identity
    };
    if worker
        .send(platform::windows::context_menu::ShellMenuCommand::Load {
            session_id: identity.session_id,
            request_id: identity.request_id,
            target,
            include_extended_verbs: false,
            owner_window: native_window_handle(ui),
        })
        .is_err()
    {
        if let Ok(mut menu) = menu.lock() {
            clear_current_shell_menu_failure(&mut menu, identity.session_id, identity.request_id);
        }
        ui.set_context_shell_loading(false);
        project_filtered_context_menu(ui, menu, ui.get_context_search().as_str());
        eprintln!(
            "{{\"event\":\"shell_menu_load_error\",\"session\":{},\"request\":{},\"operation\":\"dispatch\"}}",
            identity.session_id, identity.request_id,
        );
    } else {
        eprintln!(
            "{{\"event\":\"shell_menu_load_started\",\"session\":{},\"request\":{},\"window\":{},\"tab\":{},\"navigation_request\":{}}}",
            identity.session_id,
            identity.request_id,
            identity.key.window_id.0,
            identity.key.tab_id.0,
            identity.key.navigation_request.0,
        );
    }
}
fn begin_shell_menu_load_for(
    ui: &AppWindow,
    menu: &SharedQuickMenu,
    worker: &platform::windows::context_menu::ShellMenuWorker,
    key: QuickMenuKey,
    target: platform::windows::context_menu::ShellMenuLoadTarget,
) {
    let identity = {
        let mut menu = menu.lock().expect("quick menu mutex is not poisoned");
        menu.next_request = menu.next_request.wrapping_add(1).max(1);
        let request_id = menu.next_request;
        let session_id = (u64::from(key.window_id.0) << 32) | request_id;
        let identity = QuickMenuIdentity {
            session_id,
            request_id,
            key,
            ready: false,
        };
        menu.identity = Some(identity.clone());
        identity
    };
    if worker
        .send(platform::windows::context_menu::ShellMenuCommand::Load {
            session_id: identity.session_id,
            request_id: identity.request_id,
            target,
            include_extended_verbs: false,
            owner_window: native_window_handle(ui),
        })
        .is_err()
    {
        if let Ok(mut menu) = menu.lock() {
            clear_current_shell_menu_failure(&mut menu, identity.session_id, identity.request_id);
        }
        ui.set_context_shell_loading(false);
        project_filtered_context_menu(ui, menu, ui.get_context_search().as_str());
    }
}
fn built_in_folder_row(language: Language) -> ContextCommandRow {
    quick_menu_row(1, 0, Texts::new(language).folder(), true, false, false)
}

fn quick_menu_row(
    id: i32,
    node_id: i32,
    label: &str,
    enabled: bool,
    checked: bool,
    submenu: bool,
) -> ContextCommandRow {
    ContextCommandRow {
        id,
        node_id,
        label: label.into(),
        enabled,
        separator: false,
        search_text: "".into(),
        hint: "".into(),
        shell: false,
        checked,
        default: false,
        submenu,
        loading: false,
        placeholder: false,
        icon_kind: built_in_menu_icon_kind(id, node_id),
    }
}

fn built_in_menu_icon_kind(id: i32, node_id: i32) -> i32 {
    match node_id {
        NODE_VIEW => 1,
        NODE_SORT => 2,
        NODE_GROUP => 3,
        NODE_COLUMNS => 6,
        _ => match id {
            CMD_REFRESH => 4,
            1 => 5,
            _ => 0,
        },
    }
}
fn built_in_context_rows(
    app: &AppState,
    can_paste: bool,
    background: bool,
) -> (Vec<ContextCommandRow>, HashMap<i32, Vec<ContextCommandRow>>) {
    let language = app.language;
    let selected = app.active().selected.len();
    let zh = |chinese: &'static str, english: &'static str| {
        if language == Language::Chinese {
            chinese
        } else {
            english
        }
    };
    let mut submenus = HashMap::new();
    let mut rows = Vec::new();
    if background {
        let view = app.active_view_mode();
        let (sort_field, sort_direction, group_field, group_direction) =
            if app.active().page_source == PageSource::Search {
                (
                    app.search_view.sort_field,
                    app.search_view.sort_direction,
                    GroupField::None,
                    SortDirection::Ascending,
                )
            } else {
                let preference = app
                    .active()
                    .visible_path()
                    .map(|path| app.directory_preference(path))
                    .unwrap_or(app.default_directory_view);
                (
                    preference.sort_field,
                    preference.sort_direction,
                    preference.group_field,
                    preference.group_direction,
                )
            };
        rows.extend([
            quick_menu_row(-1, NODE_VIEW, zh("查看", "View"), true, false, true),
            quick_menu_row(-1, NODE_SORT, zh("排序方式", "Sort by"), true, false, true),
            quick_menu_row(
                -1,
                NODE_GROUP,
                zh("分组依据", "Group by"),
                app.active().page_source != PageSource::Search
                    && !matches!(
                        app.active().visible_location(),
                        Some(NavigationLocation::Library(_))
                    ),
                false,
                true,
            ),
            quick_menu_row(CMD_REFRESH, 0, zh("刷新", "Refresh"), true, false, false),
            quick_menu_separator(),
        ]);
        let view_labels = [
            (ViewMode::ExtraLargeIcons, "超大图标", "Extra large icons"),
            (ViewMode::LargeIcons, "大图标", "Large icons"),
            (ViewMode::MediumIcons, "中等图标", "Medium icons"),
            (ViewMode::SmallIcons, "小图标", "Small icons"),
            (ViewMode::List, "列表", "List"),
            (ViewMode::Details, "详细信息", "Details"),
            (ViewMode::Tiles, "平铺", "Tiles"),
            (ViewMode::Content, "内容", "Content"),
        ];
        submenus.insert(
            NODE_VIEW,
            view_labels
                .into_iter()
                .map(|(mode, chinese, english)| {
                    quick_menu_row(
                        CMD_VIEW_BASE + i32::from(mode.storage_code()),
                        0,
                        zh(chinese, english),
                        true,
                        view.storage_code() == mode.storage_code(),
                        false,
                    )
                })
                .collect(),
        );
        let sort_labels = [
            (SortField::Name, "名称", "Name"),
            (SortField::Modified, "修改时间", "Date modified"),
            (SortField::Created, "创建时间", "Date created"),
            (SortField::Kind, "类型", "Type"),
            (SortField::Size, "大小", "Size"),
        ];
        let mut sort_rows = sort_labels
            .into_iter()
            .map(|(field, chinese, english)| {
                quick_menu_row(
                    CMD_SORT_BASE + i32::from(field.storage_code()),
                    0,
                    zh(chinese, english),
                    true,
                    sort_field == field,
                    false,
                )
            })
            .collect::<Vec<_>>();
        sort_rows.push(quick_menu_separator());
        sort_rows.push(quick_menu_row(
            CMD_SORT_ASC,
            0,
            zh("升序", "Ascending"),
            true,
            sort_direction == SortDirection::Ascending,
            false,
        ));
        sort_rows.push(quick_menu_row(
            CMD_SORT_DESC,
            0,
            zh("降序", "Descending"),
            true,
            sort_direction == SortDirection::Descending,
            false,
        ));
        submenus.insert(NODE_SORT, sort_rows);
        let group_labels = [
            (GroupField::None, "无", "None"),
            (GroupField::Name, "名称", "Name"),
            (GroupField::Modified, "修改日期", "Date modified"),
            (GroupField::Created, "创建日期", "Date created"),
            (GroupField::Kind, "类型", "Type"),
            (GroupField::Size, "大小", "Size"),
        ];
        let enabled = app.active().page_source != PageSource::Search;
        let mut group_rows = group_labels
            .into_iter()
            .map(|(field, chinese, english)| {
                quick_menu_row(
                    CMD_GROUP_BASE + i32::from(field.storage_code()),
                    0,
                    zh(chinese, english),
                    enabled,
                    group_field == field,
                    false,
                )
            })
            .collect::<Vec<_>>();
        group_rows.push(quick_menu_separator());
        group_rows.push(quick_menu_row(
            CMD_GROUP_ASC,
            0,
            zh("分组升序", "Group ascending"),
            enabled,
            group_direction == SortDirection::Ascending,
            false,
        ));
        group_rows.push(quick_menu_row(
            CMD_GROUP_DESC,
            0,
            zh("分组降序", "Group descending"),
            enabled,
            group_direction == SortDirection::Descending,
            false,
        ));
        submenus.insert(NODE_GROUP, group_rows);
        rows.push(quick_menu_row(
            -1,
            NODE_CREATE,
            Texts::new(language).new_menu(),
            true,
            false,
            true,
        ));
        if app
            .active()
            .visible_path()
            .is_some_and(crate::network::is_unc_path)
        {
            rows.push(quick_menu_row(
                CMD_ADD_NETWORK_LOCATION,
                0,
                zh("添加到网络位置", "Add to network locations"),
                true,
                false,
                false,
            ));
        }
    } else {
        if app.active().page_source == PageSource::Search {
            rows.push(quick_menu_row(
                CMD_OPEN_FILE_LOCATION,
                0,
                Texts::new(language).open_file_location(),
                selected == 1,
                false,
                false,
            ));
            rows.push(quick_menu_separator());
        }
        let selected_path = if selected == 1 {
            app.active()
                .selected
                .first()
                .and_then(|id| app.active().visible_entry(*id))
                .filter(|entry| entry.kind == crate::domain::EntryKind::Directory)
                .map(|entry| entry.path())
        } else {
            None
        };
        if let Some(path) = selected_path {
            let pinned = platform::windows::quick_access::contains(&app.sidebar, &path);
            rows.push(quick_menu_row(
                if pinned {
                    CMD_QUICK_ACCESS_UNPIN
                } else {
                    CMD_QUICK_ACCESS_PIN
                },
                0,
                if pinned {
                    zh("从快速访问取消固定", "Unpin from Quick access")
                } else {
                    zh("固定到快速访问", "Pin to Quick access")
                },
                !app.quick_access_pending.contains(&path),
                false,
                false,
            ));
        }
        rows.push(quick_menu_row(
            2,
            0,
            zh("复制", "Copy"),
            selected > 0,
            false,
            false,
        ));
        rows.push(quick_menu_row(
            3,
            0,
            zh("剪切", "Cut"),
            selected > 0,
            false,
            false,
        ));
    }
    rows.push(quick_menu_row(
        4,
        0,
        zh("粘贴", "Paste"),
        can_paste,
        false,
        false,
    ));
    if !background {
        rows.push(quick_menu_row(
            5,
            0,
            zh("重命名", "Rename"),
            selected == 1,
            false,
            false,
        ));
        rows.push(quick_menu_row(
            6,
            0,
            zh("删除", "Delete"),
            selected > 0,
            false,
            false,
        ));
    }
    (rows, submenus)
}
fn project_context_menu(
    ui: &AppWindow,
    state: &WindowSessions,
    menu: &SharedQuickMenu,
    background: bool,
) {
    // A new pointer target must never inherit executable Shell command identities.
    if ui.get_context_menu_open() {
        ui.invoke_dismiss_context_menu();
    }
    let (built_in_rows, built_in_submenus, language) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        let (rows, submenus) = built_in_context_rows(&app, app.clipboard_has_files, background);
        (rows, submenus, app.language)
    };
    let key = quick_menu_key(state, background).map(|(key, _)| key);
    let (loading, cache_hit, session_hit) = menu
        .lock()
        .map(|mut menu| {
            let session_ready = key.as_ref().is_some_and(|key| {
                menu.identity
                    .as_ref()
                    .is_some_and(|identity| &identity.key == key && identity.ready)
            });
            let snapshot = key.as_ref().and_then(|key| menu.snapshots.get(key));
            let (shell_rows, loading, cache_hit, session_hit) =
                project_cached_shell_rows(session_ready, snapshot);
            menu.built_in_rows = built_in_rows.clone();
            menu.built_in_key = key.clone();
            menu.language = Some(language);
            menu.create_submenu_open = false;
            menu.all_rows = compose_quick_menu_rows(&built_in_rows, &shell_rows);
            menu.submenu_rows.clear();
            menu.submenu_history.clear();
            if !session_ready {
                menu.submenu_tokens.clear();
                menu.create_submenu_token = None;
                menu.preloaded_submenu_rows.clear();
                menu.loaded_submenu_rows.clear();
                menu.next_submenu_node = 0;
            }
            menu.built_in_submenu_rows = built_in_submenus.clone();
            menu.active_submenu_token = None;
            (loading, cache_hit, session_hit)
        })
        .unwrap_or_default();
    ui.set_context_search("".into());
    ui.set_context_shell_loading(loading);
    ui.set_context_shell_elapsed_ms(0);
    ui.set_context_submenu_open(false);
    ui.set_context_submenu_parent_open(false);
    ui.set_context_submenu_loading(false);
    ui.set_context_submenu_commands(ModelRc::new(
        VecModel::from(Vec::<ContextCommandRow>::new()),
    ));
    project_filtered_context_menu(ui, menu, "");
    ui.set_context_menu_open(true);
    if let Some(window_id) = window_id_for_ui(ui) {
        update_root_popup_projection(window_id);
        open_quick_menu_popup(
            window_id,
            ui.get_context_menu_anchor_x(),
            ui.get_context_menu_anchor_y(),
        );
    }
    eprintln!(
        "{{\"event\":\"quick_menu_projected\",\"background\":{},\"cache_hit\":{},\"session_hit\":{}}}",
        background, cache_hit, session_hit
    );
}
fn native_window_handle(ui: &AppWindow) -> isize {
    component_window_handle(ui)
}

fn handle_window_adaptation(
    window: &slint::Window,
    event: &winit::event::WindowEvent,
    installed_hwnd: &mut isize,
) {
    if matches!(event, winit::event::WindowEvent::Destroyed) {
        *installed_hwnd = 0;
        return;
    }
    // Native windows do not exist while Slint components are initially wired.
    // Resolve the HWND from the live event instead of capturing zero at startup.
    let hwnd = slint_window_handle(window);
    if hwnd == 0 {
        return;
    }
    if *installed_hwnd != hwnd {
        let recovery = platform::windows::window_drag_recovery::install(hwnd);
        let dpi = platform::windows::window_dpi::install(hwnd);
        if recovery.is_ok() && dpi.is_ok() {
            *installed_hwnd = hwnd;
            wire_window_trace(hwnd);
        }
        for error in [recovery.err(), dpi.err()].into_iter().flatten() {
            eprintln!("failed to install window adaptation: {error}");
        }
    }
}

fn wire_secondary_window_adaptation<T: slint::ComponentHandle>(ui: &T) {
    let mut installed_hwnd = 0;
    ui.window().on_winit_window_event(move |window, event| {
        handle_window_adaptation(window, event, &mut installed_hwnd);
        EventResult::Propagate
    });
}

fn begin_component_window_drag<T: slint::ComponentHandle>(ui: &T) {
    // Keep winit's release and recovery bookkeeping; the native adapter repairs
    // the packed screen coordinates before Windows starts its move loop.
    ui.window().with_winit_window(|window| {
        if let Err(error) = window.drag_window() {
            eprintln!("failed to begin window drag: {error}");
        }
    });
}

fn component_window_handle<T: slint::ComponentHandle>(ui: &T) -> isize {
    slint_window_handle(ui.window())
}

fn slint_window_handle(window: &slint::Window) -> isize {
    use slint::winit_030::winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    window
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

fn window_id_for_ui(ui: &AppWindow) -> Option<WindowId> {
    let hwnd = native_window_handle(ui);
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .find_map(|(id, runtime)| (native_window_handle(&runtime.ui) == hwnd).then_some(*id))
    })
}

fn trace_quick_menu(event: &str, detail: impl AsRef<str>) {
    platform::windows::window_trace::log_diagnostic(event, detail.as_ref());
}
fn popup_rows(rows: &[ContextCommandRow]) -> Vec<PopupCommandRow> {
    rows.iter()
        .map(|row| PopupCommandRow {
            stable_id: if row.shell && row.submenu {
                format!("shell-session/node/{}", row.node_id)
            } else if row.shell {
                format!("shell-session/command/{}", row.id)
            } else if row.submenu {
                format!("app-menu/{}", row.node_id)
            } else if row.separator || row.placeholder || row.loading {
                "menu-decoration".to_owned()
            } else {
                format!("app-command/{}", row.id)
            }
            .into(),
            label: row.label.clone(),
            hint: row.hint.clone(),
            enabled: row.enabled,
            separator: row.separator,
            checked: row.checked,
            checkable: !row.separator
                && !row.loading
                && !row.placeholder
                && (row.checked
                    || (!row.shell
                        && ((CMD_VIEW_BASE..CMD_VIEW_BASE + 8).contains(&row.id)
                            || (CMD_SORT_BASE..CMD_SORT_BASE + 5).contains(&row.id)
                            || matches!(
                                row.id,
                                CMD_SORT_ASC | CMD_SORT_DESC | CMD_GROUP_ASC | CMD_GROUP_DESC
                            )
                            || (CMD_GROUP_BASE..CMD_GROUP_BASE + 6).contains(&row.id)
                            || (CMD_COLUMN_TOGGLE_BASE
                                ..CMD_COLUMN_TOGGLE_BASE + ColumnKind::COUNT as i32)
                                .contains(&row.id)
                            || (CMD_SIDEBAR_VISIBILITY_BASE..CMD_SIDEBAR_VISIBILITY_BASE + 5)
                                .contains(&row.id)))),
            expanded: false,
            default: row.default,
            submenu: row.submenu,
            loading: row.loading,
            placeholder: row.placeholder,
            icon_kind: row.icon_kind,
        })
        .collect()
}

fn sync_popup_semantics(popup: &QuickMenuPopupRuntime) {
    let Some(session) = popup.session.identity() else {
        return;
    };
    let project = |rows: ModelRc<PopupCommandRow>, branch: usize, expanded_index: Option<i32>| {
        (0..rows.row_count())
            .filter_map(|index| {
                rows.row_data(index).map(|mut row| {
                    let base = row.stable_id.as_str().split('@').next().unwrap_or_default();
                    row.stable_id = if base.starts_with("app-") {
                        format!("{base}@window/{}/branch/{branch}", session.owner_window.0)
                    } else {
                        format!(
                            "{base}@window/{}/menu/{}/branch/{branch}",
                            session.owner_window.0, session.generation
                        )
                    }
                    .into();
                    row.expanded = row.submenu && expanded_index == Some(index as i32);
                    row
                })
            })
            .collect::<Vec<_>>()
    };
    let open_child = |index: usize| {
        popup
            .branches
            .get(index)
            .filter(|slot| slot.event.is_some())
            .map(|slot| slot.parent_row_index)
    };
    popup.root.set_rows(ModelRc::new(VecModel::from(project(
        popup.root.get_rows(),
        0,
        open_child(0),
    ))));
    for (index, slot) in popup
        .branches
        .iter()
        .enumerate()
        .filter(|(_, slot)| slot.event.is_some())
    {
        slot.window.set_rows(ModelRc::new(VecModel::from(project(
            slot.window.get_rows(),
            index + 1,
            open_child(index + 1),
        ))));
    }
}

fn create_quick_menu_popup(
    ui: &AppWindow,
    state: &WindowSessions,
) -> Result<QuickMenuPopupRuntime, slint::PlatformError> {
    let owner_hwnd = native_window_handle(ui);
    platform::windows::quick_menu_window::prepare_window(owner_hwnd);
    let root = match QuickMenuWindow::new() {
        Ok(root) => root,
        Err(error) => {
            platform::windows::quick_menu_window::cancel_prepared_window();
            return Err(error);
        }
    };
    if let Ok(app) = state.lock() {
        root.set_dark_theme(app.dark_theme());
        root.set_backdrop_enabled(app.quick_menu_backdrop);
    }
    Ok(QuickMenuPopupRuntime {
        root,
        branches: Vec::new(),
        session: Default::default(),
        owner_hwnd,
        client_anchor: crate::quick_menu_popup::PhysicalPoint::new(0, 0),
        work_area: crate::quick_menu_popup::PhysicalRect::new(0, 0, 1, 1),
        root_rect: None,
        next_generation: 0,
        next_branch: 0,
        shown_once: false,
        presentation: PopupPresentation::Hidden,
        cloak_generation: None,
    })
}

fn root_popup_height_for_content(content_height: f32, loading: bool, scale: f32) -> i32 {
    let loading_height = if loading { 20.0 } else { 0.0 };
    ((41.0 + content_height + loading_height) * scale)
        .ceil()
        .max(41.0) as i32
}

fn root_popup_height(ui: &AppWindow, scale: f32) -> i32 {
    root_popup_height_for_content(
        ui.get_context_menu_content_height(),
        ui.get_context_shell_loading(),
        scale,
    )
}

fn submenu_popup_height(ui: &AppWindow, scale: f32) -> i32 {
    ((8.0
        + ui.get_context_submenu_content_height()
        + if ui.get_context_submenu_loading() {
            20.0
        } else {
            0.0
        })
        * scale)
        .ceil()
        .max(28.0) as i32
}

fn context_row_anchor(rows: &[ContextCommandRow], index: i32, header_height: f32) -> f32 {
    if index < 0 {
        return header_height;
    }
    header_height
        + rows
            .iter()
            .take(index as usize)
            .map(|row| if row.separator { 7.0 } else { 24.0 })
            .sum::<f32>()
}

fn update_root_popup_projection(window_id: WindowId) {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let Some(runtime) = runtimes.get(&window_id) else {
            return;
        };
        let ui = &runtime.ui;
        let popup = &runtime.quick_menu_popup.root;
        let rows = (0..ui.get_context_commands().row_count())
            .filter_map(|index| ui.get_context_commands().row_data(index))
            .collect::<Vec<_>>();
        popup.set_rows(ModelRc::new(VecModel::from(popup_rows(&rows))));
        popup.set_content_height(ui.get_context_menu_content_height());
        popup.set_loading(ui.get_context_shell_loading());
        popup.set_search(ui.get_context_search());
        popup.set_active_index(ui.get_context_active_index());
        popup.set_dark_theme(ui.get_dark_theme());
        popup.set_backdrop_enabled(ui.get_quick_menu_backdrop());
        let texts = ui.global::<UiTexts>().get_values();
        popup.set_search_text(texts.text_context_search);
        popup.set_loading_text(texts.text_context_loading);
        popup.set_empty_text(texts.text_context_empty);
        sync_popup_semantics(&runtime.quick_menu_popup);
    });
    resize_quick_menu_root_and_reposition_submenus(window_id);
    schedule_root_popup_presentation(window_id);
}

fn schedule_root_popup_presentation(window_id: WindowId) {
    let event = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        if !root_popup_presentation_ready(
            popup.presentation,
            runtime.ui.get_context_shell_loading(),
        ) {
            return None;
        }
        let session = popup.session.identity()?;
        let branch = popup.session.branches().first()?.id;
        popup.root.window().request_redraw();
        Some(crate::quick_menu_popup::MenuEventIdentity { session, branch })
    });
    let Some(event) = event else { return };
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        finish_root_popup_presentation(window_id, event);
    });
}

fn root_popup_presentation_ready(presentation: PopupPresentation, shell_loading: bool) -> bool {
    presentation == PopupPresentation::ShownCloaked && !shell_loading
}

fn submenu_slot_is_current(
    session: &crate::quick_menu_popup::QuickMenuPopupSession,
    depth: usize,
    event: Option<crate::quick_menu_popup::MenuEventIdentity>,
) -> bool {
    event.is_some_and(|event| {
        session.matches_event(event)
            && session.branches().get(depth + 1).map(|branch| branch.id) == Some(event.branch)
    })
}

fn replace_submenu_slot_rows_if_current(
    session: &crate::quick_menu_popup::QuickMenuPopupSession,
    depth: usize,
    event: Option<crate::quick_menu_popup::MenuEventIdentity>,
    slot_rows: &mut Vec<ContextCommandRow>,
    projected_rows: &[ContextCommandRow],
) -> bool {
    let current = submenu_slot_is_current(session, depth, event);
    if current {
        *slot_rows = projected_rows.to_vec();
    }
    current
}

fn submenu_projection_needs_presentation(
    presentation: PopupPresentation,
    cloak_event_pending: bool,
    loading: bool,
) -> bool {
    presentation == PopupPresentation::ShownCloaked && !cloak_event_pending && !loading
}

fn present_loaded_submenu_popup(
    window_id: WindowId,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    slint::Timer::single_shot(Duration::from_millis(16), move || {
        finish_submenu_popup_presentation(window_id, depth, event);
    });
}
fn update_open_submenu_projection(window_id: WindowId) {
    let projection = WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let runtime = runtimes.get_mut(&window_id)?;
        let ui = &runtime.ui;
        let rows = (0..ui.get_context_submenu_commands().row_count())
            .filter_map(|index| ui.get_context_submenu_commands().row_data(index))
            .collect::<Vec<_>>();
        let content_height = ui.get_context_submenu_content_height();
        let loading = ui.get_context_submenu_loading();
        let active_index = ui.get_context_submenu_active_index();
        let popup = &mut runtime.quick_menu_popup;
        let depth = popup.session.branches().len().checked_sub(2)?;
        let submenu = popup.branches.get_mut(depth)?;
        if !replace_submenu_slot_rows_if_current(
            &popup.session,
            depth,
            submenu.event,
            &mut submenu.rows,
            &rows,
        ) {
            return None;
        }
        let present_after_update = submenu_projection_needs_presentation(
            submenu.presentation,
            submenu.cloak_event.is_some(),
            loading,
        );
        Some((
            submenu.window.clone_strong(),
            submenu.event,
            depth,
            rows,
            content_height,
            loading,
            active_index,
            popup.work_area.height,
            present_after_update,
        ))
    });
    let Some((
        submenu,
        event,
        depth,
        rows,
        content_height,
        loading,
        active_index,
        work_area_height,
        present_after_update,
    )) = projection
    else {
        trace_quick_menu(
            "quick_menu_submenu_projection_skipped",
            format!("window={}", window_id.0),
        );
        return;
    };
    trace_quick_menu(
        "quick_menu_submenu_projected",
        format!(
            "window={} depth={} branch={:?} rows={} loading={} presentation_needed={}",
            window_id.0,
            depth,
            event.map(|event| event.branch.0),
            rows.len(),
            loading,
            present_after_update,
        ),
    );
    submenu.set_rows(ModelRc::new(VecModel::from(popup_rows(&rows))));
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        if let Some(runtime) = runtimes.get(&window_id) {
            sync_popup_semantics(&runtime.quick_menu_popup);
        }
    });
    submenu.set_content_height(content_height);
    submenu.set_loading(loading);
    submenu.set_active_index(active_index);
    let scale = submenu.window().scale_factor();
    let height = ((8.0 + content_height + if loading { 20.0 } else { 0.0 }) * scale)
        .ceil()
        .max(28.0) as i32;
    submenu.set_window_height(height.min(work_area_height.max(1)) as f32 / scale);
    reposition_quick_submenu_popups(window_id);
    if present_after_update && let Some(event) = event {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id)
                && current_slot_matches(&runtime.quick_menu_popup, depth, event)
                && let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth)
            {
                slot.cloak_event = Some(event);
            }
        });
        trace_quick_menu(
            "quick_menu_submenu_presentation_scheduled",
            format!(
                "window={} depth={} branch={}",
                window_id.0, depth, event.branch.0,
            ),
        );
        submenu.window().request_redraw();
        present_loaded_submenu_popup(window_id, depth, event);
    }
}

fn normalized_quick_menu_backdrop_opacity(opacity: f32) -> u8 {
    opacity.round().clamp(0.0, 100.0) as u8
}

fn open_quick_menu_popup(window_id: WindowId, client_x: f32, client_y: f32) {
    let started_at = Instant::now();
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else { return };
        let popup = &mut runtime.quick_menu_popup;
        let ui = &runtime.ui;
        let owner_hwnd = native_window_handle(ui);
        if owner_hwnd == 0 {
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"owner_unavailable\"}}");
            return;
        }
        popup.owner_hwnd = owner_hwnd;
        close_quick_submenu_windows(popup);
        popup.next_branch = popup.next_branch.wrapping_add(1).max(1);
        let (tab_id, request_id) = runtime.sessions.lock().ok().map(|app| {
            let tab = app.window(window_id).expect("popup owner window exists").active_tab;
            (tab, app.tab(tab).expect("popup owner tab exists").latest_request())
        }).unwrap_or((TabId(0), RequestId(0)));
        let shell_generation = runtime
            ._quick_menu
            .lock()
            .ok()
            .and_then(|menu| menu.identity.as_ref().map(|identity| identity.request_id));
        popup.next_generation = shell_generation.unwrap_or_else(|| {
            popup.next_generation.wrapping_add(1).max(1)
        });
        let session = crate::quick_menu_popup::MenuSessionId {
            owner_window: window_id,
            tab_id,
            request_id,
            generation: popup.next_generation,
        };
        let root_branch = crate::quick_menu_popup::MenuBranchId(popup.next_branch);
        let scale = ui.window().scale_factor();
        let client = crate::quick_menu_popup::PhysicalPoint::new(
            (client_x * scale).round() as i32,
            (client_y * scale).round() as i32,
        );
        popup.client_anchor = client;
        let Ok(anchor) = platform::windows::quick_menu_window::client_point_to_screen(popup.owner_hwnd, client) else {
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"client_to_screen\"}}");
            return;
        };
        let Ok(work_area) = platform::windows::quick_menu_window::work_area_for_point(anchor) else {
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"work_area\"}}");
            return;
        };
        popup.work_area = work_area;
        let placement = crate::quick_menu_popup::place_root_popup(
            anchor,
            crate::quick_menu_popup::PhysicalSize::new(
                (320.0 * scale).ceil() as i32,
                root_popup_height(ui, scale),
            ),
            work_area,
        );
        popup.root_rect = Some(placement.rect);
        popup.root.set_window_height(placement.rect.height as f32 / scale);
        popup.root.window().set_position(slint::PhysicalPosition::new(placement.rect.x, placement.rect.y));
        let root_hwnd = component_window_handle(&popup.root);
        let owner_attached = platform::windows::quick_menu_window::attach_owner(
            root_hwnd,
            popup.owner_hwnd,
        )
        .is_ok();
        if owner_attached
            && let Err(error) = platform::windows::quick_menu_window::set_backdrop(
                root_hwnd,
                ui.get_quick_menu_backdrop(),
                ui.get_dark_theme(),
                normalized_quick_menu_backdrop_opacity(ui.get_quick_menu_backdrop_opacity()),
            )
        {
            trace_quick_menu("quick_menu_backdrop_failed", error.to_string());
        }
        let cloaked = owner_attached
            && platform::windows::quick_menu_window::set_cloaked(root_hwnd, true).is_ok();
        if cloaked {
            popup.presentation = PopupPresentation::Cloaked;
            platform::windows::quick_menu_window::show_without_activation(root_hwnd);
        }
        let shown = cloaked && popup.root.show().is_ok();
        if !shown {
            let _ = popup.root.hide();
            let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
            popup.presentation = PopupPresentation::Hidden;
            popup.cloak_generation = None;
            ui.set_context_menu_open(false);
            ui.invoke_dismiss_context_menu();
            eprintln!("{{\"event\":\"quick_menu_popup_open_failed\",\"stage\":\"show_or_owner\"}}");
            return;
        }
        popup.presentation = PopupPresentation::ShownCloaked;
        popup.cloak_generation = Some(popup.next_generation);
        popup.session.open_root(session, root_branch);
        popup.root.window().request_redraw();
        let first_show = !popup.shown_once;
        popup.shown_once = true;
        eprintln!(
            "{{\"event\":\"quick_menu_popup_opened\",\"window\":{},\"generation\":{},\"first_show\":{},\"elapsed_us\":{},\"x\":{},\"y\":{},\"width\":{},\"height\":{},\"horizontal_flipped\":{},\"vertical_flipped\":{},\"height_limited\":{}}}",
            window_id.0, popup.next_generation, first_show, started_at.elapsed().as_micros(),
            placement.rect.x, placement.rect.y,
            placement.rect.width, placement.rect.height, placement.horizontal_flipped,
            placement.vertical_flipped, placement.height_limited,
        );
    });
}

fn resize_quick_menu_root_and_reposition_submenus(window_id: WindowId) {
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        let Some(root_rect) = popup.root_rect else {
            return;
        };
        let scale = popup.root.window().scale_factor();
        let stable_rect = crate::quick_menu_popup::stable_root_size(
            root_rect,
            root_popup_height(&runtime.ui, scale),
            popup.work_area,
        );
        popup.root_rect = Some(stable_rect);
        popup
            .root
            .set_window_height(stable_rect.height as f32 / scale);
    });
    reposition_quick_submenu_popups(window_id);
}

fn reposition_quick_menu_root_and_submenus(window_id: WindowId) {
    let started_at = Instant::now();
    let root = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        if !popup.session.is_open() {
            return None;
        }
        let scale = popup.root.window().scale_factor();
        let anchor = platform::windows::quick_menu_window::client_point_to_screen(
            popup.owner_hwnd,
            popup.client_anchor,
        )
        .ok()?;
        let work_area = platform::windows::quick_menu_window::work_area_for_point(anchor).ok()?;
        let placement = crate::quick_menu_popup::place_root_popup(
            anchor,
            crate::quick_menu_popup::PhysicalSize::new(
                (320.0 * scale).ceil() as i32,
                root_popup_height(&runtime.ui, scale),
            ),
            work_area,
        );
        Some((placement, scale, work_area))
    });
    let Some((placement, scale, work_area)) = root else {
        return;
    };
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        popup.work_area = work_area;
        popup.root_rect = Some(placement.rect);
        popup
            .root
            .set_window_height(placement.rect.height as f32 / scale);
        popup
            .root
            .window()
            .set_position(slint::PhysicalPosition::new(
                placement.rect.x,
                placement.rect.y,
            ));
    });
    reposition_quick_submenu_popups(window_id);
    eprintln!(
        "{{\"event\":\"quick_menu_popup_repositioned\",\"window\":{},\"elapsed_us\":{},\"x\":{},\"y\":{}}}",
        window_id.0,
        started_at.elapsed().as_micros(),
        placement.rect.x,
        placement.rect.y,
    );
}
fn reposition_quick_submenu_popups(window_id: WindowId) {
    let started_at = Instant::now();
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        let Some(root_rect) = popup.root_rect else {
            return;
        };
        let work_area = popup.work_area;
        let mut parent_scale = popup.root.window().scale_factor();
        let requests = popup
            .branches
            .iter()
            .filter(|slot| slot.event.is_some())
            .map(|branch| {
                let branch_scale = branch.window.window().scale_factor();
                let request = crate::quick_menu_popup::SubmenuPlacementRequest {
                    anchor_y: (branch.anchor_y * parent_scale).round() as i32,
                    desired_size: crate::quick_menu_popup::PhysicalSize::new(
                        (280.0 * branch_scale).ceil() as i32,
                        (branch.window.get_window_height() * branch_scale).ceil() as i32,
                    ),
                };
                parent_scale = branch_scale;
                request
            })
            .collect::<Vec<_>>();
        let placements =
            crate::quick_menu_popup::place_submenu_chain(root_rect, &requests, work_area);
        for (branch, placement) in popup
            .branches
            .iter()
            .filter(|slot| slot.event.is_some())
            .zip(placements)
        {
            branch.window.window().set_position(slint::PhysicalPosition::new(
                placement.rect.x,
                placement.rect.y,
            ));
        }
        eprintln!(
            "{{\"event\":\"quick_menu_submenus_repositioned\",\"window\":{},\"elapsed_us\":{},\"root_x\":{},\"root_y\":{}}}",
            window_id.0,
            started_at.elapsed().as_micros(),
            root_rect.x,
            root_rect.y,
        );
    });
}

fn hide_quick_submenu_slots_from(popup: &mut QuickMenuPopupRuntime, depth: usize) {
    for slot in popup.branches.iter_mut().skip(depth) {
        let hwnd = component_window_handle(&slot.window);
        let _ = slot.window.hide();
        let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
        slot.event = None;
        slot.rows.clear();
        slot.cloak_event = None;
        slot.presentation = PopupPresentation::Hidden;
    }
    sync_popup_semantics(popup);
}

fn close_quick_submenu_windows(popup: &mut QuickMenuPopupRuntime) {
    hide_quick_submenu_slots_from(popup, 0);
}

fn quick_menu_popup_owns_foreground(popup: &QuickMenuPopupRuntime) -> bool {
    let handles = std::iter::once(component_window_handle(&popup.root))
        .chain(
            popup
                .branches
                .iter()
                .filter(|slot| slot.event.is_some())
                .map(|branch| component_window_handle(&branch.window)),
        )
        .collect::<Vec<_>>();
    platform::windows::quick_menu_window::foreground_belongs_to(popup.owner_hwnd, &handles)
}

fn close_quick_menu_popup(window_id: WindowId, restore_focus: bool) {
    let started_at = Instant::now();
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        let foreground_owned = restore_focus && quick_menu_popup_owns_foreground(popup);
        let was_open = popup.session.close_all();
        close_quick_submenu_windows(popup);
        let root_hwnd = component_window_handle(&popup.root);
        let _ = popup.root.hide();
        let _ = platform::windows::quick_menu_window::set_cloaked(root_hwnd, false);
        popup.presentation = PopupPresentation::Hidden;
        popup.cloak_generation = None;
        popup.root_rect = None;
        let focus_restored = was_open && foreground_owned;
        if focus_restored {
            platform::windows::quick_menu_window::focus_window(popup.owner_hwnd);
        }
        if was_open {
            eprintln!(
                "{{\"event\":\"quick_menu_popup_closed\",\"window\":{},\"elapsed_us\":{},\"focus_restored\":{}}}",
                window_id.0,
                started_at.elapsed().as_micros(),
                focus_restored,
            );
        }
    });
}

fn open_quick_submenu_popup(window_id: WindowId, anchor_y: f32, parent_depth: Option<usize>) {
    open_quick_submenu_popup_attempt(window_id, anchor_y, parent_depth, 2);
}

fn open_quick_submenu_popup_attempt(
    window_id: WindowId,
    anchor_y: f32,
    parent_depth: Option<usize>,
    retries_remaining: u8,
) {
    let started_at = Instant::now();
    let mut retry_reason = None;
    let opened = WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let runtime = runtimes.get_mut(&window_id)?;
        let popup = &mut runtime.quick_menu_popup;
        let ui = &runtime.ui;
        if !popup.session.is_open() || !ui.get_context_submenu_open() {
            return None;
        }
        if popup.owner_hwnd == 0 || native_window_handle(ui) != popup.owner_hwnd {
            return None;
        }

        let depth = parent_depth.map_or(0, |depth| depth + 1);
        let session = popup.session.identity()?;
        let parent_branch = popup.session.branches().get(depth)?.id;
        let parent_event = crate::quick_menu_popup::MenuEventIdentity {
            session,
            branch: parent_branch,
        };
        if !popup.session.close_to_branch(parent_event) {
            return None;
        }
        let parent_rect = if depth == 0 {
            let position = popup.root.window().position();
            let size = popup.root.window().size();
            crate::quick_menu_popup::PhysicalRect::new(
                position.x,
                position.y,
                size.width as i32,
                size.height as i32,
            )
        } else {
            let parent = popup.branches.get(depth - 1)?;
            let position = parent.window.window().position();
            let size = parent.window.window().size();
            crate::quick_menu_popup::PhysicalRect::new(
                position.x,
                position.y,
                size.width as i32,
                size.height as i32,
            )
        };

        popup.next_branch = popup.next_branch.wrapping_add(1).max(1);
        let new_branch = crate::quick_menu_popup::MenuBranchId(popup.next_branch);
        if !popup.session.push_branch(parent_event, new_branch) {
            return None;
        }
        let event = crate::quick_menu_popup::MenuEventIdentity {
            session,
            branch: new_branch,
        };
        hide_quick_submenu_slots_from(popup, depth + 1);

        let scale = if depth == 0 {
            popup.root.window().scale_factor()
        } else {
            popup.branches[depth - 1].window.window().scale_factor()
        };
        let rows = (0..ui.get_context_submenu_commands().row_count())
            .filter_map(|index| ui.get_context_submenu_commands().row_data(index))
            .collect::<Vec<_>>();
        let effective_anchor_y = if anchor_y > 0.0 {
            anchor_y
        } else {
            context_row_anchor(&rows, ui.get_context_submenu_active_index(), 12.0)
        };
        let placement = crate::quick_menu_popup::place_submenu_popup_with_margins(
            crate::quick_menu_popup::PhysicalRect::new(
                parent_rect.x,
                parent_rect
                    .y
                    .saturating_add((effective_anchor_y * scale).round() as i32),
                parent_rect.width,
                parent_rect.height,
            ),
            crate::quick_menu_popup::PhysicalSize::new(
                (280.0 * scale).ceil() as i32,
                submenu_popup_height(ui, scale),
            ),
            popup.work_area,
            0,
            0,
        );

        if depth == popup.branches.len() {
            platform::windows::quick_menu_window::prepare_window(popup.owner_hwnd);
            let window = match QuickSubmenuWindow::new() {
                Ok(window) => window,
                Err(error) => {
                    platform::windows::quick_menu_window::cancel_prepared_window();
                    retry_reason = Some(format!("create_window:{error}"));
                    return None;
                }
            };
            wire_submenu_popup_callbacks(&window, window_id, depth);
            popup.branches.push(QuickSubmenuPopupRuntime {
                parent_row_index: -1,
                window,
                event: None,
                rows: Vec::new(),
                active_index: -1,
                anchor_y: 0.0,
                presentation: PopupPresentation::Hidden,
                cloak_event: None,
            });
        }
        let parent_row_index = if depth == 0 {
            ui.get_context_active_index()
        } else {
            popup
                .branches
                .get(depth - 1)
                .map_or(-1, |parent| parent.active_index)
        };
        let slot = popup.branches.get_mut(depth)?;
        slot.parent_row_index = parent_row_index;
        let already_visible = slot.event.is_some();
        slot.event = Some(event);
        slot.rows = rows.clone();
        slot.anchor_y = effective_anchor_y;
        slot.cloak_event = None;
        slot.window
            .set_rows(ModelRc::new(VecModel::from(popup_rows(&rows))));
        slot.window
            .set_content_height(ui.get_context_submenu_content_height());
        slot.window.set_loading(ui.get_context_submenu_loading());
        slot.window
            .set_active_index(ui.get_context_submenu_active_index());
        slot.window.set_dark_theme(ui.get_dark_theme());
        slot.window
            .set_backdrop_enabled(ui.get_quick_menu_backdrop());
        let texts = ui.global::<UiTexts>().get_values();
        slot.window.set_loading_text(texts.text_context_loading);
        slot.window.set_empty_text(texts.text_context_empty);
        slot.window
            .set_window_height(placement.rect.height as f32 / scale);
        slot.window
            .window()
            .set_position(slint::PhysicalPosition::new(
                placement.rect.x,
                placement.rect.y,
            ));

        let hwnd = component_window_handle(&slot.window);
        if let Err(error) =
            platform::windows::quick_menu_window::attach_owner(hwnd, popup.owner_hwnd)
        {
            retry_reason = Some(format!("attach_owner:{error}"));
            hide_quick_submenu_slots_from(popup, depth);
            let _ = popup.session.close_branch_and_descendants(event);
            return None;
        }
        if let Err(error) = platform::windows::quick_menu_window::set_backdrop(
            hwnd,
            ui.get_quick_menu_backdrop(),
            ui.get_dark_theme(),
            normalized_quick_menu_backdrop_opacity(ui.get_quick_menu_backdrop_opacity()),
        ) {
            trace_quick_menu("quick_submenu_backdrop_failed", error.to_string());
        }
        let loading = ui.get_context_submenu_loading();
        if already_visible {
            if slot.presentation == PopupPresentation::Presented {
                slot.cloak_event = None;
            } else if loading {
                let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, true);
                slot.presentation = PopupPresentation::ShownCloaked;
                slot.cloak_event = None;
            } else {
                slot.presentation = PopupPresentation::ShownCloaked;
                slot.cloak_event = Some(event);
            }
            slot.window.window().request_redraw();
        } else {
            if let Err(error) = platform::windows::quick_menu_window::set_cloaked(hwnd, true) {
                retry_reason = Some(format!("cloak:{error}"));
                hide_quick_submenu_slots_from(popup, depth);
                let _ = popup.session.close_branch_and_descendants(event);
                return None;
            }
            slot.presentation = PopupPresentation::Cloaked;
            platform::windows::quick_menu_window::show_without_activation(hwnd);
            if let Err(error) = slot.window.show() {
                retry_reason = Some(format!("show:{error}"));
                let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
                hide_quick_submenu_slots_from(popup, depth);
                let _ = popup.session.close_branch_and_descendants(event);
                return None;
            }
            slot.presentation = PopupPresentation::ShownCloaked;
            slot.cloak_event = (!loading).then_some(event);
            slot.window.window().request_redraw();
        }
        sync_popup_semantics(popup);
        Some((
            depth,
            event,
            already_visible,
            placement.horizontal_flipped,
            placement.height_limited,
        ))
    });

    let Some((depth, event, reused, horizontal_flipped, height_limited)) = opened else {
        if let Some(reason) = retry_reason {
            trace_quick_menu(
                "quick_menu_submenu_open_retry",
                format!(
                    "window={} parent_depth={:?} anchor_y={} retries_remaining={} reason={}",
                    window_id.0, parent_depth, anchor_y, retries_remaining, reason,
                ),
            );
            if retries_remaining > 0 {
                slint::Timer::single_shot(Duration::from_millis(16), move || {
                    open_quick_submenu_popup_attempt(
                        window_id,
                        anchor_y,
                        parent_depth,
                        retries_remaining - 1,
                    );
                });
                return;
            }
        }
        trace_quick_menu(
            "quick_menu_submenu_open_failed",
            format!(
                "window={} parent_depth={:?} anchor_y={}",
                window_id.0, parent_depth, anchor_y,
            ),
        );
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_close_context_submenu();
            }
        });
        return;
    };
    trace_quick_menu(
        "quick_menu_submenu_opened",
        format!(
            "window={} depth={} branch={} reused={} elapsed_us={} horizontal_flipped={} height_limited={}",
            window_id.0,
            depth,
            event.branch.0,
            reused,
            started_at.elapsed().as_micros(),
            horizontal_flipped,
            height_limited,
        ),
    );
    eprintln!(
        "{{\"event\":\"quick_menu_submenu_opened\",\"window\":{},\"depth\":{},\"branch\":{},\"reused\":{},\"elapsed_us\":{},\"horizontal_flipped\":{},\"height_limited\":{}}}",
        window_id.0,
        depth,
        event.branch.0,
        reused,
        started_at.elapsed().as_micros(),
        horizontal_flipped,
        height_limited,
    );
}

fn current_submenu_event(
    window_id: WindowId,
    depth: usize,
) -> Option<crate::quick_menu_popup::MenuEventIdentity> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        let event = popup.branches.get(depth)?.event?;
        (popup.session.matches_event(event)
            && popup
                .session
                .branches()
                .get(depth + 1)
                .map(|branch| branch.id)
                == Some(event.branch))
        .then_some(event)
    })
}

fn activate_submenu_slot_row(window_id: WindowId, depth: usize, index: i32) -> bool {
    if index < 0 {
        return false;
    }
    let row = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        let slot = popup.branches.get(depth)?;
        slot.event?;
        slot.rows.get(index as usize).cloned()
    });
    let Some(row) = row else {
        return false;
    };
    if !row.enabled || row.separator || row.loading || row.placeholder {
        return false;
    }
    if row.submenu {
        let can_open = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes.get(&window_id).is_some_and(|runtime| {
                depth + 2 >= runtime.quick_menu_popup.session.branches().len()
            })
        });
        if !can_open {
            return false;
        }
        if let Some(ui) = window_ui(window_id) {
            ui.invoke_cancel_context_submenu_hover();
            ui.set_context_submenu_active_index(index);
            ui.invoke_open_context_submenu(-index - 1);
        }
    } else {
        let event = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            let popup = &runtimes.get(&window_id)?.quick_menu_popup;
            let event = popup.branches.get(depth)?.event?;
            submenu_slot_is_current(&popup.session, depth, Some(event)).then_some(event)
        });
        let Some(event) = event else {
            return false;
        };
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id) {
                hide_quick_submenu_slots_from(&mut runtime.quick_menu_popup, depth + 1);
                let _ = runtime.quick_menu_popup.session.close_to_branch(event);
            }
        });
        if let Some(ui) = window_ui(window_id) {
            ui.set_context_menu_open(false);
            ui.invoke_invoke_context_command(row.id);
            if !row.shell {
                ui.invoke_dismiss_context_menu();
            }
        }
        let menu_closed = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .get(&window_id)
                .is_none_or(|runtime| !runtime.ui.get_context_menu_open())
        });
        if menu_closed {
            close_quick_menu_popup(window_id, true);
        }
    }
    row.submenu
}

fn open_submenu_slot_row(window_id: WindowId, depth: usize, index: i32) -> bool {
    if index < 0 {
        return false;
    }
    let prepared = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        if depth + 2 < popup.session.branches().len() {
            return None;
        }
        let row = popup.branches.get(depth)?.rows.get(index as usize)?.clone();
        (row.enabled && row.submenu && row.node_id > 0).then_some((
            row.node_id,
            popup.branches.get(depth)?.rows.clone(),
            popup.branches.get(depth)?.anchor_y,
        ))
    });
    let Some((node_id, parent_rows, parent_anchor_y)) = prepared else {
        return false;
    };
    let ui = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let known = runtime
            ._quick_menu
            .lock()
            .ok()
            .is_some_and(|menu| menu.submenu_tokens.contains_key(&node_id));
        known.then(|| runtime.ui.clone_strong())
    });
    let Some(ui) = ui else {
        return false;
    };
    let parent_height = context_menu_content_height(&parent_rows);
    ui.set_context_submenu_parent_commands(ModelRc::new(VecModel::from(parent_rows)));
    ui.set_context_submenu_parent_content_height(parent_height);
    ui.set_context_submenu_parent_anchor_y(parent_anchor_y);
    ui.set_context_submenu_active_index(index);
    ui.invoke_open_context_submenu(-index - 1);
    true
}

fn close_submenu_slot_and_descendants(
    window_id: WindowId,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
        let Some(runtime) = runtimes.get_mut(&window_id) else {
            return;
        };
        let popup = &mut runtime.quick_menu_popup;
        if current_slot_matches(popup, depth, event) {
            hide_quick_submenu_slots_from(popup, depth);
            let _ = popup.session.close_branch_and_descendants(event);
        }
    });
}

fn current_slot_matches(
    popup: &QuickMenuPopupRuntime,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) -> bool {
    popup.branches.get(depth).and_then(|slot| slot.event) == Some(event)
        && popup.session.matches_event(event)
}

fn finish_submenu_popup_presentation(
    window_id: WindowId,
    depth: usize,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    let hwnd = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        let slot = popup.branches.get(depth)?;
        (slot.cloak_event == Some(event)
            && slot.presentation == PopupPresentation::ShownCloaked
            && current_slot_matches(popup, depth, event))
        .then(|| component_window_handle(&slot.window))
    });
    let Some(hwnd) = hwnd else {
        trace_quick_menu(
            "quick_menu_submenu_presentation_skipped",
            format!(
                "window={} depth={} branch={} reason=state_mismatch",
                window_id.0, depth, event.branch.0,
            ),
        );
        return;
    };
    let flush_result = platform::windows::quick_menu_window::flush_compositor();
    let uncloak_result = flush_result
        .as_ref()
        .map_err(ToString::to_string)
        .and_then(|_| {
            platform::windows::quick_menu_window::set_cloaked(hwnd, false)
                .map_err(|error| error.to_string())
        });
    let presented = uncloak_result.is_ok();
    trace_quick_menu(
        "quick_menu_submenu_presentation_finished",
        format!(
            "window={} depth={} branch={} hwnd={} presented={} error={:?}",
            window_id.0,
            depth,
            event.branch.0,
            hwnd,
            presented,
            uncloak_result.err(),
        ),
    );
    if presented {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id)
                && current_slot_matches(&runtime.quick_menu_popup, depth, event)
                && let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth)
            {
                slot.presentation = PopupPresentation::Presented;
                slot.cloak_event = None;
            }
        });
    } else {
        let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
        close_submenu_slot_and_descendants(window_id, depth, event);
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_close_context_submenu();
            }
        });
    }
}

fn wire_submenu_popup_callbacks(submenu: &QuickSubmenuWindow, window_id: WindowId, depth: usize) {
    let weak = submenu.as_weak();
    submenu.on_move(move |index| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        let Some(submenu) = weak.upgrade() else {
            return;
        };
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id) {
                let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth) else {
                    return;
                };
                slot.active_index = if index == -1 || index == 1 {
                    next_enabled_context_index(&slot.rows, slot.active_index, index)
                } else {
                    index
                };
                submenu.set_active_index(slot.active_index);
                if depth + 2 == runtime.quick_menu_popup.session.branches().len() {
                    runtime
                        .ui
                        .set_context_submenu_active_index(slot.active_index);
                }
            }
        });
    });
    submenu.on_activate(move |index| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        let _ = activate_submenu_slot_row(window_id, depth, index);
    });
    submenu.on_open_submenu(move |index, anchor_y| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        if let Some(ui) = window_ui(window_id) {
            ui.invoke_cancel_context_submenu_hover();
        }
        if open_submenu_slot_row(window_id, depth, index) {
            open_quick_submenu_popup(window_id, anchor_y, Some(depth));
        }
    });
    submenu.on_hover(move |index, anchor_y| {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        if index >= 0 {
            WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
                if let Some(runtime) = runtimes.get_mut(&window_id)
                    && let Some(slot) = runtime.quick_menu_popup.branches.get_mut(depth)
                {
                    slot.active_index = index;
                    slot.window.set_active_index(index);
                }
            });
            let timer = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                runtimes
                    .get(&window_id)
                    .map(|runtime| runtime._quick_submenu_timer.clone())
            });
            if let Some(timer) = timer {
                timer.stop();
                let expected_event = current_submenu_event(window_id, depth);
                timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(250),
                    move || {
                        let target_is_current = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                            runtimes.get(&window_id).is_some_and(|runtime| {
                                runtime
                                    .quick_menu_popup
                                    .branches
                                    .get(depth)
                                    .is_some_and(|slot| {
                                        slot.event == expected_event && slot.active_index == index
                                    })
                            })
                        });
                        if target_is_current && open_submenu_slot_row(window_id, depth, index) {
                            open_quick_submenu_popup(window_id, anchor_y, Some(depth));
                        }
                    },
                );
            }
        } else {
            WINDOW_RUNTIMES.with_borrow(|runtimes| {
                if let Some(runtime) = runtimes.get(&window_id) {
                    runtime.ui.invoke_cancel_context_submenu_hover();
                }
            });
        }
    });
    submenu.on_cancel_hover(move || {
        if current_submenu_event(window_id, depth).is_none() {
            return;
        }
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_cancel_context_submenu_hover();
            }
        });
    });
    submenu.on_dismiss(move || {
        let Some(event) = current_submenu_event(window_id, depth) else {
            return;
        };
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_close_context_submenu();
            }
        });
        close_submenu_slot_and_descendants(window_id, depth, event);
    });
    submenu.window().on_winit_window_event(move |_, event| {
        if matches!(event, winit::event::WindowEvent::RedrawRequested)
            && let Some(event_identity) = current_submenu_event(window_id, depth)
        {
            slint::Timer::single_shot(Duration::from_millis(16), move || {
                finish_submenu_popup_presentation(window_id, depth, event_identity);
            });
        }
        if matches!(event, winit::event::WindowEvent::Focused(false))
            && let Some(event_identity) = current_submenu_event(window_id, depth)
        {
            slint::Timer::single_shot(Duration::from_millis(1), move || {
                if current_submenu_event(window_id, depth) == Some(event_identity)
                    && !quick_menu_popup_has_focus(window_id)
                {
                    dismiss_quick_menu_session(window_id, false);
                }
            });
        }
        if current_submenu_event(window_id, depth).is_some()
            && matches!(event, winit::event::WindowEvent::ScaleFactorChanged { .. })
        {
            reposition_quick_submenu_popups(window_id);
        }
        if matches!(event, winit::event::WindowEvent::Destroyed)
            && let Some(event_identity) = current_submenu_event(window_id, depth)
        {
            close_submenu_slot_and_descendants(window_id, depth, event_identity);
        }
        EventResult::Propagate
    });
}
fn quick_menu_event_is_current(
    window_id: WindowId,
    event: crate::quick_menu_popup::MenuEventIdentity,
) -> bool {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .get(&window_id)
            .is_some_and(|runtime| runtime.quick_menu_popup.session.matches_event(event))
    })
}

fn wire_root_popup_callbacks(root: &QuickMenuWindow, window_id: WindowId) {
    root.on_filter(move |query| {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id) {
                close_quick_submenu_windows(&mut runtime.quick_menu_popup);
                if let (Some(session), Some(root_branch)) = (
                    runtime.quick_menu_popup.session.identity(),
                    runtime
                        .quick_menu_popup
                        .session
                        .branches()
                        .first()
                        .map(|branch| branch.id),
                ) {
                    let _ = runtime.quick_menu_popup.session.close_to_branch(
                        crate::quick_menu_popup::MenuEventIdentity {
                            session,
                            branch: root_branch,
                        },
                    );
                }
            }
        });
        let ui = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .get(&window_id)
                .map(|runtime| runtime.ui.clone_strong())
        });
        if let Some(ui) = ui {
            ui.invoke_filter_context_menu(query);
        }
    });
    let weak = root.as_weak();
    root.on_move(move |index| {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                if index == -1 || index == 1 {
                    runtime.ui.invoke_move_context_selection(index);
                } else {
                    runtime.ui.set_context_active_index(index);
                }
                if let Some(root) = weak.upgrade() {
                    root.set_active_index(runtime.ui.get_context_active_index());
                }
            }
        });
    });
    root.on_activate(move |index| {
        if let Some(ui) = window_ui(window_id) {
            ui.set_context_active_index(index);
            ui.invoke_activate_context_selection();
        }
        let menu_closed = WINDOW_RUNTIMES.with_borrow(|runtimes| {
            runtimes
                .get(&window_id)
                .is_none_or(|runtime| !runtime.ui.get_context_menu_open())
        });
        if menu_closed {
            close_quick_menu_popup(window_id, true);
        }
    });
    root.on_open_submenu(move |index, anchor_y| {
        if let Some(ui) = window_ui(window_id) {
            ui.invoke_cancel_context_submenu_hover();
            ui.set_context_active_index(index);
            ui.invoke_open_context_submenu(index);
        }
        open_quick_submenu_popup(window_id, anchor_y, None);
    });
    root.on_hover(move |index, anchor_y| {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                let row = (index >= 0)
                    .then(|| runtime.ui.get_context_commands().row_data(index as usize))
                    .flatten();
                trace_quick_menu(
                    "quick_menu_root_hover",
                    format!(
                        "window={} index={} anchor_y={} label={:?} submenu={} node={} enabled={}",
                        window_id.0,
                        index,
                        anchor_y,
                        row.as_ref().map(|row| row.label.as_str()),
                        row.as_ref().is_some_and(|row| row.submenu),
                        row.as_ref().map_or(0, |row| row.node_id),
                        row.as_ref().is_some_and(|row| row.enabled),
                    ),
                );
                runtime.ui.set_context_active_index(index.max(0));
                runtime.ui.set_context_submenu_anchor_y(anchor_y);
                runtime.ui.invoke_hover_context_submenu(index);
            }
        });
    });
    root.on_cancel_hover(move || {
        WINDOW_RUNTIMES.with_borrow(|runtimes| {
            if let Some(runtime) = runtimes.get(&window_id) {
                runtime.ui.invoke_cancel_context_submenu_hover();
            }
        });
    });
    root.on_dismiss(move || dismiss_quick_menu_session(window_id, true));
    root.window().on_close_requested(move || {
        dismiss_quick_menu_session(window_id, true);
        slint::CloseRequestResponse::HideWindow
    });
    root.window().on_winit_window_event(move |_, event| {
        if matches!(event, winit::event::WindowEvent::RedrawRequested)
            && let Some(event_identity) = quick_menu_root_event(window_id)
        {
            slint::Timer::single_shot(Duration::from_millis(16), move || {
                finish_root_popup_presentation(window_id, event_identity);
            });
        }
        if matches!(event, winit::event::WindowEvent::Focused(false))
            && let Some(event_identity) = quick_menu_root_event(window_id)
        {
            slint::Timer::single_shot(Duration::from_millis(1), move || {
                if quick_menu_event_is_current(window_id, event_identity)
                    && !quick_menu_popup_has_focus(window_id)
                    && !window_belongs_to_quick_menu_owner(window_id)
                {
                    dismiss_quick_menu_session(window_id, false);
                }
            });
        }
        if matches!(event, winit::event::WindowEvent::ScaleFactorChanged { .. }) {
            reposition_quick_menu_root_and_submenus(window_id);
        }
        EventResult::Propagate
    });
}

fn window_belongs_to_quick_menu_owner(window_id: WindowId) -> bool {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let Some(runtime) = runtimes.get(&window_id) else {
            return false;
        };
        platform::windows::quick_menu_window::foreground_belongs_to(
            runtime.quick_menu_popup.owner_hwnd,
            &[native_window_handle(&runtime.ui)],
        )
    })
}

fn finish_root_popup_presentation(
    window_id: WindowId,
    event: crate::quick_menu_popup::MenuEventIdentity,
) {
    let hwnd = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let runtime = runtimes.get(&window_id)?;
        let popup = &runtime.quick_menu_popup;
        (popup.cloak_generation == Some(event.session.generation)
            && root_popup_presentation_ready(
                popup.presentation,
                runtime.ui.get_context_shell_loading(),
            )
            && popup.session.matches_event(event))
        .then(|| component_window_handle(&popup.root))
    });
    let Some(hwnd) = hwnd else {
        return;
    };
    let presented = platform::windows::quick_menu_window::flush_compositor().is_ok()
        && platform::windows::quick_menu_window::set_cloaked(hwnd, false).is_ok();
    if presented {
        WINDOW_RUNTIMES.with_borrow_mut(|runtimes| {
            if let Some(runtime) = runtimes.get_mut(&window_id)
                && runtime.quick_menu_popup.session.matches_event(event)
            {
                runtime.quick_menu_popup.presentation = PopupPresentation::Presented;
                runtime.quick_menu_popup.cloak_generation = None;
            }
        });
    } else {
        let _ = platform::windows::quick_menu_window::set_cloaked(hwnd, false);
        dismiss_quick_menu_session(window_id, false);
    }
}
fn quick_menu_root_event(
    window_id: WindowId,
) -> Option<crate::quick_menu_popup::MenuEventIdentity> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        Some(crate::quick_menu_popup::MenuEventIdentity {
            session: popup.session.identity()?,
            branch: popup.session.branches().first()?.id,
        })
    })
}

fn quick_menu_popup_has_focus(window_id: WindowId) -> bool {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let Some(runtime) = runtimes.get(&window_id) else {
            return false;
        };
        let handles = std::iter::once(component_window_handle(&runtime.quick_menu_popup.root))
            .chain(
                runtime
                    .quick_menu_popup
                    .branches
                    .iter()
                    .filter(|slot| slot.event.is_some())
                    .map(|branch| component_window_handle(&branch.window)),
            )
            .collect::<Vec<_>>();
        platform::windows::quick_menu_window::foreground_belongs_to(
            runtime.quick_menu_popup.owner_hwnd,
            &handles,
        )
    })
}

fn forward_quick_menu_key(window_id: WindowId, event: &winit::event::KeyEvent) {
    use slint::platform::{Key as SlintKey, WindowEvent};
    use winit::keyboard::{Key, NamedKey};
    let text: slint::SharedString = match &event.logical_key {
        Key::Character(text) => text.as_str().into(),
        Key::Named(key) => match key {
            NamedKey::ArrowUp => SlintKey::UpArrow.into(),
            NamedKey::ArrowDown => SlintKey::DownArrow.into(),
            NamedKey::ArrowLeft => SlintKey::LeftArrow.into(),
            NamedKey::ArrowRight => SlintKey::RightArrow.into(),
            NamedKey::Enter => SlintKey::Return.into(),
            NamedKey::Escape => SlintKey::Escape.into(),
            NamedKey::Backspace => SlintKey::Backspace.into(),
            NamedKey::Delete => SlintKey::Delete.into(),
            NamedKey::Home => SlintKey::Home.into(),
            NamedKey::End => SlintKey::End.into(),
            NamedKey::Tab => SlintKey::Tab.into(),
            NamedKey::Space => " ".into(),
            NamedKey::Shift => SlintKey::Shift.into(),
            NamedKey::Control => SlintKey::Control.into(),
            NamedKey::Alt => SlintKey::Alt.into(),
            _ => return,
        },
        _ => return,
    };
    let event = if event.state == winit::event::ElementState::Released {
        WindowEvent::KeyReleased { text }
    } else if event.repeat {
        WindowEvent::KeyPressRepeated { text }
    } else {
        WindowEvent::KeyPressed { text }
    };
    // Keep native focus on the owner while Slint edits the popup's focused control.
    let target = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let popup = &runtimes.get(&window_id)?.quick_menu_popup;
        popup.session.is_open().then(|| {
            popup
                .branches
                .iter()
                .rev()
                .find(|slot| slot.event.is_some())
                .map(|slot| (None, Some(slot.window.clone_strong())))
                .unwrap_or_else(|| (Some(popup.root.clone_strong()), None))
        })
    });
    if let Some((root, submenu)) = target {
        if let Some(root) = root {
            root.window().dispatch_event(event);
        } else if let Some(submenu) = submenu {
            submenu.window().dispatch_event(event);
        }
    }
}

fn dismiss_quick_menu_session(window_id: WindowId, restore_focus: bool) {
    let ui = window_ui(window_id);
    close_quick_menu_popup(window_id, restore_focus);
    if let Some(ui) = ui {
        if ui.get_context_menu_open() {
            ui.set_context_menu_open(false);
        }
        ui.invoke_dismiss_context_menu();
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
enum DraggedEntry {
    Local(EntryId, PathBuf),
    Remote {
        id: EntryId,
        identity: crate::domain::WebDavEntry,
        name: String,
        size: Option<u64>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragButton {
    Left,
    Right,
}

#[derive(Debug)]
struct InternalDrag {
    origin_tab: TabId,
    request_id: RequestId,
    entries: Vec<DraggedEntry>,
    start_x: f32,
    start_y: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DeferredEntryMenu {
    origin_tab: TabId,
    request_id: RequestId,
    entry_id: i32,
    x: f32,
    y: f32,
}

#[derive(Default)]
struct FileDragGesture {
    press: Option<(f32, f32, DragButton)>,
    pending: Option<InternalDrag>,
    // A right press may still become a drag, so its entry menu waits for the native release.
    entry_menu: Option<DeferredEntryMenu>,
}

impl FileDragGesture {
    fn cancel(&mut self, reason: &str) {
        if let Some(drag) = self.pending.take() {
            crate::operation_audit::record(
                "drag_cancelled",
                format!("reason={reason} drag={drag:?}"),
            );
        }
        self.press = None;
        self.entry_menu = None;
    }

    fn arm(&mut self, x: f32, y: f32) {
        self.arm_button(x, y, DragButton::Left);
    }

    fn arm_right(&mut self, x: f32, y: f32) {
        self.arm_button(x, y, DragButton::Right);
    }

    fn arm_button(&mut self, x: f32, y: f32, button: DragButton) {
        self.cancel("new_pointer_press");
        if x.is_finite() && y.is_finite() {
            self.press = Some((x, y, button));
        }
    }

    fn bind_entry(&mut self, app: &AppState, id: EntryId) {
        self.bind_button(app, id, DragButton::Left);
    }

    /// Returns false when no native right press authorized this gesture, so the menu opens at once.
    fn bind_right_press(&mut self, app: &AppState, entry_id: i32, x: f32, y: f32) -> bool {
        if !matches!(self.press, Some((_, _, DragButton::Right))) || entry_id < 0 {
            return false;
        }
        let tab = app.active();
        self.entry_menu = Some(DeferredEntryMenu {
            origin_tab: tab.id,
            request_id: tab.latest_request(),
            entry_id,
            x,
            y,
        });
        self.bind_button(app, EntryId(entry_id as u32), DragButton::Right);
        true
    }

    /// A native right release that never crossed the drag threshold becomes the ordinary entry menu.
    fn release_right(&mut self, app: &AppState) -> Option<DeferredEntryMenu> {
        let menu = self.entry_menu.take();
        self.cancel("right_release");
        let menu = menu?;
        let tab = app.active();
        (tab.id == menu.origin_tab && tab.latest_request() == menu.request_id).then_some(menu)
    }

    fn bind_button(&mut self, app: &AppState, id: EntryId, button: DragButton) {
        let Some((start_x, start_y, pressed)) = self.press.take() else {
            return;
        };
        if pressed != button {
            return;
        }
        let tab = app.active();
        let Some(entry) = tab.visible_entry(id) else {
            return;
        };
        let entries = if entry.webdav.is_some() {
            let selected = if tab.selected.contains(&id) {
                tab.selected
                    .iter()
                    .filter_map(|id| tab.visible_entry(*id))
                    .collect::<Vec<_>>()
            } else {
                vec![entry]
            };
            selected
                .into_iter()
                .filter(|entry| {
                    entry.webdav.is_some() && entry.kind == crate::domain::EntryKind::File
                })
                .filter_map(|entry| {
                    let identity = entry.webdav.clone()?;
                    Some(DraggedEntry::Remote {
                        id: entry.id,
                        identity,
                        name: entry.display_name.clone(),
                        size: entry.size_bytes,
                    })
                })
                .collect()
        } else if tab.selected.contains(&id) {
            tab.selected
                .iter()
                .filter_map(|id| tab.visible_entry(*id))
                .filter(|entry| entry.webdav.is_none())
                .map(|entry| DraggedEntry::Local(entry.id, entry.path()))
                .collect()
        } else {
            vec![DraggedEntry::Local(entry.id, entry.path())]
        };
        if entries.is_empty() {
            return;
        }
        let drag = InternalDrag {
            origin_tab: tab.id,
            request_id: tab.latest_request(),
            entries,
            start_x,
            start_y,
        };
        crate::operation_audit::record("drag_armed", format!("button={button:?} {drag:?}"));
        self.pending = Some(drag);
    }

    fn take_native_start(&mut self, app: &AppState, x: f32, y: f32) -> Option<InternalDrag> {
        let drag = self.pending.as_ref()?;
        let tab = app.active();
        if tab.id != drag.origin_tab
            || tab.latest_request() != drag.request_id
            || drag.entries.is_empty()
            || drag.entries.iter().any(|entry| match entry {
                DraggedEntry::Local(id, path) => tab
                    .visible_entry(*id)
                    .is_none_or(|entry| entry.webdav.is_some() || !entry.path.matches(path)),
                DraggedEntry::Remote { id, identity, .. } => tab
                    .visible_entry(*id)
                    .is_none_or(|entry| entry.webdav.as_ref() != Some(identity)),
            })
        {
            self.cancel("source_identity_changed");
            return None;
        }
        if !x.is_finite() || !y.is_finite() || (x - drag.start_x).hypot(y - drag.start_y) < 4.0 {
            return None;
        }
        self.entry_menu = None;
        self.pending.take()
    }
}

fn wire_file_drag_gesture(
    ui: &AppWindow,
    state: WindowSessions,
    gesture: Rc<RefCell<FileDragGesture>>,
) {
    let for_arm = gesture.clone();
    ui.on_arm_internal_drag(move |x, y| for_arm.borrow_mut().arm(x, y));
    let for_begin = gesture.clone();
    let state_for_begin = state.clone();
    ui.on_begin_internal_drag(move |entry_id| {
        if let Ok(app) = state_for_begin.lock() {
            for_begin
                .borrow_mut()
                .bind_entry(&app, EntryId(entry_id as u32));
        }
    });
    let for_arm_right = gesture.clone();
    ui.on_arm_internal_right_drag(move |x, y| for_arm_right.borrow_mut().arm_right(x, y));
    let for_right = gesture.clone();
    let state_for_right = state.clone();
    let weak = ui.as_weak();
    ui.on_begin_internal_right_drag(move |entry_id, x, y| {
        let deferred = state_for_right.lock().is_ok_and(|app| {
            for_right
                .borrow_mut()
                .bind_right_press(&app, entry_id, x, y)
        });
        if !deferred && let Some(ui) = weak.upgrade() {
            ui.invoke_show_entry_menu(entry_id, x, y);
        }
    });
    let for_release_right = gesture.clone();
    let weak = ui.as_weak();
    ui.on_release_internal_right_drag(move || {
        let menu = state
            .lock()
            .ok()
            .and_then(|app| for_release_right.borrow_mut().release_right(&app));
        if let Some(menu) = menu
            && let Some(ui) = weak.upgrade()
        {
            ui.invoke_show_entry_menu(menu.entry_id, menu.x, menu.y);
        }
    });
    ui.on_cancel_internal_drag(move || gesture.borrow_mut().cancel("release_or_context_change"));
}

fn wire_internal_drag_drop(
    ui: &AppWindow,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    state: WindowSessions,
) {
    let gesture = Rc::new(RefCell::new(FileDragGesture::default()));
    wire_file_drag_gesture(ui, state.clone(), gesture.clone());
    let weak = ui.as_weak();
    ui.on_update_internal_drag(move |x, y| {
        let Some(ui) = weak.upgrade() else {
            return false;
        };
        let drag = state
            .lock()
            .ok()
            .and_then(|app| gesture.borrow_mut().take_native_start(&app, x, y));
        let Some(drag) = drag else { return false };
        if drag
            .entries
            .iter()
            .all(|entry| matches!(entry, DraggedEntry::Remote { .. }))
        {
            let files = drag
                .entries
                .iter()
                .filter_map(|entry| match entry {
                    DraggedEntry::Remote {
                        name,
                        size,
                        identity,
                        ..
                    } => Some((name.clone(), *size, identity.clone())),
                    DraggedEntry::Local(_, _) => None,
                })
                .collect::<Vec<_>>();
            let prepared = state
                .lock()
                .ok()
                .and_then(|app| webdav_ui::prepare_remote_drag(&app, &files));
            ui.invoke_release_internal_drag_pointer();
            ui.set_drop_hover_entry_id(-1);
            let Some(prepared) = prepared else {
                if let Ok(mut app) = state.lock() {
                    app.operation_errors
                        .push("webdav drag could not start".to_owned());
                }
                return true;
            };
            let files = prepared
                .into_iter()
                .map(|file| platform::windows::drag_drop::VirtualOutboundFile {
                    name: file.name,
                    size: file.size,
                    open: file.open,
                })
                .collect();
            match run_outbound_ole(&ui, || {
                platform::windows::drag_drop::begin_virtual_outbound_drag(files)
            }) {
                Ok(_) => {}
                Err(error) => {
                    if let Ok(mut app) = state.lock() {
                        app.operation_errors
                            .push(format!("outbound drag failed: {error}"));
                    }
                }
            }
            return true;
        }
        let paths = drag
            .entries
            .iter()
            .filter_map(|entry| match entry {
                DraggedEntry::Local(_, path) => Some(path.clone()),
                DraggedEntry::Remote { .. } => None,
            })
            .collect::<Vec<_>>();
        let source_directories = paths
            .iter()
            .filter_map(|path| path.parent().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        // Only a native pointer move can enter OLE; releasing a Slint item never authorizes a file operation.
        ui.invoke_release_internal_drag_pointer();
        ui.set_drop_hover_entry_id(-1);
        crate::operation_audit::record("drag_ole_started", format!("{drag:?} pointer=({x},{y})"));
        let result = run_outbound_ole(&ui, || {
            platform::windows::drag_drop::begin_outbound_drag(
                &paths,
                platform::windows::drag_drop::DropEffect::Move,
            )
        });
        crate::operation_audit::record(
            "drag_ole_finished",
            format!("source={paths:?} result={result:?}"),
        );
        match result {
            Ok(result) if should_refresh_outbound_drag_source(result) => {
                refresh_affected_tabs(
                    &directory_sender,
                    &network_directory_sender,
                    &state,
                    &source_directories,
                );
            }
            Ok(_) => {}
            Err(error) => {
                if let Ok(mut app) = state.lock() {
                    app.operation_errors
                        .push(format!("outbound drag failed: {error}"));
                }
            }
        }
        true
    });
}

/// Runs a file drag from inside the winit cursor callback that authorized it.
fn run_outbound_ole<T>(ui: &AppWindow, drag: impl FnOnce() -> T) -> T {
    // OLE owns the pointer until the drop, so Slint would keep the source row hovered and open
    // its tooltip; the caller also swallows the stale cursor move that started the drag.
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerExited);
    let result = drag();
    // DoDragDrop pumps messages inside this callback, and winit buffers events posted there
    // (such as our own right-drop menu) until another event arrives. A fresh post drains them.
    let _ = slint::invoke_from_event_loop(|| {});
    result
}

fn wire_address_drag(ui: &AppWindow, state: WindowSessions) {
    let start = Arc::new(Mutex::new(None::<(f32, f32, PathBuf)>));
    let state_for_begin = state.clone();
    let start_for_begin = start.clone();
    ui.on_begin_address_drag(move |x, y| {
        let path = state_for_begin.lock().ok().and_then(|app| {
            let tab = app.active();
            (tab.kind == TabKind::Files
                && tab.page_source == PageSource::Directory
                && tab.load_state == LoadState::Complete)
                .then(|| tab.visible_path().map(Path::to_path_buf))
                .flatten()
        });
        if let Some(path) = path
            && let Ok(mut drag) = start_for_begin.lock()
        {
            *drag = Some((x, y, path));
        }
    });
    let weak = ui.as_weak();
    let start_for_update = start.clone();
    let shared = state.shared.clone();
    ui.on_update_address_drag(move |x, y| {
        let path = start_for_update.lock().ok().and_then(|mut drag| {
            let (start_x, start_y, path) = drag.as_ref()?.clone();
            if ((x - start_x).powi(2) + (y - start_y).powi(2)).sqrt() < 4.0 {
                return None;
            }
            *drag = None;
            Some(path)
        });
        let Some(path) = path else { return };
        if let Some(ui) = weak.upgrade() {
            ui.invoke_release_internal_drag_pointer();
        }
        match platform::windows::drag_drop::begin_outbound_drag(
            &[path],
            platform::windows::drag_drop::DropEffect::Link,
        ) {
            Ok(_) => {}
            Err(error) => {
                if let Ok(mut app) = shared.lock() {
                    app.operation_errors
                        .push(format!("address drag failed: {error}"));
                }
            }
        }
    });
    ui.on_end_address_drag(move || {
        if let Ok(mut drag) = start.lock() {
            *drag = None;
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

fn projected_rectangle_hits(ui: &AppWindow, rect: SelectionRect) -> Option<HashSet<EntryId>> {
    let geometry = file_layout_geometry(projected_view_mode(ui));
    let grid = ui.get_grid_rows();
    if let Some(grid) = grid
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>()
    {
        let mut hits = HashSet::new();
        for row in grid.rows_between(rect.top, rect.bottom) {
            let top = grid.top_of(row);
            for (column, id) in grid.entry_ids(row).into_iter().enumerate() {
                let left = 16.0 + column as f32 * (geometry.card_width + 8.0);
                if rect.intersects(SelectionRect {
                    left,
                    top,
                    right: left + geometry.card_width,
                    bottom: top + geometry.card_height,
                }) {
                    hits.insert(id);
                }
            }
        }
        return Some(hits);
    }
    let rows = directory_rows(ui)?;
    let (left, right) = if projected_view_mode(ui) == ViewMode::Details {
        FileHitGeometry {
            viewport_x: ui.get_file_viewport_x(),
            viewport_width: ui.get_file_viewport_width(),
            columns_width: ui.get_details_hit_width(),
            ..FileHitGeometry::default()
        }
        .details_range()
    } else {
        (16.0, (ui.get_file_viewport_width() - 16.0).max(16.0))
    };
    let mut hits = HashSet::new();
    if right <= left {
        return Some(hits);
    }
    for row in rows.rows_between(rect.top, rect.bottom) {
        let top = rows.top_of(row);
        if let Some(id) = rows.entry_id(row)
            && rect.intersects(SelectionRect {
                left,
                top,
                right,
                bottom: top + geometry.row_height,
            })
        {
            hits.insert(id);
        }
    }
    Some(hits)
}

fn rectangle_selection_hits(
    tab: &TabSession,
    view_mode: ViewMode,
    grid_columns: usize,
    viewport_width: f32,
    details_viewport_x: f32,
    details_columns_width: f32,
    rect: SelectionRect,
) -> HashSet<EntryId> {
    let geometry = file_layout_geometry(view_mode);
    let (row_height, card_width, card_height, gap) = (
        geometry.row_height,
        geometry.card_width,
        geometry.card_height,
        if geometry.grid { 8.0 } else { 0.0 },
    );
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
    let grid_content_left = 16.0;
    let candidate_start = |position: f32, extent: f32| {
        ((position.max(0.0) / extent).floor() as usize).saturating_sub(1)
    };
    let mut hits = HashSet::new();
    if !geometry.grid {
        let (item_left, item_right) = if view_mode == ViewMode::Details {
            FileHitGeometry {
                viewport_x: details_viewport_x,
                viewport_width,
                columns_width: details_columns_width,
                ..FileHitGeometry::default()
            }
            .details_range()
        } else {
            (16.0, (viewport_width - 16.0).max(16.0))
        };
        if item_right <= item_left || rect.right < item_left || rect.left >= item_right {
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
                left: item_left,
                top: slot as f32 * row_height,
                right: item_right,
                bottom: (slot + 1) as f32 * row_height,
            };
            if rect.intersects(item) {
                hits.insert(entry.id);
            }
        }
    } else {
        let columns = grid_columns.max(1);
        let column_extent = card_width + gap;
        let first_column =
            candidate_start(rect.left - grid_content_left, column_extent).min(columns - 1);
        let last_column = (((rect.right - grid_content_left).max(0.0) / column_extent).floor()
            as usize)
            .min(columns - 1);
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
                let left = grid_content_left + column as f32 * column_extent;
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
    hits
}

#[allow(clippy::too_many_arguments)]
fn rectangle_selection_hits_for_app(
    app: &AppState,
    tab_id: TabId,
    view_mode: ViewMode,
    grid_columns: usize,
    viewport_width: f32,
    details_viewport_x: f32,
    details_columns_width: f32,
    rect: SelectionRect,
    shown_rows: Option<&VecModel<FileRow>>,
) -> HashSet<EntryId> {
    let Some(tab) = app.tab(tab_id) else {
        return HashSet::new();
    };
    let grouped = matches!(tab.visible_location(), Some(NavigationLocation::Library(_)))
        || tab
            .visible_path()
            .map(|path| app.directory_preference(path).group_field != GroupField::None)
            .unwrap_or(false);
    if tab.page_source == PageSource::Search || !grouped {
        return rectangle_selection_hits(
            tab,
            view_mode,
            grid_columns,
            viewport_width,
            details_viewport_x,
            details_columns_width,
            rect,
        );
    }
    let geometry = file_layout_geometry(view_mode);
    let mut hits = HashSet::new();
    if geometry.grid {
        let groups = directory_group_projections(app, tab, tab.visible_entries());
        let projection = IconProjection::from_groups(
            &groups,
            grid_columns,
            group_header_height(&groups),
            geometry.row_height as u64,
        );
        for entry in tab.visible_entries() {
            let Some(position) = projection.entry_position(entry.id) else {
                continue;
            };
            let Some(top) = projection.offsets.row_start(position.row_index) else {
                continue;
            };
            let left = 16.0 + position.column_index as f32 * (geometry.card_width + 8.0);
            if rect.intersects(SelectionRect {
                left,
                top: top as f32,
                right: left + geometry.card_width,
                bottom: top as f32 + geometry.card_height,
            }) {
                hits.insert(entry.id);
            }
        }
    } else {
        let (left, right) = if view_mode == ViewMode::Details {
            FileHitGeometry {
                viewport_x: details_viewport_x,
                viewport_width,
                columns_width: details_columns_width,
                ..FileHitGeometry::default()
            }
            .details_range()
        } else {
            (16.0, (viewport_width - 16.0).max(16.0))
        };
        if right <= left {
            return hits;
        }
        // Shown grouped rows already carry the projection; walk only rows inside the rectangle.
        if let Some(model) = shown_rows
            && let Some(range) =
                file_row_index::rows_between(model, geometry.row_height, rect.top, rect.bottom)
            && let Some(mut top) = file_row_index::top_of(model, geometry.row_height, range.start)
        {
            for row in range.filter_map(|index| model.row_data(index)) {
                let height = if row.group_header {
                    group_header_height_for_detail(row.group_detail.as_str()) as f32
                } else {
                    geometry.row_height
                };
                if !row.group_header
                    && row.id > 0
                    && rect.intersects(SelectionRect {
                        left,
                        top,
                        right,
                        bottom: top + geometry.row_height,
                    })
                {
                    hits.insert(EntryId(row.id as u32));
                }
                top += height;
            }
            return hits;
        }
        let groups = directory_group_projections(app, tab, tab.visible_entries());
        let projection = ListProjection::from_groups(
            &groups,
            group_header_height(&groups),
            geometry.row_height as u64,
        );
        for entry in tab.visible_entries() {
            let Some(position) = projection.entry_position(entry.id) else {
                continue;
            };
            let Some(top) = projection.offsets.row_start(position) else {
                continue;
            };
            if rect.intersects(SelectionRect {
                left,
                top: top as f32,
                right,
                bottom: top as f32 + geometry.row_height,
            }) {
                hits.insert(entry.id);
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
            if tab.latest_request() != request_id {
                None
            } else {
                let changed = mutate_selection(tab, |tab| {
                    restore_rectangle_selection_snapshot(tab, snapshot.as_ref(), focused, anchor);
                });
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
            let view_mode = app.active_view_mode();
            let grid_columns = ui.get_grid_column_count().max(1) as usize;
            let viewport_width = ui.get_file_viewport_width();

            let files = ui.get_files();
            let shown_rows = app
                .tab(tab_id)
                .filter(|tab| {
                    ui.get_projected_file_tab_id() == tab_id.0 as i32
                        && ui.get_projected_file_request_id() == tab.latest_request().0 as i32
                })
                .and_then(|_| files.as_any().downcast_ref::<VecModel<FileRow>>());
            let hits = projected_rectangle_hits(ui, rect).unwrap_or_else(|| {
                rectangle_selection_hits_for_app(
                    &app,
                    tab_id,
                    view_mode,
                    grid_columns,
                    viewport_width,
                    ui.get_file_viewport_x(),
                    ui.get_details_hit_width(),
                    rect,
                    shown_rows,
                )
            });
            let tab = app.active_window_state_mut().tabs.get_mut(&tab_id).unwrap();
            if tab.latest_request() != request_id {
                None
            } else {
                let previous_focus = tab.focused;
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
                request_id: tab.latest_request(),
                start_x: x,
                start_content_y: y - ui.get_file_viewport_y(),
                pointer_x: x,
                pointer_y: y,
                snapshot: Arc::new(tab.selected.iter().copied().collect()),
                snapshot_order: tab.selected.to_vec(),
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
                    let mode = projected_view_mode(&ui);
                    let maximum =
                        projected_scroll_maximum(&ui, mode, ui.get_file_viewport_height());
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

fn update_network_discovery(
    state: &WindowSessions,
    sender: &mpsc::SyncSender<NetworkDiscoveryRequest>,
    expanded: bool,
) {
    let window_id = state.window_id;
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let coordinator = app.network_discovery.entry(window_id).or_default();
        if expanded {
            let (request_id, cancel) = coordinator.begin();
            Some(NetworkDiscoveryRequest::Discover {
                window_id,
                request_id,
                cancel,
            })
        } else {
            coordinator.cancel_current();
            None
        }
    };
    if let Some(request) = request
        && sender.try_send(request).is_err()
        && let Ok(mut app) = state.lock()
    {
        if let Some(coordinator) = app.network_discovery.get_mut(&window_id) {
            coordinator.cancel_current();
        }
        app.network_discovery_errors
            .insert(window_id, "Windows 网络发现正忙，请稍后重试".to_owned());
    }
    refresh_all_windows(state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
}

fn network_discovery_needed(app: &AppState, window_id: WindowId) -> bool {
    app.network_discovery
        .get(&window_id)
        .is_none_or(|discovery| discovery.devices().is_empty())
}
#[derive(Clone, Copy)]
enum SystemIntegrationAction {
    EnableFolder,
    RepairFolder,
    RestoreFolder,
    EnableWinE,
    RepairWinE,
    RestoreWinE,
}

impl SystemIntegrationAction {
    fn error_slot(self, integration: &mut app_state::IntegrationState) -> &mut Option<String> {
        match self {
            Self::EnableFolder | Self::RepairFolder | Self::RestoreFolder => {
                &mut integration.folder_error
            }
            Self::EnableWinE | Self::RepairWinE | Self::RestoreWinE => &mut integration.win_e_error,
        }
    }

    fn run(self) -> std::io::Result<()> {
        match self {
            Self::EnableFolder => platform::windows::shell_integration::enable_folder_open(),
            Self::RepairFolder => platform::windows::shell_integration::repair_folder_open(),
            Self::RestoreFolder => platform::windows::shell_integration::restore_folder_open(),
            Self::EnableWinE => platform::windows::shell_integration::enable_win_e(),
            Self::RepairWinE => platform::windows::shell_integration::repair_win_e(),
            Self::RestoreWinE => platform::windows::shell_integration::restore_win_e(),
        }
    }
}

fn refresh_system_integration_state(state: SharedSessions) {
    let generation = if let Ok(mut app) = state.lock() {
        app.integration.generation = app.integration.generation.wrapping_add(1).max(1);
        app.integration.generation
    } else {
        return;
    };
    thread::spawn(move || {
        let folder = platform::windows::shell_integration::folder_open_status();
        let win_e = platform::windows::shell_integration::win_e_status();
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut app) = state.lock()
                && app.integration.generation == generation
            {
                app.integration.folder_error = folder.as_ref().err().map(ToString::to_string);
                app.integration.win_e_error = win_e.as_ref().err().map(ToString::to_string);
                app.integration.folder_status = folder.ok();
                app.integration.win_e_status = win_e.ok();
            }
            refresh_all_windows(&state, RefreshRegions::SETTINGS);
        });
    });
}

fn run_system_integration_action(state: SharedSessions, action: SystemIntegrationAction) {
    if let Ok(mut app) = state.lock() {
        if app.integration.busy {
            return;
        }
        app.integration.busy = true;
        app.integration.generation = app.integration.generation.wrapping_add(1).max(1);
        *action.error_slot(&mut app.integration) = None;
    }
    refresh_all_windows(&state, RefreshRegions::SETTINGS);
    thread::spawn(move || {
        let result = action.run();

        let folder = platform::windows::shell_integration::folder_open_status();
        let win_e = platform::windows::shell_integration::win_e_status();
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut app) = state.lock() {
                app.integration.busy = false;
                *action.error_slot(&mut app.integration) =
                    result.err().map(|error| error.to_string());
                app.integration.folder_status = folder.ok();
                app.integration.win_e_status = win_e.ok();
            }
            refresh_all_windows(&state, RefreshRegions::SETTINGS);
        });
    });
}

fn wire_system_integration(ui: &AppWindow, state: &WindowSessions) {
    let shared = state.shared.clone();
    ui.on_enable_folder_integration({
        let state = shared.clone();
        move || run_system_integration_action(state.clone(), SystemIntegrationAction::EnableFolder)
    });
    ui.on_repair_folder_integration({
        let state = shared.clone();
        move || run_system_integration_action(state.clone(), SystemIntegrationAction::RepairFolder)
    });
    ui.on_restore_folder_integration({
        let state = shared.clone();
        move || run_system_integration_action(state.clone(), SystemIntegrationAction::RestoreFolder)
    });
    ui.on_enable_win_e_integration({
        let state = shared.clone();
        move || run_system_integration_action(state.clone(), SystemIntegrationAction::EnableWinE)
    });
    ui.on_repair_win_e_integration({
        let state = shared.clone();
        move || run_system_integration_action(state.clone(), SystemIntegrationAction::RepairWinE)
    });
    ui.on_restore_win_e_integration({
        let state = shared.clone();
        move || run_system_integration_action(state.clone(), SystemIntegrationAction::RestoreWinE)
    });
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeFileDragAction {
    Arm(DragButton),
    ReleaseRight,
    Cancel,
}

/// Only native presses may authorize a file drag; Slint item events merely bind the pressed entry.
fn native_file_drag_action(event: &winit::event::WindowEvent) -> Option<NativeFileDragAction> {
    use winit::event::{ElementState, MouseButton, WindowEvent};
    match event {
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Left,
            ..
        } => Some(NativeFileDragAction::Arm(DragButton::Left)),
        WindowEvent::MouseInput {
            state: ElementState::Pressed,
            button: MouseButton::Right,
            ..
        } => Some(NativeFileDragAction::Arm(DragButton::Right)),
        // Explorer opens the entry menu on release; handling it at the native entry keeps the
        // menu independent of a Slint item that window activation may rebuild (#53).
        WindowEvent::MouseInput {
            state: ElementState::Released,
            button: MouseButton::Right,
            ..
        } => Some(NativeFileDragAction::ReleaseRight),
        WindowEvent::MouseInput { .. }
        | WindowEvent::Focused(false)
        | WindowEvent::CloseRequested
        | WindowEvent::Occluded(true)
        | WindowEvent::CursorLeft { .. }
        | WindowEvent::Resized(_)
        | WindowEvent::ScaleFactorChanged { .. }
        | WindowEvent::MouseWheel { .. }
        | WindowEvent::KeyboardInput { .. } => Some(NativeFileDragAction::Cancel),
        _ => None,
    }
}

fn cancel_scrollbar_pointer(ui: &AppWindow) -> bool {
    if !ui.get_scrollbar_pointer_active() {
        return false;
    }
    // Slint keeps its mouse grab on Windows CursorLeft while a button is held (#178).
    // Exit cancels that grab without synthesizing a click or affecting other gestures.
    ui.window()
        .dispatch_event(slint::platform::WindowEvent::PointerExited);
    true
}

fn prepare_scrollbar_event(ui: &AppWindow, event: &winit::event::WindowEvent) {
    use winit::event::WindowEvent;
    if matches!(
        event,
        WindowEvent::CursorLeft { .. }
            | WindowEvent::Focused(false)
            | WindowEvent::Occluded(true)
            | WindowEvent::CloseRequested
            | WindowEvent::Destroyed
    ) {
        cancel_scrollbar_pointer(ui);
    }
}

fn prepare_context_menu_event(
    ui: &AppWindow,
    event: &winit::event::WindowEvent,
    cursor_position: winit::dpi::PhysicalPosition<f64>,
) -> bool {
    // Menu cleanup returns early for every left press, even when no menu is open.
    if matches!(
        event,
        winit::event::WindowEvent::MouseInput {
            state: winit::event::ElementState::Pressed,
            ..
        }
    ) {
        let logical = cursor_position.to_logical::<f32>(f64::from(ui.window().scale_factor()));
        ui.invoke_dismiss_address_edit_outside(logical.x, logical.y);
    }
    should_close_context_menu(event)
}

fn should_close_context_menu(event: &winit::event::WindowEvent) -> bool {
    matches!(
        event,
        winit::event::WindowEvent::MouseInput {
            state: winit::event::ElementState::Pressed,
            button: winit::event::MouseButton::Left,
            ..
        } | winit::event::WindowEvent::Focused(false)
            | winit::event::WindowEvent::Occluded(true)
            | winit::event::WindowEvent::Destroyed
    )
}

fn file_list_type_select_allowed(
    file_list_target: bool,
    rectangle_selection_active: bool,
    modifiers: [bool; 4],
    ime_composing: bool,
    text: Option<&str>,
) -> bool {
    file_list_target
        && !rectangle_selection_active
        && !modifiers.into_iter().any(|pressed| pressed)
        && !ime_composing
        && text.is_some_and(|value| {
            let mut chars = value.chars();
            chars
                .next()
                .is_some_and(|character| character.is_alphanumeric())
                && chars.next().is_none()
        })
}
fn keyboard_shortcuts_suppressed(rename_editing: bool, context_menu_open: bool) -> bool {
    rename_editing || context_menu_open
}

fn delete_shortcut_request(
    is_delete: bool,
    control: bool,
    alt: bool,
    settings_active: bool,
    editing_address: bool,
    shift: bool,
) -> Option<bool> {
    (is_delete && !control && !alt && !settings_active && !editing_address).then_some(shift)
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
        event::{ElementState, WindowEvent},
        keyboard::{Key, ModifiersState, NamedKey},
    };

    let weak = ui.as_weak();
    let exit_weak = confirmations.exit.clone();
    let modifiers = Cell::new(ModifiersState::empty());
    let ime_composing = Cell::new(false);
    let type_select = Rc::new(RefCell::new(TypeSelectState::default()));
    let cursor_position = Cell::new(winit::dpi::PhysicalPosition::new(0.0, 0.0));
    let ctrl_wheel_accumulator = Cell::new(0.0_f32);
    let ctrl_wheel_context = Cell::new((0, 0));
    let geometry_refresh = slint::Timer::default();
    let mut installed_hwnd = 0;
    ui.window().on_winit_window_event(move |window, event| {
        handle_window_adaptation(window, event, &mut installed_hwnd);
        if let Some(ui) = weak.upgrade() {
            prepare_scrollbar_event(&ui, event);
            let position = || {
                cursor_position
                    .get()
                    .to_logical::<f32>(f64::from(ui.window().scale_factor()))
            };
            match native_file_drag_action(event) {
                Some(NativeFileDragAction::Arm(DragButton::Left)) => {
                    let position = position();
                    ui.invoke_arm_internal_drag(position.x, position.y);
                }
                Some(NativeFileDragAction::Arm(DragButton::Right)) => {
                    let position = position();
                    ui.invoke_arm_internal_right_drag(position.x, position.y);
                }
                Some(NativeFileDragAction::ReleaseRight) => ui.invoke_release_internal_right_drag(),
                Some(NativeFileDragAction::Cancel) => ui.invoke_cancel_internal_drag(),
                None => {}
            }
        }
        if matches!(event, WindowEvent::Focused(true)) {
            if let Ok(mut app) = state.lock() {
                app.active_window = window_id;
            }
            begin_drive_capacity_refresh(&state, None, Duration::from_secs(15));
        }
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
                    dismiss_quick_menu_session(window_id, false);
                    if let Ok(mut app) = state.lock() {
                        app.cancel_tab_drag_for_window(window_id);
                        let _ = app.close_window(window_id);
                    }
                    project_native_insertion_indicator(None, &state);
                    remove_window_runtime(window_id);
                    return EventResult::Propagate;
                }
                WindowCloseAction::ExitApplication => {
                    dismiss_quick_menu_session(window_id, false);
                    platform::windows::network::record_runtime_event("event_loop_exit_requested");
                    platform::windows::drag_drop::begin_shutdown_current();
                    return EventResult::Propagate;
                }
                WindowCloseAction::Ignore => return EventResult::PreventDefault,
            }
        }
        if matches!(event, WindowEvent::Focused(true))
            && let Some(ui) = weak.upgrade()
            && ui.get_context_menu_open()
        {
            // The owner receives activation before the underlying control's mouse event.
            // Retire the popup here so the same click can reach that control.
            dismiss_quick_menu_session(window_id, false);
        }
        let Some(ui) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        if prepare_context_menu_event(&ui, event, cursor_position.get()) {
            if platform::windows::has_pointer_capture(native_window_handle(&ui)) {
                platform::windows::release_pointer_capture();
            }
            if let Some(ui) = weak.upgrade() {
                if ui.get_rectangle_selection_pointer_active() {
                    ui.invoke_cancel_rectangle_selection();
                }
                if ui.invoke_has_column_drag() {
                    ui.invoke_cancel_column_drag_from_window();
                }
                let popup_handles = WINDOW_RUNTIMES.with_borrow(|runtimes| {
                    runtimes
                        .get(&window_id)
                        .map(|runtime| {
                            std::iter::once(component_window_handle(&runtime.quick_menu_popup.root))
                                .chain(
                                    runtime
                                        .quick_menu_popup
                                        .branches
                                        .iter()
                                        .filter(|slot| slot.event.is_some())
                                        .map(|branch| component_window_handle(&branch.window)),
                                )
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                });
                let focus_entered_owned_popup = matches!(event, WindowEvent::Focused(false))
                    && platform::windows::quick_menu_window::foreground_belongs_to(
                        native_window_handle(&ui),
                        &popup_handles,
                    );
                if ui.get_context_menu_open() && !focus_entered_owned_popup {
                    ui.set_context_menu_open(false);
                    ui.invoke_dismiss_context_menu();
                    close_quick_menu_popup(window_id, false);
                }
                ui.set_drop_menu_open(false);
                if state
                    .lock()
                    .is_ok_and(|app| app.tab_drag.is_some_and(|drag| drag.window_id == window_id))
                {
                    ui.invoke_cancel_tab_drag();
                }
            }
            if let Ok(mut app) = state.lock() {
                app.pending_right_drops.remove(&window_id);
            }
            project_native_insertion_indicator(None, &state);
            return EventResult::Propagate;
        }
        if let WindowEvent::ModifiersChanged(changed) = event {
            modifiers.set(changed.state());
            if !changed.state().control_key() {
                ctrl_wheel_accumulator.set(0.0);
            }
            return EventResult::Propagate;
        }
        if let WindowEvent::KeyboardInput { event, .. } = event
            && weak.upgrade().is_some_and(|ui| ui.get_context_menu_open())
        {
            forward_quick_menu_key(window_id, event);
            return EventResult::PreventDefault;
        }
        if let WindowEvent::Ime(ime) = event {
            match ime {
                winit::event::Ime::Preedit(value, _) => {
                    ime_composing.set(!value.is_empty());
                }
                winit::event::Ime::Commit(_) | winit::event::Ime::Disabled => {
                    ime_composing.set(false);
                }
                winit::event::Ime::Enabled => {}
            }
            type_select.borrow_mut().clear();
            return EventResult::Propagate;
        }
        if matches!(
            event,
            WindowEvent::Focused(false) | WindowEvent::Occluded(true)
        ) {
            ime_composing.set(false);
            type_select.borrow_mut().clear();
        }
        if matches!(
            event,
            WindowEvent::Resized(_)
                | WindowEvent::ScaleFactorChanged { .. }
                | WindowEvent::Moved(_)
        ) {
            let resized = matches!(event, WindowEvent::Resized(_));
            if resized {
                platform::windows::window_trace::log_request(
                    native_window_handle(&ui),
                    "winit-resized",
                );
            }
            if (resized || matches!(event, WindowEvent::ScaleFactorChanged { .. }))
                && !geometry_refresh.running()
            {
                // Slint commits geometry after this filter. Coalesce model reflows
                // and read its logical layout instead of mixing physical size and DPI.
                let weak = ui.as_weak();
                let shared_state = shared_state.clone();
                let scheduler = senders.thumbnail.clone();
                geometry_refresh.start(slint::TimerMode::SingleShot, Duration::ZERO, move || {
                    if let Some(ui) = weak.upgrade() {
                        reflow_after_window_resize(&ui, &shared_state, window_id);
                        request_visible_file_images(&ui, &shared_state, window_id, &scheduler);
                    }
                });
            }
            reposition_quick_menu_root_and_submenus(window_id);
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
            let type_select_active = type_select.borrow_mut().clear();
            let mut cancelled = cancel_scrollbar_pointer(&ui);
            if ui.get_rectangle_selection_pointer_active() {
                ui.invoke_cancel_rectangle_selection();
                cancelled = true;
            }
            if ui.get_drop_menu_open() {
                ui.set_drop_menu_open(false);
                if let Ok(mut app) = state.lock() {
                    app.pending_right_drops.remove(&window_id);
                }
                cancelled = true;
            }
            if state.lock().is_ok_and(|app| app.tab_drag.is_some()) {
                ui.invoke_cancel_tab_drag();
                cancelled = true;
            }
            if state.lock().is_ok_and(|app| app.column_drag.is_some()) {
                ui.invoke_cancel_column_drag_from_window();
                cancelled = true;
            }
            if !cancelled
                && !type_select_active
                && !ui.get_active_is_settings()
                && !ui.get_address_editing()
                && !ui.get_rename_editing()
                && ui.get_file_list_keyboard_target()
                && request_cancel_clipboard_cut(&state, &senders.clipboard)
            {
                cancelled = true;
            }
            if !cancelled
                && !type_select_active
                && !ui.get_active_is_settings()
                && !ui.get_address_editing()
                && !ui.get_rename_editing()
                && ui.get_file_list_keyboard_target()
            {
                ui.invoke_clear_selection();
                cancelled = true;
            }
            return if cancelled || type_select_active {
                EventResult::PreventDefault
            } else {
                EventResult::Propagate
            };
        }
        if matches!(event, WindowEvent::KeyboardInput { .. })
            && keyboard_shortcuts_suppressed(ui.get_rename_editing(), ui.get_context_menu_open())
        {
            type_select.borrow_mut().clear();
            return EventResult::Propagate;
        }
        if matches!(
            event,
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            }
        ) {
            type_select.borrow_mut().clear();
            let logical = cursor_position
                .get()
                .to_logical::<f32>(f64::from(ui.window().scale_factor()));
            if logical.x >= ui.get_file_list_left() || logical.y < ui.get_file_list_top() {
                ui.set_sidebar_keyboard_active(false);
            }
        }
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                cursor_position.set(*position);
                let logical = position.to_logical::<f32>(f64::from(ui.window().scale_factor()));
                if ui.invoke_update_internal_drag(logical.x, logical.y) {
                    // This move predates the drop; replaying it would re-hover the source row.
                    EventResult::PreventDefault
                } else {
                    EventResult::Propagate
                }
            }
            WindowEvent::MouseWheel { delta, .. } if !ui.get_active_is_settings() => {
                let logical = cursor_position
                    .get()
                    .to_logical::<f32>(f64::from(ui.window().scale_factor()));
                // The last folder keeps its rectangle after returning home, so that
                // hit test would swallow the home page wheel and scroll a hidden list.
                if ui.get_active_is_home()
                    || !pointer_targets_file_area(
                        logical.x,
                        logical.y,
                        ui.get_file_list_left(),
                        ui.get_file_list_top(),
                        ui.get_file_viewport_width(),
                        ui.get_file_viewport_height(),
                    )
                {
                    ctrl_wheel_accumulator.set(0.0);
                    return EventResult::Propagate;
                }
                let view_mode = projected_view_mode(&ui);
                let control = modifiers.get().control_key();
                if control {
                    if ui.get_context_menu_open()
                        || ui.get_rename_editing()
                        || ui.get_rectangle_selection_pointer_active()
                        || ui.get_drop_menu_open()
                        || state
                            .lock()
                            .is_ok_and(|app| app.tab_drag.is_some() || app.column_drag.is_some())
                    {
                        ctrl_wheel_accumulator.set(0.0);
                        return EventResult::Propagate;
                    }
                    let context = (
                        ui.get_projected_file_tab_id(),
                        ui.get_projected_file_request_id(),
                    );
                    if ctrl_wheel_context.replace(context) != context {
                        ctrl_wheel_accumulator.set(0.0);
                    }
                    let mut accumulated = ctrl_wheel_accumulator.get();
                    let step = ctrl_wheel_step(delta, ui.window().scale_factor(), &mut accumulated);
                    ctrl_wheel_accumulator.set(accumulated);
                    if step != 0 {
                        let mut next = view_mode;
                        for _ in 0..step.unsigned_abs() {
                            next = next.step_ctrl_wheel(step > 0);
                        }
                        if next != view_mode {
                            let anchor = capture_zoom_anchor(
                                &ui,
                                logical.x - ui.get_file_list_left(),
                                logical.y - ui.get_file_list_top(),
                            );
                            set_view_mode(&state, next);
                            refresh_view_windows(&state, Some(&ui));
                            restore_zoom_anchor(&ui, anchor);
                            request_visible_file_images(
                                &ui,
                                &shared_state,
                                window_id,
                                &senders.thumbnail,
                            );
                        }
                    }
                    return EventResult::PreventDefault;
                }
                ctrl_wheel_accumulator.set(0.0);
                let delta = logical_scroll_delta(delta, view_mode, ui.window().scale_factor());
                apply_file_scroll_delta(&ui, delta);
                request_visible_file_images(&ui, &shared_state, window_id, &senders.thumbnail);
                EventResult::PreventDefault
            }
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let modifiers = modifiers.get();
                let control = modifiers.control_key();
                let alt = modifiers.alt_key();
                let shift = modifiers.shift_key() || platform::windows::shift_key_down();
                let super_key = modifiers.super_key();
                let editing_address = ui.get_address_editing();
                let settings_active = ui.get_active_is_settings();
                let type_select_active = type_select.borrow().is_active();
                let character = match &event.logical_key {
                    Key::Character(value) => Some(value.as_str()),
                    _ => None,
                };
                if let Some(permanent) = delete_shortcut_request(
                    matches!(&event.logical_key, Key::Named(NamedKey::Delete)),
                    control,
                    alt,
                    settings_active,
                    editing_address,
                    shift,
                ) {
                    ui.invoke_request_delete(permanent);
                    return EventResult::PreventDefault;
                }
                if editing_address || settings_active {
                    type_select.borrow_mut().clear();
                }
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
                                app.pending_right_drops.remove(&window_id);
                            }
                        } else if !type_select_active {
                            ui.invoke_clear_selection();
                        }
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && !settings_active
                        && !editing_address
                        && ui.get_file_list_keyboard_target()
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("z")) =>
                    {
                        let origin_tab = state
                            .lock()
                            .ok()
                            .map(|app| app.active_window_state().active_tab);
                        if let Some(origin_tab) = origin_tab {
                            request_undo(&shared_state, &senders.operation, origin_tab);
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
                    _ if file_list_type_select_allowed(
                        ui.get_file_list_keyboard_target(),
                        ui.get_rectangle_selection_pointer_active(),
                        [control, alt, shift, super_key],
                        ime_composing.get(),
                        event.text.as_deref(),
                    ) =>
                    {
                        let typed = event.text.as_ref().and_then(|value| value.chars().next());
                        if let Some((tab_id, input)) =
                            typed.and_then(|typed| begin_file_list_quick_search(&state, typed))
                        {
                            type_select.borrow_mut().clear();
                            refresh_regions(&ui, &state, RefreshRegions::NAVIGATION);
                            ui.invoke_focus_address_editor();
                            schedule_search(
                                senders.everything.clone(),
                                state.clone(),
                                weak.clone(),
                                tab_id,
                                input,
                            );
                            true
                        } else {
                            let target = typed.and_then(|typed| {
                                let app = state.shared.lock().ok()?;
                                let window = app.window(window_id)?;
                                let tab = window.tabs.get(&window.active_tab)?;
                                (tab.kind == TabKind::Files).then_some(())?;
                                let context = type_select_context(&app, tab);
                                let projection = type_select_projection(&app, tab);
                                let focused = tab.focused;
                                drop(app);
                                type_select.borrow_mut().select(
                                    context,
                                    Instant::now(),
                                    typed,
                                    &projection,
                                    focused,
                                )
                            });
                            if let Some(entry_id) = target {
                                let update = mutate_window_selection(&state, window_id, |tab| {
                                    tab.select_entry(entry_id, false, false);
                                });
                                if let Some((tab_id, changed)) = update {
                                    update_file_rows(&ui, &state, tab_id, &changed);
                                    update_selection_summary(&ui, &state);
                                    let request_id = state.lock().ok().and_then(|app| {
                                        app.tab(tab_id).map(|tab| tab.latest_request())
                                    });
                                    if let Some(request_id) = request_id {
                                        reveal_entry(
                                            &ui,
                                            &state.shared,
                                            tab_id,
                                            request_id,
                                            entry_id,
                                        );
                                        request_visible_file_images(
                                            &ui,
                                            &state.shared,
                                            window_id,
                                            &senders.thumbnail,
                                        );
                                    }
                                }
                            }
                            true
                        }
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
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("n")) =>
                    {
                        create_new_window(&ui, window_id, &senders, &confirmations, &state.shared);
                        true
                    }
                    _ if control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("w")) =>
                    {
                        match ctrl_w_action(ui.get_can_close_tab()) {
                            CtrlWAction::CloseActiveTab => ui.invoke_close_tab(-1),
                            CtrlWAction::RequestWindowClose => ui.invoke_request_close(),
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

fn should_start_native_tab_drag(became_dragging: bool, native_started: bool) -> bool {
    became_dragging && !native_started
}
fn should_refresh_outbound_drag_source(
    result: platform::windows::drag_drop::OutboundDropResult,
) -> bool {
    result.dropped && result.effect == platform::windows::drag_drop::DropEffect::Move
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
    let (validated, reason) = platform::windows::drag_drop::negotiate_target_effect(
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
    origin_tab: TabId,
    state: SharedSessions,
    operation_sender: mpsc::Sender<FileOperationRequest>,
) {
    crate::operation_audit::record(
        "drop_dispatch",
        format!("tab={origin_tab:?} intent={intent:?}"),
    );
    let staging_root = intent.staging_root.clone();
    thread::spawn(move || match prepare_drop_operation(intent) {
        Ok(PreparedDrop::Operation(kind, items)) => {
            let _ = slint::invoke_from_event_loop(move || {
                let _ = enqueue_operation(
                    &state,
                    &operation_sender,
                    origin_tab,
                    kind,
                    items,
                    staging_root,
                );
            });
        }
        Ok(PreparedDrop::Shortcuts(shortcuts)) => {
            platform::windows::drag_drop::discard_drop_staging(staging_root);
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
            platform::windows::drag_drop::discard_drop_staging(staging_root);
            let _ = slint::invoke_from_event_loop(move || {
                if let Ok(mut app) = state.lock() {
                    app.operation_errors.push(error);
                }
            });
        }
    });
}

fn submit_quick_access_change(state: SharedSessions, path: PathBuf, pin: bool) {
    let pending_path = path.clone();
    {
        let Ok(mut app) = state.lock() else { return };
        if !app.quick_access_pending.insert(pending_path.clone()) {
            return;
        }
        app.quick_access_generation = app.quick_access_generation.wrapping_add(1).max(1);
    }
    thread::spawn(move || {
        let result = if pin {
            platform::windows::quick_access::pin(&path)
        } else {
            platform::windows::quick_access::unpin(&path)
        };
        let refreshed = platform::explorer_pinned_locations().unwrap_or_default();
        let state_for_ui = state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut app) = state_for_ui.lock() {
                app.quick_access_pending.remove(&pending_path);
                app.sidebar
                    .retain(|location| location.kind == KnownLocationKind::Drive);
                app.sidebar.splice(0..0, refreshed);
                if let Err(ref error) = result {
                    app.operation_errors
                        .push(format!("quick access operation failed: {error}"));
                }
                eprintln!(
                    "{{\"event\":\"quick_access_operation_finished\",\"pin\":{pin},\"path\":{:?},\"result\":{:?}}}",
                    path, result
                );
            }
            refresh_all_windows(
                &state_for_ui,
                RefreshRegions::SIDEBAR | RefreshRegions::HOME,
            );
        });
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
                                    .find(|entry| entry.path.matches(&row.path))
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
            .and_then(|mut app| app.pending_right_drops.remove(&state_for_choice.window_id));
        let Some((origin_tab, intent)) = pending else {
            return;
        };
        let staging_root = intent.staging_root.clone();
        match selected_right_drop(intent, choice) {
            Ok(Some(intent)) => dispatch_drop_operation(
                intent,
                origin_tab,
                state_for_choice.shared.clone(),
                operation_for_choice.clone(),
            ),
            Ok(None) => platform::windows::drag_drop::discard_drop_staging(staging_root),
            Err(error) => {
                platform::windows::drag_drop::discard_drop_staging(staging_root);
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
            if matches!(
                intent.target,
                platform::windows::drag_drop::DropTarget::QuickAccessPin
            ) {
                if intent.paths.len() == 1 {
                    submit_quick_access_change(state.shared.clone(), intent.paths[0].clone(), true);
                } else if let Ok(mut app) = state.lock() {
                    let language = app.language;
                    app.operation_errors.push(
                        match language {
                            Language::Chinese => "这里只能固定一个文件夹",
                            Language::English => "Only one folder can be pinned here",
                        }
                        .to_owned(),
                    );
                }
            } else if drop_requires_choice(&intent) {
                let state_for_ui = state.clone();
                let weak_for_ui = weak_for_intents.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let staging_root = intent.staging_root.clone();
                    let Some(ui) = weak_for_ui.upgrade() else {
                        platform::windows::drag_drop::discard_drop_staging(staging_root);
                        return;
                    };
                    let Ok((client_left, client_top, _, _)) =
                        platform::windows::drag_drop::client_screen_rect(native_window_handle(&ui))
                    else {
                        platform::windows::drag_drop::discard_drop_staging(staging_root);
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
                    let pending_saved = state_for_ui.lock().is_ok_and(|mut app| {
                        let Some(origin_tab) = app
                            .window(state_for_ui.window_id)
                            .map(|window| window.active_tab)
                        else {
                            return false;
                        };
                        if let Some((_, previous)) = app
                            .pending_right_drops
                            .insert(state_for_ui.window_id, (origin_tab, intent))
                        {
                            platform::windows::drag_drop::discard_drop_staging(
                                previous.staging_root,
                            );
                        }
                        true
                    });
                    if !pending_saved {
                        platform::windows::drag_drop::discard_drop_staging(staging_root);
                        return;
                    }
                    eprintln!("drag-drop: showing right-drop menu x={x} y={y}");
                    ui.invoke_show_drop_menu(x, y);
                });
            } else {
                let staging_root = intent.staging_root.clone();
                let origin_tab = state
                    .lock()
                    .ok()
                    .and_then(|app| app.window(state.window_id).map(|window| window.active_tab));
                if let Some(origin_tab) = origin_tab {
                    dispatch_drop_operation(
                        intent,
                        origin_tab,
                        state.shared.clone(),
                        operation_sender.clone(),
                    );
                } else {
                    platform::windows::drag_drop::discard_drop_staging(staging_root);
                }
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
    let Ok((_, client_top, _, _)) =
        platform::windows::drag_drop::client_screen_rect(native_window_handle(ui))
    else {
        return;
    };
    let scale = ui.window().scale_factor();
    let list_top = client_top + (ui.get_file_list_top() * scale).round() as i32;
    let bottom = client_top
        + ((ui.get_file_list_top() + ui.get_file_viewport_height()) * scale).round() as i32;
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
    let view_mode = projected_view_mode(ui);
    let maximum = projected_scroll_maximum(ui, view_mode, ui.get_file_viewport_height());
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
    let (left, top, _right, bottom) =
        platform::windows::drag_drop::client_screen_rect(hwnd).ok()?;
    let scale = ui.window().scale_factor();
    let list_top = (ui.get_file_list_top() * scale).round() as i32;
    let list_left = (ui.get_file_list_left() * scale).round() as i32;
    let viewport = (-ui.get_file_viewport_y() * scale).max(0.0);
    let view_mode = projected_view_mode(ui);
    let row_height = file_row_height(view_mode) * scale;
    let (target_left, target_right) = if view_mode == ViewMode::Details {
        let geometry = FileHitGeometry {
            viewport_x: ui.get_file_viewport_x(),
            viewport_width: ui.get_file_viewport_width(),
            columns_width: ui.get_details_hit_width(),
            ..FileHitGeometry::default()
        };
        let (target_left, target_right) = geometry.details_range();
        (
            left + list_left + (target_left * scale).round() as i32,
            left + list_left + (target_right * scale).round() as i32,
        )
    } else {
        (
            left + list_left,
            left + list_left + (ui.get_file_viewport_width() * scale).round() as i32,
        )
    };
    let groups = directory_group_projections(app, tab, tab.visible_entries());
    let list_projection = (!view_mode.uses_grid_layout()).then(|| {
        ListProjection::from_groups(
            &groups,
            group_header_height(&groups),
            file_row_height(view_mode) as u64,
        )
    });
    let icon_projection = view_mode.uses_grid_layout().then(|| {
        IconProjection::from_groups(
            &groups,
            ui.get_grid_column_count().max(1) as usize,
            group_header_height(&groups),
            file_row_height(view_mode) as u64,
        )
    });
    let folder_rows = tab
        .visible_entries()
        .iter()
        .filter(|entry| entry.kind == crate::domain::EntryKind::Directory)
        .filter_map(|entry| {
            let (visual_row, column) = if let Some(projection) = icon_projection.as_ref() {
                let position = projection.entry_position(entry.id)?;
                (position.row_index, position.column_index)
            } else {
                (list_projection.as_ref()?.entry_position(entry.id)?, 0)
            };
            let row_start = if let Some(projection) = icon_projection.as_ref() {
                projection.offsets.row_start(visual_row)? as f32
            } else {
                list_projection.as_ref()?.offsets.row_start(visual_row)? as f32
            };
            let row_top = top + list_top + (row_start * scale - viewport).round() as i32;
            let row_bottom = row_top + row_height.round() as i32;
            let card = file_layout_geometry(view_mode);
            let item_left = if card.grid {
                left + list_left
                    + (16.0 * scale).round() as i32
                    + (column as f32 * (card.card_width + 8.0) * scale).round() as i32
            } else {
                target_left
            };
            let item_right = if card.grid {
                item_left + (card.card_width * scale).round() as i32
            } else {
                target_right
            };
            (item_right > item_left && row_bottom > top + list_top && row_top < bottom).then(|| {
                platform::windows::drag_drop::FolderDropTarget {
                    left: item_left,
                    top: row_top,
                    right: item_right,
                    bottom: row_bottom.min(bottom),
                    path: entry.path(),
                }
            })
        })
        .collect();
    Some(platform::windows::drag_drop::DropTargetSnapshot {
        current: Some(current),
        folder_rows,
        quick_access_pin: {
            let left_px = left + (ui.get_quick_access_drop_left() * scale).round() as i32;
            let top_px = top + (ui.get_quick_access_drop_top() * scale).round() as i32;
            let width_px = (ui.get_quick_access_drop_width() * scale).round() as i32;
            let height_px = (ui.get_quick_access_drop_height() * scale).round() as i32;
            (width_px > 0 && height_px > 0).then_some(
                platform::windows::drag_drop::DropTargetRect {
                    left: left_px,
                    top: top_px,
                    right: left_px + width_px,
                    bottom: top_px + height_px,
                },
            )
        },
    })
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
    let platform::windows::drag_drop::DropTarget::Directory(target) = intent.target else {
        return Err("快速访问固定不能进入文件任务".to_owned());
    };
    let target = platform::windows::network::network_drive_to_unc(&target).unwrap_or(target);
    let target_metadata =
        std::fs::metadata(&target).map_err(|error| format!("拖放目标不可用：{error}"))?;
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
        let source = platform::windows::network::network_drive_to_unc(&source).unwrap_or(source);
        let metadata = std::fs::symlink_metadata(&source)
            .map_err(|error| format!("拖放来源不可用：{error}"))?;
        let _ = metadata.file_type();
        let name = source
            .file_name()
            .ok_or_else(|| "拖放来源没有可用名称".to_owned())?;
        let destination = target.join(name);
        if kind.is_none() && target.starts_with(&source) {
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
                platform::windows::shortcut::shortcut_destination(&target, &source)
                    .map_err(|error| error.to_string())?;
            if reserved.contains(&destination) {
                let stem = source
                    .file_stem()
                    .or_else(|| source.file_name())
                    .ok_or_else(|| "拖放来源没有可用名称".to_owned())?;
                for index in 2_u64.. {
                    let mut name = stem.to_os_string();
                    name.push(format!(" ({index})"));
                    let mut candidate = target.join(name);
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

fn wire_window_trace(hwnd: isize) {
    let path = if let Some(path) = platform::windows::window_trace::requested_path() {
        path
    } else if cfg!(debug_assertions) {
        platform::windows::window_trace::default_path()
    } else {
        return;
    };
    if let Err(error) = platform::windows::window_trace::install(hwnd, &path) {
        eprintln!(
            "failed to install window trace at {}: {error}",
            path.display()
        );
    }
}
fn wire_window_controls(ui: &AppWindow) {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    #[cfg(windows)]
    ui.window().with_winit_window(|window| {
        window.set_corner_preference(CornerPreference::Round);
        window.set_undecorated_shadow(true);
    });

    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        platform::windows::window_trace::log_request(hwnd, "move-request");
        begin_component_window_drag(&ui);
    });

    let weak = ui.as_weak();
    ui.on_drag_window_after_menu_dismiss(move || {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let hwnd = native_window_handle(&ui);
        platform::windows::window_trace::log_request(hwnd, "move-request-after-menu-dismiss");
        begin_component_window_drag(&ui);
    });
}

/// 二级弹窗与确认窗口保持同一套窗口行为：不可缩放、保持前置。
fn configure_secondary_dialog_window<T: slint::ComponentHandle>(ui: &T) {
    #[cfg(windows)]
    use winit::platform::windows::{CornerPreference, WindowExtWindows};

    ui.window().with_winit_window(|window| {
        window.set_resizable(false);
        window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
        #[cfg(windows)]
        {
            window.set_corner_preference(CornerPreference::Round);
            window.set_undecorated_shadow(true);
        }
    });
    wire_secondary_window_adaptation(ui);
}

fn configure_confirmation_window(ui: &ConfirmationWindow) {
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
            begin_component_window_drag(&ui);
        }
    });
    configure_secondary_dialog_window(ui);
}

fn wire_confirmation_windows(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    delete_ui: &ConfirmationWindow,
    conflict_ui: &ConfirmationWindow,
    exit_ui: &ConfirmationWindow,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: WindowSessions,
) {
    for confirmation in [delete_ui, conflict_ui, exit_ui] {
        configure_confirmation_window(confirmation);
    }

    let delete_weak = delete_ui.as_weak();
    let state_for_delete = state.clone();
    delete_ui.on_safe_cancel(move || {
        let demo_mode = delete_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if !demo_mode && let Ok(mut app) = state_for_delete.lock() {
            app.pending_permanent_delete = None;
            app.pending_imported_network_location_remove = None;
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
        let (pending, imported_remove) = state_for_delete
            .lock()
            .map(|mut app| {
                (
                    app.pending_permanent_delete.take(),
                    app.pending_imported_network_location_remove.take(),
                )
            })
            .unwrap_or_default();
        if let Some(ui) = delete_weak.upgrade() {
            let _ = ui.hide();
        }
        if let Some(location) = imported_remove {
            let state = state_for_delete.shared.clone();
            thread::spawn(move || {
                if let Err(error) = mutate_imported_network_location(&location, None) {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Ok(mut app) = state.lock() {
                            app.operation_errors.push(format!(
                                "Failed to remove Explorer network location: {error}"
                            ));
                        }
                    });
                }
            });
            return;
        }
        if let Some((origin_tab, items)) = pending {
            submit_delete_items(
                &state_for_delete,
                &sender_for_delete,
                origin_tab,
                true,
                items,
            );
        }
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
        refresh_all_windows(&state_for_conflict, RefreshRegions::STATUS);
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
            refresh_all_windows(&state_for_conflict, RefreshRegions::STATUS);
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
                    position_window_centered(&ui, None, &window);
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
        position_window_centered(ui, operation_ui, confirmation_ui);
        confirmation_ui
            .window()
            .with_winit_window(|window| window.focus_window());
        return;
    }
    position_window_centered(ui, operation_ui, confirmation_ui);
    if let Err(error) = confirmation_ui.show() {
        platform::windows::window_trace::log_diagnostic(
            "confirmation-window-show-failed",
            &error.to_string(),
        );
        eprintln!("failed to show confirmation window: {error}");
        return;
    }
    confirmation_ui
        .window()
        .with_winit_window(|window| window.focus_window());
}

/// 二级弹窗的显示路径：先按发起窗口定位，再显示并聚焦；已可见时只重新定位并聚焦。
fn show_centered_dialog<T: slint::ComponentHandle>(main: &AppWindow, ui: &T) {
    position_window_centered(main, None, ui);
    if !ui.window().is_visible() {
        let _ = ui.show();
    }
    ui.window()
        .with_winit_window(|window| window.focus_window());
}

fn explorer_network_location_warning(language: Language) -> &'static str {
    match language {
        Language::Chinese => "此更改会同步影响 Explorer。",
        Language::English => "This change also affects Explorer.",
    }
}
fn mutate_imported_network_location(
    location: &NetworkLocation,
    new_name: Option<&str>,
) -> io::Result<()> {
    if location.source != NetworkLocationSource::WindowsImported {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network location is not imported from Explorer",
        ));
    }
    let shell_path = location.shell_path.as_deref().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Explorer network location has no filesystem identity",
        )
    })?;
    match new_name {
        Some(name) => platform::windows::network::rename_network_location(
            shell_path,
            std::ffi::OsStr::new(name.trim()),
        )
        .map(|_| ()),
        None => platform::windows::network::remove_network_location(shell_path),
    }
}
fn configure_network_location_rename_window(
    ui: &NetworkLocationRenameWindow,
    id: u64,
    name: &str,
    language: Language,
    dark_theme: bool,
    source: NetworkLocationSource,
) {
    ui.set_location_id(id.to_string().into());
    ui.set_name(name.into());
    ui.set_dark_theme(dark_theme);
    match language {
        Language::Chinese => {
            ui.set_title_text("重命名网络位置".into());
            ui.set_detail_text(
                if source == NetworkLocationSource::WindowsImported {
                    explorer_network_location_warning(Language::Chinese)
                } else {
                    "输入新的显示名称。"
                }
                .into(),
            );
            ui.set_cancel_text("取消".into());
            ui.set_save_text("保存".into());
            ui.set_close_text("关闭".into());
        }
        Language::English => {
            ui.set_title_text("Rename network location".into());
            ui.set_detail_text(
                if source == NetworkLocationSource::WindowsImported {
                    explorer_network_location_warning(Language::English)
                } else {
                    "Enter a new display name."
                }
                .into(),
            );
            ui.set_cancel_text("Cancel".into());
            ui.set_save_text("Save".into());
            ui.set_close_text("Close".into());
        }
    }
}

fn wire_network_location_rename_window(ui: &NetworkLocationRenameWindow, state: SharedSessions) {
    configure_secondary_dialog_window(ui);
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
    ui.on_safe_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_location_id("".into());
            let _ = ui.hide();
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            begin_component_window_drag(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_save(move |name| {
        let id = weak
            .upgrade()
            .and_then(|ui| ui.get_location_id().as_str().parse::<u64>().ok());
        let Some(id) = id else { return };
        let imported = state.lock().ok().and_then(|app| {
            app.imported_network_locations
                .iter()
                .find(|location| location.id == id)
                .cloned()
        });
        if let Some(location) = imported {
            let state_for_result = state.clone();
            let name = name.to_string();
            thread::spawn(move || {
                if let Err(error) = mutate_imported_network_location(&location, Some(&name)) {
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Ok(mut app) = state_for_result.lock() {
                            app.operation_errors.push(format!(
                                "Failed to rename Explorer network location: {error}"
                            ));
                        }
                    });
                }
            });
        } else if let Ok(mut app) = state.lock() {
            let mut catalog = NetworkLocationCatalog::new(app.network_locations.clone());
            match catalog.rename(id, name.as_str()) {
                Ok(()) => {
                    app.network_locations = catalog.locations().to_vec();
                    app.network_location_generation =
                        app.network_location_generation.wrapping_add(1).max(1);
                }
                Err(error) => app
                    .operation_errors
                    .push(format!("Failed to rename network location: {error:?}")),
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_location_id("".into());
            let _ = ui.hide();
        }
        refresh_all_windows(&state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
    });
}

fn configure_network_login_window(ui: &NetworkLoginWindow, state: &SharedSessions, target: &Path) {
    let language = state
        .lock()
        .map(|app| app.language)
        .unwrap_or(Language::Chinese);
    ui.set_target_text(display_path(target).into());
    match language {
        Language::Chinese => {
            ui.set_title_text("登录网络位置".into());
            ui.set_detail_text("输入此网络位置使用的 Windows 凭据。".into());
            ui.set_user_label("用户名".into());
            ui.set_password_label("密码".into());
            ui.set_remember_label("记住凭据（由 Windows 管理）".into());
            ui.set_conflict_text(
                "Windows 已使用另一账号连接此服务器。断开会影响 Explorer 和其他软件。".into(),
            );
            ui.set_cancel_text("取消".into());
            ui.set_login_text("登录".into());
            ui.set_reconnect_text("断开并重新登录".into());
            ui.set_close_text("关闭".into());
        }
        Language::English => {
            ui.set_title_text("Sign in to network location".into());
            ui.set_detail_text("Enter the Windows credentials for this network location.".into());
            ui.set_user_label("Username".into());
            ui.set_password_label("Password".into());
            ui.set_remember_label("Remember credentials (managed by Windows)".into());
            ui.set_conflict_text(
                "Windows is already connected to this server with another account. Disconnecting can affect Explorer and other apps."
                    .into(),
            );
            ui.set_cancel_text("Cancel".into());
            ui.set_login_text("Sign in".into());
            ui.set_reconnect_text("Disconnect and sign in".into());
            ui.set_close_text("Close".into());
        }
    }
}

fn network_auth_error_text(
    error: platform::windows::network::NetworkAuthError,
    language: Language,
) -> String {
    use platform::windows::network::NetworkAuthErrorKind;

    let message = match (language, error.kind) {
        (Language::Chinese, NetworkAuthErrorKind::AccessDenied) => "Windows 拒绝了访问",
        (Language::Chinese, NetworkAuthErrorKind::LogonFailure) => "用户名或密码不正确",
        (Language::Chinese, NetworkAuthErrorKind::CredentialConflict) => {
            "Windows 已使用另一账号连接此服务器"
        }
        (Language::Chinese, NetworkAuthErrorKind::BadPath) => "网络路径不存在",
        (Language::Chinese, NetworkAuthErrorKind::Unavailable) => "网络位置暂时不可用",
        (Language::Chinese, NetworkAuthErrorKind::Other) => "无法登录网络位置",
        (Language::English, NetworkAuthErrorKind::AccessDenied) => "Windows denied access",
        (Language::English, NetworkAuthErrorKind::LogonFailure) => {
            "The username or password is incorrect"
        }
        (Language::English, NetworkAuthErrorKind::CredentialConflict) => {
            "Windows is connected to this server with another account"
        }
        (Language::English, NetworkAuthErrorKind::BadPath) => "The network path does not exist",
        (Language::English, NetworkAuthErrorKind::Unavailable) => {
            "The network location is unavailable"
        }
        (Language::English, NetworkAuthErrorKind::Other) => {
            "Unable to sign in to the network location"
        }
    };
    format!("{message}（Windows {}）", error.code)
}

fn wire_network_login_window(
    owner: &AppWindow,
    login: &NetworkLoginWindow,
    local_sender: mpsc::Sender<DirectoryRequest>,
    network_sender: mpsc::SyncSender<DirectoryRequest>,
    state: SharedSessions,
    login_state: Arc<Mutex<NetworkLoginCoordinator>>,
) {
    configure_secondary_dialog_window(login);
    let login_weak = login.as_weak();
    let login_state_for_cancel = login_state.clone();
    login.on_safe_cancel(move || {
        if let Ok(mut coordinator) = login_state_for_cancel.lock() {
            coordinator.cancel();
        }
        if let Some(login) = login_weak.upgrade() {
            login.set_password("".into());
            login.set_conflict(false);
            login.set_busy(false);
            let _ = login.hide();
        }
    });
    let owner_weak = owner.as_weak();
    let login_weak = login.as_weak();
    login.on_drag_window(move || {
        if let Some(login) = login_weak.upgrade() {
            begin_component_window_drag(&login);
        }
    });
    let login_weak = login.as_weak();
    login.on_login(move |username, password, remember, disconnect_first| {
        let session = login_state
            .lock()
            .ok()
            .and_then(|coordinator| coordinator.current.clone());
        let Some(session) = session else {
            return;
        };
        if let Some(login) = login_weak.upgrade() {
            login.set_busy(true);
        }
        let state = state.clone();
        let local_sender = local_sender.clone();
        let network_sender = network_sender.clone();
        let login_weak = login_weak.clone();
        let owner_weak = owner_weak.clone();
        let login_state = login_state.clone();
        let username = username.to_string();
        let mut password = password.to_string();
        let auth_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let auth_done_for_monitor = auth_done.clone();
        let state_for_monitor = state.clone();
        let login_state_for_monitor = login_state.clone();
        let session_for_monitor = session.clone();
        thread::spawn(move || {
            while !auth_done_for_monitor.load(std::sync::atomic::Ordering::Acquire) {
                let valid = state_for_monitor.lock().ok().is_some_and(|app| {
                    app.window_for_tab(session_for_monitor.tab_id)
                        == Some(session_for_monitor.window_id)
                        && app.tab(session_for_monitor.tab_id).is_some_and(|tab| {
                            tab.latest_request() == session_for_monitor.failed_request_id
                                && tab.requested_location().as_ref().and_then(NavigationLocation::directory_path)
                                    == Some(session_for_monitor.target.as_path())
                        })
                });
                if !valid {
                    if let Ok(mut coordinator) = login_state_for_monitor.lock() {
                        coordinator.cancel_generation(session_for_monitor.generation);
                    }
                    return;
                }
                thread::sleep(Duration::from_millis(50));
            }
        });
        thread::spawn(move || {
            let result = (|| {
                if disconnect_first {
                    platform::windows::network::isolated_force_disconnect_network_share(
                        &session.target,
                        &session.cancel,
                    )?;
                }
                platform::windows::network::isolated_connect_network_share(
                    &session.target,
                    Some(&username),
                    Some(&password),
                    remember,
                    &session.cancel,
                )
            })();
            auth_done.store(true, std::sync::atomic::Ordering::Release);
            unsafe { password.as_bytes_mut() }.fill(0);
            let _ = slint::invoke_from_event_loop(move || {
                let still_current = login_state
                    .lock()
                    .ok()
                    .is_some_and(|coordinator| coordinator.is_current(session.generation))
                    && state.lock().ok().is_some_and(|app| {
                        app.window_for_tab(session.tab_id) == Some(session.window_id)
                            && app.tab(session.tab_id).is_some_and(|tab| {
                                tab.latest_request() == session.failed_request_id
                                    && tab.requested_location().as_ref().and_then(NavigationLocation::directory_path)
                                        == Some(session.target.as_path())
                            })
                    });
                if !still_current {
                    let superseded = login_state.lock().ok().is_some_and(|coordinator| {
                        coordinator
                            .current
                            .as_ref()
                            .is_some_and(|current| current.generation != session.generation)
                    });
                    if !superseded && let Some(login) = login_weak.upgrade() {
                        login.set_password("".into());
                        login.set_busy(false);
                        let _ = login.hide();
                    }
                    return;
                }
                match result {
                    Ok(_) => {
                        if let Ok(mut coordinator) = login_state.lock() {
                            coordinator.finish(session.generation);
                        }
                        if let Some(login) = login_weak.upgrade() {
                            login.set_password("".into());
                            login.set_busy(false);
                            login.set_conflict(false);
                            let _ = login.hide();
                        }
                        submit_path_navigation(
                            &local_sender,
                            &network_sender,
                            &state,
                            session.tab_id,
                            session.target,
                            NavigationKind::Refresh,
                        );
                        if let Some(target_ui) =
                            window_ui(session.window_id).or_else(|| owner_weak.upgrade())
                        {
                            refresh_ui(
                                &target_ui,
                                &WindowSessions::new(state.clone(), session.window_id),
                            );
                        }
                    }
                    Err(error)
                        if error.kind
                            == platform::windows::network::NetworkAuthErrorKind::CredentialConflict =>
                    {
                        if let Some(login) = login_weak.upgrade() {
                            login.set_busy(false);
                            login.set_conflict(true);
                        }
                    }
                    Err(error) => {
                        if let Some(login) = login_weak.upgrade() {
                            login.set_busy(false);
                            let language = state
                                .lock()
                                .map(|app| app.language)
                                .unwrap_or(Language::Chinese);
                            login.set_detail_text(network_auth_error_text(error, language).into());
                        }
                    }
                }
            });
        });
    });
}

/// 物理屏幕上的一个窗口矩形，只用于计算二级窗口落点。
#[derive(Clone, Copy)]
struct WindowRect {
    position: (i32, i32),
    size: (u32, u32),
}

/// 二级窗口的锚点：发起它的窗口及其所在显示器。
#[derive(Clone, Copy)]
struct SecondaryWindowAnchor {
    window: WindowRect,
    monitor: WindowRect,
}

/// 居中于锚点窗口，并夹取在锚点所在显示器内。
/// 独立成纯函数，便于覆盖多屏、负坐标和越界情形。
fn centered_window_position(
    anchor: WindowRect,
    target_size: (u32, u32),
    monitor: WindowRect,
) -> (i32, i32) {
    let (target_width, target_height) = (target_size.0 as i32, target_size.1 as i32);
    let centered_x = anchor.position.0 + (anchor.size.0 as i32 - target_width) / 2;
    let centered_y = anchor.position.1 + (anchor.size.1 as i32 - target_height) / 2;
    // 弹窗比显示器还大时退回显示器原点，保证结果始终落在可见区域内。
    let max_x = (monitor.position.0 + monitor.size.0 as i32 - target_width).max(monitor.position.0);
    let max_y =
        (monitor.position.1 + monitor.size.1 as i32 - target_height).max(monitor.position.1);
    (
        centered_x.clamp(monitor.position.0, max_x),
        centered_y.clamp(monitor.position.1, max_y),
    )
}

/// 冲突确认在文件操作窗口可见时以它为准，其余二级窗口以发起它的主窗口为准。
fn secondary_window_anchor(
    ui: &AppWindow,
    operation_ui: Option<&OperationWindow>,
) -> Option<SecondaryWindowAnchor> {
    let mut anchor = None;
    let mut calculate = |source: &winit::window::Window| {
        let Some(monitor) = source.current_monitor() else {
            return;
        };
        let monitor_position = monitor.position();
        let monitor_size = monitor.size();
        let source_position = source.outer_position().unwrap_or(monitor_position);
        let source_size = source.outer_size();
        anchor = Some(SecondaryWindowAnchor {
            window: WindowRect {
                position: (source_position.x, source_position.y),
                size: (source_size.width, source_size.height),
            },
            monitor: WindowRect {
                position: (monitor_position.x, monitor_position.y),
                size: (monitor_size.width, monitor_size.height),
            },
        });
    };
    if let Some(operation_ui) = operation_ui.filter(|window| window.window().is_visible()) {
        operation_ui.window().with_winit_window(&mut calculate);
    } else {
        ui.window().with_winit_window(calculate);
    }
    anchor
}

/// 所有二级窗口共用的落点：居中于发起窗口，并夹取在该窗口所在显示器内。
fn position_window_centered<T: slint::ComponentHandle>(
    ui: &AppWindow,
    operation_ui: Option<&OperationWindow>,
    target_ui: &T,
) {
    let Some(anchor) = secondary_window_anchor(ui, operation_ui) else {
        return;
    };
    let target_size = target_ui.window().size();
    let (x, y) = centered_window_position(
        anchor.window,
        (target_size.width, target_size.height),
        anchor.monitor,
    );
    target_ui
        .window()
        .set_position(slint::PhysicalPosition::new(x, y));
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
    operation_ui.window().on_close_requested(move || {
        platform::windows::window_trace::log_diagnostic(
            "operation-window-hide-requested",
            "source=window-close",
        );
        if operation_weak.upgrade().is_none() {
            platform::windows::window_trace::log_diagnostic(
                "operation-window-hide-failed",
                "source=window-close,error=window-unavailable",
            );
        }
        slint::CloseRequestResponse::HideWindow
    });
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_drag_window(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            begin_component_window_drag(&operation_ui);
        }
    });
    #[cfg(windows)]
    operation_ui.window().with_winit_window(|window| {
        window.set_corner_preference(CornerPreference::Round);
        window.set_undecorated_shadow(true);
        window.set_resizable(false);
    });
    wire_secondary_window_adaptation(operation_ui);

    let state_for_cancel = state.clone();
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_cancel_operation(move |id| {
        platform::windows::window_trace::log_diagnostic(
            "operation-window-cancel-requested",
            &format!("id={id}"),
        );
        if webdav_ui::cancel_transfer(id as u64) {
            if let Some(operation_ui) = operation_weak.upgrade() {
                refresh_operation_window(&operation_ui, &state_for_cancel);
            }
            return;
        }
        cancel_operations(&state_for_cancel, [OperationId(id as u64)]);
        if let Some(operation_ui) = operation_weak.upgrade() {
            refresh_operation_window(&operation_ui, &state_for_cancel);
        }
    });

    let state_for_pause = state.clone();
    let operation_weak = operation_ui.as_weak();
    operation_ui.on_set_operation_paused(move |id, paused| {
        platform::windows::window_trace::log_diagnostic(
            "operation-window-pause-requested",
            &format!("id={id},paused={paused}"),
        );
        if let Ok(mut app) = state_for_pause.lock() {
            let id = OperationId(id as u64);
            if let Err(error) = app.operations.set_paused(id, paused) {
                app.operation_errors.push(format!(
                    "unable to pause or resume operation {} from {:?}",
                    id.0, error.from
                ));
            }
        }
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

    wire_operation_window_opener(ui, operation_ui, state.clone());

    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    let state_for_auto_open = state.clone();
    let mut shown_notifications = HashSet::new();
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(250),
        move || {
            let Some(operation_ui) = operation_weak.upgrade() else {
                return;
            };
            webdav_ui::prune_finished_transfers();
            if webdav_ui::has_transfer() {
                refresh_operation_badges(&state_for_auto_open);
                refresh_operation_window(&operation_ui, &state_for_auto_open);
                if webdav_ui::has_active_transfer() && !operation_ui.window().is_visible() {
                    if let Some(ui) = ui_weak.upgrade() {
                        position_window_centered(&ui, None, &operation_ui);
                    }
                    let _ = operation_ui.show();
                }
            }
            let removed = state_for_auto_open
                .lock()
                .map(|mut app| {
                    app.operations.prune_transient_with_retained_cleanup(
                        Duration::ZERO,
                        Duration::from_secs(10),
                    )
                })
                .unwrap_or_default();
            if removed > 0 {
                refresh_operation_badges(&state_for_auto_open);
                refresh_operation_window(&operation_ui, &state_for_auto_open);
                let should_close = state_for_auto_open
                    .lock()
                    .is_ok_and(|app| should_close_operation_window(&app));
                if should_close {
                    let _ = operation_ui.hide();
                }
            }
            if operation_ui.window().is_visible() {
                let rows = operation_ui.get_operations();
                let pause_changed = state_for_auto_open.lock().is_ok_and(|app| {
                    operation_pause_ack_changed(
                        &app,
                        rows.iter()
                            .map(|row| (OperationId(row.id as u64), row.pause_pending)),
                    )
                });
                if pause_changed {
                    refresh_operation_progress(&operation_ui, &state_for_auto_open);
                }
            }
            let pending_notifications = state_for_auto_open
                .lock()
                .map(|app| pending_operation_window_notifications(&app, &mut shown_notifications))
                .unwrap_or_default();
            if pending_notifications.is_empty() {
                return;
            }
            let already_visible = operation_ui.window().is_visible();
            refresh_operation_window(&operation_ui, &state_for_auto_open);
            if !already_visible {
                if let Some(ui) = ui_weak.upgrade() {
                    position_window_centered(&ui, None, &operation_ui);
                }
                if operation_ui.show().is_err() {
                    return;
                }
            }
            shown_notifications.extend(pending_notifications);
        },
    );
    timer
}

fn operation_pause_ack_changed(
    app: &AppState,
    mut displayed: impl Iterator<Item = (OperationId, bool)>,
) -> bool {
    displayed.any(|(id, pending)| {
        app.operations.task(id).is_some_and(|task| {
            task.state == OperationState::Paused
                && task.resource != OperationResource::Cleanup
                && pending == task.cancellation.is_pause_acknowledged()
        })
    })
}

fn wire_operation_window_opener(
    ui: &AppWindow,
    operation_ui: &OperationWindow,
    state: SharedSessions,
) {
    let operation_weak = operation_ui.as_weak();
    let ui_weak = ui.as_weak();
    ui.on_open_operation_window(move || {
        if let (Some(ui), Some(operation_ui)) = (ui_weak.upgrade(), operation_weak.upgrade()) {
            refresh_operation_window(&operation_ui, &state);
            position_window_centered(&ui, None, &operation_ui);
            let _ = operation_ui.show();
        }
    });
}

fn should_close_conflict_window(operation_id: &str, app: &AppState) -> bool {
    operation_id.parse::<u64>().ok().is_some_and(|id| {
        app.operations.task(OperationId(id)).is_none_or(|task| {
            task.state != OperationState::WaitingConflict || task.cancellation.is_cancelled()
        })
    })
}

fn should_close_operation_window(app: &AppState) -> bool {
    !app.operations.iter().any(operation_task_has_visible_row)
}

fn pending_operation_window_notifications(
    app: &AppState,
    shown: &mut HashSet<(OperationId, u32, bool)>,
) -> Vec<(OperationId, u32, bool)> {
    shown.retain(|(id, round, _)| {
        app.operations
            .task(*id)
            .is_some_and(|task| task.execution_round == *round)
    });
    operation_window_notifications(app)
        .into_iter()
        .filter(|notification| !shown.contains(notification))
        .collect()
}
fn operation_window_notifications(app: &AppState) -> HashSet<(OperationId, u32, bool)> {
    app.operations
        .iter()
        .filter_map(|task| {
            if task.kind == FileOperationKind::PermanentDelete {
                return None;
            }
            let failed = matches!(
                task.state,
                OperationState::Failed | OperationState::PartiallyCompleted
            );
            let slow = matches!(
                task.state,
                OperationState::Preflight | OperationState::Running | OperationState::Paused
            ) && task.cancellation.active_elapsed(task.started_at)
                >= Duration::from_millis(800);
            // A failure deserves a new notification even after its progress window was dismissed.
            (failed || slow).then_some((task.id, task.execution_round, failed))
        })
        .collect()
}

fn refresh_operation_window(ui: &OperationWindow, state: &SharedSessions) {
    let (rows, dark_theme, language) = {
        let Ok(app) = state.lock() else {
            return;
        };
        (operation_rows(&app), app.dark_theme(), app.language)
    };
    let window_height = operation_window_height(rows.len());
    update_operation_rows(ui, rows);
    ui.set_demo_mode(false);
    ui.set_demo_notice("".into());
    ui.window()
        .set_size(slint::LogicalSize::new(580.0, window_height));
    ui.set_dark_theme(dark_theme);
    let (title, cancel, retry, pause, resume, pausing, cancelling, empty, close) = match language {
        Language::Chinese => (
            "文件操作",
            "取消",
            "重试",
            "暂停",
            "继续",
            "正在暂停",
            "正在取消",
            "没有文件操作",
            "关闭",
        ),
        Language::English => (
            "File operations",
            "Cancel",
            "Retry",
            "Pause",
            "Resume",
            "Pausing",
            "Cancelling",
            "No file operations",
            "Close",
        ),
    };
    ui.set_text_file_operations(title.into());
    ui.set_text_cancel_operation(cancel.into());
    ui.set_text_retry_operation(retry.into());
    ui.set_text_pause_operation(pause.into());
    ui.set_text_resume_operation(resume.into());
    ui.set_text_pausing_operation(pausing.into());
    ui.set_text_cancelling_operation(cancelling.into());
    ui.set_text_no_operations(empty.into());

    ui.set_text_window_close(close.into());
}

fn refresh_operation_progress(ui: &OperationWindow, state: &SharedSessions) {
    let rows = state
        .lock()
        .map(|app| operation_rows(&app))
        .unwrap_or_default();
    update_operation_rows(ui, rows);
}

fn queue_operation_progress_refresh(
    operation_weak: slint::Weak<OperationWindow>,
    state: SharedSessions,
    queued: &Arc<AtomicBool>,
) {
    if queued.swap(true, Ordering::AcqRel) {
        return;
    }
    let queued_for_callback = queued.clone();
    if slint::invoke_from_event_loop(move || {
        if let Some(operation_ui) = operation_weak.upgrade() {
            refresh_operation_progress(&operation_ui, &state);
        }
        queued_for_callback.store(false, Ordering::Release);
    })
    .is_err()
    {
        queued.store(false, Ordering::Release);
    }
}

fn update_operation_rows(ui: &OperationWindow, rows: Vec<OperationRow>) {
    let current = ui.get_operations();
    let Some(model) = current.as_any().downcast_ref::<VecModel<OperationRow>>() else {
        ui.set_operations(ModelRc::new(VecModel::from(rows)));
        return;
    };

    let common = model.row_count().min(rows.len());
    for (index, row) in rows.iter().take(common).cloned().enumerate() {
        if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
    while model.row_count() > rows.len() {
        model.remove(model.row_count() - 1);
    }
    for row in rows.into_iter().skip(common) {
        model.push(row);
    }
}

fn refresh_debug_operation_window(ui: &OperationWindow, state: &SharedSessions) {
    refresh_operation_window(ui, state);
    let language = state
        .lock()
        .map(|app| app.language)
        .unwrap_or(Language::Chinese);
    ui.set_operations(ModelRc::new(VecModel::from(debug_operation_rows(language))));
    ui.set_demo_mode(true);
    ui.set_demo_notice(
        match language {
            Language::Chinese => {
                "陈列室示例：共 12 行，含真实窗口不会保留的完成/取消状态，请滚动查看"
            }
            Language::English => {
                "Showcase: 12 rows, including completed/cancelled states not retained in real use"
            }
        }
        .into(),
    );
    ui.window().set_size(slint::LogicalSize::new(580.0, 650.0));
}

fn operation_window_height(row_count: usize) -> f32 {
    (48.0 + 230.0 * row_count.clamp(1, 3) as f32).min(650.0)
}

fn debug_operation_rows(language: Language) -> Vec<OperationRow> {
    let states = [
        OperationState::Queued,
        OperationState::Preflight,
        OperationState::Running,
        OperationState::Paused,
        OperationState::WaitingConflict,
        OperationState::Cancelling,
        OperationState::Completed,
        OperationState::Cancelled,
        OperationState::PartiallyCompleted,
        OperationState::Failed,
    ];
    let mut rows = states
        .into_iter()
        .enumerate()
        .map(|(index, state)| debug_operation_row(language, state, index as i32))
        .collect::<Vec<_>>();
    let mut estimating = debug_operation_row(language, OperationState::Running, 10);
    estimating.status = estimating_text(language).into();
    estimating.speed = estimating_text(language).into();
    estimating.eta = estimating_text(language).into();
    estimating.progress = 0.03;
    estimating.percent = "3%".into();
    estimating.transferred = "32 MB / 1.0 GB".into();
    estimating.file_progress = "1 / 32".into();
    let mut pausing = debug_operation_row(language, OperationState::Paused, 11);
    pausing.status = match language {
        Language::Chinese => "正在暂停",
        Language::English => "Pausing",
    }
    .into();
    pausing.pause_pending = true;
    pausing.can_pause = false;
    rows.insert(3, estimating);
    rows.insert(5, pausing);
    rows
}

fn debug_operation_row(language: Language, state: OperationState, index: i32) -> OperationRow {
    let (title, source_line, destination_line, current_item_line) = match language {
        Language::Chinese => (
            "复制示例文件",
            r"来源：C:\Users\Example\Documents",
            r"目标：D:\Backup\Documents",
            "当前项：Example document with a long name.pdf",
        ),
        Language::English => (
            "Copy sample files",
            r"From: C:\Users\Example\Documents",
            r"To: D:\Backup\Documents",
            "Current item: Example document with a long name.pdf",
        ),
    };
    let status = operation_status_text(language, state);
    let queue = if state == OperationState::Queued {
        match language {
            Language::Chinese => "队列第 2 项",
            Language::English => "Queue position 2",
        }
    } else {
        ""
    };
    let failure_summary = if matches!(
        state,
        OperationState::PartiallyCompleted | OperationState::Failed
    ) {
        match language {
            Language::Chinese => "失败 1 项：目标文件正被占用",
            Language::English => "1 failed: destination file is in use",
        }
    } else {
        ""
    };
    let progress_known = !matches!(state, OperationState::Queued | OperationState::Preflight);
    let (progress, transferred, speed, eta, file_progress) = match state {
        OperationState::Queued => (0.0, "0 MB", "", "—", "0 / 32"),
        OperationState::Preflight => (0.0, "0 MB", status, status, status),
        OperationState::Running => (
            0.38,
            "384 MB / 1.0 GB",
            "48 MB/s",
            "预计剩余 14 秒",
            "12 / 32",
        ),
        OperationState::Paused => (0.38, "384 MB / 1.0 GB", status, "—", "12 / 32"),
        OperationState::WaitingConflict => (0.38, "384 MB / 1.0 GB", status, "—", "12 / 32"),
        OperationState::Cancelling => (0.38, "384 MB / 1.0 GB", "", "—", "12 / 32"),
        OperationState::Completed => (1.0, "1.0 GB / 1.0 GB", "", "—", "32 / 32"),
        OperationState::Cancelled => (0.38, "384 MB / 1.0 GB", "", "—", "12 / 32"),
        OperationState::PartiallyCompleted => (0.75, "768 MB / 1.0 GB", "", "—", "24 / 32"),
        OperationState::Failed => (0.25, "256 MB / 1.0 GB", "", "—", "8 / 32"),
    };
    let (speed, eta, file_progress) = if language == Language::English {
        (
            match state {
                OperationState::Preflight => "Preparing",
                OperationState::Running => "48 MB/s",
                OperationState::Paused => "Paused",
                OperationState::WaitingConflict => "Waiting for conflict decision",
                _ => speed,
            },
            match state {
                OperationState::Preflight => "Preparing",
                OperationState::Running => "About 14 seconds remaining",
                _ => eta,
            },
            if state == OperationState::Preflight {
                "Preparing"
            } else {
                file_progress
            },
        )
    } else {
        (speed, eta, file_progress)
    };
    let accessible_label = [
        title,
        status,
        queue,
        failure_summary,
        source_line,
        destination_line,
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join("，");
    OperationRow {
        id: -(index + 1),
        title: title.into(),
        status: status.into(),
        queue: queue.into(),
        failure_summary: failure_summary.into(),
        accessible_label: accessible_label.into(),
        source_line: source_line.into(),
        destination_line: destination_line.into(),
        current_item_line: current_item_line.into(),
        percent: if progress_known {
            format!("{:.0}%", progress * 100.0)
        } else {
            String::new()
        }
        .into(),
        file_progress: file_progress.into(),
        transferred: transferred.into(),
        speed: speed.into(),
        eta: eta.into(),
        progress,
        progress_known,
        state: operation_state_index(state),
        paused: state == OperationState::Paused,
        pause_pending: false,
        cancel_pending: state == OperationState::Cancelling,
        can_pause: matches!(state, OperationState::Running | OperationState::Paused),
        can_cancel: matches!(
            state,
            OperationState::Queued
                | OperationState::Preflight
                | OperationState::Running
                | OperationState::Paused
                | OperationState::WaitingConflict
        ),
        can_retry: matches!(
            state,
            OperationState::PartiallyCompleted | OperationState::Failed
        ),
    }
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
fn recovered_cleanup_requests(
    operations: &mut OperationManager,
    records: Vec<crate::fs::file_operations::CleanupTaskRecord>,
) -> Vec<FileOperationRequest> {
    for record in records {
        let items = record
            .items
            .iter()
            .filter_map(|item| {
                let pending = item.remaining_path.clone()?;
                Some(OperationItem::cleanup_pending(
                    item.original_path.clone(),
                    pending,
                    record.record_path.clone(),
                ))
            })
            .collect::<Vec<_>>();
        if items.is_empty() {
            if let Err(error) = crate::fs::file_operations::discard_empty_cleanup_task(&record) {
                eprintln!("unable to discard completed cleanup task: {error:?}");
            }
        } else {
            operations.submit_recovered_cleanup(None, items);
        }
    }
    let Some(id) = operations
        .start_next(OperationResource::Cleanup)
        .ok()
        .flatten()
    else {
        return Vec::new();
    };
    mark_operation_running_if_ready(operations, id);
    operations
        .task(id)
        .map(|task| {
            vec![FileOperationRequest {
                id,
                kind: task.kind,
                resource: task.resource,
                items: task.items.clone(),
                undo_items: task.undo_items.clone(),
                undo_source_manifests: task.undo_source_manifests.clone(),
                cancellation: task.cancellation.clone(),
            }]
        })
        .unwrap_or_default()
}

fn scan_cleanup_diagnostics(
    operation_sender: mpsc::Sender<FileOperationRequest>,
    state: SharedSessions,
) {
    let Some(record_root) = cleanup_record_root() else {
        return;
    };
    thread::spawn(move || {
        let discovered = crate::fs::file_operations::discover_cleanup_tasks(&record_root);
        let state_for_ui = state.clone();
        let _ = slint::invoke_from_event_loop(move || {
            let requests = if let Ok(mut app) = state_for_ui.lock() {
                match discovered {
                    Ok(records) => recovered_cleanup_requests(&mut app.operations, records),
                    Err(error) => {
                        app.operation_errors.push(format!(
                            "Unable to read AsterFiles cleanup records: {error:?}"
                        ));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            for request in requests {
                let _ = operation_sender.send(request);
            }
            refresh_all_windows(&state_for_ui, RefreshRegions::STATUS);
        });
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
                    sequence: platform::windows::clipboard::sequence_number(),
                },
                ClipboardRequest::Changed(sequence) => ClipboardEvent::Changed {
                    sequence: platform::windows::clipboard::sequence_number().max(sequence),
                    result: platform::windows::clipboard::read_file_list()
                        .map_err(|error| error.to_string()),
                },
                ClipboardRequest::CancelCut { paths, sequence } => {
                    match platform::windows::clipboard::cancel_file_cut_if_current(sequence, &paths)
                    {
                        Ok((sequence, snapshot)) => ClipboardEvent::Changed {
                            sequence,
                            result: Ok(snapshot),
                        },
                        Err(error) => ClipboardEvent::Changed {
                            sequence: platform::windows::clipboard::sequence_number(),
                            result: Err(error.to_string()),
                        },
                    }
                }
                ClipboardRequest::ReadPaste { origin_tab, target } => ClipboardEvent::Paste {
                    origin_tab,
                    result: platform::windows::clipboard::read_file_list()
                        .map(|clipboard| {
                            clipboard.map(|clipboard| {
                                let target =
                                    platform::windows::network::network_drive_to_unc(&target)
                                        .unwrap_or(target);
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
                                        let source =
                                            platform::windows::network::network_drive_to_unc(
                                                &source,
                                            )
                                            .unwrap_or(source);
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
                },
            };
            let _ = event_sender.send(event);
        }
    });
    (request_sender, event_receiver)
}

fn forward_clipboard_notifications(
    receiver: mpsc::Receiver<u32>,
    sender: mpsc::Sender<ClipboardRequest>,
) {
    thread::spawn(move || {
        while let Ok(sequence) = receiver.recv() {
            if sender.send(ClipboardRequest::Changed(sequence)).is_err() {
                break;
            }
        }
    });
}

fn quick_menu_for_session(
    session_id: u64,
    request_id: u64,
) -> Option<(SharedQuickMenu, AppWindow)> {
    WINDOW_RUNTIMES.with_borrow(|runtimes| {
        let window_id = WindowId((session_id >> 32) as u32);
        let runtime = runtimes.get(&window_id)?;
        let matches = runtime._quick_menu.lock().ok().is_some_and(|menu| {
            menu.identity.as_ref().is_some_and(|identity| {
                identity.session_id == session_id && identity.request_id == request_id
            })
        });
        matches.then(|| (runtime._quick_menu.clone(), runtime.ui.clone_strong()))
    })
}

fn start_shell_menu_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<platform::windows::context_menu::ShellMenuEvent>,
    worker: platform::windows::context_menu::ShellMenuWorker,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    _quick_menu: SharedQuickMenu,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let weak = weak.clone();
            let worker = worker.clone();
            let directory_sender = directory_sender.clone();
            let network_directory_sender = network_directory_sender.clone();
            let state = state.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let (session_id, request_id) = match &event {
                    platform::windows::context_menu::ShellMenuEvent::Loaded {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::SubmenuLoaded {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::SubmenuError {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::Invoked {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::Error {
                        session_id,
                        request_id,
                        ..
                    }
                    | platform::windows::context_menu::ShellMenuEvent::Closed {
                        session_id,
                        request_id,
                    } => (*session_id, *request_id),
                };
                let Some((menu_state, ui)) = quick_menu_for_session(session_id, request_id)
                    .or_else(|| {
                        weak.upgrade()
                            .map(|ui| (Arc::new(Mutex::new(QuickMenuState::default())), ui))
                    })
                else {
                    return;
                };
                match event {
                    platform::windows::context_menu::ShellMenuEvent::Loaded {
                        items,
                        elapsed_ms,
                        ..
                    } => {
                        let identity = menu_state
                            .lock()
                            .ok()
                            .and_then(|menu| menu.identity.clone());
                        let accepted = identity.as_ref().is_some_and(|identity| {
                            quick_menu_key_is_current(&state, &identity.key)
                        });
                        if !accepted {
                            let _ = worker.send(
                                platform::windows::context_menu::ShellMenuCommand::Close {
                                    session_id,
                                    request_id,
                                },
                            );
                            eprintln!(
                                "{{\"event\":\"shell_menu_stale\",\"session\":{session_id},\"request\":{request_id},\"phase\":\"load\"}}"
                            );
                            return;
                        }
                        if let Ok(mut menu) = menu_state.lock() {
                            apply_loaded_shell_menu(&mut menu, session_id, request_id, items);
                        }
                        if ui.get_context_menu_open() {
                            ui.set_context_shell_loading(false);
                            ui.set_context_shell_elapsed_ms(elapsed_ms.min(i32::MAX as u128) as i32);
                            project_filtered_context_menu(
                                &ui,
                                &menu_state,
                                ui.get_context_search().as_str(),
                            );
                            let create_open = menu_state
                                .lock()
                                .ok()
                                .is_some_and(|menu| menu.create_submenu_open);
                            if create_open
                                && let Some(index) = (0..ui.get_context_commands().row_count())
                                    .find(|index| {
                                        ui.get_context_commands()
                                            .row_data(*index)
                                            .is_some_and(|row| row.node_id == NODE_CREATE)
                                    })
                            {
                                ui.set_context_submenu_open(false);
                                ui.invoke_open_context_submenu(index as i32);
                            }
                        }
                        eprintln!(
                            "{{\"event\":\"shell_menu_loaded\",\"session\":{session_id},\"request\":{request_id},\"elapsed_ms\":{elapsed_ms}}}"
                        );
                    }
                    platform::windows::context_menu::ShellMenuEvent::SubmenuLoaded {
                        submenu_request_id,
                        token,
                        items,
                        elapsed_ms,
                        ..
                    } => {
                        let item_count = items.len();
                        let shell_items_nonempty = !items.is_empty();
                        let accepted = menu_state.lock().ok().is_some_and(|menu| {
                            submenu_result_matches(
                                &menu,
                                session_id,
                                request_id,
                                submenu_request_id,
                                token,
                            ) && menu.identity.as_ref().is_some_and(|identity| {
                                quick_menu_key_is_current(&state, &identity.key)
                            })
                        });
                        trace_quick_menu(
                            "quick_menu_submenu_result_received",
                            format!(
                                "session={} request={} submenu_request={} token={} items={} elapsed_ms={} accepted={}",
                                session_id,
                                request_id,
                                submenu_request_id,
                                token,
                                item_count,
                                elapsed_ms,
                                accepted,
                            ),
                        );
                        if !accepted {
                            return;
                        }
                        if let Ok(mut menu) = menu_state.lock() {
                            let rows = project_shell_menu_items(&mut menu, items, Some(token));
                            if shell_items_nonempty {
                                menu.loaded_submenu_rows.insert(token, rows.clone());
                            }
                            menu.submenu_rows = rows;
                        }
                        ui.set_context_submenu_loading(false);
                        project_context_submenu(&ui, &menu_state);
                        eprintln!(
                            "{{\"event\":\"shell_submenu_loaded\",\"session\":{session_id},\"request\":{request_id},\"elapsed_ms\":{elapsed_ms}}}"
                        );
                    }
                    platform::windows::context_menu::ShellMenuEvent::SubmenuError {
                        submenu_request_id,
                        token,
                        message,
                        elapsed_ms,
                        ..
                    } => {
                        let accepted = menu_state.lock().ok().is_some_and(|menu| {
                            submenu_result_matches(
                                &menu,
                                session_id,
                                request_id,
                                submenu_request_id,
                                token,
                            ) && menu.identity.as_ref().is_some_and(|identity| {
                                quick_menu_key_is_current(&state, &identity.key)
                            })
                        });
                        if accepted {
                            ui.set_context_submenu_loading(false);
                            let create = menu_state
                                .lock()
                                .ok()
                                .is_some_and(|menu| menu.create_submenu_token == Some(token));
                            if create {
                                let language = menu_state
                                    .lock()
                                    .ok()
                                    .and_then(|menu| menu.language)
                                    .unwrap_or(Language::English);
                                let rows = vec![built_in_folder_row(language)];
                                if let Ok(mut menu) = menu_state.lock() {
                                    menu.submenu_rows = rows.clone();
                                }
                                ui.set_context_submenu_active_index(0);
                                ui.set_context_submenu_content_height(context_menu_content_height(
                                    &rows,
                                ));
                                ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
                                project_context_submenu(&ui, &menu_state);
                            }
                            if let Ok(mut app) = state.lock() {
                                app.operation_errors.push(format!(
                                    "Shell submenu failed after {elapsed_ms} ms: {message}"
                                ));
                            }
                        }
                    }
                    platform::windows::context_menu::ShellMenuEvent::Invoked {
                        invocation,
                        elapsed_ms,
                        ..
                    } => {
                        let identity = menu_state
                            .lock()
                            .ok()
                            .and_then(|menu| menu.identity.clone());
                        if !identity.as_ref().is_some_and(|identity| {
                            quick_menu_key_is_current(&state, &identity.key)
                        }) {
                            let _ = worker.send(
                                platform::windows::context_menu::ShellMenuCommand::Close {
                                    session_id,
                                    request_id,
                                },
                            );
                            return;
                        }
                        if let platform::windows::context_menu::ClassicMenuInvocation::BuiltIn {
                            verb,
                        } = invocation
                        {
                            if identity.as_ref().is_some_and(|identity| {
                                matches!(identity.key.scope, QuickMenuScope::Content { .. })
                            }) {
                                match verb.as_str() {
                                    "copy" => ui.invoke_copy_selection(false),
                                    "cut" => ui.invoke_copy_selection(true),
                                    "paste" => ui.invoke_paste_files(),
                                    "delete" => {
                                        let permanent =
                                            menu_state.lock().ok().is_some_and(|mut menu| {
                                                std::mem::take(
                                                    &mut menu.pending_shell_delete_permanent,
                                                )
                                            });
                                        ui.invoke_request_delete(permanent);
                                    }
                                    "rename" => ui.invoke_begin_rename(),
                                    _ => {}
                                }
                            } else if let Ok(mut app) = state.lock() {
                                app.operation_errors.push(format!(
                                    "Ignored unsupported built-in sidebar Shell verb: {verb}"
                                ));
                            }
                        } else if let Some(identity) = identity {
                            match &identity.key.scope {
                                QuickMenuScope::Content { location, .. } => {
                                    if let Some(location) = shell_menu_invocation_refresh_target(
                                        &state,
                                        identity.key.tab_id,
                                        location.as_ref(),
                                        &invocation,
                                    ) {
                                        let _ = submit_location_navigation(
                                            &directory_sender,
                                            &network_directory_sender,
                                            &state,
                                            identity.key.tab_id,
                                            location,
                                            NavigationKind::Refresh,
                                        );
                                    }
                                }
                                QuickMenuScope::Sidebar {
                                    group, stable_id, ..
                                } => {
                                    let affected = state.lock().ok().and_then(|app| {
                                        sidebar_menu_target(
                                            &app,
                                            identity.key.window_id,
                                            *group,
                                            stable_id,
                                        )
                                    });
                                    if let Some(affected) = affected
                                        && let Some(tab) = state.lock().ok().and_then(|app| {
                                            app.tab(identity.key.tab_id)
                                                .map(|tab| tab.visible_location().cloned())
                                        })
                                    {
                                        let matches = match (&affected.destination, tab) {
                                            (
                                                SidebarDestination::Directory(path),
                                                Some(NavigationLocation::Directory(current)),
                                            ) => *path == current,
                                            (
                                                SidebarDestination::Library(library),
                                                Some(NavigationLocation::Library(current)),
                                            ) => *library == current,
                                            (
                                                SidebarDestination::WebDav(place),
                                                Some(NavigationLocation::WebDav(current)),
                                            ) => place == &current,
                                            _ => false,
                                        };
                                        if matches {
                                            let location = match affected.destination {
                                                SidebarDestination::Directory(path) => {
                                                    NavigationLocation::Directory(path)
                                                }
                                                SidebarDestination::Library(library) => {
                                                    NavigationLocation::Library(library)
                                                }
                                                SidebarDestination::WebDav(place) => {
                                                    NavigationLocation::WebDav(place)
                                                }
                                                SidebarDestination::External(_) => {
                                                    unreachable!()
                                                }
                                            };
                                            let _ = submit_location_navigation(
                                                &directory_sender,
                                                &network_directory_sender,
                                                &state,
                                                identity.key.tab_id,
                                                location,
                                                NavigationKind::Refresh,
                                            );
                                        }
                                    }
                                    refresh_all_windows(&state, RefreshRegions::CONTENT);
                                }
                                QuickMenuScope::SidebarBackground => {}
                            }
                        }
                        let _ =
                            worker.send(platform::windows::context_menu::ShellMenuCommand::Close {
                                session_id,
                                request_id,
                            });
                        eprintln!(
                            "{{\"event\":\"shell_menu_invoked\",\"session\":{session_id},\"request\":{request_id},\"elapsed_ms\":{elapsed_ms}}}"
                        );
                    }
                    platform::windows::context_menu::ShellMenuEvent::Error {
                        operation,
                        message,
                        elapsed_ms,
                        ..
                    } => {
                        let current = menu_state.lock().ok().is_some_and(|menu| {
                            menu.identity.as_ref().is_some_and(|identity| {
                                identity.session_id == session_id
                                    && identity.request_id == request_id
                                    && quick_menu_key_is_current(&state, &identity.key)
                            })
                        });
                        if current {
                            ui.set_context_shell_loading(false);
                            let create_open = menu_state
                                .lock()
                                .ok()
                                .is_some_and(|menu| menu.create_submenu_open);
                            if let Ok(mut menu) = menu_state.lock() {
                                clear_current_shell_menu_failure(&mut menu, session_id, request_id);
                            }
                            if create_open {
                                let language = menu_state
                                    .lock()
                                    .ok()
                                    .and_then(|menu| menu.language)
                                    .unwrap_or(Language::English);
                                let rows = vec![built_in_folder_row(language)];
                                if let Ok(mut menu) = menu_state.lock() {
                                    menu.submenu_rows = rows.clone();
                                }
                                ui.set_context_submenu_loading(false);
                                ui.set_context_submenu_active_index(0);
                                ui.set_context_submenu_content_height(context_menu_content_height(
                                    &rows,
                                ));
                                ui.set_context_submenu_commands(ModelRc::new(VecModel::from(rows)));
                                project_context_submenu(&ui, &menu_state);
                            }
                            if ui.get_context_menu_open() {
                                project_filtered_context_menu(
                                    &ui,
                                    &menu_state,
                                    ui.get_context_search().as_str(),
                                );
                            }
                            if let Ok(mut app) = state.lock() {
                                app.operation_errors.push(format!(
                                    "Shell menu {operation} failed after {elapsed_ms} ms: {message}"
                                ));
                            }
                            eprintln!(
                                "{{\"event\":\"shell_menu_error\",\"session\":{session_id},\"request\":{request_id},\"operation\":\"{operation}\",\"elapsed_ms\":{elapsed_ms},\"message\":{message:?}}}"
                            );
                        } else {
                            eprintln!(
                                "{{\"event\":\"shell_menu_stale_error\",\"session\":{session_id},\"request\":{request_id},\"operation\":\"{operation}\",\"elapsed_ms\":{elapsed_ms}}}"
                            );
                        }
                        let _ =
                            worker.send(platform::windows::context_menu::ShellMenuCommand::Close {
                                session_id,
                                request_id,
                            });
                    }
                    platform::windows::context_menu::ShellMenuEvent::Closed { .. } => {
                        if let Ok(mut menu) = menu_state.lock()
                            && menu.identity.as_ref().is_some_and(|identity| {
                                identity.session_id == session_id
                                    && identity.request_id == request_id
                            })
                        {
                            menu.identity = None;
                        }
                    }
                }
            });
        }
    });
}
fn start_clipboard_event_pump(
    receiver: mpsc::Receiver<ClipboardEvent>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    state: SharedSessions,
) {
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            let operation_sender = operation_sender.clone();
            let directory_sender = directory_sender.clone();
            let network_directory_sender = network_directory_sender.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let cut_paths_before = state
                    .lock()
                    .map(|app| app.cut_paths.clone())
                    .unwrap_or_default();
                match event {
                    ClipboardEvent::Written {
                        result: Err(error), ..
                    }
                    | ClipboardEvent::Paste {
                        result: Err(error), ..
                    }
                    | ClipboardEvent::Changed {
                        result: Err(error), ..
                    } => {
                        if let Ok(mut app) = state.lock() {
                            app.operation_errors.push(error);
                        }
                    }
                    ClipboardEvent::Paste {
                        origin_tab,
                        result: Ok(Some((kind, items))),
                    } => {
                        let _ = enqueue_operation(
                            &state,
                            &operation_sender,
                            origin_tab,
                            kind,
                            items,
                            None,
                        );
                    }
                    ClipboardEvent::Written {
                        result: Ok(()),
                        paths,
                        cut,
                        sequence,
                    } => {
                        if let Ok(mut app) = state.lock() {
                            app.clipboard_has_files = true;
                            app.cut_paths = if cut { paths } else { Vec::new() };
                            app.clipboard_sequence = Some(sequence);
                        }
                    }
                    ClipboardEvent::Changed {
                        sequence,
                        result: Ok(snapshot),
                    } => {
                        let directories = state
                            .lock()
                            .map(|mut app| apply_clipboard_snapshot(&mut app, sequence, snapshot))
                            .unwrap_or_default();
                        if !directories.is_empty() {
                            refresh_affected_tabs(
                                &directory_sender,
                                &network_directory_sender,
                                &state,
                                &directories,
                            );
                        }
                    }
                    ClipboardEvent::Paste {
                        result: Ok(None), ..
                    } => {}
                }
                let cut_paths_after = state
                    .lock()
                    .map(|app| app.cut_paths.clone())
                    .unwrap_or_default();
                let changed_paths = cut_paths_before
                    .into_iter()
                    .chain(cut_paths_after)
                    .collect::<Vec<_>>();
                refresh_cut_rows(&state, &changed_paths);
            });
        }
    });
}

fn clear_completed_cut_paths(app: &mut AppState, restored: &[PathBuf]) -> Vec<PathBuf> {
    let changed = app
        .cut_paths
        .iter()
        .filter(|path| restored.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    app.cut_paths.retain(|path| !restored.contains(path));
    changed
}

fn refresh_cut_rows(state: &SharedSessions, changed_paths: &[PathBuf]) {
    if changed_paths.is_empty() {
        return;
    }
    let changed_paths = changed_paths.iter().collect::<HashSet<_>>();
    let windows = WINDOW_RUNTIMES.with_borrow(|runtimes| {
        runtimes
            .iter()
            .map(|(window_id, runtime)| (*window_id, runtime.ui.clone_strong()))
            .collect::<Vec<_>>()
    });
    for (window_id, ui) in windows {
        let update = state.lock().ok().and_then(|app| {
            let window = app.window(window_id)?;
            let tab = window.tabs.get(&window.active_tab)?;
            let entries = tab
                .entries
                .iter()
                .chain(tab.pending_entries().iter())
                .filter(|entry| changed_paths.contains(&entry.path()))
                .map(|entry| entry.id)
                .collect::<HashSet<_>>();
            Some((tab.id, entries))
        });
        if let Some((tab_id, entries)) = update {
            update_file_rows(&ui, state, tab_id, &entries);
        }
    }
}

fn apply_clipboard_snapshot(
    app: &mut AppState,
    sequence: u32,
    snapshot: Option<platform::windows::clipboard::ClipboardFileList>,
) -> Vec<PathBuf> {
    if app.clipboard_sequence == Some(sequence) {
        return Vec::new();
    }
    app.clipboard_sequence = Some(sequence);
    app.clipboard_has_files = snapshot.is_some();
    let preserves_cut = snapshot.as_ref().is_some_and(|clipboard| {
        clipboard.operation == platform::windows::clipboard::ClipboardOperation::Move
            && clipboard.paths == app.cut_paths
    });
    if app.cut_paths.is_empty() || preserves_cut {
        return Vec::new();
    }
    let mut directories = app
        .cut_paths
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    directories.sort_unstable();
    directories.dedup();
    app.cut_paths.clear();
    directories
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
                    action: PendingFocusAction::Select,
                },
            );
        }
    }
}

fn pending_focus_is_valid(app: &AppState, tab_id: TabId, pending: &PendingFocus) -> bool {
    let Some(tab) = app.tab(tab_id) else {
        return false;
    };
    if tab.visible_path() != Some(pending.directory.as_path()) {
        return false;
    }
    match pending.action {
        PendingFocusAction::Select => true,
        PendingFocusAction::Reveal { window_id } | PendingFocusAction::Rename { window_id } => {
            app.window_for_tab(tab_id) == Some(window_id)
                && app
                    .window(window_id)
                    .is_some_and(|window| window.active_tab == tab_id)
        }
    }
}

fn queue_completed_rename(app: &mut AppState, origin_tab: TabId, target: PathBuf) {
    let Some(directory) = target.parent().map(Path::to_path_buf) else {
        return;
    };
    let Some(window_id) = app.window_for_tab(origin_tab) else {
        return;
    };
    if app
        .window(window_id)
        .is_none_or(|window| window.active_tab != origin_tab)
    {
        return;
    }
    if app.tab(origin_tab).and_then(TabSession::visible_path) != Some(directory.as_path()) {
        return;
    }
    app.focus_after_refresh.insert(
        origin_tab,
        PendingFocus {
            directory,
            request_id: None,
            paths: vec![target],
            action: PendingFocusAction::Rename { window_id },
        },
    );
}

fn directory_path_snapshot(tab: &TabSession) -> HashSet<PathBuf> {
    tab.visible_entries()
        .iter()
        .map(|entry| entry.path())
        .collect()
}

fn detect_unique_created_path(
    baseline: &HashSet<PathBuf>,
    entries: &[FileEntry],
) -> Option<PathBuf> {
    let mut added = entries
        .iter()
        .filter(|entry| !baseline.contains(&entry.path()))
        .map(|entry| entry.path());
    let path = added.next()?;
    added.next().is_none().then_some(path)
}

fn rename_input_for_entry(entry: &FileEntry) -> (String, usize) {
    let input = entry.original_name.to_string_lossy().into_owned();
    let selection_end = if entry.kind == crate::domain::EntryKind::File {
        input
            .rfind('.')
            .filter(|index| *index > 0)
            .unwrap_or(input.len())
    } else {
        input.len()
    };
    (input, selection_end)
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
    network_directory_sender: mpsc::SyncSender<DirectoryRequest>,
    everything_sender: mpsc::Sender<EverythingRequest>,
    shortcut_sender: mpsc::SyncSender<ShortcutRequest>,
    thumbnail_scheduler: ThumbnailScheduler,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    let operation_weak = operation_ui.as_weak();
    let conflict_weak = conflict_ui.as_weak();
    let delete_weak = delete_ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    let progress_refresh_queued = Arc::new(AtomicBool::new(false));
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let event = match event {
                FileOperationEvent::RecycleProgress { id, progress } => {
                    if let Ok(mut app) = state.lock()
                        && let Some(task) = app.operations.task_mut(id)
                        && !task.cancellation.is_cancelled()
                    {
                        match progress {
                            platform::windows::file_operation::RecycleProgress::Preparing {
                                prepared,
                                current,
                                ..
                            } => {
                                task.progress.prepared_items = prepared;
                                if let Some(current) = current {
                                    task.progress.current_item = Some(current);
                                }
                            }
                            platform::windows::file_operation::RecycleProgress::Executing {
                                current,
                                ..
                            } => {
                                if task.state == OperationState::Preflight {
                                    let _ = app.operations.mark_running(id);
                                }
                                if let Some(task) = app.operations.task_mut(id)
                                    && let Some(current) = current
                                {
                                    task.progress.current_item = Some(current);
                                }
                            }
                        }
                    }
                    queue_operation_progress_refresh(
                        operation_weak.clone(),
                        state.clone(),
                        &progress_refresh_queued,
                    );
                    continue;
                }
                FileOperationEvent::RecycleDiscovery {
                    id,
                    discovered_items,
                    discovered_bytes,
                    complete,
                    current_item,
                } => {
                    if let Ok(mut app) = state.lock()
                        && let Some(task) = app.operations.task_mut(id)
                        && !task.cancellation.is_cancelled()
                    {
                        task.progress.discovered_files =
                            task.progress.discovered_files.max(discovered_items);
                        task.progress.discovered_bytes =
                            task.progress.discovered_bytes.max(discovered_bytes);
                        task.progress.scanning_complete |= complete;
                        task.progress.current_item = Some(current_item);
                    }
                    queue_operation_progress_refresh(
                        operation_weak.clone(),
                        state.clone(),
                        &progress_refresh_queued,
                    );
                    continue;
                }
                FileOperationEvent::CopyRecovery { id, recovering } => {
                    if let Ok(mut app) = state.lock()
                        && let Some(task) = app.operations.task_mut(id)
                        && task.state.is_active()
                        && !task.cancellation.is_cancelled()
                    {
                        task.progress.copy_recovering = recovering;
                    }
                    queue_operation_progress_refresh(
                        operation_weak.clone(),
                        state.clone(),
                        &progress_refresh_queued,
                    );
                    continue;
                }
                FileOperationEvent::Progress {
                    id,
                    completed_items,
                    completed_files,
                    skipped_files,
                    total_files,
                    discovered_files,
                    processed_bytes,
                    skipped_bytes,
                    total_bytes,
                    discovered_bytes,
                    scanning_complete,
                    current_item,
                    recent_speed_bps,
                } => {
                    if let Ok(mut app) = state.lock()
                        && let Some(task) = app.operations.task_mut(id)
                        && !task.cancellation.is_cancelled()
                    {
                        task.progress.completed_items = completed_items;
                        task.progress.completed_files = completed_files;
                        task.progress.skipped_files = skipped_files;
                        task.progress.total_files = total_files;
                        task.progress.discovered_files =
                            task.progress.discovered_files.max(discovered_files);
                        task.progress.processed_bytes = processed_bytes;
                        task.progress.skipped_bytes = skipped_bytes;
                        task.progress.total_bytes = total_bytes;
                        task.progress.discovered_bytes =
                            task.progress.discovered_bytes.max(discovered_bytes);
                        task.progress.scanning_complete |= scanning_complete;
                        task.progress.current_item = Some(current_item);
                        task.progress.recent_speed_bps = recent_speed_bps;
                    }
                    queue_operation_progress_refresh(
                        operation_weak.clone(),
                        state.clone(),
                        &progress_refresh_queued,
                    );
                    continue;
                }
                event => event,
            };
            let weak = weak.clone();
            let operation_weak = operation_weak.clone();
            let conflict_weak = conflict_weak.clone();
            let delete_weak = delete_weak.clone();
            let exit_weak = exit_weak.clone();
            let sender = sender.clone();
            let directory_sender = directory_sender.clone();
            let network_directory_sender = network_directory_sender.clone();
            let state = state.clone();
            let everything_sender = everything_sender.clone();
            let shortcut_sender = shortcut_sender.clone();
            let thumbnail_scheduler = thumbnail_scheduler.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let event_operation_id = match &event {
                    FileOperationEvent::DestinationCreated { id, .. }
                    | FileOperationEvent::Progress { id, .. }
                    | FileOperationEvent::CopyRecovery { id, .. }
                    | FileOperationEvent::RecycleProgress { id, .. }
                    | FileOperationEvent::RecycleDiscovery { id, .. }
                    | FileOperationEvent::PermanentDeleteStage { id, .. }
                    | FileOperationEvent::PermanentDeleteCommitted { id, .. }
                    | FileOperationEvent::Conflict { id, .. }
                    | FileOperationEvent::Finished { id, .. } => *id,
                };
                match event {
                    FileOperationEvent::DestinationCreated { path, .. } => {
                        let parent = path.parent().map(Path::to_path_buf);
                        if let Some(parent) = parent {
                            refresh_affected_tabs(
                                &directory_sender,
                                &network_directory_sender,
                                &state,
                                &[parent],
                            );
                        }
                    }
                    FileOperationEvent::Progress { .. }
                    | FileOperationEvent::CopyRecovery { .. }
                    | FileOperationEvent::RecycleProgress { .. }
                    | FileOperationEvent::RecycleDiscovery { .. } => unreachable!(),
                    FileOperationEvent::PermanentDeleteStage {
                        id,
                        stage,
                        item_updates,
                        affected_directories,
                    } => {
                        if let Ok(mut app) = state.lock()
                            && let Some(task) = app.operations.task_mut(id)
                        {
                            task.set_permanent_delete_stage(stage);
                            if stage == PermanentDeleteStage::SourceRemoved {
                                task.source_removed_at.get_or_insert_with(Instant::now);
                            }
                            if stage == PermanentDeleteStage::ReleasingSpace {
                                task.cleanup_started_at.get_or_insert_with(Instant::now);
                            }
                            for (index, pending, record) in item_updates {
                                if let Some(item) = task.items.get_mut(index) {
                                    item.destination = Some(pending);
                                    item.cleanup_record = Some(record);
                                    item.permanent_delete_phase =
                                        PermanentDeletePhase::CleanupPending;
                                }
                            }
                        }
                        if !affected_directories.is_empty() {
                            refresh_affected_tabs(
                                &directory_sender,
                                &network_directory_sender,
                                &state,
                                &affected_directories,
                            );
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
                    FileOperationEvent::PermanentDeleteCommitted {
                        id,
                        cleanup,
                        result,
                        item_states,
                    } => {
                        let (affected, requests) = {
                            let mut app = state.lock().expect("app state mutex is not poisoned");
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
                            let (origin_tab, task_items) = app
                                .operations
                                .task(id)
                                .map(|task| (task.origin_tab, task.items.clone()))
                                .unwrap_or((None, Vec::new()));
                            let mut affected = result.affected_directories.clone();
                            let registered = release_operation_directories(
                                &mut app,
                                FileOperationKind::PermanentDelete,
                                &task_items,
                            );
                            affected.extend(registered.iter().cloned());
                            affected.sort();
                            affected.dedup();
                            let _ = app.operations.finish(id, terminal, result);

                            if let Some(cleanup) = cleanup {
                                let cleanup_id = app.operations.submit(
                                    OperationResource::Cleanup,
                                    FileOperationKind::PermanentDelete,
                                    origin_tab,
                                    cleanup.items,
                                );
                                if let Some(task) = app.operations.task_mut(cleanup_id) {
                                    task.source_removed_at = Some(Instant::now());
                                    task.set_permanent_delete_stage(
                                        PermanentDeleteStage::ReleasingSpace,
                                    );
                                }
                            }

                            let mut requests = Vec::new();
                            for resource in [OperationResource::Cleanup, OperationResource::Local] {
                                let next =
                                    app.operations.start_next(resource).ok().flatten().and_then(
                                        |next_id| {
                                            mark_operation_running_if_ready(
                                                &mut app.operations,
                                                next_id,
                                            );
                                            app.operations.task(next_id).cloned().map(|task| {
                                                let request = FileOperationRequest {
                                                    id: next_id,
                                                    kind: task.kind,
                                                    resource: task.resource,
                                                    items: task.items.clone(),
                                                    undo_items: task.undo_items.clone(),
                                                    undo_source_manifests: task
                                                        .undo_source_manifests
                                                        .clone(),
                                                    cancellation: task.cancellation.clone(),
                                                };
                                                if resource != OperationResource::Cleanup {
                                                    register_operation_directories(
                                                        &mut app,
                                                        request.kind,
                                                        &request.items,
                                                    );
                                                }
                                                request
                                            })
                                        },
                                    );
                                if let Some(next) = next {
                                    requests.push(next);
                                }
                            }
                            (affected, requests)
                        };
                        for request in requests {
                            let _ = sender.send(request);
                        }
                        refresh_operation_badges(&state);
                        let affected_roots = affected_drive_roots(&affected);
                        if !affected_roots.is_empty() {
                            begin_drive_capacity_refresh(
                                &state,
                                Some(&affected_roots),
                                Duration::ZERO,
                            );
                        }
                        refresh_affected_tabs(
                            &directory_sender,
                            &network_directory_sender,
                            &state,
                            &affected,
                        );
                    }
                    event @ FileOperationEvent::Finished { .. } => {
                        let Some(OperationCompletion {
                            affected,
                            next,
                            clear_completed_cut,
                            undo_failure,
                            containment_notice,
                            rename_update,
                        }) = finish_file_operation(&state, event)
                        else {
                            return;
                        };
                        if let Some((window_id, language, message)) = containment_notice
                            && let Some(ui) = window_ui(window_id)
                        {
                            platform::windows::show_error_dialog(
                                native_window_handle(&ui),
                                Texts::new(language).self_containment_title(),
                                message,
                            );
                        }
                        if let (Some(ui), Some(message)) = (weak.upgrade(), undo_failure) {
                            show_undo_failure_dialog(
                                &ui,
                                state
                                    .lock()
                                    .map(|app| app.language)
                                    .unwrap_or(Language::Chinese),
                                &message,
                            );
                        }
                        if !clear_completed_cut.is_empty() {
                            let changed = state
                                .lock()
                                .map(|mut app| {
                                    clear_completed_cut_paths(&mut app, &clear_completed_cut)
                                })
                                .unwrap_or_default();
                            refresh_cut_rows(&state, &changed);
                        }
                        if let Some(request) = next {
                            let _ = sender.send(request);
                        }
                        rename_updates::present(
                            &state,
                            rename_update,
                            &directory_sender,
                            &network_directory_sender,
                            &everything_sender,
                            &shortcut_sender,
                            &thumbnail_scheduler,
                        );
                        refresh_operation_badges(&state);
                        let affected_roots = affected_drive_roots(&affected);
                        if !affected_roots.is_empty() {
                            begin_drive_capacity_refresh(
                                &state,
                                Some(&affected_roots),
                                Duration::ZERO,
                            );
                        }
                        refresh_affected_tabs(
                            &directory_sender,
                            &network_directory_sender,
                            &state,
                            &affected,
                        );
                    }
                }
                if let Some(conflict_ui) = conflict_weak.upgrade()
                    && !conflict_ui.get_demo_mode()
                    && state.lock().is_ok_and(|app| {
                        should_close_conflict_window(conflict_ui.get_operation_id().as_str(), &app)
                    })
                {
                    conflict_ui.set_operation_id("".into());
                    conflict_ui.set_apply_all(false);
                    let _ = conflict_ui.hide();
                }
                let rename_update = state.lock().ok().and_then(|app| {
                    let task = app.operations.task(event_operation_id)?;
                    (task.kind == FileOperationKind::Rename
                        && task.state != OperationState::Running)
                        .then(|| {
                            (
                                task.origin_tab
                                    .and_then(|tab_id| app.window_for_tab(tab_id)),
                                task.state == OperationState::Completed,
                                task.items.iter().find_map(|item| item.error.clone()),
                            )
                        })
                });
                if let Some((Some(window_id), completed, error)) = rename_update
                    && let Some(ui) = window_ui(window_id)
                {
                    if completed {
                        if let Ok(mut app) = state.lock() {
                            app.rename_targets.remove(&window_id);
                        }
                        ui.set_rename_submitting(false);
                        ui.set_rename_editing(false);
                    } else if let Some(message) = error {
                        if let Ok(mut app) = state.lock() {
                            app.operation_errors.push(message);
                        }
                        ui.set_rename_submitting(false);
                        ui.set_rename_submit_generation(ui.get_rename_submit_generation() + 1);
                    }
                }
                if state
                    .lock()
                    .is_ok_and(|app| app.exit_after_cancel && !app.operations.has_active_tasks())
                {
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
    directory_watch_refresh::start_watch_refresh_pump(
        event_receiver,
        directory_sender,
        state,
        failed_roots,
    );
    timer
}
fn watched_roots(app: &AppState) -> std::collections::HashSet<PathBuf> {
    app.windows
        .values()
        .flat_map(|window| window.tabs.values())
        .flat_map(|tab| match tab.visible_location() {
            Some(NavigationLocation::Home | NavigationLocation::WebDav(_)) => Vec::new(),
            Some(NavigationLocation::Directory(path)) => vec![path.clone()],
            Some(NavigationLocation::Library(library)) => app
                .libraries
                .iter()
                .find(|candidate| candidate.id.as_os_str() == library.identity.as_os_str())
                .into_iter()
                .flat_map(|definition| definition.sources.iter())
                .filter_map(|source| source.path.clone())
                .collect(),
            None => Vec::new(),
        })
        .filter(|path| !crate::network::is_unc_path(path))
        .collect()
}

fn refresh_affected_tabs(
    sender: &mpsc::Sender<DirectoryRequest>,
    network_sender: &mpsc::SyncSender<DirectoryRequest>,
    state: &SharedSessions,
    directories: &[PathBuf],
) {
    let targets = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.windows
            .values()
            .flat_map(|window| window.tabs.values())
            .filter_map(|tab| {
                let location = tab.visible_location()?.clone();
                let affected = match &location {
                    NavigationLocation::Home | NavigationLocation::WebDav(_) => false,
                    NavigationLocation::Directory(path) => directories.contains(path),
                    NavigationLocation::Library(library) => app
                        .libraries
                        .iter()
                        .find(|candidate| candidate.id.as_os_str() == library.identity.as_os_str())
                        .is_some_and(|definition| {
                            definition.sources.iter().any(|source| {
                                source
                                    .path
                                    .as_ref()
                                    .is_some_and(|path| directories.contains(path))
                            })
                        }),
                };
                affected.then_some((tab.id, location))
            })
            .collect::<Vec<_>>()
    };
    for (tab, location) in targets {
        let pending = state.lock().ok().and_then(|mut app| {
            let path = location.directory_path()?;
            let pending = app.focus_after_refresh.remove(&tab)?;
            (pending.directory == path && pending_focus_is_valid(&app, tab, &pending))
                .then_some(pending)
        });
        if submit_location_navigation(
            sender,
            network_sender,
            state,
            tab,
            location,
            NavigationKind::BackgroundRefresh,
        ) {
            if let Some(mut pending) = pending
                && let Ok(mut app) = state.lock()
            {
                pending.request_id = app.tab(tab).map(|session| session.latest_request());
                app.focus_after_refresh.insert(tab, pending);
            }
            refresh_tab_window(state, tab);
        }
    }
}

fn deliver_directory_event_to_ui<E>(
    enqueue: impl FnOnce(mpsc::Sender<()>) -> Result<(), E>,
    alive: impl Fn() -> bool,
) -> bool {
    let (completed, completion) = mpsc::channel();
    if enqueue(completed).is_err() {
        return false;
    }
    loop {
        match completion.recv_timeout(Duration::from_millis(20)) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return true,
            Err(mpsc::RecvTimeoutError::Timeout) if !alive() => return false,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn start_event_pump(
    ui: &AppWindow,
    receiver: Arc<Mutex<mpsc::Receiver<DirectoryEvent>>>,
    icon_sender: mpsc::Sender<IconRequest>,
    thumbnail_scheduler: ThumbnailScheduler,
    shortcut_sender: mpsc::SyncSender<ShortcutRequest>,
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
            let thumbnail_scheduler = thumbnail_scheduler.clone();
            let shortcut_sender = shortcut_sender.clone();
            let everything_sender = everything_sender.clone();
            let waiting_state = state.clone();
            if !deliver_directory_event_to_ui(
                |completed| {
                    weak.upgrade_in_event_loop(move |ui| {
                        // Retain this guard until the model and visible rows have consumed the event.
                        let _completed = completed;
                        let accepted = state
                            .lock()
                            .ok()
                            .is_some_and(|app| directory_event_is_current(&app, &event));
                        let routed_tab = accepted.then_some(event.request_identity().0);
                        let batch = match &event {
                            DirectoryEvent::Batch {
                                tab_id, request_id, ..
                            } if accepted => Some((*tab_id, *request_id)),
                            _ => None,
                        };
                        let finished = match &event {
                            DirectoryEvent::Finished {
                                tab_id, request_id, ..
                            } if accepted => Some((*tab_id, *request_id)),
                            _ => None,
                        };
                        let network_tab = routed_tab.is_some_and(|tab_id| {
                            state.lock().ok().is_some_and(|app| {
                                app.tab(tab_id)
                                    .and_then(TabSession::visible_path)
                                    .is_some_and(crate::network::is_unc_path)
                            })
                        });
                        let retained_refresh = finished.is_some_and(|(tab_id, _)| {
                            state.lock().ok().is_some_and(|app| {
                                app.tab(tab_id)
                                    .is_some_and(TabSession::retains_directory_view)
                            })
                        });
                        let network_root_finished = finished.is_some_and(|(tab_id, request_id)| {
                            state.lock().ok().is_some_and(|app| {
                                app.tab(tab_id).is_some_and(|tab| {
                                    tab.latest_request() == request_id
                                        && tab
                                            .requested_location()
                                            .as_ref()
                                            .and_then(NavigationLocation::directory_path)
                                            .is_some_and(crate::network::is_unc_server_root)
                                })
                            })
                        });
                        if network_root_finished {
                            platform::windows::network::record_runtime_event(
                                "network_root_event_apply_started",
                            );
                        }
                        let focus_action = finished.and_then(|(tab_id, request_id)| {
                            let app = state.lock().ok()?;
                            app.focus_after_refresh
                                .get(&tab_id)
                                .filter(|pending| pending.request_id == Some(request_id))
                                .map(|pending| pending.action)
                        });
                        let network_scroll =
                            finished
                                .filter(|_| network_tab)
                                .and_then(|(tab_id, request_id)| {
                                    let target = state.lock().ok().and_then(|app| {
                                        app.tab(tab_id).filter(|tab| tab.accepts(request_id))?;
                                        let window_id = app.window_for_tab(tab_id)?;
                                        (app.window(window_id)?.active_tab == tab_id)
                                            .then(|| window_ui(window_id))
                                            .flatten()
                                    })?;
                                    (target.get_file_viewport_y() < 0.0).then(|| {
                                        (
                                            target.clone_strong(),
                                            capture_zoom_anchor(&target, 16.0, 0.0),
                                        )
                                    })
                                });
                        let change = apply_event(&state, event);
                        if !change.accepted {
                            return;
                        }
                        let icon_requests = change.icon_requests;
                        if network_root_finished {
                            platform::windows::network::record_runtime_event(
                                "network_root_event_apply_completed",
                            );
                        }
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
                            defer_visible_file_images(
                                state.clone(),
                                tab_id,
                                thumbnail_scheduler.clone(),
                            );
                            if let Some(target_ui) = state
                                .lock()
                                .ok()
                                .and_then(|app| app.window_for_tab(tab_id))
                                .and_then(window_ui)
                            {
                                submit_visible_shortcuts(
                                    &shortcut_sender,
                                    &state,
                                    &target_ui,
                                    tab_id,
                                    request_id,
                                );
                            }
                        } else {
                            if let Some(tab_id) = routed_tab {
                                refresh_tab_window(&state, tab_id);
                            } else {
                                refresh_all_windows(&state, RefreshRegions::CONTENT);
                            }
                            if let Some((target, anchor)) = network_scroll {
                                restore_zoom_anchor(&target, anchor);
                            }
                            if let Some((tab_id, request_id)) = finished
                                && let Some(target_ui) = state
                                    .lock()
                                    .ok()
                                    .and_then(|app| app.window_for_tab(tab_id))
                                    .and_then(window_ui)
                            {
                                if !network_tab {
                                    submit_visible_folder_sizes(
                                        &everything_sender,
                                        &state,
                                        tab_id,
                                        request_id,
                                        &target_ui,
                                    );
                                }
                                let focused = state.lock().ok().and_then(|app| {
                                    app.tab(tab_id)
                                        .filter(|tab| tab.latest_request() == request_id)
                                        .and_then(|tab| tab.focused)
                                });
                                if focus_action == Some(PendingFocusAction::Select) {
                                    reveal_completed_selection(
                                        &target_ui, &state, tab_id, request_id,
                                    );
                                } else if let Some(entry_id) = focused.filter(|_| {
                                    focus_action.is_some() || (!network_tab && !retained_refresh)
                                }) {
                                    reveal_entry(&target_ui, &state, tab_id, request_id, entry_id);
                                }
                                submit_visible_shortcuts(
                                    &shortcut_sender,
                                    &state,
                                    &target_ui,
                                    tab_id,
                                    request_id,
                                );
                                request_completed_file_images(
                                    &target_ui,
                                    &state,
                                    tab_id,
                                    &thumbnail_scheduler,
                                );
                            }
                            if network_root_finished {
                                platform::windows::network::record_runtime_event(
                                    "network_root_ui_refresh_completed",
                                );
                            }
                        }
                    })
                },
                || {
                    waiting_state
                        .lock()
                        .is_ok_and(|app| !app.windows.is_empty())
                },
            ) {
                break;
            }
        }
    });
}

fn focus_bottom_selected_entry(app: &mut AppState, tab_id: TabId) {
    let focused = app.tab(tab_id).and_then(|tab| {
        directory_group_projections(app, tab, &tab.entries)
            .into_iter()
            .flat_map(|group| group.entries)
            .rfind(|id| tab.selected.contains(id))
    });
    if let Some(focused) = focused
        && let Some(tab) = app.tab_mut(tab_id)
    {
        tab.focused = Some(focused);
        tab.selection_anchor = Some(focused);
    }
}

fn selected_entry_span(
    groups: &[group_projection::GroupProjection],
    view_mode: ViewMode,
    columns: usize,
    selected: &[EntryId],
) -> Option<(f32, f32)> {
    let geometry = file_layout_geometry(view_mode);
    let header_height = group_header_height(groups);
    let positions = if geometry.grid {
        let projection =
            IconProjection::from_groups(groups, columns, header_height, geometry.row_height as u64);
        selected
            .iter()
            .filter_map(|id| {
                projection
                    .entry_position(*id)
                    .and_then(|position| projection.offsets.row_start(position.row_index))
            })
            .collect::<Vec<_>>()
    } else {
        let projection =
            ListProjection::from_groups(groups, header_height, geometry.row_height as u64);
        selected
            .iter()
            .filter_map(|id| {
                projection
                    .entry_position(*id)
                    .and_then(|position| projection.offsets.row_start(position))
            })
            .collect::<Vec<_>>()
    };
    Some((
        *positions.iter().min()? as f32,
        *positions.iter().max()? as f32 + geometry.row_height,
    ))
}

fn completed_selection_scroll_target(
    current: f32,
    first_top: f32,
    last_bottom: f32,
    visible_height: f32,
    maximum: f32,
) -> f32 {
    if last_bottom - first_top > visible_height {
        // The bottommost result anchors a full viewport of preceding results.
        (visible_height - last_bottom).clamp(-maximum, 0.0)
    } else {
        reveal_scroll_target(
            current,
            first_top,
            last_bottom - first_top,
            visible_height,
            maximum,
        )
    }
}

struct VisibleSelectionBeforeReorder {
    tab_id: TabId,
    request_id: RequestId,
    selected: Vec<EntryId>,
}

fn visible_selection_before_reorder(
    ui: &AppWindow,
    app: &AppState,
    tab_id: TabId,
    request_id: RequestId,
) -> Option<VisibleSelectionBeforeReorder> {
    let window = app.window(app.window_for_tab(tab_id)?)?;
    let tab = window.tabs.get(&tab_id)?;
    if window.active_tab != tab_id
        || !tab.accepts_page(request_id, PageSource::Directory)
        || tab.load_state != LoadState::Complete
        || tab.selected.is_empty()
        || ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != request_id.0 as i32
    {
        return None;
    }
    let mode = app.view_mode_for_tab(tab_id)?;
    let groups = directory_group_projections(app, tab, &tab.entries);
    let (_, bottom) = selected_entry_span(
        &groups,
        mode,
        ui.get_grid_column_count().max(1) as usize,
        &tab.selected,
    )?;
    let bottom = bottom + ui.get_file_viewport_y();
    let row_height = file_layout_geometry(mode).row_height;
    // A background reorder may keep a visible result in view, but cannot undo a user's scroll.
    (bottom - row_height >= -0.5 && bottom <= ui.get_file_viewport_height() + 0.5).then(|| {
        VisibleSelectionBeforeReorder {
            tab_id,
            request_id,
            selected: tab.selected.to_vec(),
        }
    })
}

fn restore_visible_selection_after_reorder(
    ui: &AppWindow,
    state: &SharedSessions,
    before: Option<VisibleSelectionBeforeReorder>,
) {
    let Some(before) = before else {
        return;
    };
    let unchanged = state.lock().is_ok_and(|app| {
        app.window_for_tab(before.tab_id)
            .and_then(|id| app.window(id))
            .filter(|window| window.active_tab == before.tab_id)
            .and_then(|window| window.tabs.get(&before.tab_id))
            .is_some_and(|tab| {
                tab.accepts_page(before.request_id, PageSource::Directory)
                    && tab.selected == before.selected
            })
    });
    if unchanged {
        reveal_completed_selection(ui, state, before.tab_id, before.request_id);
    }
}
fn reveal_completed_selection(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    let span = state.lock().ok().and_then(|app| {
        let window = app.window(app.window_for_tab(tab_id)?)?;
        let tab = window.tabs.get(&tab_id)?;
        if window.active_tab != tab_id || tab.latest_request() != request_id {
            return None;
        }
        let view_mode = app.view_mode_for_tab(tab_id)?;
        let groups = directory_group_projections(&app, tab, &tab.entries);
        let span = selected_entry_span(
            &groups,
            view_mode,
            ui.get_grid_column_count().max(1) as usize,
            &tab.selected,
        )?;
        Some((view_mode, span))
    });
    let Some((view_mode, (first_top, last_bottom))) = span else {
        return;
    };
    let visible_height = ui
        .get_file_viewport_height()
        .max(file_layout_geometry(view_mode).row_height);
    let maximum = projected_scroll_maximum(ui, view_mode, visible_height);
    ui.set_file_viewport_y(completed_selection_scroll_target(
        ui.get_file_viewport_y(),
        first_top,
        last_bottom,
        visible_height,
        maximum,
    ));
}

fn reveal_scroll_target(
    current: f32,
    entry_top: f32,
    entry_extent: f32,
    visible_height: f32,
    maximum: f32,
) -> f32 {
    let row_top = entry_top + current;
    let row_bottom = row_top + entry_extent;
    let viewport = if row_top < 0.0 {
        -entry_top
    } else if row_bottom > visible_height {
        visible_height - entry_top - entry_extent
    } else {
        current
    };
    viewport.clamp(-maximum, 0.0)
}
fn reveal_entry(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    entry_id: EntryId,
) {
    let snapshot = state.lock().ok().and_then(|app| {
        let window_id = app.window_for_tab(tab_id)?;
        let window = app.window(window_id)?;
        let tab = window.tabs.get(&tab_id)?;
        (window.active_tab == tab_id && tab.latest_request() == request_id).then(|| {
            let entries = directory_display_entries(tab);
            (
                app.view_mode_for_tab(tab_id)
                    .unwrap_or(app.default_directory_view.view_mode),
                tab.page_source,
                tab.search_total,
                entries.iter().position(|entry| entry.id == entry_id),
                directory_group_projections(&app, tab, entries),
            )
        })
    });
    let Some((view_mode, page_source, search_total, entry_index, groups)) = snapshot else {
        return;
    };
    let columns = ui.get_grid_column_count().max(1) as usize;
    if page_source == PageSource::Search {
        let index = entry_id.0.saturating_sub(1);
        let maximum = search_logical_maximum(
            search_total.unwrap_or(0),
            view_mode,
            columns,
            ui.get_file_viewport_height(),
        );
        let logical_scroll =
            search_scroll_for_index(index, view_mode, columns).clamp(-maximum, 0.0);
        ui.set_search_scroll_y(logical_scroll);
        let window = search_window_for_index(index, search_total.unwrap_or(0), columns);
        ui.set_file_viewport_y(search_window_viewport_y(
            logical_scroll,
            window,
            view_mode,
            columns,
        ));
        refresh_tab_window(state, tab_id);
        return;
    }

    let geometry = file_layout_geometry(view_mode);
    let entry_top = if geometry.grid {
        let projection = IconProjection::from_groups(
            &groups,
            columns,
            group_header_height(&groups),
            geometry.row_height as u64,
        );
        projection
            .entry_position(entry_id)
            .and_then(|position| projection.offsets.row_start(position.row_index))
            .map(|value| value as f32)
    } else {
        let projection = ListProjection::from_groups(
            &groups,
            group_header_height(&groups),
            geometry.row_height as u64,
        );
        projection
            .entry_position(entry_id)
            .and_then(|position| projection.offsets.row_start(position))
            .map(|value| value as f32)
    }
    .or_else(|| entry_index.map(|index| index as f32 * geometry.row_height));
    let Some(entry_top) = entry_top else {
        return;
    };
    let visible_height = ui.get_file_viewport_height().max(geometry.row_height);
    let maximum = projected_scroll_maximum(ui, view_mode, visible_height);
    ui.set_file_viewport_y(reveal_scroll_target(
        ui.get_file_viewport_y(),
        entry_top,
        geometry.row_height,
        visible_height,
        maximum,
    ));
}

fn directory_event_is_current(app: &AppState, event: &DirectoryEvent) -> bool {
    let (tab_id, request_id) = event.request_identity();
    let Some(tab) = app.tab(tab_id) else {
        return false;
    };
    match event {
        DirectoryEvent::Finished { path, library, .. } => {
            let location = library
                .clone()
                .map(NavigationLocation::Library)
                .unwrap_or_else(|| NavigationLocation::Directory(path.clone()));
            tab.accepts_directory(request_id)
                && tab.requested_location().as_ref() == Some(&location)
        }
        DirectoryEvent::Cancelled { .. } => {
            tab.latest_request() == request_id
                && tab.page_source == PageSource::Directory
                && matches!(
                    tab.load_state,
                    LoadState::Loading | LoadState::Partial | LoadState::Cancelled
                )
        }
        _ => tab.accepts_directory(request_id),
    }
}

struct DirectoryEventChange {
    accepted: bool,
    icon_requests: Vec<IconRequest>,
}

fn apply_event(state: &SharedSessions, event: DirectoryEvent) -> DirectoryEventChange {
    let acknowledgement = match &event {
        DirectoryEvent::Batch {
            acknowledgement, ..
        } => Some(acknowledgement.clone()),
        _ => None,
    };
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let accepted = directory_event_is_current(&app, &event);
    if !accepted {
        if let Some(acknowledgement) = acknowledgement {
            let _ = acknowledgement.send(());
        }
        return DirectoryEventChange {
            accepted: false,
            icon_requests: Vec::new(),
        };
    }
    let mut icon_requests = Vec::new();
    match event {
        DirectoryEvent::Batch {
            tab_id,
            request_id,
            entries,
            ..
        } => {
            let preference = app
                .tab(tab_id)
                .and_then(|tab| tab.visible_path())
                .map(|path| app.directory_preference(path))
                .unwrap_or(app.default_directory_view);
            let grid_layout = app
                .view_mode_for_tab(tab_id)
                .is_some_and(ViewMode::uses_grid_layout);
            if !grid_layout {
                icon_requests.extend(entries.iter().filter_map(|entry| {
                    ordinary_icon_request(&mut app, tab_id, request_id, entry)
                }));
            }
            app.tab_mut(tab_id)
                .expect("accepted tab exists")
                .accept_directory_batch(request_id, entries, preference);
        }
        DirectoryEvent::Finished {
            tab_id,
            request_id,
            path,
            skipped,
            source_failures,
            library,
        } => {
            let location = library
                .clone()
                .map(NavigationLocation::Library)
                .unwrap_or_else(|| NavigationLocation::Directory(path.clone()));
            let pending_focus = app
                .focus_after_refresh
                .get(&tab_id)
                .cloned()
                .filter(|pending| {
                    library.is_none()
                        && pending.request_id == Some(request_id)
                        && pending.directory == path
                });
            let invalid_reveal = pending_focus.as_ref().is_some_and(|pending| {
                matches!(pending.action, PendingFocusAction::Reveal { .. })
                    && !pending_focus_is_valid(&app, tab_id, pending)
            });
            if invalid_reveal {
                app.focus_after_refresh.remove(&tab_id);
            }
            let focus =
                pending_focus.filter(|pending| pending_focus_is_valid(&app, tab_id, pending));
            let shell_create = app
                .window_for_tab(tab_id)
                .and_then(|window_id| app.pending_shell_creates.get(&window_id).cloned())
                .filter(|pending| {
                    library.is_none() && pending.tab_id == tab_id && pending.directory == path
                });
            let shell_create = shell_create.and_then(|pending| {
                app.pending_shell_creates.remove(&pending.window_id);
                (pending.started_at.elapsed() <= Duration::from_secs(5)
                    && app
                        .window(pending.window_id)
                        .is_some_and(|window| window.active_tab == tab_id))
                .then_some(pending)
            });
            let consumed_focus = focus.is_some();
            let language = app.language;
            let preference = library
                .as_ref()
                .map(|_| app.default_directory_view)
                .unwrap_or_else(|| app.directory_preference(&path));
            if app
                .tab(tab_id)
                .is_some_and(TabSession::retains_directory_view)
            {
                // The retained snapshot used this request ID with different entry IDs.
                app.icons.retain(|(icon_tab, _, _), _| *icon_tab != tab_id);
            }
            let (location_path, shell_created) = {
                let tab = app.tab_mut(tab_id).expect("accepted tab exists");
                let warning = if source_failures > 0 {
                    Some(format!("library_sources:{source_failures}"))
                } else {
                    (skipped > 0).then(|| skipped.to_string())
                };
                if !tab.finish_directory_navigation(request_id, location, preference, warning) {
                    return DirectoryEventChange {
                        accepted: false,
                        icon_requests,
                    };
                }
                let mut reveal_missing = false;
                if let Some(focus) = focus.as_ref() {
                    let ids = focus
                        .paths
                        .iter()
                        .filter_map(|target| {
                            tab.entries
                                .iter()
                                .find(|entry| entry.path.matches(target))
                                .map(|entry| entry.id)
                        })
                        .collect::<Vec<_>>();
                    if let Some(focused) = ids.last().copied() {
                        tab.selected = ids.into();
                        tab.focused = Some(focused);
                        tab.selection_anchor = Some(focused);
                    } else {
                        reveal_missing = matches!(focus.action, PendingFocusAction::Reveal { .. });
                    }
                }
                let shell_created = shell_create.as_ref().and_then(|pending| {
                    detect_unique_created_path(&pending.baseline, &tab.entries)
                });
                if let Some(target) = shell_created.as_ref()
                    && focus.is_none()
                    && let Some(entry) = tab.entries.iter().find(|entry| entry.path.matches(target))
                {
                    tab.selected = vec![entry.id].into();
                    tab.focused = Some(entry.id);
                    tab.selection_anchor = Some(entry.id);
                }
                if reveal_missing {
                    tab.error = Some(format!(
                        "reveal_target_missing:{}",
                        Texts::new(language).reveal_target_missing()
                    ));
                }
                (tab.visible_path().map(Path::to_path_buf), shell_created)
            };
            if focus
                .as_ref()
                .is_some_and(|focus| focus.action == PendingFocusAction::Select)
            {
                focus_bottom_selected_entry(&mut app, tab_id);
            }
            if consumed_focus {
                app.focus_after_refresh.remove(&tab_id);
            }
            if let Some(focus) = focus
                && let PendingFocusAction::Rename { window_id } = focus.action
                && let Some(entry) = app
                    .tab(tab_id)
                    .and_then(|tab| tab.focused.and_then(|id| tab.visible_entry(id).cloned()))
            {
                let (input, selection_end) = rename_input_for_entry(&entry);
                app.pending_rename_ui.insert(
                    window_id,
                    PendingRenameUi {
                        tab_id,
                        request_id,
                        entry_id: entry.id,
                        input,
                        selection_end,
                    },
                );
            }
            if let Some(pending) = shell_create
                && let Some(target) = shell_created
                && let Some(entry) = app
                    .tab(tab_id)
                    .and_then(|tab| tab.entries.iter().find(|entry| entry.path.matches(&target)))
                    .cloned()
            {
                app.focus_after_refresh.remove(&tab_id);
                let (input, selection_end) = rename_input_for_entry(&entry);
                app.pending_rename_ui.insert(
                    pending.window_id,
                    PendingRenameUi {
                        tab_id,
                        request_id,
                        entry_id: entry.id,
                        input,
                        selection_end,
                    },
                );
            }
            if let Some(location_path) = location_path
                && !crate::network::is_unc_path(&location_path)
            {
                icon_requests.push(IconRequest {
                    tab_id,
                    request_id,
                    target: IconTarget::Location,
                    path: location_path,
                    thumbnail: false,
                    requested_px: 0,
                    type_key: None,
                });
            }
        }
        DirectoryEvent::Cancelled { tab_id, request_id } => {
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.cancel_directory_navigation(request_id)
            {
                app.focus_after_refresh.remove(&tab_id);
            }
        }
        DirectoryEvent::Failed {
            tab_id,
            request_id,
            kind,
            message,
        } => {
            let message = webdav_ui::listing_error_text(app.language, &message);
            if let Some(tab) = app.tab_mut(tab_id)
                && tab.fail_directory_navigation(request_id, classify_error(kind), message)
            {
                app.focus_after_refresh.remove(&tab_id);
            }
        }
        DirectoryEvent::Slow { tab_id, request_id } => {
            if let Some(tab) = app.tab_mut(tab_id) {
                tab.mark_directory_slow(request_id);
            }
        }
        DirectoryEvent::WebDavFinished {
            tab_id,
            request_id,
            connection_id,
            revision,
            root,
            path,
            incomplete,
        } => {
            let location = NavigationLocation::WebDav(crate::domain::WebDavPlace {
                connection_id,
                revision,
                root,
                path,
            });
            let language = app.language;
            let preference = app.default_directory_view;
            let warning = incomplete.map(|detail| webdav_incomplete_text(language, &detail));
            if let Some(tab) = app.tab_mut(tab_id) {
                tab.finish_remote_directory(request_id, location, preference, warning);
            }
        }
    }
    // Release the producer only after the batch was applied or rejected.
    if let Some(acknowledgement) = acknowledgement {
        let _ = acknowledgement.send(());
    }
    DirectoryEventChange {
        accepted: true,
        icon_requests,
    }
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

#[allow(clippy::too_many_arguments)]
fn search_library_grouped_page(
    client: &platform::windows::everything::EverythingClient,
    sources: Vec<PathBuf>,
    recursive: bool,
    query: String,
    sort: platform::windows::everything::EverythingSort,
    offset: u32,
    limit: u32,
    timeout: Duration,
) -> Result<GroupedSearchPage, platform::windows::everything::EverythingError> {
    let requested = offset.saturating_add(limit);
    let mut items = Vec::new();
    let mut total = 0_u32;
    let mut file_total = 0_u32;
    let mut response_offsets_valid = true;
    for source in sources {
        let page = search_grouped_page(
            client,
            (Some(source), recursive),
            query.clone(),
            sort,
            0,
            requested,
            timeout,
        )?;
        total = total.saturating_add(page.total);
        file_total = file_total.saturating_add(page.file_total);
        response_offsets_valid &= page.response_offsets_valid;
        items.extend(page.items);
    }
    items.sort_by(|left, right| {
        left.name
            .to_string_lossy()
            .to_lowercase()
            .cmp(&right.name.to_string_lossy().to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
    });
    let items = items
        .into_iter()
        .skip(offset as usize)
        .take(limit as usize)
        .collect();
    Ok(GroupedSearchPage {
        items,
        total,
        file_total,
        response_offsets_valid,
    })
}
fn everything_status(
    client: Option<&platform::windows::everything::EverythingClient>,
    timeout: Duration,
) -> Result<
    platform::windows::everything::EverythingStatus,
    platform::windows::everything::EverythingError,
> {
    let status = client
        .ok_or(platform::windows::everything::EverythingError::NotConfigured)?
        .status(timeout)?;
    if status.database_loaded {
        Ok(status)
    } else {
        Err(platform::windows::everything::EverythingError::DatabaseNotLoaded)
    }
}

fn wait_for_everything_status(
    client: &platform::windows::everything::EverythingClient,
) -> Result<
    platform::windows::everything::EverythingStatus,
    platform::windows::everything::EverythingError,
> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        match client.status(Duration::from_millis(750)) {
            Ok(status) if status.database_loaded => return Ok(status),
            Ok(_) if Instant::now() >= deadline => {
                return Err(platform::windows::everything::EverythingError::DatabaseNotLoaded);
            }
            Ok(_) => thread::sleep(Duration::from_millis(250)),
            Err(error @ platform::windows::everything::EverythingError::UnsupportedVersion(_))
            | Err(
                error @ platform::windows::everything::EverythingError::UnsupportedArchitecture,
            )
            | Err(error @ platform::windows::everything::EverythingError::InvalidExecutable(_)) => {
                return Err(error);
            }
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(250)),
        }
    }
}
fn spawn_everything_worker(
    config: crate::domain::EverythingConfig,
    state: SharedSessions,
) -> (
    mpsc::Sender<EverythingRequest>,
    mpsc::Receiver<EverythingEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<EverythingRequest>();
    let (event_sender, event_receiver) = mpsc::channel::<EverythingEvent>();
    let (folder_sender, folder_receiver) =
        mpsc::sync_channel::<FolderSizeWork>(FOLDER_SIZE_QUEUE_CAPACITY);
    let folder_events = event_sender.clone();
    let folder_config = config.clone();
    let folder_state = state.clone();
    thread::spawn(move || {
        let mut client = platform_everything_config(&folder_config)
            .and_then(|value| platform::windows::everything::EverythingClient::new(value).ok());
        while let Ok(first) = folder_receiver.recv() {
            let mut batch = vec![first];
            while batch.len() < FOLDER_SIZE_QUEUE_CAPACITY {
                match folder_receiver.try_recv() {
                    Ok(work) => batch.push(work),
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
                }
            }
            batch.sort_by_key(|work| match work {
                FolderSizeWork::Query { tab_id, .. } => {
                    let active = folder_state.lock().ok().is_some_and(|app| {
                        app.window_for_tab(*tab_id) == Some(app.active_window)
                            && app.active_window_state().active_tab == *tab_id
                    });
                    !active
                }
                FolderSizeWork::Configure(_) => false,
            });
            for work in batch {
                match work {
                    FolderSizeWork::Query { tab_id, query } => {
                        let valid = folder_state.lock().ok().is_some_and(|app| {
                            app.tab(tab_id).is_some_and(|tab| {
                                tab.accepts_page(query.request_id, PageSource::Directory)
                                    && tab.folder_sizes.accepts(&query)
                            })
                        });
                        if !valid {
                            continue;
                        }
                        let state =
                            client
                                .as_ref()
                                .map_or(FolderSizeState::Disconnected, |client| {
                                    folder_size_state(
                                        client.folder_size(&query.key.path, Duration::from_secs(2)),
                                    )
                                });
                        let _ = folder_events.send(EverythingEvent::FolderSize {
                            tab_id,
                            query,
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
                    let recursive = depth == SearchDepth::Recursive;
                    let library_sources = match &scope {
                        SearchScope::Library { sources, .. } => Some(sources.clone()),
                        _ => None,
                    };
                    let directory_scope = match scope {
                        SearchScope::Global => None,
                        SearchScope::Directory(path) => Some(path),
                        SearchScope::Library { .. } => None,
                    };
                    if cancel.load(std::sync::atomic::Ordering::Acquire) {
                        let _ = event_sender.send(EverythingEvent::SearchSkipped {
                            tab_id,
                            request_id,
                            offset,
                        });
                        continue;
                    }
                    let page_result = if let Some(sources) = library_sources {
                        search_library_grouped_page(
                            client,
                            sources,
                            recursive,
                            query,
                            sort,
                            offset,
                            SEARCH_PAGE_LIMIT,
                            Duration::from_secs(3),
                        )
                    } else {
                        search_grouped_page(
                            client,
                            (directory_scope, recursive),
                            query,
                            sort,
                            offset,
                            SEARCH_PAGE_LIMIT,
                            Duration::from_secs(3),
                        )
                    };
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
                                .filter(|(_, item)| {
                                    !crate::domain::folder_size_scheduler::is_internal_cleanup_path(
                                        &item.path,
                                    )
                                })
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
                                    path: item.path.into(),
                                    kind: if item.is_directory {
                                        crate::domain::EntryKind::Directory
                                    } else {
                                        crate::domain::EntryKind::File
                                    },
                                    open_target: None,
                                    library_source_index: None,
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
                                    created: item.created,
                                    webdav: None,
                                })
                                .collect::<Vec<_>>();
                            if !cancel.load(std::sync::atomic::Ordering::Acquire) {
                                let response_valid = response_offsets_valid;
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
                EverythingRequest::FolderSize { tab_id, query } => {
                    match folder_sender.try_send(FolderSizeWork::Query {
                        tab_id,
                        query: query.clone(),
                    }) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(_)) => {
                            let _ = event_sender.send(EverythingEvent::FolderSize {
                                tab_id,
                                query,
                                state: FolderSizeState::TimedOut,
                            });
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => {
                            let _ = event_sender.send(EverythingEvent::FolderSize {
                                tab_id,
                                query,
                                state: FolderSizeState::Disconnected,
                            });
                        }
                    }
                }
                EverythingRequest::Configure(config) => {
                    let _ = folder_sender.send(FolderSizeWork::Configure(config.clone()));
                    client = platform_everything_config(&config).and_then(|value| {
                        platform::windows::everything::EverythingClient::new(value).ok()
                    });
                }
                EverythingRequest::Discover(generation) => {
                    let installations = platform::windows::everything::EverythingClient::discover();
                    let usable = |item: &&platform::windows::everything::EverythingInstallation| {
                        item.executable_path.is_file()
                    };
                    let installation = installations
                        .iter()
                        .filter(usable)
                        .find(|item| item.running && item.instance_name == "1.5a")
                        .or_else(|| {
                            installations
                                .iter()
                                .filter(usable)
                                .find(|item| item.running)
                        })
                        .or_else(|| installations.iter().find(|item| usable(item)))
                        .cloned();
                    let Some(installation) = installation else {
                        let _ = event_sender.send(EverythingEvent::Status {
                            generation,
                            result: Err(
                                platform::windows::everything::EverythingError::NotConfigured,
                            ),
                        });
                        continue;
                    };
                    let configured = crate::domain::EverythingConfig {
                        executable_path: (!installation.executable_path.as_os_str().is_empty())
                            .then_some(installation.executable_path),
                        instance_name: installation.instance_name,
                        verified_version: None,
                        allow_launch: true,
                    };
                    client = platform_everything_config(&configured).and_then(|value| {
                        platform::windows::everything::EverythingClient::new(value).ok()
                    });
                    match everything_status(client.as_ref(), Duration::from_secs(2)) {
                        Ok(status) => {
                            let _ =
                                folder_sender.send(FolderSizeWork::Configure(configured.clone()));
                            let _ = event_sender.send(EverythingEvent::Discovered {
                                generation,
                                config: configured,
                                status,
                            });
                        }
                        Err(error) => {
                            let _ = event_sender.send(EverythingEvent::Status {
                                generation,
                                result: Err(error),
                            });
                        }
                    }
                }
                EverythingRequest::TestConnection(generation) => {
                    let result = everything_status(client.as_ref(), Duration::from_secs(2));
                    let _ = event_sender.send(EverythingEvent::Status { generation, result });
                }
                EverythingRequest::Start(generation) => {
                    let result = client
                        .as_ref()
                        .ok_or(platform::windows::everything::EverythingError::NotConfigured)
                        .and_then(|client| {
                            client.start()?;
                            wait_for_everything_status(client)
                        });
                    let _ = event_sender.send(EverythingEvent::Status { generation, result });
                }
                EverythingRequest::PickExecutable {
                    generation,
                    owner_window,
                } => {
                    let picker_events = event_sender.clone();
                    thread::spawn(move || {
                        let result =
                            platform::windows::everything_file_picker::pick_everything_executable(
                                owner_window,
                            );
                        let _ = picker_events
                            .send(EverythingEvent::ExecutablePicked { generation, result });
                    });
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
fn with_folder_scheduler<T>(
    tab: &mut TabSession,
    action: impl FnOnce(
        &mut crate::domain::folder_size_scheduler::FolderSizeScheduler,
        &mut Vec<FileEntry>,
    ) -> T,
) -> T {
    let mut scheduler = std::mem::take(&mut tab.folder_sizes);
    let result = action(&mut scheduler, Arc::make_mut(&mut tab.entries));
    tab.folder_sizes = scheduler;
    result
}

fn cancel_folder_sizes(tab: &mut TabSession) {
    tab.folder_sizes.cancel(tab.latest_request());
}

fn start_folder_size_query(app: &mut AppState, tab_id: TabId, query: &FolderSizeQuery) -> bool {
    let Some(tab) = app.tab_mut(tab_id) else {
        return false;
    };
    tab.accepts_page(query.request_id, PageSource::Directory) && tab.folder_sizes.start(query)
}

fn apply_folder_size_event(
    app: &mut AppState,
    tab_id: TabId,
    query: &FolderSizeQuery,
    state: FolderSizeState,
) -> FolderSizeCommit {
    let Some(tab) = app.tab_mut(tab_id) else {
        return FolderSizeCommit::Ignored;
    };
    if !tab.accepts_page(query.request_id, PageSource::Directory) {
        return FolderSizeCommit::Ignored;
    }
    with_folder_scheduler(tab, |scheduler, entries| {
        scheduler.complete(query, state, entries)
    })
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
enum FolderSizeProjection {
    Rows(HashSet<EntryId>),
    Resorted,
}

fn folder_size_projection(
    app: &mut AppState,
    tab_id: TabId,
    commit: FolderSizeCommit,
) -> Option<FolderSizeProjection> {
    match commit {
        FolderSizeCommit::Visible(entry_id) => {
            Some(FolderSizeProjection::Rows(HashSet::from([entry_id])))
        }
        FolderSizeCommit::CompleteSort => {
            app.tab_mut(tab_id)?.resort_entries();
            Some(FolderSizeProjection::Resorted)
        }
        FolderSizeCommit::Staged => Some(FolderSizeProjection::Rows(HashSet::new())),
        FolderSizeCommit::Ignored => None,
    }
}

/// Returns the projected request when rows moved, so newly visible shortcuts can be resolved.
fn present_folder_size_projection(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    projection: FolderSizeProjection,
    images: &ThumbnailScheduler,
) -> Option<RequestId> {
    let moved = match projection {
        FolderSizeProjection::Rows(changed) => {
            update_file_rows(ui, state, tab_id, &changed);
            None
        }
        FolderSizeProjection::Resorted => reproject_resorted_rows(ui, state, tab_id, images),
    };
    update_tab_status(ui, state, tab_id);
    moved
}

/// A completed size sort reorders entries, so rows are re-projected by position. Existing
/// models are patched in place so delegates, scroll position and pointer gestures survive.
fn reproject_resorted_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    images: &ThumbnailScheduler,
) -> Option<RequestId> {
    let (window_id, request_id) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        let window_id = app.window_for_tab(tab_id)?;
        let window = app.window(window_id)?;
        let tab = window.tabs.get(&tab_id)?;
        let request_id = tab.latest_request();
        (window.active_tab == tab_id
            && tab.page_source == PageSource::Directory
            && ui.get_projected_file_tab_id() == tab_id.0 as i32
            && ui.get_projected_file_request_id() == request_id.0 as i32)
            .then_some((window_id, request_id))?
    };
    rename_updates::refresh_projection(ui, state, window_id, images);
    Some(request_id)
}

fn start_everything_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<EverythingEvent>,
    search_sender: mpsc::Sender<EverythingRequest>,
    images: ThumbnailScheduler,
    shortcuts: mpsc::SyncSender<ShortcutRequest>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            let shortcuts = shortcuts.clone();
            let sender_for_search_consistency = search_sender.clone();
            let images = images.clone();
            if weak.upgrade_in_event_loop(move |ui| {
            let routed_tab = match &event {
                EverythingEvent::SearchPage { tab_id, .. }
                | EverythingEvent::SearchFailed { tab_id, .. }
                | EverythingEvent::SearchSkipped { tab_id, .. } => Some(*tab_id),
                EverythingEvent::FolderSize { .. }
                | EverythingEvent::Status { .. }
                | EverythingEvent::Discovered { .. }
                | EverythingEvent::ExecutablePicked { .. } => None,
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
                    tab.search_state = match error {
                        platform::windows::everything::EverythingError::NotConfigured
                        | platform::windows::everything::EverythingError::InvalidExecutable(_) => SearchState::NotConfigured,
                        platform::windows::everything::EverythingError::NotRunning(_) => SearchState::Disconnected,
                        platform::windows::everything::EverythingError::UnsupportedVersion(_) => SearchState::UnsupportedVersion,
                        platform::windows::everything::EverythingError::UnsupportedArchitecture => SearchState::UnsupportedArchitecture,
                        platform::windows::everything::EverythingError::DatabaseNotLoaded => SearchState::NotIndexed,
                        platform::windows::everything::EverythingError::QueryRejected => SearchState::SyntaxError,
                        platform::windows::everything::EverythingError::Timeout => SearchState::TimedOut,
                        _ => SearchState::Failed,
                    };
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
                EverythingEvent::FolderSize { tab_id, query, state: size } => {
                    let commit = apply_folder_size_event(&mut app, tab_id, &query, size);
                    let next = app
                        .tab_mut(tab_id)
                        .map(|tab| {
                            match commit {
                                FolderSizeCommit::Staged => tab.folder_sizes.next_complete_queries(),
                                FolderSizeCommit::Visible(_) => tab.folder_sizes.next_visible_queries(&tab.entries),
                                FolderSizeCommit::Ignored | FolderSizeCommit::CompleteSort => Vec::new(),
                            }
                        })
                        .unwrap_or_default();
                    let querying = next.iter().map(|query| query.key.entry_id).collect::<HashSet<_>>();
                    for query in next {
                        if start_folder_size_query(&mut app, tab_id, &query) {
                            let _ = sender_for_search_consistency.send(
                                EverythingRequest::FolderSize { tab_id, query }
                            );
                        }
                    }
                    folder_size_update = folder_size_projection(&mut app, tab_id, commit)
                        .map(|mut projection| {
                            if let FolderSizeProjection::Rows(ids) = &mut projection { ids.extend(querying); }
                            (tab_id, projection)
                        });
                }
                EverythingEvent::Status { generation, result }
                    if generation == app.everything.generation =>
                {
                    app.everything.busy = false;
                    match result {
                        Ok(status) => {
                            app.everything.status =
                                everything_connected_status(app.language, &status);
                            app.everything.folder_sizes_indexed = Some(status.folder_size_indexed);
                            app.everything.config.verified_version =
                                Some(status.version.to_string());
                        }
                        Err(error) => {
                            app.everything.status = everything_error_text(app.language, &error);
                            app.everything.folder_sizes_indexed = None;
                        }
                    }
                }
                EverythingEvent::Discovered { generation, config, status }
                    if generation == app.everything.generation =>
                {
                    app.everything.busy = false;
                    apply_everything_discovery(&mut app, generation, config, &status);
                }
                EverythingEvent::ExecutablePicked { generation, result }
                    if generation == app.everything.generation =>
                {
                    app.everything.busy = false;
                    match result {
                        Ok(Some(path)) => {
                            app.everything.config.executable_path = Some(path);
                            app.everything.config.verified_version = None;
                            app.everything.generation = app.everything.generation.saturating_add(1);
                            app.everything.status.clear();
                            app.everything.folder_sizes_indexed = None;
                            let _ = sender_for_search_consistency.send(EverythingRequest::Configure(
                                app.everything.config.clone(),
                            ));
                        }
                        Ok(None) => {}
                        Err(_) => {
                            app.everything.status = everything_picker_error_text(app.language).to_owned();
                        }
                    }
                }
                EverythingEvent::Status { .. }
                | EverythingEvent::Discovered { .. }
                | EverythingEvent::ExecutablePicked { .. } => {}
            }
            drop(app);
            if let Some((tab_id, projection)) = folder_size_update {
                if let Some(window_id) = state.lock().ok().and_then(|app| app.window_for_tab(tab_id))
                    && let Some(target_ui) = window_ui(window_id)
                    && let Some(request_id) =
                        present_folder_size_projection(&target_ui, &state, tab_id, projection, &images)
                {
                    submit_visible_shortcuts(&shortcuts, &state, &target_ui, tab_id, request_id);
                }
                return;
            }
            if let Some(tab_id) = routed_tab {
                refresh_tab_window(&state, tab_id);
                defer_visible_file_images(state.clone(), tab_id, images);
            } else {
                refresh_all_windows(&state, RefreshRegions::SETTINGS);
            }
        }).is_err() { break; }
        }
    });
}

fn empty_file_row() -> FileRow {
    FileRow {
        id: 0,
        loaded: false,
        group_header: false,
        group_label: "".into(),
        group_detail: "".into(),
        group_count: 0,
        name: "".into(),
        name_segments: ModelRc::new(VecModel::default()),
        kind: "".into(),
        parent_path: "".into(),
        size: "".into(),
        modified: "".into(),
        created: "".into(),
        is_directory: false,
        selected: false,
        focused: false,
        cut: false,
        thumbnail_state: 0,
        icon: Image::default(),
    }
}

fn group_header_file_row(label: &str, detail: &str, entry_count: usize) -> FileRow {
    let mut row = empty_file_row();
    row.loaded = true;
    row.group_header = true;
    row.group_label = label.into();
    row.group_detail = detail.into();
    row.group_count = entry_count.min(i32::MAX as usize) as i32;
    row
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
                tab.latest_request() == request_id && tab.page_source == PageSource::Search
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

fn sort_field_ui_value(field: SortField) -> i32 {
    match field {
        SortField::Name => 0,
        SortField::Kind => 1,
        SortField::Size => 2,
        SortField::Modified => 3,
        SortField::Created => 4,
    }
}

fn everything_sort(
    field: SortField,
    direction: SortDirection,
) -> platform::windows::everything::EverythingSort {
    use platform::windows::everything::EverythingSort;
    match (field, direction) {
        (SortField::Name, SortDirection::Ascending) => EverythingSort::NameAscending,
        (SortField::Name, SortDirection::Descending) => EverythingSort::NameDescending,
        (SortField::Kind, SortDirection::Ascending) => EverythingSort::PathAscending,
        (SortField::Kind, SortDirection::Descending) => EverythingSort::PathDescending,
        (SortField::Size, SortDirection::Ascending) => EverythingSort::SizeAscending,
        (SortField::Size, SortDirection::Descending) => EverythingSort::SizeDescending,
        (SortField::Modified, SortDirection::Ascending) => EverythingSort::ModifiedAscending,
        (SortField::Modified, SortDirection::Descending) => EverythingSort::ModifiedDescending,
        (SortField::Created, SortDirection::Ascending) => EverythingSort::CreatedAscending,
        (SortField::Created, SortDirection::Descending) => EverythingSort::CreatedDescending,
    }
}
fn schedule_search(
    sender: mpsc::Sender<EverythingRequest>,
    state: WindowSessions,
    weak: slint::Weak<AppWindow>,
    tab_id: TabId,
    input: String,
) {
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(160));
        let query = state.lock().ok().and_then(|app| {
            let window = app.window(state.window_id)?;
            (window.active_tab == tab_id).then_some(())?;
            let tab = window.tabs.get(&tab_id)?;
            (tab.address_editing
                && tab.address_input == input
                && tab.address_mode == AddressMode::Smart)
                .then(|| tab.search_query.clone())
        });
        if let Some(query) = query {
            let _ = weak.upgrade_in_event_loop(move |ui| {
                submit_search(&sender, &state, Some(&ui), tab_id, query);
            });
        }
    });
}

fn begin_file_list_quick_search(state: &WindowSessions, typed: char) -> Option<(TabId, String)> {
    let mut app = state.lock().ok()?;
    if !app.file_list_quick_search {
        return None;
    }
    let tab_id = app.active_window_state().active_tab;
    let tab = app.active_window_state_mut().tabs.get_mut(&tab_id)?;
    if tab.kind != TabKind::Files {
        return None;
    }
    tab.begin_smart_address_edit();
    tab.search_depth = SearchDepth::CurrentFolder;
    let input = format!("{}{}", tab.address_input, typed);
    tab.update_address_input(input.clone());
    Some((tab_id, input))
}
fn submit_search(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    ui: Option<&AppWindow>,
    tab_id: TabId,
    query: String,
) {
    if state.lock().ok().is_some_and(|app| {
        app.tab(tab_id)
            .and_then(TabSession::visible_location)
            .is_some_and(|location| matches!(location, NavigationLocation::WebDav(_)))
    }) {
        return;
    }
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.cancel_column_drag();
        let library_sources = app
            .tab(tab_id)
            .and_then(TabSession::visible_location)
            .and_then(|location| match location {
                NavigationLocation::Home => None,
                NavigationLocation::Library(library) => app
                    .libraries
                    .iter()
                    .find(|candidate| candidate.id.as_os_str() == library.identity.as_os_str())
                    .map(|definition| {
                        definition
                            .sources
                            .iter()
                            .filter_map(|source| source.path.clone())
                            .collect::<Vec<_>>()
                    }),
                NavigationLocation::Directory(_) | NavigationLocation::WebDav(_) => None,
            });
        let Some(tab) = app.tab_mut(tab_id) else {
            return;
        };
        let mut scope = tab.search_scope.clone();
        if let (SearchScope::Library { sources, .. }, Some(library_sources)) =
            (&mut scope, library_sources)
        {
            *sources = library_sources;
        }
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
            request_id: tab.latest_request(),
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
fn submit_folder_size_queries(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    queries: Vec<FolderSizeQuery>,
) {
    for query in queries {
        let started = state
            .lock()
            .ok()
            .is_some_and(|mut app| start_folder_size_query(&mut app, tab_id, &query));
        if !started {
            continue;
        }
        if sender
            .send(EverythingRequest::FolderSize {
                tab_id,
                query: query.clone(),
            })
            .is_err()
            && let Ok(mut app) = state.lock()
            && let Some(tab) = app.tab_mut(tab_id)
        {
            tab.folder_sizes.reject(&query);
        }
    }
}

fn submit_visible_folder_sizes(
    sender: &mpsc::Sender<EverythingRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
    ui: &AppWindow,
) {
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != request_id.0 as i32
    {
        return;
    }
    let queries = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if app.everything.folder_sizes_indexed == Some(false) {
            return;
        }
        let Some(tab) = app.tab(tab_id) else {
            return;
        };
        if tab.latest_request() != request_id {
            return;
        }
        let indices = visible_shortcut_entry_ids(ui, &app, tab)
            .into_iter()
            .filter_map(|id| tab.visible_entry_index(id))
            .collect::<Vec<_>>();
        let complete_sort = tab.sort_field == SortField::Size;
        let has_unknown = if complete_sort {
            tab.entries.iter().any(|entry| {
                entry.kind == crate::domain::EntryKind::Directory
                    && entry.folder_size == FolderSizeState::Unknown
            })
        } else {
            indices
                .iter()
                .filter_map(|index| tab.entries.get(*index))
                .any(|entry| {
                    entry.kind == crate::domain::EntryKind::Directory
                        && entry.folder_size == FolderSizeState::Unknown
                })
        };
        let tab = app.tab_mut(tab_id).unwrap();
        if !has_unknown {
            tab.folder_sizes.clear_visible_range();
            return;
        }
        if complete_sort {
            tab.folder_sizes
                .begin_complete_sort(request_id, &tab.entries)
        } else {
            tab.folder_sizes
                .visible_entry_queries(request_id, &tab.entries, &indices)
        }
    };
    let changed = queries
        .iter()
        .map(|query| query.key.entry_id)
        .collect::<HashSet<_>>();
    submit_folder_size_queries(sender, state, tab_id, queries);
    update_file_rows(ui, state, tab_id, &changed);
}

fn visible_shortcut_entry_ids(ui: &AppWindow, app: &AppState, tab: &TabSession) -> Vec<EntryId> {
    if ui.get_projected_file_tab_id() != tab.id.0 as i32
        || ui.get_projected_file_request_id() != tab.latest_request().0 as i32
    {
        return Vec::new();
    }

    let top = (-ui.get_file_viewport_y()).max(0.0);
    let height = ui
        .get_file_viewport_height()
        .max(file_row_height(projected_view_mode(ui)));
    let grid = ui.get_grid_rows();
    if let Some(grid) = grid
        .as_any()
        .downcast_ref::<directory_model::DirectoryGrid>()
    {
        return grid
            .rows_between((top - height).max(0.0), top + height * 2.0)
            .flat_map(|row| grid.entry_ids(row))
            .collect();
    }
    if let Some(rows) = directory_rows(ui) {
        return rows
            .rows_between((top - height).max(0.0), top + height * 2.0)
            .filter_map(|row| rows.entry_id(row))
            .collect();
    }
    let entries = tab.visible_entries();
    let view_mode = app
        .view_mode_for_tab(tab.id)
        .unwrap_or(app.default_directory_view.view_mode);
    if matches!(tab.load_state, LoadState::Loading | LoadState::Partial)
        && !tab.retains_directory_view()
    {
        let columns = if view_mode.uses_grid_layout() {
            ui.get_grid_column_count().max(1) as usize
        } else {
            1
        };
        let visible_rows = (ui
            .get_file_viewport_height()
            .max(file_row_height(view_mode))
            / file_row_height(view_mode))
        .ceil() as usize;
        let start = (((-ui.get_file_viewport_y()).max(0.0) / file_row_height(view_mode)).floor()
            as usize
            * columns)
            .min(entries.len());
        let prefetch = visible_rows.saturating_mul(columns).max(1);
        return entries[start.saturating_sub(prefetch)..entries.len().min(start + prefetch * 2)]
            .iter()
            .map(|entry| entry.id)
            .collect();
    }
    let row_height = file_row_height(view_mode);
    let viewport_top = (-ui.get_file_viewport_y()).max(0.0);
    let viewport_height = ui.get_file_viewport_height().max(row_height);
    let files = ui.get_files();
    if !view_mode.uses_grid_layout()
        && ui.get_projected_file_tab_id() == tab.id.0 as i32
        && ui.get_projected_file_request_id() == tab.latest_request().0 as i32
        && let Some(model) = files.as_any().downcast_ref::<VecModel<FileRow>>()
        && let Some(range) = file_row_index::rows_between(
            model,
            row_height,
            (viewport_top - viewport_height).max(0.0),
            viewport_top + viewport_height * 2.0,
        )
    {
        // The shown rows already carry the grouped order; no projection rebuild per scroll.
        return range
            .filter_map(|index| model.row_data(index))
            .filter(|row| !row.group_header && row.id > 0)
            .map(|row| EntryId(row.id as u32))
            .collect();
    }
    let groups = directory_group_projections(app, tab, tab.visible_entries());
    let request_top = (viewport_top - viewport_height).max(0.0) as u64;
    let request_bottom = (viewport_top + viewport_height * 2.0) as u64;
    let mut ids = Vec::new();
    if view_mode.uses_grid_layout() {
        let projection = IconProjection::from_groups(
            &groups,
            ui.get_grid_column_count().max(1) as usize,
            group_header_height(&groups),
            row_height as u64,
        );
        for (index, row) in projection.rows.iter().enumerate() {
            let Some(top) = projection.offsets.row_start(index) else {
                continue;
            };
            if top > request_bottom {
                break;
            }
            if top.saturating_add(row_height as u64) < request_top {
                continue;
            }
            if let IconVisualRow::Entries { entries, .. } = row {
                ids.extend(entries.iter().copied());
            }
        }
    } else {
        let projection =
            ListProjection::from_groups(&groups, group_header_height(&groups), row_height as u64);
        for (index, row) in projection.rows.iter().enumerate() {
            let Some(top) = projection.offsets.row_start(index) else {
                continue;
            };
            if top > request_bottom {
                break;
            }
            if top.saturating_add(row_height as u64) < request_top {
                continue;
            }
            if let ListVisualRow::Entry { entry_id } = row {
                ids.push(*entry_id);
            }
        }
    }
    ids
}

fn submit_visible_shortcuts(
    sender: &mpsc::SyncSender<ShortcutRequest>,
    state: &SharedSessions,
    ui: &AppWindow,
    tab_id: TabId,
    request_id: RequestId,
) {
    let requests = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        if !app.tab(tab_id).is_some_and(|tab| {
            tab.latest_request() == request_id && tab.page_source == PageSource::Directory
        }) {
            return;
        }
        app.shortcut_requests
            .retain(|request| request.tab_id != tab_id || request.request_id == request_id);
        app.shortcut_completed
            .retain(|request| request.tab_id != tab_id || request.request_id == request_id);
        let tab = app.tab(tab_id).expect("validated tab exists");
        let ids = visible_shortcut_entry_ids(ui, &app, tab);
        let candidates = ids
            .into_iter()
            .filter_map(|entry_id| {
                let entry = tab.visible_entry(entry_id)?;
                (entry.open_target.is_none()
                    && entry
                        .path
                        .extension()
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk")))
                .then(|| ShortcutRequest {
                    tab_id,
                    request_id,
                    entry_id,
                    path: entry.path(),
                })
            })
            .filter(|request| {
                !app.shortcut_requests.contains(request)
                    && !app.shortcut_completed.contains(request)
            })
            .collect::<Vec<_>>();
        for request in &candidates {
            app.shortcut_requests.insert(request.clone());
        }
        candidates
    };
    for request in requests {
        if sender.try_send(request.clone()).is_err()
            && let Ok(mut app) = state.lock()
        {
            app.shortcut_requests.remove(&request);
        }
    }
}

fn spawn_shortcut_worker(
    state: SharedSessions,
) -> (
    mpsc::SyncSender<ShortcutRequest>,
    mpsc::Receiver<ShortcutEvent>,
) {
    let (request_sender, request_receiver) =
        mpsc::sync_channel::<ShortcutRequest>(SHORTCUT_QUEUE_CAPACITY);
    let (event_sender, event_receiver) = mpsc::channel::<ShortcutEvent>();
    thread::spawn(move || {
        platform::set_current_thread_low_priority();
        while let Ok(request) = request_receiver.recv() {
            let current = state.lock().ok().is_some_and(|app| {
                app.tab(request.tab_id).is_some_and(|tab| {
                    tab.latest_request() == request.request_id
                        && tab
                            .visible_entry(request.entry_id)
                            .is_some_and(|entry| entry.path.matches(&request.path))
                })
            });
            if !current {
                if let Ok(mut app) = state.lock() {
                    app.shortcut_requests.remove(&request);
                    app.shortcut_completed.insert(request);
                }
                continue;
            }
            match platform::resolve_shortcut_target(&request.path) {
                Ok(Some(target)) if target.is_directory != Some(false) => {
                    if event_sender
                        .send(ShortcutEvent { request, target })
                        .is_err()
                    {
                        break;
                    }
                }
                _ => {
                    if let Ok(mut app) = state.lock() {
                        app.shortcut_requests.remove(&request);
                        app.shortcut_completed.insert(request);
                    }
                }
            }
        }
    });
    (request_sender, event_receiver)
}

fn apply_shortcut_event(state: &SharedSessions, event: ShortcutEvent) -> Option<(TabId, EntryId)> {
    let mut app = state.lock().ok()?;
    app.shortcut_requests.remove(&event.request);
    app.shortcut_completed.insert(event.request.clone());
    let tab = app.tab_mut(event.request.tab_id)?;
    if !tab.accepts(event.request.request_id) || tab.page_source != PageSource::Directory {
        return None;
    }
    let was_partial = matches!(tab.load_state, LoadState::Loading | LoadState::Partial)
        && !tab.retains_directory_view();
    let changed = tab.apply_shortcut_target(
        event.request.entry_id,
        &event.request.path,
        event.target.path,
        event.target.is_directory,
    );
    if changed {
        if was_partial {
            tab.sort_pending();
        } else {
            tab.resort_entries();
        }
    }
    changed.then_some((event.request.tab_id, event.request.entry_id))
}

fn start_shortcut_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<ShortcutEvent>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    let target_ui = state
                        .lock()
                        .ok()
                        .and_then(|app| app.window_for_tab(event.request.tab_id))
                        .and_then(window_ui);
                    let visible_selection = target_ui.as_ref().and_then(|target_ui| {
                        state.lock().ok().and_then(|app| {
                            visible_selection_before_reorder(
                                target_ui,
                                &app,
                                event.request.tab_id,
                                event.request.request_id,
                            )
                        })
                    });
                    if let Some((tab_id, entry_id)) = apply_shortcut_event(&state, event) {
                        if let Some(target_ui) = target_ui {
                            refresh_tab_window(&state, tab_id);
                            restore_visible_selection_after_reorder(
                                &target_ui,
                                &state,
                                visible_selection,
                            );
                        } else {
                            update_file_rows(&ui, &state, tab_id, &HashSet::from([entry_id]));
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
fn spawn_icon_workers(
    worker_count: usize,
    state: SharedSessions,
) -> (
    mpsc::Sender<IconRequest>,
    ThumbnailScheduler,
    mpsc::Receiver<IconEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<IconRequest>();
    let request_receiver = Arc::new(Mutex::new(request_receiver));
    let thumbnail_scheduler = ThumbnailScheduler::new(request_sender.clone());
    let (event_sender, event_receiver) = mpsc::sync_channel::<IconEvent>(THUMBNAIL_QUEUE_CAPACITY);
    for _ in 0..worker_count {
        let thumbnails = thumbnail_scheduler.clone();
        let events = event_sender.clone();
        let state = state.clone();
        thread::spawn(move || {
            let _shell_apartment = platform::windows::shell_icons::initialize_shell_worker().ok();
            loop {
                let (generation, key) = thumbnails.recv();
                let request = IconRequest {
                    tab_id: key.tab_id,
                    request_id: key.request_id,
                    target: IconTarget::Entry(key.entry_id),
                    path: key.path.clone(),
                    thumbnail: true,
                    requested_px: key.requested_px,
                    type_key: None,
                };
                run_icon_request(
                    request,
                    Some((generation, key)),
                    &state,
                    &events,
                    &thumbnails,
                );
            }
        });
    }
    {
        let requests = request_receiver;
        let thumbnails = thumbnail_scheduler.clone();
        let events = event_sender;
        let state = state.clone();
        thread::spawn(move || {
            let _shell_apartment = platform::windows::shell_icons::initialize_shell_worker().ok();
            loop {
                let request = requests
                    .lock()
                    .expect("icon request receiver mutex is not poisoned")
                    .recv();
                let Ok(request) = request else { break };
                run_icon_request(request, None, &state, &events, &thumbnails);
            }
        });
    }
    (request_sender, thumbnail_scheduler, event_receiver)
}

fn run_icon_request(
    request: IconRequest,
    thumbnail_plan: Option<(u64, ThumbnailKey)>,
    state: &SharedSessions,
    events: &mpsc::SyncSender<IconEvent>,
    thumbnails: &ThumbnailScheduler,
) {
    let planned_current = thumbnail_plan
        .as_ref()
        .is_none_or(|(_, key)| thumbnails.accepts(key));
    let is_current = request.type_key.is_some()
        || state.lock().ok().is_some_and(|app| {
            app.tab(request.tab_id)
                .is_some_and(|tab| tab.latest_request() == request.request_id)
        }) && planned_current;
    if !is_current {
        if !request.thumbnail
            && let Ok(mut app) = state.lock()
        {
            app.ordinary_icon_requests.remove(&(
                request.tab_id,
                request.request_id,
                request.path.clone(),
            ));
        }
        if let Some((_, key)) = thumbnail_plan {
            thumbnails.finish(&key);
        }
        return;
    }
    let cached = state.lock().ok().and_then(|app| {
        if request.thumbnail {
            let key = (request.path.clone(), request.requested_px);
            app.image_cache
                .thumbnails
                .get(&key)
                .map(|image| (image.clone(), true))
                .or_else(|| {
                    app.image_cache
                        .grid_icons
                        .get(&key)
                        .map(|image| (image.clone(), false))
                })
        } else if let Some(key) = request.type_key.as_ref() {
            app.image_cache
                .types
                .get(&(key.clone(), request.requested_px))
                .map(|image| (image.clone(), false))
        } else {
            app.image_cache
                .ordinary
                .get(&request.path)
                .map(|image| (image.clone(), false))
        }
    });
    let (icon, actual_thumbnail) = if request.thumbnail {
        if let Some((cached, actual)) = cached {
            (Some(cached), actual)
        } else {
            match platform::windows::shell_icons::shell_grid_image_rgba(
                &request.path,
                request.requested_px,
            ) {
                Ok(result) => {
                    crate::operation_audit::record(
                        "grid_image_extracted",
                        format!(
                            "tab={:?} request={:?} path={:?} requested_px={} returned_px={}x{} source={:?} thumbnail_error={:?}",
                            request.tab_id,
                            request.request_id,
                            request.path,
                            request.requested_px,
                            result.image.width,
                            result.image.height,
                            result.source,
                            result.thumbnail_error,
                        ),
                    );
                    let actual = matches!(
                        result.source,
                        platform::windows::shell_icons::GridImageSource::Thumbnail(_)
                    );
                    (Some(result.image), actual)
                }
                Err(error) => {
                    crate::operation_audit::record(
                        "grid_image_failed",
                        format!(
                            "tab={:?} request={:?} path={:?} requested_px={} error={error}",
                            request.tab_id, request.request_id, request.path, request.requested_px,
                        ),
                    );
                    (None, false)
                }
            }
        }
    } else if let Some(key) = request.type_key.as_ref() {
        (
            file_images::load_shared_icon(
                state,
                key,
                request.requested_px,
                cached.map(|(image, _)| image),
            ),
            false,
        )
    } else {
        (
            cached
                .map(|(image, _)| image)
                .or_else(|| platform::windows::shell_icons::shell_icon_rgba(&request.path).ok()),
            false,
        )
    };
    let thumbnail_failed = request.thumbnail && icon.is_none();
    if let Some((_, key)) = thumbnail_plan.as_ref()
        && !thumbnails.accepts(key)
    {
        thumbnails.finish(key);
        return;
    }
    if let Some(icon) = icon {
        let _ = events.send(IconEvent {
            tab_id: request.tab_id,
            request_id: request.request_id,
            target: request.target,
            path: request.path,
            icon,
            actual_thumbnail,
            requested_px: request.requested_px,
            type_key: request.type_key,
            thumbnail_failed: false,
            thumbnail_plan: thumbnail_plan.clone(),
        });
    } else if request.thumbnail {
        let _ = events.send(IconEvent {
            tab_id: request.tab_id,
            request_id: request.request_id,
            target: request.target,
            path: request.path,
            icon: platform::windows::shell_icons::ShellIconRgba {
                width: 0,
                height: 0,
                pixels: Vec::new().into(),
            },
            actual_thumbnail: false,
            requested_px: request.requested_px,
            type_key: None,
            thumbnail_failed,
            thumbnail_plan: thumbnail_plan.clone(),
        });
    } else if let Ok(mut app) = state.lock() {
        if let Some(key) = request.type_key {
            app.type_icon_requests.remove(&(key, request.requested_px));
        }
        app.ordinary_icon_requests
            .remove(&(request.tab_id, request.request_id, request.path));
    }
    if let Some((_, key)) = thumbnail_plan {
        thumbnails.finish(&key);
    }
}

fn drive_projection(
    language: Language,
    capacity: Option<&DriveCapacityState>,
) -> (f32, i32, String) {
    let texts = Texts::new(language);
    match capacity {
        Some(capacity) if capacity.status == DriveCapacityStatus::Unavailable => {
            (0.0, 3, texts.drive_usage_unavailable().to_owned())
        }
        Some(capacity) if capacity.status == DriveCapacityStatus::Ready => {
            let progress =
                platform::windows::drives::used_ratio(capacity.total, capacity.available);
            let warning =
                platform::windows::drives::is_low_space(capacity.total, capacity.available);
            (
                progress,
                if warning { 2 } else { 1 },
                texts.drive_usage_status(warning).to_owned(),
            )
        }
        None | Some(_) => (0.0, 0, texts.drive_usage_loading().to_owned()),
    }
}

fn start_sidebar_icon_loader(ui: &AppWindow, state: SharedSessions) {
    let locations = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.sidebar
            .iter()
            .map(|location| location.path.clone())
            .collect::<Vec<_>>()
    };
    let weak = ui.as_weak();
    thread::spawn(move || {
        let _shell_apartment = platform::windows::shell_icons::initialize_shell_worker().ok();
        for path in locations {
            if crate::network::is_unc_path(&path) {
                continue;
            }
            let Ok(icon) = platform::windows::shell_icons::shell_icon_rgba(&path) else {
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
                    refresh_all_windows(&state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn start_drive_capacity_timer(state: SharedSessions) -> slint::Timer {
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_secs(15),
        move || begin_drive_capacity_refresh(&state, None, Duration::from_secs(60)),
    );
    timer
}
fn start_sidebar_loader(ui: &AppWindow, state: SharedSessions) {
    reload_quick_access(ui.as_weak(), state.clone());
    reload_drives(ui.as_weak(), state);
}

fn home_capacity_detail(language: Language, capacity: Option<&DriveCapacityState>) -> String {
    let texts = Texts::new(language);
    match capacity {
        Some(capacity) if capacity.status == DriveCapacityStatus::Ready => match language {
            Language::Chinese => format!(
                "{} 可用，共 {}",
                texts.size(Some(capacity.available)),
                texts.size(Some(capacity.total))
            ),
            Language::English => format!(
                "{} free of {}",
                texts.size(Some(capacity.available)),
                texts.size(Some(capacity.total))
            ),
        },
        Some(capacity) if capacity.status == DriveCapacityStatus::Unavailable => {
            texts.drive_usage_unavailable().to_owned()
        }
        None | Some(_) => texts.drive_usage_loading().to_owned(),
    }
}

fn home_item_rows(
    app: &AppState,
    tab_id: TabId,
    locations: &[KnownLocation],
    source_kind: i32,
    columns: usize,
) -> Vec<HomeCardRow> {
    let selected = app
        .home_pages
        .get(&tab_id)
        .and_then(|page| page.selected.as_deref());
    locations
        .iter()
        .enumerate()
        .map(|(index, location)| {
            let stable_id = format!("home:{source_kind}:{}", index);
            let (progress, state, _) = location
                .drive_root
                .as_deref()
                .map_or((0.0, -1, String::new()), |root| {
                    drive_projection(app.language, app.drive_capacity.get(root))
                });
            HomeItemRow {
                stable_id: stable_id.clone().into(),
                label: location.label.clone().into(),
                detail: location
                    .drive_root
                    .as_deref()
                    .map_or_else(
                        || display_path(&location.path),
                        |_| {
                            home_capacity_detail(
                                app.language,
                                location
                                    .drive_root
                                    .as_deref()
                                    .and_then(|root| app.drive_capacity.get(root)),
                            )
                        },
                    )
                    .into(),
                icon: app
                    .sidebar_icons
                    .get(&location.path)
                    .map(shell_icon_image)
                    .unwrap_or_default(),
                progress,
                state,
                status: "".into(),
                selected: selected == Some(stable_id.as_str()),
                focused: selected == Some(stable_id.as_str()),
                source_kind,
                menu_group: if source_kind == 0 { 0 } else { 1 },
                menu_stable_id: if source_kind == 2 {
                    "".into()
                } else {
                    let sidebar_index = app.sidebar.iter().position(|candidate| {
                        candidate.path == location.path && candidate.kind == location.kind
                    });
                    sidebar_index
                        .map_or_else(|| "".into(), |index| format!("location:{index}").into())
                },
            }
        })
        .collect::<Vec<_>>()
        .chunks(columns.max(1))
        .map(|entries| HomeCardRow {
            entries: ModelRc::new(VecModel::from(entries.to_vec())),
        })
        .collect()
}

fn home_content_width_for_window(window_width: f32) -> f32 {
    let horizontal_padding = if window_width < WINDOW_MINIMAL_WIDTH {
        36.0
    } else {
        60.0
    };
    (window_width - navigation_sidebar_width(window_width) - horizontal_padding).max(264.0)
}

fn sorted_network_locations(app: &AppState) -> Vec<&NetworkLocation> {
    let mut locations = app
        .imported_network_locations
        .iter()
        .chain(app.network_locations.iter())
        .collect::<Vec<_>>();
    locations.sort_by_key(|location| location.sort_order);
    locations
}

fn home_network_rows(app: &AppState, tab_id: TabId, columns: usize) -> Vec<HomeCardRow> {
    let selected = app
        .home_pages
        .get(&tab_id)
        .and_then(|page| page.selected.as_deref());
    sorted_network_locations(app)
        .into_iter()
        .map(|location| {
            let stable_id = format!("home:2:{}", location.id);
            let (detail, icon) = match &location.target {
                NetworkTarget::WindowsPath(path) | NetworkTarget::ShellItemId(path) => (
                    display_path(path),
                    app.sidebar_icons
                        .get(path)
                        .map(shell_icon_image)
                        .unwrap_or_default(),
                ),
                NetworkTarget::WebDav(connection) => (
                    format!(
                        "{}://{}:{}{}",
                        if connection.https { "https" } else { "http" },
                        connection.host,
                        connection.port,
                        connection.root
                    ),
                    Image::default(),
                ),
            };
            HomeItemRow {
                stable_id: stable_id.clone().into(),
                label: location.display_name.clone().into(),
                detail: detail.into(),
                icon,
                progress: 0.0,
                state: -1,
                status: "".into(),
                selected: selected == Some(stable_id.as_str()),
                focused: selected == Some(stable_id.as_str()),
                source_kind: 2,
                menu_group: 2,
                menu_stable_id: format!("network-location:{}", location.id).into(),
            }
        })
        .collect::<Vec<_>>()
        .chunks(columns.max(1))
        .map(|entries| HomeCardRow {
            entries: ModelRc::new(VecModel::from(entries.to_vec())),
        })
        .collect()
}

fn home_item_target(app: &AppState, stable_id: &str) -> Option<PathBuf> {
    let mut parts = stable_id.strip_prefix("home:")?.split(':');
    let source = parts.next()?.parse::<i32>().ok()?;
    let index = parts.next()?.parse::<usize>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    match source {
        0 => app
            .sidebar
            .iter()
            .filter(|location| location.kind == KnownLocationKind::Pinned)
            .nth(index),
        1 => app
            .sidebar
            .iter()
            .filter(|location| location.kind == KnownLocationKind::Drive)
            .nth(index),
        2 => {
            return sorted_network_locations(app)
                .into_iter()
                .find(|location| location.id == index as u64)
                .and_then(|location| match &location.target {
                    NetworkTarget::WindowsPath(path) => Some(path.clone()),
                    NetworkTarget::ShellItemId(_) | NetworkTarget::WebDav(_) => None,
                });
        }
        _ => None,
    }
    .map(|location| location.path.clone())
}

fn home_item_network_target(app: &AppState, stable_id: &str) -> Option<NetworkTarget> {
    let id = stable_id.strip_prefix("home:2:")?.parse::<u64>().ok()?;
    sorted_network_locations(app)
        .into_iter()
        .find(|location| location.id == id)
        .map(|location| location.target.clone())
}

fn replace_quick_access_projection(
    app: &mut AppState,
    generation: u64,
    locations: Vec<KnownLocation>,
) -> bool {
    if app.quick_access_generation != generation {
        return false;
    }
    app.sidebar
        .retain(|location| location.kind == KnownLocationKind::Drive);
    app.sidebar.splice(0..0, locations);
    true
}

fn replace_drive_projection(
    app: &mut AppState,
    generation: u64,
    drives: Vec<KnownLocation>,
) -> bool {
    if app.drive_list_generation != generation {
        return false;
    }
    app.sidebar
        .retain(|location| location.kind != KnownLocationKind::Drive);
    app.sidebar.extend(drives);
    true
}

fn reload_quick_access(weak: slint::Weak<AppWindow>, state: SharedSessions) {
    let generation = {
        let Ok(mut app) = state.lock() else { return };
        app.quick_access_generation = app.quick_access_generation.wrapping_add(1).max(1);
        app.quick_access_generation
    };
    thread::spawn(move || {
        let locations = platform::explorer_pinned_locations().unwrap_or_default();
        let state_for_ui = state.clone();
        let weak_for_icons = weak.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let accepted = state_for_ui.lock().is_ok_and(|mut app| {
                replace_quick_access_projection(&mut app, generation, locations)
            });
            if accepted {
                refresh_all_windows(
                    &state_for_ui,
                    RefreshRegions::SIDEBAR | RefreshRegions::HOME,
                );
            }
            if accepted && let Some(owner) = weak_for_icons.upgrade() {
                start_sidebar_icon_loader(&owner, state_for_ui.clone());
            } else if accepted {
                start_sidebar_icon_loader(&ui, state_for_ui.clone());
            }
        });
    });
}

fn reload_drives(weak: slint::Weak<AppWindow>, state: SharedSessions) {
    let generation = {
        let Ok(mut app) = state.lock() else { return };
        app.drive_list_generation = app.drive_list_generation.wrapping_add(1).max(1);
        app.drive_list_generation
    };
    thread::spawn(move || {
        let drives = platform::logical_drive_locations().collect::<Vec<_>>();
        let state_for_ui = state.clone();
        let weak_for_icons = weak.clone();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let accepted = state_for_ui
                .lock()
                .is_ok_and(|mut app| replace_drive_projection(&mut app, generation, drives));
            if accepted {
                begin_drive_capacity_refresh(&state_for_ui, None, Duration::ZERO);
                refresh_all_windows(
                    &state_for_ui,
                    RefreshRegions::SIDEBAR | RefreshRegions::HOME,
                );
            }
            if accepted && let Some(owner) = weak_for_icons.upgrade() {
                start_sidebar_icon_loader(&owner, state_for_ui.clone());
            } else if accepted {
                start_sidebar_icon_loader(&ui, state_for_ui.clone());
            }
        });
    });
}

fn begin_drive_capacity_refresh(
    state: &SharedSessions,
    requested_roots: Option<&HashSet<PathBuf>>,
    minimum_age: Duration,
) {
    let requests = {
        let Ok(mut app) = state.lock() else { return };
        let roots = app
            .sidebar
            .iter()
            .filter_map(|location| location.drive_root.clone())
            .collect::<HashSet<_>>();
        app.drive_capacity.retain(|root, _| roots.contains(root));
        let now = Instant::now();
        let mut requests = Vec::new();
        for root in roots {
            if requested_roots.is_some_and(|targets| !targets.contains(&root)) {
                continue;
            }

            let should_refresh = app.drive_capacity.get(&root).is_none_or(|capacity| {
                capacity.status != DriveCapacityStatus::Loading
                    && capacity
                        .last_updated
                        .is_none_or(|updated| now.saturating_duration_since(updated) >= minimum_age)
            });
            if should_refresh {
                app.drive_generation = app.drive_generation.wrapping_add(1).max(1);
                let generation = app.drive_generation;
                app.drive_capacity
                    .insert(root.clone(), DriveCapacityState::loading(generation));
                requests.push((root, generation));
            }
        }
        requests
    };
    if !requests.is_empty() {
        refresh_all_windows(state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
    }
    for (root, generation) in requests {
        let timeout_state = state.clone();
        let timeout_root = root.clone();
        slint::Timer::single_shot(Duration::from_secs(10), move || {
            let timed_out = if let Ok(mut app) = timeout_state.lock()
                && app
                    .drive_capacity
                    .get(&timeout_root)
                    .is_some_and(|capacity| {
                        capacity.status == DriveCapacityStatus::Loading
                            && capacity.request_generation == generation
                    }) {
                if let Some(capacity) = app.drive_capacity.get_mut(&timeout_root) {
                    capacity.status = DriveCapacityStatus::Unavailable;
                    capacity.last_updated = Some(Instant::now());
                    capacity.request_generation =
                        capacity.request_generation.wrapping_add(1).max(1);
                }
                true
            } else {
                false
            };
            if timed_out {
                eprintln!(
                    "{{\"event\":\"drive_capacity_timed_out\",\"root\":{:?},\"generation\":{generation}}}",
                    timeout_root
                );
                refresh_all_windows(
                    &timeout_state,
                    RefreshRegions::SIDEBAR | RefreshRegions::HOME,
                );
            }
        });
        let state_for_result = state.clone();
        thread::spawn(move || {
            let result = platform::windows::drives::query_capacity(&root);
            let root_for_log = root.clone();
            let _ = slint::invoke_from_event_loop(move || {
                let accepted = if let Ok(mut app) = state_for_result.lock() {
                    let still_present = app
                        .sidebar
                        .iter()
                        .any(|location| location.drive_root.as_ref() == Some(&root));
                    if still_present
                        && app
                            .drive_capacity
                            .get(&root)
                            .is_some_and(|capacity| capacity.request_generation == generation)
                    {
                        if let Some(capacity) = app.drive_capacity.get_mut(&root) {
                            capacity.apply(result);
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                };
                if accepted {
                    eprintln!(
                        "{{\"event\":\"drive_capacity_updated\",\"root\":{:?},\"generation\":{generation}}}",
                        root_for_log
                    );
                    refresh_all_windows(
                        &state_for_result,
                        RefreshRegions::SIDEBAR | RefreshRegions::HOME,
                    );
                }
            });
        });
    }
}

fn drive_root_for_path(path: &Path) -> Option<PathBuf> {
    let value = path.as_os_str().to_string_lossy();
    let bytes = value.as_bytes();
    (bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic())
        .then(|| PathBuf::from(format!("{}:\\", (bytes[0] as char).to_ascii_uppercase())))
}

fn affected_drive_roots(paths: &[PathBuf]) -> HashSet<PathBuf> {
    paths
        .iter()
        .filter_map(|path| drive_root_for_path(path))
        .collect()
}

fn library_stable_id(identity: &std::ffi::OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;
    identity
        .encode_wide()
        .map(|unit| format!("{unit:04x}"))
        .collect()
}

fn start_library_loader(
    ui: &AppWindow,
    state: SharedSessions,
    directory_sender: mpsc::Sender<DirectoryRequest>,
) {
    reload_libraries(ui.as_weak(), state.clone(), directory_sender.clone());
    start_library_watcher(ui.as_weak(), state, directory_sender);
}

fn reload_libraries(
    weak: slint::Weak<AppWindow>,
    state: SharedSessions,
    directory_sender: mpsc::Sender<DirectoryRequest>,
) {
    let generation = {
        let Ok(mut app) = state.lock() else { return };
        app.library_generation = app.library_generation.wrapping_add(1).max(1);
        app.library_generation
    };
    thread::spawn(move || {
        let result = platform::windows::libraries::enumerate();
        let mut icons = HashMap::new();
        if let Ok(enumeration) = result.as_ref() {
            let _shell_apartment = platform::windows::shell_icons::initialize_shell_worker().ok();
            for library in &enumeration.libraries {
                let Some(definition_path) = library.definition_path.as_ref() else {
                    continue;
                };
                if let Ok(icon) = platform::windows::shell_icons::shell_icon_rgba(definition_path) {
                    icons.insert(library.id.as_os_str().to_owned(), icon);
                }
            }
        }
        let state_for_ui = state.clone();
        let _ = weak.upgrade_in_event_loop(move |_ui| {
            let targets = {
                let Ok(mut app) = state_for_ui.lock() else {
                    return;
                };
                if app.library_generation != generation {
                    return;
                }
                match result {
                    Ok(enumeration) => {
                        app.libraries = enumeration.libraries;
                        app.library_failures = enumeration.failures;
                        app.library_icons = icons;
                    }
                    Err(error) => {
                        app.libraries.clear();
                        app.library_icons.clear();
                        app.library_failures = vec![platform::windows::libraries::LibraryFailure {
                            shell_identity: None,
                            message: error.to_string(),
                        }];
                    }
                }
                app.windows
                    .values()
                    .flat_map(|window| window.tabs.values())
                    .filter_map(|tab| match tab.current_location().as_ref()? {
                        NavigationLocation::Library(library) => Some((tab.id, library.clone())),
                        NavigationLocation::Home
                        | NavigationLocation::Directory(_)
                        | NavigationLocation::WebDav(_) => None,
                    })
                    .collect::<Vec<_>>()
            };
            refresh_all_windows(
                &state_for_ui,
                RefreshRegions::SIDEBAR | RefreshRegions::HOME,
            );
            for (tab_id, library) in targets {
                submit_library_navigation(
                    &directory_sender,
                    &state_for_ui,
                    tab_id,
                    library,
                    NavigationKind::Refresh,
                );
            }
        });
    });
}

fn start_library_watcher(
    weak: slint::Weak<AppWindow>,
    state: SharedSessions,
    directory_sender: mpsc::Sender<DirectoryRequest>,
) {
    thread::spawn(move || {
        let Ok(root) = platform::windows::libraries::folder_path() else {
            return;
        };
        let (events, receiver) = mpsc::channel();
        let Ok(_watcher) = platform::windows::directory_watch::DirectoryWatch::start(root, events)
        else {
            return;
        };
        while receiver.recv().is_ok() {
            thread::sleep(Duration::from_millis(120));
            while receiver.try_recv().is_ok() {}
            let weak_for_ui = weak.clone();
            let state_for_ui = state.clone();
            let sender_for_ui = directory_sender.clone();
            if slint::invoke_from_event_loop(move || {
                reload_libraries(weak_for_ui, state_for_ui, sender_for_ui);
            })
            .is_err()
            {
                break;
            }
        }
    });
}
fn imported_network_locations(
    locations: Vec<platform::windows::network::NetworkLocation>,
) -> Vec<NetworkLocation> {
    locations
        .into_iter()
        .enumerate()
        .filter_map(|(index, location)| {
            let id = stable_network_location_id(&location.shell_path);
            let target = location
                .target
                .map(NetworkTarget::WindowsPath)
                .or_else(|| location.shell_identity.map(NetworkTarget::ShellItemId))?;
            Some(NetworkLocation {
                id,
                source: NetworkLocationSource::WindowsImported,
                display_name: location.label,
                sort_order: index as u32,
                target,
                shell_path: Some(location.shell_path),
            })
        })
        .collect()
}

fn publish_imported_network_locations(
    state: SharedSessions,
    imported: Vec<NetworkLocation>,
) -> bool {
    slint::invoke_from_event_loop(move || {
        let changed = state.lock().is_ok_and(|mut app| {
            if app.imported_network_locations == imported {
                return false;
            }
            app.imported_network_locations = imported;
            app.network_location_generation =
                app.network_location_generation.wrapping_add(1).max(1);
            true
        });
        if changed {
            refresh_all_windows(&state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
        }
    })
    .is_ok()
}

fn start_network_location_loader(_ui: &AppWindow, state: SharedSessions) {
    thread::spawn(move || {
        let load = || {
            platform::windows::network::enumerate_network_locations()
                .map(imported_network_locations)
        };
        let initial = load().ok();
        let Ok(root) = platform::windows::network::network_locations_folder() else {
            if let Some(imported) = initial {
                let _ = publish_imported_network_locations(state, imported);
            }
            return;
        };
        let (events, receiver) = mpsc::channel();
        let Ok(_watcher) = platform::windows::directory_watch::DirectoryWatch::start(root, events)
        else {
            if let Some(imported) = initial {
                let _ = publish_imported_network_locations(state, imported);
            }
            return;
        };
        if let Some(imported) = initial
            && !publish_imported_network_locations(state.clone(), imported)
        {
            return;
        }
        while receiver.recv().is_ok() {
            thread::sleep(Duration::from_millis(120));
            while receiver.try_recv().is_ok() {}
            let Ok(imported) = load() else { continue };
            if !publish_imported_network_locations(state.clone(), imported) {
                break;
            }
        }
    });
}

fn normalize_restored_location(location: NavigationLocation) -> NavigationLocation {
    match location {
        NavigationLocation::Home => NavigationLocation::Home,
        NavigationLocation::Directory(path)
            if crate::domain::folder_size_scheduler::is_internal_cleanup_path(&path) =>
        {
            NavigationLocation::Directory(initial_path())
        }
        NavigationLocation::Directory(path) => NavigationLocation::Directory(
            platform::windows::network::network_drive_to_unc(&path).unwrap_or(path),
        ),
        library @ NavigationLocation::Library(_) => library,
        place @ NavigationLocation::WebDav(_) => place,
    }
}

fn stable_network_location_id(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;

    path.as_os_str()
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        })
}

fn spawn_network_discovery_worker() -> (
    mpsc::SyncSender<NetworkDiscoveryRequest>,
    mpsc::Receiver<NetworkDiscoveryEvent>,
) {
    let (request_sender, request_receiver) = mpsc::sync_channel(1);
    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            run_network_discovery(request, &event_sender);
        }
    });
    (request_sender, event_receiver)
}

fn run_network_discovery(
    request: NetworkDiscoveryRequest,
    event_sender: &mpsc::Sender<NetworkDiscoveryEvent>,
) {
    let NetworkDiscoveryRequest::Discover {
        window_id,
        request_id,
        cancel,
    } = request;
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let result = platform::windows::network::isolated_network_devices(&cancel);
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    match result {
        Ok(devices) => {
            for batch in devices.chunks(16) {
                if cancel.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let devices = batch
                    .iter()
                    .map(|device| NetworkDeviceTarget {
                        id: crate::network::network_device_id(&device.target),
                        display_name: device.label.clone(),
                        shell_identity: None,
                        unc_path: crate::network::is_unc_path(&device.target)
                            .then(|| device.target.clone()),
                    })
                    .collect();
                let _ = event_sender.send(NetworkDiscoveryEvent::Batch {
                    window_id,
                    request_id,
                    devices,
                });
            }
            let _ = event_sender.send(NetworkDiscoveryEvent::Finished {
                window_id,
                request_id,
            });
        }
        Err(error) => {
            if error.kind() == io::ErrorKind::TimedOut
                && let Ok(devices) =
                    platform::windows::network::network_devices_from_imported_locations()
                && !devices.is_empty()
            {
                let devices = devices
                    .into_iter()
                    .map(|device| NetworkDeviceTarget {
                        id: crate::network::network_device_id(&device.target),
                        display_name: device.label,
                        shell_identity: None,
                        unc_path: Some(device.target),
                    })
                    .collect();
                let _ = event_sender.send(NetworkDiscoveryEvent::Batch {
                    window_id,
                    request_id,
                    devices,
                });
                let _ = event_sender.send(NetworkDiscoveryEvent::Finished {
                    window_id,
                    request_id,
                });
                return;
            }
            let _ = event_sender.send(NetworkDiscoveryEvent::Failed {
                window_id,
                request_id,
                error: crate::network::classify_network_error(&error),
            });
        }
    }
}

fn network_error_text(error: crate::network::NetworkErrorKind, language: Language) -> &'static str {
    use crate::network::NetworkErrorKind;
    match (language, error) {
        (Language::Chinese, NetworkErrorKind::PermissionDenied) => {
            "无权访问 Windows 网络设备，点击重试"
        }
        (Language::Chinese, NetworkErrorKind::TimedOut) => "获取网络设备超时，点击重试",
        (Language::Chinese, NetworkErrorKind::Disconnected) => "网络不可用，点击重试",
        (Language::Chinese, _) => "无法获取 Windows 网络设备，点击重试",
        (Language::English, NetworkErrorKind::PermissionDenied) => {
            "Windows network access was denied. Click to retry"
        }
        (Language::English, NetworkErrorKind::TimedOut) => {
            "Network discovery timed out. Click to retry"
        }
        (Language::English, NetworkErrorKind::Disconnected) => {
            "The network is unavailable. Click to retry"
        }
        (Language::English, _) => "Unable to discover Windows network devices. Click to retry",
    }
}
fn start_network_discovery_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<NetworkDiscoveryEvent>,
    state: SharedSessions,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            let state = state.clone();
            let _ = weak.upgrade_in_event_loop(move |_ui| {
                let mut error_message = None;
                if let Ok(mut app) = state.lock() {
                    match event {
                        NetworkDiscoveryEvent::Batch {
                            window_id,
                            request_id,
                            devices,
                        } => {
                            let accepted = app
                                .network_discovery
                                .get_mut(&window_id)
                                .is_some_and(|coordinator| coordinator.append(request_id, devices));
                            if accepted {
                                app.network_discovery_errors.remove(&window_id);
                            }
                        }
                        NetworkDiscoveryEvent::Finished {
                            window_id,
                            request_id,
                        } => {
                            let accepted = app
                                .network_discovery
                                .get_mut(&window_id)
                                .is_some_and(|coordinator| coordinator.finish(request_id));
                            if accepted {
                                app.network_discovery_errors.remove(&window_id);
                                if let Some(devices) = app
                                    .network_discovery
                                    .get(&window_id)
                                    .filter(|discovery| !discovery.devices().is_empty())
                                    .map(|discovery| discovery.devices().to_vec())
                                {
                                    for other_window in
                                        app.windows.keys().copied().collect::<Vec<_>>()
                                    {
                                        if other_window != window_id {
                                            app.network_discovery.insert(
                                                other_window,
                                                DiscoveryCoordinator::with_devices(devices.clone()),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        NetworkDiscoveryEvent::Failed {
                            window_id,
                            request_id,
                            error,
                        } => {
                            if let Some(coordinator) = app.network_discovery.get_mut(&window_id)
                                && coordinator.fail(request_id, error)
                            {
                                let message = network_error_text(error, app.language).to_owned();
                                app.network_discovery_errors.insert(window_id, message);
                                error_message = Some((window_id, error));
                            }
                        }
                    }
                }
                if let Some((window_id, kind)) = error_message {
                    eprintln!(
                        "{{\"event\":\"network_discovery_failed\",\"window\":{},\"kind\":{:?}}}",
                        window_id.0, kind
                    );
                }
                refresh_all_windows(&state, RefreshRegions::SIDEBAR | RefreshRegions::HOME);
            });
        }
    });
}
fn start_icon_event_pump(
    ui: &AppWindow,
    receiver: mpsc::Receiver<IconEvent>,
    state: SharedSessions,
    thumbnails: ThumbnailScheduler,
) {
    let weak = ui.as_weak();
    thread::spawn(move || {
        while let Ok(first) = receiver.recv() {
            // One bounded callback in flight; merge results without delaying input until drag end.
            let events = std::iter::once(first)
                .chain(receiver.try_iter().take(31))
                .collect();
            let (completed, applied) = mpsc::channel();
            let state = state.clone();
            let thumbnails = thumbnails.clone();
            if weak
                .upgrade_in_event_loop(move |ui| {
                    file_images::present_batch(&ui, &state, &thumbnails, events);
                    let _ = completed.send(());
                })
                .is_err()
                || applied.recv().is_err()
            {
                break;
            }
        }
    });
}

#[derive(Debug, Clone)]
struct IconUpdate {
    tab_id: TabId,
    entry_id: Option<EntryId>,
    type_key: Option<platform::windows::shell_icons::ShellTypeIconKey>,
}

fn apply_icon_event(state: &SharedSessions, event: IconEvent) -> Option<IconUpdate> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    if event.thumbnail_failed {
        if !icon_event_is_current(&app, &event) {
            return None;
        }
        let key = (
            event.tab_id,
            event.request_id,
            event.path,
            event.requested_px,
        );
        let attempt = app
            .thumbnail_failures
            .get(&key)
            .map_or(1, |(attempts, _)| attempts.saturating_add(1));
        app.thumbnail_failures
            .insert(key, (attempt, Instant::now()));
        return Some(IconUpdate {
            tab_id: event.tab_id,
            entry_id: match event.target {
                IconTarget::Entry(entry_id) => Some(entry_id),
                IconTarget::Location => None,
            },
            type_key: None,
        });
    }
    if event.requested_px == 0 {
        app.ordinary_icon_requests
            .remove(&(event.tab_id, event.request_id, event.path.clone()));
    }
    if let Some(key) = event.type_key.as_ref() {
        app.type_icon_requests
            .remove(&(key.clone(), event.requested_px));
        if event.path != key.query_path() {
            return None;
        }
        app.image_cache
            .types
            .insert((key.clone(), event.requested_px), event.icon);
        return Some(IconUpdate {
            tab_id: event.tab_id,
            entry_id: None,
            type_key: event.type_key,
        });
    }
    if !icon_event_is_current(&app, &event) {
        return None;
    }
    let entry_id = match event.target {
        IconTarget::Entry(entry_id) => {
            if event.requested_px == 0 {
                app.icons.insert(
                    (event.tab_id, event.request_id, entry_id),
                    event.icon.clone(),
                );
            }
            Some(entry_id)
        }
        IconTarget::Location => None,
    };
    if event.requested_px > 0 {
        app.thumbnail_failures.remove(&(
            event.tab_id,
            event.request_id,
            event.path.clone(),
            event.requested_px,
        ));
        let cache = if event.actual_thumbnail {
            &mut app.image_cache.thumbnails
        } else {
            &mut app.image_cache.grid_icons
        };
        cache.insert((event.path.clone(), event.requested_px), event.icon);
    } else {
        app.image_cache.ordinary.insert(event.path, event.icon);
    }
    Some(IconUpdate {
        tab_id: event.tab_id,
        entry_id,
        type_key: event.type_key,
    })
}

fn icon_event_is_current(app: &AppState, event: &IconEvent) -> bool {
    app.tab(event.tab_id).is_some_and(|tab| {
        tab.latest_request() == event.request_id
            && match event.target {
                IconTarget::Entry(entry_id) => tab.visible_entry(entry_id).is_some_and(|entry| {
                    event.type_key.as_ref().is_some_and(|key| {
                        entry_type_icon_key(&entry.path(), entry.kind).as_ref() == Some(key)
                            && event.path == key.query_path()
                    }) || (event.type_key.is_none() && entry.path.matches(&event.path))
                }),
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

/// Shows a freshly projected list and indexes it so later patches skip full scans (#175).
fn set_file_rows(ui: &AppWindow, rows: Vec<FileRow>, visible_order: bool) {
    let layout = file_row_index::RowLayout::build(&rows, visible_order, |row| {
        group_header_height_for_detail(row.group_detail.as_str()) as f32
    });
    let model = Rc::new(VecModel::from(rows));
    file_row_index::install(&model, layout);
    ui.set_files(ModelRc::from(model));
}

fn append_active_file_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    tab_id: TabId,
    request_id: RequestId,
) {
    let app = state.lock().expect("app state mutex is not poisoned");
    if app.active_window_state().active_tab != tab_id {
        return;
    }
    let tab = app.active();
    if tab.latest_request() != request_id || tab.load_state != LoadState::Partial {
        return;
    }
    if tab.retains_directory_view() {
        let projection_current = ui.get_projected_file_tab_id() == tab_id.0 as i32
            && ui.get_projected_file_request_id() == request_id.0 as i32;
        if projection_current {
            ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
        } else {
            let window_id = app.active_window;
            drop(app);
            refresh_window_ui(ui, state, window_id);
        }
        return;
    }
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != request_id.0 as i32
    {
        drop(app);
        refresh_tab_window(state, tab_id);
        return;
    }
    let anchor = (ui.get_file_viewport_y() < 0.0)
        .then(|| capture_zoom_anchor(ui, 16.0, 0.0))
        .flatten();
    let flat = !matches!(tab.visible_location(), Some(NavigationLocation::Library(_)))
        && tab
            .visible_path()
            .is_none_or(|path| app.directory_preference(path).group_field == GroupField::None);
    let appended = flat
        && !app.active_view_mode().uses_grid_layout()
        && directory_rows(ui).is_some_and(|rows| rows.append_pending(tab, &app));
    if !appended {
        install_directory_models(ui, &app, tab, app.active_view_mode(), true);
    }
    restore_zoom_anchor(ui, anchor);
    ui.set_page_state(agent_debug::page_projection(tab.load_state, false).index);
    ui.set_loading_hint_visible(false);
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_sort_field(sort_field_ui_value(tab.sort_field));
    ui.set_sort_descending(tab.sort_direction == SortDirection::Descending);
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
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
    if let Some(type_key) = update.type_key {
        let entry_ids = file_images::resident_ids(ui, &app, window_id, tab)
            .into_iter()
            .filter_map(|id| tab.visible_entry(id))
            .filter(|entry| {
                entry_type_icon_key(&entry.path(), entry.kind).as_ref() == Some(&type_key)
            })
            .map(|entry| entry.id)
            .collect::<HashSet<_>>();
        drop(app);
        file_images::patch_images(ui, state, update.tab_id, &entry_ids);
        return;
    }
    let Some(entry_id) = update.entry_id else {
        let path = tab.visible_path();
        ui.set_current_location_icon(
            path.and_then(|path| app.image_cache.ordinary.get(path))
                .map(shell_icon_image)
                .unwrap_or_default(),
        );
        return;
    };
    drop(app);
    file_images::patch_images(ui, state, update.tab_id, &HashSet::from([entry_id]));
}

/// Rows whose selected or focused flag differs from `before`. Members kept across a mutation
/// keep their row, so a Ctrl-click inside a 100k selection patches only the toggled and focus
/// rows instead of every selected row (#161).
fn selection_changed_rows(
    before: &Selection,
    before_focus: Option<EntryId>,
    tab: &TabSession,
) -> HashSet<EntryId> {
    before
        .members()
        .symmetric_difference(tab.selected.members())
        .copied()
        .chain(before_focus)
        .chain(tab.focused)
        .collect()
}

fn mutate_selection(
    tab: &mut TabSession,
    mutate: impl FnOnce(&mut TabSession),
) -> HashSet<EntryId> {
    let before = tab.selected.clone();
    let before_focus = tab.focused;
    mutate(tab);
    selection_changed_rows(&before, before_focus, tab)
}

fn mutate_active_selection(
    state: &WindowSessions,
    mutate: impl FnOnce(&mut TabSession),
) -> Option<(TabId, HashSet<EntryId>)> {
    let mut app = state.lock().ok()?;
    let tab_id = app.active_window_state().active_tab;
    let tab = app.active_window_state_mut().tabs.get_mut(&tab_id)?;
    Some((tab_id, mutate_selection(tab, mutate)))
}
fn mutate_window_selection(
    state: &SharedSessions,
    window_id: WindowId,
    mutate: impl FnOnce(&mut TabSession),
) -> Option<(TabId, HashSet<EntryId>)> {
    let mut app = state.lock().ok()?;
    let window = app.windows.get_mut(&window_id)?;
    let tab_id = window.active_tab;
    let tab = window.tabs.get_mut(&tab_id)?;
    Some((tab_id, mutate_selection(tab, mutate)))
}

fn update_tab_status(ui: &AppWindow, state: &SharedSessions, tab_id: TabId) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let Some(window) = app
        .window_for_tab(tab_id)
        .and_then(|window_id| app.window(window_id))
    else {
        return;
    };
    let Some(tab) = window.tabs.get(&tab_id) else {
        return;
    };
    if window.active_tab == tab_id
        && ui.get_projected_file_tab_id() == tab_id.0 as i32
        && ui.get_projected_file_request_id() == tab.latest_request().0 as i32
    {
        ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
    }
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
        || ui.get_projected_file_request_id() != tab.latest_request().0 as i32
    {
        return;
    }

    if let Some(rows) = directory_rows(ui) {
        rows.patch_entries(tab, &app, changed);
        return;
    }
    let texts = Texts::new(app.language);
    let Some(view_mode) = app.view_mode_for_tab(tab_id) else {
        return;
    };
    let grid_requested_px = file_thumbnail_request_px(view_mode, ui.window().scale_factor());
    let rows = changed
        .iter()
        .filter_map(|id| tab.visible_entry(*id))
        .map(|entry| {
            (
                entry.id,
                file_row(entry, tab, texts, &app, grid_requested_px),
            )
        })
        .collect::<HashMap<_, _>>();
    if rows.is_empty() {
        return;
    }

    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        return;
    };
    for (entry_id, index) in file_row_index::locate(model, tab, rows.keys().copied()) {
        model.set_row_data(index, rows[&entry_id].clone());
    }

    let grid_model = ui.get_grid_rows();
    for (entry_id, updated) in rows {
        let Some((row_index, entry_index)) = GRID_ENTRY_POSITIONS.with_borrow(|by_window| {
            by_window
                .get(&window_id)
                .and_then(|positions| positions.get(&entry_id).copied())
        }) else {
            continue;
        };
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
        if entries
            .row_data(entry_index)
            .is_some_and(|existing| existing.id == entry_id.0 as i32)
        {
            entries.set_row_data(entry_index, updated);
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

fn project_column_layout(ui: &AppWindow, tab: &TabSession, app: &AppState) {
    let normal = tab
        .visible_path()
        .map(|path| app.directory_preference(path).columns)
        .unwrap_or(app.default_directory_view.columns);
    let search = app.search_view.columns;
    ui.set_normal_name_width(normal.widths[ColumnKind::Name.storage_code() as usize] as f32);
    ui.set_normal_kind_width(normal.widths[ColumnKind::Kind.storage_code() as usize] as f32);
    ui.set_normal_size_width(normal.widths[ColumnKind::Size.storage_code() as usize] as f32);
    ui.set_normal_modified_width(
        normal.widths[ColumnKind::Modified.storage_code() as usize] as f32,
    );
    ui.set_normal_created_width(normal.widths[ColumnKind::Created.storage_code() as usize] as f32);
    ui.set_search_name_width(search.widths[ColumnKind::Name.storage_code() as usize] as f32);
    ui.set_search_parent_width(search.widths[ColumnKind::Kind.storage_code() as usize] as f32);
    ui.set_search_size_width(search.widths[ColumnKind::Size.storage_code() as usize] as f32);
    ui.set_search_modified_width(
        search.widths[ColumnKind::Modified.storage_code() as usize] as f32,
    );
    ui.set_search_created_width(search.widths[ColumnKind::Created.storage_code() as usize] as f32);
    let layout = if tab.page_source == PageSource::Search {
        search
    } else {
        normal
    };
    let label = |kind: ColumnKind| match (app.language, kind, tab.page_source) {
        (Language::Chinese, ColumnKind::Name, _) => "名称",
        (Language::English, ColumnKind::Name, _) => "Name",
        (Language::Chinese, ColumnKind::Kind, PageSource::Search) => "父目录",
        (Language::English, ColumnKind::Kind, PageSource::Search) => "Parent path",
        (Language::Chinese, ColumnKind::Kind, _) => "类型",
        (Language::English, ColumnKind::Kind, _) => "Type",
        (Language::Chinese, ColumnKind::Size, _) => "大小",
        (Language::English, ColumnKind::Size, _) => "Size",
        (Language::Chinese, ColumnKind::Modified, _) => "修改时间",
        (Language::English, ColumnKind::Modified, _) => "Date modified",
        (Language::Chinese, ColumnKind::Created, _) => "创建时间",
        (Language::English, ColumnKind::Created, _) => "Date created",
    };
    ui.set_columns(ModelRc::new(VecModel::from(
        layout
            .order
            .iter()
            .map(|kind| {
                let code = kind.storage_code() as usize;
                ColumnRow {
                    kind: code as i32,
                    label: label(*kind).into(),
                    visible: layout.visible[code],
                    min_width: 64.0,
                    content_left: if *kind == ColumnKind::Name { 10.0 } else { 8.0 },
                    content_right: 8.0,
                    icon_slot: if *kind == ColumnKind::Name { 25.0 } else { 0.0 },
                }
            })
            .collect::<Vec<_>>(),
    )));
}
fn update_selection_summary(ui: &AppWindow, state: &WindowSessions) {
    refresh_regions(ui, state, RefreshRegions::COLUMNS | RefreshRegions::STATUS);
}

fn undo_failure_message(
    language: Language,
    source_kind: Option<UndoSourceKind>,
    reason: &str,
) -> String {
    let changed = reason.contains("item changed after the original operation")
        || reason.contains("item tree changed after the original operation")
        || reason.contains("created folder changed after creation")
        || reason.contains("created folder is no longer empty");
    match (language, source_kind, changed) {
        (Language::Chinese, Some(UndoSourceKind::Copy), true) => {
            "无法撤销复制：复制后的文件或文件夹已经被修改。为避免丢失这些修改，AsterFiles 没有删除它。".to_owned()
        }
        (Language::English, Some(UndoSourceKind::Copy), true) => {
            "Could not undo the copy because the copied file or folder was modified. AsterFiles kept it to avoid losing those changes.".to_owned()
        }
        (Language::Chinese, Some(UndoSourceKind::Rename | UndoSourceKind::Move), true) => {
            "无法撤销：操作后的文件或文件夹已经被修改。为避免丢失这些修改，AsterFiles 保持当前位置不变。".to_owned()
        }
        (Language::English, Some(UndoSourceKind::Rename | UndoSourceKind::Move), true) => {
            "Could not undo because the file or folder was modified after the operation. AsterFiles left it in place to avoid losing those changes.".to_owned()
        }
        (Language::Chinese, Some(UndoSourceKind::CreateFolder), true) => {
            "无法撤销新建文件夹：该文件夹已经包含内容或被修改。为避免丢失数据，AsterFiles 没有删除它。".to_owned()
        }
        (Language::English, Some(UndoSourceKind::CreateFolder), true) => {
            "Could not undo folder creation because the folder now contains content or was modified. AsterFiles kept it to avoid data loss.".to_owned()
        }
        (Language::Chinese, _, _) => format!("无法安全撤销文件操作。\n\n原因：{reason}"),
        (Language::English, _, _) => format!("The file operation could not be safely undone.\n\nReason: {reason}"),
    }
}

fn show_undo_failure_dialog(ui: &AppWindow, language: Language, message: &str) {
    let title = match language {
        Language::Chinese => "无法撤销文件操作",
        Language::English => "Could not undo file operation",
    };
    platform::windows::show_error_dialog(native_window_handle(ui), title, message);
}

fn operation_rows(app: &AppState) -> Vec<OperationRow> {
    let mut rows = app
        .operations
        .iter()
        .filter(|task| operation_task_has_visible_row(task))
        .map(|task| {
            let title = match (app.language, task.kind) {
                (Language::Chinese, FileOperationKind::CreateFolder) => "新建文件夹",
                (Language::Chinese, FileOperationKind::Rename) => "重命名",
                (Language::Chinese, FileOperationKind::Copy) => "复制",
                (Language::Chinese, FileOperationKind::Move) => "移动",
                (Language::Chinese, FileOperationKind::RecycleDelete) => "移到回收站",
                (
                    Language::Chinese,
                    FileOperationKind::PermanentDelete | FileOperationKind::FastRemove,
                ) => "删除",
                (Language::Chinese, FileOperationKind::Undo) => match task.undo_source_kind {
                    Some(UndoSourceKind::CreateFolder) => "撤销新建文件夹",
                    Some(UndoSourceKind::Rename) => "撤销重命名",
                    Some(UndoSourceKind::Copy) => "撤销复制",
                    Some(UndoSourceKind::Move) => "撤销移动",
                    Some(UndoSourceKind::RecycleDelete) => "恢复回收站项目",
                    None => "撤销文件操作",
                },
                (Language::English, FileOperationKind::CreateFolder) => "Create folder",
                (Language::English, FileOperationKind::Rename) => "Rename",
                (Language::English, FileOperationKind::Copy) => "Copy",
                (Language::English, FileOperationKind::Move) => "Move",
                (Language::English, FileOperationKind::RecycleDelete) => "Move to Recycle Bin",
                (
                    Language::English,
                    FileOperationKind::PermanentDelete | FileOperationKind::FastRemove,
                ) => "Delete",
                (Language::English, FileOperationKind::Undo) => match task.undo_source_kind {
                    Some(UndoSourceKind::CreateFolder) => "Undo create folder",
                    Some(UndoSourceKind::Rename) => "Undo rename",
                    Some(UndoSourceKind::Copy) => "Undo copy",
                    Some(UndoSourceKind::Move) => "Undo move",
                    Some(UndoSourceKind::RecycleDelete) => "Restore recycled items",
                    None => "Undo file operation",
                },
            };
            let pause_pending =
                task.state == OperationState::Paused && !task.cancellation.is_pause_acknowledged();
            let recovering_copy = task.resource == OperationResource::Network
                && task.state == OperationState::Running
                && matches!(task.kind, FileOperationKind::Copy | FileOperationKind::Move)
                && task.progress.copy_recovering;
            let waiting_network = task.resource == OperationResource::Network
                && task.state == OperationState::Running
                && matches!(task.kind, FileOperationKind::Copy | FileOperationKind::Move)
                && task.progress.recent_speed_bps == Some(0);
            let status = if pause_pending {
                Texts::new(app.language).operation_pausing()
            } else if recovering_copy {
                Texts::new(app.language).operation_recovering_copy()
            } else if waiting_network {
                Texts::new(app.language).operation_waiting_network()
            } else if task.kind == FileOperationKind::PermanentDelete {
                match (app.language, task.permanent_delete_stage, task.state) {
                    (
                        Language::Chinese,
                        Some(
                            PermanentDeleteStage::SourceRemoved
                            | PermanentDeleteStage::ReleasingSpace,
                        ),
                        OperationState::Queued | OperationState::Preflight,
                    ) => "已从原位置移除，等待释放空间",
                    (
                        Language::English,
                        Some(
                            PermanentDeleteStage::SourceRemoved
                            | PermanentDeleteStage::ReleasingSpace,
                        ),
                        OperationState::Queued | OperationState::Preflight,
                    ) => "Removed from the original location; waiting to release space",
                    (
                        Language::Chinese,
                        Some(
                            PermanentDeleteStage::SourceRemoved
                            | PermanentDeleteStage::ReleasingSpace,
                        ),
                        OperationState::Running
                        | OperationState::Paused
                        | OperationState::Cancelling,
                    ) => "已从原位置移除，正在释放空间",
                    (
                        Language::English,
                        Some(
                            PermanentDeleteStage::SourceRemoved
                            | PermanentDeleteStage::ReleasingSpace,
                        ),
                        OperationState::Running
                        | OperationState::Paused
                        | OperationState::Cancelling,
                    ) => "Removed from the original location; releasing space",
                    (Language::Chinese, _, OperationState::Completed) => "空间已释放",
                    (Language::English, _, OperationState::Completed) => "Disk space released",
                    _ => operation_status_text(app.language, task.state),
                }
            } else {
                operation_status_text(app.language, task.state)
            };
            let queue = app.operations.queue_position(task.id).map_or_else(
                String::new,
                |position| match app.language {
                    Language::Chinese => format!("队列第 {position} 项"),
                    Language::English => format!("Queue position {position}"),
                },
            );
            let recycle_progress = uses_local_recycle_batch(task.kind, task.resource);
            let committed_cleanup = task.kind == FileOperationKind::PermanentDelete
                && task.resource == OperationResource::Cleanup;
            let cleanup_in_progress = committed_cleanup
                && matches!(
                    task.state,
                    OperationState::Queued
                        | OperationState::Preflight
                        | OperationState::Running
                        | OperationState::Paused
                        | OperationState::Cancelling
                );
            let progress_known = if committed_cleanup {
                task.state == OperationState::Completed
            } else {
                !recycle_progress && task.progress.scanning_complete
            };
            let progress = if recycle_progress {
                0.0
            } else if committed_cleanup && task.state == OperationState::Completed {
                1.0
            } else if progress_known {
                task.progress
                    .total_bytes
                    .filter(|total| *total > 0)
                    .map(|total| task.progress.accounted_bytes() as f32 / total as f32)
                    .unwrap_or_else(|| {
                        if task.items.is_empty() {
                            0.0
                        } else {
                            task.progress.completed_items as f32 / task.progress.total_items as f32
                        }
                    })
            } else {
                0.0
            };
            let (transferred, speed_text, eta_text) = if committed_cleanup {
                (
                    format!(
                        "{:.1} MB",
                        task.progress.processed_bytes as f64 / 1_048_576.0
                    ),
                    String::new(),
                    "—".to_owned(),
                )
            } else if recycle_progress {
                let transferred = String::new();
                let texts = Texts::new(app.language);
                let phase = if task.state == OperationState::Preflight {
                    texts.recycle_preparing(
                        task.progress.prepared_items.min(task.progress.total_items),
                        task.progress.total_items,
                    )
                } else {
                    texts
                        .recycle_discovery_phase(task.progress.scanning_complete)
                        .to_owned()
                };
                (transferred, phase, "—".to_owned())
            } else if progress_known {
                task.progress
                    .total_bytes
                    .filter(|total| *total > 0)
                    .map(|total| {
                        let speed = task.progress.recent_speed_bps.map(|speed| speed as f64);
                        let remaining = total.saturating_sub(task.progress.accounted_bytes());
                        (
                            if task.progress.skipped_files > 0 {
                                Texts::new(app.language).operation_processed_bytes(
                                    task.progress.accounted_bytes(),
                                    Some(total),
                                )
                            } else {
                                format!(
                                    "{:.1} / {:.1} MB",
                                    task.progress.processed_bytes as f64 / 1_048_576.0,
                                    total as f64 / 1_048_576.0
                                )
                            },
                            match (task.state, speed) {
                                (OperationState::Paused, _) => status.to_owned(),
                                (OperationState::WaitingConflict, _) => operation_status_text(
                                    app.language,
                                    OperationState::WaitingConflict,
                                )
                                .to_owned(),
                                (OperationState::Running, Some(speed)) => {
                                    format!("{:.1} MB/s", speed / 1_048_576.0)
                                }
                                (OperationState::Running, None) => {
                                    estimating_text(app.language).to_owned()
                                }
                                _ => String::new(),
                            },
                            match (task.state, speed) {
                                (OperationState::Running, Some(speed)) if speed > 0.0 => {
                                    format_remaining_time(app.language, remaining as f64 / speed)
                                }
                                (OperationState::Running, None) => {
                                    estimating_text(app.language).to_owned()
                                }
                                _ => "—".to_owned(),
                            },
                        )
                    })
                    .unwrap_or_else(|| {
                        (
                            format!(
                                "{}/{}",
                                task.progress.completed_items, task.progress.total_items
                            ),
                            String::new(),
                            String::new(),
                        )
                    })
            } else {
                (
                    if task.progress.skipped_files > 0 {
                        Texts::new(app.language)
                            .operation_processed_bytes(task.progress.accounted_bytes(), None)
                    } else if matches!(task.kind, FileOperationKind::Copy | FileOperationKind::Move)
                    {
                        Texts::new(app.language).operation_copied(task.progress.processed_bytes)
                    } else {
                        format!(
                            "{:.1} MB",
                            task.progress.processed_bytes as f64 / 1_048_576.0
                        )
                    },
                    match task.state {
                        OperationState::Running => task
                            .progress
                            .recent_speed_bps
                            .map(|speed| format!("{:.1} MB/s", speed as f64 / 1_048_576.0))
                            .unwrap_or_else(|| estimating_text(app.language).to_owned()),
                        OperationState::Paused | OperationState::WaitingConflict => {
                            status.to_owned()
                        }
                        _ => String::new(),
                    },
                    if task.state == OperationState::Running {
                        Texts::new(app.language)
                            .operation_total_pending()
                            .to_owned()
                    } else {
                        "—".to_owned()
                    },
                )
            };
            let (speed_text, eta_text) = if recovering_copy {
                (
                    Texts::new(app.language)
                        .operation_recovering_copy()
                        .to_owned(),
                    "—".to_owned(),
                )
            } else if waiting_network {
                (
                    Texts::new(app.language)
                        .operation_waiting_network()
                        .to_owned(),
                    "—".to_owned(),
                )
            } else {
                (speed_text, eta_text)
            };
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
                .map(|path| {
                    if task.kind == FileOperationKind::PermanentDelete
                        && task.permanent_delete_stage.is_some_and(|stage| {
                            matches!(
                                stage,
                                PermanentDeleteStage::SourceRemoved
                                    | PermanentDeleteStage::ReleasingSpace
                            )
                        })
                    {
                        display_path(path)
                    } else {
                        path.parent().map(display_path).unwrap_or_default()
                    }
                })
                .unwrap_or_default();
            let current_item = task
                .progress
                .current_item
                .as_deref()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let failure_summary = operation_failure_summary(app.language, task);
            let source_line = match app.language {
                Language::Chinese => format!("来源：{source}"),
                Language::English => format!("From: {source}"),
            };
            let destination_line = if is_cleanup_completion_receipt(task) {
                String::new()
            } else if task.kind == FileOperationKind::PermanentDelete
                && task.permanent_delete_stage.is_some_and(|stage| {
                    matches!(
                        stage,
                        PermanentDeleteStage::SourceRemoved | PermanentDeleteStage::ReleasingSpace
                    )
                })
            {
                match app.language {
                    Language::Chinese => format!("遗留位置：{destination}"),
                    Language::English => format!("Remaining at: {destination}"),
                }
            } else {
                match app.language {
                    Language::Chinese => format!("目标：{destination}"),
                    Language::English => format!("To: {destination}"),
                }
            };
            let current_item_line = match app.language {
                Language::Chinese => format!("当前项：{current_item}"),
                Language::English => format!("Current item: {current_item}"),
            };
            let accessible_label = [
                title.to_owned(),
                status.to_owned(),
                queue.clone(),
                failure_summary.clone(),
                current_item_line.clone(),
                transferred.clone(),
                speed_text.clone(),
                eta_text.clone(),
            ]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("，");
            OperationRow {
                id: task.id.0 as i32,
                title: title.into(),
                status: status.into(),
                queue: queue.into(),
                failure_summary: failure_summary.into(),
                accessible_label: accessible_label.into(),
                source_line: source_line.into(),
                destination_line: destination_line.into(),
                current_item_line: current_item_line.into(),
                percent: if progress_known {
                    format!("{:.0}%", (progress * 100.0).clamp(0.0, 100.0))
                } else {
                    String::new()
                }
                .into(),
                file_progress: if committed_cleanup {
                    match (app.language, task.state) {
                        (Language::Chinese, OperationState::Completed) => "空间已释放".to_owned(),
                        (Language::English, OperationState::Completed) => {
                            "Disk space released".to_owned()
                        }
                        (Language::Chinese, _) => {
                            format!("已处理 {} 项", task.progress.completed_items)
                        }
                        (Language::English, _) => {
                            format!("{} processed", task.progress.completed_items)
                        }
                    }
                } else if recycle_progress {
                    let texts = Texts::new(app.language);
                    if task.state == OperationState::Preflight {
                        texts.recycle_preparing(
                            task.progress.prepared_items.min(task.progress.total_items),
                            task.progress.total_items,
                        )
                    } else {
                        texts.recycle_discovered(
                            task.progress.discovered_files,
                            task.progress.discovered_bytes,
                            task.progress.scanning_complete,
                        )
                    }
                } else {
                    task.progress
                        .total_files
                        .map(|total| {
                            format!("{} / {total}", task.progress.accounted_files().min(total))
                        })
                        .unwrap_or_else(|| {
                            if task.state == OperationState::Preflight {
                                operation_status_text(app.language, task.state).to_owned()
                            } else if task.state == OperationState::Running
                                && task.progress.processed_bytes > 0
                                && matches!(
                                    task.kind,
                                    FileOperationKind::Copy | FileOperationKind::Move
                                )
                            {
                                Texts::new(app.language)
                                    .operation_copying_files(task.progress.completed_files)
                            } else if task.state == OperationState::Running {
                                Texts::new(app.language)
                                    .operation_total_pending()
                                    .to_owned()
                            } else {
                                status.to_owned()
                            }
                        })
                }
                .into(),
                transferred: transferred.into(),
                speed: speed_text.into(),
                eta: eta_text.into(),
                paused: task.state == OperationState::Paused,
                pause_pending: !committed_cleanup
                    && task.state == OperationState::Paused
                    && !task.cancellation.is_pause_acknowledged(),
                cancel_pending: !committed_cleanup && task.state == OperationState::Cancelling,
                progress,
                progress_known,
                state: operation_state_index(task.state),
                can_cancel: actions::cancellation_reason(task).is_none(),
                can_pause: !cleanup_in_progress
                    && !recycle_progress
                    && matches!(task.state, OperationState::Running | OperationState::Paused),
                can_retry: task.kind != FileOperationKind::Undo
                    && matches!(
                        task.state,
                        OperationState::Failed | OperationState::PartiallyCompleted
                    )
                    && task.items.iter().any(|item| {
                        matches!(
                            item.state,
                            ItemState::Failed | ItemState::Cancelled | ItemState::Pending
                        )
                    }),
            }
        })
        .collect::<Vec<_>>();
    rows.extend(webdav_ui::transfer_rows(app.language));
    rows.sort_by_key(|row| (operation_row_rank(row.state), row.id));
    rows
}

fn operation_task_has_visible_row(task: &crate::domain::file_operations::OperationTask) -> bool {
    if task.recovered_cleanup {
        return matches!(
            task.state,
            OperationState::Failed
                | OperationState::PartiallyCompleted
                | OperationState::WaitingConflict
        );
    }
    !matches!(
        task.state,
        OperationState::Completed | OperationState::Cancelled
    ) || is_cleanup_completion_receipt(task)
}

fn is_cleanup_completion_receipt(task: &crate::domain::file_operations::OperationTask) -> bool {
    task.kind == FileOperationKind::PermanentDelete
        && task.resource == OperationResource::Cleanup
        && task.state == OperationState::Completed
        && !task.recovered_cleanup
}

fn operation_row_rank(state: i32) -> i32 {
    match state {
        2 | 3 => 0,
        9 | 1 | 4 => 1,
        0 => 2,
        7 | 8 => 3,
        _ => 4,
    }
}

fn operation_status_text(language: Language, state: OperationState) -> &'static str {
    match (language, state) {
        (Language::Chinese, OperationState::Queued) => "排队中",
        (Language::English, OperationState::Queued) => "Queued",
        (Language::Chinese, OperationState::Preflight) => "正在准备",
        (Language::English, OperationState::Preflight) => "Preparing",
        (Language::Chinese, OperationState::Running) => "正在运行",
        (Language::English, OperationState::Running) => "Running",
        (Language::Chinese, OperationState::Paused) => "已暂停",
        (Language::English, OperationState::Paused) => "Paused",
        (Language::Chinese, OperationState::WaitingConflict) => "等待处理冲突",
        (Language::English, OperationState::WaitingConflict) => "Waiting for conflict decision",
        (Language::Chinese, OperationState::Cancelling) => "正在取消",
        (Language::English, OperationState::Cancelling) => "Cancelling",
        (Language::Chinese, OperationState::Completed) => "已完成",
        (Language::English, OperationState::Completed) => "Completed",
        (Language::Chinese, OperationState::Cancelled) => "已取消",
        (Language::English, OperationState::Cancelled) => "Cancelled",
        (Language::Chinese, OperationState::PartiallyCompleted) => "部分完成",
        (Language::English, OperationState::PartiallyCompleted) => "Partially completed",
        (Language::Chinese, OperationState::Failed) => "失败",
        (Language::English, OperationState::Failed) => "Failed",
    }
}

fn estimating_text(language: Language) -> &'static str {
    match language {
        Language::Chinese => "正在估算",
        Language::English => "Estimating",
    }
}

fn operation_error_text(language: Language, error: &str) -> &str {
    if error == file_operation_worker::SELF_CONTAINMENT_ERROR {
        Texts::new(language).self_containment_message()
    } else {
        error
    }
}

fn operation_failure_summary(
    language: Language,
    task: &crate::domain::file_operations::OperationTask,
) -> String {
    let failures = task
        .items
        .iter()
        .filter_map(|item| item.error.as_deref())
        .map(|error| operation_error_text(language, error))
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return String::new();
    }
    if task
        .items
        .iter()
        .any(|item| item.state == ItemState::Succeeded && item.error.is_some())
    {
        return Texts::new(language).move_source_retained(failures.len(), failures[0]);
    }
    match language {
        Language::Chinese => format!("失败 {} 项：{}", failures.len(), failures[0]),
        Language::English => format!("{} failed: {}", failures.len(), failures[0]),
    }
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
fn refresh_regions(ui: &AppWindow, state: &WindowSessions, regions: RefreshRegions) {
    refresh_window_regions(ui, &state.shared, state.window_id, regions);
}

fn refresh_window_ui(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    let regions = if ui.get_projected_file_tab_id() < 0 {
        RefreshRegions::ALL
    } else {
        RefreshRegions::CONTENT
    };
    refresh_window_regions(ui, state, window_id, regions);
}

fn reflow_after_window_resize(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    let old_home_columns = ui.get_home_column_count();
    let logical_width = logical_window_width(ui);
    ui.set_window_width(logical_width);
    ui.set_home_content_width(home_content_width_for_window(logical_width));
    let mode = projected_view_mode(ui);
    let columns_changed = !ui.get_active_is_home()
        && !ui.get_active_is_settings()
        && mode.uses_grid_layout()
        && ui.get_grid_column_count() != grid_column_count_for_view(ui, mode) as i32;
    let anchor = columns_changed
        .then(|| capture_zoom_anchor(ui, 16.0, 0.0))
        .flatten();
    // Geometry and breadcrumb width are cheap; only a changed grid topology needs new rows.
    let mut regions = RefreshRegions::LAYOUT;
    if columns_changed {
        regions = regions | RefreshRegions::FILES;
    }
    if ui.get_active_is_home() && old_home_columns != ui.get_home_column_count() {
        regions = regions | RefreshRegions::HOME;
    }
    refresh_window_regions(ui, state, window_id, regions);
    if columns_changed {
        restore_zoom_anchor(ui, anchor);
    }
}

fn apply_pending_rename_ui(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    let pending = state.lock().ok().and_then(|mut app| {
        let pending = app.pending_rename_ui.get(&window_id)?.clone();
        let window = app.window(window_id)?;
        let tab = window.tabs.get(&pending.tab_id)?;
        if tab.latest_request() != pending.request_id || tab.focused != Some(pending.entry_id) {
            app.pending_rename_ui.remove(&window_id);
            return None;
        }
        if window.active_tab != pending.tab_id {
            return None;
        }
        app.pending_rename_ui.remove(&window_id)
    });
    if let Some(pending) = pending {
        if let Ok(mut app) = state.lock()
            && let Some(tab) = app.tab(pending.tab_id)
        {
            let target = actions::EntryTarget {
                tab: actions::ActionTarget {
                    window_id,
                    tab_id: pending.tab_id,
                    request_id: tab.latest_request(),
                },
                entry_id: pending.entry_id,
            };
            app.rename_targets.insert(window_id, target);
        }
        ui.set_rename_entry_id(pending.entry_id.0 as i32);
        ui.set_rename_input(pending.input.into());
        ui.set_rename_selection_end(pending.selection_end as i32);
        ui.set_rename_submitting(false);
        ui.set_rename_editing(true);
    }
}

fn refresh_ui(ui: &AppWindow, state: &WindowSessions) {
    refresh_window_ui(ui, &state.shared, state.window_id);
}

fn type_select_context(app: &AppState, tab: &TabSession) -> TypeSelectContext {
    let (group_field, group_direction) = if tab.page_source == PageSource::Search {
        (GroupField::None, SortDirection::Ascending)
    } else {
        tab.visible_path()
            .map(|path| {
                let preference = app.directory_preference(path);
                (preference.group_field, preference.group_direction)
            })
            .unwrap_or((GroupField::None, SortDirection::Ascending))
    };
    TypeSelectContext {
        tab_id: tab.id,
        request_id: tab.latest_request(),
        view_mode: app
            .view_mode_for_tab(tab.id)
            .unwrap_or(app.default_directory_view.view_mode),
        page_source: tab.page_source,
        group_field,
        group_direction,
        sort_field: if tab.page_source == PageSource::Search {
            tab.search_sort_field
        } else {
            tab.sort_field
        },
        sort_direction: if tab.page_source == PageSource::Search {
            tab.search_sort_direction
        } else {
            tab.sort_direction
        },
    }
}

fn type_select_projection(app: &AppState, tab: &TabSession) -> Vec<(EntryId, String)> {
    let entries = directory_display_entries(tab);
    let by_id = entries
        .iter()
        .map(|entry| (entry.id, entry))
        .collect::<HashMap<_, _>>();
    directory_group_projections(app, tab, entries)
        .into_iter()
        .flat_map(|group| group.entries)
        .filter_map(|id| by_id.get(&id).map(|entry| (id, entry.display_name.clone())))
        .collect()
}

fn directory_group_projections(
    app: &AppState,
    tab: &TabSession,
    entries: &[FileEntry],
) -> Vec<group_projection::GroupProjection> {
    if let Some(NavigationLocation::Library(location)) = tab.visible_location()
        && let Some(library) = app
            .libraries
            .iter()
            .find(|library| library.id.as_os_str() == location.identity.as_os_str())
    {
        return library_source_group_projections(library, entries);
    }
    let preference = tab
        .visible_path()
        .map(|path| app.directory_preference(path))
        .unwrap_or(app.default_directory_view);
    group_projection::project_groups(
        entries,
        preference.group_field,
        preference.group_direction,
        GroupProjectionContext {
            language: app.language,
            now: std::time::SystemTime::now(),
            utc_offset_seconds: platform::local_utc_offset_seconds(),
        },
    )
}

fn library_source_group_projections(
    library: &platform::windows::libraries::WindowsLibrary,
    entries: &[FileEntry],
) -> Vec<group_projection::GroupProjection> {
    library
        .sources
        .iter()
        .enumerate()
        .filter_map(|(source_index, source)| {
            let source_path = source.path.as_deref()?;
            let group_entries = entries
                .iter()
                .filter(|entry| entry.library_source_index == Some(source_index))
                .map(|entry| entry.id)
                .collect();
            Some(group_projection::GroupProjection {
                key: library_stable_id(source.shell_identity.as_os_str()),
                label: source.display_name.to_string_lossy().into_owned(),
                detail: display_path(source_path),
                entries: group_entries,
                header_visible: true,
            })
        })
        .collect()
}

fn directory_display_entries(tab: &TabSession) -> &[FileEntry] {
    if tab.retains_directory_view() {
        &tab.entries
    } else if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) {
        tab.pending_entries()
    } else if tab.has_failed_location() {
        &[]
    } else {
        &tab.entries
    }
}

fn shell_integration_projection(
    status: Option<&platform::windows::shell_integration::ShellIntegrationStatus>,
    language: Language,
) -> (bool, bool, String, Option<PathBuf>) {
    use platform::windows::shell_integration::{
        ShellIntegrationRepairReason, ShellIntegrationStatus,
    };
    match status {
        Some(ShellIntegrationStatus::Enabled { executable }) => (
            true,
            false,
            match language {
                Language::Chinese => "已启用".to_owned(),
                Language::English => "Enabled".to_owned(),
            },
            Some(executable.clone()),
        ),
        Some(ShellIntegrationStatus::NeedsRepair {
            bound_executable,
            reason,
        }) => (
            true,
            true,
            match (language, reason) {
                (Language::Chinese, ShellIntegrationRepairReason::ExecutableMovedOrMissing) => {
                    "程序已移动或不存在，需要修复".to_owned()
                }
                (Language::Chinese, ShellIntegrationRepairReason::RegistryChanged) => {
                    "Windows 配置已变化，需要修复".to_owned()
                }
                (Language::English, ShellIntegrationRepairReason::ExecutableMovedOrMissing) => {
                    "The program moved or is missing; repair is required".to_owned()
                }
                (Language::English, ShellIntegrationRepairReason::RegistryChanged) => {
                    "Windows settings changed; repair is required".to_owned()
                }
            },
            Some(bound_executable.clone()),
        ),
        Some(ShellIntegrationStatus::Disabled) => (
            false,
            false,
            match language {
                Language::Chinese => "未启用".to_owned(),
                Language::English => "Disabled".to_owned(),
            },
            None,
        ),
        None => (
            false,
            false,
            match language {
                Language::Chinese => "正在读取…".to_owned(),
                Language::English => "Checking…".to_owned(),
            },
            None,
        ),
    }
}
fn dismiss_stale_rename(ui: &AppWindow, state: &SharedSessions, window_id: WindowId) {
    if !ui.get_rename_editing() {
        return;
    }
    let mut app = state.lock().expect("app state mutex is not poisoned");
    let current = app.rename_targets.get(&window_id).is_some_and(|target| {
        app.window(window_id).is_some_and(|window| {
            window.active_tab == target.tab.tab_id
                && window.tabs.get(&target.tab.tab_id).is_some_and(|tab| {
                    tab.latest_request() == target.tab.request_id
                        && tab.visible_entry(target.entry_id).is_some()
                })
        })
    });
    if !current {
        app.rename_targets.remove(&window_id);
        drop(app);
        ui.set_rename_editing(false);
        ui.set_rename_submitting(false);
    }
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
fn file_row(
    entry: &FileEntry,
    tab: &TabSession,
    texts: Texts,
    app: &AppState,
    grid_requested_px: Option<u32>,
) -> FileRow {
    debug_assert!(
        entry.webdav.is_some() || {
            let path = entry.path();
            let leaf = if crate::network::is_unc_path(&path) {
                crate::network::unc_leaf_name(&path)
            } else {
                path.file_name().map(ToOwned::to_owned)
            };
            leaf.as_deref() == Some(entry.original_name.as_os_str())
        },
        "entry identity must retain its original file name"
    );
    let (icon, thumbnail_state) = file_images::row_image(entry, tab, app, grid_requested_px);
    file_row_content(
        entry,
        texts,
        tab.selected.contains(&entry.id),
        tab.focused == Some(entry.id),
        app.cut_paths.contains(&entry.path()),
        icon,
        thumbnail_state,
        tab.folder_sizes.state_for(entry),
    )
}

#[allow(clippy::too_many_arguments)]
fn file_row_content(
    entry: &FileEntry,
    texts: Texts,
    selected: bool,
    focused: bool,
    cut: bool,
    icon: Image,
    thumbnail_state: i32,
    folder_size: FolderSizeState,
) -> FileRow {
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
    FileRow {
        id: entry.id.0 as i32,
        loaded: true,
        group_header: false,
        group_label: "".into(),
        group_detail: "".into(),
        group_count: 0,
        name: entry.display_name.clone().into(),
        name_segments: ModelRc::new(VecModel::from(name_segments)),
        kind: if entry.kind == crate::domain::EntryKind::File {
            entry
                .webdav
                .as_ref()
                .map(|item| Path::new(item.path.as_str()))
                .unwrap_or_else(|| Path::new(entry.path.file_name().unwrap_or_default()))
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
            texts.folder_size(folder_size).into()
        } else {
            texts.size(entry.size_bytes).into()
        },
        modified: texts.modified(entry.modified).into(),
        created: texts.modified(entry.created).into(),
        is_directory: entry.kind == crate::domain::EntryKind::Directory,
        selected,
        focused,
        cut,
        thumbnail_state,
        icon,
    }
}

fn shell_icon_image(icon: &platform::windows::shell_icons::ShellIconRgba) -> Image {
    file_images::image(icon)
}

fn status_text(tab: &TabSession, texts: Texts) -> String {
    if tab.load_state == LoadState::Failed
        && matches!(tab.visible_location(), Some(NavigationLocation::WebDav(_)))
        && let Some(message) = tab.error.as_deref()
    {
        return message.to_owned();
    }
    if tab.error.as_deref() == Some("network_slow")
        && matches!(tab.load_state, LoadState::Loading | LoadState::Partial)
    {
        return match texts.language {
            Language::Chinese => "网络连接较慢，仍在等待…".to_owned(),
            Language::English => "The network connection is slow. Still waiting…".to_owned(),
        };
    }
    if let Some(message) = tab
        .error
        .as_deref()
        .and_then(|value| value.strip_prefix("reveal_target_missing:"))
    {
        return message.to_owned();
    }
    if let Some(failed) = tab
        .error
        .as_deref()
        .and_then(|value| value.strip_prefix("library_sources:"))
        .and_then(|value| value.parse::<usize>().ok())
    {
        return texts.library_partial_sources_failed(failed);
    }
    if let Some(progress) = tab.folder_sizes.progress()
        && progress.completed < progress.total
    {
        return match texts.language {
            Language::Chinese => format!(
                "正在获取文件夹大小（已完成 {}/{}）",
                progress.completed, progress.total
            ),
            Language::English => format!(
                "Getting folder sizes ({}/{})",
                progress.completed, progress.total
            ),
        };
    }
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
    match tab.displayed_load_state() {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EverythingOperation {
    Discover,
    Pick,
    Start,
    Test,
}

fn begin_everything_operation(
    state: &SharedSessions,
    operation: EverythingOperation,
) -> Option<u64> {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    if app.everything.busy {
        return None;
    }
    app.everything.busy = true;
    app.everything.status = everything_progress_text(app.language, operation);
    Some(app.everything.generation)
}

fn everything_progress_text(language: Language, operation: EverythingOperation) -> String {
    match (language, operation) {
        (Language::Chinese, EverythingOperation::Discover) => "正在自动发现 Everything…",
        (Language::Chinese, EverythingOperation::Pick) => "正在选择 Everything 程序…",
        (Language::Chinese, EverythingOperation::Start) => "正在启动 Everything…",
        (Language::Chinese, EverythingOperation::Test) => "正在测试连接…",
        (Language::English, EverythingOperation::Discover) => "Detecting Everything…",
        (Language::English, EverythingOperation::Pick) => "Choosing the Everything program…",
        (Language::English, EverythingOperation::Start) => "Starting Everything…",
        (Language::English, EverythingOperation::Test) => "Testing connection…",
    }
    .to_owned()
}

fn everything_picker_error_text(language: Language) -> &'static str {
    match language {
        Language::Chinese => "无法打开程序选择器。请手动输入 Everything64.exe 的路径。",
        Language::English => {
            "Unable to open the program picker. Enter the path to Everything64.exe manually."
        }
    }
}

fn everything_error_text(
    language: Language,
    error: &platform::windows::everything::EverythingError,
) -> String {
    use platform::windows::everything::EverythingError;
    match (language, error) {
        (Language::Chinese, EverythingError::NotConfigured) => {
            "尚未配置 Everything。请选择 Everything64.exe，或使用自动发现。"
        }
        (Language::Chinese, EverythingError::InvalidExecutable(_)) => {
            "程序路径无效。请选择 Everything 1.5 x64 的 Everything64.exe。"
        }
        (Language::Chinese, EverythingError::NotRunning(_)) => {
            "该 Everything 实例未运行。请启动后重试，并确认实例名一致。"
        }
        (Language::Chinese, EverythingError::Timeout) => {
            "连接 Everything 超时。请确认程序已完成启动，且实例名一致。"
        }
        (Language::Chinese, EverythingError::UnsupportedVersion(_)) => {
            "Everything 版本不受支持。请安装 Everything 1.5 x64。"
        }
        (Language::Chinese, EverythingError::UnsupportedArchitecture) => {
            "Everything 架构不受支持。请使用 Everything 1.5 x64。"
        }
        (Language::Chinese, EverythingError::DatabaseNotLoaded) => {
            "Everything 数据库尚未加载。请等待加载完成后重新测试。"
        }
        (Language::Chinese, EverythingError::StartFailed(_)) => {
            "启动 Everything 失败。请检查程序路径，或手动启动后重试。"
        }
        (Language::Chinese, EverythingError::FolderSizePipeUnavailable(_)
            | EverythingError::FolderSizeDisconnected
            | EverythingError::FolderSizeRejected(_)) => {
                "Everything 文件夹大小索引不可用。请在 Everything 中启用文件夹大小索引，并确认该目录已收录。"
            }
        (Language::Chinese, _) => {
            "无法连接 Everything。请检查程序路径、实例名和运行状态后重试。"
        }
        (Language::English, EverythingError::NotConfigured) => {
            "Everything is not configured. Choose Everything64.exe or use Auto-detect."
        }
        (Language::English, EverythingError::InvalidExecutable(_)) => {
            "The program path is invalid. Choose Everything64.exe from Everything 1.5 x64."
        }
        (Language::English, EverythingError::NotRunning(_)) => {
            "The configured Everything instance is not running. Start it and confirm the instance name matches."
        }
        (Language::English, EverythingError::Timeout) => {
            "The Everything connection timed out. Wait for startup to finish and confirm the instance name."
        }
        (Language::English, EverythingError::UnsupportedVersion(_)) => {
            "This Everything version is unsupported. Install Everything 1.5 x64."
        }
        (Language::English, EverythingError::UnsupportedArchitecture) => {
            "This Everything architecture is unsupported. Use Everything 1.5 x64."
        }
        (Language::English, EverythingError::DatabaseNotLoaded) => {
            "The Everything database is not loaded yet. Wait for it to finish, then test again."
        }
        (Language::English, EverythingError::StartFailed(_)) => {
            "Everything could not be started. Check the program path or start it manually, then retry."
        }
        (Language::English, EverythingError::FolderSizePipeUnavailable(_)
            | EverythingError::FolderSizeDisconnected
            | EverythingError::FolderSizeRejected(_)) => {
                "Everything folder-size indexing is unavailable. Enable folder-size indexing and include this folder."
            }
        (Language::English, _) => {
            "Unable to connect to Everything. Check the program path, instance name, and running state."
        }
    }
    .to_owned()
}
fn everything_connected_status(
    language: Language,
    status: &platform::windows::everything::EverythingStatus,
) -> String {
    let database = match (language, status.database_loaded) {
        (Language::Chinese, true) => "数据库已加载",
        (Language::Chinese, false) => "数据库未加载",
        (Language::English, true) => "Database loaded",
        (Language::English, false) => "Database not loaded",
    };
    let folder_size_status = match (language, status.folder_size_indexed) {
        (Language::Chinese, true) => "文件夹大小索引已启用",
        (Language::Chinese, false) => "文件夹大小索引未启用",
        (Language::English, true) => "Folder-size indexing enabled",
        (Language::English, false) => "Folder-size indexing disabled",
    };
    let instance = if status.instance_name.is_empty() {
        match language {
            Language::Chinese => "默认实例",
            Language::English => "Default instance",
        }
        .to_owned()
    } else {
        status.instance_name.clone()
    };
    match language {
        Language::Chinese => format!(
            "连接成功 · 版本 {} · 实例 {} · {database} · {folder_size_status}",
            status.version, instance
        ),
        Language::English => format!(
            "Connected · Version {} · Instance {} · {database} · {folder_size_status}",
            status.version, instance
        ),
    }
}

fn apply_everything_discovery(
    app: &mut AppState,
    generation: u64,
    mut config: crate::domain::EverythingConfig,
    status: &platform::windows::everything::EverythingStatus,
) -> bool {
    if generation != app.everything.generation {
        return false;
    }
    config.verified_version = Some(status.version.to_string());
    app.everything.config = config;
    app.everything.status = everything_connected_status(app.language, status);
    app.everything.folder_sizes_indexed = Some(status.folder_size_indexed);
    true
}

fn search_error_page_text(state: SearchState, texts: Texts) -> (&'static str, &'static str) {
    match (texts.language, state) {
        (Language::Chinese, SearchState::NotConfigured) => (
            "需要安装并配置 Everything",
            "AsterFiles 搜索依赖 Everything 1.5 x64。请先安装 Everything，再到设置中确认程序路径和实例。",
        ),
        (Language::English, SearchState::NotConfigured) => (
            "Install and configure Everything",
            "AsterFiles search requires Everything 1.5 x64. Install Everything, then confirm its program path and instance in Settings.",
        ),
        (Language::Chinese, SearchState::Disconnected) => (
            "无法连接 Everything",
            "Everything 可能尚未运行，或程序路径与实例配置不正确。请启动 Everything，或前往设置检查配置。",
        ),
        (Language::English, SearchState::Disconnected) => (
            "Can't connect to Everything",
            "Everything may not be running, or its program path and instance may be incorrect. Start Everything or check its settings.",
        ),
        (Language::Chinese, SearchState::NotIndexed) => (
            "Everything 索引尚未就绪",
            "请等待 Everything 完成索引，并在 Everything 设置中确认索引功能已启用。",
        ),
        (Language::English, SearchState::NotIndexed) => (
            "Everything index isn't ready",
            "Wait for Everything to finish indexing and confirm indexing is enabled in Everything settings.",
        ),
        (Language::Chinese, SearchState::UnsupportedVersion) => (
            "Everything 版本不受支持",
            "AsterFiles 需要 Everything 1.5。请安装兼容版本，并前往设置重新发现或检查配置。",
        ),
        (Language::English, SearchState::UnsupportedVersion) => (
            "Everything version isn't supported",
            "AsterFiles requires Everything 1.5. Install a compatible version, then auto-detect it or check the configuration in Settings.",
        ),
        (Language::Chinese, SearchState::UnsupportedArchitecture) => (
            "需要 Everything x64",
            "当前 Everything 位数不兼容。请安装 Everything 1.5 x64，并前往设置重新配置。",
        ),
        (Language::English, SearchState::UnsupportedArchitecture) => (
            "Everything x64 is required",
            "The installed Everything architecture is incompatible. Install Everything 1.5 x64, then update the configuration in Settings.",
        ),
        (Language::Chinese, SearchState::SyntaxError) => {
            ("搜索表达式有误", "请检查搜索内容后重试。")
        }
        (Language::English, SearchState::SyntaxError) => (
            "Invalid search expression",
            "Check the search text and try again.",
        ),
        (Language::Chinese, SearchState::TimedOut) => {
            ("搜索超时", "Everything 未及时响应，请稍后重试。")
        }
        (Language::English, SearchState::TimedOut) => (
            "Search timed out",
            "Everything did not respond in time. Try again.",
        ),
        (Language::Chinese, _) => ("搜索失败", "Everything 搜索发生错误，请重试。"),
        (Language::English, _) => ("Search failed", "Everything search failed. Try again."),
    }
}
fn apply_ui_texts(ui: &AppWindow, language: Language) {
    ui.global::<UiTexts>()
        .set_values(Texts::new(language).ui_texts());
}

fn display_path(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
}

fn navigation_display_name(location: &NavigationLocation, language: Language) -> String {
    match location {
        NavigationLocation::Home => match language {
            Language::Chinese => "主页".to_owned(),
            Language::English => "Home".to_owned(),
        },
        _ => location.display_name(),
    }
}

fn network_location_default_name(path: &Path) -> String {
    path.file_name()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
        .or_else(|| crate::network::unc_host_key(path))
        .unwrap_or_else(|| display_path(path))
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
mod issue_139_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "drag_safety_tests.rs"]
mod drag_safety_tests;

#[cfg(test)]
#[path = "grouped_grid_tests.rs"]
mod grouped_grid_tests;

#[cfg(test)]
mod navigation_transition_tests;
