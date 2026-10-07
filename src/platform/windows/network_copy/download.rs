use super::{
    STALL_LIMIT, acquire_read_slot, cancelled, copy_query, failed, query_retryable, retry_backoff,
    retry_delay,
};
use crate::platform::windows::{copy_execution::CopyExecution, copy_file::CopyFileError};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        fs::OpenOptionsExt,
        io::AsRawHandle,
    },
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread,
    time::{Duration, Instant},
};
use windows_sys::Win32::{
    Foundation::{ERROR_HANDLE_EOF, FILETIME, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM, FILE_ATTRIBUTE_TEMPORARY,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FindClose, FindFirstStreamW, FindNextStreamW,
        FindStreamInfoStandard, GetFileInformationByHandle, SetFileAttributesW, SetFileTime,
        WIN32_FIND_STREAM_DATA,
    },
};

const BLOCK_SIZE: usize = 1024 * 1024;
const POLL: Duration = Duration::from_millis(20);

#[derive(Clone, Debug, PartialEq, Eq)]
struct StreamVersion {
    name: OsString,
    size: u64,
}

#[derive(Clone, Debug)]
struct SourceVersion {
    identity: (u32, u64),
    size: u64,
    modified: u64,
    created: u64,
    accessed: u64,
    attributes: u32,
    streams: Vec<StreamVersion>,
}

impl SourceVersion {
    fn same_content(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.size == other.size
            && self.modified == other.modified
            && self.created == other.created
            && self.streams == other.streams
    }
}

fn timestamp(value: FILETIME) -> u64 {
    (u64::from(value.dwHighDateTime) << 32) | u64::from(value.dwLowDateTime)
}

fn filetime(value: u64) -> FILETIME {
    FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn stream_path(path: &Path, name: &OsStr) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(name);
    PathBuf::from(value)
}

fn changed() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "network copy source changed; partial contents were not published",
    )
}

fn handle_version(file: &File) -> io::Result<SourceVersion> {
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "copy source is not a file",
        ));
    }
    Ok(SourceVersion {
        identity: (
            info.dwVolumeSerialNumber,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ),
        size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        modified: timestamp(info.ftLastWriteTime),
        created: timestamp(info.ftCreationTime),
        accessed: timestamp(info.ftLastAccessTime),
        attributes: info.dwFileAttributes,
        streams: Vec::new(),
    })
}

fn streams(path: &Path, size: u64) -> io::Result<Vec<StreamVersion>> {
    struct Search(windows_sys::Win32::Foundation::HANDLE);
    impl Drop for Search {
        fn drop(&mut self) {
            unsafe {
                FindClose(self.0);
            }
        }
    }
    let mut result = vec![StreamVersion {
        name: OsString::new(),
        size,
    }];
    let mut data: WIN32_FIND_STREAM_DATA = unsafe { std::mem::zeroed() };
    let handle = unsafe {
        FindFirstStreamW(
            wide(path).as_ptr(),
            FindStreamInfoStandard,
            std::ptr::addr_of_mut!(data).cast(),
            0,
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(ERROR_HANDLE_EOF as i32) {
            Ok(result)
        } else {
            Err(error)
        };
    }
    let search = Search(handle);
    loop {
        let count = data
            .cStreamName
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(data.cStreamName.len());
        let name = OsString::from_wide(&data.cStreamName[..count]);
        if name != OsStr::new("::$DATA") {
            let units = &data.cStreamName[..count];
            if data.StreamSize < 0
                || units.first() != Some(&58)
                || !units.ends_with(&[58, 36, 68, 65, 84, 65])
                || units.iter().any(|unit| matches!(*unit, 47 | 92))
                || result.iter().any(|stream| stream.name == name)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid source data stream",
                ));
            }
            result.push(StreamVersion {
                name,
                size: data.StreamSize as u64,
            });
        }
        if unsafe { FindNextStreamW(search.0, std::ptr::addr_of_mut!(data).cast()) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_HANDLE_EOF as i32) {
                return Err(error);
            }
            break;
        }
    }
    result[1..].sort_by(|left, right| left.name.cmp(&right.name));
    Ok(result)
}

