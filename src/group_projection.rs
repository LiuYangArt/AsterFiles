use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{SystemTime, UNIX_EPOCH},
};

use crate::{
    domain::{EntryId, EntryKind, FileEntry, FolderSizeState, GroupField, SortDirection},
    i18n::Language,
};

const FLAT_GROUP_KEY: &str = "flat";

#[derive(Debug, Clone, Copy)]
pub struct GroupProjectionContext {
    pub language: Language,
    pub now: SystemTime,
    pub utc_offset_seconds: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupProjection {
    pub key: String,
    pub label: String,
    pub entries: Vec<EntryId>,
    pub header_visible: bool,
}

#[derive(Debug)]
pub struct GroupProjectionBuilder {
    field: GroupField,
    direction: SortDirection,
    context: GroupProjectionContext,
    groups: BTreeMap<GroupSortKey, GroupProjection>,
    seen_entries: HashSet<EntryId>,
}

impl GroupProjectionBuilder {
    pub fn new(
        field: GroupField,
        direction: SortDirection,
        context: GroupProjectionContext,
    ) -> Self {
        Self {
            field,
            direction,
            context,
            groups: BTreeMap::new(),
            seen_entries: HashSet::new(),
        }
    }

    pub fn extend(&mut self, entries: &[FileEntry]) {
        for entry in entries {
            if !self.seen_entries.insert(entry.id) {
                continue;
            }
            let bucket = group_bucket(entry, self.field, self.context);
            self.groups
                .entry(bucket.sort_key)
                .or_insert_with(|| GroupProjection {
                    key: bucket.key,
                    label: bucket.label,
                    entries: Vec::new(),
                    header_visible: self.field != GroupField::None,
                })
                .entries
                .push(entry.id);
        }
    }

