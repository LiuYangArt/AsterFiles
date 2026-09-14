use super::actions::{
    Action, ActionError, ActionReceipt, ActionTarget, ActionWorkers, EntryTarget, WindowSnapshot,
};
use super::*;
use std::fs;

const TIMEOUT: Duration = Duration::from_secs(15);
macro_rules! check {
    ($evidence:expr, $name:expr, $passed:expr $(,)?) => {{
        let passed = $passed;
        $evidence.check($name, passed)
    }};
}

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> io::Result<Self> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("asterfiles-actions-{}-{nonce}", std::process::id()));
        fs::create_dir(&root)?;
        Ok(Self(root))
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Only this invocation's exclusively created directory is disposable.
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Evidence {
    checks: Vec<(&'static str, bool)>,
    trace: Vec<String>,
}

impl Evidence {
    fn check(&mut self, name: &'static str, passed: bool) -> io::Result<()> {
        self.checks.push((name, passed));
        if passed {
            Ok(())
        } else {
            Err(io::Error::other(format!("check failed: {name}")))
        }
    }

    fn write(&self, path: &Path, failure: Option<&io::Error>) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let checks = self
            .checks
            .iter()
            .map(|(name, passed)| format!("    \"{name}\": {passed}"))
            .collect::<Vec<_>>()
            .join(",\n");
        let trace = self
            .trace
            .iter()
            .map(|line| format!("    {}", json_string(line)))
            .collect::<Vec<_>>()
            .join(",\n");
        let failure = failure
            .map(|error| json_string(&error.to_string()))
            .unwrap_or_else(|| "null".into());
        fs::write(
            path,
            format!(
                "{{\n  \"schema_version\": 1,\n  \"scenario\": \"agent-actions\",\n  \"scope\": \"real_app_actions_and_workers_no_ui_isolated_temp_directory\",\n  \"passed\": {},\n  \"failure\": {failure},\n  \"checks\": {{\n{checks}\n  }},\n  \"trace\": [\n{trace}\n  ]\n}}\n",
                failure == "null"
            ),
        )
    }
}

fn json_string(value: &str) -> String {
    let mut result = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            ch if ch.is_control() => result.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}

struct Harness {
    state: SharedSessions,
    workers: ActionWorkers,
    directory_events: mpsc::Receiver<DirectoryEvent>,
    queued_operations: mpsc::Receiver<FileOperationRequest>,
    operation_sender: mpsc::Sender<FileOperationRequest>,
    operation_events: mpsc::Receiver<FileOperationEvent>,
}

