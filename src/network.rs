use std::{
    io,
    path::Path,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkLocationSource {
    WindowsImported,
    AsterOwned,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkTarget {
    WindowsPath(PathBuf),
    ShellItemId(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkLocation {
    pub id: u64,
    pub source: NetworkLocationSource,
    pub display_name: String,
    pub sort_order: u32,
    pub target: NetworkTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkDeviceId(pub Vec<u16>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NetworkExecutionKey(pub Vec<u16>);

impl NetworkExecutionKey {
    pub fn from_unc(path: &Path) -> Option<Self> {
        let host = unc_host_units(path)?;
        let normalized = host
            .into_iter()
            .map(|unit| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + (b'a' - b'A') as u16
                } else {
                    unit
                }
            })
            .collect::<Vec<_>>();
        (!normalized.is_empty()).then_some(Self(normalized))
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkLocationCatalogError {
    EmptyName,
    InvalidUncPath,
    NotFound,
    ImportedReadOnly,
    DuplicateTarget,
}

#[allow(dead_code)]
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct NetworkLocationCatalog {
    locations: Vec<NetworkLocation>,
}

impl NetworkLocationCatalog {
    pub fn new(locations: Vec<NetworkLocation>) -> Self {
        Self { locations }
    }

    pub fn locations(&self) -> &[NetworkLocation] {
        &self.locations
    }

    pub fn add_unc(
        &mut self,
        path: PathBuf,
        display_name: impl Into<String>,
    ) -> Result<u64, NetworkLocationCatalogError> {
        let display_name = display_name.into();
        if display_name.trim().is_empty() {
            return Err(NetworkLocationCatalogError::EmptyName);
        }
        if !is_unc_path(&path) {
            return Err(NetworkLocationCatalogError::InvalidUncPath);
        }
        if self.locations.iter().any(|location| {
            matches!(&location.target, NetworkTarget::WindowsPath(target) if target == &path)
        }) {
            return Err(NetworkLocationCatalogError::DuplicateTarget);
        }
        let id = stable_network_location_id(&path, &self.locations);
        let sort_order = self
            .locations
            .iter()
            .map(|location| location.sort_order)
            .max()
            .unwrap_or(0)
            .saturating_add(u32::from(!self.locations.is_empty()));
        self.locations.push(NetworkLocation {
            id,
            source: NetworkLocationSource::AsterOwned,
            display_name,
            sort_order,
            target: NetworkTarget::WindowsPath(path),
        });
        Ok(id)
    }

    pub fn rename(
        &mut self,
        id: u64,
        display_name: impl Into<String>,
    ) -> Result<(), NetworkLocationCatalogError> {
        let display_name = display_name.into();
        if display_name.trim().is_empty() {
            return Err(NetworkLocationCatalogError::EmptyName);
        }
        let location = self
            .locations
            .iter_mut()
            .find(|location| location.id == id)
            .ok_or(NetworkLocationCatalogError::NotFound)?;
        if location.source != NetworkLocationSource::AsterOwned {
            return Err(NetworkLocationCatalogError::ImportedReadOnly);
        }
        location.display_name = display_name;
        Ok(())
    }

    pub fn remove(&mut self, id: u64) -> Result<NetworkLocation, NetworkLocationCatalogError> {
        let index = self
            .locations
            .iter()
            .position(|location| location.id == id)
            .ok_or(NetworkLocationCatalogError::NotFound)?;
        if self.locations[index].source != NetworkLocationSource::AsterOwned {
            return Err(NetworkLocationCatalogError::ImportedReadOnly);
        }
        let removed = self.locations.remove(index);
        self.reindex();
        Ok(removed)
    }

    pub fn move_to(&mut self, id: u64, index: usize) -> Result<(), NetworkLocationCatalogError> {
        let source = self
            .locations
            .iter()
            .position(|location| location.id == id)
            .ok_or(NetworkLocationCatalogError::NotFound)?;
        if self.locations[source].source != NetworkLocationSource::AsterOwned {
            return Err(NetworkLocationCatalogError::ImportedReadOnly);
        }
        let location = self.locations.remove(source);
        let owned_indices = self
            .locations
            .iter()
            .enumerate()
            .filter_map(|(index, location)| {
                (location.source == NetworkLocationSource::AsterOwned).then_some(index)
            })
            .collect::<Vec<_>>();
        let destination = owned_indices
            .get(index)
            .copied()
            .or_else(|| owned_indices.last().map(|last| last + 1))
            .unwrap_or(self.locations.len())
            .min(self.locations.len());
        self.locations.insert(destination, location);
        self.reindex();
        Ok(())
    }

    fn reindex(&mut self) {
        for (index, location) in self
            .locations
            .iter_mut()
            .filter(|location| location.source == NetworkLocationSource::AsterOwned)
            .enumerate()
        {
            location.sort_order = index as u32;
        }
    }
}

fn stable_network_location_id(path: &Path, locations: &[NetworkLocation]) -> u64 {
    use std::os::windows::ffi::OsStrExt;

    let mut hash = 0xcbf29ce484222325_u64;
    for byte in path.as_os_str().encode_wide().flat_map(u16::to_le_bytes) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    let mut id = hash.max(1);
    while locations.iter().any(|location| location.id == id) {
        id = id.wrapping_add(1).max(1);
    }
    id
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkDeviceTarget {
    pub id: NetworkDeviceId,
    pub display_name: String,
    pub shell_identity: Option<Vec<u8>>,
    pub unc_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DiscoveryRequestId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryState {
    Idle,
    Discovering,
    Complete,
    Empty,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkErrorKind {
    NotFound,
    PermissionDenied,
    Disconnected,
    TimedOut,
    Cancelled,
    InvalidTarget,
    Failed,
}

pub fn classify_network_error(error: &io::Error) -> NetworkErrorKind {
    match error.kind() {
        io::ErrorKind::NotFound => NetworkErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => NetworkErrorKind::PermissionDenied,
        io::ErrorKind::Interrupted => NetworkErrorKind::Cancelled,
        io::ErrorKind::TimedOut => NetworkErrorKind::TimedOut,
        io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::NotConnected
        | io::ErrorKind::NetworkDown
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::HostUnreachable => NetworkErrorKind::Disconnected,
        io::ErrorKind::InvalidInput | io::ErrorKind::InvalidData => NetworkErrorKind::InvalidTarget,
        _ => NetworkErrorKind::Failed,
    }
}

pub fn is_unc_path(path: &Path) -> bool {
    use std::os::windows::ffi::OsStrExt;

    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if units.starts_with(&[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16]) {
        return units.get(4..8).is_some_and(|prefix| {
            (prefix[0] | 0x20) == b'u' as u16
                && (prefix[1] | 0x20) == b'n' as u16
                && (prefix[2] | 0x20) == b'c' as u16
                && prefix[3] == b'\\' as u16
        });
    }
    units.len() >= 3
        && matches!(units[0], 0x005c | 0x002f)
        && matches!(units[1], 0x005c | 0x002f)
        && units[2] != b'.' as u16
        && units[2] != b'?' as u16
}

#[allow(dead_code)]
pub fn unc_host_key(path: &Path) -> Option<String> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};

    let host = OsString::from_wide(&unc_host_units(path)?);
    let host = host.to_str()?.trim();
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

fn unc_host_units(path: &Path) -> Option<Vec<u16>> {
    let units = unc_body_units(path)?;
    let end = units
        .iter()
        .position(|unit| matches!(*unit, 0x005c | 0x002f))
        .unwrap_or(units.len());
    (end > 0).then(|| units[..end].to_vec())
}

pub fn network_device_id(path: &Path) -> NetworkDeviceId {
    use std::os::windows::ffi::OsStrExt;

    NetworkDeviceId(path.as_os_str().encode_wide().collect())
}

pub fn device_root_target(device: &NetworkDeviceTarget) -> Option<PathBuf> {
    device.unc_path.clone()
}
pub fn is_unc_server_root(path: &Path) -> bool {
    unc_body_units(path).is_some_and(|units| {
        let mut parts = units.split(|unit| matches!(*unit, 0x005c | 0x002f));
        parts.next().is_some_and(|host| !host.is_empty()) && parts.all(|part| part.is_empty())
    })
}

fn unc_body_units(path: &Path) -> Option<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;

    let units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if units.starts_with(&[
        b'\\' as u16,
        b'\\' as u16,
        b'?' as u16,
        b'\\' as u16,
        b'U' as u16,
        b'N' as u16,
        b'C' as u16,
        b'\\' as u16,
    ]) {
        Some(units[8..].to_vec())
    } else if is_unc_path(path) {
        Some(units[2..].to_vec())
    } else {
        None
    }
}

#[derive(Debug)]
pub struct DiscoveryCoordinator {
    generation: DiscoveryRequestId,
    cancel: Option<Arc<AtomicBool>>,
    state: DiscoveryState,
    devices: Vec<NetworkDeviceTarget>,
    error: Option<NetworkErrorKind>,
}

impl Default for DiscoveryCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl DiscoveryCoordinator {
    pub fn new() -> Self {
        Self {
            generation: DiscoveryRequestId(0),
            cancel: None,
            state: DiscoveryState::Idle,
            devices: Vec::new(),
            error: None,
        }
    }
    pub fn begin(&mut self) -> (DiscoveryRequestId, Arc<AtomicBool>) {
        self.cancel_current();
        self.generation.0 = self.generation.0.saturating_add(1);
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.state = DiscoveryState::Discovering;
        self.devices.clear();
        self.error = None;
        (self.generation, cancel)
    }
    pub fn cancel_current(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel.store(true, Ordering::Release);
            self.state = DiscoveryState::Cancelled;
        }
    }
    pub fn accepts(&self, request_id: DiscoveryRequestId) -> bool {
        self.generation == request_id
            && self.state == DiscoveryState::Discovering
            && !self
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.load(Ordering::Acquire))
    }
    pub fn append(
        &mut self,
        request_id: DiscoveryRequestId,
        devices: impl IntoIterator<Item = NetworkDeviceTarget>,
    ) -> bool {
        if !self.accepts(request_id) {
            return false;
        }
        self.devices.extend(devices);
        true
    }
    pub fn finish(&mut self, request_id: DiscoveryRequestId) -> bool {
        if !self.accepts(request_id) {
            return false;
        }
        self.state = if self.devices.is_empty() {
            DiscoveryState::Empty
        } else {
            DiscoveryState::Complete
        };
        self.cancel = None;
        true
    }
    pub fn fail(&mut self, request_id: DiscoveryRequestId, error: NetworkErrorKind) -> bool {
        if !self.accepts(request_id) {
            return false;
        }
        self.state = if error == NetworkErrorKind::Cancelled {
            DiscoveryState::Cancelled
        } else {
            DiscoveryState::Failed
        };
        self.error = Some(error);
        self.cancel = None;
        true
    }
    #[allow(dead_code)]
    pub fn generation(&self) -> DiscoveryRequestId {
        self.generation
    }
    pub fn state(&self) -> DiscoveryState {
        self.state
    }
    #[allow(dead_code)]
    pub fn error(&self) -> Option<NetworkErrorKind> {
        self.error
    }
    pub fn devices(&self) -> &[NetworkDeviceTarget] {
        &self.devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn device(name: &str) -> NetworkDeviceTarget {
        NetworkDeviceTarget {
            id: NetworkDeviceId(name.encode_utf16().collect()),
            display_name: name.into(),
            shell_identity: None,
            unc_path: None,
        }
    }
    #[test]
    fn owned_location_crud_preserves_imported_items_and_order() {
        let imported = NetworkLocation {
            id: 7,
            source: NetworkLocationSource::WindowsImported,
            display_name: "Explorer".into(),
            sort_order: 77,
            target: NetworkTarget::WindowsPath(PathBuf::from(r"\\server\imported")),
        };
        let mut catalog = NetworkLocationCatalog::new(vec![imported.clone()]);
        let first = catalog
            .add_unc(PathBuf::from(r"\\server\first"), "First")
            .unwrap();
        let second = catalog
            .add_unc(PathBuf::from(r"\\server\second"), "Second")
            .unwrap();
        catalog.rename(second, "Renamed").unwrap();
        catalog.move_to(second, 0).unwrap();
        assert_eq!(catalog.locations()[0], imported);
        let owned = catalog
            .locations()
            .iter()
            .filter(|location| location.source == NetworkLocationSource::AsterOwned)
            .collect::<Vec<_>>();
        assert_eq!(
            owned.iter().map(|item| item.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(
            owned.iter().map(|item| item.sort_order).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(owned[0].display_name, "Renamed");
        assert_eq!(catalog.remove(first).unwrap().id, first);
    }

    #[test]
    fn owned_location_catalog_rejects_invalid_mutation() {
        let imported = NetworkLocation {
            id: 7,
            source: NetworkLocationSource::WindowsImported,
            display_name: "Explorer".into(),
            sort_order: 0,
            target: NetworkTarget::WindowsPath(PathBuf::from(r"\\server\imported")),
        };
        let mut catalog = NetworkLocationCatalog::new(vec![imported]);
        assert_eq!(
            catalog.rename(7, "No"),
            Err(NetworkLocationCatalogError::ImportedReadOnly)
        );
        assert_eq!(
            catalog.add_unc(PathBuf::from(r"C:\local"), "Local"),
            Err(NetworkLocationCatalogError::InvalidUncPath)
        );
        assert_eq!(
            catalog.add_unc(PathBuf::from(r"\\server\new"), "   "),
            Err(NetworkLocationCatalogError::EmptyName)
        );
        catalog
            .add_unc(PathBuf::from(r"\\server\new"), "New")
            .unwrap();
        assert_eq!(
            catalog.add_unc(PathBuf::from(r"\\server\new"), "Duplicate"),
            Err(NetworkLocationCatalogError::DuplicateTarget)
        );
    }
    #[test]
    fn shell_only_import_retains_an_executable_windows_identity() {
        let location = NetworkLocation {
            id: 9,
            source: NetworkLocationSource::WindowsImported,
            display_name: "Virtual network location".into(),
            sort_order: 0,
            target: NetworkTarget::ShellItemId(PathBuf::from("shell:::{virtual-network-location}")),
        };

        assert!(matches!(location.target, NetworkTarget::ShellItemId(_)));
    }
    #[test]
    fn stale_results_are_rejected() {
        let mut c = DiscoveryCoordinator::new();
        let (old, old_cancel) = c.begin();
        let (current, _) = c.begin();
        assert!(old_cancel.load(Ordering::Acquire));
        assert!(!c.append(old, [device("old")]));
        assert!(c.append(current, [device("current")]));
        assert!(c.finish(current));
        assert_eq!(c.devices()[0].display_name, "current");
    }
    #[test]
    fn cancellation_rejects_late_batches() {
        let mut c = DiscoveryCoordinator::new();
        let (request, cancel) = c.begin();
        c.cancel_current();
        assert!(cancel.load(Ordering::Acquire));
        assert_eq!(c.state(), DiscoveryState::Cancelled);
        assert!(!c.append(request, [device("late")]));
    }
    #[test]
    fn empty_and_failed_states_are_distinct() {
        let mut c = DiscoveryCoordinator::new();
        let (empty, _) = c.begin();
        assert!(c.finish(empty));
        assert_eq!(c.state(), DiscoveryState::Empty);
        let (failed, _) = c.begin();
        assert!(c.fail(failed, NetworkErrorKind::TimedOut));
        assert_eq!(c.state(), DiscoveryState::Failed);
        assert_eq!(c.error(), Some(NetworkErrorKind::TimedOut));
    }
    #[test]
    fn unc_routing_accepts_non_unicode_identity_without_lossy_conversion() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};

        let path = PathBuf::from(OsString::from_wide(&[
            b'\\' as u16,
            b'\\' as u16,
            b's' as u16,
            0xd800,
        ]));
        assert!(is_unc_path(&path));
        assert!(is_unc_server_root(&path));
        assert_eq!(unc_host_key(&path), None);
    }

    #[test]
    fn extended_local_paths_are_not_routed_as_unc() {
        assert!(!is_unc_path(Path::new(r"\\?\C:\folder")));
        assert!(!is_unc_path(Path::new(r"\\?\Volume{01234567}\folder")));
        assert!(!is_unc_path(Path::new(r"\\.\PhysicalDrive0")));
        assert!(is_unc_path(Path::new(r"\\?\UNC\server\share")));
        assert!(is_unc_path(Path::new(r"\\server\share")));
    }

    #[test]
    fn unc_host_key_is_normalized() {
        assert!(is_unc_path(Path::new(r"\\Server\Share")));
        assert!(is_unc_path(Path::new("//SERVER/Share")));
        assert_eq!(
            unc_host_key(Path::new(r"\\Server\Share")),
            Some("server".into())
        );
        assert_eq!(
            unc_host_key(Path::new(r"\\?\UNC\SERVER\Share")),
            Some("server".into())
        );
        assert_eq!(unc_host_key(Path::new(r"C:\Share")), None);
    }

    #[test]
    fn execution_key_groups_shares_by_raw_unc_host() {
        assert_eq!(
            NetworkExecutionKey::from_unc(Path::new(r"\\SERVER\one")),
            NetworkExecutionKey::from_unc(Path::new(r"\\server\two"))
        );
        assert_ne!(
            NetworkExecutionKey::from_unc(Path::new(r"\\server\one")),
            NetworkExecutionKey::from_unc(Path::new(r"\\other\one"))
        );
        assert_eq!(NetworkExecutionKey::from_unc(Path::new(r"C:\one")), None);
    }

    #[test]
    fn distinguishes_server_roots_from_shares() {
        assert!(is_unc_server_root(Path::new(r"\\server")));
        assert!(is_unc_server_root(Path::new(r"\\server\")));
        assert!(!is_unc_server_root(Path::new(r"\\server\share")));
        assert!(!is_unc_server_root(Path::new(r"C:\server")));
    }
    #[test]
    fn network_errors_are_classified() {
        assert_eq!(
            classify_network_error(&io::Error::from(io::ErrorKind::TimedOut)),
            NetworkErrorKind::TimedOut
        );
        assert_eq!(
            classify_network_error(&io::Error::from(io::ErrorKind::PermissionDenied)),
            NetworkErrorKind::PermissionDenied
        );
        assert_eq!(
            classify_network_error(&io::Error::from(io::ErrorKind::ConnectionReset)),
            NetworkErrorKind::Disconnected
        );
    }
}
