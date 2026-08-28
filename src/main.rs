#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod domain;
mod fs;
mod i18n;
mod platform;
mod session_store;

fn main() -> Result<(), slint::PlatformError> {
    #[cfg(windows)]
    {
        use slint::winit_030::winit::platform::windows::{
            CornerPreference, WindowAttributesExtWindows,
        };

        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(|attributes| {
                attributes
                    .with_decorations(false)
                    .with_undecorated_shadow(true)
                    .with_corner_preference(CornerPreference::Round)
            })
            .select()?;
    }

    app::run()
}
