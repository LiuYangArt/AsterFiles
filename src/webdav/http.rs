//! HTTP/WebDAV client. Redirects are manual so control requests stay on the origin
//! and download hops can drop credentials before they leave that origin.
use std::{
    collections::HashSet,
    io::{self, Read, Write},
    path::Path,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    time::{Duration, Instant},
};

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, LOCATION};
use tokio::{runtime::Runtime, task::AbortHandle};
use url::Url;

use crate::{
    operation_audit,
    webdav::{
        path::{PathError, ResourcePath, endpoint_url},
        xml::{self, ListedItem, ParseFinish, XmlFailure},
    },
};

const IDLE_LIMIT: Duration = Duration::from_secs(60);
const MAX_REDIRECTS: u32 = 5;
const BATCH_ITEMS: usize = 256;
const BATCH_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    Cancelled,
    Unauthorized,
    Forbidden,
    NotFound,
    TooManyRequests,
    Server(u16),
    Certificate,
    Redirect(RedirectReject),
    ExpiredLink,
    Network,
    TimedOut,
    InvalidXml,
    Incomplete(String),
    Conflict,
    InvalidName,
    InsufficientSpace,
    Protocol(&'static str),
}

impl Failure {
    pub fn code(&self) -> String {
        match self {
            Self::Cancelled => "cancelled".to_owned(),
            Self::Unauthorized => "unauthorized".to_owned(),
            Self::Forbidden => "forbidden".to_owned(),
            Self::NotFound => "not_found".to_owned(),
            Self::TooManyRequests => "too_many_requests".to_owned(),
            Self::Server(status) => format!("server:{status}"),
            Self::Certificate => "certificate".to_owned(),
            Self::Redirect(reason) => format!("redirect:{reason:?}"),
            Self::ExpiredLink => "expired_link".to_owned(),
            Self::Network => "network".to_owned(),
            Self::TimedOut => "timed_out".to_owned(),
            Self::InvalidXml => "invalid_xml".to_owned(),
            Self::Incomplete(_) => "incomplete".to_owned(),
            Self::Conflict => "conflict".to_owned(),
            Self::InvalidName => "invalid_name".to_owned(),
            Self::InsufficientSpace => "insufficient_space".to_owned(),
            Self::Protocol(code) => (*code).to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectReject {
    Downgrade,
    Loop,
    TooMany,
    CrossOrigin,
    Scheme,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub connection_id: u64,
    pub revision: u64,
    pub https: bool,
    pub host: String,
    pub port: u16,
    pub root: ResourcePath,
    pub username: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListFinish {
    pub incomplete: Option<String>,
    pub error_responses: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadFinish {
    pub path: std::path::PathBuf,
    pub bytes: u64,
    pub length_known: bool,
}

pub fn list_children(
    endpoint: &Endpoint,
    password: &str,
    directory: &ResourcePath,
    cancel: &Arc<AtomicBool>,
    on_batch: &mut dyn FnMut(Vec<ListedItem>) -> io::Result<()>,
) -> Result<ListFinish, Failure> {
    if !directory.starts_with(&endpoint.root) {
        return Err(Failure::Protocol("outside_root"));
    }
    let url = collection_url(endpoint, directory)?;
    let (tx, rx) = mpsc::sync_channel(8);
    let pump = Pump {
        method: "PROPFIND".to_owned(),
        url,
        username: endpoint.username.clone(),
        password: password.to_owned(),
        body: Some(PROPFIND_BODY.to_owned()),
        content_type: Some("application/xml; charset=utf-8"),
        depth: Some("1"),
        allow_cross_origin: false,
        connection_id: endpoint.connection_id,
        cancel: Arc::clone(cancel),
        tx,
    };
    let runtime = runtime()?;
    let handle = runtime.spawn(async move { pump.run().await });
    let parsed = parse_listing(rx, endpoint, directory, cancel, on_batch);
    handle.abort();
    parsed
}

pub fn download_file(
    endpoint: &Endpoint,
    password: &str,
    source: &ResourcePath,
    destination: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<DownloadFinish, Failure> {
    let name = source
        .name()
        .ok_or(Failure::Protocol("directory_download"))?;
    validate_leaf(name)?;
    if destination.exists() {
        return Err(Failure::Conflict);
    }
    let parent = destination
        .parent()
        .ok_or(Failure::Protocol("destination_parent"))?;
    let temp = exclusive_temp(parent, name)?;
    let url = file_url(endpoint, source)?;
    let result = download_to_path(endpoint, password, url, &temp, cancel, on_progress);
    match result {
        Ok(finish) => match std::fs::rename(&temp, destination) {
            Ok(()) => Ok(DownloadFinish {
                path: destination.to_path_buf(),
                bytes: finish.bytes,
                length_known: finish.length_known,
            }),
            Err(error) => {
                let _ = std::fs::remove_file(&temp);
                if destination.exists() {
                    Err(Failure::Conflict)
                } else {
                    Err(io_failure(error))
                }
            }
        },
        Err(error) => {
            let _ = std::fs::remove_file(&temp);
            Err(error)
        }
    }
}

pub fn open_file_body(
    endpoint: &Endpoint,
    password: &str,
    source: &ResourcePath,
    cancel: &Arc<AtomicBool>,
) -> Result<Box<dyn Read + Send>, Failure> {
    let name = source
        .name()
        .ok_or(Failure::Protocol("directory_download"))?;
    validate_leaf(name)?;
    let url = file_url(endpoint, source)?;
    let (tx, rx) = mpsc::sync_channel(8);
    let pump = Pump {
        method: "GET".to_owned(),
        url,
        username: endpoint.username.clone(),
        password: password.to_owned(),
        body: None,
        content_type: None,
        depth: None,
        allow_cross_origin: true,
        connection_id: endpoint.connection_id,
        cancel: Arc::clone(cancel),
        tx,
    };
    let task = runtime()?
        .spawn(async move { pump.run().await })
        .abort_handle();
    Ok(Box::new(FileBody {
        rx,
        pending: Vec::new(),
        position: 0,
        eof: false,
        cancel: Arc::clone(cancel),
        last_byte: Instant::now(),
        saw_head: false,
        expected: None,
        read_bytes: 0,
        task,
    }))
}

fn download_to_path(
    endpoint: &Endpoint,
    password: &str,
    url: Url,
    temp: &Path,
    cancel: &Arc<AtomicBool>,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<DownloadFinish, Failure> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)
        .map_err(io_failure)?;
    let (tx, rx) = mpsc::sync_channel(8);
    let pump = Pump {
        method: "GET".to_owned(),
        url,
        username: endpoint.username.clone(),
        password: password.to_owned(),
        body: None,
        content_type: None,
        depth: None,
        allow_cross_origin: true,
        connection_id: endpoint.connection_id,
        cancel: Arc::clone(cancel),
        tx,
    };
    let runtime = runtime()?;
    let handle = runtime.spawn(async move { pump.run().await });
    let written = write_body(&mut file, rx, cancel, on_progress);
    handle.abort();
    let written = match written {
        Ok(written) => written,
        Err(error) => {
            drop(file);
            return Err(error);
        }
    };
    file.flush().map_err(io_failure)?;
    drop(file);
    Ok(DownloadFinish {
        path: temp.to_path_buf(),
        bytes: written.bytes,
        length_known: written.length_known,
    })
}

struct Written {
    bytes: u64,
    length_known: bool,
}

struct FileBody {
    rx: Receiver<Msg>,
    pending: Vec<u8>,
    position: usize,
    eof: bool,
    cancel: Arc<AtomicBool>,
    last_byte: Instant,
    saw_head: bool,
    expected: Option<u64>,
    read_bytes: u64,
    task: AbortHandle,
}

impl Drop for FileBody {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.task.abort();
    }
}

impl Read for FileBody {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        while self.position >= self.pending.len() && !self.eof {
            if self.cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Msg::Head { status, length }) => {
                    self.saw_head = true;
                    self.expected = length;
                    self.last_byte = Instant::now();
                    if let Some(failure) = failure_for_status(status, false) {
                        return Err(io::Error::other(failure.code()));
                    }
                }
                Ok(Msg::Data(bytes)) => {
                    if !self.saw_head {
                        return Err(io::Error::other("missing head"));
                    }
                    self.pending = bytes;
                    self.position = 0;
                    self.last_byte = Instant::now();
                }
                Ok(Msg::Eof) => {
                    if !self.saw_head {
                        return Err(io::Error::other("missing head"));
                    }
                    if self
                        .expected
                        .is_some_and(|expected| self.read_bytes != expected)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "length mismatch",
                        ));
                    }
                    self.eof = true;
                }
                Ok(Msg::Failure(failure)) => return Err(io::Error::other(failure.code())),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.last_byte.elapsed() > IDLE_LIMIT {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "idle"));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected"));
                }
            }
        }
        let available = self.pending.len().saturating_sub(self.position);
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.pending[self.position..self.position + count]);
        self.position += count;
        self.read_bytes = self.read_bytes.saturating_add(count as u64);
        if self
            .expected
            .is_some_and(|expected| self.read_bytes > expected)
        {
            return Err(io::Error::other("length mismatch"));
        }
        if self.position == self.pending.len() {
            self.pending.clear();
            self.position = 0;
        }
        Ok(count)
    }
}

