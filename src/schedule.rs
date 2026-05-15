use crate::config::{
    load_runtime_config, runtime_config_path, save_runtime_config, RuntimeConfigFile,
    ScheduleConfig, WorkspaceConfig,
};
use crate::defaults::validate_registration_name;
use crate::{has_cli_option, platform_scheduler, workspace, Cli, ScheduleAction};
use anyhow::{bail, Result};
use console::style;
use std::ffi::OsString;

pub(crate) fn execute_action(
    cli: &Cli,
    action: Option<ScheduleAction>,
    raw_args: &[OsString],
) -> Result<()> {
    match action {
        Some(ScheduleAction::Install { name, hour, minute }) => {
            install(cli, &name, hour, minute, raw_args)
        }
        Some(ScheduleAction::Uninstall { name, all }) => uninstall(&name, all),
        Some(ScheduleAction::List) => list(),
        None => interactive_wizard(cli),
    }
}

fn ensure_valid_daily_time(hour: u8, minute: u8) -> Result<()> {
    if hour >= 24 {
        bail!("Hour must be between 0 and 23");
    }
    if minute >= 60 {
        bail!("Minute must be between 0 and 59");
    }
    Ok(())
}

fn install(
    cli: &Cli,
    schedule_name: &str,
    hour: u8,
    minute: u8,
    raw_args: &[OsString],
) -> Result<()> {
    ensure_valid_daily_time(hour, minute)?;
    platform_scheduler::validate_schedule_name(schedule_name)?;

    let dir_explicit = has_cli_option(raw_args, Some("-d"), "--dir");
    let log_explicit = has_cli_option(raw_args, None, "--log-file");
    if cli.workspace.is_some() && (dir_explicit || log_explicit) {
        bail!("Use either --workspace or --dir/--log-file when installing a schedule, not both");
    }

    let mut config = load_runtime_config()?;
    let workspace_name = if let Some(workspace_name) = cli.workspace.as_deref() {
        validate_registration_name("workspace", workspace_name)?;
        if config.find_workspace(workspace_name).is_none() {
            bail!("Workspace not found: {workspace_name}");
        }
        workspace_name.to_string()
    } else {
        let workspace_name = "default".to_string();
        config.upsert_workspace(WorkspaceConfig {
            name: workspace_name.clone(),
            dir: cli.dir.clone(),
            log_file: Some(cli.log_file.clone()),
        });
        workspace_name
    };

    platform_scheduler::install_workspace(schedule_name, &workspace_name, hour, minute)?;
    config.upsert_schedule(ScheduleConfig {
        name: schedule_name.to_string(),
        workspace: workspace_name.clone(),
        hour,
        minute,
    });
    save_runtime_config(&config)?;

    println!("  Schedule:  {schedule_name}");
    println!("  Workspace: {workspace_name}");
    println!("  Config:    {}", runtime_config_path()?.display());
    Ok(())
}

fn uninstall(name: &str, all: bool) -> Result<()> {
    let mut config = load_runtime_config()?;

    if all {
        let mut names: Vec<String> = config
            .schedules
            .iter()
            .map(|schedule| schedule.name.clone())
            .collect();
        if !names.iter().any(|name| name == "default") {
            names.push("default".to_string());
        }
        names.sort();
        names.dedup();

        for schedule_name in &names {
            platform_scheduler::uninstall(schedule_name)?;
        }
        config.schedules.clear();
        save_runtime_config(&config)?;
        println!("Removed all registered schedules.");
        return Ok(());
    }

    platform_scheduler::validate_schedule_name(name)?;
    platform_scheduler::uninstall(name)?;
    config.remove_schedule(name);
    save_runtime_config(&config)?;
    println!("Removed schedule metadata: {name}");
    Ok(())
}

fn list() -> Result<()> {
    let config = load_runtime_config()?;
    print_registered(&config);
    println!();
    platform_scheduler::print_config(&configured_names(&config))
}

