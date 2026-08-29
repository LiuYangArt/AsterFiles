pub mod windows_shell_icons;

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownLocationKind {
    Home,
    Desktop,
    Downloads,
    Documents,
    Pictures,
    Music,
    Videos,
    Drive,
}

#[derive(Debug, Clone)]
pub struct KnownLocation {
    pub kind: KnownLocationKind,
    pub label: String,
    pub path: PathBuf,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortcutTarget {
    pub path: PathBuf,
    pub is_directory: Option<bool>,
}

#[cfg(windows)]
pub fn system_uses_dark_theme() -> bool {
    use std::{ffi::c_void, ptr};
    use windows_sys::Win32::{
        Foundation::ERROR_SUCCESS,
        System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW},
    };

    let subkey = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize\0"
        .encode_utf16()
        .collect::<Vec<_>>();
    let value_name = "AppsUseLightTheme\0".encode_utf16().collect::<Vec<_>>();
    let mut value = 1_u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let result = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            subkey.as_ptr(),
            value_name.as_ptr(),
            RRF_RT_REG_DWORD,
            ptr::null_mut(),
            (&mut value as *mut u32).cast::<c_void>(),
            &mut size,
        )
    };
    result == ERROR_SUCCESS && value == 0
}

#[cfg(not(windows))]
pub fn system_uses_dark_theme() -> bool {
    false
}

