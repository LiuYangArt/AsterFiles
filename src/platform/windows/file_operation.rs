use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::Shell::{
            FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
            FOFX_EARLYFAILURE, FOFX_RECYCLEONDELETE, FileOperation, IFileOperation,
            IFileOperationProgressSink, IFileOperationProgressSink_Impl, ILGetSize, IShellItem,
            SHCreateItemFromIDList, SHCreateItemFromParsingName, SHGetIDListFromObject,
            SIGDN_FILESYSPATH,
        },
    },
    core::{Error as WindowsError, HRESULT, PCWSTR, implement},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperationItemResult {
    pub index: usize,
    pub path: PathBuf,
    pub result: Result<(), String>,
    pub recycled_identity: Option<Vec<u8>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleResult {
    pub items: Vec<FileOperationItemResult>,
    pub aborted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreRecycleResult {
    Completed,
    Pending {
        temporary: PathBuf,
        identity: crate::domain::file_operations::FileIdentity,
        message: String,
    },
}

#[derive(Debug, Clone)]
struct DeleteCallbackResult {
    result: HRESULT,
    recycled_identity: Option<Vec<u8>>,
    created_path: Option<PathBuf>,
}

pub fn restore_recycled(
    original: &Path,
    absolute_pidl: &[u8],
    is_cancelled: impl Fn() -> bool + Send + Sync + 'static,
) -> Result<RestoreRecycleResult, String> {
    let original = original.to_path_buf();
    let absolute_pidl = absolute_pidl.to_vec();
    let is_cancelled: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(is_cancelled);
    thread::Builder::new()
        .name("asterfiles-recycle-restore".into())
        .spawn(move || restore_recycled_on_com_thread(&original, &absolute_pidl, is_cancelled))
        .map_err(|error| format!("failed to start recycle restore worker: {error}"))?
        .join()
        .map_err(|_| "recycle restore worker terminated unexpectedly".to_owned())?
}

fn restore_recycled_on_com_thread(
    original: &Path,
    absolute_pidl: &[u8],
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<RestoreRecycleResult, String> {
    let com = ComApartment::initialize().map_err(|error| error.to_string())?;
    let result = restore_recycled_batch(original, absolute_pidl, &is_cancelled);
    drop(com);
    result
}

fn restore_recycled_batch(
    original: &Path,
    absolute_pidl: &[u8],
    is_cancelled: &Arc<dyn Fn() -> bool + Send + Sync>,
) -> Result<RestoreRecycleResult, String> {
    if is_cancelled() {
        return Err("recycle restore cancelled".to_owned());
    }
    if std::fs::symlink_metadata(original).is_ok() {
        return Err(format!(
            "restore destination already exists: {}",
            original.display()
        ));
    }
    let parent = original
        .parent()
        .ok_or("restore destination has no parent")?;
    let temporary = unique_restore_path(original);
    let name = temporary
        .file_name()
        .ok_or("restore temporary destination has no name")?;
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_LOCAL_SERVER) }
            .map_err(|error| windows_error(error).to_string())?;
    unsafe { operation.SetOperationFlags(restore_flags()) }
        .map_err(|error| windows_error(error).to_string())?;
    let recycled = shell_item_from_pidl(absolute_pidl)?;
    let parent_wide = shell_path(parent).map_err(|error| error.to_string())?;
    let parent_item: IShellItem =
        unsafe { SHCreateItemFromParsingName(PCWSTR(parent_wide.as_ptr()), None) }
            .map_err(|error| windows_error(error).to_string())?;
    let mut name_wide = name.encode_wide().collect::<Vec<_>>();
    if name_wide.contains(&0) {
        return Err("restore destination name is invalid".to_owned());
    }
    name_wide.push(0);
    let results = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let sink = IFileOperationProgressSink::from(DeleteResultSink {
        index: 0,
        results: results.clone(),
        is_cancelled: is_cancelled.clone(),
    });
    unsafe { operation.MoveItem(&recycled, &parent_item, PCWSTR(name_wide.as_ptr()), &sink) }
        .map_err(|error| windows_error(error).to_string())?;
    if is_cancelled() {
        return Err("recycle restore cancelled".to_owned());
    }
    unsafe { operation.PerformOperations() }.map_err(|error| windows_error(error).to_string())?;
    let callback = results
        .lock()
        .expect("restore result sink poisoned")
        .remove(&0);
    let aborted = unsafe { operation.GetAnyOperationsAborted() }
        .map(|value| value.as_bool())
        .unwrap_or(true);
    match callback {
        Some(callback)
            if callback.result.is_ok()
                && callback.recycled_identity.is_some()
                && callback
                    .created_path
                    .as_deref()
                    .is_some_and(|path| same_windows_path(path, &temporary))
                && !aborted
                && std::fs::symlink_metadata(&temporary).is_ok() =>
        {
            let identity = crate::fs::file_operations::file_identity(&temporary)
                .map_err(|error| format!("{error:?}"))?;
            if std::fs::symlink_metadata(original).is_ok() {
                return Ok(RestoreRecycleResult::Pending {
                    temporary,
                    identity,
                    message: format!(
                        "原位置已出现同名项目，已将回收站项目安全恢复到临时位置：{}",
                        original.display()
                    ),
                });
            }
            match std::fs::rename(&temporary, original) {
                Ok(()) => Ok(RestoreRecycleResult::Completed),
                Err(error) => Ok(RestoreRecycleResult::Pending {
                    temporary,
                    identity,
                    message: format!("回收站项目已恢复，等待移回原位置：{error}"),
                }),
            }
        }
        Some(callback) if callback.result.is_err() => {
            Err(windows_error(WindowsError::from(callback.result)).to_string())
        }
        _ => Err("Shell did not restore the recycled item".to_owned()),
    }
}

fn unique_restore_path(original: &Path) -> PathBuf {
    let parent = original.parent().unwrap_or_else(|| Path::new(""));
    let file_name = original
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("item"));
    for index in 1_u64.. {
        let mut name = OsString::from(".asterfiles-restore-");
        name.push(index.to_string());
        name.push("-");
        name.push(file_name);
        let candidate = parent.join(name);
        if std::fs::symlink_metadata(&candidate).is_err() {
            return candidate;
        }
    }
    unreachable!()
}
fn restore_flags() -> FILEOPERATION_FLAGS {
    FILEOPERATION_FLAGS(FOFX_EARLYFAILURE.0 | FOF_NOERRORUI.0 | FOF_SILENT.0)
}

