#![cfg_attr(test, allow(dead_code))]

//! Current-user Windows Shell registration for AsterFiles.
//!
//! Every public operation is synchronous and may access the registry. Callers must run it on a
//! worker thread rather than the UI thread.

use std::{
    ffi::{OsStr, OsString},
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
};

use windows_sys::Win32::{
    Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS},
    System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_BINARY, REG_DWORD,
        REG_OPEN_CREATE_OPTIONS, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE, RegCloseKey,
        RegCreateKeyExW, RegDeleteTreeW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
        RegSetValueExW,
    },
    UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify},
};

const CLASSES_BASE: &str = r"Software\Classes";
const STATE_BASE: &str = r"Software\AsterFiles\ShellIntegration";
const FOLDER_STATE: &str = "FolderOpen";
const WIN_E_STATE: &str = "WinE";
const ACTIVE: &str = "Active";
const BOUND_EXECUTABLE: &str = "BoundExecutable";
const BACKUP_COMPLETE: &str = "BackupComplete";

// Folder is the Windows Shell class shared by filesystem folders and drive roots.
const FOLDER_TARGETS: &[Target] = &[
    Target::new("Open", r"Folder\shell\open\command", Some("%1")),
    Target::new("Explore", r"Folder\shell\explore\command", Some("%1")),
];
const WIN_E_TARGETS: &[Target] = &[Target::new(
    "OpenNewWindow",
    r"CLSID\{52205fd8-5dfb-447d-801a-d0b52f2e83e1}\shell\opennewwindow\command",
    None,
)];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellIntegrationStatus {
    Disabled,
    Enabled {
        executable: PathBuf,
    },
    NeedsRepair {
        bound_executable: PathBuf,
        reason: ShellIntegrationRepairReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellIntegrationRepairReason {
    ExecutableMovedOrMissing,
    RegistryChanged,
}

pub fn folder_open_status() -> io::Result<ShellIntegrationStatus> {
    status(
        &RegistryScope::production(),
        FOLDER_STATE,
        FOLDER_TARGETS,
        &std::env::current_exe()?,
    )
}

pub fn enable_folder_open() -> io::Result<()> {
    enable(
        &RegistryScope::production(),
        FOLDER_STATE,
        FOLDER_TARGETS,
        &std::env::current_exe()?,
    )?;
    notify_shell();
    Ok(())
}

pub fn repair_folder_open() -> io::Result<()> {
    enable_folder_open()
}

pub fn restore_folder_open() -> io::Result<()> {
    restore(&RegistryScope::production(), FOLDER_STATE, FOLDER_TARGETS)?;
    notify_shell();
    Ok(())
}

pub fn win_e_status() -> io::Result<ShellIntegrationStatus> {
    status(
        &RegistryScope::production(),
        WIN_E_STATE,
        WIN_E_TARGETS,
        &std::env::current_exe()?,
    )
}

pub fn enable_win_e() -> io::Result<()> {
    enable(
        &RegistryScope::production(),
        WIN_E_STATE,
        WIN_E_TARGETS,
        &std::env::current_exe()?,
    )?;
    notify_shell();
    Ok(())
}

pub fn repair_win_e() -> io::Result<()> {
    enable_win_e()
}

pub fn restore_win_e() -> io::Result<()> {
    restore(&RegistryScope::production(), WIN_E_STATE, WIN_E_TARGETS)?;
    notify_shell();
    Ok(())
}

fn notify_shell() {
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            ptr::null(),
            ptr::null(),
        )
    };
}

#[derive(Clone, Copy)]
struct Target {
    backup_prefix: &'static str,
    key: &'static str,
    argument: Option<&'static str>,
}

