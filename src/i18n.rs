use std::time::SystemTime;

use crate::domain::{EntryKind, FolderSizeState, LoadState, SearchState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
}

impl Language {
    pub const fn storage_code(self) -> u8 {
        match self {
            Self::Chinese => 0,
            Self::English => 1,
        }
    }

    pub const fn from_storage_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Chinese),
            1 => Some(Self::English),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Texts {
    pub language: Language,
}

#[allow(dead_code)]
impl Texts {
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    pub fn loading(self) -> &'static str {
        self.choose("正在加载…", "Loading…")
    }

    pub fn settings(self) -> &'static str {
        self.choose("设置", "Settings")
    }

    pub fn new_menu(self) -> &'static str {
        self.choose("新建", "New")
    }

    pub fn folder(self) -> &'static str {
        self.choose("文件夹", "Folder")
    }

    pub fn open_file_location(self) -> &'static str {
        self.choose("打开所在位置", "Open file location")
    }

    pub fn no_parent_location(self) -> &'static str {
        self.choose("此项目没有可打开的父目录", "This item has no parent folder")
    }

    pub fn undo_empty(self) -> &'static str {
        self.choose("没有可撤销的文件操作", "There is no file operation to undo")
    }

    pub fn undo_busy(self) -> &'static str {
        self.choose(
            "文件操作进行中，暂时无法撤销",
            "Wait for the current file operation before undoing",
        )
    }

    pub fn undo_partial(self) -> &'static str {
        self.choose(
            "部分文件无法安全撤销，可再次按 Ctrl+Z 重试",
            "Some files could not be safely undone; press Ctrl+Z to retry",
        )
    }

    pub fn undo_unavailable(self) -> &'static str {
        self.choose("撤销工作线程不可用", "The undo worker is unavailable")
    }

    pub fn reveal_target_missing(self) -> &'static str {
        self.choose(
            "目标已被移动、重命名或删除",
            "The item was moved, renamed, or deleted",
        )
    }

    pub fn sidebar_categories(self) -> [&'static str; 5] {
        match self.language {
            Language::Chinese => ["快速访问", "库", "磁盘", "网络位置", "网络"],
            Language::English => [
                "Quick access",
                "Libraries",
                "Drives",
                "Network locations",
                "Network",
            ],
        }
    }

    pub fn search_state(self, state: SearchState) -> &'static str {
        match state {
            SearchState::Waiting => self.choose("输入内容以搜索", "Type to search"),
            SearchState::Searching => self.choose("正在搜索…", "Searching…"),
            SearchState::Partial => self.choose("正在加载更多结果…", "Loading more results…"),
            SearchState::Complete => self.choose("搜索完成", "Search complete"),
            SearchState::NoResults => self.choose("没有搜索结果", "No results"),
            SearchState::NotConfigured => {
                self.choose("尚未配置 Everything", "Everything is not configured")
            }
            SearchState::Disconnected => {
                self.choose("Everything 已断开", "Everything is disconnected")
            }
            SearchState::NotIndexed => self.choose(
                "此位置未被 Everything 索引",
                "This location is not indexed by Everything",
            ),
            SearchState::UnsupportedVersion => self.choose(
                "Everything 版本不受支持",
                "Everything version is not supported",
            ),
            SearchState::UnsupportedArchitecture => {
                self.choose("需要 Everything x64", "Everything x64 is required")
            }
            SearchState::SyntaxError => self.choose("搜索语法有误", "Invalid search syntax"),
            SearchState::TimedOut => self.choose("搜索超时", "Search timed out"),
            SearchState::Cancelled => self.choose("搜索已取消", "Search cancelled"),
            SearchState::Failed => self.choose("搜索失败", "Search failed"),
        }
    }

    pub fn state(self, state: LoadState) -> &'static str {
        match state {
            LoadState::Idle => self.choose("就绪", "Ready"),
            LoadState::Loading => self.loading(),
            LoadState::Partial => self.choose("正在加载更多…", "Loading more…"),
            LoadState::Complete => self.choose("加载完成", "Complete"),
            LoadState::Cancelled => self.choose("已取消", "Cancelled"),
            LoadState::NotFound => self.choose("找不到该位置", "Location not found"),
            LoadState::PermissionDenied => self.choose("无权访问该位置", "Permission denied"),
            LoadState::Disconnected => self.choose("位置已断开", "Location disconnected"),
            LoadState::Failed => self.choose("无法打开该位置", "Unable to open location"),
        }
    }

    pub fn items(self, count: usize, skipped: usize) -> String {
        match (self.language, skipped) {
            (Language::Chinese, 0) => format!("{count} 个项目"),
            (Language::Chinese, _) => format!("{count} 个项目 · {skipped} 个未读取"),
            (Language::English, 0) => format!("{count} items"),
            (Language::English, _) => format!("{count} items · {skipped} skipped"),
        }
    }

    pub fn kind(self, kind: EntryKind) -> &'static str {
        match kind {
            EntryKind::Directory => self.choose("文件夹", "Folder"),
            EntryKind::File => self.choose("文件", "File"),
            EntryKind::Other => self.choose("其他", "Other"),
        }
    }

    pub fn modified(self, value: Option<SystemTime>) -> String {
        let Some(value) = value else {
            return "—".to_owned();
        };
        let Ok(duration) = value.elapsed() else {
            return "—".to_owned();
        };
        let seconds = duration.as_secs();
        if seconds < 60 {
            self.choose("刚刚", "just now").to_owned()
        } else if seconds < 3_600 {
            let minutes = seconds / 60;
            self.choose(format!("{minutes} 分钟前"), format!("{minutes}m ago"))
        } else if seconds < 86_400 {
            let hours = seconds / 3_600;
            self.choose(format!("{hours} 小时前"), format!("{hours}h ago"))
        } else {
            let days = seconds / 86_400;
            self.choose(format!("{days} 天前"), format!("{days}d ago"))
        }
    }

    pub fn folder_size(self, state: FolderSizeState) -> String {
        match state {
            FolderSizeState::Unknown => String::new(),
            FolderSizeState::Querying => self.choose("查询中…", "Querying…").to_owned(),
            FolderSizeState::Value(bytes) => self.size(Some(bytes)),
            FolderSizeState::NotIndexed => self.choose("未索引", "Not indexed").to_owned(),
            FolderSizeState::NotFound => self.choose("未命中", "Not found").to_owned(),
            FolderSizeState::TimedOut => self.choose("查询超时", "Timed out").to_owned(),
            FolderSizeState::Disconnected => self
                .choose("Everything 已断开", "Everything disconnected")
                .to_owned(),
            FolderSizeState::ProtocolError => {
                self.choose("响应错误", "Invalid response").to_owned()
            }
        }
    }

    pub fn library_partial_sources_failed(self, failed: usize) -> String {
        match self.language {
            Language::Chinese => format!("{failed} 个库位置不可用，已显示其余内容"),
            Language::English if failed == 1 => {
                "1 library location is unavailable. Showing the remaining content.".to_owned()
            }
            Language::English => format!(
                "{failed} library locations are unavailable. Showing the remaining content."
            ),
        }
    }

    pub fn library_no_default_save_location(self) -> &'static str {
        self.choose(
            "此库没有可用的默认保存位置",
            "This library has no available default save location",
        )
    }

    pub fn library_unavailable(self) -> &'static str {
        self.choose("此库当前不可用", "This library is currently unavailable")
    }

    pub fn library_all_sources_failed(self) -> &'static str {
        self.choose(
            "无法读取此库的任何位置",
            "None of this library's locations could be read",
        )
    }
    pub fn size(self, value: Option<u64>) -> String {
        let Some(bytes) = value else {
            return String::new();
        };
        const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit = 0;
        while size >= 1024.0 && unit < UNITS.len() - 1 {
            size /= 1024.0;
            unit += 1;
        }
        if unit == 0 {
            format!("{bytes} {}", UNITS[unit])
        } else {
            format!("{size:.1} {}", UNITS[unit])
        }
    }

    pub fn recycle_preparing(self, prepared: usize, total: usize) -> String {
        match self.language {
            Language::Chinese => format!("正在准备 · {prepared} / {total} 个所选项目"),
            Language::English => format!("Preparing · {prepared} / {total} selected items"),
        }
    }

    pub fn recycle_discovered(self, items: usize, bytes: u64, complete: bool) -> String {
        let items = format_grouped_decimal(items);
        let size = (bytes > 0)
            .then(|| self.size(Some(bytes)))
            .map(|size| match self.language {
                Language::Chinese => format!("（{size}）"),
                Language::English => format!(" ({size})"),
            })
            .unwrap_or_default();
        let phase = if complete {
            self.choose(" · 正在移到回收站", " · Moving to Recycle Bin")
        } else {
            ""
        };
        match self.language {
            Language::Chinese => format!("已发现 {items} 项{size}{phase}"),
            Language::English => format!("{items} items discovered{size}{phase}"),
        }
    }

    pub fn recycle_discovery_phase(self, complete: bool) -> &'static str {
        if complete {
            self.choose("已完成内容统计", "Discovery complete")
        } else {
            self.choose("正在发现项目", "Discovering items")
        }
    }

    fn choose<T>(self, chinese: T, english: T) -> T {
        match self.language {
            Language::Chinese => chinese,
            Language::English => english,
        }
    }
}

