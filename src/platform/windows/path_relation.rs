use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io,
    os::windows::{ffi::OsStringExt, fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};

use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_INFO, FILE_NAME_OPENED, FileIdInfo, GetFileInformationByHandle,
    GetFileInformationByHandleEx, GetFinalPathNameByHandleW,
};

use crate::domain::file_operations::CancellationToken;

#[derive(Debug)]
struct Identity {
    standard: (u32, u64),
    local: Option<(u64, [u8; 16])>,
}

impl Identity {
    fn same_file(&self, other: &Self) -> bool {
        match (self.local, other.local) {
            (Some(left), Some(right)) => left == right,
            // A local path and a loopback share can identify the same directory.
            _ => self.standard == other.standard,
        }
    }
}

fn identity(path: &Path, open_link: bool) -> io::Result<Identity> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if open_link {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(flags)
        .open(path)?;
    // Resolve the opened handle so mapped drives and directory aliases use the same policy.
    let mut name = vec![0_u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                name.as_mut_ptr(),
                name.len() as u32,
                FILE_NAME_OPENED,
            )
        };
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if (length as usize) < name.len() {
            name.truncate(length as usize);
            break;
        }
        name.resize(length as usize + 1, 0);
    }
    let resolved = OsString::from_wide(&name);
    let mut standard: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut standard) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let standard = (
        standard.dwVolumeSerialNumber,
        (u64::from(standard.nFileIndexHigh) << 32) | u64::from(standard.nFileIndexLow),
    );
    if crate::network::is_unc_path(Path::new(&resolved)) {
        // SMB servers need not implement FileIdInfo; use their standard handle identity.
        return Ok(Identity {
            standard,
            local: None,
        });
    }
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            std::ptr::addr_of_mut!(info).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Identity {
        standard,
        local: Some((info.VolumeSerialNumber, info.FileId.Identifier)),
    })
}

// Only the worker's safety comparison resolves aliases; operation paths remain unchanged.
pub fn destination_is_within_source(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
) -> io::Result<bool> {
    let check_cancel = || -> io::Result<()> {
        if cancel.is_cancelled() {
            Err(io::ErrorKind::Interrupted.into())
        } else {
            Ok(())
        }
    };
    check_cancel()?;
    // Preserve link-copy semantics by identifying the source entry itself.
    let source_id = identity(source, true)?;
    let absolute = std::path::absolute(destination)?;
    let mut existing = absolute.as_path();
    let resolved = loop {
        check_cancel()?;
        match fs::symlink_metadata(existing) {
            Ok(_) => {
                break fs::canonicalize(existing)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                existing = existing.parent().ok_or(error)?;
            }
            Err(error) => return Err(error),
        }
    };
    // Walk the resolved parent chain: a junction's lexical parent can be unrelated.
    for ancestor in resolved.ancestors() {
        check_cancel()?;
        if identity(ancestor, false)?.same_file(&source_id) {
            return Ok(true);
        }
    }
    check_cancel()?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::Identity;

    #[test]
    fn issue_135_remote_identity_matches_local_alias_without_truncating_local_ids() {
        let local = Identity {
            standard: (7, 42),
            local: Some((7, [1; 16])),
        };
        let remote = Identity {
            standard: (7, 42),
            local: None,
        };
        let other_local = Identity {
            standard: (7, 42),
            local: Some((7, [2; 16])),
        };
        let other_remote = Identity {
            standard: (7, 43),
            local: None,
        };
        assert!(local.same_file(&remote));
        assert!(remote.same_file(&local));
        assert!(remote.same_file(&remote));
        assert!(!local.same_file(&other_local));
        assert!(!remote.same_file(&other_remote));
    }
}
