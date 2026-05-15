use crate::logging::{now_iso, setup_logging, JsonLogger, LogRecord};
use anyhow::{bail, Context, Result};
use log::{debug, error, info, warn};
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) struct RunOptions<'a> {
    pub(crate) dir: &'a str,
    pub(crate) dry_run: bool,
    pub(crate) verbose: bool,
    pub(crate) log_file: &'a str,
}

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

#[derive(Debug, Default)]
struct GcResult {
    scanned_repos: usize,
    scanned_worktrees: usize,
    removed: Vec<String>,
    skipped: Vec<String>,
    errors: Vec<String>,
}

pub(crate) fn run(options: RunOptions<'_>) -> Result<()> {
    setup_logging(options.verbose)?;
    check_prerequisites()?;

    let logger = JsonLogger::new(Some(options.log_file))?;

    let base_dir = PathBuf::from(options.dir);
    if !base_dir.is_dir() {
        bail!("Directory does not exist: {}", base_dir.display());
    }

    info!(
        "worktree-gc starting (dir: {}, dry_run: {})",
        base_dir.display(),
        options.dry_run
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
        process_repo(repo, options.dry_run, &mut result, &logger);
    }

    info!("--- Summary ---");
    info!("Repos scanned: {}", result.scanned_repos);
    info!("Worktrees checked: {}", result.scanned_worktrees);
    info!(
        "Removed: {} {}",
        result.removed.len(),
        if options.dry_run { "(dry-run)" } else { "" }
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

    logger.write(&LogRecord::Summary {
        timestamp: now_iso(),
        scanned_repos: result.scanned_repos,
        scanned_worktrees: result.scanned_worktrees,
        removed_count: result.removed.len(),
        skipped_count: result.skipped.len(),
        error_count: result.errors.len(),
        dry_run: options.dry_run,
    });

    Ok(())
}

fn find_repos(base_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut repos = Vec::new();
    let entries = fs::read_dir(base_dir)
        .with_context(|| format!("Cannot read directory: {}", base_dir.display()))?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            let git_dir = path.join(".git");
            if git_dir.is_dir() {
                repos.push(path);
            }
        }
    }
    repos.sort();
    Ok(repos)
}

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
        assert!(entries[2].branch.is_none());
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
}
