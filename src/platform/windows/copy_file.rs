use std::{ffi::c_void, io, mem::size_of, os::windows::ffi::OsStrExt, path::Path};

use windows::{
    Win32::{
        Foundation::{
            ERROR_BAD_NET_NAME, ERROR_BAD_NETPATH, ERROR_CONNECTION_UNAVAIL,
            ERROR_HOST_UNREACHABLE, ERROR_NETNAME_DELETED, ERROR_NETWORK_UNREACHABLE,
            ERROR_NO_NETWORK, ERROR_REQUEST_ABORTED, ERROR_REQUEST_PAUSED, ERROR_SEM_TIMEOUT,
            ERROR_UNEXP_NET_ERR, WIN32_ERROR,
        },
        Storage::FileSystem::{
            COPY_FILE_ENABLE_SPARSE_COPY, COPY_FILE_FAIL_IF_EXISTS,
            COPY_FILE_REQUEST_COMPRESSED_TRAFFIC, COPY_FILE_RESUME_FROM_PAUSE,
            COPYFILE2_CALLBACK_CHUNK_FINISHED, COPYFILE2_EXTENDED_PARAMETERS, COPYFILE2_MESSAGE,
            COPYFILE2_MESSAGE_ACTION, COPYFILE2_PROGRESS_CANCEL, COPYFILE2_PROGRESS_CONTINUE,
            COPYFILE2_PROGRESS_PAUSE, CopyFile2,
        },
    },
    core::PCWSTR,
};

use crate::domain::file_operations::CancellationToken;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFileErrorKind {
    Cancelled,
    Failed,
}

#[derive(Debug)]
pub struct CopyFileError {
    pub kind: CopyFileErrorKind,
    pub error: io::Error,
}

struct CopyContext<'a> {
    cancel: &'a CancellationToken,
    progress: &'a mut dyn FnMut(u64),
    last_total: u64,
    pause_requested: bool,
    callback_panicked: bool,
}

