//! File task admission/completion coordinates shared state under the existing application lock.
//! Worker messages contain paths and task identities; window presentation stays in app.
use super::file_operation_worker::{
    FileOperationEvent, FileOperationRequest, undo_source_kind, uses_local_recycle_batch,
};
use super::{
    AppState, RecentOperationChanges, SharedSessions, WindowId, queue_completed_focus,
    queue_completed_rename, refresh_all_windows, refresh_operation_badges, undo_failure_message,
};
use crate::{
    domain::{
        TabId,
        file_operations::{
            FileOperationKind, ItemState, OperationId, OperationItem, OperationManager,
            OperationResource, OperationResult, OperationState, PermanentDeleteStage,
            UndoBeginError, UndoEntry, UndoHistory, UndoItem,
        },
    },
    i18n::{Language, Texts},
    platform,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
fn capture_undo_source_manifests(
    kind: FileOperationKind,
    resource: OperationResource,
    items: &[OperationItem],
) -> Vec<Option<Vec<(PathBuf, crate::domain::file_operations::FileIdentity)>>> {
    if resource != OperationResource::Local
        || !matches!(kind, FileOperationKind::Rename | FileOperationKind::Move)
    {
        return vec![None; items.len()];
    }
    let mut remaining = UndoHistory::MAX_SNAPSHOT_ITEMS;
    items
        .iter()
        .map(|item| {
            let source = item.source.as_deref()?;
            let manifest =
                crate::fs::file_operations::collect_tree_identities(source, remaining).ok()?;
            remaining = remaining.saturating_sub(manifest.len());
            Some(manifest)
        })
        .collect()
}

pub(super) fn fail_operation_dispatch(state: &SharedSessions, first: OperationId) {
    let Ok(mut app) = state.lock() else {
        return;
    };
    let Some(resource) = app.operations.task(first).map(|task| task.resource) else {
        return;
    };
    let mut next = Some(first);
    while let Some(id) = next {
        let mut result = OperationResult {
            succeeded: Vec::new(),
            skipped: Vec::new(),
            failed: Vec::new(),
            affected_directories: Vec::new(),
        };
        if let Some(task) = app.operations.task_mut(id) {
            for item in &mut task.items {
                item.state = ItemState::Failed;
                item.error = Some("queue-unavailable".to_owned());
                if let Some(path) = item.source.clone().or_else(|| item.destination.clone()) {
                    result.failed.push((path, "queue-unavailable".to_owned()));
                }
            }
        }
        // Only the first request reached dispatch; queued followers never registered directories.
        if id == first
            && let Some(task) = app.operations.task(id)
        {
            let kind = task.kind;
            let items = task.items.clone();
            release_operation_directories(&mut app, kind, &items);
        }
        let _ = app.operations.finish(id, OperationState::Failed, result);
        next = app.operations.start_next(resource).ok().flatten();
    }
}

fn enqueue_rename_preparation(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    origin_tab: TabId,
    mut items: Vec<OperationItem>,
) -> Option<OperationId> {
    if items.iter().any(|item| {
        item.source
            .as_deref()
            .is_some_and(crate::domain::folder_size_scheduler::is_internal_cleanup_path)
            || item
                .destination
                .as_deref()
                .is_some_and(crate::domain::folder_size_scheduler::is_internal_cleanup_path)
    }) {
        return None;
    }
    let id = {
        let mut app = state.lock().ok()?;
        app.tab(origin_tab)?;
        app.operations.submit_preparing(origin_tab, items.clone())
    };
    let state = state.clone();
    let sender = sender.clone();
    thread::spawn(move || {
        for item in &mut items {
            if let Some(source) = item.source.take() {
                item.source = Some(
                    platform::windows::network::network_drive_to_unc(&source).unwrap_or(source),
                );
            }
            if let Some(destination) = item.destination.take() {
                item.destination = Some(
                    platform::windows::network::network_drive_to_unc(&destination)
                        .unwrap_or(destination),
                );
            }
        }
        let resource = operation_resource(&items);
        let manifests = capture_undo_source_manifests(FileOperationKind::Rename, resource, &items);
        let request = {
            let Ok(mut app) = state.lock() else {
                return;
            };
            if !app
                .operations
                .complete_preparation(id, resource, items, manifests)
            {
                return;
            }
            app.operations
                .start_next(resource)
                .ok()
                .flatten()
                .and_then(|next| {
                    mark_operation_running_if_ready(&mut app.operations, next);
                    let task = app.operations.task(next)?;
                    let request = FileOperationRequest {
                        id: next,
                        kind: task.kind,
                        resource: task.resource,
                        items: task.items.clone(),
                        undo_items: task.undo_items.clone(),
                        undo_source_manifests: task.undo_source_manifests.clone(),
                        cancellation: task.cancellation.clone(),
                    };
                    register_operation_directories(&mut app, request.kind, &request.items);
                    Some(request)
                })
        };
        if let Some(request) = request {
            let request_id = request.id;
            if sender.send(request).is_err() {
                fail_operation_dispatch(&state, request_id);
            }
        }
        refresh_operation_badges(&state);
    });
    Some(id)
}

pub(super) fn enqueue_operation(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    origin_tab: TabId,
    kind: FileOperationKind,
    mut items: Vec<OperationItem>,
) -> Option<OperationId> {
    if items.is_empty() {
        return None;
    }
    if kind == FileOperationKind::Rename {
        return enqueue_rename_preparation(state, sender, origin_tab, items);
    }
    for item in &mut items {
        if let Some(source) = item.source.take() {
            item.source =
                Some(platform::windows::network::network_drive_to_unc(&source).unwrap_or(source));
        }
        if let Some(destination) = item.destination.take() {
            item.destination = Some(
                platform::windows::network::network_drive_to_unc(&destination)
                    .unwrap_or(destination),
            );
        }
    }
    if items.iter().any(|item| {
        item.source
            .as_deref()
            .is_some_and(crate::domain::folder_size_scheduler::is_internal_cleanup_path)
            || item
                .destination
                .as_deref()
                .is_some_and(crate::domain::folder_size_scheduler::is_internal_cleanup_path)
    }) {
        return None;
    }
    let resource = operation_resource(&items);
    let (operation_id, request) = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.tab(origin_tab)?;
        let undo_source_manifests = capture_undo_source_manifests(kind, resource, &items);
        crate::operation_audit::record(
            "operation_enqueued",
            format!("tab={origin_tab:?} kind={kind:?} items={items:?}"),
        );
        let operation_id = app
            .operations
            .submit(resource, kind, Some(origin_tab), items);
        app.operations
            .set_undo_source_manifests(operation_id, undo_source_manifests);
        if app.operations.active_id(resource).is_some() {
            return Some(operation_id);
        }
        let request = app
            .operations
            .start_next(resource)
            .ok()
            .flatten()
            .and_then(|id| {
                mark_operation_running_if_ready(&mut app.operations, id);
                let task = app.operations.task(id)?;
                let request = FileOperationRequest {
                    id,
                    kind: task.kind,
                    resource: task.resource,
                    items: task.items.clone(),
                    undo_items: task.undo_items.clone(),
                    undo_source_manifests: task.undo_source_manifests.clone(),
                    cancellation: task.cancellation.clone(),
                };
                register_operation_directories(&mut app, request.kind, &request.items);
                Some(request)
            });
        (operation_id, request)
    };
    refresh_operation_badges(state);
    if let Some(request) = request
        && sender.send(request).is_err()
    {
        fail_operation_dispatch(state, operation_id);
        return None;
    }
    Some(operation_id)
}