fn write_body(
    file: &mut std::fs::File,
    rx: Receiver<Msg>,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64, Option<u64>),
) -> Result<Written, Failure> {
    let mut expected: Option<u64> = None;
    let mut seen_head = false;
    let mut written = 0_u64;
    let mut last_byte = Instant::now();
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(Failure::Cancelled);
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(Msg::Head { status, length }) => {
                seen_head = true;
                if let Some(failure) = failure_for_status(status, false) {
                    return Err(failure);
                }
                expected = length;
                last_byte = Instant::now();
            }
            Ok(Msg::Data(bytes)) => {
                file.write_all(&bytes).map_err(io_failure)?;
                written = written.saturating_add(bytes.len() as u64);
                if expected.is_some_and(|expected| written > expected) {
                    return Err(Failure::Protocol("length_mismatch"));
                }
                last_byte = Instant::now();
                on_progress(written, expected);
            }
            Ok(Msg::Eof) => {
                if !seen_head {
                    return Err(Failure::Network);
                }
                if expected.is_some_and(|expected| written != expected) {
                    return Err(Failure::Protocol("length_mismatch"));
                }
                return Ok(Written {
                    bytes: written,
                    length_known: expected.is_some(),
                });
            }
            Ok(Msg::Failure(failure)) => return Err(failure),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_byte.elapsed() > IDLE_LIMIT {
                    return Err(Failure::TimedOut);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return Err(Failure::Network),
        }
    }
}

