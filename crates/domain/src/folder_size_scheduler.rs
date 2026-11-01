use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

use super::{EntryId, FileEntry, FolderSizeState, RequestId};

pub fn is_internal_cleanup_path(path: &Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(".asterfiles-cleanup")
    })
}
pub const FOLDER_SIZE_QUEUE_CAPACITY: usize = 32;
pub const FOLDER_SIZE_SUBMIT_LIMIT: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderSizeStrategy {
    VisibleRange,
    CompleteForSort,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FolderSizeKey {
    pub entry_id: EntryId,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct FolderSizeQuery {
    pub request_id: RequestId,
    pub generation: u64,
    pub strategy: FolderSizeStrategy,
    pub key: FolderSizeKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderSizeProgress {
    pub completed: usize,
    pub total: usize,
}

#[derive(Debug, Clone)]
pub struct FolderSizeScheduler {
    request_id: RequestId,
    generation: u64,
    strategy: FolderSizeStrategy,
    queued: HashSet<FolderSizeKey>,
    in_flight: HashSet<FolderSizeKey>,
    visible_snapshot: Vec<FolderSizeKey>,
    visible_positions: HashMap<FolderSizeKey, usize>,
    complete_snapshot: Vec<FolderSizeKey>,
    staged: HashMap<FolderSizeKey, FolderSizeState>,
}

impl Default for FolderSizeScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl FolderSizeScheduler {
    pub fn new() -> Self {
        Self {
            request_id: RequestId(0),
            generation: 0,
            strategy: FolderSizeStrategy::VisibleRange,
            queued: HashSet::new(),
            in_flight: HashSet::new(),
            visible_snapshot: Vec::new(),
            visible_positions: HashMap::new(),
            complete_snapshot: Vec::new(),
            staged: HashMap::new(),
        }
    }

    pub fn cancel(&mut self, request_id: RequestId) {
        self.request_id = request_id;
        self.generation = self.generation.wrapping_add(1);
        self.strategy = FolderSizeStrategy::VisibleRange;
        self.queued.clear();
        self.in_flight.clear();
        self.visible_snapshot.clear();
        self.visible_positions.clear();
        self.complete_snapshot.clear();
        self.staged.clear();
    }

    pub fn progress(&self) -> Option<FolderSizeProgress> {
        (self.strategy == FolderSizeStrategy::CompleteForSort).then_some(FolderSizeProgress {
            completed: self.staged.len(),
            total: self.complete_snapshot.len(),
        })
    }

    pub fn visible_queries(
        &mut self,
        request_id: RequestId,
        entries: &[FileEntry],
        first_row: usize,
        visible_rows: usize,
    ) -> Vec<FolderSizeQuery> {
        let prefetch = visible_rows.max(1);
        let start = first_row.saturating_sub(prefetch).min(entries.len());
        let end = first_row
            .saturating_add(visible_rows)
            .saturating_add(prefetch)
            .min(entries.len());
        self.visible_entry_queries(request_id, entries, &(start..end).collect::<Vec<_>>())
    }

    pub fn clear_visible_range(&mut self) {
        self.visible_snapshot.clear();
        self.visible_positions.clear();
    }

    pub fn visible_entry_queries(
        &mut self,
        request_id: RequestId,
        entries: &[FileEntry],
        indices: &[usize],
    ) -> Vec<FolderSizeQuery> {
        if self.request_id != request_id || self.strategy != FolderSizeStrategy::VisibleRange {
            self.cancel(request_id);
        }
        self.visible_snapshot.clear();
        self.visible_positions.clear();
        for index in indices {
            let Some(entry) = entries.get(*index) else {
                continue;
            };
            if !entry.is_directory() || is_internal_cleanup_path(&entry.path) {
                continue;
            }
            let key = key_for_entry(entry);
            if self.visible_positions.insert(key.clone(), *index).is_none() {
                self.visible_snapshot.push(key);
            }
        }
        self.next_visible_queries(entries)
    }

    pub fn next_visible_queries(&mut self, entries: &[FileEntry]) -> Vec<FolderSizeQuery> {
        if self.strategy != FolderSizeStrategy::VisibleRange {
            return Vec::new();
        }
        let candidates = self
            .visible_snapshot
            .iter()
            .filter(|key| {
                self.visible_positions
                    .get(*key)
                    .and_then(|index| entries.get(*index))
                    .is_some_and(|entry| {
                        entry.accepts_folder_size(key.entry_id, &key.path)
                            && entry.folder_size == FolderSizeState::Unknown
                    })
                    && !self.queued.contains(*key)
                    && !self.in_flight.contains(*key)
            })
            .take(
                FOLDER_SIZE_SUBMIT_LIMIT
                    .saturating_sub(self.queued.len().saturating_add(self.in_flight.len())),
            )
            .cloned()
            .collect();
        self.mark_queued(candidates)
    }
    pub fn begin_complete_sort(
        &mut self,
        request_id: RequestId,
        entries: &[FileEntry],
    ) -> Vec<FolderSizeQuery> {
        self.cancel(request_id);
        self.strategy = FolderSizeStrategy::CompleteForSort;
        self.complete_snapshot = entries
            .iter()
            .filter(|entry| entry.is_directory() && !is_internal_cleanup_path(&entry.path))
            .map(key_for_entry)
            .collect();
        for entry in entries.iter() {
            if entry.is_directory()
                && !is_internal_cleanup_path(&entry.path)
                && entry.folder_size.is_terminal()
            {
                self.staged.insert(key_for_entry(entry), entry.folder_size);
            }
        }
        self.next_complete_queries()
    }

    pub fn next_complete_queries(&mut self) -> Vec<FolderSizeQuery> {
        if self.strategy != FolderSizeStrategy::CompleteForSort {
            return Vec::new();
        }
        let candidates = self
            .complete_snapshot
            .iter()
            .filter(|key| {
                !self.staged.contains_key(*key)
                    && !self.queued.contains(*key)
                    && !self.in_flight.contains(*key)
            })
            .take(
                FOLDER_SIZE_SUBMIT_LIMIT
                    .saturating_sub(self.queued.len().saturating_add(self.in_flight.len())),
            )
            .cloned()
            .collect::<Vec<_>>();
        self.mark_queued(candidates)
    }

    pub fn start(&mut self, query: &FolderSizeQuery) -> bool {
        if !self.accepts(query) || !self.queued.remove(&query.key) {
            return false;
        }
        self.in_flight.insert(query.key.clone());
        true
    }

    pub fn reject(&mut self, query: &FolderSizeQuery) {
        if self.accepts(query) {
            self.queued.remove(&query.key);
            self.in_flight.remove(&query.key);
        }
    }

    pub fn state_for(&self, entry: &FileEntry) -> FolderSizeState {
        if entry.folder_size == FolderSizeState::Unknown {
            let key = key_for_entry(entry);
            if self.queued.contains(&key)
                || self.in_flight.contains(&key)
                || self.staged.contains_key(&key)
            {
                return FolderSizeState::Querying;
            }
        }
        entry.folder_size
    }

    pub fn querying_ids(&self) -> HashSet<EntryId> {
        self.queued
            .iter()
            .chain(&self.in_flight)
            .chain(self.staged.keys())
            .map(|key| key.entry_id)
            .collect()
    }

    pub fn complete(
        &mut self,
        query: &FolderSizeQuery,
        state: FolderSizeState,
        entries: &mut [FileEntry],
    ) -> FolderSizeCommit {
        if !self.accepts(query) || !self.in_flight.remove(&query.key) || !state.is_terminal() {
            return FolderSizeCommit::Ignored;
        }
        match self.strategy {
            FolderSizeStrategy::VisibleRange => {
                let Some(entry) = matching_entry_mut(entries, &query.key) else {
                    return FolderSizeCommit::Ignored;
                };
                entry.set_folder_size(query.key.entry_id, &query.key.path, state);
                FolderSizeCommit::Visible(query.key.entry_id)
            }
            FolderSizeStrategy::CompleteForSort => {
                self.staged.insert(query.key.clone(), state);
                if self.staged.len() != self.complete_snapshot.len() {
                    return FolderSizeCommit::Staged;
                }
                for key in &self.complete_snapshot {
                    if let Some(state) = self.staged.get(key).copied()
                        && let Some(entry) = matching_entry_mut(entries, key)
                    {
                        entry.set_folder_size(key.entry_id, &key.path, state);
                    }
                }
                FolderSizeCommit::CompleteSort
            }
        }
    }

    pub fn accepts(&self, query: &FolderSizeQuery) -> bool {
        self.request_id == query.request_id
            && self.generation == query.generation
            && self.strategy == query.strategy
    }

    fn mark_queued(&mut self, candidates: Vec<FolderSizeKey>) -> Vec<FolderSizeQuery> {
        self.queued.extend(candidates.iter().cloned());
        candidates
            .into_iter()
            .map(|key| FolderSizeQuery {
                request_id: self.request_id,
                generation: self.generation,
                strategy: self.strategy,
                key,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderSizeCommit {
    Ignored,
    Staged,
    Visible(EntryId),
    CompleteSort,
}

fn key_for_entry(entry: &FileEntry) -> FolderSizeKey {
    FolderSizeKey {
        entry_id: entry.id,
        path: entry.path.clone(),
    }
}

fn matching_entry_mut<'a>(
    entries: &'a mut [FileEntry],
    key: &FolderSizeKey,
) -> Option<&'a mut FileEntry> {
    entries
        .iter_mut()
        .find(|entry| entry.accepts_folder_size(key.entry_id, &key.path))
}

impl FolderSizeState {
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Unknown | Self::Querying)
    }
}

impl FileEntry {
    fn is_directory(&self) -> bool {
        self.kind == super::EntryKind::Directory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{EntryKind, NameHighlightSegment};
    use std::ffi::OsString;

    fn entry(id: u32, name: &str) -> FileEntry {
        FileEntry {
            id: EntryId(id),
            original_name: OsString::from(name),
            display_name: name.to_owned(),
            name_highlights: Vec::<NameHighlightSegment>::new(),
            path: PathBuf::from(format!(r"C:\sizes\{name}")),
            kind: EntryKind::Directory,
            open_target: None,
            library_source_index: None,
            parent_display: r"C:\sizes".to_owned(),
            size_bytes: None,
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
            webdav: None,
        }
    }

    #[test]
    fn internal_cleanup_path_matching_is_component_exact_and_case_insensitive() {
        assert!(is_internal_cleanup_path(Path::new(
            r"C:\files\.asterfiles-cleanup"
        )));
        assert!(is_internal_cleanup_path(Path::new(
            r"C:\files\.ASTERFILES-CLEANUP\task\payload-0"
        )));
        assert!(!is_internal_cleanup_path(Path::new(
            r"C:\files\.asterfiles-cleanup-copy"
        )));
        assert!(!is_internal_cleanup_path(Path::new(
            r"C:\files\my.asterfiles-cleanup"
        )));
    }

    #[test]
    fn internal_cleanup_entries_never_schedule_visible_folder_sizes() {
        let mut entries = vec![
            entry(1, "ordinary"),
            entry(2, ".asterfiles-cleanup"),
            entry(3, ".asterfiles-cleanup-copy"),
        ];
        entries[1].path = PathBuf::from(r"C:\sizes\.ASTERFILES-CLEANUP\task");
        let mut scheduler = FolderSizeScheduler::new();

        let queries = scheduler.visible_queries(RequestId(12), &entries, 0, 3);

        assert_eq!(queries.len(), 2);
        assert!(queries.iter().all(|query| query.key.entry_id != EntryId(2)));
        assert_eq!(entries[1].folder_size, FolderSizeState::Unknown);
    }

    #[test]
    fn internal_cleanup_entries_never_schedule_complete_sort_folder_sizes() {
        let mut entries = vec![
            entry(1, "ordinary"),
            entry(2, ".asterfiles-cleanup"),
            entry(3, ".asterfiles-cleanup-copy"),
        ];
        entries[1].path = PathBuf::from(r"C:\sizes\.asterfiles-cleanup\task");
        let mut scheduler = FolderSizeScheduler::new();

        let queries = scheduler.begin_complete_sort(RequestId(13), &entries);

        assert_eq!(queries.len(), 2);
        assert!(queries.iter().all(|query| query.key.entry_id != EntryId(2)));
        assert_eq!(
            scheduler.progress(),
            Some(FolderSizeProgress {
                completed: 0,
                total: 2,
            })
        );
        assert_eq!(entries[1].folder_size, FolderSizeState::Unknown);
    }
    #[test]
    fn visible_range_prefetches_one_screen_and_deduplicates() {
        let entries = (0..100)
            .map(|index| entry(index + 1, &format!("folder-{index:03}")))
            .collect::<Vec<_>>();
        let mut scheduler = FolderSizeScheduler::new();

        let first = scheduler.visible_queries(RequestId(7), &entries, 50, 10);
        let duplicate = scheduler.visible_queries(RequestId(7), &entries, 50, 10);

        assert_eq!(first.len(), 24);
        assert_eq!(first.first().unwrap().key.entry_id, EntryId(41));
        assert_eq!(first.last().unwrap().key.entry_id, EntryId(64));
        assert!(duplicate.is_empty());
    }

    #[test]
    fn visible_range_past_the_last_entry_is_empty() {
        let entries = vec![entry(1, "folder")];
        let mut scheduler = FolderSizeScheduler::new();

        let queries = scheduler.visible_queries(RequestId(7), &entries, 900, 10);

        assert!(queries.is_empty());
    }

    #[test]
    fn complete_sort_stages_failures_and_commits_only_at_the_end() {
        let mut entries = vec![entry(1, "a"), entry(2, "b"), entry(3, "c")];
        entries[0].folder_size = FolderSizeState::Value(9);
        let mut scheduler = FolderSizeScheduler::new();
        let queries = scheduler.begin_complete_sort(RequestId(8), &entries);
        assert_eq!(queries.len(), 2);
        assert_eq!(
            scheduler.progress(),
            Some(FolderSizeProgress {
                completed: 1,
                total: 3
            })
        );

        assert!(scheduler.start(&queries[0]));
        assert_eq!(
            scheduler.complete(&queries[0], FolderSizeState::NotIndexed, &mut entries),
            FolderSizeCommit::Staged
        );
        assert_eq!(entries[1].folder_size, FolderSizeState::Unknown);
        assert_eq!(scheduler.state_for(&entries[1]), FolderSizeState::Querying);

        assert!(scheduler.start(&queries[1]));
        assert_eq!(
            scheduler.complete(&queries[1], FolderSizeState::Value(0), &mut entries),
            FolderSizeCommit::CompleteSort
        );
        assert_eq!(entries[0].folder_size, FolderSizeState::Value(9));
        assert_eq!(entries[1].folder_size, FolderSizeState::NotIndexed);
        assert_eq!(entries[2].folder_size, FolderSizeState::Value(0));
    }

    #[test]
    fn cancellation_rejects_old_generation_even_when_ids_are_reused() {
        let mut entries = vec![entry(1, "old")];
        let mut scheduler = FolderSizeScheduler::new();
        let query = scheduler
            .visible_queries(RequestId(2), &entries, 0, 1)
            .remove(0);
        assert!(scheduler.start(&query));
        scheduler.cancel(RequestId(3));
        assert!(!scheduler.accepts(&query));
        assert_eq!(
            scheduler.complete(&query, FolderSizeState::Value(12), &mut entries),
            FolderSizeCommit::Ignored
        );
    }

    #[test]
    fn rejecting_started_query_releases_it_for_resubmission() {
        let entries = vec![entry(1, "a")];
        let mut scheduler = FolderSizeScheduler::new();
        let query = scheduler
            .visible_queries(RequestId(4), &entries, 0, 1)
            .remove(0);
        assert!(scheduler.start(&query));

        scheduler.reject(&query);

        assert_eq!(entries[0].folder_size, FolderSizeState::Unknown);
        assert_eq!(
            scheduler
                .visible_queries(RequestId(4), &entries, 0, 1)
                .len(),
            1
        );
    }
    #[test]
    fn issue_165_visible_entries_follow_sparse_grid_positions_with_bounded_index() {
        let mut entries = (0..100_000)
            .map(|index| entry(index + 1, &format!("folder-{index:06}")))
            .collect::<Vec<_>>();
        let mut scheduler = FolderSizeScheduler::new();
        let indices = [99_999, 50_000, 12, 50_000, usize::MAX];
        let queries = scheduler.visible_entry_queries(RequestId(165), &entries, &indices);
        assert_eq!(
            queries
                .iter()
                .map(|query| query.key.entry_id)
                .collect::<Vec<_>>(),
            [EntryId(100_000), EntryId(50_001), EntryId(13)]
        );
        assert_eq!(scheduler.visible_positions.len(), 3);
        assert_eq!(scheduler.visible_snapshot.len(), 3);
        assert_eq!(entries[0].folder_size, FolderSizeState::Unknown);
        assert_eq!(entries[99_999].folder_size, FolderSizeState::Unknown);
        assert_eq!(
            scheduler.state_for(&entries[99_999]),
            FolderSizeState::Querying
        );
        assert!(
            scheduler
                .visible_entry_queries(RequestId(165), &entries, &indices)
                .is_empty()
        );
        for query in &queries {
            assert!(scheduler.start(query));
            assert_eq!(
                scheduler.complete(query, FolderSizeState::Value(165), &mut entries),
                FolderSizeCommit::Visible(query.key.entry_id)
            );
        }
        let next = scheduler.visible_entry_queries(RequestId(165), &entries, &[80_000]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].key.entry_id, EntryId(80_001));
        assert_eq!(scheduler.visible_positions.len(), 1);
        scheduler.cancel(RequestId(166));
        assert!(scheduler.visible_positions.is_empty());
    }

    #[test]
    fn issue_165_query_planning_preserves_shared_entries_and_querying_presentation() {
        use std::sync::Arc;
        let entries = Arc::new(
            (0..100_000)
                .map(|index| entry(index + 1, &format!("folder-{index:06}")))
                .collect::<Vec<_>>(),
        );
        let presentation = entries.clone();
        let mut scheduler = FolderSizeScheduler::new();
        let queries = scheduler.visible_entry_queries(RequestId(165), &entries, &[99_999, 90_000]);
        assert!(Arc::ptr_eq(&entries, &presentation));
        assert_eq!(Arc::strong_count(&entries), 2);
        assert_eq!(entries[99_999].folder_size, FolderSizeState::Unknown);
        assert_eq!(
            scheduler.state_for(&entries[99_999]),
            FolderSizeState::Querying
        );
        assert_eq!(
            scheduler.querying_ids(),
            HashSet::from([EntryId(100_000), EntryId(90_001)])
        );
        assert!(scheduler.start(&queries[0]));
        assert_eq!(
            scheduler.state_for(&entries[99_999]),
            FolderSizeState::Querying
        );
        scheduler.reject(&queries[0]);
        assert_eq!(
            scheduler.state_for(&entries[99_999]),
            FolderSizeState::Unknown
        );
        scheduler.cancel(RequestId(166));
        assert_eq!(
            scheduler.state_for(&entries[90_000]),
            FolderSizeState::Unknown
        );
        assert!(scheduler.querying_ids().is_empty());
        assert!(Arc::ptr_eq(&entries, &presentation));
    }

    #[test]
    fn issue_165_visible_index_rechecks_identity_after_reorder() {
        let mut entries = vec![entry(1, "a"), entry(2, "b")];
        let mut scheduler = FolderSizeScheduler::new();
        let query = scheduler
            .visible_entry_queries(RequestId(165), &entries, &[0])
            .remove(0);
        scheduler.reject(&query);
        entries.swap(0, 1);
        assert!(
            scheduler.next_visible_queries(&entries).is_empty(),
            "a stale row must not query a different entry"
        );
        let next = scheduler.visible_entry_queries(RequestId(165), &entries, &[1]);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].key.entry_id, EntryId(1));
    }

    #[test]
    fn complete_sort_refills_in_bounded_batches() {
        let mut entries = (0..60)
            .map(|index| entry(index + 1, &format!("folder-{index:03}")))
            .collect::<Vec<_>>();
        let mut scheduler = FolderSizeScheduler::new();
        let mut queries = scheduler.begin_complete_sort(RequestId(11), &entries);
        assert_eq!(queries.len(), FOLDER_SIZE_SUBMIT_LIMIT);
        for query in queries.drain(..) {
            assert!(scheduler.start(&query));
            assert_ne!(
                scheduler.complete(&query, FolderSizeState::Value(1), &mut entries),
                FolderSizeCommit::CompleteSort
            );
        }
        let next = scheduler.next_complete_queries();
        assert_eq!(next.len(), FOLDER_SIZE_SUBMIT_LIMIT);
    }
}