impl Target {
    const fn new(
        backup_prefix: &'static str,
        key: &'static str,
        argument: Option<&'static str>,
    ) -> Self {
        Self {
            backup_prefix,
            key,
            argument,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegistryValue {
    value_type: REG_VALUE_TYPE,
    data: Vec<u8>,
}

#[derive(Clone)]
struct RegistryScope {
    classes_base: String,
    state_base: String,
}

impl RegistryScope {
    fn production() -> Self {
        Self {
            classes_base: CLASSES_BASE.to_owned(),
            state_base: STATE_BASE.to_owned(),
        }
    }

    fn class_key(&self, relative: &str) -> String {
        format!(r"{}\{}", self.classes_base, relative)
    }

    fn state_key(&self, integration: &str) -> String {
        format!(r"{}\{}", self.state_base, integration)
    }
}

fn status(
    scope: &RegistryScope,
    integration: &str,
    targets: &[Target],
    current_executable: &Path,
) -> io::Result<ShellIntegrationStatus> {
    let state_path = scope.state_key(integration);
    if read_dword(&state_path, ACTIVE)? != Some(1)
        && read_dword(&state_path, BACKUP_COMPLETE)? != Some(1)
    {
        return Ok(ShellIntegrationStatus::Disabled);
    }
    let Some(bound_executable) = read_string(&state_path, BOUND_EXECUTABLE)? else {
        return Ok(ShellIntegrationStatus::NeedsRepair {
            bound_executable: PathBuf::new(),
            reason: ShellIntegrationRepairReason::RegistryChanged,
        });
    };
    let bound_executable = PathBuf::from(bound_executable);
    for target in targets {
        if !target_is_owned(scope, *target, &bound_executable)? {
            return Ok(ShellIntegrationStatus::NeedsRepair {
                bound_executable,
                reason: ShellIntegrationRepairReason::RegistryChanged,
            });
        }
    }
    if bound_executable != current_executable || !bound_executable.is_file() {
        return Ok(ShellIntegrationStatus::NeedsRepair {
            bound_executable,
            reason: ShellIntegrationRepairReason::ExecutableMovedOrMissing,
        });
    }
    Ok(ShellIntegrationStatus::Enabled {
        executable: current_executable.to_owned(),
    })
}

fn enable(
    scope: &RegistryScope,
    integration: &str,
    targets: &[Target],
    executable: &Path,
) -> io::Result<()> {
    let state_path = scope.state_key(integration);
    let has_backup = read_dword(&state_path, BACKUP_COMPLETE)? == Some(1);
    if !has_backup {
        let backup_result = (|| {
            for target in targets {
                backup_value(
                    &state_path,
                    target.backup_prefix,
                    "Default",
                    read_value(&scope.class_key(target.key), "")?,
                )?;
                backup_value(
                    &state_path,
                    target.backup_prefix,
                    "DelegateExecute",
                    read_value(&scope.class_key(target.key), "DelegateExecute")?,
                )?;
            }
            write_string(&state_path, BOUND_EXECUTABLE, executable.as_os_str())?;
            write_dword(&state_path, BACKUP_COMPLETE, 1)
        })();
        if backup_result.is_err() {
            let _ = delete_tree(HKEY_CURRENT_USER, &state_path, true);
            return backup_result;
        }
    }

    let result = (|| {
        write_string(&state_path, BOUND_EXECUTABLE, executable.as_os_str())?;
        for target in targets {
            let key = scope.class_key(target.key);
            write_string(&key, "", command_for(executable, target.argument))?;
            write_string(&key, "DelegateExecute", "")?;
        }
        write_dword(&state_path, ACTIVE, 1)
    })();
    if result.is_err() && !has_backup {
        let _ = restore_backups(scope, &state_path, targets);
        let _ = delete_tree(HKEY_CURRENT_USER, &state_path, true);
    }
    result
}

fn restore_backups(scope: &RegistryScope, state_path: &str, targets: &[Target]) -> io::Result<()> {
    for target in targets {
        let key = scope.class_key(target.key);
        restore_value(state_path, target.backup_prefix, "Default", &key, "")?;
        restore_value(
            state_path,
            target.backup_prefix,
            "DelegateExecute",
            &key,
            "DelegateExecute",
        )?;
    }
    Ok(())
}
fn restore(scope: &RegistryScope, integration: &str, targets: &[Target]) -> io::Result<()> {
    let state_path = scope.state_key(integration);
    if read_dword(&state_path, ACTIVE)? != Some(1)
        && read_dword(&state_path, BACKUP_COMPLETE)? != Some(1)
    {
        return Ok(());
    }
    let bound_executable = read_string(&state_path, BOUND_EXECUTABLE)?
        .map(PathBuf::from)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing bound executable"))?;

    for target in targets {
        let key = scope.class_key(target.key);
        if target_is_owned(scope, *target, &bound_executable)? {
            restore_value(&state_path, target.backup_prefix, "Default", &key, "")?;
            restore_value(
                &state_path,
                target.backup_prefix,
                "DelegateExecute",
                &key,
                "DelegateExecute",
            )?;
        }
    }
    delete_tree(HKEY_CURRENT_USER, &state_path, true)
}

fn target_is_owned(scope: &RegistryScope, target: Target, executable: &Path) -> io::Result<bool> {
    let key = scope.class_key(target.key);
    Ok(
        read_value(&key, "")? == Some(string_value(command_for(executable, target.argument)))
            && read_value(&key, "DelegateExecute")? == Some(string_value("")),
    )
}

fn command_for(executable: &Path, argument: Option<&str>) -> OsString {
    let mut command = OsString::from("\"");
    command.push(executable.as_os_str());
    command.push("\"");
    if let Some(argument) = argument {
        command.push(" \"");
        command.push(argument);
        command.push("\"");
    }
    command
}

fn backup_value(
    state_path: &str,
    prefix: &str,
    field: &str,
    value: Option<RegistryValue>,
) -> io::Result<()> {
    match value {
        Some(value) => {
            write_dword(state_path, &format!("{prefix}{field}Present"), 1)?;
            write_dword(
                state_path,
                &format!("{prefix}{field}Type"),
                value.value_type,
            )?;
            write_raw(
                state_path,
                &format!("{prefix}{field}Data"),
                REG_BINARY,
                &value.data,
            )
        }
        None => write_dword(state_path, &format!("{prefix}{field}Present"), 0),
    }
}

fn restore_value(
    state_path: &str,
    prefix: &str,
    field: &str,
    target_path: &str,
    target_name: &str,
) -> io::Result<()> {
    let marker = read_dword(state_path, &format!("{prefix}{field}Present"))?;
    if marker == Some(0) {
        return delete_value(target_path, target_name);
    }
    if marker != Some(1) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing registry backup marker",
        ));
    }
    let value_type = read_dword(state_path, &format!("{prefix}{field}Type"))?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing registry backup type")
    })?;
    let data = read_value(state_path, &format!("{prefix}{field}Data"))?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing registry backup data"))?
        .data;
    write_raw(target_path, target_name, value_type, &data)
}

