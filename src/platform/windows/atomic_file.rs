use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum InjectedFailure {
    None,
    #[cfg(test)]
    TemporaryWrite,
    #[cfg(test)]
    Sync,
}

pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    write_impl(path, bytes, InjectedFailure::None)
}

#[cfg(test)]
pub(crate) fn write_with_failure(
    path: &Path,
    bytes: &[u8],
    failure: AtomicWriteFailure,
) -> io::Result<()> {
    let failure = match failure {
        AtomicWriteFailure::TemporaryWrite => InjectedFailure::TemporaryWrite,
        AtomicWriteFailure::Sync => InjectedFailure::Sync,
    };
    write_impl(path, bytes, failure)
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum AtomicWriteFailure {
    TemporaryWrite,
    Sync,
}

fn write_impl(path: &Path, bytes: &[u8], failure: InjectedFailure) -> io::Result<()> {
    let (temporary_path, mut temporary_file) = create_temporary(path)?;
    let prepared = prepare_temporary(&temporary_path, &mut temporary_file, bytes, failure);
    drop(temporary_file);
    if let Err(error) = prepared {
        return Err(clean_up_after_error(&temporary_path, error));
    }

    let temporary_wide = wide_null(&temporary_path);
    let destination_wide = wide_null(path);
    if unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            destination_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        let error = stage_error("replace", path, io::Error::last_os_error());
        return Err(clean_up_after_error(&temporary_path, error));
    }
    Ok(())
}

fn create_temporary(path: &Path) -> io::Result<(PathBuf, File)> {
    let parent = path.parent().ok_or_else(|| {
        stage_error(
            "temporary-create",
            path,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination has no parent directory",
            ),
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        stage_error(
            "temporary-create",
            path,
            io::Error::new(io::ErrorKind::InvalidInput, "destination has no file name"),
        )
    })?;
    for _ in 0..128 {
        let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
        let mut temporary_name = OsString::from(".");
        temporary_name.push(file_name);
        temporary_name.push(format!(
            ".asterfiles-writing-{}-{sequence}",
            std::process::id()
        ));
        let temporary_path = parent.join(temporary_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => return Ok((temporary_path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(stage_error("temporary-create", &temporary_path, error)),
        }
    }
    Err(stage_error(
        "temporary-create",
        path,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique temporary file",
        ),
    ))
}

fn prepare_temporary(
    path: &Path,
    file: &mut File,
    bytes: &[u8],
    _failure: InjectedFailure,
) -> io::Result<()> {
    #[cfg(test)]
    if matches!(_failure, InjectedFailure::TemporaryWrite) {
        file.write_all(&bytes[..bytes.len() / 2])
            .map_err(|error| stage_error("temporary-write", path, error))?;
        return Err(stage_error(
            "temporary-write",
            path,
            io::Error::from_raw_os_error(112),
        ));
    }

    file.write_all(bytes)
        .map_err(|error| stage_error("temporary-write", path, error))?;

    #[cfg(test)]
    if matches!(_failure, InjectedFailure::Sync) {
        return Err(stage_error(
            "temporary-sync",
            path,
            io::Error::other("injected synchronization failure"),
        ));
    }

    file.sync_all()
        .map_err(|error| stage_error("temporary-sync", path, error))
}

fn clean_up_after_error(temporary_path: &Path, error: io::Error) -> io::Error {
    match fs::remove_file(temporary_path) {
        Ok(()) => error,
        Err(cleanup) if cleanup.kind() == io::ErrorKind::NotFound => error,
        Err(cleanup) => {
            let kind = error.kind();
            io::Error::new(
                kind,
                AtomicFileError {
                    message: format!(
                        "{error}; temporary cleanup failed for {temporary_path:?}: {cleanup}"
                    ),
                    stage: stage(&error).unwrap_or("unknown"),
                    raw_os_error: raw_os_error(&error),
                    cleanup_raw_os_error: cleanup.raw_os_error(),
                },
            )
        }
    }
}

pub(crate) fn stage(error: &io::Error) -> Option<&'static str> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<AtomicFileError>())
        .map(|inner| inner.stage)
}

pub(crate) fn cleanup_raw_os_error(error: &io::Error) -> Option<i32> {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<AtomicFileError>())
        .and_then(|inner| inner.cleanup_raw_os_error)
}
pub(crate) fn raw_os_error(error: &io::Error) -> Option<i32> {
    error.raw_os_error().or_else(|| {
        error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<AtomicFileError>())
            .and_then(|inner| inner.raw_os_error)
    })
}

#[derive(Debug)]
struct AtomicFileError {
    message: String,
    stage: &'static str,
    raw_os_error: Option<i32>,
    cleanup_raw_os_error: Option<i32>,
}

impl std::fmt::Display for AtomicFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AtomicFileError {}
fn stage_error(stage: &'static str, path: &Path, error: io::Error) -> io::Error {
    let kind = error.kind();
    let raw_os_error = error.raw_os_error();
    io::Error::new(
        kind,
        AtomicFileError {
            message: format!("atomic session {stage} failed for {path:?}: {error}"),
            stage,
            raw_os_error,
            cleanup_raw_os_error: None,
        },
    )
}

fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