fn schedule_operation_notice_clear(state: SharedSessions, generation: u64) {
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(4));
        let _ = slint::invoke_from_event_loop(move || {
            if let Ok(mut app) = state.lock()
                && app
                    .operation_notice
                    .as_ref()
                    .is_some_and(|(current, _)| *current == generation)
            {
                app.operation_notice = None;
            }
            refresh_all_windows(&state);
        });
    });
}
fn show_operation_notice(state: &SharedSessions, message: String) {
    let generation = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        app.operation_notice_generation = app.operation_notice_generation.wrapping_add(1).max(1);
        let generation = app.operation_notice_generation;
        app.operation_notice = Some((generation, message));
        generation
    };
    refresh_all_windows(state);
    schedule_operation_notice_clear(state.clone(), generation);
}
pub(super) fn request_undo(
    state: &SharedSessions,
    sender: &mpsc::Sender<FileOperationRequest>,
    origin_tab: TabId,
) {
    let request = {
        let mut app = state.lock().expect("app state mutex is not poisoned");
        let busy = app
            .operations
            .iter()
            .any(|task| task.resource == OperationResource::Local && task.state.is_active());
        let entry = match app.undo_history.begin(busy) {
            Ok(entry) => entry,
            Err(UndoBeginError::Empty) => {
                let message = Texts::new(app.language).undo_empty().to_owned();
                drop(app);
                show_operation_notice(state, message);
                return;
            }
            Err(UndoBeginError::Busy) => {
                let message = Texts::new(app.language).undo_busy().to_owned();
                drop(app);
                show_operation_notice(state, message);
                return;
            }
        };
        let source_kind = entry.source_kind;
        let undo_items = entry.items;
        let submitted_id =
            app.operations
                .submit_undo(Some(origin_tab), source_kind, undo_items.clone());
        let Some(id) = app
            .operations
            .start_next(OperationResource::Local)
            .ok()
            .flatten()
        else {
            let message = Texts::new(app.language).undo_unavailable().to_owned();
            app.operations.abort_before_dispatch(submitted_id);
            app.undo_history.finish(undo_items, Some(message.clone()));
            drop(app);
            show_operation_notice(state, message);
            return;
        };
        let _ = app.operations.mark_running(id);
        let task = app.operations.task(id).expect("started undo task exists");
        FileOperationRequest {
            id,
            kind: task.kind,
            resource: task.resource,
            items: task.items.clone(),
            undo_items: task.undo_items.clone(),
            undo_source_manifests: vec![None; task.items.len()],
            cancellation: task.cancellation.clone(),
        }
    };
    refresh_operation_badges(state);
    let operation_id = request.id;
    let undo_items = request.undo_items.clone();
    if sender.send(request).is_err()
        && let Ok(mut app) = state.lock()
    {
        let message = Texts::new(app.language).undo_unavailable().to_owned();
        app.operations.abort_before_dispatch(operation_id);
        app.undo_history.finish(undo_items, Some(message.clone()));
        drop(app);
        show_operation_notice(state, message);
    }
}

