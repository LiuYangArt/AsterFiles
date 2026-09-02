#![allow(dead_code)]

use std::{
    cell::Cell,
    io,
    marker::PhantomData,
    mem::size_of,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
    rc::Rc,
    sync::mpsc,
    thread,
    time::Duration,
};
use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, POINT, RPC_E_CHANGED_MODE, WPARAM},
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        UI::{
            Shell::{
                CMF_EXTENDEDVERBS, CMF_NORMAL, CMIC_MASK_PTINVOKE, CMINVOKECOMMANDINFOEX,
                GCS_VERBW, IContextMenu, IContextMenu2, IContextMenu3, IShellFolder,
                SHBindToParent, SHParseDisplayName,
            },
            WindowsAndMessaging::{
                CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
                DispatchMessageW, GetMenuItemCount, GetMenuItemInfoW, HMENU, MENUITEMINFOW,
                MFS_CHECKED, MFS_DEFAULT, MFS_DISABLED, MFT_SEPARATOR, MIIM_FTYPE, MIIM_ID,
                MIIM_STATE, MIIM_STRING, MIIM_SUBMENU, MSG, PM_NOREMOVE, PM_REMOVE, PeekMessageW,
                RegisterClassW, SW_SHOWNORMAL, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx,
                TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DRAWITEM, WM_INITMENUPOPUP,
                WM_MEASUREITEM, WM_MENUCHAR, WNDCLASSW,
            },
        },
    },
    core::{Interface, PCSTR, PCWSTR, PSTR, PWSTR, w},
};
use windows_sys::Win32::System::Registry::{HKEY_CLASSES_ROOT, RRF_RT_REG_SZ, RegGetValueW};

const FIRST_COMMAND_ID: u32 = 1;
const LAST_COMMAND_ID: u32 = 0x7fff;
const DYNAMIC_SUBMENU_WAIT: Duration = Duration::from_millis(300);
const DYNAMIC_SUBMENU_POLL: Duration = Duration::from_millis(10);
const REGISTRY_COMMAND_VERB_PREFIX: &str = "registry-command:";
const MENU_WINDOW_CLASS: PCWSTR = w!("AsterFiles.ClassicMenuOwner");

