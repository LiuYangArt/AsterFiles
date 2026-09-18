use super::super::directory_loading::{
    DirectoryEvent, DirectoryRequest, network_directory_request,
    spawn_directory_workers_with_library_reader,
};
use super::super::library_source_group_projections;
use super::{POLL, SOURCE_SLOTS, SourceReader, start};
use crate::{
    domain::{EntryId, FileEntry, FolderSizeState, LibraryLocationId, TabId},
    platform,
};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

static HANG_STARTED: [AtomicBool; 4] = [const { AtomicBool::new(false) }; 4];

fn fixture_reader(
    path: &Path,
    _visibility: crate::domain::FileVisibility,
    cancel: &AtomicBool,
    on_batch: &mut dyn FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize> {
    let spelling = path.to_string_lossy();
    let index = (0..4).find(|index| spelling.starts_with(&format!(r"\\fixture-{index}\")));
    let ignored = AtomicBool::new(false);
    platform::windows::network::test_directory_stream_started(
        path,
        cancel,
        index.map(|index| &HANG_STARTED[index]).unwrap_or(&ignored),
        on_batch,
    )
}

struct Fixture(PathBuf);
impl Fixture {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "asterfiles-issue-104-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn directory(&self, name: &str, count: usize) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).unwrap();
        for index in 0..count {
            std::fs::write(path.join(format!("item-{index:04}.txt")), b"104").unwrap();
        }
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct CancelOnDrop(Vec<Arc<AtomicBool>>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        for cancel in &self.0 {
            cancel.store(true, Ordering::Release);
        }
    }
}

fn fixture_library() -> LibraryLocationId {
    LibraryLocationId::new("shell:issue-104:fixture".into(), "Issue 104 fixture".into())
}

fn request(id: u32, sources: Vec<(usize, PathBuf)>) -> DirectoryRequest {
    let mut request = network_directory_request("");
    request.tab_id = TabId(id);
    request.library = Some(fixture_library());
    request.library_sources = Some(sources);
    request
}

struct Loader {
    sender: Option<mpsc::Sender<DirectoryRequest>>,
    events: mpsc::Receiver<DirectoryEvent>,
    handle: Option<thread::JoinHandle<()>>,
}
impl Loader {
    fn new(reader: SourceReader) -> Self {
        let (events, receiver) = mpsc::channel();
        let (sender, handle) = start(events, reader);
        Self {
            sender: Some(sender),
            events: receiver,
            handle: Some(handle),
        }
    }
    fn send(&self, request: DirectoryRequest) {
        self.sender.as_ref().unwrap().send(request).unwrap();
    }
    fn receive(&self) -> DirectoryEvent {
        self.events.recv_timeout(Duration::from_secs(10)).unwrap()
    }
    fn collect(&self) -> (Vec<FileEntry>, Vec<usize>, DirectoryEvent) {
        let mut entries = Vec::new();
        let mut batches = Vec::new();
        loop {
            match self.receive() {
                DirectoryEvent::NetworkBatch {
                    entries: batch,
                    acknowledgement,
                    ..
                } => {
                    batches.push(batch.len());
                    entries.extend(batch);
                    acknowledgement.send(()).unwrap();
                }
                terminal => {
                    if let DirectoryEvent::Finished { library, .. } = &terminal {
                        assert_eq!(library.as_ref(), Some(&fixture_library()));
                        assert_eq!(library.as_ref().unwrap().display_name, "Issue 104 fixture");
                    }
                    return (entries, batches, terminal);
                }
            }
        }
    }
}
impl Drop for Loader {
    fn drop(&mut self) {
        drop(self.sender.take());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[test]
fn issue_104_four_hanging_library_helpers_do_not_block_healthy_sources_or_local_navigation() {
    for started in &HANG_STARTED {
        started.store(false, Ordering::Release);
    }
    let fixture = Fixture::new("isolation");
    let mut cancellation = CancelOnDrop(Vec::new());
    let (sender, network_sender, events) =
        spawn_directory_workers_with_library_reader(4, 4, fixture_reader);
    for index in 0..4 {
        let healthy = fixture.directory(&format!("healthy-{index}"), 1);
        let request = request(
            index as u32 + 1,
            vec![
                (1, PathBuf::from(format!(r"\\fixture-{index}\share\hang"))),
                (5, healthy),
            ],
        );
        cancellation.0.push(request.cancel.clone());
        sender.send(request).unwrap();
    }
    let started_at = Instant::now();
    while !HANG_STARTED
        .iter()
        .all(|started| started.load(Ordering::Acquire))
    {
        assert!(
            started_at.elapsed() < Duration::from_secs(10),
            "four real helpers must report ready"
        );
        thread::sleep(POLL);
    }
    let mut local = network_directory_request("");
    local.tab_id = TabId(99);
    local.path = fixture.directory("ordinary-local", 1);
    let local_started = Instant::now();
    sender.send(local).unwrap();
    let mut healthy_tabs = HashSet::new();
    let mut local_finished = false;
    while healthy_tabs.len() < 4 || !local_finished {
        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            DirectoryEvent::NetworkBatch {
                tab_id,
                entries,
                acknowledgement,
                ..
            } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].library_source_index, Some(5));
                healthy_tabs.insert(tab_id);
                acknowledgement.send(()).unwrap();
            }
            DirectoryEvent::Finished {
                tab_id: TabId(99), ..
            } => local_finished = true,
            DirectoryEvent::Batch {
                tab_id: TabId(99), ..
            } => {}
            unexpected => panic!("library finished while source still hung: {unexpected:?}"),
        }
    }
    let local_elapsed = local_started.elapsed();
    assert!(local_elapsed < Duration::from_secs(5));
    let cancel_started = Instant::now();
    for cancel in &cancellation.0 {
        cancel.store(true, Ordering::Release);
    }
    let mut cancelled = HashSet::new();
    while cancelled.len() < 4 {
        match events.recv_timeout(Duration::from_secs(10)).unwrap() {
            DirectoryEvent::Cancelled { tab_id, .. } => {
                cancelled.insert(tab_id);
            }
            unexpected => panic!("unexpected terminal: {unexpected:?}"),
        }
    }
    drop(sender);
    drop(network_sender);
    std::fs::create_dir_all("artifacts/state/windows-libraries").unwrap();
    let cancel_elapsed = cancel_started.elapsed();
    assert!(cancel_elapsed < Duration::from_secs(5));
    std::fs::write("artifacts/state/windows-libraries/issue-104-isolation.json",
        format!("{{\n  \"issue\":104,\n  \"real_hanging_helpers_started\":4,\n  \"healthy_library_tabs_before_cancellation\":{},\n  \"ordinary_local_finished_before_cancellation\":{},\n  \"cancelled_after_helpers_reaped\":{},\n  \"local_and_healthy_complete_ms\":{},\n  \"cancel_and_reap_ms\":{}\n}}\n",
            healthy_tabs.len(), local_finished, cancelled.len(), local_elapsed.as_millis(), cancel_elapsed.as_millis())).unwrap();
    eprintln!(
        "#104: 4 real hung helpers; 4 healthy library batches and ordinary local completion before cancellation; 4 Cancelled after helper reclamation"
    );
}

#[test]
fn issue_104_deduplicates_sources_preserves_sparse_indices_paths_and_global_ids() {
    let fixture = Fixture::new("identity");
    let first = fixture.directory("first", 1);
    let second = fixture.directory("second", 1);
    let loader = Loader::new(fixture_reader);
    loader.send(request(
        1,
        vec![(1, first.clone()), (3, first.clone()), (5, second.clone())],
    ));
    let (entries, _, terminal) = loader.collect();
    assert!(matches!(
        terminal,
        DirectoryEvent::Finished {
            source_failures: 0,
            ..
        }
    ));
    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.id)
            .collect::<HashSet<_>>()
            .len(),
        2
    );
    assert_eq!(entries[0].original_name, entries[1].original_name);
    let library = platform::windows::libraries::WindowsLibrary {
        id: platform::windows::libraries::LibraryId::new("shell:issue-104".into()),
        definition_path: None,
        display_name: "Issue 104".into(),
        pinned: false,
        sort_order: 0,
        sources: (0..6)
            .map(|index| platform::windows::libraries::LibrarySource {
                shell_identity: format!("shell:issue-104:source-{index}").into(),
                display_name: format!("Source {index}").into(),
                path: match index {
                    1 => Some(first.clone()),
                    5 => Some(second.clone()),
                    _ => None,
                },
            })
            .collect(),
        default_save_path: None,
    };
    let mut reversed = entries.clone();
    reversed.sort_by_key(|entry| std::cmp::Reverse(entry.library_source_index));
    let groups = library_source_group_projections(&library, &reversed);
    assert_eq!(
        groups
            .iter()
            .map(|group| group.label.as_str())
            .collect::<Vec<_>>(),
        ["Source 1", "Source 5"]
    );
    for (group, index) in groups.iter().zip([1, 5]) {
        assert_eq!(
            group.entries,
            entries
                .iter()
                .filter(|entry| entry.library_source_index == Some(index))
                .map(|entry| entry.id)
                .collect::<Vec<_>>()
        );
    }
    for (index, path) in [(1, first), (5, second)] {
        assert!(
            entries
                .iter()
                .any(|entry| entry.library_source_index == Some(index)
                    && entry.path == path.join("item-0000.txt"))
        );
    }
}

