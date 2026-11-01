use std::{ffi::OsString, io, os::windows::ffi::OsStringExt, path::PathBuf};

use windows::{
    Win32::{
        Foundation::{ERROR_CANCELLED, HWND, RPC_E_CHANGED_MODE},
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            Common::COMDLG_FILTERSPEC, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
            FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        },
    },
    core::{HRESULT, PCWSTR, PWSTR},
};

struct ComGuard;

impl ComGuard {
    fn initialize() -> io::Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Err(io::Error::other(
                "Everything program picker requires an STA thread",
            ));
        }
        result.ok().map_err(windows_error)?;
        Ok(Self)
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

pub fn pick_everything_executable(owner_window: isize) -> io::Result<Option<PathBuf>> {
    let _com = ComGuard::initialize()?;
    let dialog: IFileOpenDialog = unsafe {
        CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).map_err(windows_error)?
    };

    let filter_name = wide("Everything 1.5 x64 (Everything64.exe)");
    let filter_pattern = wide("Everything64.exe");
    let default_extension = wide("exe");
    let default_name = wide("Everything64.exe");
    let filter = [COMDLG_FILTERSPEC {
        pszName: PCWSTR(filter_name.as_ptr()),
        pszSpec: PCWSTR(filter_pattern.as_ptr()),
    }];

    unsafe {
        dialog
            .SetOptions(FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST | FOS_PATHMUSTEXIST)
            .map_err(windows_error)?;
        dialog.SetFileTypes(&filter).map_err(windows_error)?;
        dialog
            .SetDefaultExtension(PCWSTR(default_extension.as_ptr()))
            .map_err(windows_error)?;
        dialog
            .SetFileName(PCWSTR(default_name.as_ptr()))
            .map_err(windows_error)?;
    }

    let owner = (owner_window != 0).then_some(HWND(owner_window as *mut _));
    if let Err(error) = unsafe { dialog.Show(owner) } {
        return if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) {
            Ok(None)
        } else {
            Err(windows_error(error))
        };
    }

    let item = unsafe { dialog.GetResult() }.map_err(windows_error)?;
    let value = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.map_err(windows_error)?;
    Ok(Some(take_shell_path(value)))
}

fn take_shell_path(value: PWSTR) -> PathBuf {
    if value.is_null() {
        return PathBuf::new();
    }
    let mut length = 0;
    unsafe {
        while *value.0.add(length) != 0 {
            length += 1;
        }
    }
    let path = PathBuf::from(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(value.0, length)
    }));
    unsafe { CoTaskMemFree(Some(value.0.cast())) };
    path
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}
