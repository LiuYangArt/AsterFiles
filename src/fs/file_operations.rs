use std::{
    ffi::{OsStr, OsString},
    fs, io,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crate::domain::file_operations::{
    CancellationToken, ConflictAction, ConflictCategory, FileIdentity, UndoItem,
};

#[cfg(test)]
const COPY_TEST_CHUNK_SIZE: usize = 1024 * 1024;
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NameValidationError {
    Empty,
    DotName,
    InvalidCharacter(char),
    TrailingSpaceOrDot,
    ReservedName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperationReport {
    pub files: usize,
    pub directories: usize,
    pub bytes: u64,
    pub skipped: Vec<PathBuf>,
    pub affected_directories: Vec<PathBuf>,
    pub cleanup_pending: Option<PathBuf>,
    pub completed_paths: Vec<PathBuf>,
    pub undo_identities: Vec<(PathBuf, FileIdentity)>,
    pub undo_root: Option<PathBuf>,
    pub undo_root_created_exclusively: bool,
}

impl FileOperationReport {
    fn new() -> Self {
        Self {
            files: 0,
            directories: 0,
            bytes: 0,
            skipped: Vec::new(),
            affected_directories: Vec::new(),
            cleanup_pending: None,
            completed_paths: Vec::new(),
            undo_identities: Vec::new(),
            undo_root: None,
            undo_root_created_exclusively: false,
        }
    }

    fn affect(&mut self, path: &Path) {
        if let Some(parent) = path.parent()
            && !self.affected_directories.iter().any(|item| item == parent)
        {
            self.affected_directories.push(parent.to_path_buf());
        }
        if !self.completed_paths.iter().any(|item| item == path) {
            self.completed_paths.push(path.to_path_buf());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationError {
    Cancelled,
    DestinationCommittedSourceRetained {
        source: PathBuf,
        destination: PathBuf,
        message: String,
    },
    InvalidName(NameValidationError),
    SourceInsideDestination,
    DestinationExists(PathBuf),
    ConflictSkipped(PathBuf),
    Io {
        path: PathBuf,
        kind: io::ErrorKind,
        message: String,
    },
}

impl OperationError {
    fn io(path: &Path, error: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

pub fn validate_name(name: &OsStr) -> Result<(), NameValidationError> {
    if name.is_empty() {
        return Err(NameValidationError::Empty);
    }
    let text = name.to_string_lossy();
    if text == "." || text == ".." {
        return Err(NameValidationError::DotName);
    }
    if text.ends_with(' ') || text.ends_with('.') {
        return Err(NameValidationError::TrailingSpaceOrDot);
    }
    if let Some(character) = text.chars().find(|character| {
        matches!(
            character,
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
        ) || *character < ' '
    }) {
        return Err(NameValidationError::InvalidCharacter(character));
    }
    let stem = text.split('.').next().unwrap_or_default();
    let upper = stem.to_ascii_uppercase();
    let reserved = matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || upper.strip_prefix("COM").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        })
        || upper.strip_prefix("LPT").is_some_and(|suffix| {
            matches!(suffix, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
        });
    if reserved {
        return Err(NameValidationError::ReservedName);
    }
    Ok(())
}

pub fn keep_both_path(destination: &Path) -> PathBuf {
    if !path_exists(destination) {
        return destination.to_path_buf();
    }
    let parent = destination.parent().unwrap_or_else(|| Path::new(""));
    let file_name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("item"));
    let is_directory = fs::symlink_metadata(destination)
        .map(|metadata| metadata.file_type().is_dir())
        .unwrap_or(false);
    let (stem, extension) = if is_directory {
        (file_name.to_os_string(), None)
    } else {
        split_name(file_name)
    };
    for index in 2_u64.. {
        let mut candidate = stem.clone();
        candidate.push(format!(" ({index})"));
        if let Some(extension) = &extension {
            candidate.push(".");
            candidate.push(extension);
        }
        let path = parent.join(candidate);
        if !path_exists(&path) {
            return path;
        }
    }
    unreachable!()
}

pub fn create_folder(parent: &Path, name: &OsStr) -> Result<PathBuf, OperationError> {
    create_folder_with_identity(parent, name).map(|(path, _)| path)
}

pub fn create_folder_with_identity(
    parent: &Path,
    name: &OsStr,
) -> Result<(PathBuf, FileIdentity), OperationError> {
    validate_name(name).map_err(OperationError::InvalidName)?;
    let requested = parent.join(name);
    let path = if path_exists(&requested) {
        keep_both_path(&requested)
    } else {
        requested
    };
    fs::create_dir(&path).map_err(|error| OperationError::io(&path, error))?;
    let identity = file_identity(&path)?;
    Ok((path, identity))
}

pub fn rename_path(source: &Path, new_name: &OsStr) -> Result<PathBuf, OperationError> {
    rename_path_with_identity(source, new_name).map(|(path, _)| path)
}

pub fn rename_path_with_identity(
    source: &Path,
    new_name: &OsStr,
) -> Result<(PathBuf, FileIdentity), OperationError> {
    validate_name(new_name).map_err(OperationError::InvalidName)?;
    let parent = source.parent().unwrap_or_else(|| Path::new(""));
    let destination = parent.join(new_name);
    if source == destination {
        let identity = file_identity(&destination)?;
        return Ok((destination, identity));
    }
    if path_exists(&destination) && !same_path_ignoring_ascii_case(source, &destination) {
        return Err(OperationError::DestinationExists(destination));
    }
    if same_path_ignoring_ascii_case(source, &destination) {
        let temporary = unique_sibling(source, ".asterfiles-rename");
        fs::rename(source, &temporary).map_err(|error| OperationError::io(source, error))?;
        if let Err(error) = fs::rename(&temporary, &destination) {
            let _ = fs::rename(&temporary, source);
            return Err(OperationError::io(&destination, error));
        }
    } else {
        fs::rename(source, &destination).map_err(|error| OperationError::io(source, error))?;
    }
    let identity = file_identity(&destination)?;
    Ok((destination, identity))
}

pub fn file_identity(path: &Path) -> Result<FileIdentity, OperationError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| OperationError::io(path, error))?;
    #[cfg(windows)]
    {
        use std::{os::windows::ffi::OsStrExt, ptr};
        use windows::Win32::{
            Foundation::CloseHandle,
            Storage::FileSystem::{
                CreateFileW, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO, FILE_READ_ATTRIBUTES,
                FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileBasicInfo, FileIdInfo,
                GetFileInformationByHandleEx, OPEN_EXISTING,
            },
        };

        let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
        if wide.contains(&0) {
            return Err(OperationError::Io {
                path: path.to_path_buf(),
                kind: io::ErrorKind::InvalidInput,
                message: "path contains a null character".to_owned(),
            });
        }
        wide.push(0);
        let handle = unsafe {
            CreateFileW(
                windows::core::PCWSTR(wide.as_ptr()),
                FILE_READ_ATTRIBUTES.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                None,
            )
        }
        .map_err(|error| OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::Other,
            message: error.to_string(),
        })?;
        let mut info = FILE_ID_INFO::default();
        let mut basic = FILE_BASIC_INFO::default();
        let result = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileIdInfo,
                ptr::addr_of_mut!(info).cast(),
                std::mem::size_of::<FILE_ID_INFO>() as u32,
            )
        };
        let basic_result = unsafe {
            GetFileInformationByHandleEx(
                handle,
                FileBasicInfo,
                ptr::addr_of_mut!(basic).cast(),
                std::mem::size_of::<FILE_BASIC_INFO>() as u32,
            )
        };
        let _ = unsafe { CloseHandle(handle) };
        result.map_err(|error| OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::Other,
            message: error.to_string(),
        })?;
        basic_result.map_err(|error| OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::Other,
            message: error.to_string(),
        })?;
        Ok(FileIdentity {
            volume_serial: info.VolumeSerialNumber,
            file_index: info.FileId.Identifier,
            is_directory: metadata.file_type().is_dir(),
            size_bytes: metadata.len(),
            modified: metadata.modified().ok(),
            change_time: basic.ChangeTime,
        })
    }
    #[cfg(not(windows))]
    {
        Ok(FileIdentity {
            volume_serial: 0,
            file_index: [0; 16],
            is_directory: metadata.file_type().is_dir(),
            size_bytes: metadata.len(),
            modified: metadata.modified().ok(),
            change_time: 0,
        })
    }
}

pub enum UndoExecution {
    Completed(FileOperationReport),
    RetryWith {
        item: UndoItem,
        message: String,
        affected_directories: Vec<PathBuf>,
    },
}

pub fn execute_undo_item(
    item: &UndoItem,
    cancel: &CancellationToken,
) -> Result<UndoExecution, OperationError> {
    check_cancel(cancel)?;
    match item {
        UndoItem::RemoveEmptyDirectory {
            path,
            identity,
            quarantined,
        } => {
            ensure_identity(path, *identity)?;
            if !*quarantined && file_identity(path)?.change_time != identity.change_time {
                return Err(OperationError::Io {
                    path: path.clone(),
                    kind: io::ErrorKind::InvalidData,
                    message: "created folder changed after creation".to_owned(),
                });
            }
            if fs::read_dir(path)
                .map_err(|error| OperationError::io(path, error))?
                .next()
                .is_some()
            {
                return Err(OperationError::Io {
                    path: path.clone(),
                    kind: io::ErrorKind::DirectoryNotEmpty,
                    message: "created folder is no longer empty".to_owned(),
                });
            }
            let isolated = if *quarantined {
                path.clone()
            } else {
                let isolated = unique_sibling_preserving_name(path, ".asterfiles-undo-empty");
                fs::rename(path, &isolated).map_err(|error| OperationError::io(path, error))?;
                isolated
            };
            if let Err(error) = ensure_identity(&isolated, *identity).and_then(|_| {
                if fs::read_dir(&isolated)
                    .map_err(|error| OperationError::io(&isolated, error))?
                    .next()
                    .is_none()
                {
                    Ok(())
                } else {
                    Err(OperationError::Io {
                        path: isolated.clone(),
                        kind: io::ErrorKind::DirectoryNotEmpty,
                        message: "created folder changed while being isolated".to_owned(),
                    })
                }
            }) {
                if !*quarantined && fs::rename(&isolated, path).is_ok() {
                    return Err(error);
                }
                return Ok(UndoExecution::RetryWith {
                    item: UndoItem::RemoveEmptyDirectory {
                        path: isolated,
                        identity: *identity,
                        quarantined: true,
                    },
                    message: format!("新建文件夹已隔离，但清理前发现变化：{error:?}"),
                    affected_directories: item.directories(),
                });
            }
            match fs::remove_dir(&isolated) {
                Ok(()) => {
                    let mut report = FileOperationReport::new();
                    report.affected_directories.extend(item.directories());
                    report.directories = 1;
                    Ok(UndoExecution::Completed(report))
                }
                Err(error) => Ok(UndoExecution::RetryWith {
                    item: UndoItem::RemoveEmptyDirectory {
                        path: isolated,
                        identity: *identity,
                        quarantined: true,
                    },
                    message: format!("新建文件夹已隔离，等待完成清理：{error}"),
                    affected_directories: item.directories(),
                }),
            }
        }
        UndoItem::RemoveCreated { path, manifest } => {
            ensure_manifest_strict(path, manifest)?;
            let quarantined = unique_sibling_preserving_name(path, ".asterfiles-undo");
            fs::rename(path, &quarantined).map_err(|error| OperationError::io(path, error))?;
            let quarantined_manifest = match rebase_manifest(manifest, path, &quarantined)
                .and_then(|manifest| refresh_manifest_identities(&manifest))
            {
                Ok(manifest) => manifest,
                Err(error) => {
                    let _ = fs::rename(&quarantined, path);
                    return Err(error);
                }
            };
            if let Err(error) = ensure_manifest(&quarantined, &quarantined_manifest) {
                if fs::rename(&quarantined, path).is_err() {
                    return Ok(UndoExecution::RetryWith {
                        item: UndoItem::RemoveQuarantined {
                            path: quarantined,
                            manifest: quarantined_manifest,
                        },
                        message: format!("撤销隔离后发现内容变化，且无法恢复原位置：{error:?}"),
                        affected_directories: item.directories(),
                    });
                }
                return Err(error);
            }
            remove_quarantined(&quarantined, quarantined_manifest, item.directories())
        }
        UndoItem::RemoveQuarantined { path, manifest } => {
            ensure_manifest(path, manifest)?;
            remove_quarantined(path, manifest.clone(), item.directories())
        }
        UndoItem::MoveBack {
            current,
            original,
            manifest,
            manifest_complete,
        } => {
            ensure_undo_manifest(current, manifest, *manifest_complete, true)?;
            if path_exists(original) {
                return Err(OperationError::DestinationExists(original.clone()));
            }
            let quarantined = unique_sibling_preserving_name(current, ".asterfiles-undo-move");
            fs::rename(current, &quarantined)
                .map_err(|error| OperationError::io(current, error))?;
            let quarantined_manifest = match rebase_manifest(manifest, current, &quarantined)
                .and_then(|manifest| refresh_manifest_identities(&manifest))
            {
                Ok(manifest) => manifest,
                Err(error) => {
                    let _ = fs::rename(&quarantined, current);
                    return Err(error);
                }
            };
            if let Err(error) = ensure_undo_manifest(
                &quarantined,
                &quarantined_manifest,
                *manifest_complete,
                false,
            ) {
                if fs::rename(&quarantined, current).is_err() {
                    return Ok(UndoExecution::RetryWith {
                        item: UndoItem::MoveBackQuarantined {
                            current: quarantined,
                            original: original.clone(),
                            manifest: quarantined_manifest,
                            manifest_complete: *manifest_complete,
                        },
                        message: format!(
                            "撤销移动隔离后发现内容变化，且无法恢复当前位置：{error:?}"
                        ),
                        affected_directories: item.directories(),
                    });
                }
                return Err(error);
            }
            execute_quarantined_move_back(
                &quarantined,
                original,
                &quarantined_manifest,
                *manifest_complete,
                cancel,
                item.directories(),
            )
        }
        UndoItem::MoveBackQuarantined {
            current,
            original,
            manifest,
            manifest_complete,
        } => {
            ensure_undo_manifest(current, manifest, *manifest_complete, true)?;
            if path_exists(original) {
                return Err(OperationError::DestinationExists(original.clone()));
            }
            execute_quarantined_move_back(
                current,
                original,
                manifest,
                *manifest_complete,
                cancel,
                item.directories(),
            )
        }
        UndoItem::FinalizeRestore {
            temporary,
            original,
            identity,
        } => {
            ensure_identity(temporary, *identity)?;
            if file_identity(temporary)?.change_time != identity.change_time {
                return Err(OperationError::Io {
                    path: temporary.clone(),
                    kind: io::ErrorKind::InvalidData,
                    message: "restored item changed while waiting".to_owned(),
                });
            }
            if path_exists(original) {
                return Err(OperationError::DestinationExists(original.clone()));
            }
            fs::rename(temporary, original).map_err(|error| OperationError::io(original, error))?;
            let mut report = FileOperationReport::new();
            report.affect(temporary);
            report.affect(original);
            Ok(UndoExecution::Completed(report))
        }
        UndoItem::RestoreRecycled { .. } => Err(OperationError::Io {
            path: PathBuf::new(),
            kind: io::ErrorKind::Unsupported,
            message: "recycle restore must use the Windows Shell".to_owned(),
        }),
    }
}

