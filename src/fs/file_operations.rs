use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use crate::domain::file_operations::{CancellationToken, ConflictAction, ConflictCategory};

const COPY_BUFFER_SIZE: usize = 1024 * 1024;
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
    validate_name(name).map_err(OperationError::InvalidName)?;
    let requested = parent.join(name);
    let path = if path_exists(&requested) {
        keep_both_path(&requested)
    } else {
        requested
    };
    fs::create_dir(&path).map_err(|error| OperationError::io(&path, error))?;
    Ok(path)
}

pub fn rename_path(source: &Path, new_name: &OsStr) -> Result<PathBuf, OperationError> {
    validate_name(new_name).map_err(OperationError::InvalidName)?;
    let parent = source.parent().unwrap_or_else(|| Path::new(""));
    let destination = parent.join(new_name);
    if source == destination {
        return Ok(destination);
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
    Ok(destination)
}

pub type FileProgressCallback<'a> = dyn FnMut(u64, bool, &Path) + 'a;
pub type DestinationCreatedCallback<'a> = dyn FnMut(&Path) + 'a;

#[allow(dead_code)]
pub fn copy_path(
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
        &mut |_, _, _| {},
        &mut |_| {},
    )
}

pub fn copy_path_with_progress(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
) -> Result<FileOperationReport, OperationError> {
    let same_location = source == destination;
    let kept_destination = same_location.then(|| keep_both_path(destination));
    let destination = kept_destination.as_deref().unwrap_or(destination);
    reject_destination_inside_source(source, destination)?;
    let mut report = FileOperationReport::new();
    copy_entry(
        source,
        destination,
        cancel,
        resolve_conflict,
        progress,
        destination_created,
        &mut report,
    )?;
    Ok(report)
}

#[allow(dead_code)]
pub fn move_path(
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
        &mut |_, _, _| {},
    )
}

pub fn move_path_with_progress(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    progress: &mut FileProgressCallback<'_>,
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
    let destination_metadata = fs::symlink_metadata(destination).ok();
    if source_metadata.file_type().is_dir()
        && destination_metadata
            .as_ref()
            .is_some_and(|metadata| metadata.file_type().is_dir())
    {
        match resolve_conflict(ConflictCategory::ExistingDirectory, source, destination) {
            ConflictAction::Skip => {
                let mut report = FileOperationReport::new();
                report.skipped.push(source.to_path_buf());
                return Ok(report);
            }
            ConflictAction::KeepBoth => {
                return move_path_with_progress(
                    source,
                    &keep_both_path(destination),
                    cancel,
                    resolve_conflict,
                    progress,
                );
            }
            ConflictAction::Replace => {
                let mut report = FileOperationReport::new();
                move_directory_merged(
                    source,
                    destination,
                    cancel,
                    resolve_conflict,
                    progress,
                    &mut report,
                )?;
                report.affect(source);
                report.affect(destination);
                return Ok(report);
            }
        }
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
        progress,
        &mut |_| {},
        &mut report,
    )?;
    remove_entry(source, &CancellationToken::new(), &mut report)?;
    report.affect(source);
    Ok(report)
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

fn copy_entry(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    check_cancel(cancel)?;
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| OperationError::io(source, error))?;
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
        progress,
        destination_created,
        report,
    )
}

fn copy_resolved_entry(
    source: &Path,
    resolution: &DestinationResolution,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    let source_metadata =
        fs::symlink_metadata(source).map_err(|error| OperationError::io(source, error))?;
    let file_type = source_metadata.file_type();
    if file_type.is_symlink() {
        copy_symlink_safely(
            source,
            &resolution.path,
            &source_metadata,
            resolution.replace_existing,
            cancel,
        )?;
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

fn copy_directory(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    progress: &mut FileProgressCallback<'_>,
    destination_created: &mut DestinationCreatedCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    if !path_exists(destination) {
        fs::create_dir(destination).map_err(|error| OperationError::io(destination, error))?;
        report.directories += 1;
        report.affect(destination);
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
            progress,
            destination_created,
            report,
        )?;
    }
    Ok(())
}

fn replace_directory_safely(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
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
        progress,
        destination_created,
        report,
    );
    if let Err(error) = result {
        let _ = remove_entry(
            &temporary,
            &CancellationToken::new(),
            &mut FileOperationReport::new(),
        );
        return Err(error);
    }
    replace_with_temporary(&temporary, destination)
}