#[cfg(windows)]
mod windows_impl {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        ptr,
    };

    use windows_sys::{
        Win32::{
            Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW},
            System::Com::CoTaskMemFree,
            UI::{
                Shell::{
                    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music,
                    FOLDERID_Pictures, FOLDERID_Profile, FOLDERID_Videos, SHGetKnownFolderPath,
                    ShellExecuteW,
                },
                WindowsAndMessaging::SW_SHOWNORMAL,
            },
        },
        core::GUID,
    };

    use super::{KnownLocation, KnownLocationKind, Path, PathBuf, ShortcutTarget};
    use windows::{
        Win32::{
            Storage::FileSystem::{
                FILE_ATTRIBUTE_DIRECTORY, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
                WIN32_FIND_DATAW,
            },
            System::Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoUninitialize, IPersistFile, STGM_READ,
            },
            UI::Shell::{IShellLinkW, SLGP_RAWPATH, ShellLink},
        },
        core::Interface,
    };

    pub fn double_click_interval() -> std::time::Duration {
        use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetDoubleClickTime;
        std::time::Duration::from_millis(unsafe { GetDoubleClickTime() }.into())
    }
    pub fn known_locations() -> Vec<KnownLocation> {
        let specifications = [
            (KnownLocationKind::Home, "主页", FOLDERID_Profile),
            (KnownLocationKind::Desktop, "桌面", FOLDERID_Desktop),
            (KnownLocationKind::Downloads, "下载", FOLDERID_Downloads),
            (KnownLocationKind::Documents, "文档", FOLDERID_Documents),
            (KnownLocationKind::Pictures, "图片", FOLDERID_Pictures),
            (KnownLocationKind::Music, "音乐", FOLDERID_Music),
            (KnownLocationKind::Videos, "视频", FOLDERID_Videos),
        ];
        specifications
            .into_iter()
            .filter_map(|(kind, label, id)| {
                known_folder(&id).map(|path| KnownLocation {
                    kind,
                    label: label.to_owned(),
                    path,
                })
            })
            .chain(logical_drives())
            .collect()
    }

    fn known_folder(id: &GUID) -> Option<PathBuf> {
        let mut pointer = ptr::null_mut();
        let result = unsafe { SHGetKnownFolderPath(id, 0, ptr::null_mut(), &mut pointer) };
        if result < 0 || pointer.is_null() {
            return None;
        }
        let length = unsafe {
            let mut length = 0;
            while *pointer.add(length) != 0 {
                length += 1;
            }
            length
        };
        let path = PathBuf::from(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(pointer, length)
        }));
        unsafe { CoTaskMemFree(pointer.cast()) };
        Some(path)
    }

    fn logical_drives() -> impl Iterator<Item = KnownLocation> {
        let mask = unsafe { GetLogicalDrives() };
        (0..26)
            .filter(move |index| mask & (1 << index) != 0)
            .map(|index| {
                let letter = (b'A' + index as u8) as char;
                let path = PathBuf::from(format!(r"{letter}:\"));
                let volume = volume_label(&path);
                let label = if volume.is_empty() {
                    format!("{letter}:")
                } else {
                    format!("{volume} ({letter}:)")
                };
                KnownLocation {
                    kind: KnownLocationKind::Drive,
                    label,
                    path,
                }
            })
    }

    fn volume_label(path: &Path) -> String {
        let root = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let mut label = [0_u16; 261];
        let success = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                label.as_mut_ptr(),
                label.len() as u32,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                0,
            )
        };
        if success == 0 {
            return String::new();
        }
        let length = label
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(label.len());
        String::from_utf16_lossy(&label[..length])
    }

    pub fn open_path(path: &Path) -> std::io::Result<()> {
        shell_execute(path, None)
    }
    pub fn resolve_shortcut_target(path: &Path) -> std::io::Result<Option<ShortcutTarget>> {
        if !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
        {
            return Ok(None);
        }

        let link_path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if initialized.is_err() {
            return Err(std::io::Error::other(format!(
                "CoInitializeEx failed: {initialized:?}"
            )));
        }
        let result = (|| unsafe {
            let link: IShellLinkW =
                CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER).map_err(windows_error)?;
            let persist: IPersistFile = link.cast().map_err(windows_error)?;
            persist
                .Load(windows::core::PCWSTR(link_path.as_ptr()), STGM_READ)
                .map_err(windows_error)?;

            let mut target = vec![0_u16; 32_768];
            let mut find_data = std::mem::zeroed::<WIN32_FIND_DATAW>();
            link.GetPath(&mut target, &mut find_data, SLGP_RAWPATH.0 as u32)
                .map_err(windows_error)?;
            let length = target
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(target.len());
            if length == 0 {
                return Ok(None);
            }
            let target_path = PathBuf::from(OsString::from_wide(&target[..length]));
            let target_wide = target_path
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            let live_attributes = GetFileAttributesW(windows::core::PCWSTR(target_wide.as_ptr()));
            let is_directory = if live_attributes == INVALID_FILE_ATTRIBUTES {
                (find_data.dwFileAttributes != 0)
                    .then_some(find_data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0)
            } else {
                Some(live_attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0)
            };
            Ok(Some(ShortcutTarget {
                path: target_path,
                is_directory,
            }))
        })();
        unsafe { CoUninitialize() };
        result
    }

    fn windows_error(error: windows::core::Error) -> std::io::Error {
        std::io::Error::other(error.to_string())
    }

    pub fn request_folder_access(path: &Path) -> std::io::Result<()> {
        let mut arguments = OsString::from("\"");
        arguments.push(path.as_os_str());
        arguments.push("\"");
        shell_execute(Path::new("explorer.exe"), Some(arguments.as_os_str()))
    }

    fn shell_execute(target: &Path, arguments: Option<&std::ffi::OsStr>) -> std::io::Result<()> {
        let target = target
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let arguments =
            arguments.map(|value| value.encode_wide().chain(Some(0)).collect::<Vec<_>>());
        let operation = "open\0".encode_utf16().collect::<Vec<_>>();
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                operation.as_ptr(),
                target.as_ptr(),
                arguments
                    .as_ref()
                    .map_or(ptr::null(), |value| value.as_ptr()),
                ptr::null(),
                SW_SHOWNORMAL,
            )
        } as isize;
        if result <= 32 {
            Err(std::io::Error::other(format!(
                "ShellExecuteW failed with code {result}"
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(windows)]
pub use windows_impl::{
    double_click_interval, known_locations, open_path, request_folder_access,
    resolve_shortcut_target,
};

#[cfg(not(windows))]
pub fn double_click_interval() -> std::time::Duration {
    std::time::Duration::from_millis(500)
}
#[cfg(not(windows))]
pub fn known_locations() -> Vec<KnownLocation> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|path| {
            vec![KnownLocation {
                kind: KnownLocationKind::Home,
                label: "Home".to_owned(),
                path,
            }]
        })
        .unwrap_or_default()
}

#[cfg(not(windows))]
pub fn resolve_shortcut_target(_path: &Path) -> std::io::Result<Option<ShortcutTarget>> {
    Ok(None)
}
#[cfg(not(windows))]
pub fn open_path(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
}
#[cfg(not(windows))]
pub fn request_folder_access(path: &Path) -> std::io::Result<()> {
    open_path(path)
}