    pub fn finish(self) -> Vec<GroupProjection> {
        let mut groups = self.groups.into_values().collect::<Vec<_>>();
        if self.direction == SortDirection::Descending {
            groups.reverse();
        }
        groups
    }
}

pub fn project_groups(
    entries: &[FileEntry],
    field: GroupField,
    direction: SortDirection,
    context: GroupProjectionContext,
) -> Vec<GroupProjection> {
    let mut builder = GroupProjectionBuilder::new(field, direction, context);
    builder.extend(entries);
    builder.finish()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListVisualRow {
    GroupHeader {
        key: String,
        label: String,
        entry_count: usize,
    },
    Entry {
        entry_id: EntryId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OffsetLocation {
    pub row_index: usize,
    pub offset_within_row: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualOffsets {
    row_starts: Vec<u64>,
    total_extent: u64,
}

impl VisualOffsets {
    fn from_extents(extents: impl IntoIterator<Item = u64>) -> Self {
        let mut row_starts = Vec::new();
        let mut total_extent = 0_u64;
        for extent in extents {
            row_starts.push(total_extent);
            total_extent = total_extent.saturating_add(extent);
        }
        Self {
            row_starts,
            total_extent,
        }
    }

    pub fn row_start(&self, row_index: usize) -> Option<u64> {
        self.row_starts.get(row_index).copied()
    }

    pub fn locate(&self, offset: u64) -> Option<OffsetLocation> {
        if offset >= self.total_extent || self.row_starts.is_empty() {
            return None;
        }
        let row_index = self.row_starts.partition_point(|start| *start <= offset) - 1;
        Some(OffsetLocation {
            row_index,
            offset_within_row: offset - self.row_starts[row_index],
        })
    }
}

#[derive(Debug, Clone)]
pub struct ListProjection {
    pub rows: Vec<ListVisualRow>,
    pub offsets: VisualOffsets,
    entry_positions: HashMap<EntryId, usize>,
}

impl ListProjection {
    pub fn from_groups(groups: &[GroupProjection], header_extent: u64, entry_extent: u64) -> Self {
        let mut rows = Vec::new();
        let mut extents = Vec::new();
        let mut entry_positions = HashMap::new();

        for group in groups {
            if group.header_visible {
                rows.push(ListVisualRow::GroupHeader {
                    key: group.key.clone(),
                    label: group.label.clone(),
                    entry_count: group.entries.len(),
                });
                extents.push(header_extent);
            }
            for entry_id in &group.entries {
                let row_index = rows.len();
                rows.push(ListVisualRow::Entry {
                    entry_id: *entry_id,
                });
                extents.push(entry_extent);
                entry_positions.insert(*entry_id, row_index);
            }
        }

        Self {
            rows,
            offsets: VisualOffsets::from_extents(extents),
            entry_positions,
        }
    }

    pub fn entry_position(&self, entry_id: EntryId) -> Option<usize> {
        self.entry_positions.get(&entry_id).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IconVisualRow {
    GroupHeader {
        key: String,
        label: String,
        entry_count: usize,
    },
    Entries {
        group_key: String,
        entries: Vec<EntryId>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IconEntryPosition {
    pub row_index: usize,
    pub column_index: usize,
}

#[derive(Debug, Clone)]
pub struct IconProjection {
    pub rows: Vec<IconVisualRow>,
    pub offsets: VisualOffsets,
    entry_positions: HashMap<EntryId, IconEntryPosition>,
}

impl IconProjection {
    pub fn from_groups(
        groups: &[GroupProjection],
        columns: usize,
        header_extent: u64,
        entry_row_extent: u64,
    ) -> Self {
        let columns = columns.max(1);
        let mut rows = Vec::new();
        let mut extents = Vec::new();
        let mut entry_positions = HashMap::new();

        for group in groups {
            if group.header_visible {
                rows.push(IconVisualRow::GroupHeader {
                    key: group.key.clone(),
                    label: group.label.clone(),
                    entry_count: group.entries.len(),
                });
                extents.push(header_extent);
            }
            for chunk in group.entries.chunks(columns) {
                let row_index = rows.len();
                for (column_index, entry_id) in chunk.iter().enumerate() {
                    entry_positions.insert(
                        *entry_id,
                        IconEntryPosition {
                            row_index,
                            column_index,
                        },
                    );
                }
                rows.push(IconVisualRow::Entries {
                    group_key: group.key.clone(),
                    entries: chunk.to_vec(),
                });
                extents.push(entry_row_extent);
            }
        }

        Self {
            rows,
            offsets: VisualOffsets::from_extents(extents),
            entry_positions,
        }
    }

    pub fn entry_position(&self, entry_id: EntryId) -> Option<IconEntryPosition> {
        self.entry_positions.get(&entry_id).copied()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct GroupSortKey {
    rank: u16,
    stable: String,
}

struct GroupBucket {
    sort_key: GroupSortKey,
    key: String,
    label: String,
}

fn group_bucket(
    entry: &FileEntry,
    field: GroupField,
    context: GroupProjectionContext,
) -> GroupBucket {
    match field {
        GroupField::None => bucket(0, FLAT_GROUP_KEY, String::new()),
        GroupField::Name => name_bucket(entry, context.language),
        GroupField::Modified => date_bucket(entry.modified, context),
        GroupField::Created => date_bucket(entry.created, context),
        GroupField::Kind => kind_bucket(entry, context.language),
        GroupField::Size => size_bucket(entry, context.language),
    }
}

fn bucket(rank: u16, stable: impl Into<String>, label: impl Into<String>) -> GroupBucket {
    let stable = stable.into();
    GroupBucket {
        sort_key: GroupSortKey {
            rank,
            stable: stable.clone(),
        },
        key: stable,
        label: label.into(),
    }
}

fn name_bucket(entry: &FileEntry, language: Language) -> GroupBucket {
    let first = entry
        .display_name
        .trim_start()
        .chars()
        .next()
        .unwrap_or('\0');
    if first.is_ascii_alphabetic() {
        let label = first.to_ascii_uppercase().to_string();
        return bucket(
            100 + label.as_bytes()[0] as u16,
            format!("name:{label}"),
            label,
        );
    }
    if first.is_ascii_digit() {
        return bucket(50, "name:number", "0–9");
    }
    if first.is_alphabetic() {
        let label = first.to_uppercase().collect::<String>();
        let stable = format!("name:unicode:{:06x}", first as u32);
        return bucket(500, stable, label);
    }
    bucket(900, "name:other", choose(language, "其他", "Other"))
}

fn kind_bucket(entry: &FileEntry, language: Language) -> GroupBucket {
    match entry.kind {
        EntryKind::Directory => bucket(0, "kind:folder", choose(language, "文件夹", "File folder")),
        EntryKind::File => {
            let extension = entry
                .path
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .map(str::to_uppercase);
            match extension {
                Some(extension) => bucket(
                    100,
                    format!("kind:extension:{}", extension.to_lowercase()),
                    match language {
                        Language::Chinese => format!("{extension} 文件"),
                        Language::English => format!("{extension} File"),
                    },
                ),
                None => bucket(200, "kind:file", choose(language, "文件", "File")),
            }
        }
        EntryKind::Other => bucket(300, "kind:other", choose(language, "其他", "Other")),
    }
}

fn size_bucket(entry: &FileEntry, language: Language) -> GroupBucket {
    let size = match entry.folder_size {
        FolderSizeState::Value(size) => Some(size),
        _ => entry.size_bytes,
    };
    let Some(size) = size else {
        return bucket(900, "size:unknown", choose(language, "未知", "Unknown"));
    };
    let (rank, key, chinese, english) = match size {
        0 => (0, "size:empty", "空", "Empty"),
        1..=16_383 => (10, "size:tiny", "极小（0–16 KB）", "Tiny (0–16 KB)"),
        16_384..=1_048_575 => (20, "size:small", "小（16 KB–1 MB）", "Small (16 KB–1 MB)"),
        1_048_576..=134_217_727 => (30, "size:medium", "中（1–128 MB）", "Medium (1–128 MB)"),
        134_217_728..=1_073_741_823 => {
            (40, "size:large", "大（128 MB–1 GB）", "Large (128 MB–1 GB)")
        }
        1_073_741_824..=4_294_967_295 => (50, "size:huge", "很大（1–4 GB）", "Huge (1–4 GB)"),
        _ => (60, "size:gigantic", "巨大（>4 GB）", "Gigantic (>4 GB)"),
    };
    bucket(rank, key, choose(language, chinese, english))
}

fn date_bucket(value: Option<SystemTime>, context: GroupProjectionContext) -> GroupBucket {
    let Some(value) = value else {
        return bucket(
            900,
            "date:unknown",
            choose(context.language, "未知", "Unspecified"),
        );
    };
    let today = local_day(context.now, context.utc_offset_seconds);
    let day = local_day(value, context.utc_offset_seconds);
    if day > today {
        return bucket(0, "date:future", choose(context.language, "未来", "Future"));
    }
    if day == today {
        return bucket(10, "date:today", choose(context.language, "今天", "Today"));
    }
    if day == today - 1 {
        return bucket(
            20,
            "date:yesterday",
            choose(context.language, "昨天", "Yesterday"),
        );
    }

    let today_date = civil_from_days(today);
    let date = civil_from_days(day);
    let current_week_start = today - weekday_from_monday(today) as i64;
    if day >= current_week_start {
        return bucket(
            30,
            "date:earlier-this-week",
            choose(context.language, "本周早些时候", "Earlier this week"),
        );
    }
    if day >= current_week_start - 7 {
        return bucket(
            40,
            "date:last-week",
            choose(context.language, "上周", "Last week"),
        );
    }
    if date.year == today_date.year && date.month == today_date.month {
        return bucket(
            50,
            "date:earlier-this-month",
            choose(context.language, "本月早些时候", "Earlier this month"),
        );
    }
    let (previous_year, previous_month) = if today_date.month == 1 {
        (today_date.year - 1, 12)
    } else {
        (today_date.year, today_date.month - 1)
    };
    if date.year == previous_year && date.month == previous_month {
        return bucket(
            60,
            "date:last-month",
            choose(context.language, "上个月", "Last month"),
        );
    }
    if date.year == today_date.year {
        return bucket(
            70,
            "date:earlier-this-year",
            choose(context.language, "今年早些时候", "Earlier this year"),
        );
    }
    bucket(
        80,
        "date:long-ago",
        choose(context.language, "很久以前", "A long time ago"),
    )
}

fn choose(language: Language, chinese: &'static str, english: &'static str) -> &'static str {
    match language {
        Language::Chinese => chinese,
        Language::English => english,
    }
}

fn local_day(time: SystemTime, utc_offset_seconds: i32) -> i64 {
    let seconds = match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs().min(i64::MAX as u64) as i64,
        Err(error) => -(error.duration().as_secs().min(i64::MAX as u64) as i64),
    };
    seconds
        .saturating_add(utc_offset_seconds as i64)
        .div_euclid(86_400)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CivilDate {
    year: i64,
    month: i64,
}

fn civil_from_days(days_since_epoch: i64) -> CivilDate {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 }.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_piece = (5 * day_of_year + 2) / 153;
    let month = month_piece + if month_piece < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    CivilDate { year, month }
}

fn weekday_from_monday(days_since_epoch: i64) -> u8 {
    (days_since_epoch + 3).rem_euclid(7) as u8
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, path::PathBuf, time::Duration};

    use super::*;
    use crate::domain::NameHighlightSegment;

    fn context() -> GroupProjectionContext {
        GroupProjectionContext {
            language: Language::English,
            now: UNIX_EPOCH + Duration::from_secs(20_332 * 86_400 + 12 * 3_600),
            utc_offset_seconds: 8 * 3_600,
        }
    }

    fn entry(id: u32, name: &str, kind: EntryKind, size: Option<u64>) -> FileEntry {
        FileEntry {
            id: EntryId(id),
            original_name: OsString::from(name),
            display_name: name.to_owned(),
            name_highlights: Vec::<NameHighlightSegment>::new(),
            path: PathBuf::from(name),
            kind,
            open_target: None,
            parent_display: String::new(),
            size_bytes: size,
            folder_size: FolderSizeState::Unknown,
            modified: None,
            created: None,
        }
    }

    #[test]
    fn none_produces_one_flat_group_without_a_header() {
        let entries = [
            entry(1, "b.txt", EntryKind::File, Some(2)),
            entry(2, "a.txt", EntryKind::File, Some(1)),
        ];
        let groups = project_groups(
            &entries,
            GroupField::None,
            SortDirection::Ascending,
            context(),
        );
        assert_eq!(groups.len(), 1);
        assert!(!groups[0].header_visible);
        assert_eq!(groups[0].entries, [EntryId(1), EntryId(2)]);
    }

    #[test]
    fn incremental_batches_preserve_order_and_ignore_duplicate_entries() {
        let first = [
            entry(1, "a.txt", EntryKind::File, None),
            entry(2, "b.txt", EntryKind::File, None),
        ];
        let second = [
            entry(2, "b.txt", EntryKind::File, None),
            entry(3, "apple.txt", EntryKind::File, None),
        ];
        let mut builder =
            GroupProjectionBuilder::new(GroupField::Name, SortDirection::Ascending, context());
        builder.extend(&first);
        builder.extend(&second);
        let groups = builder.finish();
        assert_eq!(groups[0].entries, [EntryId(1), EntryId(3)]);
        assert_eq!(groups[1].entries, [EntryId(2)]);
    }

    #[test]
    fn descending_reverses_groups_but_not_entries_inside_each_group() {
        let entries = [
            entry(1, "alpha.txt", EntryKind::File, None),
            entry(2, "apple.txt", EntryKind::File, None),
            entry(3, "beta.txt", EntryKind::File, None),
        ];
        let groups = project_groups(
            &entries,
            GroupField::Name,
            SortDirection::Descending,
            context(),
        );
        assert_eq!(groups[0].label, "B");
        assert_eq!(groups[1].entries, [EntryId(1), EntryId(2)]);
    }

    #[test]
    fn size_unknown_is_separate_from_empty() {
        let entries = [
            entry(1, "unknown", EntryKind::File, None),
            entry(2, "empty", EntryKind::File, Some(0)),
        ];
        let groups = project_groups(
            &entries,
            GroupField::Size,
            SortDirection::Ascending,
            context(),
        );
        assert_eq!(groups[0].key, "size:empty");
        assert_eq!(groups[1].key, "size:unknown");
    }

    #[test]
    fn date_buckets_use_the_provided_local_offset() {
        let mut today = entry(1, "today", EntryKind::File, None);
        today.modified = Some(context().now - Duration::from_secs(21 * 3_600));
        let groups = project_groups(
            &[today],
            GroupField::Modified,
            SortDirection::Ascending,
            context(),
        );
        assert_eq!(groups[0].key, "date:yesterday");
    }

    #[test]
    fn list_headers_are_not_entry_rows_and_offsets_are_binary_locatable() {
        let entries = [
            entry(1, "alpha", EntryKind::File, None),
            entry(2, "beta", EntryKind::File, None),
        ];
        let groups = project_groups(
            &entries,
            GroupField::Name,
            SortDirection::Ascending,
            context(),
        );
        let projection = ListProjection::from_groups(&groups, 30, 20);
        assert!(matches!(
            projection.rows[0],
            ListVisualRow::GroupHeader { .. }
        ));
        assert!(matches!(
            projection.rows[1],
            ListVisualRow::Entry {
                entry_id: EntryId(1)
            }
        ));
        assert_eq!(projection.entry_position(EntryId(2)), Some(3));
        assert_eq!(projection.offsets.locate(81).unwrap().row_index, 3);
    }

    #[test]
    fn icon_projection_maps_entry_to_row_and_column() {
        let entries = [
            entry(1, "alpha", EntryKind::File, None),
            entry(2, "apple", EntryKind::File, None),
            entry(3, "atom", EntryKind::File, None),
        ];
        let groups = project_groups(
            &entries,
            GroupField::Name,
            SortDirection::Ascending,
            context(),
        );
        let projection = IconProjection::from_groups(&groups, 2, 30, 80);
        assert!(matches!(
            projection.rows[0],
            IconVisualRow::GroupHeader { .. }
        ));
        assert_eq!(
            projection.entry_position(EntryId(3)),
            Some(IconEntryPosition {
                row_index: 2,
                column_index: 0,
            })
        );
    }
}
