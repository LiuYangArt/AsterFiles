#![allow(dead_code)]

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use crate::domain::TabId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperationKind {
    CreateFolder,
    Rename,
    Copy,
    Move,
    RecycleDelete,
    PermanentDelete,
    FastRemove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationState {
    Queued,
    Preflight,
    Running,
    Paused,
    WaitingConflict,
    Cancelling,
    Completed,
    Cancelled,
    PartiallyCompleted,
    Failed,
}

impl OperationState {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Preflight
                | Self::Running
                | Self::Paused
                | Self::WaitingConflict
                | Self::Cancelling
        )
    }
    pub fn is_terminal(self) -> bool {
        !self.is_active()
    }
    pub fn can_transition_to(self, next: Self) -> bool {
        use OperationState::*;
        matches!(
            (self, next),
            (Queued, Preflight | Cancelling | Cancelled | Failed)
                | (Preflight, Running | Cancelling | Completed | Failed)
                | (
                    Running,
                    Paused | WaitingConflict | Cancelling | Completed | PartiallyCompleted | Failed
                )
                | (Paused, Running | Cancelling)
                | (
                    WaitingConflict,
                    Running | Cancelling | PartiallyCompleted | Failed
                )
                | (Cancelling, Cancelled | PartiallyCompleted | Failed)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConflictCategory {
    ExistingFile,
    ExistingDirectory,
    TypeMismatch,
    DestinationReadOnly,
    SourceInsideDestination,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction {
    Replace,
    Skip,
    KeepBoth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictDecision {
    pub action: ConflictAction,
    pub apply_to_all: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    pub path: PathBuf,
    pub is_directory: bool,
    pub size_bytes: Option<u64>,
    pub modified: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationConflict {
    pub category: ConflictCategory,
    pub source: FileSnapshot,
    pub destination: FileSnapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    Pending,
    Running,
    Succeeded,
    Skipped,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationItem {
    pub source: Option<PathBuf>,
    pub destination: Option<PathBuf>,
    pub state: ItemState,
    pub error: Option<String>,
}

impl OperationItem {
    pub fn pending(source: Option<PathBuf>, destination: Option<PathBuf>) -> Self {
        Self {
            source,
            destination,
            state: ItemState::Pending,
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OperationProgress {
    pub total_items: usize,
    pub completed_items: usize,
    pub total_files: Option<usize>,
    pub completed_files: usize,
    pub processed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub current_item: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationResult {
    pub succeeded: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub failed: Vec<(PathBuf, String)>,
    pub affected_directories: Vec<PathBuf>,
}

#[derive(Debug, Default)]
struct PauseState {
    paused: bool,
    started: Option<Instant>,
    accumulated: Duration,
}

#[derive(Debug)]
struct OperationControl {
    cancelled: AtomicBool,
    pause: Mutex<PauseState>,
    wake: Condvar,
}

#[derive(Debug, Clone)]
pub struct CancellationToken(Arc<OperationControl>);
impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}
impl CancellationToken {
    pub fn new() -> Self {
        Self(Arc::new(OperationControl {
            cancelled: AtomicBool::new(false),
            pause: Mutex::new(PauseState::default()),
            wake: Condvar::new(),
        }))
    }
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Release);
        self.0.wake.notify_all();
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    pub fn cancellation_flag(&self) -> &AtomicBool {
        &self.0.cancelled
    }
    pub fn pause(&self) {
        if let Ok(mut state) = self.0.pause.lock()
            && !state.paused
        {
            state.paused = true;
            state.started = Some(Instant::now());
        }
    }
    pub fn resume(&self) {
        if let Ok(mut state) = self.0.pause.lock()
            && state.paused
        {
            state.paused = false;
            if let Some(started) = state.started.take() {
                state.accumulated += started.elapsed();
            }
            self.0.wake.notify_all();
        }
    }
    pub fn is_paused(&self) -> bool {
        self.0.pause.lock().is_ok_and(|state| state.paused)
    }
    pub fn wait_if_paused(&self) {
        let Ok(mut state) = self.0.pause.lock() else {
            return;
        };
        while state.paused && !self.is_cancelled() {
            let Ok(next) = self.0.wake.wait(state) else {
                return;
            };
            state = next;
        }
    }
    pub fn active_elapsed(&self, started: Instant) -> Duration {
        let elapsed = started.elapsed();
        let paused = self.0.pause.lock().map_or(Duration::ZERO, |state| {
            state.accumulated
                + state
                    .started
                    .map_or(Duration::ZERO, |value| value.elapsed())
        });
        elapsed.saturating_sub(paused)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperationResource {
    Local,
    Network,
}

impl OperationResource {
    const fn index(self) -> usize {
        match self {
            Self::Local => 0,
            Self::Network => 1,
        }
    }
}

#[derive(Debug, Clone)]
pub struct OperationTask {
    pub id: OperationId,
    pub kind: FileOperationKind,
    pub resource: OperationResource,
    pub created_at: SystemTime,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub origin_tab: Option<TabId>,
    pub state: OperationState,
    pub execution_round: u32,
    pub items: Vec<OperationItem>,
    pub progress: OperationProgress,
    pub conflict: Option<OperationConflict>,
    pub result: Option<OperationResult>,
    pub cancellation: CancellationToken,
    conflict_defaults: HashMap<ConflictCategory, ConflictAction>,
}

impl OperationTask {
    fn new(
        id: OperationId,
        resource: OperationResource,
        kind: FileOperationKind,
        origin_tab: Option<TabId>,
        items: Vec<OperationItem>,
    ) -> Self {
        Self {
            id,
            kind,
            resource,
            created_at: SystemTime::now(),
            started_at: Instant::now(),
            finished_at: None,
            origin_tab,
            state: OperationState::Queued,
            execution_round: 1,
            progress: OperationProgress {
                total_items: items.len(),
                ..Default::default()
            },
            items,
            conflict: None,
            result: None,
            cancellation: CancellationToken::new(),
            conflict_defaults: HashMap::new(),
        }
    }
    pub fn transition(&mut self, next: OperationState) -> Result<(), StateTransitionError> {
        if !self.state.can_transition_to(next) {
            return Err(StateTransitionError {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
    pub fn conflict_default(&self, category: ConflictCategory) -> Option<ConflictAction> {
        self.conflict_defaults.get(&category).copied()
    }
    pub fn set_conflict(
        &mut self,
        conflict: OperationConflict,
    ) -> Result<(), StateTransitionError> {
        self.transition(OperationState::WaitingConflict)?;
        self.cancellation.pause();
        self.conflict = Some(conflict);
        Ok(())
    }
    pub fn resolve_conflict(
        &mut self,
        decision: ConflictDecision,
    ) -> Result<(), StateTransitionError> {
        let Some(conflict) = self.conflict.take() else {
            return Err(StateTransitionError {
                from: self.state,
                to: OperationState::Running,
            });
        };
        if decision.apply_to_all {
            self.conflict_defaults
                .insert(conflict.category, decision.action);
        }
        self.cancellation.resume();
        self.transition(OperationState::Running)
    }
    pub fn request_cancel(&mut self) -> Result<(), StateTransitionError> {
        self.cancellation.cancel();
        match self.state {
            OperationState::Queued => self.transition(OperationState::Cancelled),
            OperationState::Preflight
            | OperationState::Running
            | OperationState::Paused
            | OperationState::WaitingConflict => self.transition(OperationState::Cancelling),
            OperationState::Cancelling => Ok(()),
            _ => Err(StateTransitionError {
                from: self.state,
                to: OperationState::Cancelling,
            }),
        }
    }
    pub fn toggle_pause(&mut self) -> Result<(), StateTransitionError> {
        match self.state {
            OperationState::Running => {
                self.cancellation.pause();
                self.transition(OperationState::Paused)
            }
            OperationState::Paused => {
                self.cancellation.resume();
                self.transition(OperationState::Running)
            }
            _ => Err(StateTransitionError {
                from: self.state,
                to: OperationState::Paused,
            }),
        }
    }
    fn prepare_retry(&mut self) -> bool {
        let mut retry_count = 0;
        for item in &mut self.items {
            if matches!(
                item.state,
                ItemState::Failed | ItemState::Cancelled | ItemState::Pending
            ) {
                item.state = ItemState::Pending;
                item.error = None;
                retry_count += 1;
            }
        }
        if retry_count == 0 {
            return false;
        }
        self.execution_round += 1;
        self.state = OperationState::Queued;
        self.progress = OperationProgress {
            total_items: retry_count,
            ..Default::default()
        };
        self.conflict = None;
        self.result = None;
        self.started_at = Instant::now();
        self.finished_at = None;
        self.cancellation = CancellationToken::new();
        self.conflict_defaults.clear();
        true
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateTransitionError {
    pub from: OperationState,
    pub to: OperationState,
}

#[derive(Debug, Default)]
pub struct OperationManager {
    next_id: u64,
    tasks: BTreeMap<OperationId, OperationTask>,
    queues: [VecDeque<OperationId>; 2],
    active: [Option<OperationId>; 2],
}

impl OperationManager {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            queues: [VecDeque::new(), VecDeque::new()],
            active: [None, None],
            ..Default::default()
        }
    }
    pub fn submit(
        &mut self,
        resource: OperationResource,
        kind: FileOperationKind,
        origin_tab: Option<TabId>,
        items: Vec<OperationItem>,
    ) -> OperationId {
        let id = OperationId(self.next_id);
        self.next_id += 1;
        self.tasks.insert(
            id,
            OperationTask::new(id, resource, kind, origin_tab, items),
        );
        self.queues[resource.index()].push_back(id);
        id
    }
    pub fn iter(&self) -> impl Iterator<Item = &OperationTask> {
        self.tasks.values()
    }
    pub fn task(&self, id: OperationId) -> Option<&OperationTask> {
        self.tasks.get(&id)
    }
    pub fn task_mut(&mut self, id: OperationId) -> Option<&mut OperationTask> {
        self.tasks.get_mut(&id)
    }
    pub fn active_id(&self, resource: OperationResource) -> Option<OperationId> {
        self.active[resource.index()]
    }
    pub fn has_active_tasks(&self) -> bool {
        self.tasks.values().any(|task| task.state.is_active())
    }
    pub fn start_next(
        &mut self,
        resource: OperationResource,
    ) -> Result<Option<OperationId>, StateTransitionError> {
        let slot = resource.index();
        if self.active[slot].is_some() {
            return Ok(None);
        }
        while let Some(id) = self.queues[slot].pop_front() {
            let task = self.tasks.get_mut(&id).expect("queued task must exist");
            if task.state != OperationState::Queued {
                continue;
            }
            task.transition(OperationState::Preflight)?;
            self.active[slot] = Some(id);
            return Ok(Some(id));
        }
        Ok(None)
    }
    pub fn mark_running(&mut self, id: OperationId) -> Result<(), StateTransitionError> {
        self.require_active_mut(id)?
            .transition(OperationState::Running)
    }
    pub fn finish(
        &mut self,
        id: OperationId,
        state: OperationState,
        result: OperationResult,
    ) -> Result<(), StateTransitionError> {
        let task = self.require_active_mut(id)?;
        task.transition(state)?;
        task.result = Some(result);
        task.finished_at = Some(Instant::now());
        self.active[task.resource.index()] = None;
        Ok(())
    }
    pub fn clear_terminal(&mut self) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|_, task| !task.state.is_terminal());
        before - self.tasks.len()
    }
    pub fn prune_transient(&mut self, minimum_age: Duration) -> usize {
        let before = self.tasks.len();
        self.tasks.retain(|_, task| {
            !matches!(
                task.state,
                OperationState::Completed | OperationState::Cancelled
            ) || task
                .finished_at
                .is_none_or(|finished| finished.elapsed() < minimum_age)
        });
        before - self.tasks.len()
    }
    pub fn cancel(&mut self, id: OperationId) -> Result<(), StateTransitionError> {
        let resource =
            self.tasks
                .get(&id)
                .map(|task| task.resource)
                .ok_or(StateTransitionError {
                    from: OperationState::Failed,
                    to: OperationState::Cancelling,
                })?;
        let was_active = self.active[resource.index()] == Some(id);
        let task = self.tasks.get_mut(&id).expect("operation must exist");
        task.request_cancel()?;
        if !was_active {
            self.queues[resource.index()].retain(|queued| *queued != id);
        }
        Ok(())
    }
    pub fn toggle_pause(&mut self, id: OperationId) -> Result<(), StateTransitionError> {
        self.tasks
            .get_mut(&id)
            .expect("operation must exist")
            .toggle_pause()
    }
    pub fn remove_terminal(&mut self, id: OperationId) -> bool {
        let Some(resource) = self
            .tasks
            .get(&id)
            .filter(|task| task.state.is_terminal())
            .map(|task| task.resource)
        else {
            return false;
        };
        self.tasks.remove(&id);
        self.queues[resource.index()].retain(|queued| *queued != id);
        true
    }
    pub fn retry(&mut self, id: OperationId) -> bool {
        let resource = self.tasks.get(&id).map(|task| task.resource);
        if resource.is_some_and(|resource| self.active[resource.index()] == Some(id)) {
            return false;
        }
        let Some(task) = self.tasks.get_mut(&id) else {
            return false;
        };
        if !task.state.is_terminal() || !task.prepare_retry() {
            return false;
        }
        self.queues[task.resource.index()].push_back(id);
        true
    }
    fn require_active_mut(
        &mut self,
        id: OperationId,
    ) -> Result<&mut OperationTask, StateTransitionError> {
        let resource = self.tasks.get(&id).map(|task| task.resource);
        if resource.is_none_or(|resource| self.active[resource.index()] != Some(id)) {
            let from = self
                .tasks
                .get(&id)
                .map(|task| task.state)
                .unwrap_or(OperationState::Failed);
            return Err(StateTransitionError {
                from,
                to: OperationState::Running,
            });
        }
        Ok(self.tasks.get_mut(&id).expect("active task must exist"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(name: &str) -> OperationItem {
        OperationItem::pending(Some(PathBuf::from(name)), None)
    }
    #[test]
    fn manager_runs_only_one_write_task() {
        let mut manager = OperationManager::new();
        let first = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("a")],
        );
        let second = manager.submit(
            OperationResource::Local,
            FileOperationKind::Move,
            None,
            vec![item("b")],
        );
        assert_eq!(
            manager.start_next(OperationResource::Local).unwrap(),
            Some(first)
        );
        assert_eq!(manager.start_next(OperationResource::Local).unwrap(), None);
        manager.mark_running(first).unwrap();
        manager
            .finish(
                first,
                OperationState::Completed,
                OperationResult {
                    succeeded: vec![PathBuf::from("a")],
                    skipped: vec![],
                    failed: vec![],
                    affected_directories: vec![],
                },
            )
            .unwrap();
        assert_eq!(
            manager.start_next(OperationResource::Local).unwrap(),
            Some(second)
        );
    }
    #[test]
    fn local_and_network_tasks_run_in_parallel_but_same_resource_is_serial() {
        let mut manager = OperationManager::new();
        let local = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("local")],
        );
        let network = manager.submit(
            OperationResource::Network,
            FileOperationKind::Copy,
            None,
            vec![item("network")],
        );
        let queued_network = manager.submit(
            OperationResource::Network,
            FileOperationKind::Move,
            None,
            vec![item("network-2")],
        );
        assert_eq!(
            manager.start_next(OperationResource::Local).unwrap(),
            Some(local)
        );
        assert_eq!(
            manager.start_next(OperationResource::Network).unwrap(),
            Some(network)
        );
        assert_eq!(manager.active_id(OperationResource::Local), Some(local));
        assert_eq!(manager.active_id(OperationResource::Network), Some(network));
        assert_eq!(
            manager.start_next(OperationResource::Network).unwrap(),
            None
        );
        manager.mark_running(local).unwrap();
        manager.mark_running(network).unwrap();
        manager
            .finish(
                local,
                OperationState::Completed,
                OperationResult {
                    succeeded: vec![],
                    skipped: vec![],
                    failed: vec![],
                    affected_directories: vec![],
                },
            )
            .unwrap();
        assert_eq!(manager.active_id(OperationResource::Network), Some(network));
        manager
            .finish(
                network,
                OperationState::Completed,
                OperationResult {
                    succeeded: vec![],
                    skipped: vec![],
                    failed: vec![],
                    affected_directories: vec![],
                },
            )
            .unwrap();
        assert_eq!(
            manager.start_next(OperationResource::Network).unwrap(),
            Some(queued_network)
        );
    }

    #[test]
    fn cancelling_queued_network_task_removes_only_network_queue_entry() {
        let mut manager = OperationManager::new();
        let local = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("local")],
        );
        let network = manager.submit(
            OperationResource::Network,
            FileOperationKind::Copy,
            None,
            vec![item("network")],
        );
        let queued_network = manager.submit(
            OperationResource::Network,
            FileOperationKind::Move,
            None,
            vec![item("network-2")],
        );
        manager.start_next(OperationResource::Local).unwrap();
        manager.start_next(OperationResource::Network).unwrap();
        manager.mark_running(local).unwrap();
        manager.mark_running(network).unwrap();
        manager.cancel(queued_network).unwrap();
        assert_eq!(
            manager.task(queued_network).unwrap().state,
            OperationState::Cancelled
        );
        manager
            .finish(
                network,
                OperationState::Completed,
                OperationResult {
                    succeeded: vec![],
                    skipped: vec![],
                    failed: vec![],
                    affected_directories: vec![],
                },
            )
            .unwrap();
        assert_eq!(
            manager.start_next(OperationResource::Network).unwrap(),
            None
        );
    }
    #[test]
    fn invalid_state_transition_is_rejected() {
        let mut task = OperationTask::new(
            OperationId(1),
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("a")],
        );
        assert_eq!(
            task.transition(OperationState::Completed),
            Err(StateTransitionError {
                from: OperationState::Queued,
                to: OperationState::Completed
            })
        );
    }
    #[test]
    fn conflict_default_is_scoped_to_category() {
        let mut task = OperationTask::new(
            OperationId(1),
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("a")],
        );
        task.transition(OperationState::Preflight).unwrap();
        task.transition(OperationState::Running).unwrap();
        let snapshot = FileSnapshot {
            path: PathBuf::from("a"),
            is_directory: false,
            size_bytes: Some(1),
            modified: None,
        };
        task.set_conflict(OperationConflict {
            category: ConflictCategory::ExistingFile,
            source: snapshot.clone(),
            destination: snapshot,
        })
        .unwrap();
        task.resolve_conflict(ConflictDecision {
            action: ConflictAction::Skip,
            apply_to_all: true,
        })
        .unwrap();
        assert_eq!(
            task.conflict_default(ConflictCategory::ExistingFile),
            Some(ConflictAction::Skip)
        );
        assert_eq!(task.conflict_default(ConflictCategory::TypeMismatch), None);
    }
    #[test]
    fn conflict_wait_is_excluded_from_active_elapsed_time() {
        let mut task = OperationTask::new(
            OperationId(1),
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("a")],
        );
        task.transition(OperationState::Preflight).unwrap();
        task.transition(OperationState::Running).unwrap();
        task.started_at = Instant::now() - Duration::from_millis(700);
        let snapshot = FileSnapshot {
            path: PathBuf::from("a"),
            is_directory: false,
            size_bytes: Some(1),
            modified: None,
        };

        task.set_conflict(OperationConflict {
            category: ConflictCategory::ExistingFile,
            source: snapshot.clone(),
            destination: snapshot,
        })
        .unwrap();
        assert_eq!(task.state, OperationState::WaitingConflict);
        assert!(task.cancellation.is_paused());
        let before = task.cancellation.active_elapsed(task.started_at);
        std::thread::sleep(Duration::from_millis(30));
        let during = task.cancellation.active_elapsed(task.started_at);
        assert!(during.saturating_sub(before) < Duration::from_millis(10));

        task.resolve_conflict(ConflictDecision {
            action: ConflictAction::Replace,
            apply_to_all: false,
        })
        .unwrap();
        assert_eq!(task.state, OperationState::Running);
        assert!(!task.cancellation.is_paused());
        std::thread::sleep(Duration::from_millis(20));
        assert!(
            task.cancellation.active_elapsed(task.started_at) >= during + Duration::from_millis(15)
        );
    }

    #[test]
    fn cancelling_queued_task_removes_it_from_queue() {
        let mut manager = OperationManager::new();
        let first = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("a")],
        );
        let second = manager.submit(
            OperationResource::Local,
            FileOperationKind::Move,
            None,
            vec![item("b")],
        );
        manager.cancel(first).unwrap();
        assert_eq!(
            manager.task(first).unwrap().state,
            OperationState::Cancelled
        );
        assert_eq!(
            manager.start_next(OperationResource::Local).unwrap(),
            Some(second)
        );
    }
    #[test]
    fn running_task_can_pause_resume_and_cancel() {
        let mut manager = OperationManager::new();
        let id = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("a")],
        );
        manager.start_next(OperationResource::Local).unwrap();
        manager.mark_running(id).unwrap();

        manager.toggle_pause(id).unwrap();
        assert_eq!(manager.task(id).unwrap().state, OperationState::Paused);
        assert!(manager.task(id).unwrap().cancellation.is_paused());

        manager.toggle_pause(id).unwrap();
        assert_eq!(manager.task(id).unwrap().state, OperationState::Running);
        assert!(!manager.task(id).unwrap().cancellation.is_paused());

        manager.toggle_pause(id).unwrap();
        manager.cancel(id).unwrap();
        assert_eq!(manager.task(id).unwrap().state, OperationState::Cancelling);
        assert!(manager.task(id).unwrap().cancellation.is_cancelled());
    }
    #[test]
    fn retry_keeps_successes() {
        let mut manager = OperationManager::new();
        let id = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("ok"), item("failed"), item("pending")],
        );
        manager.start_next(OperationResource::Local).unwrap();
        manager.mark_running(id).unwrap();
        {
            let task = manager.task_mut(id).unwrap();
            task.items[0].state = ItemState::Succeeded;
            task.items[1].state = ItemState::Failed;
        }
        manager
            .finish(
                id,
                OperationState::PartiallyCompleted,
                OperationResult {
                    succeeded: vec![PathBuf::from("ok")],
                    skipped: vec![],
                    failed: vec![(PathBuf::from("failed"), "locked".into())],
                    affected_directories: vec![],
                },
            )
            .unwrap();
        assert!(manager.retry(id));
        let task = manager.task(id).unwrap();
        assert_eq!(task.execution_round, 2);
        assert_eq!(task.progress.total_items, 2);
        assert_eq!(task.items[0].state, ItemState::Succeeded);
        assert_eq!(task.items[1].state, ItemState::Pending);
    }

    #[test]
    fn terminal_cleanup_keeps_active_tasks() {
        let mut manager = OperationManager::new();
        let finished = manager.submit(
            OperationResource::Local,
            FileOperationKind::Copy,
            None,
            vec![item("done")],
        );
        let queued = manager.submit(
            OperationResource::Local,
            FileOperationKind::Move,
            None,
            vec![item("queued")],
        );
        assert_eq!(
            manager.start_next(OperationResource::Local).unwrap(),
            Some(finished)
        );
        manager.mark_running(finished).unwrap();
        manager
            .finish(
                finished,
                OperationState::Completed,
                OperationResult {
                    succeeded: vec![PathBuf::from("done")],
                    skipped: vec![],
                    failed: vec![],
                    affected_directories: vec![],
                },
            )
            .unwrap();

        assert_eq!(manager.clear_terminal(), 1);
        assert!(manager.task(finished).is_none());
        assert_eq!(manager.task(queued).unwrap().state, OperationState::Queued);
    }
}
