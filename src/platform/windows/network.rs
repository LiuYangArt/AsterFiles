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
#[cfg(windows)]
use crate::fs::file_operations::FileProgressKind;

pub fn record_runtime_event(event: &str) {
    crate::operation_audit::record(event, "source=network_runtime");
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

pub fn network_locations_folder() -> io::Result<PathBuf> {
    known_folder_path(&FOLDERID_NETHOOD)
}

fn explorer_network_location_path_in(root: &Path, shell_path: &Path) -> io::Result<PathBuf> {
    let canonical_root = std::fs::canonicalize(root)?;
    let canonical_item = std::fs::canonicalize(shell_path)?;
    if canonical_item.parent() != Some(canonical_root.as_path()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "network location is not a direct Explorer NetHood item",
        ));
    }
    Ok(canonical_item)
}

fn explorer_network_location_path(shell_path: &Path) -> io::Result<PathBuf> {
    explorer_network_location_path_in(&network_locations_folder()?, shell_path)
}

fn valid_network_location_name(name: &OsStr) -> bool {
    let mut components = Path::new(name).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}
pub fn rename_network_location(shell_path: &Path, name: &OsStr) -> io::Result<PathBuf> {
    if !valid_network_location_name(name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "network location name must be one non-empty path component",
        ));
    }
    let source = explorer_network_location_path(shell_path)?;
    let destination = source
        .parent()
        .expect("validated NetHood item has a parent")
        .join(name);
    std::fs::rename(&source, &destination)?;
    Ok(destination)
}

