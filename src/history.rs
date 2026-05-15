use crate::logging::LogRecord;
use crate::HistoryLast;
use anyhow::{Context, Result};
use std::fs;
use std::io::BufRead;
use std::path::PathBuf;

pub(crate) fn show_history(
    log_file: &str,
    last: HistoryLast,
    action_filter: Option<&str>,
    repo_filter: Option<&str>,
) -> Result<()> {
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
            Err(_) => continue,
        }
    }

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
                    LogRecord::Summary { .. } => return true,
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
