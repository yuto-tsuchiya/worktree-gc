use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::ffi::OsString;
use std::str::FromStr;

mod config;
mod defaults;
mod gc;
mod history;
mod logging;
mod platform_scheduler;
mod schedule;
mod ui;
mod update;
mod workspace;

#[cfg(test)]
use config::WorkspaceConfig;
use config::{load_runtime_config, runtime_config_path, save_runtime_config, RuntimeConfigFile};
use defaults::{default_dir, default_log_file, validate_registration_name};

/// Automatically clean up git worktrees whose pull requests have been merged.
#[derive(Parser, Debug, Clone)]
#[command(name = "worktree-gc", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Directory to scan for git repositories
    #[arg(short, long, env = "WORKTREE_GC_DIR", default_value_t = default_dir(), global = true)]
    dir: String,

    /// Show what would be removed without actually removing
    #[arg(short = 'n', long, global = true)]
    dry_run: bool,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Log file path (JSONL format)
    #[arg(long, env = "WORKTREE_GC_LOG", default_value_t = default_log_file(), global = true)]
    log_file: String,

    /// Named workspace to use from the runtime configuration
    #[arg(long, global = true)]
    workspace: Option<String>,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// Run worktree garbage collection
    Run,
    /// Show or update runtime configuration
    Config {
        #[command(subcommand)]
        action: Option<ConfigAction>,
    },
    /// Manage scheduled automatic execution (interactive wizard if no subcommand given)
    Schedule {
        #[command(subcommand)]
        action: Option<ScheduleAction>,
    },
    /// Manage named workspaces (interactive wizard if no subcommand given)
    Workspace {
        #[command(subcommand)]
        action: Option<WorkspaceAction>,
    },
    /// Show execution history from the JSONL log
    History {
        /// Number of recent records to show, or "all"
        #[arg(long, default_value = "10")]
        last: HistoryLast,
        /// Filter by action: removed, skipped, error, summary
        #[arg(short, long)]
        action: Option<String>,
        /// Filter by repo (substring match)
        #[arg(short, long)]
        repo: Option<String>,
    },
    /// Update worktree-gc to the latest release from GitHub
    Update {
        /// Only check for a newer version without installing it
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ConfigAction {
    /// Save current runtime option values as defaults
    Set,
    /// Remove a saved runtime setting
    Unset {
        #[command(subcommand)]
        field: ConfigField,
    },
}

#[derive(Subcommand, Debug, Clone)]
enum ConfigField {
    /// Remove the saved work directory default
    Dir,
    /// Remove the saved log file default
    LogFile,
    /// Remove all saved runtime defaults
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryLast {
    Count(usize),
    All,
}

impl FromStr for HistoryLast {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }

        let count = value.parse::<usize>().map_err(|_| {
            format!("invalid value '{value}': expected a positive integer or 'all'")
        })?;

        Ok(Self::Count(count))
    }
}

#[derive(Subcommand, Debug, Clone)]
enum ScheduleAction {
    /// Install daily scheduled execution (launchd on macOS, systemd on Linux)
    Install {
        /// Schedule name
        #[arg(long, default_value = "default")]
        name: String,
        /// Hour to run (0-23)
        #[arg(long, default_value_t = 9)]
        hour: u8,
        /// Minute to run (0-59)
        #[arg(long, default_value_t = 0)]
        minute: u8,
    },
    /// Remove scheduled execution
    Uninstall {
        /// Schedule name
        #[arg(long, default_value = "default")]
        name: String,
        /// Remove all registered schedules
        #[arg(long)]
        all: bool,
    },
    /// List registered schedules
    List,
}

#[derive(Subcommand, Debug, Clone)]
enum WorkspaceAction {
    /// Add or update a named workspace
    Add {
        /// Workspace name
        name: String,
        /// Directory to scan for git repositories
        #[arg(short, long)]
        dir: String,
    },
    /// Remove a named workspace
    Remove {
        /// Workspace name
        name: String,
    },
    /// List registered workspaces
    List,
}

