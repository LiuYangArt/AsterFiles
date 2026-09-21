//! Diagnose how another app launched AsterFiles, and recover Explorer `/select` identity.
//!
//! Folder Open only receives the parent directory. Downloaders that also start
//! `explorer.exe /select,<file>` leave that file on the Explorer command line, which this
//! module records and can rewrite as an #118 reveal. The desktop `explorer.exe` is never closed.

use std::{
    ffi::OsString,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};

use windows_sys::{
    Wdk::System::Threading::{NtQueryInformationProcess, ProcessCommandLineInformation},
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE},
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
        UI::WindowsAndMessaging::{
            EnumWindows, GetClassNameW, GetShellWindow, GetWindowThreadProcessId, PostMessageW,
            WM_CLOSE,
        },
    },
};

use crate::platform::windows::address_path::{ExternalLaunchPath, select_target_from_command_line};

#[derive(Debug, Clone)]
pub struct LaunchProbe {
    pub parent_pid: Option<u32>,
    pub parent_image: Option<PathBuf>,
    pub parent_command: Option<OsString>,
    pub self_command: OsString,
    pub parent_is_shell: bool,
    pub explorer_selects: Vec<ExplorerSelectProcess>,
}

#[derive(Debug, Clone)]
pub struct ExplorerSelectProcess {
    pub pid: u32,
    pub is_shell: bool,
    pub command: OsString,
    pub target: PathBuf,
}

pub fn parent_is_desktop_shell() -> bool {
    let parent = parent_process_id();
    parent.is_some() && parent == shell_process_id()
}

pub fn collect() -> LaunchProbe {
    let self_command = current_command_line();
    let parent_pid = parent_process_id();
    let shell_pid = shell_process_id();
    let parent_is_shell = parent_pid == shell_pid && parent_pid.is_some();
    let parent_image = parent_pid.and_then(process_image_path);
    let parent_command = parent_pid.and_then(process_command_line);
    let explorer_selects = explorer_select_processes(shell_pid);
    LaunchProbe {
        parent_pid,
        parent_image,
        parent_command,
        self_command,
        parent_is_shell,
        explorer_selects,
    }
}

pub fn record(probe: &LaunchProbe, paths: &[ExternalLaunchPath]) {
    let kinds = paths
        .iter()
        .map(|item| if item.select { "select" } else { "open" })
        .collect::<Vec<_>>()
        .join(",");
    let selects = probe
        .explorer_selects
        .iter()
        .map(|item| {
            format!(
                "pid={} shell={} cmd={:?} target={:?}",
                item.pid, item.is_shell, item.command, item.target
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    crate::operation_audit::record(
        "external-launch-probe",
        format!(
            "path_count={} kinds={kinds} parent_pid={:?} parent_is_shell={} parent_image={:?} parent_cmd={:?} self_cmd={:?} explorer_selects=[{selects}]",
            paths.len(),
            probe.parent_pid,
            probe.parent_is_shell,
            probe.parent_image,
            probe.parent_command,
            probe.self_command,
        ),
    );
}

pub fn reveal_from_explorer_select(folder: &Path, probe: &LaunchProbe) -> Option<PathBuf> {
    if let Some(command) = &probe.parent_command
        && let Some(target) = select_target_from_command_line(command)
        && target_belongs_to_folder(folder, &target)
    {
        return Some(target);
    }
    probe
        .explorer_selects
        .iter()
        .find(|item| target_belongs_to_folder(folder, &item.target))
        .map(|item| item.target.clone())
}

pub fn close_explorer_select_windows(folder: &Path, probe: &LaunchProbe) {
    let pids = probe
        .explorer_selects
        .iter()
        .filter(|item| !item.is_shell && target_belongs_to_folder(folder, &item.target))
        .map(|item| item.pid)
        .collect::<Vec<_>>();
    if pids.is_empty() {
        return;
    }
    let mut state = CloseExplorerState { pids };
    unsafe {
        let _ = EnumWindows(
            Some(close_explorer_enum),
            (&mut state as *mut CloseExplorerState) as isize,
        );
    }
}

fn target_belongs_to_folder(folder: &Path, target: &Path) -> bool {
    target
        .parent()
        .is_some_and(|parent| path_eq_ignore_ascii_case(parent, folder))
        || path_eq_ignore_ascii_case(target, folder)
}

fn path_eq_ignore_ascii_case(left: &Path, right: &Path) -> bool {
    let left = trim_trailing_slash(left.as_os_str().encode_wide().collect());
    let right = trim_trailing_slash(right.as_os_str().encode_wide().collect());
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            (*a <= 0x7f && b <= 0x7f && (*a as u8).eq_ignore_ascii_case(&(b as u8))) || *a == b
        })
}

fn trim_trailing_slash(mut wide: Vec<u16>) -> Vec<u16> {
    while wide.last() == Some(&u16::from(b'\\')) || wide.last() == Some(&u16::from(b'/')) {
        wide.pop();
    }
    wide
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

fn explorer_select_processes(shell_pid: Option<u32>) -> Vec<ExplorerSelectProcess> {
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut found = Vec::new();
    unsafe {
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if exe_is_explorer(&entry.szExeFile)
                    && let Some(command) = process_command_line(entry.th32ProcessID)
                    && let Some(target) = select_target_from_command_line(&command)
                {
                    found.push(ExplorerSelectProcess {
                        pid: entry.th32ProcessID,
                        is_shell: Some(entry.th32ProcessID) == shell_pid,
                        command,
                        target,
                    });
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
    }
    found
}

fn exe_is_explorer(exe: &[u16]) -> bool {
    let end = exe.iter().position(|unit| *unit == 0).unwrap_or(exe.len());
    let name = OsString::from_wide(&exe[..end]);
    name.to_string_lossy().eq_ignore_ascii_case("explorer.exe")
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

struct CloseExplorerState {
    pids: Vec<u32>,
}

unsafe extern "system" fn close_explorer_enum(hwnd: HWND, lparam: isize) -> i32 {
    let state = unsafe { &*(lparam as *const CloseExplorerState) };
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    if !state.pids.contains(&pid) {
        return 1;
    }
    let mut class = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, class.as_mut_ptr(), class.len() as i32) };
    if length <= 0 {
        return 1;
    }
    let class_name = OsString::from_wide(&class[..length as usize]);
    let class_name = class_name.to_string_lossy();
    if class_name.eq_ignore_ascii_case("CabinetWClass")
        || class_name.eq_ignore_ascii_case("ExploreWClass")
    {
        unsafe { PostMessageW(hwnd, WM_CLOSE, 0, 0) };
        crate::operation_audit::record(
            "external-launch-probe",
            format!("closed-explorer-window pid={pid} class={class_name}"),
        );
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_118_explorer_select_matches_the_opened_folder() {
        let folder = Path::new(r"D:\Downloads");
        let probe = LaunchProbe {
            parent_pid: Some(1),
            parent_image: Some(PathBuf::from(r"C:\Windows\explorer.exe")),
            parent_command: Some(OsString::from(
                r#"C:\Windows\explorer.exe /select,"D:\Downloads\file.zip""#,
            )),
            self_command: OsString::from(r#"asterfiles.exe "D:\Downloads\.""#),
            parent_is_shell: false,
            explorer_selects: Vec::new(),
        };
        assert_eq!(
            reveal_from_explorer_select(folder, &probe).as_deref(),
            Some(Path::new(r"D:\Downloads\file.zip"))
        );
    }
}
