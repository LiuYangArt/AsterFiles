use std::time::SystemTime;

use crate::domain::{EntryKind, LoadState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Chinese,
    English,
}

impl Language {
    pub fn toggle(self) -> Self {
        match self {
            Self::Chinese => Self::English,
            Self::English => Self::Chinese,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Texts {
    pub language: Language,
}

impl Texts {
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    pub fn loading(self) -> &'static str {
        self.choose("正在加载…", "Loading…")
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
