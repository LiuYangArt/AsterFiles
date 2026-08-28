use std::{
    collections::{HashMap, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Instant,
};

use slint::{
    ModelRc, VecModel,
    winit_030::{EventResult, WinitWindowAccessor, winit},
};

use crate::{
    domain::{EntryId, FileEntry, LoadState, NavigationKind, RequestId, TabId, TabSession},
    fs::{ReadOutcome, read_directory_batches, sort_entries},
    i18n::{Language, Texts},
    session_store,
};

slint::include_modules!();

const WORKER_COUNT: usize = 4;

type SharedSessions = Arc<Mutex<AppState>>;

#[derive(Debug)]
struct AppState {
    tabs: HashMap<TabId, TabSession>,
    tab_order: Vec<TabId>,
    active_tab: TabId,
    closed_tabs: VecDeque<PathBuf>,
    next_tab_id: u32,
    language: Language,
}

impl AppState {
    fn new(initial_paths: Vec<PathBuf>) -> Self {
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
        let active_tab = tab_order[0];
        let next_tab_id = tab_order.len() as u32 + 1;
        Self {
            tabs,
            tab_order,
            active_tab,
            closed_tabs: VecDeque::new(),
            next_tab_id,
            language: Language::Chinese,
        }
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

    fn close_active(&mut self) -> Option<TabId> {
        if self.tab_order.len() == 1 {
            return None;
        }
        let closing = self.active_tab;
        if let Some(mut tab) = self.tabs.remove(&closing) {
            tab.cancel_pending();
            if let Some(path) = tab.current_path.take() {
                self.closed_tabs.push_front(path);
                self.closed_tabs.truncate(10);
            }
        }
        let index = self
            .tab_order
            .iter()
            .position(|id| *id == closing)
            .expect("active tab is present in tab order");
        self.tab_order.remove(index);
        self.active_tab = self.tab_order[index.min(self.tab_order.len() - 1)];
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
}

#[derive(Debug)]
struct DirectoryRequest {
    tab_id: TabId,
    request_id: RequestId,
    path: PathBuf,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    started_at: Instant,
}

#[derive(Debug)]
enum DirectoryEvent {
    Batch {
        tab_id: TabId,
        request_id: RequestId,
        entries: Vec<FileEntry>,
        first_batch_ms: u128,
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
        elapsed_ms: u128,
    },
    Failed {
        tab_id: TabId,
        request_id: RequestId,
        kind: io::ErrorKind,
        message: String,
    },
}

pub fn run() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let restored_paths = session_store::default_path()
        .and_then(|path| session_store::load(&path).ok())
        .filter(|paths| !paths.is_empty())
        .unwrap_or_else(|| vec![initial_path()]);
    let state = Arc::new(Mutex::new(AppState::new(restored_paths)));
    let (request_sender, event_receiver) = spawn_directory_workers(WORKER_COUNT);
    let event_receiver = Arc::new(Mutex::new(event_receiver));

    wire_callbacks(&ui, request_sender.clone(), state.clone());
    wire_mouse_navigation(&ui);
    start_event_pump(&ui, event_receiver, state.clone());
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
    for (tab_id, path) in initial_tabs {
        submit_navigation(
            &request_sender,
            &state,
            tab_id,
            path,
            NavigationKind::Refresh,
        );
    }

    let result = ui.run();
    let paths = state
        .lock()
        .expect("app state mutex is not poisoned")
        .stable_paths();
    if let Some(path) = session_store::default_path() {
        let _ = session_store::save(&path, &paths);
    }
    result
}

fn submit_navigation(
    sender: &mpsc::Sender<DirectoryRequest>,
    state: &SharedSessions,
    tab_id: TabId,
    path: PathBuf,
    kind: NavigationKind,
) {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let Some(tab) = app.tabs.get_mut(&tab_id) else {
            return;
        };
        let (request_id, cancel) = tab.begin_navigation(path.clone(), kind);
        DirectoryRequest {
            tab_id,
            request_id,
            path,
            cancel,
            started_at: Instant::now(),
        }
    };
    let _ = sender.send(request);
}

fn wire_callbacks(ui: &AppWindow, sender: mpsc::Sender<DirectoryRequest>, state: SharedSessions) {
    let weak = ui.as_weak();
    let sender_for_path = sender.clone();
    let state_for_path = state.clone();
    ui.on_navigate_path(move |path| {
        let tab_id = state_for_path
            .lock()
            .expect("app state mutex is not poisoned")
            .active_tab;
        submit_navigation(
            &sender_for_path,
            &state_for_path,
            tab_id,
            PathBuf::from(path.as_str()),
            NavigationKind::Normal,
        );
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state_for_path);
        }
    });

    let weak = ui.as_weak();
    let sender_for_entry = sender.clone();
    let state_for_entry = state.clone();
    ui.on_open_entry(move |entry_id| {
        let target = state_for_entry
            .lock()
            .expect("app state mutex is not poisoned")
            .active()
            .entry_path(EntryId(entry_id as u32));
        if let Some(target) = target {
            let tab_id = state_for_entry
                .lock()
                .expect("app state mutex is not poisoned")
                .active_tab;
            submit_navigation(
                &sender_for_entry,
                &state_for_entry,
                tab_id,
                target,
                NavigationKind::Normal,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_entry);
            }
        }
    });

    let weak = ui.as_weak();
    let sender_for_new = sender.clone();
    let state_for_new = state.clone();
    ui.on_new_tab(move || {
        let (tab_id, path) = {
            let mut app = state_for_new
                .lock()
                .expect("app state mutex is not poisoned");
            let path = app
                .active()
                .current_path
                .clone()
                .unwrap_or_else(initial_path);
            let tab_id = app.create_tab(path.clone());
            (tab_id, path)
        };
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
    });

    let weak = ui.as_weak();
    let state_for_close = state.clone();
    ui.on_close_tab(move || {
        state_for_close
            .lock()
            .expect("app state mutex is not poisoned")
            .close_active();
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
        let target = {
            let app = state_for_back
                .lock()
                .expect("app state mutex is not poisoned");
            app.active()
                .back_target()
                .map(|path| (app.active_tab, path))
        };
        if let Some((tab_id, path)) = target {
            submit_navigation(
                &sender_for_back,
                &state_for_back,
                tab_id,
                path,
                NavigationKind::Back,
            );
            if let Some(ui) = weak.upgrade() {
                refresh_ui(&ui, &state_for_back);
            }
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
    let sender_for_up = sender.clone();
    let state_for_up = state.clone();
    ui.on_navigate_up(move || {
        let target = {
            let app = state_for_up
                .lock()
                .expect("app state mutex is not poisoned");
            app.active()
                .current_path
                .as_deref()
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
                    .current_path
                    .clone()
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
    ui.on_toggle_language(move || {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.language = app.language.toggle();
        drop(app);
        if let Some(ui) = weak.upgrade() {
            refresh_ui(&ui, &state);
        }
    });
}

fn wire_mouse_navigation(ui: &AppWindow) {
    let weak = ui.as_weak();
    ui.window().on_winit_window_event(move |_, event| {
        let winit::event::WindowEvent::MouseInput {
            state: winit::event::ElementState::Released,
            button,
            ..
        } = event
        else {
            return EventResult::Propagate;
        };
        let Some(ui) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        match button {
            winit::event::MouseButton::Back => {
                if ui.get_can_navigate_back() {
                    ui.invoke_navigate_back();
                }
                EventResult::PreventDefault
            }
            winit::event::MouseButton::Forward => {
                if ui.get_can_navigate_forward() {
                    ui.invoke_navigate_forward();
                }
                EventResult::PreventDefault
            }
            _ => EventResult::Propagate,
        }
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
            first_batch_ms: request.started_at.elapsed().as_millis(),
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
            elapsed_ms: request.started_at.elapsed().as_millis(),
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
            if weak
                .upgrade_in_event_loop(move |ui| {
                    apply_event(&state, event);
                    refresh_ui(&ui, &state);
                })
                .is_err()
            {
                break;
            }
        }
    });
}

fn apply_event(state: &SharedSessions, event: DirectoryEvent) {
    let mut app = state.lock().expect("app state mutex is not poisoned");
    match event {
        DirectoryEvent::Batch {
            tab_id,
            request_id,
            entries,
            first_batch_ms,
        } => {
            if let Some(tab) = app.tabs.get_mut(&tab_id)
                && tab.accepts(request_id)
            {
                tab.append_pending(entries);
                if tab.first_batch_ms.is_none() {
                    tab.first_batch_ms = Some(first_batch_ms);
                }
            } else if let Some(tab) = app.tabs.get_mut(&tab_id) {
                tab.discarded_results += 1;
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
                sort_entries(&mut tab.pending_entries);
                tab.commit_pending();
                tab.commit_path(path);
                tab.error = (skipped > 0).then(|| skipped.to_string());
            }
        }
        DirectoryEvent::Cancelled {
            tab_id,
            request_id,
            elapsed_ms,
        } => {
            if let Some(tab) = app.tabs.get_mut(&tab_id)
                && tab.latest_request == request_id
            {
                tab.discard_pending();
                tab.load_state = LoadState::Cancelled;
                tab.cancel_elapsed_ms = Some(elapsed_ms);
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

fn refresh_ui(ui: &AppWindow, state: &SharedSessions) {
    let app = state.lock().expect("app state mutex is not poisoned");
    let texts = Texts::new(app.language);
    let tab = app.active();
    let display_entries = if matches!(tab.load_state, LoadState::Partial) {
        &tab.pending_entries
    } else {
        &tab.entries
    };
    let file_rows = display_entries
        .iter()
        .map(|entry| file_row(entry, texts))
        .collect::<Vec<_>>();
    ui.set_files(ModelRc::new(VecModel::from(file_rows)));
    ui.set_current_path(
        tab.current_path
            .as_deref()
            .map(display_path)
            .unwrap_or_default()
            .into(),
    );
    ui.set_status_text(status_text(tab, texts).into());
    ui.set_tabs(ModelRc::new(VecModel::from(
        app.tab_order
            .iter()
            .filter_map(|id| app.tabs.get(id))
            .map(|tab| TabRow {
                id: tab.id.0 as i32,
                title: tab
                    .current_path
                    .as_deref()
                    .and_then(Path::file_name)
                    .map(|name| name.to_string_lossy().into_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| {
                        display_path(tab.current_path.as_deref().unwrap_or(Path::new("C:\\")))
                    })
                    .into(),
                active: tab.id == app.active_tab,
                loading: matches!(tab.load_state, LoadState::Loading | LoadState::Partial),
            })
            .collect::<Vec<_>>(),
    )));
    ui.set_can_navigate_back(!tab.back_history.is_empty());
    ui.set_can_navigate_forward(!tab.forward_history.is_empty());
    ui.set_can_navigate_up(tab.current_path.as_deref().and_then(Path::parent).is_some());
    ui.set_can_refresh(!matches!(
        tab.load_state,
        LoadState::Loading | LoadState::Partial
    ));
    ui.set_can_close_tab(app.tab_order.len() > 1);
    ui.set_can_restore_tab(!app.closed_tabs.is_empty());
    ui.set_language_label(match app.language {
        Language::Chinese => "EN".into(),
        Language::English => "中文".into(),
    });
    apply_ui_texts(ui, app.language);
}

fn file_row(entry: &FileEntry, texts: Texts) -> FileRow {
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
    }
}

fn status_text(tab: &TabSession, texts: Texts) -> String {
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
        drives,
        name,
        kind,
        modified,
        size,
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
            "磁盘",
            "名称",
            "类型",
            "修改时间",
            "大小",
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
            "Drives",
            "Name",
            "Type",
            "Modified",
            "Size",
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
    ui.set_text_drives(drives.into());
    ui.set_text_name(name.into());
    ui.set_text_type(kind.into());
    ui.set_text_modified(modified.into());
    ui.set_text_size(size.into());
}

fn display_path(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
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
    fn closing_a_tab_preserves_another_session() {
        let mut app = AppState::new(vec![PathBuf::from("one")]);
        let second = app.create_tab(PathBuf::from("two"));
        assert_eq!(app.active_tab, second);
        assert_eq!(app.close_active(), Some(TabId(1)));
        assert_eq!(app.active().current_path, Some(PathBuf::from("one")));
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
