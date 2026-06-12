//! Workflow tests for `cct archive` / `cct unarchive` driven entirely inside
//! a tempdir via the `_in` variants.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use cct::archive::{run_archive_in, run_unarchive_in, ArchiveOptions};

struct ArchiveFixture {
    _tmp: tempfile::TempDir,
    projects: PathBuf,
    archive: PathBuf,
}

impl ArchiveFixture {
    fn new() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let projects = tmp.path().join("projects");
        let archive = tmp.path().join("archive");
        fs::create_dir_all(&projects)?;
        Ok(Self {
            _tmp: tmp,
            projects,
            archive,
        })
    }

    /// Session whose LAST content timestamp controls its age.
    fn write_session(&self, project: &str, id: &str, timestamp: &str) -> PathBuf {
        let dir = self.projects.join(project);
        fs::create_dir_all(&dir).unwrap();
        let line = format!(
            r#"{{"type":"user","timestamp":"{timestamp}","message":{{"content":"prompt of {id}"}}}}"#
        );
        let path = dir.join(format!("{id}.jsonl"));
        fs::write(&path, format!("{line}\n")).unwrap();
        path
    }

    fn opts(days: u64) -> ArchiveOptions {
        ArchiveOptions {
            older_than_days: days,
            project: None,
            dry_run: false,
            skip_confirm: true,
        }
    }

    fn zst(&self, project: &str, name: &str) -> PathBuf {
        self.archive.join(project).join(format!("{name}.zst"))
    }
}

fn recent_timestamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).expect("read")
}

#[test]
fn archive_moves_only_cold_sessions() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    let old = fx.write_session("-proj", "old-session", "2020-01-01T00:00:00Z");
    let original = read(&old);
    let fresh = fx.write_session("-proj", "fresh-session", &recent_timestamp());

    run_archive_in(ArchiveFixture::opts(90), &fx.projects, &fx.archive)?;

    assert!(!old.exists(), "cold session must leave projects");
    assert!(fx.zst("-proj", "old-session.jsonl").exists());
    assert!(fresh.exists(), "fresh session must stay");
    assert!(!fx.zst("-proj", "fresh-session.jsonl").exists());

    // Roundtrip: restore and compare content.
    run_unarchive_in(&["old-session".to_string()], &fx.projects, &fx.archive, false)?;
    assert_eq!(read(&old), original);
    assert!(!fx.zst("-proj", "old-session.jsonl").exists());
    Ok(())
}

#[test]
fn archive_carries_session_subdir() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    fx.write_session("-proj", "with-subdir", "2020-01-01T00:00:00Z");
    let tools = fx.projects.join("-proj").join("with-subdir").join("tool-results");
    fs::create_dir_all(&tools)?;
    fs::write(tools.join("trace.log"), "tool output\n")?;

    run_archive_in(ArchiveFixture::opts(90), &fx.projects, &fx.archive)?;

    assert!(fx
        .archive
        .join("-proj/with-subdir/tool-results/trace.log.zst")
        .exists());
    assert!(
        !fx.projects.join("-proj").join("with-subdir").exists(),
        "emptied subdir must be removed from projects"
    );

    run_unarchive_in(&["with-subdir".to_string()], &fx.projects, &fx.archive, false)?;
    assert_eq!(read(&tools.join("trace.log")), "tool output\n");
    assert!(
        !fx.archive.join("-proj").exists(),
        "emptied archive project dir must be removed"
    );
    Ok(())
}

#[test]
fn archive_dry_run_changes_nothing() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    let old = fx.write_session("-proj", "dry-session", "2020-01-01T00:00:00Z");

    let opts = ArchiveOptions {
        dry_run: true,
        ..ArchiveFixture::opts(90)
    };
    run_archive_in(opts, &fx.projects, &fx.archive)?;

    assert!(old.exists());
    assert!(!fx.archive.exists());
    Ok(())
}

#[test]
fn unarchive_resolves_unique_prefix_and_rejects_ambiguous() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    fx.write_session("-proj", "aaaa1111", "2020-01-01T00:00:00Z");
    fx.write_session("-proj", "aaaa2222", "2020-01-01T00:00:00Z");
    run_archive_in(ArchiveFixture::opts(90), &fx.projects, &fx.archive)?;

    assert!(
        run_unarchive_in(&["aaaa".to_string()], &fx.projects, &fx.archive, false).is_err(),
        "ambiguous prefix must fail"
    );
    run_unarchive_in(&["aaaa1".to_string()], &fx.projects, &fx.archive, false)?;
    assert!(fx.projects.join("-proj").join("aaaa1111.jsonl").exists());
    assert!(fx.zst("-proj", "aaaa2222.jsonl").exists(), "other stays archived");
    Ok(())
}

#[test]
fn unarchive_refuses_when_live_session_exists() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    let live = fx.write_session("-proj", "dupe1234", "2020-01-01T00:00:00Z");
    run_archive_in(ArchiveFixture::opts(90), &fx.projects, &fx.archive)?;
    // Recreate a live session with the same id.
    fs::create_dir_all(live.parent().unwrap())?;
    fs::write(&live, "{}\n")?;

    assert!(run_unarchive_in(&["dupe1234".to_string()], &fx.projects, &fx.archive, false).is_err());
    assert!(fx.zst("-proj", "dupe1234.jsonl").exists(), "archive copy must survive");
    Ok(())
}

#[test]
fn restored_file_gets_fresh_mtime() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    let live = fx.write_session("-proj", "mtime123", "2020-01-01T00:00:00Z");
    // Make the live file look old so the carried-over archive mtime is old too.
    let old_time = filetime::FileTime::from_unix_time(1_577_836_800, 0); // 2020-01-01
    filetime::set_file_mtime(&live, old_time)?;

    run_archive_in(ArchiveFixture::opts(90), &fx.projects, &fx.archive)?;
    run_unarchive_in(&["mtime123".to_string()], &fx.projects, &fx.archive, false)?;

    let restored_mtime = fs::metadata(&live)?.modified()?;
    let age = std::time::SystemTime::now()
        .duration_since(restored_mtime)
        .unwrap_or_default();
    // A fresh mtime keeps Claude Code's age-based cleanup from reaping the
    // session right after restore.
    assert!(age.as_secs() < 60, "restored mtime must be current, age {age:?}");
    Ok(())
}

#[test]
fn keep_mtime_flag_preserves_original_mtime() -> Result<()> {
    let fx = ArchiveFixture::new()?;
    let live = fx.write_session("-proj", "keepm123", "2020-01-01T00:00:00Z");
    let old_time = filetime::FileTime::from_unix_time(1_577_836_800, 0); // 2020-01-01
    filetime::set_file_mtime(&live, old_time)?;

    run_archive_in(ArchiveFixture::opts(90), &fx.projects, &fx.archive)?;
    run_unarchive_in(&["keepm123".to_string()], &fx.projects, &fx.archive, true)?;

    let restored = filetime::FileTime::from_last_modification_time(&fs::metadata(&live)?);
    assert_eq!(
        restored.unix_seconds(),
        old_time.unix_seconds(),
        "--keep-mtime must carry the original mtime through archive and restore"
    );
    Ok(())
}
