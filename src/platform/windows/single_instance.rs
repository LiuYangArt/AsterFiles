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
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_MESSAGE,
            PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_MESSAGE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
            WaitNamedPipeW,
        },
        Threading::CreateMutexW,
    },
};

const INSTANCE_NAME: &str = "Local\\AsterFiles.SingleInstance.v1";
const PIPE_NAME: &str = r"\\.\pipe\AsterFiles.ExternalPaths.v1";
const MAX_PATHS: usize = 1_024;
const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

pub enum InstanceOutcome {
    Primary(PrimaryInstance),
    Forwarded,
    Fallback,
}

pub struct PrimaryInstance {
    _mutex: OwnedHandle,
    receiver: Option<mpsc::Receiver<Vec<PathBuf>>>,
}

impl PrimaryInstance {
    pub fn take_receiver(&mut self) -> mpsc::Receiver<Vec<PathBuf>> {
        self.receiver
            .take()
            .expect("primary instance receiver can only be taken once")
    }
}

pub fn coordinate(paths: &[PathBuf]) -> io::Result<InstanceOutcome> {
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

fn listen(pipe_name: &'static str, sender: mpsc::Sender<Vec<PathBuf>>) {
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

fn forward_to(pipe_name: &str, paths: &[PathBuf]) -> io::Result<()> {
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

fn read_message(handle: HANDLE) -> io::Result<Vec<PathBuf>> {
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

fn encode_paths(paths: &[PathBuf]) -> io::Result<Vec<u8>> {
    if paths.len() > MAX_PATHS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many paths",
        ));
    }
    let mut units = Vec::new();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(paths.len() as u32).to_le_bytes());
    for path in paths {
        units.clear();
        units.extend(path.as_os_str().encode_wide());
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
        for unit in &units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
    }
    Ok(bytes)
}

fn decode_paths(bytes: &[u8]) -> io::Result<Vec<PathBuf>> {
    let mut offset = 0;
    let count = read_u32(bytes, &mut offset)? as usize;
    if count > MAX_PATHS {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too many paths"));
    }
    let mut paths = Vec::with_capacity(count);
    for _ in 0..count {
        let length = read_u32(bytes, &mut offset)? as usize;
        if length.saturating_mul(2) > MAX_MESSAGE_BYTES
            || offset.saturating_add(length.saturating_mul(2)) > bytes.len()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid path length",
            ));
        }
        let units = bytes[offset..offset + length * 2]
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        offset += length * 2;
        paths.push(PathBuf::from(OsString::from_wide(&units)));
    }
    if offset != bytes.len() {
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

    #[test]
    fn path_payload_preserves_unicode_spaces_and_multiple_paths() {
        let paths = vec![
            PathBuf::from(r"C:\Folder With Spaces"),
            PathBuf::from(r"C:\中文\深层目录"),
            PathBuf::from(r"\\server\share\folder"),
        ];
        assert_eq!(decode_paths(&encode_paths(&paths).unwrap()).unwrap(), paths);
    }

    #[test]
    fn empty_payload_represents_a_plain_win_e_launch() {
        let paths = Vec::new();
        assert_eq!(decode_paths(&encode_paths(&paths).unwrap()).unwrap(), paths);
    }
    #[test]
    fn named_pipe_transfers_a_message_larger_than_the_read_buffer() {
        let pipe_name = format!(
            r"\\.\pipe\AsterFiles.ExternalPaths.Test.{}.{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        );
        let expected = (0..128)
            .map(|index| PathBuf::from(format!(r"C:\路径 {index}\{}", "x".repeat(64))))
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
    fn malformed_payload_is_rejected() {
        assert!(decode_paths(&[1, 0, 0, 0, 5, 0, 0, 0, 1, 0]).is_err());
    }
}
