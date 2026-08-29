use std::{
    cell::Cell,
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Instant,
};

use slint::{
    Image, ModelRc, Rgba8Pixel, SharedPixelBuffer, VecModel,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{
    agent_debug::{self, AgentScenario},
    domain::{
        EntryId, FileEntry, LoadState, NavigationKind, RequestId, SortField, TabId, TabSession,
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
            session_store::ThemeMode::System,
            Language::Chinese,
            false,
        )
    }

    fn new(
        initial_paths: Vec<PathBuf>,
        active_index: usize,
        column_order: [u8; 4],
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
        }
    }

    fn duplicate_active_tab(&mut self) -> Option<TabId> {
        let source_id = self.active_tab;
        let source = self.tabs.get(&source_id)?;
        if source.load_state != LoadState::Complete {
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

    fn close_tab(&mut self, closing: TabId) -> Option<TabId> {
        if self.tab_order.len() == 1 {
            return None;
        }
        let index = self.tab_order.iter().position(|id| *id == closing)?;
        let closing_was_active = closing == self.active_tab;
        if let Some(mut tab) = self.tabs.remove(&closing) {
            tab.cancel_pending();
            self.icons.retain(|(tab_id, _, _), _| *tab_id != closing);
            if let Some(path) = tab.current_path.take() {
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

struct IconEvent {
    tab_id: TabId,
    request_id: RequestId,
    target: IconTarget,
    path: PathBuf,
    icon: platform::windows_shell_icons::ShellIconRgba,
}

pub fn run(scenario: Option<AgentScenario>) -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
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
    let (restored_paths, active_index, window, column_order, theme_mode, language) = restored
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
    let (icon_sender, icon_receiver) = spawn_icon_workers(ICON_WORKER_COUNT, state.clone());

    wire_callbacks(&ui, request_sender.clone(), state.clone());
    wire_mouse_navigation(&ui);
    wire_window_controls(&ui);
    start_event_pump(&ui, event_receiver, icon_sender, state.clone());
    start_icon_event_pump(&ui, icon_receiver, state.clone());
    {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.sidebar = platform::known_locations();
    }
    start_sidebar_icon_loader(&ui, state.clone());
    refresh_ui(&ui, &state);
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
    let (paths, active_tab, column_order, theme_mode, language) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        let active_tab = app
            .tab_order
            .iter()
            .position(|id| *id == app.active_tab)
            .unwrap_or(0);
        (
            app.stable_paths(),
            active_tab,
            app.column_order,
            app.theme_mode,
            app.language,
        )
    };
    let position = ui.window().position();
    let size = ui.window().size();
    if scenario.is_none()
        && let Some(path) = session_store::default_path()
        && let Ok(session) = session_store::SessionState::with_settings(
            session_store::WindowPlacement {
                x: position.x,
                y: position.y,
                width: (size.width as f32 / ui.window().scale_factor()).round() as u32,
                height: (size.height as f32 / ui.window().scale_factor()).round() as u32,
            },
            active_tab,
            paths,
            column_order,
            theme_mode,
            language,
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

fn wire_callbacks(ui: &AppWindow, sender: mpsc::Sender<DirectoryRequest>, state: SharedSessions) {
    let weak = ui.as_weak();
    let sender_for_path = sender.clone();
    let state_for_path = state.clone();
    ui.on_navigate_path(move |path| {
        let input = path.to_string();
        let target = PathBuf::from(path.as_str());
        let tab_id = {
            let mut app = state_for_path
                .lock()
                .expect("app state mutex is not poisoned");
            let tab_id = app.active_tab;
            let tab = app
                .tabs
                .get_mut(&tab_id)
                .expect("active tab session exists");
            tab.update_address_input(input);
            tab.address_editing = true;
            tab_id
        };
        submit_navigation(
            &sender_for_path,
            &state_for_path,
            tab_id,
            target,
            NavigationKind::Normal,
        );
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_path);
        }
    });

    let weak = ui.as_weak();
    let state_for_edit = state.clone();
    ui.on_begin_address_edit(move || {
        let mut app = state_for_edit
            .lock()
            .expect("app state mutex is not poisoned");
        let tab_id = app.active_tab;
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.begin_address_edit();
        }
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_edit);
            ui.invoke_focus_address_editor();
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
        if let Some(tab) = app.tabs.get_mut(&tab_id) {
            tab.set_sort(field);
        }
        drop(app);
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
        reorder_column(&mut app.column_order, kind as u8, offset);
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_columns);
        }
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
    let sender_for_refresh = sender;
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
    });

    let weak = ui.as_weak();
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
    });

    ui.on_open_settings(|| {});
}

