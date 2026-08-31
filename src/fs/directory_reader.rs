use std::{
    fs, io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use crate::domain::{EntryId, EntryKind, FileEntry, FileVisibility};

pub const DIRECTORY_BATCH_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    Complete { skipped: usize },
    Cancelled,
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
    let mut batch = Vec::with_capacity(DIRECTORY_BATCH_SIZE);
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
        let original_name = directory_entry.file_name();
        batch.push(FileEntry {
            id: EntryId(next_id),
            display_name: original_name.to_string_lossy().into_owned(),
            name_highlights: Vec::new(),
            original_name,
            path: path.clone(),
            kind,
            open_target,
            parent_display: path
                .parent()
                .map(|value| value.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default(),
            size_bytes: metadata.is_file().then_some(metadata.len()),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: metadata.modified().ok(),
        });
        next_id += 1;

        if batch.len() == DIRECTORY_BATCH_SIZE {
            on_batch(std::mem::take(&mut batch));
            batch = Vec::with_capacity(DIRECTORY_BATCH_SIZE);
        }
    }
    if !batch.is_empty() {
        on_batch(batch);
    }
    Ok(ReadOutcome::Complete { skipped })
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

#[cfg(not(windows))]
fn metadata_is_visible(_: &fs::Metadata, _: FileVisibility) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

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
