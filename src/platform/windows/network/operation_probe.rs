use super::*;

pub fn try_run_operation_probe_from_args() -> io::Result<bool> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).and_then(|arg| arg.to_str()) != Some("--agent-network-copy-probe") {
        return Ok(false);
    }
    if args.len() == 4 && (args[2] == "--complete" || args[2] == "--resume-complete") {
        run_complete_probe(Path::new(&args[3]), args[2] == "--resume-complete")?;
        return Ok(true);
    }
    if args.len() != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "probe requires one source file",
        ));
    }
    if is_stall_probe() {
        run_stall_probe()?;
        return Ok(true);
    }
    let source = PathBuf::from(&args[2]);
    let metadata = std::fs::symlink_metadata(&source)?;
    if !metadata.is_file() || metadata.len() < 128 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "probe requires a regular file of at least 128 MiB",
        ));
    }
    let root = std::env::current_dir()?.join("artifacts/state/issue-136/parent-probe");
    std::fs::create_dir_all(&root)?;
    let destination_root = root.join(format!(
        "run-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir(&destination_root)?;
    let destination = destination_root.join("copy.bin");
    let cancel = crate::domain::file_operations::CancellationToken::new();
    let transferred = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let done = std::sync::Arc::new(AtomicBool::new(false));
    let controller_cancel = cancel.clone();
    let controller_bytes = transferred.clone();
    let controller_done = done.clone();
    let controller = std::thread::spawn(move || {
        let result = (|| -> io::Result<(u64, u64)> {
            let wait_for = |condition: &dyn Fn() -> bool| -> io::Result<()> {
                let started = std::time::Instant::now();
                loop {
                    if condition() {
                        return Ok(());
                    }
                    if controller_done.load(Ordering::Acquire)
                        || started.elapsed() > Duration::from_secs(15)
                    {
                        return Err(io::Error::other(
                            "copy probe condition timed out or helper finished early",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            };
            wait_for(&|| controller_bytes.load(Ordering::Acquire) > 0)?;
            controller_cancel.pause();
            wait_for(&|| controller_cancel.is_pause_acknowledged())?;
            // The parent publishes the final byte snapshot in the same polling iteration as acknowledgement.
            std::thread::sleep(Duration::from_millis(50));
            let paused_bytes = controller_bytes.load(Ordering::Acquire);
            let paused_at = std::time::Instant::now();
            while paused_at.elapsed() < Duration::from_secs(12) {
                if controller_done.load(Ordering::Acquire)
                    || controller_bytes.load(Ordering::Acquire) != paused_bytes
                {
                    return Err(io::Error::other(
                        "copy advanced or parent exited during confirmed pause",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            controller_cancel.resume();
            wait_for(&|| controller_bytes.load(Ordering::Acquire) > paused_bytes)?;
            let resumed_bytes = controller_bytes.load(Ordering::Acquire);
            controller_cancel.pause();
            wait_for(&|| controller_cancel.is_pause_acknowledged())?;
            Ok((paused_bytes, resumed_bytes))
        })();
        controller_cancel.cancel();
        result
    });
    let operation = isolated_network_file_operation(
        IsolatedNetworkOperationKind::Copy,
        &source,
        Some(&destination),
        &cancel,
        &mut |_, _, _| crate::domain::file_operations::ConflictAction::Skip,
        &mut |_, _| {},
        &mut |bytes, _, _| {
            transferred.fetch_add(bytes, Ordering::AcqRel);
        },
        &mut |_, _, _| {},
        &mut |_| {},
    );
    done.store(true, Ordering::Release);
    let probe = controller
        .join()
        .map_err(|_| io::Error::other("copy probe controller panicked"))?;
    // Only this freshly created, absolute local destination belongs to the probe.
    std::fs::remove_dir_all(&destination_root)?;
    let (paused_bytes, resumed_bytes) = probe?;
    if !operation.is_err_and(|error| error.kind() == io::ErrorKind::Interrupted) {
        return Err(io::Error::other(
            "copy probe did not stop through parent cancellation",
        ));
    }
    std::fs::write(
        root.join("result.json"),
        format!(
            "{{\"parent_pause_acknowledged\":true,\"paused_for_seconds\":12,\"paused_bytes\":{paused_bytes},\"resumed_bytes\":{resumed_bytes},\"cancelled_while_paused\":true,\"helper_reaped\":true,\"destination_removed\":true}}\n"
        ),
    )?;
    Ok(true)
}

// Only an explicit headless probe invocation may select the synthetic waiting helper.
pub(super) fn is_stall_probe() -> bool {
    let args: Vec<_> = std::env::args_os().collect();
    args.len() == 3 && args[1] == "--agent-network-copy-probe" && args[2] == "--stall"
}

fn run_stall_probe() -> io::Result<()> {
    use crate::domain::file_operations::{CancellationToken, ConflictAction};
    let root = std::env::current_dir()?.join("artifacts/state/issue-137/stall-probe");
    std::fs::create_dir_all(&root)?;
    let run = root.join(format!(
        "run-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir(&run)?;
    let result = (|| -> io::Result<String> {
        let source = run.join("source.bin");
        let destination = run.join("destination.bin");
        let bytes: Vec<_> = (0..262_144_u32).map(|index| (index % 251) as u8).collect();
        std::fs::write(&source, &bytes)?;
        let cancel = CancellationToken::new();
        let started = std::time::Instant::now();
        let mut first_progress_ms = None;
        let mut transferred = 0;
        let report = isolated_network_file_operation(
            IsolatedNetworkOperationKind::Copy,
            &source,
            Some(&destination),
            &cancel,
            &mut |_, _, _| ConflictAction::Skip,
            &mut |_, _| {},
            &mut |count, _, _| {
                if count > 0 {
                    first_progress_ms.get_or_insert(started.elapsed().as_millis());
                }
                transferred += count;
            },
            &mut |_, _, _| {},
            &mut |_| {},
        )?;
        let elapsed_ms = started.elapsed().as_millis();
        let first_progress_ms = first_progress_ms
            .ok_or_else(|| io::Error::other("stall probe got no byte progress"))?;
        if first_progress_ms < 15_000
            || cancel.is_paused()
            || transferred != bytes.len() as u64
            || report.files != 1
            || std::fs::read(&destination)? != bytes
        {
            return Err(io::Error::other(
                "stall probe did not preserve the unpaused wait and exact file content",
            ));
        }
        std::fs::remove_file(&destination)?;
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let requested = std::sync::Arc::new(std::sync::Mutex::new(None));
        let worker_requested = requested.clone();
        let controller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(2));
            *worker_requested.lock().expect("cancel timestamp lock") =
                Some(std::time::Instant::now());
            worker_cancel.cancel();
        });
        let cancellation = isolated_network_file_operation(
            IsolatedNetworkOperationKind::Copy,
            &source,
            Some(&destination),
            &cancel,
            &mut |_, _, _| ConflictAction::Skip,
            &mut |_, _| {},
            &mut |_, _, _| {},
            &mut |_, _, _| {},
            &mut |_| {},
        );
        controller
            .join()
            .map_err(|_| io::Error::other("stall probe controller panicked"))?;
        let cancel_elapsed_ms = requested
            .lock()
            .expect("cancel timestamp lock")
            .ok_or_else(|| io::Error::other("stall cancellation was not requested"))?
            .elapsed()
            .as_millis();
        if !cancellation.is_err_and(|error| error.kind() == io::ErrorKind::Interrupted)
            || cancel_elapsed_ms > 3_000
            || destination.exists()
        {
            return Err(io::Error::other(
                "blocked helper did not cancel within three seconds",
            ));
        }
        Ok(format!(
            "{{\"unpaused_wait_seconds\":15,\"first_progress_ms\":{first_progress_ms},\"elapsed_ms\":{elapsed_ms},\"bytes\":{transferred},\"content_equal\":true,\"cancel_elapsed_ms\":{cancel_elapsed_ms},\"helper_reaped\":true,\"probe_files_removed\":true}}\n"
        ))
    })();
    let cleanup = std::fs::remove_dir_all(&run);
    let json = result?;
    cleanup?;
    std::fs::write(root.join("result.json"), json)
}

fn run_complete_probe(source: &Path, exercise_resume: bool) -> io::Result<()> {
    use crate::domain::file_operations::{CancellationToken, ConflictAction};
    use std::io::{Read, Write};
    let source = std::fs::canonicalize(source)?;
    let metadata = std::fs::metadata(&source)?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "complete probe requires one regular file",
        ));
    }
    if exercise_resume && metadata.len() < 256 * 1024 * 1024 {
        return Err(io::Error::other("resume probe requires at least 256 MiB"));
    }
    let root = std::env::current_dir()?.join("artifacts/state/issue-137/complete-probe");
    std::fs::create_dir_all(&root)?;
    let run = root.join(format!(
        "run-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir(&run)?;
    let destination = run.join("copy.bin");
    let mut log = std::fs::File::create(run.join("progress.jsonl"))?;
    writeln!(
        log,
        "{{\"phase\":\"copy_start\",\"expected_bytes\":{}}}",
        metadata.len()
    )?;
    log.flush()?;
    let mut log_error = None;
    let result = (|| -> io::Result<String> {
        let started = std::time::Instant::now();
        let mut last_log = std::time::Instant::now();
        let mut transferred = 0_u64;
        let cancel = CancellationToken::new();
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let done = std::sync::Arc::new(AtomicBool::new(false));
        let controller = exercise_resume.then(|| {
            let cancel = cancel.clone();
            let observed = observed.clone();
            let done = done.clone();
            std::thread::spawn(move || {
                let result = resume_complete_control(&cancel, &observed, &done);
                if result.is_err() {
                    cancel.cancel();
                }
                result
            })
        });
        let report = isolated_network_file_operation(
            IsolatedNetworkOperationKind::Copy,
            &source,
            Some(&destination),
            &cancel,
            &mut |_, _, _| ConflictAction::Skip,
            &mut |_, _| {},
            &mut |bytes, complete, _| {
                transferred += bytes;
                observed.store(transferred, Ordering::Release);
                if complete == FileProgressKind::Completed
                    || last_log.elapsed() >= Duration::from_secs(2)
                {
                    if let Err(error) = writeln!(
                        log,
                        "{{\"phase\":\"copy\",\"bytes\":{transferred},\"elapsed_ms\":{}}}",
                        started.elapsed().as_millis()
                    )
                    .and_then(|_| log.flush())
                    {
                        log_error.get_or_insert(error);
                    }
                    last_log = std::time::Instant::now();
                }
            },
            &mut |_, _, _| {},
            &mut |_| {},
        );
        done.store(true, Ordering::Release);
        let pause = controller
            .map(|controller| {
                controller
                    .join()
                    .map_err(|_| io::Error::other("resume probe controller panicked"))?
            })
            .transpose();
        let report = report?;
        let paused_bytes = pause?.unwrap_or(0);
        if let Some(error) = log_error.take() {
            return Err(error);
        }
        let copy_duration_ms = started.elapsed().as_millis();
        if report.files != 1 || report.bytes != metadata.len() || transferred != metadata.len() {
            return Err(io::Error::other(format!(
                "complete probe totals disagree: files={}, report_bytes={}, transferred={transferred}, expected={}",
                report.files,
                report.bytes,
                metadata.len()
            )));
        }
        writeln!(
            log,
            "{{\"phase\":\"compare_start\",\"bytes\":{transferred}}}"
        )?;
        log.flush()?;
        let comparison_started = std::time::Instant::now();
        let mut original = std::fs::File::open(&source)?;
        let mut copy = std::fs::File::open(&destination)?;
        let mut original_buffer = vec![0_u8; 1024 * 1024];
        let mut copy_buffer = vec![0_u8; 1024 * 1024];
        let mut compared = 0_u64;
        let mut last_log = std::time::Instant::now();
        while compared < metadata.len() {
            let count = (metadata.len() - compared).min(original_buffer.len() as u64) as usize;
            original.read_exact(&mut original_buffer[..count])?;
            copy.read_exact(&mut copy_buffer[..count])?;
            if original_buffer[..count] != copy_buffer[..count] {
                return Err(io::Error::other(format!(
                    "complete probe content differs near byte {compared}"
                )));
            }
            compared += count as u64;
            if last_log.elapsed() >= Duration::from_secs(2) {
                writeln!(log, "{{\"phase\":\"compare\",\"bytes\":{compared}}}")?;
                log.flush()?;
                last_log = std::time::Instant::now();
            }
        }
        if original.read(&mut original_buffer[..1])? != 0 || copy.read(&mut copy_buffer[..1])? != 0
        {
            return Err(io::Error::other(
                "complete probe length changed during comparison",
            ));
        }
        let comparison_duration_ms = comparison_started.elapsed().as_millis();
        Ok(format!(
            "{{\"files\":{},\"bytes\":{transferred},\"pause_exercised\":{exercise_resume},\"paused_bytes\":{paused_bytes},\"paused_for_seconds\":2,\"copy_duration_ms\":{copy_duration_ms},\"comparison_duration_ms\":{comparison_duration_ms},\"content_equal\":true,\"helper_reaped\":true,\"destination_removed\":true}}\n",
            report.files
        ))
    })();
    // Keep small evidence in the exclusive run directory; remove only its owned copy.
    let cleanup = match std::fs::remove_file(&destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        result => result,
    };
    let result = result.and_then(|json| cleanup.map(|_| json));
    match result {
        Ok(json) => {
            std::fs::write(run.join("result.json"), &json)?;
            std::fs::write(root.join("result.json"), json)?;
            Ok(())
        }
        Err(error) => {
            writeln!(log, "{{\"phase\":\"failed\",\"elapsed_error\":true}}")?;
            std::fs::write(run.join("error.txt"), error.to_string())?;
            Err(error)
        }
    }
}

fn resume_complete_control(
    cancel: &crate::domain::file_operations::CancellationToken,
    observed: &std::sync::atomic::AtomicU64,
    done: &AtomicBool,
) -> io::Result<u64> {
    let wait = |condition: &dyn Fn() -> bool, timeout: Duration| -> io::Result<()> {
        let start = std::time::Instant::now();
        while !condition() {
            if done.load(Ordering::Acquire) || start.elapsed() >= timeout {
                return Err(io::Error::other(
                    "resume probe condition timed out or copy ended early",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    };
    wait(
        &|| observed.load(Ordering::Acquire) >= 128 * 1024 * 1024,
        Duration::from_secs(120),
    )?;
    cancel.pause();
    wait(&|| cancel.is_pause_acknowledged(), Duration::from_secs(5))?;
    std::thread::sleep(Duration::from_millis(50));
    let paused = observed.load(Ordering::Acquire);
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        if done.load(Ordering::Acquire) || observed.load(Ordering::Acquire) != paused {
            return Err(io::Error::other("copy advanced during confirmed pause"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    cancel.resume();
    Ok(paused)
}
