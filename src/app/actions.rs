use super::*;
use std::ffi::OsString;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActionTarget {
    pub window_id: WindowId,
    pub tab_id: TabId,
    pub request_id: RequestId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EntryTarget {
    pub tab: ActionTarget,
    pub entry_id: EntryId,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActionError {
    WindowClosed,
    TabClosed,
    StaleRequest,
    EntryUnavailable,
    NotAllowed,
    InvalidName,
    QueueUnavailable,
    TaskUnavailable,
}
impl ActionError {
    pub fn code(self) -> &'static str {
        match self {
            Self::WindowClosed => "window-closed",
            Self::TabClosed => "tab-closed",
            Self::StaleRequest => "stale-request",
            Self::EntryUnavailable => "entry-unavailable",
            Self::NotAllowed => "not-allowed",
            Self::InvalidName => "invalid-name",
            Self::QueueUnavailable => "queue-unavailable",
            Self::TaskUnavailable => "task-unavailable",
        }
    }
}
#[derive(Clone)]
pub(super) struct ActionWorkers {
    pub directory: mpsc::Sender<DirectoryRequest>,
    pub network_directory: mpsc::SyncSender<DirectoryRequest>,
    pub operation: mpsc::Sender<FileOperationRequest>,
}
#[derive(Debug)]
pub(super) enum Action {
    OpenDirectory {
        target: ActionTarget,
        path: PathBuf,
    },
    Refresh {
        target: ActionTarget,
    },
    SwitchTab {
        window_id: WindowId,
        tab_id: TabId,
    },
    Select {
        target: EntryTarget,
        toggle: bool,
        extend: bool,
    },
    Rename {
        target: EntryTarget,
        name: OsString,
    },
    Query {
        window_id: WindowId,
    },
    Cancel {
        window_id: WindowId,
        operation_id: OperationId,
    },
}
#[derive(Debug)]
pub(super) enum ActionReceipt {
    Completed,
    DirectoryAccepted {
        target: ActionTarget,
    },
    OperationAccepted {
        id: OperationId,
    },
    CancellationRequested {
        id: OperationId,
        state: OperationState,
    },
    State(WindowSnapshot),
}
#[derive(Debug)]
pub(super) struct OperationAvailability {
    pub operation: &'static str,
    pub reason: Option<ActionError>,
}
#[derive(Debug)]
pub(super) struct EntrySnapshot {
    pub target: EntryTarget,
    pub path: PathBuf,
    pub selected: bool,
}
#[derive(Debug)]
pub(super) struct TabSnapshot {
    pub target: ActionTarget,
    pub location: Option<NavigationLocation>,
    pub load_state: LoadState,
    pub selected: Vec<EntryId>,
    pub entries: Vec<EntrySnapshot>,
    pub operations: Vec<OperationAvailability>,
}
#[derive(Debug)]
pub(super) struct OperationSnapshot {
    pub id: OperationId,
    pub state: OperationState,
    pub cancellation_requested: bool,
    pub cancel_reason: Option<ActionError>,
    pub error: Option<String>,
}
#[derive(Debug)]
pub(super) struct WindowSnapshot {
    pub window_id: WindowId,
    pub active_tab: TabId,
    pub tabs: Vec<TabSnapshot>,
    pub operations: Vec<OperationSnapshot>,
    pub revision: u64,
}

pub(super) fn target(state: &WindowSessions) -> Result<ActionTarget, ActionError> {
    let app = state.shared.lock().map_err(|_| ActionError::WindowClosed)?;
    let window = app
        .window(state.window_id)
        .ok_or(ActionError::WindowClosed)?;
    let tab = window
        .tabs
        .get(&window.active_tab)
        .ok_or(ActionError::TabClosed)?;
    Ok(ActionTarget {
        window_id: state.window_id,
        tab_id: tab.id,
        request_id: tab.latest_request,
    })
}
fn checked_tab(app: &AppState, target: ActionTarget) -> Result<&TabSession, ActionError> {
    let window = app
        .window(target.window_id)
        .ok_or(ActionError::WindowClosed)?;
    let tab = window
        .tabs
        .get(&target.tab_id)
        .ok_or(ActionError::TabClosed)?;
    if tab.latest_request != target.request_id {
        return Err(ActionError::StaleRequest);
    }
    Ok(tab)
}
fn entry_page_available(tab: &TabSession) -> bool {
    tab.kind == TabKind::Files && matches!(tab.load_state, LoadState::Complete | LoadState::Partial)
}
fn checked_entry(app: &AppState, target: EntryTarget) -> Result<&FileEntry, ActionError> {
    let tab = checked_tab(app, target.tab)?;
    if !entry_page_available(tab) {
        return Err(ActionError::NotAllowed);
    }
    tab.visible_entry(target.entry_id)
        .filter(|entry| entry.id == target.entry_id)
        .ok_or(ActionError::EntryUnavailable)
}
pub(super) fn cancellation_reason(
    task: &crate::domain::file_operations::OperationTask,
) -> Option<ActionError> {
    let committed_cleanup = task.kind == FileOperationKind::PermanentDelete
        && task.resource == OperationResource::Cleanup;
    let cancellable = !committed_cleanup
        && matches!(
            task.state,
            OperationState::Queued
                | OperationState::Preflight
                | OperationState::Running
                | OperationState::Paused
                | OperationState::WaitingConflict
        );
    (!cancellable).then_some(ActionError::NotAllowed)
}
pub(super) fn snapshot(
    state: &SharedSessions,
    window_id: WindowId,
) -> Result<WindowSnapshot, ActionError> {
    let app = state.lock().map_err(|_| ActionError::WindowClosed)?;
    let window = app.window(window_id).ok_or(ActionError::WindowClosed)?;
    let mut tabs = window
        .tabs
        .values()
        .map(|tab| {
            let target = ActionTarget {
                window_id,
                tab_id: tab.id,
                request_id: tab.latest_request,
            };
            let files = tab.kind == TabKind::Files;
            let loading = matches!(tab.load_state, LoadState::Loading | LoadState::Partial);
            let entries = if entry_page_available(tab) {
                tab.visible_entries()
            } else {
                &[]
            };
            let visible_ids = entries.iter().map(|entry| entry.id).collect::<HashSet<_>>();
            let selected = tab
                .selected
                .iter()
                .copied()
                .filter(|id| visible_ids.contains(id))
                .collect::<HashSet<_>>();
            TabSnapshot {
                target,
                location: tab.visible_location().cloned(),
                load_state: tab.load_state,
                selected: tab
                    .selected
                    .iter()
                    .copied()
                    .filter(|id| selected.contains(id))
                    .collect(),
                entries: entries
                    .iter()
                    .map(|entry| EntrySnapshot {
                        target: EntryTarget {
                            tab: target,
                            entry_id: entry.id,
                        },
                        path: entry.path.clone(),
                        selected: selected.contains(&entry.id),
                    })
                    .collect(),
                operations: [
                    ("open-directory", files),
                    (
                        "refresh",
                        files && !loading && tab.visible_location().is_some(),
                    ),
                    ("switch-tab", true),
                    ("select", !entries.is_empty()),
                    (
                        "rename",
                        tab.load_state == LoadState::Complete && !entries.is_empty(),
                    ),
                    ("query", true),
                ]
                .into_iter()
                .map(|(operation, enabled)| OperationAvailability {
                    operation,
                    reason: (!enabled).then_some(ActionError::NotAllowed),
                })
                .collect(),
            }
        })
        .collect::<Vec<_>>();
    tabs.sort_by_key(|tab| tab.target.tab_id.0);
    let mut operations = app
        .operations
        .iter()
        .map(|task| OperationSnapshot {
            id: task.id,
            state: task.state,
            cancellation_requested: task.cancellation.is_cancelled(),
            cancel_reason: cancellation_reason(task),
            error: task.items.iter().find_map(|item| item.error.clone()),
        })
        .collect::<Vec<_>>();
    operations.sort_by_key(|task| task.id.0);
    // The content token covers asynchronous model changes as well as explicit actions.
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    window_id.0.hash(&mut hash);
    window.active_tab.0.hash(&mut hash);
    for tab in &tabs {
        tab.target.tab_id.0.hash(&mut hash);
        tab.target.request_id.0.hash(&mut hash);
        match &tab.location {
            None => 0u8.hash(&mut hash),
            Some(NavigationLocation::Home) => 1u8.hash(&mut hash),
            Some(NavigationLocation::Directory(path)) => {
                2u8.hash(&mut hash);
                path.hash(&mut hash);
            }
            Some(NavigationLocation::Library(library)) => {
                3u8.hash(&mut hash);
                library.identity.hash(&mut hash);
            }
        }
        format!("{:?}", tab.load_state).hash(&mut hash);
        for entry in &tab.entries {
            entry.target.entry_id.0.hash(&mut hash);
            entry.path.hash(&mut hash);
            entry.selected.hash(&mut hash);
        }
    }
    for task in &operations {
        task.id.0.hash(&mut hash);
        format!("{:?}", task.state).hash(&mut hash);
        task.cancellation_requested.hash(&mut hash);
        task.cancel_reason.map(ActionError::code).hash(&mut hash);
        task.error.hash(&mut hash);
    }
    Ok(WindowSnapshot {
        window_id,
        active_tab: window.active_tab,
        tabs,
        operations,
        revision: hash.finish(),
    })
}

pub(super) fn execute(
    state: &SharedSessions,
    workers: &ActionWorkers,
    action: Action,
) -> Result<ActionReceipt, ActionError> {
    match action {
        Action::Query { window_id } => snapshot(state, window_id).map(ActionReceipt::State),
        Action::Select {
            target,
            toggle,
            extend,
        } => {
            let mut app = state.lock().map_err(|_| ActionError::WindowClosed)?;
            checked_entry(&app, target)?;
            app.windows
                .get_mut(&target.tab.window_id)
                .ok_or(ActionError::WindowClosed)?
                .tabs
                .get_mut(&target.tab.tab_id)
                .ok_or(ActionError::TabClosed)?
                .select_entry(target.entry_id, toggle, extend);
            Ok(ActionReceipt::Completed)
        }
        Action::SwitchTab { window_id, tab_id } => {
            let mut app = state.lock().map_err(|_| ActionError::WindowClosed)?;
            if !app
                .window(window_id)
                .ok_or(ActionError::WindowClosed)?
                .tabs
                .contains_key(&tab_id)
            {
                return Err(ActionError::TabClosed);
            }
            app.pending_shell_creates.remove(&window_id);
            app.pending_rename_ui.remove(&window_id);
            app.focus_after_refresh.retain(|_, pending| !matches!(pending.action,
                PendingFocusAction::Reveal { window_id: owner } | PendingFocusAction::Rename { window_id: owner } if owner == window_id));
            app.windows
                .get_mut(&window_id)
                .ok_or(ActionError::WindowClosed)?
                .active_tab = tab_id;
            Ok(ActionReceipt::Completed)
        }
        Action::OpenDirectory { target, path } => navigate(state, workers, target, Some(path)),
        Action::Refresh { target } => navigate(state, workers, target, None),
        Action::Rename { target, name } => rename(state, &workers.operation, target, name),
        Action::Cancel {
            window_id,
            operation_id,
        } => cancel(state, window_id, operation_id),
    }
}
fn navigate(
    state: &SharedSessions,
    workers: &ActionWorkers,
    target: ActionTarget,
    path: Option<PathBuf>,
) -> Result<ActionReceipt, ActionError> {
    let (location, mut kind) = {
        let app = state.lock().map_err(|_| ActionError::WindowClosed)?;
        let tab = checked_tab(&app, target)?;
        if tab.kind != TabKind::Files {
            return Err(ActionError::NotAllowed);
        }
        if let Some(path) = path {
            if !path.is_absolute() {
                return Err(ActionError::NotAllowed);
            }
            (NavigationLocation::Directory(path), NavigationKind::Normal)
        } else {
            if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) {
                return Err(ActionError::NotAllowed);
            }
            (
                tab.requested_location
                    .clone()
                    .or_else(|| tab.current_location.clone())
                    .ok_or(ActionError::NotAllowed)?,
                NavigationKind::Refresh,
            )
        }
    };
    if kind == NavigationKind::Normal {
        let app = state.lock().map_err(|_| ActionError::WindowClosed)?;
        let tab = checked_tab(&app, target)?;
        if tab.visible_location() == Some(&location) {
            match tab.load_state {
                LoadState::Complete => return Ok(ActionReceipt::Completed),
                LoadState::Loading | LoadState::Partial => {
                    return Ok(ActionReceipt::DirectoryAccepted { target });
                }
                _ => kind = NavigationKind::Refresh,
            }
        }
    }
    let sent = submit_location_navigation(
        &workers.directory,
        &workers.network_directory,
        state,
        target.tab_id,
        location,
        kind,
    );
    let app = state.lock().map_err(|_| ActionError::WindowClosed)?;
    let tab = app
        .window(target.window_id)
        .ok_or(ActionError::WindowClosed)?
        .tabs
        .get(&target.tab_id)
        .ok_or(ActionError::TabClosed)?;
    if !sent {
        return Err(ActionError::QueueUnavailable);
    }
    Ok(ActionReceipt::DirectoryAccepted {
        target: ActionTarget {
            request_id: tab.latest_request,
            ..target
        },
    })
}
pub(super) fn cancel(
    state: &SharedSessions,
    window_id: WindowId,
    id: OperationId,
) -> Result<ActionReceipt, ActionError> {
    let mut app = state.lock().map_err(|_| ActionError::WindowClosed)?;
    app.window(window_id).ok_or(ActionError::WindowClosed)?;
    let task = app
        .operations
        .task(id)
        .ok_or(ActionError::TaskUnavailable)?;
    if let Some(reason) = cancellation_reason(task) {
        return Err(reason);
    }
    app.operations
        .cancel(id)
        .map_err(|_| ActionError::NotAllowed)?;
    if let Some(response) = app.conflict_responses.remove(&id) {
        let _ = response.send(crate::domain::file_operations::ConflictDecision {
            action: crate::domain::file_operations::ConflictAction::Skip,
            apply_to_all: false,
        });
    }
    let state = app
        .operations
        .task(id)
        .ok_or(ActionError::TaskUnavailable)?
        .state;
    Ok(ActionReceipt::CancellationRequested { id, state })
}

