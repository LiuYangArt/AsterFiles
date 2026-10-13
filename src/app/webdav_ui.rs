//! WebDAV locations, downloads, and the commands that must stay out of the local path pipeline.
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Instant,
};

use slint::ComponentHandle;

use crate::{
    domain::{NavigationKind, NavigationLocation, TabId},
    i18n::Language,
    network::{NetworkLocationCatalog, NetworkTarget, WebDavConnection},
    webdav::{self, Endpoint, Failure},
};

use super::{
    AppState, DirectoryRequest, SharedSessions, show_centered_dialog, submit_webdav_navigation,
};

const TRANSFER_ID_BASE: u64 = 1_600_000_000;

#[derive(Clone)]
struct Transfer {
    id: u64,
    title: String,
    destination: String,
    current: String,
    bytes: u64,
    state: i32,
    message: String,
    cancel: Arc<AtomicBool>,
    started: Instant,
}

fn transfers() -> &'static Mutex<Vec<Transfer>> {
    static TRANSFERS: Mutex<Vec<Transfer>> = Mutex::new(Vec::new());
    &TRANSFERS
}

fn next_transfer_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(TRANSFER_ID_BASE);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(super) fn transfer_rows(language: Language) -> Vec<super::OperationRow> {
    let Ok(jobs) = transfers().lock() else {
        return Vec::new();
    };
    jobs.iter()
        .map(|job| super::OperationRow {
            id: job.id as i32,
            title: match language {
                Language::Chinese => "下载",
                Language::English => "Download",
            }
            .into(),
            status: job.message.clone().into(),
            queue: String::new().into(),
            failure_summary: if job.state == 8 {
                job.message.clone().into()
            } else {
                String::new().into()
            },
            accessible_label: job.title.clone().into(),
            source_line: job.title.clone().into(),
            destination_line: job.destination.clone().into(),
            current_item_line: job.current.clone().into(),
            percent: String::new().into(),
            file_progress: format!("{} B", job.bytes).into(),
            transferred: format!("{} B", job.bytes).into(),
            speed: String::new().into(),
            eta: "—".into(),
            progress: 0.0,
            progress_known: false,
            state: job.state,
            paused: false,
            pause_pending: false,
            cancel_pending: job.state == 4,
            can_pause: false,
            can_cancel: job.state == 2,
            can_retry: false,
        })
        .collect()
}

pub(super) fn cancel_transfer(id: u64) -> bool {
    let Ok(mut jobs) = transfers().lock() else {
        return false;
    };
    let Some(job) = jobs
        .iter_mut()
        .find(|job| job.id == id && (job.state == 2 || job.state == 4))
    else {
        return false;
    };
    job.cancel.store(true, Ordering::Release);
    job.state = 4;
    true
}

pub(super) fn has_transfer() -> bool {
    transfers().lock().is_ok_and(|jobs| !jobs.is_empty())
}

pub(super) fn has_active_transfer() -> bool {
    transfers()
        .lock()
        .is_ok_and(|jobs| jobs.iter().any(|job| job.state == 2 || job.state == 4))
}

pub(super) fn prune_finished_transfers() {
    let Ok(mut jobs) = transfers().lock() else {
        return;
    };
    jobs.retain(|job| job.state == 2 || job.state == 4 || job.started.elapsed().as_secs() < 10);
}

pub(super) fn configure_dialog(ui: &super::WebDavLocationWindow, language: Language, dark: bool) {
    ui.set_dark_theme(dark);
    ui.set_error_text("".into());
    match language {
        Language::Chinese => {
            ui.set_title_text(if ui.get_location_id().is_empty() {
                "添加 WebDAV 位置".into()
            } else {
                "编辑 WebDAV 位置".into()
            });
            ui.set_name_label("显示名称".into());
            ui.set_host_label("主机".into());
            ui.set_port_label("端口".into());
            ui.set_root_label("根路径".into());
            ui.set_user_label("用户名".into());
            ui.set_password_label("密码".into());
            ui.set_https_label("使用 HTTPS".into());
            ui.set_cancel_text("取消".into());
            ui.set_save_text("保存".into());
            ui.set_close_text("关闭".into());
        }
        Language::English => {
            ui.set_title_text(if ui.get_location_id().is_empty() {
                "Add WebDAV location".into()
            } else {
                "Edit WebDAV location".into()
            });
            ui.set_name_label("Display name".into());
            ui.set_host_label("Host".into());
            ui.set_port_label("Port".into());
            ui.set_root_label("Root path".into());
            ui.set_user_label("Username".into());
            ui.set_password_label("Password".into());
            ui.set_https_label("Use HTTPS".into());
            ui.set_cancel_text("Cancel".into());
            ui.set_save_text("Save".into());
            ui.set_close_text("Close".into());
        }
    }
}

