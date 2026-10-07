use super::*;
use crate::domain::file_operations::CancellationToken;
use crate::platform::windows::copy_file::CopyFileErrorKind;
use std::sync::{Condvar, Mutex, atomic::AtomicUsize};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "asterfiles-block-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn source(&self) -> PathBuf {
        let path = self.0.join("源 (test) & data.bin");
        fs::write(
            &path,
            (0..(BLOCK_SIZE * 3 + 17))
                .map(|index| (index % 251) as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Faults {
    errors: AtomicUsize,
    block_once: AtomicBool,
    entered: AtomicBool,
    released: Mutex<bool>,
    wake: Condvar,
    reads: AtomicUsize,
    active: AtomicUsize,
    opens: Mutex<Vec<u64>>,
    deny: AtomicBool,
}
impl Faults {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}
struct ReleaseOnDrop(Arc<Faults>);
impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct FaultSource(Arc<Faults>);
impl Source for FaultSource {
    fn version(&self, path: &Path) -> io::Result<SourceVersion> {
        NativeSource.version(path)
    }
    fn open(
        &self,
        path: &Path,
        stream: &StreamVersion,
        expected: &SourceVersion,
        offset: u64,
    ) -> io::Result<Box<dyn SourceReader>> {
        self.0.opens.lock().unwrap().push(offset);
        if self.0.deny.load(Ordering::Acquire) {
            return Err(io::Error::from_raw_os_error(5));
        }
        let reader = NativeSource.open(path, stream, expected, offset)?;
        self.0.active.fetch_add(1, Ordering::AcqRel);
        Ok(Box::new(FaultReader {
            reader,
            state: self.0.clone(),
            position: offset,
        }))
    }
}
struct FaultReader {
    reader: Box<dyn SourceReader>,
    state: Arc<Faults>,
    position: u64,
}
impl Read for FaultReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.position == BLOCK_SIZE as u64 {
            if self
                .state
                .errors
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                .is_ok()
            {
                return Err(io::Error::from_raw_os_error(64));
            }
            if self.state.block_once.swap(false, Ordering::AcqRel) {
                self.state.entered.store(true, Ordering::Release);
                let mut released = self.state.released.lock().unwrap();
                while !*released {
                    released = self.state.wake.wait(released).unwrap();
                }
                // A superseded read must be discarded even if it returns successfully later.
                bytes.fill(0xee);
                return Ok(bytes.len());
            }
        }
        let count = self.reader.read(bytes)?;
        self.position += count as u64;
        self.state.reads.fetch_add(count, Ordering::AcqRel);
        Ok(count)
    }
}
impl SourceReader for FaultReader {
    fn validate(&self) -> io::Result<()> {
        self.reader.validate()
    }
}

