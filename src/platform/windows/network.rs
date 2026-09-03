#[cfg(windows)]
use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime},
};
#[cfg(windows)]
use windows::{
    Win32::{
        Foundation::RPC_E_CHANGED_MODE,
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        UI::Shell::{
            BHID_EnumItems, IEnumShellItems, IShellItem, SHCreateItemFromParsingName,
            SHGetKnownFolderPath, SIGDN_DESKTOPABSOLUTEPARSING, SIGDN_FILESYSPATH,
            SIGDN_NORMALDISPLAY,
        },
    },
    core::{GUID, PCWSTR, PWSTR},
};

#[cfg(windows)]
use crate::domain::{EntryId, EntryKind, FileEntry, FileVisibility, FolderSizeState};

pub fn record_runtime_event(event: &str) {
    record_runtime_detail(event);
}

fn record_runtime_detail(event: &str) {
    use std::{fs::OpenOptions, io::Write};

    let Some(local_data) = std::env::var_os("LOCALAPPDATA") else {
        return;
    };
    let directory = PathBuf::from(local_data).join("AsterFiles").join("logs");
    if std::fs::create_dir_all(&directory).is_err() {
        return;
    }
    let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("network-runtime.jsonl"))
    else {
        return;
    };
    let timestamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let _ = writeln!(
        file,
        "{{\"timestamp_ms\":{timestamp},\"pid\":{},\"event\":\"{event}\"}}",
        std::process::id(),
    );
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkLocation {
    pub label: String,
    pub target: Option<PathBuf>,
    pub shell_path: PathBuf,
    pub shell_identity: Option<PathBuf>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkRootItem {
    pub label: String,
    pub target: PathBuf,
    pub is_directory: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkDevice {
    pub label: String,
    pub target: PathBuf,
    pub is_directory: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct NetworkResult {
    pub code: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum NetworkAuthErrorKind {
    AccessDenied,
    LogonFailure,
    CredentialConflict,
    BadPath,
    Unavailable,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct NetworkAuthError {
    pub kind: NetworkAuthErrorKind,
    pub code: u32,
}

impl std::fmt::Display for NetworkAuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "network authentication failed: {:?} ({})",
            self.kind, self.code
        )
    }
}

impl std::error::Error for NetworkAuthError {}

const FOLDERID_NETHOOD: GUID = GUID::from_u128(0xc5abbf53_e17f_4121_8900_86626fc2c973);
const NETWORK_NAMESPACE: &str = "::{F02C1A0D-BE21-4350-88B0-7367FC96EF3C}";

struct ComGuard {
    initialized: bool,
}
impl ComGuard {
    fn new() -> io::Result<Self> {
        let result = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if result.is_ok() {
            Ok(Self { initialized: true })
        } else if result == RPC_E_CHANGED_MODE {
            Ok(Self { initialized: false })
        } else {
            Err(io::Error::other(format!(
                "CoInitializeEx failed: {result:?}"
            )))
        }
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}

pub fn enumerate_network_locations() -> io::Result<Vec<NetworkLocation>> {
    let _com = ComGuard::new()?;
    let items = enumerate_shell_folder(&known_folder_path(&FOLDERID_NETHOOD)?)?;
    Ok(items
        .into_iter()
        .filter_map(|item| {
            let shell_path = item
                .file_system_path
                .clone()
                .or_else(|| item.parsing_path.clone())?;
            let target_link = if shell_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("lnk"))
            {
                shell_path.clone()
            } else {
                shell_path.join("target.lnk")
            };
            let target = crate::platform::resolve_shortcut_target(&target_link)
                .ok()
                .flatten()
                .map(|resolved| resolved.path)
                .or_else(|| crate::network::is_unc_path(&shell_path).then(|| shell_path.clone()));
            let label = item.label.unwrap_or_else(|| {
                shell_path
                    .file_stem()
                    .unwrap_or(shell_path.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            });
            Some(NetworkLocation {
                label,
                target,
                shell_path,
                shell_identity: item.parsing_path,
            })
        })
        .collect())
}

pub fn enumerate_network_root(root: &Path) -> io::Result<Vec<NetworkRootItem>> {
    let _com = ComGuard::new()?;
    let mut result = enumerate_shell_folder(root)?
        .into_iter()
        .filter_map(|item| {
            let target = item.file_system_path.or(item.parsing_path)?;
            Some(NetworkRootItem {
                label: item
                    .label
                    .unwrap_or_else(|| target.to_string_lossy().into_owned()),
                target,
                is_directory: true,
            })
        })
        .collect::<Vec<_>>();
    result.sort_by(|left, right| {
        left.label
            .to_ascii_lowercase()
            .cmp(&right.label.to_ascii_lowercase())
    });
    Ok(result)
}

pub fn enumerate_network_devices() -> io::Result<Vec<NetworkDevice>> {
    let _com = ComGuard::new()?;
    Ok(enumerate_shell_folder(Path::new(NETWORK_NAMESPACE))?
        .into_iter()
        .filter_map(|item| {
            let target = item.file_system_path.or(item.parsing_path)?;
            if !target.to_string_lossy().starts_with(r"\\") {
                return None;
            }
            Some(NetworkDevice {
                label: item
                    .label
                    .unwrap_or_else(|| target.to_string_lossy().into_owned()),
                target,
                is_directory: true,
            })
        })
        .collect())
}

pub fn network_devices_from_imported_locations() -> io::Result<Vec<NetworkDevice>> {
    use std::collections::HashSet;

    let mut seen = HashSet::new();
    let mut devices = Vec::new();
    for location in enumerate_network_locations()? {
        let Some(target) = location.target else {
            continue;
        };
        let Some(host) = unc_host_display_name(&target) else {
            continue;
        };
        let root = PathBuf::from(format!(r"\\{host}"));
        let identity = root.as_os_str().encode_wide().collect::<Vec<_>>();
        if !seen.insert(identity) {
            continue;
        }
        devices.push(NetworkDevice {
            label: host,
            target: root,
            is_directory: true,
        });
    }
    Ok(devices)
}

fn unc_host_display_name(path: &Path) -> Option<String> {
    use std::ffi::OsString;

    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let start = if units.starts_with(&[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ]) {
        8
    } else if crate::network::is_unc_path(path) {
        2
    } else {
        return None;
    };
    let end = units[start..]
        .iter()
        .position(|unit| matches!(*unit, 0x005c | 0x002f))
        .map_or(units.len(), |offset| start + offset);
    (end > start).then(|| {
        OsString::from_wide(&units[start..end])
            .to_string_lossy()
            .into_owned()
    })
}

pub fn network_drive_to_unc(path: &Path) -> io::Result<PathBuf> {
    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if units.len() < 2
        || units[1] != b':' as u16
        || !((b'A' as u16..=b'Z' as u16).contains(&units[0])
            || (b'a' as u16..=b'z' as u16).contains(&units[0]))
    {
        return Ok(path.to_owned());
    }
    let local = [units[0], b':' as u16, 0];
    let mut capacity = 256_u32;
    loop {
        let mut remote = vec![0_u16; capacity as usize];
        let result = unsafe {
            windows_sys::Win32::NetworkManagement::WNet::WNetGetConnectionW(
                local.as_ptr(),
                remote.as_mut_ptr(),
                &mut capacity,
            )
        };
        if result == windows_sys::Win32::Foundation::ERROR_MORE_DATA {
            capacity = capacity.saturating_mul(2).max(512);
            continue;
        }
        if result != windows_sys::Win32::Foundation::NO_ERROR {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let length = remote
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(remote.len());
        let mut output = remote[..length].to_vec();
        let mut suffix = units[2..]
            .iter()
            .copied()
            .skip_while(|unit| *unit == b'\\' as u16 || *unit == b'/' as u16)
            .peekable();
        if suffix.peek().is_some() {
            if output.last() != Some(&(b'\\' as u16)) {
                output.push(b'\\' as u16);
            }
            output.extend(suffix);
        }
        return Ok(PathBuf::from(OsString::from_wide(&output)));
    }
}

#[allow(dead_code)]
pub fn connect_network_share(
    path: &Path,
    username: Option<&str>,
    password: Option<&str>,
    remember: bool,
) -> Result<NetworkResult, NetworkAuthError> {
    let root = normalize_share_root(path)?;
    let remote = wide_null(root.as_os_str());
    let user = username.map(|value| wide_null(OsStr::new(value)));
    let mut pass = password.map(|value| wide_null(OsStr::new(value)));
    let resource = windows_sys::Win32::NetworkManagement::WNet::NETRESOURCEW {
        dwType: windows_sys::Win32::NetworkManagement::WNet::RESOURCETYPE_DISK,
        lpRemoteName: remote.as_ptr() as _,
        ..Default::default()
    };
    let code = unsafe {
        windows_sys::Win32::NetworkManagement::WNet::WNetAddConnection3W(
            ptr::null_mut(),
            &resource,
            pass.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
            user.as_ref().map_or(ptr::null(), |value| value.as_ptr()),
            0,
        )
    };
    if code != windows_sys::Win32::Foundation::NO_ERROR {
        clear_secret(&mut pass);
        return Err(network_auth_error(code));
    }
    if remember {
        if let (Some(username), Some(password)) = (username, password) {
            let result = write_network_credential(&root, username, password);
            clear_secret(&mut pass);
            result?;
        } else {
            clear_secret(&mut pass);
        }
    } else {
        clear_secret(&mut pass);
    }
    Ok(NetworkResult { code })
}

#[allow(dead_code)]
pub fn disconnect_network_share(path: &Path) -> Result<NetworkResult, NetworkAuthError> {
    disconnect_network_share_inner(path, false)
}

#[allow(dead_code)]
pub fn force_disconnect_network_share(path: &Path) -> Result<NetworkResult, NetworkAuthError> {
    disconnect_network_share_inner(path, true)
}

fn disconnect_network_share_inner(
    path: &Path,
    force: bool,
) -> Result<NetworkResult, NetworkAuthError> {
    let root = normalize_share_root(path)?;
    let name = wide_null(root.as_os_str());
    let code = unsafe {
        windows_sys::Win32::NetworkManagement::WNet::WNetCancelConnection2W(
            name.as_ptr(),
            0,
            force as i32,
        )
    };
    if code == windows_sys::Win32::Foundation::NO_ERROR {
        Ok(NetworkResult { code })
    } else {
        Err(network_auth_error(code))
    }
}

fn normalize_share_root(path: &Path) -> Result<PathBuf, NetworkAuthError> {
    let normalized = network_drive_to_unc(path).map_err(|error| {
        network_auth_error(
            error
                .raw_os_error()
                .map_or(windows_sys::Win32::Foundation::ERROR_BAD_NETPATH, |code| {
                    code as u32
                }),
        )
    })?;
    share_root(&normalized)
        .map_err(|_| network_auth_error(windows_sys::Win32::Foundation::ERROR_BAD_NETPATH))
}

fn write_network_credential(
    root: &Path,
    username: &str,
    password: &str,
) -> Result<(), NetworkAuthError> {
    use windows_sys::Win32::Security::Credentials::{
        CRED_MAX_CREDENTIAL_BLOB_SIZE, CRED_PERSIST_ENTERPRISE, CRED_TYPE_DOMAIN_PASSWORD,
        CREDENTIALW, CredWriteW,
    };

    let mut target = root
        .as_os_str()
        .encode_wide()
        .skip_while(|unit| *unit == b'\\' as u16)
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut user = wide_null(OsStr::new(username));
    let mut secret = OsStr::new(password)
        .encode_wide()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    if secret.len() > CRED_MAX_CREDENTIAL_BLOB_SIZE as usize {
        clear_bytes(&mut secret);
        return Err(network_auth_error(
            windows_sys::Win32::Foundation::ERROR_INVALID_PASSWORD,
        ));
    }
    let credential = CREDENTIALW {
        Type: CRED_TYPE_DOMAIN_PASSWORD,
        TargetName: target.as_mut_ptr(),
        CredentialBlobSize: secret.len() as u32,
        CredentialBlob: secret.as_mut_ptr(),
        Persist: CRED_PERSIST_ENTERPRISE,
        UserName: user.as_mut_ptr(),
        ..Default::default()
    };
    let written = unsafe { CredWriteW(&credential, 0) };
    clear_bytes(&mut secret);
    if written != 0 {
        Ok(())
    } else {
        let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
        Err(network_auth_error(code))
    }
}

fn clear_secret(secret: &mut Option<Vec<u16>>) {
    if let Some(secret) = secret {
        secret.fill(0);
    }
}

fn clear_bytes(secret: &mut [u8]) {
    secret.fill(0);
}

fn network_auth_error(code: u32) -> NetworkAuthError {
    use windows_sys::Win32::Foundation::*;
    let kind = match code {
        ERROR_ACCESS_DENIED => NetworkAuthErrorKind::AccessDenied,
        ERROR_LOGON_FAILURE | ERROR_BAD_USERNAME | ERROR_INVALID_PASSWORD => {
            NetworkAuthErrorKind::LogonFailure
        }
        ERROR_SESSION_CREDENTIAL_CONFLICT => NetworkAuthErrorKind::CredentialConflict,
        ERROR_BAD_NET_NAME
        | ERROR_BAD_NETPATH
        | ERROR_NO_NET_OR_BAD_PATH
        | ERROR_INVALID_NAME
        | ERROR_BAD_DEVICE => NetworkAuthErrorKind::BadPath,
        ERROR_NETWORK_UNREACHABLE
        | ERROR_NO_NETWORK
        | ERROR_CONNECTION_UNAVAIL
        | ERROR_CONNECTION_REFUSED
        | ERROR_HOST_UNREACHABLE
        | ERROR_PROTOCOL_UNREACHABLE
        | ERROR_NOT_CONNECTED => NetworkAuthErrorKind::Unavailable,
        _ => NetworkAuthErrorKind::Other,
    };
    NetworkAuthError { kind, code }
}

struct ShellItemInfo {
    label: Option<String>,
    parsing_path: Option<PathBuf>,
    file_system_path: Option<PathBuf>,
}
fn enumerate_shell_folder(path: &Path) -> io::Result<Vec<ShellItemInfo>> {
    let wide = wide_null(path.as_os_str());
    let root: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None).map_err(windows_error)? };
    let items: IEnumShellItems = unsafe {
        root.BindToHandler(None, &BHID_EnumItems)
            .map_err(windows_error)?
    };
    let mut result = Vec::new();
    loop {
        let mut next = [None];
        let mut fetched = 0;
        if unsafe { items.Next(&mut next, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let Some(item) = next[0].take() else { continue };
        result.push(ShellItemInfo {
            label: display_name(&item, SIGDN_NORMALDISPLAY),
            parsing_path: display_name(&item, SIGDN_DESKTOPABSOLUTEPARSING).map(PathBuf::from),
            file_system_path: display_name(&item, SIGDN_FILESYSPATH).map(PathBuf::from),
        });
    }
    Ok(result)
}
fn display_name(item: &IShellItem, kind: windows::Win32::UI::Shell::SIGDN) -> Option<String> {
    let value = unsafe { item.GetDisplayName(kind).ok()? };
    let text = take_shell_string(value);
    (!text.is_empty()).then(|| text.to_string_lossy().into_owned())
}
fn take_shell_string(value: PWSTR) -> OsString {
    if value.is_null() {
        return OsString::new();
    }
    unsafe {
        let mut len = 0;
        while *value.0.add(len) != 0 {
            len += 1;
        }
        let result = OsString::from_wide(std::slice::from_raw_parts(value.0, len));
        CoTaskMemFree(Some(value.0.cast()));
        result
    }
}
fn known_folder_path(id: &GUID) -> io::Result<PathBuf> {
    let result =
        unsafe { SHGetKnownFolderPath(id, windows::Win32::UI::Shell::KNOWN_FOLDER_FLAG(0), None) };
    let value = result.map_err(windows_error)?;
    Ok(take_shell_string(value).into())
}
#[allow(dead_code)]
fn share_root(path: &Path) -> io::Result<PathBuf> {
    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let separator = |unit: u16| unit == b'\\' as u16 || unit == b'/' as u16;
    let ascii_equal = |unit: u16, uppercase: u8| {
        unit == uppercase as u16 || unit == uppercase.to_ascii_lowercase() as u16
    };
    let extended_unc = units.len() >= 8
        && units[..4] == [b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]
        && ascii_equal(units[4], b'U')
        && ascii_equal(units[5], b'N')
        && ascii_equal(units[6], b'C')
        && separator(units[7]);
    let body = if extended_unc {
        &units[8..]
    } else if units.starts_with(&[b'\\' as u16, b'\\' as u16]) {
        &units[2..]
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network path must be UNC",
        ));
    };
    let mut components = body
        .split(|unit| separator(*unit))
        .filter(|part| !part.is_empty());
    let server = components
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing server"))?;
    let share = components
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing share"))?;
    let mut root = vec![b'\\' as u16, b'\\' as u16];
    root.extend_from_slice(server);
    root.push(b'\\' as u16);
    root.extend_from_slice(share);
    Ok(PathBuf::from(OsString::from_wide(&root)))
}
fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}
fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unc_is_unchanged() {
        let p = Path::new(r"\\server\share\folder");
        assert_eq!(network_drive_to_unc(p).unwrap(), p);
    }
    #[cfg(windows)]
    #[test]
    fn share_root_preserves_non_unicode_components_and_normalizes_extended_unc() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        let server = [b's' as u16, 0xd800];
        let share = [b'x' as u16, 0xdc00];
        let mut extended = vec![
            b'\\' as u16,
            b'\\' as u16,
            b'?' as u16,
            b'\\' as u16,
            b'U' as u16,
            b'N' as u16,
            b'C' as u16,
            b'\\' as u16,
        ];
        extended.extend_from_slice(&server);
        extended.push(b'\\' as u16);
        extended.extend_from_slice(&share);
        extended.extend_from_slice(&[b'\\' as u16, b'd' as u16]);
        let root = share_root(Path::new(&OsString::from_wide(&extended))).unwrap();
        let mut expected = vec![b'\\' as u16, b'\\' as u16];
        expected.extend_from_slice(&server);
        expected.push(b'\\' as u16);
        expected.extend_from_slice(&share);
        assert_eq!(root.as_os_str().encode_wide().collect::<Vec<_>>(), expected);
    }
    #[test]
    fn share_root_is_extracted() {
        assert_eq!(
            share_root(Path::new(r"\\server\share\folder")).unwrap(),
            PathBuf::from(r"\\server\share")
        );
    }
    #[test]
    fn authentication_error_codes_are_classified() {
        use windows_sys::Win32::Foundation::*;

        assert_eq!(
            network_auth_error(ERROR_ACCESS_DENIED).kind,
            NetworkAuthErrorKind::AccessDenied
        );
        assert_eq!(
            network_auth_error(ERROR_LOGON_FAILURE).kind,
            NetworkAuthErrorKind::LogonFailure
        );
        assert_eq!(
            network_auth_error(ERROR_SESSION_CREDENTIAL_CONFLICT).kind,
            NetworkAuthErrorKind::CredentialConflict
        );
        assert_eq!(
            network_auth_error(ERROR_BAD_NETPATH).kind,
            NetworkAuthErrorKind::BadPath
        );
        assert_eq!(
            network_auth_error(ERROR_NETWORK_UNREACHABLE).kind,
            NetworkAuthErrorKind::Unavailable
        );
        assert_eq!(
            network_auth_error(ERROR_EXTENDED_ERROR).kind,
            NetworkAuthErrorKind::Other
        );
        assert_eq!(
            network_auth_error(ERROR_LOGON_FAILURE).code,
            ERROR_LOGON_FAILURE
        );
    }

    #[test]
    fn authentication_normalizes_deep_unc_to_share_root() {
        assert_eq!(
            normalize_share_root(Path::new(r"\\server\share\folder\file.txt")).unwrap(),
            PathBuf::from(r"\\server\share")
        );
    }
    #[test]
    fn network_root_item_preserves_unc_target() {
        let item = NetworkRootItem {
            label: "共享".to_owned(),
            target: PathBuf::from(r"\\服务器\共享"),
            is_directory: true,
        };
        assert_eq!(item.target, PathBuf::from(r"\\服务器\共享"));
    }

    #[test]
    fn configured_network_root_can_be_enumerated() {
        let Some(root) = std::env::var_os("ASTERFILES_NETWORK_TEST_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let items = enumerate_network_root(&root).expect("configured network root is readable");
        assert!(!items.is_empty(), "configured network root has shares");
        assert!(
            items
                .iter()
                .all(|item| crate::network::is_unc_path(&item.target))
        );
        assert!(
            items
                .iter()
                .all(|item| crate::network::unc_leaf_name(&item.target).is_some()),
            "network root entries must have a stable final component: {items:#?}"
        );
    }
    #[test]
    fn non_unc_is_rejected() {
        assert!(share_root(Path::new(r"C:\folder")).is_err());
    }
}

