use std::{
    ffi::{OsString, c_void},
    io,
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, Weak, mpsc},
};

use windows::{
    Win32::{
        Foundation::{HWND, POINTL},
        System::{
            Com::{DVASPECT_CONTENT, FORMATETC, IDataObject, TYMED_HGLOBAL},
            Ole::{
                DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_MOVE, DROPEFFECT_NONE, IDropTarget,
                IDropTarget_Impl, OleInitialize, OleUninitialize, RegisterDragDrop,
                ReleaseStgMedium, RevokeDragDrop,
            },
            SystemServices::{MK_CONTROL, MK_SHIFT, MODIFIERKEYS_FLAGS},
        },
        UI::Shell::{DragQueryFileW, HDROP},
    },
    core::{Ref, implement},
};

const CF_HDROP: u16 = 15;
const MK_ALT: u32 = 0x20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DropEffect {
    #[default]
    None,
    Copy,
    Move,
}

impl DropEffect {
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Copy => "copy",
            Self::Move => "move",
        }
    }

    fn native(self) -> DROPEFFECT {
        match self {
            Self::None => DROPEFFECT_NONE,
            Self::Copy => DROPEFFECT_COPY,
            Self::Move => DROPEFFECT_MOVE,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropIntent {
    pub paths: Vec<PathBuf>,
    pub target: PathBuf,
    pub effect: DropEffect,
}

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
type SharedTarget = Arc<Mutex<Option<PathBuf>>>;
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
    ) {
        if let Ok(mut state) = self.state.lock() {
            state.record(event);
            state.source_count = paths.len();
            state.target = target.map(|path| path.as_os_str().to_string_lossy().into_owned());
            state.negotiated_effect = effect.name();
            state.rejection_reason = reason;
        }
    }

    fn target(&self) -> Option<PathBuf> {
        self.target.lock().ok().and_then(|target| target.clone())
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
        let target = self.target();
        let (effect, reason) = negotiate_effect(&paths, target.as_deref(), key_state.0);
        set_native_effect(native_effect, effect);
        if let Ok(mut context) = self.context.lock() {
            context.paths = paths.clone();
            context.effect = effect;
        }
        self.update(
            DragDropEvent::Entered,
            &paths,
            target.as_deref(),
            effect,
            reason,
        );
        Ok(())
    }

    fn DragOver(
        &self,
        key_state: MODIFIERKEYS_FLAGS,
        _point: &POINTL,
        native_effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let target = self.target();
        let paths = self
            .context
            .lock()
            .map(|context| context.paths.clone())
            .unwrap_or_default();
        let (effect, reason) = negotiate_effect(&paths, target.as_deref(), key_state.0);
        set_native_effect(native_effect, effect);
        if let Ok(mut context) = self.context.lock() {
            context.effect = effect;
        }
        self.update(
            DragDropEvent::Moved,
            &paths,
            target.as_deref(),
            effect,
            reason,
        );
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        if let Ok(mut context) = self.context.lock() {
            *context = DragContext::default();
        }
        self.update(DragDropEvent::Left, &[], None, DropEffect::None, None);
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
        let target = self.target();
        let (effect, reason) = negotiate_effect(&paths, target.as_deref(), key_state.0);
        set_native_effect(native_effect, effect);
        if let (Some(target), None) = (target.clone(), reason) {
            let _ = self.intents.send(DropIntent {
                paths: paths.clone(),
                target,
                effect,
            });
        }
        self.update(
            DragDropEvent::Dropped,
            &paths,
            target.as_deref(),
            effect,
            reason,
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
        return (DropEffect::None, Some("link_pending_p3_d4"));
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
            (DropEffect::None, Some("link_pending_p3_d4"))
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