thread_local! {
    static ACTIVE_MENU_SESSION: Cell<*const ClassicMenuSession> = const { Cell::new(ptr::null()) };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassicMenuItem {
    pub command_id: Option<u32>,
    pub title: String,
    pub verb: Option<String>,
    pub enabled: bool,
    pub checked: bool,
    pub default: bool,
    pub kind: ClassicMenuItemKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassicMenuItemKind {
    Command,
    Separator,
    Submenu {
        token: ShellMenuSubmenuToken,
        items: Vec<ClassicMenuItem>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassicMenuInvocation {
    BuiltIn { verb: String },
    Shell { verb: Option<String> },
}

pub type ShellMenuSessionId = u64;
pub type ShellMenuRequestId = u64;
pub type ShellMenuSubmenuToken = u64;

#[derive(Debug, Clone)]
pub enum ShellMenuLoadTarget {
    Paths(Vec<PathBuf>),
    Background(PathBuf),
}

#[derive(Debug)]
pub enum ShellMenuCommand {
    Load {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        target: ShellMenuLoadTarget,
        include_extended_verbs: bool,
        owner_window: isize,
    },
    LoadSubmenu {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        submenu_request_id: ShellMenuRequestId,
        token: ShellMenuSubmenuToken,
    },
    Invoke {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        command_id: u32,
        owner_window: isize,
        screen_x: i32,
        screen_y: i32,
    },
    Close {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
    },
}

#[derive(Debug)]
pub enum ShellMenuEvent {
    Loaded {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        items: Vec<ClassicMenuItem>,
        elapsed_ms: u128,
    },
    SubmenuLoaded {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        submenu_request_id: ShellMenuRequestId,
        token: ShellMenuSubmenuToken,
        items: Vec<ClassicMenuItem>,
        elapsed_ms: u128,
    },
    SubmenuError {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        submenu_request_id: ShellMenuRequestId,
        token: ShellMenuSubmenuToken,
        message: String,
        elapsed_ms: u128,
    },
    Invoked {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        invocation: ClassicMenuInvocation,
        elapsed_ms: u128,
    },
    Error {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
        operation: &'static str,
        message: String,
        elapsed_ms: u128,
    },
    Closed {
        session_id: ShellMenuSessionId,
        request_id: ShellMenuRequestId,
    },
}

#[derive(Clone)]
pub struct ShellMenuWorker {
    commands: mpsc::Sender<ShellMenuCommand>,
}

impl ShellMenuWorker {
    pub fn spawn() -> (Self, mpsc::Receiver<ShellMenuEvent>) {
        let (commands, command_rx) = mpsc::channel();
        let (event_tx, events) = mpsc::channel();
        thread::spawn(move || shell_menu_worker_loop(command_rx, event_tx));
        (Self { commands }, events)
    }

    pub fn send(&self, command: ShellMenuCommand) -> io::Result<()> {
        self.commands
            .send(command)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "shell menu worker stopped"))
    }
}

fn shell_menu_worker_loop(
    commands: mpsc::Receiver<ShellMenuCommand>,
    events: mpsc::Sender<ShellMenuEvent>,
) {
    let mut session: Option<(ShellMenuSessionId, ShellMenuRequestId, ClassicMenuSession)> = None;
    ensure_sta_message_queue();
    loop {
        pump_sta_messages();
        let command = match commands.recv_timeout(Duration::from_millis(10)) {
            Ok(command) => command,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        match command {
            ShellMenuCommand::Load {
                session_id,
                request_id,
                target,
                include_extended_verbs,
                owner_window,
            } => {
                let started = std::time::Instant::now();
                let result = match target {
                    ShellMenuLoadTarget::Paths(paths) => ClassicMenuSession::for_paths_with_owner(
                        &paths,
                        include_extended_verbs,
                        owner_window,
                    ),
                    ShellMenuLoadTarget::Background(folder) => {
                        ClassicMenuSession::for_background_with_owner(
                            &folder,
                            include_extended_verbs,
                            owner_window,
                        )
                    }
                }
                .and_then(|mut menu| {
                    let items = menu.items_top_level()?;
                    menu.preload_background_submenus(&items);
                    session = Some((session_id, request_id, menu));
                    Ok(items)
                });
                match result {
                    Ok(items) => {
                        let _ = events.send(ShellMenuEvent::Loaded {
                            session_id,
                            request_id,
                            items,
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ShellMenuEvent::Error {
                            session_id,
                            request_id,
                            operation: "load",
                            message: error.to_string(),
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                }
            }
            ShellMenuCommand::LoadSubmenu {
                session_id,
                request_id,
                submenu_request_id,
                token,
            } => {
                let started = std::time::Instant::now();
                let result = session
                    .as_mut()
                    .filter(|(id, req, _)| *id == session_id && *req == request_id)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "shell menu session not found")
                    })
                    .and_then(|(_, _, menu)| menu.load_submenu(token));
                match result {
                    Ok(items) => {
                        let _ = events.send(ShellMenuEvent::SubmenuLoaded {
                            session_id,
                            request_id,
                            submenu_request_id,
                            token,
                            items,
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ShellMenuEvent::SubmenuError {
                            session_id,
                            request_id,
                            submenu_request_id,
                            token,
                            message: error.to_string(),
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                }
            }
            ShellMenuCommand::Invoke {
                session_id,
                request_id,
                command_id,
                owner_window,
                screen_x,
                screen_y,
            } => {
                let started = std::time::Instant::now();
                let result = session
                    .as_ref()
                    .filter(|(id, req, _)| *id == session_id && *req == request_id)
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "shell menu session not found")
                    })
                    .and_then(|(_, _, menu)| {
                        menu.invoke_command(command_id, owner_window, screen_x, screen_y)
                    });
                match result {
                    Ok(invocation) => {
                        let _ = events.send(ShellMenuEvent::Invoked {
                            session_id,
                            request_id,
                            invocation,
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                    Err(error) => {
                        let _ = events.send(ShellMenuEvent::Error {
                            session_id,
                            request_id,
                            operation: "invoke",
                            message: error.to_string(),
                            elapsed_ms: started.elapsed().as_millis(),
                        });
                    }
                }
            }
            ShellMenuCommand::Close {
                session_id,
                request_id,
            } => {
                if session
                    .as_ref()
                    .is_some_and(|(id, req, _)| *id == session_id && *req == request_id)
                {
                    session = None;
                }
                let _ = events.send(ShellMenuEvent::Closed {
                    session_id,
                    request_id,
                });
            }
        }
        pump_sta_messages();
    }
}

fn ensure_sta_message_queue() {
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE);
    }
}

fn pump_sta_messages() {
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, 0, 0, PM_REMOVE) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

pub struct ClassicMenuSession {
    menu: HMENU,
    context_menu: Option<IContextMenu>,
    context_menu2: Option<IContextMenu2>,
    context_menu3: Option<IContextMenu3>,
    com_initialized: bool,
    submenus: Vec<SubmenuRegistration>,
    registry_commands: std::collections::HashMap<u32, RegistryCascadeCommand>,
    folder: Option<PathBuf>,
    preloaded_submenus: std::collections::HashMap<ShellMenuSubmenuToken, Vec<ClassicMenuItem>>,
    _thread_affinity: PhantomData<Rc<()>>,
}

#[derive(Clone, Copy)]
struct SubmenuRegistration {
    menu: HMENU,
    parent: HMENU,
}

struct RegistryCascadeCommand {
    command: String,
    elevated: bool,
}

impl ClassicMenuSession {
    pub fn for_paths_with_owner(
        paths: &[PathBuf],
        include_extended_verbs: bool,
        owner_window: isize,
    ) -> io::Result<Self> {
        validate_selection(paths)?;
        Self::create(include_extended_verbs, None, || {
            create_selection_context_menu(paths, HWND(owner_window as *mut _))
        })
    }

    pub fn for_background_with_owner(
        folder: &Path,
        include_extended_verbs: bool,
        owner_window: isize,
    ) -> io::Result<Self> {
        validate_background(folder)?;
        Self::create(include_extended_verbs, Some(folder.to_path_buf()), || {
            create_background_context_menu(folder, HWND(owner_window as *mut _))
        })
    }

    fn create(
        include_extended_verbs: bool,
        folder: Option<PathBuf>,
        create_context_menu: impl FnOnce() -> io::Result<IContextMenu>,
    ) -> io::Result<Self> {
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        if initialized == RPC_E_CHANGED_MODE {
            return Err(io::Error::other(
                "classic menu requires an STA owning thread",
            ));
        }
        initialized.ok().map_err(windows_error)?;
        let context_menu = match create_context_menu() {
            Ok(value) => value,
            Err(error) => {
                unsafe { CoUninitialize() };
                return Err(error);
            }
        };
        let context_menu3 = context_menu.cast::<IContextMenu3>().ok();
        let context_menu2 = context_menu.cast::<IContextMenu2>().ok();
        let menu = match unsafe { CreatePopupMenu() } {
            Ok(menu) => menu,
            Err(error) => {
                drop(context_menu3);
                drop(context_menu2);
                drop(context_menu);
                unsafe { CoUninitialize() };
                return Err(windows_error(error));
            }
        };
        let flags = CMF_NORMAL
            | if include_extended_verbs {
                CMF_EXTENDEDVERBS
            } else {
                0
            };
        let query = unsafe {
            context_menu.QueryContextMenu(menu, 0, FIRST_COMMAND_ID, LAST_COMMAND_ID, flags)
        };
        if query.is_err() {
            unsafe {
                let _ = DestroyMenu(menu);
            }
            drop(context_menu3);
            drop(context_menu2);
            drop(context_menu);
            unsafe { CoUninitialize() };
            return Err(io::Error::other(format!(
                "QueryContextMenu failed: {query:?}"
            )));
        }
        Ok(Self {
            menu,
            context_menu: Some(context_menu),
            context_menu2,
            context_menu3,
            com_initialized: true,
            submenus: Vec::new(),
            registry_commands: std::collections::HashMap::new(),
            folder,
            preloaded_submenus: std::collections::HashMap::new(),
            _thread_affinity: PhantomData,
        })
    }

    pub fn items(&self) -> io::Result<Vec<ClassicMenuItem>> {
        read_menu_recursive(self.menu, self)
    }

    fn items_top_level(&mut self) -> io::Result<Vec<ClassicMenuItem>> {
        let menu = self.menu;
        self.read_menu_level(menu)
    }

    fn read_menu_level(&mut self, menu: HMENU) -> io::Result<Vec<ClassicMenuItem>> {
        read_menu_level(menu, self)
    }

    fn register_submenu(&mut self, menu: HMENU, parent: HMENU) -> ShellMenuSubmenuToken {
        if let Some(index) = self
            .submenus
            .iter()
            .position(|registration| registration.menu == menu)
        {
            return index as ShellMenuSubmenuToken + 1;
        }
        self.submenus.push(SubmenuRegistration { menu, parent });
        self.submenus.len() as ShellMenuSubmenuToken
    }

    fn load_submenu(&mut self, token: ShellMenuSubmenuToken) -> io::Result<Vec<ClassicMenuItem>> {
        if let Some(items) = self.preloaded_submenus.get(&token) {
            return Ok(items.clone());
        }
        let index = token
            .checked_sub(1)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid submenu token"))?;
        let registration = *self.submenus.get(index).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "shell submenu token not found")
        })?;
        let position = submenu_position(registration.parent, registration.menu)?;
        self.initialize_submenu(registration.parent, registration.menu, position);
        pump_sta_messages();
        let mut submenu = submenu_at_position(registration.parent, position)?;
        self.submenus[index].menu = submenu;
        let items = self.read_menu_level(submenu)?;
        if !items.is_empty() {
            return Ok(items);
        }

        // Background-menu extensions can publish dynamic items noticeably later than
        // selection-menu extensions, so keep the STA responsive until a fixed deadline.
        let deadline = std::time::Instant::now() + DYNAMIC_SUBMENU_WAIT;
        while std::time::Instant::now() < deadline {
            thread::sleep(DYNAMIC_SUBMENU_POLL);
            pump_sta_messages();
            submenu = submenu_at_position(registration.parent, position)?;
            self.submenus[index].menu = submenu;
            let items = self.read_menu_level(submenu)?;
            if !items.is_empty() {
                return Ok(items);
            }
        }
        let verb = menu_item_verb(registration.parent, position, self);
        if let Some(verb) = verb.as_deref()
            && let Some(items) = registry_cascade_items(verb, &mut self.registry_commands)
        {
            return Ok(items);
        }
        if verb.as_deref() == Some("windows.share")
            && let Some(folder) = self.folder.clone()
        {
            return load_selected_folder_submenu(&folder, "windows.share");
        }
        Ok(Vec::new())
    }

    fn preload_background_submenus(&mut self, items: &[ClassicMenuItem]) {
        if self.folder.is_none() {
            return;
        }
        let candidates = items
            .iter()
            .filter_map(|item| match &item.kind {
                ClassicMenuItemKind::Submenu { token, .. }
                    if matches!(
                        item.verb.as_deref(),
                        Some("powershell7x64" | "windows.share")
                    ) =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        for token in candidates {
            if let Ok(items) = self.load_submenu(token)
                && !items.is_empty()
            {
                self.preloaded_submenus.insert(token, items);
            }
        }
    }

    fn submenu_verb(&self, token: ShellMenuSubmenuToken) -> Option<String> {
        let index = token
            .checked_sub(1)
            .and_then(|value| usize::try_from(value).ok())?;
        let registration = *self.submenus.get(index)?;
        let position = submenu_position(registration.parent, registration.menu).ok()?;
        menu_item_verb(registration.parent, position, self)
    }

    fn invoke_command(
        &self,
        command_id: u32,
        owner_window: isize,
        screen_x: i32,
        screen_y: i32,
    ) -> io::Result<ClassicMenuInvocation> {
        let offset = command_id
            .checked_sub(FIRST_COMMAND_ID)
            .ok_or_else(|| io::Error::other("shell returned an invalid menu command"))?;
        let context_menu = self
            .context_menu
            .as_ref()
            .ok_or_else(|| io::Error::other("shell context menu was already released"))?;
        if let Some(command) = self.registry_commands.get(&command_id) {
            launch_registry_command(command, owner_window)?;
            return Ok(ClassicMenuInvocation::Shell {
                verb: Some(format!("{REGISTRY_COMMAND_VERB_PREFIX}{command_id}")),
            });
        }
        let verb = command_verb(context_menu, command_id);
        if let Some(verb) = verb.as_deref().filter(|verb| is_builtin_verb(verb)) {
            return Ok(ClassicMenuInvocation::BuiltIn {
                verb: verb.to_owned(),
            });
        }
        let invocation = CMINVOKECOMMANDINFOEX {
            cbSize: size_of::<CMINVOKECOMMANDINFOEX>() as u32,
            fMask: CMIC_MASK_PTINVOKE,
            hwnd: HWND(owner_window as *mut _),
            lpVerb: PCSTR(offset as usize as *const u8),
            lpParameters: PCSTR::null(),
            lpDirectory: PCSTR::null(),
            nShow: SW_SHOWNORMAL.0,
            dwHotKey: 0,
            hIcon: Default::default(),
            lpTitle: PCSTR::null(),
            lpVerbW: PCWSTR::null(),
            lpParametersW: PCWSTR::null(),
            lpDirectoryW: PCWSTR::null(),
            lpTitleW: PCWSTR::null(),
            ptInvoke: POINT {
                x: screen_x,
                y: screen_y,
            },
        };
        let base = (&invocation as *const CMINVOKECOMMANDINFOEX).cast();
        unsafe { context_menu.InvokeCommand(&*base) }.map_err(windows_error)?;
        Ok(ClassicMenuInvocation::Shell { verb })
    }
    pub fn show_native_and_invoke(
        &self,
        owner_window: isize,
        screen_x: i32,
        screen_y: i32,
    ) -> io::Result<Option<ClassicMenuInvocation>> {
        let popup_owner = PopupOwner::create(HWND(owner_window as *mut _), self)?;
        let selected = unsafe {
            TrackPopupMenuEx(
                self.menu,
                (TPM_RETURNCMD | TPM_RIGHTBUTTON).0,
                screen_x,
                screen_y,
                popup_owner.hwnd,
                None,
            )
        };
        if selected.0 == 0 {
            return Ok(None);
        }
        let command_id = selected.0 as u32;
        self.invoke_command(command_id, owner_window, screen_x, screen_y)
            .map(Some)
    }

    fn initialize_submenu(&self, parent: HMENU, submenu: HMENU, position: u32) {
        let _ = self.forward_menu_message(
            windows::Win32::UI::WindowsAndMessaging::WM_INITMENU,
            WPARAM(parent.0 as usize),
            LPARAM::default(),
        );
        let _ = self.forward_menu_message(
            WM_INITMENUPOPUP,
            WPARAM(submenu.0 as usize),
            LPARAM(position as isize),
        );
    }

    fn forward_menu_message(
        &self,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        if !is_dynamic_menu_message(message) {
            return None;
        }
        if matches!(
            message,
            windows::Win32::UI::WindowsAndMessaging::WM_INITMENU | WM_INITMENUPOPUP
        ) && let Some(menu2) = &self.context_menu2
            && unsafe { menu2.HandleMenuMsg(message, wparam, lparam) }.is_ok()
        {
            return Some(LRESULT::default());
        }
        if let Some(menu3) = &self.context_menu3 {
            let mut result = LRESULT::default();
            if unsafe { menu3.HandleMenuMsg2(message, wparam, lparam, Some(&mut result)) }.is_ok() {
                return Some(result);
            }
        }
        if let Some(menu2) = &self.context_menu2
            && unsafe { menu2.HandleMenuMsg(message, wparam, lparam) }.is_ok()
        {
            return Some(LRESULT::default());
        }
        None
    }

    fn release_com_interfaces(&mut self) {
        self.context_menu3 = None;
        self.context_menu2 = None;
        self.context_menu = None;
    }
}

fn load_selected_folder_submenu(folder: &Path, verb: &str) -> io::Result<Vec<ClassicMenuItem>> {
    let mut session = ClassicMenuSession::for_paths_with_owner(&[folder.to_path_buf()], false, 0)?;
    let items = session.items_top_level()?;
    let token = items
        .iter()
        .find_map(|item| match &item.kind {
            ClassicMenuItemKind::Submenu { token, .. } if item.verb.as_deref() == Some(verb) => {
                Some(*token)
            }
            _ => None,
        })
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "folder-equivalent submenu not found",
            )
        })?;
    session.load_submenu(token)
}

fn menu_item_verb(parent: HMENU, position: u32, session: &ClassicMenuSession) -> Option<String> {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_ID,
        ..Default::default()
    };
    unsafe { GetMenuItemInfoW(parent, position, true, &mut info) }.ok()?;
    (FIRST_COMMAND_ID..=LAST_COMMAND_ID)
        .contains(&info.wID)
        .then_some(info.wID)
        .and_then(|id| {
            session
                .context_menu
                .as_ref()
                .and_then(|menu| command_verb(menu, id))
        })
}

fn registry_cascade_items(
    verb: &str,
    commands: &mut std::collections::HashMap<u32, RegistryCascadeCommand>,
) -> Option<Vec<ClassicMenuItem>> {
    let cascade_key = registry_string(
        HKEY_CLASSES_ROOT,
        &format!(r"Directory\Background\shell\{verb}"),
        "ExtendedSubCommandsKey",
    )?;
    let shell_key = format!(r"{cascade_key}\shell");
    let mut items = Vec::new();
    for key in registry_subkeys(HKEY_CLASSES_ROOT, &shell_key)? {
        let item_key = format!(r"{shell_key}\{key}");
        let title = registry_string(HKEY_CLASSES_ROOT, &item_key, "MUIVerb")
            .or_else(|| registry_default_string(HKEY_CLASSES_ROOT, &item_key))
            .unwrap_or_else(|| key.clone());
        let command = registry_default_string(HKEY_CLASSES_ROOT, &format!(r"{item_key}\command"))?;
        let command_id = LAST_COMMAND_ID.checked_sub(u32::try_from(commands.len()).ok()?)?;
        let elevated = registry_value_exists(HKEY_CLASSES_ROOT, &item_key, "HasLUAShield")
            || key.eq_ignore_ascii_case("runas");
        commands.insert(command_id, RegistryCascadeCommand { command, elevated });
        items.push(ClassicMenuItem {
            command_id: Some(command_id),
            title: clean_menu_title(&title),
            verb: Some(format!("{REGISTRY_COMMAND_VERB_PREFIX}{command_id}")),
            enabled: true,
            checked: false,
            default: false,
            kind: ClassicMenuItemKind::Command,
        });
    }
    (!items.is_empty()).then_some(items)
}

fn launch_registry_command(
    command: &RegistryCascadeCommand,
    owner_window: isize,
) -> io::Result<()> {
    use windows_sys::Win32::UI::Shell::{SEE_MASK_UNICODE, SHELLEXECUTEINFOW, ShellExecuteExW};
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let executable = wide_null_str("cmd.exe");
    let parameters = wide_null_str(&format!("/S /C \"{}\"", command.command));
    let verb = command.elevated.then(|| wide_null_str("runas"));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_UNICODE,
        hwnd: owner_window as *mut _,
        lpVerb: verb.as_ref().map_or(ptr::null(), |verb| verb.as_ptr()),
        lpFile: executable.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    if unsafe { ShellExecuteExW(&mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn registry_value_exists(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> bool {
    let key = wide_null_str(key);
    let value = wide_null_str(value);
    (unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            0,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
        )
    }) == 0
}

fn registry_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
    value: &str,
) -> Option<String> {
    let key = wide_null_str(key);
    let value = wide_null_str(value);
    let mut bytes = 0_u32;
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            ptr::null_mut(),
            &mut bytes,
        )
    };
    if status != 0 || bytes < 2 {
        return None;
    }
    let mut buffer = vec![0_u16; bytes as usize / 2];
    let status = unsafe {
        RegGetValueW(
            root,
            key.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_SZ,
            ptr::null_mut(),
            buffer.as_mut_ptr().cast(),
            &mut bytes,
        )
    };
    if status != 0 {
        return None;
    }
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..length]))
}

