use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use super::{EntryId, FileEntry, FolderSizeState, RequestId};

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
        entries: &mut [FileEntry],
        first_row: usize,
        visible_rows: usize,
    ) -> Vec<FolderSizeQuery> {
        if self.request_id != request_id || self.strategy != FolderSizeStrategy::VisibleRange {
            self.cancel(request_id);
        }
        let prefetch = visible_rows.max(1);
        let start = first_row.saturating_sub(prefetch).min(entries.len());
        let end = first_row
            .saturating_add(visible_rows)
            .saturating_add(prefetch)
            .min(entries.len());
        self.visible_snapshot = entries[start..end]
            .iter()
            .filter(|entry| entry.is_directory())
            .map(key_for_entry)
            .collect();
        self.next_visible_queries(entries)
    }

    pub fn next_visible_queries(&mut self, entries: &mut [FileEntry]) -> Vec<FolderSizeQuery> {
        if self.strategy != FolderSizeStrategy::VisibleRange {
            return Vec::new();
        }
        let candidates = self
            .visible_snapshot
            .iter()
            .filter(|key| {
                matching_entry(entries, key)
                    .is_some_and(|entry| entry.folder_size == FolderSizeState::Unknown)
                    && !self.queued.contains(*key)
                    && !self.in_flight.contains(*key)
            })
            .take(
                FOLDER_SIZE_SUBMIT_LIMIT
                    .saturating_sub(self.queued.len().saturating_add(self.in_flight.len())),
            )
            .cloned()
            .collect();
        self.mark_queued(entries, candidates)
    }
    pub fn begin_complete_sort(
        &mut self,
        request_id: RequestId,
        entries: &mut [FileEntry],
    ) -> Vec<FolderSizeQuery> {
        self.cancel(request_id);
        self.strategy = FolderSizeStrategy::CompleteForSort;
        self.complete_snapshot = entries
            .iter()
            .filter(|entry| entry.is_directory())
            .map(key_for_entry)
            .collect();
        for entry in entries.iter() {
            if entry.is_directory() && entry.folder_size.is_terminal() {
                self.staged.insert(key_for_entry(entry), entry.folder_size);
            }
        }
        self.next_complete_queries(entries)
    }

    pub fn next_complete_queries(&mut self, entries: &mut [FileEntry]) -> Vec<FolderSizeQuery> {
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
        self.mark_queued(entries, candidates)
    }

    pub fn start(&mut self, query: &FolderSizeQuery) -> bool {
        if !self.accepts(query) || !self.queued.remove(&query.key) {
            return false;
        }
        self.in_flight.insert(query.key.clone());
        true
    }

    pub fn reject(&mut self, query: &FolderSizeQuery, entries: &mut [FileEntry]) {
        if !self.accepts(query)
            || !(self.queued.remove(&query.key) || self.in_flight.remove(&query.key))
        {
            return;
        }
        if let Some(entry) = matching_entry_mut(entries, &query.key)
            && entry.folder_size == FolderSizeState::Querying
        {
            entry.folder_size = FolderSizeState::Unknown;
        }
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

    fn mark_queued(
        &mut self,
        entries: &mut [FileEntry],
        candidates: Vec<FolderSizeKey>,
    ) -> Vec<FolderSizeQuery> {
        for key in &candidates {
            self.queued.insert(key.clone());
            if let Some(entry) = matching_entry_mut(entries, key) {
                entry.folder_size = FolderSizeState::Querying;
            }
        }
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

fn matching_entry<'a>(entries: &'a [FileEntry], key: &FolderSizeKey) -> Option<&'a FileEntry> {
    entries
        .iter()
        .find(|entry| entry.accepts_folder_size(key.entry_id, &key.path))
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
            parent_display: r"C:\sizes".to_owned(),
            size_bytes: None,
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
        }
    }

    #[test]
    fn visible_range_prefetches_one_screen_and_deduplicates() {
        let mut entries = (0..100)
            .map(|index| entry(index + 1, &format!("folder-{index:03}")))
            .collect::<Vec<_>>();
        let mut scheduler = FolderSizeScheduler::new();

        let first = scheduler.visible_queries(RequestId(7), &mut entries, 50, 10);
        let duplicate = scheduler.visible_queries(RequestId(7), &mut entries, 50, 10);

        assert_eq!(first.len(), 24);
        assert_eq!(first.first().unwrap().key.entry_id, EntryId(41));
        assert_eq!(first.last().unwrap().key.entry_id, EntryId(64));
        assert!(duplicate.is_empty());
    }

    #[test]
    fn visible_range_past_the_last_entry_is_empty() {
        let mut entries = vec![entry(1, "folder")];
        let mut scheduler = FolderSizeScheduler::new();

        let queries = scheduler.visible_queries(RequestId(7), &mut entries, 900, 10);

        assert!(queries.is_empty());
    }

    #[test]
    fn complete_sort_stages_failures_and_commits_only_at_the_end() {
        let mut entries = vec![entry(1, "a"), entry(2, "b"), entry(3, "c")];
        entries[0].folder_size = FolderSizeState::Value(9);
        let mut scheduler = FolderSizeScheduler::new();
        let queries = scheduler.begin_complete_sort(RequestId(8), &mut entries);
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
        assert_eq!(entries[1].folder_size, FolderSizeState::Querying);

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
            .visible_queries(RequestId(2), &mut entries, 0, 1)
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
        let mut entries = vec![entry(1, "a")];
        let mut scheduler = FolderSizeScheduler::new();
        let query = scheduler
            .visible_queries(RequestId(4), &mut entries, 0, 1)
            .remove(0);
        assert!(scheduler.start(&query));

        scheduler.reject(&query, &mut entries);

        assert_eq!(entries[0].folder_size, FolderSizeState::Unknown);
        assert_eq!(
            scheduler
                .visible_queries(RequestId(4), &mut entries, 0, 1)
                .len(),
            1
        );
    }
    #[test]
    fn complete_sort_refills_in_bounded_batches() {
        let mut entries = (0..60)
            .map(|index| entry(index + 1, &format!("folder-{index:03}")))
            .collect::<Vec<_>>();
        let mut scheduler = FolderSizeScheduler::new();
        let mut queries = scheduler.begin_complete_sort(RequestId(11), &mut entries);
        assert_eq!(queries.len(), FOLDER_SIZE_SUBMIT_LIMIT);
        for query in queries.drain(..) {
            assert!(scheduler.start(&query));
            assert_ne!(
                scheduler.complete(&query, FolderSizeState::Value(1), &mut entries),
                FolderSizeCommit::CompleteSort
            );
        }
        let next = scheduler.next_complete_queries(&mut entries);
        assert_eq!(next.len(), FOLDER_SIZE_SUBMIT_LIMIT);
    }
}
