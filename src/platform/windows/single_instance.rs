use std::{
    ffi::OsString,
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    ptr,
    sync::mpsc,
    thread,
};

use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_PIPE_BUSY,
        ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_GENERIC_WRITE,
        OPEN_EXISTING, PIPE_ACCESS_INBOUND, ReadFile, WriteFile,
    },
    System::{
        Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, GetNamedPipeServerProcessId,
            PIPE_READMODE_MESSAGE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE,
            PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
        },
        Threading::CreateMutexW,
    },
};

use crate::platform::windows::address_path::ExternalLaunchPath;

const INSTANCE_NAME: &str = "Local\\AsterFiles.SingleInstance.v1";
const PIPE_NAME: &str = r"\\.\pipe\AsterFiles.ExternalPaths.v1";
const MAX_PATHS: usize = 1_024;
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;
const IPC_MAGIC: u32 = 0x3150_4641;

pub enum InstanceOutcome {
    Primary(PrimaryInstance),
    Forwarded,
    Fallback,
}

pub struct PrimaryInstance {
    _mutex: OwnedHandle,
    receiver: Option<mpsc::Receiver<Vec<ExternalLaunchPath>>>,
}

impl PrimaryInstance {
    pub fn take_receiver(&mut self) -> mpsc::Receiver<Vec<ExternalLaunchPath>> {
        self.receiver
            .take()
            .expect("primary instance receiver can only be taken once")
    }
}

pub fn forward_paths(paths: &[ExternalLaunchPath]) -> io::Result<()> {
    forward_to(PIPE_NAME, paths)
}

pub fn coordinate(paths: &[ExternalLaunchPath]) -> io::Result<InstanceOutcome> {
    let name = wide(INSTANCE_NAME);
    let mutex = unsafe { CreateMutexW(ptr::null(), 0, name.as_ptr()) };
    if mutex.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mutex = OwnedHandle(mutex);
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(if forward_to(PIPE_NAME, paths).is_ok() {
            InstanceOutcome::Forwarded
        } else {
            InstanceOutcome::Fallback
        });
    }
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("external-path-ipc".to_owned())
        .spawn(move || listen(PIPE_NAME, sender))?;
    Ok(InstanceOutcome::Primary(PrimaryInstance {
        _mutex: mutex,
        receiver: Some(receiver),
    }))
}

fn listen(pipe_name: &'static str, sender: mpsc::Sender<Vec<ExternalLaunchPath>>) {
    while let Ok(pipe) = create_pipe(pipe_name) {
        let connected = unsafe { ConnectNamedPipe(pipe.0, ptr::null_mut()) } != 0
            || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
        if connected
            && let Ok(paths) = read_message(pipe.0)
            && sender.send(paths).is_err()
        {
            break;
        }
        unsafe { DisconnectNamedPipe(pipe.0) };
    }
}

fn create_pipe(pipe_name: &str) -> io::Result<OwnedHandle> {
    let name = wide(pipe_name);
    let handle = unsafe {
        CreateNamedPipeW(
            name.as_ptr(),
            PIPE_ACCESS_INBOUND | FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            PIPE_UNLIMITED_INSTANCES,
            0,
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

fn forward_to(pipe_name: &str, paths: &[ExternalLaunchPath]) -> io::Result<()> {
    forward_to_with_authorizer(pipe_name, paths, grant_foreground_permission)
}

fn forward_to_with_authorizer(
    pipe_name: &str,
    paths: &[ExternalLaunchPath],
    authorize: impl FnOnce(HANDLE) -> io::Result<u32>,
) -> io::Result<()> {
    let bytes = encode_paths(paths)?;
    let name = wide(pipe_name);
    let mut last_error = io::Error::from_raw_os_error(ERROR_PIPE_BUSY as i32);
    for _ in 0..4 {
        let handle = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                ptr::null_mut(),
            )
        };
        if handle != INVALID_HANDLE_VALUE {
            let handle = OwnedHandle(handle);
            // Grant before publishing paths: the receiver can handle them immediately.
            // Windows may deny focus, but that must never prevent opening the folder.
            let permission = authorize(handle.0);
            crate::operation_audit::record(
                "external-open-foreground-permission",
                format!("result={permission:?}"),
            );
            let mut written = 0;
            if unsafe {
                WriteFile(
                    handle.0,
                    bytes.as_ptr(),
                    bytes.len() as u32,
                    &mut written,
                    ptr::null_mut(),
                )
            } != 0
                && written as usize == bytes.len()
            {
                crate::operation_audit::record(
                    "external-open-forwarded",
                    format!("path_count={}", paths.len()),
                );
                return Ok(());
            }
            return Err(io::Error::last_os_error());
        }
        last_error = io::Error::last_os_error();
        match last_error.raw_os_error().map(|error| error as u32) {
            Some(ERROR_PIPE_BUSY) => {
                let _ = unsafe { WaitNamedPipeW(name.as_ptr(), 250) };
            }
            Some(ERROR_FILE_NOT_FOUND) => thread::sleep(std::time::Duration::from_millis(100)),
            _ => break,
        }
    }
    Err(last_error)
}

fn read_message(handle: HANDLE) -> io::Result<Vec<ExternalLaunchPath>> {
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
            break;
        }
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "empty partial IPC message",
            ));
        }
    }
    decode_paths(&bytes)
}

fn encode_paths(paths: &[ExternalLaunchPath]) -> io::Result<Vec<u8>> {
    if paths.len() > MAX_PATHS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many paths",
        ));
    }
    let mut units = Vec::new();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&IPC_MAGIC.to_le_bytes());
    bytes.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    for launch in paths {
        units.clear();
        units.extend(launch.path.as_os_str().encode_wide());
        if units.len() > u32::MAX as usize
            || bytes
                .len()
                .saturating_add(8)
                .saturating_add(units.len().saturating_mul(2))
                > MAX_MESSAGE_BYTES
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "path message too large",
            ));
        }
        bytes.extend_from_slice(&u32::from(launch.select).to_le_bytes());
        bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for unit in &units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    Ok(bytes)
}