pub(super) fn show_new_dialog(
    ui: &super::WebDavLocationWindow,
    owner: &super::AppWindow,
    app: &AppState,
) {
    ui.set_location_id("".into());
    ui.set_name("".into());
    ui.set_host("".into());
    ui.set_port("443".into());
    ui.set_root("/".into());
    ui.set_username("".into());
    ui.set_password("".into());
    ui.set_https(true);
    configure_dialog(ui, app.language, app.dark_theme());
    show_centered_dialog(owner, ui);
}

pub(super) fn show_edit_dialog(
    ui: &super::WebDavLocationWindow,
    owner: &super::AppWindow,
    app: &AppState,
    id: u64,
) {
    let Some(location) = app
        .network_locations
        .iter()
        .find(|location| location.id == id)
    else {
        return;
    };
    let NetworkTarget::WebDav(connection) = &location.target else {
        return;
    };
    ui.set_location_id(id.to_string().into());
    ui.set_name(location.display_name.clone().into());
    ui.set_host(connection.host.clone().into());
    ui.set_port(connection.port.to_string().into());
    ui.set_root(connection.root.clone().into());
    ui.set_username(connection.username.clone().into());
    ui.set_password("".into());
    ui.set_https(connection.https);
    configure_dialog(ui, app.language, app.dark_theme());
    show_centered_dialog(owner, ui);
}

pub(super) fn save_from_dialog(ui: &super::WebDavLocationWindow, app: &mut AppState) -> bool {
    let mut password = ui.get_password().to_string();
    let typed_password = !password.is_empty();
    let https = ui.get_https();
    let port = ui.get_port().parse::<u16>().unwrap_or(0);
    let editing = ui.get_location_id().parse::<u64>().ok();
    let previous = editing.and_then(|id| {
        app.network_locations
            .iter()
            .find(|location| location.id == id)
            .and_then(|location| match &location.target {
                NetworkTarget::WebDav(connection) => Some(connection.clone()),
                _ => None,
            })
    });
    let revision = previous
        .as_ref()
        .map(|connection| connection.revision)
        .unwrap_or(1);
    let endpoint = match webdav::validate_endpoint(
        ui.get_name().as_str(),
        https,
        ui.get_host().as_str(),
        port,
        ui.get_root().as_str(),
        ui.get_username().as_str(),
        revision,
    ) {
        Ok(endpoint) => endpoint,
        Err(_) => {
            ui.set_error_text(match app.language {
                Language::Chinese => "请检查名称、主机、端口、根路径和用户名".into(),
                Language::English => "Check the name, host, port, root path, and username".into(),
            });
            return false;
        }
    };
    if password.is_empty()
        && let Some(previous) = previous.as_ref()
        && let Ok(Some(existing)) = webdav::load_password(previous.connection_id)
    {
        password = existing;
    }
    if password.is_empty() && previous.is_none() {
        ui.set_error_text(match app.language {
            Language::Chinese => "新建位置需要密码".into(),
            Language::English => "A new location needs a password".into(),
        });
        return false;
    }
    let endpoint_changed = previous.as_ref().is_some_and(|connection| {
        connection.connection_id != endpoint.connection_id
            || connection.https != endpoint.https
            || connection.host != endpoint.host
            || connection.port != endpoint.port
            || connection.root != endpoint.root.canonical()
            || connection.username != endpoint.username
    });
    let revision = if previous.is_none() {
        1
    } else if endpoint_changed || typed_password {
        previous
            .as_ref()
            .map(|connection| connection.revision.saturating_add(1))
            .unwrap_or(1)
    } else {
        previous
            .as_ref()
            .map(|connection| connection.revision)
            .unwrap_or(1)
    };
    if password.is_empty() && endpoint_changed {
        ui.set_error_text(match app.language {
            Language::Chinese => "更改服务器或用户名时需要重新输入密码".into(),
            Language::English => {
                "Enter the password again when the server or username changes".into()
            }
        });
        return false;
    }
    if (typed_password || endpoint_changed)
        && !password.is_empty()
        && webdav::store_password(endpoint.connection_id, &endpoint.username, &password).is_err()
    {
        ui.set_error_text(match app.language {
            Language::Chinese => "密码没有写入 Windows 凭据".into(),
            Language::English => "The password could not be stored in Windows credentials".into(),
        });
        return false;
    }
    let connection = WebDavConnection {
        connection_id: endpoint.connection_id,
        revision,
        https: endpoint.https,
        host: endpoint.host,
        port: endpoint.port,
        root: endpoint.root.canonical(),
        username: endpoint.username,
    };
    let mut catalog = NetworkLocationCatalog::new(app.network_locations.clone());
    let saved = if let Some(id) = editing {
        catalog.update_webdav(id, endpoint.display_name, connection)
    } else {
        catalog
            .add_webdav(endpoint.display_name, connection)
            .map(|_| ())
    };
    if let Err(error) = saved {
        ui.set_error_text(format!("{error:?}").into());
        return false;
    }
    if let Some(previous) = previous
        .as_ref()
        .filter(|connection| connection.connection_id != endpoint.connection_id)
    {
        let _ = webdav::delete_password(previous.connection_id);
    }
    app.network_locations = catalog.locations().to_vec();
    app.network_location_generation = app.network_location_generation.wrapping_add(1).max(1);
    let connection_id = endpoint.connection_id;
    for window in app.windows.values_mut() {
        for tab in window.tabs.values_mut() {
            let matches_connection = [tab.visible_location(), tab.requested_location().as_ref()]
                .into_iter()
                .flatten()
                .filter_map(NavigationLocation::webdav_place)
                .any(|place| place.connection_id == connection_id);
            if matches_connection {
                tab.cancel_pending();
            }
        }
    }
    ui.set_password("".into());
    true
}