fn read_string(path: &str, name: &str) -> io::Result<Option<OsString>> {
    let Some(value) = read_value(path, name)? else {
        return Ok(None);
    };
    if value.value_type != REG_SZ || value.data.len() % 2 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid registry string {path}\\{name}"),
        ));
    }
    let mut units = value
        .data
        .chunks_exact(2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
        .collect::<Vec<_>>();
    while units.last() == Some(&0) {
        units.pop();
    }
    Ok(Some(OsString::from_wide(&units)))
}

fn read_dword(path: &str, name: &str) -> io::Result<Option<u32>> {
    let Some(value) = read_value(path, name)? else {
        return Ok(None);
    };
    if value.value_type != REG_DWORD || value.data.len() != 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid registry DWORD {path}\\{name}"),
        ));
    }
    Ok(Some(u32::from_le_bytes(value.data.try_into().unwrap())))
}

fn read_value(path: &str, name: &str) -> io::Result<Option<RegistryValue>> {
    let Some(key) = open_key(HKEY_CURRENT_USER, path, KEY_READ)? else {
        return Ok(None);
    };
    let name = wide(name);
    let mut value_type = 0;
    let mut size = 0;
    let result = unsafe {
        RegQueryValueExW(
            key.0,
            name.as_ptr(),
            ptr::null(),
            &mut value_type,
            ptr::null_mut(),
            &mut size,
        )
    };
    if result == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(result)?;
    let mut data = vec![0; size as usize];
    loop {
        let mut actual_size = data.len() as u32;
        let result = unsafe {
            RegQueryValueExW(
                key.0,
                name.as_ptr(),
                ptr::null(),
                &mut value_type,
                data.as_mut_ptr(),
                &mut actual_size,
            )
        };
        if result == ERROR_MORE_DATA {
            data.resize(actual_size as usize, 0);
            continue;
        }
        check(result)?;
        data.truncate(actual_size as usize);
        return Ok(Some(RegistryValue { value_type, data }));
    }
}

