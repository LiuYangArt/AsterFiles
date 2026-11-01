use super::*;

fn temporary_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "asterfiles-snapshot-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&root).unwrap();
    root
}

#[test]
fn issue_137_snapshot_concurrent_reads_never_interrupt_writer_or_tear_data() {
    let root = temporary_root("concurrent");
    let path = root.join("snapshot");
    atomic_write(&path, &[0; 8192]).unwrap();
    let done = std::sync::Arc::new(AtomicBool::new(false));
    std::thread::scope(|scope| {
        let writer_done = done.clone();
        let path = &path;
        scope.spawn(move || {
            for sequence in 1..=1500_u32 {
                let bytes = vec![(sequence % 251) as u8; if sequence % 2 == 0 { 8192 } else { 97 }];
                atomic_write(path, &bytes).unwrap();
            }
            writer_done.store(true, Ordering::Release);
        });
        let mut reads = 0;
        while !done.load(Ordering::Acquire) {
            if let Some(bytes) = available_snapshot(read_snapshot(path)).unwrap() {
                assert!(matches!(bytes.len(), 97 | 8192));
                assert!(bytes.iter().all(|value| *value == bytes[0]));
                reads += 1;
            }
            std::thread::yield_now();
        }
        assert!(reads > 0);
    });
    assert!(std::fs::metadata(&path).unwrap().len() <= 8192);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn issue_137_snapshot_busy_reader_does_not_block_parent_polling() {
    let root = temporary_root("busy");
    let path = root.join("snapshot");
    atomic_write(&path, &[1; 9]).unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    file.lock().unwrap();
    let started = std::time::Instant::now();
    assert!(available_snapshot(read_snapshot(&path)).unwrap().is_none());
    assert!(started.elapsed() < Duration::from_secs(1));
    drop(file);
    assert_eq!(read_snapshot(&path).unwrap(), [1; 9]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn issue_137_snapshot_write_lock_has_bounded_wait() {
    let root = temporary_root("deadline");
    let path = root.join("snapshot");
    atomic_write(&path, &[1; 9]).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    file.lock_shared().unwrap();
    let started = std::time::Instant::now();
    let error = atomic_write(&path, &[2; 9]).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(error.to_string().contains(path.to_str().unwrap()));
    drop(file);
    assert_eq!(read_snapshot(&path).unwrap(), [1; 9]);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn issue_137_monitor_retains_snapshot_failure_path_and_system_reason() {
    let root = temporary_root("error");
    let scan_path = root.join("channel.scan");
    std::fs::create_dir(&scan_path).unwrap();
    let expected = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&scan_path)
        .unwrap_err()
        .to_string();
    let result = execute_network_operation(
        IsolatedNetworkOperationKind::Copy,
        &root.join("missing"),
        Some(&root.join("destination")),
        &root.join("channel.event"),
        &root.join("channel.response"),
    )
    .unwrap_err()
    .to_string();
    assert!(result.contains(scan_path.to_str().unwrap()), "{result}");
    assert!(result.contains(&expected), "{result}");
    assert!(!result.contains("operation interrupted"), "{result}");
    std::fs::remove_dir_all(root).unwrap();
}
