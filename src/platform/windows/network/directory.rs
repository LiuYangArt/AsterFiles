use super::*;
use std::{io::Read, time::Instant};

const MAX_BATCH_ITEMS: usize = 256;
const MAX_BATCH_BYTES: usize = 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const BATCH_INTERVAL: Duration = Duration::from_millis(100);

pub fn isolated_directory(
    path: &Path,
    visibility: FileVisibility,
    cancel: &AtomicBool,
    on_batch: impl FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize> {
    #[cfg(not(test))]
    use std::os::windows::process::CommandExt;

    let files = StreamFiles::create()?;
    write_directory_input(&files.input(), path, visibility)?;
    #[cfg(test)]
    let command = tests::child_command(&files, "enumerate");
    #[cfg(not(test))]
    let command = {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg(CHILD_PREFIX)
            .arg("directory")
            .arg(files.input())
            .arg(files.output())
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    };
    receive_stream(command, &files.output(), cancel, ISOLATED_TIMEOUT, on_batch)
}

struct StreamFiles(PathBuf);
impl StreamFiles {
    fn create() -> io::Result<Self> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let stamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "asterfiles-directory-{}-{stamp}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }
    fn input(&self) -> PathBuf {
        self.0.join("input")
    }
    fn output(&self) -> PathBuf {
        self.0.join("batch")
    }
}
impl Drop for StreamFiles {
    fn drop(&mut self) {
        // Only these files are owned by the protocol; never recursively remove an input directory.
        for path in [
            self.input(),
            self.output(),
            self.output().with_extension("pending"),
        ] {
            let _ = std::fs::remove_file(path);
        }
        let diagnostic = self.output().with_extension("stderr");
        if std::fs::metadata(&diagnostic).is_ok_and(|metadata| metadata.len() == 0) {
            let _ = std::fs::remove_file(diagnostic);
        }
        let _ = std::fs::remove_dir(&self.0);
    }
}

fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "network directory cancelled")
}

fn receive_stream(
    mut command: Command,
    output: &Path,
    cancel: &AtomicBool,
    timeout: Duration,
    mut on_batch: impl FnMut(Vec<FileEntry>) -> io::Result<()>,
) -> io::Result<usize> {
    if cancel.load(Ordering::Acquire) {
        return Err(cancelled());
    }
    let diagnostic = output.with_extension("stderr");
    command.stderr(Stdio::from(std::fs::File::create(&diagnostic)?));
    #[cfg(test)]
    command.env("RUST_BACKTRACE", "1");
    let job = KillOnCloseJob::create()?;
    let mut child = command.spawn()?;
    let result = (|| {
        job.assign(&child)?;
        let mut last_progress = Instant::now();
        let mut skipped = 0_usize;
        let mut next_id = 1_u32;
        let mut completed = false;
        loop {
            if cancel.load(Ordering::Acquire) {
                return Err(cancelled());
            }
            match std::fs::File::open(output) {
                Ok(file) => {
                    if completed {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "data after directory completion",
                        ));
                    }
                    let (entries, batch_skipped, done) = read_batch(file)?;
                    for entry in &entries {
                        if entry.id.0 != next_id {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "out of order directory entry",
                            ));
                        }
                        next_id = next_id.checked_add(1).ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "directory identity overflow",
                            )
                        })?;
                    }
                    skipped = skipped.checked_add(batch_skipped).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidData, "skipped count overflow")
                    })?;
                    if !entries.is_empty() {
                        on_batch(entries)?;
                    }
                    if cancel.load(Ordering::Acquire) {
                        return Err(cancelled());
                    }
                    // Removing the slot acknowledges UI consumption, not merely queue delivery.
                    std::fs::remove_file(output)?;
                    completed = done;
                    last_progress = Instant::now();
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            if let Some(status) = child.try_wait()? {
                return if status.success() && completed {
                    Ok(skipped)
                } else {
                    Err(io::Error::other(format!(
                        "network directory helper exited with {status}; completed={completed}; stderr={diagnostic:?}"
                    )))
                };
            }
            if last_progress.elapsed() >= timeout {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "network directory helper made no progress",
                ));
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    })();
    // Reap on every path, including callback/protocol errors and failed Job assignment.
    let _ = child.kill();
    let status = child.wait()?;
    #[cfg(test)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        assert_eq!(
            unsafe { WaitForSingleObject(child.as_raw_handle() as _, 0) },
            0
        );
        eprintln!(
            "#103 helper_pid={} exit_status={status} exited_handle_signalled=true",
            child.id()
        );
    }
    #[cfg(not(test))]
    let _ = status;
    match std::fs::read_to_string(&diagnostic) {
        Ok(stderr) if result.is_err() && !stderr.is_empty() => {
            eprintln!("directory helper stderr ({diagnostic:?}):\n{stderr}");
        }
        Ok(_) => {
            let _ = std::fs::remove_file(&diagnostic);
        }
        Err(error) => eprintln!("cannot read directory helper stderr {diagnostic:?}: {error}"),
    }
    result
}