fn decode_paths(bytes: &[u8]) -> io::Result<Vec<ExternalLaunchPath>> {
    let mut offset = 0;
    let first = read_u32(bytes, &mut offset)?;
    if first == IPC_MAGIC {
        decode_versioned_paths(bytes, &mut offset, true)
    } else {
        offset = 0;
        decode_versioned_paths(bytes, &mut offset, false)
    }
}

fn decode_versioned_paths(
    bytes: &[u8],
    offset: &mut usize,
    versioned: bool,
) -> io::Result<Vec<ExternalLaunchPath>> {
    let count = read_u32(bytes, offset)? as usize;
    if count > MAX_PATHS {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too many paths"));
    }
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let select = if versioned {
            read_u32(bytes, offset)? != 0
        } else {
            false
        };
        let length = read_u32(bytes, offset)? as usize;
        if length.saturating_mul(2) > MAX_MESSAGE_BYTES
            || offset.saturating_add(length.saturating_mul(2)) > bytes.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid path length",
            ));
        }
        let units = bytes[*offset..*offset + length * 2]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        *offset += length * 2;
        paths.push(ExternalLaunchPath {
            path: PathBuf::from(OsString::from_wide(&units)),
            select,
        });
    }
    if *offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing IPC data",
        ));
    }
    Ok(paths)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::ffi::OsStrExt;

    fn launch(path: &str, select: bool) -> ExternalLaunchPath {
        ExternalLaunchPath {
            path: PathBuf::from(path),
            select,
        }
    }

    #[test]
    fn path_payload_preserves_unicode_spaces_and_multiple_paths() {
        let paths = vec![
            launch(r"C:\Folder With Spaces", false),
            launch(r"C:\中文\深层目录", false),
            launch(r"\\server\share\folder", false),
        ];
        assert_eq!(decode_paths(&encode_paths(&paths).unwrap()).unwrap(), paths);
    }

    #[test]
    fn issue_118_path_payload_preserves_select_flag() {
        let paths = vec![
            launch(r"C:\Folder With Spaces", false),
            launch(r"D:\Downloads\file.zip", true),
            launch(r"\\server\share\file.txt", true),
        ];
        assert_eq!(decode_paths(&encode_paths(&paths).unwrap()).unwrap(), paths);
    }

    #[test]
    fn empty_payload_represents_a_plain_win_e_launch() {
        let paths = Vec::new();
        assert_eq!(decode_paths(&encode_paths(&paths).unwrap()).unwrap(), paths);
    }

    #[test]
    fn issue_118_legacy_payload_without_select_flags_still_decodes() {
        let mut bytes = Vec::new();
        let path = PathBuf::from(r"C:\Folder With Spaces");
        let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
        for unit in units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            decode_paths(&bytes).unwrap(),
            vec![launch(r"C:\Folder With Spaces", false)]
        );
    }
    #[test]
    fn named_pipe_transfers_a_message_larger_than_the_read_buffer() {
        let pipe_name = format!(
            r"\\.\pipe\AsterFiles.ExternalPaths.Test.{}.{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        );
        let expected = (0..128)
            .map(|index| launch(&format!(r"C:\路径 {index}\{}", "x".repeat(64)), false))
            .collect::<Vec<_>>();
        let server_name = pipe_name.clone();
        let server = thread::spawn(move || {
            let pipe = create_pipe(&server_name)?;
            let connected = unsafe { ConnectNamedPipe(pipe.0, ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            if !connected {
                return Err(io::Error::last_os_error());
            }
            read_message(pipe.0)
        });

        forward_to(&pipe_name, &expected).expect("message is forwarded");
        assert_eq!(
            server.join().expect("server thread joins").unwrap(),
            expected
        );
    }
    #[test]
    fn issue_110_authorizes_actual_server_before_delivery_even_when_permission_is_denied() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };

        for denied in [false, true] {
            let pipe_name = format!(
                r"\\.\pipe\AsterFiles.Foreground.Test.{}.{}",
                std::process::id(),
                denied,
            );
            let authorization_attempted = Arc::new(AtomicBool::new(false));
            let server_authorization_attempted = authorization_attempted.clone();
            let server_name = pipe_name.clone();
            let server = thread::spawn(move || {
                let pipe = create_pipe(&server_name)?;
                let connected = unsafe { ConnectNamedPipe(pipe.0, ptr::null_mut()) } != 0
                    || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
                if !connected {
                    return Err(io::Error::last_os_error());
                }
                let paths = read_message(pipe.0)?;
                assert!(
                    server_authorization_attempted.load(Ordering::SeqCst),
                    "authorization must precede delivery"
                );
                Ok(paths)
            });
            let expected = vec![
                launch(r"C:\中文 folder", false),
                launch(r"\\server\share", true),
            ];
            forward_to_with_authorizer(&pipe_name, &expected, |handle| {
                let process_id = pipe_server_process_id(handle)?;
                assert_eq!(process_id, std::process::id());
                authorization_attempted.store(true, Ordering::SeqCst);
                if denied {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied))
                } else {
                    Ok(process_id)
                }
            })
            .expect("denied focus must not lose the request or launch an extra instance");
            assert_eq!(server.join().unwrap().unwrap(), expected);
        }
    }

    #[test]
    fn malformed_payload_is_rejected() {
        assert!(decode_paths(&[1, 0, 0, 0, 5, 0, 0, 0, 1, 0]).is_err());
    }
}
