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
        let metadata = fs::metadata(&path).unwrap_or(entry_metadata);
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
            let metadata = fs::metadata(&path).unwrap_or(entry_metadata);
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
    let mut kind = if metadata.is_dir() {
        EntryKind::Directory
    } else if metadata.is_file() {
        EntryKind::File
    } else {
        EntryKind::Other
    };
    let mut open_target = None;
    if let Ok(Some(target)) = crate::platform::resolve_shortcut_target(&path)
        && target.is_directory != Some(false)
    {
        kind = EntryKind::Directory;
        open_target = Some(target.path);
    }
    FileEntry {
        id: EntryId(id),
        display_name: original_name.to_string_lossy().into_owned(),
        name_highlights: Vec::new(),
        original_name,
        path: path.clone(),
        kind,
        open_target,
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