#[cfg(test)]
fn publish_batch(
    output: &Path,
    entries: &[FileEntry],
    skipped: usize,
    done: bool,
) -> io::Result<()> {
    publish_frame(output, &encode_batch(entries, skipped, done)?)
}

fn publish_frame(output: &Path, bytes: &[u8]) -> io::Result<()> {
    publish_frame_with_probe(output, bytes, frame_slot_exists)
}

fn publish_frame_with_probe(
    output: &Path,
    bytes: &[u8],
    mut slot_exists: impl FnMut(&Path) -> io::Result<bool>,
) -> io::Result<()> {
    let pending = output.with_extension("pending");
    let report = |stage: &str, error: io::Error| {
        eprintln!("directory frame {stage}: output={output:?}; error={error:?}");
        error
    };
    std::fs::write(&pending, bytes).map_err(|error| report("write", error))?;
    std::fs::rename(&pending, output).map_err(|error| report("publish", error))?;
    while slot_exists(output).map_err(|error| report("acknowledge", error))? {
        std::thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

fn frame_slot_exists(output: &Path) -> io::Result<bool> {
    use windows_sys::Win32::Storage::FileSystem::{GetFileAttributesW, INVALID_FILE_ATTRIBUTES};

    let path = output
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    // Avoid opening a handle that would prolong deletion of the single frame slot.
    if unsafe { GetFileAttributesW(path.as_ptr()) } != INVALID_FILE_ATTRIBUTES {
        return Ok(true);
    }
    frame_slot_error(io::Error::last_os_error())
}

fn frame_slot_error(error: io::Error) -> io::Result<bool> {
    use windows_sys::Win32::Foundation::{
        ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
    };
    match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_FILE_NOT_FOUND | ERROR_PATH_NOT_FOUND) => Ok(false),
        // DeleteFile can overlap GetFileAttributes even without an open reader handle.
        // Only absence acknowledges consumption; access denied keeps backpressure in place.
        Some(ERROR_ACCESS_DENIED) => Ok(true),
        _ => Err(error),
    }
}

pub(super) fn run_child(path: &Path, visibility: FileVisibility, output: &Path) -> io::Result<()> {
    stream_directory(path, visibility, |bytes| publish_frame(output, bytes))
}

fn stream_directory(
    path: &Path,
    visibility: FileVisibility,
    mut emit: impl FnMut(&[u8]) -> io::Result<()>,
) -> io::Result<()> {
    let mut transport_failed = false;
    let result = enumerate_directory(path, visibility, |entries, skipped, done| {
        let result = encode_batch(entries, skipped, done).and_then(|bytes| emit(&bytes));
        transport_failed = result.is_err();
        result
    });
    match result {
        Ok(()) => Ok(()),
        // A broken transport cannot carry another frame, especially after completion was sent.
        Err(error) if transport_failed => Err(error),
        Err(error) => emit(&encode_error(&error)),
    }
}

fn enumerate_directory(
    path: &Path,
    visibility: FileVisibility,
    mut emit: impl FnMut(&[FileEntry], usize, bool) -> io::Result<()>,
) -> io::Result<()> {
    let mut entries = Vec::with_capacity(MAX_BATCH_ITEMS);
    let mut batch_bytes = 13_usize;
    let mut skipped = 0_usize;
    let mut next_id = 1_u32;
    let mut last_batch = Instant::now();
    for result in std::fs::read_dir(path)? {
        if last_batch.elapsed() >= BATCH_INTERVAL {
            emit(&entries, skipped, false)?;
            entries.clear();
            batch_bytes = 13;
            skipped = 0;
            last_batch = Instant::now();
        }
        let directory_entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                skipped = skipped.saturating_add(1);
                continue;
            }
        };
        let entry = match crate::fs::read_directory_entry(directory_entry, visibility, next_id) {
            Ok(Some(entry)) => entry,
            Ok(None) => continue,
            Err(_) => {
                skipped = skipped.saturating_add(1);
                continue;
            }
        };
        let estimated_bytes = 64
            + 2 * (entry.original_name.encode_wide().count()
                + entry.path.as_os_str().encode_wide().count());
        if batch_bytes + estimated_bytes > MAX_BATCH_BYTES && !entries.is_empty() {
            emit(&entries, skipped, false)?;
            entries.clear();
            batch_bytes = 13;
            skipped = 0;
            last_batch = Instant::now();
        }
        entries.push(entry);
        batch_bytes += estimated_bytes;
        let first = next_id == 1;
        next_id = next_id.checked_add(1).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "directory identity overflow")
        })?;
        if first || entries.len() >= MAX_BATCH_ITEMS || last_batch.elapsed() >= BATCH_INTERVAL {
            emit(&entries, skipped, false)?;
            entries.clear();
            batch_bytes = 13;
            skipped = 0;
            last_batch = Instant::now();
        }
    }
    if !entries.is_empty() || skipped > 0 {
        emit(&entries, skipped, false)?;
    }
    emit(&[], 0, true)
}

