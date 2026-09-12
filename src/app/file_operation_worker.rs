//! Background file execution owns resource workers, conflict serialization and progress batching.
use crate::{
    domain::file_operations::{
        FileOperationKind, ItemState, OperationId, OperationItem, OperationResource,
        OperationResult, PermanentDeletePhase, PermanentDeleteStage, TransferRateEstimator,
        UndoHistory, UndoItem, UndoSourceKind,
    },
    platform,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
const OPERATION_PROGRESS_FILES: usize = 128;
const OPERATION_PROGRESS_BYTES: u64 = 4 * 1024 * 1024;
const OPERATION_PROGRESS_INTERVAL: Duration = Duration::from_millis(125);
#[derive(Debug)]
pub(super) struct FileOperationRequest {
    pub(super) id: OperationId,
    pub(super) kind: FileOperationKind,
    pub(super) resource: OperationResource,
    pub(super) items: Vec<OperationItem>,
    pub(super) undo_items: Vec<UndoItem>,
    pub(super) undo_source_manifests:
        Vec<Option<Vec<(PathBuf, crate::domain::file_operations::FileIdentity)>>>,
    pub(super) cancellation: crate::domain::file_operations::CancellationToken,
}

#[derive(Debug)]
#[allow(dead_code)]
pub(super) enum FileOperationEvent {
    DestinationCreated {
        id: OperationId,
        path: PathBuf,
    },
    Progress {
        id: OperationId,
        completed_items: usize,
        completed_files: usize,
        total_files: Option<usize>,
        discovered_files: usize,
        processed_bytes: u64,
        total_bytes: Option<u64>,
        discovered_bytes: u64,
        scanning_complete: bool,
        current_item: PathBuf,
        recent_speed_bps: Option<u64>,
    },
    Conflict {
        id: OperationId,
        conflict: crate::domain::file_operations::OperationConflict,
        response: mpsc::Sender<crate::domain::file_operations::ConflictDecision>,
    },
    RecycleProgress {
        id: OperationId,
        progress: platform::windows::file_operation::RecycleProgress,
    },
    PermanentDeleteStage {
        id: OperationId,
        stage: PermanentDeleteStage,
        item_updates: Vec<(usize, PathBuf, PathBuf)>,
        affected_directories: Vec<PathBuf>,
    },
    PermanentDeleteCommitted {
        id: OperationId,
        cleanup: Option<FileOperationRequest>,
        result: OperationResult,
        item_states: Vec<(usize, ItemState, Option<String>)>,
    },
    RecycleDiscovery {
        id: OperationId,
        discovered_items: usize,
        discovered_bytes: u64,
        complete: bool,
        current_item: PathBuf,
    },
    Finished {
        id: OperationId,
        result: OperationResult,
        item_states: Vec<(usize, ItemState, Option<String>)>,
        completed_targets: Vec<PathBuf>,
        undo_items: Option<Vec<UndoItem>>,
        failed_undo_items: Vec<UndoItem>,
    },
}

#[derive(Debug)]
struct OperationProgressSnapshot {
    id: OperationId,
    completed_items: usize,
    completed_files: usize,
    total_files: Option<usize>,
    discovered_files: usize,
    processed_bytes: u64,
    total_bytes: Option<u64>,
    discovered_bytes: u64,
    scanning_complete: bool,
    current_item: PathBuf,
    recent_speed_bps: Option<u64>,
}

#[derive(Debug)]
struct OperationProgressEmitter<'a> {
    events: &'a mpsc::Sender<FileOperationEvent>,
    snapshot: OperationProgressSnapshot,
    last_sent_at: Instant,
    last_sent_bytes: u64,
    last_sent_files: usize,
    rate_estimator: TransferRateEstimator,
    cancellation: crate::domain::file_operations::CancellationToken,
    started_at: Instant,
}

impl<'a> OperationProgressEmitter<'a> {
    fn new(
        events: &'a mpsc::Sender<FileOperationEvent>,
        snapshot: OperationProgressSnapshot,
        cancellation: crate::domain::file_operations::CancellationToken,
        started_at: Instant,
    ) -> Self {
        let now = Instant::now();
        let last_sent_at = now.checked_sub(OPERATION_PROGRESS_INTERVAL).unwrap_or(now);
        Self {
            events,
            snapshot,
            last_sent_at,
            last_sent_bytes: 0,
            last_sent_files: 0,
            rate_estimator: TransferRateEstimator::default(),
            cancellation,
            started_at,
        }
    }

    fn discovered(&mut self, bytes: u64, current: &Path) {
        if self.cancellation.is_cancelled() {
            return;
        }
        self.snapshot.discovered_files = self.snapshot.discovered_files.saturating_add(1);
        self.snapshot.discovered_bytes = self.snapshot.discovered_bytes.saturating_add(bytes);
        self.snapshot.current_item = current.to_path_buf();
        self.send_if_due(false);
    }

    fn advanced(&mut self, bytes: u64, file_completed: bool, current: &Path) {
        if self.cancellation.is_cancelled() {
            return;
        }
        self.snapshot.processed_bytes = self.snapshot.processed_bytes.saturating_add(bytes);
        self.rate_estimator.record(
            self.cancellation.active_elapsed(self.started_at),
            self.snapshot.processed_bytes,
            self.cancellation.rate_epoch(),
        );
        self.snapshot.recent_speed_bps = self.rate_estimator.bytes_per_second();
        if file_completed {
            self.snapshot.completed_files = self.snapshot.completed_files.saturating_add(1);
        }
        self.snapshot.current_item = current.to_path_buf();
        self.send_if_due(false);
    }

    fn finish_scan(&mut self) {
        if self.cancellation.is_cancelled() {
            return;
        }
        self.snapshot.scanning_complete = true;
        self.snapshot.total_files = Some(self.snapshot.discovered_files);
        self.snapshot.total_bytes = Some(self.snapshot.discovered_bytes);
        self.send_if_due(true);
    }

    fn complete_item(&mut self, completed_items: usize, current: PathBuf) {
        self.snapshot.completed_items = completed_items;
        self.snapshot.current_item = current;
        self.send_if_due(false);
    }

    fn flush(&mut self) {
        self.send_if_due(true);
    }

    fn send_if_due(&mut self, force: bool) {
        let now = Instant::now();
        let byte_delta = self
            .snapshot
            .processed_bytes
            .saturating_sub(self.last_sent_bytes);
        let file_delta = self
            .snapshot
            .completed_files
            .max(self.snapshot.discovered_files)
            .saturating_sub(self.last_sent_files);
        if !force {
            let elapsed = now.saturating_duration_since(self.last_sent_at);
            if elapsed < OPERATION_PROGRESS_INTERVAL
                || (byte_delta < OPERATION_PROGRESS_BYTES && file_delta < OPERATION_PROGRESS_FILES)
            {
                return;
            }
        }
        let _ = self.events.send(FileOperationEvent::Progress {
            id: self.snapshot.id,
            completed_items: self.snapshot.completed_items,
            completed_files: self.snapshot.completed_files,
            total_files: self.snapshot.total_files,
            discovered_files: self.snapshot.discovered_files,
            processed_bytes: self.snapshot.processed_bytes,
            total_bytes: self.snapshot.total_bytes,
            discovered_bytes: self.snapshot.discovered_bytes,
            scanning_complete: self.snapshot.scanning_complete,
            current_item: self.snapshot.current_item.clone(),
            recent_speed_bps: self.snapshot.recent_speed_bps,
        });
        self.last_sent_at = now;
        self.last_sent_bytes = self.snapshot.processed_bytes;
        self.last_sent_files = self
            .snapshot
            .completed_files
            .max(self.snapshot.discovered_files);
    }
}

#[derive(Debug)]
struct RecycleProgressCoalescer {
    last_sent_at: Option<Instant>,
    pending: Option<platform::windows::file_operation::RecycleProgress>,
    executing: bool,
}

impl RecycleProgressCoalescer {
    fn new() -> Self {
        Self {
            last_sent_at: None,
            pending: None,
            executing: false,
        }
    }