fn main() -> Result<()> {
    let raw_args: Vec<OsString> = std::env::args_os().collect();
    let mut cli = Cli::parse();
    apply_saved_runtime_config(&mut cli, &raw_args)?;
    let command = match cli.command.clone() {
        Some(command) => command,
        None => {
            if should_open_interactive_menu(&raw_args) {
                match interactive_command_menu(cli.dry_run) {
                    Ok(Some(command)) => command,
                    Ok(None) => return Ok(()),
                    Err(err) => return Err(err),
                }
            } else {
                Commands::Run
            }
        }
    };

    execute_command(&cli, command, &raw_args)
}

fn should_open_interactive_menu(args: &[OsString]) -> bool {
    args.len() <= 1
}

fn has_cli_option(args: &[OsString], short: Option<&str>, long: &str) -> bool {
    args.iter().skip(1).any(|arg| {
        let value = arg.to_string_lossy();
        short.is_some_and(|short| value == short)
            || value == long
            || value.starts_with(&format!("{long}="))
    })
}

fn apply_saved_runtime_config(cli: &mut Cli, args: &[OsString]) -> Result<()> {
    let config = load_runtime_config()?;
    apply_runtime_config_values(
        cli,
        args,
        &config,
        std::env::var_os("WORKTREE_GC_DIR").is_some(),
        std::env::var_os("WORKTREE_GC_LOG").is_some(),
    )
}

fn apply_runtime_config_values(
    cli: &mut Cli,
    args: &[OsString],
    config: &RuntimeConfigFile,
    env_dir_set: bool,
    env_log_set: bool,
) -> Result<()> {
    let dir_explicit = has_cli_option(args, Some("-d"), "--dir") || env_dir_set;
    let log_explicit = has_cli_option(args, None, "--log-file") || env_log_set;
    let workspace = if let Some(name) = cli.workspace.as_deref() {
        validate_registration_name("workspace", name)?;
        Some(
            config
                .find_workspace(name)
                .with_context(|| format!("Workspace not found: {name}"))?,
        )
    } else {
        None
    };

    if !dir_explicit {
        if let Some(workspace) = workspace {
            cli.dir = workspace.dir.clone();
        } else if let Some(dir) = &config.dir {
            cli.dir = dir.clone();
        }
    }

    if !log_explicit {
        if let Some(workspace_log_file) =
            workspace.and_then(|workspace| workspace.log_file.as_ref())
        {
            cli.log_file = workspace_log_file.clone();
        } else if let Some(log_file) = &config.log_file {
            cli.log_file = log_file.clone();
        }
    }

    Ok(())
}

fn interactive_command_menu(dry_run: bool) -> Result<Option<Commands>> {
    use dialoguer::Select;

    println!();
    println!("  {}", ui::title("worktree-gc"));
    println!("  {}", ui::subtitle("Select a command"));
    if dry_run {
        println!("  {} {}", ui::label("Run mode"), ui::warning("dry-run"));
    }
    println!();

    let run_label = if dry_run {
        "Run cleanup now (--dry-run)"
    } else {
        "Run cleanup now"
    };
    let choices = vec![
        run_label,
        "Manage schedule",
        "Manage workspaces",
        "Show history",
        "Show config",
        "Cancel",
    ];

    let selection = Select::new()
        .with_prompt("Choose a command")
        .items(&choices)
        .default(0)
        .interact()?;

    match selection {
        0 => Ok(Some(Commands::Run)),
        1 => Ok(Some(Commands::Schedule { action: None })),
        2 => Ok(Some(Commands::Workspace { action: None })),
        3 => Ok(Some(Commands::History {
            last: HistoryLast::Count(10),
            action: None,
            repo: None,
        })),
        4 => Ok(Some(Commands::Config { action: None })),
        _ => {
            println!("{}", ui::muted("Cancelled."));
            Ok(None)
        }
    }
}

