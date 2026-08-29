use std::{
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
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
    pub path: PathBuf,
    pub result: Result<(), String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecycleResult {
    pub items: Vec<FileOperationItemResult>,
    pub aborted: bool,
}

pub fn recycle(paths: &[PathBuf]) -> RecycleResult {
    let owned_paths = paths.to_vec();
    match thread::Builder::new()
        .name("asterfiles-recycle".into())
        .spawn(move || recycle_on_com_thread(owned_paths))
    {
        Ok(worker) => worker
            .join()
            .unwrap_or_else(|_| failed_result(paths, "recycle worker terminated unexpectedly")),
        Err(error) => failed_result(paths, &format!("failed to start recycle worker: {error}")),
    }
}

fn recycle_on_com_thread(paths: Vec<PathBuf>) -> RecycleResult {
    let com = match ComApartment::initialize() {
        Ok(com) => com,
        Err(error) => return failed_result(&paths, &error.to_string()),
    };

    let mut items = Vec::with_capacity(paths.len());
    let mut aborted = false;
    for path in paths {
        let result = recycle_one(&path);
        aborted |= result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted);
        items.push(FileOperationItemResult {
            path,
            result: result.map_err(|error| error.to_string()),
        });
    }
    drop(com);
    RecycleResult { items, aborted }
}

fn recycle_one(path: &Path) -> io::Result<()> {
    let wide_path = shell_path(path)?;
    let item: IShellItem = unsafe { SHCreateItemFromParsingName(PCWSTR(wide_path.as_ptr()), None) }
        .map_err(windows_error)?;
    let operation: IFileOperation =
        unsafe { CoCreateInstance(&FileOperation, None, CLSCTX_LOCAL_SERVER) }
            .map_err(windows_error)?;

    unsafe {
        operation
            .SetOperationFlags(recycle_flags())
            .map_err(windows_error)?;
        let sink_state = std::sync::Arc::new(std::sync::Mutex::new(None));
        let sink = IFileOperationProgressSink::from(DeleteResultSink {
            result: sink_state.clone(),
        });
        operation.DeleteItem(&item, &sink).map_err(windows_error)?;
        operation.PerformOperations().map_err(windows_error)?;
        if operation
            .GetAnyOperationsAborted()
            .map_err(windows_error)?
            .as_bool()
        {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "recycle operation aborted",
            ));
        }
        delete_result(&sink_state)?;
    }
    Ok(())
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
    result: std::sync::Arc<std::sync::Mutex<Option<HRESULT>>>,
}

fn delete_result(result_state: &std::sync::Mutex<Option<HRESULT>>) -> io::Result<()> {
    match *result_state.lock().expect("delete result sink poisoned") {
        Some(result) => result
            .ok()
            .map_err(|_| windows_error(WindowsError::from(result))),
        None => Err(io::Error::other("shell did not report a recycle result")),
    }
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
        Ok(())
    }
    fn PostDeleteItem(
        &self,
        _flags: u32,
        _item: windows::core::Ref<IShellItem>,
        result: HRESULT,
        _created: windows::core::Ref<IShellItem>,
    ) -> windows::core::Result<()> {
        *self.result.lock().expect("delete result sink poisoned") = Some(result);
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
            .map(|path| FileOperationItemResult {
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
        let result = recycle(&[PathBuf::new()]);
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
    fn shell_item_failure_is_reported_as_item_failure() {
        let state = std::sync::Mutex::new(Some(HRESULT(0x80004005_u32 as i32)));
        assert!(delete_result(&state).is_err());
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
