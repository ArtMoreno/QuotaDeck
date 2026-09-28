use anyhow::Result;
use clap::Parser;
use herdr_agent_quota::cli::{Cli, Command};

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Applied before anything reads the environment, so every existing
    // `var_os("HERDR_PLUGIN_STATE_DIR")` lookup keeps working untouched.
    if let Some(directory) = cli.state_dir.as_ref() {
        std::env::set_var("HERDR_PLUGIN_STATE_DIR", directory);
    }
    match cli.command {
        Command::Refresh {
            provider,
            force,
            json,
        } => herdr_agent_quota::refresh::run(&provider.providers(), force, json),
        Command::Watch {
            provider,
            interval_seconds,
        } => herdr_agent_quota::refresh::watch(&provider.providers(), interval_seconds),
        Command::Startup { provider } => herdr_agent_quota::refresh::startup(&provider.providers()),
        Command::Event => herdr_agent_quota::refresh::event(),
        Command::Focus => herdr_agent_quota::refresh::focus(),
        Command::Dashboard { json: false } => herdr_agent_quota::dashboard::run(),
        Command::Dashboard { json: true } => herdr_agent_quota::dashboard::run_json(),
        Command::Settings => herdr_agent_quota::settings::run(),
        Command::Configure {
            check,
            apply,
            uninstall,
            agent,
            watch_interval_seconds,
            sidebar_layout,
            quota_percent,
            row_gap,
            fields,
            brand_colors,
            brand_glyphs,
            agent_order,
            low_quota_alert,
            reload_herdr,
        } => {
            let agents = if uninstall {
                let agents = herdr_agent_quota::cli::AgentSelection::from_uninstall_args(&agent);
                herdr_agent_quota::prefs::clear(herdr_agent_quota::prefs::UNINSTALL_AGENTS)?;
                agents.map_err(anyhow::Error::msg)?
            } else {
                herdr_agent_quota::cli::AgentSelection::from_args_or_env(&agent)
            };
            herdr_agent_quota::configure::run(
                check,
                apply,
                uninstall,
                &agents,
                herdr_agent_quota::cli::ConfigureOptions {
                    watch_interval_seconds,
                    sidebar_layout,
                    quota_percent,
                    row_gap,
                    fields,
                    brand_colors,
                    brand_glyphs,
                    agent_order,
                    low_quota_alert,
                },
            )
            .and_then(|()| {
                if reload_herdr {
                    herdr_agent_quota::herdr::reload_config()
                } else {
                    Ok(())
                }
            })
        }
        Command::OpenSettings => herdr_agent_quota::herdr::open_settings_pane(),
        Command::OpenDashboard => herdr_agent_quota::herdr::open_dashboard(),
        Command::OpenDashboardSplit => herdr_agent_quota::herdr::open_dashboard_split(),
        Command::ClaudeStatusline => herdr_agent_quota::configure::claude::run_statusline_hook(),
        Command::AgyStatusline => herdr_agent_quota::configure::agy::run_statusline_hook(),
        Command::ParseHerdrActionJson { log_id } => {
            use std::io::Read;
            let mut input = String::new();
            std::io::stdin().read_to_string(&mut input)?;
            println!(
                "{}",
                herdr_agent_quota::herdr::parse_action_json(&input, log_id.as_deref())?
            );
            Ok(())
        }
    }
}
