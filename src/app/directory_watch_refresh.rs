use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::{AppState, DirectoryRequest, SharedSessions, submit_navigation};
use crate::{
    domain::{
        EntryKind, FileEntry, FileVisibility, LoadState, NavigationKind, PageSource, RequestId,
        TabId,
    },
    fs::read_path_entry,
    platform::windows::directory_watch::{DirectoryChange, DirectoryWatchEvent},
};

const COALESCE_DELAY: Duration = Duration::from_millis(120);
const REFRESH_IDLE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Default)]
struct RefreshGate(HashMap<WatchKey, Option<Instant>>);

impl RefreshGate {
    fn started(&mut self, key: WatchKey) {
        self.0.insert(key, None);
    }

    fn update(&mut self, app: &AppState, now: Instant) {
        self.0.retain(|(tab_id, root), ready_at| {
            let Some(tab) = app.tab(*tab_id).filter(|tab| {
                tab.page_source == PageSource::Directory
                    && tab.visible_path() == Some(root.as_path())
            }) else {
                return false;
            };
            if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) {
                return true;
            }
            let deadline = ready_at.get_or_insert(now + REFRESH_IDLE_INTERVAL);
            now < *deadline
        });
    }

    fn waiting(&self, key: &WatchKey) -> bool {
        self.0.contains_key(key)
    }
}

type WatchKey = (TabId, PathBuf);

#[derive(Clone, Debug)]
struct WatchBatch {
    root: PathBuf,
    paths: HashSet<PathBuf>,
    uncertain: bool,
}

#[derive(Clone, Debug)]
struct PendingWatch {
    tab_id: TabId,
    observed_request: RequestId,
    batch: WatchBatch,
}

impl PendingWatch {
    fn key(&self) -> WatchKey {
        (self.tab_id, self.batch.root.clone())
    }
}

struct Probe {
    pending: PendingWatch,
    request_id: RequestId,
    visibility: FileVisibility,
    entries: Arc<Vec<FileEntry>>,
}

struct ProbeResult {
    probe: Probe,
    changed: bool,
}

struct Delivery {
    key: WatchKey,
    retry: Option<PendingWatch>,
    refreshed: bool,
}

struct CompletedSample {
    root: PathBuf,
    results: Vec<ProbeResult>,
}

fn spawn_probe_sample(
    root: PathBuf,
    probes: Vec<Probe>,
    sender: mpsc::Sender<CompletedSample>,
    sample: impl FnOnce(Vec<Probe>) -> Vec<ProbeResult> + Send + 'static,
) {
    thread::spawn(move || {
        let results = sample(probes);
        let _ = sender.send(CompletedSample { root, results });
    });
}

fn probes_by_root(probes: Vec<Probe>) -> HashMap<PathBuf, Vec<Probe>> {
    let mut roots: HashMap<PathBuf, Vec<Probe>> = HashMap::new();
    for probe in probes {
        roots
            .entry(probe.pending.batch.root.clone())
            .or_default()
            .push(probe);
    }
    roots
}