fn shell_item_from_pidl(bytes: &[u8]) -> Result<IShellItem, String> {
    if bytes.len() < 2 || bytes[bytes.len() - 2..] != [0, 0] {
        return Err("invalid recycled item identity".to_owned());
    }
    let mut words = vec![0_u16; bytes.len().div_ceil(2)];
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), words.as_mut_ptr().cast(), bytes.len());
        SHCreateItemFromIDList(words.as_ptr().cast())
    }
    .map_err(|error| windows_error(error).to_string())
}

fn shell_item_path(item: &IShellItem) -> Option<PathBuf> {
    let value = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok()?;
    if value.is_null() {
        return None;
    }
    let wide = unsafe { value.as_wide() };
    let path = PathBuf::from(OsString::from_wide(wide));
    unsafe { CoTaskMemFree(Some(value.as_ptr().cast())) };
    Some(path)
}

fn same_windows_path(left: &Path, right: &Path) -> bool {
    left.as_os_str()
        .encode_wide()
        .map(|value| {
            if value <= 0x7f {
                (value as u8).to_ascii_lowercase() as u16
            } else {
                value
            }
        })
        .eq(right.as_os_str().encode_wide().map(|value| {
            if value <= 0x7f {
                (value as u8).to_ascii_lowercase() as u16
            } else {
                value
            }
        }))
}

