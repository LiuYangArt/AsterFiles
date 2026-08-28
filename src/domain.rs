use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size_bytes: Option<u64>,
    pub modified: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

impl EntryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Directory => "Folder",
            Self::File => "File",
            Self::Other => "Other",
        }
    }
}
