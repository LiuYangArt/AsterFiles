use std::{
    ffi::{OsString, c_void},
    io,
    mem::{ManuallyDrop, size_of},
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    ptr,
    sync::{Arc, Mutex, OnceLock, Weak, mpsc},
};

use windows_sys::Win32::{
    Foundation::{HWND as RawHwnd, POINT as RawPoint, RECT},
    Graphics::Gdi::ClientToScreen,
    UI::WindowsAndMessaging::GetClientRect,
};

use windows::{
    Win32::{
        Foundation::{
            DATA_S_SAMEFORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
            DV_E_FORMATETC, E_NOTIMPL, HWND, OLE_E_ADVISENOTSUPPORTED, POINTL,
        },
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
                DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE, DROPEFFECT_NONE,
                DoDragDrop, IDropSource, IDropSource_Impl, IDropTarget, IDropTarget_Impl,
                OleInitialize, OleUninitialize, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
            },
            SystemServices::{MK_CONTROL, MK_LBUTTON, MK_RBUTTON, MK_SHIFT, MODIFIERKEYS_FLAGS},
        },
        UI::Shell::{DROPFILES, DragQueryFileW, HDROP, SHCreateStdEnumFmtEtc},
    },
    core::{Error as WindowsError, HRESULT, PCWSTR, Ref, implement},
};
use windows_sys::Win32::Foundation::GlobalFree;

const CF_HDROP: u16 = 15;
const MK_ALT: u32 = 0x20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DropEffect {
    #[default]
    None,
    Copy,
    Move,
    Link,
}

