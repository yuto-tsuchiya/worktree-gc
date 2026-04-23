use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use console::style;
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;
use std::sync::Mutex;

mod scheduler;
mod update;

/// Automatically clean up git worktrees whose pull requests have been merged.
#[derive(Parser, Debug)]
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
enum HistoryLast {
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
        /// Hour to run (0-23)
        #[arg(long, default_value_t = 9)]
        hour: u8,
        /// Minute to run (0-59)
        #[arg(long, default_value_t = 0)]
        minute: u8,
    },
    /// Remove scheduled execution
    Uninstall,
}

fn default_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn default_log_file() -> String {
    dirs::home_dir()
        .map(|h| {
            h.join(".local/share/worktree-gc/gc.jsonl")
                .to_string_lossy()
                .to_string()
        })
        .unwrap_or_else(|| "gc.jsonl".to_string())
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct RuntimeConfigFile {
    dir: Option<String>,
    log_file: Option<String>,
}

fn runtime_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Cannot determine config directory")?;
    Ok(base.join("worktree-gc").join("config.json"))
}

fn load_runtime_config() -> Result<RuntimeConfigFile> {
    let path = runtime_config_path()?;
    if !path.exists() {
        return Ok(RuntimeConfigFile::default());
    }

    let content =
        fs::read_to_string(&path).with_context(|| format!("Cannot read {}", path.display()))?;
    let config = serde_json::from_str(&content)
        .with_context(|| format!("Cannot parse {}", path.display()))?;
    Ok(config)
}

fn save_runtime_config(config: &RuntimeConfigFile) -> Result<()> {
    let path = runtime_config_path()?;
    if config.dir.is_none() && config.log_file.is_none() {
        if path.exists() {
            fs::remove_file(&path).with_context(|| format!("Cannot remove {}", path.display()))?;
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("Cannot create {}", parent.display()))?;
    }

    let content = serde_json::to_string_pretty(config)?;
    fs::write(&path, content).with_context(|| format!("Cannot write {}", path.display()))?;
    Ok(())
}

// --- Git data structures ---

#[derive(Debug, Clone)]
struct RepoInfo {
    path: PathBuf,
    owner: String,
    repo: String,
}

#[derive(Debug, Clone)]
struct WorktreeEntry {
    path: PathBuf,
    branch: Option<String>,
    is_main: bool,
}

#[derive(Debug, Deserialize)]
struct GhPr {
    number: u64,
    url: String,
}

// --- Results tracking ---

#[derive(Debug, Default)]
struct GcResult {
    scanned_repos: usize,
    scanned_worktrees: usize,
    removed: Vec<String>,
    skipped: Vec<String>,
    errors: Vec<String>,
}

// --- Structured JSONL log ---

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum LogRecord {
    Removed {
        timestamp: String,
        repo: String,
        branch: String,
        worktree: String,
        pr_number: u64,
        pr_url: String,
    },
    Skipped {
        timestamp: String,
        repo: String,
        branch: String,
        worktree: String,
        reason: String,
    },
    Error {
        timestamp: String,
        repo: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
        error: String,
    },
    Summary {
        timestamp: String,
        scanned_repos: usize,
        scanned_worktrees: usize,
        removed_count: usize,
        skipped_count: usize,
        error_count: usize,
        dry_run: bool,
    },
}

fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

struct JsonLogger {
    file: Option<Mutex<fs::File>>,
}

impl JsonLogger {
    fn new(path: Option<&str>) -> Result<Self> {
        let file = match path {
            Some(p) => {
                let path = PathBuf::from(p);
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent)?;
                }
                let f = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("Cannot open log file: {}", path.display()))?;
                Some(Mutex::new(f))
            }
            None => None,
        };
        Ok(Self { file })
    }

    fn write(&self, record: &LogRecord) {
        if let Some(ref file) = self.file {
            if let Ok(json) = serde_json::to_string(record) {
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{json}");
                }
            }
        }
    }
}

