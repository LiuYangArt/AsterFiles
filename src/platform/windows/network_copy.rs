use super::{
    copy_file::{CopyFileError, CopyFileErrorKind},
    network::KillOnCloseJob,
};
use crate::domain::file_operations::CancellationToken;
use std::{
    ffi::OsString,
    fs,
    io::{self, Read},
    os::windows::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime},
};

const STALL_LIMIT: Duration = Duration::from_secs(120);
// Remote writes retain their separate process-based implementation.
const MAX_RETRIES: u32 = 12;
mod download;
mod read_control;
use read_control::{acquire_read_slot, cancel_read_worker};

fn retry_backoff(attempt: u32) -> Duration {
    Duration::from_secs((1_u64 << attempt.min(5)).min(30))
}

#[cfg(test)]
thread_local! {
    static TEST_PENDING_STOP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_SPAWNS: std::cell::RefCell<Option<Arc<std::sync::atomic::AtomicUsize>>> = const { std::cell::RefCell::new(None) };
}

#[derive(Default)]
struct Output {
    percent: Option<u32>,
    revision: u64,
    errors: Vec<u32>,
    tail: String,
    read_error: Option<String>,
}

fn parse_percent(line: &str) -> Option<u32> {
    let number = line.trim().strip_suffix('%')?.trim();
    let value: f64 = number.parse().ok()?;
    (value.is_finite() && (0.0..=100.0).contains(&value)).then_some((value * 100.0).round() as u32)
}

fn consume_line(output: &mut Output, line: &str) {
    if let Some(percent) = parse_percent(line) {
        if output.percent != Some(percent) {
            output.percent = Some(percent);
            output.revision += 1;
        }
        return;
    }
    for fragment in line.split("(0x").skip(1) {
        if let Some((hex, _)) = fragment.split_once(')')
            && hex.len() <= 8
            && let Ok(code) = u32::from_str_radix(hex, 16)
            && !output.errors.contains(&code)
            && output.errors.len() < 32
        {
            output.errors.push(code);
        }
    }
    if !line.trim().is_empty() {
        output.tail.push_str(line);
        output.tail.push('\n');
        if output.tail.len() > 8192 {
            let cutoff = output
                .tail
                .char_indices()
                .find(|(offset, _)| *offset >= output.tail.len() - 8192)
                .map(|(offset, _)| offset)
                .unwrap_or(0);
            output.tail.drain(..cutoff);
        }
    }
}

fn oem_text(bytes: &[u8]) -> String {
    use windows_sys::Win32::Globalization::{CP_OEMCP, MultiByteToWideChar};
    let length = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            std::ptr::null_mut(),
            0,
        )
    };
    if length == 0 {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut units = vec![0_u16; length as usize];
    let written = unsafe {
        MultiByteToWideChar(
            CP_OEMCP,
            0,
            bytes.as_ptr(),
            bytes.len() as i32,
            units.as_mut_ptr(),
            length,
        )
    };
    String::from_utf16_lossy(&units[..written as usize])
}

fn consume_bytes(output: &mut Output, line: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        if matches!(*byte, 10 | 13) {
            if !line.is_empty() {
                consume_line(output, &oem_text(line));
                line.clear();
            }
        } else if line.len() < 4096 {
            line.push(*byte);
        }
    }
}

fn retryable(code: u32) -> bool {
    matches!(
        code,
        51 | 53
            | 54
            | 55
            | 58
            | 59
            | 64
            | 67
            | 121
            | 995
            | 1201
            | 1222
            | 1231
            | 1232
            | 1236
            | 1237
            | 1460
    )
}

fn failed(error: io::Error) -> CopyFileError {
    CopyFileError {
        kind: CopyFileErrorKind::Failed,
        error,
    }
}
fn cancelled() -> CopyFileError {
    CopyFileError {
        kind: CopyFileErrorKind::Cancelled,
        error: io::ErrorKind::Interrupted.into(),
    }
}

const STOP_GRACE: Duration = Duration::from_millis(250);

fn process_exited(handle: windows_sys::Win32::Foundation::HANDLE) -> io::Result<bool> {
    use windows_sys::Win32::{
        Foundation::{WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::WaitForSingleObject,
    };
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(true),
        WAIT_TIMEOUT => Ok(false),
        _ => Err(io::Error::last_os_error()),
    }
}