fn execute_command(cli: &Cli, command: Commands, raw_args: &[OsString]) -> Result<()> {
    match command {
        Commands::Schedule { action } => schedule::execute_action(cli, action, raw_args),
        Commands::Workspace { action } => match action {
            Some(action) => workspace::execute_action(cli, action, raw_args),
            None => workspace::interactive_wizard(cli),
        },
        Commands::Config { action } => match action {
            Some(ConfigAction::Set) => set_runtime_config(cli, raw_args),
            Some(ConfigAction::Unset { field }) => unset_runtime_config(field),
            None => show_config(cli),
        },
        Commands::History { last, action, repo } => {
            history::show_history(&cli.log_file, last, action.as_deref(), repo.as_deref())
        }
        Commands::Update { check } => update::run(check),
        Commands::Run => run_gc_from_command(cli, raw_args),
    }
}

fn run_gc_from_command(cli: &Cli, raw_args: &[OsString]) -> Result<()> {
    if should_open_interactive_menu(raw_args) && cli.workspace.is_none() {
        let mut cli = cli.clone();
        workspace::prompt_run_if_needed(&mut cli)?;
        return run_gc(&cli);
    }

    run_gc(cli)
}

fn show_config(cli: &Cli) -> Result<()> {
    let config_path = runtime_config_path()?;
    let saved = load_runtime_config()?;

    println!("{}", ui::title("Runtime configuration"));
    println!("  {} {}", ui::label("Work dir"), ui::path(&cli.dir));
    println!("  {} {}", ui::label("Dry run"), ui::enabled(cli.dry_run));
    println!("  {} {}", ui::label("Verbose"), ui::enabled(cli.verbose));
    println!("  {} {}", ui::label("Log file"), ui::path(&cli.log_file));
    println!(
        "  {} {}",
        ui::label("Config"),
        ui::path(&config_path.display().to_string())
    );
    println!(
        "  {} {}",
        ui::label("Saved dir"),
        saved
            .dir
            .as_deref()
            .map(ui::path)
            .unwrap_or_else(|| ui::muted("(not set)"))
    );
    println!(
        "  {} {}",
        ui::label("Saved log"),
        saved
            .log_file
            .as_deref()
            .map(ui::path)
            .unwrap_or_else(|| ui::muted("(not set)"))
    );
    println!();
    workspace::print_registered(&saved);
    println!();
    schedule::print_registered(&saved);
    println!();

    platform_scheduler::print_config(&schedule::configured_names(&saved))
}

fn set_runtime_config(cli: &Cli, raw_args: &[OsString]) -> Result<()> {
    let mut config = load_runtime_config()?;
    let set_dir = has_cli_option(raw_args, Some("-d"), "--dir");
    let set_log_file = has_cli_option(raw_args, None, "--log-file");

    if !set_dir && !set_log_file {
        bail!(
            "Specify at least one setting to save, e.g. `worktree-gc config set -d /path/to/repos`"
        );
    }

    if set_dir {
        config.dir = Some(cli.dir.clone());
    }
    if set_log_file {
        config.log_file = Some(cli.log_file.clone());
    }

    save_runtime_config(&config)?;

    println!("{}", ui::success("Saved runtime defaults"));
    if set_dir {
        println!("  {} {}", ui::label("Work dir"), ui::path(&cli.dir));
    }
    if set_log_file {
        println!("  {} {}", ui::label("Log file"), ui::path(&cli.log_file));
    }
    println!(
        "  {} {}",
        ui::label("Config"),
        ui::path(&runtime_config_path()?.display().to_string())
    );
    Ok(())
}

fn unset_runtime_config(field: ConfigField) -> Result<()> {
    let mut config = load_runtime_config()?;

    match field {
        ConfigField::Dir => config.dir = None,
        ConfigField::LogFile => config.log_file = None,
        ConfigField::All => {
            config.dir = None;
            config.log_file = None;
        }
    }

    save_runtime_config(&config)?;

    match field {
        ConfigField::Dir => println!("{}", ui::success("Removed saved work directory default.")),
        ConfigField::LogFile => println!("{}", ui::success("Removed saved log file default.")),
        ConfigField::All => println!("{}", ui::success("Removed all saved runtime defaults.")),
    }

    Ok(())
}