pub(super) fn rename(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    target: EntryTarget,
    name: OsString,
) -> Result<ActionReceipt, ActionError> {
    crate::fs::file_operations::validate_name(&name).map_err(|_| ActionError::InvalidName)?;
    let item = {
        let app = state.lock().map_err(|_| ActionError::WindowClosed)?;
        let tab = checked_tab(&app, target.tab)?;
        if tab.kind != TabKind::Files || tab.load_state != LoadState::Complete {
            return Err(ActionError::NotAllowed);
        }
        let entry = checked_entry(&app, target)?;
        let parent = entry.path.parent().ok_or(ActionError::NotAllowed)?;
        OperationItem::pending(Some(entry.path.clone()), Some(parent.join(name)))
    };
    enqueue_operation(
        state,
        sender,
        target.tab.tab_id,
        FileOperationKind::Rename,
        vec![item],
        None,
    )
    .map(|id| ActionReceipt::OperationAccepted { id })
    .ok_or(ActionError::QueueUnavailable)
}
#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (SharedSessions, WindowSessions, ActionWorkers) {
        let app = AppState::new_for_test(vec![PathBuf::from(r"C:\old")], 0, [0, 1, 2, 3]);
        let window_id = app.active_window;
        let shared = Arc::new(Mutex::new(app));
        let window = WindowSessions::new(shared.clone(), window_id);
        let (directory, _) = mpsc::channel();
        let (network_directory, _) = mpsc::sync_channel(1);
        let (operation, _) = mpsc::channel();
        (
            shared,
            window,
            ActionWorkers {
                directory,
                network_directory,
                operation,
            },
        )
    }
    fn active_tab(app: &mut AppState) -> &mut TabSession {
        let id = app.active_window_state().active_tab;
        app.tab_mut(id).unwrap()
    }
    fn entry(id: u32, path: &str) -> FileEntry {
        FileEntry {
            id: EntryId(id),
            original_name: Path::new(path).file_name().unwrap().to_os_string(),
            display_name: "same.txt".into(),
            name_highlights: Vec::new(),
            path: path.into(),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            library_source_index: None,
            parent_display: String::new(),
            size_bytes: Some(1),
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
        }
    }
    #[test]
    fn failed_navigation_never_relabels_previous_entries_as_current_targets() {
        let (state, window, workers) = setup();
        {
            let mut app = state.lock().unwrap();
            let tab = active_tab(&mut app);
            tab.replace_entries(vec![entry(1, r"C:\old\same.txt")]);
            tab.begin_directory_navigation(PathBuf::from(r"C:\missing"), NavigationKind::Normal);
            tab.load_state = LoadState::NotFound;
        }
        let current = target(&window).unwrap();
        let view = snapshot(&state, window.window_id).unwrap();
        assert!(view.tabs[0].entries.is_empty());
        assert!(
            view.tabs[0]
                .operations
                .iter()
                .filter(|item| matches!(item.operation, "select" | "rename"))
                .all(|item| item.reason.is_some())
        );
        let target = EntryTarget {
            tab: current,
            entry_id: EntryId(1),
        };
        assert!(matches!(
            execute(
                &state,
                &workers,
                Action::Select {
                    target,
                    toggle: false,
                    extend: false
                }
            ),
            Err(ActionError::NotAllowed)
        ));
        assert!(matches!(
            rename(&state, &workers.operation, target, "new.txt".into()),
            Err(ActionError::NotAllowed)
        ));
    }
    #[test]
    fn partial_navigation_projects_only_new_visible_entries() {
        let (state, window, workers) = setup();
        {
            let mut app = state.lock().unwrap();
            let tab = active_tab(&mut app);
            tab.replace_entries(vec![entry(1, r"C:\old\same.txt")]);
            tab.begin_directory_navigation(PathBuf::from(r"C:\new"), NavigationKind::Normal);
            tab.append_pending(vec![entry(2, r"C:\new\same.txt")]);
        }
        let view = snapshot(&state, window.window_id).unwrap();
        assert_eq!(view.tabs[0].entries.len(), 1);
        let target = view.tabs[0].entries[0].target;
        assert_eq!(target.entry_id, EntryId(2));
        assert!(matches!(
            execute(
                &state,
                &workers,
                Action::Select {
                    target: EntryTarget {
                        entry_id: EntryId(1),
                        ..target
                    },
                    toggle: false,
                    extend: false
                }
            ),
            Err(ActionError::EntryUnavailable)
        ));

        assert_eq!(
            view.tabs[0].entries[0].path,
            PathBuf::from(r"C:\new\same.txt")
        );
        assert!(
            execute(
                &state,
                &workers,
                Action::Select {
                    target,
                    toggle: false,
                    extend: false
                }
            )
            .is_ok()
        );
        assert!(matches!(
            rename(&state, &workers.operation, target, "new.txt".into()),
            Err(ActionError::NotAllowed)
        ));
    }
    #[test]
    fn disconnected_directory_transports_end_the_request() {
        for path in [r"C:\new", r"\\server\share"] {
            let (state, window, workers) = setup();
            let target = target(&window).unwrap();
            assert!(matches!(
                execute(
                    &state,
                    &workers,
                    Action::OpenDirectory {
                        target,
                        path: path.into()
                    }
                ),
                Err(ActionError::QueueUnavailable)
            ));
            let snapshot = snapshot(&state, window.window_id).unwrap();
            assert_eq!(snapshot.tabs[0].load_state, LoadState::Failed);
            assert!(snapshot.tabs[0].entries.is_empty());
        }
    }
    #[test]
    fn disconnected_library_transport_ends_the_refresh_request() {
        let (state, window, workers) = setup();
        let identity = OsString::from("library-test-identity");
        let location = LibraryLocationId::new(identity.clone(), "Library".into());
        {
            let mut app = state.lock().unwrap();
            app.libraries
                .push(platform::windows::libraries::WindowsLibrary {
                    id: platform::windows::libraries::LibraryId::new(identity),
                    definition_path: None,
                    display_name: "Library".into(),
                    pinned: false,
                    sort_order: 0,
                    sources: Vec::new(),
                    default_save_path: None,
                });
            let tab = active_tab(&mut app);
            tab.current_location = Some(NavigationLocation::Library(location));
            tab.load_state = LoadState::Complete;
        }
        let target = target(&window).unwrap();
        assert!(matches!(
            execute(&state, &workers, Action::Refresh { target }),
            Err(ActionError::QueueUnavailable)
        ));
        let snapshot = snapshot(&state, window.window_id).unwrap();
        assert_eq!(snapshot.tabs[0].load_state, LoadState::Failed);
        assert!(snapshot.tabs[0].entries.is_empty());
    }

    #[test]
    fn failed_dispatch_releases_only_its_registered_directory_counts() {
        let (state, _, _) = setup();
        let directory = PathBuf::from(r"C:\source");
        let item = OperationItem::pending(
            Some(directory.join("old.txt")),
            Some(directory.join("new.txt")),
        );
        let (first, queued) = {
            let mut app = state.lock().unwrap();
            let first = app.operations.submit(
                OperationResource::Local,
                FileOperationKind::Rename,
                None,
                vec![item.clone()],
            );
            let queued = app.operations.submit(
                OperationResource::Local,
                FileOperationKind::Rename,
                None,
                vec![item.clone()],
            );
            assert_eq!(
                app.operations.start_next(OperationResource::Local).unwrap(),
                Some(first)
            );
            app.operations.mark_running(first).unwrap();
            register_operation_directories(
                &mut app,
                FileOperationKind::Rename,
                std::slice::from_ref(&item),
            );
            // A different resource can own another count for the same directory.
            register_operation_directories(
                &mut app,
                FileOperationKind::Rename,
                std::slice::from_ref(&item),
            );
            (first, queued)
        };
        super::file_operation_coordinator::fail_operation_dispatch(&state, first);
        let mut app = state.lock().unwrap();
        assert_eq!(
            app.operations.task(first).unwrap().state,
            OperationState::Failed
        );
        assert_eq!(
            app.operations.task(queued).unwrap().state,
            OperationState::Failed
        );
        assert_eq!(app.active_operation_directories.get(&directory), Some(&1));
        release_operation_directories(&mut app, FileOperationKind::Rename, &[item]);
        assert!(!app.active_operation_directories.contains_key(&directory));
    }

    #[test]
    fn reopening_the_current_loading_request_does_not_report_completion() {
        let (state, window, workers) = setup();
        {
            let mut app = state.lock().unwrap();
            active_tab(&mut app)
                .begin_directory_navigation(PathBuf::from(r"C:\new"), NavigationKind::Normal);
        }
        let target = target(&window).unwrap();
        assert!(
            matches!(execute(&state, &workers, Action::OpenDirectory { target, path: r"C:\new".into() }), Ok(ActionReceipt::DirectoryAccepted { target: actual }) if actual == target)
        );
    }
    #[test]
    fn committed_cleanup_cancellation_is_rejected_without_changing_the_token() {
        let (state, window, _) = setup();
        let id = {
            let mut app = state.lock().unwrap();
            let id = app.operations.submit(
                OperationResource::Cleanup,
                FileOperationKind::PermanentDelete,
                None,
                Vec::new(),
            );
            app.operations
                .start_next(OperationResource::Cleanup)
                .unwrap();
            app.operations.mark_running(id).unwrap();
            app.operations
                .task_mut(id)
                .unwrap()
                .set_permanent_delete_stage(PermanentDeleteStage::ReleasingSpace);
            id
        };
        assert!(matches!(
            cancel(&state, window.window_id, id),
            Err(ActionError::NotAllowed)
        ));
        let view = snapshot(&state, window.window_id).unwrap();
        let task = view.operations.iter().find(|task| task.id == id).unwrap();
        assert_eq!(task.cancel_reason, Some(ActionError::NotAllowed));
        assert!(!task.cancellation_requested);
        let app = state.lock().unwrap();
        assert_eq!(
            app.operations.task(id).unwrap().state,
            OperationState::Running
        );
        assert!(
            !operation_rows(&app)
                .iter()
                .find(|row| row.id == id.0 as i32)
                .unwrap()
                .can_cancel
        );
    }

    #[test]
    fn repeated_cancellation_uses_the_same_disabled_state_as_the_ui() {
        let (state, window, _) = setup();
        let id = {
            let mut app = state.lock().unwrap();
            let id = app.operations.submit(
                OperationResource::Local,
                FileOperationKind::Rename,
                None,
                Vec::new(),
            );
            app.operations.start_next(OperationResource::Local).unwrap();
            app.operations.mark_running(id).unwrap();
            id
        };
        assert!(matches!(
            cancel(&state, window.window_id, id),
            Ok(ActionReceipt::CancellationRequested {
                state: OperationState::Cancelling,
                ..
            })
        ));
        assert!(matches!(
            cancel(&state, window.window_id, id),
            Err(ActionError::NotAllowed)
        ));
        let view = snapshot(&state, window.window_id).unwrap();
        let task = view.operations.iter().find(|task| task.id == id).unwrap();
        assert_eq!(task.state, OperationState::Cancelling);
        assert_eq!(task.cancel_reason, Some(ActionError::NotAllowed));
        assert!(task.cancellation_requested);
        let app = state.lock().unwrap();
        assert!(
            !operation_rows(&app)
                .iter()
                .find(|row| row.id == id.0 as i32)
                .unwrap()
                .can_cancel
        );
    }

    #[test]
    fn snapshot_revision_includes_raw_location_and_task_cancellation_availability() {
        let (state, window, _) = setup();
        let before = snapshot(&state, window.window_id).unwrap().revision;
        let id = {
            let mut app = state.lock().unwrap();
            active_tab(&mut app).current_location = Some(NavigationLocation::Home);
            let tab_id = app.active().id;
            app.operations.submit_preparing(tab_id, Vec::new())
        };
        let queued = snapshot(&state, window.window_id).unwrap();
        assert_ne!(before, queued.revision);
        assert_eq!(queued.operations[0].cancel_reason, None);
        cancel(&state, window.window_id, id).unwrap();
        assert_eq!(
            snapshot(&state, window.window_id).unwrap().operations[0].cancel_reason,
            Some(ActionError::NotAllowed)
        );
    }
}