#[test]
fn issue_104_large_source_is_incremental_and_backpressured() {
    let fixture = Fixture::new("incremental");
    let loader = Loader::new(fixture_reader);
    loader.send(request(1, vec![(1, fixture.directory("many", 301))]));
    let first = loader.receive();
    assert!(
        loader
            .events
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    let first_count = match first {
        DirectoryEvent::NetworkBatch {
            entries,
            acknowledgement,
            ..
        } => {
            acknowledgement.send(()).unwrap();
            entries.len()
        }
        other => panic!("expected first batch: {other:?}"),
    };
    let (entries, batches, terminal) = loader.collect();
    assert!(first_count < 301);
    assert!(!batches.is_empty());
    assert_eq!(entries.len() + first_count, 301);
    assert!(matches!(terminal, DirectoryEvent::Finished { .. }));
}

#[test]
fn issue_104_partial_failure_total_failure_and_empty_library() {
    let fixture = Fixture::new("failures");
    let healthy = fixture.directory("healthy", 1);
    let missing = fixture.0.join("missing");
    let loader = Loader::new(fixture_reader);
    let mut partial = request(1, vec![(1, healthy), (5, missing.clone())]);
    partial.unavailable_library_sources = 1;
    loader.send(partial);
    let (entries, _, terminal) = loader.collect();
    assert_eq!(entries.len(), 1);
    assert!(matches!(
        terminal,
        DirectoryEvent::Finished {
            source_failures: 2,
            ..
        }
    ));
    loader.send(request(2, vec![(5, missing)]));
    assert!(matches!(loader.collect().2, DirectoryEvent::Failed { .. }));
    loader.send(request(3, vec![]));
    assert!(matches!(
        loader.collect().2,
        DirectoryEvent::Finished {
            source_failures: 0,
            ..
        }
    ));
    let mut unavailable = request(4, vec![]);
    unavailable.unavailable_library_sources = 2;
    loader.send(unavailable);
    assert!(matches!(
        loader.collect().2,
        DirectoryEvent::Failed {
            kind: io::ErrorKind::NotFound,
            ..
        }
    ));
}

static CANCELLATION_STARTED: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

fn wait_for_cancel_reader(
    _: &Path,
    _: crate::domain::FileVisibility,
    cancel: &AtomicBool,
    _: &mut dyn FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize> {
    CANCELLATION_STARTED.fetch_add(1, Ordering::Release);
    while !cancel.load(Ordering::Acquire) {
        thread::sleep(POLL);
    }
    Err(io::ErrorKind::Interrupted.into())
}

#[test]
fn issue_104_closed_input_cancels_active_and_queued_sources() {
    CANCELLATION_STARTED.store(0, Ordering::Release);
    let mut loader = Loader::new(wait_for_cancel_reader);
    for id in 1..=9 {
        loader.send(request(
            id,
            vec![(id as usize, PathBuf::from(format!("source-{id}")))],
        ));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while CANCELLATION_STARTED.load(Ordering::Acquire) < SOURCE_SLOTS {
        assert!(Instant::now() < deadline);
        thread::sleep(POLL);
    }
    drop(loader.sender.take());
    let mut cancelled = HashSet::new();
    for _ in 0..9 {
        match loader.receive() {
            DirectoryEvent::Cancelled { tab_id, .. } => {
                cancelled.insert(tab_id);
            }
            other => panic!("expected cancellation: {other:?}"),
        }
    }
    assert_eq!(cancelled.len(), 9);
}

#[test]
fn issue_104_dropped_receiver_releases_backpressured_source() {
    let fixture = Fixture::new("receiver-drop");
    let (events, receiver) = mpsc::channel();
    let (sender, handle) = start(events, fixture_reader);
    sender
        .send(request(1, vec![(1, fixture.directory("many", 301))]))
        .unwrap();
    let first = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
    drop(receiver);
    drop(first);
    drop(sender);
    handle.join().unwrap();
}
#[test]
fn issue_104_library_source_hides_internal_cleanup_directory() {
    let fixture = Fixture::new("cleanup");
    let source = fixture.directory("source", 1);
    std::fs::create_dir(source.join(".asterfiles-cleanup")).unwrap();
    std::fs::create_dir(source.join(".asterfiles-cleanup-copy")).unwrap();
    let loader = Loader::new(fixture_reader);
    loader.send(request(1, vec![(1, source.clone())]));
    let (entries, _, terminal) = loader.collect();
    assert!(matches!(
        terminal,
        DirectoryEvent::Finished {
            source_failures: 0,
            ..
        }
    ));
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .any(|entry| entry.path == source.join(".asterfiles-cleanup-copy"))
    );
    assert!(
        !entries
            .iter()
            .any(|entry| entry.path == source.join(".asterfiles-cleanup"))
    );
}

fn partial_failure_reader(
    path: &Path,
    _: crate::domain::FileVisibility,
    _: &AtomicBool,
    on_batch: &mut dyn FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize> {
    if path == Path::new("partial") {
        on_batch(vec![FileEntry {
            id: EntryId(777),
            display_name: "preserved.txt".into(),
            name_highlights: Vec::new(),
            original_name: "preserved.txt".into(),
            path: path.join("preserved.txt"),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            library_source_index: None,
            parent_display: "partial".into(),
            size_bytes: Some(104),
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
        }])?;
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    if path == Path::new("denied") {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    if path == Path::new("broken") {
        return Err(io::ErrorKind::ConnectionReset.into());
    }
    Err(io::ErrorKind::NotFound.into())
}

#[test]
fn issue_104_preserves_delivered_entries_when_every_source_fails_and_prioritizes_permission_error()
{
    let loader = Loader::new(partial_failure_reader);
    let mut partial = request(
        1,
        vec![
            (1, PathBuf::from("partial")),
            (3, PathBuf::from("denied")),
            (5, PathBuf::from("missing")),
        ],
    );
    partial.unavailable_library_sources = 1;
    loader.send(partial);
    let (entries, batches, terminal) = loader.collect();
    assert_eq!(batches, [1]);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, Path::new("partial").join("preserved.txt"));
    assert_eq!(entries[0].library_source_index, Some(1));
    assert_eq!(entries[0].id, EntryId(1));
    assert!(matches!(
        terminal,
        DirectoryEvent::Finished {
            source_failures: 4,
            ..
        }
    ));

    loader.send(request(
        2,
        vec![
            (1, PathBuf::from("denied")),
            (3, PathBuf::from("broken")),
            (5, PathBuf::from("missing")),
        ],
    ));
    let (entries, batches, terminal) = loader.collect();
    assert!(entries.is_empty());
    assert!(batches.is_empty());
    assert!(matches!(
        terminal,
        DirectoryEvent::Failed {
            kind: io::ErrorKind::PermissionDenied,
            ..
        }
    ));
}

#[test]
fn issue_104_parent_cancel_reaps_real_helper_while_page_acknowledgement_is_pending() {
    let fixture = Fixture::new("cancel-unacknowledged");
    let loader = Loader::new(fixture_reader);
    let pending_request = request(1, vec![(1, fixture.directory("many", 301))]);
    let cancel = pending_request.cancel.clone();
    loader.send(pending_request);
    let acknowledgement = match loader.receive() {
        DirectoryEvent::NetworkBatch {
            entries,
            acknowledgement,
            ..
        } => {
            assert!(!entries.is_empty());
            assert!(entries.len() < 301);
            acknowledgement
        }
        other => panic!("expected real helper batch: {other:?}"),
    };
    assert!(
        loader
            .events
            .recv_timeout(Duration::from_millis(100))
            .is_err()
    );
    let started = Instant::now();
    cancel.store(true, Ordering::Release);
    assert!(matches!(
        loader.receive(),
        DirectoryEvent::Cancelled {
            tab_id: TabId(1),
            ..
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(
        acknowledgement.send(()).is_err(),
        "reaped source no longer waits for page acknowledgement"
    );
    assert!(loader.sender.is_some());
    loader.send(request(2, vec![]));
    assert!(matches!(
        loader.collect().2,
        DirectoryEvent::Finished {
            tab_id: TabId(2),
            ..
        }
    ));
    eprintln!(
        "#104: parent cancellation reclaimed real helper with pending page acknowledgement in {} ms; input remained open",
        started.elapsed().as_millis()
    );
}