fn write_string(path: &str, name: &str, value: impl AsRef<OsStr>) -> io::Result<()> {
    write_raw(path, name, REG_SZ, &string_value(value).data)
}

fn string_value(value: impl AsRef<OsStr>) -> RegistryValue {
    RegistryValue {
        value_type: REG_SZ,
        data: value
            .as_ref()
            .encode_wide()
            .chain(Some(0))
            .flat_map(u16::to_le_bytes)
            .collect(),
    }
}

fn write_dword(path: &str, name: &str, value: u32) -> io::Result<()> {
    write_raw(path, name, REG_DWORD, &value.to_le_bytes())
}

fn write_raw(path: &str, name: &str, value_type: REG_VALUE_TYPE, data: &[u8]) -> io::Result<()> {
    let key = create_key(HKEY_CURRENT_USER, path, KEY_READ | KEY_WRITE)?;
    let name = wide(name);
    check(unsafe {
        RegSetValueExW(
            key.0,
            name.as_ptr(),
            0,
            value_type,
            data.as_ptr(),
            data.len() as u32,
        )
    })
}

fn delete_value(path: &str, name: &str) -> io::Result<()> {
    let Some(key) = open_key(HKEY_CURRENT_USER, path, KEY_WRITE)? else {
        return Ok(());
    };
    let result = unsafe { RegDeleteValueW(key.0, wide(name).as_ptr()) };
    if result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(result)
    }
}

fn delete_tree(root: HKEY, path: &str, missing_ok: bool) -> io::Result<()> {
    let result = unsafe { RegDeleteTreeW(root, wide(path).as_ptr()) };
    if missing_ok && result == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        check(result)
    }
}

