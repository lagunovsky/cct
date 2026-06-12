//! `cct rename` — migrate session dir + rewrite cwd.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::path_codec::{absolutize_lexical, encode_project_dir, projects_dir, short_project};
use crate::term::{BOLD, DIM, GREEN, RED, RESET};

pub struct RenameArgs {
    pub old_arg: String,
    pub new_arg: String,
    pub move_dir: bool,
    pub dry_run: bool,
}

pub fn rename_project(args: RenameArgs) -> Result<()> {
    let projects = projects_dir()?;
    rename_project_in(args, &projects)
}

/// Test-friendly variant: takes the projects directory explicitly so tests can
/// drive a tempdir without touching `~/.claude/projects`.
pub fn rename_project_in(args: RenameArgs, projects: &Path) -> Result<()> {
    // Don't canonicalize: encoded session dirs are derived from the path Claude
    // Code itself recorded (typically `os.Getwd()` output), which on macOS is the
    // unresolved `/var/...` form rather than `/private/var/...`. Symlink-resolving
    // here would break that symmetry and we'd fail to find the session dir.
    let old_abs = absolutize_lexical(&args.old_arg).with_context(|| "resolve old path")?;
    let new_abs = absolutize_lexical(&args.new_arg).with_context(|| "resolve new path")?;
    if old_abs == new_abs {
        bail!("old and new paths are the same: {}", old_abs.display());
    }

    let old_sess_dir = projects.join(encode_project_dir(&old_abs.to_string_lossy()));
    let new_sess_dir = projects.join(encode_project_dir(&new_abs.to_string_lossy()));

    let info = fs::metadata(&old_sess_dir).with_context(|| {
        format!(
            "no sessions dir for {}\n  expected: {}",
            old_abs.display(),
            old_sess_dir.display()
        )
    })?;
    if !info.is_dir() {
        bail!("not a directory: {}", old_sess_dir.display());
    }
    // On case-insensitive filesystems (default APFS) a case-only rename makes
    // `exists()` see the source itself — that's a legitimate rename, not a
    // conflict with another project.
    if new_sess_dir.exists() && !same_dir_entry(&old_sess_dir, &new_sess_dir) {
        bail!(
            "target sessions dir already exists: {}",
            new_sess_dir.display()
        );
    }
    if args.move_dir && new_abs.exists() && !same_dir_entry(&old_abs, &new_abs) {
        bail!("target project dir already exists: {}", new_abs.display());
    }

    let (referencing_files, total_matches) =
        scan_files_for_path(&old_sess_dir, old_abs.to_string_lossy().as_bytes())?;

    println!("{BOLD}Rename project sessions:{RESET}");
    println!("  from: {}", old_abs.display());
    println!("  to:   {}", new_abs.display());
    println!(
        "  sessions: {} → {}",
        short_project(&old_sess_dir.to_string_lossy()),
        short_project(&new_sess_dir.to_string_lossy())
    );
    println!(
        "  files referencing old cwd: {} ({} total replacements)",
        referencing_files.len(),
        total_matches
    );
    if args.move_dir {
        println!(
            "  project dir: {} → {} {DIM}(--move-dir){RESET}",
            old_abs.display(),
            new_abs.display()
        );
    }

    if args.dry_run {
        println!("\n{DIM}(dry-run — no changes made){RESET}");
        return Ok(());
    }

    if args.move_dir {
        fs::rename(&old_abs, &new_abs).with_context(|| "move project dir")?;
    }
    if let Err(err) = fs::rename(&old_sess_dir, &new_sess_dir) {
        if args.move_dir {
            fs::rename(&new_abs, &old_abs).ok();
        }
        return Err(err.into());
    }

    let mut failed: Vec<String> = Vec::new();
    let old_bytes = old_abs.to_string_lossy().as_bytes().to_vec();
    let new_bytes = new_abs.to_string_lossy().as_bytes().to_vec();
    for old_file in &referencing_files {
        let rel = match old_file.strip_prefix(&old_sess_dir) {
            Ok(r) => r.to_path_buf(),
            Err(err) => {
                failed.push(format!("{}: rel: {err}", old_file.display()));
                continue;
            }
        };
        let new_file = new_sess_dir.join(rel);
        if let Err(err) = replace_in_file(&new_file, &old_bytes, &new_bytes) {
            failed.push(format!("{}: {err}", new_file.display()));
        }
    }

    println!("\n{GREEN}Done.{RESET} Index will refresh on next search.");
    if !failed.is_empty() {
        println!("\n{RED}Errors:{RESET}");
        for e in &failed {
            println!("  {e}");
        }
        return Err(anyhow!("{} file(s) failed", failed.len()));
    }
    Ok(())
}

/// True when both paths refer to the same directory entry (case-only rename
/// on a case-insensitive filesystem).
fn same_dir_entry(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Scan every file under `dir` (jsonl logs AND tool-results etc., matching
/// what `cct transfer` rewrites) for path references.
pub fn scan_files_for_path(dir: &Path, needle: &[u8]) -> Result<(Vec<PathBuf>, usize)> {
    let mut files = Vec::new();
    let mut total = 0;
    walk_dir(dir, &mut |p| {
        if let Ok(data) = fs::read(p) {
            let count = count_overlapping(&data, needle);
            if count > 0 {
                files.push(p.to_path_buf());
                total += count;
            }
        }
    })?;
    Ok((files, total))
}

fn walk_dir(dir: &Path, visit: &mut dyn FnMut(&Path)) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            walk_dir(&path, visit)?;
        } else {
            visit(&path);
        }
    }
    Ok(())
}

/// True when the byte can continue a path segment name. A path reference
/// match is only valid when the next byte is NOT one of these — otherwise
/// `/Temp/foo` would also match inside `/Temp/foo2` or `/Temp/foo.bak`.
/// A following `/` is fine: that's a reference to a path inside the project.
fn extends_path_segment(b: u8) -> bool {
    b.is_ascii_alphanumeric()
        || matches!(b, b'-' | b'_' | b'.' | b'~' | b'+' | b'@' | b'#' | b'%' | b'=')
        || b >= 0x80
}

fn is_path_ref_at(haystack: &[u8], i: usize, needle: &[u8]) -> bool {
    if i + needle.len() > haystack.len() || &haystack[i..i + needle.len()] != needle {
        return false;
    }
    match haystack.get(i + needle.len()) {
        Some(&next) => !extends_path_segment(next),
        None => true,
    }
}

pub fn count_overlapping(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    let mut count = 0;
    let mut i = 0;
    while i + needle.len() <= haystack.len() {
        if is_path_ref_at(haystack, i, needle) {
            count += 1;
            i += needle.len();
        } else {
            i += 1;
        }
    }
    count
}

pub fn replace_in_file(path: &Path, old: &[u8], new: &[u8]) -> Result<()> {
    let data = fs::read(path)?;
    if count_overlapping(&data, old) == 0 {
        return Ok(());
    }
    let updated = replace_all(&data, old, new);
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, updated)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn replace_all(data: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if is_path_ref_at(data, i, old) {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}