fn run_gc(cli: &Cli) -> Result<()> {
    gc::run(gc::RunOptions {
        dir: &cli.dir,
        dry_run: cli.dry_run,
        verbose: cli.verbose,
        log_file: &cli.log_file,
    })
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_history_command_uses_defaults() {
        let cli = Cli {
            command: None,
            dir: ".".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/tmp/does-not-exist.jsonl".to_string(),
            workspace: None,
        };

        let result = execute_command(
            &cli,
            Commands::History {
                last: HistoryLast::Count(10),
                action: None,
                repo: None,
            },
            &[OsString::from("worktree-gc"), OsString::from("history")],
        );

        assert!(result.is_ok());
    }

    #[test]
    fn test_default_dir_is_current_dir() {
        let expected = std::env::current_dir().unwrap();
        assert_eq!(default_dir(), expected.to_string_lossy().to_string());
    }

    #[test]
    fn test_should_open_interactive_menu_only_for_bare_command() {
        assert!(should_open_interactive_menu(&[OsString::from(
            "worktree-gc"
        )]));
        assert!(!should_open_interactive_menu(&[
            OsString::from("worktree-gc"),
            OsString::from("--dry-run")
        ]));
        assert!(!should_open_interactive_menu(&[
            OsString::from("worktree-gc"),
            OsString::from("-d"),
            OsString::from("/tmp/repos")
        ]));
    }

    #[test]
    fn test_has_cli_option_detects_short_and_long_forms() {
        assert!(has_cli_option(
            &[OsString::from("worktree-gc"), OsString::from("-d")],
            Some("-d"),
            "--dir"
        ));
        assert!(has_cli_option(
            &[
                OsString::from("worktree-gc"),
                OsString::from("--dir=/tmp/repos")
            ],
            Some("-d"),
            "--dir"
        ));
        assert!(has_cli_option(
            &[
                OsString::from("worktree-gc"),
                OsString::from("--log-file"),
                OsString::from("/tmp/gc.jsonl")
            ],
            None,
            "--log-file"
        ));
        assert!(!has_cli_option(
            &[OsString::from("worktree-gc"), OsString::from("config")],
            Some("-d"),
            "--dir"
        ));
    }

    #[test]
    fn test_runtime_config_deserializes_legacy_fields() {
        let config: RuntimeConfigFile =
            serde_json::from_str(r#"{"dir":"/repos","log_file":"/tmp/gc.jsonl"}"#).unwrap();

        assert_eq!(config.dir.as_deref(), Some("/repos"));
        assert_eq!(config.log_file.as_deref(), Some("/tmp/gc.jsonl"));
        assert!(config.workspaces.is_empty());
        assert!(config.schedules.is_empty());
    }

    #[test]
    fn test_apply_runtime_config_uses_workspace_before_legacy_defaults() {
        let mut cli = Cli {
            command: None,
            dir: "/builtin".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/builtin/gc.jsonl".to_string(),
            workspace: Some("team".to_string()),
        };
        let config = RuntimeConfigFile {
            dir: Some("/legacy".to_string()),
            log_file: Some("/legacy/gc.jsonl".to_string()),
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/workspaces/team".to_string(),
                log_file: Some("/workspaces/team/gc.jsonl".to_string()),
            }],
            schedules: Vec::new(),
        };

        apply_runtime_config_values(
            &mut cli,
            &[
                OsString::from("worktree-gc"),
                OsString::from("run"),
                OsString::from("--workspace"),
                OsString::from("team"),
            ],
            &config,
            false,
            false,
        )
        .unwrap();

        assert_eq!(cli.dir, "/workspaces/team");
        assert_eq!(cli.log_file, "/workspaces/team/gc.jsonl");
    }

    #[test]
    fn test_apply_runtime_config_cli_dir_overrides_workspace_dir() {
        let mut cli = Cli {
            command: None,
            dir: "/override".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/builtin/gc.jsonl".to_string(),
            workspace: Some("team".to_string()),
        };
        let config = RuntimeConfigFile {
            dir: Some("/legacy".to_string()),
            log_file: Some("/legacy/gc.jsonl".to_string()),
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/workspaces/team".to_string(),
                log_file: Some("/workspaces/team/gc.jsonl".to_string()),
            }],
            schedules: Vec::new(),
        };

        apply_runtime_config_values(
            &mut cli,
            &[
                OsString::from("worktree-gc"),
                OsString::from("run"),
                OsString::from("--workspace"),
                OsString::from("team"),
                OsString::from("--dir"),
                OsString::from("/override"),
            ],
            &config,
            false,
            false,
        )
        .unwrap();

        assert_eq!(cli.dir, "/override");
        assert_eq!(cli.log_file, "/workspaces/team/gc.jsonl");
    }

    #[test]
    fn test_apply_runtime_config_uses_global_log_when_workspace_log_is_default() {
        let mut cli = Cli {
            command: None,
            dir: "/builtin".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/builtin/gc.jsonl".to_string(),
            workspace: Some("team".to_string()),
        };
        let config = RuntimeConfigFile {
            dir: Some("/legacy".to_string()),
            log_file: Some("/global/gc.jsonl".to_string()),
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/workspaces/team".to_string(),
                log_file: None,
            }],
            schedules: Vec::new(),
        };

        apply_runtime_config_values(
            &mut cli,
            &[
                OsString::from("worktree-gc"),
                OsString::from("--workspace=team"),
            ],
            &config,
            false,
            false,
        )
        .unwrap();

        assert_eq!(cli.dir, "/workspaces/team");
        assert_eq!(cli.log_file, "/global/gc.jsonl");
    }

    #[test]
    fn test_apply_runtime_config_explicit_log_overrides_workspace_log() {
        let mut cli = Cli {
            command: None,
            dir: "/builtin".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/explicit/gc.jsonl".to_string(),
            workspace: Some("team".to_string()),
        };
        let config = RuntimeConfigFile {
            dir: Some("/legacy".to_string()),
            log_file: Some("/global/gc.jsonl".to_string()),
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/workspaces/team".to_string(),
                log_file: Some("/workspaces/team/gc.jsonl".to_string()),
            }],
            schedules: Vec::new(),
        };

        apply_runtime_config_values(
            &mut cli,
            &[
                OsString::from("worktree-gc"),
                OsString::from("--workspace"),
                OsString::from("team"),
                OsString::from("--log-file=/explicit/gc.jsonl"),
            ],
            &config,
            false,
            false,
        )
        .unwrap();

        assert_eq!(cli.dir, "/workspaces/team");
        assert_eq!(cli.log_file, "/explicit/gc.jsonl");
    }

    #[test]
    fn test_apply_runtime_config_rejects_unknown_workspace() {
        let mut cli = Cli {
            command: None,
            dir: "/builtin".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/builtin/gc.jsonl".to_string(),
            workspace: Some("missing".to_string()),
        };

        let result = apply_runtime_config_values(
            &mut cli,
            &[
                OsString::from("worktree-gc"),
                OsString::from("--workspace=missing"),
            ],
            &RuntimeConfigFile::default(),
            false,
            false,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_validate_registration_name() {
        assert!(validate_registration_name("workspace", "team-a_1").is_ok());
        assert!(validate_registration_name("workspace", "Team").is_err());
        assert!(validate_registration_name("workspace", "../team").is_err());
        assert!(validate_registration_name("workspace", "").is_err());
    }

    #[test]
    fn test_history_last_parses_all() {
        assert_eq!("all".parse::<HistoryLast>().unwrap(), HistoryLast::All);
        assert_eq!("ALL".parse::<HistoryLast>().unwrap(), HistoryLast::All);
    }

    #[test]
    fn test_history_last_parses_count() {
        assert_eq!("50".parse::<HistoryLast>().unwrap(), HistoryLast::Count(50));
    }

    #[test]
    fn test_history_last_rejects_invalid_value() {
        assert!("foo".parse::<HistoryLast>().is_err());
    }
}
