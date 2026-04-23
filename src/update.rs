use anyhow::{Context, Result};

const REPO_OWNER: &str = "yuto-ts";
const REPO_NAME: &str = "worktree-gc";

pub fn run(check_only: bool) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{current}");
    println!("Checking for updates from github.com/{REPO_OWNER}/{REPO_NAME}...");

    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()
        .context("Failed to configure release fetcher")?
        .fetch()
        .context("Failed to fetch releases (check your internet connection)")?;

    let latest = match releases.first() {
        Some(r) => r,
        None => {
            println!("No releases found on GitHub.");
            return Ok(());
        }
    };

    let latest_ver = latest.version.trim_start_matches('v');
    println!("Latest version:  v{latest_ver}");

    match self_update::version::bump_is_greater(current, latest_ver) {
        Ok(true) => {}
        Ok(false) => {
            println!("Already up to date.");
            return Ok(());
        }
        Err(e) => anyhow::bail!("Failed to compare versions: {e}"),
    }

    if check_only {
        println!("Run `worktree-gc update` to upgrade.");
        return Ok(());
    }

    println!("Updating to v{latest_ver}...");

    self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("worktree-gc")
        .show_download_progress(true)
        .current_version(current)
        .build()
        .context("Failed to configure updater")?
        .update()
        .context("Failed to apply update")?;

    println!("Updated successfully to v{latest_ver}!");
    Ok(())
}
