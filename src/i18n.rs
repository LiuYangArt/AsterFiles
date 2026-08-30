use std::time::SystemTime;

use crate::domain::{AddressMode, EntryKind, FolderSizeState, LoadState, SearchScope, SearchState};

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
    pub fn everything(self) -> &'static str {
        self.choose("Everything", "Everything")
    }

    pub fn everything_program_path(self) -> &'static str {
        self.choose("程序路径", "Program path")
    }

    pub fn everything_instance(self) -> &'static str {
        self.choose("实例", "Instance")
    }

    pub fn everything_version(self) -> &'static str {
        self.choose("已验证版本", "Verified version")
    }

    pub fn everything_allow_launch(self) -> &'static str {
        self.choose("需要时启动 Everything", "Start Everything when needed")
    }

    pub fn everything_test_connection(self) -> &'static str {
        self.choose("测试连接", "Test connection")
    }

    pub fn everything_start(self) -> &'static str {
        self.choose("启动 Everything", "Start Everything")
    }

    pub fn address_accessible_name(self, mode: AddressMode) -> &'static str {
        match mode {
            AddressMode::Normal => self.choose("地址栏", "Address bar"),
            AddressMode::Smart => self.choose("智能地址栏", "Smart address bar"),
        }
    }

    pub fn search_scope(self, scope: &SearchScope) -> &'static str {
        match scope {
            SearchScope::Global => self.choose("全部位置", "Everywhere"),
            SearchScope::Directory(_) => {
                self.choose("当前文件夹及子文件夹", "Current folder and subfolders")
            }
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
            SearchState::SyntaxError => self.choose("搜索语法有误", "Invalid search syntax"),
            SearchState::TimedOut => self.choose("搜索超时", "Search timed out"),
            SearchState::Cancelled => self.choose("搜索已取消", "Search cancelled"),
            SearchState::Failed => self.choose("搜索失败", "Search failed"),
        }
    }

    pub fn search_name_column(self) -> &'static str {
        self.choose("名称", "Name")
    }

    pub fn search_parent_column(self) -> &'static str {
        self.choose("父目录", "Parent folder")
    }

    pub fn search_size_column(self) -> &'static str {
        self.choose("大小", "Size")
    }

    pub fn search_modified_column(self) -> &'static str {
        self.choose("修改时间", "Date modified")
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

    fn choose<T>(self, chinese: T, english: T) -> T {
        match self.language {
            Language::Chinese => chinese,
            Language::English => english,
        }
    }
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
