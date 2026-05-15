use crate::{ui, validate_registration_name};
use anyhow::{bail, Context, Result};
use log::warn;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const DEFAULT_SCHEDULE_NAME: &str = "default";
const LEGACY_SCHEDULE_NAME: &str = "daily";

pub fn validate_schedule_name(name: &str) -> Result<()> {
    validate_registration_name("schedule", name)?;
    if name == LEGACY_SCHEDULE_NAME {
        bail!("schedule name '{LEGACY_SCHEDULE_NAME}' is reserved for legacy migration");
    }
    Ok(())
}

fn workspace_run_args(workspace_name: &str) -> Vec<String> {
    vec![
        "run".to_string(),
        "--workspace".to_string(),
        workspace_name.to_string(),
    ]
}

// ============================================================
// macOS (launchd)
// ============================================================

#[cfg(target_os = "macos")]
const LEGACY_LABEL: &str = "com.worktree-gc.daily";

#[cfg(target_os = "macos")]
fn schedule_label(schedule_name: &str) -> Result<String> {
    validate_schedule_name(schedule_name)?;
    Ok(format!("com.worktree-gc.{schedule_name}"))
}

#[cfg(target_os = "macos")]
fn launch_agents_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join("Library/LaunchAgents"))
}

#[cfg(target_os = "macos")]
fn plist_path(schedule_name: &str) -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{}.plist", schedule_label(schedule_name)?)))
}

#[cfg(target_os = "macos")]
fn legacy_plist_path() -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("{LEGACY_LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn log_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".local/share/worktree-gc"))
}

#[cfg(target_os = "macos")]
pub fn install_workspace(
    schedule_name: &str,
    workspace_name: &str,
    hour: u8,
    minute: u8,
) -> Result<()> {
    validate_registration_name("workspace", workspace_name)?;
    install_with_args(
        schedule_name,
        workspace_run_args(workspace_name),
        hour,
        minute,
    )
}

#[cfg(target_os = "macos")]
fn install_with_args(
    schedule_name: &str,
    run_args: Vec<String>,
    hour: u8,
    minute: u8,
) -> Result<()> {
    let exe = env::current_exe().context("Cannot determine binary path")?;
    let exe_str = exe.to_string_lossy();
    let label = schedule_label(schedule_name)?;
    let plist = plist_path(schedule_name)?;
    let log_dir = log_dir()?;
    let stdout_log = log_dir.join(format!("{schedule_name}-launchd-stdout.log"));
    let stderr_log = log_dir.join(format!("{schedule_name}-launchd-stderr.log"));
    let current_path = env::var("PATH").unwrap_or_default();

    fs::create_dir_all(plist.parent().unwrap())?;
    fs::create_dir_all(&log_dir)?;

    if schedule_name == DEFAULT_SCHEDULE_NAME {
        remove_legacy_launchd_schedule()?;
    }

    if plist.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", &plist.to_string_lossy()])
            .output();
    }

    let content = render_launchd_plist(
        &label,
        &exe_str,
        &run_args,
        &current_path,
        &stdout_log.to_string_lossy(),
        &stderr_log.to_string_lossy(),
        hour,
        minute,
    );

    fs::write(&plist, &content).with_context(|| format!("Failed to write {}", plist.display()))?;

    let output = Command::new("launchctl")
        .args(["load", &plist.to_string_lossy()])
        .output()
        .context("Failed to run launchctl load")?;

    if !output.status.success() {
        bail!(
            "launchctl load failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("{}", ui::success("✓ Schedule installed (launchd)"));
    println!("  {} {}", ui::label("Name"), ui::name(schedule_name));
    println!(
        "  {} {}",
        ui::label("Plist"),
        ui::path(&plist.display().to_string())
    );
    println!("  {} {}", ui::label("Binary"), ui::path(&exe_str));
    println!(
        "  {} {}",
        ui::label("Time"),
        ui::value(&format!("{hour:02}:{minute:02} daily"))
    );
    Ok(())
}

#[cfg(target_os = "macos")]
fn render_launchd_plist(
    label: &str,
    exe: &str,
    run_args: &[String],
    path: &str,
    stdout_log: &str,
    stderr_log: &str,
    hour: u8,
    minute: u8,
) -> String {
    let mut program_arguments = format!("        <string>{exe}</string>\n");
    for arg in run_args {
        program_arguments.push_str(&format!("        <string>{arg}</string>\n"));
    }

    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>

    <key>ProgramArguments</key>
    <array>
{program_arguments}    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{path}</string>
    </dict>

    <key>StartCalendarInterval</key>
    <dict>
        <key>Hour</key>
        <integer>{hour}</integer>
        <key>Minute</key>
        <integer>{minute}</integer>
    </dict>

    <key>StandardOutPath</key>
    <string>{stdout_log}</string>
    <key>StandardErrorPath</key>
    <string>{stderr_log}</string>
</dict>
</plist>
"#
    )
}

