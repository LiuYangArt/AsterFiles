use std::{
    collections::HashSet,
    ffi::OsString,
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_NOTIFY_ENUM_DIR, ERROR_OPERATION_ABORTED, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ACTION_ADDED, FILE_ACTION_MODIFIED, FILE_ACTION_REMOVED,
        FILE_ACTION_RENAMED_NEW_NAME, FILE_ACTION_RENAMED_OLD_NAME, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY, FILE_NOTIFY_CHANGE_ATTRIBUTES,
        FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME, FILE_NOTIFY_CHANGE_FILE_NAME,
        FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SECURITY, FILE_NOTIFY_CHANGE_SIZE,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadDirectoryChangesW,
    },
    System::{
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Threading::{CreateEventW, INFINITE, ResetEvent, WaitForSingleObject},
    },
};

const BUFFER_SIZE: usize = 64 * 1024;
const NOTIFY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
    | FILE_NOTIFY_CHANGE_DIR_NAME
    | FILE_NOTIFY_CHANGE_ATTRIBUTES
    | FILE_NOTIFY_CHANGE_SIZE
    | FILE_NOTIFY_CHANGE_LAST_WRITE
    | FILE_NOTIFY_CHANGE_CREATION
    | FILE_NOTIFY_CHANGE_SECURITY;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DirectoryChange {
    Added(PathBuf),
    Removed(PathBuf),
    Modified(PathBuf),
    Renamed { from: PathBuf, to: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirectoryWatchEvent {
    Changes {
        root: PathBuf,
        changes: Vec<DirectoryChange>,
    },
    Overflow {
        root: PathBuf,
    },
    Error {
        root: PathBuf,
        message: String,
    },
}

pub struct DirectoryWatch {
    shared: Arc<WatchShared>,
    worker: Option<JoinHandle<()>>,
}

struct WatchShared {
    directory: Mutex<isize>,
    cancelled: AtomicBool,
}

impl DirectoryWatch {
    pub fn start(
        root: impl AsRef<Path>,
        recursive: bool,
        events: mpsc::Sender<DirectoryWatchEvent>,
    ) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        let directory = open_directory(&root)?;
        let shared = Arc::new(WatchShared {
            directory: Mutex::new(directory as isize),
            cancelled: AtomicBool::new(false),
        });
        let worker_shared = shared.clone();
        let worker_root = root.clone();
        let worker = thread::Builder::new()
            .name("asterfiles-directory-watch".into())
            .spawn(move || watch_directory(worker_root, recursive, events, worker_shared));

        match worker {
            Ok(worker) => Ok(Self {
                shared,
                worker: Some(worker),
            }),
            Err(error) => {
                close_directory(&shared);
                Err(error)
            }
        }
    }

    pub fn cancel(&mut self) {
        self.shared.cancelled.store(true, Ordering::Release);
        let directory = *self
            .shared
            .directory
            .lock()
            .expect("directory watch handle poisoned");
        if directory != 0 {
            unsafe {
                CancelIoEx(directory as HANDLE, ptr::null());
            }
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for DirectoryWatch {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn open_directory(path: &Path) -> io::Result<HANDLE> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory watch path is empty",
        ));
    }
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "directory watch path contains a null character",
        ));
    }
    wide.push(0);

    let handle = unsafe {
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
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn watch_directory(
    root: PathBuf,
    recursive: bool,
    events: mpsc::Sender<DirectoryWatchEvent>,
    shared: Arc<WatchShared>,
) {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        send_error(&events, &root, io::Error::last_os_error());
        close_directory(&shared);
        return;
    }

    let directory = *shared
        .directory
        .lock()
        .expect("directory watch handle poisoned") as HANDLE;
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut coalescer = ChangeCoalescer::default();

    while !shared.cancelled.load(Ordering::Acquire) {
        unsafe {
            ResetEvent(event);
        }
        let mut overlapped = OVERLAPPED {
            hEvent: event,
            ..Default::default()
        };
        let started = unsafe {
            ReadDirectoryChangesW(
                directory,
                buffer.as_mut_ptr().cast(),
                buffer.len() as u32,
                recursive.into(),
                NOTIFY_FILTER,
                ptr::null_mut(),
                &mut overlapped,
                None,
            )
        };
        if started == 0 {
            let error = io::Error::last_os_error();
            if !is_cancelled(&shared, &error) {
                send_error(&events, &root, error);
            }
            break;
        }

        if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
            send_error(&events, &root, io::Error::last_os_error());
            break;
        }

        let mut bytes = 0u32;
        let completed = unsafe { GetOverlappedResult(directory, &overlapped, &mut bytes, 0) };
        if completed == 0 {
            let error = io::Error::last_os_error();
            if is_cancelled(&shared, &error) {
                break;
            }
            if error.raw_os_error() == Some(ERROR_NOTIFY_ENUM_DIR as i32) {
                coalescer.clear();
                if events
                    .send(DirectoryWatchEvent::Overflow { root: root.clone() })
                    .is_err()
                {
                    break;
                }
                continue;
            }
            send_error(&events, &root, error);
            break;
        }

        if bytes == 0 {
            coalescer.clear();
            if events
                .send(DirectoryWatchEvent::Overflow { root: root.clone() })
                .is_err()
            {
                break;
            }
            continue;
        }

        match parse_notifications(&buffer[..bytes as usize]) {
            Ok(notifications) => {
                for notification in notifications {
                    coalescer.push(notification);
                }
                coalescer.flush_pending_rename();
                let changes = absolute_changes(&root, coalescer.take_changes());
                if !changes.is_empty()
                    && events
                        .send(DirectoryWatchEvent::Changes {
                            root: root.clone(),
                            changes,
                        })
                        .is_err()
                {
                    break;
                }
            }
            Err(error) => {
                coalescer.clear();
                if events
                    .send(DirectoryWatchEvent::Error {
                        root: root.clone(),
                        message: error.to_string(),
                    })
                    .is_err()
                {
                    break;
                }
            }
        }
    }

    let trailing = absolute_changes(&root, coalescer.finish());
    if !trailing.is_empty() && !shared.cancelled.load(Ordering::Acquire) {
        let _ = events.send(DirectoryWatchEvent::Changes {
            root: root.clone(),
            changes: trailing,
        });
    }
    unsafe {
        CloseHandle(event);
    }
    close_directory(&shared);
}