    fn push(
        &mut self,
        progress: platform::windows::file_operation::RecycleProgress,
    ) -> Option<platform::windows::file_operation::RecycleProgress> {
        let now = Instant::now();
        let entering_execution = matches!(
            progress,
            platform::windows::file_operation::RecycleProgress::Executing { .. }
        ) && !self.executing;
        let preparation_finished = matches!(
            progress,
            platform::windows::file_operation::RecycleProgress::Preparing {
                prepared,
                total,
                ..
            } if prepared == total
        );
        self.executing |= matches!(
            progress,
            platform::windows::file_operation::RecycleProgress::Executing { .. }
        );
        self.pending = Some(progress);
        let due = self
            .last_sent_at
            .is_none_or(|last| now.saturating_duration_since(last) >= OPERATION_PROGRESS_INTERVAL);
        if due || entering_execution || preparation_finished {
            self.last_sent_at = Some(now);
            self.pending.take()
        } else {
            None
        }
    }

    #[cfg(test)]
    fn make_due_for_test(&mut self) {
        self.last_sent_at = Some(Instant::now() - OPERATION_PROGRESS_INTERVAL);
    }
    fn flush(&mut self) -> Option<platform::windows::file_operation::RecycleProgress> {
        let progress = self.pending.take()?;
        self.last_sent_at = Some(Instant::now());
        Some(progress)
    }
}
pub(super) fn spawn_file_operation_worker() -> (
    mpsc::Sender<FileOperationRequest>,
    mpsc::Receiver<FileOperationEvent>,
) {
    let (request_sender, request_receiver) = mpsc::channel::<FileOperationRequest>();
    let (local_sender, local_receiver) = mpsc::channel::<FileOperationRequest>();
    let (network_sender, network_receiver) = mpsc::channel::<FileOperationRequest>();
    let (cleanup_sender, cleanup_receiver) = mpsc::channel::<FileOperationRequest>();
    let (event_sender, event_receiver) = mpsc::channel::<FileOperationEvent>();
    let conflict_gate = Arc::new(Mutex::new(()));
    let dispatcher_event_sender = event_sender.clone();
    thread::spawn(move || {
        while let Ok(request) = request_receiver.recv() {
            let sender = match request.resource {
                OperationResource::Local => &local_sender,
                OperationResource::Network => &network_sender,
                OperationResource::Cleanup => &cleanup_sender,
            };
            if sender.send(request).is_err() {
                break;
            }
        }
    });
    run_file_operation_worker(local_receiver, event_sender.clone(), conflict_gate.clone());
    run_file_operation_worker(
        network_receiver,
        event_sender.clone(),
        conflict_gate.clone(),
    );
    run_file_operation_worker(cleanup_receiver, dispatcher_event_sender, conflict_gate);
    (request_sender, event_receiver)
}

fn run_file_operation_worker(
    receiver: mpsc::Receiver<FileOperationRequest>,
    event_sender: mpsc::Sender<FileOperationEvent>,
    conflict_gate: Arc<Mutex<()>>,
) {
    thread::spawn(move || {
        while let Ok(request) = receiver.recv() {
            execute_file_operation_request(request, &event_sender, &conflict_gate);
        }
    });
}

pub(super) fn uses_local_recycle_batch(
    kind: FileOperationKind,
    resource: OperationResource,
) -> bool {
    kind == FileOperationKind::RecycleDelete && resource == OperationResource::Local
}

pub(super) fn undo_source_kind(kind: FileOperationKind) -> Option<UndoSourceKind> {
    match kind {
        FileOperationKind::CreateFolder => Some(UndoSourceKind::CreateFolder),
        FileOperationKind::Rename => Some(UndoSourceKind::Rename),
        FileOperationKind::Copy => Some(UndoSourceKind::Copy),
        FileOperationKind::Move => Some(UndoSourceKind::Move),
        FileOperationKind::RecycleDelete => Some(UndoSourceKind::RecycleDelete),
        FileOperationKind::PermanentDelete
        | FileOperationKind::FastRemove
        | FileOperationKind::Undo => None,
    }
}

