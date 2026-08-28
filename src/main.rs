mod app;
mod domain;
mod fs;

fn main() -> Result<(), slint::PlatformError> {
    app::run()
}
