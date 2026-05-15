use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub(crate) struct RuntimeConfigFile {
    pub(crate) dir: Option<String>,
    pub(crate) log_file: Option<String>,
    #[serde(default)]
    pub(crate) workspaces: Vec<WorkspaceConfig>,
    #[serde(default)]
    pub(crate) schedules: Vec<ScheduleConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceConfig {
    pub(crate) name: String,
    pub(crate) dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) log_file: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub(crate) struct ScheduleConfig {
    pub(crate) name: String,
    pub(crate) workspace: String,
    pub(crate) hour: u8,
    pub(crate) minute: u8,
}

impl RuntimeConfigFile {
    pub(crate) fn find_workspace(&self, name: &str) -> Option<&WorkspaceConfig> {
        self.workspaces
            .iter()
            .find(|workspace| workspace.name == name)
    }

    pub(crate) fn upsert_workspace(&mut self, workspace: WorkspaceConfig) {
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

    pub(crate) fn remove_workspace(&mut self, name: &str) -> bool {
        let before = self.workspaces.len();
        self.workspaces.retain(|workspace| workspace.name != name);
        before != self.workspaces.len()
    }

    pub(crate) fn upsert_schedule(&mut self, schedule: ScheduleConfig) {
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

    pub(crate) fn remove_schedule(&mut self, name: &str) -> bool {
        let before = self.schedules.len();
        self.schedules.retain(|schedule| schedule.name != name);
        before != self.schedules.len()
    }
}

pub(crate) fn runtime_config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Cannot determine config directory")?;
    Ok(base.join("worktree-gc").join("config.json"))
}

pub(crate) fn load_runtime_config() -> Result<RuntimeConfigFile> {
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

pub(crate) fn save_runtime_config(config: &RuntimeConfigFile) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_upsert_workspace_sorts_and_replaces_by_name() {
        let mut config = RuntimeConfigFile::default();

        config.upsert_workspace(WorkspaceConfig {
            name: "zeta".to_string(),
            dir: "/repos/zeta".to_string(),
            log_file: None,
        });
        config.upsert_workspace(WorkspaceConfig {
            name: "alpha".to_string(),
            dir: "/repos/alpha".to_string(),
            log_file: Some("/logs/alpha.jsonl".to_string()),
        });
        config.upsert_workspace(WorkspaceConfig {
            name: "zeta".to_string(),
            dir: "/repos/zeta-new".to_string(),
            log_file: Some("/logs/zeta.jsonl".to_string()),
        });

        assert_eq!(
            config
                .workspaces
                .iter()
                .map(|workspace| workspace.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "zeta"]
        );
        assert_eq!(
            config
                .find_workspace("zeta")
                .map(|workspace| (workspace.dir.as_str(), workspace.log_file.as_deref())),
            Some(("/repos/zeta-new", Some("/logs/zeta.jsonl")))
        );
    }

    #[test]
    fn test_remove_workspace_reports_whether_entry_existed() {
        let mut config = RuntimeConfigFile {
            workspaces: vec![WorkspaceConfig {
                name: "team".to_string(),
                dir: "/repos/team".to_string(),
                log_file: None,
            }],
            ..RuntimeConfigFile::default()
        };

        assert!(config.remove_workspace("team"));
        assert!(!config.remove_workspace("team"));
        assert!(config.workspaces.is_empty());
    }

    #[test]
    fn test_upsert_schedule_sorts_and_replaces_by_name() {
        let mut config = RuntimeConfigFile::default();

        config.upsert_schedule(ScheduleConfig {
            name: "nightly".to_string(),
            workspace: "team".to_string(),
            hour: 23,
            minute: 30,
        });
        config.upsert_schedule(ScheduleConfig {
            name: "daily".to_string(),
            workspace: "personal".to_string(),
            hour: 9,
            minute: 0,
        });
        config.upsert_schedule(ScheduleConfig {
            name: "nightly".to_string(),
            workspace: "team".to_string(),
            hour: 1,
            minute: 15,
        });

        assert_eq!(
            config
                .schedules
                .iter()
                .map(|schedule| schedule.name.as_str())
                .collect::<Vec<_>>(),
            vec!["daily", "nightly"]
        );
        assert_eq!(
            config
                .schedules
                .iter()
                .find(|schedule| schedule.name == "nightly")
                .map(|schedule| (schedule.hour, schedule.minute)),
            Some((1, 15))
        );
    }

    #[test]
    fn test_remove_schedule_reports_whether_entry_existed() {
        let mut config = RuntimeConfigFile {
            schedules: vec![ScheduleConfig {
                name: "default".to_string(),
                workspace: "team".to_string(),
                hour: 9,
                minute: 0,
            }],
            ..RuntimeConfigFile::default()
        };

        assert!(config.remove_schedule("default"));
        assert!(!config.remove_schedule("default"));
        assert!(config.schedules.is_empty());
    }
}
