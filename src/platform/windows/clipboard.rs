use std::{
    ffi::OsString,
    io,
    mem::{ManuallyDrop, size_of},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr, thread,
    time::Duration,
};

use windows::{
    Win32::{
        Foundation::{DATA_S_SAMEFORMATETC, DV_E_FORMATETC, E_NOTIMPL, OLE_E_ADVISENOTSUPPORTED},
        System::{
            Com::{
                DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject,
                IDataObject_Impl, IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0,
                TYMED_HGLOBAL,
            },
            DataExchange::RegisterClipboardFormatW,
            Memory::{
                GMEM_MOVEABLE, GMEM_ZEROINIT, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
            },
            Ole::{
                OleFlushClipboard, OleGetClipboard, OleInitialize, OleSetClipboard,
                OleUninitialize, ReleaseStgMedium,
            },
        },
        UI::Shell::{DROPFILES, DragQueryFileW, HDROP, SHCreateStdEnumFmtEtc},
    },
    core::{Error as WindowsError, HRESULT, PCWSTR, implement},
};
use windows_sys::Win32::Foundation::GlobalFree;

const CF_HDROP: u16 = 15;
const DROPEFFECT_COPY: u32 = 1;
const DROPEFFECT_MOVE: u32 = 2;
const CLIPBOARD_RETRIES: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardOperation {
    Copy,
    Move,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFileList {
    pub paths: Vec<PathBuf>,
    pub operation: ClipboardOperation,
}

pub fn write_file_list(paths: &[PathBuf], operation: ClipboardOperation) -> io::Result<()> {
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "clipboard file list is empty",
        ));
    }

    let _ole = OleApartment::initialize()?;
    let effect = match operation {
        ClipboardOperation::Copy => DROPEFFECT_COPY,
        ClipboardOperation::Move => DROPEFFECT_MOVE,
    };
    let data_object = IDataObject::from(ClipboardDataObject {
        formats: vec![
            (CF_HDROP, encode_dropfiles(paths)?),
            (
                preferred_drop_effect_format()?,
                effect.to_ne_bytes().to_vec(),
            ),
        ],
    });

    retry_clipboard(|| unsafe {
        OleSetClipboard(&data_object)?;
        OleFlushClipboard()
    })
}

pub fn read_file_list() -> io::Result<Option<ClipboardFileList>> {
    let _ole = OleApartment::initialize()?;
    let data_object = retry_get_clipboard().map_err(windows_error)?;

    let drop_format = format_etc(CF_HDROP);
    if unsafe { data_object.QueryGetData(&drop_format) }.is_err() {
        return Ok(None);
    }
    let drop_medium = unsafe { data_object.GetData(&drop_format) }.map_err(windows_error)?;
    let drop_medium = StgMediumGuard::new(drop_medium);
    let paths = read_dropfiles(drop_medium.medium())?;
    let operation = read_drop_effect(&data_object).unwrap_or(ClipboardOperation::Copy);
    Ok(Some(ClipboardFileList { paths, operation }))
}

fn retry_clipboard(mut action: impl FnMut() -> windows::core::Result<()>) -> io::Result<()> {
    let mut last_error = None;
    for attempt in 0..CLIPBOARD_RETRIES {
        match action() {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < CLIPBOARD_RETRIES {
            thread::sleep(Duration::from_millis(5 * (attempt as u64 + 1)));
        }
    }
    Err(windows_error(last_error.expect("clipboard retry must run")))
}

fn retry_get_clipboard() -> windows::core::Result<IDataObject> {
    let mut last_error = None;
    for attempt in 0..CLIPBOARD_RETRIES {
        match unsafe { OleGetClipboard() } {
            Ok(data_object) => return Ok(data_object),
            Err(error) => last_error = Some(error),
        }
        if attempt + 1 < CLIPBOARD_RETRIES {
            thread::sleep(Duration::from_millis(5 * (attempt as u64 + 1)));
        }
    }
    Err(last_error.expect("clipboard retry must run"))
}

fn encode_dropfiles(paths: &[PathBuf]) -> io::Result<Vec<u8>> {
    let mut names = Vec::<u16>::new();
    for path in paths {
        validate_path(path)?;
        names.extend(path.as_os_str().encode_wide());
        names.push(0);
    }
    names.push(0);
    let header_size = size_of::<DROPFILES>();
    let mut bytes = vec![0_u8; header_size + names.len() * size_of::<u16>()];
    let header = DROPFILES {
        pFiles: header_size as u32,
        pt: Default::default(),
        fNC: false.into(),
        fWide: true.into(),
    };
    unsafe {
        ptr::copy_nonoverlapping(
            (&header as *const DROPFILES).cast::<u8>(),
            bytes.as_mut_ptr(),
            header_size,
        );
        ptr::copy_nonoverlapping(
            names.as_ptr().cast::<u8>(),
            bytes.as_mut_ptr().add(header_size),
            names.len() * size_of::<u16>(),
        );
    }
    Ok(bytes)
}

fn read_dropfiles(medium: &STGMEDIUM) -> io::Result<Vec<PathBuf>> {
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err(io::Error::other("clipboard file list is not HGLOBAL data"));
    }
    let handle = unsafe { medium.u.hGlobal };
    if handle.is_invalid() || unsafe { GlobalSize(handle) } < size_of::<DROPFILES>() {
        return Err(io::Error::other("clipboard file list is malformed"));
    }
    let drop_handle = HDROP(handle.0);
    let count = unsafe { DragQueryFileW(drop_handle, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { DragQueryFileW(drop_handle, index, None) };
        let mut buffer = vec![0_u16; length as usize + 1];
        let copied = unsafe { DragQueryFileW(drop_handle, index, Some(&mut buffer)) };
        if copied == 0 && length != 0 {
            return Err(io::Error::last_os_error());
        }
        paths.push(PathBuf::from(OsString::from_wide(
            &buffer[..copied as usize],
        )));
    }
    Ok(paths)
}

fn validate_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.as_os_str().encode_wide().any(|unit| unit == 0) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid clipboard path",
        ))
    } else {
        Ok(())
    }
}

