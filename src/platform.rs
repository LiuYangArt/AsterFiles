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

    use super::{KnownLocation, KnownLocationKind, Path, PathBuf};

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
        let path = path
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>();
        let operation = "open\0".encode_utf16().collect::<Vec<_>>();
        let result = unsafe {
            ShellExecuteW(
                ptr::null_mut(),
                operation.as_ptr(),
                path.as_ptr(),
                ptr::null(),
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
pub use windows_impl::{double_click_interval, known_locations, open_path};

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
pub fn open_path(path: &Path) -> std::io::Result<()> {
    std::process::Command::new("xdg-open")
        .arg(path)
        .spawn()
        .map(|_| ())
}
