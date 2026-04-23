# worktree-gc

Automatically clean up git worktrees whose pull requests have been merged.

Scans repositories under a directory, checks each worktree's branch via `gh pr list --state merged`, and removes worktrees (+ local branches) that are confirmed merged. Skips the main worktree, unmerged branches, and detached HEADs.

## Prerequisites

- [Git](https://git-scm.com/)
- [GitHub CLI (`gh`)](https://cli.github.com/) — authenticated via `gh auth login`

## Installation

### Prebuilt Binary (Recommended)

```sh
# macOS (Apple Silicon)
curl -L https://github.com/yuto-ts/worktree-gc/releases/latest/download/worktree-gc-aarch64-apple-darwin.tar.gz | tar xz && sudo mv worktree-gc /usr/local/bin/

# macOS (Intel)
curl -L https://github.com/yuto-ts/worktree-gc/releases/latest/download/worktree-gc-x86_64-apple-darwin.tar.gz | tar xz && sudo mv worktree-gc /usr/local/bin/

# Linux (x86_64)
curl -L https://github.com/yuto-ts/worktree-gc/releases/latest/download/worktree-gc-x86_64-unknown-linux-gnu.tar.gz | tar xz && sudo mv worktree-gc /usr/local/bin/

# Linux (aarch64)
curl -L https://github.com/yuto-ts/worktree-gc/releases/latest/download/worktree-gc-aarch64-unknown-linux-gnu.tar.gz | tar xz && sudo mv worktree-gc /usr/local/bin/
```

### From Source

Requires [Rust toolchain](https://rustup.rs/).

```sh
cargo install --git https://github.com/yuto-ts/worktree-gc
```

## Usage

```sh
worktree-gc               # interactive menu
worktree-gc run           # run cleanup
worktree-gc run --dry-run # preview without removing
worktree-gc run -d /path/to/repos

worktree-gc update        # update to latest release
worktree-gc update --check

worktree-gc history       # show last 10 log records
worktree-gc history --last all -a removed

worktree-gc schedule      # interactive schedule wizard (launchd / systemd)
worktree-gc schedule install --hour 9 --minute 0
worktree-gc schedule uninstall

worktree-gc config        # show effective config
worktree-gc config set -d /path/to/repos
```

**Options** (apply to all subcommands):

| Option | Env | Default |
|---|---|---|
| `-d, --dir` | `WORKTREE_GC_DIR` | current directory |
| `-n, --dry-run` | — | false |
| `-v, --verbose` | — | false |
| `--log-file` | `WORKTREE_GC_LOG` | `~/.local/share/worktree-gc/gc.jsonl` |

## Logging

Writes JSONL to `--log-file` (one record per event). Example:

```jsonl
{"action":"removed","timestamp":"...","repo":"owner/repo","branch":"feat/123","pr_number":42,"pr_url":"..."}
{"action":"skipped","timestamp":"...","repo":"owner/repo","branch":"develop","reason":"not_merged"}
{"action":"summary","timestamp":"...","scanned_repos":5,"removed_count":2,"error_count":0,"dry_run":false}
```

```sh
jq 'select(.action=="removed")' gc.jsonl
jq -r 'select(.action=="removed") | .repo' gc.jsonl | sort | uniq -c | sort -rn
```

## License

[MIT](LICENSE)
