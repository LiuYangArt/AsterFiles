use super::*;
use std::{os::windows::fs::OpenOptionsExt, sync::mpsc, thread};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "asterfiles-issue-185-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
    fn temporary_files(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".asterfiles-copy-")
            })
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        for entry in fs::read_dir(&self.0).unwrap().flatten() {
            if entry.path().is_file() {
                let _ = remove_copy_temporary(&entry.path());
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy(
    source: &Path,
    destination: &Path,
    replace: bool,
) -> Result<FileOperationReport, OperationError> {
    let mut report = FileOperationReport::new();
    copy_file_safely(
        source,
        destination,
        replace,
        &CopyExecution::local(CancellationToken::new()),
        &mut |_, _, _| {},
        &mut report,
    )?;
    Ok(report)
}

#[test]
fn issue_185_success_and_overwrite_consume_only_the_owned_temporary() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source.bin");
    let destination = fixture.0.join("destination.bin");
    let other_task = fixture
        .0
        .join(format!(".asterfiles-copy-{}-1-1", std::process::id()));
    let user_file = fixture.0.join(".asterfiles-copy-user");
    fs::write(&source, b"first content").unwrap();
    fs::write(&other_task, b"other task").unwrap();
    fs::write(&user_file, b"user content").unwrap();
    for (content, replace) in [
        (b"first content".as_slice(), false),
        (b"second content".as_slice(), true),
    ] {
        fs::write(&source, content).unwrap();
        let report = copy(&source, &destination, replace).unwrap();
        assert_eq!(fs::read(&destination).unwrap(), content);
        assert_eq!(report.files, 1);
        assert_eq!(
            report.completed_paths.as_slice(),
            std::slice::from_ref(&destination)
        );
        let mut actual = fixture.temporary_files();
        actual.sort();
        let mut expected = vec![other_task.clone(), user_file.clone()];
        expected.sort();
        assert_eq!(actual, expected);
    }
    assert_eq!(fs::read(other_task).unwrap(), b"other task");
    assert_eq!(fs::read(user_file).unwrap(), b"user content");
}

#[test]
fn issue_185_readonly_temporary_is_removed_when_publication_fails() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source.bin");
    let destination = fixture.0.join("occupied-directory");
    fs::write(&source, b"readonly source").unwrap();
    let mut permissions = fs::metadata(&source).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&source, permissions).unwrap();
    fs::create_dir(&destination).unwrap();
    assert!(matches!(
        copy(&source, &destination, false),
        Err(OperationError::Io { .. })
    ));
    assert!(fixture.temporary_files().is_empty());
    assert_eq!(fs::read(&source).unwrap(), b"readonly source");
    assert!(fs::metadata(&source).unwrap().permissions().readonly());
    assert!(destination.is_dir());
}

#[test]
fn issue_185_blocked_cleanup_reports_the_exact_owned_path() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source.bin");
    let destination = fixture.0.join("destination.bin");
    fs::write(&source, b"source content").unwrap();
    let mut held = None;
    let mut temporary = None;
    let result = copy_file_safely(
        &source,
        &destination,
        false,
        &CopyExecution::local(CancellationToken::new()),
        &mut |_, kind, _| {
            if kind == FileProgressKind::Completed {
                let paths = fixture.temporary_files();
                assert_eq!(paths.len(), 1);
                temporary = Some(paths[0].clone());
                held = Some(
                    fs::OpenOptions::new()
                        .read(true)
                        .share_mode(3)
                        .open(&paths[0])
                        .unwrap(),
                );
            }
        },
        &mut FileOperationReport::new(),
    );
    let temporary = temporary.unwrap();
    match result {
        Err(OperationError::Io { path, message, .. }) => {
            assert_eq!(path, temporary);
            assert!(message.contains("temporary copy cleanup failed"));
            assert!(message.contains(&temporary.display().to_string()));
        }
        other => panic!("expected tracked cleanup failure: {other:?}"),
    }
    assert!(!destination.exists());
    assert!(temporary.exists());
    drop(held);
    remove_copy_temporary(&temporary).unwrap();
    assert!(fixture.temporary_files().is_empty());
}

#[test]
fn issue_185_cancelled_copy_keeps_original_destination_without_temporary_files() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source.bin");
    let destination = fixture.0.join("destination.bin");
    fs::write(&source, vec![7; 64 * 1024 * 1024]).unwrap();
    fs::write(&destination, b"original destination").unwrap();
    let cancel = CancellationToken::new();
    let result = copy_file_safely(
        &source,
        &destination,
        true,
        &CopyExecution::local(cancel.clone()),
        &mut |_, kind, _| {
            if kind == FileProgressKind::Transferred {
                cancel.cancel();
            }
        },
        &mut FileOperationReport::new(),
    );
    assert_eq!(result, Err(OperationError::Cancelled));
    assert_eq!(fs::read(destination).unwrap(), b"original destination");
    assert!(fixture.temporary_files().is_empty());
}

#[test]
fn issue_185_pause_resume_publishes_content_and_consumes_temporary() {
    let fixture = Fixture::new();
    let source = fixture.0.join("source.bin");
    let destination = fixture.0.join("destination.bin");
    fs::write(&source, vec![9; 64 * 1024 * 1024]).unwrap();
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let target = destination.clone();
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut requested = false;
        copy_file_safely(
            &source,
            &target,
            false,
            &CopyExecution::local(worker_cancel.clone()),
            &mut |_, kind, _| {
                if kind == FileProgressKind::Transferred && !requested {
                    requested = true;
                    worker_cancel.pause();
                    sender.send(()).unwrap();
                }
            },
            &mut FileOperationReport::new(),
        )
    });
    let started = receiver.recv_timeout(Duration::from_secs(10));
    let deadline = Instant::now() + Duration::from_secs(10);
    while started.is_ok() && !cancel.is_pause_acknowledged() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    let paused = cancel.is_pause_acknowledged();
    let private_count = fixture.temporary_files().len();
    let published_while_paused = destination.exists();
    cancel.resume();
    let result = worker.join().unwrap();
    assert!(started.is_ok());
    assert!(paused);
    assert_eq!(private_count, 1);
    assert!(!published_while_paused);
    result.unwrap();
    assert_eq!(fs::read(destination).unwrap(), vec![9; 64 * 1024 * 1024]);
    assert!(fixture.temporary_files().is_empty());
}