fn interactive_wizard(cli: &Cli) -> Result<()> {
    use dialoguer::Select;

    let mut config = load_runtime_config()?;
    println!();
    println!("  {}", style("worktree-gc scheduler").bold());
    println!("  {}", style("Manage automatic cleanup schedules").dim());
    println!();
    print_registered(&config);
    println!();
    workspace::print_registered(&config);
    println!();

    let choices = vec![
        "Add schedule",
        "Update schedule",
        "Remove schedule",
        "Show scheduler status",
        "Cancel",
    ];
    let default_choice = if config.schedules.is_empty() { 0 } else { 1 };
    let selection = Select::new()
        .with_prompt("What would you like to do?")
        .items(&choices)
        .default(default_choice)
        .interact()?;

    match selection {
        0 => add_schedule_interactive(cli, &mut config),
        1 => update_schedule_interactive(cli, &mut config),
        2 => remove_schedule_interactive(&mut config),
        3 => platform_scheduler::print_config(&configured_names(&config)),
        _ => {
            println!("Cancelled.");
            Ok(())
        }
    }
}

fn add_schedule_interactive(cli: &Cli, config: &mut RuntimeConfigFile) -> Result<()> {
    use dialoguer::{Confirm, Input};

    let name: String = Input::new()
        .with_prompt("Schedule name")
        .default(default_new_schedule_name(config))
        .validate_with(|input: &String| -> std::result::Result<(), String> {
            platform_scheduler::validate_schedule_name(input).map_err(|err| err.to_string())?;
            Ok(())
        })
        .interact_text()?;

    if config
        .schedules
        .iter()
        .any(|schedule| schedule.name == name)
    {
        bail!("Schedule already exists: {name}");
    }

    let workspace = workspace::prompt_choice(cli, config, None)?;
    let (hour, minute) = prompt_schedule_time("09:00")?;
    print_schedule_summary(&name, &workspace, hour, minute);

    if !Confirm::new()
        .with_prompt("Install this schedule?")
        .default(true)
        .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    install_from_parts(config, &name, &workspace, hour, minute)
}

fn update_schedule_interactive(cli: &Cli, config: &mut RuntimeConfigFile) -> Result<()> {
    use dialoguer::{Confirm, Select};

    if config.schedules.is_empty() {
        println!("No registered schedules to update.");
        return add_schedule_interactive(cli, config);
    }

    let schedule_labels: Vec<String> = config
        .schedules
        .iter()
        .map(|schedule| {
            format!(
                "{} ({}, {:02}:{:02})",
                schedule.name, schedule.workspace, schedule.hour, schedule.minute
            )
        })
        .collect();
    let selection = Select::new()
        .with_prompt("Select a schedule to update")
        .items(&schedule_labels)
        .default(0)
        .interact()?;
    let current = config.schedules[selection].clone();

    let workspace = workspace::prompt_choice(cli, config, Some(&current.workspace))?;
    let (hour, minute) =
        prompt_schedule_time(&format!("{:02}:{:02}", current.hour, current.minute))?;
    print_schedule_summary(&current.name, &workspace, hour, minute);

    if !Confirm::new()
        .with_prompt("Update this schedule?")
        .default(true)
        .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    install_from_parts(config, &current.name, &workspace, hour, minute)
}

fn remove_schedule_interactive(config: &mut RuntimeConfigFile) -> Result<()> {
    use dialoguer::{Confirm, Select};

    if config.schedules.is_empty() {
        println!("No registered schedules to remove.");
        return Ok(());
    }

    let schedule_labels: Vec<String> = config
        .schedules
        .iter()
        .map(|schedule| {
            format!(
                "{} ({}, {:02}:{:02})",
                schedule.name, schedule.workspace, schedule.hour, schedule.minute
            )
        })
        .collect();
    let selection = Select::new()
        .with_prompt("Select a schedule to remove")
        .items(&schedule_labels)
        .default(0)
        .interact()?;
    let schedule_name = config.schedules[selection].name.clone();

    if !Confirm::new()
        .with_prompt(format!("Remove schedule '{schedule_name}'?"))
        .default(false)
        .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    platform_scheduler::uninstall(&schedule_name)?;
    config.remove_schedule(&schedule_name);
    save_runtime_config(config)?;
    println!("Removed schedule metadata: {schedule_name}");
    Ok(())
}

