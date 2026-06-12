//! `cct transfer` — transfer specific sessions between projects.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::path_codec::{
    absolutize_lexical, clean_path_lexical, encode_project_dir, projects_dir, short_project,
};
use crate::picker::{confirm, run_multi_picker, PickerItem};
use crate::recent::{first_user_prompt, session_activity_time_ms, session_title};
use crate::rename::{count_overlapping, replace_in_file};
use crate::term::{fmt_size, BOLD, CYAN, DIM, GREEN, RED, RESET};

pub struct TransferArgs {
    pub target: String,
    pub from: Option<String>,
    pub session_args: Vec<String>,
    pub dry_run: bool,
    pub skip_confirm: bool,
}

pub fn run_transfer(args: TransferArgs) -> Result<()> {
    let src_str = args.from.unwrap_or_else(|| ".".to_string());
    let src_abs = absolutize_lexical(&src_str)?;
    let dst_abs = absolutize_lexical(&args.target)?;

    let mut ids: Vec<String> = Vec::new();
    for raw in &args.session_args {
        for p in raw.split(',') {
            let trimmed = p.trim();
            if !trimmed.is_empty() {
                ids.push(trimmed.to_string());
            }
        }
    }

    let mut skip_confirm = args.skip_confirm;
    if ids.is_empty() {
        let sessions = list_project_sessions(&src_abs)
            .with_context(|| format!("list sessions in {}", src_abs.display()))?;
        ids = pick_sessions_interactive(&sessions, &src_abs)?;
        skip_confirm = true;
    }

    if !args.dry_run && !skip_confirm {
        let prompt = format!(
            "Transfer {} session(s) to {}? [y/N]: ",
            ids.len(),
            dst_abs.display()
        );
        if !confirm(&prompt) {
            println!("{DIM}aborted{RESET}");
            return Ok(());
        }
    }

    transfer_sessions(&ids, &src_abs, &dst_abs, args.dry_run)
}

#[derive(Clone, Debug)]
pub struct SessionEntry {
    pub id: String,
    pub path: PathBuf,
    pub mtime_ms: i64,
    pub size: u64,
    pub title: Option<String>,
}

pub fn list_project_sessions(proj_abs: &Path) -> Result<Vec<SessionEntry>> {
    let dir = projects_dir()?.join(encode_project_dir(&proj_abs.to_string_lossy()));
    let mut out = Vec::new();
    let entries = fs::read_dir(&dir).with_context(|| format!("read dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let id = p
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let mtime_ms = session_activity_time_ms(&p);
        let title = session_title(&p);
        out.push(SessionEntry {
            id,
            path: p,
            mtime_ms,
            size,
            title,
        });
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.mtime_ms));
    Ok(out)
}

fn pick_sessions_interactive(sessions: &[SessionEntry], src_abs: &Path) -> Result<Vec<String>> {
    if sessions.is_empty() {
        return Err(anyhow!("no sessions in {}", src_abs.display()));
    }

    let mut items: Vec<PickerItem> = Vec::new();
    for s in sessions {
        let first = first_user_prompt(&s.path).unwrap_or_default();
        let when = chrono::DateTime::from_timestamp_millis(s.mtime_ms)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "?".into());
        let id_short = s.id.chars().take(8).collect::<String>();
        let stats = format!("  {}", fmt_size(s.size));
        let name = match &s.title {
            Some(t) if !t.is_empty() => format!("  {GREEN}{BOLD}{t}{RESET}"),
            _ => String::new(),
        };
        let line1 =
            format!("{CYAN}{id_short}{RESET}{name}  {DIM}{when}{RESET}  {DIM}{stats}{RESET}");
        let line2 = if first.is_empty() {
            String::new()
        } else {
            let mut s = String::from("> ");
            for ch in first.chars().take(200) {
                s.push(ch);
            }
            s
        };
        let haystack = format!(
            "{} {} {} {}",
            s.id,
            s.title.as_deref().unwrap_or(""),
            when,
            first
        )
        .to_lowercase();
        items.push(PickerItem {
            id: s.id.clone(),
            line1,
            line2,
            haystack,
        });
    }
    let title = format!("Sessions in {}", short_project(&src_abs.to_string_lossy()));
    run_multi_picker(&items, &title)
}

fn resolve_session_id(src_dir: &Path, q: &str) -> Result<String> {
    let exact = src_dir.join(format!("{q}.jsonl"));
    if exact.exists() {
        return Ok(q.to_string());
    }

    // 1) Match by session id prefix — cheap, only inspects file names.
    let mut matches: Vec<String> = Vec::new();
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(stem) = name.strip_suffix(".jsonl") else {
            continue;
        };
        if stem.starts_with(q) {
            matches.push(stem.to_string());
        }
    }
    if matches.len() == 1 {
        return Ok(matches.remove(0));
    }
    if matches.len() > 1 {
        bail!("id prefix {:?} is ambiguous ({} matches)", q, matches.len());
    }

    // 2) Fall back to an exact session-title (`/rename`) match — reads files,
    //    so only done when no id matched.
    let mut by_title: Vec<String> = Vec::new();
    for entry in fs::read_dir(src_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if session_title(&p).as_deref() == Some(q) {
            if let Some(stem) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) {
                by_title.push(stem);
            }
        }
    }
    match by_title.len() {
        0 => Err(anyhow!(
            "no session matches {:?} in {}",
            q,
            src_dir.display()
        )),
        1 => Ok(by_title.remove(0)),
        n => Err(anyhow!("title {:?} is ambiguous ({n} matches)", q)),
    }
}