pub(super) struct RemoteDragFile {
    pub name: String,
    pub size: Option<u64>,
    pub open:
        std::sync::Arc<dyn Fn() -> std::io::Result<Box<dyn std::io::Read + Send>> + Send + Sync>,
}

pub(super) fn prepare_remote_drag(
    app: &AppState,
    entries: &[(String, Option<u64>, crate::domain::WebDavEntry)],
) -> Option<Vec<RemoteDragFile>> {
    let first = entries.first()?;
    let (endpoint, password) = stored_endpoint(app, first.2.connection_id)?;
    let mut prepared = Vec::new();
    for (name, size, identity) in entries {
        if identity.connection_id != endpoint.connection_id {
            continue;
        }
        if crate::fs::file_operations::validate_name(std::ffi::OsStr::new(name)).is_err() {
            continue;
        }
        let Ok(source) = webdav::ResourcePath::parse_root(&identity.path) else {
            continue;
        };
        let endpoint = endpoint.clone();
        let password = password.clone();
        prepared.push(RemoteDragFile {
            name: name.clone(),
            size: *size,
            open: std::sync::Arc::new(move || open_remote_reader(&endpoint, &password, &source)),
        });
    }
    (!prepared.is_empty()).then_some(prepared)
}

fn open_remote_reader(
    endpoint: &webdav::Endpoint,
    password: &str,
    source: &webdav::ResourcePath,
) -> std::io::Result<Box<dyn std::io::Read + Send>> {
    let cancel = Arc::new(AtomicBool::new(false));
    let slot = transfer_slots()
        .acquire(endpoint.connection_id, &cancel)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled"))?;
    let body = webdav::open_file_body(endpoint, password, source, &cancel)
        .map_err(|error| std::io::Error::other(error.code()))?;
    Ok(Box::new(SlotReader { body, _slot: slot }))
}

struct SlotReader {
    body: Box<dyn std::io::Read + Send>,
    _slot: webdav::SlotGuard,
}

impl std::io::Read for SlotReader {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        self.body.read(output)
    }
}

fn stored_endpoint(app: &AppState, connection_id: u64) -> Option<(webdav::Endpoint, String)> {
    let connection = app
        .network_locations
        .iter()
        .find_map(|location| match &location.target {
            NetworkTarget::WebDav(connection) if connection.connection_id == connection_id => {
                Some(connection.clone())
            }
            _ => None,
        })?;
    let root = webdav::ResourcePath::parse_root(&connection.root).ok()?;
    let password = webdav::load_password(connection.connection_id)
        .ok()
        .flatten()?;
    if password.is_empty() {
        return None;
    }
    Some((
        webdav::Endpoint {
            connection_id: connection.connection_id,
            revision: connection.revision,
            https: connection.https,
            host: connection.host,
            port: connection.port,
            root,
            username: connection.username,
            display_name: String::new(),
        },
        password,
    ))
}