fn wait_for_exit(
    mut exited: impl FnMut() -> io::Result<bool>,
    grace: Duration,
) -> io::Result<bool> {
    let started = Instant::now();
    loop {
        if exited()? {
            return Ok(true);
        }
        if started.elapsed() >= grace {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

struct Running {
    child: Child,
    stdout: Option<ChildStdout>,
    line: Vec<u8>,
    output: Arc<Mutex<Output>>,
    termination_requested: bool,
    _job: KillOnCloseJob,
}
impl Running {
    fn transferred_io(&self) -> Option<(u64, u64)> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Threading::{GetProcessIoCounters, IO_COUNTERS};
        let mut counters: IO_COUNTERS = unsafe { std::mem::zeroed() };
        (unsafe { GetProcessIoCounters(self.child.as_raw_handle() as _, &mut counters) } != 0)
            .then_some((counters.ReadTransferCount, counters.WriteTransferCount))
    }

    // A terminating SMB process can keep its pipe open indefinitely. Only read bytes already available.
    fn poll_output(&mut self) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::{Foundation::ERROR_BROKEN_PIPE, System::Pipes::PeekNamedPipe};
        let Some(stdout) = self.stdout.as_mut() else {
            return Ok(());
        };
        let mut buffer = [0_u8; 4096];
        for _ in 0..64 {
            let mut available = 0;
            if unsafe {
                PeekNamedPipe(
                    stdout.as_raw_handle() as _,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null_mut(),
                    &mut available,
                    std::ptr::null_mut(),
                )
            } == 0
            {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_BROKEN_PIPE as i32) {
                    return Err(error);
                }
                if !self.line.is_empty() {
                    consume_line(
                        &mut self.output.lock().expect("robocopy output lock"),
                        &oem_text(&self.line),
                    );
                    self.line.clear();
                }
                self.stdout = None;
                return Ok(());
            }
            if available == 0 {
                return Ok(());
            }
            let count = stdout.read(&mut buffer[..(available as usize).min(4096)])?;
            consume_bytes(
                &mut self.output.lock().expect("robocopy output lock"),
                &mut self.line,
                &buffer[..count],
            );
        }
        Ok(())
    }

    fn stop(&mut self) -> io::Result<bool> {
        use std::os::windows::io::AsRawHandle;
        if !self.termination_requested {
            if self.child.try_wait()?.is_none() {
                self.child.kill()?;
            }
            self.termination_requested = true;
        }
        // TerminateProcess stops user threads, but Windows may retain pending network I/O.
        // Never wait for that cleanup on the pause/retry controller.
        #[cfg(test)]
        let simulate_pending = TEST_PENDING_STOP.with(|slot| slot.replace(false));
        let exited = wait_for_exit(
            || {
                let exited = process_exited(self.child.as_raw_handle() as _)?;
                #[cfg(test)]
                if simulate_pending {
                    return Ok(false);
                }
                Ok(exited)
            },
            STOP_GRACE,
        )?;
        crate::operation_audit::record(
            "network-copy-stop",
            format!("pid={} exited={exited}", self.child.id()),
        );
        self.stdout = None;
        Ok(exited)
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if !self.termination_requested {
            let _ = self.child.kill();
        }
    }
}

struct Staging {
    destination: PathBuf,
    filename: OsString,
    path: PathBuf,
    attempt: u32,
    retired: bool,
}
impl Staging {
    fn new(source: &Path, destination: &Path) -> io::Result<Self> {
        let mut stage = Self {
            destination: destination.to_path_buf(),
            filename: source
                .file_name()
                .ok_or_else(|| io::Error::other("missing source filename"))?
                .to_owned(),
            path: destination.with_extension("stage-0"),
            attempt: 0,
            retired: false,
        };
        stage.create()?;
        Ok(stage)
    }
    fn file(&self) -> PathBuf {
        self.path.join(&self.filename)
    }
    fn create(&mut self) -> io::Result<()> {
        super::network::register_copy_staging(&self.destination, &self.file())?;
        fs::create_dir(&self.path)?;
        self.retired = false;
        Ok(())
    }
    fn renew(&mut self) -> io::Result<()> {
        if self.retired {
            self.attempt += 1;
            self.path = self
                .destination
                .with_extension(format!("stage-{}", self.attempt));
            self.create()?;
        }
        Ok(())
    }
    fn stop(&mut self, running: &mut Running, reason: &str) -> io::Result<()> {
        if !running.stop()? {
            self.retired = true;
            crate::operation_audit::record(
                "network-copy-retired",
                format!(
                    "pid={} reason={reason} staged={} termination_requested=true cleanup_pending=true",
                    running.child.id(),
                    self.file().display()
                ),
            );
            super::network::retire_copy_staging(&self.destination, &self.file())?;
        }
        Ok(())
    }
    fn cleanup(&self) -> io::Result<()> {
        if self.retired {
            return Ok(());
        }
        match fs::remove_file(self.file()) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
        }
        fs::remove_dir(&self.path)
    }
}

