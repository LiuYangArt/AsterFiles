use std::{
    collections::HashMap,
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use windows::{
    Win32::{
        System::Com::{
            CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoUninitialize,
        },
        UI::Shell::{
            FILEOPERATION_FLAGS, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT,
            FOFX_RECYCLEONDELETE, FileOperation, IFileOperation, IFileOperationProgressSink,
            IFileOperationProgressSink_Impl, IShellItem, SHCreateItemFromParsingName,
        },
    },
    core::{Error as WindowsError, HRESULT, PCWSTR, implement},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileOperationItemResult {
    pub index: usize,
    pub path: PathBuf,
    pub result: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleResult {
    pub items: Vec<FileOperationItemResult>,
    pub aborted: bool,
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
    let results = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = IFileOperationProgressSink::from(DeleteResultSink {
        results: results.clone(),
        is_cancelled: is_cancelled.clone(),
    });
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
                match unsafe { operation.DeleteItem(&item, &sink) }.map_err(windows_error) {
                    Ok(()) => queued.push(path.clone()),
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
    results: std::sync::Arc<std::sync::Mutex<Vec<HRESULT>>>,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
}

fn merge_recycle_results(
    paths: &[PathBuf],
    mut initial: HashMap<usize, Result<(), String>>,
    queued: &[PathBuf],
    reported: Vec<HRESULT>,
    perform_error: Option<&str>,
    aborted: bool,
) -> Vec<FileOperationItemResult> {
    let mut reported = queued.iter().zip(reported);
    paths
        .iter()
        .cloned()
        .enumerate()
        .map(|(index, path)| {
            let result = initial.remove(&index).unwrap_or_else(|| {
                reported.next().map_or_else(
                    || {
                        Err(perform_error.map(str::to_owned).unwrap_or_else(|| {
                            if aborted {
                                "recycle operation aborted".to_owned()
                            } else {
                                "shell did not report a recycle result".to_owned()
                            }
                        }))
                    },
                    |(_, result)| {
                        result
                            .ok()
                            .map_err(|_| windows_error(WindowsError::from(result)).to_string())
                    },
                )
            });
            FileOperationItemResult {
                index,
                path,
                result,
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
        Ok(())
    }
    fn PostMoveItem(
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
        _created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        self.results
            .lock()
            .expect("delete result sink poisoned")
            .push(result);
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
        let reported = vec![HRESULT(0), HRESULT(0x80004005_u32 as i32)];
        let results = merge_recycle_results(&paths, HashMap::new(), &paths, reported, None, false);
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
            Vec::new(),
            None,
            false,
        );
        assert_eq!(results[0].result, Err("invalid path".to_owned()));
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
