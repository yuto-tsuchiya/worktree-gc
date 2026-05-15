use crate::config::{
    load_runtime_config, runtime_config_path, save_runtime_config, RuntimeConfigFile,
    WorkspaceConfig,
};
use crate::defaults::{default_log_file, validate_registration_name};
use crate::ui;
use crate::{has_cli_option, Cli, WorkspaceAction};
use anyhow::{bail, Result};
use std::ffi::OsString;
use std::path::PathBuf;

pub(crate) fn execute_action(
    cli: &Cli,
    action: WorkspaceAction,
    raw_args: &[OsString],
) -> Result<()> {
    match action {
        WorkspaceAction::Add { name, dir } => {
            let log_file = if has_cli_option(raw_args, None, "--log-file") {
                Some(cli.log_file.clone())
            } else {
                None
            };
            add_workspace(&name, &dir, log_file)
        }
        WorkspaceAction::Remove { name } => remove_workspace(&name),
        WorkspaceAction::List => list_workspaces(),
    }
}

pub(crate) fn prompt_run_if_needed(cli: &mut Cli) -> Result<ui::Navigation> {
    let config = load_runtime_config()?;
    if config.workspaces.len() <= 1 {
        return Ok(ui::Navigation::Done);
    }

    let workspace_labels: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| format!("{} ({})", workspace.name, workspace.dir))
        .collect();
    let selection = match ui::select("Select workspace to run cleanup", &workspace_labels, 0)? {
        ui::MenuAction::Selected(selection) => selection,
        ui::MenuAction::Back => return Ok(ui::Navigation::Back),
    };
    let workspace = &config.workspaces[selection];

    cli.workspace = Some(workspace.name.clone());
    cli.dir = workspace.dir.clone();
    if let Some(log_file) = &workspace.log_file {
        cli.log_file = log_file.clone();
    } else if let Some(log_file) = &config.log_file {
        cli.log_file = log_file.clone();
    }

    println!();
    println!("  {}", ui::title("Run cleanup"));
    println!("  {} {}", ui::label("Workspace"), ui::name(&workspace.name));
    println!("  {} {}", ui::label("Directory"), ui::path(&cli.dir));
    println!("  {} {}", ui::label("Log file"), ui::path(&cli.log_file));
    println!();

    Ok(ui::Navigation::Done)
}

fn add_workspace(name: &str, dir: &str, log_file: Option<String>) -> Result<()> {
    validate_registration_name("workspace", name)?;
    let dir_path = PathBuf::from(dir);
    if !dir_path.is_dir() {
        bail!("Workspace directory does not exist: {}", dir_path.display());
    }

    let mut config = load_runtime_config()?;
    config.upsert_workspace(WorkspaceConfig {
        name: name.to_string(),
        dir: dir.to_string(),
        log_file,
    });
    save_runtime_config(&config)?;

    println!("{}", ui::success("Saved workspace"));
    println!("  {} {}", ui::label("Name"), ui::name(name));
    println!("  {} {}", ui::label("Dir"), ui::path(dir));
    if let Some(workspace) = config.find_workspace(name) {
        println!(
            "  {} {}",
            ui::label("Log"),
            ui::value(&workspace_log_display(workspace, &config))
        );
    }
    println!(
        "  {} {}",
        ui::label("Config"),
        ui::path(&runtime_config_path()?.display().to_string())
    );
    Ok(())
}

fn remove_workspace(name: &str) -> Result<()> {
    validate_registration_name("workspace", name)?;
    let mut config = load_runtime_config()?;
    let referencing_schedules: Vec<_> = config
        .schedules
        .iter()
        .filter(|schedule| schedule.workspace == name)
        .map(|schedule| schedule.name.clone())
        .collect();

    if !referencing_schedules.is_empty() {
        bail!(
            "Workspace '{name}' is used by schedule(s): {}. Remove those schedules first.",
            referencing_schedules.join(", ")
        );
    }

    if !config.remove_workspace(name) {
        bail!("Workspace not found: {name}");
    }

    save_runtime_config(&config)?;
    println!("{} {}", ui::success("Removed workspace:"), ui::name(name));
    Ok(())
}

fn list_workspaces() -> Result<()> {
    let config = load_runtime_config()?;
    print_registered(&config);
    Ok(())
}

pub(crate) fn interactive_wizard(cli: &Cli) -> Result<ui::Navigation> {
    let config = load_runtime_config()?;
    println!();
    println!("  {}", ui::title("worktree-gc workspaces"));
    println!("  {}", ui::subtitle("Manage named scan directories"));
    println!();
    print_registered(&config);
    println!();

    let choices = vec![
        "Add workspace",
        "Update workspace",
        "Remove workspace",
        "List workspaces",
        "Cancel",
    ];
    let default_choice = if config.workspaces.is_empty() { 0 } else { 1 };
    let selection = match ui::select("What would you like to do?", &choices, default_choice)? {
        ui::MenuAction::Selected(selection) => selection,
        ui::MenuAction::Back => return Ok(ui::Navigation::Back),
    };

    match selection {
        0 => add_workspace_interactive(cli, &config)?,
        1 => update_workspace_interactive(cli, &config)?,
        2 => remove_workspace_interactive(&config)?,
        3 => {
            print_registered(&config);
        }
        _ => {
            println!("{}", ui::muted("Cancelled."));
        }
    }
    Ok(ui::Navigation::Done)
}