fn preferred_drop_effect_format() -> io::Result<u16> {
    let name = "Preferred DropEffect\0".encode_utf16().collect::<Vec<_>>();
    let format = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
    if format == 0 {
        Err(io::Error::last_os_error())
    } else {
        u16::try_from(format).map_err(|_| io::Error::last_os_error())
    }
}

fn read_drop_effect(data_object: &IDataObject) -> Option<ClipboardOperation> {
    let format = format_etc(preferred_drop_effect_format().ok()?);
    let medium = unsafe { data_object.GetData(&format) }.ok()?;
    let medium = StgMediumGuard::new(medium);
    if medium.medium().tymed != TYMED_HGLOBAL.0 as u32 {
        return None;
    }
    let handle = unsafe { medium.medium().u.hGlobal };
    if handle.is_invalid() || unsafe { GlobalSize(handle) } < size_of::<u32>() {
        return None;
    }
    let pointer = unsafe { GlobalLock(handle) }.cast::<u32>();
    if pointer.is_null() {
        return None;
    }
    let effect = unsafe { pointer.read_unaligned() };
    let _ = unsafe { GlobalUnlock(handle) };
    if effect & DROPEFFECT_MOVE != 0 {
        Some(ClipboardOperation::Move)
    } else {
        Some(ClipboardOperation::Copy)
    }
}

fn format_etc(format: u16) -> FORMATETC {
    FORMATETC {
        cfFormat: format,
        ptd: ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

fn allocate_medium(bytes: &[u8]) -> windows::core::Result<STGMEDIUM> {
    let memory = unsafe { GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len()) }?;
    let pointer = unsafe { GlobalLock(memory) }.cast::<u8>();
    if pointer.is_null() {
        unsafe { GlobalFree(memory.0 as _) };
        return Err(WindowsError::from_thread());
    }
    unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, bytes.len()) };
    let _ = unsafe { GlobalUnlock(memory) };
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: memory },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

fn windows_error(error: WindowsError) -> io::Error {
    io::Error::other(error.to_string())
}

struct OleApartment;

impl OleApartment {
    fn initialize() -> io::Result<Self> {
        unsafe { OleInitialize(None) }
            .map(|()| Self)
            .map_err(windows_error)
    }
}

impl Drop for OleApartment {
    fn drop(&mut self) {
        unsafe { OleUninitialize() };
    }
}

struct StgMediumGuard(STGMEDIUM);

impl StgMediumGuard {
    fn new(medium: STGMEDIUM) -> Self {
        Self(medium)
    }

    fn medium(&self) -> &STGMEDIUM {
        &self.0
    }
}

impl Drop for StgMediumGuard {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

#[implement(IDataObject)]
struct ClipboardDataObject {
    formats: Vec<(u16, Vec<u8>)>,
}

#[allow(non_snake_case)]
impl IDataObject_Impl for ClipboardDataObject_Impl {
    fn GetData(&self, format: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { format.as_ref() }.ok_or_else(|| WindowsError::from(E_NOTIMPL))?;
        self.formats
            .iter()
            .find(|(id, _)| supports_format(format, *id))
            .map(|(_, bytes)| allocate_medium(bytes))
            .unwrap_or_else(|| Err(DV_E_FORMATETC.into()))
    }

    fn GetDataHere(
        &self,
        _format: *const FORMATETC,
        _medium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, format: *const FORMATETC) -> HRESULT {
        let Some(format) = (unsafe { format.as_ref() }) else {
            return DV_E_FORMATETC;
        };
        if self
            .formats
            .iter()
            .any(|(id, _)| supports_format(format, *id))
        {
            HRESULT(0)
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _input: *const FORMATETC, output: *mut FORMATETC) -> HRESULT {
        if let Some(output) = unsafe { output.as_mut() } {
            output.ptd = ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _format: *const FORMATETC,
        _medium: *const STGMEDIUM,
        _release: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let formats = self
            .formats
            .iter()
            .map(|(id, _)| format_etc(*id))
            .collect::<Vec<_>>();
        unsafe { SHCreateStdEnumFmtEtc(&formats) }
    }

    fn DAdvise(
        &self,
        _format: *const FORMATETC,
        _flags: u32,
        _sink: windows::core::Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _connection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

fn supports_format(format: &FORMATETC, expected: u16) -> bool {
    format.cfFormat == expected
        && format.dwAspect == DVASPECT_CONTENT.0
        && format.lindex == -1
        && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropfiles_preserves_windows_paths() {
        let paths = vec![
            PathBuf::from(r"C:\资料\一.txt"),
            PathBuf::from(r"\\server\share\two"),
        ];
        let bytes = encode_dropfiles(&paths).unwrap();
        let header = unsafe { bytes.as_ptr().cast::<DROPFILES>().read_unaligned() };
        assert_eq!(header.pFiles as usize, size_of::<DROPFILES>());
        assert!(header.fWide.as_bool());
        assert!(bytes.ends_with(&[0, 0, 0, 0]));
    }

    #[test]
    fn data_object_formats_match_explorer_contract() {
        let effect_format = 0xC123;
        assert!(supports_format(&format_etc(CF_HDROP), CF_HDROP));
        assert!(supports_format(&format_etc(effect_format), effect_format));
        assert!(!supports_format(&format_etc(99), CF_HDROP));
    }
}
