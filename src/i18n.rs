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

// The build script derives the Slint record from this resource declaration.
macro_rules! ui_texts {
    ($($field:ident: ($chinese:literal, $english:literal)),* $(,)?) => {
        impl Texts {
            pub fn ui_texts(self) -> asterfiles_ui::UiTextValues {
                asterfiles_ui::UiTextValues {
                    $($field: self.choose($chinese, $english).into(),)*
                }
            }
        }
    };
}

ui_texts! {
    text_back: ("后退", "Back"),
    text_forward: ("前进", "Forward"),
    text_up: ("向上", "Up"),
    text_refresh: ("刷新", "Refresh"),
    text_home: ("主页", "Home"),
    text_desktop: ("桌面", "Desktop"),
    text_downloads: ("下载", "Downloads"),
    text_documents: ("文档", "Documents"),
    text_pictures: ("图片", "Pictures"),
    text_music: ("音乐", "Music"),
    text_videos: ("视频", "Videos"),
    text_quick_access: ("快速访问", "Quick access"),
    text_drives: ("磁盘", "Drives"),
    text_home_quick_access: ("快速访问", "Quick access"),
    text_home_drives: ("磁盘", "Drives"),
    text_home_network_locations: ("网络位置", "Network locations"),
    text_home_network_locations_empty: ("暂无网络位置", "No network locations"),
    text_network_locations: ("网络位置", "Network locations"),
    text_network: ("网络", "Network"),
    text_network_discovery_loading: ("正在发现网络设备…", "Discovering network devices…"),
    text_network_discovery_empty: ("未发现网络设备", "No network devices found"),
    text_network_discovery_error: ("网络设备发现失败", "Network discovery failed"),
    text_name: ("名称", "Name"),
    text_type: ("类型", "Type"),
    text_modified: ("修改时间", "Modified"),
    text_size: ("大小", "Size"),
    text_request_access: ("使用 Windows 请求访问权限", "Request access with Windows"),
    text_sign_in_network_location: ("登录网络位置", "Sign in to network location"),
    text_settings: ("设置", "Settings"),
    text_settings_title: ("设置", "Settings"),
    text_theme: ("主题", "Theme"),
    text_theme_system: ("跟随系统", "Use system setting"),
    text_theme_light: ("浅色", "Light"),
    text_theme_dark: ("深色", "Dark"),
    text_language: ("语言", "Language"),
    text_language_chinese: ("中文", "中文"),
    text_language_english: ("English", "English"),
    text_show_hidden_files: ("显示隐藏文件", "Show hidden files"),
    text_show_system_files: ("显示系统文件", "Show system files"),
    text_file_list_quick_search: ("文件列表快速搜索", "File list quick search"),
    text_new_tab_opens_home: ("新标签页打开 Home", "Open Home in new tabs"),
    text_quick_menu_backdrop: ("右键菜单磨砂效果", "Context menu acrylic effect"),
    text_quick_menu_backdrop_opacity: ("菜单背景不透明度", "Menu background opacity"),
    text_system_integration: ("系统集成", "System integration"),
    text_folder_default_title: ("默认使用 AsterFiles 打开文件夹", "Open folders with AsterFiles by default"),
    text_folder_default_detail: ("让本地文件夹和磁盘的默认打开动作进入 AsterFiles。仅影响当前用户，不需要管理员权限。", "Route the default action for local folders and drives to AsterFiles. This affects only the current user and requires no administrator access."),
    text_win_e_title: ("使用 Win+E 打开 AsterFiles", "Open AsterFiles with Win+E"),
    text_win_e_detail: ("将 Windows 的资源管理器快捷键转到 AsterFiles，可与文件夹默认打开独立设置。", "Route the Windows File Explorer shortcut to AsterFiles independently of folder handling."),
    text_win_e_risk: ("实验功能：依赖 Windows 内部行为，系统更新后可能失效。", "Experimental: this depends on internal Windows behavior and may stop working after an update."),
    text_integration_status: ("状态：", "Status:"),
    text_integration_path_label: ("程序：", "Program:"),
    text_integration_enable: ("启用", "Enable"),
    text_integration_repair: ("修复", "Repair"),
    text_integration_restore: ("恢复 Windows 文件资源管理器", "Restore Windows File Explorer"),
    text_integration_experimental: ("实验功能", "Experimental"),
    text_settings_general: ("常规", "General"),
    text_settings_appearance: ("外观", "Appearance"),
    text_settings_developer: ("开发工具", "Developer tools"),
    text_settings_about: ("关于", "About"),
    text_version: ("版本", "Version"),
    text_everything_title: ("Everything 集成配置", "Everything integration setup"),
    text_everything_connection: ("连接配置", "Connection settings"),
    text_everything_path: ("程序路径", "Program path"),
    text_select_everything_program: ("选择程序", "Choose program"),
    text_everything_instance: ("Everything 实例名", "Everything instance name"),
    text_everything_instance_help: ("“1.5a”是实例名，不是版本号。通常保持默认；仅在自定义或同时运行多个实例时修改，并与运行中的实例一致。", "“1.5a” is an instance name, not a version. Keep the default unless you use a custom or multiple instance, and match the running instance."),
    text_everything_actions: ("连接状态与操作", "Connection status and actions"),
    text_test_connection: ("测试连接", "Test connection"),
    text_start_everything: ("启动 Everything", "Start Everything"),
    text_everything_help: ("使用说明", "How it works"),
    text_everything_help_install: ("• 需要安装 Everything 1.5 x64 并保持后台运行，数据库须完成加载。", "• Install Everything 1.5 x64, keep it running in the background, and wait for its database to finish loading."),
    text_everything_help_dependencies: ("• 地址栏快速搜索、搜索结果索引信息和普通目录文件夹大小依赖 Everything；文件夹大小索引需启用，目录也必须已收录。", "• Address-bar search, indexed result details, and local folder sizes depend on Everything. Folder-size indexing must be enabled and the folder included."),
    text_everything_help_independent: ("• 普通目录浏览和 UNC 网络路径不依赖 Everything；未索引位置不会回退为递归扫描。", "• Normal folder browsing and UNC navigation do not depend on Everything. Unindexed locations never fall back to slow recursive scanning."),
    text_everything_help_settings: ("• 修改程序路径或实例名后请重新测试；AsterFiles 不会自动修改 Everything 的索引设置。", "• Retest after changing the path or instance. AsterFiles never changes Everything index settings automatically."),
    text_discover_everything: ("自动发现", "Auto-detect"),
    text_download_everything: ("下载 Everything", "Download Everything"),
    text_everything_settings: ("前往 Everything 设置", "Open Everything settings"),
    text_ui_showcase: ("UI 陈列室", "UI showcase"),
    text_ui_showcase_detail: ("直接打开难以触发的窗口和状态；演示操作不会修改文件或任务。", "Open hard-to-reach windows and states directly. Demo actions do not change files or tasks."),
    text_show_delete_confirmation: ("永久删除确认", "Delete confirmation"),
    text_show_conflict_confirmation: ("文件冲突", "File conflict"),
    text_show_exit_confirmation: ("退出任务确认", "Exit confirmation"),
    text_show_operation_window: ("文件进度窗口", "File progress window"),
    text_window_trace_title: ("窗口交互诊断", "Window interaction diagnostics"),
    text_window_trace_detail: ("只记录窗口移动与缩放消息，不改变窗口行为。问题复现后关闭应用并把日志交给开发者。", "Records move and resize messages without changing window behavior. After reproducing the issue, close the app and send the log to the developer."),
    text_window_trace_start: ("开始记录", "Start recording"),
    text_window_trace_active: ("正在记录", "Recording"),
    text_window_trace_path: ("日志位置", "Log location"),
    text_loading: ("正在加载…", "Loading…"),
    text_empty_folder: ("此文件夹为空", "This folder is empty"),
    text_no_search_results: ("没有搜索结果", "No search results"),
    text_new_tab: ("新建标签", "New tab"),
    text_close_tab: ("关闭标签", "Close tab"),
    text_all_tabs: ("全部标签", "All tabs"),
    text_window_minimize: ("最小化", "Minimize"),
    text_window_restore: ("还原", "Restore"),
    text_window_maximize: ("最大化", "Maximize"),
    text_window_close: ("关闭", "Close"),
    text_address: ("路径", "Path"),
    text_breadcrumb_overflow: ("显示隐藏的上级目录", "Show hidden parent folders"),
    text_cancel_edit: ("取消编辑", "Cancel editing"),
    text_search_recursive: ("搜索当前文件夹及子文件夹", "Search current folder and subfolders"),
    text_search_current: ("仅搜索当前文件夹", "Search current folder only"),
    text_search_global_disabled: ("全局搜索不支持切换范围", "Search depth is unavailable for global search"),
    text_drop_copy: ("复制到此处", "Copy here"),
    text_drop_move: ("移动到此处", "Move here"),
    text_drop_link: ("在此处创建快捷方式", "Create shortcut here"),
    text_drop_cancel: ("取消", "Cancel"),
    text_context_search: ("搜索命令", "Search commands"),
    text_context_loading: ("正在加载 Windows 菜单…", "Loading Windows menu…"),
    text_context_empty: ("没有匹配的命令", "No matching commands"),
    text_file_operations: ("文件操作", "File operations"),
    text_task_count: ("个任务", "tasks"),
    text_cancel_operation: ("取消", "Cancel"),
    text_retry_operation: ("重试", "Retry"),
    text_everything: ("Everything", "Everything"),
    text_libraries: ("库", "库"),
    text_parent_path: ("父目录", "父目录"),
}

