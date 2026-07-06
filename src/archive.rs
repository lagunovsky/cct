//! `cct archive` / `cct unarchive` — move cold sessions out of
//! `~/.claude/projects` into a zstd-compressed mirror under
//! `~/.claude/archive`. The search index keeps its documents: archived
//! entries ride through incremental updates untouched, and snippet reads
//! fall back to on-the-fly decompression.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, bail, Context, Result};

use crate::path_codec::projects_dir;
use crate::recent::session_activity_time_ms;
use crate::term::{fmt_size, BOLD, DIM, GREEN, RED, RESET};

/// Compression level: ~5.5x on real session logs at ~60 MB/s. Higher levels
/// buy almost nothing (+0.2x) for a 10x slowdown.
const ZSTD_LEVEL: i32 = 12;

const ZST_EXT: &str = "zst";

pub const DEFAULT_OLDER_THAN_DAYS: u64 = 90;

pub fn archive_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".claude").join("archive"))
}

/// Where a live file under `projects_root` lives in the archive: the same
/// relative path with `.zst` appended.
pub fn archive_path_for_roots(
    live: &Path,
    projects_root: &Path,
    archive_root: &Path,
) -> Option<PathBuf> {
    let rel = live.strip_prefix(projects_root).ok()?;
    let mut name = rel.as_os_str().to_owned();
    name.push(".");
    name.push(ZST_EXT);
    Some(archive_root.join(PathBuf::from(name)))
}

/// Archive twin of an indexed live path, using the real roots.
pub fn archive_path_for(live: &Path) -> Option<PathBuf> {
    let projects = projects_dir().ok()?;
    let archive = archive_root().ok()?;
    archive_path_for_roots(live, &projects, &archive)
}

/// True when the live file is gone but its archived twin exists.
pub fn is_archived(live: &Path) -> bool {
    archive_path_for(live).is_some_and(|p| p.exists())
}

/// Decompress the archived twin of `live` in full. Returns None when there
/// is no archive or it cannot be read.
pub fn read_archived(live: &Path) -> Option<Vec<u8>> {
    read_zst(&archive_path_for(live)?).ok()
}

/// Decompress a `.zst` file fully into memory. Used at index time to feed an
/// archived session's content to the indexer from its compressed source.
pub fn read_zst(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    zstd::stream::decode_all(file).with_context(|| format!("decompress {}", path.display()))
}

/// Compress `src` into `dst` (atomically via tmp+rename), carrying over the
/// source mtime so age remains visible in the archive.
fn compress_file(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut reader = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let mtime = reader.metadata().ok().and_then(|m| m.modified().ok());
    let tmp = dst.with_extension("zst.tmp");
    let out = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    let mut encoder = zstd::stream::Encoder::new(out, ZSTD_LEVEL)?;
    std::io::copy(&mut reader, &mut encoder).with_context(|| format!("compress {}", src.display()))?;
    let mut out = encoder.finish()?;
    out.flush()?;
    if let Some(mtime) = mtime {
        let _ = out.set_times(fs::FileTimes::new().set_modified(mtime));
    }
    drop(out);
    fs::rename(&tmp, dst).with_context(|| format!("replace {}", dst.display()))?;
    Ok(())
}

/// Decompress `src` into `dst` (atomically via tmp+rename). By default the
/// restored file gets a current mtime ON PURPOSE: Claude Code's cleanup
/// deletes transcripts by file age, and a restored session must not be reaped
/// immediately. With `keep_mtime` the archive's mtime (carried over from the
/// original file at archive time) is preserved instead.
fn decompress_file(src: &Path, dst: &Path, keep_mtime: bool) -> Result<()> {
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let file = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let original_mtime = file.metadata().ok().and_then(|m| m.modified().ok());
    let data = zstd::stream::decode_all(file)
        .with_context(|| format!("decompress {}", src.display()))?;
    let tmp = dst.with_extension("restore.tmp");
    let mut out = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
    out.write_all(&data)?;
    out.flush()?;
    let mtime = if keep_mtime {
        original_mtime.unwrap_or_else(SystemTime::now)
    } else {
        SystemTime::now()
    };
    let _ = out.set_times(fs::FileTimes::new().set_modified(mtime));
    drop(out);
    fs::rename(&tmp, dst).with_context(|| format!("replace {}", dst.display()))?;
    Ok(())
}

