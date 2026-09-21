use std::{
    ffi::OsString,
    io,
    os::windows::{
        ffi::{OsStrExt, OsStringExt},
        process::CommandExt,
    },
    path::PathBuf,
    process::Command,
    ptr,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_BROKEN_PIPE, ERROR_FILE_NOT_FOUND,
        ERROR_MORE_DATA, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED,
        ERROR_PIPE_NOT_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_READ,
        FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, ReadFile, WriteFile,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
            PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
            PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, PeekNamedPipe, WaitNamedPipeW,
        },
        Threading::{CREATE_NO_WINDOW, CreateMutexW},
    },
};

use crate::platform::windows::address_path::ExternalLaunchPath;

const INSTANCE_NAME: &str = "Local\\AsterFiles.SingleInstance.v1";
const PIPE_NAME: &str = r"\\.\pipe\AsterFiles.ExternalPaths.v1";
const MAX_PATHS: usize = 1_024;
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const IPC_MAGIC: u32 = 0x3250_4641;
const OPEN_MESSAGE: u32 = 1;
const SELECTION_MESSAGE: u32 = 2;
const ACK_MESSAGE: u32 = 3;
const ACK_BYTES: usize = 12;
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(5);
const ACK_TIMEOUT: Duration = Duration::from_secs(5);

pub enum InstanceOutcome {
    Primary(PrimaryInstance),
    Forwarded,
}

pub struct ExternalOpenRequest {
    pub paths: Vec<ExternalLaunchPath>,
    pub selection: Option<mpsc::Receiver<Option<PathBuf>>>,
}

pub struct PrimaryInstance {
    _mutex: OwnedHandle,
    receiver: Option<mpsc::Receiver<ExternalOpenRequest>>,
}

impl PrimaryInstance {
    pub fn take_receiver(&mut self) -> mpsc::Receiver<ExternalOpenRequest> {
        self.receiver
            .take()
            .expect("primary instance receiver can only be taken once")
    }
}

pub fn claim_primary() -> io::Result<Option<PrimaryInstance>> {
    let name = wide(INSTANCE_NAME);
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() {
        return Err(io::Error::last_os_error());
    }
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    let mutex = OwnedHandle(mutex);
    if already_exists {
        return Ok(None);
    }
    // Publish a connectable endpoint before callers can treat this process as ready.
    let pipe = create_pipe(PIPE_NAME, true)?;
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("external-path-ipc".to_owned())
        .spawn(move || listen(PIPE_NAME, pipe, sender))?;
    Ok(Some(PrimaryInstance {
        _mutex: mutex,
        receiver: Some(receiver),
    }))
}

pub fn coordinate(paths: &[ExternalLaunchPath]) -> io::Result<InstanceOutcome> {
    if let Some(primary) = claim_primary()? {
        return Ok(InstanceOutcome::Primary(primary));
    }
    let bytes = encode_open(paths, false)?;
    let pipe = connect_with_start(PIPE_NAME, || Ok(()))?;
    deliver_messages(pipe, &bytes, paths.len(), None, grant_foreground_permission)?;
    Ok(InstanceOutcome::Forwarded)
}

pub fn deliver_shell_open(
    paths: Vec<ExternalLaunchPath>,
    selection: Option<mpsc::Receiver<Option<PathBuf>>>,
) -> io::Result<()> {
    let bytes = encode_open(&paths, selection.is_some())?;
    let pipe = connect_with_start(PIPE_NAME, || {
        Command::new(std::env::current_exe()?)
            .arg("--shell-ui")
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
        Ok(())
    })?;
    deliver_messages(
        pipe,
        &bytes,
        paths.len(),
        selection,
        grant_foreground_permission,
    )
}

fn listen(pipe_name: &str, pipe: OwnedHandle, sender: mpsc::Sender<ExternalOpenRequest>) {
    listen_connections(pipe_name, pipe, sender, std::iter::repeat(()));
}

