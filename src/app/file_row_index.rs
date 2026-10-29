//! Entry → row lookups for the projected file list model (#175).
//!
//! `VecModel` offers no lookup by entry, so row patches, visible-range plans, scroll extents and
//! rectangle hits used to walk and clone every `FileRow`. In a 100k-entry directory that cost
//! 5–40 ms per batch, pointer move, viewport change or icon result. The index lives beside the
//! model, is keyed by the model allocation and is checked against the row ID before every use;
//! a stale or missing index falls back to the original scan.
use super::FileRow;
use crate::domain::{EntryId, FileEntry, TabSession};
use slint::{Model, VecModel};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    ops::Range,
    rc::{Rc, Weak},
};

pub(super) enum RowLayout {
    /// Non-grouped directory rows mirror `TabSession::visible_entries()`; the set records which
    /// entries already have rows while a load appends batches.
    VisibleOrder { projected: HashSet<EntryId> },
    /// Grouped, library and search projections record each entry's row when built.
    Recorded {
        positions: HashMap<EntryId, usize>,
        /// Header rows and their heights, in row order; every other row is one row height.
        headers: Vec<(usize, f32)>,
    },
}

struct FileRowIndex {
    model: Weak<VecModel<FileRow>>,
    rows: usize,
    layout: RowLayout,
}

thread_local! {
    static INDEXES: RefCell<HashMap<usize, FileRowIndex>> = RefCell::new(HashMap::new());
}

fn identity(model: &VecModel<FileRow>) -> usize {
    std::ptr::from_ref(model) as usize
}

impl RowLayout {
    pub(super) fn build(
        rows: &[FileRow],
        visible_order: bool,
        header_height: impl Fn(&FileRow) -> f32,
    ) -> Self {
        if visible_order && !rows.iter().any(|row| row.group_header) {
            return Self::VisibleOrder {
                projected: rows.iter().map(|row| EntryId(row.id as u32)).collect(),
            };
        }
        let mut positions = HashMap::with_capacity(rows.len());
        let mut headers = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            if row.group_header {
                headers.push((index, header_height(row)));
            } else if row.id > 0 {
                positions.insert(EntryId(row.id as u32), index);
            }
        }
        Self::Recorded { positions, headers }
    }

    fn headers(&self) -> &[(usize, f32)] {
        match self {
            Self::VisibleOrder { .. } => &[],
            Self::Recorded { headers, .. } => headers,
        }
    }

    fn entries(&self) -> usize {
        match self {
            Self::VisibleOrder { projected } => projected.len(),
            Self::Recorded { positions, .. } => positions.len(),
        }
    }
}

/// Registers the layout of a model that is about to be shown.
pub(super) fn install(model: &Rc<VecModel<FileRow>>, layout: RowLayout) {
    INDEXES.with_borrow_mut(|indexes| {
        indexes.retain(|_, index| index.model.strong_count() > 0);
        indexes.insert(
            identity(model),
            FileRowIndex {
                model: Rc::downgrade(model),
                rows: model.row_count(),
                layout,
            },
        );
    });
}

fn with_index<T>(
    model: &VecModel<FileRow>,
    action: impl FnOnce(&mut FileRowIndex) -> T,
) -> Option<T> {
    INDEXES.with_borrow_mut(|indexes| {
        let index = indexes.get_mut(&identity(model))?;
        (index.model.strong_count() > 0 && index.rows == model.row_count()).then(|| action(index))
    })
}

/// Pending indices that have no row yet, or `None` when the model is not in visible order.
pub(super) fn unprojected(model: &VecModel<FileRow>, pending: &[FileEntry]) -> Option<Vec<usize>> {
    with_index(model, |index| match &index.layout {
        RowLayout::VisibleOrder { projected } => Some(
            pending
                .iter()
                .enumerate()
                .filter(|(_, entry)| !projected.contains(&entry.id))
                .map(|(position, _)| position)
                .collect(),
        ),
        RowLayout::Recorded { .. } => None,
    })
    .flatten()
}

/// Records rows inserted into a visible-order model; any other layout is dropped.
pub(super) fn mark_projected(model: &VecModel<FileRow>, ids: impl IntoIterator<Item = EntryId>) {
    INDEXES.with_borrow_mut(|indexes| {
        let Some(index) = indexes.get_mut(&identity(model)) else {
            return;
        };
        match &mut index.layout {
            RowLayout::VisibleOrder { projected } => {
                projected.extend(ids);
                index.rows = model.row_count();
            }
            RowLayout::Recorded { .. } => {
                indexes.remove(&identity(model));
            }
        }
    });
}