#[cfg(windows)]
const ISOLATED_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(windows)]
const CHILD_PREFIX: &str = "--asterfiles-network-child";

#[cfg(windows)]
pub fn isolated_network_devices(cancel: &AtomicBool) -> io::Result<Vec<NetworkDevice>> {
    run_isolated(
        "devices",
        |input| std::fs::write(input, []),
        cancel,
        read_result,
    )
    .map(|items| {
        items
            .into_iter()
            .map(|(label, target, is_directory)| NetworkDevice {
                label,
                target,
                is_directory,
            })
            .collect::<Vec<_>>()
    })
}

#[cfg(windows)]
pub fn isolated_network_root(root: &Path, cancel: &AtomicBool) -> io::Result<Vec<NetworkRootItem>> {
    run_isolated(
        "root",
        |input| write_utf16_path(input, root),
        cancel,
        read_result,
    )
    .map(|items| {
        items
            .into_iter()
            .map(|(label, target, is_directory)| NetworkRootItem {
                label,
                target,
                is_directory,
            })
            .collect::<Vec<_>>()
    })
}

#[cfg(windows)]
#[allow(dead_code)]
pub fn isolated_connect_network_share(
    path: &Path,
    username: Option<&str>,
    password: Option<&str>,
    remember: bool,
    cancel: &AtomicBool,
) -> Result<NetworkResult, NetworkAuthError> {
    let result = run_isolated(
        "connect",
        |input| write_auth_input(input, path, username, password, remember),
        cancel,
        read_auth_result,
    );
    match result {
        Ok(result) => result,
        Err(error) => Err(network_auth_error(error.raw_os_error().map_or(
            match error.kind() {
                io::ErrorKind::Interrupted => windows_sys::Win32::Foundation::ERROR_CANCELLED,
                io::ErrorKind::TimedOut => windows_sys::Win32::Foundation::ERROR_TIMEOUT,
                _ => windows_sys::Win32::Foundation::ERROR_GEN_FAILURE,
            },
            |code| code as u32,
        ))),
    }
}