pub fn remove_network_location(shell_path: &Path) -> io::Result<()> {
    let item = explorer_network_location_path(shell_path)?;
    let metadata = std::fs::symlink_metadata(&item)?;
    if metadata.is_dir() {
        std::fs::remove_dir_all(item)
    } else {
        std::fs::remove_file(item)
    }
}
pub fn enumerate_network_locations() -> io::Result<Vec<NetworkLocation>> {
    let _com = ComGuard::new()?;
    let items = enumerate_shell_folder(&network_locations_folder()?)?;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolatedNetworkOperationKind {
    Copy,
    Move,
    PermanentDelete,
    Recycle,
}

#[cfg(windows)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolatedNetworkOperationReport {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    pub skipped: Vec<PathBuf>,
    pub affected_directories: Vec<PathBuf>,
    pub completed_paths: Vec<PathBuf>,
    pub aborted: bool,
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
pub fn isolated_network_file_operation(
    kind: IsolatedNetworkOperationKind,
    source: &Path,
    destination: Option<&Path>,
    cancel: &crate::domain::file_operations::CancellationToken,
    resolve_conflict: &mut dyn FnMut(
        crate::domain::file_operations::ConflictCategory,
        &Path,
        &Path,
    ) -> crate::domain::file_operations::ConflictAction,
    discovered: &mut dyn FnMut(u64, &Path),
    progress: &mut dyn FnMut(u64, FileProgressKind, &Path),
    scan: &mut dyn FnMut(usize, u64, bool),
    recovering: &mut dyn FnMut(bool),
) -> io::Result<IsolatedNetworkOperationReport> {
    recovering(false);
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let base = std::env::temp_dir().join(format!(
        "asterfiles-network-operation-{}-{stamp}",
        std::process::id()
    ));
    let input = base.with_extension("in");
    let output = base.with_extension("out");
    let completed = base.with_extension("complete");
    let event = base.with_extension("event");
    let response = base.with_extension("response");
    let diagnostic = base.with_extension("stderr");
    let control = base.with_extension("control");
    let acknowledgement = base.with_extension("ack");
    let scan_path = base.with_extension("scan");
    let progress_path = base.with_extension("progress");
    let temporary_copy = base.with_extension("temporary-copy");
    write_network_operation_input(&input, kind, source, destination)?;
    let result = (|| {
        use std::os::windows::process::CommandExt;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg(CHILD_PREFIX)
            .arg(if operation_probe::is_stall_probe() {
                "network-operation-probe-stall"
            } else {
                "network-operation"
            })
            .arg(&input)
            .arg(&output)
            .creation_flags(0x08000000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(std::fs::File::create(&diagnostic)?));
        let mut child = command.spawn()?;
        let job = match KillOnCloseJob::create() {
            Ok(job) => job,
            Err(error) => {
                let _ = child.kill();

                return Err(error);
            }
        };
        if let Err(error) = job.assign(&child) {
            let _ = child.kill();

            return Err(error);
        }
        let result = (|| {
            let mut last_sequence = 0_u64;
            let mut last_progress = NetworkOperationProgress::default();
            let mut control_sequence = 0_u64;
            let mut sent_control = 0_u8;
            let mut last_scan = None;
            let mut cancelling_since = None;
            let mut last_rate_tick = std::time::Instant::now();
            loop {
                let desired_control = if cancel.is_cancelled() {
                    2
                } else {
                    u8::from(cancel.is_paused())
                };
                if desired_control != sent_control {
                    control_sequence += 1;
                    let mut bytes = control_sequence.to_le_bytes().to_vec();
                    bytes.push(desired_control);
                    atomic_write(&control, &bytes)?;
                    sent_control = desired_control;
                    if desired_control == 2 {
                        cancelling_since = Some(std::time::Instant::now());
                    }
                }
                if sent_control == 1
                    && available_snapshot(read_snapshot(&acknowledgement))?
                        .is_some_and(|bytes| bytes == control_sequence.to_le_bytes())
                {
                    cancel.acknowledge_pause();
                }
                if let Some(bytes) = available_snapshot(read_snapshot(&scan_path))?
                    && bytes.len() == 17
                    && last_scan.as_ref() != Some(&bytes)
                {
                    let files = u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize;
                    let count = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
                    scan(files, count, bytes[16] != 0);
                    last_scan = Some(bytes);
                }
                if let Some(snapshot) =
                    available_snapshot(read_network_operation_progress(&progress_path))?
                    && snapshot != last_progress
                {
                    deliver_network_operation_progress(
                        &snapshot,
                        &last_progress,
                        discovered,
                        progress,
                        recovering,
                    );
                    last_progress = snapshot;
                }
                if last_rate_tick.elapsed() >= Duration::from_secs(1) {
                    let current = if last_progress.current_path.as_os_str().is_empty() {
                        source
                    } else {
                        &last_progress.current_path
                    };
                    progress(0, FileProgressKind::Transferred, current);
                    last_rate_tick = std::time::Instant::now();
                }
                if let Some((
                    sequence,
                    NetworkOperationEvent::Conflict {
                        category,
                        source,
                        destination,
                    },
                )) = available_snapshot(read_network_operation_event(&event))?
                    && sequence > last_sequence
                {
                    last_sequence = sequence;
                    let action = resolve_conflict(category, &source, &destination);
                    write_network_operation_response(&response, sequence, action)?;
                }
                let status = child.try_wait()?;
                let result_ready = available_snapshot(read_snapshot(&completed))?
                    .is_some_and(|bytes| bytes == [1]);
                if status.is_some() || result_ready {
                    if let Some(snapshot) =
                        available_snapshot(read_network_operation_progress(&progress_path))?
                    {
                        deliver_network_operation_progress(
                            &snapshot,
                            &last_progress,
                            discovered,
                            progress,
                            recovering,
                        );
                    }
                    if let Some(bytes) = available_snapshot(read_snapshot(&scan_path))?
                        && bytes.len() == 17
                    {
                        scan(
                            u64::from_le_bytes(bytes[..8].try_into().unwrap()) as usize,
                            u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
                            bytes[16] != 0,
                        );
                    }
                    if cancel.is_cancelled() {
                        return Err(io::Error::new(
                            io::ErrorKind::Interrupted,
                            "network operation helper cancelled",
                        ));
                    }
                    if let Some(status) = status
                        && !status.success()
                        && !result_ready
                    {
                        let detail = std::fs::read_to_string(&diagnostic).unwrap_or_default();
                        return Err(io::Error::other(format!(
                            "network operation helper exited with {status}: {}\n{}",
                            source.display(),
                            detail.trim()
                        )));
                    }
                    return read_network_operation_result(&output);
                }
                if cancelling_since
                    .is_some_and(|started| started.elapsed() >= Duration::from_secs(1))
                {
                    let _ = child.kill();

                    return Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "network operation helper cancelled",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })();
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        drop(job);
        let cleanup =
            temporary_copy::cleanup_registered(&temporary_copy, child.id(), result.is_ok());
        if matches!(cleanup, Ok(true)) {
            let _ = std::fs::remove_file(&temporary_copy);
        }
        if let Err(cleanup) = cleanup {
            record_runtime_event(&format!(
                "temporary_copy_cleanup_failed code={:?}",
                cleanup.raw_os_error()
            ));
            return Err(match result {
                Err(error) => io::Error::new(error.kind(), format!("{error}; {cleanup}")),
                Ok(_) => cleanup,
            });
        }
        result
    })();
    for path in [
        &input,
        &output,
        &completed,
        &event,
        &response,
        &diagnostic,
        &control,
        &acknowledgement,
        &scan_path,
        &progress_path,
    ] {
        let _ = std::fs::remove_file(path);
    }
    recovering(false);
    result
}

#[cfg(windows)]
mod operation_probe;
#[cfg(windows)]
pub use operation_probe::try_run_operation_probe_from_args;

#[cfg(windows)]
#[derive(Debug)]
enum NetworkOperationEvent {
    Conflict {
        category: crate::domain::file_operations::ConflictCategory,
        source: PathBuf,
        destination: PathBuf,
    },
}
#[cfg(windows)]
mod directory;
#[cfg(windows)]
pub use directory::isolated_directory;
#[cfg(all(windows, test))]
pub(crate) use directory::test_directory_stream_started;

#[cfg(windows)]
pub(crate) struct KillOnCloseJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl KillOnCloseJob {
    pub(crate) fn create() -> io::Result<Self> {
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

    pub(crate) fn assign(&self, child: &std::process::Child) -> io::Result<()> {
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
        directory::run_child(&path, visibility, &output)?;
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
    if mode == "network-operation" || mode == "network-operation-probe-stall" {
        // This dedicated diagnostic mode models a source blocked before its first byte.
        if mode == "network-operation-probe-stall" {
            std::thread::sleep(Duration::from_secs(15));
        }
        let (kind, source, destination) = read_network_operation_input(&input)?;
        let event = input.with_extension("event");
        let response = input.with_extension("response");
        let result =
            execute_network_operation(kind, &source, destination.as_deref(), &event, &response);
        write_network_operation_result(&output, &result)?;
        // A finished task can outlive its read-only SMB worker during kernel cleanup.
        atomic_write(&output.with_extension("complete"), &[1])?;
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
fn write_network_operation_input(
    path: &Path,
    kind: IsolatedNetworkOperationKind,
    source: &Path,
    destination: Option<&Path>,
) -> io::Result<()> {
    let mut bytes = vec![match kind {
        IsolatedNetworkOperationKind::Copy => 0,
        IsolatedNetworkOperationKind::Move => 1,
        IsolatedNetworkOperationKind::PermanentDelete => 2,
        IsolatedNetworkOperationKind::Recycle => 3,
    }];
    write_units(
        &mut bytes,
        &source.as_os_str().encode_wide().collect::<Vec<_>>(),
    )?;
    write_optional_path(&mut bytes, destination)?;
    std::fs::write(path, bytes)
}

#[cfg(windows)]
fn read_network_operation_input(
    path: &Path,
) -> io::Result<(IsolatedNetworkOperationKind, PathBuf, Option<PathBuf>)> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    let kind = match read_byte(&bytes, &mut offset)? {
        0 => IsolatedNetworkOperationKind::Copy,
        1 => IsolatedNetworkOperationKind::Move,
        2 => IsolatedNetworkOperationKind::PermanentDelete,
        3 => IsolatedNetworkOperationKind::Recycle,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid network operation kind",
            ));
        }
    };
    let source = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
    let destination = read_optional_path(&bytes, &mut offset)?;
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing network operation input data",
        ));
    }
    Ok((kind, source, destination))
}