fn encode_batch(entries: &[FileEntry], skipped: usize, done: bool) -> io::Result<Vec<u8>> {
    if done && (!entries.is_empty() || skipped != 0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid completion batch",
        ));
    }
    if entries.len() > MAX_BATCH_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many helper items",
        ));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&u64::try_from(skipped).unwrap_or(u64::MAX).to_le_bytes());
    bytes.push(u8::from(done));
    for entry in entries {
        bytes.extend_from_slice(&entry.id.0.to_le_bytes());
        write_units(
            &mut bytes,
            &entry.original_name.encode_wide().collect::<Vec<_>>(),
        )?;
        write_units(
            &mut bytes,
            &entry.path.as_os_str().encode_wide().collect::<Vec<_>>(),
        )?;
        bytes.push(match entry.kind {
            EntryKind::Directory => 0,
            EntryKind::File => 1,
            EntryKind::Other => 2,
        });
        write_optional_u64(&mut bytes, entry.size_bytes);
        write_system_time(&mut bytes, entry.modified)?;
        write_system_time(&mut bytes, entry.created)?;
        if bytes.len() > MAX_BATCH_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "helper result too large",
            ));
        }
    }
    Ok(bytes)
}

fn encode_error(error: &io::Error) -> Vec<u8> {
    let mut bytes = vec![0; 13];
    bytes[12] = 2;
    if let Some(code) = error.raw_os_error() {
        bytes.push(0);
        bytes.extend_from_slice(&code.to_le_bytes());
    } else {
        bytes.push(match error.kind() {
            io::ErrorKind::NotFound => 1,
            io::ErrorKind::PermissionDenied => 2,
            io::ErrorKind::Interrupted => 3,
            io::ErrorKind::TimedOut => 4,
            io::ErrorKind::InvalidData => 5,
            _ => 6,
        });
    }
    bytes
}

fn decode_error(bytes: &[u8], offset: &mut usize) -> io::Result<io::Error> {
    let tag = read_byte(bytes, offset)?;
    let error = match tag {
        0 => {
            let code = read_u32(bytes, offset)? as i32;
            if code == 0 {
                return Err(io::ErrorKind::InvalidData.into());
            }
            io::Error::from_raw_os_error(code)
        }
        1 => io::ErrorKind::NotFound.into(),
        2 => io::ErrorKind::PermissionDenied.into(),
        3 => io::ErrorKind::Interrupted.into(),
        4 => io::ErrorKind::TimedOut.into(),
        5 => io::ErrorKind::InvalidData.into(),
        6 => io::ErrorKind::Other.into(),
        _ => return Err(io::ErrorKind::InvalidData.into()),
    };
    if *offset != bytes.len() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    Ok(error)
}

fn read_batch(reader: impl Read) -> io::Result<(Vec<FileEntry>, usize, bool)> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_BATCH_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper result too large",
        ));
    }
    let mut offset = 0;
    let count = read_u32(&bytes, &mut offset)? as usize;
    if count > MAX_BATCH_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many helper items",
        ));
    }
    let skipped = usize::try_from(read_u64(&bytes, &mut offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid skipped count"))?;
    let done = match read_byte(&bytes, &mut offset)? {
        0 => false,
        1 if count == 0 && skipped == 0 => true,
        2 if count == 0 && skipped == 0 => return Err(decode_error(&bytes, &mut offset)?),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid completion batch",
            ));
        }
    };
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let id = EntryId(read_u32(&bytes, &mut offset)?);
        let original_name = OsString::from_wide(&read_units_at(&bytes, &mut offset)?);
        let entry_path = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
        let kind = match read_byte(&bytes, &mut offset)? {
            0 => EntryKind::Directory,
            1 => EntryKind::File,
            2 => EntryKind::Other,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid entry kind",
                ));
            }
        };
        let size_bytes = read_optional_u64(&bytes, &mut offset)?;
        let modified = read_system_time(&bytes, &mut offset)?;
        let created = read_system_time(&bytes, &mut offset)?;
        entries.push(FileEntry {
            id,
            display_name: original_name.to_string_lossy().into_owned(),
            name_highlights: Vec::new(),
            original_name,
            path: entry_path.clone(),
            kind,
            open_target: None,
            library_source_index: None,
            parent_display: entry_path
                .parent()
                .map(|value| value.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default(),
            size_bytes,
            folder_size: if crate::network::is_unc_path(&entry_path) {
                FolderSizeState::NotIndexed
            } else {
                FolderSizeState::Unknown
            },
            modified,
            created,
        });
    }
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing directory helper data",
        ));
    }
    Ok((entries, skipped, done))
}

