use super::*;
use std::{cell::RefCell, os::windows::fs::MetadataExt, path::Component};

thread_local! {
    static REGISTRATION: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub(super) struct Registration(Option<PathBuf>);

impl Registration {
    pub(super) fn new(path: PathBuf) -> Self {
        Self(REGISTRATION.with(|slot| slot.replace(Some(path))))
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        REGISTRATION.with(|slot| slot.replace(self.0.take()));
    }
}

const MAX_STAGED_PATHS: usize = 4096;

#[derive(Default)]
struct Registry {
    current: PathBuf,
    staged: PathBuf,
    retired: Vec<PathBuf>,
}

fn read_registry(path: &Path) -> io::Result<Registry> {
    let bytes = match read_snapshot(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Registry::default()),
        Err(error) => return Err(error),
    };
    let mut offset = 0;
    let current = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
    let staged = PathBuf::from(OsString::from_wide(&read_units_at(&bytes, &mut offset)?));
    let count = read_u64(&bytes, &mut offset)?;
    if count > MAX_STAGED_PATHS as u64
        || count + u64::from(!staged.as_os_str().is_empty()) > MAX_STAGED_PATHS as u64
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporary copy registry exceeds its ownership limit",
        ));
    }
    let mut retired = Vec::with_capacity(count as usize);
    for _ in 0..count {
        retired.push(PathBuf::from(OsString::from_wide(&read_units_at(
            &bytes,
            &mut offset,
        )?)));
    }
    if offset != bytes.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "trailing temporary copy registry data",
        ));
    }
    Ok(Registry {
        current,
        staged,
        retired,
    })
}

fn write_registry(path: &Path, registry: &Registry) -> io::Result<()> {
    if registry.retired.len() + usize::from(!registry.staged.as_os_str().is_empty())
        > MAX_STAGED_PATHS
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporary copy registry exceeds its ownership limit",
        ));
    }
    let mut bytes = Vec::new();
    for entry in [&registry.current, &registry.staged] {
        write_units(
            &mut bytes,
            &entry.as_os_str().encode_wide().collect::<Vec<_>>(),
        )?;
    }
    bytes.extend_from_slice(&(registry.retired.len() as u64).to_le_bytes());
    for entry in &registry.retired {
        write_units(
            &mut bytes,
            &entry.as_os_str().encode_wide().collect::<Vec<_>>(),
        )?;
    }
    if bytes.len() > 1_048_576 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "temporary copy registry exceeds its snapshot limit",
        ));
    }
    atomic_write(path, &bytes)
}

// Publish exact ownership before either the copy or its staging file can be created.
pub(crate) fn register(path: &Path) -> io::Result<()> {
    register_paths(path, None)
}
pub(crate) fn register_staging(path: &Path, staged: &Path) -> io::Result<()> {
    register_paths(path, Some(staged))
}
fn register_paths(path: &Path, staged: Option<&Path>) -> io::Result<()> {
    REGISTRATION.with(|slot| {
        let Some(registry_path) = slot.borrow().clone() else {
            return Ok(());
        };
        let mut registry = read_registry(&registry_path)?;
        registry.current = path.to_path_buf();
        registry.staged = staged.unwrap_or_else(|| Path::new("")).to_path_buf();
        write_registry(&registry_path, &registry)
    })
}

// A terminated SMB process can retain its target handle until the driver finishes cancellation.
// Keep that target separate from subsequent attempts and preserve its ownership for later cleanup.
pub(crate) fn retire_staging(path: &Path, staged: &Path) -> io::Result<()> {
    REGISTRATION.with(|slot| {
        let Some(registry_path) = slot.borrow().clone() else {
            return Ok(());
        };
        let mut registry = read_registry(&registry_path)?;
        if registry.current != path || registry.staged != staged {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cannot retire unregistered copy staging",
            ));
        }
        validate_staging_ownership(path, staged)?;
        registry.retired.push(std::mem::take(&mut registry.staged));
        write_registry(&registry_path, &registry)
    })
}

fn owned_temporary_name(path: &Path, pid: u32) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let prefix = format!(".asterfiles-copy-{pid}-");
    let Some(suffix) = name.strip_prefix(&prefix) else {
        return false;
    };
    let Some((stamp, sequence)) = suffix.split_once('-') else {
        return false;
    };
    stamp.parse::<u128>().is_ok_and(|value| value > 0)
        && sequence.parse::<u64>().is_ok_and(|value| value > 0)
}