fn absolute_pidl(item: &IShellItem) -> Option<Vec<u8>> {
    let pidl = unsafe { SHGetIDListFromObject(item) }.ok()?;
    if pidl.is_null() {
        return None;
    }
    let size = unsafe { ILGetSize(Some(pidl.cast_const())) } as usize;
    let bytes = (size >= 2)
        .then(|| unsafe { std::slice::from_raw_parts(pidl.cast::<u8>(), size).to_vec() });
    unsafe { CoTaskMemFree(Some(pidl.cast())) };
    bytes
}

pub fn recycle(
    paths: &[PathBuf],
    is_cancelled: impl Fn() -> bool + Send + Sync + 'static,
) -> RecycleResult {
    let owned_paths = paths.to_vec();
    let is_cancelled: Arc<dyn Fn() -> bool + Send + Sync> = Arc::new(is_cancelled);
    match thread::Builder::new()
        .name("asterfiles-recycle".into())
        .spawn(move || recycle_on_com_thread(owned_paths, is_cancelled))
    {
        Ok(worker) => worker
            .join()
            .unwrap_or_else(|_| failed_result(paths, "recycle worker terminated unexpectedly")),
        Err(error) => failed_result(paths, &format!("failed to start recycle worker: {error}")),
    }
}

fn recycle_on_com_thread(
    paths: Vec<PathBuf>,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> RecycleResult {
    let com = match ComApartment::initialize() {
        Ok(com) => com,
        Err(error) => return failed_result(&paths, &error.to_string()),
    };

    let result = recycle_batch(&paths, is_cancelled);
    drop(com);
    result
}

fn recycle_batch(
    paths: &[PathBuf],
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
) -> RecycleResult {
    let operation: IFileOperation =
        match unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_LOCAL_SERVER) } {
            Ok(operation) => operation,
            Err(error) => return failed_result(paths, &windows_error(error).to_string()),
        };
    let results = std::sync::Arc::new(std::sync::Mutex::new(HashMap::new()));
    let mut queued = Vec::with_capacity(paths.len());
    let mut initial = HashMap::new();

    if let Err(error) = unsafe { operation.SetOperationFlags(recycle_flags()) } {
        return failed_result(paths, &windows_error(error).to_string());
    }
    for (index, path) in paths.iter().enumerate() {
        if is_cancelled() {
            break;
        }
        let item: io::Result<IShellItem> = shell_path(path).and_then(|wide_path| unsafe {
            SHCreateItemFromParsingName(PCWSTR(wide_path.as_ptr()), None).map_err(windows_error)
        });
        match item {
            Ok(item) => {
                let sink = IFileOperationProgressSink::from(DeleteResultSink {
                    index,
                    results: results.clone(),
                    is_cancelled: is_cancelled.clone(),
                });
                match unsafe { operation.DeleteItem(&item, &sink) }.map_err(windows_error) {
                    Ok(()) => queued.push((index, path.clone())),
                    Err(error) => {
                        initial.insert(index, Err(error.to_string()));
                    }
                }
            }
            Err(error) => {
                initial.insert(index, Err(error.to_string()));
            }
        }
    }

    let perform_error = if queued.is_empty() || is_cancelled() {
        None
    } else {
        unsafe { operation.PerformOperations() }
            .map_err(windows_error)
            .err()
            .map(|error| error.to_string())
    };
    let aborted = unsafe { operation.GetAnyOperationsAborted() }
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let reported = results.lock().expect("delete result sink poisoned").clone();
    let cancelled = is_cancelled();
    let items = merge_recycle_results(
        paths,
        initial,
        &queued,
        reported,
        perform_error.as_deref(),
        aborted || cancelled,
    );
    RecycleResult {
        items,
        aborted: aborted || cancelled,
    }
}

fn recycle_flags() -> FILEOPERATION_FLAGS {
    FILEOPERATION_FLAGS(
        FOF_ALLOWUNDO.0
            | FOFX_RECYCLEONDELETE.0
            | FOF_NOCONFIRMATION.0
            | FOF_NOERRORUI.0
            | FOF_SILENT.0,
    )
}

