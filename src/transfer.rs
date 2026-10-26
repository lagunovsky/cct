//! `cct transfer` — transfer specific sessions between projects.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::fs_scan::find_session_log;
use crate::index::catalog::read_catalog;
use crate::path_codec::{
    absolutize_lexical, clean_path_lexical, encode_project_dir, index_root, projects_dir,
    short_project,
};
use crate::picker::{confirm, run_multi_picker, PickerItem};
use crate::recent::{first_user_prompt, session_activity_time_ms, session_title};
use crate::rename::{count_overlapping, replace_in_file};
use crate::term::{fmt_size, BOLD, CYAN, DIM, GREEN, RED, RESET};

pub struct TransferArgs {
    pub from: Option<String>,
    /// Sessions, then the target project last.
    pub args: Vec<String>,
    pub dry_run: bool,
    pub skip_confirm: bool,
}

pub fn run_transfer(args: TransferArgs) -> Result<()> {
    let (target, session_args) = args
        .args
        .split_last()
        .ok_or_else(|| anyhow!("missing target project"))?;
    let dst_abs = absolutize_lexical(target)?;
    let from = args.from.as_deref().map(absolutize_lexical).transpose()?;
    let projects = projects_dir()?;

    // Keep arguments whole here: a session title set via /rename may itself
    // contain commas. Comma-splitting happens during resolution, only when
    // the whole argument doesn't match anything.
    let ids: Vec<String> = session_args
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if ids.is_empty() {
        // `cct transfer abc123` with the target forgotten would otherwise
        // offer to move cwd's sessions into a new project named abc123.
        if !dst_abs.exists() && find_session_log(&projects, target)?.is_some() {
            bail!("{target:?} is a session, not a project — usage: cct transfer <session>... <target>");
        }
        let src_abs = match from {
            Some(src) => src,
            None => absolutize_lexical(".")?,
        };
        let sessions = list_project_sessions(&src_abs)
            .with_context(|| format!("list sessions in {}", src_abs.display()))?;
        let ids = pick_sessions_interactive(&sessions, &src_abs)?;
        return transfer_sessions(&ids, &src_abs, &dst_abs, args.dry_run);
    }

    // With --from everything resolves inside that project, as titles may need.
    let groups: BTreeMap<PathBuf, Vec<String>> = match from {
        Some(src) => BTreeMap::from([(src, ids)]),
        None => {
            let cwd = absolutize_lexical(".")?;
            let titled = |title: &str| catalog_paths_titled(title);
            let mut groups: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
            for raw in &ids {
                for found in locate_session_arg(&projects, Some(&cwd), raw, &titled)? {
                    groups.entry(found.project).or_default().push(found.id);
                }
            }
            groups
        }
    };

    if !args.dry_run && !args.skip_confirm {
        let total: usize = groups.values().map(Vec::len).sum();
        for (src, ids) in &groups {
            println!("  {DIM}from{RESET} {}: {}", short_project(&src.to_string_lossy()), ids.join(", "));
        }
        let prompt = format!("Transfer {total} session(s) to {}? [y/N]: ", dst_abs.display());
        if !confirm(&prompt) {
            println!("{DIM}aborted{RESET}");
            return Ok(());
        }
    }

    for (src_abs, ids) in &groups {
        transfer_sessions_in(ids, src_abs, &dst_abs, args.dry_run, &projects)?;
    }
    Ok(())
}

/// A session an argument named: the project it belongs to and its id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocatedSession {
    pub project: PathBuf,
    pub id: String,
}

/// [`locate_session_in`] for one CLI argument: tried whole first (a title may
/// contain commas), then as a comma-separated list.
pub fn locate_session_arg(
    projects: &Path,
    scope: Option<&Path>,
    raw: &str,
    titled: &dyn Fn(&str) -> Vec<PathBuf>,
) -> Result<Vec<LocatedSession>> {
    match locate_session_in(projects, scope, raw, titled) {
        Ok(found) => Ok(vec![found]),
        Err(whole_err) => {
            let parts: Vec<&str> = raw.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
            if parts.len() < 2 {
                return Err(whole_err);
            }
            parts
                .into_iter()
                .map(|part| locate_session_in(projects, scope, part, titled))
                .collect()
        }
    }
}