impl DropEffect {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Copy => "copy",
            Self::Move => "move",
            Self::Link => "link",
        }
    }

    fn native(self) -> DROPEFFECT {
        match self {
            Self::None => DROPEFFECT_NONE,
            Self::Copy => DROPEFFECT_COPY,
            Self::Move => DROPEFFECT_MOVE,
            Self::Link => DROPEFFECT_LINK,
        }
    }

    fn from_native(effect: DROPEFFECT) -> Self {
        if effect.0 & DROPEFFECT_MOVE.0 != 0 {
            Self::Move
        } else if effect.0 & DROPEFFECT_COPY.0 != 0 {
            Self::Copy
        } else if effect.0 & DROPEFFECT_LINK.0 != 0 {
            Self::Link
        } else {
            Self::None
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIntent {
    pub paths: Vec<PathBuf>,
    pub target: PathBuf,
    pub effect: DropEffect,
    pub right_button: bool,
    pub screen_x: i32,
    pub screen_y: i32,
    pub allowed_effects: u32,
}

pub const ALLOW_COPY: u32 = 1;
pub const ALLOW_MOVE: u32 = 2;
pub const ALLOW_LINK: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragDropLifecycle {
    Unregistered,
    Registered,
    Dragging,
    Revoked,
}

impl DragDropLifecycle {
    pub fn name(self) -> &'static str {
        match self {
            Self::Unregistered => "unregistered",
            Self::Registered => "registered",
            Self::Dragging => "dragging",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragDropEvent {
    Registered,
    Entered,
    Moved,
    Left,
    Dropped,
    Revoked,
}

impl DragDropEvent {
    pub fn name(self) -> &'static str {
        match self {
            Self::Registered => "registered",
            Self::Entered => "entered",
            Self::Moved => "moved",
            Self::Left => "left",
            Self::Dropped => "dropped",
            Self::Revoked => "revoked",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DragDropState {
    pub lifecycle: DragDropLifecycle,
    pub registered: bool,
    pub source_count: usize,
    pub target: Option<String>,
    pub negotiated_effect: &'static str,
    pub rejection_reason: Option<&'static str>,
    pub last_event: Option<DragDropEvent>,
    pub event_sequence: u64,
    pub cursor_y: Option<i32>,
}

impl Default for DragDropState {
    fn default() -> Self {
        Self {
            lifecycle: DragDropLifecycle::Unregistered,
            registered: false,
            source_count: 0,
            target: None,
            negotiated_effect: "none",
            rejection_reason: Some("not_registered"),
            last_event: None,
            event_sequence: 0,
            cursor_y: None,
        }
    }
}

impl DragDropState {
    fn record(&mut self, event: DragDropEvent) {
        self.event_sequence = self.event_sequence.saturating_add(1);
        self.last_event = Some(event);
        match event {
            DragDropEvent::Registered => {
                self.lifecycle = DragDropLifecycle::Registered;
                self.registered = true;
                self.rejection_reason = None;
            }
            DragDropEvent::Entered | DragDropEvent::Moved => {
                self.lifecycle = DragDropLifecycle::Dragging
            }
            DragDropEvent::Left | DragDropEvent::Dropped => {
                self.lifecycle = DragDropLifecycle::Registered;
                self.cursor_y = None;
                self.source_count = 0;
                self.target = None;
                self.negotiated_effect = "none";
                self.rejection_reason = None;
            }
            DragDropEvent::Revoked => {
                self.lifecycle = DragDropLifecycle::Revoked;
                self.registered = false;
                self.source_count = 0;
                self.target = None;
                self.negotiated_effect = "none";
                self.rejection_reason = Some("registration_revoked");
            }
        }
    }
}

type SharedState = Arc<Mutex<DragDropState>>;

#[derive(Debug, Clone, Default)]
pub struct DropTargetSnapshot {
    pub current: Option<PathBuf>,
    pub folder_rows: Vec<FolderDropTarget>,
}

#[derive(Debug, Clone)]
pub struct FolderDropTarget {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
    pub path: PathBuf,
}

impl DropTargetSnapshot {
    fn target_at(&self, point: &POINTL) -> Option<PathBuf> {
        self.folder_rows
            .iter()
            .find(|row| {
                point.x >= row.left
                    && point.x < row.right
                    && point.y >= row.top
                    && point.y < row.bottom
            })
            .map(|row| row.path.clone())
            .or_else(|| self.current.clone())
    }
}

type SharedTarget = Arc<Mutex<DropTargetSnapshot>>;
static LIVE_STATE: OnceLock<Mutex<Weak<Mutex<DragDropState>>>> = OnceLock::new();

pub fn current_state() -> DragDropState {
    LIVE_STATE
        .get()
        .and_then(|state| state.lock().ok()?.upgrade())
        .and_then(|state| state.lock().ok().map(|state| state.clone()))
        .unwrap_or_default()
}

#[derive(Default)]
struct DragContext {
    paths: Vec<PathBuf>,
    effect: DropEffect,
    allowed_effects: u32,
    right_button: bool,
}

#[implement(IDropTarget)]
struct NativeDropTarget {
    state: SharedState,
    target: SharedTarget,
    context: Mutex<DragContext>,
    intents: mpsc::Sender<DropIntent>,
}

impl NativeDropTarget {
    fn update(
        &self,
        event: DragDropEvent,
        paths: &[PathBuf],
        target: Option<&Path>,
        effect: DropEffect,
        reason: Option<&'static str>,
        point: Option<&POINTL>,
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.record(event);
            state.source_count = paths.len();
            state.target = target.map(|path| path.as_os_str().to_string_lossy().into_owned());
            state.negotiated_effect = effect.name();
            state.rejection_reason = reason;
            state.cursor_y = point.map(|point| point.y);
        }
    }

    fn target(&self, point: &POINTL) -> Option<PathBuf> {
        self.target
            .lock()
            .ok()
            .and_then(|target| target.target_at(point))
    }
}

#[allow(non_snake_case)]
impl IDropTarget_Impl for NativeDropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        native_effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = data
            .as_ref()
            .map(read_drop_paths)
            .transpose()
            .map_err(windows::core::Error::from)?
            .unwrap_or_default();
        let target = self.target(_point);
        let offered = unsafe { native_effect.as_ref() }
            .copied()
            .unwrap_or(DROPEFFECT_NONE);
        let (effect, reason) = negotiate_effect(&paths, target.as_deref(), key_state.0);
        set_native_effect(native_effect, effect);
        if let Ok(mut context) = self.context.lock() {
            context.paths = paths.clone();
            context.effect = effect;
            context.allowed_effects = allowed_effects(offered);
            context.right_button = key_state.0 & MK_RBUTTON.0 != 0;
        }
        self.update(
            DragDropEvent::Entered,
            &paths,
            target.as_deref(),
            effect,
            reason,
            Some(_point),
        );
        Ok(())
    }

    fn DragOver(
        &self,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        native_effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let target = self.target(_point);
        let paths = self
            .context
            .lock()
            .map(|context| context.paths.clone())
            .unwrap_or_default();
        let (effect, reason) = negotiate_effect(&paths, target.as_deref(), key_state.0);
        set_native_effect(native_effect, effect);
        if let Ok(mut context) = self.context.lock() {
            context.effect = effect;
            context.right_button |= key_state.0 & MK_RBUTTON.0 != 0;
        }
        self.update(
            DragDropEvent::Moved,
            &paths,
            target.as_deref(),
            effect,
            reason,
            Some(_point),
        );
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Ok(mut context) = self.context.lock() {
            *context = DragContext::default();
        }
        self.update(DragDropEvent::Left, &[], None, DropEffect::None, None, None);
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<IDataObject>,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        native_effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let paths = data
            .as_ref()
            .map(read_drop_paths)
            .transpose()
            .map_err(windows::core::Error::from)?
            .unwrap_or_default();
        let target = self.target(_point);
        let offered = unsafe { native_effect.as_ref() }
            .copied()
            .unwrap_or(DROPEFFECT_NONE);
        let (effect, reason) = negotiate_effect(&paths, target.as_deref(), key_state.0);
        set_native_effect(native_effect, effect);
        if let (Some(target), None) = (target.clone(), reason) {
            let (allowed_effects, tracked_right_button) = self
                .context
                .lock()
                .map(|context| (context.allowed_effects, context.right_button))
                .unwrap_or_else(|_| (allowed_effects(offered), false));
            let _ = self.intents.send(DropIntent {
                paths: paths.clone(),
                target,
                effect,
                right_button: tracked_right_button || key_state.0 & MK_RBUTTON.0 != 0,
                screen_x: _point.x,
                screen_y: _point.y,
                allowed_effects,
            });
        }
        self.update(
            DragDropEvent::Dropped,
            &paths,
            target.as_deref(),
            effect,
            reason,
            Some(_point),
        );
        if let Ok(mut context) = self.context.lock() {
            *context = DragContext::default();
        }
        Ok(())
    }
}

fn set_native_effect(output: *mut DROPEFFECT, effect: DropEffect) {
    if let Some(output) = unsafe { output.as_mut() } {
        *output = effect.native();
    }
}

pub fn allowed_effects(effect: DROPEFFECT) -> u32 {
    let mut allowed = 0;
    if effect.0 & DROPEFFECT_COPY.0 != 0 {
        allowed |= ALLOW_COPY;
    }
    if effect.0 & DROPEFFECT_MOVE.0 != 0 {
        allowed |= ALLOW_MOVE;
    }
    if effect.0 & DROPEFFECT_LINK.0 != 0 {
        allowed |= ALLOW_LINK;
    }
    allowed
}
pub fn negotiate_effect(
    paths: &[PathBuf],
    target: Option<&Path>,
    key_state: u32,
) -> (DropEffect, Option<&'static str>) {
    let Some(target) = target else {
        return (DropEffect::None, Some("target_unavailable"));
    };
    if paths.is_empty() {
        return (DropEffect::None, Some("unsupported_data"));
    }
    if paths
        .iter()
        .any(|path| path == target || path.parent() == Some(target))
    {
        return (DropEffect::None, Some("same_location"));
    }
    if paths.iter().any(|path| target.starts_with(path)) {
        return (DropEffect::None, Some("source_or_descendant"));
    }
    if key_state & MK_ALT != 0
        || key_state & (MK_CONTROL.0 | MK_SHIFT.0) == (MK_CONTROL.0 | MK_SHIFT.0)
    {
        return (DropEffect::Link, None);
    }
    if key_state & MK_CONTROL.0 != 0 {
        return (DropEffect::Copy, None);
    }
    if key_state & MK_SHIFT.0 != 0 {
        return (DropEffect::Move, None);
    }
    if paths.iter().all(|path| same_volume(path, target)) {
        (DropEffect::Move, None)
    } else {
        (DropEffect::Copy, None)
    }
}

fn same_volume(source: &Path, target: &Path) -> bool {
    volume_identity(source)
        .zip(volume_identity(target))
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(&right))
}

fn volume_identity(path: &Path) -> Option<String> {
    let value = path.as_os_str().to_string_lossy().replace('/', "\\");
    if let Some(value) = value.strip_prefix("\\\\") {
        let mut parts = value.split('\\');
        return Some(format!("\\\\{}\\{}", parts.next()?, parts.next()?));
    }
    value
        .get(..2)
        .filter(|value| value.ends_with(':'))
        .map(str::to_owned)
}

fn read_drop_paths(data: &IDataObject) -> io::Result<Vec<PathBuf>> {
    let format = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = unsafe { data.GetData(&format) }.map_err(windows_error)?;
    let result = read_hdrop(unsafe { HDROP(medium.u.hGlobal.0) });
    unsafe { ReleaseStgMedium(&mut medium) };
    result
}

fn read_hdrop(drop: HDROP) -> io::Result<Vec<PathBuf>> {
    let count = unsafe { DragQueryFileW(drop, u32::MAX, None) };
    let mut paths = Vec::with_capacity(count as usize);
    for index in 0..count {
        let length = unsafe { DragQueryFileW(drop, index, None) };
        let mut buffer = vec![0_u16; length as usize + 1];
        let copied = unsafe { DragQueryFileW(drop, index, Some(&mut buffer)) };
        if copied != length {
            return Err(io::Error::other("unable to read CF_HDROP path"));
        }
        paths.push(PathBuf::from(OsString::from_wide(
            &buffer[..length as usize],
        )));
    }
    Ok(paths)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutboundDropResult {
    pub effect: DropEffect,
    pub dropped: bool,
    pub performed_effect_reported: bool,
}

pub fn begin_outbound_drag(
    paths: &[PathBuf],
    preferred_effect: DropEffect,
) -> io::Result<OutboundDropResult> {
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "outbound drag file list is empty",
        ));
    }
    if preferred_effect == DropEffect::None {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "outbound drag preferred effect is unavailable",
        ));
    }

    let _ole = OleApartment::initialize()?;
    let preferred_format = clipboard_format("Preferred DropEffect")?;
    let performed_format = clipboard_format("Performed DropEffect")?;
    let performed_effect = Arc::new(Mutex::new(None));
    let data_object = IDataObject::from(OutboundDataObject {
        formats: vec![
            (CF_HDROP, encode_dropfiles(paths)?),
            (
                preferred_format,
                preferred_effect.native().0.to_ne_bytes().to_vec(),
            ),
        ],
        performed_format,
        performed_effect: performed_effect.clone(),
    });
    let drop_source = IDropSource::from(NativeDropSource);
    let allowed = DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0);
    let mut native_effect = DROPEFFECT_NONE;
    let result = unsafe { DoDragDrop(&data_object, &drop_source, allowed, &mut native_effect) };
    if result.is_err() {
        return Err(windows_error(WindowsError::from(result)));
    }

    let reported = performed_effect.lock().ok().and_then(|effect| *effect);
    let dropped = result == DRAGDROP_S_DROP;
    Ok(OutboundDropResult {
        effect: if dropped {
            reported.unwrap_or_else(|| DropEffect::from_native(native_effect))
        } else {
            DropEffect::None
        },
        dropped,
        performed_effect_reported: reported.is_some(),
    })
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