fn close_directory(shared: &WatchShared) {
    let handle = {
        let mut directory = shared
            .directory
            .lock()
            .expect("directory watch handle poisoned");
        std::mem::replace(&mut *directory, 0)
    };
    if handle != 0 {
        unsafe {
            CloseHandle(handle as HANDLE);
        }
    }
}

fn is_cancelled(shared: &WatchShared, error: &io::Error) -> bool {
    shared.cancelled.load(Ordering::Acquire)
        || error.raw_os_error() == Some(ERROR_OPERATION_ABORTED as i32)
}

fn send_error(events: &mpsc::Sender<DirectoryWatchEvent>, root: &Path, error: io::Error) {
    let _ = events.send(DirectoryWatchEvent::Error {
        root: root.to_path_buf(),
        message: error.to_string(),
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawNotification {
    action: u32,
    relative_path: PathBuf,
}

fn parse_notifications(buffer: &[u8]) -> io::Result<Vec<RawNotification>> {
    let mut notifications = Vec::new();
    let mut offset = 0usize;
    loop {
        if buffer.len().saturating_sub(offset) < 12 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory change record header is truncated",
            ));
        }
        let next_offset = read_u32(buffer, offset)? as usize;
        let action = read_u32(buffer, offset + 4)?;
        let name_bytes = read_u32(buffer, offset + 8)? as usize;
        if !name_bytes.is_multiple_of(2) || buffer.len().saturating_sub(offset + 12) < name_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory change record name is truncated",
            ));
        }
        let name_start = offset + 12;
        let name = buffer[name_start..name_start + name_bytes]
            .chunks_exact(2)
            .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
            .collect::<Vec<_>>();
        notifications.push(RawNotification {
            action,
            relative_path: PathBuf::from(OsString::from_wide(&name)),
        });

        if next_offset == 0 {
            break;
        }
        if next_offset < 12 || offset.saturating_add(next_offset) >= buffer.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "directory change record offset is invalid",
            ));
        }
        offset += next_offset;
    }
    Ok(notifications)
}

