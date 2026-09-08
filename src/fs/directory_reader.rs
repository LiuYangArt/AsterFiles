use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use crate::domain::{EntryId, EntryKind, FileEntry, FileVisibility};

pub const DIRECTORY_FIRST_BATCH_SIZE: usize = 32;
pub const DIRECTORY_BATCH_SIZE: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    Complete { skipped: usize },
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceReadState {
    Complete { skipped: usize },
    NotFound,
    PermissionDenied,
    Disconnected,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceReadOutcome {
    pub source: PathBuf,
    pub state: SourceReadState,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AggregateReadOutcome {
    pub sources: Vec<SourceReadOutcome>,
    pub cancelled: bool,
}

#[cfg(test)]
fn read_directory_batches(
    path: &Path,
    cancel: &Arc<AtomicBool>,
    on_batch: impl FnMut(Vec<FileEntry>),
) -> io::Result<ReadOutcome> {
    read_directory_batches_filtered(path, cancel, FileVisibility::default(), on_batch)
}
pub fn read_directory_batches_filtered(
    path: &Path,
    cancel: &Arc<AtomicBool>,
    visibility: FileVisibility,
    mut on_batch: impl FnMut(Vec<FileEntry>),
) -> io::Result<ReadOutcome> {
    let mut batch_limit = DIRECTORY_FIRST_BATCH_SIZE;
    let mut batch = Vec::with_capacity(batch_limit);
    let mut skipped = 0;
    let mut next_id = 1_u32;

    for result in fs::read_dir(path)? {
        if cancel.load(AtomicOrdering::Acquire) {
            return Ok(ReadOutcome::Cancelled);
        }
        let directory_entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        let entry_metadata = match directory_entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if !metadata_is_visible(&entry_metadata, visibility) {
            continue;
        }
        let path = directory_entry.path();
        let metadata = metadata_for_entry(&path, entry_metadata);
        if cancel.load(AtomicOrdering::Acquire) {
            return Ok(ReadOutcome::Cancelled);
        }
        batch.push(file_entry(
            directory_entry.file_name(),
            path,
            metadata,
            next_id,
        ));
        next_id = next_id.checked_add(1).expect("directory entry ID overflow");

        if batch.len() == batch_limit {
            on_batch(std::mem::take(&mut batch));
            batch_limit = DIRECTORY_BATCH_SIZE;
            batch = Vec::with_capacity(batch_limit);
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    Ok(ReadOutcome::Complete { skipped })
}

pub fn read_aggregate_directory_batches_filtered(
    sources: &[PathBuf],
    cancel: &Arc<AtomicBool>,
    visibility: FileVisibility,
    mut on_batch: impl FnMut(Vec<FileEntry>),
) -> AggregateReadOutcome {
    let mut aggregate = AggregateReadOutcome::default();
    let mut batch_limit = DIRECTORY_FIRST_BATCH_SIZE;
    let mut batch = Vec::with_capacity(batch_limit);
    let mut next_id = 1_u32;

    for (source_index, source) in sources.iter().enumerate() {
        if cancel.load(AtomicOrdering::Acquire) {
            aggregate.cancelled = true;
            aggregate.sources.push(SourceReadOutcome {
                source: source.clone(),
                state: SourceReadState::Cancelled,
                message: None,
            });
            break;
        }

        let entries = match fs::read_dir(source) {
            Ok(entries) => entries,
            Err(error) => {
                aggregate.sources.push(source_error(source, error));
                continue;
            }
        };
        let mut skipped = 0;
        let mut source_cancelled = false;
        for result in entries {
            if cancel.load(AtomicOrdering::Acquire) {
                source_cancelled = true;
                break;
            }
            let directory_entry = match result {
                Ok(entry) => entry,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            let entry_metadata = match directory_entry.metadata() {
                Ok(metadata) => metadata,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };
            if !metadata_is_visible(&entry_metadata, visibility) {
                continue;
            }
            let path = directory_entry.path();
            let metadata = metadata_for_entry(&path, entry_metadata);
            if cancel.load(AtomicOrdering::Acquire) {
                source_cancelled = true;
                break;
            }
            let mut entry = file_entry(directory_entry.file_name(), path, metadata, next_id);
            entry.library_source_index = Some(source_index);
            batch.push(entry);
            next_id = next_id.checked_add(1).expect("directory entry ID overflow");

            if batch.len() == batch_limit {
                on_batch(std::mem::take(&mut batch));
                batch_limit = DIRECTORY_BATCH_SIZE;
                batch = Vec::with_capacity(batch_limit);
            }
        }
        if source_cancelled {
            aggregate.cancelled = true;
            aggregate.sources.push(SourceReadOutcome {
                source: source.clone(),
                state: SourceReadState::Cancelled,
                message: None,
            });
            break;
        }
        aggregate.sources.push(SourceReadOutcome {
            source: source.clone(),
            state: SourceReadState::Complete { skipped },
            message: None,
        });
    }

    if !batch.is_empty() {
        on_batch(batch);
    }
    aggregate
}

fn file_entry(
    original_name: std::ffi::OsString,
    path: PathBuf,
    metadata: fs::Metadata,
    id: u32,
) -> FileEntry {
    let kind = if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    FileEntry {
        id: EntryId(id),
        display_name: original_name.to_string_lossy().into_owned(),
        name_highlights: Vec::new(),
        original_name,
        path: path.clone(),
        kind,
        open_target: None,
        library_source_index: None,
        parent_display: path
            .parent()
            .map(|value| value.as_os_str().to_string_lossy().into_owned())
            .unwrap_or_default(),
        size_bytes: metadata.is_file().then_some(metadata.len()),
        folder_size: crate::domain::FolderSizeState::Unknown,
        modified: metadata.modified().ok(),
        created: metadata.created().ok(),
    }
}

#[cfg(windows)]
fn metadata_for_entry(path: &Path, metadata: fs::Metadata) -> fs::Metadata {
    use std::os::windows::fs::MetadataExt;

    if attributes_need_followup_metadata(metadata.file_attributes()) {
        fs::metadata(path).unwrap_or(metadata)
    } else {
        metadata
    }
}

#[cfg(windows)]
fn attributes_need_followup_metadata(attributes: u32) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn source_error(source: &Path, error: io::Error) -> SourceReadOutcome {
    let state = match error.kind() {
        io::ErrorKind::NotFound => SourceReadState::NotFound,
        io::ErrorKind::PermissionDenied => SourceReadState::PermissionDenied,
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkDown
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::NotConnected
        | io::ErrorKind::TimedOut => SourceReadState::Disconnected,
        _ => SourceReadState::Failed,
    };
    SourceReadOutcome {
        source: source.to_path_buf(),
        state,
        message: Some(error.to_string()),
    }
}
#[cfg(windows)]
fn metadata_is_visible(metadata: &fs::Metadata, visibility: FileVisibility) -> bool {
    use std::os::windows::fs::MetadataExt;

    attributes_are_visible(metadata.file_attributes(), visibility)
}

#[cfg(windows)]
fn attributes_are_visible(attributes: u32, visibility: FileVisibility) -> bool {
    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

    (visibility.show_hidden || attributes & FILE_ATTRIBUTE_HIDDEN == 0)
        && (visibility.show_system || attributes & FILE_ATTRIBUTE_SYSTEM == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_batch_is_small_and_following_batches_are_larger() {
        assert_eq!(DIRECTORY_FIRST_BATCH_SIZE, 32);
        assert_eq!(DIRECTORY_BATCH_SIZE, 256);
    }
    #[test]
    fn default_visibility_shows_hidden_but_not_system_entries() {
        assert_eq!(
            FileVisibility::default(),
            FileVisibility {
                show_hidden: true,
                show_system: false,
            }
        );
    }

    #[test]
    fn reads_the_current_directory_in_batches() {
        let cancel = Arc::new(AtomicBool::new(false));
        let mut entries = Vec::new();
        let outcome =
            read_directory_batches(Path::new("."), &cancel, |batch| entries.extend(batch))
                .expect("current directory must be readable");
        assert!(matches!(outcome, ReadOutcome::Complete { .. }));
        assert!(
            entries
                .iter()
                .any(|entry| entry.display_name == "Cargo.toml")
        );
    }

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "asterfiles-directory-reader-{name}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("system clock is valid")
                    .as_nanos()
            ));
            fs::create_dir_all(&path).expect("temporary fixture directory can be created");
            Self(path)
        }

        fn child(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn aggregate_read_keeps_real_paths_same_names_and_global_ids() {
        let fixture = TempTree::new("identity");
        let first = fixture.child("first");
        let second = fixture.child("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        fs::write(first.join("same.txt"), b"first").unwrap();
        fs::write(second.join("same.txt"), b"second").unwrap();

        let cancel = Arc::new(AtomicBool::new(false));
        let mut entries = Vec::new();
        let result = read_aggregate_directory_batches_filtered(
            &[first.clone(), second.clone()],
            &cancel,
            FileVisibility::default(),
            |batch| entries.extend(batch),
        );

        assert!(!result.cancelled);
        assert_eq!(result.sources.len(), 2);
        assert!(
            result
                .sources
                .iter()
                .all(|source| source.state == SourceReadState::Complete { skipped: 0 })
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, EntryId(1));
        assert_eq!(entries[1].id, EntryId(2));
        assert_eq!(entries[0].display_name, "same.txt");
        assert_eq!(entries[1].display_name, "same.txt");
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == first.join("same.txt"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.path == second.join("same.txt"))
        );
        assert_eq!(entries[0].library_source_index, Some(0));
        assert_eq!(entries[1].library_source_index, Some(1));
    }

    #[test]
    fn aggregate_read_batches_across_source_boundaries() {
        let fixture = TempTree::new("batches");
        let first = fixture.child("first");
        let second = fixture.child("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        for index in 0..16 {
            fs::write(first.join(format!("first-{index:03}.txt")), b"").unwrap();
        }
        for index in 0..284 {
            fs::write(second.join(format!("second-{index:03}.txt")), b"").unwrap();
        }

        let cancel = Arc::new(AtomicBool::new(false));
        let mut batch_sizes = Vec::new();
        let mut ids = Vec::new();
        let result = read_aggregate_directory_batches_filtered(
            &[first, second],
            &cancel,
            FileVisibility::default(),
            |batch| {
                batch_sizes.push(batch.len());
                ids.extend(batch.into_iter().map(|entry| entry.id.0));
            },
        );

        assert!(!result.cancelled);
        assert_eq!(batch_sizes, vec![32, 256, 12]);
        assert_eq!(ids, (1..=300).collect::<Vec<_>>());
    }

    #[test]
    fn aggregate_read_continues_after_missing_source_and_accepts_empty_library() {
        let fixture = TempTree::new("partial");
        let missing = fixture.child("missing");
        let available = fixture.child("available");
        fs::create_dir_all(&available).unwrap();
        fs::write(available.join("kept.txt"), b"").unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut entries = Vec::new();

        let result = read_aggregate_directory_batches_filtered(
            &[missing.clone(), available],
            &cancel,
            FileVisibility::default(),
            |batch| entries.extend(batch),
        );

        assert_eq!(result.sources.len(), 2);
        assert_eq!(result.sources[0].source, missing);
        assert_eq!(result.sources[0].state, SourceReadState::NotFound);
        assert_eq!(
            result.sources[1].state,
            SourceReadState::Complete { skipped: 0 }
        );
        assert_eq!(entries.len(), 1);

        let empty = read_aggregate_directory_batches_filtered(
            &[],
            &cancel,
            FileVisibility::default(),
            |_| panic!("empty library must not emit a batch"),
        );
        assert_eq!(empty, AggregateReadOutcome::default());
    }

    #[test]
    fn aggregate_read_cancels_between_batches() {
        let fixture = TempTree::new("cancel");
        for index in 0..64 {
            fs::write(fixture.child(&format!("item-{index:03}.txt")), b"").unwrap();
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_after_first = cancel.clone();
        let mut batches = 0;

        let result = read_aggregate_directory_batches_filtered(
            std::slice::from_ref(&fixture.0),
            &cancel,
            FileVisibility::default(),
            |_| {
                batches += 1;
                cancel_after_first.store(true, AtomicOrdering::Release);
            },
        );

        assert!(result.cancelled);
        assert_eq!(batches, 1);
        assert_eq!(result.sources[0].state, SourceReadState::Cancelled);
    }

    #[test]
    fn source_errors_are_classified_for_the_coordinator() {
        let path = Path::new("source");
        assert_eq!(
            source_error(path, io::Error::from(io::ErrorKind::PermissionDenied)).state,
            SourceReadState::PermissionDenied
        );
        assert_eq!(
            source_error(path, io::Error::from(io::ErrorKind::TimedOut)).state,
            SourceReadState::Disconnected
        );
        assert_eq!(
            source_error(path, io::Error::from(io::ErrorKind::InvalidData)).state,
            SourceReadState::Failed
        );
    }
    #[cfg(windows)]
    #[test]
    fn hidden_and_system_attributes_are_filtered_independently() {
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        let defaults = FileVisibility::default();
        assert!(attributes_are_visible(HIDDEN, defaults));
        assert!(!attributes_are_visible(SYSTEM, defaults));
        assert!(!attributes_are_visible(HIDDEN | SYSTEM, defaults));

        let hidden_off = FileVisibility {
            show_hidden: false,
            show_system: true,
        };
        assert!(!attributes_are_visible(HIDDEN, hidden_off));
        assert!(attributes_are_visible(SYSTEM, hidden_off));

        let all = FileVisibility {
            show_hidden: true,
            show_system: true,
        };
        assert!(attributes_are_visible(HIDDEN | SYSTEM, all));
    }

    #[cfg(windows)]
    #[test]
    fn issue_64_only_reparse_points_need_followup_metadata() {
        assert!(!attributes_need_followup_metadata(0));
        assert!(!attributes_need_followup_metadata(0x10));
        assert!(attributes_need_followup_metadata(0x400));
        assert!(attributes_need_followup_metadata(0x410));
    }

    #[cfg(windows)]
    #[test]
    fn issue_64_shortcuts_do_not_block_directory_batches() {
        let fixture = TempTree::new("shortcut-batch");
        fs::write(
            fixture.child("folder.lnk"),
            b"not parsed during enumeration",
        )
        .unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let mut entries = Vec::new();
        read_directory_batches_filtered(
            &fixture.0,
            &cancel,
            FileVisibility {
                show_hidden: true,
                show_system: true,
            },
            |batch| entries.extend(batch),
        )
        .unwrap();
        let entry = entries
            .iter()
            .find(|entry| entry.display_name == "folder.lnk")
            .unwrap();
        assert_eq!(entry.kind, EntryKind::File);
        assert!(entry.open_target.is_none());
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "explicit 100k-entry directory performance evidence"]
    fn issue_64_directory_loading_performance_evidence() {
        use std::time::Instant;

        let fixture = TempTree::new("performance");
        let ordinary = fixture.child("ordinary");
        let shortcuts = fixture.child("shortcuts");
        fs::create_dir_all(&ordinary).unwrap();
        fs::create_dir_all(&shortcuts).unwrap();
        for index in 0..100_000_u32 {
            fs::File::create(ordinary.join(format!("file-{index:06}.txt"))).unwrap();
        }
        for index in 0..10_000_u32 {
            fs::File::create(shortcuts.join(format!("link-{index:05}.lnk"))).unwrap();
        }

        let measure = |path: &Path| {
            let cancel = Arc::new(AtomicBool::new(false));
            let started = Instant::now();
            let mut first_batch_ms = None;
            let mut count = 0_usize;
            let outcome = read_directory_batches_filtered(
                path,
                &cancel,
                FileVisibility {
                    show_hidden: true,
                    show_system: true,
                },
                |batch| {
                    count += batch.len();
                    first_batch_ms.get_or_insert_with(|| started.elapsed().as_millis());
                },
            )
            .unwrap();
            assert_eq!(outcome, ReadOutcome::Complete { skipped: 0 });
            (
                first_batch_ms.unwrap_or(0),
                started.elapsed().as_millis(),
                count,
            )
        };
        let ordinary_result = measure(&ordinary);
        let shortcut_result = measure(&shortcuts);
        let artifact = PathBuf::from("artifacts/perf/directory-loading/issue-64.json");
        fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        fs::write(
            artifact,
            format!(
                concat!(
                    "{{\n  \"schema_version\": 1,\n",
                    "  \"ordinary\": {{\"count\": {}, \"first_batch_ms\": {}, \"full_enumeration_ms\": {}, \"followup_path_metadata_reads\": 0}},\n",
                    "  \"shortcuts\": {{\"count\": {}, \"first_batch_ms\": {}, \"full_enumeration_ms\": {}, \"resolved_during_enumeration\": 0}},\n",
                    "  \"batch_sizes\": [32, 256]\n}}\n"
                ),
                ordinary_result.2,
                ordinary_result.0,
                ordinary_result.1,
                shortcut_result.2,
                shortcut_result.0,
                shortcut_result.1,
            ),
        )
        .unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn follows_directory_reparse_points_for_navigation() {
        let users = Path::new(r"C:\Users");
        let candidate = ["All Users", "Default User"]
            .into_iter()
            .map(|name| users.join(name))
            .find(|path| path.exists());
        let Some(candidate) = candidate else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let mut entries = Vec::new();
        read_directory_batches_filtered(
            users,
            &cancel,
            FileVisibility {
                show_hidden: true,
                show_system: true,
            },
            |batch| entries.extend(batch),
        )
        .expect("users directory must be readable");
        let name = candidate
            .file_name()
            .expect("candidate has a name")
            .to_string_lossy();
        let entry = entries
            .iter()
            .find(|entry| entry.display_name == name)
            .expect("reparse point is listed");
        assert_eq!(entry.kind, EntryKind::Directory);
    }

    #[test]
    fn honours_cancellation_before_enumeration() {
        let cancel = Arc::new(AtomicBool::new(true));
        let outcome = read_directory_batches(Path::new("."), &cancel, |_| {})
            .expect("current directory must be readable");
        assert_eq!(outcome, ReadOutcome::Cancelled);
    }
}
