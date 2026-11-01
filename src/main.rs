#![cfg_attr(all(windows, not(test)), windows_subsystem = "windows")]

#[cfg(test)]
mod accessibility_tests;
mod agent_debug;
mod app;
mod group_projection;
mod i18n;
#[cfg(test)]
mod input_order_tests;
mod quick_menu_popup;
mod session_store;

use asterfiles_domain as domain;
use asterfiles_domain::{network, operation_audit};
use asterfiles_platform_windows as platform;
use asterfiles_platform_windows::fs;
use asterfiles_webdav as webdav;

#[cfg(windows)]
fn main_network_child() -> std::io::Result<bool> {
    platform::windows::network::try_run_child_from_args()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    struct AuditFlush;
    impl Drop for AuditFlush {
        fn drop(&mut self) {
            #[cfg(windows)]
            if let Err(error) = platform::windows::window_trace::flush() {
                eprintln!("window trace flush failed: {error}");
            }
            if let Err(error) = operation_audit::flush() {
                eprintln!("file operation audit flush failed: {error}");
            }
        }
    }
    let _audit_flush = AuditFlush;
    #[cfg(windows)]
    if main_network_child()? {
        return Ok(());
    }

    #[cfg(windows)]
    if platform::windows::network::try_run_operation_probe_from_args()? {
        return Ok(());
    }

    #[cfg(windows)]
    if platform::windows::shell_select_trap::probe::try_run()? {
        return Ok(());
    }

    let mut agent_options = agent_debug::AgentOptions::from_env()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;
    let external_paths = agent_options.take_external_paths();

    // The application owns its colors; keep native popup styling aligned with the startup system theme.
    unsafe {
        std::env::set_var(
            "SLINT_STYLE",
            if platform::system_uses_dark_theme() {
                "fluent-dark"
            } else {
                "fluent-light"
            },
        )
    };

    if let Some(scenario) = agent_options.scenario {
        let output = agent_options
            .state_output()
            .expect("scenario has a default state output");
        if agent_debug::export_scenario(
            scenario,
            &output,
            agent_options.no_ui,
            &mut std::io::stdout(),
        )? {
            return Ok(());
        }
    }

    #[cfg(windows)]
    if !external_paths.is_empty() {
        platform::windows::launch_probe::record(
            &platform::windows::launch_probe::collect(),
            &external_paths,
        );
    }

    #[cfg(windows)]
    if !agent_options.shell_ui
        && platform::windows::shell_select_trap::run_launcher(&external_paths)?
    {
        return Ok(());
    }

    #[cfg(windows)]
    let (external_paths, primary_instance, external_path_receiver, late_shell_select) = {
        use platform::windows::single_instance::{self, InstanceOutcome};
        if agent_options.shell_ui {
            let Some(mut primary) = single_instance::claim_primary()? else {
                return Ok(());
            };
            let receiver = primary.take_receiver();
            let request = receiver.recv_timeout(std::time::Duration::from_secs(10))?;
            (request.paths, primary, receiver, request.selection)
        } else {
            match single_instance::coordinate(&external_paths)? {
                InstanceOutcome::Primary(mut primary) => {
                    let receiver = primary.take_receiver();
                    (external_paths, primary, receiver, None)
                }
                InstanceOutcome::Forwarded => return Ok(()),
            }
        }
    };
    #[cfg(windows)]
    {
        use slint::winit_030::winit::platform::windows::{
            CornerPreference, WindowAttributesExtWindows,
        };
        use slint::winit_030::winit::window::WindowLevel;

        slint::BackendSelector::new()
            .backend_name("winit".into())
            .with_winit_window_attributes_hook(|attributes| {
                if let Some(owner) =
                    platform::windows::quick_menu_window::take_pending_window_owner()
                {
                    return attributes
                        .with_owner_window(owner as _)
                        .with_active(false)
                        .with_decorations(false)
                        .with_transparent(true)
                        .with_window_level(WindowLevel::Normal)
                        .with_visible(false)
                        .with_drag_and_drop(false)
                        .with_skip_taskbar(true)
                        .with_undecorated_shadow(false)
                        .with_corner_preference(CornerPreference::Round);
                }
                if attributes.title == "AsterFiles Tab Drag Preview" {
                    return attributes
                        .with_decorations(false)
                        .with_transparent(true)
                        .with_window_level(WindowLevel::AlwaysOnTop)
                        .with_visible(false)
                        .with_drag_and_drop(false)
                        .with_skip_taskbar(true)
                        .with_undecorated_shadow(false);
                }
                attributes
                    .with_decorations(false)
                    .with_drag_and_drop(false)
                    .with_undecorated_shadow(true)
                    .with_corner_preference(CornerPreference::Round)
            })
            .select()?;
    }

    app::run(
        agent_options.scenario,
        external_paths,
        Some(external_path_receiver),
        late_shell_select,
    )?;
    drop(primary_instance);
    Ok(())
}