fn add_workspace_interactive(cli: &Cli, config: &RuntimeConfigFile) -> Result<()> {
    use dialoguer::{Confirm, Input};

    let name: String = Input::new()
        .with_prompt("Workspace name")
        .default(default_new_workspace_name(config))
        .validate_with(|input: &String| -> std::result::Result<(), String> {
            validate_registration_name("workspace", input).map_err(|err| err.to_string())?;
            Ok(())
        })
        .interact_text()?;

    let default_log_file = cli.log_file.clone();
    let (dir, log_file) = prompt_workspace_settings(cli, None, &default_log_file)?;
    print_workspace_summary(&name, &dir, log_file.as_deref(), &default_log_file);

    if !Confirm::new()
        .with_prompt("Save this workspace?")
        .default(true)
        .interact()?
    {
        println!("Cancelled.");
        return Ok(());
    }

    add_workspace(&name, &dir, log_file)
}

fn update_workspace_interactive(cli: &Cli, config: &RuntimeConfigFile) -> Result<()> {
    use dialoguer::Confirm;

    if config.workspaces.is_empty() {
        println!("{}", ui::muted("No registered workspaces to update."));
        return Ok(());
    }

    let workspace_labels: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| format!("{} ({})", workspace.name, workspace.dir))
        .collect();
    let selection = match ui::select("Select a workspace to update", &workspace_labels, 0)? {
        ui::MenuAction::Selected(selection) => selection,
        ui::MenuAction::Back => return Ok(()),
    };
    let workspace = config.workspaces[selection].clone();

    let default_log_file = cli.log_file.clone();
    let (dir, log_file) = prompt_workspace_settings_for_existing(&workspace, &default_log_file)?;
    print_workspace_summary(
        &workspace.name,
        &dir,
        log_file.as_deref(),
        &default_log_file,
    );

    if !Confirm::new()
        .with_prompt("Update this workspace?")
        .default(true)
        .interact()?
    {
        println!("{}", ui::muted("Cancelled."));
        return Ok(());
    }

    add_workspace(&workspace.name, &dir, log_file)
}

fn remove_workspace_interactive(config: &RuntimeConfigFile) -> Result<()> {
    use dialoguer::Confirm;

    if config.workspaces.is_empty() {
        println!("{}", ui::muted("No registered workspaces to remove."));
        return Ok(());
    }

    let workspace_labels: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| format!("{} ({})", workspace.name, workspace.dir))
        .collect();
    let selection = match ui::select("Select a workspace to remove", &workspace_labels, 0)? {
        ui::MenuAction::Selected(selection) => selection,
        ui::MenuAction::Back => return Ok(()),
    };
    let workspace_name = config.workspaces[selection].name.clone();

    if !Confirm::new()
        .with_prompt(format!("Remove workspace '{workspace_name}'?"))
        .default(false)
        .interact()?
    {
        println!("{}", ui::muted("Cancelled."));
        return Ok(());
    }

    remove_workspace(&workspace_name)
}

fn prompt_workspace_settings(
    cli: &Cli,
    current: Option<&WorkspaceConfig>,
    default_effective_log_file: &str,
) -> Result<(String, Option<String>)> {
    let fallback_dir = current
        .map(|workspace| workspace.dir.clone())
        .unwrap_or_else(|| cli.dir.clone());
    let fallback_log = current
        .and_then(|workspace| workspace.log_file.clone())
        .unwrap_or_default();
    prompt_workspace_settings_with_defaults(
        &fallback_dir,
        &fallback_log,
        default_effective_log_file,
    )
}

fn prompt_workspace_settings_for_existing(
    current: &WorkspaceConfig,
    default_effective_log_file: &str,
) -> Result<(String, Option<String>)> {
    prompt_workspace_settings_with_defaults(
        &current.dir,
        current.log_file.as_deref().unwrap_or(""),
        default_effective_log_file,
    )
}

fn prompt_workspace_settings_with_defaults(
    default_dir: &str,
    default_log_file: &str,
    default_effective_log_file: &str,
) -> Result<(String, Option<String>)> {
    use dialoguer::Input;

    let dir: String = Input::new()
        .with_prompt("Directory to scan")
        .default(default_dir.to_string())
        .interact_text()?;
    let log_file: String = Input::new()
        .with_prompt(format!(
            "Log file (blank/default uses {default_effective_log_file})"
        ))
        .default(default_log_file.to_string())
        .allow_empty(true)
        .interact_text()?;
    let log_file = if log_file.trim().is_empty() || log_file.trim().eq_ignore_ascii_case("default")
    {
        None
    } else {
        Some(log_file)
    };

    Ok((dir, log_file))
}

