use anyhow::{bail, Result};

pub(crate) fn default_dir() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

pub(crate) fn default_log_file() -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_dir_is_current_dir() {
        let expected = std::env::current_dir().unwrap();
        assert_eq!(default_dir(), expected.to_string_lossy().to_string());
    }

    #[test]
    fn test_validate_registration_name() {
        assert!(validate_registration_name("workspace", "team-a_1").is_ok());
        assert!(validate_registration_name("workspace", "Team").is_err());
        assert!(validate_registration_name("workspace", "../team").is_err());
        assert!(validate_registration_name("workspace", "").is_err());
    }
}
