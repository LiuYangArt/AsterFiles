use std::{
    fs::{self, OpenOptions},
    io,
    os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
    path::Path,
};

use windows_sys::Win32::Storage::FileSystem::{
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FileIdInfo,
    GetFileInformationByHandleEx,
};

use crate::{domain::file_operations::CancellationToken, fs::file_operations::OperationError};

fn identity(path: &Path, open_link: bool) -> io::Result<(u64, [u8; 16])> {
    let mut flags = FILE_FLAG_BACKUP_SEMANTICS;
    if open_link {
        flags |= FILE_FLAG_OPEN_REPARSE_POINT;
    }
    let file = OpenOptions::new()
        .access_mode(0)
        .custom_flags(flags)
        .open(path)?;
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
    Ok((info.VolumeSerialNumber, info.FileId.Identifier))
}

// Only the worker's safety comparison resolves aliases; operation paths remain unchanged.
pub fn destination_is_within_source(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
) -> Result<bool, OperationError> {
    let check_cancel = || {
        if cancel.is_cancelled() {
            Err(OperationError::Cancelled)
        } else {
            Ok(())
        }
    };
    check_cancel()?;
    // Preserve link-copy semantics by identifying the source entry itself.
    let source_id = identity(source, true).map_err(|e| OperationError::io(source, e))?;
    let absolute =
        std::path::absolute(destination).map_err(|e| OperationError::io(destination, e))?;
    let mut existing = absolute.as_path();
    let resolved = loop {
        check_cancel()?;
        match fs::symlink_metadata(existing) {
            Ok(_) => {
                break fs::canonicalize(existing).map_err(|e| OperationError::io(existing, e))?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                existing = existing
                    .parent()
                    .ok_or_else(|| OperationError::io(existing, error))?;
            }
            Err(error) => return Err(OperationError::io(existing, error)),
        }
    };
    // Walk the resolved parent chain: a junction's lexical parent can be unrelated.
    for ancestor in resolved.ancestors() {
        check_cancel()?;
        if identity(ancestor, false).map_err(|e| OperationError::io(ancestor, e))? == source_id {
            return Ok(true);
        }
    }
    check_cancel()?;
    Ok(false)
}