fn registry_default_string(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
) -> Option<String> {
    registry_string(root, key, "")
}

fn registry_subkeys(
    root: windows_sys::Win32::System::Registry::HKEY,
    key: &str,
) -> Option<Vec<String>> {
    use windows_sys::Win32::System::Registry::{
        KEY_READ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    };
    let key = wide_null_str(key);
    let mut handle = ptr::null_mut();
    if unsafe { RegOpenKeyExW(root, key.as_ptr(), 0, KEY_READ, &mut handle) } != 0 {
        return None;
    }
    let mut names = Vec::new();
    for index in 0.. {
        let mut buffer = vec![0_u16; 256];
        let mut length = buffer.len() as u32;
        let status = unsafe {
            RegEnumKeyExW(
                handle,
                index,
                buffer.as_mut_ptr(),
                &mut length,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            )
        };
        if status == 259 {
            break;
        }
        if status != 0 {
            unsafe { RegCloseKey(handle) };
            return None;
        }
        names.push(String::from_utf16_lossy(&buffer[..length as usize]));
    }
    unsafe { RegCloseKey(handle) };
    Some(names)
}

fn wide_null_str(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

impl Drop for ClassicMenuSession {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.menu);
        }
        self.release_com_interfaces();
        if self.com_initialized {
            unsafe { CoUninitialize() };
        }
    }
}