/// Parse `90d` / `3m` / `1y` / bare days into days.
pub fn parse_age_days(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    let (num, unit) = match raw.char_indices().rfind(|(_, c)| c.is_ascii_digit()) {
        Some((i, _)) => raw.split_at(i + 1),
        None => bail!("bad age {raw:?}: expected forms like 90d, 3m, 1y, or plain days"),
    };
    let n: u64 = num
        .parse()
        .with_context(|| format!("bad age number in {raw:?}"))?;
    match unit.trim() {
        "" | "d" | "day" | "days" => Ok(n),
        "w" | "week" | "weeks" => Ok(n * 7),
        "m" | "month" | "months" => Ok(n * 30),
        "y" | "year" | "years" => Ok(n * 365),
        other => bail!("bad age unit {other:?} in {raw:?} (use d/w/m/y)"),
    }
}

/// One session eligible for archiving: the jsonl plus its optional subdir.
struct ColdSession {
    id: String,
    jsonl: PathBuf,
    subdir: Option<PathBuf>,
    bytes: u64,
    last_activity_ms: i64,
}

pub struct ArchiveOptions {
    pub older_than_days: u64,
    /// Restrict to one project (absolute path already resolved); None = all.
    pub project: Option<PathBuf>,
    pub dry_run: bool,
    pub skip_confirm: bool,
    /// Daemon mode: print nothing unless something was archived or failed.
    pub quiet: bool,
}

pub fn run_archive(opts: ArchiveOptions) -> Result<()> {
    let projects = projects_dir()?;
    let archive = archive_root()?;
    run_archive_in(opts, &projects, &archive)
}

pub fn run_archive_in(opts: ArchiveOptions, projects: &Path, archive: &Path) -> Result<()> {
    let cutoff_ms = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
        - (opts.older_than_days as i64) * 24 * 3600 * 1000;

    let project_dirs = match &opts.project {
        Some(abs) => {
            let dir = projects.join(crate::path_codec::encode_project_dir(
                &abs.to_string_lossy(),
            ));
            if !dir.is_dir() {
                bail!("no sessions dir for {}", abs.display());
            }
            vec![dir]
        }
        None => {
            let mut dirs = Vec::new();
            for entry in
                fs::read_dir(projects).with_context(|| format!("read {}", projects.display()))?
            {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    dirs.push(entry.path());
                }
            }
            dirs.sort();
            dirs
        }
    };

    let mut cold: Vec<ColdSession> = Vec::new();
    for dir in &project_dirs {
        collect_cold_sessions(dir, cutoff_ms, &mut cold)?;
    }
    if cold.is_empty() {
        if !opts.quiet {
            println!(
                "Nothing to archive: no sessions older than {} days.",
                opts.older_than_days
            );
        }
        return Ok(());
    }
    cold.sort_by_key(|s| s.last_activity_ms);

    let total_bytes: u64 = cold.iter().map(|s| s.bytes).sum();
    if !opts.quiet {
        println!(
            "{BOLD}Archive {} session(s) older than {} days{RESET} ({} on disk)",
            cold.len(),
            opts.older_than_days,
            fmt_size(total_bytes)
        );
        println!("  from: {}", projects.display());
        println!("  to:   {} {DIM}(zstd){RESET}", archive.display());
    }
    if opts.dry_run {
        for s in &cold {
            println!("  - {}  {DIM}{}{RESET}", s.id, fmt_size(s.bytes));
        }
        println!("\n{DIM}(dry-run — no changes made){RESET}");
        return Ok(());
    }
    if !opts.skip_confirm {
        let prompt = format!("Archive {} session(s)? [y/N]: ", cold.len());
        if !crate::picker::confirm(&prompt) {
            println!("{DIM}aborted{RESET}");
            return Ok(());
        }
    }

    let mut archived = 0usize;
    let mut archived_bytes = 0u64;
    let mut compressed_bytes = 0u64;
    let mut failed: Vec<String> = Vec::new();
    for s in &cold {
        match archive_session(s, projects, archive) {
            Ok(zst_bytes) => {
                archived += 1;
                archived_bytes += s.bytes;
                compressed_bytes += zst_bytes;
            }
            Err(err) => failed.push(format!("{}: {err:#}", s.id)),
        }
    }

    if !opts.quiet || archived > 0 || !failed.is_empty() {
        println!(
            "\n{GREEN}Done.{RESET} Archived {archived}/{} session(s): {} → {} ({:.1}x)",
            cold.len(),
            fmt_size(archived_bytes),
            fmt_size(compressed_bytes),
            if compressed_bytes > 0 {
                archived_bytes as f64 / compressed_bytes as f64
            } else {
                0.0
            }
        );
        println!("{DIM}Restore with: cct unarchive <id>{RESET}");
    }
    if !failed.is_empty() {
        println!("\n{RED}Errors:{RESET}");
        for e in &failed {
            println!("  {e}");
        }
        return Err(anyhow!("{} session(s) failed", failed.len()));
    }
    Ok(())
}

