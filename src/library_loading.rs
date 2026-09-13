use super::directory_loading::{DirectoryEvent, DirectoryRequest};
use crate::{
    domain::{EntryId, FileEntry},
    platform,
};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

const SOURCE_SLOTS: usize = 4;
pub(super) const POLL: Duration = Duration::from_millis(10);
pub(super) type SourceReader = fn(
    &Path,
    crate::domain::FileVisibility,
    &AtomicBool,
    &mut dyn FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize>;

struct Source {
    job: u64,
    index: usize,
    path: PathBuf,
    request: Arc<DirectoryRequest>,
}

struct Aggregate {
    request: Arc<DirectoryRequest>,
    remaining: usize,
    successes: usize,
    failures: usize,
    skipped: usize,
    error: io::ErrorKind,
    seen: HashSet<PathBuf>,
    next_id: u32,
}

impl Aggregate {
    fn batch(&mut self, source: usize, entries: Vec<FileEntry>) -> Vec<FileEntry> {
        entries
            .into_iter()
            .filter_map(|mut entry| {
                if !self.seen.insert(entry.path.clone()) {
                    return None;
                }
                entry.id = EntryId(self.next_id);
                self.next_id = self
                    .next_id
                    .checked_add(1)
                    .expect("library entry ID overflow");
                entry.library_source_index = Some(source);
                Some(entry)
            })
            .collect()
    }

    fn terminal(&self) -> DirectoryEvent {
        let request = &self.request;
        if request.cancelled() {
            DirectoryEvent::Cancelled {
                tab_id: request.tab_id,
                request_id: request.request_id,
            }
        } else if self.successes == 0 && self.failures > 0 && self.seen.is_empty() {
            DirectoryEvent::Failed {
                tab_id: request.tab_id,
                request_id: request.request_id,
                kind: self.error,
                message: "all library sources failed".to_owned(),
            }
        } else {
            DirectoryEvent::Finished {
                tab_id: request.tab_id,
                request_id: request.request_id,
                path: PathBuf::new(),
                skipped: self.skipped,
                source_failures: self.failures,
                library: request.library.clone(),
            }
        }
    }
}

enum SourceEvent {
    Batch {
        job: u64,
        index: usize,
        entries: Vec<FileEntry>,
        acknowledgement: mpsc::Sender<()>,
    },
    Done {
        job: u64,
        index: usize,
        network: bool,
        result: io::Result<usize>,
    },
}

pub(super) fn read_source(
    path: &Path,
    visibility: crate::domain::FileVisibility,
    cancel: &AtomicBool,
    on_batch: &mut dyn FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize> {
    // A local-looking library path may cross a mapped drive or reparse point into SMB.
    // Both resource domains therefore need process termination, not only UNC paths.
    platform::windows::network::isolated_directory(path, visibility, cancel, on_batch)
}

pub(super) fn start(
    events: mpsc::Sender<DirectoryEvent>,
    reader: SourceReader,
) -> (mpsc::Sender<DirectoryRequest>, thread::JoinHandle<()>) {
    let (sender, requests) = mpsc::channel::<DirectoryRequest>();
    let handle = thread::spawn(move || schedule(requests, events, reader));
    (sender, handle)
}

fn schedule(
    requests: mpsc::Receiver<DirectoryRequest>,
    events: mpsc::Sender<DirectoryEvent>,
    reader: SourceReader,
) {
    let (source_sender, source_events) = mpsc::sync_channel(SOURCE_SLOTS * 2);
    let mut jobs = HashMap::<u64, Aggregate>::new();
    let mut queues: [VecDeque<Source>; 2] = std::array::from_fn(|_| VecDeque::new());
    let mut active = [0_usize; 2];
    let mut workers = HashMap::new();
    let mut next_job = 0_u64;
    let mut input_open = true;
    loop {
        // Limit each ingress turn so a busy producer cannot starve source delivery or cancellation.
        for _ in 0..32 {
            if !input_open {
                break;
            }
            match requests.try_recv() {
                Ok(request) => {
                    next_job += 1;
                    let request = Arc::new(request);
                    let mut paths = HashSet::new();
                    let mut remaining = 0;
                    for (index, path) in request.library_sources.as_deref().unwrap_or_default() {
                        if paths.insert(path.clone()) {
                            queues[usize::from(crate::network::is_unc_path(path))].push_back(
                                Source {
                                    job: next_job,
                                    index: *index,
                                    path: path.clone(),
                                    request: request.clone(),
                                },
                            );
                            remaining += 1;
                        }
                    }
                    jobs.insert(
                        next_job,
                        Aggregate {
                            failures: request.unavailable_library_sources,
                            request,
                            remaining,
                            successes: 0,
                            skipped: 0,
                            error: io::ErrorKind::NotFound,
                            seen: HashSet::new(),
                            next_id: 1,
                        },
                    );
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    input_open = false;
                    for job in jobs.values() {
                        job.request.cancel.store(true, Ordering::Release);
                    }
                    break;
                }
            }
        }
        for (domain, queue) in queues.iter_mut().enumerate() {
            queue.retain(|source| {
                if source.request.cancelled() {
                    jobs.get_mut(&source.job).unwrap().remaining -= 1;
                    false
                } else {
                    true
                }
            });
            while active[domain] < SOURCE_SLOTS {
                let Some(source) = queue.pop_front() else {
                    break;
                };
                active[domain] += 1;
                let sender = source_sender.clone();
                let key = (source.job, source.index);
                let worker = thread::spawn(move || {
                    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        reader(
                            &source.path,
                            source.request.visibility,
                            &source.request.cancel,
                            &mut |entries| source_batch(&source, &sender, entries),
                        )
                    }))
                    .unwrap_or_else(|_| Err(io::Error::other("library source worker panicked")));
                    let _ = sender.send(SourceEvent::Done {
                        job: source.job,
                        index: source.index,
                        network: domain == 1,
                        result,
                    });
                });
                workers.insert(key, worker);
            }
        }
        let completed = jobs
            .iter()
            .filter_map(|(id, job)| (job.remaining == 0).then_some(*id))
            .collect::<Vec<_>>();
        for id in completed {
            let job = jobs.remove(&id).unwrap();
            if events.send(job.terminal()).is_err() {
                for job in jobs.values() {
                    job.request.cancel.store(true, Ordering::Release);
                }
                input_open = false;
            }
        }
        if !input_open && jobs.is_empty() {
            break;
        }
        match source_events.recv_timeout(POLL) {
            Ok(SourceEvent::Batch {
                job,
                index,
                entries,
                acknowledgement,
            }) => {
                if let Some(aggregate) = jobs.get_mut(&job)
                    && !aggregate.request.cancelled()
                {
                    let entries = aggregate.batch(index, entries);
                    if entries.is_empty() {
                        let _ = acknowledgement.send(());
                    } else if events
                        .send(DirectoryEvent::NetworkBatch {
                            tab_id: aggregate.request.tab_id,
                            request_id: aggregate.request.request_id,
                            entries,
                            acknowledgement,
                        })
                        .is_err()
                    {
                        aggregate.request.cancel.store(true, Ordering::Release);
                    }
                }
            }
            Ok(SourceEvent::Done {
                job,
                index,
                network,
                result,
            }) => {
                if let Some(worker) = workers.remove(&(job, index)) {
                    let _ = worker.join();
                }
                active[usize::from(network)] -= 1;
                if let Some(aggregate) = jobs.get_mut(&job) {
                    aggregate.remaining -= 1;
                    platform::windows::network::record_runtime_event(&format!(
                        "library_source_finished tab={} request={} source={index} network={network} result={result:?}",
                        aggregate.request.tab_id.0, aggregate.request.request_id.0,
                    ));
                    match result {
                        Ok(skipped) => {
                            aggregate.successes += 1;
                            aggregate.skipped += skipped;
                        }
                        Err(error) => {
                            aggregate.failures += 1;
                            if error.kind() == io::ErrorKind::PermissionDenied {
                                aggregate.error = io::ErrorKind::PermissionDenied;
                            } else if aggregate.error != io::ErrorKind::PermissionDenied
                                && error.kind() != io::ErrorKind::NotFound
                            {
                                aggregate.error = io::ErrorKind::Other;
                            }
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn source_batch(
    source: &Source,
    sender: &mpsc::SyncSender<SourceEvent>,
    entries: Vec<FileEntry>,
) -> io::Result<()> {
    if source.request.cancelled() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let (acknowledgement, applied) = mpsc::channel();
    sender
        .send(SourceEvent::Batch {
            job: source.job,
            index: source.index,
            entries,
            acknowledgement,
        })
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))?;
    loop {
        if source.request.cancelled() {
            return Err(io::ErrorKind::Interrupted.into());
        }
        match applied.recv_timeout(POLL) {
            Ok(()) => return Ok(()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
        }
    }
}

#[cfg(test)]
#[path = "library_loading_tests.rs"]
mod tests;
