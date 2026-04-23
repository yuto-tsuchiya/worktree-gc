# worktree-gc

Automatically clean up git worktrees whose pull requests have been merged.

## Features

- 🔍 **Auto-Detection** — Scans all repositories under a directory and finds worktrees with merged PRs
- 🗑️ **Safe Cleanup** — Removes worktrees and local branches only when the PR is confirmed merged on GitHub
- 📝 **Dry Run** — Preview what would be removed without making any changes
- 📋 **Structured Logging** — JSONL log file for programmatic analysis, human-readable text on stdout
- ⏰ **Scheduled Execution** — Includes launchd (macOS) and systemd (Linux) configurations for daily runs

## How It Works

1. Scans all subdirectories under the target directory for Git repositories
2. Lists worktrees for each repository via `git worktree list --porcelain`
3. Checks each worktree's branch against GitHub using `gh pr list --state merged`
4. If the PR has been merged:
   - Runs `git worktree remove --force`
   - Deletes the local branch with `git branch -D`
   - Runs `git worktree prune`

## What It Does

- Scans repositories under the configured directory and inspects their worktrees
- Considers only non-main worktrees for cleanup
- Removes a worktree only when its branch has a merged GitHub pull request
- Removes the local worktree with `git worktree remove --force`
- Attempts to delete the corresponding local branch with `git branch -D`
- Runs `git worktree prune` after successful removals in that repository
- Supports `--dry-run` so you can preview removals before making changes

## What It Does Not Do

- Does not remove the main worktree
- Does not remove worktrees whose PR is not merged
- Does not remove detached HEAD worktrees
- Does not delete remote branches on GitHub
- Does not merge pull requests or change PR state
- Does not guarantee local branch deletion; branch cleanup is best-effort after the worktree is removed

## Prerequisites