fn operation_resource(items: &[OperationItem]) -> OperationResource {
    if items.iter().any(|item| {
        item.source
            .as_deref()
            .is_some_and(crate::network::is_unc_path)
            || item
                .destination
                .as_deref()
                .is_some_and(crate::network::is_unc_path)
    }) {
        OperationResource::Network
    } else {
        OperationResource::Local
    }
}

fn operation_directories(kind: FileOperationKind, items: &[OperationItem]) -> HashSet<PathBuf> {
    items
        .iter()
        .flat_map(|item| match kind {
            FileOperationKind::CreateFolder => item
                .destination
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .into_iter()
                .collect::<Vec<_>>(),
            FileOperationKind::Rename => [
                item.source.as_deref().and_then(Path::parent),
                item.destination.as_deref().and_then(Path::parent),
            ]
            .into_iter()
            .flatten()
            .map(Path::to_path_buf)
            .collect(),
            FileOperationKind::Copy | FileOperationKind::Move => item
                .destination
                .as_ref()
                .into_iter()
                .flat_map(|destination| {
                    [
                        Some(destination.clone()),
                        destination.parent().map(Path::to_path_buf),
                    ]
                })
                .flatten()
                .chain(
                    (kind == FileOperationKind::Move)
                        .then(|| item.source.as_deref().and_then(Path::parent))
                        .flatten()
                        .map(Path::to_path_buf),
                )
                .collect(),
            FileOperationKind::RecycleDelete
            | FileOperationKind::PermanentDelete
            | FileOperationKind::FastRemove => item
                .source
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .into_iter()
                .collect(),
            FileOperationKind::Undo => [item.source.as_deref(), item.destination.as_deref()]
                .into_iter()
                .flatten()
                .filter_map(Path::parent)
                .map(Path::to_path_buf)
                .collect(),
        })
        .collect()
}

