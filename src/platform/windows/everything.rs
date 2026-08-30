use std::{
    ffi::{OsStr, OsString, c_void},
    fmt,
    mem::zeroed,
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
    process::Command,
    ptr,
    sync::{Mutex, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::{
        CloseHandle, ERROR_BROKEN_PIPE, ERROR_IO_PENDING, ERROR_NO_DATA, ERROR_OPERATION_ABORTED,
        ERROR_PIPE_BUSY, ERROR_SUCCESS, GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, HWND,
        INVALID_HANDLE_VALUE, LPARAM, LRESULT, WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
    },
    Storage::FileSystem::{
        CreateFileW, FILE_FLAG_OVERLAPPED, GetDriveTypeW, OPEN_EXISTING, ReadFile, WriteFile,
    },
    System::{
        DataExchange::COPYDATASTRUCT,
        IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED},
        Pipes::WaitNamedPipeW,
        Registry::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW},
        Threading::{CreateEventW, WaitForSingleObject},
    },
    UI::WindowsAndMessaging::{
        CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW,
        EnumWindows, FindWindowW, GWLP_USERDATA, GetClassNameW, GetMessageW, GetWindowLongPtrW,
        HWND_MESSAGE, KillTimer, MSG, PostQuitMessage, RegisterClassExW, SMTO_ABORTIFHUNG,
        SendMessageTimeoutW, SetTimer, SetWindowLongPtrW, TranslateMessage, WM_COPYDATA,
        WM_NCCREATE, WM_TIMER, WM_USER, WNDCLASSEXW,
    },
};
const IPC_CLASS: &str = "EVERYTHING_TASKBAR_NOTIFICATION";
const REPLY_CLASS: &str = "AsterFiles_Everything_IPC_Reply";
const QUERY2: usize = 18;
const REPLY: usize = 0x41535445;
const HEADER: usize = 28;
const LIST_HEADER: usize = 20;
const ITEM_SIZE: usize = 8;
const REQ_NAME: u32 = 1;
const REQ_PATH: u32 = 2;
const REQ_FULL: u32 = 4;
const REQ_SIZE: u32 = 0x10;
const REQ_MODIFIED: u32 = 0x40;
const REQ_ATTRIBUTES: u32 = 0x100;
const REQ_HIGHLIGHTED_NAME: u32 = 0x2000;
const EVERYTHING3_FOLDER_SIZE_COMMAND: u32 = 18;
const EVERYTHING3_RESPONSE_OK_MORE_DATA: u32 = 100;
const EVERYTHING3_RESPONSE_OK: u32 = 200;
const EVERYTHING3_RESPONSE_BAD_REQUEST: u32 = 400;
const EVERYTHING3_RESPONSE_CANCELLED: u32 = 401;
const EVERYTHING3_RESPONSE_NOT_FOUND: u32 = 404;
const EVERYTHING3_RESPONSE_OUT_OF_MEMORY: u32 = 500;
const EVERYTHING3_RESPONSE_INVALID_COMMAND: u32 = 501;
const EVERYTHING3_UINT64_MAX: u64 = u64::MAX;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlatformEverythingConfig {
    pub executable_path: PathBuf,
    pub instance_name: String,
    pub allow_start: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingInstallation {
    pub executable_path: PathBuf,
    pub instance_name: String,
    pub running: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EverythingVersion {
    pub major: u32,
    pub minor: u32,
    pub revision: u32,
    pub build: u32,
}
impl fmt::Display for EverythingVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{}.{}.{}",
            self.major, self.minor, self.revision, self.build
        )
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingStatus {
    pub version: EverythingVersion,
    pub instance_name: String,
    pub database_loaded: bool,
    pub folder_size_indexed: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)]