pub(super) fn start_download(
    state: SharedSessions,
    entries: Vec<crate::domain::FileEntry>,
    open_after: bool,
) {
    let (endpoint, password, parent, language) = {
        let app = state.lock().expect("app state mutex is not poisoned");
        let Some(first) = entries.first().and_then(|entry| entry.webdav.as_ref()) else {
            return;
        };
        let Some(connection) =
            app.network_locations
                .iter()
                .find_map(|location| match &location.target {
                    NetworkTarget::WebDav(connection)
                        if connection.connection_id == first.connection_id =>
                    {
                        Some(connection.clone())
                    }
                    _ => None,
                })
        else {
            return;
        };
        let Ok(root) = webdav::ResourcePath::parse_root(&connection.root) else {
            return;
        };
        let endpoint = Endpoint {
            connection_id: connection.connection_id,
            revision: connection.revision,
            https: connection.https,
            host: connection.host,
            port: connection.port,
            root,
            username: connection.username,
            display_name: String::new(),
        };
        let Ok(Some(password)) = webdav::load_password(connection.connection_id) else {
            if let Ok(mut jobs) = transfers().lock() {
                jobs.push(Transfer {
                    id: next_transfer_id(),
                    title: first.path.clone(),
                    destination: String::new(),
                    current: String::new(),
                    bytes: 0,
                    state: 8,
                    message: failure_text(app.language, &Failure::Unauthorized),
                    cancel: Arc::new(AtomicBool::new(false)),
                    started: Instant::now(),
                });
            }
            return;
        };
        let Ok(parent) = crate::platform::windows::network::downloads_directory() else {
            return;
        };
        (endpoint, password, parent, app.language)
    };
    for entry in entries {
        let Some(identity) = entry.webdav.clone() else {
            continue;
        };
        let Ok(source) = webdav::ResourcePath::parse_root(&identity.path) else {
            continue;
        };
        let endpoint = endpoint.clone();
        let password = password.clone();
        let parent = parent.clone();
        let name = entry.display_name.clone();
        let directory = entry.kind == crate::domain::EntryKind::Directory;
        let cancel = Arc::new(AtomicBool::new(false));
        let id = next_transfer_id();
        if let Ok(mut jobs) = transfers().lock() {
            jobs.push(Transfer {
                id,
                title: name.clone(),
                destination: parent.display().to_string(),
                current: name.clone(),
                bytes: 0,
                state: 2,
                message: match language {
                    Language::Chinese => "正在下载".to_owned(),
                    Language::English => "Downloading".to_owned(),
                },
                cancel: cancel.clone(),
                started: Instant::now(),
            });
        }
        thread::spawn(move || {
            let Some(_slot) = transfer_slots().acquire(endpoint.connection_id, &cancel) else {
                finish_transfer(id, Err(Failure::Cancelled), false, language);
                return;
            };
            let result = if directory {
                webdav::download_tree(
                    &endpoint,
                    &password,
                    &source,
                    &parent,
                    &cancel,
                    &mut |progress| update_transfer(id, progress.bytes, &progress.current),
                )
                .map(|finish| finish.path)
            } else {
                let destination = parent.join(&name);
                webdav::download_file(
                    &endpoint,
                    &password,
                    &source,
                    &destination,
                    &cancel,
                    &mut |bytes, _total| update_transfer(id, bytes, &name),
                )
                .map(|finish| finish.path)
            };
            finish_transfer(id, result, open_after, language);
        });
    }
}

fn transfer_slots() -> Arc<webdav::SlotPool> {
    static SLOTS: std::sync::OnceLock<Arc<webdav::SlotPool>> = std::sync::OnceLock::new();
    SLOTS.get_or_init(webdav::SlotPool::new).clone()
}

fn update_transfer(id: u64, bytes: u64, current: &str) {
    let Ok(mut jobs) = transfers().lock() else {
        return;
    };
    if let Some(job) = jobs.iter_mut().find(|job| job.id == id) {
        job.bytes = bytes;
        job.current = current.to_owned();
    }
}