fn read_u32(buffer: &[u8], offset: usize) -> io::Result<u32> {
    let bytes = buffer
        .get(offset..offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "record is truncated"))?;
    Ok(u32::from_le_bytes(
        bytes.try_into().expect("four byte slice"),
    ))
}

#[derive(Default)]
struct ChangeCoalescer {
    pending_rename: Option<PathBuf>,
    changes: Vec<DirectoryChange>,
    seen: HashSet<DirectoryChange>,
}

impl ChangeCoalescer {
    fn push(&mut self, notification: RawNotification) {
        if notification.action != FILE_ACTION_RENAMED_NEW_NAME {
            self.flush_unpaired_rename_if_needed(notification.action);
        }
        match notification.action {
            FILE_ACTION_ADDED => {
                self.push_change(DirectoryChange::Added(notification.relative_path))
            }
            FILE_ACTION_REMOVED => {
                self.push_change(DirectoryChange::Removed(notification.relative_path))
            }
            FILE_ACTION_MODIFIED => {
                self.push_change(DirectoryChange::Modified(notification.relative_path))
            }
            FILE_ACTION_RENAMED_OLD_NAME => {
                self.pending_rename = Some(notification.relative_path);
            }
            FILE_ACTION_RENAMED_NEW_NAME => {
                if let Some(from) = self.pending_rename.take() {
                    self.push_change(DirectoryChange::Renamed {
                        from,
                        to: notification.relative_path,
                    });
                } else {
                    self.push_change(DirectoryChange::Added(notification.relative_path));
                }
            }
            _ => {}
        }
    }

    fn flush_pending_rename(&mut self) {
        if let Some(path) = self.pending_rename.take() {
            self.push_change(DirectoryChange::Removed(path));
        }
    }
    fn take_changes(&mut self) -> Vec<DirectoryChange> {
        self.seen.clear();
        std::mem::take(&mut self.changes)
    }

    fn finish(mut self) -> Vec<DirectoryChange> {
        if let Some(path) = self.pending_rename.take() {
            self.push_change(DirectoryChange::Removed(path));
        }
        self.changes
    }

    fn clear(&mut self) {
        self.pending_rename = None;
        self.changes.clear();
        self.seen.clear();
    }

    fn flush_unpaired_rename_if_needed(&mut self, next_action: u32) {
        if next_action == FILE_ACTION_RENAMED_OLD_NAME {
            if let Some(path) = self.pending_rename.take() {
                self.push_change(DirectoryChange::Removed(path));
            }
        } else if self.pending_rename.is_some() {
            let path = self.pending_rename.take().expect("pending rename exists");
            self.push_change(DirectoryChange::Removed(path));
        }
    }

    fn push_change(&mut self, change: DirectoryChange) {
        if self.seen.insert(change.clone()) {
            self.changes.push(change);
        }
    }
}