pub fn copy_file(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, CopyFileError> {
    let source_wide = wide_path(source);
    let destination_wide = wide_path(destination);
    let mut context = CopyContext {
        cancel,
        progress,
        last_total: 0,
        pause_requested: false,
        callback_panicked: false,
    };
    let mut resume = false;
    let network_copy =
        crate::network::is_unc_path(source) || crate::network::is_unc_path(destination);
    let mut retry_count = 0_u8;

    loop {
        if cancel.is_cancelled() {
            return Err(cancelled_error());
        }
        context.pause_requested = false;
        let mut flags = COPY_FILE_FAIL_IF_EXISTS | COPY_FILE_ENABLE_SPARSE_COPY;
        if network_copy {
            flags |= COPY_FILE_REQUEST_COMPRESSED_TRAFFIC;
        }
        if resume {
            flags |= COPY_FILE_RESUME_FROM_PAUSE;
        }
        let parameters = COPYFILE2_EXTENDED_PARAMETERS {
            dwSize: size_of::<COPYFILE2_EXTENDED_PARAMETERS>() as u32,
            dwCopyFlags: flags,
            pfCancel: cancel.windows_cancellation_flag() as *const _ as *mut windows::core::BOOL,
            pProgressRoutine: Some(copy_progress),
            pvCallbackContext: (&mut context as *mut CopyContext<'_>).cast::<c_void>(),
        };
        let result = unsafe {
            CopyFile2(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(destination_wide.as_ptr()),
                Some(&parameters),
            )
        };
        if context.callback_panicked {
            return Err(CopyFileError {
                kind: CopyFileErrorKind::Failed,
                error: io::Error::other("CopyFile2 progress callback failed"),
            });
        }
        match result {
            Ok(()) => return Ok(context.last_total),
            Err(error) if cancel.is_cancelled() => return Err(cancelled_error()),
            Err(error)
                if context.pause_requested
                    && WIN32_ERROR::from_error(&error) == Some(ERROR_REQUEST_PAUSED) =>
            {
                cancel.wait_if_paused();
                if cancel.is_cancelled() {
                    return Err(cancelled_error());
                }
                resume = true;
            }
            Err(error)
                if network_copy
                    && retry_count < 2
                    && WIN32_ERROR::from_error(&error).is_some_and(is_retryable_network_error) =>
            {
                retry_count += 1;
                let _ = std::fs::remove_file(destination);
                resume = false;
                std::thread::sleep(std::time::Duration::from_millis(
                    250 * u64::from(retry_count),
                ));
            }
            Err(error) => {
                let code = WIN32_ERROR::from_error(&error)
                    .map(|value| format!("Win32 {}", value.0))
                    .unwrap_or_else(|| format!("HRESULT 0x{:08X}", error.code().0 as u32));
                return Err(CopyFileError {
                    kind: CopyFileErrorKind::Failed,
                    error: io::Error::other(format!(
                        "CopyFile2 failed ({code}) from {} to {}: {error}",
                        source.display(),
                        destination.display()
                    )),
                });
            }
        }
    }
}

unsafe extern "system" fn copy_progress(
    message: *const COPYFILE2_MESSAGE,
    callback_context: *const c_void,
) -> COPYFILE2_MESSAGE_ACTION {
    let Some(context) = (unsafe { (callback_context as *mut CopyContext<'_>).as_mut() }) else {
        return COPYFILE2_PROGRESS_CANCEL;
    };
    if context.cancel.is_cancelled() {
        return COPYFILE2_PROGRESS_CANCEL;
    }
    if !message.is_null() {
        let message = unsafe { &*message };
        if message.Type == COPYFILE2_CALLBACK_CHUNK_FINISHED {
            let total = unsafe { message.Info.ChunkFinished }.uliTotalBytesTransferred;
            let advanced = total.saturating_sub(context.last_total);
            context.last_total = context.last_total.max(total);
            if advanced > 0
                && std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (context.progress)(advanced)
                }))
                .is_err()
            {
                context.callback_panicked = true;
                return COPYFILE2_PROGRESS_CANCEL;
            }
        }
    }
    if context.cancel.is_paused() {
        context.cancel.acknowledge_pause();
        context.pause_requested = true;
        COPYFILE2_PROGRESS_PAUSE
    } else {
        COPYFILE2_PROGRESS_CONTINUE
    }
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn is_retryable_network_error(error: WIN32_ERROR) -> bool {
    matches!(
        error,
        ERROR_BAD_NETPATH
            | ERROR_BAD_NET_NAME
            | ERROR_CONNECTION_UNAVAIL
            | ERROR_HOST_UNREACHABLE
            | ERROR_NETNAME_DELETED
            | ERROR_NETWORK_UNREACHABLE
            | ERROR_NO_NETWORK
            | ERROR_SEM_TIMEOUT
            | ERROR_UNEXP_NET_ERR
    )
}

fn cancelled_error() -> CopyFileError {
    CopyFileError {
        kind: CopyFileErrorKind::Cancelled,
        error: io::Error::from_raw_os_error(ERROR_REQUEST_ABORTED.0 as i32),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::mpsc, thread, time::Duration};

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "asterfiles-copyfile2-{}-{}-{}",
                std::process::id(),
                UNIQUE_TEST_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    static UNIQUE_TEST_DIRECTORY: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(1);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn issue_60_copyfile2_preserves_content_modified_time_and_attributes() {
        use windows::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_READONLY, GetFileAttributesW,
            INVALID_FILE_ATTRIBUTES, SetFileAttributesW,
        };

        let temp = TempDir::new();
        let source = temp.0.join("源 文件.bin");
        let destination = temp.0.join("目标 文件.bin");
        fs::write(&source, b"copyfile2 metadata").unwrap();
        let source_wide = wide_path(&source);
        unsafe {
            SetFileAttributesW(
                PCWSTR(source_wide.as_ptr()),
                FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY,
            )
            .unwrap();
        }
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        copy_file(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"copyfile2 metadata");
        assert_eq!(
            fs::metadata(&destination).unwrap().modified().unwrap(),
            modified
        );
        let destination_wide = wide_path(&destination);
        let attributes = unsafe { GetFileAttributesW(PCWSTR(destination_wide.as_ptr())) };
        assert_ne!(attributes, INVALID_FILE_ATTRIBUTES);
        assert!(attributes & FILE_ATTRIBUTE_HIDDEN.0 != 0);
        assert!(attributes & FILE_ATTRIBUTE_READONLY.0 != 0);
        unsafe {
            SetFileAttributesW(PCWSTR(destination_wide.as_ptr()), Default::default()).unwrap()
        };
    }

    #[test]
    fn issue_60_copyfile2_preserves_alternate_data_stream() {
        let temp = TempDir::new();
        let source = temp.0.join("source.bin");
        let destination = temp.0.join("destination.bin");
        fs::write(&source, b"main").unwrap();
        fs::write(format!("{}:asterfiles", source.display()), b"stream").unwrap();
        copy_file(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            fs::read(format!("{}:asterfiles", destination.display())).unwrap(),
            b"stream"
        );
    }

    #[test]
    fn issue_60_copyfile2_cancel_removes_partial_destination() {
        let temp = TempDir::new();
        let source = temp.0.join("source.bin");
        let destination = temp.0.join("destination.bin");
        fs::write(&source, vec![7_u8; 64 * 1024 * 1024]).unwrap();
        let cancel = CancellationToken::new();
        let callback_cancel = cancel.clone();
        let result = copy_file(&source, &destination, &cancel, &mut move |_| {
            callback_cancel.cancel()
        });
        assert_eq!(result.unwrap_err().kind, CopyFileErrorKind::Cancelled);
        assert!(!destination.exists());
    }

    #[test]
    fn issue_60_copyfile2_pause_resumes_same_destination() {
        let temp = TempDir::new();
        let source = temp.0.join("source.bin");
        let destination = temp.0.join("destination.bin");
        fs::write(&source, vec![9_u8; 64 * 1024 * 1024]).unwrap();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker_destination = destination.clone();
        let (progress_sender, progress_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            copy_file(&source, &worker_destination, &worker_cancel, &mut |bytes| {
                progress_sender.send(bytes).unwrap();
            })
        });
        progress_receiver
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        cancel.pause();
        thread::sleep(Duration::from_millis(100));
        assert!(destination.exists());
        cancel.resume();
        worker.join().unwrap().unwrap();
        assert_eq!(fs::metadata(destination).unwrap().len(), 64 * 1024 * 1024);
    }
}