fn running_operation_directories(
    kind: FileOperationKind,
    items: &[OperationItem],
) -> HashSet<PathBuf> {
    operation_directories(kind, items)
        .into_iter()
        .filter(|path| !crate::network::is_unc_path(path))
        .collect()
}

pub(super) fn register_operation_directories(
    app: &mut AppState,
    kind: FileOperationKind,
    items: &[OperationItem],
) {
    for directory in running_operation_directories(kind, items) {
        *app.active_operation_directories
            .entry(directory)
            .or_insert(0) += 1;
    }
}

pub(super) fn release_operation_directories(
    app: &mut AppState,
    kind: FileOperationKind,
    items: &[OperationItem],
) -> HashSet<PathBuf> {
    let directories = running_operation_directories(kind, items);
    for directory in &directories {
        let remove = app
            .active_operation_directories
            .get_mut(directory)
            .is_some_and(|count| {
                *count = count.saturating_sub(1);
                *count == 0
            });
        if remove {
            app.active_operation_directories.remove(directory);
        }
    }
    directories
}

pub(super) fn mark_recent_operation_changes(
    app: &mut AppState,
    directories: &HashSet<PathBuf>,
    items: &[OperationItem],
) {
    let now = Instant::now();
    for directory in directories {
        let paths = items
            .iter()
            .filter(|item| item.state == ItemState::Succeeded)
            .flat_map(|item| [item.source.as_ref(), item.destination.as_ref()])
            .flatten()
            .filter(|path| {
                path.parent() == Some(directory.as_path())
                    || path.as_path() == directory
                    || path.starts_with(directory)
            })
            .cloned()
            .collect();
        app.recent_operation_changes.insert(
            directory.clone(),
            RecentOperationChanges {
                paths,
                recorded_at: now,
            },
        );
    }
}