#[cfg(windows)]
fn network_operation_error(error: crate::fs::file_operations::OperationError) -> io::Error {
    use crate::fs::file_operations::OperationError;
    match error {
        OperationError::Io {
            path,
            kind,
            message,
        } => io::Error::new(kind, format!("{}: {message}", path.display())),
        OperationError::SourceInsideDestination => io::Error::other("SourceInsideDestination"),
        OperationError::Cancelled => io::Error::from(io::ErrorKind::Interrupted),
        OperationError::DestinationExists(path) => io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{}: {}",
                path.display(),
                io::Error::from(io::ErrorKind::AlreadyExists)
            ),
        ),
        OperationError::ConflictSkipped(path) => io::Error::new(
            io::ErrorKind::Interrupted,
            format!(
                "{}: {}",
                path.display(),
                io::Error::from(io::ErrorKind::Interrupted)
            ),
        ),
        OperationError::InvalidName(_) => io::Error::from(io::ErrorKind::InvalidInput),
        OperationError::DestinationCommittedSourceRetained {
            source,
            destination,
            message,
        } => io::Error::other(format!(
            "{} → {}: {message}",
            source.display(),
            destination.display()
        )),
    }
}
#[cfg(windows)]
fn execute_network_operation(
    kind: IsolatedNetworkOperationKind,
    source: &Path,
    destination: Option<&Path>,
    event_path: &Path,
    response_path: &Path,
) -> io::Result<IsolatedNetworkOperationReport> {
    use crate::domain::file_operations::{CancellationToken, ConflictAction};

    let _temporary_registration =
        temporary_copy::Registration::new(event_path.with_extension("temporary-copy"));
    let cancel = CancellationToken::new();
    let scan = crate::fs::file_operations::CopyScanProgress::default();
    let shared_progress =
        std::sync::Arc::new(std::sync::Mutex::new(NetworkOperationProgress::default()));
    let copy_progress_registration = CopyProgressRegistration::new(shared_progress.clone());
    let mut monitor = NetworkOperationMonitor::start(
        event_path,
        cancel.clone(),
        scan.clone(),
        shared_progress.clone(),
    );
    let ipc_error = monitor.error.clone();
    let operation = (|| {
        let sequence = std::cell::Cell::new(0_u64);
        let mut conflict = |category, source: &Path, destination: &Path| {
            sequence.set(sequence.get().saturating_add(1));
            if let Err(error) = write_network_operation_event(
                event_path,
                sequence.get(),
                &NetworkOperationEvent::Conflict {
                    category,
                    source: source.to_path_buf(),
                    destination: destination.to_path_buf(),
                },
            ) {
                ipc_error
                    .lock()
                    .expect("IPC error lock")
                    .get_or_insert(error);
                cancel.cancel();
            }
            loop {
                match available_snapshot(read_network_operation_response(response_path)) {
                    Ok(Some((response_sequence, action)))
                        if response_sequence == sequence.get() =>
                    {
                        return action;
                    }
                    Err(error) => {
                        ipc_error
                            .lock()
                            .expect("IPC error lock")
                            .get_or_insert(error);
                        cancel.cancel();
                    }
                    _ => {}
                }
                if cancel.is_cancelled() {
                    return ConflictAction::Skip;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let mut discovered = |bytes, path: &Path| {
            let mut snapshot = shared_progress.lock().expect("operation progress lock");
            snapshot.discovered_files += 1;
            snapshot.discovered_bytes += bytes;
            snapshot.discovered_path = path.to_path_buf();
        };
        let mut progress = |bytes, kind, path: &Path| {
            let mut snapshot = shared_progress.lock().expect("operation progress lock");
            match kind {
                FileProgressKind::Transferred => snapshot.transferred_bytes += bytes,
                FileProgressKind::Completed => {
                    snapshot.transferred_bytes += bytes;
                    snapshot.completed_files += 1;
                }
                FileProgressKind::Skipped => {
                    snapshot.skipped_bytes += bytes;
                    snapshot.skipped_files += 1;
                }
            }
            snapshot.current_path = path.to_path_buf();
        };

        let report = match kind {
            IsolatedNetworkOperationKind::Copy => {
                let destination = destination.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing copy destination")
                })?;
                crate::fs::file_operations::copy_path_with_progress(
                    source,
                    destination,
                    &cancel,
                    &mut conflict,
                    &mut discovered,
                    &mut progress,
                    &mut |_| {},
                    &scan,
                )
                .map_err(network_operation_error)?
            }
            IsolatedNetworkOperationKind::Move => {
                let destination = destination.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "missing move destination")
                })?;
                crate::fs::file_operations::move_path_with_progress(
                    source,
                    destination,
                    &cancel,
                    &mut conflict,
                    &mut discovered,
                    &mut progress,
                )
                .map_err(network_operation_error)?
            }
            IsolatedNetworkOperationKind::PermanentDelete => {
                crate::fs::file_operations::permanently_delete(source, &cancel)
                    .map_err(network_operation_error)?
            }
            IsolatedNetworkOperationKind::Recycle => {
                let result = crate::platform::windows::file_operation::recycle(
                    &[source.to_path_buf()],
                    || false,
                );
                if let Some(error) = result.items.into_iter().find_map(|item| item.result.err()) {
                    return Err(io::Error::other(error));
                }
                return Ok(IsolatedNetworkOperationReport {
                    files: 0,
                    directories: 0,
                    bytes: 0,
                    skipped: Vec::new(),
                    affected_directories: source
                        .parent()
                        .map(Path::to_path_buf)
                        .into_iter()
                        .collect(),
                    completed_paths: vec![source.to_path_buf()],
                    aborted: result.aborted,
                });
            }
        };
        Ok(IsolatedNetworkOperationReport {
            files: report.files,
            directories: report.directories,
            bytes: report.bytes,
            skipped: report.skipped,
            affected_directories: report.affected_directories,
            completed_paths: report.completed_paths,
            aborted: false,
        })
    })();
    drop(copy_progress_registration);
    monitor.finish();
    if let Some(error) = monitor.error.lock().expect("IPC error lock").take() {
        return Err(error);
    }
    operation
}

