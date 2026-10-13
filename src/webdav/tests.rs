use std::{
    collections::HashMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use super::{
    Endpoint, Failure, ResourcePath, download_file, download_tree, list_children, validate_endpoint,
};

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
}

struct Server {
    port: u16,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve<F>(handler: F) -> (u16, Server)
where
    F: Fn(Request, TcpStream) + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&shutdown);
    let handler = Arc::new(handler);
    let handle = thread::spawn(move || {
        while !flag.load(Ordering::Acquire) {
            let Ok((stream, _)) = listener.accept() else {
                break;
            };
            if flag.load(Ordering::Acquire) {
                break;
            }
            let handler = Arc::clone(&handler);
            thread::spawn(move || {
                if let Ok(request) = read_request(&stream) {
                    handler(request, stream);
                }
            });
        }
    });
    (
        port,
        Server {
            port,
            shutdown,
            handle: Some(handle),
        },
    )
}

fn read_request(mut stream: &TcpStream) -> std::io::Result<Request> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut bytes = Vec::new();
    let mut one = [0_u8; 1];
    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 65_536 {
        if stream.read(&mut one)? == 0 {
            break;
        }
        bytes.push(one[0]);
    }
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.split("\r\n");
    let mut request = lines.next().unwrap_or("").split_whitespace();
    let method = request.next().unwrap_or("").to_owned();
    let path = request.next().unwrap_or("").to_owned();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
        }
    }
    // Closing with an unread body makes Windows reset the socket, which hides the status.
    let length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    let mut remaining = length;
    let mut buffer = [0_u8; 1024];
    while remaining > 0 {
        let take = remaining.min(buffer.len());
        let count = stream.read(&mut buffer[..take])?;
        if count == 0 {
            break;
        }
        remaining -= count;
    }
    Ok(Request {
        method,
        path,
        headers,
    })
}

fn endpoint(port: u16) -> Endpoint {
    validate_endpoint(
        "OpenList",
        false,
        "127.0.0.1",
        port,
        "/dav/mount",
        "reader",
        1,
    )
    .unwrap()
}

fn propfind_item(name: &str, directory: bool, size: Option<u64>) -> String {
    let kind = if directory {
        "<D:resourcetype><D:collection/></D:resourcetype>"
    } else {
        "<D:resourcetype/>"
    };
    let length = size
        .map(|size| format!("<D:getcontentlength>{size}</D:getcontentlength>"))
        .unwrap_or_default();
    format!(
        "<D:response><D:href>/dav/mount/{name}</D:href><D:propstat><D:prop>{kind}{length}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>"
    )
}

fn write_chunk(stream: &mut TcpStream, data: &str) {
    if write!(stream, "{:X}\r\n{data}\r\n", data.len()).is_ok() {
        let _ = stream.flush();
    }
}

