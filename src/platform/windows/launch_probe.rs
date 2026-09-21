//! Read-only diagnostics for external launches; never infer targets from unrelated processes.

use std::{ffi::OsString, os::windows::ffi::OsStringExt, path::PathBuf};

use windows_sys::{
    Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation},
    Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Environment::GetCommandLineW,
            Threading::{
                GetCurrentProcess, GetCurrentProcessId, OpenProcess,
                PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
            },
        },
        UI::WindowsAndMessaging::{GetShellWindow, GetWindowThreadProcessId},
    },
};

use crate::platform::windows::address_path::ExternalLaunchPath;

#[derive(Debug, Clone)]
pub struct LaunchProbe {
    pub parent_pid: Option<u32>,
    pub parent_image: Option<PathBuf>,
    pub parent_command: Option<OsString>,
    pub self_command: OsString,
    pub parent_is_shell: bool,
}

pub fn collect() -> LaunchProbe {
    let self_command = current_command_line();
    let parent_pid = parent_process_id();
    let shell_pid = shell_process_id();
    let parent_is_shell = parent_pid == shell_pid && parent_pid.is_some();
    let parent_image = parent_pid.and_then(process_image_path);
    let parent_command = parent_pid.and_then(process_command_line);
    LaunchProbe {
        parent_pid,
        parent_image,
        parent_command,
        self_command,
        parent_is_shell,
    }
}

pub fn record(probe: &LaunchProbe, paths: &[ExternalLaunchPath]) {
    let kinds = paths
        .iter()
        .map(|item| if item.select { "select" } else { "open" })
        .collect::<Vec<_>>()
        .join(",");
    crate::operation_audit::record(
        "external-launch-probe",
        format!(
            "path_count={} kinds={kinds} parent_pid={:?} parent_is_shell={} parent_image={:?} parent_cmd={:?} self_cmd={:?}",
            paths.len(),
            probe.parent_pid,
            probe.parent_is_shell,
            probe.parent_image,
            probe.parent_command,
            probe.self_command,
        ),
    );
}

fn current_command_line() -> OsString {
    wide_string(unsafe { GetCommandLineW() }).unwrap_or_default()
}

fn process_image_path(pid: u32) -> Option<PathBuf> {
    with_query_process(pid, |handle| {
        let mut buffer = [0u16; 1024];
        let mut size = buffer.len() as u32;
        let ok =
            unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) } != 0;
        if !ok || size == 0 {
            return None;
        }
        Some(PathBuf::from(OsString::from_wide(&buffer[..size as usize])))
    })
}

fn process_command_line(pid: u32) -> Option<OsString> {
    with_query_process(pid, |handle| {
        let mut needed = 0;
        let first = unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessCommandLineInformation,
                std::ptr::null_mut(),
                0,
                &mut needed,
            )
        };
        if first as u32 != 0xC000_0004 || needed == 0 {
            return None;
        }
        let mut buffer = vec![0u8; needed as usize];
        let status = unsafe {
            NtQueryInformationProcess(
                handle,
                ProcessCommandLineInformation,
                buffer.as_mut_ptr().cast(),
                needed,
                &mut needed,
            )
        };
        if status < 0 || buffer.len() < std::mem::size_of::<UnicodeString>() {
            return None;
        }
        let header = unsafe { buffer.as_ptr().cast::<UnicodeString>().read_unaligned() };
        if header.buffer.is_null() || header.length == 0 {
            return None;
        }
        let units = (header.length as usize) / 2;
        Some(OsString::from_wide(unsafe {
            std::slice::from_raw_parts(header.buffer, units)
        }))
    })
}

fn with_query_process<T>(pid: u32, query: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let result = query(handle);
    unsafe { CloseHandle(handle) };
    result
}

fn shell_process_id() -> Option<u32> {
    let hwnd = unsafe { GetShellWindow() };
    if hwnd.is_null() {
        return None;
    }
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some(pid)
}

fn parent_process_id() -> Option<u32> {
    #[repr(C)]
    struct ProcessBasicInformation {
        exit_status: i32,
        _pad0: u32,
        peb_base_address: usize,
        affinity_mask: usize,
        base_priority: i32,
        _pad1: u32,
        unique_process_id: usize,
        inherited_from_unique_process_id: usize,
    }

    let mut info = unsafe { std::mem::zeroed::<ProcessBasicInformation>() };
    let mut needed = 0;
    let status = unsafe {
        NtQueryInformationProcess(
            GetCurrentProcess(),
            0,
            std::ptr::from_mut(&mut info).cast(),
            std::mem::size_of::<ProcessBasicInformation>() as u32,
            &mut needed,
        )
    };
    if status >= 0 {
        let pid = info.inherited_from_unique_process_id as u32;
        if pid != 0 {
            return Some(pid);
        }
    }

    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return None;
    }
    let current = unsafe { GetCurrentProcessId() };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut parent = None;
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == current {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    parent.filter(|pid| *pid != 0)
}

fn wide_string(ptr: *const u16) -> Option<OsString> {
    if ptr.is_null() {
        return None;
    }
    let mut length = 0;
    unsafe {
        while *ptr.add(length) != 0 {
            length += 1;
        }
    }
    Some(OsString::from_wide(unsafe {
        std::slice::from_raw_parts(ptr, length)
    }))
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *const u16,
}
