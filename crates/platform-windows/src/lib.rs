#![deny(clippy::undocumented_unsafe_blocks)]

use crate as platform;
use asterfiles_domain as domain;
use asterfiles_domain::{network, operation_audit};
pub mod fs;

#[cfg(windows)]
pub mod windows;

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownLocationKind {
    Pinned,
    Drive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownLocation {
    pub kind: KnownLocationKind,
    pub label: String,
    pub path: PathBuf,
    pub drive_root: Option<PathBuf>,
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
    // SAFETY: The registry key and names are NUL-terminated; value and size are writable DWORD storage valid for this synchronous query.
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

#[cfg(windows)]
mod windows_impl {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
        ptr,
    };

    use windows_sys::Win32::{
        Storage::FileSystem::{GetLogicalDrives, GetVolumeInformationW},
        System::Com::CoTaskMemFree,
        UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
    };

    use super::{KnownLocation, KnownLocationKind, Path, PathBuf, ShortcutTarget};
    use windows::{
        Win32::{
            Foundation::RPC_E_CHANGED_MODE,
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
        // SAFETY: This no-argument system query borrows no Rust memory and returns an integer, not an owned resource.
        std::time::Duration::from_millis(unsafe { GetDoubleClickTime() }.into())
    }
    pub fn logical_drive_locations() -> impl Iterator<Item = KnownLocation> {
        logical_drives()
    }

    pub fn explorer_pinned_locations() -> std::io::Result<Vec<KnownLocation>> {
        use windows::{
            Win32::{
                System::SystemServices::SFGAO_FOLDER,
                UI::Shell::{
                    BHID_EnumItems, IEnumShellItems, IShellItem, IShellItem2,
                    SHCreateItemFromParsingName, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
                    SIGDN_NORMALDISPLAY,
                },
            },
            core::{Interface, PCWSTR, PWSTR},
        };
        struct ComGuard;
        impl Drop for ComGuard {
            fn drop(&mut self) {
                // SAFETY: COM uninitialization must balance success on this same thread. The eager
                // then_some failure path does not establish that prerequisite; see #183.
                unsafe { CoUninitialize() };
            }
        }
        // SAFETY: COM initialization affects this thread only; a successful result is balanced by the local guard.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let _guard = initialized.then_some(ComGuard);
        let namespace = "shell:::{3936e9e4-d92c-4eee-a85a-bc16d5ea0819}\0"
            .encode_utf16()
            .collect::<Vec<_>>();
        // SAFETY: The NUL-terminated namespace buffer stays alive for the call; the returned interface owns its COM reference.
        let root: IShellItem = unsafe {
            SHCreateItemFromParsingName(PCWSTR(namespace.as_ptr()), None)
                .map_err(std::io::Error::other)?
        };
        // SAFETY: root owns a live Shell interface on this thread; the enumeration handler GUID has the requested interface type.
        let items: IEnumShellItems = unsafe {
            root.BindToHandler(None, &BHID_EnumItems)
                .map_err(std::io::Error::other)?
        };
        let mut result = Vec::new();
        loop {
            let mut next = [None];
            let mut fetched = 0;
            // SAFETY: next has one initialized interface slot and fetched is writable; items remains owned on this COM thread.
            if unsafe { items.Next(&mut next, Some(&mut fetched)) }.is_err() || fetched == 0 {
                break;
            }
            let Some(item) = next[0].take() else { continue };
            let pinned = item
                .cast::<IShellItem2>()
                // SAFETY: properties is a successful IShellItem2 cast on the same thread; GetBool borrows only the static property key.
                .and_then(|properties| unsafe {
                    properties
                        .GetBool(&windows::Win32::Storage::EnhancedStorage::PKEY_Home_IsPinned)
                })
                .is_ok_and(|value| value.as_bool());
            if !pinned {
                continue;
            }
            // SAFETY: item owns a live Shell interface and the attribute mask contains only SFGAO_FOLDER.
            let attributes = unsafe { item.GetAttributes(SFGAO_FOLDER) }.unwrap_or_default();
            if !attributes.contains(SFGAO_FOLDER) {
                continue;
            }
            // SAFETY: item is alive on this COM thread; successful display names are task-allocated strings consumed by take_shell_string.
            let Ok(path) = (unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) })
                // SAFETY: The same live item is queried; the returned task-allocated string is consumed by take_shell_string.
                .or_else(|_| unsafe { item.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) })
            else {
                continue;
            };
            let path = take_shell_string(path);
            if path.as_os_str().is_empty() {
                continue;
            }
            // SAFETY: The live item returns an owned, NUL-terminated Shell string, which the map below copies and frees.
            let label = unsafe { item.GetDisplayName(SIGDN_NORMALDISPLAY) }
                .map(take_shell_string)
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|_| {
                    path.file_name()
                        .unwrap_or(path.as_os_str())
                        .to_string_lossy()
                        .into_owned()
                });
            result.push(KnownLocation {
                kind: KnownLocationKind::Pinned,
                label,
                path,
                drive_root: None,
            });
        }
        return Ok(result);

        fn take_shell_string(value: PWSTR) -> PathBuf {
            if value.is_null() {
                return PathBuf::new();
            }
            let mut len = 0;
            // SAFETY: value is a non-null task-allocated UTF-16 string returned by GetDisplayName and remains allocated until below.
            unsafe {
                while *value.0.add(len) != 0 {
                    len += 1;
                }
            }
            // SAFETY: len was counted before the terminating NUL in this live, aligned Shell UTF-16 allocation.
            let path = PathBuf::from(OsString::from_wide(unsafe {
                std::slice::from_raw_parts(value.0, len)
            }));
            // SAFETY: The Shell transferred this allocation to us; its contents have been copied and it is freed exactly once.
            unsafe { CoTaskMemFree(value.0.cast()) };
            path
        }
    }

    fn logical_drives() -> impl Iterator<Item = KnownLocation> {
        // SAFETY: This no-argument query returns a drive bitmask and does not borrow or retain Rust memory.
        let mask = unsafe { GetLogicalDrives() };
        (0..26)
            .filter(move |index| mask & (1 << index) != 0)
            .map(|index| {
                let letter = (b'A' + index as u8) as char;
                let drive_root = PathBuf::from(format!(r"{letter}:\"));
                let volume = volume_label(&drive_root);
                let label = if volume.is_empty() {
                    format!("{letter}:")
                } else {
                    format!("{volume} ({letter}:)")
                };
                KnownLocation {
                    kind: KnownLocationKind::Drive,
                    label,
                    path: drive_root.clone(),
                    drive_root: Some(drive_root),
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
        // SAFETY: root is NUL-terminated and label has label.len() writable UTF-16 elements; unused optional outputs are null.
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
    pub fn open_windows_credentials() -> std::io::Result<()> {
        shell_execute(
            Path::new("control.exe"),
            Some(std::ffi::OsStr::new("/name Microsoft.CredentialManager")),
        )
    }
    pub fn open_url(url: &str) -> std::io::Result<()> {
        shell_execute(Path::new(url), None)
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
        // SAFETY: Initialization is thread-local; only successful calls are balanced below, and an existing apartment is reused.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let should_uninitialize = initialized.is_ok();
        if initialized.is_err() && initialized != RPC_E_CHANGED_MODE {
            return Err(std::io::Error::other(format!(
                "CoInitializeEx failed: {initialized:?}"
            )));
        }
        // SAFETY: COM calls stay on this thread; path buffers remain NUL-terminated and live, and each returned interface owns its reference.
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
        if should_uninitialize {
            // SAFETY: This thread is balancing exactly its successful CoInitializeEx call after all local interfaces have been dropped.
            unsafe { CoUninitialize() };
        }
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
        // SAFETY: Target, verb and optional arguments are NUL-terminated buffers kept alive through this synchronous Shell call.
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
#[allow(unused_imports)]
pub use windows_impl::{
    double_click_interval, explorer_pinned_locations, logical_drive_locations, open_path, open_url,
    open_windows_credentials, request_folder_access, resolve_shortcut_target,
};

#[cfg(windows)]
pub fn set_current_thread_low_priority() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_BELOW_NORMAL,
    };

    // SAFETY: GetCurrentThread returns the current thread pseudo-handle; SetThreadPriority does not retain it or borrow Rust memory.
    unsafe {
        let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
    }
}

#[cfg(not(windows))]
pub fn set_current_thread_low_priority() {}

pub fn local_utc_offset_seconds() -> i32 {
    use windows_sys::Win32::System::Time::{
        GetTimeZoneInformation, TIME_ZONE_ID_INVALID, TIME_ZONE_INFORMATION,
    };
    let mut information = TIME_ZONE_INFORMATION::default();
    // SAFETY: information is initialized writable TIME_ZONE_INFORMATION storage for the duration of the synchronous query.
    let state = unsafe { GetTimeZoneInformation(&mut information) };
    if state == TIME_ZONE_ID_INVALID {
        return 0;
    }
    let seasonal_bias = match state {
        1 => information.StandardBias,
        2 => information.DaylightBias,
        _ => 0,
    };
    -(information.Bias.saturating_add(seasonal_bias)).saturating_mul(60)
}