fn execute_quarantined_move_back(
    current: &Path,
    original: &Path,
    manifest: &[(PathBuf, FileIdentity)],
    manifest_complete: bool,
    cancel: &CancellationToken,
    affected_directories: Vec<PathBuf>,
) -> Result<UndoExecution, OperationError> {
    if fs::rename(current, original).is_ok() {
        let mut report = FileOperationReport::new();
        report.affect(current);
        report.affect(original);
        return Ok(UndoExecution::Completed(report));
    }
    check_cancel(cancel)?;
    if !manifest_complete {
        return Ok(UndoExecution::RetryWith {
            item: UndoItem::MoveBackQuarantined {
                current: current.to_path_buf(),
                original: original.to_path_buf(),
                manifest: manifest.to_vec(),
                manifest_complete,
            },
            message: "撤销移动暂时无法恢复原位置，可重试".to_owned(),
            affected_directories,
        });
    }
    let staging = unique_sibling_preserving_name(original, ".asterfiles-undo-restore");
    let mut no_conflict = |_: ConflictCategory, _: &Path, _: &Path| ConflictAction::Skip;
    let mut discovered = |_: u64, _: &Path| {};
    let mut progress = |_: u64, _: bool, _: &Path| {};
    let copy = copy_path_with_progress(
        current,
        &staging,
        cancel,
        &mut no_conflict,
        &mut discovered,
        &mut progress,
        &mut |_| {},
    );
    let copy_report = match copy {
        Ok(report) => report,
        Err(error) => {
            return Ok(UndoExecution::RetryWith {
                item: UndoItem::MoveBackQuarantined {
                    current: current.to_path_buf(),
                    original: original.to_path_buf(),
                    manifest: manifest.to_vec(),
                    manifest_complete,
                },
                message: format!(
                    "撤销移动尚未完成；临时副本保留在 {}，可重试：{error:?}",
                    staging.display()
                ),
                affected_directories,
            });
        }
    };
    let Some(staging_manifest) = complete_manifest_from_report(&staging, &copy_report) else {
        return Ok(UndoExecution::RetryWith {
            item: UndoItem::MoveBackQuarantined {
                current: current.to_path_buf(),
                original: original.to_path_buf(),
                manifest: manifest.to_vec(),
                manifest_complete,
            },
            message: format!(
                "临时副本无法确认，已保留在 {}；撤销可重试",
                staging.display()
            ),
            affected_directories,
        });
    };
    if ensure_manifest_strict(&staging, &staging_manifest).is_err() {
        return Ok(UndoExecution::RetryWith {
            item: UndoItem::MoveBackQuarantined {
                current: current.to_path_buf(),
                original: original.to_path_buf(),
                manifest: manifest.to_vec(),
                manifest_complete,
            },
            message: format!(
                "临时副本发生变化，已保留在 {}；撤销可重试",
                staging.display()
            ),
            affected_directories,
        });
    }
    if path_exists(original) {
        return Ok(UndoExecution::RetryWith {
            item: UndoItem::MoveBackQuarantined {
                current: current.to_path_buf(),
                original: original.to_path_buf(),
                manifest: manifest.to_vec(),
                manifest_complete,
            },
            message: format!(
                "原位置已被占用；临时副本保留在 {}，清理冲突后可重试",
                staging.display()
            ),
            affected_directories,
        });
    }
    if let Err(error) = fs::rename(&staging, original) {
        return Ok(UndoExecution::RetryWith {
            item: UndoItem::MoveBackQuarantined {
                current: current.to_path_buf(),
                original: original.to_path_buf(),
                manifest: manifest.to_vec(),
                manifest_complete,
            },
            message: format!(
                "撤销移动无法恢复原位置；临时副本保留在 {}：{error}",
                staging.display()
            ),
            affected_directories,
        });
    }
    match remove_quarantined(current, manifest.to_vec(), affected_directories) {
        Ok(UndoExecution::Completed(mut report)) => {
            report.affect(original);
            Ok(UndoExecution::Completed(report))
        }
        other => other,
    }
}

fn complete_manifest_from_report(
    root: &Path,
    report: &FileOperationReport,
) -> Option<Vec<(PathBuf, FileIdentity)>> {
    let mut expected = report
        .undo_identities
        .iter()
        .filter(|(path, _)| path == root || path.starts_with(root))
        .cloned()
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| left.0.cmp(&right.0));
    expected.dedup_by(|left, right| left.0 == right.0);
    let current = collect_tree_identities(root, expected.len().saturating_add(1)).ok()?;
    if current.len() != expected.len()
        || current
            .iter()
            .zip(&expected)
            .any(|((path, actual), (expected_path, expected))| {
                path != expected_path || !same_stable_file(*expected, *actual)
            })
    {
        return None;
    }
    Some(current)
}
fn remove_quarantined(
    path: &Path,
    manifest: Vec<(PathBuf, FileIdentity)>,
    affected_directories: Vec<PathBuf>,
) -> Result<UndoExecution, OperationError> {
    let mut removal_order = manifest.clone();
    removal_order.sort_by(|left, right| {
        right
            .0
            .components()
            .count()
            .cmp(&left.0.components().count())
            .then_with(|| right.0.cmp(&left.0))
    });
    let mut report = FileOperationReport::new();
    for (entry, identity) in removal_order {
        if let Err(error) = ensure_identity(&entry, identity).and_then(|_| {
            let metadata =
                fs::symlink_metadata(&entry).map_err(|error| OperationError::io(&entry, error))?;
            if is_traversable_directory(&metadata) {
                fs::remove_dir(&entry).map_err(|error| OperationError::io(&entry, error))?;
                report.directories += 1;
            } else {
                fs::remove_file(&entry).map_err(|error| OperationError::io(&entry, error))?;
                report.files += 1;
            }
            Ok(())
        }) {
            let remaining = manifest
                .into_iter()
                .filter(|(entry, _)| path_exists(entry))
                .collect::<Vec<_>>();
            return Ok(UndoExecution::RetryWith {
                item: UndoItem::RemoveQuarantined {
                    path: path.to_path_buf(),
                    manifest: remaining,
                },
                message: format!("撤销已完成，但临时清理失败：{error:?}"),
                affected_directories,
            });
        }
    }
    report.affected_directories.extend(affected_directories);
    Ok(UndoExecution::Completed(report))
}
fn rebase_manifest(
    manifest: &[(PathBuf, FileIdentity)],
    old_root: &Path,
    new_root: &Path,
) -> Result<Vec<(PathBuf, FileIdentity)>, OperationError> {
    manifest
        .iter()
        .map(|(path, identity)| {
            let relative = path
                .strip_prefix(old_root)
                .map_err(|_| OperationError::Io {
                    path: path.clone(),
                    kind: io::ErrorKind::InvalidData,
                    message: "undo manifest is outside its root".to_owned(),
                })?;
            let rebased = if relative.as_os_str().is_empty() {
                new_root.to_path_buf()
            } else {
                new_root.join(relative)
            };
            Ok((rebased, *identity))
        })
        .collect()
}

fn refresh_manifest_identities(
    manifest: &[(PathBuf, FileIdentity)],
) -> Result<Vec<(PathBuf, FileIdentity)>, OperationError> {
    manifest
        .iter()
        .map(|(path, _)| file_identity(path).map(|identity| (path.clone(), identity)))
        .collect()
}
fn ensure_undo_manifest(
    root: &Path,
    expected: &[(PathBuf, FileIdentity)],
    complete: bool,
    strict: bool,
) -> Result<(), OperationError> {
    if complete {
        if strict {
            ensure_manifest_strict(root, expected)
        } else {
            ensure_manifest(root, expected)
        }
    } else if expected.len() == 1 && expected[0].0 == root {
        ensure_identity(root, expected[0].1)?;
        if strict && file_identity(root)?.change_time != expected[0].1.change_time {
            return Err(OperationError::Io {
                path: root.to_path_buf(),
                kind: io::ErrorKind::InvalidData,
                message: "item changed after the original operation".to_owned(),
            });
        }
        Ok(())
    } else {
        Err(OperationError::Io {
            path: root.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "undo manifest is incomplete".to_owned(),
        })
    }
}
fn ensure_manifest_strict(
    root: &Path,
    expected: &[(PathBuf, FileIdentity)],
) -> Result<(), OperationError> {
    ensure_manifest(root, expected)?;
    for (path, identity) in expected {
        let actual = file_identity(path)?;
        if actual.change_time != identity.change_time {
            return Err(OperationError::Io {
                path: path.clone(),
                kind: io::ErrorKind::InvalidData,
                message: "item changed after the original operation".to_owned(),
            });
        }
    }
    Ok(())
}

fn ensure_manifest(
    root: &Path,
    expected: &[(PathBuf, FileIdentity)],
) -> Result<(), OperationError> {
    let mut actual_paths = Vec::new();
    collect_tree_paths(root, expected.len().saturating_add(1), &mut actual_paths)?;
    if actual_paths.len() != expected.len()
        || actual_paths
            .iter()
            .zip(expected)
            .any(|(actual, (path, _))| actual != path)
    {
        return Err(OperationError::Io {
            path: root.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "item tree changed after the original operation".to_owned(),
        });
    }
    for (path, identity) in expected {
        ensure_identity(path, *identity)?;
    }
    Ok(())
}

pub fn collect_tree_identities(
    root: &Path,
    limit: usize,
) -> Result<Vec<(PathBuf, FileIdentity)>, OperationError> {
    let mut paths = Vec::new();
    collect_tree_paths(root, limit, &mut paths)?;
    paths
        .into_iter()
        .map(|path| file_identity(&path).map(|identity| (path, identity)))
        .collect()
}

fn collect_tree_paths(
    root: &Path,
    limit: usize,
    paths: &mut Vec<PathBuf>,
) -> Result<(), OperationError> {
    if paths.len() >= limit {
        return Err(OperationError::Io {
            path: root.to_path_buf(),
            kind: io::ErrorKind::OutOfMemory,
            message: "undo snapshot item limit exceeded".to_owned(),
        });
    }
    paths.push(root.to_path_buf());
    let metadata = fs::symlink_metadata(root).map_err(|error| OperationError::io(root, error))?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        let children = fs::read_dir(root).map_err(|error| OperationError::io(root, error))?;
        for child in children {
            if paths.len() >= limit {
                return Err(OperationError::Io {
                    path: root.to_path_buf(),
                    kind: io::ErrorKind::OutOfMemory,
                    message: "undo snapshot item limit exceeded".to_owned(),
                });
            }
            let child = child
                .map_err(|error| OperationError::io(root, error))?
                .path();
            collect_tree_paths(&child, limit, paths)?;
        }
    }
    Ok(())
}

fn same_stable_file(expected: FileIdentity, actual: FileIdentity) -> bool {
    expected.volume_serial == actual.volume_serial
        && expected.file_index == actual.file_index
        && expected.is_directory == actual.is_directory
}

fn ensure_identity(path: &Path, expected: FileIdentity) -> Result<(), OperationError> {
    let actual = file_identity(path)?;
    if same_stable_file(expected, actual)
        && (expected.is_directory
            || (expected.size_bytes == actual.size_bytes && expected.modified == actual.modified))
    {
        Ok(())
    } else {
        Err(OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "item changed after the original operation".to_owned(),
        })
    }
}

