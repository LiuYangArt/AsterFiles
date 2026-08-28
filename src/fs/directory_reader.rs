use std::{
    fs, io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use crate::domain::{EntryId, EntryKind, FileEntry};

pub const DIRECTORY_BATCH_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadOutcome {
    Complete { skipped: usize },
    Cancelled,
}

pub fn read_directory_batches(
    path: &Path,
    cancel: &Arc<AtomicBool>,
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
        let metadata = match directory_entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        if cancel.load(AtomicOrdering::Acquire) {
            return Ok(ReadOutcome::Cancelled);
        }
        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        let original_name = directory_entry.file_name();
        batch.push(FileEntry {
            id: EntryId(next_id),
            display_name: original_name.to_string_lossy().into_owned(),
            original_name,
            path: directory_entry.path(),
            kind,
            size_bytes: metadata.is_file().then_some(metadata.len()),
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn honours_cancellation_before_enumeration() {
        let cancel = Arc::new(AtomicBool::new(true));
        let outcome = read_directory_batches(Path::new("."), &cancel, |_| {})
            .expect("current directory must be readable");
        assert_eq!(outcome, ReadOutcome::Cancelled);
    }
}
