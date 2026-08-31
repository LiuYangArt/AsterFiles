use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize, IPersistFile,
        },
        UI::Shell::{IShellLinkW, ShellLink},
    },
    core::{Interface, PCWSTR},
};

pub fn create_shortcut(source: &Path, destination: &Path) -> io::Result<()> {
    let _apartment = ComApartment::initialize()?;
    let source = wide(source)?;
    let destination = wide(destination)?;
    let link: IShellLinkW = unsafe { CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER) }
        .map_err(windows_error)?;
    unsafe { link.SetPath(PCWSTR(source.as_ptr())) }.map_err(windows_error)?;
    let persist: IPersistFile = link.cast().map_err(windows_error)?;
    unsafe { persist.Save(PCWSTR(destination.as_ptr()), true) }.map_err(windows_error)
}

pub fn shortcut_destination(target: &Path, source: &Path) -> io::Result<PathBuf> {
    let stem = source
        .file_stem()
        .or_else(|| source.file_name())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "shortcut source has no name")
        })?;
    let mut destination = target.join(stem);
    destination.set_extension("lnk");
    if !destination.exists() {
        return Ok(destination);
    }
    for index in 2_u64.. {
        let mut name = stem.to_os_string();
        name.push(format!(" ({index})"));
        let mut candidate = target.join(name);
        candidate.set_extension("lnk");
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    unreachable!()
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty shortcut path",
        ));
    }
    let mut value = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if value.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shortcut path contains NUL",
        ));
    }
    value.push(0);
    Ok(value)
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

struct ComApartment;
impl ComApartment {
    fn initialize() -> io::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(windows_error)?;
        Ok(Self)
    }
}
impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcut_names_use_windows_keep_both_suffix() {
        let root = std::env::temp_dir().join(format!("asterfiles-shortcut-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("Report.lnk"), b"existing").unwrap();
        assert_eq!(
            shortcut_destination(&root, Path::new(r"C:\Docs\Report.txt")).unwrap(),
            root.join("Report (2).lnk")
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