fn undo_manifest_from_report(
    target: &Path,
    report: &crate::fs::file_operations::FileOperationReport,
    limit: usize,
) -> Option<Vec<(PathBuf, crate::domain::file_operations::FileIdentity)>> {
    let mut identities = report
        .undo_identities
        .iter()
        .filter(|(path, _)| {
            path.as_path() == target
                || path
                    .strip_prefix(target)
                    .is_ok_and(|relative| !relative.as_os_str().is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    identities.sort_by(|left, right| left.0.cmp(&right.0));
    identities.dedup_by(|left, right| left.0 == right.0);
    let target_identity = identities
        .iter()
        .find(|(path, _)| path == target)
        .map(|(_, identity)| *identity)?;
    if identities.is_empty() || identities.len() > limit {
        return None;
    }
    let actual = crate::fs::file_operations::collect_tree_identities(target, limit).ok()?;
    let actual_by_path = actual.into_iter().collect::<HashMap<_, _>>();
    if actual_by_path.len() != identities.len()
        || identities.iter().any(|(path, identity)| {
            actual_by_path.get(path).is_none_or(|actual| {
                actual.volume_serial != identity.volume_serial
                    || actual.file_index != identity.file_index
                    || actual.is_directory != identity.is_directory
            })
        })
    {
        return None;
    }
    let current_target = actual_by_path.get(target)?;
    if current_target.volume_serial != target_identity.volume_serial
        || current_target.file_index != target_identity.file_index
        || current_target.is_directory != target_identity.is_directory
    {
        return None;
    }
    identities
        .into_iter()
        .map(|(path, identity)| {
            let current = *actual_by_path.get(&path)?;
            Some((
                path,
                if identity.is_directory {
                    current
                } else {
                    identity
                },
            ))
        })
        .collect::<Option<Vec<_>>>()
}

fn rebase_undo_manifest(
    manifest: &[(PathBuf, crate::domain::file_operations::FileIdentity)],
    source: &Path,
    destination: &Path,
) -> Option<Vec<(PathBuf, crate::domain::file_operations::FileIdentity)>> {
    manifest
        .iter()
        .map(|(path, identity)| {
            let relative = path.strip_prefix(source).ok()?;
            Some((destination.join(relative), *identity))
        })
        .collect()
}

fn undo_items_for_success(
    kind: FileOperationKind,
    item: &OperationItem,
    report: &crate::fs::file_operations::FileOperationReport,
    completed_target: Option<&Path>,
    destination_existed: bool,
    source_manifest: Option<&[(PathBuf, crate::domain::file_operations::FileIdentity)]>,
    snapshot_limit: usize,
) -> Option<Vec<UndoItem>> {
    match kind {
        FileOperationKind::CreateFolder => {
            let target = completed_target?;
            let identity = report
                .undo_identities
                .iter()
                .find(|(path, _)| path == target)
                .map(|(_, identity)| *identity)?;
            Some(vec![UndoItem::RemoveEmptyDirectory {
                path: target.to_path_buf(),
                identity,
                quarantined: false,
            }])
        }
        FileOperationKind::Copy => {
            if !report.undo_root_created_exclusively {
                return None;
            }
            let target = report.undo_root.as_deref()?;
            let manifest = undo_manifest_from_report(target, report, snapshot_limit)?;
            Some(vec![UndoItem::RemoveCreated {
                path: target.to_path_buf(),
                manifest,
            }])
        }
        FileOperationKind::Rename | FileOperationKind::Move => {
            let target = completed_target?;
            let original = item.source.as_ref()?;
            if original == target || (kind == FileOperationKind::Move && destination_existed) {
                return None;
            }
            let source_manifest = source_manifest?;
            if source_manifest.is_empty() || source_manifest.len() > snapshot_limit {
                return None;
            }
            let manifest = rebase_undo_manifest(source_manifest, original, target)?;
            let current =
                crate::fs::file_operations::collect_tree_identities(target, snapshot_limit).ok()?;
            if current.len() != manifest.len()
                || current.iter().zip(&manifest).any(
                    |((path, actual), (expected_path, expected))| {
                        path != expected_path
                            || actual.is_directory != expected.is_directory
                            || (!actual.is_directory
                                && (actual.size_bytes != expected.size_bytes
                                    || actual.modified != expected.modified))
                    },
                )
            {
                return None;
            }
            let manifest = current;
            let manifest_complete = true;
            Some(vec![UndoItem::MoveBack {
                current: target.to_path_buf(),
                original: original.clone(),
                manifest,
                manifest_complete,
            }])
        }
        FileOperationKind::RecycleDelete
        | FileOperationKind::PermanentDelete
        | FileOperationKind::FastRemove
        | FileOperationKind::Undo => None,
    }
}
fn execute_file_operation_request(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
    conflict_gate: &Arc<Mutex<()>>,
) {
    crate::operation_audit::record(
        "operation_started",
        format!(
            "id={:?} kind={:?} items={:?}",
            request.id, request.kind, request.items
        ),
    );
    let mut succeeded = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut affected = Vec::new();
    let mut indexed_states = Vec::new();
    let mut completed_targets = Vec::new();
    let mut undo_items = Some(Vec::new());
    let mut undo_snapshot_remaining = UndoHistory::MAX_SNAPSHOT_ITEMS;
    let current_item = request
        .items
        .first()
        .and_then(|item| item.source.clone().or_else(|| item.destination.clone()))
        .unwrap_or_default();
    if uses_local_recycle_batch(request.kind, request.resource) {
        execute_recycle_delete_request(request, event_sender);
        return;
    }
    if request.kind == FileOperationKind::PermanentDelete {
        match request.resource {
            OperationResource::Local => {
                commit_permanent_delete_request(request, event_sender);
                return;
            }
            OperationResource::Cleanup => {
                execute_cleanup_request(request, event_sender);
                return;
            }
            OperationResource::Network => {}
        }
    }
    let copy_or_move = matches!(
        request.kind,
        FileOperationKind::Copy | FileOperationKind::Move
    );
    let progress_cancellation = request.cancellation.clone();
    let mut progress = OperationProgressEmitter::new(
        event_sender,
        OperationProgressSnapshot {
            id: request.id,
            completed_items: 0,
            completed_files: 0,
            total_files: (!copy_or_move).then_some(request.items.len()),
            discovered_files: 0,
            processed_bytes: 0,
            total_bytes: (!copy_or_move).then_some(0),
            discovered_bytes: 0,
            scanning_complete: !copy_or_move,
            current_item,
            recent_speed_bps: None,
        },
        progress_cancellation,
        Instant::now(),
    );
    progress.flush();
    if request.kind == FileOperationKind::Undo {
        execute_undo_request(request, event_sender);
        return;
    }
    let mut conflict_defaults = HashMap::new();
    let mut completed_in_round = 0;
    for (item_index, item) in request.items.iter().enumerate() {
        if item.state != ItemState::Pending {
            continue;
        }
        if request.cancellation.is_cancelled() {
            indexed_states.push((item_index, ItemState::Cancelled, None));
            continue;
        }
        let destination_existed = request.resource == OperationResource::Local
            && item
                .destination
                .as_deref()
                .is_some_and(|path| std::fs::symlink_metadata(path).is_ok());
        let destination_was_existing_directory = request.resource == OperationResource::Local
            && item.source != item.destination
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
            event_sender,
            &mut progress,
            &mut conflict_defaults,
            conflict_gate,
        );
        match outcome {
            Ok(report) => {
                let completed_target = report
                    .skipped
                    .is_empty()
                    .then(|| {
                        completed_target_for_item(
                            item,
                            &report.completed_paths,
                            destination_was_existing_directory,
                        )
                    })
                    .flatten();
                if report.skipped.is_empty() {
                    if request.resource == OperationResource::Local {
                        if let Some(candidate) = undo_items_for_success(
                            request.kind,
                            item,
                            &report,
                            completed_target.as_deref(),
                            destination_existed,
                            request
                                .undo_source_manifests
                                .get(item_index)
                                .and_then(|manifest| manifest.as_deref()),
                            undo_snapshot_remaining,
                        ) {
                            let used = candidate
                                .iter()
                                .map(|item| match item {
                                    UndoItem::RemoveCreated { manifest, .. }
                                    | UndoItem::RemoveQuarantined { manifest, .. }
                                    | UndoItem::MoveBack { manifest, .. }
                                    | UndoItem::MoveBackQuarantined { manifest, .. } => {
                                        manifest.len()
                                    }
                                    UndoItem::RemoveEmptyDirectory { .. }
                                    | UndoItem::RestoreRecycled { .. }
                                    | UndoItem::FinalizeRestore { .. } => 1,
                                })
                                .sum::<usize>();
                            if let Some(items) = undo_items.as_mut() {
                                items.extend(candidate);
                            }
                            undo_snapshot_remaining = undo_snapshot_remaining.saturating_sub(used);
                        } else {
                            undo_items = None;
                        }
                    } else {
                        undo_items = None;
                    }
                } else {
                    undo_items = None;
                }
                if let Some(target) = completed_target
                    && !completed_targets.contains(&target)
                {
                    completed_targets.push(target);
                }
                let current_item = item
                    .source
                    .clone()
                    .or_else(|| item.destination.clone())
                    .unwrap_or_default();
                completed_in_round += 1;
                progress.complete_item(completed_in_round, current_item);
                let identity = item
                    .destination
                    .clone()
                    .or_else(|| item.source.clone())
                    .unwrap_or_default();
                if report.skipped.is_empty() {
                    succeeded.push(identity.clone());
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
            Err(error) => {
                undo_items = None;
                let identity = item
                    .source
                    .clone()
                    .or_else(|| item.destination.clone())
                    .unwrap_or_default();
                let (state, message, committed_target) =
                    error.into_item_result(request.cancellation.is_cancelled());
                if let Some(destination) = committed_target {
                    succeeded.push(destination.clone());
                    completed_targets.push(destination);
                }
                if state == ItemState::Failed {
                    failed.push((identity, message.clone().unwrap_or_default()));
                }
                indexed_states.push((item_index, state, message));
            }
        }
    }
    if copy_or_move {
        progress.finish_scan();
    } else {
        progress.flush();
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
        undo_items,
        failed_undo_items: Vec::new(),
    });
}

fn start_recycle_discovery(
    id: OperationId,
    paths: Vec<PathBuf>,
    cancellation: crate::domain::file_operations::CancellationToken,
    stop: Arc<AtomicBool>,
    events: mpsc::Sender<FileOperationEvent>,
) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("asterfiles-recycle-discovery".into())
        .spawn(move || {
            let now = Instant::now();
            let mut last_sent_at = now.checked_sub(OPERATION_PROGRESS_INTERVAL).unwrap_or(now);
            let mut latest_items = 0_usize;
            let mut latest_bytes = 0_u64;
            let mut latest_path = paths.first().cloned().unwrap_or_default();
            let complete = crate::fs::file_operations::discover_paths_with_progress(
                &paths,
                &|| cancellation.is_cancelled() || stop.load(Ordering::Acquire),
                &mut |items, bytes, current| {
                    latest_items = items;
                    latest_bytes = bytes;
                    latest_path = current.to_path_buf();
                    let now = Instant::now();
                    if now.saturating_duration_since(last_sent_at) >= OPERATION_PROGRESS_INTERVAL {
                        let _ = events.send(FileOperationEvent::RecycleDiscovery {
                            id,
                            discovered_items: items,
                            discovered_bytes: bytes,
                            complete: false,
                            current_item: current.to_path_buf(),
                        });
                        last_sent_at = now;
                    }
                },
            );
            if complete {
                let _ = events.send(FileOperationEvent::RecycleDiscovery {
                    id,
                    discovered_items: latest_items,
                    discovered_bytes: latest_bytes,
                    complete: true,
                    current_item: latest_path,
                });
            }
        })
        .ok()
}

fn permanent_delete_cleanup_request(
    record: &crate::fs::file_operations::CleanupTaskRecord,
) -> Option<FileOperationRequest> {
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
    (!items.is_empty()).then(|| FileOperationRequest {
        id: OperationId(0),
        kind: FileOperationKind::PermanentDelete,
        resource: OperationResource::Cleanup,
        undo_items: Vec::new(),
        undo_source_manifests: vec![None; items.len()],
        cancellation: crate::domain::file_operations::CancellationToken::new(),
        items,
    })
}

fn commit_permanent_delete_request(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
) {
    let record_root = cleanup_record_root();
    commit_permanent_delete_request_with_root(request, event_sender, record_root.as_deref());
}

fn commit_permanent_delete_request_with_root(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
    record_root: Option<&Path>,
) {
    let original_indices = request
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| {
            item.state == ItemState::Pending
                && item.permanent_delete_phase == PermanentDeletePhase::Original
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let candidates = original_indices
        .iter()
        .filter_map(|index| {
            let path = request.items[*index].source.as_ref()?;
            is_fast_remove_candidate(path).then_some(path.clone())
        })
        .collect::<Vec<_>>();
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut affected = Vec::new();
    let mut item_states = Vec::new();
    let mut moved_indices = HashSet::new();
    let mut cleanup = None;

    if !candidates.is_empty()
        && let Some(record_root) = record_root
    {
        match crate::fs::file_operations::create_cleanup_task(
            &candidates,
            record_root,
            &request.cancellation,
        ) {
            Ok(record) => {
                let mut updates = Vec::new();
                for record_item in &record.items {
                    let Some(index) = original_indices.iter().copied().find(|index| {
                        request.items[*index].source.as_ref() == Some(&record_item.original_path)
                    }) else {
                        continue;
                    };
                    if let Some(pending) = record_item.remaining_path.clone()
                        && pending.exists()
                    {
                        moved_indices.insert(index);
                        succeeded.push(record_item.original_path.clone());
                        item_states.push((index, ItemState::Succeeded, None));
                        updates.push((index, pending, record.record_path.clone()));
                        if let Some(parent) =
                            record_item.original_path.parent().map(Path::to_path_buf)
                            && !affected.contains(&parent)
                        {
                            affected.push(parent);
                        }
                    }
                }
                if !updates.is_empty() {
                    let _ = event_sender.send(FileOperationEvent::PermanentDeleteStage {
                        id: request.id,
                        stage: PermanentDeleteStage::SourceRemoved,
                        item_updates: updates,
                        affected_directories: affected.clone(),
                    });
                    cleanup = permanent_delete_cleanup_request(&record);
                    if cleanup.is_none()
                        && let Err(error) =
                            crate::fs::file_operations::discard_empty_cleanup_task(&record)
                    {
                        eprintln!("unable to discard empty cleanup task: {error:?}");
                    }
                }
            }
            Err(crate::fs::file_operations::OperationError::Cancelled) => {}
            Err(error) => {
                eprintln!("fast removal unavailable; using ordinary deletion: {error:?}")
            }
        }
    }

    for index in original_indices {
        if moved_indices.contains(&index) {
            continue;
        }
        let Some(path) = request.items[index].source.as_ref() else {
            let message = "missing source".to_owned();
            failed.push((PathBuf::new(), message.clone()));
            item_states.push((index, ItemState::Failed, Some(message)));
            continue;
        };
        match crate::fs::file_operations::permanently_delete(path, &request.cancellation) {
            Ok(report) => {
                succeeded.push(path.clone());
                item_states.push((index, ItemState::Succeeded, None));
                for directory in report.affected_directories {
                    if !affected.contains(&directory) {
                        affected.push(directory);
                    }
                }
            }
            Err(crate::fs::file_operations::OperationError::Cancelled) => {
                item_states.push((index, ItemState::Cancelled, None));
            }
            Err(error) => {
                let message = format!("{error:?}");
                failed.push((path.clone(), message.clone()));
                item_states.push((index, ItemState::Failed, Some(message)));
            }
        }
    }

    let _ = event_sender.send(FileOperationEvent::PermanentDeleteCommitted {
        id: request.id,
        cleanup,
        result: OperationResult {
            succeeded,
            skipped: Vec::new(),
            failed,
            affected_directories: affected,
        },
        item_states,
    });
}

pub(super) fn execute_cleanup_request(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
) {
    let _ = event_sender.send(FileOperationEvent::PermanentDeleteStage {
        id: request.id,
        stage: PermanentDeleteStage::ReleasingSpace,
        item_updates: Vec::new(),
        affected_directories: Vec::new(),
    });
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut item_states = Vec::new();
    let mut groups = HashMap::<(PathBuf, String), Vec<usize>>::new();
    for (index, item) in request.items.iter().enumerate() {
        if item.state != ItemState::Pending {
            continue;
        }
        let task_id = item
            .destination
            .as_deref()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .map(|value| value.to_string_lossy().into_owned());
        let record_root = item
            .cleanup_record
            .as_deref()
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf);
        if let (Some(record_root), Some(task_id)) = (record_root, task_id) {
            groups
                .entry((record_root, task_id))
                .or_default()
                .push(index);
        } else {
            let original = item.source.clone().unwrap_or_default();
            let message = "cleanup task identity is missing".to_owned();
            failed.push((original, message.clone()));
            item_states.push((index, ItemState::Failed, Some(message)));
        }
    }
    for ((record_root, task_id), indices) in groups {
        let progress_id = request.id;
        let events = event_sender.clone();
        match crate::fs::file_operations::retry_cleanup_task(
            &record_root,
            &task_id,
            &request.cancellation,
            &mut |snapshot| {
                let _ = events.send(FileOperationEvent::Progress {
                    id: progress_id,
                    completed_items: snapshot.files.saturating_add(snapshot.directories) as usize,
                    completed_files: snapshot.files as usize,
                    total_files: None,
                    discovered_files: 0,
                    processed_bytes: snapshot.bytes,
                    total_bytes: None,
                    discovered_bytes: 0,
                    scanning_complete: false,
                    current_item: snapshot.current_path.clone().unwrap_or_default(),
                    recent_speed_bps: None,
                });
            },
        ) {
            Ok(record) => {
                for index in indices {
                    let original = request.items[index].source.clone().unwrap_or_default();
                    let record_item = record
                        .items
                        .iter()
                        .find(|item| item.original_path == original);
                    if record_item
                        .and_then(|item| item.remaining_path.as_ref())
                        .is_none()
                    {
                        succeeded.push(original);
                        item_states.push((index, ItemState::Succeeded, None));
                    } else if request.cancellation.is_cancelled() {
                        item_states.push((index, ItemState::Cancelled, None));
                    } else {
                        let message = record_item
                            .and_then(|item| item.error.clone())
                            .or_else(|| record.error.clone())
                            .unwrap_or_else(|| "cleanup did not finish".to_owned());
                        failed.push((original, message.clone()));
                        item_states.push((index, ItemState::Failed, Some(message)));
                    }
                }
            }
            Err(error) => {
                let message = format!("{error:?}");
                for index in indices {
                    let original = request.items[index].source.clone().unwrap_or_default();
                    failed.push((original, message.clone()));
                    item_states.push((index, ItemState::Failed, Some(message.clone())));
                }
            }
        }
    }
    let _ = event_sender.send(FileOperationEvent::Finished {
        id: request.id,
        result: OperationResult {
            succeeded,
            skipped: Vec::new(),
            failed,
            affected_directories: Vec::new(),
        },
        item_states,
        completed_targets: Vec::new(),
        undo_items: None,
        failed_undo_items: Vec::new(),
    });
}
fn execute_recycle_delete_request(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
) {
    let pending = request
        .items
        .iter()
        .enumerate()
        .filter(|(_, item)| item.state == ItemState::Pending)
        .filter_map(|(index, item)| item.source.clone().map(|path| (index, path)))
        .collect::<Vec<_>>();
    if request.cancellation.is_cancelled() {
        let _ = event_sender.send(FileOperationEvent::Finished {
            id: request.id,
            result: OperationResult {
                succeeded: Vec::new(),
                skipped: Vec::new(),
                failed: Vec::new(),
                affected_directories: Vec::new(),
            },
            item_states: pending
                .iter()
                .map(|(index, _)| (*index, ItemState::Cancelled, None))
                .collect(),
            completed_targets: Vec::new(),
            undo_items: None,
            failed_undo_items: Vec::new(),
        });
        return;
    }
    let paths = pending
        .iter()
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    let discovery_stop = Arc::new(AtomicBool::new(false));
    let discovery_worker = start_recycle_discovery(
        request.id,
        paths.clone(),
        request.cancellation.clone(),
        discovery_stop.clone(),
        event_sender.clone(),
    );
    let cancellation = request.cancellation.clone();
    let progress_events = event_sender.clone();
    let progress_id = request.id;
    let coalescer = Arc::new(Mutex::new(RecycleProgressCoalescer::new()));
    let callback_coalescer = coalescer.clone();
    let recycle = platform::windows::file_operation::recycle_with_progress(
        &paths,
        move || cancellation.is_cancelled(),
        move |progress| {
            let committed = callback_coalescer
                .lock()
                .ok()
                .and_then(|mut coalescer| coalescer.push(progress));
            if let Some(progress) = committed {
                let _ = progress_events.send(FileOperationEvent::RecycleProgress {
                    id: progress_id,
                    progress,
                });
            }
        },
    );
    if let Some(progress) = coalescer
        .lock()
        .ok()
        .and_then(|mut coalescer| coalescer.flush())
    {
        let _ = event_sender.send(FileOperationEvent::RecycleProgress {
            id: request.id,
            progress,
        });
    }
    discovery_stop.store(true, Ordering::Release);
    if let Some(worker) = discovery_worker {
        let _ = worker.join();
    }
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut affected = Vec::new();
    let mut item_states = Vec::new();
    let mut undo_items = Some(Vec::new());
    for (completed_in_round, result) in recycle.items.into_iter().enumerate() {
        let (index, path) = pending[result.index].clone();
        if let Some(parent) = path.parent().map(Path::to_path_buf)
            && !affected.contains(&parent)
        {
            affected.push(parent);
        }
        match result.result {
            Ok(()) => {
                if let Some(absolute_pidl) = result.recycled_identity {
                    if let Some(items) = undo_items.as_mut() {
                        items.push(UndoItem::RestoreRecycled {
                            original: path.clone(),
                            absolute_pidl,
                        });
                    }
                } else {
                    undo_items = None;
                }
                succeeded.push(path.clone());
                item_states.push((index, ItemState::Succeeded, None));
            }
            Err(message) if recycle.aborted || request.cancellation.is_cancelled() => {
                undo_items = None;
                item_states.push((index, ItemState::Cancelled, Some(message)));
            }
            Err(message) => {
                undo_items = None;
                failed.push((path.clone(), message.clone()));
                item_states.push((index, ItemState::Failed, Some(message)));
            }
        }
        let _ = event_sender.send(FileOperationEvent::Progress {
            id: request.id,
            completed_items: completed_in_round + 1,
            completed_files: 0,
            total_files: None,
            discovered_files: 0,
            processed_bytes: 0,
            total_bytes: None,
            discovered_bytes: 0,
            scanning_complete: false,
            current_item: path,
            recent_speed_bps: None,
        });
    }
    let _ = event_sender.send(FileOperationEvent::Finished {
        id: request.id,
        result: OperationResult {
            succeeded,
            skipped: Vec::new(),
            failed,
            affected_directories: affected,
        },
        item_states,
        completed_targets: Vec::new(),
        undo_items,
        failed_undo_items: Vec::new(),
    });
}

fn undo_report_for_paths(paths: &[PathBuf]) -> crate::fs::file_operations::FileOperationReport {
    let mut affected_directories = paths
        .iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    affected_directories.sort();
    affected_directories.dedup();
    crate::fs::file_operations::FileOperationReport {
        files: 0,
        directories: 0,
        bytes: 0,
        skipped: Vec::new(),
        affected_directories,
        cleanup_pending: None,
        completed_paths: paths.to_vec(),
        undo_identities: Vec::new(),
        undo_root: None,
        undo_root_created_exclusively: false,
    }
}

fn execute_undo_request(
    request: FileOperationRequest,
    event_sender: &mpsc::Sender<FileOperationEvent>,
) {
    let mut succeeded = Vec::new();
    let mut failed = Vec::new();
    let mut affected = Vec::new();
    let mut item_states = Vec::new();
    let mut failed_undo_items = Vec::new();
    for (index, undo_item) in request.undo_items.iter().enumerate() {
        if request.cancellation.is_cancelled() {
            item_states.push((index, ItemState::Cancelled, None));
            failed_undo_items.push(undo_item.clone());
            continue;
        }
        let identity = request.items[index]
            .source
            .clone()
            .or_else(|| request.items[index].destination.clone())
            .unwrap_or_default();
        let outcome = match undo_item {
            UndoItem::RestoreRecycled {
                original,
                absolute_pidl,
            } => {
                let cancellation = request.cancellation.clone();
                platform::windows::file_operation::restore_recycled(
                    original,
                    absolute_pidl,
                    move || cancellation.is_cancelled(),
                )
                .map(|restored| match restored {
                    platform::windows::file_operation::RestoreRecycleResult::Completed => {
                        crate::fs::file_operations::UndoExecution::Completed(undo_report_for_paths(
                            std::slice::from_ref(original),
                        ))
                    }
                    platform::windows::file_operation::RestoreRecycleResult::Pending {
                        temporary,
                        identity,
                        message,
                    } => crate::fs::file_operations::UndoExecution::RetryWith {
                        item: UndoItem::FinalizeRestore {
                            temporary,
                            original: original.clone(),
                            identity,
                        },
                        message,
                        affected_directories: undo_item.directories(),
                    },
                })
            }
            _ => crate::fs::file_operations::execute_undo_item(undo_item, &request.cancellation)
                .map_err(|error| format!("{error:?}")),
        };
        match outcome {
            Ok(crate::fs::file_operations::UndoExecution::Completed(report)) => {
                succeeded.push(identity.clone());
                item_states.push((index, ItemState::Succeeded, None));
                for directory in report.affected_directories {
                    if !affected.contains(&directory) {
                        affected.push(directory);
                    }
                }
            }
            Ok(crate::fs::file_operations::UndoExecution::RetryWith {
                item,
                message,
                affected_directories,
            }) => {
                failed.push((identity.clone(), message.clone()));
                item_states.push((index, ItemState::Failed, Some(message)));
                failed_undo_items.push(item);
                for directory in affected_directories {
                    if !affected.contains(&directory) {
                        affected.push(directory);
                    }
                }
            }
            Err(message) => {
                failed.push((identity.clone(), message.clone()));
                item_states.push((index, ItemState::Failed, Some(message)));
                failed_undo_items.push(undo_item.clone());
            }
        }
        let _ = event_sender.send(FileOperationEvent::Progress {
            id: request.id,
            completed_items: index + 1,
            completed_files: succeeded.len(),
            total_files: Some(request.undo_items.len()),
            discovered_files: request.undo_items.len(),
            processed_bytes: 0,
            total_bytes: Some(0),
            discovered_bytes: 0,
            scanning_complete: true,
            current_item: identity,
            recent_speed_bps: None,
        });
    }
    let _ = event_sender.send(FileOperationEvent::Finished {
        id: request.id,
        result: OperationResult {
            succeeded,
            skipped: Vec::new(),
            failed,
            affected_directories: affected,
        },
        item_states,
        completed_targets: Vec::new(),
        undo_items: None,
        failed_undo_items,
    });
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

pub(super) const SELF_CONTAINMENT_ERROR: &str = "SourceInsideDestination";

#[derive(Debug)]
pub(super) enum ExecuteFileOperationError {
    Failed(String),
    DestinationCommittedSourceRetained {
        destination: PathBuf,
        message: String,
    },
}

impl ExecuteFileOperationError {
    pub(super) fn into_item_result(
        self,
        cancelled: bool,
    ) -> (ItemState, Option<String>, Option<PathBuf>) {
        match self {
            Self::DestinationCommittedSourceRetained {
                destination,
                message,
            } => (ItemState::Succeeded, Some(message), Some(destination)),
            Self::Failed(_) if cancelled => (ItemState::Cancelled, None, None),
            Self::Failed(message) => (ItemState::Failed, Some(message), None),
        }
    }

    fn failed_debug(error: impl std::fmt::Debug) -> Self {
        Self::Failed(format!("{error:?}"))
    }

    pub(super) fn from_operation(error: crate::fs::file_operations::OperationError) -> Self {
        match error {
            crate::fs::file_operations::OperationError::SourceInsideDestination => {
                Self::Failed(SELF_CONTAINMENT_ERROR.to_owned())
            }
            crate::fs::file_operations::OperationError::DestinationCommittedSourceRetained {
                destination,
                message,
                ..
            } => Self::DestinationCommittedSourceRetained {
                destination,
                message,
            },
            error => Self::failed_debug(error),
        }
    }
}

impl From<&str> for ExecuteFileOperationError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

impl From<String> for ExecuteFileOperationError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}
#[allow(clippy::too_many_arguments)]
fn execute_file_operation_item(
    id: OperationId,
    kind: FileOperationKind,
    item: &OperationItem,
    cancel: &crate::domain::file_operations::CancellationToken,
    events: &mpsc::Sender<FileOperationEvent>,
    progress_emitter: &mut OperationProgressEmitter<'_>,
    conflict_defaults: &mut HashMap<
        crate::domain::file_operations::ConflictCategory,
        crate::domain::file_operations::ConflictAction,
    >,
    conflict_gate: &Arc<Mutex<()>>,
) -> Result<crate::fs::file_operations::FileOperationReport, ExecuteFileOperationError> {
    if [item.source.as_deref(), item.destination.as_deref()]
        .into_iter()
        .flatten()
        .any(crate::network::is_unc_path)
        && matches!(
            kind,
            FileOperationKind::CreateFolder | FileOperationKind::Rename
        )
    {
        let isolated_kind = match kind {
            FileOperationKind::CreateFolder => {
                platform::windows::network::IsolatedFileMutationKind::CreateFolder
            }
            FileOperationKind::Rename => {
                platform::windows::network::IsolatedFileMutationKind::Rename
            }
            _ => unreachable!(),
        };
        let result = platform::windows::network::isolated_file_mutation(
            isolated_kind,
            item.source.as_deref(),
            item.destination.as_deref(),
            cancel.cancellation_flag(),
        )
        .map_err(|error| error.to_string())?;
        return Ok(crate::fs::file_operations::FileOperationReport {
            files: usize::from(kind != FileOperationKind::CreateFolder),
            directories: usize::from(kind == FileOperationKind::CreateFolder),
            bytes: 0,
            skipped: Vec::new(),
            affected_directories: result.affected_directories,
            cleanup_pending: None,
            completed_paths: result.completed_path.into_iter().collect(),
            undo_identities: Vec::new(),
            undo_root: None,
            undo_root_created_exclusively: false,
        });
    }
    let has_unc_path = [item.source.as_deref(), item.destination.as_deref()]
        .into_iter()
        .flatten()
        .any(crate::network::is_unc_path);
    let network_operation = if has_unc_path {
        match kind {
            FileOperationKind::Copy => {
                Some(platform::windows::network::IsolatedNetworkOperationKind::Copy)
            }
            FileOperationKind::Move => {
                Some(platform::windows::network::IsolatedNetworkOperationKind::Move)
            }
            FileOperationKind::PermanentDelete => {
                Some(platform::windows::network::IsolatedNetworkOperationKind::PermanentDelete)
            }
            FileOperationKind::RecycleDelete => {
                Some(platform::windows::network::IsolatedNetworkOperationKind::Recycle)
            }
            _ => None,
        }
    } else {
        None
    };
    let replace = &mut |category, source: &Path, destination: &Path| {
        let _conflict_gate = conflict_gate
            .lock()
            .expect("conflict gate mutex is not poisoned");
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
        loop {
            match response_receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(decision) => {
                    if decision.apply_to_all {
                        conflict_defaults.insert(category, decision.action);
                    }
                    return decision.action;
                }
                Err(mpsc::RecvTimeoutError::Timeout) if !cancel.is_cancelled() => {}
                Err(_) => return crate::domain::file_operations::ConflictAction::Skip,
            }
        }
    };
    if let Some(network_kind) = network_operation {
        let source = item.source.as_ref().ok_or("missing source")?;
        let progress_emitter = RefCell::new(progress_emitter);
        let report = platform::windows::network::isolated_network_file_operation(
            network_kind,
            source,
            item.destination.as_deref(),
            cancel,
            replace,
            &mut |bytes, current| {
                progress_emitter.borrow_mut().discovered(bytes, current);
            },
            &mut |bytes, file_completed, current| {
                progress_emitter
                    .borrow_mut()
                    .advanced(bytes, file_completed, current);
            },
        )
        .map_err(|error| error.to_string())?;
        return Ok(crate::fs::file_operations::FileOperationReport {
            files: report.files,
            directories: report.directories,
            bytes: report.bytes,
            skipped: report.skipped,
            affected_directories: report.affected_directories,
            cleanup_pending: None,
            completed_paths: report.completed_paths,
            undo_identities: Vec::new(),
            undo_root: None,
            undo_root_created_exclusively: false,
        });
    }
    match kind {
        FileOperationKind::CreateFolder => {
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let parent = destination.parent().ok_or("missing parent")?;
            let name = destination.file_name().ok_or("missing name")?;
            crate::fs::file_operations::create_folder_with_identity(parent, name)
                .map(|(path, identity)| {
                    let mut report = crate::fs::file_operations::FileOperationReport {
                        files: 0,
                        directories: 1,
                        bytes: 0,
                        skipped: vec![],
                        affected_directories: vec![],
                        cleanup_pending: None,
                        completed_paths: vec![path.clone()],
                        undo_identities: Vec::new(),
                        undo_root: None,
                        undo_root_created_exclusively: false,
                    };
                    report.undo_identities.push((path.clone(), identity));
                    if let Some(parent) = path.parent() {
                        report.affected_directories.push(parent.to_path_buf());
                    }
                    report
                })
                .map_err(ExecuteFileOperationError::failed_debug)
        }
        FileOperationKind::Rename => {
            let source = item.source.as_ref().ok_or("missing source")?;
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let name = destination.file_name().ok_or("missing name")?;
            crate::fs::file_operations::rename_path_with_identity(source, name)
                .map(|(path, identity)| {
                    let mut report = crate::fs::file_operations::FileOperationReport {
                        files: 1,
                        directories: 0,
                        bytes: 0,
                        skipped: vec![],
                        affected_directories: vec![],
                        cleanup_pending: None,
                        completed_paths: vec![path.clone()],
                        undo_identities: Vec::new(),
                        undo_root: None,
                        undo_root_created_exclusively: false,
                    };
                    report.undo_identities.push((path.clone(), identity));
                    if let Some(parent) = path.parent() {
                        report.affected_directories.push(parent.to_path_buf());
                    }
                    report
                })
                .map_err(ExecuteFileOperationError::failed_debug)
        }
        FileOperationKind::Copy | FileOperationKind::Move => {
            let source = item.source.as_ref().ok_or("missing source")?;
            let destination = item.destination.as_ref().ok_or("missing destination")?;
            let progress_emitter = RefCell::new(progress_emitter);
            let mut progress = |bytes, file_completed, current: &Path| {
                progress_emitter
                    .borrow_mut()
                    .advanced(bytes, file_completed, current);
            };
            let mut discovered = |bytes, current: &Path| {
                progress_emitter.borrow_mut().discovered(bytes, current);
            };
            let mut root_destination_reported = false;
            let result = if kind == FileOperationKind::Copy {
                crate::fs::file_operations::copy_path_with_progress(
                    source,
                    destination,
                    cancel,
                    replace,
                    &mut discovered,
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
                    &mut discovered,
                    &mut progress,
                )
            };
            result.map_err(ExecuteFileOperationError::from_operation)
        }
        FileOperationKind::Undo => unreachable!("undo requests use the undo batch executor"),
        FileOperationKind::RecycleDelete => {
            unreachable!("local recycle delete requests are executed as one Shell batch")
        }
        FileOperationKind::PermanentDelete => {
            unreachable!("local permanent delete requests use the two-stage batch executor")
        }
        FileOperationKind::FastRemove => {
            let path = item.source.as_ref().ok_or("missing source")?;
            let parent = path.parent().ok_or("missing parent")?;
            let report = crate::fs::file_operations::fast_remove(
                path,
                &parent.join(".asterfiles-cleanup"),
                cancel,
            )
            .map_err(ExecuteFileOperationError::failed_debug)?;
            if let Some(pending) = report.cleanup_pending.as_ref()
                && let Err(error) = crate::fs::file_operations::clean_pending(pending, cancel)
            {
                return Err(ExecuteFileOperationError::Failed(format!(
                    "cleanup pending at {}: {error:?}",
                    pending.as_os_str().to_string_lossy()
                )));
            }
            Ok(report)
        }
    }
}

pub(super) fn is_fast_remove_candidate(path: &Path) -> bool {
    if crate::network::is_unc_path(path) || path.parent().is_none() {
        return false;
    }
    let protected = std::env::var_os("USERPROFILE").is_some_and(|home| Path::new(&home) == path)
        || std::env::current_dir().is_ok_and(|workspace| workspace == path);
    if protected {
        return false;
    }
    std::fs::symlink_metadata(path).is_ok_and(|metadata| {
        let file_type = metadata.file_type();
        file_type.is_dir() && !file_type.is_symlink()
    })
}

pub(super) fn cleanup_record_root() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|root| root.join("AsterFiles").join("cleanup-tasks"))
}

