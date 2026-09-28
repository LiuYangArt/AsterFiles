use super::*;
use crate::domain::file_operations::{CancellationToken, ConflictAction, ConflictDecision};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn issue_136_scan_monitor_reports_totals_while_copy_waits_for_conflict() {
    let fixture = Fixture(std::env::temp_dir().join(format!(
        "asterfiles-issue-136-monitor-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )));
    let source = fixture.0.join("source");
    let destination = fixture.0.join("destination");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::create_dir_all(&destination).unwrap();
    for name in ["first.bin", "second.bin"] {
        std::fs::write(source.join(name), b"new content").unwrap();
        std::fs::write(destination.join(name), b"old").unwrap();
    }
    let cancellation = CancellationToken::new();
    let request = FileOperationRequest {
        id: OperationId(136),
        kind: FileOperationKind::Copy,
        resource: OperationResource::Local,
        items: vec![OperationItem::pending(
            Some(source),
            Some(destination.clone()),
        )],
        undo_items: vec![],
        undo_source_manifests: vec![],
        cancellation: cancellation.clone(),
    };
    let (sender, receiver) = mpsc::channel();
    let worker = thread::spawn(move || {
        execute_file_operation_request(request, &sender, &Arc::new(Mutex::new(())));
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut pending_conflict = None;
    let mut complete_total_before_transfer = false;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(FileOperationEvent::Conflict { response, .. }) => pending_conflict = Some(response),
            Ok(FileOperationEvent::Progress {
                scanning_complete: true,
                total_files: Some(2),
                total_bytes: Some(22),
                processed_bytes: 0,
                ..
            }) => complete_total_before_transfer = true,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if complete_total_before_transfer && pending_conflict.is_some() {
            break;
        }
    }
    let observed_during_conflict = complete_total_before_transfer && pending_conflict.is_some();
    if observed_during_conflict {
        let _ = pending_conflict.take().unwrap().send(ConflictDecision {
            action: ConflictAction::Replace,
            apply_to_all: true,
        });
    } else {
        cancellation.cancel();
        drop(pending_conflict);
    }
    let completion_deadline = Instant::now() + Duration::from_secs(5);
    let mut finished = false;
    while Instant::now() < completion_deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(FileOperationEvent::Finished { .. }) => {
                finished = true;
                break;
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    if !finished {
        cancellation.cancel();
    }
    let worker_result = worker.join();
    assert!(worker_result.is_ok());
    assert!(
        observed_during_conflict,
        "independent scan totals must arrive while the copy is blocked on conflict"
    );
    assert!(
        finished,
        "copy worker must finish after resolving the conflict"
    );
    for name in ["first.bin", "second.bin"] {
        assert_eq!(
            std::fs::read(destination.join(name)).unwrap(),
            b"new content"
        );
    }
}