#[allow(dead_code)]
impl Texts {
    pub fn new(language: Language) -> Self {
        Self { language }
    }

    pub fn rename_validation(
        self,
        error: crate::domain::name_validation::NameValidationError,
    ) -> String {
        use crate::domain::name_validation::NameValidationError;
        match error {
            NameValidationError::Empty => self
                .choose("名称不能为空。", "The name cannot be empty.")
                .to_owned(),
            NameValidationError::DotName => self
                .choose("不能使用这个名称。", "This name cannot be used.")
                .to_owned(),
            NameValidationError::InvalidCharacter(character) => match self.language {
                Language::Chinese => format!("名称不能包含字符“{character}”。"),
                Language::English => format!("The name cannot contain '{character}'."),
            },
            NameValidationError::TrailingSpaceOrDot => self
                .choose(
                    "名称不能以空格或句点结尾。",
                    "The name cannot end with a space or period.",
                )
                .to_owned(),
            NameValidationError::ReservedName => self
                .choose(
                    "这是 Windows 保留名称，不能使用。",
                    "This name is reserved by Windows.",
                )
                .to_owned(),
        }
    }

    pub fn action_error(self, code: &str) -> &'static str {
        match code {
            "window-closed" => self.choose("目标窗口已关闭。", "The target window is closed."),
            "tab-closed" => self.choose(
                "目标标签已关闭或移至其他窗口。",
                "The target tab was closed or moved to another window.",
            ),
            "stale-request" => self.choose(
                "页面已更新，请重新选择目标。",
                "The page changed. Select the target again.",
            ),
            "entry-unavailable" => self.choose(
                "目标文件已不在当前页面。",
                "The target item is no longer on this page.",
            ),
            "busy" => self.choose(
                "当前操作尚未完成，请稍后重试。",
                "An operation is still in progress. Try again shortly.",
            ),
            "invalid-name" => self.choose("文件名无效。", "The file name is invalid."),
            "task-unavailable" => {
                self.choose("文件任务已不存在。", "The file task no longer exists.")
            }
            "not_cancellable" => self.choose(
                "此文件任务当前无法取消。",
                "This file task cannot be cancelled now.",
            ),
            "queue-unavailable" => self.choose(
                "后台工作线程不可用。",
                "The background worker is unavailable.",
            ),
            _ => self.choose(
                "当前无法执行此操作。",
                "This action is unavailable in the current state.",
            ),
        }
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