/// Find the session `q` names without being told its project. An id or a
/// unique id prefix is looked up in every project — it names one session
/// wherever it lives. A `/rename` title is not unique: it is looked up in
/// `scope` (cwd) first, then everywhere via `titled`, and a title shared by
/// sessions in several projects is an error that asks for `--from`.
pub fn locate_session_in(
    projects: &Path,
    scope: Option<&Path>,
    q: &str,
    titled: &dyn Fn(&str) -> Vec<PathBuf>,
) -> Result<LocatedSession> {
    if let Some(jsonl) = find_session_log(projects, q)? {
        return located(&jsonl);
    }
    if let Some(scope) = scope {
        let dir = projects.join(encode_project_dir(&scope.to_string_lossy()));
        if dir.is_dir() {
            if let Ok(id) = resolve_session_id(&dir, q) {
                return Ok(LocatedSession {
                    project: scope.to_path_buf(),
                    id,
                });
            }
        }
    }
    // The catalog may lag behind a later /rename; the file has the last word.
    let mut by_title: Vec<PathBuf> = titled(q)
        .into_iter()
        .filter(|p| p.parent().and_then(Path::parent) == Some(projects))
        .filter(|p| session_title(p).as_deref() == Some(q))
        .collect();
    by_title.sort();
    by_title.dedup();
    match by_title.len() {
        0 => bail!("no session matches {q:?}"),
        1 => located(&by_title[0]),
        n => {
            let mut msg = format!("title {q:?} is ambiguous ({n} sessions) — pass --from <project>:");
            for p in &by_title {
                let id = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                let project = project_of(p)
                    .map(|pr| short_project(&pr.to_string_lossy()))
                    .unwrap_or_else(|_| "?".into());
                msg.push_str(&format!("\n  {id}  {project}"));
            }
            bail!(msg)
        }
    }
}

fn located(jsonl: &Path) -> Result<LocatedSession> {
    let id = jsonl
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(LocatedSession {
        project: project_of(jsonl)?,
        id,
    })
}

/// The project a session file belongs to. Its directory name is a lossy
/// encoding of the project path, so the path comes from the session's own
/// `cwd` records: the first whose encoding is that directory name. (Later
/// records may carry a subdirectory the agent moved into.)
fn project_of(jsonl: &Path) -> Result<PathBuf> {
    let dir_name = jsonl
        .parent()
        .and_then(Path::file_name)
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let file = fs::File::open(jsonl).with_context(|| format!("open {}", jsonl.display()))?;
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if !line.contains("\"cwd\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(cwd) = value.get("cwd").and_then(|c| c.as_str()) {
            if encode_project_dir(cwd) == dir_name {
                return Ok(PathBuf::from(cwd));
            }
        }
    }
    bail!(
        "cannot tell which project {} belongs to — pass --from <project>",
        jsonl.display()
    )
}

/// Session files the search index's catalog lists under `title`. Empty when
/// there is no index: the title is then found only in cwd's project.
fn catalog_paths_titled(title: &str) -> Vec<PathBuf> {
    let Ok(index_dir) = index_root() else {
        return Vec::new();
    };
    read_catalog(&index_dir)
        .map(|entries| {
            entries
                .into_iter()
                .filter(|e| e.title.as_deref() == Some(title))
                .map(|e| e.path)
                .collect()
        })
        .unwrap_or_default()
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
    // Only path-safe queries may hit the filesystem directly: an id like
    // `../other/abc` would otherwise resolve to a file outside `src_dir` and
    // later land the transferred session outside the target dir too.
    if !q.contains(['/', '\\']) && q != "." && q != ".." {
        let exact = src_dir.join(format!("{q}.jsonl"));
        if exact.exists() {
            return Ok(q.to_string());
        }
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
        n => Err(anyhow!(
            "title {q:?} is ambiguous ({n} matches) — use an id:\n  {}",
            by_title.join("\n  ")
        )),
    }
}

/// Resolve raw CLI arguments to session ids. Each argument is tried whole
/// first (it may be a title containing commas); only when nothing matches is
/// it treated as a comma-separated list of ids/prefixes/titles.
fn resolve_session_args(src_dir: &Path, ids: &[String]) -> Result<Vec<String>> {
    let mut resolved = Vec::new();
    for raw in ids {
        match resolve_session_id(src_dir, raw) {
            Ok(id) => resolved.push(id),
            Err(whole_err) => {
                let parts: Vec<&str> = raw
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                if parts.len() < 2 {
                    return Err(whole_err);
                }
                for part in parts {
                    resolved.push(resolve_session_id(src_dir, part)?);
                }
            }
        }
    }
    Ok(resolved)
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
    let mut seen_ids = std::collections::BTreeSet::new();
    for id in resolve_session_args(&src_dir, ids)? {
        // Two prefixes (or a prefix and a title) can resolve to the same
        // session; transferring it once is success, not an error.
        if !seen_ids.insert(id.clone()) {
            continue;
        }
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
            // One unreadable subdir must not abort the rewrite of the other
            // already-moved sessions (their files would keep the old cwd).
            let walked = walk_files(&p.dst_subdir, &mut |fp| {
                if let Err(err) = replace_in_file(fp, &src_bytes, &dst_bytes) {
                    failed.push(format!("{}/{}: {err}", p.id, fp.display()));
                }
            });
            if let Err(err) = walked {
                failed.push(format!("{}: walk subdir: {err}", p.id));
            }
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
