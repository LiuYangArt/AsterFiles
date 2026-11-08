use std::io;

use windows_sys::Win32::{
    Foundation::{HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Threading::WaitForSingleObject,
    },
};

/// # Safety
/// The handle, event, OVERLAPPED and request buffer must remain valid until this call returns.
/// The request must have been successfully issued with this OVERLAPPED and event.
pub(super) unsafe fn finish_overlapped(
    handle: HANDLE,
    overlapped: &OVERLAPPED,
    timeout: u32,
) -> io::Result<u32> {
    // SAFETY: The caller retains the pending request and its owned event for the entire wait.
    unsafe {
        finish_overlapped_with_wait(handle, overlapped, timeout, |event, timeout| {
            WaitForSingleObject(event, timeout)
        })
    }
}

unsafe fn finish_overlapped_with_wait(
    handle: HANDLE,
    overlapped: &OVERLAPPED,
    timeout: u32,
    wait: impl FnOnce(HANDLE, u32) -> u32,
) -> io::Result<u32> {
    let wait_result = wait(overlapped.hEvent, timeout);
    let wait_error = match wait_result {
        WAIT_OBJECT_0 => None,
        WAIT_TIMEOUT => Some(io::Error::from(io::ErrorKind::TimedOut)),
        _ => Some(io::Error::last_os_error()),
    };
    let mut transferred = 0;
    if wait_error.is_some() {
        // SAFETY: Cancellation alone does not end the kernel's borrow of request storage.
        // The blocking completion query below must run before the caller can release it.
        unsafe { CancelIoEx(handle, overlapped) };
    }
    // SAFETY: Both successful waits and cancelled waits retain valid request storage here.
    // Waiting also covers cancellation racing with normal completion.
    let completed = unsafe { GetOverlappedResult(handle, overlapped, &mut transferred, 1) };
    if let Some(error) = wait_error {
        Err(error)
    } else if completed == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(transferred)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::windows::ffi::OsStrExt,
        path::PathBuf,
        ptr,
        sync::atomic::{AtomicU64, Ordering},
    };
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_INVALID_HANDLE, ERROR_IO_INCOMPLETE, ERROR_OPERATION_ABORTED,
            INVALID_HANDLE_VALUE, WAIT_FAILED,
        },
        Storage::FileSystem::{
            CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
            FILE_NOTIFY_CHANGE_FILE_NAME, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
            OPEN_EXISTING, ReadDirectoryChangesW,
        },
        System::Threading::CreateEventW,
    };

    struct NativeRead {
        root: PathBuf,
        directory: HANDLE,
        event: HANDLE,
        overlapped: Box<OVERLAPPED>,
        _buffer: Vec<u8>,
    }

    impl NativeRead {
        fn start() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "asterfiles-issue-180-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&root).unwrap();
            let wide = root
                .as_os_str()
                .encode_wide()
                .chain(Some(0))
                .collect::<Vec<_>>();
            // SAFETY: The fresh directory and NUL-terminated path remain valid; both handles are owned by the fixture.
            let directory = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    FILE_LIST_DIRECTORY,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
            assert_ne!(directory, INVALID_HANDLE_VALUE);
            // SAFETY: No pointers are borrowed by this unnamed event creation.
            let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
            assert!(!event.is_null());
            let mut read = Self {
                root,
                directory,
                event,
                overlapped: Box::new(OVERLAPPED {
                    hEvent: event,
                    ..Default::default()
                }),
                _buffer: vec![0; 4096],
            };
            // SAFETY: Box and Vec allocations do not move when the fixture moves; Drop drains the request even on test panic.
            let started = unsafe {
                ReadDirectoryChangesW(
                    directory,
                    read._buffer.as_mut_ptr().cast(),
                    read._buffer.len() as u32,
                    0,
                    FILE_NOTIFY_CHANGE_FILE_NAME,
                    ptr::null_mut(),
                    read.overlapped.as_mut(),
                    None,
                )
            };
            assert_ne!(started, 0, "{}", io::Error::last_os_error());
            read
        }

        fn completion(&self) -> io::Result<u32> {
            let mut transferred = 0;
            // SAFETY: The fixture owns live storage and only queries this request without waiting.
            if unsafe {
                GetOverlappedResult(
                    self.directory,
                    self.overlapped.as_ref(),
                    &mut transferred,
                    0,
                )
            } == 0
            {
                Err(io::Error::last_os_error())
            } else {
                Ok(transferred)
            }
        }

        fn assert_drained(&self) {
            assert_ne!(
                self.completion()
                    .err()
                    .and_then(|error| error.raw_os_error()),
                Some(ERROR_IO_INCOMPLETE as i32)
            );
            // SAFETY: The fixture retains its owned completion event until Drop.
            assert_eq!(unsafe { WaitForSingleObject(self.event, 0) }, WAIT_OBJECT_0);
        }
    }

    impl Drop for NativeRead {
        fn drop(&mut self) {
            let mut transferred = 0;
            // SAFETY: Drain before releasing event, directory, OVERLAPPED or buffer, including assertion failures.
            unsafe {
                CancelIoEx(self.directory, self.overlapped.as_ref());
                GetOverlappedResult(
                    self.directory,
                    self.overlapped.as_ref(),
                    &mut transferred,
                    1,
                );
                CloseHandle(self.event);
                CloseHandle(self.directory);
            }
            std::fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn issue_180_wait_failure_alone_leaves_native_request_pending() {
        let read = NativeRead::start();
        // SAFETY: An invalid wait target injects WAIT_FAILED without invalidating the actual request event.
        let wait = unsafe { WaitForSingleObject(ptr::null_mut(), 0) };
        assert_eq!(wait, WAIT_FAILED);
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_HANDLE as i32)
        );
        assert_eq!(
            read.completion().unwrap_err().raw_os_error(),
            Some(ERROR_IO_INCOMPLETE as i32)
        );
        eprintln!(
            "#180 pre-fix evidence: WAIT_FAILED leaves ReadDirectoryChangesW pending; fixture drains before freeing storage"
        );
    }

    #[test]
    fn issue_180_normal_completion_drains_before_storage_release() {
        let read = NativeRead::start();
        std::fs::write(read.root.join("created.txt"), b"test").unwrap();
        // SAFETY: The fixture retains all pending request storage for the completion call.
        let completed =
            unsafe { finish_overlapped(read.directory, read.overlapped.as_ref(), 5000) };
        assert!(completed.unwrap() > 0);
        read.assert_drained();
    }

    #[test]
    fn issue_180_timeout_drains_before_storage_release() {
        let read = NativeRead::start();
        // SAFETY: The fixture retains all pending request storage for the completion call.
        let result = unsafe { finish_overlapped(read.directory, read.overlapped.as_ref(), 0) };
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        read.assert_drained();
        assert_eq!(
            read.completion().unwrap_err().raw_os_error(),
            Some(ERROR_OPERATION_ABORTED as i32)
        );
    }

    #[test]
    fn issue_180_external_cancellation_drains_before_storage_release() {
        let read = NativeRead::start();
        // SAFETY: This is the fixture's live pending request, retained for the subsequent completion call.
        unsafe { CancelIoEx(read.directory, read.overlapped.as_ref()) };
        // SAFETY: The fixture retains all pending request storage for the completion call.
        let result = unsafe { finish_overlapped(read.directory, read.overlapped.as_ref(), 5000) };
        assert_eq!(
            result.unwrap_err().raw_os_error(),
            Some(ERROR_OPERATION_ABORTED as i32)
        );
        read.assert_drained();
    }

    #[test]
    fn issue_180_wait_failure_drains_and_preserves_original_error() {
        let read = NativeRead::start();
        // SAFETY: Only the wait target is injected; the real request event and storage remain valid.
        let result = unsafe {
            finish_overlapped_with_wait(read.directory, read.overlapped.as_ref(), 5000, |_, _| {
                WaitForSingleObject(ptr::null_mut(), 0)
            })
        };
        assert_eq!(
            result.unwrap_err().raw_os_error(),
            Some(ERROR_INVALID_HANDLE as i32)
        );
        read.assert_drained();
        assert_eq!(
            read.completion().unwrap_err().raw_os_error(),
            Some(ERROR_OPERATION_ABORTED as i32)
        );
        eprintln!(
            "#180 fixed evidence: WAIT_FAILED preserved, request completed with ERROR_OPERATION_ABORTED before storage release"
        );
    }
}