struct PopupOwner {
    hwnd: HWND,
}
impl PopupOwner {
    fn create(parent: HWND, session: &ClassicMenuSession) -> io::Result<Self> {
        let class = WNDCLASSW {
            lpfnWndProc: Some(menu_window_proc),
            lpszClassName: MENU_WINDOW_CLASS,
            ..Default::default()
        };
        unsafe {
            let _ = RegisterClassW(&class);
        }
        ACTIVE_MENU_SESSION.with(|active| active.set(session));
        match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                MENU_WINDOW_CLASS,
                w!(""),
                WINDOW_STYLE::default(),
                0,
                0,
                0,
                0,
                (!parent.is_invalid()).then_some(parent),
                None,
                None,
                None,
            )
        } {
            Ok(hwnd) => Ok(Self { hwnd }),
            Err(error) => {
                ACTIVE_MENU_SESSION.with(|active| active.set(ptr::null()));
                Err(windows_error(error))
            }
        }
    }
}
impl Drop for PopupOwner {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
        ACTIVE_MENU_SESSION.with(|active| active.set(ptr::null()));
    }
}

unsafe extern "system" fn menu_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if let Some(result) = ACTIVE_MENU_SESSION
        .with(|active| {
            let session = active.get();
            (!session.is_null()).then(|| unsafe { &*session })
        })
        .and_then(|session| session.forward_menu_message(message, wparam, lparam))
    {
        return result;
    }
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn validate_selection(paths: &[PathBuf]) -> io::Result<()> {
    let Some(first) = paths.first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "shell menu selection is empty",
        ));
    };
    let parent = first.parent();
    if parent.is_none()
        || paths
            .iter()
            .any(|path| path.as_os_str().is_empty() || path.parent() != parent)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic menu selection must share one parent folder",
        ));
    }
    Ok(())
}