/// Re-indexes a registered model after a full reconciliation walk.
pub(super) fn reset_visible_order(
    model: &VecModel<FileRow>,
    ids: impl IntoIterator<Item = EntryId>,
) {
    INDEXES.with_borrow_mut(|indexes| {
        if let Some(index) = indexes.get_mut(&identity(model))
            && index.model.strong_count() > 0
        {
            index.rows = model.row_count();
            index.layout = RowLayout::VisibleOrder {
                projected: ids.into_iter().collect(),
            };
        }
    });
}

/// Rebuild after an in-place reorder such as rename or folder-size sorting.
pub(super) fn reindex(model: &VecModel<FileRow>, header_height: impl Fn(&FileRow) -> f32) {
    INDEXES.with_borrow_mut(|indexes| {
        if let Some(index) = indexes.get_mut(&identity(model)) {
            let visible_order = matches!(index.layout, RowLayout::VisibleOrder { .. });
            index.layout = RowLayout::build(
                &model.iter().collect::<Vec<_>>(),
                visible_order,
                header_height,
            );
            index.rows = model.row_count();
        }
    });
}

/// Number of entry rows (headers excluded) when the index is current.
pub(super) fn projected_entries(model: &VecModel<FileRow>) -> Option<usize> {
    with_index(model, |index| index.layout.entries())
}

/// Rows of the given entries, verified against the row ID. Entries the index cannot place
/// fall back to one scan of the model, which also covers models replaced elsewhere.
pub(super) fn locate(
    model: &VecModel<FileRow>,
    tab: &TabSession,
    ids: impl IntoIterator<Item = EntryId>,
) -> Vec<(EntryId, usize)> {
    let mut found = Vec::new();
    let mut unresolved = HashSet::new();
    let candidates = ids
        .into_iter()
        .map(|id| {
            let candidate = with_index(model, |index| match &index.layout {
                RowLayout::VisibleOrder { projected } => {
                    // A current index that never projected the entry proves it has no row.
                    if projected.contains(&id) {
                        tab.visible_entry_index(id).map(Some)
                    } else {
                        Some(None)
                    }
                }
                RowLayout::Recorded { positions, .. } => Some(positions.get(&id).copied()),
            })
            .flatten();
            (id, candidate)
        })
        .collect::<Vec<_>>();
    for (id, candidate) in candidates {
        match candidate {
            Some(Some(row))
                if model
                    .row_data(row)
                    .is_some_and(|existing| existing.id == id.0 as i32) =>
            {
                found.push((id, row));
            }
            Some(None) => {}
            _ => {
                unresolved.insert(id);
            }
        }
    }
    if !unresolved.is_empty() {
        for row in 0..model.row_count() {
            if let Some(existing) = model.row_data(row)
                && unresolved.remove(&EntryId(existing.id as u32))
            {
                found.push((EntryId(existing.id as u32), row));
            }
        }
    }
    found
}

fn row_at(headers: &[(usize, f32)], row_height: f32, rows: usize, y: f32) -> usize {
    let mut offset = 0.0;
    for (header, height) in headers {
        let top = *header as f32 * row_height + offset;
        if y < top {
            break;
        }
        if y < top + height {
            return *header;
        }
        offset += height - row_height;
    }
    (((y - offset).max(0.0) / row_height).floor() as usize).min(rows.saturating_sub(1))
}

fn row_top(headers: &[(usize, f32)], row_height: f32, row: usize) -> f32 {
    row as f32 * row_height
        + headers
            .iter()
            .take_while(|(header, _)| *header < row)
            .map(|(_, height)| height - row_height)
            .sum::<f32>()
}

/// Rows whose extent touches `[top, bottom]`, matching an inclusive walk from the first row.
pub(super) fn rows_between(
    model: &VecModel<FileRow>,
    row_height: f32,
    top: f32,
    bottom: f32,
) -> Option<Range<usize>> {
    with_index(model, |index| {
        let rows = index.rows;
        if rows == 0 || bottom < 0.0 || bottom < top {
            return 0..0;
        }
        let headers = index.layout.headers();
        let top = top.max(0.0);
        let extent = row_top(headers, row_height, rows);
        if top > extent {
            return rows..rows;
        }
        let mut first = row_at(headers, row_height, rows, top);
        if first > 0 && row_top(headers, row_height, first) >= top {
            first -= 1;
        }
        let last = row_at(headers, row_height, rows, bottom);
        first..(last + 1).min(rows)
    })
}

/// Total content height of the list, or `None` when the index is not current.
pub(super) fn extent(model: &VecModel<FileRow>, row_height: f32) -> Option<f32> {
    with_index(model, |index| {
        index.rows as f32 * row_height
            + index
                .layout
                .headers()
                .iter()
                .map(|(_, height)| height - row_height)
                .sum::<f32>()
    })
}

/// Top of a row, or `None` when the index is not current.
pub(super) fn top_of(model: &VecModel<FileRow>, row_height: f32, row: usize) -> Option<f32> {
    with_index(model, |index| {
        row_top(index.layout.headers(), row_height, row)
    })
}
