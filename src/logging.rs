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