pub(crate) fn print_registered(config: &RuntimeConfigFile) {
    println!("{}", ui::title("Registered workspaces"));
    if config.workspaces.is_empty() {
        println!("  {}", ui::muted("(none)"));
        return;
    }

    for workspace in &config.workspaces {
        println!("  {}", ui::name(&workspace.name));
        println!("    {} {}", ui::label("Dir"), ui::path(&workspace.dir));
        println!(
            "    {} {}",
            ui::label("Log"),
            ui::value(&workspace_log_display(workspace, config))
        );
    }
}

fn workspace_log_display(workspace: &WorkspaceConfig, config: &RuntimeConfigFile) -> String {
    workspace.log_file.clone().unwrap_or_else(|| {
        format!(
            "(global default: {})",
            config.log_file.clone().unwrap_or_else(default_log_file)
        )
    })
}

pub(crate) fn prompt_choice(
    cli: &Cli,
    config: &mut RuntimeConfigFile,
    current_workspace: Option<&str>,
) -> Result<Option<String>> {
    let mut workspace_names: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| workspace.name.clone())
        .collect();
    if let Some(current) = current_workspace {
        if !workspace_names.iter().any(|name| name == current) {
            workspace_names.push(current.to_string());
        }
    }
    workspace_names.sort();
    workspace_names.dedup();

    let current_index = current_workspace
        .and_then(|current| workspace_names.iter().position(|name| name == current))
        .unwrap_or(0);

    if workspace_names.is_empty() {
        config.upsert_workspace(WorkspaceConfig {
            name: "default".to_string(),
            dir: cli.dir.clone(),
            log_file: Some(cli.log_file.clone()),
        });
        return Ok(Some("default".to_string()));
    }

    let mut choices = workspace_names.clone();
    choices.push("Use current --dir as default workspace".to_string());
    let selection = match ui::select("Workspace", &choices, current_index)? {
        ui::MenuAction::Selected(selection) => selection,
        ui::MenuAction::Back => return Ok(None),
    };

    if selection < workspace_names.len() {
        return Ok(Some(workspace_names[selection].clone()));
    }

    config.upsert_workspace(WorkspaceConfig {
        name: "default".to_string(),
        dir: cli.dir.clone(),
        log_file: Some(cli.log_file.clone()),
    });
    Ok(Some("default".to_string()))
}

fn default_new_workspace_name(config: &RuntimeConfigFile) -> String {
    if !config
        .workspaces
        .iter()
        .any(|workspace| workspace.name == "default")
    {
        return "default".to_string();
    }

    let mut index = 2;
    loop {
        let name = format!("workspace-{index}");
        if !config
            .workspaces
            .iter()
            .any(|workspace| workspace.name == name)
        {
            return name;
        }
        index += 1;
    }
}

fn print_workspace_summary(
    name: &str,
    dir: &str,
    log_file: Option<&str>,
    default_effective_log_file: &str,
) {
    println!();
    println!("  {}", ui::title("Summary"));
    println!("  {} {}", ui::label("Workspace"), ui::name(name));
    println!("  {} {}", ui::label("Directory"), ui::path(dir));
    println!(
        "  {} {}",
        ui::label("Log file"),
        log_file
            .map(|log_file| log_file.to_string())
            .unwrap_or_else(|| format!("(global default: {default_effective_log_file})"))
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_new_workspace_name() {
        let mut config = RuntimeConfigFile::default();
        assert_eq!(default_new_workspace_name(&config), "default");

        config.workspaces.push(WorkspaceConfig {
            name: "default".to_string(),
            dir: "/repos/default".to_string(),
            log_file: None,
        });
        assert_eq!(default_new_workspace_name(&config), "workspace-2");

        config.workspaces.push(WorkspaceConfig {
            name: "workspace-2".to_string(),
            dir: "/repos/second".to_string(),
            log_file: None,
        });
        assert_eq!(default_new_workspace_name(&config), "workspace-3");
    }

    #[test]
    fn test_workspace_log_display_uses_global_default_when_workspace_log_is_absent() {
        let config = RuntimeConfigFile {
            log_file: Some("/global/gc.jsonl".to_string()),
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/repos/team".to_string(),
                log_file: None,
            }],
            ..RuntimeConfigFile::default()
        };

        assert_eq!(
            workspace_log_display(&config.workspaces[0], &config),
            "(global default: /global/gc.jsonl)"
        );
    }

    #[test]
    fn test_workspace_log_display_prefers_workspace_log() {
        let config = RuntimeConfigFile {
            log_file: Some("/global/gc.jsonl".to_string()),
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/repos/team".to_string(),
                log_file: Some("/team/gc.jsonl".to_string()),
            }],
            ..RuntimeConfigFile::default()
        };

        assert_eq!(
            workspace_log_display(&config.workspaces[0], &config),
            "/team/gc.jsonl"
        );
    }
}
