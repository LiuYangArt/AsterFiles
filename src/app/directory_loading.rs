//! Directory workers own local/network queues; results retain tab/request identity.
use crate::{
    domain::{EntryId, FileEntry, FolderSizeState, LibraryLocationId, RequestId, TabId},
    fs::{
        ReadOutcome, SourceReadState, read_aggregate_directory_batches_filtered,
        read_directory_batches_filtered,
    },
    network::NetworkExecutionKey,
    platform,
};
use std::{
    collections::{HashSet, VecDeque},
    io,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};
#[derive(Debug)]
pub(super) struct DirectoryRequest {
    pub(super) tab_id: TabId,
    pub(super) request_id: RequestId,
    pub(super) path: PathBuf,
    pub(super) library: Option<LibraryLocationId>,
    pub(super) library_sources: Option<Vec<PathBuf>>,
    pub(super) unavailable_library_sources: usize,
    pub(super) visibility: crate::domain::FileVisibility,
    pub(super) cancel: Arc<std::sync::atomic::AtomicBool>,
}

struct NetworkDirectoryCompletion {
    key: NetworkExecutionKey,
}

#[derive(Default)]
struct NetworkDirectoryScheduler {
    pending: VecDeque<(NetworkExecutionKey, DirectoryRequest)>,
    active: HashSet<NetworkExecutionKey>,
}

impl NetworkDirectoryScheduler {
    fn push(&mut self, key: NetworkExecutionKey, request: DirectoryRequest) {
        self.pending.push_back((key, request));
    }

    fn complete(&mut self, key: &NetworkExecutionKey) {
        self.active.remove(key);
    }

    fn next_ready(&mut self) -> Option<(NetworkExecutionKey, DirectoryRequest)> {
        let index = self
            .pending
            .iter()
            .position(|(key, request)| !self.active.contains(key) && !request.cancelled())?;
        let (key, request) = self.pending.remove(index)?;
        self.active.insert(key.clone());
        Some((key, request))
    }

    fn take_cancelled(&mut self) -> Vec<DirectoryRequest> {
        let mut kept = VecDeque::with_capacity(self.pending.len());
        let mut cancelled = Vec::new();
        while let Some((key, request)) = self.pending.pop_front() {
            if request.cancelled() {
                cancelled.push(request);
            } else {
                kept.push_back((key, request));
            }
        }
        self.pending = kept;
        cancelled
    }
}

impl DirectoryRequest {
    fn cancelled(&self) -> bool {
        self.cancel.load(std::sync::atomic::Ordering::Acquire)
    }
}

#[cfg(test)]
pub(super) fn network_directory_request(path: &str) -> DirectoryRequest {
    DirectoryRequest {
        tab_id: TabId(1),
        request_id: RequestId(1),
        path: PathBuf::from(path),
        library: None,
        library_sources: None,
        unavailable_library_sources: 0,
        visibility: crate::domain::FileVisibility::default(),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    }
}

#[derive(Debug)]
pub(super) enum DirectoryEvent {
    NetworkBatch {
        tab_id: TabId,
        request_id: RequestId,
        entries: Vec<FileEntry>,
        acknowledgement: mpsc::Sender<()>,
    },
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
        source_failures: usize,
        library: Option<LibraryLocationId>,
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
    Slow {
        tab_id: TabId,
        request_id: RequestId,
    },
}

impl DirectoryEvent {
    pub(super) fn request_identity(&self) -> (TabId, RequestId) {
        match self {
            Self::NetworkBatch {
                tab_id, request_id, ..
            }
            | Self::Batch {
                tab_id, request_id, ..
            }
            | Self::Finished {
                tab_id, request_id, ..
            }
            | Self::Cancelled { tab_id, request_id }
            | Self::Failed {
                tab_id, request_id, ..
            }
            | Self::Slow { tab_id, request_id } => (*tab_id, *request_id),
        }
    }
}
pub(super) fn spawn_directory_workers(
    worker_count: usize,
    network_worker_count: usize,
) -> (
    mpsc::Sender<DirectoryRequest>,
    mpsc::SyncSender<DirectoryRequest>,
    mpsc::Receiver<DirectoryEvent>,
) {
    let network_worker_count = network_worker_count.max(1);
    let (request_sender, request_receiver) = mpsc::channel::<DirectoryRequest>();
    let (network_request_sender, network_request_receiver) =
        mpsc::sync_channel::<DirectoryRequest>(network_worker_count.saturating_mul(32));
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
    spawn_network_directory_scheduler(
        network_worker_count,
        network_request_receiver,
        event_sender.clone(),
    );
    (request_sender, network_request_sender, event_receiver)
}