fn listen_connections(
    pipe_name: &str,
    mut pipe: OwnedHandle,
    sender: mpsc::Sender<ExternalOpenRequest>,
    connections: impl IntoIterator<Item = ()>,
) {
    for () in connections {
        let accepted = accept_pipe(&pipe);
        // Keep the endpoint present even if a client disconnects before ConnectNamedPipe.
        // Each connection owns its own lifetime; one failed request cannot stop listening.
        let next = match create_pipe(pipe_name, false) {
            Ok(next) => next,
            Err(error) => {
                record_ipc_error("create-listener", &error);
                return;
            }
        };
        let connected = pipe;
        pipe = next;
        if let Err(error) = accepted {
            record_ipc_error("connect", &error);
            continue;
        }
        let sender = sender.clone();
        if let Err(error) = thread::Builder::new()
            .name("external-path-request".to_owned())
            .spawn(move || {
                if let Err(error) = receive_request(connected, &sender) {
                    record_ipc_error("receive", &error);
                }
            })
        {
            record_ipc_error("request-thread", &error);
        }
    }
}

fn record_ipc_error(stage: &str, error: &io::Error) {
    crate::operation_audit::record(
        "external-open-ipc-error",
        format!("stage={stage} error={error}"),
    );
}

fn accept_pipe(pipe: &OwnedHandle) -> io::Result<()> {
    if unsafe { ConnectNamedPipe(pipe.0, ptr::null_mut()) } != 0
        || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn receive_request(
    pipe: OwnedHandle,
    sender: &mpsc::Sender<ExternalOpenRequest>,
) -> io::Result<()> {
    let pipe = ConnectedPipe(pipe);
    let (paths, has_selection) = decode_open(&read_message(pipe.0.0)?)?;
    let (selection_sender, selection) = if has_selection {
        let (sender, receiver) = mpsc::channel();
        (Some(sender), Some(receiver))
    } else {
        (None, None)
    };
    sender
        .send(ExternalOpenRequest { paths, selection })
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "application receiver closed"))?;
    write_ack(pipe.0.0, OPEN_MESSAGE)?;
    if let Some(sender) = selection_sender {
        let result = read_message(pipe.0.0).and_then(|bytes| decode_selection(&bytes));
        match result {
            Ok(selected) => {
                sender.send(selected).map_err(|_| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "selection receiver closed")
                })?;
                write_ack(pipe.0.0, SELECTION_MESSAGE)?;
            }
            Err(error) => {
                // Release the request's pending reveal even if the launcher was terminated.
                let _ = sender.send(None);
                return Err(error);
            }
        }
    }
    // DisconnectNamedPipe discards unread data, including the final acknowledgement.
    // The client closes only after reading it, so keep the endpoint alive until then.
    wait_for_client_close(pipe.0.0, ACK_TIMEOUT)
}

fn create_pipe(pipe_name: &str, first_instance: bool) -> io::Result<OwnedHandle> {
    let name = wide(pipe_name);
    let first = if first_instance {
        FILE_FLAG_FIRST_PIPE_INSTANCE
    } else {
        0
    };
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_DUPLEX | first,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            ACK_BYTES as u32,
            MAX_MESSAGE_BYTES as u32,
            0,
            ptr::null(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(handle))
    }
}

fn open_pipe(pipe_name: &str) -> io::Result<OwnedHandle> {
    let name = wide(pipe_name);
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            FILE_GENERIC_READ | FILE_GENERIC_WRITE,
            0,
            ptr::null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(OwnedHandle(handle))
    }
}

fn connect_with_start(
    pipe_name: &str,
    start: impl FnOnce() -> io::Result<()>,
) -> io::Result<OwnedHandle> {
    let deadline = Instant::now() + CONNECTION_TIMEOUT;
    let mut start = Some(start);
    let name = wide(pipe_name);
    loop {
        let error = match open_pipe(pipe_name) {
            Ok(pipe) => return Ok(pipe),
            Err(error) => error,
        };
        match error.raw_os_error().map(|value| value as u32) {
            Some(ERROR_FILE_NOT_FOUND) => {
                if let Some(start) = start.take() {
                    start()?;
                }
            }
            Some(ERROR_PIPE_BUSY) => {}
            _ => return Err(error),
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("AsterFiles IPC was not ready within 5 seconds: {error}"),
            ));
        }
        let interval = remaining.min(Duration::from_millis(50));
        if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) {
            let _ = unsafe { WaitNamedPipeW(name.as_ptr(), interval.as_millis() as u32) };
        } else {
            thread::sleep(interval);
        }
    }
}