fn validate_staging_ownership(base: &Path, staged: &Path) -> io::Result<PathBuf> {
    let invalid = || {
        snapshot_error(
            staged,
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "invalid copy staging ownership",
            ),
        )
    };
    let stage = staged.parent().ok_or_else(invalid)?;
    let extension = stage
        .extension()
        .and_then(OsStr::to_str)
        .ok_or_else(invalid)?;
    let attempt = extension.strip_prefix("stage-").ok_or_else(invalid)?;
    let number = attempt.parse::<u64>().map_err(|_| invalid())?;
    if attempt != number.to_string()
        || stage != base.with_extension(extension)
        || staged.file_name().is_none()
    {
        return Err(invalid());
    }
    Ok(stage.to_path_buf())
}

fn retired_base(staged: &Path, pid: u32) -> io::Result<PathBuf> {
    let base = staged
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .with_extension("");
    if !owned_temporary_name(&base, pid) {
        return Err(snapshot_error(
            staged,
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unowned retired copy staging",
            ),
        ));
    }
    validate_staging_ownership(&base, staged)?;
    Ok(base)
}

// False means only retired targets are still held open; their registry must remain on disk.
pub(super) fn cleanup_registered(
    registry_path: &Path,
    pid: u32,
    completed: bool,
) -> io::Result<bool> {
    let mut registry = read_registry(registry_path)?;
    if completed {
        // Successful publication already consumed the current temporary file in the helper.
        registry.current.clear();
        registry.staged.clear();
    }
    if !registry.current.as_os_str().is_empty() {
        if !owned_temporary_name(&registry.current, pid) {
            return Err(snapshot_error(
                &registry.current,
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "temporary copy cleanup refused an unowned path",
                ),
            ));
        }
        validate_local_path(&registry.current)?;
    } else if !registry.staged.as_os_str().is_empty() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    if !registry.staged.as_os_str().is_empty() {
        validate_staging_ownership(&registry.current, &registry.staged)?;
        validate_local_path(&registry.staged)?;
    }
    // Validate every ownership claim before deleting any file from the registry.
    let mut deferred = Vec::new();
    for staged in &registry.retired {
        retired_base(staged, pid)?;
        if crate::network::is_unc_path(staged) {
            record_deferred(
                registry_path,
                staged,
                &io::Error::other("remote staging cleanup deferred"),
            );
            deferred.push(staged.clone());
            continue;
        }
        match validate_local_path(staged) {
            Ok(()) => {}
            Err(error) if deferred_cleanup_error(&error) => {
                record_deferred(registry_path, staged, &error);
                deferred.push(staged.clone());
            }
            Err(error) => return Err(error),
        }
    }
    if !registry.staged.as_os_str().is_empty() {
        remove_staging(&registry.staged)?;
    }
    if !registry.current.as_os_str().is_empty() {
        remove_owned_file(&registry.current)?;
    }
    registry.current.clear();
    registry.staged.clear();
    let mut pending = Vec::new();
    for staged in registry.retired {
        if deferred.contains(&staged) {
            pending.push(staged);
            continue;
        }
        match remove_staging(&staged) {
            Ok(()) => {}
            Err(error) if deferred_cleanup_error(&error) => {
                record_deferred(registry_path, &staged, &error);
                pending.push(staged);
            }
            Err(error) => return Err(snapshot_error(&staged, error)),
        }
    }
    registry.retired = pending;
    if !registry.retired.is_empty() {
        write_registry(registry_path, &registry)?;
        return Ok(false);
    }
    Ok(true)
}

fn deferred_cleanup_error(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(5 | 32 | 33))
}

fn record_deferred(registry: &Path, staged: &Path, error: &io::Error) {
    crate::operation_audit::record(
        "network-copy-cleanup-deferred",
        format!(
            "registry={} staged={} error={error}",
            registry.display(),
            staged.display()
        ),
    );
}

