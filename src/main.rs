mod app;
mod domain;
mod fs;
mod i18n;
mod session_store;

fn main() -> Result<(), slint::PlatformError> {
    app::run()
}