fn pipe_server_process_id(handle: HANDLE) -> io::Result<u32> {
    let mut process_id = 0;
    if unsafe { GetNamedPipeServerProcessId(handle, &mut process_id) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if process_id == 0 || process_id == u32::MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid pipe server PID",
        ));
    }
    Ok(process_id)
}

fn grant_foreground_permission(handle: HANDLE) -> io::Result<u32> {
    use windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow;

    let process_id = pipe_server_process_id(handle)?;
    if unsafe { AllowSetForegroundWindow(process_id) } == 0 {
        return Err(io::Error::other(format!(
            "server_pid={process_id} permission denied: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(process_id)
}

fn deliver_messages(
    pipe: OwnedHandle,
    first: &[u8],
    path_count: usize,
    selection: Option<mpsc::Receiver<Option<PathBuf>>>,
    mut authorize: impl FnMut(HANDLE) -> io::Result<u32>,
) -> io::Result<()> {
    let publish =
        |bytes: &[u8], kind: u32, authorize: &mut dyn FnMut(HANDLE) -> io::Result<u32>| {
            // Both activations may be handled immediately after the write. A denied focus
            // grant must never discard a path or cause a second application window.
            let permission = authorize(pipe.0);
            crate::operation_audit::record(
                "external-open-foreground-permission",
                format!("result={permission:?}"),
            );
            write_message(pipe.0, bytes)?;
            wait_for_ack(pipe.0, kind, ACK_TIMEOUT)
        };
    publish(first, OPEN_MESSAGE, &mut authorize)?;
    crate::operation_audit::record(
        "external-open-forwarded",
        format!("path_count={path_count} followup={}", selection.is_some()),
    );
    if let Some(selection) = selection {
        let selected = selection.recv().unwrap_or(None);
        publish(
            &encode_selection(selected.as_ref())?,
            SELECTION_MESSAGE,
            &mut authorize,
        )?;
        crate::operation_audit::record(
            "external-open-selection-forwarded",
            format!("selected={}", selected.is_some()),
        );
    }
    Ok(())
}

fn acknowledgement(kind: u32) -> [u8; ACK_BYTES] {
    let mut bytes = [0; ACK_BYTES];
    for (chunk, word) in bytes
        .chunks_exact_mut(4)
        .zip([IPC_MAGIC, ACK_MESSAGE, kind])
    {
        chunk.copy_from_slice(&word.to_le_bytes());
    }
    bytes
}

fn write_ack(handle: HANDLE, kind: u32) -> io::Result<()> {
    write_message(handle, &acknowledgement(kind))
}

fn available_bytes(handle: HANDLE) -> io::Result<u32> {
    let mut available = 0;
    if unsafe {
        PeekNamedPipe(
            handle,
            ptr::null_mut(),
            0,
            ptr::null_mut(),
            &mut available,
            ptr::null_mut(),
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

fn wait_for_ack(handle: HANDLE, kind: u32, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if available_bytes(handle)? >= ACK_BYTES as u32 {
            let bytes = read_message(handle)?;
            return if bytes == acknowledgement(kind) {
                Ok(())
            } else {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid IPC acknowledgement",
                ))
            };
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC acknowledgement timed out",
            ));
        }
        thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

fn wait_for_client_close(handle: HANDLE, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match available_bytes(handle) {
            Err(error)
                if matches!(
                    error.raw_os_error().map(|value| value as u32),
                    Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
                ) =>
            {
                return Ok(());
            }
            Err(error) => return Err(error),
            Ok(0) => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected IPC data after acknowledgement",
                ));
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "IPC client did not close after acknowledgement",
            ));
        }
        thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

fn write_message(handle: HANDLE, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0;
    if unsafe {
        WriteFile(
            handle,
            bytes.as_ptr(),
            bytes.len() as u32,
            &mut written,
            ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if written as usize != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "partial IPC write",
        ));
    }
    Ok(())
}

