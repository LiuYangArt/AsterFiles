//! Windows accent observation stays outside the UI apartment (#173).
use std::{sync::mpsc, thread};

use windows::{
    Foundation::TypedEventHandler,
    UI::ViewManagement::{UIColorType, UISettings},
    Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

pub struct AccentWatcher {
    stop: mpsc::Sender<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl AccentWatcher {
    pub fn start() -> std::io::Result<(Self, mpsc::Receiver<Option<u32>>)> {
        let (sender, receiver) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("windows-accent".into())
            .spawn(move || {
                // SAFETY: This dedicated thread owns its WinRT apartment and balances success below.
                if unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.is_err() {
                    let _ = sender.send(None);
                    return;
                }
                observe(sender, stopped);
                // SAFETY: All local WinRT objects and event subscriptions have been released on this thread.
                unsafe { RoUninitialize() };
            })?;
        Ok((
            Self {
                stop,
                worker: Some(worker),
            },
            receiver,
        ))
    }
}

impl Drop for AccentWatcher {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn read_accent(settings: &UISettings) -> Option<u32> {
    settings
        .GetColorValue(UIColorType::Accent)
        .ok()
        .map(|color| {
            0xff00_0000 | u32::from(color.R) << 16 | u32::from(color.G) << 8 | u32::from(color.B)
        })
}

fn observe(sender: mpsc::Sender<Option<u32>>, stopped: mpsc::Receiver<()>) {
    let Ok(settings) = UISettings::new() else {
        let _ = sender.send(None);
        return;
    };
    let changed = sender.clone();
    let token = settings.ColorValuesChanged(&TypedEventHandler::<
        UISettings,
        windows::core::IInspectable,
    >::new(move |settings, _| {
        // Windows dispatches this notification on a background thread, never on the UI thread.
        let accent = settings.as_ref().and_then(read_accent);
        let _ = changed.send(accent);
        Ok(())
    }));
    let _ = sender.send(read_accent(&settings));
    if let Ok(token) = token {
        let _ = stopped.recv();
        let _ = settings.RemoveColorValuesChanged(token);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn issue_173_accent_watcher_reads_and_stops_without_a_window() {
        let (watcher, receiver) = super::AccentWatcher::start().unwrap();
        let accent = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        eprintln!("Windows accent initial read: {accent:08x?}");
        if let Some(accent) = accent {
            assert_eq!(accent >> 24, 0xff);
        }
        drop(watcher);
    }
}