fn remove_staging(staged: &Path) -> io::Result<()> {
    remove_owned_file(staged)?;
    let stage = staged.parent().expect("validated staging parent");
    match std::fs::remove_dir(stage) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn remove_owned_file(path: &Path) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn validate_local_path(path: &Path) -> io::Result<()> {
    let validate = || -> io::Result<()> {
        if !path.is_absolute() || crate::network::is_unc_path(path) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary copy cleanup requires a local absolute path",
            ));
        }
        let mut components = path.components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            return Err(io::ErrorKind::PermissionDenied.into());
        };
        if !matches!(
            prefix.kind(),
            std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_)
        ) || components.next() != Some(Component::RootDir)
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut ancestor = PathBuf::from(prefix.as_os_str());
        ancestor.push(Path::new(r"\"));
        let root = wide_null(ancestor.as_os_str());
        if unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) } != 3 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "temporary copy cleanup skipped a non-fixed drive",
            ));
        }
        for component in components {
            if !matches!(component, Component::Normal(_)) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            ancestor.push(component);
            match std::fs::symlink_metadata(&ancestor) {
                Ok(metadata) if metadata.file_attributes() & 0x400 != 0 => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "temporary copy cleanup skipped a reparse point",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    };
    validate().map_err(|error| {
        if error.raw_os_error().is_some() {
            error
        } else {
            snapshot_error(path, error)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "asterfiles-cleanup-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn issue_137_successful_network_destination_is_not_reopened_for_cleanup() {
        let root = root("completed-network");
        let registry = root.join("registry");
        let destination = PathBuf::from(format!(
            r"\\not-a-server\share\.asterfiles-copy-{}-12345-1",
            std::process::id()
        ));
        let _registration = Registration::new(registry.clone());
        register_staging(
            &destination,
            &destination.with_extension("stage-0").join("source.bin"),
        )
        .unwrap();
        assert!(cleanup_registered(&registry, std::process::id(), true).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_registered_copy_cleanup_preserves_formal_and_other_files() {
        let root = root("owned");
        let registry = root.join("registry");
        let owned = root.join(format!(".asterfiles-copy-{}-12345-1", std::process::id()));
        let other = root.join(".asterfiles-copy-99999-12345-2");
        let formal = root.join("original.bin");
        for path in [&owned, &other, &formal] {
            std::fs::write(path, b"keep").unwrap();
        }
        let _registration = Registration::new(registry.clone());
        register(&owned).unwrap();
        cleanup_registered(&registry, std::process::id(), false).unwrap();
        assert!(!owned.exists());
        assert_eq!(std::fs::read(&formal).unwrap(), b"keep");
        assert_eq!(std::fs::read(&other).unwrap(), b"keep");
        cleanup_registered(&registry, std::process::id(), false).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_registered_staging_cleanup_removes_only_registered_file() {
        let root = root("staging");
        let registry = root.join("registry");
        let destination = root.join(format!(".asterfiles-copy-{}-12345-1", std::process::id()));
        let stage = destination.with_extension("stage-0");
        std::fs::create_dir(&stage).unwrap();
        let staged = stage.join("source.bin");
        std::fs::write(&staged, b"partial").unwrap();
        let _registration = Registration::new(registry.clone());
        register_staging(&destination, &staged).unwrap();
        cleanup_registered(&registry, std::process::id(), false).unwrap();
        assert!(!stage.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_retired_staging_survives_new_file_registration_and_locked_cleanup() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = root("retired-held");
        let registry_path = root.join("registry");
        let first = root.join(format!(".asterfiles-copy-{}-12345-1", std::process::id()));
        let first_stage = first.with_extension("stage-0");
        std::fs::create_dir(&first_stage).unwrap();
        let retired = first_stage.join("source.bin");
        std::fs::write(&retired, b"pending kernel cancellation").unwrap();
        let _registration = Registration::new(registry_path.clone());
        register_staging(&first, &retired).unwrap();
        retire_staging(&first, &retired).unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&retired)
            .unwrap();

        let second = root.join(format!(".asterfiles-copy-{}-12345-2", std::process::id()));
        register(&second).unwrap();
        let second_stage = second.with_extension("stage-1");
        let staged = second_stage.join("next.bin");
        register_staging(&second, &staged).unwrap();
        std::fs::create_dir(&second_stage).unwrap();
        std::fs::write(&staged, b"current partial").unwrap();
        assert!(!cleanup_registered(&registry_path, std::process::id(), false).unwrap());
        assert!(!second_stage.exists());
        assert!(retired.exists());
        let pending = read_registry(&registry_path).unwrap();
        assert!(pending.current.as_os_str().is_empty());
        assert!(pending.staged.as_os_str().is_empty());
        assert_eq!(pending.retired.as_slice(), std::slice::from_ref(&retired));
        assert!(!cleanup_registered(&registry_path, std::process::id(), false).unwrap());

        drop(held);
        assert!(cleanup_registered(&registry_path, std::process::id(), false).unwrap());
        assert!(!first_stage.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_current_staging_lock_still_reports_cleanup_failure() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = root("current-held");
        let registry = root.join("registry");
        let current = root.join(format!(".asterfiles-copy-{}-12345-1", std::process::id()));
        let stage = current.with_extension("stage-0");
        let staged = stage.join("source.bin");
        let _registration = Registration::new(registry.clone());
        register_staging(&current, &staged).unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(&staged, b"partial").unwrap();
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&staged)
            .unwrap();
        assert!(cleanup_registered(&registry, std::process::id(), false).is_err());
        assert_eq!(read_registry(&registry).unwrap().staged, staged);
        drop(held);
        assert!(cleanup_registered(&registry, std::process::id(), false).unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_retired_cleanup_rejects_invalid_ownership_before_removing_anything() {
        let root = root("retired-ownership");
        let registry = root.join("registry");
        let current = root.join(format!(".asterfiles-copy-{}-12345-1", std::process::id()));
        std::fs::write(&current, b"keep until all ownership is validated").unwrap();
        let malformed = [
            root.join(".asterfiles-copy-99999-12345-2.stage-0/source.bin"),
            current.with_extension("stage").join("source.bin"),
            current.with_extension("stage-01").join("source.bin"),
            current.with_extension("stage-0").join("../original.bin"),
            root.join("original.stage-0/source.bin"),
        ];
        for staged in malformed {
            write_registry(
                &registry,
                &Registry {
                    current: current.clone(),
                    staged: PathBuf::new(),
                    retired: vec![staged],
                },
            )
            .unwrap();
            assert_eq!(
                cleanup_registered(&registry, std::process::id(), false)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::PermissionDenied
            );
            assert!(current.exists());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_staging_registry_enforces_bounded_record_count() {
        let root = root("registry-limit");
        let registry = root.join("registry");
        let current = root.join(format!(".asterfiles-copy-{}-12345-1", std::process::id()));
        let staged = current.with_extension("stage-0").join("source.bin");
        assert_eq!(
            write_registry(
                &registry,
                &Registry {
                    current,
                    staged: staged.clone(),
                    retired: vec![staged; MAX_STAGED_PATHS],
                },
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        let mut bytes = Vec::new();
        write_units(&mut bytes, &[]).unwrap();
        write_units(&mut bytes, &[]).unwrap();
        bytes.extend_from_slice(&u64::MAX.to_le_bytes());
        atomic_write(&registry, &bytes).unwrap();
        assert_eq!(
            read_registry(&registry).err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_registration_failure_prevents_creating_copy() {
        let root = root("registration-failure");
        let source = root.join("source.bin");
        let destination = root.join("target.bin");
        std::fs::write(&source, b"source").unwrap();
        std::fs::create_dir(root.join("registry")).unwrap();
        let _registration = Registration::new(root.join("registry"));
        let result = crate::fs::file_operations::copy_path_with_progress(
            &source,
            &destination,
            &crate::domain::file_operations::CancellationToken::new(),
            &mut |_, _, _| crate::domain::file_operations::ConflictAction::Skip,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
            &crate::fs::file_operations::CopyScanProgress::default(),
        );
        assert!(result.is_err());
        assert!(!destination.exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn issue_137_cleanup_rejects_foreign_process_or_network_registration() {
        let root = root("refused");
        let registry = root.join("registry");
        let other = root.join(".asterfiles-copy-99999-12345-2");
        std::fs::write(&other, b"keep").unwrap();
        let _registration = Registration::new(registry.clone());
        register(&other).unwrap();
        assert_eq!(
            cleanup_registered(&registry, std::process::id(), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(std::fs::read(&other).unwrap(), b"keep");
        register(&PathBuf::from(format!(
            r"\\not-a-server\share\.asterfiles-copy-{}-12345-1",
            std::process::id()
        )))
        .unwrap();
        assert_eq!(
            cleanup_registered(&registry, std::process::id(), false)
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