#[cfg(windows)]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct NetworkOperationProgress {
    discovered_files: u64,
    discovered_bytes: u64,
    transferred_bytes: u64,
    completed_files: u64,
    skipped_files: u64,
    skipped_bytes: u64,
    recovering: bool,
    discovered_path: PathBuf,
    current_path: PathBuf,
}

#[cfg(windows)]
thread_local! {
    static COPY_PROGRESS: std::cell::RefCell<Option<std::sync::Arc<std::sync::Mutex<NetworkOperationProgress>>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(windows)]
struct CopyProgressRegistration(Option<std::sync::Arc<std::sync::Mutex<NetworkOperationProgress>>>);

#[cfg(windows)]
impl CopyProgressRegistration {
    fn new(progress: std::sync::Arc<std::sync::Mutex<NetworkOperationProgress>>) -> Self {
        Self(COPY_PROGRESS.with(|slot| slot.replace(Some(progress))))
    }
}

#[cfg(windows)]
impl Drop for CopyProgressRegistration {
    fn drop(&mut self) {
        set_copy_recovering(false);
        COPY_PROGRESS.with(|slot| slot.replace(self.0.take()));
    }
}

#[cfg(windows)]
pub(crate) fn set_copy_recovering(recovering: bool) {
    COPY_PROGRESS.with(|slot| {
        if let Some(progress) = slot.borrow().as_ref() {
            progress.lock().expect("operation progress lock").recovering = recovering;
        }
    });
}

#[cfg(windows)]
fn write_network_operation_progress(
    path: &Path,
    snapshot: &NetworkOperationProgress,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    for value in [
        snapshot.discovered_files,
        snapshot.discovered_bytes,
        snapshot.transferred_bytes,
        snapshot.completed_files,
        snapshot.skipped_files,
        snapshot.skipped_bytes,
    ] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.push(u8::from(snapshot.recovering));
    for path in [&snapshot.discovered_path, &snapshot.current_path] {
        write_units(
            &mut bytes,
            &path.as_os_str().encode_wide().collect::<Vec<_>>(),
        )?;
    }
    atomic_write(path, &bytes)
}

#[cfg(windows)]
fn read_network_operation_progress(path: &Path) -> io::Result<NetworkOperationProgress> {
    let bytes = read_snapshot(path)?;
    let mut offset = 0;
    let snapshot = NetworkOperationProgress {
        discovered_files: read_u64(&bytes, &mut offset)?,
        discovered_bytes: read_u64(&bytes, &mut offset)?,
        transferred_bytes: read_u64(&bytes, &mut offset)?,
        completed_files: read_u64(&bytes, &mut offset)?,
        skipped_files: read_u64(&bytes, &mut offset)?,
        skipped_bytes: read_u64(&bytes, &mut offset)?,
        recovering: match read_byte(&bytes, &mut offset)? {
            0 => false,
            1 => true,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid copy recovery state",
                ));
            }
        },
        discovered_path: PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?)),
        current_path: PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?)),
    };
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing operation progress data",
        ));
    }
    Ok(snapshot)
}

#[cfg(windows)]
fn deliver_network_operation_progress(
    snapshot: &NetworkOperationProgress,
    previous: &NetworkOperationProgress,
    discovered: &mut dyn FnMut(u64, &Path),
    progress: &mut dyn FnMut(u64, FileProgressKind, &Path),
    recovering: &mut dyn FnMut(bool),
) {
    if snapshot.recovering != previous.recovering {
        recovering(snapshot.recovering);
    }
    let files = snapshot
        .discovered_files
        .saturating_sub(previous.discovered_files);
    for index in 0..files {
        discovered(
            if index == 0 {
                snapshot
                    .discovered_bytes
                    .saturating_sub(previous.discovered_bytes)
            } else {
                0
            },
            &snapshot.discovered_path,
        );
    }
    let transferred = snapshot
        .transferred_bytes
        .saturating_sub(previous.transferred_bytes);
    if transferred > 0 {
        progress(
            transferred,
            FileProgressKind::Transferred,
            &snapshot.current_path,
        );
    }
    for _ in previous.completed_files..snapshot.completed_files {
        progress(0, FileProgressKind::Completed, &snapshot.current_path);
    }
    for index in previous.skipped_files..snapshot.skipped_files {
        progress(
            if index == previous.skipped_files {
                snapshot
                    .skipped_bytes
                    .saturating_sub(previous.skipped_bytes)
            } else {
                0
            },
            FileProgressKind::Skipped,
            &snapshot.current_path,
        );
    }
}

// The monitor stays independent of CopyFile2 so control reaches a blocked transfer.
#[cfg(windows)]
struct NetworkOperationMonitor {
    stop: std::sync::Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    error: std::sync::Arc<std::sync::Mutex<Option<io::Error>>>,
}