// --- Core logic ---

/// Find all main git clones (directories with a `.git` directory) under `base_dir`.
fn find_repos(base_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    let entries = fs::read_dir(base_dir)
        .with_context(|| format!("Cannot read directory: {}", base_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let git_dir = path.join(".git");
            // Main clone has .git as a directory (not a file)
            if git_dir.is_dir() {
                repos.push(path);
            }
        }
    }
    repos.sort();
    Ok(repos)
}

/// Parse `git remote get-url origin` output into (owner, repo).
fn parse_github_remote(repo_path: &Path) -> Result<(String, String)> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git remote get-url")?;

    if !output.status.success() {
        bail!("git remote get-url failed for {}", repo_path.display());
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_owner_repo(&url)
}

/// Extract (owner, repo) from a GitHub URL.
///   https://github.com/OWNER/REPO.git
///   git@github.com:OWNER/REPO.git
fn parse_owner_repo(url: &str) -> Result<(String, String)> {
    let path_part = if let Some(rest) = url.strip_prefix("https://github.com/") {
        rest
    } else if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else {
        bail!("Not a GitHub URL: {url}");
    };

    let path_part = path_part.trim_end_matches(".git");
    let parts: Vec<&str> = path_part.splitn(2, '/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        bail!("Cannot parse owner/repo from: {url}");
    }
    Ok((parts[0].to_string(), parts[1].to_string()))
}

/// List worktrees for a repository using `git worktree list --porcelain`.
fn list_worktrees(repo_path: &Path) -> Result<Vec<WorktreeEntry>> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git worktree list")?;

    if !output.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_worktree_porcelain(&stdout, repo_path)
}

/// Parse porcelain output of `git worktree list`.
///
/// Format (entries separated by blank lines):
/// ```text
/// worktree /path/to/main
/// HEAD abc123
/// branch refs/heads/main
///
/// worktree /path/to/wt
/// HEAD def456
/// branch refs/heads/feature
/// ```
fn parse_worktree_porcelain(output: &str, main_repo: &Path) -> Result<Vec<WorktreeEntry>> {
    let mut entries = Vec::new();
    let mut current_path: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    for line in output.lines() {
        if line.is_empty() {
            if let Some(path) = current_path.take() {
                let is_main = path == main_repo;
                entries.push(WorktreeEntry {
                    path,
                    branch: current_branch.take(),
                    is_main,
                });
            }
            current_branch = None;
        } else if let Some(rest) = line.strip_prefix("worktree ") {
            current_path = Some(PathBuf::from(rest));
        } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
            current_branch = Some(rest.to_string());
        }
    }

    // Handle last entry (no trailing blank line)
    if let Some(path) = current_path.take() {
        let is_main = path == main_repo;
        entries.push(WorktreeEntry {
            path,
            branch: current_branch.take(),
            is_main,
        });
    }

    Ok(entries)
}

/// Check if a branch has a merged PR using `gh pr list`.
fn is_branch_merged(owner: &str, repo: &str, branch: &str) -> Result<Option<GhPr>> {
    let output = Command::new("gh")
        .args([
            "pr",
            "list",
            "--repo",
            &format!("{owner}/{repo}"),
            "--head",
            branch,
            "--state",
            "merged",
            "--json",
            "number,url",
            "--limit",
            "1",
        ])
        .output()
        .context("Failed to run gh pr list")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("gh pr list failed: {stderr}");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let prs: Vec<GhPr> = serde_json::from_str(&stdout)
        .with_context(|| format!("Failed to parse gh output: {stdout}"))?;

    Ok(prs.into_iter().next())
}