fn retry_delay(cancel: &CancellationToken, duration: Duration) -> Result<(), CopyFileError> {
    let mut remaining = duration;
    while !remaining.is_zero() {
        cancel.wait_if_paused();
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        let tick = remaining.min(Duration::from_millis(50));
        thread::sleep(tick);
        remaining = remaining.saturating_sub(tick);
    }
    Ok(())
}

fn query_retryable(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| retryable(code as u32))
        || matches!(
            error.kind(),
            io::ErrorKind::TimedOut
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::NotConnected
                | io::ErrorKind::Interrupted
                | io::ErrorKind::WouldBlock
        )
}

// Only read-only source/identity queries belong here. Pending SMB calls are contained by the helper.
pub(crate) fn copy_query<T: Send + 'static>(
    cancel: &CancellationToken,
    query: impl Fn() -> io::Result<T> + Send + Sync + 'static,
) -> io::Result<T> {
    copy_query_with(cancel, query, STALL_LIMIT, None)
}

fn copy_query_with<T: Send + 'static>(
    cancel: &CancellationToken,
    query: impl Fn() -> io::Result<T> + Send + Sync + 'static,
    timeout: Duration,
    retry_wait: Option<Duration>,
) -> io::Result<T> {
    let query = Arc::new(query);
    let mut attempt = 0_u32;
    loop {
        let slot = acquire_read_slot(cancel)?;
        let worker_query = query.clone();
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let worker = thread::Builder::new()
            .name("network-copy-query".to_owned())
            .spawn(move || {
                let _slot = slot;
                let result = worker_query();
                let _ = sender.send(result);
            })?;
        struct QueryWorker<T>(thread::JoinHandle<T>);
        impl<T> Drop for QueryWorker<T> {
            fn drop(&mut self) {
                cancel_read_worker(&self.0);
            }
        }
        let worker = QueryWorker(worker);
        let mut waiting = Duration::ZERO;
        let result = loop {
            cancel.wait_if_paused();
            if cancel.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            let tick = Instant::now();
            let result = receiver.recv_timeout(Duration::from_millis(20));
            waiting += tick.elapsed();
            match result {
                Ok(result) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(io::Error::other("source query worker stopped"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if waiting >= timeout => {
                    break Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "network source query did not respond",
                    ));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        };
        drop(worker);
        match result {
            Err(error) if query_retryable(&error) => {
                attempt = attempt.saturating_add(1);
                super::network::set_copy_recovering(true);
                crate::operation_audit::record(
                    "network-copy-query-retry",
                    format!("attempt={attempt} error={error}"),
                );
                retry_delay(cancel, retry_wait.unwrap_or_else(|| retry_backoff(attempt)))
                    .map_err(|error| error.error)?;
            }
            result => return result,
        }
    }
}

fn source_version(
    source: &Path,
    cancel: &CancellationToken,
) -> Result<(u64, SystemTime), CopyFileError> {
    let source = source.to_owned();
    copy_query(cancel, move || {
        let metadata = fs::metadata(&source)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "copy source is not a file",
            ));
        }
        Ok((metadata.len(), metadata.modified()?))
    })
    .map_err(|error| {
        if cancel.is_cancelled() {
            cancelled()
        } else {
            failed(error)
        }
    })
}

// Robocopy performs its own long-path handling and rejects verbatim UNC shares on some NAS.
fn robocopy_path(path: &Path) -> io::Result<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Component, Prefix};
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return Ok(path.to_path_buf());
    };
    let units: Vec<_> = path.as_os_str().encode_wide().collect();
    let offset = match prefix.kind() {
        Prefix::VerbatimUNC(_, _) => 8,
        Prefix::VerbatimDisk(_) => 4,
        _ => return Ok(path.to_path_buf()),
    };
    if path.components().any(|component| match component {
        Component::Normal(value) => matches!(value.encode_wide().last(), Some(32 | 46)),
        _ => false,
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Robocopy cannot safely address a verbatim path with a trailing dot or space",
        ));
    }
    let mut normal = if offset == 8 {
        vec![92, 92]
    } else {
        Vec::new()
    };
    normal.extend_from_slice(&units[offset..]);
    Ok(PathBuf::from(OsString::from_wide(&normal)))
}