fn start_probe_samples(
    probes: Vec<Probe>,
    sampling_roots: &mut HashSet<PathBuf>,
    sender: &mpsc::Sender<CompletedSample>,
    state: &SharedSessions,
) {
    for (root, probes) in probes_by_root(probes) {
        assert!(sampling_roots.insert(root.clone()));
        let state = state.clone();
        spawn_probe_sample(root, probes, sender.clone(), move |probes| {
            probe_changes_with(
                probes,
                |probe| probe_is_current(&state, probe),
                read_path_entry,
            )
        });
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Disposition {
    Drop,
    Wait,
    DeferredOperation,
    Probe,
}

fn coalesce(events: Vec<DirectoryWatchEvent>) -> Vec<WatchBatch> {
    let mut batches: HashMap<PathBuf, WatchBatch> = HashMap::new();
    for event in events {
        let (root, changes, uncertain) = match event {
            DirectoryWatchEvent::Changes { root, changes } => (root, changes, false),
            DirectoryWatchEvent::Overflow { root } | DirectoryWatchEvent::Error { root, .. } => {
                (root, Vec::new(), true)
            }
        };
        let batch = batches.entry(root.clone()).or_insert_with(|| WatchBatch {
            root,
            paths: HashSet::new(),
            uncertain: false,
        });
        batch.uncertain |= uncertain;
        for change in changes {
            match change {
                DirectoryChange::Added(path)
                | DirectoryChange::Removed(path)
                | DirectoryChange::Modified(path) => {
                    batch.paths.insert(path);
                }
                DirectoryChange::Renamed { from, to } => {
                    batch.paths.insert(from);
                    batch.paths.insert(to);
                }
            }
        }
    }
    batches
        .into_values()
        .filter_map(|mut batch| {
            batch
                .paths
                .retain(|path| !crate::fs::is_directory_work_path(path));
            (batch.uncertain || !batch.paths.is_empty()).then_some(batch)
        })
        .collect()
}

fn merge_pending(queue: &mut HashMap<WatchKey, PendingWatch>, pending: PendingWatch) {
    if let Some(existing) = queue.get_mut(&pending.key()) {
        existing.observed_request = existing.observed_request.max(pending.observed_request);
        existing.batch.paths.extend(pending.batch.paths);
        existing.batch.uncertain |= pending.batch.uncertain;
    } else {
        queue.insert(pending.key(), pending);
    }
}

fn rename_is_active(app: &AppState, root: &Path) -> bool {
    app.operations.iter().any(|task| {
        task.kind == crate::domain::file_operations::FileOperationKind::Rename
            && task.state.is_active()
            && task.items.iter().any(|item| {
                [item.source.as_deref(), item.destination.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|path| path.parent() == Some(root))
            })
    })
}
fn disposition(app: &AppState, pending: &PendingWatch) -> Disposition {
    let Some(tab) = app.tab(pending.tab_id) else {
        return Disposition::Drop;
    };
    if !tab.accepts(tab.latest_request())
        || tab.page_source != PageSource::Directory
        || tab.visible_path() != Some(pending.batch.root.as_path())
        || (tab.latest_request() != pending.observed_request
            && matches!(tab.load_state, LoadState::Loading | LoadState::Partial)
            && !matches!(
                tab.navigation_kind(),
                NavigationKind::Refresh | NavigationKind::BackgroundRefresh
            ))
    {
        return Disposition::Drop;
    }
    if app
        .active_operation_directories
        .contains_key(&pending.batch.root)
    {
        return if rename_is_active(app, &pending.batch.root) {
            Disposition::Wait
        } else {
            Disposition::DeferredOperation
        };
    }
    if matches!(tab.load_state, LoadState::Loading | LoadState::Partial) {
        return Disposition::Wait;
    }
    Disposition::Probe
}

fn prepare_probe(app: &AppState, pending: PendingWatch) -> Probe {
    let tab = app.tab(pending.tab_id).expect("pending tab was checked");
    Probe {
        request_id: tab.latest_request(),
        visibility: app.file_visibility,
        entries: tab.entries.clone(),
        pending,
    }
}

fn same_entry(previous: Option<&FileEntry>, current: Option<&FileEntry>) -> bool {
    match (previous, current) {
        (None, None) => true,
        (Some(previous), Some(current)) => {
            let original_kind = if previous.open_target.is_some() {
                EntryKind::File
            } else {
                previous.kind
            };
            previous.path == current.path
                && previous.original_name == current.original_name
                && original_kind == current.kind
                && previous.size_bytes == current.size_bytes
                && previous.modified == current.modified
                && previous.created == current.created
        }
        _ => false,
    }
}

fn probe_is_current(state: &SharedSessions, probe: &Probe) -> bool {
    state.lock().is_ok_and(|app| {
        disposition(&app, &probe.pending) == Disposition::Probe
            && app
                .tab(probe.pending.tab_id)
                .is_some_and(|tab| tab.latest_request() == probe.request_id)
            && app.file_visibility == probe.visibility
    })
}

#[cfg(test)]
fn probe_changes(probes: Vec<Probe>) -> Vec<ProbeResult> {
    probe_changes_with(probes, |_| true, read_path_entry)
}

fn probe_changes_with(
    probes: Vec<Probe>,
    mut still_needed: impl FnMut(&Probe) -> bool,
    mut read: impl FnMut(&Path, FileVisibility, u32) -> std::io::Result<Option<FileEntry>>,
) -> Vec<ProbeResult> {
    let mut results = Vec::new();
    for probes in probes_by_root(probes).into_values() {
        let paths = probes
            .iter()
            .flat_map(|probe| probe.pending.batch.paths.iter())
            .collect::<HashSet<_>>();
        let visibility = probes[0].visibility;
        // Same-directory tabs share the disk sample; identities remain raw paths.
        let current = paths
            .into_iter()
            .filter(|path| {
                probes
                    .iter()
                    .any(|probe| probe.pending.batch.paths.contains(*path) && still_needed(probe))
            })
            .map(|path| (path.clone(), read(path, visibility, 0).map_err(|_| ())))
            .collect::<HashMap<_, _>>();
        for probe in probes {
            let previous = probe
                .entries
                .iter()
                .filter(|entry| probe.pending.batch.paths.contains(&entry.path))
                .map(|entry| (&entry.path, entry))
                .collect::<HashMap<_, _>>();
            let changed = probe.pending.batch.uncertain
                || probe
                    .pending
                    .batch
                    .paths
                    .iter()
                    .any(|path| match current.get(path) {
                        Some(Ok(entry)) => !same_entry(previous.get(path).copied(), entry.as_ref()),
                        Some(Err(())) | None => true,
                    });
            results.push(ProbeResult { probe, changed });
        }
    }
    results
}

fn apply_probe_result(
    state: &SharedSessions,
    sender: &mpsc::Sender<DirectoryRequest>,
    result: ProbeResult,
) -> Option<PendingWatch> {
    let ProbeResult { probe, changed } = result;
    let pending = probe.pending;
    {
        let Ok(mut app) = state.lock() else {
            return None;
        };
        match disposition(&app, &pending) {
            Disposition::Drop => return None,
            Disposition::DeferredOperation => {
                app.deferred_watch_directories
                    .insert(pending.batch.root.clone());
                return None;
            }
            Disposition::Wait => return Some(pending),
            Disposition::Probe => {}
        }
        let tab = app.tab(pending.tab_id).expect("pending tab was checked");
        // A completed rename may change entries without replacing the directory request.
        if tab.latest_request() != probe.request_id
            || app.file_visibility != probe.visibility
            || !Arc::ptr_eq(&tab.entries, &probe.entries)
        {
            return Some(pending);
        }
        crate::operation_audit::record(
            "watch_refresh",
            format!(
                "tab={:?} request={:?} root={:?} paths={} decision={}",
                pending.tab_id,
                probe.request_id,
                pending.batch.root,
                pending.batch.paths.len(),
                if changed { "refresh" } else { "unchanged" }
            ),
        );
    }
    if changed
        && submit_navigation(
            sender,
            state,
            pending.tab_id,
            pending.batch.root,
            NavigationKind::BackgroundRefresh,
            None,
        )
    {
        super::refresh_tab_window(state, pending.tab_id);
    }
    None
}

pub(super) fn start_watch_refresh_pump(
    receiver: mpsc::Receiver<DirectoryWatchEvent>,
    directory_sender: mpsc::Sender<DirectoryRequest>,
    state: SharedSessions,
    failed_roots: Arc<Mutex<HashSet<PathBuf>>>,
) {
    thread::spawn(move || {
        let (delivery_sender, delivery_receiver) = mpsc::channel::<Delivery>();
        let (sample_sender, sample_receiver) = mpsc::channel::<CompletedSample>();
        let mut pending = HashMap::new();
        let mut in_flight = HashSet::new();
        let mut sampling_roots = HashSet::new();
        let mut refresh_gate = RefreshGate::default();
        loop {
            let mut events = match receiver.recv_timeout(COALESCE_DELAY) {
                Ok(first) => {
                    thread::sleep(COALESCE_DELAY);
                    vec![first]
                }
                Err(mpsc::RecvTimeoutError::Timeout) => Vec::new(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            events.extend(receiver.try_iter());
            for event in &events {
                if let DirectoryWatchEvent::Error { root, message } = event {
                    eprintln!("directory watch failed for {}: {message}", root.display());
                    if let Ok(mut failed) = failed_roots.lock() {
                        failed.insert(root.clone());
                    }
                }
            }
            for delivery in delivery_receiver.try_iter() {
                in_flight.remove(&delivery.key);
                if delivery.refreshed {
                    refresh_gate.started(delivery.key);
                }
                if let Some(retry) = delivery.retry {
                    merge_pending(&mut pending, retry);
                }
            }
            for sample in sample_receiver.try_iter() {
                sampling_roots.remove(&sample.root);
                for result in sample.results {
                    let sender = directory_sender.clone();
                    let state = state.clone();
                    let delivered = delivery_sender.clone();
                    if slint::invoke_from_event_loop(move || {
                        let key = result.probe.pending.key();
                        let request_id = result.probe.request_id;
                        let changed = result.changed;
                        let retry = apply_probe_result(&state, &sender, result);
                        let refreshed = changed
                            && retry.is_none()
                            && state.lock().ok().is_some_and(|app| {
                                app.tab(key.0).is_some_and(|tab| {
                                    tab.latest_request() != request_id
                                        && tab.navigation_kind()
                                            == NavigationKind::BackgroundRefresh
                                })
                            });
                        let _ = delivered.send(Delivery {
                            key,
                            retry,
                            refreshed,
                        });
                    })
                    .is_err()
                    {
                        return;
                    }
                }
            }
            let batches = coalesce(events);
            let probes = {
                let Ok(mut app) = state.lock() else { break };
                refresh_gate.update(&app, Instant::now());
                for batch in batches {
                    if app.active_operation_directories.contains_key(&batch.root)
                        && !rename_is_active(&app, &batch.root)
                    {
                        app.deferred_watch_directories.insert(batch.root);
                        continue;
                    }
                    for tab in app.windows.values().flat_map(|window| window.tabs.values()) {
                        if tab.page_source == PageSource::Directory
                            && tab.visible_path() == Some(batch.root.as_path())
                        {
                            merge_pending(
                                &mut pending,
                                PendingWatch {
                                    tab_id: tab.id,
                                    observed_request: tab.latest_request(),
                                    batch: batch.clone(),
                                },
                            );
                        }
                    }
                }
                let mut ready = Vec::new();
                let mut waiting = HashMap::new();
                for (key, watch) in pending.drain() {
                    match disposition(&app, &watch) {
                        Disposition::Drop => {}
                        Disposition::DeferredOperation => {
                            app.deferred_watch_directories.insert(watch.batch.root);
                        }
                        Disposition::Wait => {
                            waiting.insert(key, watch);
                        }
                        Disposition::Probe
                            if refresh_gate.waiting(&key)
                                || in_flight.contains(&key)
                                || sampling_roots.contains(&watch.batch.root) =>
                        {
                            waiting.insert(key, watch);
                        }
                        Disposition::Probe => {
                            in_flight.insert(key);
                            ready.push(prepare_probe(&app, watch));
                        }
                    }
                }
                pending = waiting;
                ready
            };
            start_probe_samples(probes, &mut sampling_roots, &sample_sender, &state);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{DirectoryViewPreference, EntryId, NavigationLocation};
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "asterfiles-watch-133-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            Self(root)
        }
        fn entry(&self, name: &str, id: u32) -> FileEntry {
            read_path_entry(&self.0.join(name), FileVisibility::default(), id)
                .unwrap()
                .unwrap()
        }
        fn state(&self, entries: Vec<FileEntry>) -> SharedSessions {
            let mut app = AppState::new_for_test(vec![self.0.clone()], 0, [0, 1, 2, 3]);
            let tab = app.tab_mut(TabId(1)).unwrap();
            tab.replace_entries(entries);
            tab.load_state = LoadState::Complete;
            tab.selected = vec![EntryId(1)].into();
            tab.focused = Some(EntryId(1));
            Arc::new(Mutex::new(app))
        }
        fn pending(&self, paths: &[&str]) -> PendingWatch {
            PendingWatch {
                tab_id: TabId(1),
                observed_request: RequestId(0),
                batch: WatchBatch {
                    root: self.0.clone(),
                    uncertain: false,
                    paths: paths.iter().map(|name| self.0.join(name)).collect(),
                },
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn run_probe(state: &SharedSessions, pending: PendingWatch) -> ProbeResult {
        let probe = prepare_probe(&state.lock().unwrap(), pending);
        probe_changes(vec![probe]).pop().unwrap()
    }

    #[test]
    fn issue_176_work_notifications_are_ignored_but_external_renames_survive() {
        let root = std::env::temp_dir();
        let work = root.join(crate::fs::DIRECTORY_WORK_ROOT);
        let batch = |changes| {
            coalesce(vec![DirectoryWatchEvent::Changes {
                root: root.clone(),
                changes,
            }])
        };
        assert!(
            batch(vec![
                DirectoryChange::Added(work.clone()),
                DirectoryChange::Modified(work.clone()),
                DirectoryChange::Removed(work.join("batch"))
            ])
            .is_empty()
        );
        for (from, to) in [
            (work.clone(), root.join("outside")),
            (root.join("outside"), work.clone()),
        ] {
            let batches = batch(vec![DirectoryChange::Renamed { from, to }]);
            assert_eq!(batches.len(), 1);
            assert_eq!(batches[0].paths, HashSet::from([root.join("outside")]));
        }
        let similar = root.join(".asterfiles-directory-other");
        assert_eq!(
            batch(vec![DirectoryChange::Added(similar.clone())])[0].paths,
            HashSet::from([similar])
        );
    }

    #[test]
    fn issue_176_native_work_notifications_do_not_request_refresh() {
        use crate::platform::windows::directory_watch::DirectoryWatch;

        let fixture = Fixture::new();
        let sentinel = fixture.0.join("watch-ready.txt");
        let (sender, receiver) = mpsc::channel();
        let mut watcher = DirectoryWatch::start(&fixture.0, sender).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut attempt = 0;
        loop {
            fs::write(&sentinel, attempt.to_string()).unwrap();
            attempt += 1;
            match receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(DirectoryWatchEvent::Changes { changes, .. })
                    if changes.iter().any(|change| match change {
                        DirectoryChange::Added(path)
                        | DirectoryChange::Removed(path)
                        | DirectoryChange::Modified(path) => path == &sentinel,
                        DirectoryChange::Renamed { from, to } => {
                            from == &sentinel || to == &sentinel
                        }
                    }) =>
                {
                    break;
                }
                Ok(event) => panic!("unexpected readiness event: {event:?}"),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(
                        Instant::now() < deadline,
                        "native watcher did not become ready"
                    );
                }
                Err(error) => panic!("native watcher disconnected: {error}"),
            }
        }
        // A readiness write can produce several native deliveries; none belongs to the probe.
        let receive_changes = |deadline: Instant| loop {
            let event = receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("native watcher did not deliver the expected change");
            let DirectoryWatchEvent::Changes { root, mut changes } = event else {
                panic!("native watcher failed: {event:?}");
            };
            assert_eq!(root, fixture.0);
            changes.retain(|change| match change {
                DirectoryChange::Added(path)
                | DirectoryChange::Removed(path)
                | DirectoryChange::Modified(path) => path != &sentinel,
                DirectoryChange::Renamed { from, to } => from != &sentinel && to != &sentinel,
            });
            if !changes.is_empty() {
                return DirectoryWatchEvent::Changes { root, changes };
            }
        };
        let work = fixture.0.join(crate::fs::DIRECTORY_WORK_ROOT);
        fs::create_dir(&work).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut internal_events = Vec::new();
        loop {
            let event = receive_changes(deadline);
            let added = matches!(&event, DirectoryWatchEvent::Changes { changes, .. }
                if changes.contains(&DirectoryChange::Added(work.clone())));
            internal_events.push(event);
            if added {
                break;
            }
        }
        let child = work.join("asterfiles-directory-probe");
        fs::create_dir(&child).unwrap();
        let input = child.join("input");
        fs::write(&input, b"initial").unwrap();
        fs::write(&input, b"modified protocol content").unwrap();
        fs::remove_file(&input).unwrap();
        fs::remove_dir(&child).unwrap();
        fs::remove_dir(&work).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let event = receive_changes(deadline);
            let removed = matches!(&event, DirectoryWatchEvent::Changes { changes, .. }
                if changes.contains(&DirectoryChange::Removed(work.clone())));
            internal_events.push(event);
            if removed {
                break;
            }
        }
        assert!(coalesce(internal_events).is_empty());

        let external = fixture.0.join("external.txt");
        fs::write(&external, b"external change").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut external_events = Vec::new();
        loop {
            let event = receive_changes(deadline);
            let added = matches!(&event, DirectoryWatchEvent::Changes { changes, .. }
                if changes.contains(&DirectoryChange::Added(external.clone())));
            external_events.push(event);
            if added {
                break;
            }
        }
        let mut batches = coalesce(external_events);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paths, HashSet::from([external]));
        let state = fixture.state(Vec::new());
        let result = run_probe(
            &state,
            PendingWatch {
                tab_id: TabId(1),
                observed_request: RequestId(0),
                batch: batches.pop().unwrap(),
            },
        );
        assert!(result.changed);
        let (sender, requests) = mpsc::channel();
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(requests.try_recv().is_ok());
        watcher.cancel();
    }

    #[test]
    fn issue_176_busy_notifications_merge_and_wait_after_refresh_completion() {
        let fixture = Fixture::new();
        let state = fixture.state(vec![]);
        let mut pending = HashMap::new();
        let mut gate = RefreshGate::default();
        let now = Instant::now();
        let key = fixture.pending(&[]).key();
        gate.started(key.clone());
        let request = state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.clone(), NavigationKind::Refresh)
            .0;
        for _ in 0..100 {
            merge_pending(&mut pending, fixture.pending(&["a", "b"]));
        }
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[&key].batch.paths.len(), 2);
        gate.update(&state.lock().unwrap(), now + Duration::from_secs(30));
        assert!(gate.waiting(&key));
        assert_eq!(
            disposition(&state.lock().unwrap(), &pending[&key]),
            Disposition::Wait
        );
        assert!(
            state
                .lock()
                .unwrap()
                .tab_mut(TabId(1))
                .unwrap()
                .finish_directory_navigation(
                    request,
                    NavigationLocation::Directory(fixture.0.clone()),
                    DirectoryViewPreference::default(),
                    None
                )
        );
        gate.update(&state.lock().unwrap(), now + Duration::from_secs(31));
        gate.update(&state.lock().unwrap(), now + Duration::from_millis(31_999));
        assert!(gate.waiting(&key));
        gate.update(&state.lock().unwrap(), now + Duration::from_secs(32));
        assert!(!gate.waiting(&key));
        // Changes already reflected in the committed snapshot do not cause another refresh.
        assert!(!run_probe(&state, pending.remove(&key).unwrap()).changed);
        gate.started(key.clone());
        state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.join("elsewhere"), NavigationKind::Normal);
        gate.update(&state.lock().unwrap(), now);
        assert!(!gate.waiting(&key));
    }

    #[test]
    fn issue_133_completed_copy_notifications_do_not_clear_selection() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join(".asterfiles-copy-9"), b"copied").unwrap();
        fs::rename(
            fixture.0.join(".asterfiles-copy-9"),
            fixture.0.join("finished.txt"),
        )
        .unwrap();
        let state = fixture.state(vec![fixture.entry("finished.txt", 1)]);
        let events = vec![
            DirectoryWatchEvent::Changes {
                root: fixture.0.clone(),
                changes: vec![
                    DirectoryChange::Added(fixture.0.join(".asterfiles-copy-9")),
                    DirectoryChange::Modified(fixture.0.join(".asterfiles-copy-9")),
                ],
            },
            DirectoryWatchEvent::Changes {
                root: fixture.0.clone(),
                changes: vec![DirectoryChange::Renamed {
                    from: fixture.0.join(".asterfiles-copy-9"),
                    to: fixture.0.join("finished.txt"),
                }],
            },
        ];
        let batches = coalesce(events);
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].paths.len(), 2);
        let pending = PendingWatch {
            batch: batches.into_iter().next().unwrap(),
            ..fixture.pending(&[])
        };
        let (sender, receiver) = mpsc::channel();
        let result = run_probe(&state, pending);
        assert!(!result.changed);
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(receiver.try_recv().is_err());
        let app = state.lock().unwrap();
        assert_eq!(app.tab(TabId(1)).unwrap().selected, vec![EntryId(1)]);
        assert_eq!(app.tab(TabId(1)).unwrap().focused, Some(EntryId(1)));
    }

    #[test]
    fn issue_133_copy_refresh_then_delayed_watch_preserves_the_committed_selection() {
        use super::super::{
            DirectoryEvent, apply_event, queue_completed_focus, refresh_affected_tabs,
        };

        let fixture = Fixture::new();
        let state = fixture.state(Vec::new());
        let temporary = fixture.0.join(".asterfiles-copy-17");
        let completed = fixture.0.join("finished.txt");
        fs::write(&temporary, b"copied contents").unwrap();
        fs::rename(&temporary, &completed).unwrap();
        let batch = coalesce(vec![DirectoryWatchEvent::Changes {
            root: fixture.0.clone(),
            changes: vec![
                DirectoryChange::Added(temporary.clone()),
                DirectoryChange::Modified(temporary.clone()),
                DirectoryChange::Renamed {
                    from: temporary,
                    to: completed.clone(),
                },
            ],
        }])
        .pop()
        .unwrap();
        let pending = PendingWatch {
            batch,
            ..fixture.pending(&[])
        };
        queue_completed_focus(&mut state.lock().unwrap(), &[completed]);
        let (sender, receiver) = mpsc::channel();
        let (network_sender, _network_receiver) = mpsc::sync_channel(1);
        refresh_affected_tabs(
            &sender,
            &network_sender,
            &state,
            std::slice::from_ref(&fixture.0),
        );
        let request = receiver.try_recv().unwrap();
        {
            let app = state.lock().unwrap();
            assert_eq!(disposition(&app, &pending), Disposition::Wait);
            assert!(app.focus_after_refresh.contains_key(&TabId(1)));
        }
        apply_event(
            &state,
            DirectoryEvent::Batch {
                acknowledgement: mpsc::channel().0,
                tab_id: request.tab_id,
                request_id: request.request_id,
                entries: vec![fixture.entry("finished.txt", 1)],
            },
        );
        {
            let app = state.lock().unwrap();
            assert_eq!(disposition(&app, &pending), Disposition::Wait);
            assert!(app.focus_after_refresh.contains_key(&TabId(1)));
        }
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: request.tab_id,
                request_id: request.request_id,
                path: fixture.0.clone(),
                skipped: 0,
                source_failures: 0,
                library: None,
            },
        );
        let committed = {
            let app = state.lock().unwrap();
            let tab = app.tab(TabId(1)).unwrap();
            assert_eq!(tab.latest_request(), request.request_id);
            assert_eq!(tab.selected, vec![EntryId(1)]);
            assert_eq!(tab.focused, Some(EntryId(1)));
            assert_eq!(tab.selection_anchor, Some(EntryId(1)));
            assert!(!app.focus_after_refresh.contains_key(&TabId(1)));
            tab.entries.clone()
        };
        let result = run_probe(&state, pending);
        assert!(!result.changed);
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(receiver.try_recv().is_err());
        let app = state.lock().unwrap();
        let tab = app.tab(TabId(1)).unwrap();
        assert_eq!(tab.latest_request(), request.request_id);
        assert_eq!(tab.selected, vec![EntryId(1)]);
        assert_eq!(tab.focused, Some(EntryId(1)));
        assert_eq!(tab.selection_anchor, Some(EntryId(1)));
        assert!(Arc::ptr_eq(&committed, &tab.entries));
    }

    #[test]
    fn issue_133_resolved_folder_shortcuts_compare_the_original_file_metadata() {
        let fixture = Fixture::new();
        let original = fixture.0.join("folder.lnk");
        fs::write(&original, b"shortcut metadata").unwrap();
        let mut entry = fixture.entry("folder.lnk", 1);
        assert!(entry.set_shortcut_target(EntryId(1), &original, fixture.0.clone(), Some(true)));
        let state = fixture.state(vec![entry]);
        let pending = fixture.pending(&["folder.lnk"]);
        assert!(!run_probe(&state, pending.clone()).changed);
        fs::write(&original, b"changed shortcut file contents").unwrap();
        assert!(run_probe(&state, pending).changed);
    }

    #[test]
    fn issue_133_repeated_notifications_have_no_expiration_window() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("finished.txt"), b"copied").unwrap();
        let state = fixture.state(vec![fixture.entry("finished.txt", 1)]);
        let pending = fixture.pending(&["finished.txt", "temporary-now-absent"]);
        assert!(!run_probe(&state, pending.clone()).changed);
        thread::sleep(Duration::from_millis(2100));
        assert!(!run_probe(&state, pending).changed);
    }

    #[test]
    fn issue_133_loading_keeps_notifications_until_the_committed_snapshot_is_ready() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("finished.txt"), b"copied").unwrap();
        let state = fixture.state(Vec::new());
        let pending = fixture.pending(&["finished.txt", ".asterfiles-copy-9"]);
        {
            let mut app = state.lock().unwrap();
            let tab = app.tab_mut(TabId(1)).unwrap();
            tab.begin_directory_navigation(fixture.0.clone(), NavigationKind::Refresh);
            assert_eq!(disposition(&app, &pending), Disposition::Wait);
            let tab = app.tab_mut(TabId(1)).unwrap();
            assert!(tab.accept_directory_batch(
                tab.latest_request(),
                vec![fixture.entry("finished.txt", 1)],
                DirectoryViewPreference::default()
            ));
            assert_eq!(disposition(&app, &pending), Disposition::Wait);
            let tab = app.tab_mut(TabId(1)).unwrap();
            assert!(tab.finish_directory_navigation(
                tab.latest_request(),
                NavigationLocation::Directory(fixture.0.clone()),
                DirectoryViewPreference::default(),
                None
            ));
            tab.selected = vec![EntryId(1)].into();
            tab.focused = Some(EntryId(1));
        }
        assert!(!run_probe(&state, pending).changed);
    }

    #[test]
    fn issue_133_new_navigation_notifications_survive_an_older_pending_retry() {
        let fixture = Fixture::new();
        let state = fixture.state(Vec::new());
        let old = fixture.pending(&["old.txt"]);
        let mut queue = HashMap::new();
        merge_pending(&mut queue, old.clone());
        let request_id = state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.clone(), NavigationKind::Normal)
            .0;
        let mut current = fixture.pending(&["new.txt"]);
        current.observed_request = request_id;
        merge_pending(&mut queue, current.clone());
        merge_pending(&mut queue, old);
        let pending = queue.get(&current.key()).unwrap();
        assert_eq!(pending.observed_request, request_id);
        assert_eq!(pending.batch.paths.len(), 2);
        assert_eq!(
            disposition(&state.lock().unwrap(), pending),
            Disposition::Wait
        );

        let refresh_id = state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.clone(), NavigationKind::Refresh)
            .0;
        let mut during_refresh = fixture.pending(&["during-refresh.txt"]);
        during_refresh.observed_request = refresh_id;
        merge_pending(&mut queue, during_refresh);
        merge_pending(&mut queue, current.clone());
        let pending = queue.get(&current.key()).unwrap();
        assert_eq!(pending.observed_request, refresh_id);
        assert_eq!(pending.batch.paths.len(), 3);
        assert_eq!(
            disposition(&state.lock().unwrap(), pending),
            Disposition::Wait
        );
    }

    #[test]
    fn issue_133_stale_samples_retry_and_navigation_discards_them() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("finished.txt"), b"copied").unwrap();
        let state = fixture.state(vec![fixture.entry("finished.txt", 1)]);
        let pending = fixture.pending(&["external.txt"]);
        let result = run_probe(&state, pending.clone());
        let (sender, receiver) = mpsc::channel();
        {
            let mut app = state.lock().unwrap();
            let tab = app.tab_mut(TabId(1)).unwrap();
            let entries = tab.entries.as_ref().clone();
            let (request, _) =
                tab.begin_directory_navigation(fixture.0.clone(), NavigationKind::Refresh);
            assert!(tab.accept_directory_batch(
                request,
                entries,
                DirectoryViewPreference::default()
            ));
            assert!(tab.finish_directory_navigation(
                request,
                NavigationLocation::Directory(fixture.0.clone()),
                DirectoryViewPreference::default(),
                None
            ));
        }
        assert!(apply_probe_result(&state, &sender, result).is_some());
        assert!(receiver.try_recv().is_err());
        let mut app = state.lock().unwrap();
        app.tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.join("elsewhere"), NavigationKind::Normal);
        assert_eq!(disposition(&app, &pending), Disposition::Drop);
        app.active_window_state_mut().tabs.remove(&TabId(1));
        assert_eq!(disposition(&app, &pending), Disposition::Drop);
    }

    #[test]
    fn issue_133_real_external_changes_mixed_with_copy_echo_require_refresh() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("finished.txt"), b"copied").unwrap();
        let state = fixture.state(vec![fixture.entry("finished.txt", 1)]);
        fs::write(fixture.0.join("external.txt"), b"external").unwrap();
        let result = run_probe(
            &state,
            fixture.pending(&[".asterfiles-copy-9", "finished.txt", "external.txt"]),
        );
        assert!(result.changed);
        let (sender, receiver) = mpsc::channel();
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert_eq!(receiver.try_recv().unwrap().tab_id, TabId(1));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn issue_133_existing_temporary_named_files_are_not_hidden() {
        let fixture = Fixture::new();
        let state = fixture.state(Vec::new());
        fs::write(fixture.0.join(".asterfiles-copy-user"), b"keep visible").unwrap();
        assert!(run_probe(&state, fixture.pending(&[".asterfiles-copy-user"])).changed);
    }

    #[test]
    fn issue_133_unknown_notifications_and_metadata_changes_require_refresh() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("finished.txt"), b"copied").unwrap();
        let state = fixture.state(vec![fixture.entry("finished.txt", 1)]);
        let mut pending = fixture.pending(&[]);
        pending.batch.uncertain = true;
        assert!(run_probe(&state, pending).changed);
        fs::write(fixture.0.join("finished.txt"), b"changed contents").unwrap();
        assert!(run_probe(&state, fixture.pending(&["finished.txt"])).changed);
        fs::remove_file(fixture.0.join("finished.txt")).unwrap();
        assert!(run_probe(&state, fixture.pending(&["finished.txt"])).changed);
    }

    #[test]
    fn issue_133_unreadable_paths_request_a_refresh() {
        let fixture = Fixture::new();
        let state = fixture.state(Vec::new());
        assert!(run_probe(&state, fixture.pending(&["invalid\0name"])).changed);
    }

    #[test]
    fn issue_133_a_blocked_root_does_not_block_other_samples_or_new_notifications() {
        let blocked = Fixture::new();
        let available = Fixture::new();
        fs::write(available.0.join("external.txt"), b"new").unwrap();
        let blocked_state = blocked.state(Vec::new());
        let available_state = available.state(Vec::new());
        let blocked_probe = prepare_probe(
            &blocked_state.lock().unwrap(),
            blocked.pending(&["slow.txt"]),
        );
        let available_probe = prepare_probe(
            &available_state.lock().unwrap(),
            available.pending(&["external.txt"]),
        );
        let (sample_sender, sample_receiver) = mpsc::channel();
        let (started_sender, started_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let mut sampling_roots = HashSet::from([blocked.0.clone()]);
        spawn_probe_sample(
            blocked.0.clone(),
            vec![blocked_probe],
            sample_sender.clone(),
            move |probes| {
                started_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                probe_changes(probes)
            },
        );
        started_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        start_probe_samples(
            vec![available_probe],
            &mut sampling_roots,
            &sample_sender,
            &available_state,
        );
        let mut pending = HashMap::new();
        merge_pending(&mut pending, blocked.pending(&["queued-during-sample.txt"]));
        assert_eq!(pending.len(), 1);
        assert!(sampling_roots.contains(&blocked.0));
        let available_result = sample_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(available_result.root, available.0);
        assert!(available_result.results[0].changed);
        assert!(sample_receiver.try_recv().is_err());
        release_sender.send(()).unwrap();
        let blocked_result = sample_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(blocked_result.root, blocked.0);
    }

    #[test]
    fn issue_133_navigation_cancels_remaining_metadata_reads() {
        use super::super::{DirectoryEvent, apply_event};

        let fixture = Fixture::new();
        let state = fixture.state(Vec::new());
        let request_id = state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.clone(), NavigationKind::Refresh)
            .0;
        apply_event(
            &state,
            DirectoryEvent::Batch {
                acknowledgement: mpsc::channel().0,
                tab_id: TabId(1),
                request_id,
                entries: Vec::new(),
            },
        );
        apply_event(
            &state,
            DirectoryEvent::Finished {
                tab_id: TabId(1),
                request_id,
                path: fixture.0.clone(),
                skipped: 0,
                source_failures: 0,
                library: None,
            },
        );
        assert!(
            state
                .lock()
                .unwrap()
                .tab(TabId(1))
                .unwrap()
                .search_cancel_token()
                .is_none()
        );
        let probe = prepare_probe(&state.lock().unwrap(), fixture.pending(&["a", "b", "c"]));
        let mut reads = 0;
        let results = probe_changes_with(
            vec![probe],
            |probe| probe_is_current(&state, probe),
            |_, _, _| {
                reads += 1;
                state
                    .lock()
                    .unwrap()
                    .tab_mut(TabId(1))
                    .unwrap()
                    .begin_directory_navigation(
                        fixture.0.join("elsewhere"),
                        NavigationKind::Normal,
                    );
                Ok(None)
            },
        );
        assert_eq!(
            reads, 1,
            "navigation stops remaining paths after the current system call returns"
        );
        let (sender, requests) = mpsc::channel();
        for result in results {
            assert!(apply_probe_result(&state, &sender, result).is_none());
        }
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn issue_133_cancelled_tab_does_not_cancel_shared_paths_needed_by_another_tab() {
        let fixture = Fixture::new();
        let mut app =
            AppState::new_for_test(vec![fixture.0.clone(), fixture.0.clone()], 0, [0, 1, 2, 3]);
        for id in [TabId(1), TabId(2)] {
            let tab = app.tab_mut(id).unwrap();
            tab.begin_directory_navigation(fixture.0.clone(), NavigationKind::Refresh);
            assert!(tab.finish_directory_navigation(
                tab.latest_request(),
                NavigationLocation::Directory(fixture.0.clone()),
                DirectoryViewPreference::default(),
                None
            ));
        }
        let first = prepare_probe(&app, fixture.pending(&["first-only", "shared"]));
        let mut second_pending = fixture.pending(&["second-only", "shared"]);
        second_pending.tab_id = TabId(2);
        let second = prepare_probe(&app, second_pending);
        app.tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(fixture.0.join("elsewhere"), NavigationKind::Normal);
        let state = Arc::new(Mutex::new(app));
        let mut paths = Vec::new();
        let results = probe_changes_with(
            vec![first, second],
            |probe| probe_is_current(&state, probe),
            |path, _, _| {
                paths.push(path.to_path_buf());
                Ok(None)
            },
        );
        assert_eq!(paths.len(), 2);
        assert_eq!(
            paths.into_iter().collect::<HashSet<_>>(),
            HashSet::from([fixture.0.join("second-only"), fixture.0.join("shared"),])
        );
        let (sender, requests) = mpsc::channel();
        for result in results {
            assert!(apply_probe_result(&state, &sender, result).is_none());
        }
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn issue_155_rename_watch_waits_and_preserves_other_changes() {
        use crate::domain::file_operations::{FileOperationKind, OperationItem, OperationResource};
        let fixture = Fixture::new();
        fs::write(fixture.0.join("old.png"), b"image").unwrap();
        let state = fixture.state(vec![fixture.entry("old.png", 1)]);
        let pending = fixture.pending(&["old.png", "new.png", "other.txt"]);
        {
            let mut app = state.lock().unwrap();
            app.operations.submit(
                OperationResource::Local,
                FileOperationKind::Rename,
                Some(TabId(1)),
                vec![OperationItem::pending(
                    Some(fixture.0.join("old.png")),
                    Some(fixture.0.join("new.png")),
                )],
            );
            app.active_operation_directories
                .insert(fixture.0.clone(), 1);
            assert_eq!(disposition(&app, &pending), Disposition::Wait);
        }
        let result = run_probe(&state, pending);
        let (sender, receiver) = mpsc::channel();
        let retry = apply_probe_result(&state, &sender, result).unwrap();
        assert_eq!(retry.batch.paths.len(), 3);
        assert!(state.lock().unwrap().deferred_watch_directories.is_empty());
        fs::rename(fixture.0.join("old.png"), fixture.0.join("new.png")).unwrap();
        fs::write(fixture.0.join("other.txt"), b"external").unwrap();
        {
            let mut app = state.lock().unwrap();
            app.active_operation_directories.clear();
            app.tab_mut(TabId(1))
                .unwrap()
                .replace_entries(vec![fixture.entry("new.png", 1)]);
        }
        let result = run_probe(&state, retry);
        assert!(result.changed);
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(receiver.try_recv().is_ok());
    }

    #[test]
    fn issue_155_old_watch_snapshot_rechecks_after_local_rename() {
        let fixture = Fixture::new();
        fs::write(fixture.0.join("old.png"), b"image").unwrap();
        let state = fixture.state(vec![fixture.entry("old.png", 1)]);
        fs::rename(fixture.0.join("old.png"), fixture.0.join("new.png")).unwrap();
        let result = run_probe(&state, fixture.pending(&["old.png", "new.png"]));
        assert!(result.changed);
        state
            .lock()
            .unwrap()
            .tab_mut(TabId(1))
            .unwrap()
            .replace_entries(vec![fixture.entry("new.png", 1)]);
        let (sender, receiver) = mpsc::channel();
        let retry = apply_probe_result(&state, &sender, result).unwrap();
        assert!(receiver.try_recv().is_err());
        let result = run_probe(&state, retry);
        assert!(!result.changed);
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(receiver.try_recv().is_err());
        assert_eq!(
            state.lock().unwrap().tab(TabId(1)).unwrap().selected,
            vec![EntryId(1)]
        );
    }
    #[test]
    fn issue_155_failed_rename_watch_keeps_external_changes() {
        use crate::domain::file_operations::{
            FileOperationKind, OperationItem, OperationResource, OperationState,
        };
        let fixture = Fixture::new();
        fs::write(fixture.0.join("old.png"), b"image").unwrap();
        let state = fixture.state(vec![fixture.entry("old.png", 1)]);
        let operation = {
            let mut app = state.lock().unwrap();
            let operation = app.operations.submit(
                OperationResource::Local,
                FileOperationKind::Rename,
                Some(TabId(1)),
                vec![OperationItem::pending(
                    Some(fixture.0.join("old.png")),
                    Some(fixture.0.join("new.png")),
                )],
            );
            app.active_operation_directories
                .insert(fixture.0.clone(), 1);
            operation
        };
        let (sender, receiver) = mpsc::channel();
        let result = run_probe(&state, fixture.pending(&["old.png", "other.txt"]));
        let retry = apply_probe_result(&state, &sender, result).unwrap();
        {
            let mut app = state.lock().unwrap();
            app.operations.task_mut(operation).unwrap().state = OperationState::Failed;
            app.active_operation_directories.clear();
        }
        fs::write(fixture.0.join("other.txt"), b"external").unwrap();
        let result = run_probe(&state, retry);
        assert!(result.changed);
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(receiver.try_recv().is_ok());
        assert!(fixture.0.join("old.png").is_file());
    }
    #[test]
    fn issue_133_active_operations_own_their_final_refresh() {
        let fixture = Fixture::new();
        let state = fixture.state(Vec::new());
        let pending = fixture.pending(&["finished.txt"]);
        let result = run_probe(&state, pending.clone());
        state
            .lock()
            .unwrap()
            .active_operation_directories
            .insert(fixture.0.clone(), 1);
        assert_eq!(
            disposition(&state.lock().unwrap(), &pending),
            Disposition::DeferredOperation
        );
        let (sender, receiver) = mpsc::channel();
        assert!(apply_probe_result(&state, &sender, result).is_none());
        assert!(receiver.try_recv().is_err());
        assert!(
            state
                .lock()
                .unwrap()
                .deferred_watch_directories
                .contains(&fixture.0)
        );
    }
}
