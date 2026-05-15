use anyhow::{Context, Result};
use log::LevelFilter;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub(crate) enum LogRecord {
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

pub(crate) fn now_iso() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

pub(crate) struct JsonLogger {
    file: Option<Mutex<fs::File>>,
}

impl JsonLogger {
    pub(crate) fn new(path: Option<&str>) -> Result<Self> {
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

    pub(crate) fn write(&self, record: &LogRecord) {
        if let Some(ref file) = self.file {
            if let Ok(json) = serde_json::to_string(record) {
                if let Ok(mut f) = file.lock() {
                    let _ = writeln!(f, "{json}");
                }
            }
        }
    }
}

pub(crate) fn setup_logging(verbose: bool) -> Result<()> {
    let level = if verbose {
        LevelFilter::Debug
    } else {
        LevelFilter::Info
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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "worktree-gc-{name}-{}-{}.jsonl",
            std::process::id(),
            chrono::Local::now()
                .timestamp_nanos_opt()
                .unwrap_or_default()
        ))
    }

    #[test]
    fn test_error_record_omits_absent_optional_fields() {
        let record = LogRecord::Error {
            timestamp: "2026-05-15T00:00:00+09:00".to_string(),
            repo: "owner/repo".to_string(),
            branch: None,
            worktree: None,
            error: "boom".to_string(),
        };

        let value: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&record).unwrap()).unwrap();

        assert_eq!(value["action"], "error");
        assert_eq!(value["repo"], "owner/repo");
        assert!(value.get("branch").is_none());
        assert!(value.get("worktree").is_none());
    }

    #[test]
    fn test_json_logger_appends_json_lines() {
        let path = temp_log_path("append");
        let logger = JsonLogger::new(Some(&path.to_string_lossy())).unwrap();

        logger.write(&LogRecord::Summary {
            timestamp: "2026-05-15T00:00:00+09:00".to_string(),
            scanned_repos: 2,
            scanned_worktrees: 3,
            removed_count: 1,
            skipped_count: 1,
            error_count: 0,
            dry_run: true,
        });
        logger.write(&LogRecord::Skipped {
            timestamp: "2026-05-15T00:01:00+09:00".to_string(),
            repo: "owner/repo".to_string(),
            branch: "feature".to_string(),
            worktree: "/repos/repo-feature".to_string(),
            reason: "not_merged".to_string(),
        });

        drop(logger);
        let content = fs::read_to_string(&path).unwrap();
        let lines = content.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[0]).unwrap()["action"],
            "summary"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(lines[1]).unwrap()["action"],
            "skipped"
        );

        let _ = fs::remove_file(path);
    }
}
