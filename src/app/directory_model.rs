//! Directory presentation keeps raw entries shared and formats only rows requested by Slint.
use super::*;
use platform::windows::shell_icons::{ShellIconRgba, ShellTypeIconKey};
use slint::{ModelNotify, ModelTracker};
use std::{ops::Range, rc::Weak};

type ImageKey = (PathBuf, u32);

struct Images {
    requested_px: Option<u32>,
    ordinary: HashMap<PathBuf, ShellIconRgba>,
    types: HashMap<(ShellTypeIconKey, u32), ShellIconRgba>,
    thumbnails: HashMap<ImageKey, ShellIconRgba>,
    grid_icons: HashMap<ImageKey, ShellIconRgba>,
    entries: HashMap<EntryId, ShellIconRgba>,
    failed: HashSet<PathBuf>,
}

impl Images {
    fn new(tab: &TabSession, app: &AppState, requested_px: Option<u32>) -> Self {
        Self {
            requested_px,
            ordinary: if requested_px.is_none() {
                app.image_cache.ordinary.clone()
            } else {
                HashMap::new()
            },
            types: app.image_cache.types.clone(),
            thumbnails: app.image_cache.thumbnails.snapshot(),
            grid_icons: app.image_cache.grid_icons.snapshot(),
            entries: app
                .icons
                .iter()
                .filter_map(|((tid, request, id), image)| {
                    (requested_px.is_none() && *tid == tab.id && *request == tab.latest_request())
                        .then_some((*id, image.clone()))
                })
                .collect(),
            failed: app
                .thumbnail_failures
                .keys()
                .filter_map(|(tid, request, path, px)| {
                    (*tid == tab.id
                        && *request == tab.latest_request()
                        && Some(*px) == requested_px)
                        .then_some(path.clone())
                })
                .collect(),
        }
    }

    fn evict(&mut self, id: EntryId, path: &Path) -> bool {
        let previous = self.thumbnails.len() + self.grid_icons.len();
        self.thumbnails
            .retain(|(candidate, _), _| candidate != path);
        self.grid_icons
            .retain(|(candidate, _), _| candidate != path);
        let ordinary = self.ordinary.remove(path).is_some();
        let entry = self.entries.remove(&id).is_some();
        previous != self.thumbnails.len() + self.grid_icons.len() || ordinary || entry
    }

    fn get(&self, entry: &FileEntry) -> (Image, i32) {
        let key = self.requested_px.map(|px| (entry.path.clone(), px));
        let thumbnail = key.as_ref().and_then(|key| self.thumbnails.get(key));
        let grid = key.as_ref().and_then(|key| self.grid_icons.get(key));
        let shared = entry_type_icon_key(&entry.path, entry.kind).and_then(|key| {
            let px = if crate::network::is_unc_path(&entry.path) {
                0
            } else {
                self.requested_px.unwrap_or(0)
            };
            self.types.get(&(key, px))
        });
        let image = thumbnail.or(grid).or(shared).or_else(|| {
            if self.requested_px.is_some() {
                return None;
            }
            self.entries
                .get(&entry.id)
                .or_else(|| self.ordinary.get(&entry.path))
        });
        let state = self.requested_px.map_or(0, |_| {
            if thumbnail.is_some() {
                2
            } else if grid.is_some() || shared.is_some() {
                4
            } else if self.failed.contains(&entry.path) {
                3
            } else {
                1
            }
        });
        (image.map(file_images::image).unwrap_or_default(), state)
    }
}

struct RowState {
    entries: Arc<Vec<FileEntry>>,
    indices: HashMap<EntryId, usize>,
    visuals: Vec<ListVisualRow>,
    positions: HashMap<EntryId, usize>,
    tops: Vec<f32>,
    heights: Vec<f32>,
    extent: f32,
    row_height: f32,
    selected: HashSet<EntryId>,
    querying: HashSet<EntryId>,
    focused: Option<EntryId>,
    cut_paths: HashSet<PathBuf>,
    texts: Texts,
    images: Images,
    entry_updates: HashMap<EntryId, FileEntry>,
    image_updates: HashMap<EntryId, (Image, i32)>,
}

impl RowState {
    fn entry(&self, id: EntryId) -> Option<&FileEntry> {
        self.entry_updates.get(&id).or_else(|| {
            self.indices
                .get(&id)
                .and_then(|index| self.entries.get(*index))
        })
    }

    fn image(&self, entry: &FileEntry) -> (Image, i32) {
        self.image_updates
            .get(&entry.id)
            .cloned()
            .unwrap_or_else(|| self.images.get(entry))
    }
}