fn validate_background(folder: &Path) -> io::Result<()> {
    if folder.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "classic background menu folder is empty",
        ));
    }
    Ok(())
}

fn create_selection_context_menu(paths: &[PathBuf], owner: HWND) -> io::Result<IContextMenu> {
    let mut full_pidls = Vec::with_capacity(paths.len());
    let mut child_pidls = Vec::with_capacity(paths.len());
    let mut parent_folder = None;
    let result = (|| {
        for path in paths {
            let wide = wide_null(path);
            let mut full_pidl = ptr::null_mut();
            unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut full_pidl, 0, None) }
                .map_err(windows_error)?;
            full_pidls.push(full_pidl);
            let mut child_pidl = ptr::null_mut();
            let folder: IShellFolder = unsafe { SHBindToParent(full_pidl, Some(&mut child_pidl)) }
                .map_err(windows_error)?;
            if parent_folder.is_none() {
                parent_folder = Some(folder);
            }
            child_pidls.push(child_pidl.cast_const());
        }
        unsafe {
            parent_folder
                .as_ref()
                .expect("validated selection")
                .GetUIObjectOf(owner, &child_pidls, None)
        }
        .map_err(windows_error)
    })();
    free_pidls(full_pidls);
    result
}

fn create_background_context_menu(folder: &Path, owner: HWND) -> io::Result<IContextMenu> {
    let wide = wide_null(folder);
    let mut full_pidl = ptr::null_mut();
    unsafe { SHParseDisplayName(PCWSTR(wide.as_ptr()), None, &mut full_pidl, 0, None) }
        .map_err(windows_error)?;
    let result = (|| {
        let mut child_pidl = ptr::null_mut();
        let parent: IShellFolder =
            unsafe { SHBindToParent(full_pidl, Some(&mut child_pidl)) }.map_err(windows_error)?;
        let folder: IShellFolder =
            unsafe { parent.BindToObject(child_pidl.cast_const(), None) }.map_err(windows_error)?;
        unsafe { folder.CreateViewObject(owner) }.map_err(windows_error)
    })();
    free_pidls(vec![full_pidl]);
    result
}

