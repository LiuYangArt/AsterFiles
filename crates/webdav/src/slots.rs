//! Per-connection caps so a download cannot occupy every directory read.
use std::{
    collections::HashMap,
    sync::{Arc, Condvar, Mutex},
    time::Duration,
};

const PER_CONNECTION: u8 = 2;

#[derive(Debug)]
struct SlotState {
    counts: HashMap<u64, u8>,
}

pub struct SlotPool {
    state: Mutex<SlotState>,
    available: Condvar,
}

pub struct SlotGuard {
    pool: Arc<SlotPool>,
    connection_id: u64,
}

impl SlotPool {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SlotState {
                counts: HashMap::new(),
            }),
            available: Condvar::new(),
        })
    }

    pub fn acquire(
        self: &Arc<Self>,
        connection_id: u64,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Option<SlotGuard> {
        let mut state = self.state.lock().ok()?;
        loop {
            if cancel.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            let count = state.counts.get(&connection_id).copied().unwrap_or(0);
            if count < PER_CONNECTION {
                state.counts.insert(connection_id, count + 1);
                return Some(SlotGuard {
                    pool: Arc::clone(self),
                    connection_id,
                });
            }
            let (next, timeout) = self
                .available
                .wait_timeout(state, Duration::from_millis(50))
                .ok()?;
            if timeout.timed_out() && cancel.load(std::sync::atomic::Ordering::Acquire) {
                return None;
            }
            state = next;
        }
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        let Ok(mut state) = self.pool.state.lock() else {
            return;
        };
        if let Some(count) = state.counts.get_mut(&self.connection_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.counts.remove(&self.connection_id);
            }
        }
        self.pool.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        thread,
        time::Instant,
    };

    #[test]
    fn webdav_connection_slots_cancel_while_two_reads_are_held() {
        let pool = SlotPool::new();
        let first = pool
            .acquire(7, &AtomicBool::new(false))
            .expect("first slot");
        let second = pool
            .acquire(7, &AtomicBool::new(false))
            .expect("second slot");
        let cancel = Arc::new(AtomicBool::new(false));
        let waiting = pool.clone();
        let flag = cancel.clone();
        let started = Instant::now();
        let handle = thread::spawn(move || waiting.acquire(7, &flag));
        thread::sleep(Duration::from_millis(100));
        cancel.store(true, Ordering::Release);
        assert!(handle.join().unwrap().is_none());
        assert!(started.elapsed() < Duration::from_secs(3));
        drop(first);
        drop(second);
        assert!(
            pool.acquire(7, &AtomicBool::new(false)).is_some(),
            "released slots stay usable"
        );
    }
}