fn collect_cold_sessions(
    project_dir: &Path,
    cutoff_ms: i64,
    out: &mut Vec<ColdSession>,
) -> Result<()> {
    for entry in fs::read_dir(project_dir)
        .with_context(|| format!("read {}", project_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type()?.is_file()
            || path.extension().and_then(|s| s.to_str()) != Some("jsonl")
        {
            continue;
        }
        let last = session_activity_time_ms(&path);
        if last == 0 || last >= cutoff_ms {
            continue;
        }
        let id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let subdir = {
            let candidate = project_dir.join(&id);
            candidate.is_dir().then_some(candidate)
        };
        let mut bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if let Some(subdir) = &subdir {
            bytes += dir_bytes(subdir);
        }
        out.push(ColdSession {
            id,
            jsonl: path,
            subdir,
            bytes,
            last_activity_ms: last,
        });
    }
    Ok(())
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => total += dir_bytes(&path),
            Ok(t) if t.is_file() => {
                total += fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            }
            _ => {}
        }
    }
    total
}

/// Move one session into the archive. Each file is compressed into place
/// FIRST and the original removed only after — at any instant either the
/// live file or its archive twin exists, so a concurrent index update never
/// sees the session as deleted. Returns compressed size.
fn archive_session(s: &ColdSession, projects: &Path, archive: &Path) -> Result<u64> {
    let mut files = vec![s.jsonl.clone()];
    if let Some(subdir) = &s.subdir {
        walk_files(subdir, &mut |p| files.push(p.to_path_buf()))?;
    }

    let mut compressed = 0u64;
    for src in &files {
        let dst = archive_path_for_roots(src, projects, archive)
            .ok_or_else(|| anyhow!("{} is outside the projects dir", src.display()))?;
        if dst.exists() {
            bail!(
                "archive already has {} — unarchive or remove it first",
                dst.display()
            );
        }
        compress_file(src, &dst)?;
        compressed += fs::metadata(&dst).map(|m| m.len()).unwrap_or(0);
        fs::remove_file(src).with_context(|| format!("remove {}", src.display()))?;
    }
    if let Some(subdir) = &s.subdir {
        remove_empty_dirs(subdir);
    }
    Ok(compressed)
}

fn walk_files(dir: &Path, visit: &mut dyn FnMut(&Path)) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_files(&path, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}

fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = fs::remove_dir(dir); // fails (kept) when not empty
}

pub fn run_unarchive(refs: &[String], keep_mtime: bool) -> Result<()> {
    let projects = projects_dir()?;
    let archive = archive_root()?;
    run_unarchive_in(refs, &projects, &archive, keep_mtime)
}

pub fn run_unarchive_in(
    refs: &[String],
    projects: &Path,
    archive: &Path,
    keep_mtime: bool,
) -> Result<()> {
    if refs.is_empty() {
        bail!("unarchive needs at least one session id (or unique prefix)");
    }
    if !archive.is_dir() {
        bail!("no archive at {}", archive.display());
    }

    let mut restored = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for r in refs {
        match unarchive_one(r, projects, archive, keep_mtime) {
            Ok(id) => {
                println!("{GREEN}restored{RESET} {id}");
                restored += 1;
            }
            Err(err) => failed.push(format!("{r}: {err:#}")),
        }
    }
    println!("\n{GREEN}Done.{RESET} Restored {restored}/{} session(s).", refs.len());
    if !failed.is_empty() {
        println!("\n{RED}Errors:{RESET}");
        for e in &failed {
            println!("  {e}");
        }
        return Err(anyhow!("{} failure(s)", failed.len()));
    }
    Ok(())
}