fn spawn(
    source: &Path,
    staging: &Path,
    extra: &[OsString],
    shared: Arc<Mutex<Output>>,
) -> io::Result<Running> {
    let executable = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::other("SystemRoot is unavailable"))?
        .join("System32/robocopy.exe");
    let source = robocopy_path(source)?;
    let parent = source
        .parent()
        .ok_or_else(|| io::Error::other("copy source has no parent"))?;
    let name = source
        .file_name()
        .ok_or_else(|| io::Error::other("copy source has no filename"))?;
    let job = KillOnCloseJob::create()?;
    let mut child = Command::new(executable)
        .arg(robocopy_path(parent)?)
        .arg(robocopy_path(staging)?)
        .arg(name)
        .args([
            "/Z",
            "/COPY:DAT",
            "/NODCOPY",
            "/R:0",
            "/W:0",
            "/NJH",
            "/NJS",
            "/NDL",
            "/BYTES",
            "/NOOFFLOAD",
            "/COMPRESS",
        ])
        .args(extra)
        .creation_flags(0x08000000)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Err(error) = job.assign(&child) {
        let _ = child.kill();
        return Err(error);
    }
    let stdout = child.stdout.take();
    #[cfg(test)]
    TEST_SPAWNS.with(|slot| {
        if let Some(count) = slot.borrow().as_ref() {
            count.fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    });
    Ok(Running {
        child,
        stdout,
        line: Vec::new(),
        output: shared,
        termination_requested: false,
        _job: job,
    })
}

pub fn copy_file(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(u64),
) -> Result<u64, CopyFileError> {
    if crate::network::is_unc_path(source) {
        let parent = destination
            .parent()
            .ok_or_else(|| failed(io::Error::other("copy target has no parent")))?
            .to_owned();
        let local = copy_query(cancel, move || {
            use std::os::windows::ffi::OsStrExt;
            let resolved = fs::canonicalize(&parent)?;
            if crate::network::is_unc_path(&resolved) {
                return Ok(false);
            }
            let mut components = resolved.components();
            let Some(std::path::Component::Prefix(prefix)) = components.next() else {
                return Ok(false);
            };
            if !matches!(
                prefix.kind(),
                std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
            ) {
                return Ok(false);
            }
            let mut root = PathBuf::from(prefix.as_os_str());
            root.push(std::path::MAIN_SEPARATOR_STR);
            let wide: Vec<u16> = root.as_os_str().encode_wide().chain(Some(0)).collect();
            Ok(matches!(
                unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(wide.as_ptr()) },
                2 | 3
            ))
        })
        .map_err(failed)?;
        if local {
            return download::copy_file(source, destination, cancel, progress);
        }
    }
    copy_file_with(source, destination, cancel, progress, &[], STALL_LIMIT)
}

fn copy_file_with(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(u64),
    extra: &[OsString],
    stall_limit: Duration,
) -> Result<u64, CopyFileError> {
    super::network::set_copy_recovering(false);
    let mut staging = Staging::new(source, destination).map_err(failed)?;
    let outcome = copy_into_staging(
        source,
        destination,
        &mut staging,
        cancel,
        progress,
        extra,
        stall_limit,
    );
    super::network::set_copy_recovering(false);
    let cleanup = staging.cleanup();
    match (outcome, cleanup) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(error), Ok(())) => Err(error),
        (result, Err(cleanup)) => Err(CopyFileError {
            kind: result
                .as_ref()
                .err()
                .map_or(CopyFileErrorKind::Failed, |error| error.kind),
            error: io::Error::other(format!(
                "{}; staging cleanup failed at {}: {cleanup}",
                result.err().map_or_else(
                    || "copy completed".to_owned(),
                    |error| error.error.to_string()
                ),
                staging.path.display()
            )),
        }),
    }
}

