use std::{cmp::Ordering, fs, io, path::Path};

use crate::domain::{EntryKind, FileEntry};

#[derive(Debug)]
pub struct DirectoryLoad {
    pub entries: Vec<FileEntry>,
    pub skipped: usize,
}

pub fn load_directory(path: &Path) -> io::Result<DirectoryLoad> {
    let mut entries = Vec::new();
    let mut skipped = 0;

    for result in fs::read_dir(path)? {
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

        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        entries.push(FileEntry {
            name: directory_entry.file_name().to_string_lossy().into_owned(),
            path: directory_entry.path(),
            kind,
            size_bytes: metadata.is_file().then_some(metadata.len()),
            modified: format_modified(metadata.modified().ok()),
        });
    }

    entries.sort_unstable_by(compare_entries);
    Ok(DirectoryLoad { entries, skipped })
}

fn compare_entries(left: &FileEntry, right: &FileEntry) -> Ordering {
    match (
        left.kind == EntryKind::Directory,
        right.kind == EntryKind::Directory,
    ) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => left.name.to_lowercase().cmp(&right.name.to_lowercase()),
    }
}

fn format_modified(value: Option<std::time::SystemTime>) -> String {
    let Some(value) = value else {
        return "—".to_owned();
    };
    let Ok(duration) = value.elapsed() else {
        return "—".to_owned();
    };
    let seconds = duration.as_secs();
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 3_600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h ago", seconds / 3_600)
    } else {
        format!("{}d ago", seconds / 86_400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_current_directory() {
        let result = load_directory(Path::new(".")).expect("current directory must be readable");
        assert!(!result.entries.is_empty());
        assert!(
            result
                .entries
                .iter()
                .any(|entry| entry.name == "Cargo.toml")
        );
    }
}