pub enum EverythingSort {
    #[default]
    NameAscending,
    NameDescending,
    PathAscending,
    PathDescending,
    ExtensionAscending,
    ExtensionDescending,
    SizeAscending,
    SizeDescending,
    ModifiedAscending,
    ModifiedDescending,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EverythingItemKind {
    #[default]
    Any,
    Files,
    Folders,
}
impl EverythingSort {
    fn ipc(self) -> u32 {
        match self {
            Self::NameAscending => 1,
            Self::NameDescending => 2,
            Self::PathAscending => 3,
            Self::PathDescending => 4,
            Self::ExtensionAscending => 9,
            Self::ExtensionDescending => 10,
            Self::SizeAscending => 5,
            Self::SizeDescending => 6,
            Self::ModifiedAscending => 13,
            Self::ModifiedDescending => 14,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingSearchRequest {
    pub query: String,
    pub scope: Option<PathBuf>,
    pub offset: u32,
    pub max_results: u32,
    pub sort: EverythingSort,
    pub item_kind: EverythingItemKind,
}
impl EverythingSearchRequest {
    pub fn new(query: impl Into<String>, scope: Option<PathBuf>) -> Self {
        Self {
            query: query.into(),
            scope,
            offset: 0,
            max_results: 256,
            sort: Default::default(),
            item_kind: Default::default(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingSearchPage {
    pub total: u32,
    pub offset: u32,
    pub items: Vec<EverythingSearchItem>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingSearchItem {
    pub path: PathBuf,
    pub name: OsString,
    pub parent: PathBuf,
    pub size: Option<u64>,
    pub modified: Option<SystemTime>,
    pub is_directory: bool,
    pub name_highlights: Vec<EverythingHighlightSegment>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EverythingHighlightSegment {
    pub text: String,
    pub highlighted: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EverythingFolderSize {
    Indexed(u64),
    NotIndexed,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EverythingError {
    NotConfigured,
    InvalidExecutable(PathBuf),
    NotRunning(String),
    Timeout,
    UnsupportedVersion(EverythingVersion),
    UnsupportedArchitecture,
    DatabaseNotLoaded,
    QueryRejected,
    FolderSizePipeUnavailable(String),
    FolderSizeDisconnected,
    FolderSizeRejected(u32),
    Protocol(String),
    StartFailed(String),
    Windows(u32),
}
impl fmt::Display for EverythingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConfigured => write!(f, "Everything is not configured"),
            Self::InvalidExecutable(p) => {
                write!(f, "invalid Everything executable: {}", p.display())
            }
            Self::NotRunning(i) => write!(f, "Everything instance is not running: {i}"),
            Self::Timeout => write!(f, "Everything IPC timed out"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported Everything version: {v}"),
            Self::UnsupportedArchitecture => write!(f, "Everything x64 is required"),
            Self::DatabaseNotLoaded => write!(f, "Everything database is not loaded"),
            Self::QueryRejected => write!(f, "Everything rejected the query"),
            Self::FolderSizePipeUnavailable(i) => {
                write!(f, "Everything folder-size IPC pipe is unavailable: {i}")
            }
            Self::FolderSizeDisconnected => {
                write!(f, "Everything folder-size IPC pipe disconnected")
            }
            Self::FolderSizeRejected(code) => {
                write!(f, "Everything folder-size IPC rejected the request: {code}")
            }
            Self::Protocol(m) => write!(f, "invalid Everything response: {m}"),
            Self::StartFailed(m) => write!(f, "failed to start Everything: {m}"),
            Self::Windows(c) => write!(f, "Windows error {c}"),
        }
    }
}
impl std::error::Error for EverythingError {}
#[derive(Debug)]
pub struct EverythingClient {
    config: PlatformEverythingConfig,
    folder_size_pipe: Mutex<Option<Everything3Pipe>>,
}

impl Clone for EverythingClient {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            folder_size_pipe: Mutex::new(None),
        }
    }
}
impl EverythingClient {
    pub fn new(config: PlatformEverythingConfig) -> Result<Self, EverythingError> {
        if config.instance_name.contains(['\0', '(', ')']) {
            Err(EverythingError::NotConfigured)
        } else {
            Ok(Self {
                config,
                folder_size_pipe: Mutex::new(None),
            })
        }
    }
    #[allow(dead_code)]
    pub fn config(&self) -> &PlatformEverythingConfig {
        &self.config
    }
    pub fn discover() -> Vec<EverythingInstallation> {
        let running = running_instances();
        let mut found = registry_installations();
        for instance in running {
            if let Some(item) = found
                .iter_mut()
                .find(|x| x.instance_name.eq_ignore_ascii_case(&instance))
            {
                item.running = true
            } else {
                found.push(EverythingInstallation {
                    executable_path: PathBuf::new(),
                    instance_name: instance,
                    running: true,
                })
            }
        }
        for path in common_paths() {
            if path.is_file() && !found.iter().any(|x| x.executable_path == path) {
                found.push(EverythingInstallation {
                    executable_path: path,
                    instance_name: String::new(),
                    running: false,
                })
            }
        }
        found
    }
    pub fn status(&self, timeout: Duration) -> Result<EverythingStatus, EverythingError> {
        let w = self.window()?;
        let version = EverythingVersion {
            major: send(w, 0, 0, timeout)? as u32,
            minor: send(w, 1, 0, timeout)? as u32,
            revision: send(w, 2, 0, timeout)? as u32,
            build: send(w, 3, 0, timeout)? as u32,
        };
        if version.major != 1 || version.minor < 5 {
            return Err(EverythingError::UnsupportedVersion(version));
        }
        if send(w, 5, 0, timeout)? != 2 {
            return Err(EverythingError::UnsupportedArchitecture);
        }
        Ok(EverythingStatus {
            version,
            instance_name: self.config.instance_name.clone(),
            database_loaded: send(w, 401, 0, timeout)? != 0,
            folder_size_indexed: send(w, 411, 2, timeout)? != 0,
        })
    }
    pub fn start(&self) -> Result<(), EverythingError> {
        if !self.config.allow_start {
            return Err(EverythingError::StartFailed(
                "starting Everything is disabled".into(),
            ));
        }
        if !self.config.executable_path.is_file() {
            return Err(EverythingError::InvalidExecutable(
                self.config.executable_path.clone(),
            ));
        }
        let mut c = Command::new(&self.config.executable_path);
        if !self.config.instance_name.is_empty() {
            c.arg("-instance").arg(&self.config.instance_name);
        }
        c.arg("-startup")
            .spawn()
            .map(|_| ())
            .map_err(|e| EverythingError::StartFailed(e.to_string()))
    }
    pub fn search(
        &self,
        r: &EverythingSearchRequest,
        timeout: Duration,
    ) -> Result<EverythingSearchPage, EverythingError> {
        if !self.status(timeout)?.database_loaded {
            return Err(EverythingError::DatabaseNotLoaded);
        }
        query_ipc(
            self.window()?,
            &compose_typed_search(r.scope.as_deref(), &r.query, r.item_kind),
            r.offset,
            r.max_results.min(4096),
            r.sort,
            timeout,
        )
    }
    pub fn folder_size(
        &self,
        path: &Path,
        timeout: Duration,
    ) -> Result<EverythingFolderSize, EverythingError> {
        let deadline = std::time::Instant::now() + timeout;
        let status = self.status(remaining(deadline)?)?;
        if !status.database_loaded || !status.folder_size_indexed {
            return Ok(EverythingFolderSize::NotIndexed);
        }
        let query_path = normalize_folder_size_path(path)?;
        let first = self.query_folder_size(&query_path, remaining(deadline)?)?;
        if !matches!(
            first,
            EverythingFolderSize::Indexed(0) | EverythingFolderSize::NotIndexed
        ) || is_network_path(path)
        {
            return Ok(first);
        }
        let Ok(resolved) = std::fs::canonicalize(path) else {
            return Ok(first);
        };
        let resolved_query = normalize_folder_size_path(&resolved)?;
        if resolved_query == query_path {
            return Ok(first);
        }
        self.query_folder_size(&resolved_query, remaining(deadline)?)
    }

    fn query_folder_size(
        &self,
        path: &[u8],
        timeout: Duration,
    ) -> Result<EverythingFolderSize, EverythingError> {
        let pipe_name = everything3_pipe_name(&self.config.instance_name);
        let mut connection = self
            .folder_size_pipe
            .lock()
            .map_err(|_| EverythingError::FolderSizeDisconnected)?;
        for attempt in 0..=1 {
            if connection.is_none() {
                *connection = Some(Everything3Pipe::connect(
                    &pipe_name,
                    &self.config.instance_name,
                    timeout,
                )?);
            }
            let result = connection.as_ref().unwrap().folder_size(path, timeout);
            if result.is_err() {
                *connection = None;
            }
            if matches!(result, Err(EverythingError::FolderSizeDisconnected)) && attempt == 0 {
                continue;
            }
            return result;
        }
        Err(EverythingError::FolderSizeDisconnected)
    }
    fn window(&self) -> Result<HWND, EverythingError> {
        let class = wide(instance_class(&self.config.instance_name));
        let w = unsafe { FindWindowW(class.as_ptr(), ptr::null()) };
        if w.is_null() {
            Err(EverythingError::NotRunning(
                self.config.instance_name.clone(),
            ))
        } else {
            Ok(w)
        }
    }
}
pub fn compose_search(scope: Option<&Path>, user_query: &str) -> String {
    let q = user_query.trim();
    match scope {
        Some(path) => {
            let scope = format!(
                "ancestor:{}",
                encode_function_argument(&path.as_os_str().to_string_lossy())
            );
            if q.is_empty() {
                scope
            } else {
                format!("<{scope}> {q}")
            }
        }
        None => q.to_owned(),
    }
}

fn compose_typed_search(
    scope: Option<&Path>,
    user_query: &str,
    item_kind: EverythingItemKind,
) -> String {
    let base = compose_search(scope, user_query);
    let kind = match item_kind {
        EverythingItemKind::Any => return base,
        EverythingItemKind::Files => "file:",
        EverythingItemKind::Folders => "folder:",
    };
    if base.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind} <{base}>")
    }
}
fn encode_function_argument(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_alphanumeric() || matches!(character, '\\' | ':' | '.' | '_' | '-') {
            encoded.push(character);
        } else {
            encoded.push_str(&format!("#{}:", character as u32));
        }
    }
    encoded
}

fn instance_class(i: &str) -> String {
    if i.is_empty() {
        IPC_CLASS.into()
    } else {
        format!("{IPC_CLASS}_({i})")
    }
}
fn send(w: HWND, cmd: usize, arg: isize, timeout: Duration) -> Result<usize, EverythingError> {
    let mut result = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            w,
            WM_USER,
            cmd,
            arg,
            SMTO_ABORTIFHUNG,
            millis(timeout),
            &mut result,
        )
    };
    if ok == 0 {
        let code = unsafe { GetLastError() };
        if code == 1460 {
            Err(EverythingError::Timeout)
        } else {
            Err(EverythingError::Windows(code))
        }
    } else {
        Ok(result)
    }
}
fn query_ipc(
    target: HWND,
    q: &str,
    offset: u32,
    max: u32,
    sort: EverythingSort,
    timeout: Duration,
) -> Result<EverythingSearchPage, EverythingError> {
    register_reply()?;
    let mut state = ReplyState::default();
    let class = wide(REPLY_CLASS);
    let w = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            ptr::null_mut(),
            (&mut state as *mut ReplyState).cast(),
        )
    };
    if w.is_null() {
        return Err(last_error());
    }
    let flags = REQ_NAME
        | REQ_PATH
        | REQ_FULL
        | REQ_SIZE
        | REQ_MODIFIED
        | REQ_ATTRIBUTES
        | REQ_HIGHLIGHTED_NAME;
    let bytes = encode_query(w, q, offset, max, flags, sort.ipc());
    let cds = COPYDATASTRUCT {
        dwData: QUERY2,
        cbData: bytes.len() as u32,
        lpData: bytes.as_ptr().cast_mut().cast::<c_void>(),
    };
    let mut accepted = 0;
    let ok = unsafe {
        SendMessageTimeoutW(
            target,
            WM_COPYDATA,
            w as WPARAM,
            (&cds as *const COPYDATASTRUCT) as LPARAM,
            SMTO_ABORTIFHUNG,
            millis(timeout),
            &mut accepted,
        )
    };
    if ok == 0 || accepted == 0 {
        unsafe { DestroyWindow(w) };
        return if ok == 0 {
            Err(last_error())
        } else {
            Err(EverythingError::QueryRejected)
        };
    }
    let timer = unsafe { SetTimer(w, 1, millis(timeout), None) };
    if timer == 0 {
        unsafe { DestroyWindow(w) };
        return Err(last_error());
    }
    let mut msg = unsafe { zeroed::<MSG>() };
    while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    unsafe {
        KillTimer(w, timer);
        DestroyWindow(w);
    }
    state.result.unwrap_or(Err(EverythingError::Timeout))
}
#[derive(Default)]
struct ReplyState {
    result: Option<Result<EverythingSearchPage, EverythingError>>,
}
unsafe extern "system" fn reply_proc(w: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let c = unsafe { &*(lp as *const CREATESTRUCTW) };
        unsafe { SetWindowLongPtrW(w, GWLP_USERDATA, c.lpCreateParams as isize) };
        return 1;
    }
    let state = unsafe { GetWindowLongPtrW(w, GWLP_USERDATA) as *mut ReplyState };
    if msg == WM_COPYDATA && !state.is_null() {
        let cds = unsafe { &*(lp as *const COPYDATASTRUCT) };
        if cds.dwData == REPLY && !cds.lpData.is_null() {
            let bytes =
                unsafe { std::slice::from_raw_parts(cds.lpData.cast::<u8>(), cds.cbData as usize) };
            unsafe {
                (*state).result = Some(parse_results(bytes));
                PostQuitMessage(0)
            };
            return 1;
        }
    } else if msg == WM_TIMER && !state.is_null() {
        unsafe {
            (*state).result = Some(Err(EverythingError::Timeout));
            PostQuitMessage(0)
        };
        return 0;
    }
    unsafe { DefWindowProcW(w, msg, wp, lp) }
}
fn register_reply() -> Result<(), EverythingError> {
    static REG: OnceLock<Result<(), u32>> = OnceLock::new();
    (*REG.get_or_init(|| {
        let class = wide(REPLY_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(reply_proc),
            lpszClassName: class.as_ptr(),
            ..unsafe { zeroed() }
        };
        if unsafe { RegisterClassExW(&wc) } == 0 {
            let code = unsafe { GetLastError() };
            if code != 1410 {
                return Err(code);
            }
        }
        Ok(())
    }))
    .map_err(EverythingError::Windows)
}
fn encode_query(w: HWND, q: &str, offset: u32, max: u32, flags: u32, sort: u32) -> Vec<u8> {
    let text = q.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
    let mut b = vec![0; HEADER + text.len() * 2];
    for (o, v) in [
        (0, w as usize as u32),
        (4, REPLY as u32),
        (8, 0),
        (12, offset),
        (16, max),
        (20, flags),
        (24, sort),
    ] {
        put32(&mut b, o, v)
    }
    for (i, u) in text.into_iter().enumerate() {
        b[HEADER + i * 2..HEADER + i * 2 + 2].copy_from_slice(&u.to_le_bytes())
    }
    b
}
fn parse_results(b: &[u8]) -> Result<EverythingSearchPage, EverythingError> {
    if b.len() < LIST_HEADER {
        return protocol("truncated response header");
    }
    let total = get32(b, 0)?;
    let count = get32(b, 4)? as usize;
    let offset = get32(b, 8)?;
    let flags = get32(b, 12)?;
    let table = LIST_HEADER
        .checked_add(
            count
                .checked_mul(ITEM_SIZE)
                .ok_or_else(|| EverythingError::Protocol("item overflow".into()))?,
        )
        .ok_or_else(|| EverythingError::Protocol("table overflow".into()))?;
    if table > b.len() {
        return protocol("truncated item table");
    }
    let mut items = Vec::with_capacity(count);
    for i in 0..count {
        let o = LIST_HEADER + i * ITEM_SIZE;
        let item_flags = get32(b, o)?;
        let data = get32(b, o + 4)? as usize;
        if data < table || data > b.len() {
            return protocol("invalid item offset");
        }
        items.push(parse_item(b, data, flags, item_flags)?)
    }
    Ok(EverythingSearchPage {
        total,
        offset,
        items,
    })
}
fn parse_item(
    b: &[u8],
    mut c: usize,
    flags: u32,
    item_flags: u32,
) -> Result<EverythingSearchItem, EverythingError> {
    let mut name = None;
    let mut parent = None;
    let mut full = None;
    let mut size = None;
    let mut modified = None;
    let mut attributes = 0;
    let mut name_highlights = Vec::new();
    if flags & REQ_NAME != 0 {
        name = Some(get_wide(b, &mut c)?)
    }
    if flags & REQ_PATH != 0 {
        parent = Some(get_wide(b, &mut c)?)
    }
    if flags & REQ_FULL != 0 {
        full = Some(get_wide(b, &mut c)?)
    }
    if flags & REQ_SIZE != 0 {
        size = Some(get64a(b, &mut c)?)
    }
    if flags & REQ_MODIFIED != 0 {
        modified = filetime(get64a(b, &mut c)?)
    }
    if flags & REQ_ATTRIBUTES != 0 {
        attributes = get32a(b, &mut c)?
    }
    if flags & REQ_HIGHLIGHTED_NAME != 0 {
        let marked = get_wide(b, &mut c)?;
        name_highlights = parse_highlighted_name(&marked.to_string_lossy());
    }
    let path =
        PathBuf::from(full.ok_or_else(|| EverythingError::Protocol("missing full path".into()))?);
    Ok(EverythingSearchItem {
        name: name.unwrap_or_else(|| path.file_name().unwrap_or_default().to_os_string()),
        parent: parent
            .map(PathBuf::from)
            .unwrap_or_else(|| path.parent().unwrap_or(Path::new("")).into()),
        path,
        size,
        modified,
        is_directory: item_flags & 1 != 0 || attributes & 0x10 != 0,
        name_highlights,
    })
}
fn parse_highlighted_name(marked: &str) -> Vec<EverythingHighlightSegment> {
    let mut segments = Vec::new();
    let mut text = String::new();
    let mut highlighted = false;
    let mut characters = marked.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '*' {
            text.push(character);
            continue;
        }
        if characters.peek() == Some(&'*') {
            characters.next();
            text.push('*');
            continue;
        }
        if !text.is_empty() {
            segments.push(EverythingHighlightSegment {
                text: std::mem::take(&mut text),
                highlighted,
            });
        }
        highlighted = !highlighted;
    }
    if !text.is_empty() {
        segments.push(EverythingHighlightSegment { text, highlighted });
    }
    segments
}
fn get_wide(b: &[u8], c: &mut usize) -> Result<OsString, EverythingError> {
    let len = get32a(b, c)? as usize;
    let n = (len + 1)
        .checked_mul(2)
        .ok_or_else(|| EverythingError::Protocol("string overflow".into()))?;
    let end = c
        .checked_add(n)
        .ok_or_else(|| EverythingError::Protocol("offset overflow".into()))?;
    let data = b
        .get(*c..end)
        .ok_or_else(|| EverythingError::Protocol("truncated string".into()))?;
    if data[data.len() - 2..] != [0, 0] {
        return protocol("unterminated string");
    }
    let units = data[..len * 2]
        .chunks_exact(2)
        .map(|x| u16::from_le_bytes([x[0], x[1]]))
        .collect::<Vec<_>>();
    *c = end;
    Ok(OsString::from_wide(&units))
}
fn get32(b: &[u8], o: usize) -> Result<u32, EverythingError> {
    let x = b
        .get(o..o + 4)
        .ok_or_else(|| EverythingError::Protocol("truncated u32".into()))?;
    Ok(u32::from_le_bytes(x.try_into().unwrap()))
}
fn get32a(b: &[u8], c: &mut usize) -> Result<u32, EverythingError> {
    let v = get32(b, *c)?;
    *c += 4;
    Ok(v)
}
fn get64a(b: &[u8], c: &mut usize) -> Result<u64, EverythingError> {
    let x = b
        .get(*c..*c + 8)
        .ok_or_else(|| EverythingError::Protocol("truncated u64".into()))?;
    *c += 8;
    Ok(u64::from_le_bytes(x.try_into().unwrap()))
}
fn put32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes())
}
#[derive(Debug)]
struct Everything3Pipe {
    handle: HANDLE,
}