fn copy_into_staging(
    source: &Path,
    destination: &Path,
    staging: &mut Staging,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(u64),
    extra: &[OsString],
    stall_limit: Duration,
) -> Result<u64, CopyFileError> {
    let expected = source_version(source, cancel)?;
    let mut high_water = 0_u64;
    let mut retries = 0_u32;
    loop {
        cancel.wait_if_paused();
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if source_version(source, cancel)? != expected {
            return Err(failed(io::Error::other(format!(
                "copy source changed: {}",
                source.display()
            ))));
        }
        staging.renew().map_err(failed)?;
        let shared = Arc::new(Mutex::new(Output::default()));
        let mut running = spawn(source, &staging.path, extra, shared.clone()).map_err(failed)?;
        let mut last_change = Instant::now();
        let mut last_io = running.transferred_io();
        let mut revision = 0;
        let mut paused = false;
        let mut stalled = false;
        let exit = loop {
            if cancel.is_cancelled() {
                staging.stop(&mut running, "cancel").map_err(failed)?;
                return Err(cancelled());
            }
            if cancel.is_paused() {
                staging.stop(&mut running, "pause").map_err(failed)?;
                paused = true;
                break None;
            }
            if let Err(error) = running.poll_output() {
                shared.lock().expect("robocopy output lock").read_error = Some(error.to_string());
            }
            {
                let output = shared.lock().expect("robocopy output lock");
                if output.revision != revision {
                    revision = output.revision;
                    last_change = Instant::now();
                    let count = ((u128::from(expected.0) * u128::from(output.percent.unwrap_or(0)))
                        / 10000) as u64;
                    let count = count.min(expected.0.saturating_sub(1));
                    if count >= high_water {
                        super::network::set_copy_recovering(false);
                    }
                    if count > high_water {
                        progress(count - high_water);
                        high_water = count;
                    }
                }
            }
            if let Some(status) = running.child.try_wait().map_err(failed)? {
                // The last network error may have arrived between the previous poll and exit.
                if let Err(error) = running.poll_output() {
                    shared.lock().expect("robocopy output lock").read_error =
                        Some(error.to_string());
                }
                running.stop().map_err(failed)?;
                break status.code();
            }
            let current_io = running.transferred_io();
            if current_io.is_some() && current_io != last_io {
                last_io = current_io;
                last_change = Instant::now();
            }
            if last_change.elapsed() >= stall_limit {
                staging.stop(&mut running, "stall").map_err(failed)?;
                stalled = true;
                break None;
            }
            thread::sleep(Duration::from_millis(20));
        };
        if paused {
            cancel.wait_if_paused();
            super::network::set_copy_recovering(true);
            continue;
        }
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if cancel.is_paused() {
            cancel.wait_if_paused();
            continue;
        }
        let output = shared.lock().expect("robocopy output lock");
        if exit.is_some_and(|code| (0..8).contains(&code)) && output.read_error.is_none() {
            if source_version(source, cancel)? != expected
                || fs::metadata(staging.file()).map_err(failed)?.len() != expected.0
            {
                return Err(failed(io::Error::other(
                    "source or copied file size changed before publication",
                )));
            }
            if destination.exists() {
                return Err(failed(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "copy destination already exists",
                )));
            }
            fs::rename(staging.file(), destination).map_err(failed)?;
            if expected.0 > high_water {
                progress(expected.0 - high_water);
            }
            return Ok(expected.0);
        }
        let recoverable = stalled
            || (!output.errors.is_empty() && output.errors.iter().all(|code| retryable(*code)));
        let detail = format!(
            "robocopy failed from {} to {}: exit={exit:?}, stalled={stalled}, errors={:?}, output={}, reader={:?}",
            source.display(),
            destination.display(),
            output.errors,
            output.tail,
            output.read_error
        );
        drop(output);
        if !recoverable || retries >= MAX_RETRIES {
            return Err(failed(io::Error::other(detail)));
        }
        retries += 1;
        super::network::set_copy_recovering(true);
        crate::operation_audit::record(
            "network-copy-retry",
            format!(
                "source={} destination={} attempt={retries} bytes={high_water} stalled={stalled} {detail}",
                source.display(),
                destination.display()
            ),
        );
        retry_delay(
            cancel,
            Duration::from_secs((1_u64 << retries.min(4)).min(15)),
        )?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn issue_137_read_only_query_retries_network_error_and_timeout() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let result = copy_query_with(
            &CancellationToken::new(),
            move || match worker_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0..=15 => Err(io::Error::from_raw_os_error(53)),
                16 => Err(io::ErrorKind::TimedOut.into()),
                _ => Ok(42),
            },
            Duration::from_millis(20),
            Some(Duration::ZERO),
        )
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 18);
    }

    #[test]
    fn issue_137_blocked_read_only_query_can_pause_and_cancel_without_join() {
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (started_sender, started_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let release_receiver = Mutex::new(release_receiver);
        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let worker = thread::spawn(move || {
            copy_query(&worker_cancel, move || {
                started_sender.send(()).unwrap();
                release_receiver.lock().unwrap().recv().unwrap();
                finished_sender.send(()).unwrap();
                Ok(42)
            })
        });
        started_receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        cancel.pause();
        let started = Instant::now();
        while !cancel.is_pause_acknowledged() && started.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        let acknowledged = cancel.is_pause_acknowledged();
        cancel.cancel();
        let error = worker.join().unwrap().unwrap_err();
        release_sender.send(()).unwrap();
        finished_receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(acknowledged);
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn issue_137_pending_kernel_exit_does_not_block_controller() {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{CreateEventW, OpenProcess, SYNCHRONIZATION_SYNCHRONIZE},
        };
        let handle = if let Ok(pid) = std::env::var("ASTERFILES_TERMINATING_PROCESS_PROBE") {
            unsafe { OpenProcess(SYNCHRONIZATION_SYNCHRONIZE, 0, pid.parse().unwrap()) }
        } else {
            unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) }
        };
        assert!(!handle.is_null());
        let started = Instant::now();
        let exited = wait_for_exit(|| process_exited(handle), STOP_GRACE).unwrap();
        unsafe {
            CloseHandle(handle);
        }
        assert!(
            !exited,
            "fixture must remain unsignaled after termination was requested"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn issue_137_pending_stop_pause_acknowledges_and_resumes_in_fresh_staging() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        let content = vec![47; 16 * 1024 * 1024];
        fs::write(&source, &content).unwrap();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let target = destination.clone();
        let worker = thread::spawn(move || {
            TEST_PENDING_STOP.with(|slot| slot.set(true));
            let mut requested = false;
            let mut bytes = 0;
            let result = copy_file_with(
                &source,
                &target,
                &worker_cancel,
                &mut |count| {
                    bytes += count;
                    if !requested {
                        requested = true;
                        worker_cancel.pause();
                    }
                },
                &[OsString::from("/IORATE:4m")],
                STALL_LIMIT,
            );
            (result, bytes)
        });
        let started = Instant::now();
        while !cancel.is_pause_acknowledged() && started.elapsed() < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(10));
        }
        let acknowledged = cancel.is_pause_acknowledged();
        let retired = destination.with_extension("stage-0").join("source.bin");
        let before = if acknowledged {
            fs::read(&retired).unwrap()
        } else {
            Vec::new()
        };
        cancel.resume();
        let (result, bytes) = worker.join().unwrap();
        assert!(acknowledged);
        result.unwrap();
        assert_eq!(bytes, content.len() as u64);
        assert_eq!(fs::read(&destination).unwrap(), content);
        assert_eq!(
            fs::read(&retired).unwrap(),
            before,
            "retired output must never be reused"
        );
        assert!(!destination.with_extension("stage-1").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_robocopy_path_keeps_raw_identity_and_rejects_ambiguous_names() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let mut units: Vec<u16> = r"\\?\UNC\nas\share\".encode_utf16().collect();
        units.push(0xd800);
        let input = PathBuf::from(OsString::from_wide(&units));
        let mut expected: Vec<u16> = r"\\nas\share\".encode_utf16().collect();
        expected.push(0xd800);
        assert_eq!(
            robocopy_path(&input)
                .unwrap()
                .as_os_str()
                .encode_wide()
                .collect::<Vec<_>>(),
            expected
        );
        assert!(robocopy_path(Path::new(r"\\?\UNC\nas\share\folder.")).is_err());
    }

    #[test]
    fn issue_137_robocopy_percent_and_error_parser_is_bounded() {
        assert_eq!(parse_percent(" 12.3%\r"), Some(1230));
        assert_eq!(parse_percent("100%"), Some(10000));
        assert_eq!(parse_percent("filename 12%"), None);
        assert_eq!(parse_percent("NaN%"), None);
        let mut output = Output::default();
        for _ in 0..1000 {
            consume_line(&mut output, "error 5 (0x00000005) access denied");
        }
        assert_eq!(output.errors, [5]);
        assert!(output.tail.len() <= 8192);
        consume_line(&mut output, " 50%");
        consume_line(&mut output, " 50%");
        assert_eq!(output.revision, 1);
    }

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "asterfiles-robocopy-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn issue_137_robocopy_preserves_special_filename_metadata_and_ads() {
        let root = fixture();
        let source = root.join("源 & test (1).bin");
        let destination = root.join("copy");
        fs::write(&source, vec![7; 1024 * 1024]).unwrap();
        fs::write(
            format!("{}:asterfiles", source.display()),
            b"alternate stream",
        )
        .unwrap();
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        let mut bytes = 0;
        assert_eq!(
            copy_file(
                &source,
                &destination,
                &CancellationToken::new(),
                &mut |count| bytes += count
            )
            .unwrap(),
            1024 * 1024
        );
        assert_eq!(bytes, 1024 * 1024);
        assert_eq!(fs::read(&source).unwrap(), fs::read(&destination).unwrap());
        assert_eq!(
            fs::metadata(&destination).unwrap().modified().unwrap(),
            modified
        );
        assert_eq!(
            fs::read(format!("{}:asterfiles", destination.display())).unwrap(),
            b"alternate stream"
        );
        assert!(!destination.with_extension("stage-0").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_robocopy_cancel_removes_only_its_owned_staging() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        fs::write(&source, vec![31; 16 * 1024 * 1024]).unwrap();
        fs::write(root.join("keep.bin"), b"keep").unwrap();
        let cancel = CancellationToken::new();
        let result = copy_file_with(
            &source,
            &destination,
            &cancel,
            &mut |_| cancel.cancel(),
            &[OsString::from("/IORATE:4m")],
            STALL_LIMIT,
        );
        assert_eq!(result.unwrap_err().kind, CopyFileErrorKind::Cancelled);
        assert!(!destination.exists());
        assert!(!destination.with_extension("stage-0").exists());
        assert_eq!(fs::read(root.join("keep.bin")).unwrap(), b"keep");
        assert_eq!(fs::metadata(&source).unwrap().len(), 16 * 1024 * 1024);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_robocopy_scheduled_wait_pauses_without_any_progress() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        fs::write(&source, vec![19; 1024 * 1024]).unwrap();
        let schedule = Command::new("pwsh").args(["-NoLogo", "-NoProfile", "-Command",
            "$start=(Get-Date).AddHours(3); '/RH:'+$start.ToString('HHmm')+'-'+$start.AddMinutes(1).ToString('HHmm')"])
            .creation_flags(0x08000000).output().unwrap();
        assert!(schedule.status.success());
        let schedule = OsString::from(String::from_utf8(schedule.stdout).unwrap().trim());
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let worker_bytes = bytes.clone();
        let target = destination.clone();
        let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_spawns = spawns.clone();
        let worker = thread::spawn(move || {
            TEST_SPAWNS.with(|slot| {
                slot.replace(Some(worker_spawns));
            });
            copy_file_with(
                &source,
                &target,
                &worker_cancel,
                &mut |count| {
                    worker_bytes.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                },
                &[schedule],
                STALL_LIMIT,
            )
        });
        let started = Instant::now();
        while spawns.load(std::sync::atomic::Ordering::Acquire) == 0
            && started.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let spawned = spawns.load(std::sync::atomic::Ordering::Acquire) == 1;
        cancel.pause();
        let start = Instant::now();
        while !cancel.is_pause_acknowledged() && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        let acknowledged = cancel.is_pause_acknowledged();
        thread::sleep(Duration::from_millis(100));
        let observed_bytes = bytes.load(std::sync::atomic::Ordering::Relaxed);
        cancel.cancel();
        let result = worker.join().unwrap();
        assert!(spawned);
        assert!(acknowledged);
        assert_eq!(observed_bytes, 0);
        assert_eq!(result.unwrap_err().kind, CopyFileErrorKind::Cancelled);
        assert!(!destination.exists());
        assert!(!destination.with_extension("stage-0").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_robocopy_no_progress_restarts_and_remains_cancellable() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        fs::write(&source, vec![11; 1024 * 1024]).unwrap();
        let schedule = Command::new("pwsh").args(["-NoLogo", "-NoProfile", "-Command",
            "$start=(Get-Date).AddHours(3); '/RH:'+$start.ToString('HHmm')+'-'+$start.AddMinutes(1).ToString('HHmm')"])
            .creation_flags(0x08000000).output().unwrap();
        assert!(schedule.status.success());
        let schedule = OsString::from(String::from_utf8(schedule.stdout).unwrap().trim());
        let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_spawns = spawns.clone();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let target = destination.clone();
        let worker = thread::spawn(move || {
            TEST_PENDING_STOP.with(|slot| slot.set(true));
            TEST_SPAWNS.with(|slot| {
                slot.replace(Some(worker_spawns));
            });
            copy_file_with(
                &source,
                &target,
                &worker_cancel,
                &mut |_| {},
                &[schedule],
                Duration::from_millis(50),
            )
        });
        let start = Instant::now();
        while spawns.load(std::sync::atomic::Ordering::Acquire) < 2
            && start.elapsed() < Duration::from_secs(10)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let attempts = spawns.load(std::sync::atomic::Ordering::Acquire);
        cancel.cancel();
        let requested = Instant::now();
        let result = worker.join().unwrap();
        assert!(
            attempts >= 2,
            "a stalled child must be replaced before cancellation"
        );
        assert!(requested.elapsed() < Duration::from_secs(3));
        assert_eq!(result.unwrap_err().kind, CopyFileErrorKind::Cancelled);
        assert!(!destination.exists());
        assert!(
            destination.with_extension("stage-0").exists(),
            "retiring attempt must be isolated"
        );
        assert!(!destination.with_extension("stage-1").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_robocopy_permanent_failure_does_not_retry() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        fs::write(&source, b"source").unwrap();
        let start = Instant::now();
        let result = copy_file_with(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |_| {},
            &[OsString::from("/ASTERFILES_INVALID_TEST_SWITCH")],
            STALL_LIMIT,
        );
        let error = result.unwrap_err();
        assert_eq!(error.kind, CopyFileErrorKind::Failed);
        assert!(start.elapsed() < Duration::from_secs(3), "{}", error.error);
        assert!(!destination.exists());
        assert!(!destination.with_extension("stage-0").exists());
        assert!(!retryable(5));
        assert!(!retryable(112));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_robocopy_changed_source_is_rejected_after_pause() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        fs::write(&source, vec![29; 16 * 1024 * 1024]).unwrap();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let target = destination.clone();
        let worker_source = source.clone();
        let worker = thread::spawn(move || {
            let mut requested = false;
            copy_file_with(
                &worker_source,
                &target,
                &worker_cancel,
                &mut |_| {
                    if !requested {
                        worker_cancel.pause();
                        requested = true;
                    }
                },
                &[OsString::from("/IORATE:4m")],
                STALL_LIMIT,
            )
        });
        let start = Instant::now();
        while !cancel.is_pause_acknowledged() && start.elapsed() < Duration::from_secs(20) {
            thread::sleep(Duration::from_millis(10));
        }
        let acknowledged = cancel.is_pause_acknowledged();
        if acknowledged {
            fs::write(&source, b"new version").unwrap();
            cancel.resume();
        } else {
            cancel.cancel();
        }
        let error = worker.join().unwrap().unwrap_err();
        assert!(acknowledged);
        assert_eq!(error.kind, CopyFileErrorKind::Failed);
        assert!(error.error.to_string().contains("source changed"));
        assert!(!destination.exists());
        assert!(!destination.with_extension("stage-0").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_blocked_child_without_output_can_be_reaped_before_ack() {
        let job = KillOnCloseJob::create().unwrap();
        let mut child = Command::new("pwsh")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        job.assign(&child).unwrap();
        let stdout = child.stdout.take();
        let output = Arc::new(Mutex::new(Output::default()));
        let mut running = Running {
            child,
            stdout,
            line: Vec::new(),
            output: output.clone(),
            termination_requested: false,
            _job: job,
        };
        let cancel = CancellationToken::new();
        cancel.pause();
        assert!(!cancel.is_pause_acknowledged());
        let started = Instant::now();
        running.stop().unwrap();
        cancel.acknowledge_pause();
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(running.child.try_wait().unwrap().is_some());
        assert!(running.stdout.is_none());
        assert_eq!(output.lock().unwrap().revision, 0);
        assert!(cancel.is_pause_acknowledged());
        cancel.resume();
    }

    #[test]
    fn issue_137_robocopy_pause_reaps_process_and_resume_preserves_content() {
        let root = fixture();
        let source = root.join("source.bin");
        let destination = root.join("copy");
        let content = vec![23; 16 * 1024 * 1024];
        fs::write(&source, &content).unwrap();
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let (sender, receiver) = std::sync::mpsc::channel();
        let target = destination.clone();
        let worker = thread::spawn(move || {
            let mut announced = false;
            copy_file_with(
                &source,
                &target,
                &worker_cancel,
                &mut |_| {
                    if !announced {
                        sender.send(()).unwrap();
                        announced = true;
                    }
                },
                &[OsString::from("/IORATE:4m")],
                STALL_LIMIT,
            )
        });
        receiver.recv_timeout(Duration::from_secs(20)).unwrap();
        cancel.pause();
        let start = Instant::now();
        while !cancel.is_pause_acknowledged() && start.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        let acknowledged = cancel.is_pause_acknowledged();
        thread::sleep(Duration::from_millis(200));
        cancel.resume();
        let result = worker.join().unwrap();
        assert!(acknowledged);
        result.unwrap();
        assert_eq!(fs::read(&destination).unwrap(), content);
        fs::remove_dir_all(root).unwrap();
    }
}