#[cfg(windows)]
impl NetworkOperationMonitor {
    fn start(
        event_path: &Path,
        cancel: crate::domain::file_operations::CancellationToken,
        scan: crate::fs::file_operations::CopyScanProgress,
        progress: std::sync::Arc<std::sync::Mutex<NetworkOperationProgress>>,
    ) -> Self {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let error = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_error = error.clone();
        let control = event_path.with_extension("control");
        let acknowledgement = event_path.with_extension("ack");
        let scan_path = event_path.with_extension("scan");
        let progress_path = event_path.with_extension("progress");
        let thread = std::thread::spawn(move || {
            let mut sequence = 0;
            let mut acknowledged = 0;
            let mut last_scan = Vec::new();
            let mut last_progress = NetworkOperationProgress::default();
            let mut last_written = std::time::Instant::now();
            loop {
                let control_bytes = match available_snapshot(read_snapshot(&control)) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        worker_error
                            .lock()
                            .expect("IPC error lock")
                            .get_or_insert(error);
                        cancel.cancel();
                        break;
                    }
                };
                if let Some(bytes) = control_bytes
                    && bytes.len() == 9
                {
                    let next = u64::from_le_bytes(bytes[..8].try_into().unwrap());
                    if next > sequence {
                        sequence = next;
                        match bytes[8] {
                            0 => cancel.resume(),
                            1 => cancel.pause(),
                            2 => cancel.cancel(),
                            _ => cancel.cancel(),
                        }
                    }
                }
                let snapshot = scan.snapshot();
                let mut bytes = (snapshot.files as u64).to_le_bytes().to_vec();
                bytes.extend_from_slice(&snapshot.bytes.to_le_bytes());
                bytes.push(u8::from(snapshot.complete));
                if bytes != last_scan {
                    if let Err(error) = atomic_write(&scan_path, &bytes) {
                        worker_error
                            .lock()
                            .expect("IPC error lock")
                            .get_or_insert(error);
                        cancel.cancel();
                        break;
                    }
                    last_scan = bytes;
                }
                let stopping = worker_stop.load(Ordering::Acquire);
                let snapshot = progress.lock().expect("operation progress lock").clone();
                if snapshot != last_progress
                    && (stopping
                        || (cancel.is_pause_acknowledged() && acknowledged != sequence)
                        || last_progress.transferred_bytes == 0
                        || snapshot.completed_files != last_progress.completed_files
                        || snapshot.skipped_files != last_progress.skipped_files
                        || snapshot.recovering != last_progress.recovering
                        || last_written.elapsed() >= Duration::from_millis(125))
                {
                    if let Err(error) = write_network_operation_progress(&progress_path, &snapshot)
                    {
                        worker_error
                            .lock()
                            .expect("IPC error lock")
                            .get_or_insert(error);
                        cancel.cancel();
                        break;
                    }
                    last_progress = snapshot;
                    last_written = std::time::Instant::now();
                }
                if cancel.is_paused() && cancel.is_pause_acknowledged() && acknowledged != sequence
                {
                    if let Err(error) = atomic_write(&acknowledgement, &sequence.to_le_bytes()) {
                        worker_error
                            .lock()
                            .expect("IPC error lock")
                            .get_or_insert(error);
                        cancel.cancel();
                        break;
                    }
                    acknowledged = sequence;
                }
                if stopping {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        Self {
            stop,
            thread: Some(thread),
            error,
        }
    }

    fn finish(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
impl Drop for NetworkOperationMonitor {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(windows)]
fn write_network_operation_event(
    path: &Path,
    sequence: u64,
    event: &NetworkOperationEvent,
) -> io::Result<()> {
    let mut bytes = sequence.to_le_bytes().to_vec();
    match event {
        NetworkOperationEvent::Conflict {
            category,
            source,
            destination,
        } => {
            bytes.push(2);
            bytes.push(conflict_category_code(*category));
            write_units(
                &mut bytes,
                &source.as_os_str().encode_wide().collect::<Vec<_>>(),
            )?;
            write_units(
                &mut bytes,
                &destination.as_os_str().encode_wide().collect::<Vec<_>>(),
            )?;
        }
    }
    atomic_write(path, &bytes)
}

#[cfg(windows)]
fn read_network_operation_event(path: &Path) -> io::Result<(u64, NetworkOperationEvent)> {
    let bytes = read_snapshot(path)?;
    let mut offset = 0;
    let sequence = read_u64(&bytes, &mut offset)?;
    if read_byte(&bytes, &mut offset)? != 2 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid operation event",
        ));
    }
    let event = NetworkOperationEvent::Conflict {
        category: conflict_category_from_code(read_byte(&bytes, &mut offset)?)?,
        source: PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?)),
        destination: PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?)),
    };
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing operation event data",
        ));
    }
    Ok((sequence, event))
}

#[cfg(windows)]
fn write_network_operation_response(
    path: &Path,
    sequence: u64,
    action: crate::domain::file_operations::ConflictAction,
) -> io::Result<()> {
    let mut bytes = sequence.to_le_bytes().to_vec();
    bytes.push(match action {
        crate::domain::file_operations::ConflictAction::Replace => 0,
        crate::domain::file_operations::ConflictAction::Skip => 1,
        crate::domain::file_operations::ConflictAction::KeepBoth => 2,
    });
    atomic_write(path, &bytes)
}

#[cfg(windows)]
fn read_network_operation_response(
    path: &Path,
) -> io::Result<(u64, crate::domain::file_operations::ConflictAction)> {
    let bytes = read_snapshot(path)?;
    let mut offset = 0;
    let sequence = read_u64(&bytes, &mut offset)?;
    let action = match read_byte(&bytes, &mut offset)? {
        0 => crate::domain::file_operations::ConflictAction::Replace,
        1 => crate::domain::file_operations::ConflictAction::Skip,
        2 => crate::domain::file_operations::ConflictAction::KeepBoth,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid conflict response",
            ));
        }
    };
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing conflict response data",
        ));
    }
    Ok((sequence, action))
}