pub fn discover_paths_with_progress(
    paths: &[PathBuf],
    is_cancelled: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(usize, u64, &Path),
) -> bool {
    let mut pending = paths.iter().rev().cloned().collect::<Vec<_>>();
    let mut discovered_items = 0_usize;
    let mut discovered_bytes = 0_u64;
    while let Some(path) = pending.pop() {
        if is_cancelled() {
            return false;
        }
        let metadata = fs::symlink_metadata(&path).ok();
        let bytes = metadata.as_ref().map_or(0, discovered_size);
        discovered_items = discovered_items.saturating_add(1);
        discovered_bytes = discovered_bytes.saturating_add(bytes);
        progress(discovered_items, discovered_bytes, &path);

        let Some(metadata) = metadata else {
            continue;
        };
        if !is_traversable_directory(&metadata) {
            continue;
        }
        let Ok(children) = fs::read_dir(&path) else {
            continue;
        };
        let mut child_paths = Vec::new();
        for child in children {
            if is_cancelled() {
                return false;
            }
            if let Ok(child) = child {
                child_paths.push(child.path());
            }
        }
        pending.extend(child_paths.into_iter().rev());
    }
    true
}

fn is_traversable_directory(metadata: &fs::Metadata) -> bool {
    if !metadata.file_type().is_dir() {
        return false;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0
    }
    #[cfg(not(windows))]
    {
        true
    }
}

pub type FileProgressCallback<'a> = dyn FnMut(u64, bool, &Path) + 'a;
pub type FileDiscoveredCallback<'a> = dyn FnMut(u64, &Path) + 'a;
pub type DestinationCreatedCallback<'a> = dyn FnMut(&Path) + 'a;

pub fn copy_path_with_progress(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
) -> Result<FileOperationReport, OperationError> {
    let same_location = source == destination;
    let kept_destination = same_location.then(|| keep_both_path(destination));
    let destination = kept_destination.as_deref().unwrap_or(destination);
    reject_destination_inside_source(source, destination)?;
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| OperationError::io(source, error))?;
    let destination_existed = path_exists(destination);
    let copy_into_existing_directory = destination_existed
        && source_metadata.file_type().is_dir()
        && fs::symlink_metadata(destination).is_ok_and(|metadata| metadata.file_type().is_dir());
    let mut report = FileOperationReport::new();
    copy_entry(
        source,
        destination,
        cancel,
        resolve_conflict,
        discovered,
        progress,
        destination_created,
        &mut report,
    )?;
    let actual_root = if copy_into_existing_directory {
        None
    } else if report
        .completed_paths
        .iter()
        .any(|path| path == destination)
    {
        Some(destination.to_path_buf())
    } else {
        report
            .completed_paths
            .iter()
            .find(|path| path.parent() == destination.parent())
            .cloned()
    };
    report.undo_root = actual_root.clone();
    report.undo_root_created_exclusively =
        actual_root.is_some_and(|path| !destination_existed || path != destination);
    Ok(report)
}

pub fn move_path_with_progress(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
) -> Result<FileOperationReport, OperationError> {
    move_path_with_progress_inner(
        source,
        destination,
        cancel,
        resolve_conflict,
        discovered,
        progress,
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn move_path_with_progress_inner(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    discover_source: bool,
) -> Result<FileOperationReport, OperationError> {
    if source == destination {
        let mut report = FileOperationReport::new();
        report.affect(source);
        return Ok(report);
    }
    reject_destination_inside_source(source, destination)?;
    check_cancel(cancel)?;
    if !path_exists(destination) {
        match fs::rename(source, destination) {
            Ok(()) => {
                let mut report = FileOperationReport::new();
                report.affect(source);
                report.affect(destination);
                return Ok(report);
            }
            Err(error) if !is_cross_device(&error) => {
                return Err(OperationError::io(source, error));
            }
            Err(_) => {}
        }
    }
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| OperationError::io(source, error))?;
    if discover_source && !source_metadata.file_type().is_dir() {
        discovered(discovered_size(&source_metadata), source);
    }
    let destination_metadata = fs::symlink_metadata(destination).ok();
    if source_metadata.file_type().is_dir()
        && destination_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_dir())
    {
        let mut report = FileOperationReport::new();
        move_directory_merged(
            source,
            destination,
            cancel,
            resolve_conflict,
            discovered,
            progress,
            &mut report,
        )?;
        report.affect(source);
        report.affect(destination);
        return Ok(report);
    }
    let resolution =
        match resolve_destination(source, destination, &source_metadata, resolve_conflict) {
            Ok(resolution) => resolution,
            Err(OperationError::ConflictSkipped(_)) => {
                let mut report = FileOperationReport::new();
                report.skipped.push(source.to_path_buf());
                return Ok(report);
            }
            Err(error) => return Err(error),
        };
    if !resolution.replace_existing {
        match fs::rename(source, &resolution.path) {
            Ok(()) => {
                let mut report = FileOperationReport::new();
                report.affect(source);
                report.affect(&resolution.path);
                return Ok(report);
            }
            Err(error) if !is_cross_device(&error) => {
                return Err(OperationError::io(source, error));
            }
            Err(_) => {}
        }
    }
    let mut report = FileOperationReport::new();
    copy_resolved_entry(
        source,
        &resolution,
        cancel,
        resolve_conflict,
        discovered,
        progress,
        &mut |_| {},
        &mut report,
    )?;
    remove_source_after_committed_copy(source, &resolution.path, &mut report)?;
    Ok(report)
}