impl Harness {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(AppState::new(
            vec![NavigationLocation::Home],
            0,
            DirectoryViewPreference::default(),
            SearchViewPreference::default(),
            HashMap::new(),
            crate::domain::EverythingConfig::default(),
            session_store::ThemeMode::System,
            Language::Chinese,
            false,
        )));
        let (directory, network_directory, directory_events) = spawn_directory_workers(1, 1);
        let (operation_sender, operation_events) = spawn_file_operation_worker();
        let (operation, queued_operations) = mpsc::channel();
        Self {
            state,
            workers: ActionWorkers {
                directory,
                network_directory,
                operation,
            },
            directory_events,
            queued_operations,
            operation_sender,
            operation_events,
        }
    }

    fn execute(
        &self,
        evidence: &mut Evidence,
        action: Action,
    ) -> Result<ActionReceipt, ActionError> {
        evidence.trace.push(format!("command: {action:?}"));
        let result = actions::execute(&self.state, &self.workers, action);
        evidence.trace.push(format!("receipt: {result:?}"));
        result
    }

    fn query(&self, evidence: &mut Evidence, window_id: WindowId) -> io::Result<WindowSnapshot> {
        match self
            .execute(evidence, Action::Query { window_id })
            .map_err(action_error)?
        {
            ActionReceipt::State(snapshot) => {
                if snapshot.window_id != window_id {
                    return Err(io::Error::other(
                        "query returned a different window identity",
                    ));
                }
                Ok(snapshot)
            }
            receipt => Err(io::Error::other(format!("expected state, got {receipt:?}"))),
        }
    }

    fn target(
        &self,
        evidence: &mut Evidence,
        window: WindowId,
        tab: TabId,
    ) -> io::Result<ActionTarget> {
        self.query(evidence, window)?
            .tabs
            .into_iter()
            .find(|value| value.target.tab_id == tab)
            .map(|value| value.target)
            .ok_or_else(|| io::Error::other("queried tab missing"))
    }

    fn entry(
        &self,
        evidence: &mut Evidence,
        window: WindowId,
        tab: TabId,
        path: &Path,
    ) -> io::Result<EntryTarget> {
        self.query(evidence, window)?
            .tabs
            .into_iter()
            .find(|value| value.target.tab_id == tab)
            .and_then(|value| value.entries.into_iter().find(|entry| entry.path == path))
            .map(|entry| entry.target)
            .ok_or_else(|| io::Error::other(format!("queried entry missing: {}", path.display())))
    }

    fn navigate(
        &self,
        evidence: &mut Evidence,
        window: WindowId,
        tab: TabId,
        path: &Path,
    ) -> io::Result<ActionTarget> {
        let target = self.target(evidence, window, tab)?;
        let receipt = self
            .execute(
                evidence,
                Action::OpenDirectory {
                    target,
                    path: path.to_path_buf(),
                },
            )
            .map_err(action_error)?;
        match receipt {
            ActionReceipt::DirectoryAccepted { target } => Ok(target),
            receipt => Err(io::Error::other(format!(
                "expected directory acceptance, got {receipt:?}"
            ))),
        }
    }

    fn refresh(
        &self,
        evidence: &mut Evidence,
        window: WindowId,
        tab: TabId,
    ) -> io::Result<ActionTarget> {
        let target = self.target(evidence, window, tab)?;
        match self
            .execute(evidence, Action::Refresh { target })
            .map_err(action_error)?
        {
            ActionReceipt::DirectoryAccepted { target } => Ok(target),
            receipt => Err(io::Error::other(format!(
                "expected refresh acceptance, got {receipt:?}"
            ))),
        }
    }

    fn collect_directory(&self, target: ActionTarget) -> io::Result<Vec<DirectoryEvent>> {
        let deadline = Instant::now() + TIMEOUT;
        let mut events = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let event = self
                .directory_events
                .recv_timeout(remaining)
                .map_err(io::Error::other)?;
            let terminal = event.request_identity() == (target.tab_id, target.request_id)
                && matches!(
                    event,
                    DirectoryEvent::Finished { .. }
                        | DirectoryEvent::Failed { .. }
                        | DirectoryEvent::Cancelled { .. }
                );
            events.push(event);
            if terminal {
                return Ok(events);
            }
        }
    }

    fn wait_directory(
        &self,
        evidence: &mut Evidence,
        target: ActionTarget,
    ) -> io::Result<LoadState> {
        for event in self.collect_directory(target)? {
            evidence.trace.push(format!("directory event: {event:?}"));
            apply_event(&self.state, event);
        }
        self.query(evidence, target.window_id)?
            .tabs
            .into_iter()
            .find(|tab| tab.target == target)
            .map(|tab| tab.load_state)
            .ok_or_else(|| io::Error::other("directory request no longer current"))
    }

    fn rename(
        &self,
        evidence: &mut Evidence,
        target: EntryTarget,
        name: &str,
    ) -> io::Result<OperationId> {
        match self
            .execute(
                evidence,
                Action::Rename {
                    target,
                    name: name.into(),
                },
            )
            .map_err(action_error)?
        {
            ActionReceipt::OperationAccepted { id } => Ok(id),
            receipt => Err(io::Error::other(format!(
                "expected task acceptance, got {receipt:?}"
            ))),
        }
    }

    fn prepared_operation(&self, expected: OperationId) -> io::Result<FileOperationRequest> {
        // Holding transport delivery makes cancellation deterministic without replacing worker logic.
        let request = self
            .queued_operations
            .recv_timeout(TIMEOUT)
            .map_err(io::Error::other)?;
        if request.id != expected {
            return Err(io::Error::other("operation identity mismatch"));
        }
        Ok(request)
    }

    fn dispatch_operation(&self, expected: OperationId) -> io::Result<()> {
        self.operation_sender
            .send(self.prepared_operation(expected)?)
            .map_err(io::Error::other)
    }

    fn wait_operation(
        &self,
        evidence: &mut Evidence,
        window: WindowId,
        id: OperationId,
    ) -> io::Result<OperationState> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let event = self
                .operation_events
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .map_err(io::Error::other)?;
            evidence.trace.push(format!("file event: {event:?}"));
            if matches!(event, FileOperationEvent::Conflict { .. }) {
                return Err(io::Error::other("unexpected conflict prompt for rename"));
            }
            let finished = matches!(&event, FileOperationEvent::Finished { id: finished_id, .. } if *finished_id == id);
            if let Some(completion) = finish_file_operation(&self.state, event) {
                evidence
                    .trace
                    .push(format!("affected directories: {:?}", completion.affected));
                if let Some(next) = completion.next {
                    self.operation_sender.send(next).map_err(io::Error::other)?;
                }
            }
            if finished {
                return self
                    .query(evidence, window)?
                    .operations
                    .into_iter()
                    .find(|task| task.id == id)
                    .map(|task| task.state)
                    .ok_or_else(|| io::Error::other("completed task vanished before query"));
            }
        }
    }
}