fn read_message(handle: HANDLE) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let mut read = 0;
        let succeeded = unsafe {
            ReadFile(
                handle,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                &mut read,
                ptr::null_mut(),
            )
        };
        bytes.extend_from_slice(&buffer[..read as usize]);
        if succeeded == 0 && unsafe { GetLastError() } != ERROR_MORE_DATA {
            return Err(io::Error::last_os_error());
        }
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC message too large",
            ));
        }
        if succeeded != 0 {
            return Ok(bytes);
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty partial IPC message",
            ));
        }
    }
}

fn encode_open(paths: &[ExternalLaunchPath], has_selection: bool) -> io::Result<Vec<u8>> {
    if paths.len() > MAX_PATHS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many paths",
        ));
    }
    let mut bytes = Vec::new();
    for word in [
        IPC_MAGIC,
        OPEN_MESSAGE,
        u32::from(has_selection),
        paths.len() as u32,
    ] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    for launch in paths {
        bytes.extend_from_slice(&u32::from(launch.select).to_le_bytes());
        encode_path(&mut bytes, &launch.path)?;
    }
    Ok(bytes)
}

fn encode_selection(path: Option<&PathBuf>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for word in [IPC_MAGIC, SELECTION_MESSAGE, u32::from(path.is_some())] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    if let Some(path) = path {
        encode_path(&mut bytes, path)?;
    }
    Ok(bytes)
}

fn encode_path(bytes: &mut Vec<u8>, path: &std::path::Path) -> io::Result<()> {
    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if units.len() > u32::MAX as usize
        || bytes
            .len()
            .saturating_add(4)
            .saturating_add(units.len().saturating_mul(2))
            > MAX_MESSAGE_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path message too large",
        ));
    }
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

fn decode_open(bytes: &[u8]) -> io::Result<(Vec<ExternalLaunchPath>, bool)> {
    let mut offset = validate_header(bytes, OPEN_MESSAGE)?;
    let has_selection = read_bool(bytes, &mut offset)?;
    let count = read_u32(bytes, &mut offset)? as usize;
    if count > MAX_PATHS {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too many paths"));
    }
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let select = read_bool(bytes, &mut offset)?;
        let path = decode_path(bytes, &mut offset)?;
        paths.push(ExternalLaunchPath { path, select });
    }
    validate_end(bytes, offset)?;
    Ok((paths, has_selection))
}

fn decode_selection(bytes: &[u8]) -> io::Result<Option<PathBuf>> {
    let mut offset = validate_header(bytes, SELECTION_MESSAGE)?;
    let path = if read_bool(bytes, &mut offset)? {
        Some(decode_path(bytes, &mut offset)?)
    } else {
        None
    };
    validate_end(bytes, offset)?;
    Ok(path)
}

fn validate_header(bytes: &[u8], expected_kind: u32) -> io::Result<usize> {
    let mut offset = 0;
    if bytes.len() > MAX_MESSAGE_BYTES
        || read_u32(bytes, &mut offset)? != IPC_MAGIC
        || read_u32(bytes, &mut offset)? != expected_kind
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC message",
        ));
    }
    Ok(offset)
}

fn validate_end(bytes: &[u8], offset: usize) -> io::Result<()> {
    if offset != bytes.len() {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing IPC data",
        ))
    } else {
        Ok(())
    }
}

fn decode_path(bytes: &[u8], offset: &mut usize) -> io::Result<PathBuf> {
    let length = read_u32(bytes, offset)? as usize;
    let end = offset.saturating_add(length.saturating_mul(2));
    let raw = bytes
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid path length"))?;
    let units = raw
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    *offset = end;
    Ok(PathBuf::from(OsString::from_wide(&units)))
}