#[cfg(windows)]
pub fn isolated_force_disconnect_network_share(
    path: &Path,
    cancel: &AtomicBool,
) -> Result<NetworkResult, NetworkAuthError> {
    let result = run_isolated(
        "disconnect",
        |input| write_utf16_path(input, path),
        cancel,
        read_auth_result,
    );
    match result {
        Ok(result) => result,
        Err(error) => Err(network_auth_error(error.raw_os_error().map_or(
            match error.kind() {
                io::ErrorKind::Interrupted => windows_sys::Win32::Foundation::ERROR_CANCELLED,
                io::ErrorKind::TimedOut => windows_sys::Win32::Foundation::ERROR_TIMEOUT,
                _ => windows_sys::Win32::Foundation::ERROR_GEN_FAILURE,
            },
            |code| code as u32,
        ))),
    }
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedFileMutationResult {
    pub completed_path: Option<PathBuf>,
    pub affected_directories: Vec<PathBuf>,
}

#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolatedFileMutationKind {
    CreateFolder,
    Rename,
}

#[cfg(windows)]
pub fn isolated_file_mutation(
    kind: IsolatedFileMutationKind,
    source: Option<&Path>,
    destination: Option<&Path>,
    cancel: &AtomicBool,
) -> io::Result<IsolatedFileMutationResult> {
    run_isolated_with_timeout(
        "file-mutation",
        |input| write_file_mutation_input(input, kind, source, destination),
        cancel,
        read_file_mutation_result,
        None,
    )
}
#[cfg(windows)]
pub fn isolated_directory(
    path: &Path,
    visibility: FileVisibility,
    cancel: &AtomicBool,
) -> io::Result<(Vec<FileEntry>, usize)> {
    let (entries, skipped, truncated) = run_isolated(
        "directory",
        |input| write_directory_input(input, path, visibility),
        cancel,
        read_directory_result,
    )?;
    if truncated {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "network directory exceeds the 4096 item safety limit",
        ));
    }
    Ok((entries, skipped))
}