fn install_from_parts(
    config: &mut RuntimeConfigFile,
    name: &str,
    workspace: &str,
    hour: u8,
    minute: u8,
) -> Result<()> {
    platform_scheduler::install_workspace(name, workspace, hour, minute)?;
    config.upsert_schedule(ScheduleConfig {
        name: name.to_string(),
        workspace: workspace.to_string(),
        hour,
        minute,
    });
    save_runtime_config(config)?;

    println!("  Schedule:  {name}");
    println!("  Workspace: {workspace}");
    println!("  Config:    {}", runtime_config_path()?.display());
    Ok(())
}

fn prompt_schedule_time(default_time: &str) -> Result<(u8, u8)> {
    use dialoguer::Input;

    let time: String = Input::new()
        .with_prompt("Run time (HH:MM)")
        .default(default_time.to_string())
        .validate_with(|input: &String| -> std::result::Result<(), &str> {
            if parse_daily_time(input).is_some() {
                Ok(())
            } else {
                Err("Must be in HH:MM format, for example 09:00 or 18:30")
            }
        })
        .interact_text()?;

    Ok(parse_daily_time(&time).expect("validated daily time"))
}

fn parse_daily_time(input: &str) -> Option<(u8, u8)> {
    let (hour, minute) = input.split_once(':')?;
    if hour.len() != 2 || minute.len() != 2 {
        return None;
    }

    let hour = hour.parse::<u8>().ok()?;
    let minute = minute.parse::<u8>().ok()?;

    if hour < 24 && minute < 60 {
        Some((hour, minute))
    } else {
        None
    }
}

fn default_new_schedule_name(config: &RuntimeConfigFile) -> String {
    if !config
        .schedules
        .iter()
        .any(|schedule| schedule.name == "default")
    {
        return "default".to_string();
    }

    let mut index = 2;
    loop {
        let name = format!("schedule-{index}");
        if !config
            .schedules
            .iter()
            .any(|schedule| schedule.name == name)
        {
            return name;
        }
        index += 1;
    }
}

fn print_schedule_summary(name: &str, workspace: &str, hour: u8, minute: u8) {
    println!();
    println!("  {}", style("Summary:").bold());
    println!("  Schedule:  {}", style(name).cyan());
    println!("  Workspace: {}", style(workspace).cyan());
    println!(
        "  Run daily: {}",
        style(format!("{hour:02}:{minute:02}")).cyan()
    );
    println!();
}

pub(crate) fn print_registered(config: &RuntimeConfigFile) {
    println!("Registered schedules:");
    if config.schedules.is_empty() {
        println!("  (none)");
        return;
    }

    for schedule in &config.schedules {
        println!("  {}", schedule.name);
        println!("    Workspace: {}", schedule.workspace);
        println!(
            "    Time:      {:02}:{:02} daily",
            schedule.hour, schedule.minute
        );
    }
}

pub(crate) fn configured_names(config: &RuntimeConfigFile) -> Vec<String> {
    config
        .schedules
        .iter()
        .map(|schedule| schedule.name.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_daily_time() {
        assert_eq!(parse_daily_time("09:00"), Some((9, 0)));
        assert_eq!(parse_daily_time("18:30"), Some((18, 30)));
        assert_eq!(parse_daily_time("24:00"), None);
        assert_eq!(parse_daily_time("09:60"), None);
        assert_eq!(parse_daily_time("9:00"), None);
    }

    #[test]
    fn test_default_new_schedule_name() {
        let mut config = RuntimeConfigFile::default();
        assert_eq!(default_new_schedule_name(&config), "default");

        config.schedules.push(ScheduleConfig {
            name: "default".to_string(),
            workspace: "personal".to_string(),
            hour: 9,
            minute: 0,
        });
        assert_eq!(default_new_schedule_name(&config), "schedule-2");

        config.schedules.push(ScheduleConfig {
            name: "schedule-2".to_string(),
            workspace: "work".to_string(),
            hour: 18,
            minute: 30,
        });
        assert_eq!(default_new_schedule_name(&config), "schedule-3");
    }
}