fn move_directory_merged(
    source: &Path,
    destination: &Path,
    cancel: &CancellationToken,
    resolve_conflict: &mut dyn FnMut(ConflictCategory, &Path, &Path) -> ConflictAction,
    progress: &mut FileProgressCallback<'_>,
    report: &mut FileOperationReport,
) -> Result<(), OperationError> {
    for entry in fs::read_dir(source).map_err(|error| OperationError::io(source, error))? {
        check_cancel(cancel)?;
        let entry = entry.map_err(|error| OperationError::io(source, error))?;
        let source_child = entry.path();
        let destination_child = destination.join(entry.file_name());
        if path_exists(&destination_child) {
            let metadata = fs::symlink_metadata(&source_child)
                .map_err(|error| OperationError::io(&source_child, error))?;
            if metadata.file_type().is_dir()
                && fs::symlink_metadata(&destination_child)
                    .is_ok_and(|item| item.file_type().is_dir())
            {
                match resolve_conflict(
                    ConflictCategory::ExistingDirectory,
                    &source_child,
                    &destination_child,
                ) {
                    ConflictAction::Skip => {
                        report.skipped.push(source_child);
                        continue;
                    }
                    ConflictAction::KeepBoth => {
                        let kept = keep_both_path(&destination_child);
                        let child_report = move_path_with_progress(
                            &source_child,
                            &kept,
                            cancel,
                            resolve_conflict,
                            progress,
                        )?;
                        merge_report(report, child_report);
                    }
                    ConflictAction::Replace => move_directory_merged(
                        &source_child,
                        &destination_child,
                        cancel,
                        resolve_conflict,
                        progress,
                        report,
                    )?,
                }
                continue;
            }
        }
        match move_path_with_progress(
            &source_child,
            &destination_child,
            cancel,
            resolve_conflict,
            progress,
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
    if let Err(error) = if replace_existing {
        replace_with_temporary(&temporary, destination)
    } else {
        fs::rename(&temporary, destination).map_err(|error| OperationError::io(destination, error))
    } {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
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
    let mut input = File::open(source).map_err(|error| OperationError::io(source, error))?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)
        .map_err(|error| OperationError::io(destination, error))?;
    let mut copied = 0_u64;
    let result = (|| {
        let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
        loop {
            check_cancel(cancel)?;
            let read = input
                .read(&mut buffer)
                .map_err(|error| OperationError::io(source, error))?;
            if read == 0 {
                break;
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| OperationError::io(destination, error))?;
            copied += read as u64;
            progress(read as u64, false, source);
        }
        output
            .sync_all()
            .map_err(|error| OperationError::io(destination, error))?;
        let permissions = fs::metadata(source)
            .map_err(|error| OperationError::io(source, error))?
            .permissions();
        fs::set_permissions(destination, permissions)
            .map_err(|error| OperationError::io(destination, error))?;
        Ok(())
    })();
    result?;
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
) -> Result<(), OperationError> {
    check_cancel(cancel)?;
    let temporary = unique_sibling(destination, ".asterfiles-copy");
    if let Err(error) = copy_symlink(source, &temporary, metadata) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    let result = if replace_existing {
        replace_with_temporary(&temporary, destination)
    } else {
        fs::rename(&temporary, destination).map_err(|error| OperationError::io(destination, error))
    };
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
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
    match resolve_conflict(category, source, destination) {
        ConflictAction::Skip => Err(OperationError::ConflictSkipped(destination.to_path_buf())),
        ConflictAction::KeepBoth => Ok(DestinationResolution {
            path: keep_both_path(destination),
            replace_existing: false,
        }),
        ConflictAction::Replace if category == ConflictCategory::ExistingDirectory => {
            Ok(DestinationResolution {
                path: destination.to_path_buf(),
                replace_existing: false,
            })
        }
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
        write(&source, &vec![7_u8; COPY_BUFFER_SIZE * 2]);
        write(&destination, b"old target");
        let cancel = CancellationToken::new();
        let cancel_after_first_chunk = cancel.clone();
        let result = copy_path_with_progress(
            &source,
            &destination,
            &cancel,
            &mut replace,
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
        write(&source, &vec![9_u8; COPY_BUFFER_SIZE * 2]);
        write(&destination, b"old target");
        let cancel = CancellationToken::new();
        let cancel_after_first_chunk = cancel.clone();
        let result = move_path_with_progress(
            &source,
            &destination,
            &cancel,
            &mut replace,
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
        let content = vec![3_u8; COPY_BUFFER_SIZE + 17];
        write(&source, &content);
        write(&destination, b"old target");
        let mut increments = Vec::new();
        let report = copy_path_with_progress(
            &source,
            &destination,
            &CancellationToken::new(),
            &mut replace,
            &mut |bytes, completed, _| {
                if bytes > 0 && !completed {
                    increments.push(bytes);
                }
            },
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(fs::read(&destination).unwrap(), content);
        assert_eq!(report.bytes, (COPY_BUFFER_SIZE + 17) as u64);
        assert_eq!(increments, vec![COPY_BUFFER_SIZE as u64, 17]);
        assert!(temporary_siblings(temp.path()).is_empty());
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
