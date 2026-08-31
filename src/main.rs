#![cfg_attr(windows, windows_subsystem = "windows")]

mod agent_debug;
mod app;
mod domain;
mod fs;
mod i18n;
mod platform;
mod session_store;

fn main() -> Result<(), Box<dyn std::error::Error>> {
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

    let agent_options = agent_debug::AgentOptions::from_env()
        .map_err(|message| std::io::Error::new(std::io::ErrorKind::InvalidInput, message))?;

    if let Some(scenario) = agent_options.scenario {
        if scenario == agent_debug::AgentScenario::MultiWindowStateLayering {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_multi_window_state_layering(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        if scenario == agent_debug::AgentScenario::TabReorder {
            let output = agent_options
                .state_output()
                .expect("scenario has a default state output");
            app::export_tab_reorder_state(&output)?;
            println!(
                "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
                scenario.name(),
                output.to_string_lossy().as_ref()
            );
            if agent_options.no_ui {
                return Ok(());
            }
        }
        let mut session = domain::TabSession::new(domain::TabId(1));
        agent_debug::apply_scenario(&mut session, scenario);
        let output = agent_options
            .state_output()
            .expect("scenario has a default state output");
        agent_debug::export_state(&session, scenario, &output)?;
        println!(
            "{{\"event\":\"agent_state_exported\",\"scenario\":\"{}\",\"artifact\":{:?}}}",
            scenario.name(),
            output.to_string_lossy().as_ref()
        );
        if agent_options.no_ui {
            return Ok(());
        }
    }

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
                    .with_drag_and_drop(false)
                    .with_undecorated_shadow(true)
                    .with_corner_preference(CornerPreference::Round)
            })
            .select()?;
    }

    app::run(agent_options.scenario)?;
    Ok(())
}