#[cfg(windows)]
struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl KillOnCloseJob {
    fn create() -> io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        let job = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if job.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return Err(io::Error::last_os_error());
        }
        Ok(Self(job))
    }

    fn assign(&self, child: &std::process::Child) -> io::Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

        if unsafe { AssignProcessToJobObject(self.0, child.as_raw_handle() as _) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for KillOnCloseJob {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}
fn run_isolated<T>(
    mode: &str,
    prepare_input: impl FnOnce(&Path) -> io::Result<()>,
    cancel: &AtomicBool,
    read_output: impl FnOnce(&Path) -> io::Result<T>,
) -> io::Result<T> {
    run_isolated_with_timeout(
        mode,
        prepare_input,
        cancel,
        read_output,
        Some(ISOLATED_TIMEOUT),
    )
}

#[cfg(windows)]
fn run_isolated_with_timeout<T>(
    mode: &str,
    prepare_input: impl FnOnce(&Path) -> io::Result<()>,
    cancel: &AtomicBool,
    read_output: impl FnOnce(&Path) -> io::Result<T>,
    timeout: Option<Duration>,
) -> io::Result<T> {
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let base =
        std::env::temp_dir().join(format!("asterfiles-network-{}-{stamp}", std::process::id()));
    let input = base.with_extension("in");
    let output = base.with_extension("out");
    if let Err(error) = prepare_input(&input) {
        let _ = std::fs::remove_file(&input);
        return Err(error);
    }
    let result = (|| {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(std::env::current_exe()?);
        command.arg(CHILD_PREFIX).arg(mode).arg(&input).arg(&output);
        command
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn()?;
        let job = match KillOnCloseJob::create() {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(io::Error::other(format!(
                        "network helper exited with {status}"
                    )));
                }
                return read_output(&output);
            }
            if cancel.load(Ordering::Acquire) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "network helper cancelled",
                ));
            }
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "network helper timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    })();
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    result
}
#[cfg(windows)]
pub fn try_run_child_from_args() -> io::Result<bool> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|value| value.to_str()) != Some(CHILD_PREFIX) {
        return Ok(false);
    }
    if args.len() != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid network helper arguments",
        ));
    }
    let mode = args[2].to_string_lossy();
    let input = PathBuf::from(&args[3]);
    let output = PathBuf::from(&args[4]);
    if mode == "directory" {
        let (path, visibility) = read_directory_input(&input)?;
        let result = enumerate_directory(&path, visibility)?;
        write_directory_result(&output, &result)?;
        return Ok(true);
    }
    if mode == "connect" {
        let (path, username, mut password, remember) = read_auth_input(&input)?;
        let result =
            connect_network_share(&path, username.as_deref(), password.as_deref(), remember);
        if let Some(password) = &mut password {
            unsafe { password.as_bytes_mut() }.fill(0);
        }
        write_auth_result(&output, result)?;
        return Ok(true);
    }
    if mode == "disconnect" {
        let path = read_utf16_path(&input)?;
        write_auth_result(&output, force_disconnect_network_share(&path))?;
        return Ok(true);
    }
    if mode == "file-mutation" {
        let (kind, source, destination) = read_file_mutation_input(&input)?;
        let result = execute_file_mutation(kind, source.as_deref(), destination.as_deref())?;
        write_file_mutation_result(&output, &result)?;
        return Ok(true);
    }
    let result = if mode == "devices" {
        enumerate_network_devices()?
            .into_iter()
            .map(|item| (item.label, item.target, true))
            .collect::<Vec<_>>()
    } else if mode == "root" {
        let root = read_utf16_path(&input)?;
        enumerate_network_root(&root)?
            .into_iter()
            .map(|item| (item.label, item.target, true))
            .collect::<Vec<_>>()
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unknown network helper mode",
        ));
    };
    write_result(&output, &result)?;
    Ok(true)
}