fn remove_source_after_committed_copy(
    source: &Path,
    destination: &Path,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    if let Err(error) = remove_entry(source, &CancellationToken::new(), report) {
        return Err(OperationError::DestinationCommittedSourceRetained {
            source: source.to_path_buf(),
            destination: destination.to_path_buf(),
            message: format!("目标已完成，源仍存在：{error:?}"),
        });
    }
    report.affect(source);
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupTaskStatus {
    Recorded,
    Moved,
    Cleaning,
    Cancelled,
    Failed,
}

impl CleanupTaskStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recorded => "recorded",
            Self::Moved => "moved",
            Self::Cleaning => "cleaning",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "recorded" => Some(Self::Recorded),
            "moved" => Some(Self::Moved),
            "cleaning" => Some(Self::Cleaning),
            "cancelled" => Some(Self::Cancelled),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupTaskItem {
    pub original_path: PathBuf,
    pub remaining_path: Option<PathBuf>,
    pub identity: Option<FileIdentity>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupTaskRecord {
    pub task_id: String,
    pub items: Vec<CleanupTaskItem>,
    pub status: CleanupTaskStatus,
    pub error: Option<String>,
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub record_path: PathBuf,
    generation: u64,
}

impl CleanupTaskRecord {
    #[cfg(test)]
    pub fn remaining_paths(&self) -> impl Iterator<Item = &Path> {
        self.items
            .iter()
            .filter_map(|item| item.remaining_path.as_deref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupProgress {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
    pub current_path: Option<PathBuf>,
}

pub fn discard_empty_cleanup_task(record: &CleanupTaskRecord) -> Result<(), OperationError> {
    if record
        .items
        .iter()
        .any(|item| item.remaining_path.is_some())
    {
        return Ok(());
    }
    let task_root = record
        .record_path
        .parent()
        .ok_or_else(|| OperationError::Io {
            path: record.record_path.clone(),
            kind: io::ErrorKind::InvalidData,
            message: "cleanup record has no task directory".to_owned(),
        })?;
    for entry in fs::read_dir(task_root).map_err(|error| OperationError::io(task_root, error))? {
        let path = entry
            .map_err(|error| OperationError::io(task_root, error))?
            .path();
        if path.extension() == Some(OsStr::new("afcleanup")) {
            fs::remove_file(&path).map_err(|error| OperationError::io(&path, error))?;
        }
    }
    if fs::read_dir(task_root)
        .map_err(|error| OperationError::io(task_root, error))?
        .next()
        .is_none()
    {
        fs::remove_dir(task_root).map_err(|error| OperationError::io(task_root, error))?;
    }
    Ok(())
}
pub fn create_cleanup_task(
    paths: &[PathBuf],
    record_root: &Path,
    cancel: &CancellationToken,
) -> Result<CleanupTaskRecord, OperationError> {
    fs::create_dir_all(record_root).map_err(|error| OperationError::io(record_root, error))?;
    let task_id = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let record_task_root = record_root.join(format!(".asterfiles-cleanup-{task_id}"));
    fs::create_dir(&record_task_root)
        .map_err(|error| OperationError::io(&record_task_root, error))?;
    let mut record = CleanupTaskRecord {
        task_id: task_id.clone(),
        items: paths
            .iter()
            .map(|path| CleanupTaskItem {
                original_path: path.clone(),
                remaining_path: None,
                identity: None,
                error: None,
            })
            .collect(),
        status: CleanupTaskStatus::Recorded,
        error: None,
        files: 0,
        directories: 0,
        bytes: 0,
        record_path: record_task_root.join("record-0.afcleanup"),
        generation: 0,
    };
    write_cleanup_record(&mut record)?;

    for index in 0..record.items.len() {
        if let Err(OperationError::Cancelled) = check_cancel(cancel) {
            record.status = CleanupTaskStatus::Cancelled;
            write_cleanup_record(&mut record)?;
            return Ok(record);
        }
        let original = record.items[index].original_path.clone();
        let metadata = match fs::symlink_metadata(&original) {
            Ok(metadata) => metadata,
            Err(error) => {
                record.items[index].error = Some(error.to_string());
                record.status = CleanupTaskStatus::Failed;
                write_cleanup_record(&mut record)?;
                continue;
            }
        };
        if !is_traversable_directory(&metadata) {
            record.items[index].error =
                Some("fast removal requires a local ordinary directory".to_owned());
            record.status = CleanupTaskStatus::Failed;
            write_cleanup_record(&mut record)?;
            continue;
        }
        let identity = match file_identity(&original) {
            Ok(identity) => identity,
            Err(error) => {
                record.items[index].error = Some(format!("{error:?}"));
                record.status = CleanupTaskStatus::Failed;
                write_cleanup_record(&mut record)?;
                continue;
            }
        };
        let Some(parent) = original.parent() else {
            record.items[index].error = Some("protected filesystem root".to_owned());
            record.status = CleanupTaskStatus::Failed;
            write_cleanup_record(&mut record)?;
            continue;
        };
        let cleanup_root = parent.join(".asterfiles-cleanup");
        let cleanup_root_created = !path_exists(&cleanup_root);
        if cleanup_root_created {
            if let Err(error) = fs::create_dir(&cleanup_root) {
                record.items[index].error = Some(error.to_string());
                record.status = CleanupTaskStatus::Failed;
                write_cleanup_record(&mut record)?;
                continue;
            }
        } else if fs::symlink_metadata(&cleanup_root)
            .map(|metadata| !is_traversable_directory(&metadata))
            .unwrap_or(true)
        {
            record.items[index].error =
                Some("cleanup root is not an ordinary directory".to_owned());
            record.status = CleanupTaskStatus::Failed;
            write_cleanup_record(&mut record)?;
            continue;
        }
        #[cfg(windows)]
        let _ = crate::platform::windows::mark_internal_cleanup_directory(&cleanup_root);
        let payload_root = cleanup_root.join(&task_id);
        if !path_exists(&payload_root) {
            if let Err(error) = fs::create_dir(&payload_root) {
                record.items[index].error = Some(error.to_string());
                record.status = CleanupTaskStatus::Failed;
                write_cleanup_record(&mut record)?;
                continue;
            }
        } else if fs::symlink_metadata(&payload_root)
            .map(|metadata| !is_traversable_directory(&metadata))
            .unwrap_or(true)
        {
            record.items[index].error =
                Some("cleanup payload root is not an ordinary directory".to_owned());
            record.status = CleanupTaskStatus::Failed;
            write_cleanup_record(&mut record)?;
            continue;
        }
        if identity.volume_serial != file_identity(&payload_root)?.volume_serial {
            record.items[index].error =
                Some("cleanup storage is not on the source volume".to_owned());
            record.status = CleanupTaskStatus::Failed;
            write_cleanup_record(&mut record)?;
            continue;
        }
        let pending = payload_root.join(format!("payload-{index}"));
        record.items[index].remaining_path = Some(pending.clone());
        record.items[index].identity = Some(identity);
        write_cleanup_record(&mut record)?;
        match fs::rename(&original, &pending) {
            Ok(()) => {
                if !same_stable_file(identity, file_identity(&pending)?) {
                    record.items[index].error =
                        Some("item identity changed during cleanup move".to_owned());
                    record.status = CleanupTaskStatus::Failed;
                } else if record.items.iter().all(|item| item.error.is_none()) {
                    record.status = CleanupTaskStatus::Moved;
                }
            }
            Err(error) => {
                record.items[index].remaining_path = None;
                record.items[index].error = Some(error.to_string());
                record.status = CleanupTaskStatus::Failed;
                if fs::read_dir(&payload_root).is_ok_and(|mut entries| entries.next().is_none()) {
                    let _ = fs::remove_dir(&payload_root);
                }
                if cleanup_root_created
                    && fs::read_dir(&cleanup_root).is_ok_and(|mut entries| entries.next().is_none())
                {
                    let _ = fs::remove_dir(&cleanup_root);
                }
            }
        }
        write_cleanup_record(&mut record)?;
    }
    Ok(record)
}
pub fn discover_cleanup_tasks(
    record_root: &Path,
) -> Result<Vec<CleanupTaskRecord>, OperationError> {
    if !path_exists(record_root) {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in
        fs::read_dir(record_root).map_err(|error| OperationError::io(record_root, error))?
    {
        let entry = entry.map_err(|error| OperationError::io(record_root, error))?;
        let task_root = entry.path();
        let Some(name) = task_root.file_name().and_then(OsStr::to_str) else {
            continue;
        };
        if !name.starts_with(".asterfiles-cleanup-")
            || !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
        {
            continue;
        }
        let mut candidates = fs::read_dir(&task_root)
            .map_err(|error| OperationError::io(&task_root, error))?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension() == Some(OsStr::new("afcleanup")))
            .collect::<Vec<_>>();
        candidates.sort();
        if let Some(record) = candidates
            .iter()
            .rev()
            .find_map(|path| read_cleanup_record(path).ok())
        {
            records.push(record);
        }
    }
    records.sort_by(|left, right| left.task_id.cmp(&right.task_id));
    Ok(records)
}

struct CleanupTaskClaim {
    file: Option<fs::File>,
    path: PathBuf,
}

impl CleanupTaskClaim {
    fn acquire(record_root: &Path, task_id: &str) -> Result<Self, OperationError> {
        let path = record_root.join(format!(".asterfiles-cleanup-{task_id}.claim"));
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.create(true).share_mode(0);
        }
        #[cfg(not(windows))]
        options.create_new(true);
        let file = options
            .open(&path)
            .map_err(|error| OperationError::io(&path, error))?;
        Ok(Self {
            file: Some(file),
            path,
        })
    }
}

impl Drop for CleanupTaskClaim {
    fn drop(&mut self) {
        drop(self.file.take());
        let _ = fs::remove_file(&self.path);
    }
}
pub fn retry_cleanup_task(
    record_root: &Path,
    task_id: &str,
    cancel: &CancellationToken,
    progress: &mut dyn FnMut(&CleanupProgress),
) -> Result<CleanupTaskRecord, OperationError> {
    let _claim = CleanupTaskClaim::acquire(record_root, task_id)?;
    let mut record = discover_cleanup_tasks(record_root)?
        .into_iter()
        .find(|record| record.task_id == task_id)
        .ok_or_else(|| OperationError::Io {
            path: record_root.to_path_buf(),
            kind: io::ErrorKind::NotFound,
            message: "cleanup task record was not found".to_owned(),
        })?;
    for (index, item) in record.items.iter_mut().enumerate() {
        let Some(current) = item.remaining_path.clone() else {
            continue;
        };
        if path_exists(&current) {
            continue;
        }
        let expected = item.original_path.parent().map(|parent| {
            parent
                .join(".asterfiles-cleanup")
                .join(&record.task_id)
                .join(format!("payload-{index}"))
        });
        if expected.as_deref() != Some(current.as_path()) {
            item.error = Some("cleanup path does not match its AsterFiles record".to_owned());
            continue;
        }
        if path_exists(&item.original_path) {
            let Some(expected_identity) = item.identity else {
                item.error = Some("cleanup source identity is missing".to_owned());
                continue;
            };
            let actual_identity = match file_identity(&item.original_path) {
                Ok(actual) => actual,
                Err(error) => {
                    item.error = Some(format!("{error:?}"));
                    continue;
                }
            };
            if !same_stable_file(expected_identity, actual_identity) {
                item.error = Some(
                    "cleanup source identity no longer matches its AsterFiles record".to_owned(),
                );
                continue;
            }
            let parent_ready = current.parent().is_none_or(|parent| {
                fs::create_dir_all(parent).is_ok()
                    && file_identity(parent).is_ok_and(|parent_identity| {
                        parent_identity.volume_serial == expected_identity.volume_serial
                    })
            });
            if !parent_ready {
                item.error = Some("cleanup storage is not on the source volume".to_owned());
                continue;
            }
            match fs::rename(&item.original_path, &current) {
                Ok(()) => {
                    if file_identity(&current)
                        .is_ok_and(|actual| same_stable_file(expected_identity, actual))
                    {
                        item.error = None;
                    } else {
                        item.error = Some("item identity changed during cleanup move".to_owned());
                    }
                }
                Err(error) => {
                    item.error = Some(error.to_string());
                }
            }
            continue;
        }
        item.remaining_path = None;
        item.error = None;
    }
    record.status = CleanupTaskStatus::Cleaning;
    record.error = None;
    write_cleanup_record(&mut record)?;
    for index in 0..record.items.len() {
        let Some(current) = record.items[index].remaining_path.clone() else {
            continue;
        };
        if let Err(OperationError::Cancelled) = check_cancel(cancel) {
            record.status = CleanupTaskStatus::Cancelled;
            write_cleanup_record(&mut record)?;
            return Ok(record);
        }
        let expected = record.items[index].original_path.parent().map(|parent| {
            parent
                .join(".asterfiles-cleanup")
                .join(&record.task_id)
                .join(format!("payload-{index}"))
        });
        let identity_matches = record.items[index]
            .identity
            .and_then(|expected| {
                file_identity(&current)
                    .ok()
                    .map(|actual| same_stable_file(expected, actual))
            })
            .unwrap_or(false);
        if expected.as_deref() != Some(current.as_path()) || !identity_matches {
            let message = "cleanup path or stable identity no longer matches its AsterFiles record";
            record.items[index].error = Some(message.to_owned());
            record.status = CleanupTaskStatus::Failed;
            write_cleanup_record(&mut record)?;
            progress(&cleanup_progress(&record, Some(current)));
            continue;
        }
        let mut report = FileOperationReport::new();
        let mut deleted_bytes = 0;
        let mut progress_reporter = CleanupProgressReporter::new(&record);
        match remove_cleanup_entry(
            &current,
            cancel,
            &mut report,
            &mut deleted_bytes,
            &mut |path, report, bytes| {
                if progress_reporter.should_emit(report, bytes) {
                    progress_reporter.commit(&mut record, report, bytes);
                    progress(&cleanup_progress(&record, Some(path.to_path_buf())));
                }
            },
        ) {
            Ok(()) => {
                progress_reporter.commit(&mut record, &report, deleted_bytes);
                record.items[index].remaining_path = None;
                record.items[index].error = None;
            }
            Err(OperationError::Cancelled) => {
                progress_reporter.commit(&mut record, &report, deleted_bytes);
                record.status = CleanupTaskStatus::Cancelled;
                write_cleanup_record(&mut record)?;
                progress(&cleanup_progress(&record, Some(current)));
                return Ok(record);
            }
            Err(error) => {
                progress_reporter.commit(&mut record, &report, deleted_bytes);
                record.items[index].error = Some(format!("{error:?}"));
                record.status = CleanupTaskStatus::Failed;
                write_cleanup_record(&mut record)?;
                progress(&cleanup_progress(&record, Some(current)));
                continue;
            }
        }
        write_cleanup_record(&mut record)?;
        progress(&cleanup_progress(&record, Some(current)));
    }
    if record
        .items
        .iter()
        .any(|item| item.remaining_path.is_some())
    {
        record.status = CleanupTaskStatus::Failed;
        write_cleanup_record(&mut record)?;
        return Ok(record);
    }
    let record_task_root = record
        .record_path
        .parent()
        .unwrap_or(record_root)
        .to_path_buf();
    let cleanup_roots = record
        .items
        .iter()
        .filter_map(|item| {
            item.original_path
                .parent()
                .map(|parent| parent.join(".asterfiles-cleanup"))
        })
        .collect::<std::collections::HashSet<_>>();
    for cleanup_root in cleanup_roots {
        let payload_root = cleanup_root.join(&record.task_id);
        if path_exists(&payload_root)
            && fs::read_dir(&payload_root)
                .map_err(|error| OperationError::io(&payload_root, error))?
                .next()
                .is_none()
        {
            fs::remove_dir(&payload_root)
                .map_err(|error| OperationError::io(&payload_root, error))?;
        }
    }
    for entry in fs::read_dir(&record_task_root)
        .map_err(|error| OperationError::io(&record_task_root, error))?
    {
        let path = entry
            .map_err(|error| OperationError::io(&record_task_root, error))?
            .path();
        if path.extension() == Some(OsStr::new("afcleanup")) {
            fs::remove_file(&path).map_err(|error| OperationError::io(&path, error))?;
        }
    }
    if fs::read_dir(&record_task_root)
        .map_err(|error| OperationError::io(&record_task_root, error))?
        .next()
        .is_none()
    {
        fs::remove_dir(&record_task_root)
            .map_err(|error| OperationError::io(&record_task_root, error))?;
    }
    Ok(record)
}

struct CleanupProgressReporter {
    base_files: u64,
    base_directories: u64,
    base_bytes: u64,
    reported_files: u64,
    reported_directories: u64,
    reported_bytes: u64,
    last_sent_at: Instant,
}

impl CleanupProgressReporter {
    const INTERVAL: Duration = Duration::from_millis(125);
    const ITEM_BATCH: u64 = 128;
    const BYTE_BATCH: u64 = 4 * 1024 * 1024;

    fn new(record: &CleanupTaskRecord) -> Self {
        Self {
            base_files: record.files,
            base_directories: record.directories,
            base_bytes: record.bytes,
            reported_files: 0,
            reported_directories: 0,
            reported_bytes: 0,
            last_sent_at: Instant::now(),
        }
    }

    fn should_emit(&self, report: &FileOperationReport, bytes: u64) -> bool {
        let item_delta = report
            .files
            .saturating_add(report.directories)
            .saturating_sub(
                self.reported_files
                    .saturating_add(self.reported_directories) as usize,
            );
        item_delta >= Self::ITEM_BATCH as usize
            || bytes.saturating_sub(self.reported_bytes) >= Self::BYTE_BATCH
            || self.last_sent_at.elapsed() >= Self::INTERVAL
    }

    fn commit(&mut self, record: &mut CleanupTaskRecord, report: &FileOperationReport, bytes: u64) {
        record.files = self.base_files.saturating_add(report.files as u64);
        record.directories = self
            .base_directories
            .saturating_add(report.directories as u64);
        record.bytes = self.base_bytes.saturating_add(bytes);
        self.reported_files = report.files as u64;
        self.reported_directories = report.directories as u64;
        self.reported_bytes = bytes;
        self.last_sent_at = Instant::now();
    }
}
fn remove_cleanup_entry(
    path: &Path,
    cancel: &CancellationToken,
    report: &mut FileOperationReport,
    bytes: &mut u64,
    progress: &mut dyn FnMut(&Path, &FileOperationReport, u64),
) -> Result<(), OperationError> {
    check_cancel(cancel)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| OperationError::io(path, error))?;
    if !is_traversable_directory(&metadata) {
        if !metadata.file_type().is_dir() {
            *bytes = bytes.saturating_add(metadata.len());
        }
        fs::remove_file(path).map_err(|error| OperationError::io(path, error))?;
        report.files += 1;
        progress(path, report, *bytes);
        return Ok(());
    }
    for entry in fs::read_dir(path).map_err(|error| OperationError::io(path, error))? {
        let child = entry
            .map_err(|error| OperationError::io(path, error))?
            .path();
        remove_cleanup_entry(&child, cancel, report, bytes, progress)?;
    }
    fs::remove_dir(path).map_err(|error| OperationError::io(path, error))?;
    report.directories += 1;
    progress(path, report, *bytes);
    Ok(())
}

fn cleanup_progress(record: &CleanupTaskRecord, current_path: Option<PathBuf>) -> CleanupProgress {
    CleanupProgress {
        files: record.files,
        directories: record.directories,
        bytes: record.bytes,
        current_path,
    }
}

fn write_cleanup_record(record: &mut CleanupTaskRecord) -> Result<(), OperationError> {
    let next_generation = record.generation.saturating_add(1);
    let path = record
        .record_path
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .join(format!("record-{next_generation:020}.afcleanup"));
    let mut text = String::from("ASTERFILES_CLEANUP_V2\n");
    text.push_str(&format!(
        "task_id={}\nstatus={}\nfiles={}\ndirectories={}\nbytes={}\ngeneration={}\n",
        record.task_id,
        record.status.as_str(),
        record.files,
        record.directories,
        record.bytes,
        next_generation
    ));
    if let Some(error) = &record.error {
        text.push_str(&format!("error={}\n", hex_bytes(error.as_bytes())));
    }
    for item in &record.items {
        let remaining = item
            .remaining_path
            .as_ref()
            .map(|path| encode_path(path))
            .unwrap_or_default();
        let identity = item.identity.map(encode_identity).unwrap_or_default();
        let error = item
            .error
            .as_ref()
            .map(|error| hex_bytes(error.as_bytes()))
            .unwrap_or_default();
        text.push_str(&format!(
            "item={}|{}|{}|{}\n",
            encode_path(&item.original_path),
            remaining,
            identity,
            error
        ));
    }
    let temporary = path.with_extension("afcleanup.tmp");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| OperationError::io(&temporary, error))?;
    use std::io::Write;
    file.write_all(text.as_bytes())
        .map_err(|error| OperationError::io(&temporary, error))?;
    file.sync_all()
        .map_err(|error| OperationError::io(&temporary, error))?;
    drop(file);
    fs::rename(&temporary, &path).map_err(|error| OperationError::io(&path, error))?;
    record.record_path = path;
    record.generation = next_generation;
    Ok(())
}

fn read_cleanup_record(path: &Path) -> Result<CleanupTaskRecord, OperationError> {
    let text = fs::read_to_string(path).map_err(|error| OperationError::io(path, error))?;
    let mut lines = text.lines();
    if lines.next() != Some("ASTERFILES_CLEANUP_V2") {
        return Err(OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "not an AsterFiles cleanup record".to_owned(),
        });
    }
    let mut task_id = None;
    let mut status = None;
    let mut files = 0;
    let mut directories = 0;
    let mut bytes = 0;
    let mut generation = 0;
    let mut error = None;
    let mut items = Vec::new();
    for line in lines {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "task_id" => task_id = Some(value.to_owned()),
            "status" => status = CleanupTaskStatus::parse(value),
            "files" => files = value.parse().unwrap_or(0),
            "directories" => directories = value.parse().unwrap_or(0),
            "bytes" => bytes = value.parse().unwrap_or(0),
            "generation" => generation = value.parse().unwrap_or(0),
            "error" => {
                error = decode_hex_bytes(value).and_then(|bytes| String::from_utf8(bytes).ok())
            }
            "item" => {
                let mut fields = value.splitn(4, '|');
                let original_path =
                    fields
                        .next()
                        .and_then(decode_path)
                        .ok_or_else(|| OperationError::Io {
                            path: path.to_path_buf(),
                            kind: io::ErrorKind::InvalidData,
                            message: "invalid cleanup original path".to_owned(),
                        })?;
                let remaining_path = fields
                    .next()
                    .filter(|value| !value.is_empty())
                    .and_then(decode_path);
                let identity = fields
                    .next()
                    .filter(|value| !value.is_empty())
                    .and_then(decode_identity);
                let item_error = fields
                    .next()
                    .filter(|value| !value.is_empty())
                    .and_then(decode_hex_bytes)
                    .and_then(|bytes| String::from_utf8(bytes).ok());
                items.push(CleanupTaskItem {
                    original_path,
                    remaining_path,
                    identity,
                    error: item_error,
                });
            }
            _ => {}
        }
    }
    Ok(CleanupTaskRecord {
        task_id: task_id.ok_or_else(|| OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "cleanup record has no task id".to_owned(),
        })?,
        items,
        status: status.ok_or_else(|| OperationError::Io {
            path: path.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "cleanup record has no status".to_owned(),
        })?,
        error,
        files,
        directories,
        bytes,
        record_path: path.to_path_buf(),
        generation,
    })
}