fn shell_path(path: &Path) -> io::Result<Vec<u16>> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid recycle path",
        ));
    }
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid recycle path",
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn windows_error(error: WindowsError) -> io::Error {
    io::Error::other(error.to_string())
}

#[implement(IFileOperationProgressSink)]
struct DeleteResultSink {
    index: usize,
    results: std::sync::Arc<std::sync::Mutex<HashMap<usize, DeleteCallbackResult>>>,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

fn merge_recycle_results(
    paths: &[PathBuf],
    mut initial: HashMap<usize, Result<(), String>>,
    queued: &[(usize, PathBuf)],
    mut reported: HashMap<usize, DeleteCallbackResult>,
    perform_error: Option<&str>,
    aborted: bool,
) -> Vec<FileOperationItemResult> {
    let queued = queued.iter().map(|(index, _)| *index).collect::<Vec<_>>();
    paths
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, path)| {
            let callback = reported.remove(&index);
            let result = initial.remove(&index).unwrap_or_else(|| {
                callback.as_ref().map_or_else(
                    || {
                        Err(perform_error.map(str::to_owned).unwrap_or_else(|| {
                            if aborted {
                                "recycle operation aborted".to_owned()
                            } else if queued.contains(&index) {
                                "shell did not report a recycle result".to_owned()
                            } else {
                                "item was not queued for recycling".to_owned()
                            }
                        }))
                    },
                    |callback| {
                        callback.result.ok().map_err(|_| {
                            windows_error(WindowsError::from(callback.result)).to_string()
                        })
                    },
                )
            });
            let recycled_identity = result
                .is_ok()
                .then(|| callback.and_then(|value| value.recycled_identity))
                .flatten();
            FileOperationItemResult {
                index,
                path,
                result,
                recycled_identity,
            }
        })
        .collect()
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for DeleteResultSink_Impl {
    fn StartOperations(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn FinishOperations(&self, _result: HRESULT) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreRenameItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostRenameItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreMoveItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        _destination: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows::core::Result<()> {
        if (self.is_cancelled)() {
            Err(WindowsError::from_hresult(HRESULT(0x80004004_u32 as i32)))
        } else {
            Ok(())
        }
    }
    fn PostMoveItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        _destination: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
        result: HRESULT,
        created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        self.results
            .lock()
            .expect("move result sink poisoned")
            .insert(
                self.index,
                DeleteCallbackResult {
                    result,
                    recycled_identity: created.as_ref().and_then(absolute_pidl),
                    created_path: created.as_ref().and_then(shell_item_path),
                },
            );
        Ok(())
    }
    fn PreCopyItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        _destination: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostCopyItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        _destination: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
        _result: HRESULT,
        _created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PreDeleteItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        if (self.is_cancelled)() {
            Err(WindowsError::from_hresult(HRESULT(0x80004004_u32 as i32)))
        } else {
            Ok(())
        }
    }
    fn PostDeleteItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        result: HRESULT,
        created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        self.results
            .lock()
            .expect("delete result sink poisoned")
            .insert(
                self.index,
                DeleteCallbackResult {
                    result,
                    recycled_identity: created.as_ref().and_then(absolute_pidl),
                    created_path: None,
                },
            );
        Ok(())
    }
    fn PreNewItem(
        &self,
        _flags: u32,
        _destination: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn PostNewItem(
        &self,
        _flags: u32,
        _destination: windows::core::Ref<IShellItem>,
        _new_name: &PCWSTR,
        _template_name: &PCWSTR,
        _attributes: u32,
        _result: HRESULT,
        _created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _total: u32, _completed: u32) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> windows::core::Result<()> {
        Ok(())
    }
}
fn failed_result(paths: &[PathBuf], message: &str) -> RecycleResult {
    RecycleResult {
        items: paths
            .iter()
            .cloned()
            .enumerate()
            .map(|(index, path)| FileOperationItemResult {
                index,
                path,
                result: Err(message.to_owned()),
                recycled_identity: None,
            })
            .collect(),
        aborted: false,
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> io::Result<Self> {
        unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }
            .ok()
            .map_err(windows_error)?;
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_path_is_rejected_without_shell_ui() {
        let result = recycle(&[PathBuf::new()], || false);
        assert_eq!(result.items.len(), 1);
        assert!(result.items[0].result.is_err());
        assert!(!result.aborted);
    }

    #[test]
    fn strict_recycle_flags_disable_shell_ui_and_forbid_permanent_delete() {
        let flags = recycle_flags().0;
        for required in [
            FOF_ALLOWUNDO.0,
            FOFX_RECYCLEONDELETE.0,
            FOF_NOCONFIRMATION.0,
            FOF_NOERRORUI.0,
            FOF_SILENT.0,
        ] {
            assert_eq!(flags & required, required);
        }
    }

    #[test]
    fn batch_results_preserve_input_order_and_item_failures() {
        let paths = vec![PathBuf::from("first"), PathBuf::from("second")];
        let queued = vec![(0, paths[0].clone()), (1, paths[1].clone())];
        let reported = HashMap::from([
            (
                0,
                DeleteCallbackResult {
                    result: HRESULT(0),
                    recycled_identity: Some(vec![0, 0]),
                    created_path: None,
                },
            ),
            (
                1,
                DeleteCallbackResult {
                    result: HRESULT(0x80004005_u32 as i32),
                    recycled_identity: None,
                    created_path: None,
                },
            ),
        ]);
        let results = merge_recycle_results(&paths, HashMap::new(), &queued, reported, None, false);
        assert_eq!(
            results.iter().map(|item| &item.path).collect::<Vec<_>>(),
            [&paths[0], &paths[1]]
        );
        assert!(results[0].result.is_ok());
        assert!(results[1].result.is_err());
    }
    #[test]
    fn batch_results_keep_preparation_errors() {
        let path = PathBuf::from("bad");
        let results = merge_recycle_results(
            std::slice::from_ref(&path),
            HashMap::from([(0, Err("invalid path".to_owned()))]),
            &[],
            HashMap::new(),
            None,
            false,
        );
        assert_eq!(results[0].result, Err("invalid path".to_owned()));
    }

    #[test]
    fn issue_83_restore_flags_never_confirm_or_rename_on_collision() {
        let flags = restore_flags().0;
        assert_eq!(flags & FOFX_EARLYFAILURE.0, FOFX_EARLYFAILURE.0);
        assert_eq!(flags & FOF_NOCONFIRMATION.0, 0);
        assert_eq!(
            flags & windows::Win32::UI::Shell::FOF_RENAMEONCOLLISION.0,
            0
        );
    }

    #[test]
    fn issue_83_restore_uses_a_unique_temporary_name() {
        let parent = std::env::temp_dir().join("asterfiles-restore-name-test");
        let original = parent.join("item.txt");
        let first = unique_restore_path(&original);
        std::fs::create_dir_all(&parent).unwrap();
        std::fs::write(&first, b"occupied").unwrap();
        let second = unique_restore_path(&original);
        assert_ne!(first, second);
        assert_eq!(second.parent(), Some(parent.as_path()));
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("item.txt")
        );
        let _ = std::fs::remove_file(first);
        let _ = std::fs::remove_dir(parent);
    }
    #[test]
    fn issue_83_invalid_recycled_identity_is_rejected_before_shell_access() {
        assert!(shell_item_from_pidl(&[]).is_err());
        assert!(shell_item_from_pidl(&[1, 2, 3]).is_err());
    }

    #[test]
    fn issue_83_windows_path_comparison_handles_drive_letter_case() {
        assert!(same_windows_path(
            Path::new(r"C:\Example\File.txt"),
            Path::new(r"c:\example\file.TXT"),
        ));
        assert!(!same_windows_path(
            Path::new(r"C:\Example\File.txt"),
            Path::new(r"C:\Example\Other.txt"),
        ));
    }
    #[test]
    fn embedded_nul_is_rejected_before_shell_access() {
        let path = PathBuf::from("invalid\0path");
        assert_eq!(
            shell_path(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}