fn free_pidls(pidls: Vec<*mut windows::Win32::UI::Shell::Common::ITEMIDLIST>) {
    for pidl in pidls {
        unsafe { CoTaskMemFree(Some(pidl.cast())) };
    }
}

fn read_menu_level(
    menu: HMENU,
    session: &mut ClassicMenuSession,
) -> io::Result<Vec<ClassicMenuItem>> {
    let count = unsafe { GetMenuItemCount(Some(menu)) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut items = Vec::with_capacity(count as usize);
    for position in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_STRING | MIIM_SUBMENU,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(menu, position, true, &mut info) }.map_err(windows_error)?;
        if info.fType.contains(MFT_SEPARATOR) {
            items.push(ClassicMenuItem {
                command_id: None,
                title: String::new(),
                verb: None,
                enabled: false,
                checked: false,
                default: false,
                kind: ClassicMenuItemKind::Separator,
            });
            continue;
        }
        let mut title = vec![0_u16; info.cch as usize + 1];
        info.dwTypeData = PWSTR(title.as_mut_ptr());
        info.cch = title.len() as u32;
        unsafe { GetMenuItemInfoW(menu, position, true, &mut info) }.map_err(windows_error)?;
        let command_id = (FIRST_COMMAND_ID..=LAST_COMMAND_ID)
            .contains(&info.wID)
            .then_some(info.wID);
        let kind = if info.hSubMenu.is_invalid() {
            ClassicMenuItemKind::Command
        } else {
            ClassicMenuItemKind::Submenu {
                token: session.register_submenu(info.hSubMenu, menu),
                items: Vec::new(),
            }
        };
        items.push(ClassicMenuItem {
            command_id,
            title: clean_menu_title(&String::from_utf16_lossy(&title[..info.cch as usize])),
            verb: command_id.and_then(|id| {
                session
                    .context_menu
                    .as_ref()
                    .and_then(|menu| command_verb(menu, id))
            }),
            enabled: !info.fState.contains(MFS_DISABLED),
            checked: info.fState.contains(MFS_CHECKED),
            default: info.fState.contains(MFS_DEFAULT),
            kind,
        });
    }
    Ok(items)
}