fn encode_identity(identity: FileIdentity) -> String {
    format!(
        "{}:{}:{}",
        identity.volume_serial,
        hex_bytes(&identity.file_index),
        u8::from(identity.is_directory)
    )
}

fn decode_identity(value: &str) -> Option<FileIdentity> {
    let mut fields = value.split(':');
    let volume_serial = fields.next()?.parse().ok()?;
    let bytes = decode_hex_bytes(fields.next()?)?;
    let file_index: [u8; 16] = bytes.try_into().ok()?;
    let is_directory = fields.next()? == "1";
    Some(FileIdentity {
        volume_serial,
        file_index,
        is_directory,
        size_bytes: 0,
        modified: None,
        change_time: 0,
    })
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0xf) as usize] as char);
    }
    output
}

fn decode_hex_bytes(value: &str) -> Option<Vec<u8>> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            Some((((pair[0] as char).to_digit(16)? << 4) | (pair[1] as char).to_digit(16)?) as u8)
        })
        .collect()
}

#[cfg(windows)]
fn encode_path(path: &Path) -> String {
    use std::os::windows::ffi::OsStrExt;
    let mut bytes = Vec::new();
    for unit in path.as_os_str().encode_wide() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    hex_bytes(&bytes)
}
#[cfg(windows)]
fn decode_path(value: &str) -> Option<PathBuf> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = decode_hex_bytes(value)?;
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let wide = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect::<Vec<_>>();
    Some(PathBuf::from(OsString::from_wide(&wide)))
}
#[cfg(not(windows))]
fn encode_path(path: &Path) -> String {
    hex_bytes(path.to_string_lossy().as_bytes())
}
#[cfg(not(windows))]
fn decode_path(value: &str) -> Option<PathBuf> {
    Some(PathBuf::from(
        String::from_utf8(decode_hex_bytes(value)?).ok()?,
    ))
}
pub fn permanently_delete(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<FileOperationReport, OperationError> {
    let mut report = FileOperationReport::new();
    remove_entry(path, cancel, &mut report)?;
    report.affect(path);
    Ok(report)
}

pub fn fast_remove(
    path: &Path,
    cleanup_root: &Path,
    cancel: &CancellationToken,
) -> Result<FileOperationReport, OperationError> {
    check_cancel(cancel)?;
    fs::create_dir_all(cleanup_root).map_err(|error| OperationError::io(cleanup_root, error))?;
    let name = path.file_name().unwrap_or_else(|| OsStr::new("item"));
    let mut internal_name = OsString::from("asterfiles-cleanup-");
    internal_name.push(format!(
        "{}-",
        UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    internal_name.push(name);
    let pending = cleanup_root.join(internal_name);
    fs::rename(path, &pending).map_err(|error| OperationError::io(path, error))?;
    let mut report = FileOperationReport::new();
    report.affect(path);
    report.cleanup_pending = Some(pending);
    Ok(report)
}

#[allow(dead_code)]
pub fn clean_pending(
    path: &Path,
    cancel: &CancellationToken,
) -> Result<FileOperationReport, OperationError> {
    permanently_delete(path, cancel)
}

#[derive(Debug)]
struct DestinationResolution {
    path: PathBuf,
    replace_existing: bool,
}

#[allow(clippy::too_many_arguments)]
fn copy_entry(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    check_cancel(cancel)?;
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| OperationError::io(source, error))?;
    if !source_metadata.file_type().is_dir() {
        discovered(discovered_size(&source_metadata), source);
    }
    let resolution =
        match resolve_destination(source, destination, &source_metadata, resolve_conflict) {
            Ok(resolution) => resolution,
            Err(OperationError::ConflictSkipped(_)) => {
                report.skipped.push(source.to_path_buf());
                return Ok(());
            }
            Err(error) => return Err(error),
        };
    copy_resolved_entry(
        source,
        &resolution,
        cancel,
        resolve_conflict,
        discovered,
        progress,
        destination_created,
        report,
    )
}

#[allow(clippy::too_many_arguments)]
fn copy_resolved_entry(
    source: &Path,
    resolution: &DestinationResolution,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| OperationError::io(source, error))?;
    let file_type = source_metadata.file_type();
    if file_type.is_symlink() {
        let private_identity = copy_symlink_safely(
            source,
            &resolution.path,
            &source_metadata,
            resolution.replace_existing,
            cancel,
        )?;
        let published_identity = file_identity(&resolution.path)?;
        if !same_stable_file(private_identity, published_identity) {
            return Err(OperationError::Io {
                path: resolution.path.clone(),
                kind: io::ErrorKind::InvalidData,
                message: "copied link was replaced while being published".to_owned(),
            });
        }
        report
            .undo_identities
            .push((resolution.path.clone(), published_identity));
        report.files += 1;
        report.affect(&resolution.path);
    } else if file_type.is_dir() {
        if resolution.replace_existing
            && fs::symlink_metadata(&resolution.path)
                .is_ok_and(|metadata| !metadata.file_type().is_dir())
        {
            return replace_directory_safely(
                source,
                &resolution.path,
                cancel,
                resolve_conflict,
                discovered,
                progress,
                destination_created,
                report,
            );
        }
        copy_directory(
            source,
            &resolution.path,
            cancel,
            resolve_conflict,
            discovered,
            progress,
            destination_created,
            report,
        )?;
    } else {
        copy_file_safely(
            source,
            &resolution.path,
            resolution.replace_existing,
            cancel,
            progress,
            report,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn copy_directory(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    if !path_exists(destination) {
        fs::create_dir(destination).map_err(|error| OperationError::io(destination, error))?;
        let identity = file_identity(destination)?;
        report.directories += 1;
        report.affect(destination);
        report
            .undo_identities
            .push((destination.to_path_buf(), identity));
        destination_created(destination);
    }
    for entry in fs::read_dir(source).map_err(|error| OperationError::io(source, error))? {
        check_cancel(cancel)?;
        let entry = entry.map_err(|error| OperationError::io(source, error))?;
        copy_entry(
            &entry.path(),
            &destination.join(entry.file_name()),
            cancel,
            resolve_conflict,
            discovered,
            progress,
            destination_created,
            report,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn replace_directory_safely(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    let temporary = unique_sibling(destination, ".asterfiles-copy");
    let result = copy_directory(
        source,
        &temporary,
        cancel,
        resolve_conflict,
        discovered,
        progress,
        destination_created,
        report,
    );
    result?;
    replace_with_temporary(&temporary, destination)
}

fn move_directory_merged(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    discovered: &mut FileDiscoveredCallback<'_>,
    progress: &mut FileProgressCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    for entry in fs::read_dir(source).map_err(|error| OperationError::io(source, error))? {
        check_cancel(cancel)?;
        let entry = entry.map_err(|error| OperationError::io(source, error))?;
        let source_child = entry.path();
        let destination_child = destination.join(entry.file_name());
        let source_metadata = fs::symlink_metadata(&source_child)
            .map_err(|error| OperationError::io(&source_child, error))?;
        if !source_metadata.file_type().is_dir() {
            discovered(discovered_size(&source_metadata), &source_child);
        }
        if path_exists(&destination_child)
            && source_metadata.file_type().is_dir()
            && fs::symlink_metadata(&destination_child).is_ok_and(|item| item.file_type().is_dir())
        {
            move_directory_merged(
                &source_child,
                &destination_child,
                cancel,
                resolve_conflict,
                discovered,
                progress,
                report,
            )?;
            continue;
        }
        match move_path_with_progress_inner(
            &source_child,
            &destination_child,
            cancel,
            resolve_conflict,
            discovered,
            progress,
            false,
        ) {
            Ok(child_report) => merge_report(report, child_report),
            Err(OperationError::ConflictSkipped(_)) => report.skipped.push(source_child),
            Err(error) => return Err(error),
        }
    }
    if fs::read_dir(source)
        .map_err(|error| OperationError::io(source, error))?
        .next()
        .is_none()
    {
        fs::remove_dir(source).map_err(|error| OperationError::io(source, error))?;
        report.directories += 1;
    }
    Ok(())
}

fn discovered_size(metadata: &fs::Metadata) -> u64 {
    if metadata.file_type().is_file() {
        metadata.len()
    } else {
        0
    }
}

fn merge_report(report: &mut FileOperationReport, child: FileOperationReport) {
    report.files += child.files;
    report.directories += child.directories;
    report.bytes += child.bytes;
    report.skipped.extend(child.skipped);
    for path in child.completed_paths {
        if !report.completed_paths.contains(&path) {
            report.completed_paths.push(path);
        }
    }
    if report.undo_root.is_none() {
        report.undo_root = child.undo_root;
    }
    report.undo_root_created_exclusively |= child.undo_root_created_exclusively;
    report.undo_identities.extend(child.undo_identities);
    for directory in child.affected_directories {
        if !report.affected_directories.contains(&directory) {
            report.affected_directories.push(directory);
        }
    }
}

fn copy_file_safely(
    source: &Path,
    destination: &Path,
    replace_existing: bool,
    cancel: &CancellationToken,
    progress: &mut FileProgressCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    let temporary = unique_sibling(destination, ".asterfiles-copy");
    let result = copy_file_to_new_path(source, &temporary, cancel, progress, report);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let private_identity = match file_identity(&temporary) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if let Err(error) = if replace_existing {
        replace_with_temporary(&temporary, destination)
    } else {
        fs::rename(&temporary, destination).map_err(|error| OperationError::io(destination, error))
    } {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let published_identity = file_identity(destination)?;
    if !same_stable_file(private_identity, published_identity) {
        return Err(OperationError::Io {
            path: destination.to_path_buf(),
            kind: io::ErrorKind::InvalidData,
            message: "copied item was replaced while being published".to_owned(),
        });
    }
    report
        .undo_identities
        .push((destination.to_path_buf(), published_identity));
    report.files += 1;
    report.affect(destination);
    Ok(())
}

fn copy_file_to_new_path(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    progress: &mut FileProgressCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    let copied =
        crate::platform::windows::copy_file::copy_file(source, destination, cancel, &mut |bytes| {
            progress(bytes, false, source)
        })
        .map_err(|error| match error.kind {
            crate::platform::windows::copy_file::CopyFileErrorKind::Cancelled => {
                OperationError::Cancelled
            }
            crate::platform::windows::copy_file::CopyFileErrorKind::Failed => {
                OperationError::io(destination, error.error)
            }
        })?;
    report.bytes += copied;
    progress(0, true, source);
    Ok(())
}

fn copy_symlink_safely(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
    replace_existing: bool,
    cancel: &CancellationToken,
) -> Result<FileIdentity, OperationError> {
    check_cancel(cancel)?;
    let temporary = unique_sibling(destination, ".asterfiles-copy");
    if let Err(error) = copy_symlink(source, &temporary, metadata) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let identity = match file_identity(&temporary) {
        Ok(identity) => identity,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };
    let result = if replace_existing {
        replace_with_temporary(&temporary, destination)
    } else {
        fs::rename(&temporary, destination).map_err(|error| OperationError::io(destination, error))
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(identity)
}

fn replace_with_temporary(temporary: &Path, destination: &Path) -> Result<(), OperationError> {
    let backup = unique_sibling(destination, ".asterfiles-backup");
    fs::rename(destination, &backup).map_err(|error| OperationError::io(destination, error))?;
    if let Err(error) = fs::rename(temporary, destination) {
        let _ = fs::rename(&backup, destination);
        return Err(OperationError::io(destination, error));
    }
    let mut discarded = FileOperationReport::new();
    remove_entry(&backup, &CancellationToken::new(), &mut discarded)
}

fn resolve_destination(
    source: &Path,
    destination: &Path,
    source_metadata: &fs::Metadata,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
) -> Result<DestinationResolution, OperationError> {
    let Ok(destination_metadata) = fs::symlink_metadata(destination) else {
        return Ok(DestinationResolution {
            path: destination.to_path_buf(),
            replace_existing: false,
        });
    };
    let category = if source_metadata.file_type().is_dir()
        == destination_metadata.file_type().is_dir()
        && source_metadata.file_type().is_file() == destination_metadata.file_type().is_file()
    {
        if source_metadata.file_type().is_dir() {
            ConflictCategory::ExistingDirectory
        } else {
            ConflictCategory::ExistingFile
        }
    } else {
        ConflictCategory::TypeMismatch
    };
    if category == ConflictCategory::ExistingDirectory {
        return Ok(DestinationResolution {
            path: destination.to_path_buf(),
            replace_existing: false,
        });
    }
    match resolve_conflict(category, source, destination) {
        ConflictAction::Skip => Err(OperationError::ConflictSkipped(destination.to_path_buf())),
        ConflictAction::KeepBoth => Ok(DestinationResolution {
            path: keep_both_path(destination),
            replace_existing: false,
        }),
        ConflictAction::Replace => Ok(DestinationResolution {
            path: destination.to_path_buf(),
            replace_existing: true,
        }),
    }
}
fn remove_entry(
    path: &Path,
    cancel: &CancellationToken,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    check_cancel(cancel)?;
    let metadata = fs::symlink_metadata(path).map_err(|error| OperationError::io(path, error))?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() || !file_type.is_dir() {
        fs::remove_file(path).map_err(|error| OperationError::io(path, error))?;
        report.files += 1;
        return Ok(());
    }
    for entry in fs::read_dir(path).map_err(|error| OperationError::io(path, error))? {
        check_cancel(cancel)?;
        let entry = entry.map_err(|error| OperationError::io(path, error))?;
        remove_entry(&entry.path(), cancel, report)?;
    }
    fs::remove_dir(path).map_err(|error| OperationError::io(path, error))?;
    report.directories += 1;
    Ok(())
}

fn reject_destination_inside_source(
    source: &Path,
    destination: &Path,
) -> Result<(), OperationError> {
    let normalized_source = lexical_absolute(source)?;
    let normalized_destination = lexical_absolute(destination)?;
    if normalized_destination.starts_with(&normalized_source) {
        return Err(OperationError::SourceInsideDestination);
    }
    Ok(())
}

fn lexical_absolute(path: &Path) -> Result<PathBuf, OperationError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| OperationError::io(path, error))?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}

fn split_name(name: &OsStr) -> (OsString, Option<OsString>) {
    let path = Path::new(name);
    let stem = path.file_stem().unwrap_or(name).to_os_string();
    let extension = path.extension().map(OsStr::to_os_string);
    (stem, extension)
}

fn path_exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

fn unique_sibling_preserving_name(path: &Path, marker: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let name = path.file_name().unwrap_or_else(|| OsStr::new("item"));
    loop {
        let mut candidate_name = OsString::from(format!(
            "{marker}-{}-",
            UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        candidate_name.push(name);
        let candidate = parent.join(candidate_name);
        if !path_exists(&candidate) {
            return candidate;
        }
    }
}
fn unique_sibling(path: &Path, marker: &str) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    loop {
        let candidate = parent.join(format!(
            "{marker}-{}",
            UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if !path_exists(&candidate) {
            return candidate;
        }
    }
}

fn same_path_ignoring_ascii_case(left: &Path, right: &Path) -> bool {
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}
fn check_cancel(cancel: &CancellationToken) -> Result<(), OperationError> {
    cancel.wait_if_paused();
    if cancel.is_cancelled() {
        Err(OperationError::Cancelled)
    } else {
        Ok(())
    }
}
fn is_cross_device(error: &io::Error) -> bool {
    error
        .raw_os_error()
        .is_some_and(|code| code == 17 || code == 18)
}

#[cfg(windows)]
fn copy_symlink(
    source: &Path,
    destination: &Path,
    metadata: &fs::Metadata,
) -> Result<(), OperationError> {
    let target = fs::read_link(source).map_err(|error| OperationError::io(source, error))?;
    if metadata.is_dir() {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    }
    .map_err(|error| OperationError::io(destination, error))
}

#[cfg(unix)]
fn copy_symlink(
    source: &Path,
    destination: &Path,
    _metadata: &fs::Metadata,
) -> Result<(), OperationError> {
    let target = fs::read_link(source).map_err(|error| OperationError::io(source, error))?;
    std::os::unix::fs::symlink(target, destination)
        .map_err(|error| OperationError::io(destination, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn issue_82_cleanup_task_moves_immediately_and_is_discoverable() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("content.txt"), b"content").unwrap();
        let original_identity = file_identity(&source).unwrap();

        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(!source.exists());
        assert_eq!(record.status, CleanupTaskStatus::Moved);
        assert_eq!(record.remaining_paths().count(), 1);
        assert!(same_stable_file(
            original_identity,
            file_identity(record.remaining_paths().next().unwrap()).unwrap()
        ));
        let discovered = discover_cleanup_tasks(&records).unwrap();
        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].task_id, record.task_id);
        assert_eq!(
            discovered[0].remaining_paths().next(),
            record.remaining_paths().next()
        );
    }

    #[test]
    fn issue_82_cleanup_task_moves_multiple_siblings_into_one_payload_root() {
        let temp = TempDir::new();
        let records = temp.path().join("records");
        let sources = [temp.path().join("first"), temp.path().join("second")];
        for source in &sources {
            fs::create_dir(source).unwrap();
            fs::write(source.join("content.txt"), b"content").unwrap();
        }

        let record = create_cleanup_task(&sources, &records, &CancellationToken::new()).unwrap();

        assert!(sources.iter().all(|source| !source.exists()));
        assert_eq!(record.remaining_paths().count(), 2);
        assert!(record.remaining_paths().all(Path::exists));
    }
    #[test]
    fn issue_82_cleanup_reports_progress_inside_one_large_directory() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        for index in 0..300 {
            fs::write(source.join(format!("item-{index}.txt")), b"progress").unwrap();
        }
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let mut snapshots = Vec::new();

        retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |snapshot| snapshots.push(snapshot.clone()),
        )
        .unwrap();

        assert!(snapshots.len() >= 3);
        assert!(
            snapshots
                .windows(2)
                .all(|pair| pair[0].files <= pair[1].files)
        );
        assert!(snapshots.iter().any(|snapshot| snapshot.files >= 128));
        assert!(
            snapshots
                .last()
                .is_some_and(|snapshot| snapshot.files == 300)
        );
    }
    #[test]
    fn issue_82_cancelled_cleanup_retains_record_and_retry_finishes() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("one.txt"), b"one").unwrap();
        fs::write(source.join("two.txt"), b"two").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let cancelled =
            retry_cleanup_task(&records, &record.task_id, &cancel, &mut |_| {}).unwrap();
        assert_eq!(cancelled.status, CleanupTaskStatus::Cancelled);
        assert!(cancelled.record_path.exists());
        assert_eq!(cancelled.remaining_paths().count(), 1);

        let mut updates = Vec::new();
        let completed = retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |progress| updates.push(progress.clone()),
        )
        .unwrap();
        assert_eq!(completed.remaining_paths().count(), 0);
        assert!(!completed.record_path.exists());
        assert!(records.exists());
        assert!(!updates.is_empty());
        assert!(completed.files >= 2);
    }

    #[test]
    fn issue_82_cleanup_keeps_shared_root_after_task_finishes() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("content.txt"), b"content").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let cleanup_root = temp.path().join(".asterfiles-cleanup");

        retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();

        assert!(cleanup_root.is_dir());
        assert!(!cleanup_root.join(&record.task_id).exists());
    }

    #[test]
    fn issue_82_retry_completes_prepared_move_before_cleanup() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("content.txt"), b"content").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let pending = record.remaining_paths().next().unwrap().to_path_buf();
        fs::rename(&pending, &source).unwrap();

        let completed = retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();

        assert!(!source.exists());
        assert!(!pending.exists());
        assert_eq!(completed.remaining_paths().count(), 0);
    }

    #[test]
    fn issue_82_retry_rejects_replaced_original_identity() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("old.txt"), b"old").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let pending = record.remaining_paths().next().unwrap().to_path_buf();
        fs::remove_dir_all(&pending).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("replacement.txt"), b"replacement").unwrap();

        let failed = retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();

        assert!(source.join("replacement.txt").exists());
        assert_eq!(failed.status, CleanupTaskStatus::Failed);
        assert!(failed.record_path.exists());
        assert_eq!(failed.remaining_paths().count(), 1);
        assert!(
            failed.items[0]
                .error
                .as_deref()
                .is_some_and(|message| message.contains("identity"))
        );
    }

    #[test]
    fn issue_82_cleanup_claim_is_exclusive() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let claim = CleanupTaskClaim::acquire(&records, &record.task_id).unwrap();

        assert!(CleanupTaskClaim::acquire(&records, &record.task_id).is_err());
        drop(claim);
        assert!(CleanupTaskClaim::acquire(&records, &record.task_id).is_ok());
    }
    #[test]
    fn issue_82_retry_reconciles_an_already_cleaned_payload() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("one.txt"), b"one").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let remaining = record.remaining_paths().next().unwrap().to_path_buf();
        fs::remove_dir_all(&remaining).unwrap();
        let completed = retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(completed.remaining_paths().count(), 0);
        assert!(!completed.record_path.exists());
    }
    #[test]
    fn issue_82_discovery_falls_back_from_a_corrupt_latest_record() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("content.txt"), b"content").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let task_root = record.record_path.parent().unwrap();
        fs::write(
            task_root.join("record-99999999999999999999.afcleanup"),
            b"broken",
        )
        .unwrap();

        let discovered = discover_cleanup_tasks(&records).unwrap();

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].task_id, record.task_id);
        assert_eq!(discovered[0].remaining_paths().count(), 1);
    }
    #[test]
    fn issue_82_discovery_ignores_unknown_files_and_preserves_unknown_directory() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let records = temp.path().join("records");
        fs::create_dir(&records).unwrap();
        fs::write(records.join("unknown.txt"), b"keep").unwrap();
        fs::create_dir(records.join("unknown-dir")).unwrap();
        fs::write(&source, b"content").unwrap();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(discover_cleanup_tasks(&records).unwrap().len(), 1);
        retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert!(records.join("unknown.txt").exists());
        assert!(records.join("unknown-dir").exists());
        assert!(records.exists());
    }

    #[test]
    #[ignore = "explicit Issue #82 performance evidence"]
    fn issue_82_fast_delete_performance_evidence() {
        let artifact_dir = PathBuf::from("artifacts/perf/file-operations");
        fs::create_dir_all(&artifact_dir).unwrap();
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&source).unwrap();
        let file_count = 2_000_u64;
        for index in 0..file_count {
            fs::write(source.join(format!("{index:04}.tmp")), b"x").unwrap();
        }
        let started = std::time::Instant::now();
        let cpu_started = std::time::Instant::now();
        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        let source_disappeared_ms = started.elapsed().as_millis();
        let cleanup_started_ms = started.elapsed().as_millis();
        let completed = retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        let space_released_ms = started.elapsed().as_millis();
        fs::write(
            artifact_dir.join("issue-82-fast-delete.json"),
            format!(
                concat!(
                    "{{\n",
                    "  \"schema_version\": 1,\n",
                    "  \"issue\": 82,\n",
                    "  \"fixture_items\": {},\n",
                    "  \"fixture_bytes\": {},\n",
                    "  \"source_disappeared_ms\": {},\n",
                    "  \"cleanup_started_ms\": {},\n",
                    "  \"space_released_ms\": {},\n",
                    "  \"cpu_millis\": {},\n",
                    "  \"cancel_latency_ms\": null,\n",
                    "  \"remaining_items\": {}\n",
                    "}}\n"
                ),
                file_count + 1,
                file_count,
                source_disappeared_ms,
                cleanup_started_ms,
                space_released_ms,
                cpu_started.elapsed().as_millis(),
                completed.remaining_paths().count(),
            ),
        )
        .unwrap();
    }
    #[cfg(windows)]
    #[test]
    fn issue_82_cleanup_does_not_follow_directory_symlink() {
        use std::os::windows::fs::symlink_dir;
        let temp = TempDir::new();
        let outside = temp.path().join("outside");
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(outside.join("keep.bin"), [9_u8; 32]).unwrap();
        if let Err(error) = symlink_dir(&outside, source.join("link")) {
            eprintln!("directory symlink test skipped: {error}");
            return;
        }

        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(outside.join("keep.bin")).unwrap(), [9_u8; 32]);
    }
    #[cfg(unix)]
    #[test]
    fn issue_82_cleanup_does_not_follow_symlink() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new();
        let outside = temp.path().join("outside");
        let source = temp.path().join("source");
        let records = temp.path().join("records");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(outside.join("keep.bin"), [9_u8; 32]).unwrap();
        symlink(&outside, source.join("link")).unwrap();

        let record = create_cleanup_task(
            std::slice::from_ref(&source),
            &records,
            &CancellationToken::new(),
        )
        .unwrap();
        retry_cleanup_task(
            &records,
            &record.task_id,
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(outside.join("keep.bin")).unwrap(), [9_u8; 32]);
    }
    #[test]
    fn issue_81_recycle_discovery_counts_items_bytes_and_skips_reparse_targets() {
        let temp = TempDir::new();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("one.bin"), [1_u8, 2, 3]).unwrap();
        let nested = root.join("nested");
        fs::create_dir(&nested).unwrap();
        fs::write(nested.join("two.bin"), [4_u8, 5]).unwrap();

        let mut snapshots = Vec::new();
        assert!(discover_paths_with_progress(
            std::slice::from_ref(&root),
            &|| false,
            &mut |items, bytes, path| snapshots.push((items, bytes, path.to_path_buf())),
        ));
        let (items, bytes, _) = snapshots.last().unwrap();
        assert_eq!(*items, 4);
        assert_eq!(*bytes, 5);
        assert!(
            snapshots
                .windows(2)
                .all(|pair| { pair[0].0 < pair[1].0 && pair[0].1 <= pair[1].1 })
        );
    }

    #[test]
    fn issue_81_recycle_discovery_honors_cancellation() {
        let temp = TempDir::new();
        let root = temp.path().join("root");
        fs::create_dir(&root).unwrap();
        for index in 0..16 {
            fs::write(root.join(format!("{index}.txt")), b"x").unwrap();
        }
        let cancelled = Cell::new(false);
        let mut reports = 0_usize;
        assert!(!discover_paths_with_progress(
            std::slice::from_ref(&root),
            &|| cancelled.get(),
            &mut |_, _, _| {
                reports += 1;
                if reports == 3 {
                    cancelled.set(true);
                }
            },
        ));
        assert_eq!(reports, 3);
    }

    use crate::domain::file_operations::UndoHistory;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "asterfiles-file-operations-{}-{}-{}",
                std::process::id(),
                UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn write(path: &Path, content: &[u8]) {
        fs::write(path, content).unwrap();
    }
    fn replace(_: ConflictCategory, _: &Path, _: &Path) -> ConflictAction {
        ConflictAction::Replace
    }
    fn copy_path(
        source: &Path,
        destination: &Path,
        cancel: &CancellationToken,
        resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    ) -> Result<FileOperationReport, OperationError> {
        copy_path_with_progress(
            source,
            destination,
            cancel,
            resolve_conflict,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        )
    }
    fn move_path(
        source: &Path,
        destination: &Path,
        cancel: &CancellationToken,
        resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    ) -> Result<FileOperationReport, OperationError> {
        move_path_with_progress(
            source,
            destination,
            cancel,
            resolve_conflict,
            &mut |_, _| {},
            &mut |_, _, _| {},
        )
    }
    fn temporary_siblings(parent: &Path) -> Vec<PathBuf> {
        fs::read_dir(parent)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name().is_some_and(|name| {
                    let name = name.to_string_lossy();
                    name.starts_with(".asterfiles-copy") || name.starts_with(".asterfiles-backup")
                })
            })
            .collect()
    }

    #[test]
    fn validates_windows_names() {
        assert_eq!(
            validate_name(OsStr::new("CON.txt")),
            Err(NameValidationError::ReservedName)
        );
        assert_eq!(
            validate_name(OsStr::new("bad?.txt")),
            Err(NameValidationError::InvalidCharacter('?'))
        );
        assert_eq!(
            validate_name(OsStr::new("name.")),
            Err(NameValidationError::TrailingSpaceOrDot)
        );
        assert!(validate_name(OsStr::new("中文 📁.txt")).is_ok());
    }
    #[test]
    fn creates_folder_with_explorer_style_suffix() {
        let temp = TempDir::new();
        let created = create_folder(temp.path(), OsStr::new("New folder")).unwrap();
        assert!(created.is_dir());
        let second = create_folder(temp.path(), OsStr::new("New folder")).unwrap();
        assert_eq!(
            second.file_name().and_then(OsStr::to_str),
            Some("New folder (2)")
        );
    }
    #[test]
    fn generates_windows_style_keep_both_name() {
        let temp = TempDir::new();
        let original = temp.path().join("report.txt");
        write(&original, b"one");
        write(&temp.path().join("report (2).txt"), b"two");
        assert_eq!(
            keep_both_path(&original),
            temp.path().join("report (3).txt")
        );
    }
    #[test]
    fn renames_path() {
        let temp = TempDir::new();
        let source = temp.path().join("old.txt");
        write(&source, b"x");
        let renamed = rename_path(&source, OsStr::new("new.txt")).unwrap();
        assert_eq!(renamed, temp.path().join("new.txt"));
        assert!(!source.exists());
    }
    #[test]
    fn matching_directory_root_merges_without_a_root_conflict() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        write(&source.join("added.txt"), b"added");
        let mut conflicts = Vec::new();

        copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |category, source, destination| {
                conflicts.push((category, source.to_path_buf(), destination.to_path_buf()));
                ConflictAction::Replace
            },
        )
        .unwrap();

        assert!(conflicts.is_empty());
        assert_eq!(fs::read(destination.join("added.txt")).unwrap(), b"added");
    }

    #[test]
    fn matching_directory_root_only_reports_inner_file_conflicts() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        write(&source.join("same.txt"), b"new");
        write(&destination.join("same.txt"), b"old");
        let mut conflicts = Vec::new();

        copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |category, source, destination| {
                conflicts.push((category, source.to_path_buf(), destination.to_path_buf()));
                ConflictAction::Replace
            },
        )
        .unwrap();

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].0, ConflictCategory::ExistingFile);
        assert_eq!(conflicts[0].1, source.join("same.txt"));
        assert_eq!(conflicts[0].2, destination.join("same.txt"));
        assert_eq!(fs::read(destination.join("same.txt")).unwrap(), b"new");
    }
    #[test]
    fn nested_matching_directories_merge_without_directory_conflicts() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir_all(source.join("nested")).unwrap();
        fs::create_dir_all(destination.join("nested")).unwrap();
        write(&source.join("nested").join("added.txt"), b"added");
        let mut conflicts = Vec::new();

        move_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |category, source, destination| {
                conflicts.push((category, source.to_path_buf(), destination.to_path_buf()));
                ConflictAction::Replace
            },
        )
        .unwrap();

        assert!(conflicts.is_empty());
        assert_eq!(
            fs::read(destination.join("nested").join("added.txt")).unwrap(),
            b"added"
        );
        assert!(!source.exists());
    }
    #[test]
    fn copies_directory_by_merging_and_preserves_extra_destination_items() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        write(&source.join("same.txt"), b"new");
        write(&source.join("added.txt"), b"added");
        write(&destination.join("same.txt"), b"old");
        write(&destination.join("keep.txt"), b"keep");
        copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
        )
        .unwrap();
        assert_eq!(fs::read(destination.join("same.txt")).unwrap(), b"new");
        assert_eq!(fs::read(destination.join("keep.txt")).unwrap(), b"keep");
        assert_eq!(fs::read(destination.join("added.txt")).unwrap(), b"added");
    }
    #[test]
    fn copy_keep_both_preserves_both_files() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("target.txt");
        write(&source, b"source");
        write(&destination, b"target");
        copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |_, _, _| ConflictAction::KeepBoth,
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"target");
        assert_eq!(
            fs::read(temp.path().join("target (2).txt")).unwrap(),
            b"source"
        );
    }
    #[test]
    fn copy_pasted_into_same_folder_creates_numbered_file_copies() {
        let temp = TempDir::new();
        let source = temp.path().join("中文报告.txt");
        write(&source, b"source");
        for expected in ["中文报告 (2).txt", "中文报告 (3).txt"] {
            let report =
                copy_path(&source, &source, &CancellationToken::new(), &mut replace).unwrap();
            let copy = temp.path().join(expected);
            assert_eq!(fs::read(&copy).unwrap(), b"source");
            assert_eq!(report.completed_paths.last(), Some(&copy));
        }
    }

    #[test]
    fn copy_pasted_into_same_folder_creates_numbered_directory_copies() {
        let temp = TempDir::new();
        let source = temp.path().join("资料");
        fs::create_dir(&source).unwrap();
        write(&source.join("内容.txt"), b"source");
        copy_path(&source, &source, &CancellationToken::new(), &mut replace).unwrap();
        assert_eq!(
            fs::read(temp.path().join("资料 (2)").join("内容.txt")).unwrap(),
            b"source"
        );
    }

    #[test]
    fn move_pasted_into_same_folder_is_a_no_op() {
        let temp = TempDir::new();
        let source = temp.path().join("keep.txt");
        write(&source, b"source");
        move_path(&source, &source, &CancellationToken::new(), &mut replace).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"source");
    }
    #[test]
    fn replacing_file_preserves_old_destination_when_copy_is_cancelled() {
        let temp = TempDir::new();
        let source = temp.path().join("source.bin");
        let destination = temp.path().join("target.bin");
        write(&source, &vec![7_u8; COPY_TEST_CHUNK_SIZE * 2]);
        write(&destination, b"old target");
        let cancel = CancellationToken::new();
        let cancel_after_first_chunk = cancel.clone();
        let result = copy_path_with_progress(
            &source,
            &destination,
            &cancel,
            &mut replace,
            &mut |_, _| {},
            &mut move |_, _, _| cancel_after_first_chunk.cancel(),
            &mut |_| {},
        );
        assert_eq!(result, Err(OperationError::Cancelled));
        assert_eq!(fs::read(&destination).unwrap(), b"old target");
        assert!(source.exists());
        assert!(temporary_siblings(temp.path()).is_empty());
    }

    #[test]
    fn replacing_file_move_preserves_source_and_destination_when_cancelled() {
        let temp = TempDir::new();
        let source = temp.path().join("source.bin");
        let destination = temp.path().join("target.bin");
        write(&source, &vec![9_u8; COPY_TEST_CHUNK_SIZE * 2]);
        write(&destination, b"old target");
        let cancel = CancellationToken::new();
        let cancel_after_first_chunk = cancel.clone();
        let result = move_path_with_progress(
            &source,
            &destination,
            &cancel,
            &mut replace,
            &mut |_, _| {},
            &mut move |_, _, _| cancel_after_first_chunk.cancel(),
        );
        assert_eq!(result, Err(OperationError::Cancelled));
        assert_eq!(fs::read(&destination).unwrap(), b"old target");
        assert!(source.exists());
        assert!(temporary_siblings(temp.path()).is_empty());
    }
    #[test]
    fn replacing_file_reports_chunk_progress_and_commits_new_content() {
        let temp = TempDir::new();
        let source = temp.path().join("source.bin");
        let destination = temp.path().join("target.bin");
        let content = vec![3_u8; COPY_TEST_CHUNK_SIZE + 17];
        write(&source, &content);
        write(&destination, b"old target");
        let mut increments = Vec::new();
        let report = copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |bytes, completed, _| {
                if bytes > 0 && !completed {
                    increments.push(bytes);
                }
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), content);
        assert_eq!(report.bytes, (COPY_TEST_CHUNK_SIZE + 17) as u64);
        assert_eq!(increments, vec![COPY_TEST_CHUNK_SIZE as u64, 17]);
        assert!(temporary_siblings(temp.path()).is_empty());
    }

    #[test]
    fn issue_61_discovers_each_file_before_completion() {
        use std::cell::RefCell;

        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        write(&source.join("first.txt"), b"first");
        write(&source.join("second.txt"), b"second");
        let events = RefCell::new(Vec::new());

        copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, path| events.borrow_mut().push(("discovered", path.to_path_buf())),
            &mut |_, completed, path| {
                if completed {
                    events.borrow_mut().push(("completed", path.to_path_buf()));
                }
            },
            &mut |_| {},
        )
        .unwrap();

        let events = events.into_inner();
        assert!(events.chunks_exact(2).all(|pair| {
            pair[0].0 == "discovered" && pair[1].0 == "completed" && pair[0].1 == pair[1].1
        }));
        for path in [source.join("first.txt"), source.join("second.txt")] {
            let discovered = events
                .iter()
                .position(|event| event == &("discovered", path.clone()))
                .unwrap();
            let completed = events
                .iter()
                .position(|event| event == &("completed", path.clone()))
                .unwrap();
            assert!(discovered < completed);
        }
    }

    #[test]
    fn issue_61_discovery_counts_files_and_bytes_once() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(source.join("nested")).unwrap();
        write(&source.join("one.bin"), &[1; 3]);
        write(&source.join("nested").join("two.bin"), &[2; 5]);
        let mut discovered = Vec::new();

        copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |bytes, path| discovered.push((path.to_path_buf(), bytes)),
            &mut |_, _, _| {},
            &mut |_| {},
        )
        .unwrap();

        discovered.sort_by(|left, right| left.0.cmp(&right.0));
        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered.iter().map(|item| item.1).sum::<u64>(), 8);
        assert_eq!(
            discovered.iter().map(|item| &item.0).collect::<Vec<_>>(),
            vec![
                &source.join("nested").join("two.bin"),
                &source.join("one.bin")
            ]
        );
    }

    #[test]
    fn issue_61_cancelled_copy_stops_before_discovery() {
        let temp = TempDir::new();
        let source = temp.path().join("source.bin");
        let destination = temp.path().join("destination.bin");
        write(&source, b"content");
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut discovered = Vec::new();

        let result = copy_path_with_progress(
            &source,
            &destination,
            &cancel,
            &mut replace,
            &mut |bytes, path| discovered.push((bytes, path.to_path_buf())),
            &mut |_, _, _| {},
            &mut |_| {},
        );

        assert_eq!(result, Err(OperationError::Cancelled));
        assert!(discovered.is_empty());
        assert!(!destination.exists());
    }

    #[test]
    fn issue_61_paused_copy_waits_before_discovery() {
        use std::{sync::mpsc, thread, time::Duration};

        let temp = TempDir::new();
        let source = temp.path().join("source.bin");
        let destination = temp.path().join("destination.bin");
        write(&source, b"content");
        let cancel = CancellationToken::new();
        cancel.pause();
        let worker_cancel = cancel.clone();
        let (discovered_sender, discovered_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            copy_path_with_progress(
                &source,
                &destination,
                &worker_cancel,
                &mut replace,
                &mut |bytes, path| discovered_sender.send((bytes, path.to_path_buf())).unwrap(),
                &mut |_, _, _| {},
                &mut |_| {},
            )
        });

        let discovered_while_paused = discovered_receiver
            .recv_timeout(Duration::from_millis(50))
            .is_ok();
        cancel.resume();
        assert!(!discovered_while_paused);
        assert_eq!(
            discovered_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .0,
            7
        );
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn issue_83_empty_created_folder_can_be_undone() {
        let temp = TempDir::new();
        let folder = create_folder(temp.path(), OsStr::new("created")).unwrap();
        let item = UndoItem::RemoveCreated {
            path: folder.clone(),
            manifest: collect_tree_identities(&folder, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap(),
        };

        execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert!(!folder.exists());
    }

    #[test]
    fn issue_83_remove_created_isolated_retry_state_is_safe() {
        let temp = TempDir::new();
        let created = temp.path().join("created");
        fs::create_dir(&created).unwrap();
        write(&created.join("known.txt"), b"known");
        let manifest = collect_tree_identities(&created, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap();
        let quarantined = temp.path().join(".asterfiles-undo-created");
        fs::rename(&created, &quarantined).unwrap();
        let quarantined_manifest = refresh_manifest_identities(
            &rebase_manifest(&manifest, &created, &quarantined).unwrap(),
        )
        .unwrap();
        write(&created, b"external");
        let item = UndoItem::RemoveQuarantined {
            path: quarantined.clone(),
            manifest: quarantined_manifest,
        };

        let outcome = execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert!(matches!(outcome, UndoExecution::Completed(_)));
        assert_eq!(fs::read(created).unwrap(), b"external");
        assert!(!quarantined.exists());
    }
    #[test]
    fn issue_83_move_then_undo_restores_the_original_path() {
        let temp = TempDir::new();
        let original = temp.path().join("original.txt");
        let current = temp.path().join("moved.txt");
        write(&original, b"content");
        move_path_with_progress(
            &original,
            &current,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
        )
        .unwrap();
        let item = UndoItem::MoveBack {
            current: current.clone(),
            original: original.clone(),
            manifest: collect_tree_identities(&current, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap(),
            manifest_complete: true,
        };

        execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert_eq!(fs::read(original).unwrap(), b"content");
        assert!(!current.exists());
    }
    #[test]
    fn issue_83_non_empty_directory_rename_can_be_undone() {
        let temp = TempDir::new();
        let original = temp.path().join("folder");
        let current = temp.path().join("renamed");
        fs::create_dir(&current).unwrap();
        write(&current.join("child.txt"), b"content");
        let item = UndoItem::MoveBack {
            current: current.clone(),
            original: original.clone(),
            manifest: vec![(current.clone(), file_identity(&current).unwrap())],
            manifest_complete: false,
        };

        execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert_eq!(fs::read(original.join("child.txt")).unwrap(), b"content");
        assert!(!current.exists());
    }

    #[test]
    fn issue_83_copy_then_undo_removes_only_the_created_file() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("copy.txt");
        write(&source, b"content");
        let report = copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        )
        .unwrap();
        let item = UndoItem::RemoveCreated {
            path: destination.clone(),
            manifest: report.undo_identities,
        };

        execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"content");
        assert!(!destination.exists());
    }

    #[test]
    fn issue_83_non_empty_directory_copy_can_be_safely_undone() {
        let temp = TempDir::new();
        let source = temp.path().join("source-tree");
        let destination = temp.path().join("copied-tree");
        fs::create_dir(&source).unwrap();
        write(&source.join("child.txt"), b"content");
        let report = copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        )
        .unwrap();
        let actual =
            collect_tree_identities(&destination, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap();
        let actual_by_path = actual
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        let manifest = report
            .undo_identities
            .into_iter()
            .map(|(path, identity)| {
                let current = actual_by_path[&path];
                (
                    path,
                    if identity.is_directory {
                        current
                    } else {
                        identity
                    },
                )
            })
            .collect();
        let item = UndoItem::RemoveCreated {
            path: destination.clone(),
            manifest,
        };

        execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert!(!destination.exists());
        assert_eq!(fs::read(source.join("child.txt")).unwrap(), b"content");
    }
    #[test]
    fn issue_83_copy_report_marks_only_an_exclusive_root_as_undoable() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let exclusive = temp.path().join("exclusive");
        let existing = temp.path().join("existing");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&existing).unwrap();
        write(&source.join("child.txt"), b"content");

        let exclusive_report = copy_path_with_progress(
            &source,
            &exclusive,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(
            exclusive_report.undo_root.as_deref(),
            Some(exclusive.as_path())
        );
        assert!(exclusive_report.undo_root_created_exclusively);

        let merged_report = copy_path_with_progress(
            &source,
            &existing,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(merged_report.undo_root, None);
        assert!(!merged_report.undo_root_created_exclusively);
    }

    #[test]
    fn issue_83_replaced_file_is_never_marked_as_an_exclusive_copy() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        write(&source, b"new");
        write(&destination, b"old");
        let report = copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"new");
        assert!(!report.undo_root_created_exclusively);
    }
    #[test]
    fn issue_83_rename_then_undo_restores_the_original_name() {
        let temp = TempDir::new();
        let original = temp.path().join("original.txt");
        write(&original, b"content");
        let current = rename_path(&original, OsStr::new("renamed.txt")).unwrap();
        let item = UndoItem::MoveBack {
            current: current.clone(),
            original: original.clone(),
            manifest: collect_tree_identities(&current, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap(),
            manifest_complete: true,
        };

        execute_undo_item(&item, &CancellationToken::new()).unwrap();
        assert_eq!(fs::read(original).unwrap(), b"content");
        assert!(!current.exists());
    }
    #[test]
    fn issue_83_undo_copy_refuses_an_externally_modified_file() {
        let temp = TempDir::new();
        let copied = temp.path().join("copy.txt");
        write(&copied, b"original");
        let manifest = collect_tree_identities(&copied, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap();
        write(&copied, b"externally changed");

        let item = UndoItem::RemoveCreated {
            path: copied.clone(),
            manifest,
        };
        assert!(execute_undo_item(&item, &CancellationToken::new()).is_err());
        assert_eq!(fs::read(copied).unwrap(), b"externally changed");
    }

    #[test]
    fn issue_83_undo_copy_refuses_an_added_directory_child() {
        let temp = TempDir::new();
        let copied = temp.path().join("copy");
        fs::create_dir(&copied).unwrap();
        write(&copied.join("original.txt"), b"original");
        let manifest = collect_tree_identities(&copied, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap();
        write(&copied.join("external.txt"), b"external");

        let item = UndoItem::RemoveCreated {
            path: copied.clone(),
            manifest,
        };
        assert!(execute_undo_item(&item, &CancellationToken::new()).is_err());
        assert!(copied.join("original.txt").exists());
        assert!(copied.join("external.txt").exists());
    }

    #[test]
    fn issue_83_undo_move_refuses_an_occupied_original_path() {
        let temp = TempDir::new();
        let current = temp.path().join("renamed.txt");
        let original = temp.path().join("original.txt");
        write(&current, b"moved");
        write(&original, b"external");
        let item = UndoItem::MoveBack {
            current: current.clone(),
            original: original.clone(),
            manifest: collect_tree_identities(&current, UndoHistory::MAX_SNAPSHOT_ITEMS).unwrap(),
            manifest_complete: true,
        };

        assert!(execute_undo_item(&item, &CancellationToken::new()).is_err());
        assert_eq!(fs::read(current).unwrap(), b"moved");
        assert_eq!(fs::read(original).unwrap(), b"external");
    }

    #[test]
    fn issue_61_same_volume_move_rename_does_not_scan() {
        let temp = TempDir::new();
        let source = temp.path().join("source.bin");
        let destination = temp.path().join("destination.bin");
        write(&source, b"content");
        let mut discovered = Vec::new();

        move_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |bytes, path| discovered.push((bytes, path.to_path_buf())),
            &mut |_, _, _| {},
        )
        .unwrap();

        assert!(discovered.is_empty());
        assert!(!source.exists());
        assert_eq!(fs::read(destination).unwrap(), b"content");
    }

    #[test]
    fn directory_copy_reports_the_real_root_as_soon_as_it_is_created() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        write(&source.join("file.txt"), b"content");
        let mut created = Vec::new();

        copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |path| created.push(path.to_path_buf()),
        )
        .unwrap();

        assert_eq!(created.first(), Some(&destination));
        assert!(destination.join("file.txt").exists());
    }

    #[test]
    fn replacing_type_mismatch_does_not_predelete_destination() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("target");
        write(&source, b"replacement");
        fs::create_dir(&destination).unwrap();
        write(&destination.join("old.txt"), b"old");
        copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), b"replacement");
        assert!(source.exists());
        assert!(temporary_siblings(temp.path()).is_empty());
    }

    #[test]
    fn directory_replace_merges_without_removing_unrelated_items() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        write(&source.join("same.txt"), b"new");
        write(&destination.join("same.txt"), b"old");
        write(&destination.join("keep.txt"), b"keep");
        copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
        )
        .unwrap();
        assert_eq!(fs::read(destination.join("same.txt")).unwrap(), b"new");
        assert_eq!(fs::read(destination.join("keep.txt")).unwrap(), b"keep");
    }
    #[test]
    fn skipped_copy_returns_successful_report_without_changes() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("target.txt");
        write(&source, b"source");
        write(&destination, b"target");
        let report = copy_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |_, _, _| ConflictAction::Skip,
        )
        .unwrap();
        assert_eq!(report.skipped, vec![source]);
        assert_eq!(fs::read(destination).unwrap(), b"target");
    }

    #[test]
    fn skipped_move_conflict_leaves_only_skipped_source_item() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        write(&source.join("same.txt"), b"source");
        write(&source.join("moved.txt"), b"moved");
        write(&destination.join("same.txt"), b"target");
        let report = move_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut |category, _, _| {
                if category == ConflictCategory::ExistingFile {
                    ConflictAction::Skip
                } else {
                    ConflictAction::Replace
                }
            },
        )
        .unwrap();
        assert_eq!(report.skipped, vec![source.join("same.txt")]);
        assert!(source.join("same.txt").exists());
        assert!(!source.join("moved.txt").exists());
        assert_eq!(fs::read(destination.join("moved.txt")).unwrap(), b"moved");
    }
    #[test]
    fn rejects_copy_into_own_subtree() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let result = copy_path(
            &source,
            &source.join("child"),
            &CancellationToken::new(),
            &mut replace,
        );
        assert_eq!(result, Err(OperationError::SourceInsideDestination));
    }
    #[test]
    fn cancelled_copy_does_not_start() {
        let temp = TempDir::new();
        let source = temp.path().join("source.txt");
        let destination = temp.path().join("destination.txt");
        write(&source, b"content");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(
            copy_path(&source, &destination, &cancel, &mut replace),
            Err(OperationError::Cancelled)
        );
        assert!(!destination.exists());
    }

    #[test]
    fn move_merges_directories_then_removes_source() {
        let temp = TempDir::new();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        write(&source.join("moved.txt"), b"moved");
        write(&destination.join("kept.txt"), b"kept");
        move_path(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
        )
        .unwrap();
        assert!(!source.exists());
        assert_eq!(fs::read(destination.join("moved.txt")).unwrap(), b"moved");
        assert_eq!(fs::read(destination.join("kept.txt")).unwrap(), b"kept");
    }
    #[test]
    fn permanently_deletes_directory_tree() {
        let temp = TempDir::new();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        write(&target.join("file.txt"), b"x");
        permanently_delete(&target, &CancellationToken::new()).unwrap();
        assert!(!target.exists());
    }
    #[test]
    fn fast_remove_hides_path_before_cleanup() {
        let temp = TempDir::new();
        let target = temp.path().join("target");
        let cleanup = temp.path().join("cleanup");
        fs::create_dir(&target).unwrap();
        write(&target.join("file.txt"), b"x");
        let report = fast_remove(&target, &cleanup, &CancellationToken::new()).unwrap();
        assert!(!target.exists());
        let pending = report.cleanup_pending.unwrap();
        assert!(pending.exists());
        clean_pending(&pending, &CancellationToken::new()).unwrap();
        assert!(!pending.exists());
    }

    #[cfg(unix)]
    #[test]
    fn deletion_does_not_follow_directory_symlink() {
        use std::os::unix::fs::symlink;
        let temp = TempDir::new();
        let outside = temp.path().join("outside");
        let target = temp.path().join("target");
        fs::create_dir(&outside).unwrap();
        fs::create_dir(&target).unwrap();
        write(&outside.join("keep.txt"), b"keep");
        symlink(&outside, target.join("link")).unwrap();
        permanently_delete(&target, &CancellationToken::new()).unwrap();
        assert_eq!(fs::read(outside.join("keep.txt")).unwrap(), b"keep");
    }
}