/// Remove a worktree and optionally delete the local branch.
fn remove_worktree(repo_path: &Path, wt: &WorktreeEntry) -> Result<()> {
    info!("  Removing worktree: {}", wt.path.display());

    let output = Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(&wt.path)
        .current_dir(repo_path)
        .output()
        .context("Failed to run git worktree remove")?;

    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Try to delete the local branch (best-effort)
    if let Some(branch) = &wt.branch {
        debug!("  Deleting local branch: {branch}");
        let output = Command::new("git")
            .args(["branch", "-D", branch])
            .current_dir(repo_path)
            .output();

        match output {
            Ok(o) if !o.status.success() => {
                debug!(
                    "  Could not delete branch {branch}: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => debug!("  Could not delete branch {branch}: {e}"),
            _ => debug!("  Deleted branch: {branch}"),
        }
    }

    Ok(())
}

/// Run `git worktree prune` on a repository.
fn prune_worktrees(repo_path: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["worktree", "prune"])
        .current_dir(repo_path)
        .output()
        .context("Failed to run git worktree prune")?;

    if !output.status.success() {
        bail!(
            "git worktree prune failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Process a single repository: list worktrees, check PRs, remove merged ones.
fn process_repo(repo: &RepoInfo, dry_run: bool, result: &mut GcResult, logger: &JsonLogger) {
    info!(
        "[{}] Checking {}/{}...",
        repo.path.display(),
        repo.owner,
        repo.repo
    );
    let full_repo = format!("{}/{}", repo.owner, repo.repo);

    let worktrees = match list_worktrees(&repo.path) {
        Ok(wts) => wts,
        Err(e) => {
            error!("  Failed to list worktrees: {e}");
            result.errors.push(format!("{}: {e}", repo.path.display()));
            logger.write(&LogRecord::Error {
                timestamp: now_iso(),
                repo: full_repo,
                branch: None,
                worktree: None,
                error: format!("Failed to list worktrees: {e}"),
            });
            return;
        }
    };

    let non_main: Vec<_> = worktrees.into_iter().filter(|w| !w.is_main).collect();
    if non_main.is_empty() {
        debug!("  No extra worktrees");
        return;
    }

    result.scanned_worktrees += non_main.len();
    let mut had_removal = false;

    for wt in &non_main {
        let branch = match &wt.branch {
            Some(b) => b,
            None => {
                debug!("  Skipping detached HEAD worktree: {}", wt.path.display());
                result
                    .skipped
                    .push(format!("{} (detached HEAD)", wt.path.display()));
                logger.write(&LogRecord::Skipped {
                    timestamp: now_iso(),
                    repo: full_repo.clone(),
                    branch: "(detached)".to_string(),
                    worktree: wt.path.display().to_string(),
                    reason: "detached HEAD".to_string(),
                });
                continue;
            }
        };

        debug!("  Checking branch: {branch}");

        match is_branch_merged(&repo.owner, &repo.repo, branch) {
            Ok(Some(pr)) => {
                info!(
                    "  ✓ MERGED: {} (branch: {branch}, PR #{} {})",
                    wt.path.display(),
                    pr.number,
                    pr.url
                );

                if dry_run {
                    info!("  [dry-run] Would remove: {}", wt.path.display());
                    result.removed.push(format!(
                        "{} (branch: {branch}, PR #{})",
                        wt.path.display(),
                        pr.number
                    ));
                    logger.write(&LogRecord::Removed {
                        timestamp: now_iso(),
                        repo: full_repo.clone(),
                        branch: branch.clone(),
                        worktree: wt.path.display().to_string(),
                        pr_number: pr.number,
                        pr_url: pr.url,
                    });
                } else {
                    match remove_worktree(&repo.path, wt) {
                        Ok(()) => {
                            result.removed.push(format!(
                                "{} (branch: {branch}, PR #{})",
                                wt.path.display(),
                                pr.number
                            ));
                            logger.write(&LogRecord::Removed {
                                timestamp: now_iso(),
                                repo: full_repo.clone(),
                                branch: branch.clone(),
                                worktree: wt.path.display().to_string(),
                                pr_number: pr.number,
                                pr_url: pr.url.clone(),
                            });
                            had_removal = true;
                        }
                        Err(e) => {
                            error!("  Failed to remove worktree: {e}");
                            result.errors.push(format!("{}: {e}", wt.path.display()));
                            logger.write(&LogRecord::Error {
                                timestamp: now_iso(),
                                repo: full_repo.clone(),
                                branch: Some(branch.clone()),
                                worktree: Some(wt.path.display().to_string()),
                                error: format!("{e}"),
                            });
                        }
                    }
                }
            }
            Ok(None) => {
                debug!("  Not merged: {branch}");
                result.skipped.push(format!(
                    "{} (branch: {branch}, not merged)",
                    wt.path.display()
                ));
                logger.write(&LogRecord::Skipped {
                    timestamp: now_iso(),
                    repo: full_repo.clone(),
                    branch: branch.clone(),
                    worktree: wt.path.display().to_string(),
                    reason: "not_merged".to_string(),
                });
            }
            Err(e) => {
                warn!("  Failed to check PR status for {branch}: {e}");
                result
                    .errors
                    .push(format!("{} (branch: {branch}): {e}", wt.path.display()));
                logger.write(&LogRecord::Error {
                    timestamp: now_iso(),
                    repo: full_repo.clone(),
                    branch: Some(branch.clone()),
                    worktree: Some(wt.path.display().to_string()),
                    error: format!("Failed to check PR status: {e}"),
                });
            }
        }
    }

    if had_removal {
        if let Err(e) = prune_worktrees(&repo.path) {
            warn!("  Failed to prune: {e}");
        }
    }
}

/// Set up logging to stderr (human-readable text).
fn setup_logging(verbose: bool) -> Result<()> {
    let level = if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };

    env_logger::Builder::new()
        .filter_level(level)
        .format(|buf, record| {
            let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
            writeln!(buf, "[{ts}] {}: {}", record.level(), record.args())
        })
        .init();

    Ok(())
}

/// Check that required external tools are available.
fn check_prerequisites() -> Result<()> {
    for cmd in &["git", "gh"] {
        let status = Command::new(cmd).arg("--version").output();
        match status {
            Ok(o) if o.status.success() => {}
            _ => bail!("{cmd} is not installed or not in PATH"),
        }
    }

    let output = Command::new("gh")
        .args(["auth", "status"])
        .output()
        .context("Failed to check gh auth")?;

    if !output.status.success() {
        bail!("gh is not authenticated. Run `gh auth login` first.");
    }

    Ok(())
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

    if !has_cli_option(args, Some("-d"), "--dir") && std::env::var_os("WORKTREE_GC_DIR").is_none() {
        if let Some(dir) = config.dir {
            cli.dir = dir;
        }
    }

    if !has_cli_option(args, None, "--log-file") && std::env::var_os("WORKTREE_GC_LOG").is_none() {
        if let Some(log_file) = config.log_file {
            cli.log_file = log_file;
        }
    }

    Ok(())
}

fn interactive_command_menu(dry_run: bool) -> Result<Option<Commands>> {
    use dialoguer::Select;

    println!();
    println!("  {}", style("worktree-gc").bold());
    println!("  {}", style("Select a command").dim());
    if dry_run {
        println!("  {}", style("Run mode: dry-run").cyan());
    }
    println!();

    let run_label = if dry_run {
        "Run cleanup now (--dry-run)"
    } else {
        "Run cleanup now"
    };
    let choices = vec![
        run_label,
        "Show config",
        "Manage schedule",
        "Show history",
        "Cancel",
    ];

    let selection = Select::new()
        .with_prompt("Choose a command")
        .items(&choices)
        .default(0)
        .interact()?;

    match selection {
        0 => Ok(Some(Commands::Run)),
        1 => Ok(Some(Commands::Config { action: None })),
        2 => Ok(Some(Commands::Schedule { action: None })),
        3 => Ok(Some(Commands::History {
            last: HistoryLast::Count(10),
            action: None,
            repo: None,
        })),
        _ => {
            println!("Cancelled.");
            Ok(None)
        }
    }
}

fn execute_command(cli: &Cli, command: Commands, raw_args: &[OsString]) -> Result<()> {
    match command {
        Commands::Schedule { action } => match action {
            Some(ScheduleAction::Install { hour, minute }) => {
                scheduler::install(&cli.dir, hour, minute)
            }
            Some(ScheduleAction::Uninstall) => scheduler::uninstall(),
            None => scheduler::interactive_wizard(&cli.dir),
        },
        Commands::Config { action } => match action {
            Some(ConfigAction::Set) => set_runtime_config(cli, raw_args),
            Some(ConfigAction::Unset { field }) => unset_runtime_config(field),
            None => show_config(cli),
        },
        Commands::History { last, action, repo } => {
            show_history(&cli.log_file, last, action.as_deref(), repo.as_deref())
        }
        Commands::Update { check } => update::run(check),
        Commands::Run => run_gc(cli),
    }
}

fn show_config(cli: &Cli) -> Result<()> {
    let config_path = runtime_config_path()?;
    let saved = load_runtime_config()?;

    println!("Runtime configuration:");
    println!("  Work dir:  {}", cli.dir);
    println!(
        "  Dry run:   {}",
        if cli.dry_run { "enabled" } else { "disabled" }
    );
    println!(
        "  Verbose:   {}",
        if cli.verbose { "enabled" } else { "disabled" }
    );
    println!("  Log file:  {}", cli.log_file);
    println!("  Config:    {}", config_path.display());
    println!(
        "  Saved dir: {}",
        saved.dir.as_deref().unwrap_or("(not set)")
    );
    println!(
        "  Saved log: {}",
        saved.log_file.as_deref().unwrap_or("(not set)")
    );
    println!();

    scheduler::print_config()
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

    println!("Saved runtime defaults:");
    if set_dir {
        println!("  Work dir:  {}", cli.dir);
    }
    if set_log_file {
        println!("  Log file:  {}", cli.log_file);
    }
    println!("  Config:    {}", runtime_config_path()?.display());
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
        ConfigField::Dir => println!("Removed saved work directory default."),
        ConfigField::LogFile => println!("Removed saved log file default."),
        ConfigField::All => println!("Removed all saved runtime defaults."),
    }

    Ok(())
}

fn show_history(
    log_file: &str,
    last: HistoryLast,
    action_filter: Option<&str>,
    repo_filter: Option<&str>,
) -> Result<()> {
    use std::io::BufRead;

    let path = PathBuf::from(log_file);
    if !path.exists() {
        println!(
            "No history found (log file does not exist: {})",
            path.display()
        );
        println!("Run `worktree-gc` or `worktree-gc --dry-run` to generate logs.");
        return Ok(());
    }

    let file = fs::File::open(&path)
        .with_context(|| format!("Cannot open log file: {}", path.display()))?;
    let reader = std::io::BufReader::new(file);

    let mut records: Vec<LogRecord> = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<LogRecord>(&line) {
            Ok(record) => records.push(record),
            Err(_) => continue, // skip malformed lines
        }
    }

    // Apply filters
    let records: Vec<&LogRecord> = records
        .iter()
        .filter(|r| {
            if let Some(af) = action_filter {
                let action_name = match r {
                    LogRecord::Removed { .. } => "removed",
                    LogRecord::Skipped { .. } => "skipped",
                    LogRecord::Error { .. } => "error",
                    LogRecord::Summary { .. } => "summary",
                };
                if action_name != af {
                    return false;
                }
            }
            if let Some(rf) = repo_filter {
                let repo_name = match r {
                    LogRecord::Removed { repo, .. } => repo.as_str(),
                    LogRecord::Skipped { repo, .. } => repo.as_str(),
                    LogRecord::Error { repo, .. } => repo.as_str(),
                    LogRecord::Summary { .. } => return true, // summaries always pass repo filter
                };
                if !repo_name.contains(rf) {
                    return false;
                }
            }
            true
        })
        .collect();

    if records.is_empty() {
        println!("No matching records found.");
        return Ok(());
    }

    // Take last N records
    let start = match last {
        HistoryLast::Count(count) => records.len().saturating_sub(count),
        HistoryLast::All => 0,
    };
    let shown = &records[start..];

    println!(
        "Showing {} of {} records (from {})\n",
        shown.len(),
        records.len(),
        path.display()
    );

    for record in shown {
        match record {
            LogRecord::Summary {
                timestamp,
                scanned_repos,
                scanned_worktrees,
                removed_count,
                skipped_count,
                error_count,
                dry_run,
            } => {
                let mode = if *dry_run { " (dry-run)" } else { "" };
                println!(
                    "  \x1b[1;36m{timestamp}\x1b[0m  \x1b[1m📊 SUMMARY{mode}\x1b[0m  repos:{scanned_repos}  worktrees:{scanned_worktrees}  \x1b[32mremoved:{removed_count}\x1b[0m  skipped:{skipped_count}  \x1b[31merrors:{error_count}\x1b[0m"
                );
            }
            LogRecord::Removed {
                timestamp,
                repo,
                branch,
                pr_number,
                pr_url,
                ..
            } => {
                println!(
                    "  \x1b[1;36m{timestamp}\x1b[0m  \x1b[32m🗑  REMOVED\x1b[0m  {repo}  branch:{branch}  PR #{pr_number} {pr_url}"
                );
            }
            LogRecord::Skipped {
                timestamp,
                repo,
                branch,
                reason,
                ..
            } => {
                println!(
                    "  \x1b[1;36m{timestamp}\x1b[0m  \x1b[33m⏭  SKIPPED\x1b[0m  {repo}  branch:{branch}  reason:{reason}"
                );
            }
            LogRecord::Error {
                timestamp,
                repo,
                branch,
                error,
                ..
            } => {
                let branch_str = branch.as_deref().unwrap_or("-");
                println!(
                    "  \x1b[1;36m{timestamp}\x1b[0m  \x1b[31m❌ ERROR\x1b[0m    {repo}  branch:{branch_str}  {error}"
                );
            }
        }
    }

    // Show a quick summary at the bottom
    let summary_count = shown
        .iter()
        .filter(|r| matches!(r, LogRecord::Summary { .. }))
        .count();
    let removed_count = shown
        .iter()
        .filter(|r| matches!(r, LogRecord::Removed { .. }))
        .count();
    let error_count = shown
        .iter()
        .filter(|r| matches!(r, LogRecord::Error { .. }))
        .count();

    println!();
    println!(
        "  Shown: {} runs, {} removals, {} errors",
        summary_count, removed_count, error_count
    );

    Ok(())
}

fn run_gc(cli: &Cli) -> Result<()> {
    setup_logging(cli.verbose)?;
    check_prerequisites()?;

    let logger = JsonLogger::new(Some(&cli.log_file))?;

    let base_dir = PathBuf::from(&cli.dir);
    if !base_dir.is_dir() {
        bail!("Directory does not exist: {}", base_dir.display());
    }

    info!(
        "worktree-gc starting (dir: {}, dry_run: {})",
        base_dir.display(),
        cli.dry_run
    );

    let repos = find_repos(&base_dir)?;
    let mut result = GcResult::default();

    let repo_infos: Vec<RepoInfo> = repos
        .into_iter()
        .filter_map(|path| match parse_github_remote(&path) {
            Ok((owner, repo)) => Some(RepoInfo { path, owner, repo }),
            Err(e) => {
                debug!("Skipping {} (not a GitHub repo: {e})", path.display());
                None
            }
        })
        .collect();

    result.scanned_repos = repo_infos.len();
    info!("Found {} GitHub repositories", repo_infos.len());

    for repo in &repo_infos {
        process_repo(repo, cli.dry_run, &mut result, &logger);
    }

    // Print summary
    info!("--- Summary ---");
    info!("Repos scanned: {}", result.scanned_repos);
    info!("Worktrees checked: {}", result.scanned_worktrees);
    info!(
        "Removed: {} {}",
        result.removed.len(),
        if cli.dry_run { "(dry-run)" } else { "" }
    );
    for r in &result.removed {
        info!("  - {r}");
    }
    if !result.errors.is_empty() {
        warn!("Errors: {}", result.errors.len());
        for e in &result.errors {
            warn!("  - {e}");
        }
    }

    // Write summary to JSONL log
    logger.write(&LogRecord::Summary {
        timestamp: now_iso(),
        scanned_repos: result.scanned_repos,
        scanned_worktrees: result.scanned_worktrees,
        removed_count: result.removed.len(),
        skipped_count: result.skipped.len(),
        error_count: result.errors.len(),
        dry_run: cli.dry_run,
    });

    Ok(())
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_owner_repo_https() {
        let (owner, repo) = parse_owner_repo("https://github.com/CSA-MLT/mspf-core.git").unwrap();
        assert_eq!(owner, "CSA-MLT");
        assert_eq!(repo, "mspf-core");
    }

    #[test]
    fn test_parse_owner_repo_https_no_git() {
        let (owner, repo) = parse_owner_repo("https://github.com/CSA-MLT/mspf-core").unwrap();
        assert_eq!(owner, "CSA-MLT");
        assert_eq!(repo, "mspf-core");
    }

    #[test]
    fn test_parse_owner_repo_ssh() {
        let (owner, repo) = parse_owner_repo("git@github.com:octocat/Hello-World.git").unwrap();
        assert_eq!(owner, "octocat");
        assert_eq!(repo, "Hello-World");
    }

    #[test]
    fn test_parse_owner_repo_invalid() {
        assert!(parse_owner_repo("https://gitlab.com/foo/bar.git").is_err());
    }

    #[test]
    fn test_parse_worktree_porcelain() {
        let input = "\
worktree /home/user/prog/mspf-core
HEAD bd1c7ce3a
branch refs/heads/develop

worktree /home/user/prog/mspf-core-fix-lint
HEAD 580f761a2
branch refs/heads/fix/lint-gocognit

worktree /home/user/prog/mspf-core-detached
HEAD aaa111bbb
detached

";
        let entries =
            parse_worktree_porcelain(input, Path::new("/home/user/prog/mspf-core")).unwrap();

        assert_eq!(entries.len(), 3);

        assert!(entries[0].is_main);
        assert_eq!(entries[0].branch.as_deref(), Some("develop"));

        assert!(!entries[1].is_main);
        assert_eq!(entries[1].branch.as_deref(), Some("fix/lint-gocognit"));
        assert_eq!(
            entries[1].path,
            PathBuf::from("/home/user/prog/mspf-core-fix-lint")
        );

        assert!(!entries[2].is_main);
        assert!(entries[2].branch.is_none()); // detached
    }

    #[test]
    fn test_parse_worktree_no_trailing_newline() {
        let input = "\
worktree /repo
HEAD abc
branch refs/heads/main

worktree /repo-wt
HEAD def
branch refs/heads/feat";

        let entries = parse_worktree_porcelain(input, Path::new("/repo")).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].branch.as_deref(), Some("feat"));
    }

    #[test]
    fn test_execute_history_command_uses_defaults() {
        let cli = Cli {
            command: None,
            dir: ".".to_string(),
            dry_run: false,
            verbose: false,
            log_file: "/tmp/does-not-exist.jsonl".to_string(),
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