#[test]
fn webdav_client_lists_before_the_body_ends_and_keeps_partial_failures() {
    let gate = Arc::new(AtomicBool::new(false));
    let released = Arc::clone(&gate);
    let (port, _server) = serve(move |_request, mut stream| {
        let mut body = String::from(r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">"#);
        for index in 0..300 {
            body.push_str(&propfind_item(&format!("f{index}"), false, Some(1)));
        }
        write!(
            stream,
            "HTTP/1.1 207 Multi-Status\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .unwrap();
        write_chunk(&mut stream, &body);
        thread::sleep(Duration::from_millis(400));
        released.store(true, Ordering::Release);
        let mut rest = String::new();
        for index in 300..400 {
            rest.push_str(&propfind_item(&format!("f{index}"), false, Some(1)));
        }
        rest.push_str("</D:multistatus>");
        write_chunk(&mut stream, &rest);
        write!(stream, "0\r\n\r\n").unwrap();
    });
    let mut early = false;
    let mut count = 0_usize;
    let finish = list_children(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount").unwrap(),
        &Arc::new(AtomicBool::new(false)),
        &mut |items| {
            if !gate.load(Ordering::Acquire) {
                early = true;
            }
            count += items.len();
            Ok(())
        },
    )
    .unwrap();
    assert!(early, "first batch arrived before the server finished");
    assert_eq!(count, 400);
    assert!(finish.incomplete.is_none());

    let (port, _server) = serve(|_request, mut stream| {
        let xml = format!(
            r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">{}<D:response><D:href>/dav/mount/hidden</D:href><D:propstat><D:status>HTTP/1.1 403 Forbidden</D:status></D:propstat></D:response></D:multistatus>"#,
            propfind_item("ok.txt", false, Some(2))
        );
        write!(
            stream,
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\n\r\n{xml}",
            xml.len()
        )
        .unwrap();
    });
    let mut names = Vec::new();
    let finish = list_children(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount").unwrap(),
        &Arc::new(AtomicBool::new(false)),
        &mut |items| {
            names.extend(items.into_iter().map(|item| item.name));
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(names, vec!["ok.txt".to_owned()]);
    assert!(finish.incomplete.is_some());
}

#[test]
fn webdav_client_cancels_a_hung_listing_and_backpressure_within_three_seconds() {
    let (port, _server) = serve(|_request, mut stream| {
        stream.set_nonblocking(true).unwrap();
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(100));
            let mut probe = [0_u8; 1];
            match stream.read(&mut probe) {
                Ok(0) => break,
                Err(error) if error.kind() != std::io::ErrorKind::WouldBlock => break,
                _ => {}
            }
        }
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let started = Instant::now();
    let handle = thread::spawn(move || {
        list_children(
            &endpoint(port),
            "secret",
            &ResourcePath::parse_root("/dav/mount").unwrap(),
            &flag,
            &mut |_| Ok(()),
        )
    });
    thread::sleep(Duration::from_millis(100));
    cancel.store(true, Ordering::Release);
    let result = handle.join().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(result, Err(Failure::Cancelled));

    let (port, _server) = serve(|_request, mut stream| {
        let xml = format!(
            r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">{}</D:multistatus>"#,
            (0..300)
                .map(|index| propfind_item(&format!("f{index}"), false, Some(1)))
                .collect::<String>()
        );
        write!(
            stream,
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\n\r\n{xml}",
            xml.len()
        )
        .unwrap();
    });
    let cancel = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&cancel);
    let entered = Arc::new(AtomicBool::new(false));
    let entered_flag = Arc::clone(&entered);
    let cancel_from_waiter = Arc::clone(&cancel);
    thread::spawn(move || {
        while !entered_flag.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(10));
        }
        cancel_from_waiter.store(true, Ordering::Release);
    });
    let started = Instant::now();
    let result = list_children(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount").unwrap(),
        &flag,
        &mut |_| {
            entered.store(true, Ordering::Release);
            while !cancel.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(10));
            }
            Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "backpressure",
            ))
        },
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(result, Err(Failure::Cancelled));
}

#[test]
fn webdav_client_reports_http_errors_and_broken_xml_without_a_full_success() {
    for (status, line, expected) in [
        (401, "401 Unauthorized", Failure::Unauthorized),
        (403, "403 Forbidden", Failure::Forbidden),
        (404, "404 Not Found", Failure::NotFound),
        (429, "429 Too Many Requests", Failure::TooManyRequests),
        (500, "500 Server Error", Failure::Server(500)),
    ] {
        let (port, _server) = serve(move |_request, mut stream| {
            write!(stream, "HTTP/1.1 {line}\r\nContent-Length: 0\r\n\r\n").unwrap();
        });
        let result = list_children(
            &endpoint(port),
            "secret",
            &ResourcePath::parse_root("/dav/mount").unwrap(),
            &Arc::new(AtomicBool::new(false)),
            &mut |_| Ok(()),
        );
        assert_eq!(result, Err(expected), "status {status}");
    }
    let (port, _server) = serve(|_request, mut stream| {
        let xml = br#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:"><D:response><D:href>/dav/mount/a</D:href><D:propstat><D:prop><D:resourcetype/></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response><broken"#;
        write!(
            stream,
            "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\n\r\n",
            xml.len()
        )
        .unwrap();
        stream.write_all(xml).unwrap();
    });
    let result = list_children(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount").unwrap(),
        &Arc::new(AtomicBool::new(false)),
        &mut |_| Ok(()),
    );
    assert!(
        matches!(result, Err(Failure::Incomplete(_))),
        "broken xml returned {result:?}"
    );
}