fn parse_listing(
    rx: Receiver<Msg>,
    endpoint: &Endpoint,
    directory: &ResourcePath,
    cancel: &AtomicBool,
    on_batch: &mut dyn FnMut(Vec<ListedItem>) -> io::Result<()>,
) -> Result<ListFinish, Failure> {
    let mut reader = ChannelRead {
        rx,
        pending: Vec::new(),
        position: 0,
        eof: false,
        failed: None,
        cancel,
        last_byte: Instant::now(),
        saw_head: false,
    };
    let mut batch = Vec::new();
    let mut batch_bytes = 0_usize;
    let mut emitted = false;
    let parsed = xml::parse_propfind(
        std::io::BufReader::new(&mut reader),
        &endpoint.host,
        endpoint.port,
        endpoint.https,
        &endpoint.root,
        directory,
        |item| {
            emitted = true;
            batch_bytes = batch_bytes.saturating_add(item.name.len().saturating_add(64));
            batch.push(item);
            if batch.len() >= BATCH_ITEMS || batch_bytes >= BATCH_BYTES {
                deliver_batch(&mut batch, &mut batch_bytes, on_batch)?;
            }
            if cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            Ok(())
        },
    );
    if let Some(failure) = reader.failed.take() {
        if cancel.load(Ordering::Acquire) && matches!(failure, Failure::Network | Failure::TimedOut)
        {
            return Err(Failure::Cancelled);
        }
        return Err(failure);
    }
    match parsed {
        Ok(stats) => {
            deliver_batch(&mut batch, &mut batch_bytes, on_batch).map_err(callback_failure)?;
            let incomplete = match stats.finish {
                ParseFinish::Incomplete => {
                    Some(format!("error_responses:{}", stats.error_responses))
                }
                ParseFinish::Complete => None,
            };
            Ok(ListFinish {
                incomplete,
                error_responses: stats.error_responses,
            })
        }
        Err(XmlFailure::Io(error)) if error.kind() == io::ErrorKind::Interrupted => {
            Err(Failure::Cancelled)
        }
        Err(XmlFailure::Io(error)) if error.kind() == io::ErrorKind::TimedOut => {
            Err(Failure::TimedOut)
        }
        Err(XmlFailure::Io(error)) if error.kind() == io::ErrorKind::UnexpectedEof && emitted => {
            let _ = deliver_batch(&mut batch, &mut batch_bytes, on_batch);
            Err(Failure::Incomplete("xml".to_owned()))
        }
        Err(XmlFailure::Io(error)) => Err(io_failure(error)),
        Err(XmlFailure::Invalid { emitted: true } | XmlFailure::Limit { emitted: true }) => {
            let _ = deliver_batch(&mut batch, &mut batch_bytes, on_batch);
            Err(Failure::Incomplete("xml".to_owned()))
        }
        Err(XmlFailure::Invalid { .. } | XmlFailure::Limit { .. }) => Err(Failure::InvalidXml),
    }
}