pub(super) struct DirectoryRows {
    weak: Weak<Self>,
    state: RefCell<RowState>,
    notify: ModelNotify,
    slices: RefCell<Vec<Weak<DirectorySlice>>>,
    #[cfg(test)]
    generated: Cell<usize>,
}

fn same_entry(left: &FileEntry, right: &FileEntry) -> bool {
    left.id == right.id
        && left.path == right.path
        && left.original_name == right.original_name
        && left.display_name == right.display_name
        && left.name_highlights == right.name_highlights
        && left.kind == right.kind
        && left.open_target == right.open_target
        && left.parent_display == right.parent_display
        && left.size_bytes == right.size_bytes
        && left.folder_size == right.folder_size
        && left.modified == right.modified
        && left.created == right.created
}

fn range_between(tops: &[f32], heights: &[f32], top: f32, bottom: f32) -> Range<usize> {
    if tops.is_empty() || bottom < 0.0 || bottom < top {
        return 0..0;
    }
    let first = tops
        .partition_point(|start| *start < top.max(0.0))
        .saturating_sub(1);
    let first = if tops[first] + heights[first] < top {
        first + 1
    } else {
        first
    };
    let end = tops.partition_point(|start| *start <= bottom);
    first.min(end)..end
}

impl DirectoryRows {
    pub(super) fn new(
        entries: Arc<Vec<FileEntry>>,
        groups: Vec<group_projection::GroupProjection>,
        tab: &TabSession,
        app: &AppState,
        requested_px: Option<u32>,
        row_height: f32,
    ) -> Rc<Self> {
        let indices = entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id, index))
            .collect();
        let mut visuals = Vec::new();
        let mut positions = HashMap::with_capacity(entries.len());
        let mut tops = Vec::new();
        let mut heights = Vec::new();
        let mut extent = 0.0;
        for group in groups {
            if group.header_visible {
                let height = group_header_height_for_detail(&group.detail) as f32;
                tops.push(extent);
                heights.push(height);
                extent += height;
                visuals.push(ListVisualRow::GroupHeader {
                    key: group.key,
                    label: group.label,
                    detail: group.detail,
                    entry_count: group.entries.len(),
                });
            }
            for id in group.entries {
                positions.insert(id, visuals.len());
                visuals.push(ListVisualRow::Entry { entry_id: id });
                tops.push(extent);
                heights.push(row_height);
                extent += row_height;
            }
        }
        Rc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            state: RefCell::new(RowState {
                entries,
                indices,
                visuals,
                positions,
                tops,
                heights,
                extent,
                row_height,
                selected: tab.selected.members().clone(),
                querying: tab.folder_sizes.querying_ids(),
                focused: tab.focused,
                cut_paths: app.cut_paths.iter().cloned().collect(),
                texts: Texts::new(app.language),
                images: Images::new(tab, app, requested_px),
                entry_updates: HashMap::new(),
                image_updates: HashMap::new(),
            }),
            notify: ModelNotify::default(),
            slices: RefCell::new(Vec::new()),
            #[cfg(test)]
            generated: Cell::new(0),
        })
    }

    pub(super) fn shared(&self) -> Rc<Self> {
        self.weak.upgrade().expect("live directory model")
    }
    pub(super) fn locate(&self, id: EntryId) -> Option<usize> {
        self.state.borrow().positions.get(&id).copied()
    }
    pub(super) fn entry_id(&self, row: usize) -> Option<EntryId> {
        match self.state.borrow().visuals.get(row)? {
            ListVisualRow::Entry { entry_id } => Some(*entry_id),
            _ => None,
        }
    }
    pub(super) fn rows_between(&self, top: f32, bottom: f32) -> Range<usize> {
        let state = self.state.borrow();
        range_between(&state.tops, &state.heights, top, bottom)
    }
    pub(super) fn top_of(&self, row: usize) -> f32 {
        let state = self.state.borrow();
        state.tops.get(row).copied().unwrap_or(state.extent)
    }
    pub(super) fn extent(&self) -> f32 {
        self.state.borrow().extent
    }
    #[cfg(test)]
    pub(super) fn entry_count(&self) -> usize {
        self.state.borrow().positions.len()
    }
    #[cfg(test)]
    pub(super) fn generated_rows(&self) -> usize {
        self.generated.get()
    }

    fn notify_entry(&self, id: EntryId) {
        if let Some(row) = self.locate(id) {
            self.notify.row_changed(row);
        }
        let slices = {
            let mut slices = self.slices.borrow_mut();
            slices.retain(|slice| slice.strong_count() > 0);
            slices.iter().filter_map(Weak::upgrade).collect::<Vec<_>>()
        };
        for slice in slices {
            if let Some(index) = slice
                .ids
                .borrow()
                .iter()
                .position(|candidate| *candidate == id)
            {
                slice.notify.row_changed(index);
            }
        }
    }

    pub(super) fn evict_image(&self, id: EntryId) {
        let changed = {
            let mut state = self.state.borrow_mut();
            let Some(path) = state.entry(id).map(|entry| entry.path.clone()) else {
                return;
            };
            let active = state.image_updates.remove(&id).is_some();
            state.images.evict(id, &path) || active
        };
        if changed {
            self.notify_entry(id);
        }
    }

    pub(super) fn patch_selection(&self, tab: &TabSession, ids: &HashSet<EntryId>) {
        {
            let mut state = self.state.borrow_mut();
            for id in ids {
                if tab.selected.contains(id) {
                    state.selected.insert(*id);
                } else {
                    state.selected.remove(id);
                }
            }
            state.focused = tab.focused;
        }
        for id in ids {
            self.notify_entry(*id);
        }
    }

    pub(super) fn patch_entries(&self, tab: &TabSession, app: &AppState, ids: &HashSet<EntryId>) {
        {
            let mut state = self.state.borrow_mut();
            for id in ids {
                let Some(entry) = tab.visible_entry(*id) else {
                    continue;
                };
                if state.entry(*id).is_none_or(|old| !same_entry(old, entry)) {
                    state.entry_updates.insert(*id, entry.clone());
                }
                if tab.folder_sizes.state_for(entry) == FolderSizeState::Querying {
                    state.querying.insert(*id);
                } else {
                    state.querying.remove(id);
                }
                if app.cut_paths.contains(&entry.path) {
                    state.cut_paths.insert(entry.path.clone());
                } else {
                    state.cut_paths.remove(&entry.path);
                }
            }
        }
        self.patch_selection(tab, ids);
    }

    pub(super) fn append_pending(&self, tab: &TabSession, app: &AppState) -> bool {
        let pending = tab.pending_entries();
        let mut added_ranges: Vec<(usize, usize)> = Vec::new();
        let focus_changes = {
            let mut state = self.state.borrow_mut();
            let previous_len = state.visuals.len();
            if previous_len != state.positions.len() || previous_len > pending.len() {
                return false;
            }
            let tail_only = previous_len == 0
                || matches!(
                    state.visuals.last(),
                    Some(ListVisualRow::Entry { entry_id }) if *entry_id == pending[previous_len - 1].id
                );
            let added = if tail_only {
                (previous_len..pending.len()).collect::<Vec<_>>()
            } else {
                pending
                    .iter()
                    .enumerate()
                    .filter_map(|(index, entry)| {
                        (!state.indices.contains_key(&entry.id)).then_some(index)
                    })
                    .collect::<Vec<_>>()
            };
            if added.len() != pending.len() - previous_len {
                return false;
            }
            // Partial snapshots are model-owned, so appending never clones prior file entries.
            if !added.is_empty() && Arc::strong_count(&state.entries) > 1 {
                if state.entries.is_empty() {
                    state.entries = Arc::new(Vec::new());
                } else {
                    return false;
                }
            }
            if !added.is_empty() {
                Arc::get_mut(&mut state.entries)
                    .expect("partial snapshot is model-owned")
                    .reserve(added.len());
            }
            for index in &added {
                let entry = &pending[*index];
                let raw_index = state.entries.len();
                Arc::get_mut(&mut state.entries)
                    .expect("partial snapshot is model-owned")
                    .push(entry.clone());
                state.indices.insert(entry.id, raw_index);
                if tab.selected.contains(&entry.id) {
                    state.selected.insert(entry.id);
                } else {
                    state.selected.remove(&entry.id);
                }
                if tab.folder_sizes.state_for(entry) == FolderSizeState::Querying {
                    state.querying.insert(entry.id);
                } else {
                    state.querying.remove(&entry.id);
                }
                if app.cut_paths.contains(&entry.path) {
                    state.cut_paths.insert(entry.path.clone());
                } else {
                    state.cut_paths.remove(&entry.path);
                }
                if let Some((start, count)) = added_ranges.last_mut()
                    && *start + *count == *index
                {
                    *count += 1;
                } else {
                    added_ranges.push((*index, 1));
                }
            }
            if let Some(first) = added.first().copied() {
                let row_height = state.row_height;
                state
                    .visuals
                    .resize_with(pending.len(), || ListVisualRow::Entry {
                        entry_id: EntryId(0),
                    });
                state.tops.resize(pending.len(), 0.0);
                state.heights.resize(pending.len(), row_height);
                // Front insertions rewrite only the lightweight suffix, once per batch.
                for (index, entry) in pending.iter().enumerate().skip(first) {
                    state.visuals[index] = ListVisualRow::Entry { entry_id: entry.id };
                    state.positions.insert(entry.id, index);
                    state.tops[index] = index as f32 * row_height;
                }
                state.extent = pending.len() as f32 * row_height;
            }
            let focus_changes =
                (state.focused != tab.focused).then_some((state.focused, tab.focused));
            state.focused = tab.focused;
            focus_changes
        };
        for (index, count) in added_ranges {
            self.notify.row_added(index, count);
        }
        if let Some((previous, current)) = focus_changes {
            for id in previous.into_iter().chain(current) {
                self.notify_entry(id);
            }
        }
        true
    }

    pub(super) fn replace_from(&self, next: &DirectoryRows) {
        if std::ptr::eq(self, next) {
            return;
        }
        let mut next = next.state.borrow_mut();
        let mut changed_rows = Vec::new();
        let (old_count, new_count) = {
            let mut state = self.state.borrow_mut();
            let old_count = state.visuals.len();
            let new_count = next.visuals.len();
            for index in 0..old_count.min(new_count) {
                let differs = state.visuals[index] != next.visuals[index]
                    || match next.visuals[index] {
                        ListVisualRow::Entry { entry_id } => {
                            state.texts.language != next.texts.language
                                || state.selected.contains(&entry_id)
                                    != next.selected.contains(&entry_id)
                                || state.querying.contains(&entry_id)
                                    != next.querying.contains(&entry_id)
                                || (state.focused == Some(entry_id))
                                    != (next.focused == Some(entry_id))
                                || match (state.entry(entry_id), next.entry(entry_id)) {
                                    (Some(old), Some(new)) => {
                                        !same_entry(old, new)
                                            || state.cut_paths.contains(&old.path)
                                                != next.cut_paths.contains(&new.path)
                                    }
                                    _ => true,
                                }
                        }
                        _ => false,
                    };
                if differs {
                    changed_rows.push(index);
                }
            }
            // Keep viewport images across reorders, but never carry an old extension's icon.
            for (id, image) in &state.image_updates {
                if let (Some(old), Some(new)) = (state.entry(*id), next.entry(*id))
                    && (old.path == new.path
                        || old.path.extension().map(|ext| ext.to_ascii_lowercase())
                            == new.path.extension().map(|ext| ext.to_ascii_lowercase())
                        || (old.kind == crate::domain::EntryKind::Directory
                            && new.kind == old.kind))
                {
                    next.image_updates.insert(*id, image.clone());
                }
            }
            std::mem::swap(&mut *state, &mut *next);
            (old_count, new_count)
        };
        if new_count < old_count {
            self.notify.row_removed(new_count, old_count - new_count);
        }
        if new_count > old_count {
            self.notify.row_added(old_count, new_count - old_count);
        }
        for row in changed_rows {
            if let Some(id) = self.entry_id(row) {
                self.notify_entry(id);
            } else {
                self.notify.row_changed(row);
            }
        }
    }

    fn slice(&self, ids: Vec<EntryId>) -> Rc<DirectorySlice> {
        let slice = Rc::new(DirectorySlice {
            rows: self.shared(),
            ids: RefCell::new(ids),
            notify: ModelNotify::default(),
        });
        let mut slices = self.slices.borrow_mut();
        slices.retain(|slice| slice.strong_count() > 0);
        slices.push(Rc::downgrade(&slice));
        slice
    }
}

