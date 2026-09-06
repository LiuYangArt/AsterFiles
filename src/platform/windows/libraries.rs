use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::OsStringExt,
    path::PathBuf,
};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize, STGM_READ,
        },
        UI::Shell::{
            BHID_EnumItems, DSFT_DETECT, FOLDERID_Libraries, IEnumShellItems, IShellItem,
            IShellItemArray, IShellLibrary, KF_FLAG_DEFAULT, LFF_ALLITEMS, LOF_PINNEDTONAVPANE,
            SHGetKnownFolderItem, SHGetKnownFolderPath, SIGDN_DESKTOPABSOLUTEPARSING,
            SIGDN_FILESYSPATH, SIGDN_NORMALDISPLAY, ShellLibrary,
        },
    },
    core::PWSTR,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LibraryId(OsString);

impl LibraryId {
    #[cfg(test)]
    pub fn new(identity: OsString) -> Self {
        Self(identity)
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySource {
    pub shell_identity: OsString,
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsLibrary {
    pub id: LibraryId,
    pub definition_path: Option<PathBuf>,
    pub display_name: OsString,
    pub pinned: bool,
    pub sort_order: u32,
    pub sources: Vec<LibrarySource>,
    pub default_save_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryFailure {
    pub shell_identity: Option<OsString>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryEnumeration {
    pub libraries: Vec<WindowsLibrary>,
    pub failures: Vec<LibraryFailure>,
}

pub fn folder_path() -> io::Result<PathBuf> {
    let value = unsafe {
        SHGetKnownFolderPath(&FOLDERID_Libraries, KF_FLAG_DEFAULT, None).map_err(windows_error)?
    };
    let path = PathBuf::from(take_shell_string(value));
    if path.as_os_str().is_empty() {
        Err(io::Error::other("Windows Libraries folder path is empty"))
    } else {
        Ok(path)
    }
}
pub fn enumerate() -> io::Result<LibraryEnumeration> {
    let _apartment = ComApartment::initialize()?;
    let root: IShellItem = unsafe {
        SHGetKnownFolderItem(&FOLDERID_Libraries, KF_FLAG_DEFAULT, None).map_err(windows_error)?
    };
    let items: IEnumShellItems = unsafe {
        root.BindToHandler(None, &BHID_EnumItems)
            .map_err(windows_error)?
    };

    let mut raw = Vec::new();
    loop {
        let mut next = [None];
        let mut fetched = 0;
        let result = unsafe { items.Next(&mut next, Some(&mut fetched)) };
        if fetched == 0 {
            break;
        }
        if let Err(error) = result {
            return Err(windows_error(error));
        }
        if let Some(item) = next[0].take() {
            raw.push(item);
        }
    }
    Ok(map_shell_items(raw))
}

fn map_shell_items(items: Vec<IShellItem>) -> LibraryEnumeration {
    let mut result = LibraryEnumeration::default();
    for (sort_order, item) in items.into_iter().enumerate() {
        let identity = shell_name(&item, SIGDN_DESKTOPABSOLUTEPARSING).ok();
        match map_library(&item, sort_order as u32) {
            Ok(Some(library)) => result.libraries.push(library),
            Ok(None) => {}
            Err(error) => result.failures.push(LibraryFailure {
                shell_identity: identity,
                message: error.to_string(),
            }),
        }
    }
    result
}

fn map_library(item: &IShellItem, sort_order: u32) -> io::Result<Option<WindowsLibrary>> {
    let shell_identity = shell_name(item, SIGDN_DESKTOPABSOLUTEPARSING)?;
    let library: IShellLibrary = unsafe {
        CoCreateInstance(&ShellLibrary, None, CLSCTX_INPROC_SERVER).map_err(windows_error)?
    };
    unsafe {
        library
            .LoadLibraryFromItem(item, STGM_READ.0)
            .map_err(windows_error)?;
    }
    let pinned =
        unsafe { library.GetOptions().map_err(windows_error)? }.contains(LOF_PINNEDTONAVPANE);
    if !pinned {
        return Ok(None);
    }

    let display_name = shell_name(item, SIGDN_NORMALDISPLAY)?;
    let definition_path = shell_name(item, SIGDN_FILESYSPATH)
        .ok()
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let sources = library_sources(&library)?;
    let default_save_path = unsafe { library.GetDefaultSaveFolder::<IShellItem>(DSFT_DETECT).ok() }
        .and_then(|save_item| shell_name(&save_item, SIGDN_FILESYSPATH).ok())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    Ok(Some(WindowsLibrary {
        id: LibraryId(shell_identity),
        definition_path,
        display_name,
        pinned,
        sort_order,
        sources,
        default_save_path,
    }))
}

fn library_sources(library: &IShellLibrary) -> io::Result<Vec<LibrarySource>> {
    let items: IShellItemArray =
        unsafe { library.GetFolders(LFF_ALLITEMS).map_err(windows_error)? };
    let count = unsafe { items.GetCount().map_err(windows_error)? };
    let mut sources = Vec::with_capacity(count as usize);
    for index in 0..count {
        let item = unsafe { items.GetItemAt(index).map_err(windows_error)? };
        let path = shell_name(&item, SIGDN_FILESYSPATH)
            .ok()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let shell_identity = shell_name(&item, SIGDN_DESKTOPABSOLUTEPARSING).or_else(|_| {
            path.as_ref()
                .map(|path| path.as_os_str().to_owned())
                .ok_or_else(|| io::Error::other("library source has no stable Shell identity"))
        })?;
        sources.push(LibrarySource {
            shell_identity,
            path,
        });
    }
    Ok(sources)
}

fn shell_name(item: &IShellItem, format: windows::Win32::UI::Shell::SIGDN) -> io::Result<OsString> {
    let value = unsafe { item.GetDisplayName(format).map_err(windows_error)? };
    Ok(take_shell_string(value))
}

fn take_shell_string(value: PWSTR) -> OsString {
    if value.is_null() {
        return OsString::new();
    }
    let mut length = 0;
    unsafe {
        while *value.0.add(length) != 0 {
            length += 1;
        }
    }
    let owned = OsString::from_wide(unsafe { std::slice::from_raw_parts(value.0, length) });
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    owned
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> io::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .map(|| Self)
            .map_err(windows_error)
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
    fn library_id_preserves_non_ascii_shell_identity() {
        let identity = OsString::from(r"::{031E4825-7B94-4DC3-B131-E946B44C8DD5}\项目.library-ms");
        let id = LibraryId(identity.clone());
        assert_eq!(id.as_os_str(), identity.as_os_str());
    }

    #[test]
    fn owned_model_keeps_same_named_sources_distinct() {
        let sources = [
            LibrarySource {
                shell_identity: OsString::from(r"C:\first\same.txt"),
                path: Some(PathBuf::from(r"C:\first\same.txt")),
            },
            LibrarySource {
                shell_identity: OsString::from(r"D:\second\same.txt"),
                path: Some(PathBuf::from(r"D:\second\same.txt")),
            },
        ];
        assert_ne!(sources[0].shell_identity, sources[1].shell_identity);
        assert_ne!(sources[0].path, sources[1].path);
    }

    #[test]
    fn folder_path_reads_current_users_libraries_directory() {
        let path = folder_path().expect("Windows Libraries folder path must be readable");
        assert!(path.is_absolute());
        assert_eq!(path.file_name().and_then(OsStr::to_str), Some("Libraries"));
    }
    #[test]
    fn enumeration_reads_current_shell_libraries_without_mutating_them() {
        let enumeration = enumerate().expect("Windows Libraries namespace must be readable");
        for (index, library) in enumeration.libraries.iter().enumerate() {
            assert!(library.pinned);
            assert!(!library.id.as_os_str().is_empty());
            assert!(!library.display_name.is_empty());
            assert!(library.sort_order >= index as u32);
            assert!(
                library
                    .sources
                    .iter()
                    .all(|source| !source.shell_identity.is_empty())
            );
        }
    }
}