fn deliver_batch(
    batch: &mut Vec<ListedItem>,
    batch_bytes: &mut usize,
    on_batch: &mut dyn FnMut(Vec<ListedItem>) -> io::Result<()>,
) -> io::Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let items = std::mem::take(batch);
    *batch_bytes = 0;
    on_batch(items)
}

struct ChannelRead<'a> {
    rx: Receiver<Msg>,
    pending: Vec<u8>,
    position: usize,
    eof: bool,
    failed: Option<Failure>,
    cancel: &'a AtomicBool,
    last_byte: Instant,
    saw_head: bool,
}

impl Read for ChannelRead<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        while self.position >= self.pending.len() && !self.eof {
            if self.cancel.load(Ordering::Acquire) {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
            }
            match self.rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Msg::Head { status, .. }) => {
                    self.saw_head = true;
                    self.last_byte = Instant::now();
                    if let Some(failure) = failure_for_status(status, false) {
                        self.failed = Some(failure);
                        return Err(io::Error::other("http status"));
                    }
                }
                Ok(Msg::Data(bytes)) => {
                    self.pending = bytes;
                    self.position = 0;
                    self.last_byte = Instant::now();
                }
                Ok(Msg::Eof) => self.eof = true,
                Ok(Msg::Failure(failure)) => {
                    self.failed = Some(failure);
                    return Err(io::Error::other("webdav"));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if self.last_byte.elapsed() > IDLE_LIMIT {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "idle"));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    if self.cancel.load(Ordering::Acquire) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled"));
                    }
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "disconnected"));
                }
            }
        }
        let available = self.pending.len().saturating_sub(self.position);
        let count = available.min(output.len());
        output[..count].copy_from_slice(&self.pending[self.position..self.position + count]);
        self.position += count;
        if self.position == self.pending.len() {
            self.pending.clear();
            self.position = 0;
        }
        Ok(count)
    }
}

enum Msg {
    Head { status: u16, length: Option<u64> },
    Data(Vec<u8>),
    Eof,
    Failure(Failure),
}

struct Pump {
    method: String,
    url: Url,
    username: String,
    password: String,
    body: Option<String>,
    content_type: Option<&'static str>,
    depth: Option<&'static str>,
    allow_cross_origin: bool,
    connection_id: u64,
    cancel: Arc<AtomicBool>,
    tx: SyncSender<Msg>,
}

