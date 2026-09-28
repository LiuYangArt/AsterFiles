use crate::domain::file_operations::CancellationToken;
use std::{
    io,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

struct ReadSlots {
    active: AtomicUsize,
    limit: usize,
}
static READ_SLOTS: LazyLock<Arc<ReadSlots>> = LazyLock::new(|| {
    Arc::new(ReadSlots {
        active: AtomicUsize::new(0),
        limit: 4,
    })
});

pub(super) struct ReadSlot(Arc<ReadSlots>);
impl Drop for ReadSlot {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ReadSlots {
    fn acquire(self: &Arc<Self>, cancel: &CancellationToken) -> io::Result<ReadSlot> {
        loop {
            cancel.wait_if_paused();
            if cancel.is_cancelled() {
                return Err(io::ErrorKind::Interrupted.into());
            }
            if self
                .active
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < self.limit).then_some(count + 1)
                })
                .is_ok()
            {
                return Ok(ReadSlot(self.clone()));
            }
            super::super::network::set_copy_recovering(true);
            thread::sleep(Duration::from_millis(20));
        }
    }
}

// A permit remains with the worker until even its network handles have finished closing.
pub(super) fn acquire_read_slot(cancel: &CancellationToken) -> io::Result<ReadSlot> {
    READ_SLOTS.acquire(cancel)
}

pub(super) fn cancel_read_worker<T>(worker: &thread::JoinHandle<T>) {
    use std::os::windows::io::AsRawHandle;
    // Cancellation is only a request; never release its permit or join a pending SMB call here.
    if !worker.is_finished() {
        unsafe {
            windows_sys::Win32::System::IO::CancelSynchronousIo(worker.as_raw_handle());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Instant};
    #[test]
    fn issue_137_read_limit_preserves_pause_cancel_and_releases_only_on_worker_end() {
        let slots = Arc::new(ReadSlots {
            active: AtomicUsize::new(0),
            limit: 1,
        });
        let held = slots.acquire(&CancellationToken::new()).unwrap();
        let cancel = CancellationToken::new();
        let waiting_cancel = cancel.clone();
        let waiting_slots = slots.clone();
        let (sent, received) = mpsc::channel();
        let waiting = thread::spawn(move || {
            sent.send(waiting_slots.acquire(&waiting_cancel).map(|_| ()))
                .unwrap();
        });
        cancel.pause();
        let start = Instant::now();
        while !cancel.is_pause_acknowledged() && start.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(cancel.is_pause_acknowledged());
        assert_eq!(slots.active.load(Ordering::Acquire), 1);
        cancel.cancel();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
        waiting.join().unwrap();
        assert_eq!(slots.active.load(Ordering::Acquire), 1);
        drop(held);
        let recovered = slots.acquire(&CancellationToken::new()).unwrap();
        assert_eq!(slots.active.load(Ordering::Acquire), 1);
        drop(recovered);
        assert_eq!(slots.active.load(Ordering::Acquire), 0);
    }
}