unsafe impl Send for Everything3Pipe {}

impl Everything3Pipe {
    fn connect(
        pipe_name: &[u16],
        instance_name: &str,
        timeout: Duration,
    ) -> Result<Self, EverythingError> {
        let timeout_ms = millis(timeout);
        let mut handle = unsafe {
            CreateFileW(
                pipe_name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE && unsafe { GetLastError() } == ERROR_PIPE_BUSY {
            if unsafe { WaitNamedPipeW(pipe_name.as_ptr(), timeout_ms) } == 0 {
                return if unsafe { GetLastError() } == ERROR_PIPE_BUSY {
                    Err(EverythingError::Timeout)
                } else {
                    Err(EverythingError::FolderSizePipeUnavailable(
                        instance_name.to_owned(),
                    ))
                };
            }
            handle = unsafe {
                CreateFileW(
                    pipe_name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    ptr::null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    ptr::null_mut(),
                )
            };
        }
        if handle == INVALID_HANDLE_VALUE {
            Err(EverythingError::FolderSizePipeUnavailable(
                instance_name.to_owned(),
            ))
        } else {
            Ok(Self { handle })
        }
    }

    fn folder_size(
        &self,
        path: &[u8],
        timeout: Duration,
    ) -> Result<EverythingFolderSize, EverythingError> {
        let mut request = Vec::with_capacity(8 + path.len());
        request.extend_from_slice(&EVERYTHING3_FOLDER_SIZE_COMMAND.to_le_bytes());
        request.extend_from_slice(&(path.len() as u32).to_le_bytes());
        request.extend_from_slice(path);
        write_overlapped(self.handle, &request, timeout)?;

        let mut header = [0u8; 8];
        read_overlapped(self.handle, &mut header, timeout)?;
        let code = u32::from_le_bytes(header[..4].try_into().unwrap());
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        match code {
            EVERYTHING3_RESPONSE_OK | EVERYTHING3_RESPONSE_OK_MORE_DATA => {
                if size != 8 {
                    if size > 0 {
                        skip_pipe_bytes(self.handle, size, timeout)?;
                    }
                    return protocol("folder-size response is not 8 bytes");
                }
                let mut value = [0u8; 8];
                read_overlapped(self.handle, &mut value, timeout)?;
                Ok(decode_folder_size_value(u64::from_le_bytes(value)))
            }
            code => {
                if size > 0 {
                    skip_pipe_bytes(self.handle, size, timeout)?;
                }
                match code {
                    EVERYTHING3_RESPONSE_NOT_FOUND => {
                        Err(EverythingError::FolderSizeRejected(code))
                    }
                    EVERYTHING3_RESPONSE_BAD_REQUEST
                    | EVERYTHING3_RESPONSE_CANCELLED
                    | EVERYTHING3_RESPONSE_OUT_OF_MEMORY
                    | EVERYTHING3_RESPONSE_INVALID_COMMAND => {
                        Err(EverythingError::FolderSizeRejected(code))
                    }
                    _ => protocol("unknown folder-size response code"),
                }
            }
        }
    }
}

impl Drop for Everything3Pipe {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

fn decode_folder_size_value(value: u64) -> EverythingFolderSize {
    if value == EVERYTHING3_UINT64_MAX {
        EverythingFolderSize::NotIndexed
    } else {
        EverythingFolderSize::Indexed(value)
    }
}
fn everything3_pipe_name(instance_name: &str) -> Vec<u16> {
    let mut name = String::from(r"\\.\PIPE\Everything IPC");
    if !instance_name.is_empty() {
        name.push_str(" (");
        name.push_str(instance_name);
        name.push(')');
    }
    wide(name)
}

fn normalize_folder_size_path(path: &Path) -> Result<Vec<u8>, EverythingError> {
    let mut value = String::from_utf16(&path.as_os_str().encode_wide().collect::<Vec<_>>())
        .map_err(|_| EverythingError::Protocol("folder-size path is not valid Unicode".into()))?
        .replace('/', "\\");
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        value = format!(r"\\{rest}");
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        value = rest.to_owned();
    }
    while value.len() > 3 && value.ends_with('\\') {
        value.pop();
    }
    if value.is_empty() || value.contains('\0') {
        return Err(EverythingError::Protocol(
            "folder-size path is empty or contains NUL".into(),
        ));
    }
    Ok(value.into_bytes())
}

fn remaining(deadline: std::time::Instant) -> Result<Duration, EverythingError> {
    deadline
        .checked_duration_since(std::time::Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(EverythingError::Timeout)
}

fn is_network_path(path: &Path) -> bool {
    let text = path.as_os_str().to_string_lossy().replace('/', "\\");
    if text.starts_with(r"\\") && !text.starts_with(r"\\?\") {
        return true;
    }
    let bytes = text.as_bytes();
    if bytes.len() < 2 || bytes[1] != b':' || !bytes[0].is_ascii_alphabetic() {
        return false;
    }
    let root = wide(format!("{}:\\", bytes[0] as char));
    unsafe { GetDriveTypeW(root.as_ptr()) == 4 }
}
fn write_overlapped(
    handle: HANDLE,
    bytes: &[u8],
    timeout: Duration,
) -> Result<(), EverythingError> {
    let mut offset = 0;
    while offset < bytes.len() {
        let written = overlapped_write(handle, &bytes[offset..], timeout)?;
        if written == 0 {
            return Err(EverythingError::FolderSizeDisconnected);
        }
        offset += written;
    }
    Ok(())
}

fn read_overlapped(
    handle: HANDLE,
    bytes: &mut [u8],
    timeout: Duration,
) -> Result<(), EverythingError> {
    let mut offset = 0;
    while offset < bytes.len() {
        let read = overlapped_read(handle, &mut bytes[offset..], timeout)?;
        if read == 0 {
            return Err(EverythingError::FolderSizeDisconnected);
        }
        offset += read;
    }
    Ok(())
}

fn skip_pipe_bytes(
    handle: HANDLE,
    mut remaining: usize,
    timeout: Duration,
) -> Result<(), EverythingError> {
    let mut buffer = [0u8; 256];
    while remaining > 0 {
        let chunk = remaining.min(buffer.len());
        read_overlapped(handle, &mut buffer[..chunk], timeout)?;
        remaining -= chunk;
    }
    Ok(())
}

fn overlapped_read(
    handle: HANDLE,
    bytes: &mut [u8],
    timeout: Duration,
) -> Result<usize, EverythingError> {
    overlapped_io(handle, bytes.as_mut_ptr(), bytes.len(), true, timeout)
}

fn overlapped_write(
    handle: HANDLE,
    bytes: &[u8],
    timeout: Duration,
) -> Result<usize, EverythingError> {
    overlapped_io(
        handle,
        bytes.as_ptr() as *mut u8,
        bytes.len(),
        false,
        timeout,
    )
}

fn overlapped_io(
    handle: HANDLE,
    bytes: *mut u8,
    byte_count: usize,
    reading: bool,
    timeout: Duration,
) -> Result<usize, EverythingError> {
    let event = unsafe { CreateEventW(ptr::null(), 1, 0, ptr::null()) };
    if event.is_null() {
        return Err(last_error());
    }
    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    overlapped.hEvent = event;
    let mut transferred = 0u32;
    let ok = unsafe {
        if reading {
            ReadFile(
                handle,
                bytes,
                byte_count as u32,
                &mut transferred,
                &mut overlapped,
            )
        } else {
            WriteFile(
                handle,
                bytes,
                byte_count as u32,
                &mut transferred,
                &mut overlapped,
            )
        }
    };
    let result = if ok != 0 {
        Ok(transferred as usize)
    } else {
        let code = unsafe { GetLastError() };
        if code != ERROR_IO_PENDING {
            pipe_io_error(code)
        } else {
            match unsafe { WaitForSingleObject(event, millis(timeout)) } {
                WAIT_OBJECT_0 => {
                    if unsafe { GetOverlappedResult(handle, &overlapped, &mut transferred, 0) } != 0
                    {
                        Ok(transferred as usize)
                    } else {
                        pipe_io_error(unsafe { GetLastError() })
                    }
                }
                WAIT_TIMEOUT => {
                    unsafe {
                        CancelIoEx(handle, &overlapped);
                        GetOverlappedResult(handle, &overlapped, &mut transferred, 1);
                    }
                    Err(EverythingError::Timeout)
                }
                _ => pipe_io_error(unsafe { GetLastError() }),
            }
        }
    };
    unsafe {
        CloseHandle(event);
    }
    result
}

fn pipe_io_error(code: u32) -> Result<usize, EverythingError> {
    match code {
        ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_OPERATION_ABORTED => {
            Err(EverythingError::FolderSizeDisconnected)
        }
        _ => Err(EverythingError::Windows(code)),
    }
}
fn protocol<T>(m: &str) -> Result<T, EverythingError> {
    Err(EverythingError::Protocol(m.into()))
}
fn filetime(v: u64) -> Option<SystemTime> {
    let ticks = v.checked_sub(116_444_736_000_000_000)?;
    Some(UNIX_EPOCH + Duration::from_nanos(ticks.saturating_mul(100)))
}
fn registry_installations() -> Vec<EverythingInstallation> {
    [
        (
            HKEY_LOCAL_MACHINE,
            r"Software\voidtools\Everything 1.5a",
            "1.5a",
        ),
        (
            HKEY_CURRENT_USER,
            r"Software\voidtools\Everything 1.5a",
            "1.5a",
        ),
        (HKEY_LOCAL_MACHINE, r"Software\voidtools\Everything", ""),
        (HKEY_CURRENT_USER, r"Software\voidtools\Everything", ""),
    ]
    .into_iter()
    .filter_map(|(root, key, instance)| {
        reg_string(root, key, "ExePath").map(|p| EverythingInstallation {
            executable_path: p.into(),
            instance_name: instance.into(),
            running: false,
        })
    })
    .collect()
}
fn reg_string(root: *mut c_void, key: &str, value: &str) -> Option<OsString> {
    let k = wide(key);
    let v = wide(value);
    let mut bytes = 0;
    let status = unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != ERROR_SUCCESS || bytes < 2 {
        return None;
    }
    let mut buf = vec![0u16; bytes as usize / 2];
    if unsafe {
        RegGetValueW(
            root,
            k.as_ptr(),
            v.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buf.as_mut_ptr().cast(),
            &mut bytes,
        )
    } != ERROR_SUCCESS
    {
        return None;
    }
    let len = buf.iter().position(|x| *x == 0).unwrap_or(buf.len());
    Some(OsString::from_wide(&buf[..len]))
}
fn common_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in [
        std::env::var_os("ProgramFiles"),
        std::env::var_os("LOCALAPPDATA"),
    ]
    .into_iter()
    .flatten()
    {
        out.push(PathBuf::from(&root).join(r"Everything 1.5a\Everything64.exe"));
        out.push(PathBuf::from(root).join(r"Everything\Everything64.exe"))
    }
    out
}
fn running_instances() -> Vec<String> {
    let mut out = Vec::new();
    unsafe { EnumWindows(Some(enum_windows), (&mut out as *mut Vec<String>) as LPARAM) };
    out
}
unsafe extern "system" fn enum_windows(w: HWND, ctx: LPARAM) -> i32 {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(w, buf.as_mut_ptr(), 256) };
    if len > 0 {
        let class = String::from_utf16_lossy(&buf[..len as usize]);
        let instance = if class == IPC_CLASS {
            Some(String::new())
        } else {
            class
                .strip_prefix("EVERYTHING_TASKBAR_NOTIFICATION_(")
                .and_then(|x| x.strip_suffix(')'))
                .map(str::to_owned)
        };
        if let Some(i) = instance {
            let out = unsafe { &mut *(ctx as *mut Vec<String>) };
            if !out.contains(&i) {
                out.push(i)
            }
        }
    }
    1
}
fn wide(v: impl AsRef<OsStr>) -> Vec<u16> {
    v.as_ref().encode_wide().chain(Some(0)).collect()
}
fn millis(d: Duration) -> u32 {
    d.as_millis().clamp(1, u32::MAX as u128) as u32
}
fn last_error() -> EverythingError {
    let c = unsafe { GetLastError() };
    if c == 1460 {
        EverythingError::Timeout
    } else {
        EverythingError::Windows(c)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scope_keeps_advanced_query_and_encodes_only_the_scope() {
        assert_eq!(
            compose_search(
                Some(Path::new(r#"C:\资料 (A)!|\Emoji 😀\say "hi""#)),
                "ext:png|jpg dm:thisweek"
            ),
            r#"<ancestor:C:\资料#32:#40:A#41:#33:#124:\Emoji#32:#128512:\say#32:#34:hi#34:> ext:png|jpg dm:thisweek"#
        )
    }

    #[test]
    fn scope_uses_everything_character_entities_for_space_parentheses_and_operators() {
        assert_eq!(
            compose_search(Some(Path::new(r"D:\My Assets (A)!|")), "*.blend"),
            r"<ancestor:D:\My#32:Assets#32:#40:A#41:#33:#124:> *.blend"
        );
        assert_eq!(
            compose_search(Some(Path::new(r"\\LiuYanghomeNAS\Multimedia")), "*.mkv"),
            r"<ancestor:\\LiuYanghomeNAS\Multimedia> *.mkv"
        );
    }
    #[test]
    fn global_is_unchanged() {
        assert_eq!(
            compose_search(None, "  *.blend size:>100mb  "),
            "*.blend size:>100mb"
        )
    }
    #[test]
    fn type_filter_wraps_the_complete_scoped_advanced_query() {
        let scope = Some(Path::new(r"D:\My Assets (A)!|"));
        assert_eq!(
            compose_typed_search(scope, "*.md size:>1kb", EverythingItemKind::Files),
            r"file: <<ancestor:D:\My#32:Assets#32:#40:A#41:#33:#124:> *.md size:>1kb>"
        );
        assert_eq!(
            compose_typed_search(None, ".md", EverythingItemKind::Folders),
            "folder: <.md>"
        );
    }
    #[test]
    fn everything3_pipe_name_includes_only_the_configured_instance() {
        let unnamed = everything3_pipe_name("");
        let named = everything3_pipe_name("1.5a");
        assert_eq!(
            String::from_utf16_lossy(&unnamed[..unnamed.len() - 1]),
            r"\\.\PIPE\Everything IPC"
        );
        assert_eq!(
            String::from_utf16_lossy(&named[..named.len() - 1]),
            r"\\.\PIPE\Everything IPC (1.5a)"
        );
    }

    #[test]
    fn folder_size_path_normalization_preserves_utf8_and_trims_only_trailing_separators() {
        assert_eq!(
            normalize_folder_size_path(Path::new(r"F:/资料/零字节/")).unwrap(),
            "F:\\资料\\零字节".as_bytes()
        );
        assert_eq!(
            normalize_folder_size_path(Path::new(r"C:\")).unwrap(),
            r"C:\".as_bytes()
        );
        assert_eq!(
            normalize_folder_size_path(Path::new(r"\\?\F:\资料")).unwrap(),
            r"F:\资料".as_bytes()
        );
        assert_eq!(
            normalize_folder_size_path(Path::new(r"\\?\UNC\server\共享")).unwrap(),
            r"\\server\共享".as_bytes()
        );
    }

    #[test]
    fn unc_paths_are_network_without_touching_the_share() {
        assert!(is_network_path(Path::new(r"\\server\share\folder")));
        assert!(!is_network_path(Path::new(r"F:\local\folder")));
    }

    #[test]
    fn folder_size_response_values_distinguish_zero_and_missing_index() {
        assert_eq!(
            decode_folder_size_value(0),
            EverythingFolderSize::Indexed(0)
        );
        assert_eq!(
            decode_folder_size_value(EVERYTHING3_UINT64_MAX),
            EverythingFolderSize::NotIndexed
        );
    }
    #[test]
    fn named_class() {
        assert_eq!(
            instance_class("1.5a"),
            "EVERYTHING_TASKBAR_NOTIFICATION_(1.5a)"
        )
    }
    #[test]
    fn query2_header_carries_stable_offset_and_sort_type() {
        let encoded = encode_query(
            123usize as HWND,
            ".md",
            256,
            256,
            REQ_FULL,
            EverythingSort::ModifiedDescending.ipc(),
        );
        assert_eq!(get32(&encoded, 12).unwrap(), 256);
        assert_eq!(get32(&encoded, 16).unwrap(), 256);
        assert_eq!(get32(&encoded, 24).unwrap(), 14);
    }
    #[test]
    fn highlighted_name_parser_preserves_compact_text_and_literal_stars() {
        let cases = [
            (
                "*高级*功能计划.md",
                vec![("高级", true), ("功能计划.md", false)],
            ),
            (
                "_*high*_*level*.md",
                vec![
                    ("_", false),
                    ("high", true),
                    ("_", false),
                    ("level", true),
                    (".md", false),
                ],
            ),
            (
                "asset*.blend*1",
                vec![("asset", false), (".blend", true), ("1", false)],
            ),
            (
                "guide*.md*~",
                vec![("guide", false), (".md", true), ("~", false)],
            ),
            ("star**file*.md*", vec![("star*file", false), (".md", true)]),
        ];
        for (marked, expected) in cases {
            let actual = parse_highlighted_name(marked)
                .into_iter()
                .map(|segment| (segment.text, segment.highlighted))
                .collect::<Vec<_>>();
            assert_eq!(
                actual,
                expected
                    .into_iter()
                    .map(|(text, highlighted)| (text.to_owned(), highlighted))
                    .collect::<Vec<_>>(),
                "{marked}"
            );
        }
    }
    #[test]
    fn unicode_query_encoding() {
        let b = encode_query(123usize as HWND, "中文 😀", 2, 10, REQ_FULL, 1);
        assert_eq!(get32(&b, 0).unwrap(), 123);
        let u = b[HEADER..]
            .chunks_exact(2)
            .map(|x| u16::from_le_bytes([x[0], x[1]]))
            .take_while(|x| *x != 0)
            .collect::<Vec<_>>();
        assert_eq!(String::from_utf16(&u).unwrap(), "中文 😀")
    }
    #[test]
    fn live_search_and_folder_size() {
        let p = PathBuf::from(r"C:\Program Files\Everything 1.5a\Everything64.exe");
        if !p.is_file() {
            return;
        }
        let c = EverythingClient::new(PlatformEverythingConfig {
            executable_path: p.clone(),
            instance_name: "1.5a".into(),
            allow_start: false,
        })
        .unwrap();
        let page = c
            .search(
                &EverythingSearchRequest::new("Everything64.exe", None),
                Duration::from_secs(3),
            )
            .unwrap();

        assert!(page.items.iter().any(|item| item.path == p));
        let indexed_folder = Path::new(r"F:\CodeProjects\AsterFiles");
        assert!(
            indexed_folder.is_dir(),
            "fixed F: validation sample is missing"
        );
        let folder_size = c
            .folder_size(indexed_folder, Duration::from_secs(3))
            .unwrap();
        assert!(
            matches!(folder_size, EverythingFolderSize::Indexed(size) if size > 0),
            "Everything3 folder-size IPC did not return an indexed positive size: {folder_size:?}"
        );
        println!("everything3_sample=F:\\CodeProjects\\AsterFiles result={folder_size:?}");
        assert!(
            c.folder_size(indexed_folder, Duration::from_secs(3))
                .is_ok(),
            "the reusable Everything3 pipe failed on its second query"
        );

        let empty = PathBuf::from(r"F:\CodeProjects\AsterFiles\artifacts\state")
            .join(format!("everything-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let empty_size = loop {
            let result = c.folder_size(&empty, Duration::from_secs(1)).unwrap();
            if result == EverythingFolderSize::Indexed(0) || std::time::Instant::now() >= deadline {
                break result;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        std::fs::remove_dir(&empty).unwrap();
        assert_eq!(empty_size, EverythingFolderSize::Indexed(0));
        println!("everything3_empty_sample=temporary result={empty_size:?}");
    }
    #[test]
    fn live_scoped_markdown_search_matches_the_indexed_docs_directory() {
        let executable = PathBuf::from(r"C:\Program Files\Everything 1.5a\Everything64.exe");
        let docs = PathBuf::from(r"F:\CodeProjects\AsterFiles\docs");
        if !executable.is_file() || !docs.is_dir() {
            return;
        }
        let client = EverythingClient::new(PlatformEverythingConfig {
            executable_path: executable,
            instance_name: "1.5a".into(),
            allow_start: false,
        })
        .unwrap();
        for query in [".md", "*.md"] {
            let page = client
                .search(
                    &EverythingSearchRequest::new(query, Some(docs.clone())),
                    Duration::from_secs(3),
                )
                .unwrap();
            assert!(
                page.total >= 17,
                "{query} returned only {} items",
                page.total
            );
            assert!(!page.items.is_empty(), "{query} returned no first page");
            assert!(page.items.iter().all(|item| item.path.starts_with(&docs)));
        }
    }
    #[test]
    fn live_status() {
        let p = PathBuf::from(r"C:\Program Files\Everything 1.5a\Everything64.exe");
        if !p.is_file() {
            return;
        }
        let c = EverythingClient::new(PlatformEverythingConfig {
            executable_path: p,
            instance_name: "1.5a".into(),
            allow_start: false,
        })
        .unwrap();
        let s = c.status(Duration::from_secs(2)).unwrap();
        assert!(s.version.minor >= 5);
        assert!(s.database_loaded)
    }
}