- [Git](https://git-scm.com/)
- [GitHub CLI (`gh`)](https://cli.github.com/) — authenticated via `gh auth login`
- [Rust toolchain](https://rustup.rs/) — for building from source

## Installation

### From Source

```sh
cargo install --git https://github.com/yuto-ts/worktree-gc
```

Or clone and install locally:

```sh
git clone https://github.com/yuto-ts/worktree-gc.git
cd worktree-gc
cargo install --path .
```

## Usage

```sh
# Open the interactive command menu
worktree-gc

# Show current runtime and schedule configuration
worktree-gc config

# Save the current work directory as the default
worktree-gc config set -d /path/to/repos

# Save the current log file as the default
worktree-gc config set --log-file /path/to/gc.jsonl

# Remove a saved runtime default
worktree-gc config unset dir

# Run cleanup directly
worktree-gc run

# Preview what would be removed (recommended for first run)
worktree-gc run --dry-run

# Specify a custom directory
worktree-gc run --dir /path/to/repos

# Verbose output
worktree-gc run --verbose
```

When invoked with no arguments, `worktree-gc` opens an interactive navigation menu so you can choose **Run cleanup**, **Show config**, **Manage schedule**, or **Show history**. If you pass options such as `-d`, `--dry-run`, or `--verbose` without a subcommand, it runs cleanup directly instead of opening the menu.

### Options

| Option | Env Variable | Description |
|---|---|---|
| `-d, --dir <DIR>` | `WORKTREE_GC_DIR` | Directory to scan for git repositories (default: current working directory) |
| `-n, --dry-run` | — | Show what would be removed without actually removing |
| `-v, --verbose` | — | Enable verbose output |
| `--log-file <PATH>` | `WORKTREE_GC_LOG` | JSONL log file path (default: `~/.local/share/worktree-gc/gc.jsonl`) |

### Execution History

View past runs from the JSONL log:

```sh
# Show recent history (last 10 records)
worktree-gc history

# Show more records
worktree-gc history --last 50

# Show all records
worktree-gc history --last all

# Filter by action type
worktree-gc history -a removed    # only removals
worktree-gc history -a summary    # only run summaries
worktree-gc history -a error      # only errors

# Filter by repository name (substring match)
worktree-gc history -r mspf-auth
```

Example output:

```
  2026-04-17T09:00:01+09:00  🗑  REMOVED  CSA-MLT/mspf-auth  branch:feat/795  PR #42 https://github.com/...
  2026-04-17T09:00:02+09:00  ⏭  SKIPPED  CSA-MLT/mspf-core  branch:develop  reason:not_merged
  2026-04-17T09:00:03+09:00  📊 SUMMARY  repos:38  worktrees:36  removed:15  skipped:21  errors:0
```

## Scheduling

Built-in commands to set up daily automatic execution. Supports **launchd** (macOS) and **systemd** (Linux).

### Interactive Wizard (Recommended)

Run `schedule` with no arguments to launch the interactive setup wizard:

```sh
worktree-gc schedule
```

The wizard guides you through the configuration with prompts:

```
  ⏰ worktree-gc scheduler
     Platform: macos (launchd)

  No schedule is currently configured.
  This wizard will set up daily automatic cleanup of merged worktrees.

? Set up daily automatic execution? Yes
? Directory to scan: .
? What time should it run? (HH:MM) 09:00

  Summary:
  Scan directory: .
  Run daily at:   09:00
  Scheduler:      launchd

? Install? Yes
✓ Schedule installed (launchd)
```

If a schedule is already installed, the wizard shows the current status and lets you update the time/directory or remove it.

### Non-Interactive Commands

```sh
# Install daily schedule (defaults to 09:00)
worktree-gc schedule install

# Install with custom time
worktree-gc schedule install --hour 12 --minute 30

# Specify a custom scan directory
worktree-gc schedule install --dir /path/to/repos

# Show current runtime and schedule configuration
worktree-gc config

# Remove the schedule
worktree-gc schedule uninstall
```

`worktree-gc config` shows the effective runtime settings, including the current work directory from `-d/--dir`, the saved runtime defaults, and a schedule section focused on scheduler-specific details. Runtime setting precedence is: command-line option > environment variable > saved config > built-in default.

The `install` command automatically:
- Detects the current binary path and scan directory
- Generates the appropriate config (launchd plist or systemd unit files)
- Installs and activates the schedule
- Creates the log directory at `~/.local/share/worktree-gc/`

## Logging

When `--log-file` is specified, worktree-gc writes **JSONL** (one JSON object per line) for structured analysis. stdout remains human-readable text.

```sh
worktree-gc --log-file ~/.local/share/worktree-gc/gc.jsonl
```

Each line is one of four record types:

```jsonl
{"action":"removed","timestamp":"2026-04-16T09:00:01+09:00","repo":"CSA-MLT/mspf-auth","branch":"feat/795","worktree":"/home/user/prog/mspf-auth-feat-795","pr_number":42,"pr_url":"https://github.com/CSA-MLT/mspf-auth/pull/42"}
{"action":"skipped","timestamp":"...","repo":"CSA-MLT/mspf-core","branch":"develop","worktree":"...","reason":"not_merged"}
{"action":"error","timestamp":"...","repo":"CSA-MLT/mspf-iam","branch":"fix/dep","worktree":"...","error":"worktree remove failed"}
{"action":"summary","timestamp":"...","scanned_repos":38,"scanned_worktrees":36,"removed_count":15,"skipped_count":21,"error_count":0,"dry_run":false}
```

### Querying with jq

```sh
# List all removed worktrees
jq 'select(.action=="removed")' gc.jsonl

# Count removals per repo
jq -r 'select(.action=="removed") | .repo' gc.jsonl | sort | uniq -c | sort -rn

# Show today's summary
jq 'select(.action=="summary")' gc.jsonl | tail -1

# List errors
jq 'select(.action=="error")' gc.jsonl
```

## License

[MIT](LICENSE)