impl Pump {
    async fn run(mut self) {
        let started = Instant::now();
        let result = self.exchange().await;
        self.password.clear();
        if let Err(failure) = result {
            audit(
                &self.method,
                &self.url,
                None,
                started.elapsed().as_millis(),
                Some(failure.code()),
                self.connection_id,
            );
            let _ = self.tx.send(Msg::Failure(failure));
        }
    }

    async fn exchange(&self) -> Result<(), Failure> {
        let client = http_client()?;
        let mut url = self.url.clone();
        let mut method = self.method.clone();
        let mut body = self.body.clone();
        let mut authorization = Some(basic_header(&self.username, &self.password)?);
        let mut seen = HashSet::new();
        let mut crossed = false;
        for hop in 0..=MAX_REDIRECTS {
            if self.cancel.load(Ordering::Acquire) {
                return Err(Failure::Cancelled);
            }
            let key = format!("{method} {}", redact_url(&url));
            if !seen.insert(key) {
                return Err(Failure::Redirect(RedirectReject::Loop));
            }
            let mut headers = HeaderMap::new();
            if let Some(authorization) = authorization.as_ref().filter(|_| !crossed) {
                headers.insert(AUTHORIZATION, authorization.clone());
            }
            if let Some(content_type) = self.content_type {
                headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
            }
            if let Some(depth) = self.depth {
                headers.insert(
                    reqwest::header::HeaderName::from_static("depth"),
                    HeaderValue::from_static(depth),
                );
            }
            let mut request = client
                .request(parse_method(&method)?, url.clone())
                .headers(headers);
            if let Some(body) = body.as_ref() {
                request = request.body(body.clone());
            }
            let response = tokio::select! {
                biased;
                _ = cancelled(&self.cancel) => return Err(Failure::Cancelled),
                response = request.send() => response,
            };
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    if self.cancel.load(Ordering::Acquire) {
                        return Err(Failure::Cancelled);
                    }
                    return Err(reqwest_failure(&error));
                }
            };
            let status = response.status().as_u16();
            audit(
                &method,
                response.url(),
                Some(status),
                0,
                None,
                self.connection_id,
            );
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                if hop == MAX_REDIRECTS {
                    return Err(Failure::Redirect(RedirectReject::TooMany));
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or(Failure::Protocol("redirect_location"))?;
                let next = url
                    .join(location)
                    .map_err(|_| Failure::Protocol("redirect_location"))?;
                let decision = decide_redirect(&url, &next, self.allow_cross_origin)?;
                if decision.cross_origin {
                    authorization = None;
                    crossed = true;
                    if !self.allow_cross_origin {
                        return Err(Failure::Redirect(RedirectReject::CrossOrigin));
                    }
                }
                if matches!(status, 303) || (decision.cross_origin && matches!(status, 301 | 302)) {
                    method = "GET".to_owned();
                    body = None;
                }
                url = next;
                continue;
            }
            if crossed && matches!(status, 401 | 403 | 410) {
                return Err(Failure::ExpiredLink);
            }
            let length = response.content_length();
            self.send(Msg::Head { status, length }).await?;
            if !(200..300).contains(&status) && status != 207 {
                return Ok(());
            }
            let mut response = response;
            let mut idle = tokio::time::Instant::now() + IDLE_LIMIT;
            loop {
                tokio::select! {
                    biased;
                    _ = cancelled(&self.cancel) => return Err(Failure::Cancelled),
                    _ = tokio::time::sleep_until(idle) => return Err(Failure::TimedOut),
                    chunk = response.chunk() => {
                        match chunk {
                            Ok(Some(chunk)) => {
                                idle = tokio::time::Instant::now() + IDLE_LIMIT;
                                self.send(Msg::Data(chunk.to_vec())).await?;
                            }
                            Ok(None) => {
                                self.send(Msg::Eof).await?;
                                return Ok(());
                            }
                            Err(error) => {
                                if self.cancel.load(Ordering::Acquire) {
                                    return Err(Failure::Cancelled);
                                }
                                return Err(reqwest_failure(&error));
                            }
                        }
                    }
                }
            }
        }
        Err(Failure::Redirect(RedirectReject::TooMany))
    }

    async fn send(&self, message: Msg) -> Result<(), Failure> {
        let mut message = Some(message);
        while let Some(pending) = message.take() {
            match self.tx.try_send(pending) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Disconnected(_)) => return Err(Failure::Cancelled),
                Err(TrySendError::Full(pending)) => {
                    if self.cancel.load(Ordering::Acquire) {
                        return Err(Failure::Cancelled);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    message = Some(pending);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedirectDecision {
    pub cross_origin: bool,
}

pub fn decide_redirect(
    current: &Url,
    next: &Url,
    allow_cross_origin: bool,
) -> Result<RedirectDecision, Failure> {
    if next.scheme() != "https" && next.scheme() != "http" {
        return Err(Failure::Redirect(RedirectReject::Scheme));
    }
    if current.scheme() == "https" && next.scheme() == "http" {
        return Err(Failure::Redirect(RedirectReject::Downgrade));
    }
    let cross_origin = !same_origin(current, next);
    if cross_origin && !allow_cross_origin {
        return Err(Failure::Redirect(RedirectReject::CrossOrigin));
    }
    Ok(RedirectDecision { cross_origin })
}

fn same_origin(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host_str().eq_ignore_ascii_case(&right.host_str())
        && left.port_or_known_default() == right.port_or_known_default()
}

trait HostEq {
    fn eq_ignore_ascii_case(self, other: &Option<&str>) -> bool;
}

impl HostEq for Option<&str> {
    fn eq_ignore_ascii_case(self, other: &Option<&str>) -> bool {
        match (self, other) {
            (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
            (None, None) => true,
            _ => false,
        }
    }
}

fn failure_for_status(status: u16, direct_link: bool) -> Option<Failure> {
    if (200..300).contains(&status) || status == 207 {
        return None;
    }
    Some(match status {
        401 if direct_link => Failure::ExpiredLink,
        401 => Failure::Unauthorized,
        403 if direct_link => Failure::ExpiredLink,
        403 => Failure::Forbidden,
        404 => Failure::NotFound,
        410 if direct_link => Failure::ExpiredLink,
        429 => Failure::TooManyRequests,
        _ => Failure::Server(status),
    })
}

fn collection_url(endpoint: &Endpoint, directory: &ResourcePath) -> Result<Url, Failure> {
    endpoint_url(
        endpoint.https,
        &endpoint.host,
        endpoint.port,
        directory,
        true,
    )
    .map_err(path_failure)
}

fn file_url(endpoint: &Endpoint, source: &ResourcePath) -> Result<Url, Failure> {
    if !source.starts_with(&endpoint.root) {
        return Err(Failure::Protocol("outside_root"));
    }
    endpoint_url(endpoint.https, &endpoint.host, endpoint.port, source, false).map_err(path_failure)
}

fn path_failure(error: PathError) -> Failure {
    match error {
        PathError::OutsideRoot => Failure::Protocol("outside_root"),
        _ => Failure::Protocol("invalid_url"),
    }
}

fn validate_leaf(name: &str) -> Result<(), Failure> {
    crate::fs::file_operations::validate_name(std::ffi::OsStr::new(name))
        .map_err(|_| Failure::InvalidName)
}

fn exclusive_temp(parent: &Path, name: &str) -> Result<std::path::PathBuf, Failure> {
    for suffix in 0..32_u32 {
        let file_name = if suffix == 0 {
            format!("{name}.asterfiles-part")
        } else {
            format!("{name}.asterfiles-part-{suffix}")
        };
        let candidate = parent.join(file_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(Failure::Conflict)
}

fn io_failure(error: io::Error) -> Failure {
    if error.kind() == io::ErrorKind::Interrupted {
        return Failure::Cancelled;
    }
    if error.raw_os_error() == Some(112)
        || error.kind() == io::ErrorKind::StorageFull
        || error.kind() == io::ErrorKind::QuotaExceeded
    {
        return Failure::InsufficientSpace;
    }
    if error.kind() == io::ErrorKind::AlreadyExists {
        return Failure::Conflict;
    }
    Failure::Network
}

fn callback_failure(error: io::Error) -> Failure {
    if error.kind() == io::ErrorKind::Interrupted {
        Failure::Cancelled
    } else {
        Failure::Network
    }
}

fn reqwest_failure(error: &reqwest::Error) -> Failure {
    let text = error.to_string().to_ascii_lowercase();
    if text.contains("certificate") || text.contains("cert") {
        Failure::Certificate
    } else if error.is_timeout() {
        Failure::TimedOut
    } else {
        Failure::Network
    }
}

fn parse_method(method: &str) -> Result<reqwest::Method, Failure> {
    reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| Failure::Protocol("method"))
}

fn basic_header(username: &str, password: &str) -> Result<HeaderValue, Failure> {
    let token = base64(&format!("{username}:{password}").into_bytes());
    HeaderValue::from_str(&format!("Basic {token}")).map_err(|_| Failure::Protocol("authorization"))
}

fn base64(input: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::new();
    for chunk in input.chunks(3) {
        let first = chunk[0] as u32;
        let second = chunk.get(1).copied().unwrap_or(0) as u32;
        let third = chunk.get(2).copied().unwrap_or(0) as u32;
        let value = (first << 16) | (second << 8) | third;
        output.push(TABLE[((value >> 18) & 63) as usize] as char);
        output.push(TABLE[((value >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            output.push(TABLE[((value >> 6) & 63) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(TABLE[(value & 63) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

pub fn redact_url(url: &Url) -> String {
    let mut cleaned = url.clone();
    let _ = cleaned.set_username("");
    let _ = cleaned.set_password(None);
    cleaned.set_query(None);
    cleaned.to_string()
}

fn audit(
    method: &str,
    url: &Url,
    status: Option<u16>,
    elapsed_ms: u128,
    error: Option<String>,
    connection: u64,
) {
    operation_audit::record(
        "webdav",
        format!(
            "connection={connection} method={method} status={} elapsed_ms={elapsed_ms} url={} error={}",
            status
                .map(|status| status.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            redact_url(url),
            error.unwrap_or_else(|| "-".to_owned())
        ),
    );
}

fn runtime() -> Result<&'static Runtime, Failure> {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    if RUNTIME.get().is_none() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("asterfiles-webdav")
            .build()
            .map_err(|_| Failure::Protocol("runtime"))?;
        let _ = RUNTIME.set(runtime);
    }
    RUNTIME.get().ok_or(Failure::Protocol("runtime"))
}

fn http_client() -> Result<&'static reqwest::Client, Failure> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if CLIENT.get().is_none() {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .build()
            .map_err(|_| Failure::Protocol("client"))?;
        let _ = CLIENT.set(client);
    }
    CLIENT.get().ok_or(Failure::Protocol("client"))
}

async fn cancelled(cancel: &AtomicBool) {
    loop {
        if cancel.load(Ordering::Acquire) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

const PROPFIND_BODY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:">
  <d:prop>
    <d:displayname/>
    <d:resourcetype/>
    <d:getcontentlength/>
    <d:getlastmodified/>
  </d:prop>
</d:propfind>"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webdav_redirect_policy_rejects_downgrade_loops_and_cross_origin_control() {
        let https = Url::parse("https://nas.example:5244/dav/mount/").unwrap();
        let http = Url::parse("http://nas.example:5244/dav/mount/").unwrap();
        let other = Url::parse("https://cdn.example/file").unwrap();
        assert_eq!(
            decide_redirect(&https, &http, true),
            Err(Failure::Redirect(RedirectReject::Downgrade))
        );
        assert_eq!(
            decide_redirect(&https, &other, false),
            Err(Failure::Redirect(RedirectReject::CrossOrigin))
        );
        assert!(decide_redirect(&https, &other, true).unwrap().cross_origin);
        let mut seen = HashSet::new();
        let key = format!("GET {}", redact_url(&https));
        assert!(seen.insert(key.clone()));
        assert!(!seen.insert(key));
        let signed = Url::parse("https://cdn.example/file?sig=secret-token").unwrap();
        assert!(!redact_url(&signed).contains("secret-token"));
        assert!(!redact_url(&signed).contains('?'));
    }
}
