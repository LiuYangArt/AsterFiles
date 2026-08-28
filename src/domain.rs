use std::{
    collections::HashMap,
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Idle,
    Loading,
    Partial,
    Complete,
    Cancelled,
    NotFound,
    PermissionDenied,
    Disconnected,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationKind {
    Normal,
    Back,
    Forward,
    Refresh,
}

#[derive(Debug)]
pub struct TabSession {
    pub id: TabId,
    pub latest_request: RequestId,
    pub current_path: Option<PathBuf>,
    pub requested_path: Option<PathBuf>,
    pub navigation_kind: NavigationKind,
    pub back_history: Vec<PathBuf>,
    pub forward_history: Vec<PathBuf>,
    pub entries: Vec<FileEntry>,
    pub pending_entries: Vec<FileEntry>,
    pub entry_paths: HashMap<EntryId, PathBuf>,
    pub load_state: LoadState,
    pub error: Option<String>,
    pub selected: Vec<EntryId>,
    pub focused: Option<EntryId>,
    pub scroll_offset: f32,
    pub first_batch_ms: Option<u128>,
    pub cancel_elapsed_ms: Option<u128>,
    pub discarded_results: u64,
    cancel: Option<Arc<AtomicBool>>,
}

impl TabSession {
    pub fn new(id: TabId) -> Self {
        Self {
            id,
            latest_request: RequestId(0),
            current_path: None,
            requested_path: None,
            navigation_kind: NavigationKind::Normal,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            entries: Vec::new(),
            pending_entries: Vec::new(),
            entry_paths: HashMap::new(),
            load_state: LoadState::Idle,
            error: None,
            selected: Vec::new(),
            focused: None,
            scroll_offset: 0.0,
            first_batch_ms: None,
            cancel_elapsed_ms: None,
            discarded_results: 0,
            cancel: None,
        }
    }

    pub fn begin_navigation(
        &mut self,
        path: PathBuf,
        kind: NavigationKind,
    ) -> (RequestId, Arc<AtomicBool>) {
        self.cancel_pending();
        self.latest_request.0 += 1;
        self.requested_path = Some(path);
        self.navigation_kind = kind;
        self.load_state = LoadState::Loading;
        self.error = None;
        self.first_batch_ms = None;
        self.cancel_elapsed_ms = None;
        self.pending_entries.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        (self.latest_request, cancel)
    }

    pub fn accepts(&self, request_id: RequestId) -> bool {
        self.latest_request == request_id
            && !self
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }

    pub fn cancel_pending(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
            if matches!(self.load_state, LoadState::Loading | LoadState::Partial) {
                self.load_state = LoadState::Cancelled;
            }
        }
    }

    pub fn replace_entries(&mut self, entries: Vec<FileEntry>) {
        self.entry_paths = entries
            .iter()
            .map(|entry| (entry.id, entry.path.clone()))
            .collect();
        self.entries = entries;
    }

    pub fn append_pending(&mut self, mut entries: Vec<FileEntry>) {
        for entry in &entries {
            self.entry_paths.insert(entry.id, entry.path.clone());
        }
        self.pending_entries.append(&mut entries);
        self.load_state = LoadState::Partial;
    }

    pub fn commit_pending(&mut self) {
        let entries = std::mem::take(&mut self.pending_entries);
        self.replace_entries(entries);
        self.load_state = LoadState::Complete;
        self.cancel = None;
    }

    pub fn commit_path(&mut self, path: PathBuf) {
        let previous = self.current_path.clone();
        match self.navigation_kind {
            NavigationKind::Normal => {
                if let Some(previous) = previous.as_ref()
                    && previous != &path
                {
                    self.back_history.push(previous.clone());
                    self.forward_history.clear();
                }
            }
            NavigationKind::Back => {
                if self.back_history.last() == Some(&path) {
                    self.back_history.pop();
                    if let Some(previous) = previous {
                        self.forward_history.push(previous);
                    }
                }
            }
            NavigationKind::Forward => {
                if self.forward_history.last() == Some(&path) {
                    self.forward_history.pop();
                    if let Some(previous) = previous {
                        self.back_history.push(previous);
                    }
                }
            }
            NavigationKind::Refresh => {}
        }
        self.current_path = Some(path);
        self.requested_path = None;
        self.navigation_kind = NavigationKind::Normal;
        self.selected.clear();
        self.focused = None;
        self.scroll_offset = 0.0;
    }

    pub fn back_target(&self) -> Option<PathBuf> {
        self.back_history.last().cloned()
    }

    pub fn forward_target(&self) -> Option<PathBuf> {
        self.forward_history.last().cloned()
    }

    pub fn discard_pending(&mut self) {
        self.pending_entries.clear();
        self.entry_paths = self
            .entries
            .iter()
            .map(|entry| (entry.id, entry.path.clone()))
            .collect();
    }

    pub fn entry_path(&self, entry_id: EntryId) -> Option<PathBuf> {
        self.entry_paths.get(&entry_id).cloned()
    }
}