fn finish_transfer(
    id: u64,
    result: Result<PathBuf, Failure>,
    open_after: bool,
    language: Language,
) {
    let path = match &result {
        Ok(path) => Some(path.clone()),
        Err(_) => None,
    };
    if let Ok(mut jobs) = transfers().lock()
        && let Some(job) = jobs.iter_mut().find(|job| job.id == id)
    {
        match &result {
            Ok(_) => {
                job.state = 5;
                job.message = match language {
                    Language::Chinese => "下载完成".to_owned(),
                    Language::English => "Download complete".to_owned(),
                };
            }
            Err(Failure::Cancelled) => {
                job.state = 6;
                job.message = match language {
                    Language::Chinese => "已取消".to_owned(),
                    Language::English => "Cancelled".to_owned(),
                };
            }
            Err(error) => {
                job.state = 8;
                job.message = failure_text(language, error);
            }
        }
    }
    if open_after && let Some(path) = path {
        let _ = crate::platform::open_path(&path);
    }
}

pub(super) fn listing_error_text(language: Language, message: &str) -> String {
    let Some(code) = message.strip_prefix("webdav:") else {
        return message.to_owned();
    };
    let failure = match code {
        "unauthorized" => Failure::Unauthorized,
        "forbidden" => Failure::Forbidden,
        "not_found" => Failure::NotFound,
        "too_many_requests" => Failure::TooManyRequests,
        "certificate" => Failure::Certificate,
        "expired_link" => Failure::ExpiredLink,
        "network" => Failure::Network,
        "timed_out" => Failure::TimedOut,
        "invalid_xml" => Failure::InvalidXml,
        "conflict" => Failure::Conflict,
        "invalid_name" => Failure::InvalidName,
        "insufficient_space" => Failure::InsufficientSpace,
        "invalid_root" | "invalid_path" => Failure::Protocol("invalid_path"),
        other if other.starts_with("server:") => {
            return match language {
                Language::Chinese => "服务器拒绝了这次浏览".to_owned(),
                Language::English => "The server rejected this listing".to_owned(),
            };
        }
        other if other.starts_with("redirect:") => {
            Failure::Redirect(webdav::RedirectReject::CrossOrigin)
        }
        _ => Failure::Network,
    };
    failure_text(language, &failure)
}

pub(super) fn failure_text(language: Language, failure: &Failure) -> String {
    match (language, failure) {
        (Language::Chinese, Failure::Cancelled) => "已取消".to_owned(),
        (Language::Chinese, Failure::Unauthorized) => "登录被拒绝".to_owned(),
        (Language::Chinese, Failure::Forbidden) => "没有访问权限".to_owned(),
        (Language::Chinese, Failure::NotFound) => "找不到这个位置".to_owned(),
        (Language::Chinese, Failure::TooManyRequests) => "请求过于频繁".to_owned(),
        (Language::Chinese, Failure::Certificate) => "证书校验失败".to_owned(),
        (Language::Chinese, Failure::ExpiredLink) => "直链已失效".to_owned(),
        (Language::Chinese, Failure::Conflict) => "本地已有同名文件，未覆盖".to_owned(),
        (Language::Chinese, Failure::InvalidName) => "名称不能用作 Windows 文件名".to_owned(),
        (Language::Chinese, Failure::InsufficientSpace) => "磁盘空间不足".to_owned(),
        (Language::Chinese, Failure::Redirect(_)) => "重定向被拒绝".to_owned(),
        (Language::Chinese, Failure::Network) => "网络连接中断".to_owned(),
        (Language::Chinese, Failure::TimedOut) => "等待响应超时".to_owned(),
        (Language::Chinese, Failure::InvalidXml) => "目录数据无法读取".to_owned(),
        (Language::Chinese, Failure::Server(_)) => "服务器拒绝了这次请求".to_owned(),
        (Language::Chinese, Failure::Incomplete(_)) => "目录没有完整加载".to_owned(),
        (Language::Chinese, Failure::Protocol(_)) => "连接设置无效".to_owned(),
        (Language::English, Failure::Cancelled) => "Cancelled".to_owned(),
        (Language::English, Failure::Unauthorized) => "Sign-in was rejected".to_owned(),
        (Language::English, Failure::Forbidden) => "Access was denied".to_owned(),
        (Language::English, Failure::NotFound) => "The location was not found".to_owned(),
        (Language::English, Failure::TooManyRequests) => "Too many requests".to_owned(),
        (Language::English, Failure::Certificate) => "The certificate was rejected".to_owned(),
        (Language::English, Failure::ExpiredLink) => "The direct link has expired".to_owned(),
        (Language::English, Failure::Conflict) => {
            "A local file with this name already exists".to_owned()
        }
        (Language::English, Failure::InvalidName) => {
            "The name is not a valid Windows file name".to_owned()
        }
        (Language::English, Failure::InsufficientSpace) => "The disk is full".to_owned(),
        (Language::English, Failure::Redirect(_)) => "The redirect was rejected".to_owned(),
        (Language::English, Failure::Network) => "The network connection failed".to_owned(),
        (Language::English, Failure::TimedOut) => "The server stopped responding".to_owned(),
        (Language::English, Failure::InvalidXml) => {
            "The directory data could not be read".to_owned()
        }
        (Language::English, Failure::Server(_)) => "The server rejected this request".to_owned(),
        (Language::English, Failure::Incomplete(_)) => {
            "The directory listing is incomplete".to_owned()
        }
        (Language::English, Failure::Protocol(_)) => {
            "The connection settings are invalid".to_owned()
        }
    }
}