#[cfg(target_os = "macos")]
fn remove_legacy_launchd_schedule() -> Result<bool> {
    let legacy = legacy_plist_path()?;
    if !legacy.exists() {
        return Ok(false);
    }

    let output = Command::new("launchctl")
        .args(["unload", &legacy.to_string_lossy()])
        .output();
    if let Ok(output) = output {
        if !output.status.success() {
            warn!(
                "launchctl unload warning: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
    }

    fs::remove_file(&legacy).with_context(|| format!("Failed to remove {}", legacy.display()))?;
    Ok(true)
}

#[cfg(target_os = "macos")]
pub fn uninstall(schedule_name: &str) -> Result<()> {
    let plist = plist_path(schedule_name)?;
    let mut removed = false;

    if plist.exists() {
        let output = Command::new("launchctl")
            .args(["unload", &plist.to_string_lossy()])
            .output()
            .context("Failed to run launchctl unload")?;

        if !output.status.success() {
            warn!(
                "launchctl unload warning: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        fs::remove_file(&plist).with_context(|| format!("Failed to remove {}", plist.display()))?;
        println!("{}", ui::success("✓ Schedule removed"));
        println!("  {} {}", ui::label("Name"), ui::name(schedule_name));
        println!(
            "  {} {}",
            ui::label("Deleted"),
            ui::path(&plist.display().to_string())
        );
        removed = true;
    }

    if schedule_name == DEFAULT_SCHEDULE_NAME && remove_legacy_launchd_schedule()? {
        println!("{}", ui::success("✓ Legacy schedule removed"));
        removed = true;
    }

    if !removed {
        println!(
            "{} {}",
            ui::warning("No schedule installed"),
            ui::muted(&format!("(plist not found: {})", plist.display()))
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn print_config(schedule_names: &[String]) -> Result<()> {
    println!("{}", ui::title("Schedule configuration"));
    println!("  {} {}", ui::label("Scheduler"), ui::value("launchd"));

    let names = printable_schedule_names(schedule_names);
    let output = Command::new("launchctl")
        .args(["list"])
        .output()
        .context("Failed to run launchctl list")?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    for name in names {
        let label = schedule_label(&name)?;
        let plist = plist_path(&name)?;
        if !plist.exists() {
            println!("  {} {}", ui::name(&name), ui::warning("not installed"));
            println!(
                "    {} {}",
                ui::label("Plist"),
                ui::path(&plist.display().to_string())
            );
            continue;
        }

        let loaded = stdout.lines().any(|line| line.contains(&label));
        println!(
            "  {} {}",
            ui::name(&name),
            if loaded {
                ui::success("active")
            } else {
                ui::warning("installed but not loaded")
            }
        );
        println!(
            "    {} {}",
            ui::label("Plist"),
            ui::path(&plist.display().to_string())
        );

        let content = fs::read_to_string(&plist)?;
        if let (Some(h), Some(m)) = (
            extract_plist_integer(&content, "Hour"),
            extract_plist_integer(&content, "Minute"),
        ) {
            println!(
                "    {} {}",
                ui::label("Time"),
                ui::value(&format!("{h:02}:{m:02} daily"))
            );
        }
        if !loaded {
            println!(
                "    {} {}",
                ui::label("Activate"),
                ui::value(&format!("launchctl load {}", plist.display()))
            );
        }
    }

    let legacy = legacy_plist_path()?;
    if legacy.exists() {
        println!(
            "  {} {}",
            ui::name("legacy daily"),
            ui::warning("installed")
        );
        println!(
            "    {} {}",
            ui::label("Plist"),
            ui::path(&legacy.display().to_string())
        );
    }

    Ok(())
}

#[cfg(target_os = "macos")]
fn extract_plist_integer(content: &str, key: &str) -> Option<u32> {
    let key_tag = format!("<key>{key}</key>");
    let mut lines = content.lines();
    while let Some(line) = lines.next() {
        if line.trim() == key_tag {
            if let Some(next) = lines.next() {
                let trimmed = next.trim();
                if let Some(val) = trimmed
                    .strip_prefix("<integer>")
                    .and_then(|s| s.strip_suffix("</integer>"))
                {
                    return val.parse().ok();
                }
            }
        }
    }
    None
}

// ============================================================
// Linux (systemd user units)
// ============================================================

#[cfg(target_os = "linux")]
fn systemd_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".config/systemd/user"))
}

#[cfg(target_os = "linux")]
fn log_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".local/share/worktree-gc"))
}

#[cfg(target_os = "linux")]
fn unit_stem(schedule_name: &str) -> Result<String> {
    validate_schedule_name(schedule_name)?;
    Ok(format!("worktree-gc-{schedule_name}"))
}

#[cfg(target_os = "linux")]
fn service_path(schedule_name: &str) -> Result<PathBuf> {
    Ok(systemd_dir()?.join(format!("{}.service", unit_stem(schedule_name)?)))
}

#[cfg(target_os = "linux")]
fn timer_path(schedule_name: &str) -> Result<PathBuf> {
    Ok(systemd_dir()?.join(format!("{}.timer", unit_stem(schedule_name)?)))
}

#[cfg(target_os = "linux")]
fn legacy_service_path() -> Result<PathBuf> {
    Ok(systemd_dir()?.join("worktree-gc.service"))
}

#[cfg(target_os = "linux")]
fn legacy_timer_path() -> Result<PathBuf> {
    Ok(systemd_dir()?.join("worktree-gc.timer"))
}

#[cfg(target_os = "linux")]
pub fn install_workspace(
    schedule_name: &str,
    workspace_name: &str,
    hour: u8,
    minute: u8,
) -> Result<()> {
    validate_registration_name("workspace", workspace_name)?;
    install_with_args(
        schedule_name,
        workspace_run_args(workspace_name),
        hour,
        minute,
    )
}

#[cfg(target_os = "linux")]
fn install_with_args(
    schedule_name: &str,
    run_args: Vec<String>,
    hour: u8,
    minute: u8,
) -> Result<()> {
    let exe = env::current_exe().context("Cannot determine binary path")?;
    let exe_str = exe.to_string_lossy();
    let stem = unit_stem(schedule_name)?;
    let svc = service_path(schedule_name)?;
    let tmr = timer_path(schedule_name)?;

    fs::create_dir_all(svc.parent().unwrap())?;

    if schedule_name == DEFAULT_SCHEDULE_NAME {
        remove_legacy_systemd_schedule()?;
    }

    let _ = Command::new("systemctl")
        .args(["--user", "stop", &format!("{stem}.timer")])
        .output();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", &format!("{stem}.timer")])
        .output();

    let service_content = render_systemd_service(schedule_name, &exe_str, &run_args);
    let timer_content = render_systemd_timer(schedule_name, hour, minute);

    fs::write(&svc, &service_content)
        .with_context(|| format!("Failed to write {}", svc.display()))?;
    fs::write(&tmr, &timer_content)
        .with_context(|| format!("Failed to write {}", tmr.display()))?;

    let output = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output()
        .context("Failed to run systemctl daemon-reload")?;
    if !output.status.success() {
        bail!(
            "systemctl daemon-reload failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let timer_unit = format!("{stem}.timer");
    let output = Command::new("systemctl")
        .args(["--user", "enable", "--now", &timer_unit])
        .output()
        .context("Failed to enable timer")?;
    if !output.status.success() {
        bail!(
            "systemctl enable failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("{}", ui::success("✓ Schedule installed (systemd)"));
    println!("  {} {}", ui::label("Name"), ui::name(schedule_name));
    println!(
        "  {} {}",
        ui::label("Service"),
        ui::path(&svc.display().to_string())
    );
    println!(
        "  {} {}",
        ui::label("Timer"),
        ui::path(&tmr.display().to_string())
    );
    println!("  {} {}", ui::label("Binary"), ui::path(&exe_str));
    println!(
        "  {} {}",
        ui::label("Time"),
        ui::value(&format!("{hour:02}:{minute:02} daily"))
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn render_systemd_service(schedule_name: &str, exe: &str, run_args: &[String]) -> String {
    let args = run_args
        .iter()
        .map(|arg| quote_systemd_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "[Unit]\n\
         Description=Clean up merged git worktrees ({schedule_name})\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={} {}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        quote_systemd_arg(exe),
        args,
    )
}

#[cfg(target_os = "linux")]
fn render_systemd_timer(schedule_name: &str, hour: u8, minute: u8) -> String {
    format!(
        "[Unit]\n\
         Description=Daily cleanup of merged git worktrees ({schedule_name})\n\
         \n\
         [Timer]\n\
         OnCalendar=*-*-* {hour:02}:{minute:02}:00\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
    )
}

#[cfg(target_os = "linux")]
fn quote_systemd_arg(arg: &str) -> String {
    if arg
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '='))
    {
        return arg.to_string();
    }

    format!("'{}'", arg.replace('\\', "\\\\").replace('\'', "\\'"))
}

#[cfg(target_os = "linux")]
fn remove_legacy_systemd_schedule() -> Result<bool> {
    let svc = legacy_service_path()?;
    let tmr = legacy_timer_path()?;
    if !svc.exists() && !tmr.exists() {
        return Ok(false);
    }

    let _ = Command::new("systemctl")
        .args(["--user", "stop", "worktree-gc.timer"])
        .output();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "worktree-gc.timer"])
        .output();

    if tmr.exists() {
        fs::remove_file(&tmr)?;
    }
    if svc.exists() {
        fs::remove_file(&svc)?;
    }

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();
    Ok(true)
}

#[cfg(target_os = "linux")]
pub fn uninstall(schedule_name: &str) -> Result<()> {
    let stem = unit_stem(schedule_name)?;
    let svc = service_path(schedule_name)?;
    let tmr = timer_path(schedule_name)?;
    let mut removed = false;

    let _ = Command::new("systemctl")
        .args(["--user", "stop", &format!("{stem}.timer")])
        .output();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", &format!("{stem}.timer")])
        .output();

    if tmr.exists() {
        fs::remove_file(&tmr)?;
        println!(
            "  {} {}",
            ui::label("Deleted"),
            ui::path(&tmr.display().to_string())
        );
        removed = true;
    }
    if svc.exists() {
        fs::remove_file(&svc)?;
        println!(
            "  {} {}",
            ui::label("Deleted"),
            ui::path(&svc.display().to_string())
        );
        removed = true;
    }

    if schedule_name == DEFAULT_SCHEDULE_NAME && remove_legacy_systemd_schedule()? {
        println!("{}", ui::success("✓ Legacy schedule removed"));
        removed = true;
    }

    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .output();

    if removed {
        println!("{}", ui::success("✓ Schedule removed"));
        println!("  {} {}", ui::label("Name"), ui::name(schedule_name));
    } else {
        println!(
            "{}",
            ui::warning("No schedule installed (unit files not found)")
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn print_config(schedule_names: &[String]) -> Result<()> {
    println!("{}", ui::title("Schedule configuration"));
    println!("  {} {}", ui::label("Scheduler"), ui::value("systemd"));

    for name in printable_schedule_names(schedule_names) {
        let stem = unit_stem(&name)?;
        let svc = service_path(&name)?;
        let tmr = timer_path(&name)?;
        if !tmr.exists() {
            println!("  {} {}", ui::name(&name), ui::warning("not installed"));
            println!(
                "    {} {}",
                ui::label("Service"),
                ui::path(&svc.display().to_string())
            );
            println!(
                "    {} {}",
                ui::label("Timer"),
                ui::path(&tmr.display().to_string())
            );
            continue;
        }

        let timer_unit = format!("{stem}.timer");
        let output = Command::new("systemctl")
            .args(["--user", "is-active", &timer_unit])
            .output()
            .context("Failed to check timer status")?;

        let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
        println!("  {} {}", ui::name(&name), ui::value(&state));
        println!(
            "    {} {}",
            ui::label("Service"),
            ui::path(&svc.display().to_string())
        );
        println!(
            "    {} {}",
            ui::label("Timer"),
            ui::path(&tmr.display().to_string())
        );

        let timer_content = fs::read_to_string(&tmr)?;
        if let Some(schedule) = extract_systemd_timer_schedule(&timer_content) {
            println!("    {} {}", ui::label("Schedule"), ui::value(&schedule));
        }

        let output = Command::new("systemctl")
            .args(["--user", "list-timers", &timer_unit, "--no-pager"])
            .output();
        if let Ok(output) = output {
            let stdout = String::from_utf8_lossy(&output.stdout);
            for line in stdout.lines().skip(1).take(1) {
                println!("    {} {}", ui::label("Next run"), ui::value(line));
            }
        }
    }

    if legacy_timer_path()?.exists() {
        println!(
            "  {} {}",
            ui::name("legacy daily"),
            ui::warning("installed")
        );
        println!(
            "    {} {}",
            ui::label("Timer"),
            ui::path(&legacy_timer_path()?.display().to_string())
        );
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn extract_systemd_timer_schedule(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.trim().strip_prefix("OnCalendar="))
        .map(|s| s.to_string())
}

// ============================================================
// Unsupported platforms
// ============================================================

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn install_workspace(
    _schedule_name: &str,
    _workspace_name: &str,
    _hour: u8,
    _minute: u8,
) -> Result<()> {
    bail!("Scheduled execution is not supported on this platform. Supported: macOS (launchd), Linux (systemd).");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn uninstall(_schedule_name: &str) -> Result<()> {
    bail!("Scheduled execution is not supported on this platform.");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn print_config(_schedule_names: &[String]) -> Result<()> {
    println!("{}", ui::title("Schedule configuration"));
    println!(
        "  {} {}",
        ui::label("Scheduler"),
        ui::warning("unsupported")
    );
    println!(
        "  {} {}",
        ui::label("Status"),
        ui::muted("not supported on this platform")
    );
    Ok(())
}

fn printable_schedule_names(schedule_names: &[String]) -> Vec<String> {
    if schedule_names.is_empty() {
        return vec![DEFAULT_SCHEDULE_NAME.to_string()];
    }

    let mut names = schedule_names.to_vec();
    names.sort();
    names.dedup();
    names
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_validate_schedule_name() {
        assert!(super::validate_schedule_name("default").is_ok());
        assert!(super::validate_schedule_name("team-a_1").is_ok());
        assert!(super::validate_schedule_name("Daily").is_err());
        assert!(super::validate_schedule_name("daily").is_err());
        assert!(super::validate_schedule_name("../daily").is_err());
    }

    #[test]
    fn test_printable_schedule_names_defaults_when_empty() {
        assert_eq!(
            super::printable_schedule_names(&[]),
            vec![super::DEFAULT_SCHEDULE_NAME.to_string()]
        );
    }

    #[test]
    fn test_printable_schedule_names_sorts_and_deduplicates() {
        assert_eq!(
            super::printable_schedule_names(&[
                "nightly".to_string(),
                "default".to_string(),
                "nightly".to_string(),
            ]),
            vec!["default".to_string(), "nightly".to_string()]
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_render_launchd_plist_uses_workspace_argument() {
        let args = super::workspace_run_args("personal");
        let content = super::render_launchd_plist(
            "com.worktree-gc.personal",
            "/usr/local/bin/worktree-gc",
            &args,
            "/usr/bin:/bin",
            "/tmp/stdout.log",
            "/tmp/stderr.log",
            9,
            30,
        );

        assert!(content.contains("<string>com.worktree-gc.personal</string>"));
        assert!(content.contains("<string>--workspace</string>"));
        assert!(content.contains("<string>personal</string>"));
        assert!(content.contains("<integer>9</integer>"));
        assert!(content.contains("<integer>30</integer>"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_render_systemd_service_uses_workspace_argument() {
        let args = super::workspace_run_args("personal");
        let content =
            super::render_systemd_service("personal", "/usr/local/bin/worktree-gc", &args);

        assert!(content.contains("Description=Clean up merged git worktrees (personal)"));
        assert!(content.contains("ExecStart=/usr/local/bin/worktree-gc run --workspace personal"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_extract_systemd_timer_schedule() {
        let content = "\
[Timer]
OnCalendar=*-*-* 09:00:00
        Persistent=true
";

        assert_eq!(
            super::extract_systemd_timer_schedule(content).as_deref(),
            Some("*-*-* 09:00:00")
        );
    }
}
