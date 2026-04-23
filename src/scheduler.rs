use anyhow::{bail, Context, Result};
use log::warn;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

// ============================================================
// macOS (launchd)
// ============================================================

#[cfg(target_os = "macos")]
const LABEL: &str = "com.worktree-gc.daily";

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home
        .join("Library/LaunchAgents")
        .join(format!("{LABEL}.plist")))
}

#[cfg(target_os = "macos")]
fn log_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("Cannot determine home directory")?;
    Ok(home.join(".local/share/worktree-gc"))
}

#[cfg(target_os = "macos")]
pub fn install(scan_dir: &str, hour: u8, minute: u8) -> Result<()> {
    let exe = env::current_exe().context("Cannot determine binary path")?;
    let exe_str = exe.to_string_lossy();
    let plist = plist_path()?;
    let log_dir = log_dir()?;
    let log_file = log_dir.join("gc.jsonl");
    let stdout_log = log_dir.join("launchd-stdout.log");
    let stderr_log = log_dir.join("launchd-stderr.log");

    // Capture current PATH so launchd can find git/gh
    let current_path = env::var("PATH").unwrap_or_default();

    fs::create_dir_all(plist.parent().unwrap())?;
    fs::create_dir_all(&log_dir)?;

    // Unload first if already installed
    if plist.exists() {
        let _ = Command::new("launchctl")
            .args(["unload", &plist.to_string_lossy()])
            .output();
    }

    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LABEL}</string>

    <key>ProgramArguments</key>
    <array>
        <string>{exe_str}</string>
        <string>run</string>
        <string>--dir</string>
        <string>{scan_dir}</string>
        <string>--log-file</string>
        <string>{log_file}</string>
    </array>

    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>{current_path}</string>
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
"#,
        log_file = log_file.display(),
        stdout_log = stdout_log.display(),
        stderr_log = stderr_log.display(),
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

    println!("✓ Schedule installed (launchd)");
    println!("  Plist:    {}", plist.display());
    println!("  Binary:   {exe_str}");
    println!("  Scan dir: {scan_dir}");
    println!("  Time:     {hour:02}:{minute:02} daily");
    println!("  Log:      {}", log_file.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn uninstall() -> Result<()> {
    let plist = plist_path()?;
    if !plist.exists() {
        println!("No schedule installed (plist not found)");
        return Ok(());
    }

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

    println!("✓ Schedule removed");
    println!("  Deleted: {}", plist.display());
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn print_config() -> Result<()> {
    let plist = plist_path()?;
    println!("Schedule configuration:");
    println!("  Scheduler: launchd");

    if !plist.exists() {
        println!("  Status:    not installed");
        println!("  Plist:     {}", plist.display());
        return Ok(());
    }

    let output = Command::new("launchctl")
        .args(["list"])
        .output()
        .context("Failed to run launchctl list")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let loaded = stdout.lines().any(|l| l.contains(LABEL));

    println!(
        "  Status:    {}",
        if loaded {
            "active"
        } else {
            "installed but not loaded"
        }
    );
    println!("  Plist:     {}", plist.display());

    let content = fs::read_to_string(&plist)?;
    if let (Some(h), Some(m)) = (
        extract_plist_integer(&content, "Hour"),
        extract_plist_integer(&content, "Minute"),
    ) {
        println!("  Time:      {h:02}:{m:02} daily");
    }

    if !loaded {
        println!("  Activate:  launchctl load {}", plist.display());
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
fn service_path() -> Result<PathBuf> {
    Ok(systemd_dir()?.join("worktree-gc.service"))
}

#[cfg(target_os = "linux")]
fn timer_path() -> Result<PathBuf> {
    Ok(systemd_dir()?.join("worktree-gc.timer"))
}

#[cfg(target_os = "linux")]
pub fn install(scan_dir: &str, hour: u8, minute: u8) -> Result<()> {
    let exe = env::current_exe().context("Cannot determine binary path")?;
    let exe_str = exe.to_string_lossy();
    let svc = service_path()?;
    let tmr = timer_path()?;
    let log_dir = log_dir()?;
    let log_file = log_dir.join("gc.jsonl");

    fs::create_dir_all(svc.parent().unwrap())?;
    fs::create_dir_all(&log_dir)?;

    // Stop timer if already running
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "worktree-gc.timer"])
        .output();
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "worktree-gc.timer"])
        .output();

    let service_content = format!(
        "[Unit]\n\
         Description=Clean up git worktrees whose PRs have been merged\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStart={exe_str} run --dir {scan_dir} --log-file {log_file}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        log_file = log_file.display(),
    );

    let timer_content = format!(
        "[Unit]\n\
         Description=Daily cleanup of merged git worktrees\n\
         \n\
         [Timer]\n\
         OnCalendar=*-*-* {hour:02}:{minute:02}:00\n\
         Persistent=true\n\
         \n\
         [Install]\n\
         WantedBy=timers.target\n",
    );

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

    let output = Command::new("systemctl")
        .args(["--user", "enable", "--now", "worktree-gc.timer"])
        .output()
        .context("Failed to enable timer")?;
    if !output.status.success() {
        bail!(
            "systemctl enable failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    println!("✓ Schedule installed (systemd)");
    println!("  Service:  {}", svc.display());
    println!("  Timer:    {}", tmr.display());
    println!("  Binary:   {exe_str}");
    println!("  Scan dir: {scan_dir}");
    println!("  Time:     {hour:02}:{minute:02} daily");
    println!("  Log:      {}", log_file.display());
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn uninstall() -> Result<()> {
    let svc = service_path()?;
    let tmr = timer_path()?;

    if !svc.exists() && !tmr.exists() {
        println!("No schedule installed (unit files not found)");
        return Ok(());
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

    println!("✓ Schedule removed");
    if tmr.exists() {
        println!("  Deleted: {}", tmr.display());
    }
    if svc.exists() {
        println!("  Deleted: {}", svc.display());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn print_config() -> Result<()> {
    let svc = service_path()?;
    let tmr = timer_path()?;
    println!("Schedule configuration:");
    println!("  Scheduler: systemd");

    if !tmr.exists() {
        println!("  Status:    not installed");
        println!("  Service:   {}", svc.display());
        println!("  Timer:     {}", tmr.display());
        return Ok(());
    }

    let output = Command::new("systemctl")
        .args(["--user", "is-active", "worktree-gc.timer"])
        .output()
        .context("Failed to check timer status")?;

    let state = String::from_utf8_lossy(&output.stdout).trim().to_string();
    println!("  Status:    {state}");
    println!("  Service:   {}", svc.display());
    println!("  Timer:     {}", tmr.display());

    let timer_content = fs::read_to_string(&tmr)?;
    if let Some(schedule) = extract_systemd_timer_schedule(&timer_content) {
        println!("  Schedule:  {schedule}");
    }

    let output = Command::new("systemctl")
        .args(["--user", "list-timers", "worktree-gc.timer", "--no-pager"])
        .output();
    if let Ok(o) = output {
        let stdout = String::from_utf8_lossy(&o.stdout);
        for line in stdout.lines().skip(1).take(1) {
            println!("  Next run:  {line}");
        }
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
pub fn install(_scan_dir: &str, _hour: u8, _minute: u8) -> Result<()> {
    bail!("Scheduled execution is not supported on this platform. Supported: macOS (launchd), Linux (systemd).");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn uninstall() -> Result<()> {
    bail!("Scheduled execution is not supported on this platform.");
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn print_config() -> Result<()> {
    println!("Schedule configuration:");
    println!("  Scheduler: unsupported");
    println!("  Status:    not supported on this platform");
    Ok(())
}

// ============================================================
// Interactive wizard
// ============================================================

fn is_installed() -> bool {
    #[cfg(target_os = "macos")]
    {
        plist_path().map(|p| p.exists()).unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        timer_path().map(|p| p.exists()).unwrap_or(false)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

fn scheduler_name() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "launchd"
    }
    #[cfg(target_os = "linux")]
    {
        "systemd"
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        "unsupported"
    }
}

pub fn interactive_wizard(current_dir: &str) -> Result<()> {
    use console::style;
    use dialoguer::{Confirm, Select};

    println!();
    println!("  {}", style("⏰ worktree-gc scheduler").bold());
    println!(
        "  {}",
        style(format!(
            "   Platform: {} ({})",
            std::env::consts::OS,
            scheduler_name()
        ))
        .dim()
    );
    println!();

    let installed = is_installed();

    if installed {
        // Already installed — show status and offer actions
        print_config()?;
        println!();

        let choices = vec![
            "Update schedule (change time or directory)",
            "Remove schedule",
            "Cancel",
        ];
        let selection = Select::new()
            .with_prompt("What would you like to do?")
            .items(&choices)
            .default(0)
            .interact()?;

        match selection {
            0 => {
                // Update — ask for new settings and reinstall
                let (dir, hour, minute) = prompt_install_settings(current_dir)?;
                install(&dir, hour, minute)
            }
            1 => {
                let confirm = Confirm::new()
                    .with_prompt("Remove the daily schedule?")
                    .default(false)
                    .interact()?;
                if confirm {
                    uninstall()
                } else {
                    println!("Cancelled.");
                    Ok(())
                }
            }
            _ => {
                println!("Cancelled.");
                Ok(())
            }
        }
    } else {
        // Not installed — offer to set up
        println!("  No schedule is currently configured.");
        println!("  This wizard will set up daily automatic cleanup of merged worktrees.");
        println!();

        let confirm = Confirm::new()
            .with_prompt("Set up daily automatic execution?")
            .default(true)
            .interact()?;

        if !confirm {
            println!("Cancelled.");
            return Ok(());
        }

        let (dir, hour, minute) = prompt_install_settings(current_dir)?;

        println!();
        println!("  {}", style("Summary:").bold());
        println!("  Scan directory: {}", style(&dir).cyan());
        println!(
            "  Run daily at:   {}",
            style(format!("{hour:02}:{minute:02}")).cyan()
        );
        println!("  Scheduler:      {}", style(scheduler_name()).cyan());
        println!();

        let confirm = Confirm::new()
            .with_prompt("Install?")
            .default(true)
            .interact()?;

        if confirm {
            install(&dir, hour, minute)
        } else {
            println!("Cancelled.");
            Ok(())
        }
    }
}

fn prompt_install_settings(current_dir: &str) -> Result<(String, u8, u8)> {
    use dialoguer::Input;

    let dir: String = Input::new()
        .with_prompt("Directory to scan")
        .default(current_dir.to_string())
        .interact_text()?;

    let time: String = Input::new()
        .with_prompt("What time should it run? (HH:MM)")
        .default("09:00".to_string())
        .validate_with(|input: &String| -> std::result::Result<(), &str> {
            if parse_daily_time(input).is_some() {
                Ok(())
            } else {
                Err("Must be in HH:MM format, for example 09:00 or 18:30")
            }
        })
        .interact_text()?;

    let (hour, minute) = parse_daily_time(&time).expect("validated daily time");

    Ok((dir, hour, minute))
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

#[cfg(test)]
mod tests {
    #[test]
    fn test_parse_daily_time() {
        assert_eq!(super::parse_daily_time("09:00"), Some((9, 0)));
        assert_eq!(super::parse_daily_time("18:30"), Some((18, 30)));
        assert_eq!(super::parse_daily_time("24:00"), None);
        assert_eq!(super::parse_daily_time("09:60"), None);
        assert_eq!(super::parse_daily_time("9:00"), None);
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