    pub fn self_containment_title(self) -> &'static str {
        self.choose("无法复制或移动文件夹", "Cannot copy or move folder")
    }

    pub fn self_containment_message(self) -> &'static str {
        self.choose(
            "目标文件夹是源文件夹或其子文件夹，不能复制或移动到此位置。请选择其他目标文件夹。",
            "The destination is the source folder or one of its subfolders. Choose a different destination to copy or move this folder.",
        )
    }

    pub fn move_source_retained(self, count: usize, detail: &str) -> String {
        match self.language {
            Language::Chinese => format!("未完成 {count} 项；已完成目标保留，源有遗留：{detail}"),
            Language::English => {
                format!("{count} incomplete; completed targets kept, source retained: {detail}")
            }
        }
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

    pub fn drive_usage_loading(self) -> &'static str {
        self.choose("正在读取磁盘占用", "Checking drive usage")
    }

    pub fn drive_usage_unavailable(self) -> &'static str {
        self.choose("磁盘占用信息不可用", "Drive usage unavailable")
    }

    pub fn drive_usage_status(self, warning: bool) -> &'static str {
        match warning {
            true => self.choose("磁盘空间不足", "Low disk space"),
            false => self.choose("磁盘占用正常", "Drive usage normal"),
        }
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

    pub fn operation_pausing(self) -> &'static str {
        self.choose("正在暂停", "Pausing")
    }

    pub fn operation_recovering_copy(self) -> &'static str {
        self.choose("正在自动恢复", "Automatically recovering")
    }

    pub fn operation_waiting_network(self) -> &'static str {
        self.choose("等待网络响应", "Waiting for network response")
    }

    pub fn operation_total_pending(self) -> &'static str {
        self.choose("总大小统计中", "Calculating total size")
    }

    pub fn operation_copied(self, bytes: u64) -> String {
        let size = format!("{:.1} MB", bytes as f64 / 1_048_576.0);
        match self.language {
            Language::Chinese => format!("已复制 {size}"),
            Language::English => format!("{size} copied"),
        }
    }

    pub fn operation_copying_files(self, files: usize) -> String {
        match self.language {
            Language::Chinese => format!("正在复制 · 已完成 {files} 个文件"),
            Language::English => format!("Copying · {files} files completed"),
        }
    }

    pub fn operation_processed_bytes(self, bytes: u64, total: Option<u64>) -> String {
        let size = match total {
            Some(total) => format!(
                "{:.1} / {:.1} MB",
                bytes as f64 / 1_048_576.0,
                total as f64 / 1_048_576.0
            ),
            None => format!("{:.1} MB", bytes as f64 / 1_048_576.0),
        };
        match self.language {
            Language::Chinese => format!("已处理 {size}"),
            Language::English => format!("Processed {size}"),
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
