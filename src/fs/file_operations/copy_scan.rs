use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{Receiver, SyncSender},
    },
    time::Duration,
};

use super::{CancellationToken, OperationError, discovered_size};

pub(super) const QUEUE_CAPACITY: usize = 128;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CopyScanSnapshot {
    pub files: usize,
    pub bytes: u64,
    pub complete: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CopyScanProgress(Arc<Mutex<CopyScanSnapshot>>);

impl CopyScanProgress {
    pub fn snapshot(&self) -> CopyScanSnapshot {
        *self.0.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn discover(&self, metadata: &fs::Metadata) {
        let mut progress = self.0.lock().unwrap_or_else(|error| error.into_inner());
        progress.files = progress.files.saturating_add(1);
        progress.bytes = progress.bytes.saturating_add(discovered_size(metadata));
    }
}

pub(super) enum ScanEntry {
    Child {
        path: PathBuf,
        directory: bool,
        bytes: u64,
    },
    DirectoryEnd(PathBuf),
    Error(OperationError),
}

pub(super) struct CopyTraversal {
    receiver: Receiver<ScanEntry>,
}

impl CopyTraversal {
    pub(super) fn new(receiver: Receiver<ScanEntry>) -> Self {
        Self { receiver }
    }

    fn receive(
        &self,
        source: &Path,
        cancel: &CancellationToken,
    ) -> Result<ScanEntry, OperationError> {
        loop {
            // This consumer is the copy worker; only it can confirm that no write is running.
            cancel.wait_if_paused();
            if cancel.is_cancelled() {
                return Err(OperationError::Cancelled);
            }
            match self.receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(entry) => return Ok(entry),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(OperationError::Io {
                        path: source.to_path_buf(),
                        kind: std::io::ErrorKind::UnexpectedEof,
                        message: "copy directory scan ended before the directory was complete"
                            .to_owned(),
                    });
                }
            }
        }
    }

    pub(super) fn next_child(
        &self,
        source: &Path,
        cancel: &CancellationToken,
    ) -> Result<Option<PathBuf>, OperationError> {
        match self.receive(source, cancel)? {
            ScanEntry::Child { path, .. } if path.parent() == Some(source) => Ok(Some(path)),
            ScanEntry::DirectoryEnd(path) if path == source => Ok(None),
            ScanEntry::Child { .. } | ScanEntry::DirectoryEnd(_) => Err(OperationError::Io {
                path: source.to_path_buf(),
                kind: std::io::ErrorKind::InvalidData,
                message: "source directory changed while copying".to_owned(),
            }),
            ScanEntry::Error(error) => Err(error),
        }
    }

    pub(super) fn skip_directory(
        &self,
        source: &Path,
        cancel: &CancellationToken,
        skipped: &mut dyn FnMut(u64, &Path),
    ) -> Result<(), OperationError> {
        let mut depth = 1_usize;
        while depth > 0 {
            match self.receive(source, cancel)? {
                ScanEntry::Child {
                    directory: true, ..
                } => depth += 1,
                ScanEntry::Child {
                    directory: false,
                    path,
                    bytes,
                } => skipped(bytes, &path),
                ScanEntry::DirectoryEnd(_) => depth -= 1,
                ScanEntry::Error(_) => {}
            }
        }
        Ok(())
    }
}

pub(super) fn scan(
    source: &Path,
    metadata: &fs::Metadata,
    cancel: &CancellationToken,
    progress: &CopyScanProgress,
    sender: SyncSender<ScanEntry>,
) {
    // The copy worker owns pause acknowledgement. Scanning may fill only this bounded queue.
    let result = scan_entry(source, metadata, cancel, progress, &sender);
    match result {
        Ok(complete) => {
            progress
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .complete = complete;
        }
        Err(error) => {
            let _ = sender.send(ScanEntry::Error(error));
        }
    }
}

fn scan_entry(
    source: &Path,
    metadata: &fs::Metadata,
    cancel: &CancellationToken,
    progress: &CopyScanProgress,
    sender: &SyncSender<ScanEntry>,
) -> Result<bool, OperationError> {
    if cancel.is_cancelled() {
        return Err(OperationError::Cancelled);
    }
    if !metadata.file_type().is_dir() {
        progress.discover(metadata);
        return Ok(true);
    }
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) => {
            sender
                .send(ScanEntry::Error(OperationError::io(source, error)))
                .map_err(|_| OperationError::Cancelled)?;
            return sender
                .send(ScanEntry::DirectoryEnd(source.to_path_buf()))
                .map(|_| false)
                .map_err(|_| OperationError::Cancelled);
        }
    };
    let mut complete = true;
    for entry in entries {
        if cancel.is_cancelled() {
            return Err(OperationError::Cancelled);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                complete = false;
                sender
                    .send(ScanEntry::Error(OperationError::io(source, error)))
                    .map_err(|_| OperationError::Cancelled)?;
                continue;
            }
        };
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                complete = false;
                sender
                    .send(ScanEntry::Error(OperationError::io(&path, error)))
                    .map_err(|_| OperationError::Cancelled)?;
                continue;
            }
        };
        sender
            .send(ScanEntry::Child {
                path: path.clone(),
                directory: metadata.file_type().is_dir(),
                bytes: discovered_size(&metadata),
            })
            .map_err(|_| OperationError::Cancelled)?;
        complete &= scan_entry(&path, &metadata, cancel, progress, sender)?;
    }
    sender
        .send(ScanEntry::DirectoryEnd(source.to_path_buf()))
        .map(|_| complete)
        .map_err(|_| OperationError::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Instant};

    #[test]
    fn issue_137_copy_consumer_pauses_while_waiting_for_scan_results() {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let cancel = CancellationToken::new();
        cancel.pause();
        let worker_cancel = cancel.clone();
        let worker = thread::spawn(move || {
            CopyTraversal::new(receiver).next_child(Path::new(r"C:\fixture"), &worker_cancel)
        });
        let started = Instant::now();
        while !cancel.is_pause_acknowledged() && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
        let acknowledged = cancel.is_pause_acknowledged();
        cancel.cancel();
        let result = worker.join().unwrap();
        drop(sender);
        assert!(acknowledged);
        assert!(matches!(result, Err(OperationError::Cancelled)));
    }
}