fn format_grouped_decimal(value: usize) -> String {
    let digits = value.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_storage_codes_are_stable() {
        assert_eq!(Language::Chinese.storage_code(), 0);
        assert_eq!(Language::English.storage_code(), 1);
        assert_eq!(Language::from_storage_code(0), Some(Language::Chinese));
        assert_eq!(Language::from_storage_code(1), Some(Language::English));
        assert_eq!(Language::from_storage_code(2), None);
        assert_eq!(Language::from_storage_code(u8::MAX), None);
    }

    #[test]
    fn library_messages_cover_failures_in_both_languages() {
        let chinese = Texts::new(Language::Chinese);
        assert_eq!(
            chinese.library_partial_sources_failed(2),
            "2 个库位置不可用，已显示其余内容"
        );
        assert_eq!(
            chinese.library_no_default_save_location(),
            "此库没有可用的默认保存位置"
        );
        assert_eq!(chinese.library_unavailable(), "此库当前不可用");
        assert_eq!(
            chinese.library_all_sources_failed(),
            "无法读取此库的任何位置"
        );

        let english = Texts::new(Language::English);
        assert_eq!(
            english.library_partial_sources_failed(1),
            "1 library location is unavailable. Showing the remaining content."
        );
        assert_eq!(
            english.library_partial_sources_failed(2),
            "2 library locations are unavailable. Showing the remaining content."
        );
        assert_eq!(
            english.library_no_default_save_location(),
            "This library has no available default save location"
        );
        assert_eq!(
            english.library_unavailable(),
            "This library is currently unavailable"
        );
        assert_eq!(
            english.library_all_sources_failed(),
            "None of this library's locations could be read"
        );
    }
    #[test]
    fn folder_size_distinguishes_zero_and_failures() {
        for language in [Language::Chinese, Language::English] {
            let texts = Texts::new(language);
            assert_eq!(texts.folder_size(FolderSizeState::Value(0)), "0 B");
            assert!(!texts.folder_size(FolderSizeState::NotIndexed).is_empty());
            assert!(!texts.folder_size(FolderSizeState::NotFound).is_empty());
            assert!(!texts.folder_size(FolderSizeState::TimedOut).is_empty());
            assert!(!texts.folder_size(FolderSizeState::Disconnected).is_empty());
            assert!(!texts.folder_size(FolderSizeState::ProtocolError).is_empty());
        }
    }

    #[test]
    fn both_languages_cover_search_states() {
        for language in [Language::Chinese, Language::English] {
            let texts = Texts::new(language);
            for state in [
                SearchState::Waiting,
                SearchState::Searching,
                SearchState::Partial,
                SearchState::Complete,
                SearchState::NoResults,
                SearchState::NotConfigured,
                SearchState::Disconnected,
                SearchState::NotIndexed,
                SearchState::UnsupportedVersion,
                SearchState::UnsupportedArchitecture,
                SearchState::SyntaxError,
                SearchState::TimedOut,
                SearchState::Cancelled,
                SearchState::Failed,
            ] {
                assert!(!texts.search_state(state).is_empty());
            }
        }
    }
    #[test]
    fn both_languages_cover_core_states() {
        for language in [Language::Chinese, Language::English] {
            let texts = Texts::new(language);
            for state in [
                LoadState::Idle,
                LoadState::Loading,
                LoadState::Partial,
                LoadState::Complete,
                LoadState::Cancelled,
                LoadState::NotFound,
                LoadState::PermissionDenied,
                LoadState::Disconnected,
                LoadState::Failed,
            ] {
                assert!(!texts.state(state).is_empty());
            }
        }
    }
}