#[cfg(windows)]
fn write_auth_input(
    file: &Path,
    path: &Path,
    username: Option<&str>,
    password: Option<&str>,
    remember: bool,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    write_units(
        &mut bytes,
        &path.as_os_str().encode_wide().collect::<Vec<_>>(),
    )?;
    write_optional_units(
        &mut bytes,
        username.map(|value| OsStr::new(value).encode_wide().collect::<Vec<_>>()),
    )?;
    let mut password_units =
        password.map(|value| OsStr::new(value).encode_wide().collect::<Vec<_>>());
    write_optional_units_ref(&mut bytes, password_units.as_deref())?;
    bytes.push(u8::from(remember));
    let result = protect_bytes(&bytes).and_then(|protected| std::fs::write(file, protected));
    clear_bytes(&mut bytes);
    if let Some(password_units) = &mut password_units {
        password_units.fill(0);
    }
    result
}

#[cfg(windows)]
fn read_auth_input(file: &Path) -> io::Result<(PathBuf, Option<String>, Option<String>, bool)> {
    let protected = std::fs::read(file)?;
    let mut bytes = unprotect_bytes(&protected)?;
    let result = (|| {
        let mut offset = 0;
        let path = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
        let username = read_optional_units(&bytes, &mut offset)?
            .map(|units| String::from_utf16(&units))
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid username"))?;
        let mut password_units = read_optional_units(&bytes, &mut offset)?;
        let password = password_units
            .as_deref()
            .map(String::from_utf16)
            .transpose()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid password"))?;
        if let Some(password_units) = &mut password_units {
            password_units.fill(0);
        }
        let remember = read_byte(&bytes, &mut offset)? != 0;
        if offset != bytes.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing authentication input data",
            ));
        }
        Ok((path, username, password, remember))
    })();
    clear_bytes(&mut bytes);
    result
}

#[cfg(windows)]
fn protect_bytes(bytes: &[u8]) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData},
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "authentication input too large",
            )
        })?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    if unsafe {
        CryptProtectData(
            &input,
            ptr::null(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let protected =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe { LocalFree(output.pbData as _) };
    Ok(protected)
}

#[cfg(windows)]
fn unprotect_bytes(bytes: &[u8]) -> io::Result<Vec<u8>> {
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{
            CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
        },
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "authentication input too large",
            )
        })?,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    if unsafe {
        CryptUnprotectData(
            &input,
            ptr::null_mut(),
            ptr::null(),
            ptr::null(),
            ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut output,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let unprotected =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
    unsafe {
        std::ptr::write_bytes(output.pbData, 0, output.cbData as usize);
        LocalFree(output.pbData as _);
    }
    Ok(unprotected)
}
#[cfg(windows)]
fn write_auth_result(
    file: &Path,
    result: Result<NetworkResult, NetworkAuthError>,
) -> io::Result<()> {
    let (success, code) = match result {
        Ok(result) => (1_u8, result.code),
        Err(error) => (0_u8, error.code),
    };
    let mut bytes = Vec::with_capacity(5);
    bytes.push(success);
    bytes.extend_from_slice(&code.to_le_bytes());
    std::fs::write(file, bytes)
}

#[cfg(windows)]
fn read_auth_result(file: &Path) -> io::Result<Result<NetworkResult, NetworkAuthError>> {
    let bytes = std::fs::read(file)?;
    if bytes.len() != 5 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid authentication result",
        ));
    }
    let code = u32::from_le_bytes(bytes[1..5].try_into().unwrap());
    match bytes[0] {
        0 => Ok(Err(network_auth_error(code))),
        1 => Ok(Ok(NetworkResult { code })),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid authentication result status",
        )),
    }
}

#[cfg(windows)]
fn write_optional_units(bytes: &mut Vec<u8>, value: Option<Vec<u16>>) -> io::Result<()> {
    bytes.push(u8::from(value.is_some()));
    if let Some(value) = value {
        write_units(bytes, &value)?;
    }
    Ok(())
}
#[cfg(windows)]
fn write_optional_units_ref(bytes: &mut Vec<u8>, value: Option<&[u16]>) -> io::Result<()> {
    bytes.push(u8::from(value.is_some()));
    if let Some(value) = value {
        write_units(bytes, value)?;
    }
    Ok(())
}

#[cfg(windows)]
fn read_optional_units(bytes: &[u8], offset: &mut usize) -> io::Result<Option<Vec<u16>>> {
    if read_byte(bytes, offset)? == 0 {
        Ok(None)
    } else {
        read_units_at(bytes, offset).map(Some)
    }
}
#[cfg(windows)]
fn write_directory_input(path: &Path, value: &Path, visibility: FileVisibility) -> io::Result<()> {
    let units: Vec<u16> = value.as_os_str().encode_wide().collect();
    if units.len() > MAX_HELPER_UTF16_UNITS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
    }
    let mut bytes = Vec::with_capacity(6 + units.len() * 2);
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes.push(u8::from(visibility.show_hidden));
    bytes.push(u8::from(visibility.show_system));
    std::fs::write(path, bytes)
}

#[cfg(windows)]
fn read_directory_input(path: &Path) -> io::Result<(PathBuf, FileVisibility)> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    let value = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
    let show_hidden = read_byte(&bytes, &mut offset)? != 0;
    let show_system = read_byte(&bytes, &mut offset)? != 0;
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing directory input data",
        ));
    }
    Ok((
        value,
        FileVisibility {
            show_hidden,
            show_system,
        },
    ))
}

#[cfg(windows)]
fn write_file_mutation_input(
    path: &Path,
    kind: IsolatedFileMutationKind,
    source: Option<&Path>,
    destination: Option<&Path>,
) -> io::Result<()> {
    let mut bytes = vec![match kind {
        IsolatedFileMutationKind::CreateFolder => 0,
        IsolatedFileMutationKind::Rename => 1,
    }];
    write_optional_path(&mut bytes, source)?;
    write_optional_path(&mut bytes, destination)?;
    std::fs::write(path, bytes)
}

