mod directory_reader;
pub mod file_operations;

pub use directory_reader::ReadOutcome;

pub const DIRECTORY_WORK_ROOT: &str = ".asterfiles-directory";

pub fn is_directory_work_path(path: &std::path::Path) -> bool {
    path.components().any(|component| {
        component
            .as_os_str()
            .eq_ignore_ascii_case(DIRECTORY_WORK_ROOT)
    })
}

pub use directory_reader::read_path_entry;
pub(crate) use directory_reader::{directory_icon_requires_path, read_directory_entry};