fn unarchive_one(q: &str, projects: &Path, archive: &Path, keep_mtime: bool) -> Result<String> {
    if q.contains(['/', '\\']) || q == "." || q == ".." {
        bail!("not a session id");
    }
    // Find `<project>/<id>.jsonl.zst` across all archived projects.
    let mut matches: Vec<(PathBuf, String)> = Vec::new(); // (project dir in archive, id)
    for entry in fs::read_dir(archive).with_context(|| format!("read {}", archive.display()))? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        for file in fs::read_dir(&dir)? {
            let file = file?;
            let name = file.file_name();
            let name = name.to_string_lossy();
            let Some(stem) = name.strip_suffix(".jsonl.zst") else {
                continue;
            };
            if stem == q || stem.starts_with(q) {
                matches.push((dir.clone(), stem.to_string()));
            }
        }
    }
    let (project_dir, id) = match matches.len() {
        0 => bail!("no archived session matches"),
        1 => matches.remove(0),
        n => bail!("ambiguous ({n} matches)"),
    };

    // Restore the jsonl and every file of the session subdir.
    let rel_project = project_dir
        .strip_prefix(archive)
        .map_err(|_| anyhow!("bad archive layout"))?;
    let jsonl_zst = project_dir.join(format!("{id}.jsonl.zst"));
    let live_jsonl = projects.join(rel_project).join(format!("{id}.jsonl"));
    if live_jsonl.exists() {
        bail!("live session already exists: {}", live_jsonl.display());
    }
    decompress_file(&jsonl_zst, &live_jsonl, keep_mtime)?;

    let subdir_zst_root = project_dir.join(&id);
    if subdir_zst_root.is_dir() {
        let mut files = Vec::new();
        walk_files(&subdir_zst_root, &mut |p| files.push(p.to_path_buf()))?;
        for src in &files {
            let rel = src
                .strip_prefix(archive)
                .map_err(|_| anyhow!("bad archive layout"))?;
            let rel_str = rel.to_string_lossy();
            let Some(live_rel) = rel_str.strip_suffix(".zst") else {
                continue;
            };
            decompress_file(src, &projects.join(live_rel), keep_mtime)?;
        }
        for src in &files {
            let _ = fs::remove_file(src);
        }
        remove_empty_dirs(&subdir_zst_root);
    }
    fs::remove_file(&jsonl_zst).with_context(|| format!("remove {}", jsonl_zst.display()))?;
    let _ = fs::remove_dir(&project_dir); // cleanup if the project is now empty
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_path_mirrors_layout_with_zst_suffix() {
        let live = Path::new("/p/-Users-x-proj/abc.jsonl");
        let mapped =
            archive_path_for_roots(live, Path::new("/p"), Path::new("/a")).unwrap();
        assert_eq!(mapped, PathBuf::from("/a/-Users-x-proj/abc.jsonl.zst"));
        let sub = Path::new("/p/-Users-x-proj/abc/tool-results/t.txt");
        let mapped = archive_path_for_roots(sub, Path::new("/p"), Path::new("/a")).unwrap();
        assert_eq!(
            mapped,
            PathBuf::from("/a/-Users-x-proj/abc/tool-results/t.txt.zst")
        );
        assert!(archive_path_for_roots(Path::new("/elsewhere/f"), Path::new("/p"), Path::new("/a")).is_none());
    }

    #[test]
    fn parse_age_days_accepts_units() {
        assert_eq!(parse_age_days("90").unwrap(), 90);
        assert_eq!(parse_age_days("90d").unwrap(), 90);
        assert_eq!(parse_age_days("2w").unwrap(), 14);
        assert_eq!(parse_age_days("3m").unwrap(), 90);
        assert_eq!(parse_age_days("1y").unwrap(), 365);
        assert!(parse_age_days("soon").is_err());
        assert!(parse_age_days("3h").is_err());
    }

    #[test]
    fn compress_decompress_roundtrip_preserves_content() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("s.jsonl");
        let payload = "line one\nline two with unicode ⠋⠙\n".repeat(500);
        fs::write(&src, &payload).unwrap();
        let zst = tmp.path().join("s.jsonl.zst");
        compress_file(&src, &zst).unwrap();
        assert!(fs::metadata(&zst).unwrap().len() < payload.len() as u64 / 2);
        let restored = tmp.path().join("restored.jsonl");
        decompress_file(&zst, &restored, false).unwrap();
        assert_eq!(fs::read_to_string(&restored).unwrap(), payload);
    }
}
