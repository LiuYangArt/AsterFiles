use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Original local identity with a parent shared by entries from one directory.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryPath {
    parent: Arc<Path>,
    leaf: Box<OsStr>,
}

impl EntryPath {
    pub fn to_path_buf(&self) -> PathBuf {
        if self.leaf.is_empty() {
            self.parent.to_path_buf()
        } else {
            self.parent.join(&*self.leaf)
        }
    }

    pub fn parent(&self) -> &Arc<Path> {
        &self.parent
    }

    pub fn file_name(&self) -> Option<&OsStr> {
        if self.leaf.is_empty() {
            self.parent.file_name()
        } else {
            Some(self.leaf.as_ref())
        }
    }

    pub fn extension(&self) -> Option<&OsStr> {
        if self.leaf.is_empty() {
            self.parent.extension()
        } else {
            Path::new(&*self.leaf).extension()
        }
    }
    pub fn matches(&self, path: &Path) -> bool {
        if self.leaf.is_empty() {
            self.parent.as_ref() == path
        } else {
            path.file_name() == Some(self.leaf.as_ref())
                && path.parent() == Some(self.parent.as_ref())
        }
    }

    // A directory stream owns this cache; no global interning or filesystem access.
    pub fn share_parent(&mut self, cached: &mut Option<Arc<Path>>) {
        if let Some(parent) = cached
            && parent.as_os_str() == self.parent.as_os_str()
        {
            self.parent = Arc::clone(parent);
        } else {
            *cached = Some(Arc::clone(&self.parent));
        }
    }
}

impl From<&Path> for EntryPath {
    fn from(path: &Path) -> Self {
        match (path.parent(), path.file_name()) {
            // An ADS name such as a:b is parsed as a drive prefix when joined alone.
            (Some(parent), Some(leaf))
                if matches!(
                    Path::new(leaf).components().next(),
                    Some(std::path::Component::Normal(_))
                ) =>
            {
                Self {
                    parent: Arc::from(parent),
                    leaf: leaf.into(),
                }
            }
            _ => Self {
                parent: Arc::from(path),
                leaf: OsStr::new("").into(),
            },
        }
    }
}

impl From<PathBuf> for EntryPath {
    fn from(path: PathBuf) -> Self {
        Self::from(path.as_path())
    }
}

impl From<&str> for EntryPath {
    fn from(path: &str) -> Self {
        Self::from(Path::new(path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn issue_170_stream_names_are_not_reinterpreted_as_drive_paths() {
        for raw in [
            r"C:\foo\a:b",
            r"C:\foo\a:.txt",
            r"\\?\C:\foo\a:b.txt",
            r"\\server\share\a:b.txt",
        ] {
            let path = Path::new(raw);
            let identity = EntryPath::from(path);
            assert_eq!(identity.to_path_buf().as_os_str(), path.as_os_str());
            assert_eq!(identity.file_name(), path.file_name());
            assert_eq!(identity.extension(), path.extension());
            assert!(identity.matches(path));
        }
    }
    #[test]
    fn issue_170_shared_parent_preserves_paths_and_clone_identity() {
        let mut cached = None;
        let mut left = EntryPath::from(r"C:\original\one.txt");
        let mut right = EntryPath::from(r"C:\original\two.txt");
        left.share_parent(&mut cached);
        right.share_parent(&mut cached);
        assert!(Arc::ptr_eq(left.parent(), right.parent()));
        assert_eq!(left.to_path_buf(), Path::new(r"C:\original\one.txt"));
        assert_eq!(right.to_path_buf(), Path::new(r"C:\original\two.txt"));
        assert!(Arc::ptr_eq(right.parent(), right.clone().parent()));
        assert!(left.matches(Path::new(r"C:\original\one.txt")));
        assert!(!left.matches(Path::new(r"C:\different\one.txt")));
    }

    #[test]
    fn issue_170_roots_empty_relative_and_unc_paths_roundtrip() {
        for raw in [
            "",
            ".",
            "one.txt",
            r"C:\",
            r"C:one.txt",
            r"\\server\share",
            r"\\server\share\one.txt",
        ] {
            let path = Path::new(raw);
            let identity = EntryPath::from(path);
            assert_eq!(identity.to_path_buf(), path);
            assert!(identity.matches(path));
        }
    }

    #[cfg(windows)]
    #[test]
    fn issue_170_non_unicode_identity_never_uses_display_text() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let raw = std::ffi::OsString::from_wide(&[67, 58, 92, 0xd800, 92, 0xdc00, 46, 116]);
        let identity = EntryPath::from(Path::new(&raw));
        assert_eq!(
            identity
                .to_path_buf()
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            raw.encode_wide().collect::<Vec<_>>()
        );
        assert!(identity.matches(Path::new(&raw)));
    }
}
