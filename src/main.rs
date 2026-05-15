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

pub(crate) fn validate_registration_name(kind: &str, name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("{kind} name cannot be empty");
    };

    if name.len() > 31 {
        bail!("{kind} name must be 31 characters or fewer");
    }
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        bail!("{kind} name must start with a lowercase letter or digit");
    }
    if chars.any(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')) {
        bail!("{kind} name may only contain lowercase letters, digits, '-' and '_'");
    }

    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
struct RuntimeConfigFile {
    dir: Option<String>,
    log_file: Option<String>,
    #[serde(default)]
    workspaces: Vec<WorkspaceConfig>,
    #[serde(default)]
    schedules: Vec<ScheduleConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct WorkspaceConfig {
    name: String,
    dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    log_file: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
struct ScheduleConfig {
    name: String,
    workspace: String,
    hour: u8,
    minute: u8,
}

impl RuntimeConfigFile {
    fn find_workspace(&self, name: &str) -> Option<&WorkspaceConfig> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.name == name)
    }

    fn upsert_workspace(&mut self, workspace: WorkspaceConfig) {
        if let Some(existing) = self
            .workspaces
            .iter_mut()
            .find(|existing| existing.name == workspace.name)
        {
            *existing = workspace;
        } else {
            self.workspaces.push(workspace);
            self.workspaces.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    fn remove_workspace(&mut self, name: &str) -> bool {
        let before = self.workspaces.len();
        self.workspaces.retain(|workspace| workspace.name != name);
        before != self.workspaces.len()
    }

    fn upsert_schedule(&mut self, schedule: ScheduleConfig) {
        if let Some(existing) = self
            .schedules
            .iter_mut()
            .find(|existing| existing.name == schedule.name)
        {
            *existing = schedule;
        } else {
            self.schedules.push(schedule);
            self.schedules.sort_by(|a, b| a.name.cmp(&b.name));
        }
    }

    fn remove_schedule(&mut self, name: &str) -> bool {
        let before = self.schedules.len();
        self.schedules.retain(|schedule| schedule.name != name);
        before != self.schedules.len()
    }
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
    if config.dir.is_none()
        && config.log_file.is_none()
        && config.workspaces.is_empty()
        && config.schedules.is_empty()
    {
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
            println!("Cancelled.");
            Ok(None)
        }
    }
}

fn execute_command(cli: &Cli, command: Commands, raw_args: &[OsString]) -> Result<()> {
    match command {
        Commands::Schedule { action } => match action {
            Some(ScheduleAction::Install { name, hour, minute }) => {
                install_schedule(cli, &name, hour, minute, raw_args)
            }
            Some(ScheduleAction::Uninstall { name, all }) => uninstall_schedule(&name, all),
            Some(ScheduleAction::List) => list_schedules(),
            None => interactive_schedule_wizard(cli),
        },
        Commands::Workspace { action } => match action {
            Some(action) => execute_workspace_action(cli, action, raw_args),
            None => interactive_workspace_wizard(cli),
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
        Commands::Run => run_gc_from_command(cli, raw_args),
    }
}

fn run_gc_from_command(cli: &Cli, raw_args: &[OsString]) -> Result<()> {
    if should_open_interactive_menu(raw_args) && cli.workspace.is_none() {
        let mut cli = cli.clone();
        prompt_run_workspace_if_needed(&mut cli)?;
        return run_gc(&cli);
    }

    run_gc(cli)
}

fn prompt_run_workspace_if_needed(cli: &mut Cli) -> Result<()> {
    use dialoguer::Select;

    let config = load_runtime_config()?;
    if config.workspaces.len() <= 1 {
        return Ok(());
    }

    let workspace_labels: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| format!("{} ({})", workspace.name, workspace.dir))
        .collect();
    let selection = Select::new()
        .with_prompt("Select workspace to run cleanup")
        .items(&workspace_labels)
        .default(0)
        .interact()?;
    let workspace = &config.workspaces[selection];

    cli.workspace = Some(workspace.name.clone());
    cli.dir = workspace.dir.clone();
    if let Some(log_file) = &workspace.log_file {
        cli.log_file = log_file.clone();
    } else if let Some(log_file) = &config.log_file {
        cli.log_file = log_file.clone();
    }

    println!();
    println!("  {}", style("Run cleanup").bold());
    println!("  Workspace: {}", style(&workspace.name).cyan());
    println!("  Directory: {}", style(&cli.dir).cyan());
    println!("  Log file:  {}", cli.log_file);
    println!();

    Ok(())
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
    print_registered_workspaces(&saved);
    println!();
    print_registered_schedules(&saved);
    println!();

    scheduler::print_config(&configured_schedule_names(&saved))
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

fn execute_workspace_action(
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

    println!("Saved workspace:");
    println!("  Name:   {name}");
    println!("  Dir:    {dir}");
    if let Some(workspace) = config.find_workspace(name) {
        println!("  Log:    {}", workspace_log_display(workspace, &config));
    }
    println!("  Config: {}", runtime_config_path()?.display());
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
    println!("Removed workspace: {name}");
    Ok(())
}

fn list_workspaces() -> Result<()> {
    let config = load_runtime_config()?;
    print_registered_workspaces(&config);
    Ok(())
}

fn interactive_workspace_wizard(cli: &Cli) -> Result<()> {
    use dialoguer::Select;

    let config = load_runtime_config()?;
    println!();
    println!("  {}", style("worktree-gc workspaces").bold());
    println!("  {}", style("Manage named scan directories").dim());
    println!();
    print_registered_workspaces(&config);
    println!();

    let choices = vec![
        "Add workspace",
        "Update workspace",
        "Remove workspace",
        "List workspaces",
        "Cancel",
    ];
    let default_choice = if config.workspaces.is_empty() { 0 } else { 1 };
    let selection = Select::new()
        .with_prompt("What would you like to do?")
        .items(&choices)
        .default(default_choice)
        .interact()?;

    match selection {
        0 => add_workspace_interactive(cli, &config),
        1 => update_workspace_interactive(cli, &config),
        2 => remove_workspace_interactive(&config),
        3 => {
            print_registered_workspaces(&config);
            Ok(())
        }
        _ => {
            println!("Cancelled.");
            Ok(())
        }
    }
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
    use dialoguer::{Confirm, Select};

    if config.workspaces.is_empty() {
        println!("No registered workspaces to update.");
        return Ok(());
    }

    let workspace_labels: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| format!("{} ({})", workspace.name, workspace.dir))
        .collect();
    let selection = Select::new()
        .with_prompt("Select a workspace to update")
        .items(&workspace_labels)
        .default(0)
        .interact()?;
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
        println!("Cancelled.");
        return Ok(());
    }