#[cfg(windows)]
fn read_file_mutation_input(
    path: &Path,
) -> io::Result<(IsolatedFileMutationKind, Option<PathBuf>, Option<PathBuf>)> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    let kind = match read_byte(&bytes, &mut offset)? {
        0 => IsolatedFileMutationKind::CreateFolder,
        1 => IsolatedFileMutationKind::Rename,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid file mutation kind",
            ));
        }
    };
    let source = read_optional_path(&bytes, &mut offset)?;
    let destination = read_optional_path(&bytes, &mut offset)?;
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing file mutation input data",
        ));
    }
    Ok((kind, source, destination))
}

#[cfg(windows)]
fn execute_file_mutation(
    kind: IsolatedFileMutationKind,
    source: Option<&Path>,
    destination: Option<&Path>,
) -> io::Result<IsolatedFileMutationResult> {
    let mut affected_directories = Vec::new();
    let completed_path = match kind {
        IsolatedFileMutationKind::CreateFolder => {
            let destination = destination.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing destination")
            })?;
            let parent = destination.parent().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing destination parent")
            })?;
            let name = destination.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing destination name")
            })?;
            Some(
                crate::fs::file_operations::create_folder(parent, name)
                    .map_err(|error| io::Error::other(format!("{error:?}")))?,
            )
        }
        IsolatedFileMutationKind::Rename => {
            let source = source
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing source"))?;
            let destination = destination.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing destination")
            })?;
            let name = destination.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing destination name")
            })?;
            let destination = crate::fs::file_operations::rename_path(source, name)
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            if let Some(parent) = source.parent() {
                affected_directories.push(parent.to_path_buf());
            }
            Some(destination)
        }
    };
    let affected_path = completed_path.as_deref().or(source);
    if let Some(parent) = affected_path.and_then(Path::parent)
        && !affected_directories.contains(&parent.to_path_buf())
    {
        affected_directories.push(parent.to_path_buf());
    }
    Ok(IsolatedFileMutationResult {
        completed_path,
        affected_directories,
    })
}

#[cfg(windows)]
fn write_file_mutation_result(path: &Path, result: &IsolatedFileMutationResult) -> io::Result<()> {
    let mut bytes = Vec::new();
    write_optional_path(&mut bytes, result.completed_path.as_deref())?;
    bytes.extend_from_slice(
        &u32::try_from(result.affected_directories.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many affected paths"))?
            .to_le_bytes(),
    );
    for affected in &result.affected_directories {
        write_units(
            &mut bytes,
            &affected.as_os_str().encode_wide().collect::<Vec<_>>(),
        )?;
    }
    std::fs::write(path, bytes)
}

#[cfg(windows)]
fn read_file_mutation_result(path: &Path) -> io::Result<IsolatedFileMutationResult> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    let completed_path = read_optional_path(&bytes, &mut offset)?;
    let count = read_u32(&bytes, &mut offset)? as usize;
    if count > 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many affected paths",
        ));
    }
    let mut affected_directories = Vec::with_capacity(count);
    for _ in 0..count {
        affected_directories.push(PathBuf::from(OsString::from_wide(&read_units_at(
            &bytes,
            &mut offset,
        )?)));
    }
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing file mutation result data",
        ));
    }
    Ok(IsolatedFileMutationResult {
        completed_path,
        affected_directories,
    })
}

#[cfg(windows)]
fn write_optional_path(bytes: &mut Vec<u8>, value: Option<&Path>) -> io::Result<()> {
    bytes.push(u8::from(value.is_some()));
    if let Some(value) = value {
        write_units(bytes, &value.as_os_str().encode_wide().collect::<Vec<_>>())?;
    }
    Ok(())
}

#[cfg(windows)]
fn read_optional_path(bytes: &[u8], offset: &mut usize) -> io::Result<Option<PathBuf>> {
    if read_byte(bytes, offset)? == 0 {
        Ok(None)
    } else {
        read_units_at(bytes, offset).map(|units| Some(PathBuf::from(OsString::from_wide(&units))))
    }
}

#[cfg(windows)]
fn enumerate_directory(
    path: &Path,
    visibility: FileVisibility,
) -> io::Result<(Vec<FileEntry>, usize, bool)> {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
    const FILE_ATTRIBUTE_SYSTEM: u32 = 0x4;

    let mut entries = Vec::new();
    let mut skipped = 0_usize;
    let mut truncated = false;
    for result in std::fs::read_dir(path)? {
        if entries.len() >= MAX_HELPER_ITEMS {
            truncated = true;
            break;
        }
        let directory_entry = match result {
            Ok(entry) => entry,
            Err(_) => {
                skipped = skipped.saturating_add(1);
                continue;
            }
        };
        let metadata = match directory_entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => {
                skipped = skipped.saturating_add(1);
                continue;
            }
        };
        let attributes = metadata.file_attributes();
        if (!visibility.show_hidden && attributes & FILE_ATTRIBUTE_HIDDEN != 0)
            || (!visibility.show_system && attributes & FILE_ATTRIBUTE_SYSTEM != 0)
        {
            continue;
        }
        let entry_path = directory_entry.path();
        let original_name = directory_entry.file_name();
        let kind = if metadata.is_dir() {
            EntryKind::Directory
        } else if metadata.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };
        entries.push(FileEntry {
            id: EntryId(entries.len().saturating_add(1).min(u32::MAX as usize) as u32),
            display_name: original_name.to_string_lossy().into_owned(),
            name_highlights: Vec::new(),
            original_name,
            path: entry_path.clone(),
            kind,
            open_target: None,
            parent_display: entry_path
                .parent()
                .map(|value| value.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default(),
            size_bytes: metadata.is_file().then_some(metadata.len()),
            folder_size: FolderSizeState::NotIndexed,
            modified: metadata.modified().ok(),
            created: metadata.created().ok(),
        });
    }
    Ok((entries, skipped, truncated))
}

#[cfg(windows)]
fn write_directory_result(path: &Path, result: &(Vec<FileEntry>, usize, bool)) -> io::Result<()> {
    let (entries, skipped, truncated) = result;
    if entries.len() > MAX_HELPER_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many helper items",
        ));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&u64::try_from(*skipped).unwrap_or(u64::MAX).to_le_bytes());
    bytes.push(u8::from(*truncated));
    for entry in entries {
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
        if bytes.len() > MAX_HELPER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "helper result too large",
            ));
        }
    }
    std::fs::write(path, bytes)
}