#[implement(IDropSource)]
struct NativeDropSource;

#[allow(non_snake_case)]
impl IDropSource_Impl for NativeDropSource_Impl {
    fn QueryContinueDrag(
        &self,
        escape_pressed: windows::core::BOOL,
        key_state: MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        query_continue_drag(escape_pressed.as_bool(), key_state.0)
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

fn query_continue_drag(escape_pressed: bool, key_state: u32) -> HRESULT {
    if escape_pressed {
        DRAGDROP_S_CANCEL
    } else if key_state & (MK_LBUTTON.0 | MK_RBUTTON.0) == 0 {
        DRAGDROP_S_DROP
    } else {
        HRESULT(0)
    }
}

#[implement(IDataObject)]
struct OutboundDataObject {
    formats: Vec<(u16, Vec<u8>)>,
    performed_format: u16,
    performed_effect: Arc<Mutex<Option<DropEffect>>>,
}

#[allow(non_snake_case)]
impl IDataObject_Impl for OutboundDataObject_Impl {
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
        format: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        let format = unsafe { format.as_ref() }.ok_or_else(|| WindowsError::from(E_NOTIMPL))?;
        let medium = unsafe { medium.as_ref() }.ok_or_else(|| WindowsError::from(E_NOTIMPL))?;
        if !supports_format(format, self.performed_format) {
            return Err(DV_E_FORMATETC.into());
        }
        let effect = read_effect_medium(medium)?;
        if let Ok(mut performed) = self.performed_effect.lock() {
            *performed = Some(effect);
        }
        if release.as_bool() {
            let mut owned = medium.clone();
            unsafe { ReleaseStgMedium(&mut owned) };
        }
        Ok(())
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
        _sink: Ref<IAdviseSink>,
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

fn encode_dropfiles(paths: &[PathBuf]) -> io::Result<Vec<u8>> {
    let mut names = Vec::<u16>::new();
    for path in paths {
        validate_drag_path(path)?;
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

fn validate_drag_path(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() || path.as_os_str().encode_wide().any(|unit| unit == 0) {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid outbound drag path",
        ))
    } else {
        Ok(())
    }
}

fn clipboard_format(name: &str) -> io::Result<u16> {
    let name = format!("{name}\0").encode_utf16().collect::<Vec<_>>();
    let format = unsafe { RegisterClipboardFormatW(PCWSTR(name.as_ptr())) };
    if format == 0 {
        Err(io::Error::last_os_error())
    } else {
        u16::try_from(format).map_err(|_| io::Error::last_os_error())
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

fn supports_format(format: &FORMATETC, expected: u16) -> bool {
    format.cfFormat == expected
        && format.dwAspect == DVASPECT_CONTENT.0
        && format.lindex == -1
        && format.tymed & TYMED_HGLOBAL.0 as u32 != 0
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

fn read_effect_medium(medium: &STGMEDIUM) -> windows::core::Result<DropEffect> {
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        return Err(DV_E_FORMATETC.into());
    }
    let handle = unsafe { medium.u.hGlobal };
    if handle.is_invalid() || unsafe { GlobalSize(handle) } < size_of::<u32>() {
        return Err(DV_E_FORMATETC.into());
    }
    let pointer = unsafe { GlobalLock(handle) }.cast::<u32>();
    if pointer.is_null() {
        return Err(WindowsError::from_thread());
    }
    let effect = DropEffect::from_native(DROPEFFECT(unsafe { pointer.read_unaligned() }));
    let _ = unsafe { GlobalUnlock(handle) };
    Ok(effect)
}

pub fn client_screen_rect(hwnd: isize) -> io::Result<(i32, i32, i32, i32)> {
    if hwnd == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "window handle is unavailable",
        ));
    }
    let mut rect = RECT::default();
    let mut origin = RawPoint::default();
    let hwnd = hwnd as RawHwnd;
    if unsafe { GetClientRect(hwnd, &mut rect) } == 0
        || unsafe { ClientToScreen(hwnd, &mut origin) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok((
        origin.x,
        origin.y,
        origin.x + rect.right,
        origin.y + rect.bottom,
    ))
}

thread_local! {
    static REGISTRATION: std::cell::RefCell<Option<DragDropRegistration>> = const { std::cell::RefCell::new(None) };
}

pub fn register_current(
    hwnd: isize,
    target: SharedTarget,
    intents: mpsc::Sender<DropIntent>,
) -> io::Result<()> {
    REGISTRATION.with(|registration| {
        let mut registration = registration.borrow_mut();
        if registration.is_none() {
            *registration = Some(DragDropRegistration::register(hwnd, target, intents)?);
        }
        Ok(())
    })
}

pub fn revoke_current() {
    REGISTRATION.with(|registration| {
        registration.borrow_mut().take();
    });
}

pub struct DragDropRegistration {
    hwnd: HWND,
    target: Option<IDropTarget>,
    state: SharedState,
}

impl DragDropRegistration {
    fn register(
        hwnd: isize,
        current_target: SharedTarget,
        intents: mpsc::Sender<DropIntent>,
    ) -> io::Result<Self> {
        if hwnd == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "main window handle is not available",
            ));
        }
        unsafe { OleInitialize(None) }.map_err(windows_error)?;
        let state = Arc::new(Mutex::new(DragDropState::default()));
        let target = IDropTarget::from(NativeDropTarget {
            state: state.clone(),
            target: current_target,
            context: Mutex::new(DragContext::default()),
            intents,
        });
        let hwnd = HWND(hwnd as *mut c_void);
        if let Err(error) = unsafe { RegisterDragDrop(hwnd, &target) } {
            unsafe { OleUninitialize() };
            return Err(windows_error(error));
        }
        if let Ok(mut current) = state.lock() {
            current.record(DragDropEvent::Registered);
        }
        if let Ok(mut live) = LIVE_STATE.get_or_init(Default::default).lock() {
            *live = Arc::downgrade(&state);
        }
        Ok(Self {
            hwnd,
            target: Some(target),
            state,
        })
    }
}