impl Drop for TabSession {
    fn drop(&mut self) {
        self.cancel_pending();
    }
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub id: EntryId,
    pub original_name: OsString,
    pub display_name: String,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub size_bytes: Option<u64>,
    pub modified: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_only_accepts_its_latest_request() {
        let mut session = TabSession::new(TabId(7));
        let (first, first_cancel) =
            session.begin_navigation(PathBuf::from("first"), NavigationKind::Normal);
        let (second, _) = session.begin_navigation(PathBuf::from("second"), NavigationKind::Normal);

        assert!(first_cancel.load(Ordering::Acquire));
        assert!(!session.accepts(first));
        assert!(session.accepts(second));
    }

    #[test]
    fn failed_navigation_can_keep_last_successful_page() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from("good"));
        session.begin_navigation(PathBuf::from("missing"), NavigationKind::Normal);
        session.load_state = LoadState::NotFound;

        assert_eq!(session.current_path, Some(PathBuf::from("good")));
        assert_eq!(session.requested_path, Some(PathBuf::from("missing")));
    }

    #[test]
    fn entry_id_resolves_original_path() {
        let mut session = TabSession::new(TabId(1));
        let original = PathBuf::from("中文").join("📁");
        session.replace_entries(vec![FileEntry {
            id: EntryId(9),
            original_name: OsString::from("📁"),
            display_name: "📁".to_owned(),
            path: original.clone(),
            kind: EntryKind::Directory,
            size_bytes: None,
            modified: None,
        }]);

        assert_eq!(session.entry_path(EntryId(9)), Some(original));
        assert_eq!(session.entries[0].original_name, OsString::from("📁"));
    }

    #[test]
    fn back_and_forward_only_move_history_after_success() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from("two"));
        session.back_history = vec![PathBuf::from("one")];

        session.begin_navigation(PathBuf::from("one"), NavigationKind::Back);
        assert_eq!(session.back_history, vec![PathBuf::from("one")]);
        assert!(session.forward_history.is_empty());

        session.commit_path(PathBuf::from("one"));
        assert!(session.back_history.is_empty());
        assert_eq!(session.forward_history, vec![PathBuf::from("two")]);

        session.begin_navigation(PathBuf::from("two"), NavigationKind::Forward);
        session.commit_path(PathBuf::from("two"));
        assert_eq!(session.back_history, vec![PathBuf::from("one")]);
        assert!(session.forward_history.is_empty());
    }

    #[test]
    fn failed_history_navigation_leaves_both_stacks_unchanged() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from("two"));
        session.back_history = vec![PathBuf::from("missing")];
        session.forward_history = vec![PathBuf::from("three")];

        session.begin_navigation(PathBuf::from("missing"), NavigationKind::Back);
        session.load_state = LoadState::NotFound;

        assert_eq!(session.current_path, Some(PathBuf::from("two")));
        assert_eq!(session.back_history, vec![PathBuf::from("missing")]);
        assert_eq!(session.forward_history, vec![PathBuf::from("three")]);
    }

    #[test]
    fn refresh_does_not_change_history() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from("two"));
        session.back_history = vec![PathBuf::from("one")];
        session.forward_history = vec![PathBuf::from("three")];

        session.begin_navigation(PathBuf::from("two"), NavigationKind::Refresh);
        session.commit_path(PathBuf::from("two"));

        assert_eq!(session.back_history, vec![PathBuf::from("one")]);
        assert_eq!(session.forward_history, vec![PathBuf::from("three")]);
    }
    #[test]
    fn successful_path_commit_updates_tab_history() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from("one"));
        session.commit_path(PathBuf::from("two"));
        assert_eq!(session.back_history, vec![PathBuf::from("one")]);
        assert!(session.forward_history.is_empty());
    }
}