impl Drop for FaultReader {
    fn drop(&mut self) {
        self.state.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn until(mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "timed out waiting for test event"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn issue_137_block_download_resumes_after_more_than_twelve_disconnects_without_rereading_prefix() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    faults.errors.store(16, Ordering::Release);
    let mut bytes = 0;
    let execution = CopyExecution::local(CancellationToken::new());
    let copied = copy_with(
        Arc::new(FaultSource(faults.clone())),
        &source,
        &destination,
        &execution,
        &mut |count| bytes += count,
        Duration::from_secs(3),
        Some(Duration::ZERO),
    )
    .unwrap();
    assert!(!execution.is_recovering());
    let original = fs::read(&source).unwrap();
    assert_eq!(fs::read(&destination).unwrap(), original);
    assert_eq!(copied, original.len() as u64);
    assert_eq!(bytes, copied);
    assert_eq!(faults.reads.load(Ordering::Acquire), original.len());
    let opens = faults.opens.lock().unwrap();
    assert_eq!(opens.len(), 17);
    assert_eq!(opens[0], 0);
    assert!(opens[1..].iter().all(|offset| *offset == BLOCK_SIZE as u64));
}

fn blocked_copy(
    faults: &Arc<Faults>,
    source: &Path,
    destination: &Path,
    execution: &CopyExecution,
    timeout: Duration,
) -> thread::JoinHandle<Result<u64, CopyFileError>> {
    faults.block_once.store(true, Ordering::Release);
    let backend = Arc::new(FaultSource(faults.clone()));
    let source = source.to_owned();
    let destination = destination.to_owned();
    let execution = execution.clone();
    thread::spawn(move || {
        copy_with(
            backend,
            &source,
            &destination,
            &execution,
            &mut |_| {},
            timeout,
            Some(Duration::ZERO),
        )
    })
}

#[test]
fn issue_137_block_download_pause_discards_blocked_old_read_and_resumes_from_confirmed_offset() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    let cancel = CancellationToken::new();
    let execution = CopyExecution::local(cancel.clone());
    let _release = ReleaseOnDrop(faults.clone());
    let task = blocked_copy(
        &faults,
        &source,
        &destination,
        &execution,
        Duration::from_secs(120),
    );
    until(|| {
        faults.entered.load(Ordering::Acquire)
            && fs::metadata(&destination).is_ok_and(|meta| meta.len() == BLOCK_SIZE as u64)
    });
    cancel.pause();
    until(|| cancel.is_pause_acknowledged());
    thread::sleep(Duration::from_millis(60));
    assert_eq!(fs::metadata(&destination).unwrap().len(), BLOCK_SIZE as u64);
    assert!(!task.is_finished());
    cancel.resume();
    until(|| task.is_finished());
    task.join().unwrap().unwrap();
    assert_eq!(fs::read(&destination).unwrap(), fs::read(&source).unwrap());
    assert_eq!(
        faults.opens.lock().unwrap().as_slice(),
        &[0, BLOCK_SIZE as u64]
    );
    faults.release();
    until(|| faults.active.load(Ordering::Acquire) == 0);
    assert_eq!(fs::read(&destination).unwrap(), fs::read(&source).unwrap());
}

#[test]
fn issue_137_block_download_cancel_does_not_wait_for_stuck_source_or_keep_partial_target() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    let cancel = CancellationToken::new();
    let execution = CopyExecution::local(cancel.clone());
    let _release = ReleaseOnDrop(faults.clone());
    let task = blocked_copy(
        &faults,
        &source,
        &destination,
        &execution,
        Duration::from_secs(120),
    );
    until(|| faults.entered.load(Ordering::Acquire));
    cancel.cancel();
    until(|| task.is_finished());
    assert_eq!(
        task.join().unwrap().unwrap_err().kind,
        CopyFileErrorKind::Cancelled
    );
    assert!(!destination.exists());
    faults.release();
    until(|| faults.active.load(Ordering::Acquire) == 0);
    assert!(!destination.exists());
}

#[test]
fn issue_137_block_download_timeout_resumes_without_reusing_old_reader() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    let _release = ReleaseOnDrop(faults.clone());
    let task = blocked_copy(
        &faults,
        &source,
        &destination,
        &CopyExecution::local(CancellationToken::new()),
        Duration::from_millis(40),
    );
    until(|| task.is_finished());
    task.join().unwrap().unwrap();
    faults.release();
    until(|| faults.active.load(Ordering::Acquire) == 0);
    assert_eq!(fs::read(&destination).unwrap(), fs::read(&source).unwrap());
    assert_eq!(
        faults.opens.lock().unwrap().as_slice(),
        &[0, BLOCK_SIZE as u64]
    );
}

#[test]
fn issue_137_block_download_rejects_changed_source_before_resuming() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    faults.errors.store(1, Ordering::Release);
    let cancel = CancellationToken::new();
    let execution = CopyExecution::local(cancel.clone());
    let task = {
        let backend = Arc::new(FaultSource(faults.clone()));
        let source = source.clone();
        let destination = destination.clone();
        let cancel = cancel.clone();
        let execution = execution.clone();
        thread::spawn(move || {
            copy_with(
                backend,
                &source,
                &destination,
                &execution,
                &mut |_| cancel.pause(),
                Duration::from_secs(120),
                Some(Duration::ZERO),
            )
        })
    };
    until(|| cancel.is_pause_acknowledged() && faults.active.load(Ordering::Acquire) == 0);
    fs::write(&source, b"different source").unwrap();
    cancel.resume();
    until(|| task.is_finished());
    let error = task.join().unwrap().unwrap_err();
    assert_eq!(error.kind, CopyFileErrorKind::Failed);
    assert_eq!(error.error.kind(), io::ErrorKind::InvalidData);
    assert!(!execution.is_recovering());
    assert!(!destination.exists());
}