fn wire_mouse_navigation(ui: &AppWindow) {
    use winit::{
        event::{ElementState, MouseButton, WindowEvent},
        keyboard::{Key, ModifiersState, NamedKey},
    };

    let weak = ui.as_weak();
    let modifiers = Cell::new(ModifiersState::empty());
    ui.window().on_winit_window_event(move |_, event| {
        if let WindowEvent::ModifiersChanged(changed) = event {
            modifiers.set(changed.state());
            return EventResult::Propagate;
        }
        let Some(ui) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        match event {
            WindowEvent::KeyboardInput { event, .. }
                if event.state == ElementState::Pressed && !event.repeat =>
            {
                let modifiers = modifiers.get();
                let control = modifiers.control_key();
                let alt = modifiers.alt_key();
                let shift = modifiers.shift_key();
                let editing_address = ui.get_address_editing();
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
                        && !shift =>
                    {
                        ui.invoke_begin_address_edit();
                        true
                    }
                    _ if !editing_address
                        && control
                        && !alt
                        && !shift
                        && character.is_some_and(|value| value.eq_ignore_ascii_case("a")) =>
                    {
                        ui.invoke_select_all();
                        true
                    }
                    Key::Named(NamedKey::Space)
                        if !editing_address && control && !alt && !shift =>
                    {
                        ui.invoke_toggle_focused();
                        true
                    }
                    Key::Named(NamedKey::ArrowUp) if !editing_address && !control && !alt => {
                        ui.invoke_move_focus(-1, shift);
                        true
                    }
                    Key::Named(NamedKey::ArrowDown) if !editing_address && !control && !alt => {
                        ui.invoke_move_focus(1, shift);
                        true
                    }
                    Key::Named(NamedKey::Home) if !editing_address && !control && !alt => {
                        ui.invoke_focus_boundary(false, shift);
                        true
                    }
                    Key::Named(NamedKey::End) if !editing_address && !control && !alt => {
                        ui.invoke_focus_boundary(true, shift);
                        true
                    }
                    Key::Named(NamedKey::Enter)
                        if !editing_address && !control && !alt && !shift =>
                    {
                        ui.invoke_open_entry(-1);
                        true
                    }
                    Key::Named(NamedKey::Escape)
                        if !editing_address && !control && !alt && !shift =>
                    {
                        ui.invoke_clear_selection();
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
        ui.window().with_winit_window(|window| {
            let _ = window.drag_window();
        });
    });
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
            if weak
                .upgrade_in_event_loop(move |ui| {
                    let batch = match &event {
                        DirectoryEvent::Batch {
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
            if let Some(tab) = app.tabs.get_mut(&tab_id)
                && tab.accepts(request_id)
            {
                tab.sort_pending();
                tab.commit_pending();
                tab.commit_path(path);
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
        return;
    };
    let start = model.row_count();
    if start > tab.pending_entries.len() {
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

    ui.set_columns(ModelRc::new(VecModel::from(
        app.column_order
            .iter()
            .map(|kind| ColumnRow {
                kind: i32::from(*kind),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_selected_count(tab.selected.len() as i32);
    ui.set_status_text(status_text(tab, Texts::new(app.language)).into());
}
fn refresh_ui(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let texts = Texts::new(app.language);
    let tab = app.active();
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
    ui.set_files(ModelRc::new(VecModel::from(file_rows)));
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

    ui.set_status_text(status_text(tab, texts).into());
    let (error_page_title, error_page_description) = error_page_text(tab.load_state, texts);
    ui.set_error_page_title(error_page_title.into());
    ui.set_error_page_description(error_page_description.into());
    ui.set_tabs(ModelRc::new(VecModel::from(
        app.tab_order
            .iter()
            .filter_map(|id| app.tabs.get(id))
            .map(|tab| TabRow {
                id: tab.id.0 as i32,
                title: tab
                    .visible_path()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| {
                        display_path(tab.visible_path().unwrap_or(Path::new("C:\\")))
                    })
                    .into(),
                active: tab.id == app.active_tab,
                loading: matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
                icon: tab
                    .visible_path()
                    .and_then(|path| app.icon_cache.get(path))
                    .map(shell_icon_image)
                    .unwrap_or_default(),
                is_drive: tab.visible_path().is_some_and(is_drive_root),
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
    ui.set_columns(ModelRc::new(VecModel::from(
        app.column_order
            .iter()
            .map(|kind| ColumnRow {
                kind: i32::from(*kind),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_sort_field(match tab.sort_field {
        SortField::Name => 0,
        SortField::Kind => 1,
        SortField::Size => 2,
        SortField::Modified => 3,
    });
    ui.set_sort_descending(tab.sort_direction == crate::domain::SortDirection::Descending);
    let page_projection = agent_debug::page_projection(tab.load_state, tab.entries.is_empty());
    ui.set_page_state(page_projection.index);
    ui.set_show_request_access(
        page_projection
            .visible_page_operations
            .contains(&agent_debug::PageOperation::RequestWindowsAccess),
    );
    ui.set_can_navigate_back(tab.has_failed_location() || !tab.back_history.is_empty());
    ui.set_can_navigate_forward(!tab.forward_history.is_empty());
    ui.set_can_navigate_up(tab.visible_path().and_then(Path::parent).is_some());
    ui.set_can_refresh(!matches!(
        tab.load_state,
        LoadState::Loading | LoadState::Partial
    ));
    ui.set_can_close_tab(app.tab_order.len() > 1);
    ui.set_can_restore_tab(!app.closed_tabs.is_empty());
    ui.set_language_mode(match app.language {
        Language::Chinese => 0,
        Language::English => 1,
    });
    ui.set_theme_mode(match app.theme_mode {
        session_store::ThemeMode::System => 0,
        session_store::ThemeMode::Light => 1,
        session_store::ThemeMode::Dark => 2,
    });
    ui.set_dark_theme(app.dark_theme());
    apply_ui_texts(ui, app.language);
}

fn file_row(entry: &FileEntry, tab: &TabSession, texts: Texts, app: &AppState) -> FileRow {
    debug_assert_eq!(
        entry.path.file_name(),
        Some(entry.original_name.as_os_str()),
        "entry identity must retain its original file name"
    );
    FileRow {
        id: entry.id.0 as i32,
        name: entry.display_name.clone().into(),
        kind: texts.kind(entry.kind).into(),
        size: texts.size(entry.size_bytes).into(),
        modified: texts.modified(entry.modified).into(),
        is_directory: entry.kind == crate::domain::EntryKind::Directory,
        selected: tab.selected.contains(&entry.id),
        focused: tab.focused == Some(entry.id),
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
        close_settings,
        theme,
        theme_system,
        theme_light,
        theme_dark,
        language_label,
        chinese,
        english,
        loading,
        empty_folder,
        new_tab,
        close_tab,
        minimize,
        restore,
        maximize,
        close,
        address,
        cancel_edit,
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
            "关闭设置",
            "主题",
            "跟随系统",
            "浅色",
            "深色",
            "语言",
            "中文",
            "English",
            "正在加载…",
            "此文件夹为空",
            "新建标签",
            "关闭标签",
            "最小化",
            "还原",
            "最大化",
            "关闭",
            "路径",
            "取消编辑",
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
            "Close settings",
            "Theme",
            "Use system setting",
            "Light",
            "Dark",
            "Language",
            "中文",
            "English",
            "Loading…",
            "This folder is empty",
            "New tab",
            "Close tab",
            "Minimize",
            "Restore",
            "Maximize",
            "Close",
            "Path",
            "Cancel editing",
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
    ui.set_text_close_settings(close_settings.into());
    ui.set_text_theme(theme.into());
    ui.set_text_theme_system(theme_system.into());
    ui.set_text_theme_light(theme_light.into());
    ui.set_text_theme_dark(theme_dark.into());
    ui.set_text_language(language_label.into());
    ui.set_text_language_chinese(chinese.into());
    ui.set_text_language_english(english.into());
    ui.set_text_loading(loading.into());
    ui.set_text_empty_folder(empty_folder.into());
    ui.set_text_new_tab(new_tab.into());
    ui.set_text_close_tab(close_tab.into());
    ui.set_text_window_minimize(minimize.into());
    ui.set_text_window_restore(restore.into());
    ui.set_text_window_maximize(maximize.into());
    ui.set_text_window_close(close.into());
    ui.set_text_address(address.into());
    ui.set_text_cancel_edit(cancel_edit.into());
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
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            size_bytes: Some(1),
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
            path: PathBuf::from("same/file.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            size_bytes: Some(1),
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