#[cfg(windows)]
fn read_directory_result(path: &Path) -> io::Result<(Vec<FileEntry>, usize, bool)> {
    let bytes = std::fs::read(path)?;
    if bytes.len() > MAX_HELPER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper result too large",
        ));
    }
    let mut offset = 0;
    let count = read_u32(&bytes, &mut offset)? as usize;
    if count > MAX_HELPER_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many helper items",
        ));
    }
    let skipped = usize::try_from(read_u64(&bytes, &mut offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid skipped count"))?;
    let truncated = read_byte(&bytes, &mut offset)? != 0;
    let mut entries = Vec::with_capacity(count);
    for index in 0..count {
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
            id: EntryId(index.saturating_add(1).min(u32::MAX as usize) as u32),
            display_name: original_name.to_string_lossy().into_owned(),
            name_highlights: Vec::new(),
            original_name,
            path: entry_path.clone(),
            kind,
            open_target: None,
            parent_display: entry_path
                .parent()
                .map(|value| value.as_os_str().to_string_lossy().into_owned())
                .unwrap_or_default(),
            size_bytes,
            folder_size: FolderSizeState::NotIndexed,
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
    Ok((entries, skipped, truncated))
}

#[cfg(windows)]
fn write_units(bytes: &mut Vec<u8>, units: &[u16]) -> io::Result<()> {
    if units.len() > MAX_HELPER_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "helper item too long",
        ));
    }
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    Ok(())
}