fn read_bool(bytes: &[u8], offset: &mut usize) -> io::Result<bool> {
    match read_u32(bytes, offset)? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid IPC flag",
        )),
    }
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> io::Result<u32> {
    let end = offset.saturating_add(4);
    let raw = bytes
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated IPC message"))?;
    *offset = end;
    Ok(u32::from_le_bytes(raw.try_into().unwrap()))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

struct OwnedHandle(HANDLE);
unsafe impl Send for OwnedHandle {}
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

struct ConnectedPipe(OwnedHandle);
impl Drop for ConnectedPipe {
    fn drop(&mut self) {
        unsafe { DisconnectNamedPipe(self.0.0) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    };

    fn launch(path: &str, select: bool) -> ExternalLaunchPath {
        ExternalLaunchPath {
            path: PathBuf::from(path),
            select,
        }
    }

    fn test_pipe_name() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        format!(
            r"\\.\pipe\AsterFiles.ExternalPaths.Test.{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn finite_server(
        count: usize,
    ) -> (
        String,
        mpsc::Receiver<ExternalOpenRequest>,
        thread::JoinHandle<Vec<io::Result<()>>>,
    ) {
        let name = test_pipe_name();
        let first = create_pipe(&name, true).expect("create isolated server");
        let server_name = name.clone();
        let (sender, receiver) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut pipe = first;
            let mut workers = Vec::new();
            for _ in 0..count {
                accept_pipe(&pipe).expect("accept isolated request");
                let next = create_pipe(&server_name, false).expect("keep next endpoint ready");
                let connected = pipe;
                pipe = next;
                let sender = sender.clone();
                workers.push(thread::spawn(move || receive_request(connected, &sender)));
            }
            let results = workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .collect();
            drop(pipe);
            results
        });
        (name, receiver, server)
    }

    fn test_client(name: &str) -> OwnedHandle {
        connect_with_start(name, || panic!("isolated test server must already exist")).unwrap()
    }

    fn send_test_packet(handle: HANDLE, bytes: &[u8]) -> io::Result<()> {
        let mut offset = 4;
        let kind = read_u32(bytes, &mut offset)?;
        write_message(handle, bytes)?;
        wait_for_ack(handle, kind, Duration::from_secs(2))
    }

    #[test]
    fn path_payload_preserves_unicode_spaces_multiple_paths_and_raw_utf16() {
        let paths = vec![
            launch(r"C:\Folder With Spaces", false),
            launch(r"C:\中文\深层目录", true),
            launch(r"\\server\share\folder", false),
            ExternalLaunchPath {
                path: PathBuf::from(OsString::from_wide(&[
                    b'C' as u16,
                    b':' as u16,
                    b'\\' as u16,
                    0xd800,
                ])),
                select: true,
            },
        ];
        for followup in [false, true] {
            assert_eq!(
                decode_open(&encode_open(&paths, followup).unwrap()).unwrap(),
                (paths.clone(), followup)
            );
        }
        let target = paths.last().unwrap().path.clone();
        assert_eq!(
            decode_selection(&encode_selection(Some(&target)).unwrap()).unwrap(),
            Some(target)
        );
        assert_eq!(
            decode_selection(&encode_selection(None).unwrap()).unwrap(),
            None
        );
    }

    #[test]
    fn empty_payload_represents_a_plain_win_e_launch() {
        assert_eq!(
            decode_open(&encode_open(&[], false).unwrap()).unwrap(),
            (Vec::new(), false)
        );
    }

    #[test]
    fn malformed_and_obsolete_payloads_are_rejected() {
        assert!(decode_open(&[1, 0, 0, 0, 5, 0, 0, 0, 1, 0]).is_err());
        let mut bytes = encode_open(&[launch(r"C:\Folder", false)], false).unwrap();
        bytes[0..4].copy_from_slice(&0x3150_4641u32.to_le_bytes());
        assert!(decode_open(&bytes).is_err());
        let mut bytes = encode_open(&[], false).unwrap();
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(decode_open(&bytes).is_err());
        assert!(decode_selection(&encode_open(&[], false).unwrap()).is_err());
        let mut bytes = encode_selection(None).unwrap();
        bytes.push(0);
        assert!(decode_selection(&bytes).is_err());
    }

    #[test]
    fn named_pipe_transfers_a_message_larger_than_the_read_buffer() {
        let (name, receiver, server) = finite_server(1);
        let expected = (0..128)
            .map(|index| launch(&format!(r"C:\路径 {index}\{}", "x".repeat(64)), false))
            .collect::<Vec<_>>();
        let bytes = encode_open(&expected, false).unwrap();
        let client = test_client(&name);
        send_test_packet(client.0, &bytes).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap().paths,
            expected
        );
        drop(client);
        assert!(
            server
                .join()
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
    }

    #[test]
    fn issue_122_first_packet_opens_folder_without_waiting_for_selection() {
        let (name, receiver, server) = finite_server(1);
        let (selection_sender, selection) = mpsc::channel();
        let client = thread::spawn(move || {
            deliver_messages(
                test_client(&name),
                &encode_open(&[launch(r"C:\Folder", false)], true).unwrap(),
                1,
                Some(selection),
                pipe_server_process_id,
            )
        });
        let request = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("first packet is delivered before selection is available");
        assert_eq!(request.paths, vec![launch(r"C:\Folder", false)]);
        let selected = request.selection.unwrap();
        assert_eq!(selected.try_recv(), Err(mpsc::TryRecvError::Empty));
        selection_sender.send(None).unwrap();
        assert_eq!(selected.recv_timeout(Duration::from_secs(2)).unwrap(), None);
        client.join().unwrap().unwrap();
        assert!(
            server
                .join()
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
    }

    #[test]
    fn issue_122_concurrent_connections_keep_reversed_selections_with_their_own_request() {
        let (name, receiver, server) = finite_server(2);
        let first = test_client(&name);
        send_test_packet(
            first.0,
            &encode_open(&[launch(r"C:\First", false)], true).unwrap(),
        )
        .unwrap();
        let first_request = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        let second = test_client(&name);
        send_test_packet(
            second.0,
            &encode_open(&[launch(r"C:\Second", false)], true).unwrap(),
        )
        .unwrap();
        let second_request = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("first selection cannot block another connection");
        assert_eq!(first_request.paths[0].path, PathBuf::from(r"C:\First"));
        assert_eq!(second_request.paths[0].path, PathBuf::from(r"C:\Second"));
        let first_selection = first_request.selection.unwrap();
        let second_selection = second_request.selection.unwrap();
        let second_target = PathBuf::from(r"C:\Second\b.txt");
        send_test_packet(second.0, &encode_selection(Some(&second_target)).unwrap()).unwrap();
        assert_eq!(
            second_selection
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Some(second_target)
        );
        assert_eq!(first_selection.try_recv(), Err(mpsc::TryRecvError::Empty));
        let first_target = PathBuf::from(r"C:\First\a.txt");
        send_test_packet(first.0, &encode_selection(Some(&first_target)).unwrap()).unwrap();
        assert_eq!(
            first_selection
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            Some(first_target)
        );
        drop(first);
        drop(second);
        assert!(
            server
                .join()
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
    }

    #[test]
    fn issue_122_disconnected_launcher_completes_followup_and_does_not_block_next_request() {
        let (name, receiver, server) = finite_server(2);
        let first = test_client(&name);
        send_test_packet(
            first.0,
            &encode_open(&[launch(r"C:\First", false)], true).unwrap(),
        )
        .unwrap();
        let request = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(first);
        assert_eq!(
            request
                .selection
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap(),
            None
        );
        let second = test_client(&name);
        send_test_packet(
            second.0,
            &encode_open(&[launch(r"C:\Second", false)], false).unwrap(),
        )
        .unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap().paths,
            vec![launch(r"C:\Second", false)]
        );
        drop(second);
        let results = server.join().unwrap();
        assert!(results[0].is_err());
        assert!(results[1].is_ok());
    }

    #[test]
    fn issue_110_authorizes_each_packet_before_delivery_even_when_permission_is_denied() {
        for denied in [false, true] {
            let (name, receiver, server) = finite_server(1);
            let authorizations = Arc::new(AtomicUsize::new(0));
            let client_authorizations = authorizations.clone();
            let (selection_sender, selection) = mpsc::channel();
            let client = thread::spawn(move || {
                deliver_messages(
                    test_client(&name),
                    &encode_open(&[launch(r"C:\Folder", false)], true).unwrap(),
                    1,
                    Some(selection),
                    |handle| {
                        let pid = pipe_server_process_id(handle)?;
                        assert_eq!(pid, std::process::id());
                        client_authorizations.fetch_add(1, Ordering::SeqCst);
                        if denied {
                            Err(io::Error::from(io::ErrorKind::PermissionDenied))
                        } else {
                            Ok(pid)
                        }
                    },
                )
            });
            let request = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(authorizations.load(Ordering::SeqCst), 1);
            let target = PathBuf::from(r"C:\Folder\a.txt");
            selection_sender.send(Some(target.clone())).unwrap();
            assert_eq!(
                request
                    .selection
                    .unwrap()
                    .recv_timeout(Duration::from_secs(2))
                    .unwrap(),
                Some(target)
            );
            assert_eq!(authorizations.load(Ordering::SeqCst), 2);
            client.join().unwrap().unwrap();
            assert!(
                server
                    .join()
                    .unwrap()
                    .into_iter()
                    .all(|result| result.is_ok())
            );
        }
    }

    #[test]
    fn issue_122_fast_disconnect_does_not_kill_the_listener_or_lose_acknowledged_requests() {
        const REQUESTS: usize = 48;
        let name = test_pipe_name();
        let first = create_pipe(&name, true).unwrap();
        // Force the launch-exit race: a client leaves before ConnectNamedPipe runs.
        let abandoned = open_pipe(&name).unwrap();
        write_message(
            abandoned.0,
            &encode_open(&[launch(r"C:\Abandoned", false)], false).unwrap(),
        )
        .unwrap();
        drop(abandoned);
        let (sender, receiver) = mpsc::channel();
        let server_name = name.clone();
        let server = thread::spawn(move || {
            listen_connections(
                &server_name,
                first,
                sender,
                std::iter::repeat_n((), REQUESTS + 1),
            );
        });
        for index in 0..REQUESTS {
            let expected = vec![launch(&format!(r"C:\Quick {index}"), false)];
            deliver_messages(
                test_client(&name),
                &encode_open(&expected, false).unwrap(),
                1,
                None,
                pipe_server_process_id,
            )
            .expect("a request is complete only after the application queue acknowledges it");
            assert_eq!(
                receiver.recv_timeout(Duration::from_secs(2)).unwrap().paths,
                expected
            );
        }
        server.join().unwrap();
        // Every per-connection worker owns a Sender; disconnection proves they also finished.
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)),
            Err(mpsc::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn issue_122_no_ack_is_sent_when_the_application_queue_is_closed() {
        let (name, receiver, server) = finite_server(1);
        drop(receiver);
        let result = deliver_messages(
            test_client(&name),
            &encode_open(&[launch(r"C:\Folder", false)], false).unwrap(),
            1,
            None,
            pipe_server_process_id,
        );
        assert!(
            result.is_err(),
            "writing bytes alone cannot report a delivered request"
        );
        assert!(
            server
                .join()
                .unwrap()
                .into_iter()
                .all(|result| result.is_err())
        );
    }

    #[test]
    fn issue_122_ack_wait_expires_when_the_server_never_accepts() {
        let name = test_pipe_name();
        let server = create_pipe(&name, true).unwrap();
        let client = open_pipe(&name).unwrap();
        let started = Instant::now();
        let error = wait_for_ack(client.0, OPEN_MESSAGE, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(client);
        drop(server);
    }

    #[test]
    fn issue_122_final_ack_is_not_discarded_before_the_client_reads_it() {
        let (name, receiver, server) = finite_server(1);
        let client = test_client(&name);
        write_message(
            client.0,
            &encode_open(&[launch(r"C:\Folder", false)], false).unwrap(),
        )
        .unwrap();
        let request = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(request.paths[0].path, PathBuf::from(r"C:\Folder"));
        thread::sleep(Duration::from_millis(100));
        wait_for_ack(client.0, OPEN_MESSAGE, Duration::from_secs(1)).unwrap();
        drop(client);
        assert!(
            server
                .join()
                .unwrap()
                .into_iter()
                .all(|result| result.is_ok())
        );
    }

    #[test]
    fn issue_122_missing_endpoint_starts_once_and_waits_for_it_to_become_ready() {
        let name = test_pipe_name();
        let starts = Arc::new(AtomicUsize::new(0));
        let called = starts.clone();
        let server_name = name.clone();
        let (ready_sender, ready_receiver) = mpsc::channel();
        let pipe = connect_with_start(&name, move || {
            called.fetch_add(1, Ordering::SeqCst);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                let server = create_pipe(&server_name, true).unwrap();
                accept_pipe(&server).unwrap();
                ready_sender.send(server).unwrap();
            });
            Ok(())
        })
        .unwrap();
        let server = ready_receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(pipe_server_process_id(pipe.0).unwrap(), std::process::id());
        drop(pipe);
        drop(server);
    }
}