pub(super) struct OperationCompletion {
    pub(super) affected: Vec<PathBuf>,
    pub(super) next: Option<FileOperationRequest>,
    pub(super) clear_completed_cut: Vec<PathBuf>,
    pub(super) undo_failure: Option<String>,
    pub(super) containment_notice: Option<(WindowId, Language, &'static str)>,
}

pub(super) fn self_containment_notice(
    app: &AppState,
    id: OperationId,
    result: &OperationResult,
) -> Option<(WindowId, Language, &'static str)> {
    let task = app.operations.task(id)?;
    if !matches!(task.kind, FileOperationKind::Copy | FileOperationKind::Move)
        || !result
            .failed
            .iter()
            .any(|(_, error)| error == super::file_operation_worker::SELF_CONTAINMENT_ERROR)
    {
        return None;
    }
    // Follow the originating tab after a detach; never show in an unrelated active window.
    let window = app.window_for_tab(task.origin_tab?)?;
    Some((
        window,
        app.language,
        Texts::new(app.language).self_containment_message(),
    ))
}

fn file_operation_terminal_state(
    cancelled: bool,
    result: &OperationResult,
    has_retained_source: bool,
) -> OperationState {
    if has_retained_source {
        OperationState::PartiallyCompleted
    } else if cancelled && result.succeeded.is_empty() && result.failed.is_empty() {
        OperationState::Cancelled
    } else if cancelled || (!result.failed.is_empty() && !result.succeeded.is_empty()) {
        OperationState::PartiallyCompleted
    } else if result.failed.is_empty() {
        OperationState::Completed
    } else {
        OperationState::Failed
    }
}

pub(super) fn finish_file_operation(
    state: &SharedSessions,
    event: FileOperationEvent,
) -> Option<OperationCompletion> {
    let FileOperationEvent::Finished {
        id,
        result,
        item_states,
        completed_targets,
        undo_items,
        failed_undo_items,
    } = event
    else {
        return None;
    };
    crate::operation_audit::record(
        "operation_finished",
        format!("id={id:?} result={result:?} items={item_states:?}"),
    );
    let (affected, next, clear_completed_cut, undo_failure, containment_notice) = {
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
        let has_retained_source = app.operations.task(id).is_some_and(|task| {
            task.items.iter().any(|item| {
                item.state == ItemState::Succeeded && item.error.is_some()
            })
        });
        let terminal = file_operation_terminal_state(
            cancelled,
            &result,
            has_retained_source,
        );
        let (resource, kind, origin_tab, task_items, undo_cut_paths, undo_task_source_kind) = app
            .operations
            .task(id)
            .map(|task| {
                (
                    task.resource,
                    task.kind,
                    task.origin_tab,
                    task.items.clone(),
                    task.undo_items
                        .iter()
                        .zip(task.items.iter())
                        .filter_map(|(item, task_item)| match item {
                            UndoItem::MoveBack { original, .. }
                                if task_item.state == ItemState::Succeeded =>
                            {
                                Some(original.clone())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                    task.undo_source_kind,
                )
            })
            .unwrap_or((
                OperationResource::Local,
                FileOperationKind::Copy,
                None,
                Vec::new(),
                Vec::new(),
                None,
            ));
        let mut affected = result.affected_directories.clone();
        let registered = if resource == OperationResource::Cleanup {
            HashSet::new()
        } else {
            release_operation_directories(&mut app, kind, &task_items)
        };
        affected.extend(registered.iter().cloned());
        let deferred = app
            .deferred_watch_directories
            .iter()
            .filter(|path| registered.contains(*path))
            .cloned()
            .collect::<Vec<_>>();
        for path in deferred {
            app.deferred_watch_directories.remove(&path);
            affected.push(path);
        }
        affected.sort();
        affected.dedup();
        mark_recent_operation_changes(&mut app, &registered, &task_items);
        app.conflict_responses.remove(&id);
        if kind == FileOperationKind::CreateFolder {
            if let (Some(origin_tab), Some(target)) =
                (origin_tab, completed_targets.first().cloned())
            {
                queue_completed_rename(&mut app, origin_tab, target);
            }
        } else {
            queue_completed_focus(&mut app, &completed_targets);
        }
        if kind == FileOperationKind::Undo {
            let failure = failed_undo_items
                .first()
                .map(|_| Texts::new(app.language).undo_partial().to_owned());
            let notice = failure.clone();
            app.undo_history.finish(failed_undo_items, failure);
            if let Some(message) = notice {
                app.operation_notice_generation =
                    app.operation_notice_generation.wrapping_add(1).max(1);
                let generation = app.operation_notice_generation;
                app.operation_notice = Some((generation, message));
                schedule_operation_notice_clear(state.clone(), generation);
            }
        } else if terminal == OperationState::Completed
            && result.failed.is_empty()
            && result.skipped.is_empty()
            && task_items
                .iter()
                .all(|item| item.state == ItemState::Succeeded && item.error.is_none())
            && let (Some(source_kind), Some(items)) = (undo_source_kind(kind), undo_items)
            && !items.is_empty()
        {
            app.undo_history.push(UndoEntry { source_kind, items });
        }
        let undo_failure = if kind == FileOperationKind::Undo && !result.failed.is_empty() {
            Some(undo_failure_message(
                app.language,
                undo_task_source_kind,
                &result.failed[0].1,
            ))
        } else {
            None
        };
        let containment_notice = self_containment_notice(&app, id, &result);
        let _ = app.operations.finish(id, terminal, result);
        let next = app
            .operations
            .start_next(resource)
            .ok()
            .flatten()
            .and_then(|next_id| {
                mark_operation_running_if_ready(&mut app.operations, next_id);
                app.operations.task(next_id).cloned().map(|task| {
                    let request = FileOperationRequest {
                        id: next_id,
                        kind: task.kind,
                        resource: task.resource,
                        items: task.items.clone(),
                        undo_items: task.undo_items.clone(),
                        undo_source_manifests: task.undo_source_manifests.clone(),
                        cancellation: task.cancellation.clone(),
                    };
                    if request.resource != OperationResource::Cleanup {
                        register_operation_directories(&mut app, request.kind, &request.items);
                    }
                    request
                })
            });
        let clear_completed_cut = if kind == FileOperationKind::Undo {
            undo_cut_paths
        } else {
            Vec::new()
        };

        (affected, next, clear_completed_cut, undo_failure, containment_notice)
    };
    Some(OperationCompletion {
        affected,
        next,
        clear_completed_cut,
        undo_failure,
        containment_notice,
    })
}

pub(super) fn prepare_retry(
    state: &SharedSessions,
    id: OperationId,
) -> Option<FileOperationRequest> {
    let mut app = state.lock().ok()?;
    let resource = app.operations.task(id)?.resource;
    if !app.operations.retry(id) {
        return None;
    }
    let started = app.operations.start_next(resource).ok().flatten()?;
    mark_operation_running_if_ready(&mut app.operations, started);
    let task = app.operations.task(started)?;
    let request = FileOperationRequest {
        id: started,
        kind: task.kind,
        resource: task.resource,
        items: task.items.clone(),
        undo_items: task.undo_items.clone(),
        undo_source_manifests: vec![None; task.items.len()],
        cancellation: task.cancellation.clone(),
    };
    if request.resource != OperationResource::Cleanup {
        register_operation_directories(&mut app, request.kind, &request.items);
    }
    Some(request)
}

pub(super) fn mark_operation_running_if_ready(operations: &mut OperationManager, id: OperationId) {
    let Some((kind, resource)) = operations.task(id).map(|task| (task.kind, task.resource)) else {
        return;
    };
    if resource == OperationResource::Cleanup
        && let Some(task) = operations.task_mut(id)
    {
        task.set_permanent_delete_stage(PermanentDeleteStage::ReleasingSpace);
    }
    if !uses_local_recycle_batch(kind, resource) {
        let _ = operations.mark_running(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_101_source_retained_is_partial_and_not_an_undoable_completion() {
        let result = OperationResult {
            succeeded: vec![PathBuf::from("committed-target")],
            skipped: Vec::new(),
            failed: Vec::new(),
            affected_directories: Vec::new(),
        };
        for cancelled in [false, true] {
            assert_eq!(
                file_operation_terminal_state(cancelled, &result, true),
                OperationState::PartiallyCompleted,
            );
        }
        assert_eq!(
            file_operation_terminal_state(false, &result, false),
            OperationState::Completed,
        );
    }

    #[test]
    fn file_operations_use_network_resource_when_either_endpoint_is_unc() {
        let local = OperationItem::pending(
            Some(PathBuf::from(r"C:\source.txt")),
            Some(PathBuf::from(r"D:\target.txt")),
        );
        let to_network = OperationItem::pending(
            Some(PathBuf::from(r"C:\source.txt")),
            Some(PathBuf::from(r"\\server\share\target.txt")),
        );
        let from_network = OperationItem::pending(
            Some(PathBuf::from(r"\\server\share\source.txt")),
            Some(PathBuf::from(r"C:\target.txt")),
        );

        assert_eq!(operation_resource(&[local]), OperationResource::Local);
        assert_eq!(
            operation_resource(&[to_network]),
            OperationResource::Network
        );
        assert_eq!(
            operation_resource(&[from_network]),
            OperationResource::Network
        );
    }
    #[test]
    fn file_operation_directories_cover_delete_and_create_parents() {
        let deleted = OperationItem::pending(Some(PathBuf::from(r"C:\source\old.txt")), None);
        let created = OperationItem::pending(None, Some(PathBuf::from(r"C:\target\New folder")));
        assert_eq!(
            operation_directories(FileOperationKind::RecycleDelete, &[deleted]),
            HashSet::from([PathBuf::from(r"C:\source")])
        );
        assert_eq!(
            operation_directories(FileOperationKind::CreateFolder, &[created]),
            HashSet::from([PathBuf::from(r"C:\target")])
        );
    }
    #[test]
    fn network_operations_do_not_wait_for_local_directory_watchers() {
        let item = OperationItem::pending(Some(PathBuf::from(r"\\server\share\old.txt")), None);
        assert!(
            running_operation_directories(FileOperationKind::PermanentDelete, &[item]).is_empty()
        );
    }
}