fn action_error(error: ActionError) -> io::Error {
    io::Error::other(error.code())
}

pub(crate) fn export(path: &Path) -> io::Result<()> {
    let mut evidence = Evidence::default();
    let result = run(&mut evidence);
    evidence.write(path, result.as_ref().err())?;
    result
}

fn run(evidence: &mut Evidence) -> io::Result<()> {
    let sandbox = Sandbox::new()?;
    let first = sandbox.0.join("窗口甲");
    let second = sandbox.0.join("窗口乙");
    fs::create_dir(&first)?;
    fs::create_dir(&second)?;
    let first_file = first.join("同名中文.txt");
    let second_file = second.join("同名中文.txt");
    fs::write(&first_file, b"first-window")?;
    fs::write(&second_file, b"second-window")?;
    fs::write(first.join("冲突.txt"), b"keep-existing")?;
    let harness = Harness::new();
    let first_window = WindowId(1);
    let initial = harness.query(evidence, first_window)?;
    let first_tab = initial.active_tab;
    check!(
        evidence,
        "query_projects_real_home",
        initial.tabs.len() == 1 && initial.tabs[0].location == Some(NavigationLocation::Home),
    )?;

    let home_target = harness.target(evidence, first_window, first_tab)?;
    check!(
        evidence,
        "relative_navigation_rejected",
        matches!(
            harness.execute(
                evidence,
                Action::OpenDirectory {
                    target: home_target,
                    path: PathBuf::from("relative")
                }
            ),
            Err(ActionError::NotAllowed)
        )
    )?;
    check!(
        evidence,
        "unknown_task_rejected",
        matches!(
            harness.execute(
                evidence,
                Action::Cancel {
                    window_id: first_window,
                    operation_id: OperationId(u64::MAX)
                }
            ),
            Err(ActionError::TaskUnavailable)
        )
    )?;
    let navigation = harness.navigate(evidence, first_window, first_tab, &first)?;
    let pending = harness.query(evidence, first_window)?;
    check!(
        evidence,
        "refresh_while_loading_rejected",
        matches!(
            harness.execute(evidence, Action::Refresh { target: navigation }),
            Err(ActionError::NotAllowed)
        )
    )?;
    check!(
        evidence,
        "navigation_acceptance_is_not_completion",
        pending.tabs[0].load_state == LoadState::Loading,
    )?;
    check!(
        evidence,
        "directory_worker_completed",
        harness.wait_directory(evidence, navigation)? == LoadState::Complete,
    )?;

    check!(
        evidence,
        "loading_availability_has_reason",
        pending.tabs[0]
            .operations
            .iter()
            .any(|action| action.operation == "rename"
                && action.reason == Some(ActionError::NotAllowed))
    )?;
    let target = harness.entry(evidence, first_window, first_tab, &first_file)?;
    let before_selection = harness.query(evidence, first_window)?.revision;
    harness
        .execute(
            evidence,
            Action::Select {
                target,
                toggle: false,
                extend: false,
            },
        )
        .map_err(action_error)?;
    let selected = harness.query(evidence, first_window)?;
    check!(
        evidence,
        "selection_uses_queried_identity",
        selected.tabs[0].selected == vec![target.entry_id]
            && selected.tabs[0]
                .entries
                .iter()
                .any(|entry| entry.target == target && entry.selected),
    )?;
    check!(
        evidence,
        "selection_changes_revision",
        selected.revision != before_selection,
    )?;

    let (second_window, second_tab) = {
        let mut app = harness
            .state
            .lock()
            .map_err(|_| io::Error::other("app mutex poisoned"))?;
        let window = app.register_window(
            vec![NavigationLocation::Home],
            0,
            session_store::WindowPlacement {
                x: 0,
                y: 0,
                width: 1024,
                height: 768,
            },
        );
        (
            window,
            app.window(window)
                .expect("registered window exists")
                .active_tab,
        )
    };
    let navigation = harness.navigate(evidence, second_window, second_tab, &second)?;
    check!(
        evidence,
        "second_window_loaded",
        harness.wait_directory(evidence, navigation)? == LoadState::Complete,
    )?;
    let other = harness.entry(evidence, second_window, second_tab, &second_file)?;
    check!(
        evidence,
        "same_names_have_distinct_identity",
        target != other
    )?;

    {
        let mut app = harness
            .state
            .lock()
            .map_err(|_| io::Error::other("app mutex poisoned"))?;
        app.active_window = second_window;
    }
    let rename = harness.rename(evidence, target, "重命名完成.txt")?;
    let accepted = harness.query(evidence, first_window)?;
    check!(
        evidence,
        "rename_acceptance_is_not_completion",
        accepted
            .operations
            .iter()
            .any(|task| task.id == rename && task.state.is_active())
            && first_file.exists()
            && !first.join("重命名完成.txt").exists(),
    )?;
    harness.dispatch_operation(rename)?;
    check!(
        evidence,
        "rename_worker_completed",
        harness.wait_operation(evidence, first_window, rename)? == OperationState::Completed,
    )?;
    check!(
        evidence,
        "rename_only_touches_exact_window_path",
        !first_file.exists()
            && fs::read(first.join("重命名完成.txt"))? == b"first-window"
            && fs::read(&second_file)? == b"second-window",
    )?;

    let refreshed = harness.refresh(evidence, first_window, first_tab)?;
    check!(
        evidence,
        "refresh_invalidates_entry_identity",
        matches!(
            harness.execute(
                evidence,
                Action::Select {
                    target,
                    toggle: false,
                    extend: false
                }
            ),
            Err(ActionError::StaleRequest)
        ),
    )?;
    check!(
        evidence,
        "refresh_worker_completed",
        harness.wait_directory(evidence, refreshed)? == LoadState::Complete,
    )?;
    let renamed = harness.entry(
        evidence,
        first_window,
        first_tab,
        &first.join("重命名完成.txt"),
    )?;
    check!(
        evidence,
        "invalid_name_rejected",
        matches!(
            harness.execute(
                evidence,
                Action::Rename {
                    target: renamed,
                    name: "invalid/name".into()
                }
            ),
            Err(ActionError::InvalidName)
        ),
    )?;
    let unavailable = EntryTarget {
        entry_id: EntryId(u32::MAX),
        ..renamed
    };
    check!(
        evidence,
        "missing_entry_rejected",
        matches!(
            harness.execute(
                evidence,
                Action::Select {
                    target: unavailable,
                    toggle: false,
                    extend: false
                }
            ),
            Err(ActionError::EntryUnavailable)
        ),
    )?;

    let conflict = harness.rename(evidence, renamed, "冲突.txt")?;
    harness.dispatch_operation(conflict)?;
    check!(
        evidence,
        "rename_conflict_uses_existing_failure_rule",
        harness.wait_operation(evidence, first_window, conflict)? == OperationState::Failed,
    )?;
    check!(
        evidence,
        "conflict_never_overwrites",
        fs::read(first.join("冲突.txt"))? == b"keep-existing"
            && fs::read(first.join("重命名完成.txt"))? == b"first-window",
    )?;
    check!(
        evidence,
        "failed_task_exposes_error",
        harness
            .query(evidence, first_window)?
            .operations
            .iter()
            .any(|task| task.id == conflict && task.error.is_some()),
    )?;

    let cancelled = harness.rename(evidence, renamed, "不应创建.txt")?;
    let prepared = harness.prepared_operation(cancelled)?;
    check!(
        evidence,
        "cancel_waits_for_running_task",
        harness
            .query(evidence, first_window)?
            .operations
            .iter()
            .any(|task| task.id == cancelled && task.state == OperationState::Running)
    )?;
    let receipt = harness
        .execute(
            evidence,
            Action::Cancel {
                window_id: first_window,
                operation_id: cancelled,
            },
        )
        .map_err(action_error)?;
    check!(
        evidence,
        "cancel_acceptance_is_not_completion",
        matches!(receipt,
        ActionReceipt::CancellationRequested { id, state: OperationState::Cancelling } if id == cancelled)
    )?;
    let cancelling = harness.query(evidence, first_window)?;
    check!(
        evidence,
        "cancel_query_reports_request",
        cancelling.operations.iter().any(|task| {
            task.id == cancelled
                && task.cancellation_requested
                && task.state == OperationState::Cancelling
        }),
    )?;
    harness
        .operation_sender
        .send(prepared)
        .map_err(io::Error::other)?;
    check!(
        evidence,
        "cancel_worker_reaches_terminal",
        harness.wait_operation(evidence, first_window, cancelled)? == OperationState::Cancelled,
    )?;
    check!(
        evidence,
        "cancel_preserves_source",
        first.join("重命名完成.txt").exists() && !first.join("不应创建.txt").exists(),
    )?;
    check!(
        evidence,
        "terminal_task_cannot_cancel",
        matches!(
            harness.execute(
                evidence,
                Action::Cancel {
                    window_id: first_window,
                    operation_id: cancelled
                }
            ),
            Err(ActionError::NotAllowed)
        ),
    )?;

    let old_request = harness.refresh(evidence, first_window, first_tab)?;
    let late_events = harness.collect_directory(old_request)?;
    let new_request = harness.navigate(evidence, first_window, first_tab, &second)?;
    check!(
        evidence,
        "new_navigation_completed",
        harness.wait_directory(evidence, new_request)? == LoadState::Complete,
    )?;
    let before_late = harness.query(evidence, first_window)?.revision;
    for event in late_events {
        evidence
            .trace
            .push(format!("delayed directory event: {event:?}"));
        apply_event(&harness.state, event);
    }
    let after_late = harness.query(evidence, first_window)?;
    check!(
        evidence,
        "late_directory_results_rejected",
        before_late == after_late.revision
            && after_late.tabs[0].location == Some(NavigationLocation::Directory(second.clone())),
    )?;

    let missing = harness.navigate(evidence, first_window, first_tab, &sandbox.0.join("不存在"))?;
    check!(
        evidence,
        "missing_directory_reports_failure",
        harness.wait_directory(evidence, missing)? == LoadState::NotFound,
    )?;
    let restored = harness.navigate(evidence, first_window, first_tab, &first)?;
    check!(
        evidence,
        "navigation_recovers_after_failure",
        harness.wait_directory(evidence, restored)? == LoadState::Complete,
    )?;

    let deleted = harness.entry(
        evidence,
        first_window,
        first_tab,
        &first.join("重命名完成.txt"),
    )?;
    fs::remove_file(first.join("重命名完成.txt"))?;
    let missing_source = harness.rename(evidence, deleted, "不应成功.txt")?;
    harness.dispatch_operation(missing_source)?;
    check!(
        evidence,
        "removed_source_fails_in_real_worker",
        harness.wait_operation(evidence, first_window, missing_source)? == OperationState::Failed,
    )?;

    let disposable_tab = {
        let mut app = harness
            .state
            .lock()
            .map_err(|_| io::Error::other("app mutex poisoned"))?;
        app.create_tab_in_window(first_window, NavigationLocation::Home)
            .expect("window exists")
    };
    harness
        .execute(
            evidence,
            Action::SwitchTab {
                window_id: first_window,
                tab_id: disposable_tab,
            },
        )
        .map_err(action_error)?;
    check!(
        evidence,
        "switch_tab_updates_real_window",
        harness.query(evidence, first_window)?.active_tab == disposable_tab,
    )?;
    let closing = harness.navigate(evidence, first_window, disposable_tab, &second)?;
    let closing_events = harness.collect_directory(closing)?;
    let close_target = harness.target(evidence, first_window, disposable_tab)?;
    {
        let mut app = harness
            .state
            .lock()
            .map_err(|_| io::Error::other("app mutex poisoned"))?;
        app.active_window = first_window;
        app.close_tab(disposable_tab);
    }
    for event in closing_events {
        apply_event(&harness.state, event);
    }
    check!(
        evidence,
        "closed_tab_rejects_late_results_and_actions",
        !harness
            .query(evidence, first_window)?
            .tabs
            .iter()
            .any(|tab| tab.target.tab_id == disposable_tab)
            && matches!(
                harness.execute(
                    evidence,
                    Action::Refresh {
                        target: close_target
                    }
                ),
                Err(ActionError::TabClosed)
            ),
    )?;

    {
        let mut app = harness
            .state
            .lock()
            .map_err(|_| io::Error::other("app mutex poisoned"))?;
        app.close_window(second_window);
    }
    check!(
        evidence,
        "closed_window_does_not_redirect",
        matches!(
            harness.execute(
                evidence,
                Action::Select {
                    target: other,
                    toggle: false,
                    extend: false
                }
            ),
            Err(ActionError::WindowClosed)
        ),
    )?;
    check!(
        evidence,
        "other_window_file_remains_unchanged",
        fs::read(&second_file)? == b"second-window",
    )?;
    Ok(())
}