#[cfg(windows)]
fn conflict_category_code(category: crate::domain::file_operations::ConflictCategory) -> u8 {
    use crate::domain::file_operations::ConflictCategory::*;
    match category {
        ExistingFile => 0,
        ExistingDirectory => 1,
        TypeMismatch => 2,
        DestinationReadOnly => 3,
        SourceInsideDestination => 4,
        Other => 5,
    }
}

#[cfg(windows)]
fn conflict_category_from_code(
    code: u8,
) -> io::Result<crate::domain::file_operations::ConflictCategory> {
    use crate::domain::file_operations::ConflictCategory::*;
    match code {
        0 => Ok(ExistingFile),
        1 => Ok(ExistingDirectory),
        2 => Ok(TypeMismatch),
        3 => Ok(DestinationReadOnly),
        4 => Ok(SourceInsideDestination),
        5 => Ok(Other),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid conflict category",
        )),
    }
}

#[cfg(windows)]
fn available_snapshot<T>(result: io::Result<T>) -> io::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::WouldBlock
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn snapshot_error(path: &Path, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{}: {error}", path.display()))
}

// Locks cover only small local IPC snapshots; never wait on network I/O.
#[cfg(windows)]
fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::{Seek, Write};
    let write = || -> io::Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let started = std::time::Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock)
                    if started.elapsed() < Duration::from_secs(1) =>
                {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "local IPC snapshot lock timed out",
                    ));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error),
            }
        }
        file.rewind()?;
        file.write_all(bytes)?;
        file.set_len(bytes.len() as u64)?;
        Ok(())
    };
    write().map_err(|error| snapshot_error(path, error))
}

#[cfg(windows)]
fn read_snapshot(path: &Path) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let read = || -> io::Result<Vec<u8>> {
        let file = std::fs::File::open(path)?;
        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(io::ErrorKind::WouldBlock.into()),
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes)?;
        if bytes.is_empty() {
            // A newly created snapshot is unpublished until its first locked write.
            return Err(io::ErrorKind::WouldBlock.into());
        }
        if bytes.len() > 1_048_576 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC snapshot exceeds size limit",
            ));
        }
        Ok(bytes)
    };
    read().map_err(|error| snapshot_error(path, error))
}

#[cfg(windows)]
fn write_network_operation_result(
    path: &Path,
    result: &io::Result<IsolatedNetworkOperationReport>,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    let result = match result {
        Ok(report) => {
            bytes.push(0);
            report
        }
        Err(error) => {
            bytes.push(1);
            write_units(
                &mut bytes,
                &error.to_string().encode_utf16().collect::<Vec<_>>(),
            )?;
            return std::fs::write(path, bytes);
        }
    };
    bytes.extend_from_slice(&(result.files as u64).to_le_bytes());
    bytes.extend_from_slice(&(result.directories as u64).to_le_bytes());
    bytes.extend_from_slice(&result.bytes.to_le_bytes());
    write_path_list(&mut bytes, &result.skipped)?;
    write_path_list(&mut bytes, &result.affected_directories)?;
    write_path_list(&mut bytes, &result.completed_paths)?;
    bytes.push(u8::from(result.aborted));
    std::fs::write(path, bytes)
}

#[cfg(windows)]
fn read_network_operation_result(path: &Path) -> io::Result<IsolatedNetworkOperationReport> {
    let bytes = std::fs::read(path)?;
    let mut offset = 0;
    match read_byte(&bytes, &mut offset)? {
        0 => {}
        1 => {
            let message = String::from_utf16(&read_units_at(&bytes, &mut offset)?)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            if offset != bytes.len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "trailing operation error data",
                ));
            }
            return Err(io::Error::other(message));
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid operation result kind",
            ));
        }
    }
    let files = usize::try_from(read_u64(&bytes, &mut offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "file count too large"))?;
    let directories = usize::try_from(read_u64(&bytes, &mut offset)?)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "directory count too large"))?;
    let transferred = read_u64(&bytes, &mut offset)?;
    let skipped = read_path_list(&bytes, &mut offset)?;
    let affected_directories = read_path_list(&bytes, &mut offset)?;
    let completed_paths = read_path_list(&bytes, &mut offset)?;
    let aborted = read_byte(&bytes, &mut offset)? != 0;
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing operation result data",
        ));
    }
    Ok(IsolatedNetworkOperationReport {
        files,
        directories,
        bytes: transferred,
        skipped,
        affected_directories,
        completed_paths,
        aborted,
    })
}

#[cfg(windows)]
fn write_path_list(bytes: &mut Vec<u8>, paths: &[PathBuf]) -> io::Result<()> {
    let count = u32::try_from(paths.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "too many paths"))?;
    bytes.extend_from_slice(&count.to_le_bytes());
    for path in paths {
        write_units(bytes, &path.as_os_str().encode_wide().collect::<Vec<_>>())?;
    }
    Ok(())
}