fn open_key(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> io::Result<Option<RegKey>> {
    let mut key = ptr::null_mut();
    let result = unsafe { RegOpenKeyExW(root, wide(path).as_ptr(), 0, access, &mut key) };
    if result == ERROR_FILE_NOT_FOUND {
        Ok(None)
    } else {
        check(result)?;
        Ok(Some(RegKey(key)))
    }
}

fn create_key(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> io::Result<RegKey> {
    let mut key = ptr::null_mut();
    check(unsafe {
        RegCreateKeyExW(
            root,
            wide(path).as_ptr(),
            0,
            ptr::null(),
            REG_OPEN_CREATE_OPTIONS::default(),
            access,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    })?;
    Ok(RegKey(key))
}

struct RegKey(HKEY);
impl Drop for RegKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn check(result: u32) -> io::Result<()> {
    if result == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result as i32))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_SCOPE: AtomicU64 = AtomicU64::new(0);

    struct TestScope {
        root: String,
        registry: RegistryScope,
        executable: PathBuf,
    }

    impl TestScope {
        fn new() -> io::Result<Self> {
            let suffix = NEXT_SCOPE.fetch_add(1, Ordering::Relaxed);
            let root = format!(
                r"Software\AsterFiles\Tests\ShellIntegration-{}-{suffix}",
                std::process::id()
            );
            let registry = RegistryScope {
                classes_base: format!(r"{root}\Classes"),
                state_base: format!(r"{root}\State"),
            };
            let executable = std::env::temp_dir().join(format!(
                "asterfiles-shell-integration-{}-{suffix}.exe",
                std::process::id()
            ));
            fs::write(&executable, [])?;
            Ok(Self {
                root,
                registry,
                executable,
            })
        }
    }

    impl Drop for TestScope {
        fn drop(&mut self) {
            let _ = delete_tree(HKEY_CURRENT_USER, &self.root, true);
            let _ = fs::remove_file(&self.executable);
        }
    }

    #[test]
    fn folder_enable_status_and_restore_preserve_previous_values() -> io::Result<()> {
        let test = TestScope::new()?;
        for target in FOLDER_TARGETS {
            let key = test.registry.class_key(target.key);
            write_string(&key, "", "previous command")?;
            write_string(&key, "DelegateExecute", "previous delegate")?;
        }
        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;
        assert_eq!(
            status(
                &test.registry,
                FOLDER_STATE,
                FOLDER_TARGETS,
                &test.executable
            )?,
            ShellIntegrationStatus::Enabled {
                executable: test.executable.clone()
            }
        );
        restore(&test.registry, FOLDER_STATE, FOLDER_TARGETS)?;
        for target in FOLDER_TARGETS {
            let key = test.registry.class_key(target.key);
            assert_eq!(read_string(&key, "")?, Some("previous command".into()));
            assert_eq!(
                read_string(&key, "DelegateExecute")?,
                Some("previous delegate".into())
            );
        }
        Ok(())
    }

    #[test]
    fn restore_only_removes_values_still_owned_by_asterfiles() -> io::Result<()> {
        let test = TestScope::new()?;
        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;
        let key = test.registry.class_key(FOLDER_TARGETS[0].key);
        write_string(&key, "", "another app")?;
        restore(&test.registry, FOLDER_STATE, FOLDER_TARGETS)?;
        assert_eq!(read_string(&key, "")?, Some("another app".into()));
        assert_eq!(read_string(&key, "DelegateExecute")?, Some("".into()));
        Ok(())
    }

    #[test]
    fn repair_keeps_the_original_backup() -> io::Result<()> {
        let test = TestScope::new()?;
        let key = test.registry.class_key(FOLDER_TARGETS[0].key);
        write_string(&key, "", "original")?;
        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;
        write_string(&key, "", "damaged")?;
        let moved = test.executable.with_file_name("moved.exe");
        fs::write(&moved, [])?;
        enable(&test.registry, FOLDER_STATE, FOLDER_TARGETS, &moved)?;
        restore(&test.registry, FOLDER_STATE, FOLDER_TARGETS)?;
        assert_eq!(read_string(&key, "")?, Some("original".into()));
        fs::remove_file(moved)?;
        Ok(())
    }

    #[test]
    fn win_e_restore_does_not_affect_folder_registration() -> io::Result<()> {
        let test = TestScope::new()?;
        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;
        enable(&test.registry, WIN_E_STATE, WIN_E_TARGETS, &test.executable)?;
        restore(&test.registry, WIN_E_STATE, WIN_E_TARGETS)?;
        for target in FOLDER_TARGETS {
            assert!(target_is_owned(&test.registry, *target, &test.executable)?);
        }
        let key = test.registry.class_key(WIN_E_TARGETS[0].key);
        assert_eq!(read_value(&key, "")?, None);
        assert_eq!(read_value(&key, "DelegateExecute")?, None);
        Ok(())
    }

    #[test]
    fn status_distinguishes_a_moved_executable_from_changed_registry() -> io::Result<()> {
        let test = TestScope::new()?;
        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;

        let moved = test.executable.with_file_name("current-location.exe");
        fs::write(&moved, [])?;
        assert_eq!(
            status(&test.registry, FOLDER_STATE, FOLDER_TARGETS, &moved)?,
            ShellIntegrationStatus::NeedsRepair {
                bound_executable: test.executable.clone(),
                reason: ShellIntegrationRepairReason::ExecutableMovedOrMissing,
            }
        );

        let key = test.registry.class_key(FOLDER_TARGETS[0].key);
        write_string(&key, "", "another app")?;
        assert_eq!(
            status(&test.registry, FOLDER_STATE, FOLDER_TARGETS, &moved)?,
            ShellIntegrationStatus::NeedsRepair {
                bound_executable: test.executable.clone(),
                reason: ShellIntegrationRepairReason::RegistryChanged,
            }
        );
        fs::remove_file(moved)?;
        Ok(())
    }

    #[test]
    fn incomplete_enable_state_is_repairable_and_restorable() -> io::Result<()> {
        let test = TestScope::new()?;
        let key = test.registry.class_key(FOLDER_TARGETS[0].key);
        write_string(&key, "", "original")?;
        backup_value(
            &test.registry.state_key(FOLDER_STATE),
            FOLDER_TARGETS[0].backup_prefix,
            "Default",
            read_value(&key, "")?,
        )?;
        backup_value(
            &test.registry.state_key(FOLDER_STATE),
            FOLDER_TARGETS[0].backup_prefix,
            "DelegateExecute",
            read_value(&key, "DelegateExecute")?,
        )?;
        for target in &FOLDER_TARGETS[1..] {
            let target_key = test.registry.class_key(target.key);
            backup_value(
                &test.registry.state_key(FOLDER_STATE),
                target.backup_prefix,
                "Default",
                read_value(&target_key, "")?,
            )?;
            backup_value(
                &test.registry.state_key(FOLDER_STATE),
                target.backup_prefix,
                "DelegateExecute",
                read_value(&target_key, "DelegateExecute")?,
            )?;
        }
        write_string(
            &test.registry.state_key(FOLDER_STATE),
            BOUND_EXECUTABLE,
            test.executable.as_os_str(),
        )?;
        write_dword(&test.registry.state_key(FOLDER_STATE), BACKUP_COMPLETE, 1)?;
        write_string(&key, "", command_for(&test.executable, Some("%1")))?;
        write_string(&key, "DelegateExecute", "")?;

        assert!(matches!(
            status(
                &test.registry,
                FOLDER_STATE,
                FOLDER_TARGETS,
                &test.executable
            )?,
            ShellIntegrationStatus::NeedsRepair { .. }
        ));
        restore(&test.registry, FOLDER_STATE, FOLDER_TARGETS)?;
        assert_eq!(read_string(&key, "")?, Some("original".into()));
        assert_eq!(read_value(&key, "DelegateExecute")?, None);
        Ok(())
    }
    #[test]
    fn repeated_enable_does_not_replace_the_original_backup() -> io::Result<()> {
        let test = TestScope::new()?;
        let key = test.registry.class_key(FOLDER_TARGETS[0].key);
        write_string(&key, "", "original")?;

        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;
        enable(
            &test.registry,
            FOLDER_STATE,
            FOLDER_TARGETS,
            &test.executable,
        )?;
        restore(&test.registry, FOLDER_STATE, FOLDER_TARGETS)?;

        assert_eq!(read_string(&key, "")?, Some("original".into()));
        Ok(())
    }
    #[test]
    fn command_preserves_unicode_path() {
        assert_eq!(
            command_for(Path::new(r"C:\便携 应用\AsterFiles.exe"), Some("%1")),
            OsString::from(r#""C:\便携 应用\AsterFiles.exe" "%1""#)
        );
    }
}