impl Model for DirectoryRows {
    type Data = FileRow;
    fn row_count(&self) -> usize {
        self.state.borrow().visuals.len()
    }
    fn row_data(&self, row: usize) -> Option<FileRow> {
        let mut state = self.state.borrow_mut();
        let visual = state.visuals.get(row)?;
        #[cfg(test)]
        self.generated.set(self.generated.get() + 1);
        match visual {
            ListVisualRow::GroupHeader {
                label,
                detail,
                entry_count,
                ..
            } => Some(group_header_file_row(label, detail, *entry_count)),
            ListVisualRow::Entry { entry_id } => {
                let entry = state.entry(*entry_id)?;
                let (icon, image_state) = state.image(entry);
                let result = file_row_content(
                    entry,
                    state.texts,
                    state.selected.contains(entry_id),
                    state.focused == Some(*entry_id),
                    state.cut_paths.contains(&entry.path),
                    icon.clone(),
                    image_state,
                    if entry.folder_size == FolderSizeState::Unknown
                        && state.querying.contains(entry_id)
                    {
                        FolderSizeState::Querying
                    } else {
                        entry.folder_size
                    },
                );
                // A displayed image outlives eviction from the bounded source cache.
                if icon.size().width > 0 {
                    state
                        .image_updates
                        .insert(EntryId(result.id as u32), (icon, image_state));
                }
                Some(result)
            }
        }
    }
    fn set_row_data(&self, row: usize, data: FileRow) {
        let Some(id) = self.entry_id(row) else {
            return;
        };
        if data.id != id.0 as i32 {
            return;
        }
        {
            let mut state = self.state.borrow_mut();
            if data.selected {
                state.selected.insert(id);
            } else {
                state.selected.remove(&id);
            }
            if data.focused {
                state.focused = Some(id);
            } else if state.focused == Some(id) {
                state.focused = None;
            }
            if data.icon.size().width > 0 {
                state
                    .image_updates
                    .insert(id, (data.icon, data.thumbnail_state));
            } else {
                state.image_updates.remove(&id);
                if let Some(path) = state.entry(id).map(|entry| entry.path.clone()) {
                    state.images.evict(id, &path);
                    if data.thumbnail_state == 3 {
                        state.images.failed.insert(path);
                    } else {
                        state.images.failed.remove(&path);
                    }
                }
            }
        }
        self.notify_entry(id);
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(super) struct DirectorySlice {
    rows: Rc<DirectoryRows>,
    ids: RefCell<Vec<EntryId>>,
    notify: ModelNotify,
}

impl Model for DirectorySlice {
    type Data = FileRow;
    fn row_count(&self) -> usize {
        self.ids.borrow().len()
    }
    fn row_data(&self, row: usize) -> Option<FileRow> {
        let index = self.rows.locate(*self.ids.borrow().get(row)?)?;
        self.rows.row_data(index)
    }
    fn set_row_data(&self, row: usize, data: FileRow) {
        if let Some(index) = self
            .ids
            .borrow()
            .get(row)
            .and_then(|id| self.rows.locate(*id))
        {
            self.rows.set_row_data(index, data);
        }
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct GridState {
    visuals: Vec<IconVisualRow>,
    positions: HashMap<EntryId, (usize, usize)>,
    tops: Vec<f32>,
    heights: Vec<f32>,
    extent: f32,
}

pub(super) struct DirectoryGrid {
    weak: Weak<Self>,
    rows: Rc<DirectoryRows>,
    state: RefCell<GridState>,
    notify: ModelNotify,
    slices: RefCell<HashMap<usize, Weak<DirectorySlice>>>,
    positioned: RefCell<Weak<PositionedDirectoryGrid>>,
}

impl DirectoryGrid {
    pub(super) fn new(
        rows: Rc<DirectoryRows>,
        groups: &[group_projection::GroupProjection],
        columns: usize,
        row_height: f32,
    ) -> Rc<Self> {
        let mut visuals = Vec::new();
        let mut positions = HashMap::new();
        let mut tops = Vec::new();
        let mut heights = Vec::new();
        let mut extent = 0.0;
        for group in groups {
            if group.header_visible {
                let height = group_header_height_for_detail(&group.detail) as f32;
                tops.push(extent);
                heights.push(height);
                extent += height;
                visuals.push(IconVisualRow::GroupHeader {
                    key: group.key.clone(),
                    label: group.label.clone(),
                    detail: group.detail.clone(),
                    entry_count: group.entries.len(),
                });
            }
            for chunk in group.entries.chunks(columns.max(1)) {
                for (column, id) in chunk.iter().enumerate() {
                    positions.insert(*id, (visuals.len(), column));
                }
                tops.push(extent);
                heights.push(row_height);
                extent += row_height;
                visuals.push(IconVisualRow::Entries {
                    group_key: group.key.clone(),
                    entries: chunk.to_vec(),
                });
            }
        }
        Rc::new_cyclic(|weak| Self {
            weak: weak.clone(),
            rows,
            state: RefCell::new(GridState {
                visuals,
                positions,
                tops,
                heights,
                extent,
            }),
            notify: ModelNotify::default(),
            slices: RefCell::new(HashMap::new()),
            positioned: RefCell::new(Weak::new()),
        })
    }
    pub(super) fn shared(&self) -> Rc<Self> {
        self.weak.upgrade().expect("live grid model")
    }
    pub(super) fn rows(&self) -> Rc<DirectoryRows> {
        self.rows.clone()
    }
    pub(super) fn locate(&self, id: EntryId) -> Option<(usize, usize)> {
        self.state.borrow().positions.get(&id).copied()
    }
    pub(super) fn entry_ids(&self, row: usize) -> Vec<EntryId> {
        match self.state.borrow().visuals.get(row) {
            Some(IconVisualRow::Entries { entries, .. }) => entries.clone(),
            _ => Vec::new(),
        }
    }
    pub(super) fn rows_between(&self, top: f32, bottom: f32) -> Range<usize> {
        let state = self.state.borrow();
        range_between(&state.tops, &state.heights, top, bottom)
    }
    pub(super) fn top_of(&self, row: usize) -> f32 {
        let state = self.state.borrow();
        state.tops.get(row).copied().unwrap_or(state.extent)
    }
    pub(super) fn extent(&self) -> f32 {
        self.state.borrow().extent
    }
    pub(super) fn positioned(&self) -> Rc<PositionedDirectoryGrid> {
        if let Some(model) = self.positioned.borrow().upgrade() {
            return model;
        }
        let model = Rc::new(PositionedDirectoryGrid {
            grid: self.shared(),
        });
        *self.positioned.borrow_mut() = Rc::downgrade(&model);
        model
    }
    pub(super) fn replace_from(&self, next: &DirectoryGrid) {
        if std::ptr::eq(self, next) {
            return;
        }
        if !Rc::ptr_eq(&self.rows, &next.rows) {
            self.rows.replace_from(&next.rows);
        }
        let mut next = next.state.borrow_mut();
        let (old_count, new_count, changed) = {
            let mut state = self.state.borrow_mut();
            let old_count = state.visuals.len();
            let new_count = next.visuals.len();
            let changed = (0..old_count.min(new_count))
                .filter(|index| {
                    state.visuals[*index] != next.visuals[*index]
                        || state.tops[*index] != next.tops[*index]
                        || state.heights[*index] != next.heights[*index]
                })
                .collect::<Vec<_>>();
            std::mem::swap(&mut *state, &mut *next);
            (old_count, new_count, changed)
        };
        let slices = {
            let mut slices = self.slices.borrow_mut();
            slices.retain(|_, slice| slice.strong_count() > 0);
            slices
                .iter()
                .filter_map(|(index, slice)| slice.upgrade().map(|slice| (*index, slice)))
                .collect::<Vec<_>>()
        };
        for (index, slice) in slices {
            let ids = self.entry_ids(index);
            if *slice.ids.borrow() != ids {
                *slice.ids.borrow_mut() = ids;
                slice.notify.reset();
            }
        }
        if new_count < old_count {
            self.notify.row_removed(new_count, old_count - new_count);
        }
        if new_count > old_count {
            self.notify.row_added(old_count, new_count - old_count);
        }
        for index in changed {
            self.notify.row_changed(index);
        }
    }
}

impl Model for DirectoryGrid {
    type Data = GridRow;
    fn row_count(&self) -> usize {
        self.state.borrow().visuals.len()
    }
    fn row_data(&self, row: usize) -> Option<GridRow> {
        let state = self.state.borrow();
        Some(match state.visuals.get(row)? {
            IconVisualRow::GroupHeader {
                label,
                detail,
                entry_count,
                ..
            } => GridRow {
                group_header: true,
                group_label: label.as_str().into(),
                group_detail: detail.as_str().into(),
                group_count: (*entry_count).min(i32::MAX as usize) as i32,
                entries: ModelRc::new(VecModel::default()),
            },
            IconVisualRow::Entries { entries, .. } => GridRow {
                group_header: false,
                group_label: "".into(),
                group_detail: "".into(),
                group_count: 0,
                entries: {
                    let mut slices = self.slices.borrow_mut();
                    slices.retain(|_, slice| slice.strong_count() > 0);
                    let slice = slices.get(&row).and_then(Weak::upgrade).unwrap_or_else(|| {
                        let slice = self.rows.slice(entries.clone());
                        slices.insert(row, Rc::downgrade(&slice));
                        slice
                    });
                    ModelRc::from(slice)
                },
            },
        })
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub(super) struct PositionedDirectoryGrid {
    grid: Rc<DirectoryGrid>,
}
impl Model for PositionedDirectoryGrid {
    type Data = PositionedGridRow;
    fn row_count(&self) -> usize {
        self.grid.row_count()
    }
    fn row_data(&self, row: usize) -> Option<PositionedGridRow> {
        let data = self.grid.row_data(row)?;
        let state = self.grid.state.borrow();
        Some(PositionedGridRow {
            row: data,
            offset: state.tops[row],
            extent: state.heights[row],
        })
    }
    fn model_tracker(&self) -> &dyn ModelTracker {
        &self.grid.notify
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn fixture(count: u32, shared_icon: bool) -> (AppState, Rc<DirectoryRows>) {
        let root = PathBuf::from(r"C:\issue-165-images");
        let mut app = AppState::new_for_test(vec![root.clone()], 0, [0, 1, 2, 3]);
        let tab = app.tab_mut(TabId(1)).unwrap();
        tab.load_state = LoadState::Complete;
        tab.replace_entries(
            (1..=count)
                .map(|id| {
                    let name = format!("item-{id}.png");
                    FileEntry {
                        id: EntryId(id),
                        original_name: OsString::from(&name),
                        display_name: name.clone(),
                        name_highlights: Vec::new(),
                        path: root.join(name),
                        kind: crate::domain::EntryKind::File,
                        open_target: None,
                        library_source_index: None,
                        parent_display: root.display().to_string(),
                        size_bytes: Some(1),
                        folder_size: FolderSizeState::NotIndexed,
                        modified: None,
                        created: None,
                        webdav: None,
                    }
                })
                .collect(),
        );
        let image = ShellIconRgba {
            width: 1,
            height: 1,
            pixels: Arc::from([20, 40, 80, 255]),
        };
        if shared_icon {
            let key = entry_type_icon_key(&root.join("item-1.png"), crate::domain::EntryKind::File)
                .unwrap();
            app.image_cache.types.insert((key, 48), image);
        } else {
            app.image_cache
                .thumbnails
                .insert((root.join("item-1.png"), 48), image);
        }
        let tab = app.tab(TabId(1)).unwrap();
        let groups = directory_group_projections(&app, tab, tab.visible_entries());
        let rows = DirectoryRows::new(tab.entries.clone(), groups, tab, &app, Some(48), 28.0);
        (app, rows)
    }

    #[test]
    fn issue_165_partial_tail_and_front_insertions_reuse_raw_entries_without_formatting() {
        i_slint_backend_testing::init_no_event_loop();
        let (mut app, complete_rows) = fixture(7, false);
        drop(complete_rows);
        let entries = app.tab(TabId(1)).unwrap().entries.as_ref().clone();
        let root = PathBuf::from(r"C:\issue-165-images");
        let (request, _) = app
            .tab_mut(TabId(1))
            .unwrap()
            .begin_directory_navigation(root, NavigationKind::Normal);
        let preference = DirectoryViewPreference::default();
        assert!(app.tab_mut(TabId(1)).unwrap().accept_directory_batch(
            request,
            vec![entries[2].clone(), entries[4].clone()],
            preference,
        ));
        let tab = app.tab(TabId(1)).unwrap();
        let groups = directory_group_projections(&app, tab, tab.pending_entries());
        let rows = DirectoryRows::new(
            tab.visible_entries_snapshot(),
            groups,
            tab,
            &app,
            Some(48),
            28.0,
        );
        let original_name = rows.state.borrow().entries[0].display_name.as_ptr();
        for batch in [vec![5, 6], vec![0, 3], vec![1]] {
            assert!(
                app.tab_mut(TabId(1)).unwrap().accept_directory_batch(
                    request,
                    batch
                        .into_iter()
                        .map(|index| entries[index].clone())
                        .collect(),
                    preference,
                )
            );
            let tab = app.tab(TabId(1)).unwrap();
            assert!(rows.append_pending(tab, &app));
            assert_eq!(
                rows.generated_rows(),
                0,
                "batch projection must not format any FileRow"
            );
            assert_eq!(
                rows.state.borrow().entries[0].display_name.as_ptr(),
                original_name,
                "previous FileEntry values must not be cloned"
            );
            assert_eq!(rows.entry_count(), tab.pending_entries().len());
            for (index, entry) in tab.pending_entries().iter().enumerate() {
                assert_eq!(rows.entry_id(index), Some(entry.id));
                assert_eq!(rows.locate(entry.id), Some(index));
                assert_eq!(rows.top_of(index), index as f32 * 28.0);
                assert!(same_entry(
                    rows.state.borrow().entry(entry.id).unwrap(),
                    entry
                ));
            }
            assert_eq!(rows.extent(), tab.pending_entries().len() as f32 * 28.0);
        }
        let before = rows.generated_rows();
        assert!(rows.append_pending(app.tab(TabId(1)).unwrap(), &app));
        assert_eq!(rows.generated_rows(), before, "an empty batch is also lazy");
    }

    #[test]
    fn issue_165_querying_size_is_projected_without_mutating_shared_entries() {
        i_slint_backend_testing::init_no_event_loop();
        let (mut app, previous) = fixture(1, false);
        drop(previous);
        let tab = app.tab_mut(TabId(1)).unwrap();
        let entry = &mut Arc::get_mut(&mut tab.entries).unwrap()[0];
        entry.kind = crate::domain::EntryKind::Directory;
        entry.folder_size = FolderSizeState::Unknown;
        let tab = app.tab(TabId(1)).unwrap();
        let groups = directory_group_projections(&app, tab, tab.visible_entries());
        let rows = DirectoryRows::new(tab.entries.clone(), groups, tab, &app, Some(48), 28.0);
        let tab = app.tab_mut(TabId(1)).unwrap();
        let request = tab.latest_request();
        let queries = tab
            .folder_sizes
            .visible_entry_queries(request, &tab.entries, &[0]);
        assert_eq!(queries.len(), 1);
        let tab = app.tab(TabId(1)).unwrap();
        rows.patch_entries(tab, &app, &HashSet::from([EntryId(1)]));
        assert_eq!(rows.generated_rows(), 0);
        assert!(Arc::ptr_eq(&tab.entries, &rows.state.borrow().entries));
        assert_eq!(tab.entries[0].folder_size, FolderSizeState::Unknown);
        assert_eq!(
            rows.row_data(0).unwrap().size.as_str(),
            Texts::new(app.language).folder_size(FolderSizeState::Querying)
        );
        app.tab_mut(TabId(1)).unwrap().folder_sizes.cancel(request);
        rows.patch_entries(
            app.tab(TabId(1)).unwrap(),
            &app,
            &HashSet::from([EntryId(1)]),
        );
        assert_eq!(
            rows.row_data(0).unwrap().size.as_str(),
            Texts::new(app.language).folder_size(FolderSizeState::Unknown)
        );
        assert!(rows.state.borrow().entry_updates.is_empty());
    }

    #[test]
    fn issue_165_requested_image_survives_cache_eviction_and_case_changed_extension() {
        i_slint_backend_testing::init_no_event_loop();
        let (mut app, rows) = fixture(1, false);
        let original = rows.row_data(0).unwrap().icon;
        assert!(original.size().width > 0);
        app.image_cache.thumbnails = icon_cache::BoundedImages::new(128);
        let old = Path::new(r"C:\issue-165-images\item-1.png");
        let same_extension = Path::new(r"C:\issue-165-images\renamed.PNG");
        assert_eq!(
            app.tab_mut(TabId(1))
                .unwrap()
                .rename_loaded_entry(old, same_extension),
            Some(EntryId(1))
        );
        let tab = app.tab(TabId(1)).unwrap();
        let groups = directory_group_projections(&app, tab, tab.visible_entries());
        let next = DirectoryRows::new(tab.entries.clone(), groups, tab, &app, Some(48), 28.0);
        rows.replace_from(&next);
        assert_eq!(rows.row_data(0).unwrap().icon, original);
        let changed_extension = Path::new(r"C:\issue-165-images\renamed.jpg");
        assert_eq!(
            app.tab_mut(TabId(1))
                .unwrap()
                .rename_loaded_entry(same_extension, changed_extension),
            Some(EntryId(1))
        );
        let tab = app.tab(TabId(1)).unwrap();
        let groups = directory_group_projections(&app, tab, tab.visible_entries());
        let next = DirectoryRows::new(tab.entries.clone(), groups, tab, &app, Some(48), 28.0);
        rows.replace_from(&next);
        assert_eq!(rows.row_data(0).unwrap().icon.size().width, 0);
    }

    #[test]
    fn issue_165_image_eviction_releases_private_snapshots_without_empty_overrides() {
        i_slint_backend_testing::init_no_event_loop();
        let (_, rows) = fixture(1, false);
        assert!(rows.row_data(0).unwrap().icon.size().width > 0);
        rows.evict_image(EntryId(1));
        assert!(rows.state.borrow().image_updates.is_empty());
        assert!(rows.state.borrow().images.thumbnails.is_empty());
        assert_eq!(rows.row_data(0).unwrap().icon.size().width, 0);

        let (_, rows) = fixture(256, true);
        for index in 0..rows.row_count() {
            let row = rows.row_data(index).unwrap();
            assert!(row.icon.size().width > 0);
            rows.evict_image(EntryId(row.id as u32));
            assert!(rows.state.borrow().image_updates.is_empty());
        }
        assert_eq!(rows.state.borrow().images.types.len(), 1);
    }
}
