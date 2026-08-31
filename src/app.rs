use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use slint::{
    Image, Model, ModelNotify, ModelRc, ModelTracker, Rgba8Pixel, SharedPixelBuffer, VecModel,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{
    agent_debug::{self, AgentScenario},
    domain::{
        AddressMode, EntryId, FileEntry, FolderSizeState, LoadState, NameHighlightSegment,
        NavigationKind, PageSource, RequestId, SearchDepth, SearchScope, SearchState,
        SortDirection, SortField, TabId, TabKind, TabSession,
        file_operations::{
            FileOperationKind, ItemState, OperationId, OperationItem, OperationManager,
            OperationResult, OperationState,
        },
    },
    fs::{ReadOutcome, read_directory_batches},
    i18n::{Language, Texts},
    platform::{self, KnownLocation, KnownLocationKind},
    session_store,
};

slint::include_modules!();

const WORKER_COUNT: usize = 4;
const ICON_WORKER_COUNT: usize = 2;

type SharedSessions = Arc<Mutex<AppState>>;

#[derive(Debug)]
struct AppState {
    tabs: HashMap<TabId, TabSession>,
    tab_order: Vec<TabId>,
    active_tab: TabId,
    closed_tabs: VecDeque<PathBuf>,
    next_tab_id: u32,
    language: Language,
    theme_mode: session_store::ThemeMode,
    system_dark_theme: bool,
    icons: HashMap<(TabId, RequestId, EntryId), platform::windows_shell_icons::ShellIconRgba>,
    icon_cache: HashMap<PathBuf, platform::windows_shell_icons::ShellIconRgba>,
    sidebar_icons: HashMap<PathBuf, platform::windows_shell_icons::ShellIconRgba>,
    sidebar: Vec<KnownLocation>,
    column_order: [u8; 4],
    column_widths: session_store::ColumnWidths,
    operations: OperationManager,
    operation_errors: Vec<String>,
    rename_target: Option<(TabId, EntryId)>,
    rename_extension: Option<std::ffi::OsString>,
    focus_after_refresh: HashMap<TabId, PathBuf>,
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
}

impl AppState {
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
        let mut tabs = HashMap::new();
        let mut tab_order = Vec::new();
        for (index, path) in initial_paths.into_iter().enumerate() {
            let id = TabId(index as u32 + 1);
            let mut tab = TabSession::new(id);
            tab.current_path = Some(path);
            tabs.insert(id, tab);
            tab_order.push(id);
        }
        let active_tab = tab_order[active_index.min(tab_order.len() - 1)];
        let next_tab_id = tab_order.len() as u32 + 1;
        Self {
            tabs,
            tab_order,
            active_tab,
            closed_tabs: VecDeque::new(),
            next_tab_id,
            language,
            theme_mode,
            system_dark_theme,
            icons: HashMap::new(),
            icon_cache: HashMap::new(),
            sidebar_icons: HashMap::new(),
            sidebar: Vec::new(),
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
        }
    }

    fn duplicate_active_tab(&mut self) -> Option<TabId> {
        let source_id = self.active_tab;
        let source = self.tabs.get(&source_id)?;
        if source.kind != TabKind::Files || source.load_state != LoadState::Complete {
            return None;
        }
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let tab = TabSession::duplicate_complete(id, source);
        self.tabs.insert(id, tab);
        self.tab_order.push(id);
        self.active_tab = id;
        Some(id)
    }
    fn create_tab(&mut self, path: PathBuf) -> TabId {
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let mut tab = TabSession::new(id);
        tab.current_path = Some(path);
        self.tabs.insert(id, tab);
        self.tab_order.push(id);
        self.active_tab = id;
        id
    }

    fn open_settings(&mut self) -> TabId {
        if let Some(id) = self.tab_order.iter().copied().find(|id| {
            self.tabs
                .get(id)
                .is_some_and(|tab| tab.kind == TabKind::Settings)
        }) {
            self.active_tab = id;
            return id;
        }
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        self.tabs.insert(id, TabSession::new_settings(id));
        self.tab_order.push(id);
        self.active_tab = id;
        id
    }

    fn close_tab(&mut self, closing: TabId) -> Option<TabId> {
        if self.tab_order.len() == 1 {
            return None;
        }
        let closing_kind = self.tabs.get(&closing)?.kind;
        if closing_kind == TabKind::Files
            && self
                .tabs
                .values()
                .filter(|tab| tab.kind == TabKind::Files)
                .count()
                == 1
        {
            return None;
        }
        let index = self.tab_order.iter().position(|id| *id == closing)?;
        let closing_was_active = closing == self.active_tab;
        if let Some(mut tab) = self.tabs.remove(&closing) {
            tab.cancel_pending();
            self.icons.retain(|(tab_id, _, _), _| *tab_id != closing);
            if tab.kind == TabKind::Files
                && let Some(path) = tab.current_path.take()
            {
                self.closed_tabs.push_front(path);
                self.closed_tabs.truncate(10);
            }
        }
        self.tab_order.remove(index);
        if closing_was_active {
            self.active_tab = self.tab_order[index.min(self.tab_order.len() - 1)];
        }
        Some(self.active_tab)
    }

    fn restore_closed(&mut self) -> Option<(TabId, PathBuf)> {
        let path = self.closed_tabs.pop_front()?;
        let tab_id = self.create_tab(path.clone());
        Some((tab_id, path))
    }

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
        let active = self.active_tab;
        let mut file_index = 0;
        for id in &self.tab_order {
            let Some(tab) = self.tabs.get(id) else {
                continue;
            };
            if tab.kind != TabKind::Files {
                continue;
            }
            if *id == active {
                return file_index;
            }
            file_index += 1;
        }
        file_index.saturating_sub(1)
    }

    fn dark_theme(&self) -> bool {
        match self.theme_mode {
            session_store::ThemeMode::System => self.system_dark_theme,
            session_store::ThemeMode::Light => false,
            session_store::ThemeMode::Dark => true,
        }
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

struct SearchFileModel {
    total: usize,
    rows: std::cell::RefCell<HashMap<usize, FileRow>>,
    placeholder: FileRow,
    notify: ModelNotify,
}

impl SearchFileModel {
    fn new(total: usize, placeholder: FileRow) -> Self {
        Self {
            total,
            rows: std::cell::RefCell::new(HashMap::new()),
            placeholder,
            notify: ModelNotify::default(),
        }
    }

    fn update_page(&self, offset: usize, rows: Vec<FileRow>) {
        let mut slots = self.rows.borrow_mut();
        let mut changed = Vec::new();
        for (index, row) in rows.into_iter().enumerate() {
            let target = offset + index;
            if target < self.total {
                slots.insert(target, row);
                changed.push(target);
            }
        }
        drop(slots);
        for target in changed {
            self.notify.row_changed(target);
        }
    }

    fn update_rows(&self, rows: Vec<FileRow>) {
        let mut slots = self.rows.borrow_mut();
        let mut changed = Vec::new();
        for row in rows {
            let Some(target) = row.id.checked_sub(1).map(|id| id as usize) else {
                continue;
            };
            if target < self.total {
                slots.insert(target, row);
                changed.push(target);
            }
        }
        drop(slots);
        for target in changed {
            self.notify.row_changed(target);
        }
    }

    fn clear_page(&self, offset: usize, count: usize) {
        let end = offset.saturating_add(count).min(self.total);
        let mut slots = self.rows.borrow_mut();
        for target in offset..end {
            slots.remove(&target);
        }
        drop(slots);
        for target in offset..end {
            self.notify.row_changed(target);
        }
    }
}

impl Model for SearchFileModel {
    type Data = FileRow;

    fn row_count(&self) -> usize {
        self.total
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        (row < self.total).then(|| {
            self.rows
                .borrow()
                .get(&row)
                .cloned()
                .unwrap_or_else(|| self.placeholder.clone())
        })
    }

    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
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
        completed_paths: Vec<PathBuf>,
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
        column_order,
        search_column_order,
        column_widths,
        search_column_widths,
        everything_config,
        theme_mode,
        language,
    ) = restored
        .filter(|session| !session.tab_paths.is_empty())
        .map(|session| {
            let window = if session.window.width > 7_680 || session.window.height > 4_320 {
                default_window
            } else {
                session.window
            };
            (
                session.tab_paths,
                session.active_tab,
                window,
                session.column_order,
                session.search_column_order,
                session.column_widths,
                session.search_column_widths,
                session.everything,
                session.theme_mode,
                session.language,
            )
        })
        .unwrap_or_else(|| {
            (
                vec![initial_path()],
                0,
                default_window,
                [0, 1, 2, 3],
                [0, 1, 2, 3],
                session_store::DEFAULT_COLUMN_WIDTHS,
                session_store::DEFAULT_SEARCH_COLUMN_WIDTHS,
                crate::domain::EverythingConfig::default(),
                session_store::ThemeMode::System,
                Language::Chinese,
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
    if let Some(scenario) = scenario {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let active_tab = app.active_tab;
        agent_debug::apply_scenario(
            app.tabs
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

    wire_callbacks(
        &ui,
        &delete_ui,
        &conflict_ui,
        &exit_ui,
        request_sender.clone(),
        operation_sender.clone(),
        clipboard_sender,
        everything_sender.clone(),
        state.clone(),
    );
    wire_mouse_navigation(&ui, &exit_ui, state.clone());
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
        operation_sender,
        request_sender.clone(),
        state.clone(),
    );
    scan_cleanup_diagnostics(&ui, state.clone());
    {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.sidebar = platform::known_locations();
    }
    start_sidebar_icon_loader(&ui, state.clone());
    refresh_ui(&ui, &state);
    refresh_operation_window(&operation_ui, &state);
    refresh_confirmation_windows(&delete_ui, &conflict_ui, &exit_ui, &state);
    let initial_tabs = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.tab_order
            .iter()
            .filter_map(|id| {
                app.tabs
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

    let result = ui.run();
    for weak in [delete_weak, conflict_weak, exit_weak] {
        if let Some(window) = weak.upgrade() {
            let _ = window.hide();
        }
    }
    let (
        paths,
        active_tab,
        column_order,
        search_column_order,
        column_widths,
        search_column_widths,
        everything_config,
        theme_mode,
        language,
    ) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        let active_tab = app.stable_active_path_index();
        (
            app.stable_paths(),
            active_tab,
            app.column_order,
            app.search_column_order,
            app.column_widths,
            app.search_column_widths,
            app.everything_config.clone(),
            app.theme_mode,
            app.language,
        )
    };
    let position = ui.window().position();
    let size = ui.window().size();
    if scenario.is_none()
        && let Some(path) = session_store::default_path()
        && let Ok(session) = session_store::SessionState::with_everything_settings(
            session_store::WindowPlacement {
                x: position.x,
                y: position.y,
                width: (size.width as f32 / ui.window().scale_factor()).round() as u32,
                height: (size.height as f32 / ui.window().scale_factor()).round() as u32,
            },
            active_tab,
            paths,
            column_order,
            search_column_order,
            column_widths,
            search_column_widths,
            theme_mode,
            language,
            everything_config,
        )
    {
        let _ = session_store::save(&path, &session);
    }
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
        let Some(tab) = app.tabs.get_mut(&tab_id) else {
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
            cancel,
        }
    };
    sender.send(request).is_ok()
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
    window_y: f32,
    list_top: f32,
    viewport_y: f32,
) -> (Option<EntryId>, bool) {
    const ROW_HEIGHT: f32 = 40.0;
    let app = state.lock().expect("app state mutex is not poisoned");
    if window_y < list_top {
        return (None, true);
    }
    let index = ((window_y - list_top + (-viewport_y).max(0.0)) / ROW_HEIGHT).floor() as usize;
    let active = app.active();
    let entry = if active.page_source == PageSource::Search {
        u32::try_from(index)
            .ok()
            .and_then(|index| index.checked_add(1))
            .and_then(|id| active.visible_entry(EntryId(id)))
            .map(|entry| entry.id)
    } else {
        active.visible_entries().get(index).map(|entry| entry.id)
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
        let tab = app.active_tab;
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
        app.rename_target = Some((app.active_tab, id));
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

fn submit_rename(state: &SharedSessions, sender: &mpsc::Sender<FileOperationRequest>, name: &str) {
    let item = {
        let app = state.lock().expect("app state mutex is not poisoned");
        let target = app.rename_target;
        target
            .and_then(|(tab_id, id)| app.tabs.get(&tab_id)?.visible_entry(id))
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
    if let Some(item) = item {
        enqueue_operation(state, sender, FileOperationKind::Rename, vec![item]);
    }
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
                    if let Some(ui) = weak.upgrade() {
                        refresh_ui(&ui, &state_for_error);
                    }
                });
            }
        }
    });
}

fn native_window_handle(ui: &AppWindow) -> isize {
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
    state: SharedSessions,
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
            (app.active_tab, app.active().address_mode)
        };
        let query = if mode == AddressMode::Smart {
            let mut app = state_for_path
                .lock()
                .expect("app state mutex is not poisoned");
            app.tabs
                .get_mut(&tab_id)
                .map(|tab| {
                    tab.update_address_input(input.clone());
                    tab.search_query.clone()
                })
                .unwrap_or_else(|| input.clone())
        } else {
            input.clone()
        };
        let target = PathBuf::from(path.as_str());
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
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
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
            let tab_id = app.active_tab;
            let tab = app.tabs.get_mut(&tab_id).expect("active tab exists");
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
                    app.tabs.get(&tab_id).is_some_and(|tab| {
                        tab.address_input == input && tab.address_mode == AddressMode::Smart
                    })
                });
                if still_current {
                    let query = state
                        .lock()
                        .ok()
                        .and_then(|app| app.tabs.get(&tab_id).map(|tab| tab.search_query.clone()))
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
            let tab_id = app.active_tab;
            let tab = app.tabs.get_mut(&tab_id).expect("active tab exists");
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
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.cancel_address_edit();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_cancel_edit);
        }
    });

    let state_for_next_search_page = state.clone();
    let everything_for_next_search_page = everything_sender.clone();
    ui.on_request_search_page(move |offset| {
        let tab_id = state_for_next_search_page
            .lock()
            .expect("app state mutex is not poisoned")
            .active_tab;
        submit_search_page(
            &everything_for_next_search_page,
            &state_for_next_search_page,
            tab_id,
            offset.max(0) as u32,
        );
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
                        app.active_tab,
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
                .map(|(_, path)| (app.active_tab, path))
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
                .map(|location| (app.active_tab, location.path.clone()))
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
            (app.active_tab, app.active().latest_request)
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
                        app.active_tab,
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
            let tab_id = app.active_tab;
            let Some(tab) = app.tabs.get_mut(&tab_id) else {
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
            update_file_rows(&ui, &state_for_activate_entry, &changed_rows);
            update_selection_summary(&ui, &state_for_activate_entry);
        }
    });
    let weak = ui.as_weak();
    let state_for_select = state.clone();
    ui.on_select_entry(move |entry_id, toggle, extend| {
        let changed_rows = {
            let mut app = state_for_select
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_tab;
            let Some(tab) = app.tabs.get_mut(&tab_id) else {
                return;
            };
            let previous_selected = tab.selected.clone();
            let previous_focused = tab.focused;
            tab.select_entry(EntryId(entry_id as u32), toggle, extend);
            previous_selected
                .into_iter()
                .chain(tab.selected.iter().copied())
                .chain(previous_focused)
                .chain(tab.focused)
                .collect::<std::collections::HashSet<_>>()
        };
        if let Some(ui) = weak.upgrade() {
            update_file_rows(&ui, &state_for_select, &changed_rows);
            update_selection_summary(&ui, &state_for_select);
        }
    });

    let weak = ui.as_weak();
    let state_for_clear = state.clone();
    ui.on_clear_selection(move || {
        let mut app = state_for_clear
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.clear_selection();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_clear);
        }
    });

    let weak = ui.as_weak();
    let state_for_all = state.clone();
    ui.on_select_all(move || {
        let mut app = state_for_all
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.select_all();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_all);
        }
    });

    let weak = ui.as_weak();
    let state_for_focus = state.clone();
    ui.on_move_focus(move |delta, extend| {
        let mut app = state_for_focus
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.move_focus(delta as isize, extend);
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_focus);
        }
    });

    let weak = ui.as_weak();
    let state_for_boundary = state.clone();
    ui.on_focus_boundary(move |last, extend| {
        let mut app = state_for_boundary
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.focus_boundary(last, extend);
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_boundary);
        }
    });

    let weak = ui.as_weak();
    let state_for_toggle = state.clone();
    ui.on_toggle_focused(move || {
        let mut app = state_for_toggle
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.toggle_focused();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_toggle);
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
        let tab_id = app.active_tab;
        let search_query = if let Some(tab) = app.tabs.get_mut(&tab_id) {
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
            app.active_tab
        } else {
            TabId(tab_id as u32)
        };
        if app.tabs.contains_key(&target) {
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
        if app.tabs.contains_key(&id) {
            app.active_tab = id;
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_activate);
        }
    });

    let weak = ui.as_weak();
    let sender_for_back = sender.clone();
    let state_for_back = state.clone();
    ui.on_navigate_back(move || {
        let (restored, target) = {
            let mut app = state_for_back
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_tab;
            if app
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
                .map(|path| (app.active_tab, path))
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
                .map(|path| (app.active_tab, path))
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
                .map(|path| (app.active_tab, path))
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
                    .map(|path| (app.active_tab, path))
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
        let tab_id = app.active_tab;
        if entry_id >= 0 {
            let id = EntryId(entry_id as u32);
            if let Some(tab) = app.tabs.get_mut(&tab_id)
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
            let tab_id = app.active_tab;
            if let Some(tab) = app.tabs.get_mut(&tab_id) {
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
        let (entry_id, background) = context_target_at(
            &state_for_reopen_menu,
            y,
            weak.upgrade().map_or(0.0, |ui| ui.get_file_list_top()),
            weak.upgrade().map_or(0.0, |ui| ui.get_file_viewport_y()),
        );
        *anchor_for_reopen.lock().expect("context anchor mutex") =
            (background, x.round() as i32, y.round() as i32);
        let _ = clipboard_for_reopen.send(ClipboardRequest::CheckAvailability);
        if let Ok(mut app) = state_for_reopen_menu.lock() {
            let tab_id = app.active_tab;
            if let Some(tab) = app.tabs.get_mut(&tab_id) {
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
    let state_for_commit_rename = state.clone();
    let sender_for_rename = operation_sender.clone();
    ui.on_commit_rename(move |name| {
        submit_rename(&state_for_commit_rename, &sender_for_rename, name.as_str());
    });
    let weak = ui.as_weak();
    let state_for_cancel_rename = state.clone();
    ui.on_cancel_rename(move || {
        if let Ok(mut app) = state_for_cancel_rename.lock() {
            app.rename_target = None;
            app.rename_extension = None;
        }
        if let Some(ui) = weak.upgrade() {
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
        if state_for_close
            .lock()
            .is_ok_and(|app| app.operations.has_active_tasks())
        {
            if let (Some(ui), Some(exit_ui)) = (weak.upgrade(), exit_weak.upgrade()) {
                show_confirmation_window(&ui, None, &exit_ui);
            }
        } else if let Some(ui) = weak.upgrade() {
            let _ = ui.hide();
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

fn wire_mouse_navigation(ui: &AppWindow, exit_ui: &ConfirmationWindow, state: SharedSessions) {
    use winit::{
        event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
        keyboard::{Key, ModifiersState, NamedKey},
    };

    let weak = ui.as_weak();
    let exit_weak = exit_ui.as_weak();
    let modifiers = Cell::new(ModifiersState::empty());
    let cursor_position = Cell::new(winit::dpi::PhysicalPosition::new(0.0, 0.0));
    ui.window().on_winit_window_event(move |_, event| {
        if matches!(event, WindowEvent::CloseRequested) {
            if state
                .lock()
                .is_ok_and(|app| app.operations.has_active_tasks())
            {
                if let Some(ui) = weak.upgrade()
                    && let Some(exit_ui) = exit_weak.upgrade()
                {
                    show_confirmation_window(&ui, None, &exit_ui);
                }
                return EventResult::PreventDefault;
            }
            return EventResult::Propagate;
        }
        if should_close_context_menu(event) {
            if let Some(ui) = weak.upgrade() {
                ui.set_context_menu_open(false);
            }
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
            return EventResult::Propagate;
        }
        if matches!(event, WindowEvent::Focused(false)) {
            platform::windows::window_trace::log_request(
                native_window_handle(&ui),
                "winit-focused-false",
            );
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
                let visible_height = (window_height - ui.get_file_list_top() - 30.0).max(0.0);
                let maximum = (ui.get_files().row_count() as f32 * 40.0 - visible_height).max(0.0);
                let viewport = (ui.get_file_viewport_y() + delta).clamp(-maximum, 0.0);
                ui.set_file_viewport_y(viewport);
                if ui.get_search_results_mode() {
                    ui.invoke_request_search_page(
                        ((-viewport).max(0.0) / 40.0) as i32 / SEARCH_PAGE_LIMIT as i32
                            * SEARCH_PAGE_LIMIT as i32,
                    );
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
                        ui.invoke_clear_selection();
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
                state,
                button: MouseButton::Back,
                ..
            } => {
                if *state == ElementState::Released && ui.get_can_navigate_back() {
                    let weak = ui.as_weak();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = weak.upgrade() {
                            ui.invoke_navigate_back();
                        }
                    });
                }
                EventResult::PreventDefault
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Forward,
                ..
            } => {
                if *state == ElementState::Released && ui.get_can_navigate_forward() {
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
    let ui_weak = ui.as_weak();
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
        if let Some(ui) = ui_weak.upgrade() {
            refresh_ui(&ui, &state_for_conflict);
        }
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
        let ui_weak = ui.as_weak();
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
            if let Some(ui) = ui_weak.upgrade() {
                refresh_ui(&ui, &state_for_conflict);
            }
        };
        match action_index {
            0 => conflict_ui.on_primary_action(callback),
            1 => conflict_ui.on_secondary_action(callback),
            _ => conflict_ui.on_tertiary_action(callback),
        }
    }

    let exit_weak = exit_ui.as_weak();
    let ui_weak = ui.as_weak();
    exit_ui.on_safe_cancel(move || {
        let demo_mode = exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if !demo_mode && let Some(ui) = ui_weak.upgrade() {
            ui.set_task_center_open(true);
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
    let ui_weak = ui.as_weak();
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
            && let Some(ui) = ui_weak.upgrade()
        {
            let _ = ui.hide();
        }
    });
    let exit_weak = exit_ui.as_weak();
    let ui_weak = ui.as_weak();
    exit_ui.on_secondary_action(move || {
        let demo_mode = exit_weak.upgrade().is_some_and(|ui| ui.get_demo_mode());
        if let Some(exit_ui) = exit_weak.upgrade() {
            let _ = exit_ui.hide();
        }
        if !demo_mode && let Some(ui) = ui_weak.upgrade() {
            ui.set_task_center_open(true);
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
    let ui_weak = ui.as_weak();
    let operation_weak = operation_ui.as_weak();
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
    let ui_weak = ui.as_weak();
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
        if let Some(ui) = ui_weak.upgrade() {
            refresh_ui(&ui, &state_for_cancel);
        }
        if let Some(operation_ui) = operation_weak.upgrade() {
            let _ = operation_ui.hide();
        }
    });

    let state_for_pause = state.clone();
    let ui_weak = ui.as_weak();
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
        if let Some(ui) = ui_weak.upgrade() {
            refresh_ui(&ui, &state_for_pause);
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
                if let Some(ui) = ui_weak.upgrade() {
                    refresh_ui(&ui, &state_for_auto_open);
                }
                refresh_operation_window(&operation_ui, &state_for_auto_open);
            }
            let should_open = state_for_auto_open.lock().is_ok_and(|app| {
                app.operations.iter().any(|task| {
                    matches!(
                        task.state,
                        OperationState::Preflight
                            | OperationState::Running
                            | OperationState::Paused
                            | OperationState::WaitingConflict
                    ) && task.started_at.elapsed() >= Duration::from_millis(800)
                })
            });
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
fn scan_cleanup_diagnostics(ui: &AppWindow, state: SharedSessions) {
    let weak = ui.as_weak();
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
                if let Some(ui) = weak.upgrade() {
                    refresh_ui(&ui, &state_for_ui);
                }
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
                    refresh_ui(&ui, &state);
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
            let mut completed_paths = Vec::new();
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
                        completed_paths.extend(report.completed_paths.iter().cloned());
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
                completed_paths,
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
                            if let Ok(mut app) = state.lock() {
                                let matching_tabs = app
                                    .tabs
                                    .values()
                                    .filter(|tab| {
                                        tab.visible_path().is_some_and(|visible| visible == parent)
                                    })
                                    .map(|tab| tab.id)
                                    .collect::<Vec<_>>();
                                for tab_id in matching_tabs {
                                    app.focus_after_refresh.insert(tab_id, path.clone());
                                }
                            }
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
                        completed_paths,
                    } => {
                        let (affected, next) = {
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
                            let affected = result.affected_directories.clone();
                            app.conflict_responses.remove(&id);
                            if let Some(origin_tab) =
                                app.operations.task(id).and_then(|task| task.origin_tab)
                                && let Some(target) = completed_paths.last().cloned()
                            {
                                let visible_path =
                                    app.tabs.get(&origin_tab).and_then(|tab| tab.visible_path());
                                if target.parent().is_some_and(|parent| {
                                    visible_path.is_some_and(|path| path == parent)
                                }) {
                                    app.focus_after_refresh.insert(origin_tab, target);
                                }
                            }
                            if let Some(target) = completed_paths.last() {
                                let matching_tabs = app
                                    .tabs
                                    .values()
                                    .filter(|tab| {
                                        target.parent().is_some_and(|parent| {
                                            tab.visible_path().is_some_and(|path| path == parent)
                                        })
                                    })
                                    .map(|tab| tab.id)
                                    .collect::<Vec<_>>();
                                for tab_id in matching_tabs {
                                    app.focus_after_refresh.insert(tab_id, target.clone());
                                }
                            }
                            let _ = app.operations.finish(id, terminal, result);
                            if cancelled {
                                app.operations.remove_terminal(id);
                            }
                            let next =
                                app.operations
                                    .start_next()
                                    .ok()
                                    .flatten()
                                    .and_then(|next_id| {
                                        let _ = app.operations.mark_running(next_id);
                                        app.operations.task(next_id).map(|task| {
                                            FileOperationRequest {
                                                id: next_id,
                                                kind: task.kind,
                                                items: task.items.clone(),
                                                cancellation: task.cancellation.clone(),
                                            }
                                        })
                                    });
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
                        ui.set_rename_editing(false);
                    }
                    refresh_ui(&ui, &state);
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
                        let _ = ui.hide();
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
fn refresh_affected_tabs(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    directories: &[PathBuf],
) {
    let targets = {
        let app = state.lock().expect("app state mutex is not poisoned");
        app.tabs
            .values()
            .filter_map(|tab| {
                tab.visible_path()
                    .filter(|path| directories.iter().any(|directory| directory == *path))
                    .map(|path| (tab.id, path.to_path_buf()))
            })
            .collect::<Vec<_>>()
    };
    for (tab, path) in targets {
        submit_navigation(sender, state, tab, path, NavigationKind::Refresh);
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
    let result = read_directory_batches(&request.path, &request.cancel, |entries| {
        let _ = events.send(DirectoryEvent::Batch {
            tab_id: request.tab_id,
            request_id: request.request_id,
            entries,
        });
    });
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
                        append_active_file_rows(&ui, &state, tab_id, request_id);
                    } else {
                        if let Some((tab_id, request_id)) = finished {
                            submit_folder_sizes(&everything_sender, &state, tab_id, request_id);
                        }
                        refresh_ui(&ui, &state);
                    }
                })
                .is_err()
            {
                break;
            }
        }
    });
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
            let accepted = app
                .tabs
                .get(&tab_id)
                .is_some_and(|tab| tab.accepts(request_id));
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
                        }),
                );
                app.tabs
                    .get_mut(&tab_id)
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
            let focus_target = app.focus_after_refresh.remove(&tab_id);
            if let Some(tab) = app.tabs.get_mut(&tab_id)
                && tab.accepts(request_id)
            {
                tab.sort_pending();
                tab.commit_pending();
                tab.commit_path(path);
                if let Some(target) = focus_target
                    && let Some(id) = tab
                        .entries
                        .iter()
                        .find(|entry| entry.path == target)
                        .map(|entry| entry.id)
                {
                    tab.select_entry(id, false, false);
                }
                tab.error = (skipped > 0).then(|| skipped.to_string());
                icon_requests.push(IconRequest {
                    tab_id,
                    request_id,
                    target: IconTarget::Location,
                    path: tab.current_path.clone().expect("committed path exists"),
                });
            }
        }
        DirectoryEvent::Cancelled { tab_id, request_id } => {
            if let Some(tab) = app.tabs.get_mut(&tab_id)
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
            if let Some(tab) = app.tabs.get_mut(&tab_id)
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

fn search_grouped_page(
    client: &platform::windows::everything::EverythingClient,
    scope: (Option<PathBuf>, bool),
    query: String,
    sort: platform::windows::everything::EverythingSort,
    offset: u32,
    limit: u32,
    timeout: Duration,
) -> Result<
    (
        Vec<platform::windows::everything::EverythingSearchItem>,
        u32,
        u32,
    ),
    platform::windows::everything::EverythingError,
> {
    use platform::windows::everything::{EverythingItemKind, EverythingSearchRequest};
    let (scope, recursive) = scope;

    let mut files = EverythingSearchRequest::new(query.clone(), scope.clone());
    files.recursive = recursive;
    files.item_kind = EverythingItemKind::Files;
    files.sort = sort;
    files.offset = offset;
    files.max_results = limit;
    let file_page = client.search(&files, timeout)?;

    let mut folders = EverythingSearchRequest::new(query, scope);
    folders.recursive = recursive;
    folders.item_kind = EverythingItemKind::Folders;
    folders.sort = sort;
    folders.offset = offset.saturating_sub(file_page.total);
    folders.max_results = limit.saturating_sub(file_page.items.len() as u32);
    let folder_page = client.search(&folders, timeout)?;

    let total = file_page.total.saturating_add(folder_page.total);
    let mut items = file_page.items;
    if offset >= file_page.total || items.len() < limit as usize {
        items.extend(folder_page.items);
    }
    items.truncate(limit as usize);
    Ok((items, total, file_page.total))
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
                        Ok((items, total, file_total)) => {
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
                                let _ = event_sender.send(EverythingEvent::SearchPage {
                                    tab_id,
                                    request_id,
                                    offset,
                                    entries,
                                    total,
                                    file_total,
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
    let Some(tab) = app.tabs.get_mut(&tab_id) else {
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
            let mut app = state.lock().expect("app state mutex is not poisoned");
            match event {
                EverythingEvent::SearchPage { tab_id, request_id, offset, entries, total, file_total } => if let Some(tab) = app.tabs.get_mut(&tab_id) && tab.accepts_page(request_id, PageSource::Search) {
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
                    let evicted = tab.merge_search_page(
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
                    let active = app.active_tab == tab_id;
                    drop(app);
                    if let Some(request) = pending_request {
                        let _ = sender_for_search_consistency.send(request);
                    }
                    if active {
                        project_search_page(
                            &ui,
                            &state,
                            tab_id,
                            request_id,
                            offset,
                            total,
                            &evicted,
                        );
                    }
                    return;
                },
                EverythingEvent::SearchFailed { tab_id, request_id, offset, error } => if let Some(tab) = app.tabs.get_mut(&tab_id) && tab.accepts_page(request_id, PageSource::Search) {
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
                    if let Some(tab) = app.tabs.get_mut(&tab_id)
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
                    apply_folder_size_event(
                        &mut app,
                        tab_id,
                        request_id,
                        entry_id,
                        &path,
                        size,
                    );
                }
                EverythingEvent::Status(result) => match result {
                    Ok(status) => { app.everything_status = format!("Everything {} · {}", status.version, if status.folder_size_indexed { "文件夹大小已索引" } else { "文件夹大小未索引" }); app.everything_folder_sizes_indexed = Some(status.folder_size_indexed); app.everything_config.verified_version = Some(status.version.to_string()); }
                    Err(error) => { app.everything_status = error.to_string(); app.everything_folder_sizes_indexed = None; }
                },
            }
            drop(app); refresh_ui(&ui, &state);
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
    evicted: &[u32],
) {
    let app = state.lock().expect("app state mutex is not poisoned");
    if app.active_tab != tab_id {
        return;
    }
    let tab = app.active();
    if tab.latest_request != request_id || tab.page_source != PageSource::Search {
        return;
    }
    if offset == 0 {
        ui.set_file_viewport_y(0.0);
    }
    let model = ui.get_files();
    if let Some(model) = model.as_any().downcast_ref::<SearchFileModel>()
        && model.row_count() == total as usize
    {
        for offset in evicted {
            model.clear_page(*offset as usize, SEARCH_PAGE_LIMIT as usize);
        }
        let start_id = offset.saturating_add(1);
        let end_id = offset.saturating_add(SEARCH_PAGE_LIMIT);
        let rows = tab
            .entries
            .iter()
            .filter(|entry| (start_id..=end_id).contains(&entry.id.0))
            .map(|entry| file_row(entry, tab, Texts::new(app.language), &app))
            .collect();
        model.update_page(offset as usize, rows);
    } else {
        let model = SearchFileModel::new(total as usize, empty_file_row());
        let start_id = offset.saturating_add(1);
        let end_id = offset.saturating_add(SEARCH_PAGE_LIMIT);
        let rows = tab
            .entries
            .iter()
            .filter(|entry| (start_id..=end_id).contains(&entry.id.0))
            .map(|entry| file_row(entry, tab, Texts::new(app.language), &app))
            .collect();
        model.update_page(offset as usize, rows);
        ui.set_files(ModelRc::new(model));
    }
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
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
        let Some(tab) = app.tabs.get_mut(&tab_id) else {
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
        refresh_ui(ui, state);
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
        let Some(tab) = app.tabs.get_mut(&tab_id) else {
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
        let Some(tab) = app.tabs.get_mut(&tab_id) else {
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
                        app.tabs
                            .get(&request.tab_id)
                            .map(|tab| tab.latest_request == request.request_id)
                    })
                    .unwrap_or(false);
                if !is_current {
                    continue;
                }
                let cached = state
                    .lock()
                    .ok()
                    .and_then(|app| app.icon_cache.get(&request.path).cloned());
                let icon = cached
                    .or_else(|| platform::windows_shell_icons::shell_icon_rgba(&request.path).ok());
                if let Some(icon) = icon {
                    let _ = events.send(IconEvent {
                        tab_id: request.tab_id,
                        request_id: request.request_id,
                        target: request.target,
                        path: request.path,
                        icon,
                    });
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
                .upgrade_in_event_loop(move |ui| {
                    state
                        .lock()
                        .expect("app state mutex is not poisoned")
                        .sidebar_icons
                        .insert(path, icon);
                    refresh_ui(&ui, &state);
                })
                .is_err()
            {
                break;
            }
        }
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
                        update_icon_row(&ui, &state, update);
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
    app.icon_cache.insert(event.path, event.icon);
    Some(IconUpdate {
        tab_id: event.tab_id,
        entry_id,
    })
}

fn icon_event_is_current(app: &AppState, event: &IconEvent) -> bool {
    app.tabs.get(&event.tab_id).is_some_and(|tab| {
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
    if app.active_tab != tab_id {
        return;
    }
    let tab = app.active();
    if tab.latest_request != request_id || tab.load_state != LoadState::Partial {
        return;
    }
    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        drop(app);
        refresh_ui(ui, state);
        return;
    };
    let start = model.row_count();
    if ui.get_projected_file_tab_id() != tab_id.0 as i32
        || ui.get_projected_file_request_id() != request_id.0 as i32
        || start > tab.pending_entries.len()
    {
        drop(app);
        refresh_ui(ui, state);
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
    use slint::Model;

    let app = state.lock().expect("app state mutex is not poisoned");
    if app.active_tab != update.tab_id {
        return;
    }
    let tab = app.active();
    let Some(entry_id) = update.entry_id else {
        let path = tab.visible_path();
        ui.set_current_location_icon(
            path.and_then(|path| app.icon_cache.get(path))
                .map(shell_icon_image)
                .unwrap_or_default(),
        );
        return;
    };
    let Some(index) = tab.visible_entry_index(entry_id) else {
        return;
    };
    let Some(entry) = tab.visible_entry(entry_id) else {
        return;
    };
    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        return;
    };
    if index < model.row_count() {
        model.set_row_data(index, file_row(entry, tab, Texts::new(app.language), &app));
    }
}
fn update_file_rows(
    ui: &AppWindow,
    state: &SharedSessions,
    changed: &std::collections::HashSet<EntryId>,
) {
    use slint::Model;

    let app = state.lock().expect("app state mutex is not poisoned");
    let tab = app.active();
    if !matches!(tab.load_state, LoadState::Complete) {
        return;
    }
    let texts = Texts::new(app.language);
    let model = ui.get_files();
    let Some(model) = model.as_any().downcast_ref::<VecModel<FileRow>>() else {
        return;
    };
    for (index, entry) in tab.entries.iter().enumerate() {
        if changed.contains(&entry.id) {
            model.set_row_data(index, file_row(entry, tab, texts, &app));
        }
    }
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
fn refresh_ui(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let texts = Texts::new(app.language);
    let tab = app.active();
    let active_is_settings = tab.kind == TabKind::Settings;
    ui.set_active_is_settings(active_is_settings);
    let display_entries = if matches!(tab.load_state, LoadState::Partial) {
        &tab.pending_entries
    } else if tab.has_failed_location() {
        &[] as &[FileEntry]
    } else {
        &tab.entries
    };
    let file_rows = display_entries
        .iter()
        .map(|entry| file_row(entry, tab, texts, &app))
        .collect::<Vec<_>>();
    let projected_tab_id = tab.id.0 as i32;
    let projected_request_id = tab.latest_request.0 as i32;
    if ui.get_projected_file_tab_id() != projected_tab_id
        || ui.get_projected_file_request_id() != projected_request_id
    {
        ui.set_file_viewport_y(0.0);
        ui.set_projected_file_tab_id(projected_tab_id);
        ui.set_projected_file_request_id(projected_request_id);
    }
    if tab.page_source == PageSource::Search {
        let total = tab.search_total.unwrap_or(tab.entries.len() as u32) as usize;
        let model = SearchFileModel::new(total, empty_file_row());
        model.update_rows(file_rows);
        ui.set_files(ModelRc::new(model));
    } else {
        ui.set_files(ModelRc::new(VecModel::from(file_rows)));
    }
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
        app.tab_order
            .iter()
            .position(|id| *id == app.active_tab)
            .unwrap_or(0) as i32,
    );
    ui.set_tabs(ModelRc::new(VecModel::from(
        app.tab_order
            .iter()
            .filter_map(|id| app.tabs.get(id))
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
                active: tab.id == app.active_tab,
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
                    (Language::Chinese, KnownLocationKind::Desktop) => "桌面",
                    (Language::Chinese, KnownLocationKind::Downloads) => "下载",
                    (Language::Chinese, KnownLocationKind::Documents) => "文档",
                    (Language::Chinese, KnownLocationKind::Pictures) => "图片",
                    (Language::Chinese, KnownLocationKind::Music) => "音乐",
                    (Language::Chinese, KnownLocationKind::Videos) => "视频",
                    (Language::English, KnownLocationKind::Home) => "Home",
                    (Language::English, KnownLocationKind::Desktop) => "Desktop",
                    (Language::English, KnownLocationKind::Downloads) => "Downloads",
                    (Language::English, KnownLocationKind::Documents) => "Documents",
                    (Language::English, KnownLocationKind::Pictures) => "Pictures",
                    (Language::English, KnownLocationKind::Music) => "Music",
                    (Language::English, KnownLocationKind::Videos) => "Videos",
                    (_, KnownLocationKind::Drive) => location.label.as_str(),
                }
                .into(),
                icon_kind: match location.kind {
                    KnownLocationKind::Home => 0,
                    KnownLocationKind::Desktop => 1,
                    KnownLocationKind::Downloads => 2,
                    KnownLocationKind::Documents => 3,
                    KnownLocationKind::Pictures => 4,
                    KnownLocationKind::Music => 5,
                    KnownLocationKind::Videos => 6,
                    KnownLocationKind::Drive => 7,
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
    ui.set_can_close_tab(app.tab_order.len() > 1);
    ui.set_can_restore_tab(!app.closed_tabs.is_empty());
    ui.set_language_mode(match app.language {
        Language::Chinese => 0,
        Language::English => 1,
    });
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
        icon: app
            .icons
            .get(&(tab.id, tab.latest_request, entry.id))
            .or_else(|| app.icon_cache.get(&entry.path))
            .map(shell_icon_image)
            .unwrap_or_default(),
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
    #[test]
    fn complete_tab_duplication_shares_entries_without_reloading() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("same")], 0, [0, 1, 2, 3]);
        let source = app.tabs.get_mut(&TabId(1)).unwrap();
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
        let duplicated = app.tabs.get(&duplicate).unwrap();

        assert!(Arc::ptr_eq(&source_entries, &duplicated.entries));
        assert_eq!(duplicated.latest_request, RequestId(7));
        assert_eq!(duplicated.load_state, LoadState::Complete);
    }
    #[test]
    fn closing_a_tab_preserves_another_session() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        assert_eq!(app.active_tab, second);
        assert_eq!(app.close_tab(TabId(2)), Some(TabId(1)));
        assert_eq!(app.active().current_path, Some(PathBuf::from("one")));
    }

    #[test]
    fn closing_an_inactive_tab_keeps_the_active_tab() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);
        let second = app.create_tab(PathBuf::from("two"));
        let third = app.create_tab(PathBuf::from("three"));
        assert_eq!(app.active_tab, third);

        assert_eq!(app.close_tab(second), Some(third));
        assert_eq!(app.active_tab, third);
        assert_eq!(app.active().current_path, Some(PathBuf::from("three")));
        assert!(!app.tabs.contains_key(&second));
    }

    #[test]
    fn settings_tab_is_singleton_and_does_not_restore() {
        let mut app = AppState::new_for_test(vec![PathBuf::from("one")], 0, [0, 1, 2, 3]);

        let settings = app.open_settings();
        assert_eq!(app.open_settings(), settings);
        assert_eq!(app.tab_order.len(), 2);
        assert_eq!(app.active().kind, TabKind::Settings);

        assert_eq!(app.close_tab(settings), Some(TabId(1)));
        assert!(app.closed_tabs.is_empty());
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
        assert!(app.tabs.contains_key(&TabId(1)));
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
        let (items, total, file_total) = search_grouped_page(
            &client,
            (None, true),
            ".md".into(),
            crate::platform::windows::everything::EverythingSort::NameAscending,
            0,
            30,
            Duration::from_secs(3),
        )
        .unwrap();
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
        let tab = app.tabs.get_mut(&TabId(1)).unwrap();
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
        };
        assert!(icon_event_is_current(&app, &event));

        let location_event = IconEvent {
            tab_id: TabId(1),
            request_id: RequestId(7),
            target: IconTarget::Location,
            path: PathBuf::from("same"),
            icon: event.icon.clone(),
        };
        assert!(icon_event_is_current(&app, &location_event));

        let mut stale_request = event;
        stale_request.request_id = RequestId(6);
        assert!(!icon_event_is_current(&app, &stale_request));
        stale_request.request_id = RequestId(7);
        stale_request.path = PathBuf::from("same/other.txt");
        assert!(!icon_event_is_current(&app, &stale_request));
        app.tabs.remove(&TabId(1));
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
        let tab = app.tabs.get_mut(&TabId(1)).unwrap();
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
            app.tabs[&TabId(1)].entries[0].folder_size,
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
            app.tabs[&TabId(1)].entries[0].folder_size,
            FolderSizeState::Value(0)
        );
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
            let tab = app.tabs.get_mut(&TabId(1)).unwrap();
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
            context_target_at(&state, 170.0, 166.0, 0.0),
            (Some(EntryId(1)), false)
        );
        assert_eq!(context_target_at(&state, 250.0, 166.0, 0.0), (None, true));
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
            app.tabs.get_mut(&TabId(1)).unwrap().replace_entries(
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
            context_target_at(&state, 170.0, 166.0, -80.0),
            (Some(EntryId(3)), false)
        );
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
