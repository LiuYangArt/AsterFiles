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
    #[cfg(any(test, feature = "test-support"))]
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
    pub display_name: OsString,
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
    // SAFETY: The known-folder GUID is static; the successful call transfers a task-allocated UTF-16 string to take_shell_string.
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
    // SAFETY: The thread holds ComApartment; the static folder GUID requests a Shell item with an owned COM reference.
    let root: IShellItem = unsafe {
        SHGetKnownFolderItem(&FOLDERID_Libraries, KF_FLAG_DEFAULT, None).map_err(windows_error)?
    };
    // SAFETY: root is owned on this COM thread and the handler/interface pair requests IEnumShellItems.
    let items: IEnumShellItems = unsafe {
        root.BindToHandler(None, &BHID_EnumItems)
            .map_err(windows_error)?
    };

    let mut raw = Vec::new();
    loop {
        let mut next = [None];
        let mut fetched = 0;
        // SAFETY: next has one initialized interface slot and fetched is a live writable count; items stays owned on this thread.
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
    // SAFETY: The caller retains its initialized COM apartment and the requested ShellLibrary interface is used on that same thread.
    let library: IShellLibrary = unsafe {
        CoCreateInstance(&ShellLibrary, None, CLSCTX_INPROC_SERVER).map_err(windows_error)?
    };
    // SAFETY: item and library are live same-thread COM interfaces and STGM_READ requests read-only library access.
    unsafe {
        library
            .LoadLibraryFromItem(item, STGM_READ.0)
            .map_err(windows_error)?;
    }
    let pinned =
        // SAFETY: library is a loaded, owned interface on this COM thread; GetOptions takes no borrowed output pointer.
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
    // SAFETY: library is loaded on this COM thread; the typed call returns an owned IShellItem reference when available.
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
        // SAFETY: library is a live same-thread interface; the requested IShellItemArray is owned by the returned COM wrapper.
        unsafe { library.GetFolders(LFF_ALLITEMS).map_err(windows_error)? };
    // SAFETY: items owns a live Shell array on this thread and GetCount takes no caller-provided pointers.
    let count = unsafe { items.GetCount().map_err(windows_error)? };
    let mut sources = Vec::with_capacity(count as usize);
    for index in 0..count {
        // SAFETY: index is strictly below the successfully queried count; the array and returned item retain their COM references.
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
        let display_name = shell_name(&item, SIGDN_NORMALDISPLAY).unwrap_or_else(|_| {
            path.as_ref()
                .and_then(|path| path.file_name().map(OsStr::to_owned))
                .unwrap_or_else(|| shell_identity.clone())
        });
        sources.push(LibrarySource {
            shell_identity,
            display_name,
            path,
        });
    }
    Ok(sources)
}

fn shell_name(item: &IShellItem, format: windows::Win32::UI::Shell::SIGDN) -> io::Result<OsString> {
    // SAFETY: item is a borrowed live same-thread Shell interface; its returned task-allocated name is copied and freed below.
    let value = unsafe { item.GetDisplayName(format).map_err(windows_error)? };
    Ok(take_shell_string(value))
}

fn take_shell_string(value: PWSTR) -> OsString {
    if value.is_null() {
        return OsString::new();
    }
    let mut length = 0;
    // SAFETY: value is a non-null task-allocated, NUL-terminated UTF-16 string returned by Shell and not freed until below.
    unsafe {
        while *value.0.add(length) != 0 {
            length += 1;
        }
    }
    // SAFETY: length was counted before the terminator in the same live and aligned Shell UTF-16 allocation.
    let owned = OsString::from_wide(unsafe { std::slice::from_raw_parts(value.0, length) });
    // SAFETY: The name was allocated by Shell with the COM task allocator; we own it and have already copied its contents.
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    owned
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> io::Result<Self> {
        // SAFETY: This thread initializes its own STA; construction succeeds only when that initialization must later be balanced.
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .map(|| Self)
            .map_err(windows_error)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        // SAFETY: The stack guard stays on its initializing thread and balances exactly one successful CoInitializeEx call.
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
                display_name: OsString::from("same"),
                path: Some(PathBuf::from(r"C:\first\same.txt")),
            },
            LibrarySource {
                shell_identity: OsString::from(r"D:\second\same.txt"),
                display_name: OsString::from("same"),
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