impl Drop for DragDropRegistration {
    fn drop(&mut self) {
        let _ = unsafe { RevokeDragDrop(self.hwnd) };
        if let Ok(mut state) = self.state.lock() {
            state.record(DragDropEvent::Revoked);
        }
        self.target.take();
        if let Some(live) = LIVE_STATE.get()
            && let Ok(mut live) = live.lock()
        {
            *live = Weak::new();
        }
        unsafe { OleUninitialize() };
    }
}

fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_dropfiles_preserve_unicode_long_and_unc_paths() {
        let paths = vec![
            PathBuf::from(r"C:\资料\一.txt"),
            PathBuf::from(format!(r"C:\{}\long.txt", "segment\\".repeat(40))),
            PathBuf::from(r"\\server\share\folder"),
        ];
        let bytes = encode_dropfiles(&paths).unwrap();
        let header = unsafe { bytes.as_ptr().cast::<DROPFILES>().read_unaligned() };

        assert_eq!(header.pFiles as usize, size_of::<DROPFILES>());
        assert!(header.fWide.as_bool());
        assert!(bytes.ends_with(&[0, 0, 0, 0]));
        assert!(bytes.len() > 520);
    }

    #[test]
    fn outbound_data_formats_match_explorer_contract() {
        let preferred_format = 0xC123;
        let performed_format = 0xC124;

        assert!(supports_format(&format_etc(CF_HDROP), CF_HDROP));
        assert!(supports_format(
            &format_etc(preferred_format),
            preferred_format
        ));
        assert!(supports_format(
            &format_etc(performed_format),
            performed_format
        ));
        assert!(!supports_format(&format_etc(99), CF_HDROP));
    }

    #[test]
    fn inbound_allowed_effects_preserve_source_mask() {
        assert_eq!(allowed_effects(DROPEFFECT_NONE), 0);
        assert_eq!(allowed_effects(DROPEFFECT_COPY), ALLOW_COPY);
        assert_eq!(allowed_effects(DROPEFFECT_MOVE), ALLOW_MOVE);
        assert_eq!(allowed_effects(DROPEFFECT_LINK), ALLOW_LINK);
        assert_eq!(
            allowed_effects(DROPEFFECT(
                DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0 | DROPEFFECT_LINK.0
            )),
            ALLOW_COPY | ALLOW_MOVE | ALLOW_LINK
        );
    }

    #[test]
    fn outbound_effect_conversion_prefers_move_copy_then_link() {
        assert_eq!(DropEffect::from_native(DROPEFFECT_MOVE), DropEffect::Move);
        assert_eq!(DropEffect::from_native(DROPEFFECT_COPY), DropEffect::Copy);
        assert_eq!(DropEffect::from_native(DROPEFFECT_LINK), DropEffect::Link);
        assert_eq!(DropEffect::from_native(DROPEFFECT_NONE), DropEffect::None);
        assert_eq!(
            DropEffect::from_native(DROPEFFECT(DROPEFFECT_COPY.0 | DROPEFFECT_MOVE.0)),
            DropEffect::Move
        );
    }

    #[test]
    fn outbound_drop_source_cancels_drops_and_continues_by_mouse_state() {
        assert_eq!(query_continue_drag(true, MK_LBUTTON.0), DRAGDROP_S_CANCEL);
        assert_eq!(query_continue_drag(false, 0), DRAGDROP_S_DROP);
        assert_eq!(query_continue_drag(false, MK_LBUTTON.0), HRESULT(0));
        assert_eq!(query_continue_drag(false, MK_RBUTTON.0), HRESULT(0));
    }

    #[test]
    fn folder_row_hit_testing_prefers_folder_over_current_directory() {
        let snapshot = DropTargetSnapshot {
            current: Some(PathBuf::from(r"C:\Current")),
            folder_rows: vec![FolderDropTarget {
                left: 10,
                top: 20,
                right: 110,
                bottom: 60,
                path: PathBuf::from(r"C:\Current\Child"),
            }],
        };

        assert_eq!(
            snapshot.target_at(&POINTL { x: 50, y: 40 }),
            Some(PathBuf::from(r"C:\Current\Child"))
        );
        assert_eq!(
            snapshot.target_at(&POINTL { x: 5, y: 5 }),
            Some(PathBuf::from(r"C:\Current"))
        );
    }

    #[test]
    fn effect_negotiation_matches_windows_defaults_and_modifiers() {
        let target = Path::new(r"C:\Target");
        let same = vec![PathBuf::from(r"C:\Source\one.txt")];
        let other = vec![PathBuf::from(r"D:\Source\one.txt")];

        assert_eq!(
            negotiate_effect(&same, Some(target), 0),
            (DropEffect::Move, None)
        );
        assert_eq!(
            negotiate_effect(&other, Some(target), 0),
            (DropEffect::Copy, None)
        );
        assert_eq!(
            negotiate_effect(&same, Some(target), MK_CONTROL.0),
            (DropEffect::Copy, None)
        );
        assert_eq!(
            negotiate_effect(&other, Some(target), MK_SHIFT.0),
            (DropEffect::Move, None)
        );
    }

    #[test]
    fn effect_negotiation_rejects_unsafe_or_unsupported_targets() {
        let source = vec![PathBuf::from(r"C:\Source")];
        assert_eq!(
            negotiate_effect(&source, Some(Path::new(r"C:\Source\Child")), 0),
            (DropEffect::None, Some("source_or_descendant"))
        );
        assert_eq!(
            negotiate_effect(&source, None, 0),
            (DropEffect::None, Some("target_unavailable"))
        );
        assert_eq!(
            negotiate_effect(&source, Some(Path::new(r"C:\Target")), MK_ALT),
            (DropEffect::Link, None)
        );
    }

    #[test]
    fn unc_volume_identity_uses_server_and_share() {
        assert!(same_volume(
            Path::new(r"\\server\share\one"),
            Path::new(r"\\SERVER\SHARE\two")
        ));
        assert!(!same_volume(
            Path::new(r"\\server\share\one"),
            Path::new(r"\\server\other\two")
        ));
    }
}
