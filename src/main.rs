#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod domain;
mod fs;
mod i18n;
mod platform;
mod session_store;

fn main() -> Result<(), slint::PlatformError> {
    app::run()
}
