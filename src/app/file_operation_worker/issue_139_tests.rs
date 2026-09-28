use super::*;
use crate::app::test_support::FixtureCleanup;
use crate::domain::file_operations::{CancellationToken, ConflictAction, ConflictDecision};

#[test]
fn issue_139_skips_advance_completion_without_inflating_transfer_rate() {
    let (sender, receiver) = mpsc::channel();
    let mut emitter = OperationProgressEmitter::new(
        &sender,
        OperationProgressSnapshot {
            id: OperationId(139),
            completed_items: 0,
            completed_files: 0,
            skipped_files: 0,
            total_files: Some(2),
            discovered_files: 2,
            processed_bytes: 0,
            skipped_bytes: 0,
            total_bytes: Some(12_000),
            discovered_bytes: 12_000,
            scanning_complete: true,
            current_item: PathBuf::new(),
            recent_speed_bps: None,
        },
        CancellationToken::new(),
        Instant::now() - Duration::from_secs(1),
    );
    emitter.rate_estimator.record(Duration::ZERO, 0, 0);
    emitter.advanced(8_000, FileProgressKind::Skipped, Path::new("existing.bin"));
    assert_eq!(emitter.snapshot.processed_bytes, 0);
    assert_eq!(emitter.snapshot.completed_files, 0);
    assert!(
        emitter
            .snapshot
            .recent_speed_bps
            .is_none_or(|rate| rate == 0)
    );
    emitter.advanced(4_000, FileProgressKind::Completed, Path::new("new.bin"));
    emitter.flush();
    assert_eq!(emitter.snapshot.processed_bytes, 4_000);
    assert_eq!(emitter.snapshot.skipped_bytes, 8_000);
    assert_eq!(emitter.snapshot.completed_files, 1);
    assert_eq!(emitter.snapshot.skipped_files, 1);
    assert!(
        emitter
            .snapshot
            .recent_speed_bps
            .is_some_and(|rate| rate <= 4_000)
    );
    assert!(receiver.try_iter().any(|event| matches!(
        event,
        FileOperationEvent::Progress {
            completed_files: 1,
            skipped_files: 1,
            processed_bytes: 4_000,
            skipped_bytes: 8_000,
            ..
        }
    )));
}

#[test]
fn issue_139_mixed_and_all_skipped_transfers_finish_with_accounted_totals() {
    for kind in [FileOperationKind::Copy, FileOperationKind::Move] {
        for copy_new_file in [true, false] {
            let root = std::env::temp_dir().join(format!(
                "asterfiles-issue-139-worker-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let _cleanup = FixtureCleanup(root.clone());
            let source = root.join("source");
            let destination = root.join("destination");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::create_dir_all(&destination).unwrap();
            std::fs::write(source.join("existing.bin"), b"source").unwrap();
            std::fs::write(destination.join("existing.bin"), b"keep").unwrap();
            if copy_new_file {
                std::fs::write(source.join("new.bin"), b"new!").unwrap();
            }
            let cancellation = CancellationToken::new();
            let request = FileOperationRequest {
                id: OperationId(139),
                kind,
                resource: OperationResource::Local,
                items: vec![OperationItem::pending(
                    Some(source.clone()),
                    Some(destination.clone()),
                )],
                undo_items: Vec::new(),
                undo_source_manifests: Vec::new(),
                cancellation: cancellation.clone(),
            };
            let (sender, receiver) = mpsc::channel();
            let worker = thread::spawn(move || {
                execute_file_operation_request(request, &sender, &Arc::new(Mutex::new(())));
            });
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut final_progress = None;
            let mut finished = None;
            while Instant::now() < deadline {
                match receiver.recv_timeout(Duration::from_millis(50)) {
                    Ok(FileOperationEvent::Conflict { response, .. }) => {
                        let _ = response.send(ConflictDecision {
                            action: ConflictAction::Skip,
                            apply_to_all: true,
                        });
                    }
                    Ok(event @ FileOperationEvent::Progress { .. }) => {
                        final_progress = Some(event);
                    }
                    Ok(event @ FileOperationEvent::Finished { .. }) => {
                        finished = Some(event);
                        break;
                    }
                    Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            if finished.is_none() {
                cancellation.cancel();
            }
            worker.join().unwrap();
            let Some(FileOperationEvent::Finished {
                result,
                item_states,
                undo_items,
                ..
            }) = finished
            else {
                panic!("{kind:?} did not finish");
            };
            assert!(result.failed.is_empty(), "{result:?}");
            assert_eq!(result.skipped, vec![source.join("existing.bin")]);
            assert_eq!(
                result.succeeded,
                if copy_new_file {
                    vec![destination.clone()]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                item_states,
                vec![(
                    0,
                    if copy_new_file {
                        ItemState::Succeeded
                    } else {
                        ItemState::Skipped
                    },
                    None
                )]
            );
            assert!(undo_items.is_none());
            let Some(FileOperationEvent::Progress {
                completed_items,
                completed_files,
                skipped_files,
                processed_bytes,
                skipped_bytes,
                total_files,
                total_bytes,
                scanning_complete,
                ..
            }) = final_progress
            else {
                panic!("{kind:?} did not report final progress");
            };
            let copied_files = usize::from(copy_new_file);
            let copied_bytes = if copy_new_file { 4 } else { 0 };
            assert_eq!(completed_items, 1);
            assert_eq!(completed_files, copied_files);
            assert_eq!(skipped_files, 1);
            assert_eq!(processed_bytes, copied_bytes);
            assert_eq!(skipped_bytes, 6);
            assert_eq!(total_files, Some(copied_files + 1));
            assert_eq!(total_bytes, Some(copied_bytes + 6));
            assert!(scanning_complete);
            assert_eq!(
                std::fs::read(destination.join("existing.bin")).unwrap(),
                b"keep"
            );
            assert_eq!(
                std::fs::read(source.join("existing.bin")).unwrap(),
                b"source"
            );
            if copy_new_file {
                assert_eq!(std::fs::read(destination.join("new.bin")).unwrap(), b"new!");
                assert_eq!(
                    source.join("new.bin").exists(),
                    kind == FileOperationKind::Copy
                );
            }
        }
    }
}