fn spawn_network_directory_scheduler(
    worker_count: usize,
    requests: mpsc::Receiver<DirectoryRequest>,
    events: mpsc::Sender<DirectoryEvent>,
) {
    let (work_sender, work_receiver) = mpsc::channel::<(NetworkExecutionKey, DirectoryRequest)>();
    let (completion_sender, completion_receiver) = mpsc::channel::<NetworkDirectoryCompletion>();
    let work_receiver = Arc::new(Mutex::new(work_receiver));
    for _ in 0..worker_count {
        let work_receiver = work_receiver.clone();
        let completion_sender = completion_sender.clone();
        let events = events.clone();
        thread::spawn(move || {
            loop {
                let work = work_receiver
                    .lock()
                    .expect("network directory work receiver mutex is not poisoned")
                    .recv();
                let Ok((key, request)) = work else {
                    break;
                };
                let slow_cancel = request.cancel.clone();
                let slow_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let slow_done_for_timer = slow_done.clone();
                let slow_events = events.clone();
                let slow_tab_id = request.tab_id;
                let slow_request_id = request.request_id;
                thread::spawn(move || {
                    let started = Instant::now();
                    while started.elapsed() < Duration::from_secs(2) {
                        if slow_cancel.load(std::sync::atomic::Ordering::Acquire)
                            || slow_done_for_timer.load(std::sync::atomic::Ordering::Acquire)
                        {
                            return;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    let _ = slow_events.send(DirectoryEvent::Slow {
                        tab_id: slow_tab_id,
                        request_id: slow_request_id,
                    });
                });
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_directory_request(request, &events);
                }));
                slow_done.store(true, std::sync::atomic::Ordering::Release);
                if outcome.is_err() {
                    let _ = events.send(DirectoryEvent::Failed {
                        tab_id: slow_tab_id,
                        request_id: slow_request_id,
                        kind: io::ErrorKind::Other,
                        message: "network directory worker failed unexpectedly".to_owned(),
                    });
                }
                let _ = completion_sender.send(NetworkDirectoryCompletion { key });
            }
        });
    }
    thread::spawn(move || {
        let mut scheduler = NetworkDirectoryScheduler::default();
        let mut input_open = true;
        while input_open || !scheduler.pending.is_empty() || !scheduler.active.is_empty() {
            while let Ok(completion) = completion_receiver.try_recv() {
                scheduler.complete(&completion.key);
            }
            for request in scheduler.take_cancelled() {
                let _ = events.send(DirectoryEvent::Cancelled {
                    tab_id: request.tab_id,
                    request_id: request.request_id,
                });
            }
            while scheduler.active.len() < worker_count {
                let Some(work) = scheduler.next_ready() else {
                    break;
                };
                if work_sender.send(work).is_err() {
                    return;
                }
            }
            if scheduler.active.len() >= worker_count || !scheduler.pending.is_empty() {
                match completion_receiver.recv_timeout(Duration::from_millis(20)) {
                    Ok(completion) => scheduler.complete(&completion.key),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            } else if input_open {
                match requests.recv_timeout(Duration::from_millis(20)) {
                    Ok(request) => {
                        if let Some(key) = NetworkExecutionKey::from_unc(&request.path) {
                            scheduler.push(key, request);
                        } else {
                            let _ = events.send(DirectoryEvent::Failed {
                                tab_id: request.tab_id,
                                request_id: request.request_id,
                                kind: io::ErrorKind::InvalidInput,
                                message: "network directory request requires a UNC path".to_owned(),
                            });
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => input_open = false,
                }
            }
            while let Ok(request) = requests.try_recv() {
                if let Some(key) = NetworkExecutionKey::from_unc(&request.path) {
                    scheduler.push(key, request);
                } else {
                    let _ = events.send(DirectoryEvent::Failed {
                        tab_id: request.tab_id,
                        request_id: request.request_id,
                        kind: io::ErrorKind::InvalidInput,
                        message: "network directory request requires a UNC path".to_owned(),
                    });
                }
            }
        }
    });
}

fn read_network_root_batches(
    request: &DirectoryRequest,
    events: &mpsc::Sender<DirectoryEvent>,
) -> io::Result<ReadOutcome> {
    use std::sync::atomic::Ordering;

    if request.cancel.load(Ordering::Acquire) {
        return Ok(ReadOutcome::Cancelled);
    }
    let items = platform::windows::network::isolated_network_root(&request.path, &request.cancel)?;
    if request.cancel.load(Ordering::Acquire) {
        return Ok(ReadOutcome::Cancelled);
    }
    for (batch_index, items) in items.chunks(32).enumerate() {
        if request.cancel.load(Ordering::Acquire) {
            return Ok(ReadOutcome::Cancelled);
        }
        let entries = items
            .iter()
            .enumerate()
            .map(|(index, item)| {
                let id = batch_index
                    .saturating_mul(32)
                    .saturating_add(index)
                    .saturating_add(1)
                    .min(u32::MAX as usize) as u32;
                FileEntry {
                    id: EntryId(id),
                    original_name: crate::network::unc_leaf_name(&item.target)
                        .unwrap_or_else(|| item.label.clone().into()),
                    display_name: item.label.clone(),
                    name_highlights: Vec::new(),
                    path: item.target.clone(),
                    kind: crate::domain::EntryKind::Directory,
                    open_target: None,
                    library_source_index: None,
                    parent_display: request.path.as_os_str().to_string_lossy().into_owned(),
                    size_bytes: None,
                    folder_size: FolderSizeState::NotIndexed,
                    modified: None,
                    created: None,
                }
            })
            .collect();
        let _ = events.send(DirectoryEvent::Batch {
            tab_id: request.tab_id,
            request_id: request.request_id,
            entries,
        });
    }
    Ok(ReadOutcome::Complete { skipped: 0 })
}

pub(super) fn deliver_network_directory_batch(
    request: &DirectoryRequest,
    events: &mpsc::Sender<DirectoryEvent>,
    entries: Vec<FileEntry>,
) -> io::Result<()> {
    if request.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let (acknowledgement, applied) = mpsc::channel();
    events
        .send(DirectoryEvent::NetworkBatch {
            tab_id: request.tab_id,
            request_id: request.request_id,
            entries,
            acknowledgement,
        })
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    loop {
        if request.cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        match applied.recv_timeout(Duration::from_millis(20)) {
            Ok(()) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
        }
    }
}

fn read_network_directory_batches(
    request: &DirectoryRequest,
    events: &mpsc::Sender<DirectoryEvent>,
) -> io::Result<ReadOutcome> {
    if request.cancelled() {
        return Ok(ReadOutcome::Cancelled);
    }
    let result = platform::windows::network::isolated_directory(
        &request.path,
        request.visibility,
        &request.cancel,
        |entries| deliver_network_directory_batch(request, events, entries),
    );
    if request.cancelled() {
        return Ok(ReadOutcome::Cancelled);
    }
    result.map(|skipped| ReadOutcome::Complete { skipped })
}
pub(super) fn run_directory_request(request: DirectoryRequest, events: &mpsc::Sender<DirectoryEvent>) {
    if let Some(sources) = request.library_sources.as_ref() {
        let outcome = read_aggregate_directory_batches_filtered(
            sources,
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
        let event = if outcome.cancelled {
            DirectoryEvent::Cancelled {
                tab_id: request.tab_id,
                request_id: request.request_id,
            }
        } else {
            let successful_sources = outcome
                .sources
                .iter()
                .filter(|source| matches!(source.state, SourceReadState::Complete { .. }))
                .count();
            let total_sources = outcome.sources.len() + request.unavailable_library_sources;
            if total_sources > 0 && successful_sources == 0 {
                let kind = if outcome
                    .sources
                    .iter()
                    .any(|source| source.state == SourceReadState::PermissionDenied)
                {
                    io::ErrorKind::PermissionDenied
                } else if outcome
                    .sources
                    .iter()
                    .all(|source| source.state == SourceReadState::NotFound)
                {
                    io::ErrorKind::NotFound
                } else {
                    io::ErrorKind::Other
                };
                DirectoryEvent::Failed {
                    tab_id: request.tab_id,
                    request_id: request.request_id,
                    kind,
                    message: "all library sources failed".to_owned(),
                }
            } else {
                let skipped = outcome
                    .sources
                    .iter()
                    .filter_map(|source| match source.state {
                        SourceReadState::Complete { skipped } => Some(skipped),
                        _ => None,
                    })
                    .sum();
                let source_failures = request.unavailable_library_sources
                    + outcome
                        .sources
                        .iter()
                        .filter(|source| !matches!(source.state, SourceReadState::Complete { .. }))
                        .count();
                DirectoryEvent::Finished {
                    tab_id: request.tab_id,
                    request_id: request.request_id,
                    path: PathBuf::new(),
                    skipped,
                    source_failures,
                    library: request.library,
                }
            }
        };
        let _ = events.send(event);
        return;
    }

    if crate::network::is_unc_server_root(&request.path) {
        platform::windows::network::record_runtime_event("network_root_request_started");
    }
    let result = if crate::network::is_unc_server_root(&request.path) {
        read_network_root_batches(&request, events)
    } else if crate::network::is_unc_path(&request.path) {
        read_network_directory_batches(&request, events)
    } else {
        read_directory_batches_filtered(
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
        )
    };
    if crate::network::is_unc_server_root(&request.path) {
        platform::windows::network::record_runtime_event(match &result {
            Ok(ReadOutcome::Complete { .. }) => "network_root_request_completed",
            Ok(ReadOutcome::Cancelled) => "network_root_request_cancelled",
            Err(_) => "network_root_request_failed",
        });
    }
    let event = match result {
        Ok(ReadOutcome::Complete { skipped }) => DirectoryEvent::Finished {
            tab_id: request.tab_id,
            request_id: request.request_id,
            path: request.path,
            skipped,
            source_failures: 0,
            library: None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    #[test]
    fn network_directory_scheduler_serializes_one_host_and_allows_another() {
        let mut scheduler = NetworkDirectoryScheduler::default();
        let server = NetworkExecutionKey::from_unc(Path::new(r"\\server\one")).unwrap();
        let other = NetworkExecutionKey::from_unc(Path::new(r"\\other\one")).unwrap();
        scheduler.push(server.clone(), network_directory_request(r"\\server\one"));
        scheduler.push(server.clone(), network_directory_request(r"\\server\two"));
        scheduler.push(other.clone(), network_directory_request(r"\\other\one"));

        let (first_key, _) = scheduler.next_ready().unwrap();
        assert_eq!(first_key, server);
        let (second_key, _) = scheduler.next_ready().unwrap();
        assert_eq!(second_key, other);
        assert!(scheduler.next_ready().is_none());

        scheduler.complete(&first_key);
        let (third_key, _) = scheduler.next_ready().unwrap();
        assert_eq!(third_key, server);
    }
    #[test]
    fn network_directory_scheduler_discards_cancelled_request() {
        let mut scheduler = NetworkDirectoryScheduler::default();
        let key = NetworkExecutionKey::from_unc(Path::new(r"\\server\one")).unwrap();
        let request = network_directory_request(r"\\server\one");
        request
            .cancel
            .store(true, std::sync::atomic::Ordering::Release);
        scheduler.push(key, request);
        let cancelled = scheduler.take_cancelled();
        assert_eq!(cancelled.len(), 1);
        assert!(scheduler.next_ready().is_none());
    }

    #[test]
    fn issue_103_network_backpressure_stops_on_cancellation_or_dropped_event() {
        for cancel_request in [false, true] {
            let request = network_directory_request(r"\\server\share");
            let cancel = request.cancel.clone();
            let (sender, receiver) = mpsc::channel();
            let worker = thread::spawn(move || {
                deliver_network_directory_batch(&request, &sender, Vec::new())
            });
            let event = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
            if cancel_request {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            } else {
                drop(event);
            }
            let error = worker.join().unwrap().unwrap_err();
            assert_eq!(
                error.kind(),
                if cancel_request {
                    io::ErrorKind::Interrupted
                } else {
                    io::ErrorKind::BrokenPipe
                }
            );
        }
    }
}