#[test]
fn issue_137_block_download_does_not_retry_permission_errors_or_touch_existing_destination() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    faults.deny.store(true, Ordering::Release);
    let error = copy_with(
        Arc::new(FaultSource(faults.clone())),
        &source,
        &destination,
        &CopyExecution::local(CancellationToken::new()),
        &mut |_| {},
        Duration::from_secs(1),
        Some(Duration::ZERO),
    )
    .unwrap_err();
    assert_eq!(error.error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(faults.opens.lock().unwrap().len(), 1);
    assert!(!destination.exists());
    fs::write(&destination, b"keep existing").unwrap();
    assert!(
        copy_file(
            &source,
            &destination,
            &CopyExecution::local(CancellationToken::new()),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(fs::read(&destination).unwrap(), b"keep existing");
}

#[test]
fn issue_137_block_download_preserves_named_streams_time_and_readonly_hidden_attributes() {
    use windows_sys::Win32::Storage::FileSystem::GetFileAttributesW;
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let stream = OsStr::new(":asterfiles:$DATA");
    fs::write(stream_path(&source, stream), vec![71; BLOCK_SIZE + 57]).unwrap();
    assert_ne!(
        unsafe {
            SetFileAttributesW(
                wide(&source).as_ptr(),
                FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY,
            )
        },
        0
    );
    let before = NativeSource.version(&source).unwrap();
    let mut total = 0;
    copy_file(
        &source,
        &destination,
        &CopyExecution::local(CancellationToken::new()),
        &mut |count| total += count,
    )
    .unwrap();
    assert_eq!(total, before.size);
    assert_eq!(fs::read(&destination).unwrap(), fs::read(&source).unwrap());
    assert_eq!(
        fs::read(stream_path(&destination, stream)).unwrap(),
        vec![71; BLOCK_SIZE + 57]
    );
    let copied = NativeSource.version(&destination).unwrap();
    assert_eq!(copied.created, before.created);
    assert_eq!(copied.modified, before.modified);
    assert_eq!(
        unsafe { GetFileAttributesW(wide(&destination).as_ptr()) }
            & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY),
        FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_READONLY
    );
    unsafe {
        SetFileAttributesW(wide(&source).as_ptr(), FILE_ATTRIBUTE_NORMAL);
        SetFileAttributesW(wide(&destination).as_ptr(), FILE_ATTRIBUTE_NORMAL);
    }
}

#[test]
fn issue_149_block_download_resets_explicit_recovery_after_cancel_and_early_error() {
    let fixture = Fixture::new();
    let source = fixture.source();
    let destination = fixture.0.join("copy.tmp");
    let faults = Arc::new(Faults::default());
    faults.errors.store(1, Ordering::Release);
    let execution = CopyExecution::local(CancellationToken::new());
    let unrelated = CopyExecution::local(CancellationToken::new());
    let task = {
        let worker = execution.clone();
        let source = source.clone();
        let destination = destination.clone();
        thread::spawn(move || {
            copy_with(
                Arc::new(FaultSource(faults)),
                &source,
                &destination,
                &worker,
                &mut |_| {},
                Duration::from_secs(120),
                Some(Duration::from_secs(120)),
            )
        })
    };
    until(|| execution.is_recovering());
    assert!(!unrelated.is_recovering());
    execution.cancel().cancel();
    until(|| task.is_finished());
    assert_eq!(
        task.join().unwrap().unwrap_err().kind,
        CopyFileErrorKind::Cancelled
    );
    assert!(!execution.is_recovering());
    assert!(!destination.exists());

    let early_error = CopyExecution::local(CancellationToken::new());
    early_error.set_recovering(true);
    assert!(
        copy_file(
            &fixture.0.join("missing-source"),
            &destination,
            &early_error,
            &mut |_| {}
        )
        .is_err()
    );
    assert!(!early_error.is_recovering());
    assert!(!destination.exists());
}