struct PendingTransfer {
    id: String,
    src_jsonl: PathBuf,
    dst_jsonl: PathBuf,
    src_subdir: PathBuf,
    dst_subdir: PathBuf,
    has_subdir: bool,
    replacements: usize,
}

fn transfer_sessions(ids: &[String], src_abs: &Path, dst_abs: &Path, dry_run: bool) -> Result<()> {
    let projects = projects_dir()?;
    transfer_sessions_in(ids, src_abs, dst_abs, dry_run, &projects)
}

/// Test-friendly variant: caller supplies the projects directory so tests can
/// exercise transfer/dry-run/cwd-rewrite logic in a tempdir.
pub fn transfer_sessions_in(
    ids: &[String],
    src_abs: &Path,
    dst_abs: &Path,
    dry_run: bool,
    projects: &Path,
) -> Result<()> {
    let src_abs = clean_path_lexical(src_abs);
    let dst_abs = clean_path_lexical(dst_abs);
    if src_abs == dst_abs {
        bail!("source and target are the same project");
    }
    if ids.is_empty() {
        bail!("no sessions selected");
    }
    let src_dir = projects.join(encode_project_dir(&src_abs.to_string_lossy()));
    let dst_dir = projects.join(encode_project_dir(&dst_abs.to_string_lossy()));

    if !src_dir.exists() {
        bail!("source sessions dir not found: {}", src_dir.display());
    }

    let src_bytes = src_abs.to_string_lossy().as_bytes().to_vec();
    let dst_bytes = dst_abs.to_string_lossy().as_bytes().to_vec();

    let mut pending: Vec<PendingTransfer> = Vec::new();
    for raw in ids {
        let id = resolve_session_id(&src_dir, raw)?;
        let src_jsonl = src_dir.join(format!("{id}.jsonl"));
        let dst_jsonl = dst_dir.join(format!("{id}.jsonl"));
        if dst_jsonl.exists() {
            bail!(
                "session {id} already exists in target: {}",
                dst_jsonl.display()
            );
        }
        let src_subdir = src_dir.join(&id);
        let dst_subdir = dst_dir.join(&id);
        let has_subdir = src_subdir.is_dir();
        if has_subdir && dst_subdir.exists() {
            bail!(
                "subdir for {id} already exists in target: {}",
                dst_subdir.display()
            );
        }

        let mut replacements = count_in_file(&src_jsonl, &src_bytes);
        if has_subdir {
            walk_files(&src_subdir, &mut |p| {
                replacements += count_in_file(p, &src_bytes);
            })?;
        }

        pending.push(PendingTransfer {
            id,
            src_jsonl,
            dst_jsonl,
            src_subdir,
            dst_subdir,
            has_subdir,
            replacements,
        });
    }

    println!("{BOLD}Transfer {} session(s):{RESET}", pending.len());
    println!("  from: {}", src_abs.display());
    println!("  to:   {}", dst_abs.display());
    for p in &pending {
        let sub = if p.has_subdir { " + subdir" } else { "" };
        println!(
            "  - {}  {DIM}({} cwd refs{sub}){RESET}",
            p.id, p.replacements
        );
    }
    if dry_run {
        println!("\n{DIM}(dry-run — no changes made){RESET}");
        return Ok(());
    }

    fs::create_dir_all(&dst_dir).with_context(|| "mkdir target")?;

    let mut moved: Vec<PendingTransfer> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for p in pending {
        if let Err(err) = fs::rename(&p.src_jsonl, &p.dst_jsonl) {
            failed.push(format!("{}: rename jsonl: {err}", p.id));
            continue;
        }
        if p.has_subdir {
            if let Err(err) = fs::rename(&p.src_subdir, &p.dst_subdir) {
                fs::rename(&p.dst_jsonl, &p.src_jsonl).ok();
                failed.push(format!("{}: rename subdir: {err}", p.id));
                continue;
            }
        }
        moved.push(p);
    }

    for p in &moved {
        if let Err(err) = replace_in_file(&p.dst_jsonl, &src_bytes, &dst_bytes) {
            failed.push(format!("{} jsonl: {err}", p.id));
        }
        if p.has_subdir {
            walk_files(&p.dst_subdir, &mut |fp| {
                if let Err(err) = replace_in_file(fp, &src_bytes, &dst_bytes) {
                    failed.push(format!("{}/{}: {err}", p.id, fp.display()));
                }
            })?;
        }
    }

    if let Ok(empty) = fs::read_dir(&src_dir) {
        if empty.count() == 0 {
            fs::remove_dir(&src_dir).ok();
        }
    }

    println!(
        "\n{GREEN}Done.{RESET} Transferred {}/{} session(s).",
        moved.len(),
        moved.len() + failed.len()
    );
    if !failed.is_empty() {
        println!("\n{RED}Errors:{RESET}");
        for e in &failed {
            println!("  {e}");
        }
        return Err(anyhow!("{} failure(s)", failed.len()));
    }
    Ok(())
}

fn count_in_file(path: &Path, needle: &[u8]) -> usize {
    fs::read(path)
        .map(|data| count_overlapping(&data, needle))
        .unwrap_or(0)
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