fn submenu_position(parent: HMENU, submenu: HMENU) -> io::Result<u32> {
    let count = unsafe { GetMenuItemCount(Some(parent)) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    for position in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_SUBMENU,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(parent, position, true, &mut info) }.map_err(windows_error)?;
        if info.hSubMenu == submenu {
            return Ok(position);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "shell submenu is no longer attached to its parent",
    ))
}

fn submenu_at_position(parent: HMENU, position: u32) -> io::Result<HMENU> {
    let mut info = MENUITEMINFOW {
        cbSize: size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_SUBMENU,
        ..Default::default()
    };
    unsafe { GetMenuItemInfoW(parent, position, true, &mut info) }.map_err(windows_error)?;
    if info.hSubMenu.is_invalid() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "shell submenu disappeared during initialization",
        ));
    }
    Ok(info.hSubMenu)
}

fn read_menu_recursive(
    menu: HMENU,
    session: &ClassicMenuSession,
) -> io::Result<Vec<ClassicMenuItem>> {
    let count = unsafe { GetMenuItemCount(Some(menu)) };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut items = Vec::with_capacity(count as usize);
    for position in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_FTYPE | MIIM_ID | MIIM_STATE | MIIM_STRING | MIIM_SUBMENU,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(menu, position, true, &mut info) }.map_err(windows_error)?;
        if info.fType.contains(MFT_SEPARATOR) {
            items.push(ClassicMenuItem {
                command_id: None,
                title: String::new(),
                verb: None,
                enabled: false,
                checked: false,
                default: false,
                kind: ClassicMenuItemKind::Separator,
            });
            continue;
        }
        let mut title = vec![0_u16; info.cch as usize + 1];
        info.dwTypeData = PWSTR(title.as_mut_ptr());
        info.cch = title.len() as u32;
        unsafe { GetMenuItemInfoW(menu, position, true, &mut info) }.map_err(windows_error)?;
        let command_id = (FIRST_COMMAND_ID..=LAST_COMMAND_ID)
            .contains(&info.wID)
            .then_some(info.wID);
        let kind = if info.hSubMenu.is_invalid() {
            ClassicMenuItemKind::Command
        } else {
            session.initialize_submenu(menu, info.hSubMenu, position);
            ClassicMenuItemKind::Submenu {
                token: 0,
                items: read_menu_recursive(info.hSubMenu, session)?,
            }
        };
        items.push(ClassicMenuItem {
            command_id,
            title: clean_menu_title(&String::from_utf16_lossy(&title[..info.cch as usize])),
            verb: command_id.and_then(|id| {
                session
                    .context_menu
                    .as_ref()
                    .and_then(|menu| command_verb(menu, id))
            }),
            enabled: !info.fState.contains(MFS_DISABLED),
            checked: info.fState.contains(MFS_CHECKED),
            default: info.fState.contains(MFS_DEFAULT),
            kind,
        });
    }
    Ok(items)
}

