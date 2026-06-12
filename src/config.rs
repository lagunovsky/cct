//! cct's own config (`~/.config/cct/config`, key=value lines) and helpers
//! for Claude Code's `~/.claude/settings.json`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// Claude Code deletes transcripts older than this many days unless
/// `cleanupPeriodDays` is set in `~/.claude/settings.json`.
pub const CLAUDE_DEFAULT_CLEANUP_DAYS: u64 = 30;

/// Retention below this triggers the reindex warning.
pub const CLEANUP_WARN_THRESHOLD_DAYS: u64 = 90;

const CLEANUP_WARNING_KEY: &str = "cleanup_warning";
const AUTO_ARCHIVE_KEY: &str = "auto_archive";

pub fn config_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".config").join("cct").join("config"))
}

pub fn read_config() -> BTreeMap<String, String> {
    config_path()
        .ok()
        .map(|p| read_config_at(&p))
        .unwrap_or_default()
}

fn read_config_at(path: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(raw) = fs::read_to_string(path) else {
        return map;
    };
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            map.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    map
}

pub fn set_config_value(key: &str, value: &str) -> Result<PathBuf> {
    let path = config_path()?;
    set_config_value_at(&path, key, value)?;
    Ok(path)
}

fn set_config_value_at(path: &Path, key: &str, value: &str) -> Result<()> {
    let mut map = read_config_at(path);
    map.insert(key.to_string(), value.to_string());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut out = String::new();
    for (k, v) in &map {
        out.push_str(k);
        out.push('=');
        out.push_str(v);
        out.push('\n');
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, out).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

pub fn cleanup_warning_enabled() -> bool {
    read_config()
        .get(CLEANUP_WARNING_KEY)
        .map(|v| v != "off")
        .unwrap_or(true)
}

pub fn set_cleanup_warning(enabled: bool) -> Result<PathBuf> {
    set_config_value(CLEANUP_WARNING_KEY, if enabled { "on" } else { "off" })
}

/// Background-archiving age threshold in days; None = disabled (default).
/// Set via `cct archive --auto 90d`, stored as the raw form (`90d`).
pub fn auto_archive_days() -> Option<u64> {
    auto_archive_days_from(&read_config())
}

fn auto_archive_days_from(map: &BTreeMap<String, String>) -> Option<u64> {
    let raw = map.get(AUTO_ARCHIVE_KEY)?;
    if raw == "off" {
        return None;
    }
    crate::archive::parse_age_days(raw).ok()
}

pub fn set_auto_archive(raw: &str) -> Result<PathBuf> {
    set_config_value(AUTO_ARCHIVE_KEY, raw)
}

pub fn claude_settings_path() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".claude").join("settings.json"))
}

/// `cleanupPeriodDays` from Claude Code's settings; `None` when unset
/// (Claude Code then uses its built-in default of 30 days).
pub fn read_cleanup_period_days() -> Result<Option<u64>> {
    read_cleanup_period_days_at(&claude_settings_path()?)
}

fn read_cleanup_period_days_at(path: &Path) -> Result<Option<u64>> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse {} as JSON", path.display()))?;
    Ok(value.get("cleanupPeriodDays").and_then(|v| v.as_u64()))
}

/// Set `cleanupPeriodDays` in Claude Code's settings.json, preserving every
/// other key. Refuses to touch a file it can't parse.
pub fn write_cleanup_period_days(days: u64) -> Result<PathBuf> {
    let path = claude_settings_path()?;
    write_cleanup_period_days_at(&path, days)?;
    Ok(path)
}

fn write_cleanup_period_days_at(path: &Path, days: u64) -> Result<()> {
    if days == 0 {
        bail!("cleanupPeriodDays must be at least 1");
    }
    let mut value: serde_json::Value = match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw).with_context(|| {
            format!(
                "{} is not valid JSON — fix it manually before letting cct edit it",
                path.display()
            )
        })?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            serde_json::Value::Object(serde_json::Map::new())
        }
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let serde_json::Value::Object(map) = &mut value else {
        bail!("{} does not contain a JSON object", path.display());
    };
    map.insert(
        "cleanupPeriodDays".to_string(),
        serde_json::Value::from(days),
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let pretty = serde_json::to_string_pretty(&value)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, format!("{pretty}\n")).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_roundtrip_preserves_other_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config");
        set_config_value_at(&path, "alpha", "1").unwrap();
        set_config_value_at(&path, "cleanup_warning", "off").unwrap();
        set_config_value_at(&path, "alpha", "2").unwrap();
        let map = read_config_at(&path);
        assert_eq!(map.get("alpha").map(String::as_str), Some("2"));
        assert_eq!(map.get("cleanup_warning").map(String::as_str), Some("off"));
    }

    #[test]
    fn auto_archive_parsing_from_config_map() {
        let mut map = BTreeMap::new();
        assert_eq!(auto_archive_days_from(&map), None, "unset = disabled");
        map.insert("auto_archive".to_string(), "90d".to_string());
        assert_eq!(auto_archive_days_from(&map), Some(90));
        map.insert("auto_archive".to_string(), "6m".to_string());
        assert_eq!(auto_archive_days_from(&map), Some(180));
        map.insert("auto_archive".to_string(), "off".to_string());
        assert_eq!(auto_archive_days_from(&map), None);
        map.insert("auto_archive".to_string(), "garbage".to_string());
        assert_eq!(auto_archive_days_from(&map), None, "bad value = disabled");
    }

    #[test]
    fn settings_write_preserves_existing_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, r#"{"model":"opus","cleanupPeriodDays":30}"#).unwrap();
        write_cleanup_period_days_at(&path, 3650).unwrap();
        assert_eq!(read_cleanup_period_days_at(&path).unwrap(), Some(3650));
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"model\""), "other keys must survive: {raw}");
    }

    #[test]
    fn settings_write_creates_file_when_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        write_cleanup_period_days_at(&path, 365).unwrap();
        assert_eq!(read_cleanup_period_days_at(&path).unwrap(), Some(365));
    }

    #[test]
    fn settings_write_refuses_invalid_json() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        fs::write(&path, "{broken").unwrap();
        assert!(write_cleanup_period_days_at(&path, 365).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{broken");
    }

    #[test]
    fn missing_settings_means_default_retention() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("settings.json");
        assert_eq!(read_cleanup_period_days_at(&path).unwrap(), None);
    }
}