#[cfg(windows)]
fn read_path_list(bytes: &[u8], offset: &mut usize) -> io::Result<Vec<PathBuf>> {
    let count = read_u32(bytes, offset)? as usize;
    if count > MAX_HELPER_ITEMS {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "too many paths"));
    }
    (0..count)
        .map(|_| {
            read_units_at(bytes, offset).map(|units| PathBuf::from(OsString::from_wide(&units)))
        })
        .collect()
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
    fn explorer_network_location_mutations_are_limited_to_direct_nethood_items() {
        let root = std::env::temp_dir().join(format!(
            "asterfiles-nethood-mutation-{}",
            std::process::id()
        ));
        let outside =
            std::env::temp_dir().join(format!("asterfiles-nethood-outside-{}", std::process::id()));
        let item = root.join("NAS");
        let nested = item.join("target.lnk");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
        std::fs::create_dir_all(&item).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(&nested, b"shortcut").unwrap();

        assert_eq!(
            super::explorer_network_location_path_in(&root, &item).unwrap(),
            std::fs::canonicalize(&item).unwrap()
        );
        assert_eq!(
            super::explorer_network_location_path_in(&root, &nested)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            super::explorer_network_location_path_in(&root, &outside)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert!(super::valid_network_location_name(std::ffi::OsStr::new(
            "家庭 NAS"
        )));
        assert!(!super::valid_network_location_name(std::ffi::OsStr::new(
            ""
        )));
        assert!(!super::valid_network_location_name(std::ffi::OsStr::new(
            r"folder\name"
        )));
        assert!(!super::valid_network_location_name(std::ffi::OsStr::new(
            ".."
        )));

        std::fs::remove_dir_all(&root).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
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
    fn network_operation_codecs_preserve_raw_paths_and_reports() {
        use std::os::windows::ffi::OsStringExt;

        let source = std::path::PathBuf::from(std::ffi::OsString::from_wide(&[
            b'\\' as u16,
            b'\\' as u16,
            b's' as u16,
            0xd800,
        ]));
        let destination = source.join("目标");
        let input = std::env::temp_dir().join(format!(
            "asterfiles-network-operation-input-{}",
            std::process::id()
        ));
        let output = std::env::temp_dir().join(format!(
            "asterfiles-network-operation-output-{}",
            std::process::id()
        ));
        super::write_network_operation_input(
            &input,
            super::IsolatedNetworkOperationKind::Copy,
            &source,
            Some(&destination),
        )
        .unwrap();
        assert_eq!(
            super::read_network_operation_input(&input).unwrap(),
            (
                super::IsolatedNetworkOperationKind::Copy,
                source.clone(),
                Some(destination.clone())
            )
        );
        let report = super::IsolatedNetworkOperationReport {
            files: 2,
            directories: 1,
            bytes: 42,
            skipped: vec![source.clone()],
            affected_directories: vec![source.clone()],
            completed_paths: vec![destination],
            aborted: false,
        };
        super::write_network_operation_result(&output, &Ok(report.clone())).unwrap();
        assert_eq!(
            super::read_network_operation_result(&output).unwrap(),
            report
        );
        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(output);
    }

    #[cfg(windows)]
    #[test]
    fn network_operation_failure_preserves_path_and_system_reason() {
        let root = std::env::temp_dir().join(format!(
            "asterfiles-network-operation-error-{}-{}",
            std::process::id(),
            super::SystemTime::now()
                .duration_since(super::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&root).unwrap();
        let source = root.join("missing-source-文件.txt");
        let destination = root.join("destination.txt");
        let output = root.join("result");
        let expected_reason = std::fs::symlink_metadata(&source).unwrap_err().to_string();
        let result = super::execute_network_operation(
            super::IsolatedNetworkOperationKind::Copy,
            &source,
            Some(&destination),
            &root.join("event"),
            &root.join("response"),
        );
        assert!(result.is_err());
        super::write_network_operation_result(&output, &result).unwrap();
        let message = super::read_network_operation_result(&output)
            .unwrap_err()
            .to_string();
        assert!(message.contains(source.to_str().unwrap()), "{message}");
        assert!(message.contains(&expected_reason), "{message}");
        assert!(!message.contains("Io {"), "{message}");
        assert!(!destination.exists());

        let result = Err(super::network_operation_error(
            crate::fs::file_operations::OperationError::SourceInsideDestination,
        ));
        super::write_network_operation_result(&output, &result).unwrap();
        assert_eq!(
            super::read_network_operation_result(&output)
                .unwrap_err()
                .to_string(),
            "SourceInsideDestination"
        );
        std::fs::write(&output, [2]).unwrap();
        assert_eq!(
            super::read_network_operation_result(&output)
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn network_operation_event_and_conflict_response_round_trip() {
        let event_file = std::env::temp_dir().join(format!(
            "asterfiles-network-operation-event-{}",
            std::process::id()
        ));
        let response_file = std::env::temp_dir().join(format!(
            "asterfiles-network-operation-response-{}",
            std::process::id()
        ));
        std::fs::write(&event_file, []).unwrap();
        super::write_network_operation_event(
            &event_file,
            9,
            &super::NetworkOperationEvent::Conflict {
                category: crate::domain::file_operations::ConflictCategory::TypeMismatch,
                source: std::path::PathBuf::from(r"\\服务器\共享\源"),
                destination: std::path::PathBuf::from(r"\\服务器\共享\目标"),
            },
        )
        .unwrap();
        let (sequence, event) = super::read_network_operation_event(&event_file).unwrap();
        assert_eq!(sequence, 9);
        assert!(matches!(
            event,
            super::NetworkOperationEvent::Conflict {
                category: crate::domain::file_operations::ConflictCategory::TypeMismatch,
                ..
            }
        ));
        super::write_network_operation_response(
            &response_file,
            sequence,
            crate::domain::file_operations::ConflictAction::KeepBoth,
        )
        .unwrap();
        assert_eq!(
            super::read_network_operation_response(&response_file).unwrap(),
            (9, crate::domain::file_operations::ConflictAction::KeepBoth)
        );
        let _ = std::fs::remove_file(event_file);
        let _ = std::fs::remove_file(response_file);
    }
    #[cfg(windows)]
    #[test]
    fn issue_136_overwritten_progress_snapshots_preserve_all_totals() {
        let path = std::env::temp_dir().join(format!("asterfiles-progress-{}", std::process::id()));
        let mut snapshot = super::NetworkOperationProgress::default();
        let mut maximum_size = 0;
        for index in 1..=100 {
            snapshot.discovered_files = index;
            snapshot.discovered_bytes = index * 100;
            snapshot.transferred_bytes = index * 100;
            snapshot.completed_files = index;
            snapshot.current_path = "source".into();
            snapshot.discovered_path = "source".into();
            super::write_network_operation_progress(&path, &snapshot).unwrap();
            maximum_size = maximum_size.max(std::fs::metadata(&path).unwrap().len());
        }
        assert!(maximum_size < 100);
        let snapshot = super::read_network_operation_progress(&path).unwrap();
        let mut discovered_files = 0;
        let mut discovered_bytes = 0;
        let mut transferred = 0;
        let mut completed = 0;
        super::deliver_network_operation_progress(
            &snapshot,
            &Default::default(),
            &mut |bytes, _| {
                discovered_files += 1;
                discovered_bytes += bytes;
            },
            &mut |bytes, finished, _| {
                transferred += bytes;
                completed += usize::from(finished == super::FileProgressKind::Completed);
            },
            &mut |_| panic!("unexpected recovery"),
        );
        assert_eq!(
            (discovered_files, discovered_bytes, transferred, completed),
            (100, 10000, 10000, 100)
        );
        super::deliver_network_operation_progress(
            &snapshot,
            &snapshot,
            &mut |_, _| panic!("duplicate discovery"),
            &mut |_, _, _| panic!("duplicate progress"),
            &mut |_| panic!("duplicate recovery"),
        );
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn issue_139_network_skip_snapshots_preserve_totals_without_replay() {
        let path = std::env::temp_dir().join(format!(
            "asterfiles-skipped-progress-{}",
            std::process::id()
        ));
        let previous = super::NetworkOperationProgress {
            skipped_files: 2,
            skipped_bytes: 100,
            transferred_bytes: 20,
            completed_files: 1,
            ..Default::default()
        };
        let snapshot = super::NetworkOperationProgress {
            skipped_files: 14,
            skipped_bytes: 900,
            transferred_bytes: 70,
            completed_files: 5,
            current_path: "source.bin".into(),
            ..Default::default()
        };
        super::write_network_operation_progress(&path, &snapshot).unwrap();
        let snapshot = super::read_network_operation_progress(&path).unwrap();
        let mut skipped = 0;
        let mut skipped_bytes = 0;
        let mut transferred = 0;
        let mut completed = 0;
        super::deliver_network_operation_progress(
            &snapshot,
            &previous,
            &mut |_, _| panic!("no discovery"),
            &mut |bytes, kind, _| match kind {
                super::FileProgressKind::Transferred => transferred += bytes,
                super::FileProgressKind::Completed => {
                    transferred += bytes;
                    completed += 1;
                }
                super::FileProgressKind::Skipped => {
                    skipped += 1;
                    skipped_bytes += bytes;
                }
            },
            &mut |_| panic!("no recovery"),
        );
        assert_eq!(
            (skipped, skipped_bytes, transferred, completed),
            (12, 800, 50, 4)
        );
        super::deliver_network_operation_progress(
            &snapshot,
            &snapshot,
            &mut |_, _| panic!("duplicate discovery"),
            &mut |_, _, _| panic!("duplicate progress"),
            &mut |_| panic!("duplicate recovery"),
        );
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn issue_137_recovery_progress_preserves_logical_totals() {
        use std::sync::{Arc, Mutex};

        let path = std::env::temp_dir().join(format!(
            "asterfiles-recovery-progress-{}",
            std::process::id()
        ));
        let original = super::NetworkOperationProgress {
            transferred_bytes: 8_192,
            current_path: "source.bin".into(),
            ..Default::default()
        };
        let shared = Arc::new(Mutex::new(original.clone()));
        let registration = super::CopyProgressRegistration::new(shared.clone());
        super::set_copy_recovering(true);
        super::write_network_operation_progress(&path, &shared.lock().unwrap()).unwrap();
        let recovering = super::read_network_operation_progress(&path).unwrap();
        assert!(recovering.recovering);
        let mut recovery_changes = Vec::new();
        super::deliver_network_operation_progress(
            &recovering,
            &original,
            &mut |_, _| panic!("recovery does not discover files"),
            &mut |_, _, _| panic!("recopied bytes must not advance logical progress"),
            &mut |value| recovery_changes.push(value),
        );
        super::deliver_network_operation_progress(
            &recovering,
            &recovering,
            &mut |_, _| panic!("duplicate discovery"),
            &mut |_, _, _| panic!("duplicate progress"),
            &mut |_| panic!("duplicate recovery transition"),
        );
        super::set_copy_recovering(false);
        let mut advanced = shared.lock().unwrap().clone();
        advanced.transferred_bytes += 512;
        advanced.completed_files = 1;
        super::write_network_operation_progress(&path, &advanced).unwrap();
        let advanced = super::read_network_operation_progress(&path).unwrap();
        let mut transferred = 0;
        let mut completed = 0;
        super::deliver_network_operation_progress(
            &advanced,
            &recovering,
            &mut |_, _| panic!("no discovery"),
            &mut |bytes, finished, _| {
                transferred += bytes;
                completed += usize::from(finished == super::FileProgressKind::Completed);
            },
            &mut |value| recovery_changes.push(value),
        );
        assert_eq!(recovery_changes, [true, false]);
        assert_eq!((transferred, completed), (512, 1));

        super::set_copy_recovering(true);
        drop(registration);
        assert!(!shared.lock().unwrap().recovering);
        super::set_copy_recovering(true);
        assert!(!shared.lock().unwrap().recovering);
        std::fs::remove_file(path).unwrap();
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

#[cfg(all(windows, test))]
mod snapshot_tests;

#[cfg(windows)]
mod temporary_copy;
#[cfg(windows)]
pub(crate) use temporary_copy::register as register_copy_temporary;
#[cfg(windows)]
pub(crate) use temporary_copy::register_staging as register_copy_staging;
#[cfg(windows)]
pub(crate) use temporary_copy::retire_staging as retire_copy_staging;