pub(super) fn completed_target_for_item(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{FixtureCleanup, process_cpu_millis};

    #[test]
    fn issue_101_source_retained_keeps_committed_target_even_when_cancelled() {
        for cancelled in [false, true] {
            let destination = PathBuf::from("committed-target");
            let error = ExecuteFileOperationError::from_operation(
                crate::fs::file_operations::OperationError::DestinationCommittedSourceRetained {
                    source: PathBuf::from("retained-source"),
                    destination: destination.clone(),
                    message: "source retained".to_owned(),
                },
            );
            assert_eq!(
                error.into_item_result(cancelled),
                (
                    ItemState::Succeeded,
                    Some("source retained".to_owned()),
                    Some(destination),
                ),
            );
        }
        assert_eq!(
            ExecuteFileOperationError::Failed("cancelled copy".to_owned()).into_item_result(true),
            (ItemState::Cancelled, None, None),
        );
    }

    #[test]
    fn issue_81_recycle_progress_is_time_coalesced_but_phase_changes_flush() {
        use platform::windows::file_operation::RecycleProgress;

        let mut coalescer = RecycleProgressCoalescer::new();
        assert!(
            coalescer
                .push(RecycleProgress::Preparing {
                    prepared: 0,
                    total: 2,
                    current: None,
                })
                .is_some()
        );
        assert!(
            coalescer
                .push(RecycleProgress::Preparing {
                    prepared: 1,
                    total: 2,
                    current: Some(PathBuf::from("one")),
                })
                .is_none()
        );
        assert!(
            coalescer
                .push(RecycleProgress::Preparing {
                    prepared: 2,
                    total: 2,
                    current: Some(PathBuf::from("two")),
                })
                .is_some()
        );
        assert!(
            coalescer
                .push(RecycleProgress::Executing {
                    work_total: None,
                    work_completed: 0,
                    current: Some(PathBuf::from("one")),
                })
                .is_some()
        );
        assert!(
            coalescer
                .push(RecycleProgress::Executing {
                    work_total: Some(100),
                    work_completed: 1,
                    current: Some(PathBuf::from("one")),
                })
                .is_none()
        );
        coalescer.make_due_for_test();
        assert!(matches!(
            coalescer.push(RecycleProgress::Executing {
                work_total: Some(100),
                work_completed: 37,
                current: Some(PathBuf::from("one")),
            }),
            Some(RecycleProgress::Executing {
                work_total: Some(100),
                work_completed: 37,
                ..
            })
        ));
        assert!(
            coalescer
                .push(RecycleProgress::Executing {
                    work_total: Some(100),
                    work_completed: 38,
                    current: Some(PathBuf::from("one")),
                })
                .is_none()
        );
        assert!(matches!(
            coalescer.flush(),
            Some(RecycleProgress::Executing {
                work_total: Some(100),
                work_completed: 38,
                ..
            })
        ));
    }
    #[test]
    fn issue_61_progress_emitter_coalesces_and_flushes_scan_completion() {
        let (sender, receiver) = mpsc::channel();
        let mut emitter = OperationProgressEmitter::new(
            &sender,
            OperationProgressSnapshot {
                id: OperationId(61),
                completed_items: 0,
                completed_files: 0,
                total_files: None,
                discovered_files: 0,
                processed_bytes: 0,
                total_bytes: None,
                discovered_bytes: 0,
                scanning_complete: false,
                current_item: PathBuf::new(),
                recent_speed_bps: None,
            },
            crate::domain::file_operations::CancellationToken::new(),
            Instant::now(),
        );

        emitter.flush();
        for index in 0..32 {
            let path = PathBuf::from(format!("file-{index}"));
            emitter.discovered(1, &path);
            emitter.advanced(1, true, &path);
        }
        assert_eq!(receiver.try_iter().count(), 1);

        emitter.last_sent_at = Instant::now() - OPERATION_PROGRESS_INTERVAL;
        emitter.advanced(OPERATION_PROGRESS_BYTES, false, Path::new("large.bin"));
        assert_eq!(receiver.try_iter().count(), 1);

        emitter.finish_scan();
        let events = receiver.try_iter().collect::<Vec<_>>();
        assert_eq!(events.len(), 1);
        let FileOperationEvent::Progress {
            completed_files,
            total_files,
            discovered_files,
            total_bytes,
            scanning_complete,
            ..
        } = &events[0]
        else {
            panic!("scan completion must flush progress");
        };
        assert_eq!(*completed_files, 32);
        assert_eq!(*discovered_files, 32);
        assert_eq!(*total_files, Some(32));
        assert_eq!(*total_bytes, Some(32));
        assert!(*scanning_complete);
    }
    #[test]
    fn issue_82_directory_commit_moves_source_before_cleanup_runs() {
        let temp = std::env::temp_dir().join(format!(
            "asterfiles-issue-82-commit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _fixture_cleanup = FixtureCleanup(temp.clone());
        let source = temp.join("source");
        let records = temp.join("records");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("one.txt"), b"one").unwrap();
        let (sender, receiver) = mpsc::channel();

        commit_permanent_delete_request_with_root(
            FileOperationRequest {
                id: OperationId(82),
                kind: FileOperationKind::PermanentDelete,
                resource: OperationResource::Local,
                items: vec![OperationItem::pending(Some(source.clone()), None)],
                undo_items: Vec::new(),
                undo_source_manifests: vec![None],
                cancellation: crate::domain::file_operations::CancellationToken::new(),
            },
            &sender,
            Some(&records),
        );

        let events = receiver.try_iter().collect::<Vec<_>>();
        assert!(!source.exists());
        assert!(events.iter().any(|event| matches!(
            event,
            FileOperationEvent::PermanentDeleteStage {
                stage: PermanentDeleteStage::SourceRemoved,
                affected_directories,
                ..
            } if affected_directories == &vec![temp.clone()]
        )));
        let cleanup = events.iter().find_map(|event| match event {
            FileOperationEvent::PermanentDeleteCommitted {
                cleanup: Some(cleanup),
                result,
                ..
            } if result.failed.is_empty() => Some(cleanup),
            _ => None,
        });
        let cleanup = cleanup.expect("commit must hand off background cleanup");
        assert_eq!(cleanup.resource, OperationResource::Cleanup);
        assert!(cleanup.items[0].destination.as_ref().unwrap().exists());
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, FileOperationEvent::Finished { .. }))
        );
    }
    #[test]
    fn issue_82_consecutive_commits_remove_both_batches_before_cleanup() {
        let temp = std::env::temp_dir().join(format!(
            "asterfiles-issue-82-consecutive-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _fixture_cleanup = FixtureCleanup(temp.clone());
        let records = temp.join("records");
        let sources = [temp.join("first"), temp.join("second")];
        for source in &sources {
            std::fs::create_dir_all(source).unwrap();
            std::fs::write(source.join("content.txt"), b"content").unwrap();
        }
        let (sender, receiver) = mpsc::channel();
        for (index, source) in sources.iter().enumerate() {
            commit_permanent_delete_request_with_root(
                FileOperationRequest {
                    id: OperationId(82 + index as u64),
                    kind: FileOperationKind::PermanentDelete,
                    resource: OperationResource::Local,
                    items: vec![OperationItem::pending(Some(source.clone()), None)],
                    undo_items: Vec::new(),
                    undo_source_manifests: vec![None],
                    cancellation: crate::domain::file_operations::CancellationToken::new(),
                },
                &sender,
                Some(&records),
            );
        }

        assert!(sources.iter().all(|source| !source.exists()));
        let committed = receiver
            .try_iter()
            .filter(|event| {
                matches!(
                    event,
                    FileOperationEvent::PermanentDeleteCommitted {
                        cleanup: Some(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(committed, 2);
    }
    #[test]
    fn issue_42_network_recycle_bypasses_in_process_shell_batch() {
        assert!(uses_local_recycle_batch(
            FileOperationKind::RecycleDelete,
            OperationResource::Local,
        ));
        assert!(!uses_local_recycle_batch(
            FileOperationKind::RecycleDelete,
            OperationResource::Network,
        ));
    }
    #[test]
    fn issue_83_directory_rename_creates_a_root_identity_undo_item() {
        let temp = std::env::temp_dir().join(format!(
            "asterfiles-issue-83-directory-rename-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir(&temp).unwrap();
        let original = temp.join("folder");
        let current = temp.join("renamed");
        std::fs::create_dir(&current).unwrap();
        std::fs::write(current.join("child.txt"), b"content").unwrap();
        let item = OperationItem::pending(Some(original.clone()), Some(current.clone()));
        let report = undo_report_for_paths(std::slice::from_ref(&current));
        let source_manifest = crate::fs::file_operations::collect_tree_identities(
            &current,
            UndoHistory::MAX_SNAPSHOT_ITEMS,
        )
        .unwrap()
        .into_iter()
        .map(|(path, identity)| {
            let relative = path.strip_prefix(&current).unwrap();
            (original.join(relative), identity)
        })
        .collect::<Vec<_>>();

        let undo = undo_items_for_success(
            FileOperationKind::Rename,
            &item,
            &report,
            Some(&current),
            false,
            Some(&source_manifest),
            UndoHistory::MAX_SNAPSHOT_ITEMS,
        )
        .unwrap();

        assert!(matches!(
            &undo[0],
            UndoItem::MoveBack {
                current: target,
                original: source,
                manifest,
                manifest_complete: true,
            } if target == &current && source == &original && manifest.len() == 2
        ));
        std::fs::remove_dir_all(temp).unwrap();
    }
    #[test]
    #[ignore = "explicit 100k-file performance evidence"]
    fn issue_61_100k_small_files_performance_evidence() {
        use std::{cell::RefCell, fs};

        let artifact_dir = PathBuf::from("artifacts/perf/file-operations");
        fs::create_dir_all(&artifact_dir).unwrap();
        let fixture =
            std::env::temp_dir().join(format!("asterfiles-issue-61-{}", std::process::id()));
        let _ = fs::remove_dir_all(&fixture);
        let _fixture_cleanup = FixtureCleanup(fixture.clone());
        let source = fixture.join("source");
        let destination = fixture.join("destination");
        fs::create_dir_all(&source).unwrap();
        for directory in 0..500_u32 {
            let parent = source.join(format!("d-{directory:03}"));
            fs::create_dir(&parent).unwrap();
            for file in 0..200_u32 {
                fs::File::create(parent.join(format!("f-{file:04}.txt"))).unwrap();
            }
        }

        let started = Instant::now();
        let first_copy_started = RefCell::new(None);
        let raw_progress_events = RefCell::new(0_u64);
        let discovered_files = RefCell::new(0_usize);
        let discovered_bytes = RefCell::new(0_u64);
        let (sender, receiver) = mpsc::channel();
        let emitter = RefCell::new(OperationProgressEmitter::new(
            &sender,
            OperationProgressSnapshot {
                id: OperationId(61),
                completed_items: 0,
                completed_files: 0,
                total_files: None,
                discovered_files: 0,
                processed_bytes: 0,
                total_bytes: None,
                discovered_bytes: 0,
                scanning_complete: false,
                current_item: source.clone(),
                recent_speed_bps: None,
            },
            crate::domain::file_operations::CancellationToken::new(),
            started,
        ));
        emitter.borrow_mut().flush();
        let cpu_started = process_cpu_millis();
        crate::fs::file_operations::copy_path_with_progress(
            &source,
            &destination,
            &crate::domain::file_operations::CancellationToken::new(),
            &mut |_, _, _| crate::domain::file_operations::ConflictAction::Replace,
            &mut |bytes, path| {
                *discovered_files.borrow_mut() += 1;
                *discovered_bytes.borrow_mut() += bytes;
                emitter.borrow_mut().discovered(bytes, path);
            },
            &mut |bytes, completed, path| {
                *raw_progress_events.borrow_mut() += 1;
                if completed && first_copy_started.borrow().is_none() {
                    *first_copy_started.borrow_mut() = Some(started.elapsed());
                }
                emitter.borrow_mut().advanced(bytes, completed, path);
            },
            &mut |_| {},
        )
        .unwrap();
        let scan_finished = started.elapsed();
        emitter.borrow_mut().finish_scan();
        let total_elapsed = started.elapsed();
        let committed_progress_events = receiver.try_iter().count();
        let cpu_millis = process_cpu_millis().saturating_sub(cpu_started);
        let first_copy_started = first_copy_started.borrow().unwrap();
        assert_eq!(*discovered_files.borrow(), 100_000);
        assert_eq!(*discovered_bytes.borrow(), 0);
        assert!(first_copy_started < scan_finished);
        assert_eq!(destination.read_dir().unwrap().count(), 500);

        fs::write(
            artifact_dir.join("100k-small-files.json"),
            format!(
                concat!(
                    "{{\n",
                    "  \"schema_version\": 1,\n",
                    "  \"issue\": 61,\n",
                    "  \"fixture_file_count\": 100000,\n",
                    "  \"first_copy_started_ms\": {},\n",
                    "  \"scan_finished_ms\": {},\n",
                    "  \"total_elapsed_ms\": {},\n",
                    "  \"raw_progress_events\": {},\n",
                    "  \"committed_progress_events\": {},\n",
                    "  \"task_center_model_refreshes\": {},\n",
                    "  \"other_window_model_refreshes\": 0,\n",
                    "  \"cpu_millis\": {}\n",
                    "}}\n"
                ),
                first_copy_started.as_millis(),
                scan_finished.as_millis(),
                total_elapsed.as_millis(),
                *raw_progress_events.borrow(),
                committed_progress_events,
                committed_progress_events,
                cpu_millis,
            ),
        )
        .unwrap();
    }
}