    add_workspace(&workspace.name, &dir, log_file)
}

fn remove_workspace_interactive(config: &RuntimeConfigFile) -> Result<()> {
    use dialoguer::{Confirm, Select};

    if config.workspaces.is_empty() {
        println!("No registered workspaces to remove.");
        return Ok(());
    }

    let workspace_labels: Vec<String> = config
        .workspaces
        .iter()
        .map(|workspace| format!("{} ({})", workspace.name, workspace.dir))
        .collect();
    let selection = Select::new()
        .with_prompt("Select a workspace to remove")
        .items(&workspace_labels)
        .default(0)
        .interact()?;
    let workspace_name = config.workspaces[selection].name.clone();

    if !Confirm::new()
        .with_prompt(format!("Remove workspace '{workspace_name}'?"))
        .default(false)
        .interact()?
    {
        println!("Cancelled.");
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

fn print_registered_workspaces(config: &RuntimeConfigFile) {
    println!("Registered workspaces:");
    if config.workspaces.is_empty() {
        println!("  (none)");
        return;
    }

    for workspace in &config.workspaces {
        println!("  {}", workspace.name);
        println!("    Dir: {}", workspace.dir);
        println!("    Log: {}", workspace_log_display(workspace, config));
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
    println!("  {}", style("Summary:").bold());
    println!("  Workspace: {}", style(name).cyan());
    println!("  Directory: {}", style(dir).cyan());
    println!(
        "  Log file:  {}",
        log_file
            .map(|log_file| log_file.to_string())
            .unwrap_or_else(|| format!("(global default: {default_effective_log_file})"))
    );
    println!();
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

fn install_schedule(
    cli: &Cli,
    schedule_name: &str,
    hour: u8,
    minute: u8,
    raw_args: &[OsString],
) -> Result<()> {
    ensure_valid_daily_time(hour, minute)?;
    scheduler::validate_schedule_name(schedule_name)?;

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

    scheduler::install_workspace(schedule_name, &workspace_name, hour, minute)?;
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

fn uninstall_schedule(name: &str, all: bool) -> Result<()> {
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
            scheduler::uninstall(schedule_name)?;
        }
        config.schedules.clear();
        save_runtime_config(&config)?;
        println!("Removed all registered schedules.");
        return Ok(());
    }

    scheduler::validate_schedule_name(name)?;
    scheduler::uninstall(name)?;
    config.remove_schedule(name);
    save_runtime_config(&config)?;
    println!("Removed schedule metadata: {name}");
    Ok(())
}

fn list_schedules() -> Result<()> {
    let config = load_runtime_config()?;
    print_registered_schedules(&config);
    println!();
    scheduler::print_config(&configured_schedule_names(&config))
}

fn interactive_schedule_wizard(cli: &Cli) -> Result<()> {
    use dialoguer::Select;

    let mut config = load_runtime_config()?;
    println!();
    println!("  {}", style("worktree-gc scheduler").bold());
    println!("  {}", style("Manage automatic cleanup schedules").dim());
    println!();
    print_registered_schedules(&config);
    println!();
    print_registered_workspaces(&config);
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
        3 => scheduler::print_config(&configured_schedule_names(&config)),
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
            scheduler::validate_schedule_name(input).map_err(|err| err.to_string())?;
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

    let workspace = prompt_workspace_choice(cli, config, None)?;
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

    install_schedule_from_parts(config, &name, &workspace, hour, minute)
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

    let workspace = prompt_workspace_choice(cli, config, Some(&current.workspace))?;
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

    install_schedule_from_parts(config, &current.name, &workspace, hour, minute)
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

    scheduler::uninstall(&schedule_name)?;
    config.remove_schedule(&schedule_name);
    save_runtime_config(config)?;
    println!("Removed schedule metadata: {schedule_name}");
    Ok(())
}

fn install_schedule_from_parts(
    config: &mut RuntimeConfigFile,
    name: &str,
    workspace: &str,
    hour: u8,
    minute: u8,
) -> Result<()> {
    scheduler::install_workspace(name, workspace, hour, minute)?;
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

fn prompt_workspace_choice(
    cli: &Cli,
    config: &mut RuntimeConfigFile,
    current_workspace: Option<&str>,
) -> Result<String> {
    use dialoguer::Select;

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
        return Ok("default".to_string());
    }

    let mut choices = workspace_names.clone();
    choices.push("Use current --dir as default workspace".to_string());
    let selection = Select::new()
        .with_prompt("Workspace")
        .items(&choices)
        .default(current_index)
        .interact()?;

    if selection < workspace_names.len() {
        return Ok(workspace_names[selection].clone());
    }

    config.upsert_workspace(WorkspaceConfig {
        name: "default".to_string(),
        dir: cli.dir.clone(),
        log_file: Some(cli.log_file.clone()),
    });
    Ok("default".to_string())
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

fn print_registered_schedules(config: &RuntimeConfigFile) {
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

fn configured_schedule_names(config: &RuntimeConfigFile) -> Vec<String> {
    config
        .schedules
        .iter()
        .map(|schedule| schedule.name.clone())
        .collect()
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
    fn test_validate_registration_name() {
        assert!(validate_registration_name("workspace", "team-a_1").is_ok());
        assert!(validate_registration_name("workspace", "Team").is_err());
        assert!(validate_registration_name("workspace", "../team").is_err());
        assert!(validate_registration_name("workspace", "").is_err());
    }

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