#[test]
fn webdav_download_strips_authorization_on_cross_origin_redirects() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let origin_seen = Arc::clone(&seen);
    let direct_seen = Arc::clone(&seen);
    let direct = TcpListener::bind("127.0.0.1:0").unwrap();
    let direct_port = direct.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut stream, _) = direct.accept().unwrap();
        let request = read_request(&stream).unwrap();
        direct_seen
            .lock()
            .unwrap()
            .push(request.headers.get("authorization").cloned());
        let body = b"known-bytes";
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    });
    let (port, _server) = serve(move |request, mut stream| {
        origin_seen
            .lock()
            .unwrap()
            .push(request.headers.get("authorization").cloned());
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{direct_port}/file?sig=secret-token\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
    });
    let root = std::env::temp_dir().join(format!("asterfiles-webdav-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let source = ResourcePath::parse_root("/dav/mount/known.bin").unwrap();
    let destination = root.join("known.bin");
    let finish = download_file(
        &endpoint(port),
        "secret",
        &source,
        &destination,
        &Arc::new(AtomicBool::new(false)),
        &mut |_, _| {},
    )
    .unwrap();
    assert_eq!(std::fs::read(&finish.path).unwrap(), b"known-bytes");
    assert!(finish.length_known);
    let headers = seen.lock().unwrap().clone();
    assert!(headers[0].as_deref().unwrap().starts_with("Basic "));
    assert!(headers[1].is_none());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn webdav_download_rejects_loops_expired_links_conflicts_and_bad_names() {
    let (port, _server) = serve(|request, mut stream| {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n",
            request.path
        )
        .unwrap();
    });
    let root = std::env::temp_dir().join(format!("asterfiles-webdav-loop-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let destination = root.join("loop.bin");
    let result = download_file(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount/loop.bin").unwrap(),
        &destination,
        &Arc::new(AtomicBool::new(false)),
        &mut |_, _| {},
    );
    assert!(matches!(result, Err(Failure::Redirect(_))));
    assert!(!destination.exists());

    let direct = TcpListener::bind("127.0.0.1:0").unwrap();
    let direct_port = direct.local_addr().unwrap().port();
    thread::spawn(move || {
        let (mut stream, _) = direct.accept().unwrap();
        let _ = read_request(&stream);
        write!(
            stream,
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
    });
    let (port, _server) = serve(move |_request, mut stream| {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{direct_port}/expired\r\nContent-Length: 0\r\n\r\n"
        )
        .unwrap();
    });
    let destination = root.join("expired.bin");
    let result = download_file(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount/expired.bin").unwrap(),
        &destination,
        &Arc::new(AtomicBool::new(false)),
        &mut |_, _| {},
    );
    assert_eq!(result, Err(Failure::ExpiredLink));
    assert!(!destination.exists());

    let existing = root.join("exists.bin");
    std::fs::write(&existing, b"keep-me").unwrap();
    let result = download_file(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount/exists.bin").unwrap(),
        &existing,
        &Arc::new(AtomicBool::new(false)),
        &mut |_, _| {},
    );
    assert_eq!(result, Err(Failure::Conflict));
    assert_eq!(std::fs::read(&existing).unwrap(), b"keep-me");
    let invalid = download_file(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount/CON").unwrap(),
        &root.join("CON"),
        &Arc::new(AtomicBool::new(false)),
        &mut |_, _| {},
    );
    assert_eq!(invalid, Err(Failure::InvalidName));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn webdav_large_listing_reports_time_and_memory() {
    let gate = Arc::new(AtomicBool::new(false));
    let released = Arc::clone(&gate);
    let (port, _server) = serve(move |_request, mut stream| {
        write!(
            stream,
            "HTTP/1.1 207 Multi-Status\r\nTransfer-Encoding: chunked\r\n\r\n"
        )
        .unwrap();
        let mut first = String::from(r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">"#);
        for index in 0..300 {
            first.push_str(&propfind_item(&format!("n{index}"), false, Some(1)));
        }
        write_chunk(&mut stream, &first);
        thread::sleep(Duration::from_millis(300));
        released.store(true, Ordering::Release);
        let mut rest = String::new();
        for index in 300..100_000 {
            rest.push_str(&propfind_item(&format!("n{index}"), false, Some(1)));
            if rest.len() > 1024 * 1024 {
                write_chunk(&mut stream, &rest);
                rest.clear();
            }
        }
        rest.push_str("</D:multistatus>");
        write_chunk(&mut stream, &rest);
        write!(stream, "0\r\n\r\n").unwrap();
    });
    let started = Instant::now();
    let early = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&early);
    let gate_for_client = Arc::clone(&gate);
    let mut count = 0_usize;
    let mut batches = 0_usize;
    list_children(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount").unwrap(),
        &Arc::new(AtomicBool::new(false)),
        &mut |items| {
            if !gate_for_client.load(Ordering::Acquire) {
                flag.store(true, Ordering::Release);
            }
            count += items.len();
            batches += 1;
            Ok(())
        },
    )
    .unwrap();
    let elapsed_ms = started.elapsed().as_millis();
    let memory = working_set();
    assert!(early.load(Ordering::Acquire));
    assert_eq!(count, 100_000);
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/state/webdav");
    std::fs::create_dir_all(&directory).unwrap();
    let body = format!(
        "{{\"entries\":{count},\"batches\":{batches},\"elapsed_ms\":{elapsed_ms},\"working_set_bytes\":{memory},\"first_batch_before_response_end\":true}}\n"
    );
    std::fs::write(directory.join("listing-100k.json"), body).unwrap();
    let log_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("artifacts/logs/webdav");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::write(
        log_dir.join("listing-100k.log"),
        format!(
            "entries={count} batches={batches} elapsed_ms={elapsed_ms} working_set_bytes={memory}\n"
        ),
    )
    .unwrap();
}

#[test]
fn webdav_directory_download_writes_children_and_leaves_other_files() {
    let (port, _server) = serve(|request, mut stream| {
        if request.method == "PROPFIND" {
            let item = propfind_item("folder/hello.txt", false, Some(10));
            let body = format!(
                r#"<?xml version="1.0"?><D:multistatus xmlns:D="DAV:">{item}</D:multistatus>"#
            );
            write!(
                stream,
                "HTTP/1.1 207 Multi-Status\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        } else {
            let body = b"tree-bytes";
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    let root = std::env::temp_dir().join(format!("asterfiles-webdav-tree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let keeper = root.join("keep.txt");
    std::fs::write(&keeper, b"leave-me").unwrap();
    let finish = download_tree(
        &endpoint(port),
        "secret",
        &ResourcePath::parse_root("/dav/mount/folder").unwrap(),
        &root,
        &Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    )
    .unwrap();
    assert_eq!(
        std::fs::read(finish.path.join("hello.txt")).unwrap(),
        b"tree-bytes"
    );
    assert_eq!(std::fs::read(&keeper).unwrap(), b"leave-me");
    let _ = std::fs::remove_dir_all(&root);
}

fn working_set() -> u64 {
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut core::ffi::c_void;
        fn K32GetProcessMemoryInfo(
            process: *mut core::ffi::c_void,
            counters: *mut Counters,
            cb: u32,
        ) -> i32;
    }
    let mut counters = Counters {
        cb: std::mem::size_of::<Counters>() as u32,
        page_fault_count: 0,
        peak_working_set_size: 0,
        working_set_size: 0,
        quota_peak_paged_pool_usage: 0,
        quota_paged_pool_usage: 0,
        quota_peak_non_paged_pool_usage: 0,
        quota_non_paged_pool_usage: 0,
        pagefile_usage: 0,
        peak_pagefile_usage: 0,
    };
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok == 0 {
        0
    } else {
        counters.working_set_size as u64
    }
}

#[allow(dead_code)]
fn _touch_counter(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}
