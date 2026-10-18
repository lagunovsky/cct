//! macOS launchd integration for periodic background updates.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use anyhow::{anyhow, Context, Result};

use crate::commands;
use crate::path_codec::index_root;

const DAEMON_LABEL: &str = "com.cct.background";
const DAEMON_INTERVAL: u32 = 1800; // 30 min

pub fn install_daemon() -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(anyhow!("daemon install is only supported on macOS"));
    }
    let exe = current_exe()?;
    let logs = logs_dir()?;
    fs::create_dir_all(&logs)?;
    let plist_file = plist_path()?;
    fs::create_dir_all(plist_file.parent().unwrap())?;

    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>update</string>
    </array>
    <key>StartInterval</key>
    <integer>{interval}</integer>
    <key>RunAtLoad</key>
    <false/>
    <key>ProcessType</key>
    <string>Background</string>
    <key>LowPriorityBackgroundIO</key>
    <true/>
    <key>Nice</key>
    <integer>10</integer>
    <key>StandardOutPath</key>
    <string>{logs}/cct.log</string>
    <key>StandardErrorPath</key>
    <string>{logs}/cct.log</string>
</dict>
</plist>
"#,
        label = DAEMON_LABEL,
        exe = exe.display(),
        interval = DAEMON_INTERVAL,
        logs = logs.display(),
    );
    fs::write(&plist_file, plist)?;

    Command::new("launchctl")
        .arg("unload")
        .arg(&plist_file)
        .status()
        .ok();
    let out = Command::new("launchctl")
        .arg("load")
        .arg(&plist_file)
        .output()
        .with_context(|| "launchctl load")?;
    if !out.status.success() {
        return Err(anyhow!(
            "launchctl load failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }

    println!("Installed: {}", plist_file.display());
    println!(
        "Runs every {} minutes when on AC power.",
        DAEMON_INTERVAL / 60
    );
    println!("Logs: {}/cct.log", logs.display());
    Ok(())
}

pub fn uninstall_daemon() -> Result<()> {
    let plist_file = plist_path()?;
    Command::new("launchctl")
        .arg("unload")
        .arg(&plist_file)
        .status()
        .ok();
    match fs::remove_file(&plist_file) {
        Ok(()) | Err(_) => {}
    }
    println!("Uninstalled: {}", plist_file.display());
    Ok(())
}

pub fn run_background_update() -> Result<()> {
    if !on_ac_power() {
        return Ok(());
    }
    commands::run_silent_update(
        &index_root()?,
        crate::index::writer::WriterConfig::for_background_daemon(),
    )?;
    // Opt-in background archiving (`cct archive --auto <age>`). Runs after
    // the update+flush above, so archived tails are already in the main index.
    let days = crate::config::auto_archive_days();
    let headless_days = crate::config::auto_archive_headless_days();
    if days.is_some() || headless_days.is_some() {
        crate::archive::run_archive(crate::archive::ArchiveOptions {
            older_than_days: days,
            headless_older_than_days: headless_days,
            project: None,
            dry_run: false,
            skip_confirm: true,
            quiet: true,
        })?;
    }
    Ok(())
}

pub fn daemon_installed() -> bool {
    plist_path().map(|p| p.exists()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn on_ac_power() -> bool {
    let Ok(out) = Command::new("pmset").args(["-g", "batt"]).output() else {
        return false;
    };
    String::from_utf8_lossy(&out.stdout).contains("'AC Power'")
}

#[cfg(not(target_os = "macos"))]
fn on_ac_power() -> bool {
    true
}

fn plist_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home
        .join("Library")
        .join("LaunchAgents")
        .join(format!("{DAEMON_LABEL}.plist")))
}

fn logs_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join("Library").join("Logs"))
}

fn current_exe() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(fs::canonicalize(&exe).unwrap_or(exe))
}