#[cfg(test)]
pub(crate) use tests::test_directory_stream_started;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn entry(id: u32) -> FileEntry {
        let name = OsString::from(format!("file-{id}"));
        FileEntry {
            id: EntryId(id),
            original_name: name.clone(),
            display_name: name.to_string_lossy().into(),
            name_highlights: vec![],
            path: PathBuf::from(r"\\fixture\share").join(name),
            kind: EntryKind::File,
            open_target: None,
            library_source_index: None,
            parent_display: String::new(),
            size_bytes: Some(u64::from(id)),
            folder_size: FolderSizeState::NotIndexed,
            modified: None,
            created: None,
        }
    }

    pub(super) fn child_command(files: &StreamFiles, mode: &str) -> Command {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "platform::windows::network::directory::tests::issue_103_child_entry",
                "--nocapture",
            ])
            .env("ASTERFILES_TEST_DIRECTORY_OUTPUT", files.output())
            .env("ASTERFILES_TEST_DIRECTORY_MODE", mode)
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    pub(crate) fn test_directory_stream_started(
        path: &Path,
        cancel: &AtomicBool,
        started: &AtomicBool,
        on_batch: impl FnMut(Vec<FileEntry>) -> io::Result<()>,
    ) -> io::Result<usize> {
        let files = StreamFiles::create()?;
        write_directory_input(&files.input(), path, FileVisibility::default())?;
        let mode = if path.file_name() == Some(std::ffi::OsStr::new("hang")) {
            "hang"
        } else {
            "enumerate"
        };
        let ready = files.0.join("ready");
        struct FinishOnDrop<'a>(&'a AtomicBool);
        impl Drop for FinishOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let finished = AtomicBool::new(false);
        let mut command = child_command(&files, mode);
        command.env("ASTERFILES_DIRECTORY_TEST_READY", &ready);
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !finished.load(Ordering::Acquire) {
                    if ready.is_file() {
                        started.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            });
            let completion = FinishOnDrop(&finished);
            let result = receive_stream(
                command,
                &files.output(),
                cancel,
                Duration::from_secs(30),
                on_batch,
            );
            drop(completion);
            result
        });
        let _ = std::fs::remove_file(ready);
        eprintln!(
            "#104 directory_source={path:?} fixture={mode} stream_returned=true result={result:?}"
        );
        result
    }
    #[test]
    fn issue_103_child_entry() {
        let Some(output) = std::env::var_os("ASTERFILES_TEST_DIRECTORY_OUTPUT") else {
            return;
        };
        let output = PathBuf::from(output);
        let mode = std::env::var("ASTERFILES_TEST_DIRECTORY_MODE").unwrap();
        if let Some(ready) = std::env::var_os("ASTERFILES_DIRECTORY_TEST_READY") {
            std::fs::write(ready, std::process::id().to_string()).unwrap();
        }
        match mode.as_str() {
            "hang" => std::thread::sleep(Duration::from_secs(60)),
            "crash" => std::process::exit(17),
            "panic" => panic!("issue_138_original_child_error"),
            "ack-denied" => {
                let mut ready = false;
                publish_frame_with_probe(&output, &encode_batch(&[], 0, true).unwrap(), |_| {
                    if !ready {
                        std::fs::write(output.with_extension("denied"), b"waiting").unwrap();
                        ready = true;
                    }
                    frame_slot_error(io::Error::from_raw_os_error(5))
                })
                .unwrap();
            }
            "complete-panic" => {
                publish_batch(&output, &[], 0, true).unwrap();
                panic!("issue_138_original_child_error");
            }
            "incomplete" => {}
            "bad-order" => publish_batch(&output, &[entry(2)], 0, false).unwrap(),
            "slow" | "backpressure" => {
                for id in 1..=12 {
                    publish_batch(&output, &[entry(id)], 0, false).unwrap();
                    if mode == "slow" {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                }
                publish_batch(&output, &[], 0, true).unwrap();
            }
            "count-100k" => {
                publish_batch(&output, &[entry(1)], 0, false).unwrap();
                for start in (2..=100_000).step_by(MAX_BATCH_ITEMS) {
                    let entries = (start..=(start + MAX_BATCH_ITEMS as u32 - 1).min(100_000))
                        .map(entry)
                        .collect::<Vec<_>>();
                    publish_batch(&output, &entries, 0, false).unwrap();
                }
                publish_batch(&output, &[], 0, true).unwrap();
            }
            "enumerate" => {
                let (path, visibility) =
                    read_directory_input(&output.with_file_name("input")).unwrap();
                run_child(&path, visibility, &output).unwrap();
            }
            _ => panic!("unknown fixture"),
        }
    }

    #[test]
    fn issue_150_100k_slow_consumer_preserves_every_entry() {
        let files = StreamFiles::create().unwrap();
        let command = child_command(&files, "count-100k");
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        let mut first_ms = None;
        let mut count = 0_u32;
        let mut batches = 0;
        let mut largest = 0;
        receive_stream(
            command,
            &files.output(),
            &cancel,
            Duration::from_secs(30),
            |entries| {
                first_ms.get_or_insert_with(|| started.elapsed().as_millis());
                largest = largest.max(entries.len());
                batches += 1;
                for entry in entries {
                    count += 1;
                    assert_eq!(entry.id.0, count);
                }
                std::thread::sleep(Duration::from_millis(1));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 100_000);
        assert!(largest <= MAX_BATCH_ITEMS);
        let report = format!(
            "{{\"entries\":{count},\"batches\":{batches},\"largest_batch\":{largest},\"first_ms\":{},\"total_ms\":{},\"transport_slots\":1}}",
            first_ms.unwrap(),
            started.elapsed().as_millis()
        );
        std::fs::create_dir_all("artifacts/state/issue-150").unwrap();
        std::fs::write("artifacts/state/issue-150/100k.json", report).unwrap();
    }

    #[test]
    fn issue_138_delete_in_progress_keeps_backpressure_and_preserves_other_errors() {
        use windows_sys::Win32::Foundation::{
            ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND,
            ERROR_SHARING_VIOLATION,
        };
        assert!(
            frame_slot_error(io::Error::from_raw_os_error(ERROR_ACCESS_DENIED as i32)).unwrap()
        );
        for code in [ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND] {
            assert!(!frame_slot_error(io::Error::from_raw_os_error(code as i32)).unwrap());
        }
        assert_eq!(
            frame_slot_error(io::Error::from_raw_os_error(ERROR_SHARING_VIOLATION as i32))
                .unwrap_err()
                .raw_os_error(),
            Some(ERROR_SHARING_VIOLATION as i32)
        );
    }

    #[test]
    fn issue_138_access_denied_waits_until_absence_before_reusing_slot() {
        let files = StreamFiles::create().unwrap();
        let bytes = encode_batch(&[entry(1)], 0, false).unwrap();
        let mut queries = 0;
        publish_frame_with_probe(&files.output(), &bytes, |output| {
            queries += 1;
            assert_eq!(std::fs::read(output).unwrap(), bytes);
            if queries <= 3 {
                frame_slot_error(io::Error::from_raw_os_error(5))
            } else {
                std::fs::remove_file(output).unwrap();
                frame_slot_exists(output)
            }
        })
        .unwrap();
        assert_eq!(queries, 4);
        assert!(!frame_slot_exists(&files.output()).unwrap());
    }

    #[test]
    fn issue_138_permanent_ack_denial_after_completion_is_timed_out_and_cancelled() {
        for cancel_after_start in [false, true] {
            let files = StreamFiles::create().unwrap();
            let cancel = AtomicBool::new(false);
            std::thread::scope(|scope| {
                if cancel_after_start {
                    scope.spawn(|| {
                        let started = Instant::now();
                        while !files.output().with_extension("denied").is_file() {
                            assert!(started.elapsed() < Duration::from_secs(10));
                            std::thread::sleep(POLL_INTERVAL);
                        }
                        cancel.store(true, Ordering::Release);
                    });
                }
                let error = receive_stream(
                    child_command(&files, "ack-denied"),
                    &files.output(),
                    &cancel,
                    Duration::from_secs(2),
                    |_| Ok(()),
                )
                .unwrap_err();
                assert_eq!(
                    error.kind(),
                    if cancel_after_start {
                        io::ErrorKind::Interrupted
                    } else {
                        io::ErrorKind::TimedOut
                    }
                );
            });
            std::fs::remove_file(files.output().with_extension("denied")).unwrap();
        }
    }

    #[test]
    fn issue_138_concurrent_delete_and_ack_query_preserves_slot_reuse() {
        let files = StreamFiles::create().unwrap();
        let output = files.output();
        std::thread::scope(|scope| {
            let (delete, pending) = std::sync::mpsc::sync_channel(0);
            let (deleted, consumed) = std::sync::mpsc::sync_channel(0);
            let writer_output = &output;
            scope.spawn(move || {
                while pending.recv().is_ok() {
                    std::fs::remove_file(writer_output).unwrap();
                    if deleted.send(()).is_err() {
                        break;
                    }
                }
            });
            for _ in 0..4096 {
                std::fs::write(&output, b"frame").unwrap();
                assert!(frame_slot_exists(&output).unwrap());
                delete.send(()).unwrap();
                let started = Instant::now();
                while frame_slot_exists(&output).unwrap() {
                    assert!(started.elapsed() < Duration::from_secs(5));
                }
                consumed.recv().unwrap();
            }
            drop(delete);
            drop(consumed);
        });
    }

    #[test]
    fn issue_138_child_panic_preserves_stderr_stack_and_rejects_completed_failure() {
        for (mode, completed) in [("panic", false), ("complete-panic", true)] {
            let files = StreamFiles::create().unwrap();
            let error = receive_stream(
                child_command(&files, mode),
                &files.output(),
                &AtomicBool::new(false),
                Duration::from_secs(10),
                |_| Ok(()),
            )
            .unwrap_err();
            let message = error.to_string();
            assert!(message.contains("101"), "{message}");
            assert!(
                message.contains(&format!("completed={completed}")),
                "{message}"
            );
            assert!(message.contains("stderr="), "{message}");
            let diagnostic = files.output().with_extension("stderr");
            let stderr = std::fs::read_to_string(&diagnostic).unwrap();
            assert!(
                stderr.contains("issue_138_original_child_error"),
                "{stderr}"
            );
            assert!(stderr.contains("stack backtrace:"), "{stderr}");
            std::fs::remove_file(diagnostic).unwrap();
        }
    }

    #[test]
    fn issue_104_completion_transport_failure_does_not_publish_another_terminal_frame() {
        let fixture = StreamFiles::create().unwrap();
        let mut frames = Vec::new();
        let error = stream_directory(&fixture.0, FileVisibility::default(), |bytes| {
            frames.push(bytes.to_vec());
            Err(io::ErrorKind::BrokenPipe.into())
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
        assert_eq!(frames.len(), 1);
        let (entries, skipped, done) = read_batch(frames[0].as_slice()).unwrap();
        assert!(entries.is_empty());
        assert_eq!(skipped, 0);
        assert!(done);
    }

    #[test]
    fn issue_104_deleted_slot_is_acknowledged_while_a_shared_delete_handle_remains_open() {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };

        let files = StreamFiles::create().unwrap();
        let output = files.output();
        std::fs::write(&output, encode_batch(&[], 0, true).unwrap()).unwrap();
        let held = std::fs::OpenOptions::new()
            .access_mode(0)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .open(&output)
            .unwrap();
        assert!(frame_slot_exists(&output).unwrap());
        std::fs::remove_file(&output).unwrap();
        assert!(!frame_slot_exists(&output).unwrap());
        // Querying this handle proves acknowledgement did not require its release.
        assert_eq!(held.metadata().unwrap().len(), 13);
        drop(held);
    }
    #[test]
    fn issue_104_missing_source_preserves_error_kind_and_reaps_real_child() {
        let fixture = StreamFiles::create().unwrap();
        let started = AtomicBool::new(false);
        let error = test_directory_stream_started(
            &fixture.0.join("missing"),
            &AtomicBool::new(false),
            &started,
            |_| panic!("missing source must not produce entries"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert!(error.raw_os_error().is_some());
    }

    #[test]
    fn issue_104_error_frames_preserve_permissions_and_reject_corruption() {
        for error in [
            io::Error::from_raw_os_error(5),
            io::Error::from(io::ErrorKind::PermissionDenied),
        ] {
            let encoded = encode_error(&error);
            let decoded = read_batch(encoded.as_slice()).unwrap_err();
            assert_eq!(decoded.kind(), io::ErrorKind::PermissionDenied);
            assert_eq!(decoded.raw_os_error(), error.raw_os_error());
            for length in 0..encoded.len() {
                assert!(matches!(
                    read_batch(&encoded[..length]).unwrap_err().kind(),
                    io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
                ));
            }
            let mut trailing = encoded.clone();
            trailing.push(0);
            assert_eq!(
                read_batch(trailing.as_slice()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            let mut with_entries = encoded;
            with_entries[0] = 1;
            assert_eq!(
                read_batch(with_entries.as_slice()).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
        let mut invalid = vec![0; 14];
        invalid[12] = 2;
        invalid[13] = 255;
        assert_eq!(
            read_batch(invalid.as_slice()).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            read_batch(encode_error(&io::Error::from_raw_os_error(0)).as_slice())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn issue_104_library_stream_preserves_cleanup_filter_and_local_folder_size_queries() {
        let fixture = StreamFiles::create().unwrap();
        let data = fixture.0.join("data");
        std::fs::create_dir(&data).unwrap();
        for name in [".asterfiles-cleanup", ".ASTERFILES-CLEANUP-copy", "folder"] {
            std::fs::create_dir(data.join(name)).unwrap();
        }
        std::fs::write(data.join("kept.txt"), b"fixture").unwrap();
        let files = StreamFiles::create().unwrap();
        write_directory_input(
            &files.input(),
            &data,
            FileVisibility {
                show_hidden: true,
                show_system: true,
            },
        )
        .unwrap();
        let mut entries = Vec::new();
        receive_stream(
            child_command(&files, "enumerate"),
            &files.output(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
            |batch| {
                entries.extend(batch);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(entries.len(), 3);
        assert!(
            !entries
                .iter()
                .any(|entry| entry.display_name == ".asterfiles-cleanup")
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.display_name == ".ASTERFILES-CLEANUP-copy")
        );
        let folder = entries
            .iter()
            .find(|entry| entry.display_name == "folder")
            .unwrap();
        assert_eq!(folder.kind, EntryKind::Directory);
        assert_eq!(folder.folder_size, FolderSizeState::Unknown);
        assert_eq!(folder.path, data.join("folder"));
        for name in [".asterfiles-cleanup", ".ASTERFILES-CLEANUP-copy", "folder"] {
            std::fs::remove_dir(data.join(name)).unwrap();
        }
        std::fs::remove_file(data.join("kept.txt")).unwrap();
        std::fs::remove_dir(data).unwrap();
    }

    #[test]
    fn issue_103_streams_4096_4097_and_8193_real_files_without_duplicates() {
        let fixture = StreamFiles::create().unwrap();
        let data = fixture.0.join("data");
        std::fs::create_dir(&data).unwrap();
        let mut created = 0;
        for count in [4096, 4097, 8193] {
            while created < count {
                std::fs::write(data.join(format!("entry-{created:05}.txt")), b"fixture").unwrap();
                created += 1;
            }
            let files = StreamFiles::create().unwrap();
            write_directory_input(
                &files.input(),
                &data,
                FileVisibility {
                    show_hidden: true,
                    show_system: true,
                },
            )
            .unwrap();
            let started = Instant::now();
            let mut first = None;
            let mut peak = 0;
            let mut paths = std::collections::HashSet::new();
            let mut ids = std::collections::HashSet::new();
            let skipped = receive_stream(
                child_command(&files, "enumerate"),
                &files.output(),
                &AtomicBool::new(false),
                Duration::from_secs(10),
                |entries| {
                    first.get_or_insert(started.elapsed());
                    peak = peak.max(entries.len());
                    for entry in entries {
                        assert_eq!(entry.size_bytes, Some(7));
                        assert_eq!(entry.path.parent(), Some(data.as_path()));
                        assert!(paths.insert(entry.path));
                        assert!(ids.insert(entry.id.0));
                    }
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(skipped, 0);
            assert_eq!(paths.len(), count);
            for index in 0..count {
                assert!(paths.contains(&data.join(format!("entry-{index:05}.txt"))));
            }
            assert_eq!(ids.len(), count);
            assert!(peak <= MAX_BATCH_ITEMS);
            assert!(first.unwrap() < started.elapsed());
            eprintln!(
                "#103 count={count} first_ms={} complete_ms={} peak_batch_items={peak} in_flight_batches=1 helper_reaped=true",
                first.unwrap().as_millis(),
                started.elapsed().as_millis()
            );
        }
        for index in 0..created {
            std::fs::remove_file(data.join(format!("entry-{index:05}.txt"))).unwrap();
        }
        std::fs::remove_dir(data).unwrap();
    }

    #[test]
    fn issue_103_slow_progress_extends_idle_deadline() {
        let files = StreamFiles::create().unwrap();
        let started = Instant::now();
        let mut first = None;
        let mut count = 0;
        receive_stream(
            child_command(&files, "slow"),
            &files.output(),
            &AtomicBool::new(false),
            Duration::from_millis(700),
            |entries| {
                first.get_or_insert(started.elapsed());
                count += entries.len();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 12);
        assert!(started.elapsed() > Duration::from_millis(700));
        assert!(first.unwrap() < Duration::from_millis(700));
        eprintln!(
            "#103 slow first_ms={} complete_ms={} idle_timeout_ms=700 helper_reaped=true",
            first.unwrap().as_millis(),
            started.elapsed().as_millis()
        );
    }

    #[test]
    fn issue_103_backpressure_keeps_single_slot_until_consumed() {
        let files = StreamFiles::create().unwrap();
        let mut count = 0;
        receive_stream(
            child_command(&files, "backpressure"),
            &files.output(),
            &AtomicBool::new(false),
            Duration::from_millis(700),
            |entries| {
                count += entries.len();
                if count == 1 {
                    let before = std::fs::read(files.output()).unwrap();
                    std::thread::sleep(Duration::from_millis(900));
                    assert_eq!(std::fs::read(files.output()).unwrap(), before);
                    assert!(!files.output().with_extension("pending").exists());
                }
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(count, 12);
        eprintln!("#103 backpressure pause_ms=900 in_flight_batches=1 helper_reaped=true");
    }

    #[test]
    fn issue_103_hung_crashed_incomplete_and_invalid_children_are_reaped() {
        for (mode, expected) in [
            ("hang", io::ErrorKind::TimedOut),
            ("crash", io::ErrorKind::Other),
            ("incomplete", io::ErrorKind::Other),
            ("bad-order", io::ErrorKind::InvalidData),
        ] {
            let files = StreamFiles::create().unwrap();
            let started = Instant::now();
            let error = receive_stream(
                child_command(&files, mode),
                &files.output(),
                &AtomicBool::new(false),
                Duration::from_millis(700),
                |_| Ok(()),
            )
            .unwrap_err();
            assert_eq!(error.kind(), expected);
            assert!(started.elapsed() < Duration::from_secs(5));
            eprintln!(
                "#103 mode={mode} error={expected:?} elapsed_ms={} helper_reaped=true",
                started.elapsed().as_millis()
            );
        }
    }

    #[test]
    fn issue_103_cancel_hang_and_pending_batch_reaps_child() {
        let files = StreamFiles::create().unwrap();
        let cancel = Arc::new(AtomicBool::new(false));
        let signal = cancel.clone();
        let timer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            signal.store(true, Ordering::Release);
        });
        let started = Instant::now();
        let error = receive_stream(
            child_command(&files, "hang"),
            &files.output(),
            &cancel,
            Duration::from_secs(10),
            |_| Ok(()),
        )
        .unwrap_err();
        timer.join().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(started.elapsed() < Duration::from_secs(2));
        cancel.store(false, Ordering::Release);
        let mut callbacks = 0;
        let error = receive_stream(
            child_command(&files, "backpressure"),
            &files.output(),
            &cancel,
            Duration::from_secs(10),
            |_| {
                callbacks += 1;
                cancel.store(true, Ordering::Release);
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert_eq!(callbacks, 1);
        eprintln!("#103 cancellation hang_and_pending_batch helper_reaped=true");
    }

    #[test]
    fn issue_103_callback_failure_reaps_child() {
        let files = StreamFiles::create().unwrap();
        let error = receive_stream(
            child_command(&files, "backpressure"),
            &files.output(),
            &AtomicBool::new(false),
            Duration::from_secs(10),
            |_| Err(io::ErrorKind::BrokenPipe.into()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn issue_103_batch_preserves_raw_identity_and_rejects_unbounded_or_invalid_frames() {
        let mut original = entry(99);
        original.original_name = OsString::from_wide(&[b'a' as u16, 0xd800]);
        original.path = PathBuf::from(r"\\server\share").join(&original.original_name);
        original.modified = Some(SystemTime::UNIX_EPOCH + Duration::new(42, 7));
        let encoded = encode_batch(&[original.clone()], 5, false).unwrap();
        let (decoded, skipped, done) = read_batch(encoded.as_slice()).unwrap();
        assert_eq!(decoded[0].id, original.id);
        assert_eq!(decoded[0].path, original.path);
        assert_eq!(decoded[0].original_name, original.original_name);
        assert_eq!(decoded[0].modified, original.modified);
        assert_eq!(skipped, 5);
        assert!(!done);
        assert!(read_batch(&encoded[..encoded.len() - 1]).is_err());
        let mut trailing = encoded.clone();
        trailing.push(0);
        assert!(read_batch(trailing.as_slice()).is_err());
        assert!(encode_batch(&vec![entry(1); MAX_BATCH_ITEMS + 1], 0, false).is_err());
        assert!(encode_batch(&[entry(1)], 0, true).is_err());
        assert!(read_batch(vec![0; MAX_BATCH_BYTES + 1].as_slice()).is_err());
        let mut invalid = vec![0; 13];
        invalid[12] = 2;
        assert!(read_batch(invalid.as_slice()).is_err());
        let (empty, _, done) = read_batch(encode_batch(&[], 0, true).unwrap().as_slice()).unwrap();
        assert!(empty.is_empty() && done);
    }
}