fn command_verb(context_menu: &IContextMenu, command_id: u32) -> Option<String> {
    let offset = command_id.checked_sub(FIRST_COMMAND_ID)? as usize;
    let mut buffer = vec![0_u16; 260];
    unsafe {
        context_menu.GetCommandString(
            offset,
            GCS_VERBW,
            None,
            PSTR(buffer.as_mut_ptr().cast()),
            buffer.len() as u32,
        )
    }
    .ok()?;
    let length = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    (length != 0).then(|| String::from_utf16_lossy(&buffer[..length]).to_ascii_lowercase())
}
fn is_builtin_verb(verb: &str) -> bool {
    matches!(verb, "cut" | "copy" | "paste" | "delete" | "rename")
}
fn is_dynamic_menu_message(message: u32) -> bool {
    matches!(
        message,
        windows::Win32::UI::WindowsAndMessaging::WM_INITMENU
            | WM_INITMENUPOPUP
            | WM_DRAWITEM
            | WM_MEASUREITEM
            | WM_MENUCHAR
    )
}
fn clean_menu_title(title: &str) -> String {
    let mut cleaned = String::with_capacity(title.len());
    let mut chars = title.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '&' {
            if chars.peek() == Some(&'&') {
                cleaned.push('&');
                chars.next();
            }
        } else {
            cleaned.push(ch);
        }
    }
    cleaned
}
fn wide_null(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}
fn windows_error(error: windows::core::Error) -> io::Error {
    io::Error::other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_requires_a_shared_parent() {
        assert!(validate_selection(&[]).is_err());
        assert!(validate_selection(&[PathBuf::from(r"C:\")]).is_err());
        assert!(
            validate_selection(&[
                PathBuf::from(r"C:\one\a.txt"),
                PathBuf::from(r"C:\two\b.txt")
            ])
            .is_err()
        );
        assert!(
            validate_selection(&[
                PathBuf::from(r"C:\one\a.txt"),
                PathBuf::from(r"C:\one\b.txt")
            ])
            .is_ok()
        );
    }
    #[test]
    fn background_requires_a_folder_identity() {
        assert!(validate_background(Path::new("")).is_err());
        assert!(validate_background(Path::new(r"C:\one")).is_ok());
    }
    #[test]
    fn asterfiles_commands_are_not_sent_to_shell() {
        for verb in ["cut", "copy", "paste", "delete", "rename"] {
            assert!(is_builtin_verb(verb));
        }
        assert!(!is_builtin_verb("openwith"));
    }
    #[test]
    fn only_shell_dynamic_menu_messages_are_forwarded() {
        for message in [
            windows::Win32::UI::WindowsAndMessaging::WM_INITMENU,
            WM_INITMENUPOPUP,
            WM_DRAWITEM,
            WM_MEASUREITEM,
            WM_MENUCHAR,
        ] {
            assert!(is_dynamic_menu_message(message));
        }
        assert!(!is_dynamic_menu_message(0));
    }
    #[test]
    fn menu_titles_preserve_literal_ampersands() {
        assert_eq!(clean_menu_title("&Open"), "Open");
        assert_eq!(clean_menu_title("A && B"), "A & B");
        assert_eq!(clean_menu_title("Save &as"), "Save as");
    }

    #[test]
    fn background_registry_cascade_resolves_powershell_children() {
        let mut commands = std::collections::HashMap::new();
        let items = registry_cascade_items("powershell7x64", &mut commands)
            .expect("PowerShell 7 background cascade is registered on this machine");
        assert_eq!(
            items
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            ["Open here", "Open here as Administrator"]
        );
        assert_eq!(commands.len(), 2);
        assert!(commands.values().any(|command| command.elevated));
        assert!(commands.values().any(|command| !command.elevated));
    }

    #[test]
    fn submenu_token_is_not_a_native_handle_or_command_id() {
        let token: ShellMenuSubmenuToken = 7;
        let item = ClassicMenuItem {
            command_id: None,
            title: "Open with".to_owned(),
            verb: None,
            enabled: true,
            checked: false,
            default: false,
            kind: ClassicMenuItemKind::Submenu {
                token,
                items: Vec::new(),
            },
        };
        assert!(matches!(
            item.kind,
            ClassicMenuItemKind::Submenu { token: 7, .. }
        ));
        assert_eq!(item.command_id, None);
    }

    #[test]
    #[ignore = "requires the real Windows shell extensions installed on this machine"]
    fn probe_background_and_selected_folder_submenus() {
        fn probe(name: &str, session: &mut ClassicMenuSession) {
            let items = session.items_top_level().expect("read top-level menu");
            eprintln!("{name}: top_level_count={}", items.len());
            for item in &items {
                if let ClassicMenuItemKind::Submenu { token, .. } = item.kind {
                    let children = session.load_submenu(token).expect("load submenu");
                    eprintln!(
                        "{name}: submenu={:?} command_id={:?} verb={:?} token={token} child_count={} children={:?}",
                        item.title,
                        item.command_id,
                        item.verb,
                        children.len(),
                        children
                            .iter()
                            .map(|child| child.title.as_str())
                            .collect::<Vec<_>>()
                    );
                }
            }
        }

        let folder = std::env::current_dir().expect("current directory");
        let mut background = ClassicMenuSession::for_background_with_owner(&folder, false, 0)
            .expect("create background menu");
        probe("background", &mut background);
        let mut selected = ClassicMenuSession::for_paths_with_owner(&[folder], false, 0)
            .expect("create selected-folder menu");
        probe("selected-folder", &mut selected);
    }

    #[test]
    #[ignore = "requires the real Windows shell sharing extension"]
    fn probe_background_sharing_after_selected_folder_menu() {
        let folder = std::env::current_dir().expect("current directory");
        let mut selected =
            ClassicMenuSession::for_paths_with_owner(std::slice::from_ref(&folder), false, 0)
                .expect("create selected-folder menu");
        let selected_items = selected.items_top_level().expect("selected top level");
        let selected_token = selected_items
            .iter()
            .find_map(|item| match &item.kind {
                ClassicMenuItemKind::Submenu { token, .. }
                    if item.verb.as_deref() == Some("windows.share") =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .expect("selected sharing submenu");
        let selected_children = selected
            .load_submenu(selected_token)
            .expect("selected sharing");
        eprintln!("selected sharing children={selected_children:?}");

        let mut background = ClassicMenuSession::for_background_with_owner(&folder, false, 0)
            .expect("create background menu");
        let background_items = background.items_top_level().expect("background top level");
        let background_token = background_items
            .iter()
            .find_map(|item| match &item.kind {
                ClassicMenuItemKind::Submenu { token, .. }
                    if item.verb.as_deref() == Some("windows.share") =>
                {
                    Some(*token)
                }
                _ => None,
            })
            .expect("background sharing submenu");
        let background_children = background
            .load_submenu(background_token)
            .expect("background sharing");
        eprintln!("background sharing children={background_children:?}");
    }
}