trait SourceReader: Read + Send {
    fn validate(&self) -> io::Result<()>;
}

trait Source: Send + Sync + 'static {
    fn version(&self, path: &Path) -> io::Result<SourceVersion>;
    fn open(
        &self,
        path: &Path,
        stream: &StreamVersion,
        expected: &SourceVersion,
        offset: u64,
    ) -> io::Result<Box<dyn SourceReader>>;
}

struct NativeReader {
    file: File,
    expected: SourceVersion,
    stream_size: u64,
}
impl Read for NativeReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.file.read(bytes)
    }
}
impl SourceReader for NativeReader {
    fn validate(&self) -> io::Result<()> {
        let opened = handle_version(&self.file)?;
        if opened.identity != self.expected.identity
            || opened.size != self.stream_size
            || opened.modified != self.expected.modified
            || opened.created != self.expected.created
        {
            return Err(changed());
        }
        Ok(())
    }
}

struct NativeSource;
impl Source for NativeSource {
    fn version(&self, path: &Path) -> io::Result<SourceVersion> {
        let file = OpenOptions::new()
            .access_mode(FILE_READ_ATTRIBUTES)
            .share_mode(FILE_SHARE_READ)
            .open(path)?;
        let mut version = handle_version(&file)?;
        version.streams = streams(path, version.size)?;
        Ok(version)
    }
    fn open(
        &self,
        path: &Path,
        stream: &StreamVersion,
        expected: &SourceVersion,
        offset: u64,
    ) -> io::Result<Box<dyn SourceReader>> {
        if !self.version(path)?.same_content(expected) {
            return Err(changed());
        }
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(stream_path(path, &stream.name))?;
        let opened = handle_version(&file)?;
        if opened.identity != expected.identity
            || opened.size != stream.size
            || opened.modified != expected.modified
            || opened.created != expected.created
        {
            return Err(changed());
        }
        file.seek(SeekFrom::Start(offset))?;
        Ok(Box::new(NativeReader {
            file,
            expected: expected.clone(),
            stream_size: stream.size,
        }))
    }
}

enum ReadEvent {
    Block { offset: u64, bytes: Vec<u8> },
    Finished,
}

struct Reading {
    receiver: Receiver<io::Result<ReadEvent>>,
    abandoned: Arc<AtomicBool>,
    worker: thread::JoinHandle<()>,
}
impl Drop for Reading {
    fn drop(&mut self) {
        self.abandoned.store(true, Ordering::Release);
        super::cancel_read_worker(&self.worker);
    }
}

fn start_reading(
    backend: Arc<dyn Source>,
    source: PathBuf,
    stream: StreamVersion,
    expected: SourceVersion,
    offset: u64,
    execution: &CopyExecution,
) -> io::Result<Reading> {
    let slot = acquire_read_slot(execution)?;
    crate::operation_audit::record(
        "network-copy-block-start",
        format!(
            "source={} stream={:?} offset={offset}",
            source.display(),
            stream.name
        ),
    );
    let (sender, receiver) = mpsc::sync_channel(1);
    let abandoned = Arc::new(AtomicBool::new(false));
    let worker_abandoned = abandoned.clone();
    let worker = thread::Builder::new()
        .name("network-copy-read".to_owned())
        .spawn(move || {
            // A stale SMB read can retain this slot, but it never receives a destination handle.
            let _slot = slot;
            let transfer = || -> io::Result<()> {
                if worker_abandoned.load(Ordering::Acquire) {
                    return Ok(());
                }
                let mut reader = backend.open(&source, &stream, &expected, offset)?;
                let mut position = offset;
                while position < stream.size && !worker_abandoned.load(Ordering::Acquire) {
                    let mut bytes =
                        vec![0; (stream.size - position).min(BLOCK_SIZE as u64) as usize];
                    let count = reader.read(&mut bytes)?;
                    if count == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "source ended before its declared size",
                        ));
                    }
                    bytes.truncate(count);
                    if worker_abandoned.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if sender
                        .send(Ok(ReadEvent::Block {
                            offset: position,
                            bytes,
                        }))
                        .is_err()
                    {
                        return Ok(());
                    }
                    position += count as u64;
                }
                if !worker_abandoned.load(Ordering::Acquire) {
                    reader.validate()?;
                    // The original read handle still denies writes/deletion during this final path check.
                    if !backend.version(&source)?.same_content(&expected) {
                        return Err(changed());
                    }
                    let _ = sender.send(Ok(ReadEvent::Finished));
                }
                Ok(())
            };
            if let Err(error) = transfer() {
                let _ = sender.send(Err(error));
            }
        })?;
    Ok(Reading {
        receiver,
        abandoned,
        worker,
    })
}