fn absolute_changes(root: &Path, changes: Vec<DirectoryChange>) -> Vec<DirectoryChange> {
    changes
        .into_iter()
        .map(|change| match change {
            DirectoryChange::Added(path) => DirectoryChange::Added(root.join(path)),
            DirectoryChange::Removed(path) => DirectoryChange::Removed(root.join(path)),
            DirectoryChange::Modified(path) => DirectoryChange::Modified(root.join(path)),
            DirectoryChange::Renamed { from, to } => DirectoryChange::Renamed {
                from: root.join(from),
                to: root.join(to),
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairs_rename_records_in_one_native_delivery() {
        let mut coalescer = ChangeCoalescer::default();
        coalescer.push(raw(FILE_ACTION_RENAMED_OLD_NAME, "old.txt"));
        coalescer.push(raw(FILE_ACTION_RENAMED_NEW_NAME, "new.txt"));
        assert_eq!(
            coalescer.take_changes(),
            vec![DirectoryChange::Renamed {
                from: PathBuf::from("old.txt"),
                to: PathBuf::from("new.txt"),
            }]
        );
    }

    #[test]
    fn flushes_unpaired_rename_at_delivery_boundary() {
        let mut coalescer = ChangeCoalescer::default();
        coalescer.push(raw(FILE_ACTION_RENAMED_OLD_NAME, "gone.txt"));
        coalescer.flush_pending_rename();
        assert_eq!(
            coalescer.take_changes(),
            vec![DirectoryChange::Removed(PathBuf::from("gone.txt"))]
        );
    }
    #[test]
    fn reports_unpaired_rename_as_remove() {
        let mut coalescer = ChangeCoalescer::default();
        coalescer.push(raw(FILE_ACTION_RENAMED_OLD_NAME, "gone.txt"));
        coalescer.push(raw(FILE_ACTION_ADDED, "other.txt"));
        assert_eq!(
            coalescer.take_changes(),
            vec![
                DirectoryChange::Removed(PathBuf::from("gone.txt")),
                DirectoryChange::Added(PathBuf::from("other.txt")),
            ]
        );
    }

    #[test]
    fn deduplicates_identical_changes_within_delivery() {
        let mut coalescer = ChangeCoalescer::default();
        coalescer.push(raw(FILE_ACTION_MODIFIED, "item.txt"));
        coalescer.push(raw(FILE_ACTION_MODIFIED, "item.txt"));
        assert_eq!(
            coalescer.take_changes(),
            vec![DirectoryChange::Modified(PathBuf::from("item.txt"))]
        );
    }

    #[test]
    fn converts_relative_notifications_to_original_root_paths() {
        let root = PathBuf::from(r"C:\data");
        let changes = absolute_changes(
            &root,
            vec![DirectoryChange::Renamed {
                from: PathBuf::from("before"),
                to: PathBuf::from("nested\\after"),
            }],
        );
        assert_eq!(
            changes,
            vec![DirectoryChange::Renamed {
                from: root.join("before"),
                to: root.join("nested\\after"),
            }]
        );
    }

    #[test]
    fn parses_multiple_native_records() {
        let first = record(FILE_ACTION_ADDED, "one.txt", true);
        let second = record(FILE_ACTION_REMOVED, "two.txt", false);
        let mut buffer = first;
        buffer.extend(second);
        let parsed = parse_notifications(&buffer).expect("records should parse");
        assert_eq!(
            parsed,
            vec![
                raw(FILE_ACTION_ADDED, "one.txt"),
                raw(FILE_ACTION_REMOVED, "two.txt"),
            ]
        );
    }

    fn raw(action: u32, path: &str) -> RawNotification {
        RawNotification {
            action,
            relative_path: PathBuf::from(path),
        }
    }

    fn record(action: u32, name: &str, has_next: bool) -> Vec<u8> {
        let wide = name.encode_utf16().collect::<Vec<_>>();
        let raw_len = 12 + wide.len() * 2;
        let aligned_len = (raw_len + 3) & !3;
        let mut record = vec![0u8; aligned_len];
        let next = if has_next { aligned_len as u32 } else { 0 };
        record[0..4].copy_from_slice(&next.to_le_bytes());
        record[4..8].copy_from_slice(&action.to_le_bytes());
        record[8..12].copy_from_slice(&((wide.len() * 2) as u32).to_le_bytes());
        for (index, unit) in wide.iter().enumerate() {
            let offset = 12 + index * 2;
            record[offset..offset + 2].copy_from_slice(&unit.to_le_bytes());
        }
        record
    }
}
