pub mod file_operations;
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsString,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EntryId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Files,
    Settings,
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressMode {
    Normal,
    Smart,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchScope {
    Global,
    Directory(PathBuf),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchDepth {
    CurrentFolder,
    Recursive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageSource {
    Directory,
    Search,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SearchState {
    Waiting,
    Searching,
    Partial,
    Complete,
    NoResults,
    NotConfigured,
    Disconnected,
    NotIndexed,
    SyntaxError,
    TimedOut,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderSizeState {
    Unknown,
    Querying,
    Value(u64),
    NotIndexed,
    NotFound,
    TimedOut,
    Disconnected,
    ProtocolError,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameHighlightSegment {
    pub text: String,
    pub highlighted: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileVisibility {
    pub show_hidden: bool,
    pub show_system: bool,
}

impl Default for FileVisibility {
    fn default() -> Self {
        Self {
            show_hidden: true,
            show_system: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingConfig {
    pub executable_path: Option<PathBuf>,
    pub instance_name: String,
    pub verified_version: Option<String>,
    pub allow_launch: bool,
}

impl Default for EverythingConfig {
    fn default() -> Self {
        Self {
            executable_path: None,
            instance_name: "1.5a".to_owned(),
            verified_version: None,
            allow_launch: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortField {
    Name,
    Kind,
    Size,
    Modified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

#[derive(Debug)]
struct DirectorySnapshot {
    entries: Arc<Vec<FileEntry>>,
    error: Option<String>,
    selected: Vec<EntryId>,
    focused: Option<EntryId>,
    selection_anchor: Option<EntryId>,
}
#[derive(Debug)]
pub struct TabSession {
    pub id: TabId,
    pub kind: TabKind,
    pub latest_request: RequestId,
    pub current_path: Option<PathBuf>,
    pub requested_path: Option<PathBuf>,
    pub address_editing: bool,
    pub address_input: String,
    pub address_mode: AddressMode,
    pub search_scope: SearchScope,
    pub search_depth: SearchDepth,
    pub search_query: String,
    pub page_source: PageSource,
    pub search_state: SearchState,
    pub search_total: Option<u32>,
    pub search_file_total: Option<u32>,
    pub search_requested_pages: HashSet<u32>,
    pub search_pending_pages: VecDeque<u32>,
    pub search_cached_pages: VecDeque<u32>,
    directory_snapshot: Option<DirectorySnapshot>,
    pub navigation_kind: NavigationKind,
    pub back_history: Vec<PathBuf>,
    pub forward_history: Vec<PathBuf>,
    pub entries: Arc<Vec<FileEntry>>,
    pub pending_entries: Vec<FileEntry>,
    pub entry_indices: HashMap<EntryId, usize>,
    pub load_state: LoadState,
    pub error: Option<String>,
    pub selected: Vec<EntryId>,
    pub focused: Option<EntryId>,
    pub selection_anchor: Option<EntryId>,
    pub sort_field: SortField,
    pub sort_direction: SortDirection,
    pub search_sort_field: SortField,
    pub search_sort_direction: SortDirection,
    cancel: Option<Arc<AtomicBool>>,
}

impl TabSession {
    pub fn new(id: TabId) -> Self {
        Self {
            id,
            kind: TabKind::Files,
            latest_request: RequestId(0),
            current_path: None,
            requested_path: None,
            address_editing: false,
            address_input: String::new(),
            address_mode: AddressMode::Normal,
            search_scope: SearchScope::Global,
            search_depth: SearchDepth::Recursive,
            search_query: String::new(),
            page_source: PageSource::Directory,
            search_state: SearchState::Waiting,
            search_total: None,
            search_file_total: None,
            search_requested_pages: HashSet::new(),
            search_pending_pages: VecDeque::new(),
            search_cached_pages: VecDeque::new(),
            directory_snapshot: None,
            navigation_kind: NavigationKind::Normal,
            back_history: Vec::new(),
            forward_history: Vec::new(),
            entries: Arc::new(Vec::new()),
            pending_entries: Vec::new(),
            entry_indices: HashMap::new(),
            load_state: LoadState::Idle,
            error: None,
            selected: Vec::new(),
            focused: None,
            selection_anchor: None,
            sort_field: SortField::Name,
            sort_direction: SortDirection::Ascending,
            search_sort_field: SortField::Name,
            search_sort_direction: SortDirection::Descending,
            cancel: None,
        }
    }

    pub fn new_settings(id: TabId) -> Self {
        let mut tab = Self::new(id);
        tab.kind = TabKind::Settings;
        tab
    }

    pub fn duplicate_complete(id: TabId, source: &Self) -> Self {
        debug_assert_eq!(source.load_state, LoadState::Complete);
        Self {
            id,
            kind: TabKind::Files,
            latest_request: source.latest_request,
            current_path: source.current_path.clone(),
            requested_path: None,
            address_editing: false,
            address_input: String::new(),
            address_mode: AddressMode::Normal,
            search_scope: SearchScope::Global,
            search_depth: SearchDepth::Recursive,
            search_query: String::new(),
            page_source: PageSource::Directory,
            search_state: SearchState::Waiting,
            search_total: None,
            search_file_total: None,
            search_requested_pages: HashSet::new(),
            search_pending_pages: VecDeque::new(),
            search_cached_pages: VecDeque::new(),
            directory_snapshot: None,
            navigation_kind: NavigationKind::Normal,
            back_history: source.back_history.clone(),
            forward_history: source.forward_history.clone(),
            entries: source.entries.clone(),
            pending_entries: Vec::new(),
            entry_indices: source.entry_indices.clone(),
            load_state: LoadState::Complete,
            error: source.error.clone(),
            selected: source.selected.clone(),
            focused: source.focused,
            selection_anchor: source.selection_anchor,
            sort_field: source.sort_field,
            sort_direction: source.sort_direction,
            search_sort_field: source.search_sort_field,
            search_sort_direction: source.search_sort_direction,
            cancel: None,
        }
    }
    pub fn begin_smart_address_edit(&mut self) {
        self.address_editing = true;
        self.address_mode = AddressMode::Smart;
        self.search_scope = self
            .visible_path()
            .map(|path| SearchScope::Directory(path.to_path_buf()))
            .unwrap_or(SearchScope::Global);
        self.search_depth = SearchDepth::Recursive;
        self.search_query.clear();
        self.address_input = search_address_text(&self.search_scope, &self.search_query);
        self.search_state = SearchState::Waiting;
    }

    pub fn visible_path(&self) -> Option<&Path> {
        if matches!(
            self.load_state,
            LoadState::Loading
                | LoadState::Partial
                | LoadState::Cancelled
                | LoadState::NotFound
                | LoadState::PermissionDenied
                | LoadState::Disconnected
                | LoadState::Failed
        ) {
            self.requested_path
                .as_deref()
                .or(self.current_path.as_deref())
        } else {
            self.current_path.as_deref()
        }
    }

    pub fn has_failed_location(&self) -> bool {
        self.requested_path.is_some()
            && matches!(
                self.load_state,
                LoadState::Cancelled
                    | LoadState::NotFound
                    | LoadState::PermissionDenied
                    | LoadState::Disconnected
                    | LoadState::Failed
            )
    }

    pub fn restore_successful_location(&mut self) -> bool {
        if !self.has_failed_location() || self.current_path.is_none() {
            return false;
        }
        if let Some(failed_path) = self.requested_path.take()
            && self.forward_history.last() != Some(&failed_path)
        {
            self.forward_history.push(failed_path);
        }
        self.pending_entries.clear();
        self.load_state = LoadState::Complete;
        self.error = None;
        self.navigation_kind = NavigationKind::Normal;
        self.address_editing = false;
        self.address_input.clear();
        self.cancel = None;
        true
    }

    pub fn breadcrumb_paths(&self) -> Vec<(String, PathBuf)> {
        let Some(path) = self.visible_path() else {
            return Vec::new();
        };
        let mut segments = Vec::new();
        let mut cursor = PathBuf::new();
        for component in path.components() {
            cursor.push(component.as_os_str());
            let label = match component {
                std::path::Component::Prefix(_) => continue,
                std::path::Component::RootDir => display_path(&cursor),
                _ => component.as_os_str().to_string_lossy().into_owned(),
            };
            if !label.is_empty()
                && segments
                    .last()
                    .is_none_or(|(_, previous)| previous != &cursor)
            {
                segments.push((label, cursor.clone()));
            }
        }
        if segments.is_empty() && !path.as_os_str().is_empty() {
            segments.push((display_path(path), path.to_path_buf()));
        }
        segments
    }

    pub fn update_address_input(&mut self, input: String) {
        if self.address_mode == AddressMode::Smart {
            match &self.search_scope {
                SearchScope::Directory(path) => {
                    let prefix = search_scope_prefix(path);
                    if let Some(query) = input.strip_prefix(&prefix) {
                        self.search_query = query.to_owned();
                    } else {
                        self.search_scope = SearchScope::Global;
                        self.search_query = input.clone();
                    }
                }
                SearchScope::Global => self.search_query.clone_from(&input),
            }
        }
        self.address_input = input;
    }

    pub fn toggle_search_depth(&mut self) -> bool {
        if !matches!(self.search_scope, SearchScope::Directory(_)) {
            return false;
        }
        self.search_depth = match self.search_depth {
            SearchDepth::CurrentFolder => SearchDepth::Recursive,
            SearchDepth::Recursive => SearchDepth::CurrentFolder,
        };
        true
    }

    pub fn begin_search(
        &mut self,
        scope: SearchScope,
        query: String,
    ) -> (RequestId, Arc<AtomicBool>) {
        self.cancel_pending();
        self.latest_request.0 += 1;
        if self.page_source == PageSource::Directory {
            self.directory_snapshot = Some(DirectorySnapshot {
                entries: self.entries.clone(),
                error: self.error.clone(),
                selected: self.selected.clone(),
                focused: self.focused,
                selection_anchor: self.selection_anchor,
            });
        }
        self.page_source = PageSource::Search;
        self.address_mode = AddressMode::Smart;
        self.address_editing = true;
        self.search_scope = scope;
        self.search_query = query;
        self.address_input = search_address_text(&self.search_scope, &self.search_query);
        self.search_state = SearchState::Searching;
        self.search_total = None;
        self.search_file_total = None;
        self.search_requested_pages.clear();
        self.search_requested_pages.insert(0);
        self.search_pending_pages.clear();
        self.search_cached_pages.clear();
        self.error = None;
        self.pending_entries.clear();
        self.replace_entries(Vec::new());
        self.clear_selection();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        (self.latest_request, cancel)
    }

    pub fn search_cancel_token(&self) -> Option<Arc<AtomicBool>> {
        self.cancel.clone()
    }
    pub fn accepts_page(&self, request_id: RequestId, source: PageSource) -> bool {
        self.page_source == source && self.accepts(request_id)
    }

    #[allow(dead_code)]
    pub fn finish_search(&mut self) {
        self.commit_pending();
        self.search_state = if self.entries.is_empty() {
            SearchState::NoResults
        } else {
            SearchState::Complete
        };
    }

    pub fn cancel_address_edit(&mut self) {
        let was_search = self.page_source == PageSource::Search;
        self.cancel_pending();
        if was_search {
            self.latest_request.0 += 1;
        }
        if let Some(snapshot) = self.directory_snapshot.take() {
            self.entries = snapshot.entries;
            self.error = snapshot.error;
            self.selected = snapshot.selected;
            self.focused = snapshot.focused;
            self.selection_anchor = snapshot.selection_anchor;
            self.rebuild_entry_indices();
        }
        self.page_source = PageSource::Directory;
        self.address_editing = false;
        self.address_mode = AddressMode::Normal;
        self.address_input.clear();
        self.search_query.clear();
        self.search_total = None;
        self.search_file_total = None;
        self.search_requested_pages.clear();
        self.search_pending_pages.clear();
        self.search_cached_pages.clear();
        self.search_state = SearchState::Waiting;
    }

    pub fn begin_navigation(
        &mut self,
        path: PathBuf,
        kind: NavigationKind,
    ) -> (RequestId, Arc<AtomicBool>) {
        self.cancel_pending();
        self.latest_request.0 += 1;
        self.requested_path = Some(path);
        self.page_source = PageSource::Directory;
        self.directory_snapshot = None;
        self.search_total = None;
        self.search_file_total = None;
        self.search_requested_pages.clear();
        self.search_pending_pages.clear();
        self.search_cached_pages.clear();
        self.address_mode = AddressMode::Normal;
        self.navigation_kind = kind;
        self.load_state = LoadState::Loading;
        self.error = None;
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
                .is_some_and(|cancel| cancel.load(AtomicOrdering::Acquire))
    }

    pub fn cancel_pending(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, AtomicOrdering::Release);
            if self.page_source == PageSource::Search
                && matches!(
                    self.search_state,
                    SearchState::Searching | SearchState::Partial
                )
            {
                self.search_state = SearchState::Cancelled;
            } else if matches!(self.load_state, LoadState::Loading | LoadState::Partial) {
                self.load_state = LoadState::Cancelled;
            }
        }
    }

    pub fn replace_entries(&mut self, entries: Vec<FileEntry>) {
        self.entry_indices = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
        self.entries = Arc::new(entries);
        self.reconcile_selection();
    }

    pub fn append_pending(&mut self, mut entries: Vec<FileEntry>) -> usize {
        let start = self.pending_entries.len();
        for (offset, entry) in entries.iter().enumerate() {
            self.entry_indices.insert(entry.id, start + offset);
        }
        self.pending_entries.append(&mut entries);
        if self.page_source == PageSource::Search {
            self.search_state = SearchState::Partial;
        } else {
            self.load_state = LoadState::Partial;
        }
        start
    }

    pub fn merge_search_page(
        &mut self,
        offset: u32,
        entries: Vec<FileEntry>,
        total: u32,
        file_total: u32,
        page_size: u32,
    ) -> Vec<u32> {
        const CACHE_PAGE_LIMIT: usize = 7;
        let selected_paths = self
            .selected
            .iter()
            .filter_map(|id| {
                self.visible_entry(*id)
                    .map(|entry| (*id, entry.path.clone()))
            })
            .collect::<HashMap<_, _>>();
        let current = Arc::make_mut(&mut self.entries);
        for entry in entries {
            if let Some(index) = current.iter().position(|existing| existing.id == entry.id) {
                current[index] = entry;
            } else {
                current.push(entry);
            }
        }
        self.search_cached_pages.retain(|cached| *cached != offset);
        self.search_cached_pages.push_back(offset);
        let mut evicted = Vec::new();
        while self.search_cached_pages.len() > CACHE_PAGE_LIMIT {
            let Some(position) = self.search_cached_pages.iter().position(|cached| {
                let end = cached.saturating_add(page_size);
                !self.selected.iter().any(|id| id.0 > *cached && id.0 <= end)
            }) else {
                break;
            };
            let cached = self.search_cached_pages.remove(position).unwrap();
            let end = cached.saturating_add(page_size);
            current.retain(|entry| entry.id.0 <= cached || entry.id.0 > end);
            evicted.push(cached);
        }
        current.sort_unstable_by_key(|entry| entry.id.0);
        self.rebuild_entry_indices();
        let entry_paths = self
            .entries
            .iter()
            .map(|entry| (entry.id, entry.path.clone()))
            .collect::<HashMap<_, _>>();
        self.selected.retain(|id| {
            selected_paths
                .get(id)
                .is_none_or(|expected_path| entry_paths.get(id) == Some(expected_path))
        });
        if self.focused.is_some_and(|id| !self.selected.contains(&id)) {
            self.focused = None;
        }
        if self
            .selection_anchor
            .is_some_and(|id| !self.selected.contains(&id))
        {
            self.selection_anchor = None;
        }
        self.search_total = Some(total);
        self.search_file_total = Some(file_total);
        let loaded = self.entries.len().try_into().unwrap_or(u32::MAX);
        self.search_state = if total == 0 {
            SearchState::NoResults
        } else if loaded >= total {
            SearchState::Complete
        } else {
            SearchState::Partial
        };
        evicted
    }

    pub fn queue_search_pages(&mut self, offsets: &[u32], page_size: u32) -> Option<u32> {
        if self.page_source != PageSource::Search {
            return None;
        }
        let mut candidates = offsets
            .iter()
            .map(|offset| offset / page_size * page_size)
            .filter(|offset| self.search_total.is_none_or(|total| *offset < total))
            .filter(|offset| !self.search_cached_pages.contains(offset))
            .filter(|offset| !self.search_requested_pages.contains(offset))
            .collect::<VecDeque<_>>();
        let mut seen = HashSet::new();
        candidates.retain(|offset| seen.insert(*offset));
        self.search_pending_pages.clear();
        if !self.search_requested_pages.is_empty() {
            self.search_pending_pages = candidates;
            return None;
        }
        let offset = candidates.pop_front()?;
        self.search_requested_pages.insert(offset);
        self.search_pending_pages = candidates;
        Some(offset)
    }

    pub fn finish_search_page_request(&mut self, offset: u32) -> Option<u32> {
        self.search_requested_pages.remove(&offset);
        while let Some(next) = self.search_pending_pages.pop_front() {
            if self.search_cached_pages.contains(&next)
                || self.search_total.is_some_and(|total| next >= total)
            {
                continue;
            }
            self.search_requested_pages.insert(next);
            return Some(next);
        }
        None
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
                if let Some(position) = self.back_history.iter().rposition(|item| item == &path) {
                    let traversed = self.back_history.drain(position..).collect::<Vec<_>>();
                    if let Some(previous) = previous {
                        self.forward_history.push(previous);
                    }
                    self.forward_history.extend(traversed.into_iter().skip(1));
                }
            }
            NavigationKind::Forward => {
                if let Some(position) = self.forward_history.iter().rposition(|item| item == &path)
                {
                    let traversed = self.forward_history.drain(position..).collect::<Vec<_>>();
                    if let Some(previous) = previous {
                        self.back_history.push(previous);
                    }
                    self.back_history.extend(traversed.into_iter().skip(1));
                }
            }
            NavigationKind::Refresh => {}
        }
        let was_refresh = self.navigation_kind == NavigationKind::Refresh;
        self.current_path = Some(path);
        self.requested_path = None;
        self.address_editing = false;
        self.address_input.clear();
        self.navigation_kind = NavigationKind::Normal;
        if !was_refresh {
            self.clear_selection();
        }
    }

    pub fn back_target(&self) -> Option<PathBuf> {
        self.back_history.last().cloned()
    }
    pub fn forward_target(&self) -> Option<PathBuf> {
        self.forward_history.last().cloned()
    }

    pub fn discard_pending(&mut self) {
        self.pending_entries.clear();
        self.entry_indices = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
    }

    pub fn visible_entries(&self) -> &[FileEntry] {
        if matches!(self.load_state, LoadState::Partial) {
            &self.pending_entries
        } else {
            &self.entries
        }
    }

    pub fn visible_entry(&self, entry_id: EntryId) -> Option<&FileEntry> {
        self.visible_entry_index(entry_id)
            .and_then(|index| self.visible_entries().get(index))
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.focused = None;
        self.selection_anchor = None;
    }

    pub fn select_all(&mut self) {
        self.selected = self.entries.iter().map(|entry| entry.id).collect();
        self.focused = self.entries.first().map(|entry| entry.id);
        self.selection_anchor = self.focused;
    }

    pub fn select_entry(&mut self, entry_id: EntryId, toggle: bool, extend: bool) {
        if !self.entry_indices.contains_key(&entry_id) {
            return;
        }
        self.focused = Some(entry_id);
        if extend {
            let anchor = self.selection_anchor.unwrap_or(entry_id);
            let range = self.range_ids(anchor, entry_id);
            if toggle {
                for id in range {
                    if !self.selected.contains(&id) {
                        self.selected.push(id);
                    }
                }
            } else {
                self.selected = range;
            }
            self.selection_anchor = Some(anchor);
        } else if toggle {
            if let Some(index) = self.selected.iter().position(|id| *id == entry_id) {
                self.selected.remove(index);
            } else {
                self.selected.push(entry_id);
            }
            self.selection_anchor = Some(entry_id);
        } else {
            self.selected.clear();
            self.selected.push(entry_id);
            self.selection_anchor = Some(entry_id);
        }
    }

    pub fn move_focus(&mut self, delta: isize, extend: bool) {
        if self.entries.is_empty() {
            return;
        }
        let current = self
            .focused
            .and_then(|id| self.entry_index(id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(self.entries.len() - 1);
        let id = self.entries[next].id;
        self.focused = Some(id);
        if extend {
            let anchor = self
                .selection_anchor
                .unwrap_or_else(|| self.entries[current].id);
            self.selection_anchor = Some(anchor);
            self.selected = self.range_ids(anchor, id);
        } else {
            self.selected = vec![id];
            self.selection_anchor = Some(id);
        }
    }

    pub fn focus_boundary(&mut self, last: bool, extend: bool) {
        if self.entries.is_empty() {
            return;
        }
        let id = if last {
            self.entries.last()
        } else {
            self.entries.first()
        }
        .expect("non-empty")
        .id;
        if extend {
            let anchor = self.selection_anchor.or(self.focused).unwrap_or(id);
            self.selection_anchor = Some(anchor);
            self.selected = self.range_ids(anchor, id);
            self.focused = Some(id);
        } else {
            self.selected = vec![id];
            self.focused = Some(id);
            self.selection_anchor = Some(id);
        }
    }

    pub fn toggle_focused(&mut self) {
        if let Some(id) = self.focused {
            self.select_entry(id, true, false);
        }
    }

    pub fn set_sort(&mut self, field: SortField) {
        if self.sort_field == field {
            self.sort_direction = match self.sort_direction {
                SortDirection::Ascending => SortDirection::Descending,
                SortDirection::Descending => SortDirection::Ascending,
            };
        } else {
            self.sort_field = field;
            self.sort_direction = SortDirection::Ascending;
        }
        let field = self.sort_field;
        let direction = self.sort_direction;
        Arc::make_mut(&mut self.entries)
            .sort_unstable_by(|left, right| compare_entries(left, right, field, direction));
        self.rebuild_entry_indices();
    }

    pub fn set_search_sort(&mut self, field: SortField) {
        if self.search_sort_field == field {
            self.search_sort_direction = match self.search_sort_direction {
                SortDirection::Ascending => SortDirection::Descending,
                SortDirection::Descending => SortDirection::Ascending,
            };
        } else {
            self.search_sort_field = field;
            self.search_sort_direction = SortDirection::Ascending;
        }
    }
    pub fn sort_pending(&mut self) {
        let field = self.sort_field;
        let direction = self.sort_direction;
        self.pending_entries
            .sort_unstable_by(|left, right| compare_entries(left, right, field, direction));
    }
    pub fn resort_entries(&mut self) {
        let field = self.sort_field;
        let direction = self.sort_direction;
        Arc::make_mut(&mut self.entries)
            .sort_unstable_by(|left, right| compare_entries(left, right, field, direction));
        self.rebuild_entry_indices();
    }

    fn range_ids(&self, left: EntryId, right: EntryId) -> Vec<EntryId> {
        let Some(left) = self.entry_index(left) else {
            return Vec::new();
        };
        let Some(right) = self.entry_index(right) else {
            return Vec::new();
        };
        let (start, end) = if left <= right {
            (left, right)
        } else {
            (right, left)
        };
        self.visible_entries()[start..=end]
            .iter()
            .map(|entry| entry.id)
            .collect()
    }

    pub fn visible_entry_index(&self, id: EntryId) -> Option<usize> {
        self.entry_indices.get(&id).copied()
    }

    fn entry_index(&self, id: EntryId) -> Option<usize> {
        self.entry_indices.get(&id).copied()
    }

    fn rebuild_entry_indices(&mut self) {
        self.entry_indices = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
    }

    fn reconcile_selection(&mut self) {
        self.selected
            .retain(|id| self.entry_indices.contains_key(id));
        if self
            .focused
            .is_some_and(|id| !self.entry_indices.contains_key(&id))
        {
            self.focused = None;
        }
        if self
            .selection_anchor
            .is_some_and(|id| !self.entry_indices.contains_key(&id))
        {
            self.selection_anchor = None;
        }
    }
}

fn entry_type_key(entry: &FileEntry) -> String {
    if entry.kind == EntryKind::Directory {
        return "folder".to_owned();
    }
    entry
        .path
        .extension()
        .map(|extension| extension.to_string_lossy().to_lowercase())
        .unwrap_or_default()
}

fn entry_size_key(entry: &FileEntry) -> (u8, u64) {
    match (entry.kind, entry.folder_size, entry.size_bytes) {
        (EntryKind::Directory, FolderSizeState::Value(size), _) => (0, size),
        (EntryKind::Directory, _, _) => (1, 0),
        (_, _, Some(size)) => (0, size),
        _ => (1, 0),
    }
}
fn compare_entries(
    left: &FileEntry,
    right: &FileEntry,
    field: SortField,
    direction: SortDirection,
) -> Ordering {
    if field != SortField::Kind {
        let directory_order =
            (left.kind != EntryKind::Directory).cmp(&(right.kind != EntryKind::Directory));
        if directory_order != Ordering::Equal {
            return directory_order;
        }
    }
    let value_order = match field {
        SortField::Name => left
            .display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase()),
        SortField::Kind => entry_type_key(left).cmp(&entry_type_key(right)),
        SortField::Size => entry_size_key(left).cmp(&entry_size_key(right)),
        SortField::Modified => left.modified.cmp(&right.modified),
    };
    let value_order = match direction {
        SortDirection::Ascending => value_order,
        SortDirection::Descending => {
            if field == SortField::Size && entry_size_key(left).0 != entry_size_key(right).0 {
                value_order
            } else {
                value_order.reverse()
            }
        }
    };
    value_order.then_with(|| {
        left.display_name
            .to_lowercase()
            .cmp(&right.display_name.to_lowercase())
    })
}

fn search_scope_prefix(path: &Path) -> String {
    format!("{} ", display_path(path))
}

pub fn search_address_text(scope: &SearchScope, query: &str) -> String {
    match scope {
        SearchScope::Directory(path) => format!("{}{}", search_scope_prefix(path), query),
        SearchScope::Global => query.to_owned(),
    }
}
fn display_path(path: &Path) -> String {
    path.as_os_str().to_string_lossy().into_owned()
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
    pub name_highlights: Vec<NameHighlightSegment>,
    pub path: PathBuf,
    pub kind: EntryKind,
    pub open_target: Option<PathBuf>,
    pub parent_display: String,
    pub size_bytes: Option<u64>,
    pub folder_size: FolderSizeState,
    pub modified: Option<std::time::SystemTime>,
}

impl FileEntry {
    pub fn accepts_folder_size(&self, entry_id: EntryId, original_path: &Path) -> bool {
        self.id == entry_id && self.path == original_path
    }

    pub fn set_folder_size(
        &mut self,
        entry_id: EntryId,
        original_path: &Path,
        state: FolderSizeState,
    ) -> bool {
        if !self.accepts_folder_size(entry_id, original_path) {
            return false;
        }
        self.folder_size = state;
        true
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntryKind {
    Directory,
    File,
    Other,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: u32, name: &str, kind: EntryKind, size: Option<u64>) -> FileEntry {
        FileEntry {
            id: EntryId(id),
            original_name: name.into(),
            display_name: name.into(),
            name_highlights: Vec::new(),
            path: PathBuf::from(name),
            kind,
            open_target: None,
            parent_display: String::new(),
            size_bytes: size,
            folder_size: FolderSizeState::Unknown,
            modified: None,
        }
    }

    #[test]
    fn search_requests_are_isolated_by_request_and_page_source() {
        let mut session = TabSession::new(TabId(7));
        let (first, first_cancel) = session.begin_search(
            SearchScope::Directory(PathBuf::from(r"C:\范围 (一)")),
            "*.blend size:>100mb".to_owned(),
        );
        assert!(session.accepts_page(first, PageSource::Search));
        assert!(!session.accepts_page(first, PageSource::Directory));

        let (second, _) = session.begin_search(SearchScope::Global, "ext:png|jpg".to_owned());
        assert!(first_cancel.load(AtomicOrdering::Acquire));
        assert!(!session.accepts_page(first, PageSource::Search));
        assert!(session.accepts_page(second, PageSource::Search));
        assert_eq!(session.search_scope, SearchScope::Global);
        assert_eq!(session.search_query, "ext:png|jpg");
    }

    #[test]
    fn search_address_keeps_directory_prefix_for_drive_path() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"D:\Assets"));
        session.begin_smart_address_edit();
        assert_eq!(session.address_input, r"D:\Assets ");
        session.update_address_input(r"D:\Assets *.blend".to_owned());
        assert_eq!(
            session.search_scope,
            SearchScope::Directory(PathBuf::from(r"D:\Assets"))
        );
        assert_eq!(session.search_query, "*.blend");
        let (_, _) =
            session.begin_search(session.search_scope.clone(), session.search_query.clone());
        assert_eq!(session.address_input, r"D:\Assets *.blend");
    }

    #[test]
    fn smart_address_search_depth_defaults_to_recursive_and_only_toggles_for_a_directory() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"D:\Assets"));
        session.load_state = LoadState::Complete;
        session.begin_smart_address_edit();
        assert_eq!(session.search_depth, SearchDepth::Recursive);
        assert!(session.toggle_search_depth());
        assert_eq!(session.search_depth, SearchDepth::CurrentFolder);
        assert_eq!(session.address_input, r"D:\Assets ");

        session.update_address_input("*.blend".to_owned());
        assert_eq!(session.search_scope, SearchScope::Global);
        assert!(!session.toggle_search_depth());
        assert_eq!(session.search_depth, SearchDepth::CurrentFolder);
    }

    #[test]
    fn search_address_does_not_split_paths_containing_spaces() {
        let scope = SearchScope::Directory(PathBuf::from(r"D:\My Assets (A)!|"));
        let mut session = TabSession::new(TabId(1));
        session.begin_search(scope.clone(), "ext:png|jpg".to_owned());
        assert_eq!(session.address_input, r"D:\My Assets (A)!| ext:png|jpg");
        session.update_address_input(r"D:\My Assets (A)!| ext:png|jpg size:>1mb".to_owned());
        assert_eq!(session.search_scope, scope);
        assert_eq!(session.search_query, "ext:png|jpg size:>1mb");
    }

    #[test]
    fn search_address_keeps_unc_prefix_until_the_complete_prefix_is_deleted() {
        let path = PathBuf::from(r"\\LiuYanghomeNAS\Multimedia");
        let mut session = TabSession::new(TabId(1));
        session.begin_search(SearchScope::Directory(path.clone()), "*.mkv".to_owned());
        assert_eq!(session.address_input, r"\\LiuYanghomeNAS\Multimedia *.mkv");
        session.update_address_input(r"\\LiuYanghomeNAS\Multimedia *.mp4".to_owned());
        assert_eq!(session.search_scope, SearchScope::Directory(path));
        assert_eq!(session.search_query, "*.mp4");
        session.update_address_input("*.mkv".to_owned());
        assert_eq!(session.search_scope, SearchScope::Global);
        assert_eq!(session.search_query, "*.mkv");
    }
    #[test]
    fn search_first_page_is_visible_bounded_and_cancel_restores_directory() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"F:\CodeProjects\AsterFiles"));
        session.replace_entries(vec![entry(90, "directory-row", EntryKind::File, Some(1))]);
        session.select_entry(EntryId(90), false, false);
        let (request, _) = session.begin_search(SearchScope::Global, ".md".to_owned());

        let first_page = (1..=256)
            .map(|id| entry(id, &format!("result-{id}.md"), EntryKind::File, Some(1)))
            .collect();
        session.merge_search_page(0, first_page, 133_186, 100_000, 256);
        assert_eq!(session.entries.len(), 256);
        assert_eq!(session.search_total, Some(133_186));
        assert_eq!(session.page_source, PageSource::Search);
        assert_eq!(session.search_state, SearchState::Partial);

        session.cancel_address_edit();
        assert_eq!(session.page_source, PageSource::Directory);
        assert_eq!(session.entries.len(), 1);
        assert_eq!(session.entries[0].display_name, "directory-row");
        assert_eq!(session.selected, vec![EntryId(90)]);
        assert_eq!(session.search_total, None);
        assert!(!session.accepts_page(request, PageSource::Search));
        let late_page = vec![entry(999, "late.md", EntryKind::File, Some(1))];
        if session.accepts_page(request, PageSource::Search) {
            session.merge_search_page(768, late_page, 133_186, 100_000, 256);
        }
        assert_eq!(session.entries[0].display_name, "directory-row");
    }
    #[test]
    fn requesting_search_pages_keeps_identity_and_deduplicates_each_offset() {
        let mut session = TabSession::new(TabId(1));
        let (request, _) = session.begin_search(SearchScope::Global, ".md".into());
        session.merge_search_page(
            0,
            (1..=256)
                .map(|id| entry(id, &format!("result-{id}.md"), EntryKind::File, Some(1)))
                .collect(),
            133_186,
            100_000,
            256,
        );
        session.finish_search_page_request(0);
        assert_eq!(session.entries.len(), 256);
        assert_eq!(session.search_state, SearchState::Partial);
        assert_eq!(session.queue_search_pages(&[256, 0, 512], 256), Some(256));
        assert_eq!(session.queue_search_pages(&[1024, 768, 1280], 256), None);
        assert!(session.accepts_page(request, PageSource::Search));
        assert_eq!(session.finish_search_page_request(256), Some(1024));
        session.merge_search_page(
            256,
            (257..=512)
                .map(|id| entry(id, &format!("result-{id}.md"), EntryKind::File, Some(1)))
                .collect(),
            133_186,
            100_000,
            256,
        );
        assert_eq!(session.entries.len(), 512);
        assert_eq!(session.search_state, SearchState::Partial);
    }

    #[test]
    fn random_search_pages_merge_by_absolute_result_position() {
        let mut session = TabSession::new(TabId(1));
        session.begin_search(SearchScope::Global, ".blend".into());
        session.merge_search_page(
            512,
            (513..=514)
                .map(|id| entry(id, &format!("result-{id}.blend"), EntryKind::File, Some(1)))
                .collect(),
            100_000,
            80_000,
            256,
        );
        session.merge_search_page(
            0,
            (1..=2)
                .map(|id| entry(id, &format!("result-{id}.blend"), EntryKind::File, Some(1)))
                .collect(),
            100_000,
            80_000,
            256,
        );
        assert_eq!(
            session
                .entries
                .iter()
                .map(|entry| entry.id.0)
                .collect::<Vec<_>>(),
            vec![1, 2, 513, 514]
        );
        assert_eq!(session.entries.len(), 4);
        assert_eq!(session.search_total, Some(100_000));
    }

    #[test]
    fn search_page_cache_evicts_the_oldest_unselected_page() {
        let mut session = TabSession::new(TabId(1));
        session.begin_search(SearchScope::Global, ".blend".into());
        for page in 0..8 {
            let offset = page * 256;
            session.merge_search_page(
                offset,
                vec![entry(
                    offset + 1,
                    &format!("result-{page}.blend"),
                    EntryKind::File,
                    Some(1),
                )],
                100_000,
                80_000,
                256,
            );
        }
        assert_eq!(session.search_cached_pages.len(), 7);
        assert!(session.visible_entry(EntryId(1)).is_none());
        assert!(session.visible_entry(EntryId(257)).is_some());
    }
    #[test]
    fn folder_size_update_requires_entry_identity_and_original_path() {
        let original = PathBuf::from(r"\\server\共享\零字节");
        let mut item = FileEntry {
            id: EntryId(3),
            original_name: "零字节".into(),
            display_name: "零字节".into(),
            name_highlights: Vec::new(),
            path: original.clone(),
            kind: EntryKind::Directory,
            open_target: None,
            parent_display: r"\\server\共享".into(),
            size_bytes: None,
            folder_size: FolderSizeState::Querying,
            modified: None,
        };
        assert!(!item.set_folder_size(EntryId(4), &original, FolderSizeState::Value(0)));
        assert!(!item.set_folder_size(
            EntryId(3),
            Path::new(r"\\server\共享\别处"),
            FolderSizeState::Value(0)
        ));
        assert!(item.set_folder_size(EntryId(3), &original, FolderSizeState::Value(0)));
        assert_eq!(item.folder_size, FolderSizeState::Value(0));
    }
    #[test]
    fn partial_selection_uses_the_visible_batch_indices() {
        let mut session = TabSession::new(TabId(1));
        session.replace_entries(vec![entry(1, "old", EntryKind::File, Some(1))]);
        session.append_pending(vec![
            entry(2, "new-a", EntryKind::File, Some(2)),
            entry(3, "new-b", EntryKind::File, Some(3)),
        ]);

        session.select_entry(EntryId(2), false, false);
        session.select_entry(EntryId(3), false, true);

        assert_eq!(session.selected, vec![EntryId(2), EntryId(3)]);
    }
    #[test]
    fn visible_entry_uses_pending_entries_during_partial_load() {
        let mut session = TabSession::new(TabId(1));
        let old = entry(1, "old", EntryKind::Directory, None);
        let pending = entry(1, "pending", EntryKind::Directory, None);
        session.replace_entries(vec![old]);
        session.append_pending(vec![pending]);

        assert_eq!(
            session
                .visible_entry(EntryId(1))
                .map(|entry| entry.display_name.as_str()),
            Some("pending")
        );
        session.commit_pending();
        assert_eq!(
            session
                .visible_entry(EntryId(1))
                .map(|entry| entry.display_name.as_str()),
            Some("pending")
        );
    }

    #[test]
    fn session_only_accepts_latest_request() {
        let mut session = TabSession::new(TabId(7));
        let (first, first_cancel) =
            session.begin_navigation("first".into(), NavigationKind::Normal);
        let (second, _) = session.begin_navigation("second".into(), NavigationKind::Normal);
        assert!(first_cancel.load(AtomicOrdering::Acquire));
        assert!(!session.accepts(first));
        assert!(session.accepts(second));
    }

    #[test]
    fn refresh_commit_preserves_reconciled_selection_and_focus() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"C:\target"));
        session.replace_entries(vec![entry(1, "copied.txt", EntryKind::File, Some(1))]);
        session.select_entry(EntryId(1), false, false);

        session.begin_navigation(PathBuf::from(r"C:\target"), NavigationKind::Refresh);
        session.pending_entries = vec![entry(1, "copied.txt", EntryKind::File, Some(1))];
        session.commit_pending();
        session.commit_path(PathBuf::from(r"C:\target"));

        assert_eq!(session.selected, vec![EntryId(1)]);
        assert_eq!(session.focused, Some(EntryId(1)));
        assert_eq!(session.selection_anchor, Some(EntryId(1)));
    }

    #[test]
    fn normal_navigation_commit_clears_selection() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"C:\source"));
        session.replace_entries(vec![entry(1, "old.txt", EntryKind::File, Some(1))]);
        session.select_entry(EntryId(1), false, false);

        session.begin_navigation(PathBuf::from(r"C:\target"), NavigationKind::Normal);
        session.pending_entries = vec![entry(2, "new.txt", EntryKind::File, Some(1))];
        session.commit_pending();
        session.commit_path(PathBuf::from(r"C:\target"));

        assert!(session.selected.is_empty());
        assert_eq!(session.focused, None);
        assert_eq!(session.selection_anchor, None);
    }
    #[test]
    fn failed_location_is_visible_and_can_return_to_successful_content() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"C:\Users"));
        session.replace_entries(vec![entry(1, "DevUser", EntryKind::Directory, None)]);
        session.begin_navigation(PathBuf::from(r"C:\Users\DevUser"), NavigationKind::Normal);
        session.load_state = LoadState::PermissionDenied;

        assert_eq!(session.visible_path(), Some(Path::new(r"C:\Users\DevUser")));
        assert!(session.has_failed_location());
        assert!(session.restore_successful_location());
        assert_eq!(session.visible_path(), Some(Path::new(r"C:\Users")));
        assert_eq!(session.entries.len(), 1);
        assert_eq!(
            session.forward_target(),
            Some(PathBuf::from(r"C:\Users\DevUser"))
        );
    }
    #[test]
    fn retrying_failed_location_can_commit_it_normally() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"C:\Users"));
        session.begin_navigation(PathBuf::from(r"C:\Users\DevUser"), NavigationKind::Normal);
        session.load_state = LoadState::PermissionDenied;

        let requested = session
            .requested_path
            .clone()
            .expect("failed target exists");
        session.begin_navigation(requested.clone(), NavigationKind::Normal);
        session.commit_pending();
        session.commit_path(requested.clone());

        assert_eq!(session.visible_path(), Some(requested.as_path()));
        assert_eq!(session.back_target(), Some(PathBuf::from(r"C:\Users")));
        assert!(session.requested_path.is_none());
        assert_eq!(session.load_state, LoadState::Complete);
    }
    #[test]
    fn failed_navigation_keeps_successful_page_and_history() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some("good".into());
        session.back_history = vec!["older".into()];
        session.begin_navigation("missing".into(), NavigationKind::Normal);
        session.load_state = LoadState::NotFound;
        assert_eq!(session.current_path, Some("good".into()));
        assert_eq!(session.back_history, vec![PathBuf::from("older")]);
    }

    #[test]
    fn history_moves_only_after_success_and_supports_jumps() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some("three".into());
        session.back_history = vec!["one".into(), "two".into()];
        session.begin_navigation("one".into(), NavigationKind::Back);
        assert_eq!(session.back_history.len(), 2);
        session.commit_path("one".into());
        assert!(session.back_history.is_empty());
        assert_eq!(
            session.forward_history,
            vec![PathBuf::from("three"), PathBuf::from("two")]
        );
    }

    #[test]
    fn windows_root_breadcrumb_is_visible_without_duplicate_drive_prefix() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from(r"F:\"));
        let breadcrumbs = session.breadcrumb_paths();
        assert_eq!(
            breadcrumbs,
            vec![(r"F:\".to_owned(), PathBuf::from(r"F:\"))]
        );
    }
    #[test]
    fn address_edit_cancel_restores_successful_path() {
        let mut session = TabSession::new(TabId(1));
        session.current_path = Some(PathBuf::from("C:\\成功"));
        session.begin_smart_address_edit();
        session.update_address_input("C:\\不存在📁".to_owned());
        session.load_state = LoadState::NotFound;
        session.cancel_address_edit();
        assert!(!session.address_editing);
        assert_eq!(session.current_path, Some(PathBuf::from("C:\\成功")));
    }

    #[test]
    fn selection_supports_single_toggle_range_and_keyboard() {
        let mut session = TabSession::new(TabId(1));
        session.replace_entries(vec![
            entry(1, "a", EntryKind::File, Some(1)),
            entry(2, "b", EntryKind::File, Some(2)),
            entry(3, "c", EntryKind::File, Some(3)),
        ]);
        session.select_entry(EntryId(1), false, false);
        session.select_entry(EntryId(3), false, true);
        assert_eq!(session.selected, vec![EntryId(1), EntryId(2), EntryId(3)]);
        session.select_entry(EntryId(2), true, false);
        assert!(!session.selected.contains(&EntryId(2)));
        session.move_focus(-1, false);
        assert_eq!(session.focused, Some(EntryId(1)));
    }

    #[test]
    fn control_shift_range_adds_to_existing_selection_and_keeps_anchor() {
        let mut session = TabSession::new(TabId(1));
        session.replace_entries(vec![
            entry(1, "a", EntryKind::File, Some(1)),
            entry(2, "b", EntryKind::File, Some(2)),
            entry(3, "c", EntryKind::Directory, None),
            entry(4, "d", EntryKind::File, Some(4)),
            entry(5, "e", EntryKind::File, Some(5)),
        ]);

        session.select_entry(EntryId(1), false, false);
        session.select_entry(EntryId(5), true, false);
        session.select_entry(EntryId(3), true, true);

        assert_eq!(
            session.selected,
            vec![EntryId(1), EntryId(5), EntryId(3), EntryId(4)]
        );
        assert_eq!(session.selection_anchor, Some(EntryId(5)));
        assert_eq!(session.focused, Some(EntryId(3)));
    }
    #[test]
    fn sorting_keeps_directories_first_in_both_directions() {
        let mut session = TabSession::new(TabId(1));
        session.replace_entries(vec![
            entry(1, "small", EntryKind::File, Some(1)),
            entry(2, "folder", EntryKind::Directory, None),
            entry(3, "large", EntryKind::File, Some(10)),
        ]);
        session.set_sort(SortField::Size);
        assert_eq!(session.entries[0].kind, EntryKind::Directory);
        session.set_sort(SortField::Size);
        assert_eq!(session.entries[0].kind, EntryKind::Directory);
        assert_eq!(session.entries[1].display_name, "large");
    }
    #[test]
    fn type_sort_includes_folders_in_the_type_order() {
        let mut session = TabSession::new(TabId(1));
        session.replace_entries(vec![
            entry(1, "folder", EntryKind::Directory, None),
            entry(2, "readme", EntryKind::File, Some(1)),
            entry(3, "notes.txt", EntryKind::File, Some(1)),
        ]);

        session.set_sort(SortField::Kind);
        assert_eq!(
            session
                .entries
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["readme", "folder", "notes.txt"]
        );
        session.set_sort(SortField::Kind);
        assert_eq!(
            session
                .entries
                .iter()
                .map(|entry| entry.display_name.as_str())
                .collect::<Vec<_>>(),
            vec!["notes.txt", "folder", "readme"]
        );
    }
    #[test]
    fn size_sort_reorders_indexed_folders_within_the_folder_group() {
        let mut session = TabSession::new(TabId(1));
        let mut large_folder = entry(1, "large-folder", EntryKind::Directory, None);
        large_folder.folder_size = FolderSizeState::Value(20);
        let mut small_folder = entry(2, "small-folder", EntryKind::Directory, None);
        small_folder.folder_size = FolderSizeState::Value(5);
        session.replace_entries(vec![
            large_folder,
            entry(3, "file", EntryKind::File, Some(1)),
            small_folder,
        ]);

        session.set_sort(SortField::Size);
        assert_eq!(session.entries[0].display_name, "small-folder");
        assert_eq!(session.entries[1].display_name, "large-folder");
        session.set_sort(SortField::Size);
        assert_eq!(session.entries[0].display_name, "large-folder");
        assert_eq!(session.entries[1].display_name, "small-folder");
    }

    #[test]
    fn size_sort_places_unknown_folders_after_indexed_folders_and_can_resort_updates() {
        let mut session = TabSession::new(TabId(1));
        let mut indexed = entry(1, "indexed", EntryKind::Directory, None);
        indexed.folder_size = FolderSizeState::Value(20);
        session.replace_entries(vec![
            entry(2, "unknown", EntryKind::Directory, None),
            indexed,
        ]);
        session.set_sort(SortField::Size);
        assert_eq!(session.entries[0].display_name, "indexed");
        session.set_sort(SortField::Size);
        assert_eq!(session.entries[0].display_name, "indexed");
        assert_eq!(session.entries[1].display_name, "unknown");
        session.set_sort(SortField::Size);
        session.entries = Arc::new(vec![session.entries[0].clone(), {
            let mut updated = session.entries[1].clone();
            updated.folder_size = FolderSizeState::Value(5);
            updated
        }]);
        session.resort_entries();
        assert_eq!(session.entries[0].display_name, "unknown");
    }

    #[test]
    fn entry_id_resolves_original_unicode_path() {
        let mut session = TabSession::new(TabId(1));
        let original = PathBuf::from("中文").join("📁");
        session.replace_entries(vec![FileEntry {
            id: EntryId(9),
            original_name: "📁".into(),
            display_name: "📁".into(),
            name_highlights: Vec::new(),
            path: original.clone(),
            kind: EntryKind::Directory,
            open_target: None,
            parent_display: "中文".into(),
            size_bytes: None,
            folder_size: FolderSizeState::Unknown,
            modified: None,
        }]);
        assert_eq!(
            session
                .visible_entry(EntryId(9))
                .map(|entry| entry.path.clone()),
            Some(original)
        );
    }
}