pub(super) fn open_entry(
    state: &SharedSessions,
    sender: &std::sync::mpsc::Sender<DirectoryRequest>,
    tab_id: TabId,
    entry: &crate::domain::FileEntry,
) {
    if let Some(identity) = entry.webdav.clone() {
        if entry.kind == crate::domain::EntryKind::Directory {
            submit_webdav_navigation(
                sender,
                state,
                tab_id,
                crate::domain::WebDavPlace {
                    connection_id: identity.connection_id,
                    revision: identity.revision,
                    root: identity.root,
                    path: identity.path,
                },
                NavigationKind::Normal,
            );
        } else {
            start_download(state.clone(), vec![entry.clone()], true);
        }
    }
}

pub(super) fn selected_remote_entries(app: &AppState) -> Option<Vec<crate::domain::FileEntry>> {
    let tab = app.active();
    let entries = tab
        .selected
        .iter()
        .filter_map(|id| tab.visible_entry(*id).cloned())
        .filter(|entry| entry.webdav.is_some())
        .collect::<Vec<_>>();
    (!entries.is_empty()).then_some(entries)
}

pub(super) const CMD_ADD_WEBDAV_LOCATION: i32 = 221;
pub(super) const CMD_EDIT_WEBDAV_LOCATION: i32 = 222;
pub(super) const CMD_WEBDAV_DOWNLOAD: i32 = 223;
pub(super) const CMD_WEBDAV_OPEN: i32 = 224;

pub(super) fn entry_menu_rows(
    language: Language,
    directory: bool,
) -> Vec<super::ContextCommandRow> {
    let text = |chinese: &'static str, english: &'static str| {
        if language == Language::Chinese {
            chinese
        } else {
            english
        }
    };
    vec![
        super::quick_menu_row(
            CMD_WEBDAV_OPEN,
            0,
            if directory {
                text("打开", "Open")
            } else {
                text("下载并打开", "Download and open")
            },
            true,
            false,
            false,
        ),
        super::quick_menu_row(
            CMD_WEBDAV_DOWNLOAD,
            0,
            text("下载到“下载”文件夹", "Download to Downloads"),
            true,
            false,
            false,
        ),
    ]
}

pub(super) fn location_is_remote(app: &AppState) -> bool {
    app.active()
        .visible_location()
        .is_some_and(|location| matches!(location, NavigationLocation::WebDav(_)))
}

pub(super) fn wire_window(ui: &super::WebDavLocationWindow, state: SharedSessions) {
    super::configure_secondary_dialog_window(ui);
    ui.window().on_close_requested({
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.invoke_safe_cancel();
            }
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let weak = ui.as_weak();
    ui.on_safe_cancel(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_password("".into());
            ui.set_location_id("".into());
            let _ = ui.hide();
        }
    });
    let weak = ui.as_weak();
    ui.on_drag_window(move || {
        if let Some(ui) = weak.upgrade() {
            super::begin_component_window_drag(&ui);
        }
    });
    let weak = ui.as_weak();
    ui.on_save(move || {
        let Some(ui) = weak.upgrade() else { return };
        let saved = state
            .lock()
            .ok()
            .is_some_and(|mut app| save_from_dialog(&ui, &mut app));
        if saved {
            ui.set_password("".into());
            let _ = ui.hide();
        }
    });
}
