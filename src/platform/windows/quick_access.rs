use std::{io, os::windows::ffi::OsStrExt, path::Path};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Shell::{
            IExecuteCommand, IObjectWithSelection, IShellItem, IShellItemArray,
            SHCreateItemFromParsingName, SHCreateShellItemArrayFromShellItem,
        },
    },
    core::{GUID, Interface, PCWSTR},
};

use crate::platform::{KnownLocation, explorer_pinned_locations};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeResult {
    Changed,
    Unchanged,
}

pub fn paths_equal(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&right.as_os_str().to_string_lossy())
}

pub fn contains(items: &[KnownLocation], path: &Path) -> bool {
    items.iter().any(|item| paths_equal(&item.path, path))
}

pub fn enumerate() -> io::Result<Vec<KnownLocation>> {
    explorer_pinned_locations()
}

pub fn pin(path: &Path) -> io::Result<ChangeResult> {
    let before = enumerate()?;
    if contains(&before, path) {
        return Ok(ChangeResult::Unchanged);
    }
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Quick access accepts folders only",
        ));
    }
    super::context_menu::invoke_path_verb(path, "pintohome")?;
    let after = enumerate()?;
    if contains(&after, path) {
        Ok(ChangeResult::Changed)
    } else {
        Err(io::Error::other("Windows Shell did not pin the folder"))
    }
}

const CLSID_UNPIN_FROM_FREQUENT_EXECUTE: GUID =
    GUID::from_u128(0xee20eeba_df64_4a4e_b7bb_2d1c6b2dfcc1);

fn execute_unpin(path: &Path) -> io::Result<()> {
    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    initialized.ok().map_err(io::Error::other)?;
    let result = (|| unsafe {
        let wide = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(io::Error::other)?;
        let selection: IShellItemArray =
            SHCreateShellItemArrayFromShellItem(&item).map_err(io::Error::other)?;
        let command: IExecuteCommand = CoCreateInstance(
            &CLSID_UNPIN_FROM_FREQUENT_EXECUTE,
            None,
            CLSCTX_INPROC_SERVER,
        )
        .map_err(io::Error::other)?;
        let command_selection: IObjectWithSelection = command.cast().map_err(io::Error::other)?;
        command_selection
            .SetSelection(&selection)
            .map_err(io::Error::other)?;
        command.Execute().map_err(io::Error::other)
    })();
    unsafe { CoUninitialize() };
    result
}
pub fn unpin(path: &Path) -> io::Result<ChangeResult> {
    let before = enumerate()?;
    let Some(shell_path) = before
        .iter()
        .find(|item| paths_equal(&item.path, path))
        .map(|item| item.path.clone())
    else {
        return Ok(ChangeResult::Unchanged);
    };
    execute_unpin(&shell_path)?;
    let after = enumerate()?;
    if contains(&after, path) {
        Err(io::Error::other("Windows Shell did not unpin the folder"))
    } else {
        Ok(ChangeResult::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::KnownLocationKind;
    use std::path::PathBuf;

    #[test]
    fn windows_paths_compare_without_losing_original_identity() {
        let original = PathBuf::from(r"C:\Users\LiuYang\项目");
        assert!(paths_equal(&original, Path::new(r"c:\users\liuyang\项目")));
        let item = KnownLocation {
            kind: KnownLocationKind::Pinned,
            label: "项目".into(),
            path: original.clone(),
        };
        assert!(contains(&[item], &original));
    }

    #[test]
    fn duplicate_detection_uses_shell_projection() {
        let items = [KnownLocation {
            kind: KnownLocationKind::Pinned,
            label: "Assets".into(),
            path: PathBuf::from(r"D:\Assets"),
        }];
        assert!(contains(&items, Path::new(r"d:\assets")));
        assert!(!contains(&items, Path::new(r"D:\Other")));
    }
}