#[cfg(windows)]
fn write_optional_u64(bytes: &mut Vec<u8>, value: Option<u64>) {
    bytes.push(u8::from(value.is_some()));
    if let Some(value) = value {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
}

#[cfg(windows)]
fn read_optional_u64(bytes: &[u8], offset: &mut usize) -> io::Result<Option<u64>> {
    if read_byte(bytes, offset)? == 0 {
        Ok(None)
    } else {
        read_u64(bytes, offset).map(Some)
    }
}

#[cfg(windows)]
fn write_system_time(bytes: &mut Vec<u8>, value: Option<SystemTime>) -> io::Result<()> {
    let duration = value.and_then(|value| value.duration_since(SystemTime::UNIX_EPOCH).ok());
    match duration {
        None => bytes.push(0),
        Some(duration) => {
            bytes.push(1);
            bytes.extend_from_slice(&duration.as_secs().to_le_bytes());
            bytes.extend_from_slice(&duration.subsec_nanos().to_le_bytes());
        }
    }
    Ok(())
}

#[cfg(windows)]
fn read_system_time(bytes: &[u8], offset: &mut usize) -> io::Result<Option<SystemTime>> {
    if read_byte(bytes, offset)? == 0 {
        return Ok(None);
    }
    let seconds = read_u64(bytes, offset)?;
    let nanos = read_u32(bytes, offset)?;
    if nanos >= 1_000_000_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid nanoseconds",
        ));
    }
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::new(seconds, nanos))
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "time out of range"))
}
#[cfg(windows)]
const MAX_HELPER_ITEMS: usize = 4096;
#[cfg(windows)]
const MAX_HELPER_UTF16_UNITS: usize = 32767;
#[cfg(windows)]
const MAX_HELPER_BYTES: usize = 64 * 1024 * 1024;
#[cfg(windows)]
fn write_utf16_path(path: &Path, value: &Path) -> io::Result<()> {
    let units: Vec<u16> = value.as_os_str().encode_wide().collect();
    if units.len() > MAX_HELPER_UTF16_UNITS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "path too long"));
    }
    let mut bytes = Vec::with_capacity(4 + units.len() * 2);
    bytes.extend_from_slice(&(units.len() as u32).to_le_bytes());
    for unit in units {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    std::fs::write(path, bytes)
}
#[cfg(windows)]
fn read_utf16_path(path: &Path) -> io::Result<PathBuf> {
    let bytes = std::fs::read(path)?;
    let units = read_units(&bytes)?;
    Ok(PathBuf::from(OsString::from_wide(&units)))
}
#[cfg(windows)]
fn write_result(path: &Path, items: &[(String, PathBuf, bool)]) -> io::Result<()> {
    if items.len() > MAX_HELPER_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many helper items",
        ));
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(items.len() as u32).to_le_bytes());
    for (label, target, is_directory) in items {
        let label_units: Vec<u16> = label.encode_utf16().collect();
        let target_units: Vec<u16> = target.as_os_str().encode_wide().collect();
        if label_units.len() > MAX_HELPER_UTF16_UNITS || target_units.len() > MAX_HELPER_UTF16_UNITS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "helper item too long",
            ));
        }
        bytes.extend_from_slice(&(label_units.len() as u32).to_le_bytes());
        for unit in label_units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&(target_units.len() as u32).to_le_bytes());
        for unit in target_units {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.push(u8::from(*is_directory));
    }
    std::fs::write(path, bytes)
}
#[cfg(windows)]
fn read_result(path: &Path) -> io::Result<Vec<(String, PathBuf, bool)>> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    let count = read_u32(&bytes, &mut offset)? as usize;
    if count > MAX_HELPER_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "too many helper items",
        ));
    }
    let mut result = Vec::with_capacity(count);
    for _ in 0..count {
        let label = String::from_utf16(&read_units_at(&bytes, &mut offset)?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid UTF-16 label"))?;
        let target = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
        let is_directory = *bytes.get(offset).ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "missing directory flag")
        })? != 0;
        offset += 1;
        result.push((label, target, is_directory));
    }
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing network helper data",
        ));
    }
    Ok(result)
}
#[cfg(windows)]
fn read_units(bytes: &[u8]) -> io::Result<Vec<u16>> {
    let mut offset = 0;
    read_units_at(bytes, &mut offset).and_then(|units| {
        if offset == bytes.len() {
            Ok(units)
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trailing path data",
            ))
        }
    })
}
#[cfg(windows)]
fn read_units_at(bytes: &[u8], offset: &mut usize) -> io::Result<Vec<u16>> {
    let count = read_u32(bytes, offset)? as usize;
    if count > MAX_HELPER_UTF16_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "UTF-16 value too long",
        ));
    }
    let end = offset
        .checked_add(
            count
                .checked_mul(2)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path too long"))?,
        )
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "path too long"))?;
    if end > bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "truncated UTF-16 data",
        ));
    }
    let mut units = Vec::with_capacity(count);
    while *offset < end {
        units.push(u16::from_le_bytes([bytes[*offset], bytes[*offset + 1]]));
        *offset += 2;
    }
    Ok(units)
}
#[cfg(windows)]
fn read_byte(bytes: &[u8], offset: &mut usize) -> io::Result<u8> {
    let value = *bytes
        .get(*offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated byte"))?;
    *offset += 1;
    Ok(value)
}

#[cfg(windows)]
fn read_u64(bytes: &[u8], offset: &mut usize) -> io::Result<u64> {
    let end = offset
        .checked_add(8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid length"))?;
    let value = bytes
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated number"))?;
    *offset = end;
    Ok(u64::from_le_bytes(value.try_into().unwrap()))
}
#[cfg(windows)]
fn read_u32(bytes: &[u8], offset: &mut usize) -> io::Result<u32> {
    let end = offset
        .checked_add(4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid length"))?;
    let value = bytes
        .get(*offset..end)
        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "truncated length"))?;
    *offset = end;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

#[cfg(test)]
mod isolated_codec_tests {
    #[test]
    fn utf16_codec_helpers_are_round_trip_safe_on_windows() {
        #[cfg(windows)]
        {
            let path = std::path::PathBuf::from(r"\\服务器\共享");
            let file =
                std::env::temp_dir().join(format!("asterfiles-codec-{}", std::process::id()));
            super::write_utf16_path(&file, &path).unwrap();
            assert_eq!(super::read_utf16_path(&file).unwrap(), path);
            let _ = std::fs::remove_file(file);
        }
    }

    #[cfg(windows)]
    #[test]
    fn authentication_input_and_result_codecs_round_trip() {
        let input_file =
            std::env::temp_dir().join(format!("asterfiles-auth-input-{}", std::process::id()));
        let output_file =
            std::env::temp_dir().join(format!("asterfiles-auth-output-{}", std::process::id()));
        let path = std::path::PathBuf::from(r"\\服务器\共享\目录");
        super::write_auth_input(&input_file, &path, Some(r"域\用户"), Some("秘密"), true).unwrap();
        let protected = std::fs::read(&input_file).unwrap();
        let secret_bytes = "秘密"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert!(
            !protected
                .windows(secret_bytes.len())
                .any(|window| window == secret_bytes)
        );
        assert_eq!(
            super::read_auth_input(&input_file).unwrap(),
            (
                path,
                Some(r"域\用户".to_owned()),
                Some("秘密".to_owned()),
                true
            )
        );
        let error = super::network_auth_error(
            windows_sys::Win32::Foundation::ERROR_SESSION_CREDENTIAL_CONFLICT,
        );
        super::write_auth_result(&output_file, Err(error)).unwrap();
        assert_eq!(super::read_auth_result(&output_file).unwrap(), Err(error));
        let _ = std::fs::remove_file(input_file);
        let _ = std::fs::remove_file(output_file);
    }
    #[cfg(windows)]
    #[test]
    fn file_mutation_codecs_preserve_raw_paths() {
        use std::os::windows::ffi::OsStringExt;

        let source = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[
            b'\\' as u16,
            b'\\' as u16,
            b's' as u16,
            0xd800,
        ]));
        let destination = source.join("renamed");
        let input_file =
            std::env::temp_dir().join(format!("asterfiles-mutation-input-{}", std::process::id()));
        let output_file =
            std::env::temp_dir().join(format!("asterfiles-mutation-output-{}", std::process::id()));
        super::write_file_mutation_input(
            &input_file,
            super::IsolatedFileMutationKind::Rename,
            Some(&source),
            Some(&destination),
        )
        .unwrap();
        assert_eq!(
            super::read_file_mutation_input(&input_file).unwrap(),
            (
                super::IsolatedFileMutationKind::Rename,
                Some(source.clone()),
                Some(destination.clone())
            )
        );
        let result = super::IsolatedFileMutationResult {
            completed_path: Some(destination.clone()),
            affected_directories: vec![source],
        };
        super::write_file_mutation_result(&output_file, &result).unwrap();
        assert_eq!(
            super::read_file_mutation_result(&output_file).unwrap(),
            result
        );
        let _ = std::fs::remove_file(input_file);
        let _ = std::fs::remove_file(output_file);
    }
    #[cfg(windows)]
    #[test]
    fn directory_input_preserves_visibility_and_raw_path() {
        use std::os::windows::ffi::OsStringExt;

        let path = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[
            b'\\' as u16,
            b'\\' as u16,
            0xd800,
            b'\\' as u16,
            b'x' as u16,
        ]));
        let visibility = crate::domain::FileVisibility {
            show_hidden: false,
            show_system: true,
        };
        let file =
            std::env::temp_dir().join(format!("asterfiles-directory-input-{}", std::process::id()));
        super::write_directory_input(&file, &path, visibility).unwrap();
        assert_eq!(
            super::read_directory_input(&file).unwrap(),
            (path, visibility)
        );
        let _ = std::fs::remove_file(file);
    }

    #[cfg(windows)]
    #[test]
    fn directory_result_preserves_explicit_truncation() {
        let file = std::env::temp_dir().join(format!(
            "asterfiles-directory-truncated-{}",
            std::process::id()
        ));
        super::write_directory_result(&file, &(Vec::new(), 0, true)).unwrap();
        let (entries, skipped, truncated) = super::read_directory_result(&file).unwrap();
        assert!(entries.is_empty());
        assert_eq!(skipped, 0);
        assert!(truncated);
        let _ = std::fs::remove_file(file);
    }
    #[cfg(windows)]
    #[test]
    fn directory_result_preserves_non_unicode_identity_and_metadata() {
        use std::os::windows::ffi::OsStringExt;

        let original_name = std::ffi::OsString::from_wide(&[b'a' as u16, 0xd800]);
        let entry_path = std::path::PathBuf::from(r"\\server\share").join(&original_name);
        let modified = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::new(42, 7);
        let entries = vec![crate::domain::FileEntry {
            id: crate::domain::EntryId(99),
            original_name: original_name.clone(),
            display_name: "ignored presentation".into(),
            name_highlights: vec![],
            path: entry_path.clone(),
            kind: crate::domain::EntryKind::File,
            open_target: None,
            parent_display: String::new(),
            size_bytes: Some(123),
            folder_size: crate::domain::FolderSizeState::Unknown,
            modified: Some(modified),
            created: None,
        }];
        let file = std::env::temp_dir().join(format!(
            "asterfiles-directory-result-{}",
            std::process::id()
        ));
        super::write_directory_result(&file, &(entries, 5, false)).unwrap();
        let (decoded, skipped, truncated) = super::read_directory_result(&file).unwrap();
        assert!(!truncated);
        assert_eq!(skipped, 5);
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].id, crate::domain::EntryId(1));
        assert_eq!(decoded[0].original_name, original_name);
        assert_eq!(decoded[0].path, entry_path);
        assert_eq!(decoded[0].kind, crate::domain::EntryKind::File);
        assert_eq!(decoded[0].size_bytes, Some(123));
        assert_eq!(decoded[0].modified, Some(modified));
        assert_eq!(decoded[0].created, None);
        let _ = std::fs::remove_file(file);
    }
}

#[cfg(all(test, windows))]
mod live_import_tests {
    use super::*;

    #[test]
    #[ignore = "requires the current user's Windows Explorer Network Shortcuts"]
    fn explorer_network_shortcuts_resolve_target_links_when_present() {
        let locations = enumerate_network_locations().expect("NetHood can be enumerated");
        for location in locations {
            if location.shell_path.join("target.lnk").is_file() {
                let target = location
                    .target
                    .expect("target.lnk resolves to its remote target");
                assert_ne!(target, location.shell_path);
            }
        }
    }
}

#[cfg(all(test, windows))]
mod host_display_tests {
    use super::*;

    #[test]
    fn imported_device_host_preserves_windows_casing() {
        assert_eq!(
            unc_host_display_name(Path::new(r"\\LiuYanghomeNAS\Multimedia")),
            Some("LiuYanghomeNAS".to_owned())
        );
    }
}