struct LocalTarget {
    path: PathBuf,
    keep: bool,
}
impl Drop for LocalTarget {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn preserve_metadata(file: &File, path: &Path, version: &SourceVersion) -> io::Result<()> {
    let created = filetime(version.created);
    let accessed = filetime(version.accessed);
    let modified = filetime(version.modified);
    if unsafe { SetFileTime(file.as_raw_handle(), &created, &accessed, &modified) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = version.attributes
        & (FILE_ATTRIBUTE_READONLY
            | FILE_ATTRIBUTE_HIDDEN
            | FILE_ATTRIBUTE_SYSTEM
            | FILE_ATTRIBUTE_ARCHIVE
            | FILE_ATTRIBUTE_TEMPORARY
            | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED);
    if unsafe {
        SetFileAttributesW(
            wide(path).as_ptr(),
            if attributes == 0 {
                FILE_ATTRIBUTE_NORMAL
            } else {
                attributes
            },
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn copy_file(
    source: &Path,
    destination: &Path,
    execution: &CopyExecution,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, CopyFileError> {
    copy_with(
        Arc::new(NativeSource),
        source,
        destination,
        execution,
        progress,
        STALL_LIMIT,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn copy_with(
    backend: Arc<dyn Source>,
    source: &Path,
    destination: &Path,
    execution: &CopyExecution,
    progress: &mut dyn FnMut(u64),
    timeout: Duration,
    retry_wait: Option<Duration>,
) -> Result<u64, CopyFileError> {
    execution.set_recovering(false);
    let outcome = copy_inner(
        backend,
        source,
        destination,
        execution,
        progress,
        timeout,
        retry_wait,
    );
    execution.set_recovering(false);
    outcome
}

fn source_version(
    backend: Arc<dyn Source>,
    source: &Path,
    execution: &CopyExecution,
) -> Result<SourceVersion, CopyFileError> {
    let source = source.to_owned();
    copy_query(execution, move || backend.version(&source)).map_err(|error| {
        if execution.cancel().is_cancelled() {
            cancelled()
        } else {
            failed(error)
        }
    })
}

#[allow(clippy::too_many_arguments)]
fn copy_inner(
    backend: Arc<dyn Source>,
    source: &Path,
    destination: &Path,
    execution: &CopyExecution,
    progress: &mut dyn FnMut(u64),
    timeout: Duration,
    retry_wait: Option<Duration>,
) -> Result<u64, CopyFileError> {
    let expected = source_version(backend.clone(), source, execution)?;
    execution.cancel().wait_if_paused();
    if execution.cancel().is_cancelled() {
        return Err(cancelled());
    }
    let primary = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(failed)?;
    let mut target = LocalTarget {
        path: destination.to_owned(),
        keep: false,
    };
    let result = (|| {
        for stream in &expected.streams {
            let mut file = if stream.name.is_empty() {
                primary.try_clone().map_err(failed)?
            } else {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(stream_path(destination, &stream.name))
                    .map_err(failed)?
            };
            copy_stream(
                backend.clone(),
                source,
                stream,
                &expected,
                &mut file,
                execution,
                &mut |count| {
                    if stream.name.is_empty() {
                        progress(count);
                    }
                },
                timeout,
                retry_wait,
            )?;
            file.sync_all().map_err(failed)?;
        }
        if !source_version(backend, source, execution)?.same_content(&expected) {
            return Err(failed(changed()));
        }
        execution.cancel().wait_if_paused();
        if execution.cancel().is_cancelled() {
            return Err(cancelled());
        }
        preserve_metadata(&primary, destination, &expected).map_err(failed)?;
        Ok(expected.size)
    })();
    drop(primary);
    if result.is_ok() {
        target.keep = true;
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn copy_stream(
    backend: Arc<dyn Source>,
    source: &Path,
    stream: &StreamVersion,
    expected: &SourceVersion,
    destination: &mut File,
    execution: &CopyExecution,
    progress: &mut dyn FnMut(u64),
    timeout: Duration,
    retry_wait: Option<Duration>,
) -> Result<(), CopyFileError> {
    let mut confirmed = 0_u64;
    let mut attempt = 0_u32;
    loop {
        execution.cancel().wait_if_paused();
        if execution.cancel().is_cancelled() {
            return Err(cancelled());
        }
        let reading = start_reading(
            backend.clone(),
            source.to_owned(),
            stream.clone(),
            expected.clone(),
            confirmed,
            execution,
        )
        .map_err(|error| {
            if execution.cancel().is_cancelled() {
                cancelled()
            } else {
                failed(error)
            }
        })?;
        let mut changed_at = Instant::now();
        let result = loop {
            if execution.cancel().is_cancelled() {
                return Err(cancelled());
            }
            if execution.cancel().is_paused() {
                break None;
            }
            match reading.receiver.recv_timeout(POLL) {
                Ok(Ok(ReadEvent::Block { offset, bytes })) => {
                    if execution.cancel().is_paused() {
                        break None;
                    }
                    if execution.cancel().is_cancelled() {
                        return Err(cancelled());
                    }
                    if offset != confirmed
                        || bytes.is_empty()
                        || bytes.len() as u64 > stream.size - confirmed
                    {
                        return Err(failed(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid network copy block",
                        )));
                    }
                    destination.write_all(&bytes).map_err(failed)?;
                    confirmed += bytes.len() as u64;
                    progress(bytes.len() as u64);
                    attempt = 0;
                    changed_at = Instant::now();
                    execution.set_recovering(false);
                }
                Ok(Ok(ReadEvent::Finished)) if confirmed == stream.size => return Ok(()),
                Ok(Ok(ReadEvent::Finished)) => return Err(failed(changed())),
                Ok(Err(error)) => break Some(error),
                Err(RecvTimeoutError::Disconnected) => {
                    break Some(io::Error::other("network copy reader stopped"));
                }
                Err(RecvTimeoutError::Timeout) if changed_at.elapsed() >= timeout => {
                    break Some(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "network copy read did not respond",
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
        };
        // Dropping this receiver discards all old blocks before a paused task is acknowledged.
        drop(reading);
        execution.set_recovering(true);
        if let Some(error) = result {
            if !query_retryable(&error) {
                return Err(failed(error));
            }
            attempt = attempt.saturating_add(1);
            crate::operation_audit::record(
                "network-copy-block-retry",
                format!("offset={confirmed} attempt={attempt} error={error}"),
            );
            retry_delay(
                execution.cancel(),
                retry_wait.unwrap_or_else(|| retry_backoff(attempt)),
            )?;
        }
    }
}

#[cfg(test)]
mod tests;
