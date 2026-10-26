//! Discovery of Claude Code project files: sessions, subagent logs, text-like tool results.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, bail, Context, Result};

/// What kind of source produced the file. Used for classification only;
/// the index treats them uniformly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FileKind {
    SessionJsonl,
    SubagentJsonl,
    ToolResult,
}

impl FileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FileKind::SessionJsonl => "session_jsonl",
            FileKind::SubagentJsonl => "subagent_jsonl",
            FileKind::ToolResult => "tool_result",
        }
    }
}

/// How far into a session log to look for its entrypoint. It is on the first
/// user or assistant record; before that come only a few small ones and, for
/// a headless session, the queued prompt.
const ENTRYPOINT_SCAN_BYTES: u64 = 4 * 1024 * 1024;

/// The session log a file belongs to: itself, or `<project>/<session>.jsonl`
/// for a subagent log or tool result under `<project>/<session>/…/`.
pub fn owning_session_log(path: &Path) -> Option<PathBuf> {
    let parent = path.parent()?;
    match parent.file_name().and_then(|n| n.to_str()) {
        Some("subagents" | "tool-results") => {
            let session_dir = parent.parent()?;
            let id = session_dir.file_name()?.to_string_lossy().into_owned();
            Some(session_dir.parent()?.join(format!("{id}.jsonl")))
        }
        _ => Some(path.to_path_buf()),
    }
}

/// True for a file of a headless session (`claude -p`, the Agent SDK) — see
/// [`crate::text::headless_entrypoint`]. Subagent logs and tool results take
/// it from their session. `data` is the file's content when the caller already
/// has it; otherwise the session log is read up to its first entrypoint, from
/// the live file or the archive. Cached per session: a full reindex asks once
/// for every subagent log and tool result. A log with no entrypoint yet (just
/// created) counts as interactive and is not cached.
pub fn is_headless(path: &Path, data: Option<&[u8]>) -> bool {
    let Some(log) = owning_session_log(path) else {
        return false;
    };
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(known) = cache.lock().ok().and_then(|c| c.get(&log).copied()) {
        return known;
    }
    let found = match data.filter(|_| log == path) {
        Some(data) => data
            .split(|b| *b == b'\n')
            .find_map(crate::text::headless_entrypoint),
        None => scan_entrypoint(&log),
    };
    if let (Some(headless), Ok(mut c)) = (found, cache.lock()) {
        c.insert(log, headless);
    }
    found.unwrap_or(false)
}

fn scan_entrypoint(log: &Path) -> Option<bool> {
    let reader: Box<dyn Read> = match fs::File::open(log) {
        Ok(f) => Box::new(f),
        Err(_) => {
            let zst = fs::File::open(crate::archive::archive_path_for(log)?).ok()?;
            Box::new(zstd::stream::read::Decoder::new(zst).ok()?)
        }
    };
    let mut reader = BufReader::new(reader.take(ENTRYPOINT_SCAN_BYTES));
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line).ok()? == 0 {
            return None;
        }
        if let Some(headless) = crate::text::headless_entrypoint(&line) {
            return Some(headless);
        }
    }
}

/// The session file whose id is `q` or starts with it, in any project.
/// `None` when nothing matches; an error when a prefix fits several.
pub fn find_session_log(projects: &Path, q: &str) -> Result<Option<PathBuf>> {
    // An id never holds a path separator; refusing them keeps `../x` from
    // reaching outside the projects dir.
    if q.is_empty() || q.contains(['/', '\\']) || q == "." || q == ".." {
        return Ok(None);
    }
    let mut exact: Vec<PathBuf> = Vec::new();
    let mut prefixed: Vec<PathBuf> = Vec::new();
    for project in fs::read_dir(projects).with_context(|| format!("read dir {}", projects.display()))? {
        let project = project?;
        if !project.file_type()?.is_dir() {
            continue;
        }
        let candidate = project.path().join(format!("{q}.jsonl"));
        if candidate.is_file() {
            exact.push(candidate);
            continue;
        }
        for entry in fs::read_dir(project.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(q) && name.ends_with(".jsonl") && entry.file_type()?.is_file() {
                prefixed.push(entry.path());
            }
        }
    }
    let matches = if exact.is_empty() { prefixed } else { exact };
    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.into_iter().next()),
        n => {
            let mut msg = format!("id prefix {q:?} is ambiguous ({n} matches):");
            for p in matches.iter().take(10) {
                let id = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                msg.push_str(&format!("\n  {id}"));
            }
            bail!(msg)
        }
    }
}

#[derive(Clone, Debug)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: u128,
    /// When set, `path` is a logical live path whose bytes only exist compressed
    /// at this `.zst` source (an archived session); the indexer decompresses it
    /// instead of reading `path`. `None` for ordinary live files.
    pub zst_source: Option<PathBuf>,
}

/// Walk `~/.claude/projects/` in the cct layout:
///   `<project>/<session>.jsonl`
///   `<project>/<session>/subagents/*.jsonl`
///   `<project>/<session>/tool-results/*.{txt,json,jsonl,md,log,csv,tsv,html,htm,xml}`
///
/// Returns files sorted by path for deterministic file_id assignment.
/// Stats inside each project are run in parallel — that's where most of the
/// scan time goes on a 10k+ file corpus.
pub fn collect_cct_layout(projects_dir: &Path) -> Result<Vec<DiscoveredFile>> {
    if !projects_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut project_dirs: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(projects_dir)
        .with_context(|| format!("read dir {}", projects_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            project_dirs.push(entry.path());
        }
    }

    let workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .clamp(1, 8);

    let mut out = if workers == 1 || project_dirs.len() <= 1 {
        let mut buf = Vec::new();
        for dir in &project_dirs {
            collect_project(dir, &mut buf)?;
        }
        buf
    } else {
        let chunk_size = project_dirs.len().div_ceil(workers).max(1);
        std::thread::scope(|scope| -> Result<Vec<DiscoveredFile>> {
            let handles: Vec<_> = project_dirs
                .chunks(chunk_size)
                .map(|chunk| {
                    scope.spawn(move || -> Result<Vec<DiscoveredFile>> {
                        let mut local = Vec::new();
                        for dir in chunk {
                            collect_project(dir, &mut local)?;
                        }
                        Ok(local)
                    })
                })
                .collect();
            let mut all = Vec::new();
            for h in handles {
                let local = h.join().map_err(|_| anyhow!("scan worker panicked"))??;
                all.extend(local);
            }
            Ok(all)
        })?
    };

    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Like `collect_cct_layout` but only walks one project's session directory
/// (e.g. `~/.claude/projects/-Users-ivan-Work-projects-foo`). Used for
/// scope-restricted searches so we don't stat 10k+ files across all projects.
pub fn collect_one_project(project_session_dir: &Path) -> Result<Vec<DiscoveredFile>> {
    let mut out = Vec::new();
    if !project_session_dir.is_dir() {
        return Ok(out);
    }
    collect_project(project_session_dir, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// Walk the zstd archive mirror under `archive_root` — built by `cct archive`,
/// which duplicates the projects layout with `.zst` appended to every file —
/// and map each entry back to the logical live path it stands in for under
/// `projects_root`. A full reindex feeds these alongside live files so a
/// from-scratch rebuild keeps the archive searchable (incremental updates
/// preserve archived docs already in the catalog, but a full reindex starts
/// empty). The returned `path` is the live path; `zst_source` points at the
/// compressed file the indexer decompresses; `size`/`mtime_ms` come from it.
pub fn collect_archived_layout(
    archive_root: &Path,
    projects_root: &Path,
) -> Result<Vec<DiscoveredFile>> {
    let mut out = Vec::new();
    if !archive_root.is_dir() {
        return Ok(out);
    }
    collect_archived_dir(archive_root, archive_root, projects_root, &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn collect_archived_dir(
    dir: &Path,
    archive_root: &Path,
    projects_root: &Path,
    out: &mut Vec<DiscoveredFile>,
) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("read dir {}", dir.display()))? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let path = entry.path();
        if ty.is_dir() {
            collect_archived_dir(&path, archive_root, projects_root, out)?;
        } else if ty.is_file() {
            let Some(live) = archived_live_path(&path, archive_root, projects_root) else {
                continue;
            };
            let Some(kind) = classify_live_path(&live, projects_root) else {
                continue;
            };
            push_archived_file(out, live, kind, path)?;
        }
    }
    Ok(())
}

/// Invert `archive::archive_path_for_roots`: drop the archive root prefix and
/// the trailing `.zst` to recover the live path under `projects_root`.
fn archived_live_path(
    archived: &Path,
    archive_root: &Path,
    projects_root: &Path,
) -> Option<PathBuf> {
    let rel = archived.strip_prefix(archive_root).ok()?;
    let live_rel = rel.to_string_lossy();
    let live_rel = live_rel.strip_suffix(".zst")?;
    Some(projects_root.join(live_rel))
}

/// Classify a live path by its slot in the cct layout, mirroring exactly what
/// `collect_project` / `collect_session_subdirs` accept for live files:
///   `<project>/<id>.jsonl`                    → session
///   `<project>/<id>/subagents/*.jsonl`        → subagent
///   `<project>/<id>/tool-results/*.<textext>` → tool result
fn classify_live_path(live: &Path, projects_root: &Path) -> Option<FileKind> {
    let rel = live.strip_prefix(projects_root).ok()?;
    let comps: Vec<&std::ffi::OsStr> = rel.components().map(|c| c.as_os_str()).collect();
    let ext = live.extension().and_then(|s| s.to_str());
    match comps.len() {
        2 if ext == Some("jsonl") => Some(FileKind::SessionJsonl),
        4 => match comps[2].to_str() {
            Some("subagents") if ext == Some("jsonl") => Some(FileKind::SubagentJsonl),
            Some("tool-results") if is_text_like_tool_result(live) => Some(FileKind::ToolResult),
            _ => None,
        },
        _ => None,
    }
}

fn push_archived_file(
    out: &mut Vec<DiscoveredFile>,
    live: PathBuf,
    kind: FileKind,
    archived: PathBuf,
) -> Result<()> {
    let meta = match fs::metadata(&archived) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", archived.display())),
    };
    out.push(DiscoveredFile {
        path: live,
        kind,
        size: meta.len(),
        mtime_ms: metadata_mtime_ms(&meta),
        zst_source: Some(archived),
    });
    Ok(())
}

fn collect_project(project_path: &Path, out: &mut Vec<DiscoveredFile>) -> Result<()> {
    let entries = fs::read_dir(project_path)
        .with_context(|| format!("read dir {}", project_path.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let ty = entry.file_type()?;
        if ty.is_file() {
            if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                push_file(out, path, FileKind::SessionJsonl)?;
            }
        } else if ty.is_dir() {
            collect_session_subdirs(&path, out)?;
        }
    }
    Ok(())
}

fn collect_session_subdirs(session_dir: &Path, out: &mut Vec<DiscoveredFile>) -> Result<()> {
    let subagents = session_dir.join("subagents");
    if let Some(entries) = read_optional_dir(&subagents)? {
        for entry in entries {
            let entry = entry?;
            let ft = entry.file_type()?;
            if !ft.is_file() {
                continue;
            }
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                push_file(out, p, FileKind::SubagentJsonl)?;
            }
        }
    }

    let tool_results = session_dir.join("tool-results");
    if let Some(entries) = read_optional_dir(&tool_results)? {
        for entry in entries {
            let entry = entry?;
            let ft = entry.file_type()?;
            if !ft.is_file() {
                continue;
            }
            let p = entry.path();
            if is_text_like_tool_result(&p) {
                push_file(out, p, FileKind::ToolResult)?;
            }
        }
    }
    Ok(())
}

fn read_optional_dir(path: &Path) -> Result<Option<fs::ReadDir>> {
    match fs::read_dir(path) {
        Ok(entries) => Ok(Some(entries)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("read dir {}", path.display())),
    }
}

fn push_file(out: &mut Vec<DiscoveredFile>, path: PathBuf, kind: FileKind) -> Result<()> {
    // The file can vanish between read_dir and stat (Claude Code cleaning up
    // old sessions); skip it instead of failing the whole scan.
    let meta = match fs::metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err).with_context(|| format!("stat {}", path.display())),
    };
    out.push(DiscoveredFile {
        path,
        kind,
        size: meta.len(),
        mtime_ms: metadata_mtime_ms(&meta),
        zst_source: None,
    });
    Ok(())
}

fn metadata_mtime_ms(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub fn is_text_like_tool_result(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("txt" | "json" | "jsonl" | "md" | "log" | "csv" | "tsv" | "html" | "htm" | "xml")
    )
}

pub fn is_appendable_jsonl(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("jsonl")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collect_archived_layout_maps_zst_back_to_live_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("archive");
        let projects = tmp.path().join("projects");
        let proj = archive.join("-Users-x-proj");
        fs::create_dir_all(proj.join("abc/subagents")).unwrap();
        fs::create_dir_all(proj.join("abc/tool-results")).unwrap();
        // Content is irrelevant here: discovery only maps paths and stats size.
        fs::write(proj.join("abc.jsonl.zst"), b"z").unwrap();
        fs::write(proj.join("abc/subagents/a1.jsonl.zst"), b"z").unwrap();
        fs::write(proj.join("abc/tool-results/t.txt.zst"), b"z").unwrap();
        // A non-text tool result is skipped, just like on the live side.
        fs::write(proj.join("abc/tool-results/img.png.zst"), b"z").unwrap();

        let found = collect_archived_layout(&archive, &projects).unwrap();
        let mut mapped: Vec<(String, &str, bool)> = found
            .iter()
            .map(|f| {
                (
                    f.path
                        .strip_prefix(&projects)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    f.kind.as_str(),
                    f.zst_source.is_some(),
                )
            })
            .collect();
        mapped.sort();
        assert_eq!(
            mapped,
            vec![
                ("-Users-x-proj/abc.jsonl".to_string(), "session_jsonl", true),
                (
                    "-Users-x-proj/abc/subagents/a1.jsonl".to_string(),
                    "subagent_jsonl",
                    true
                ),
                (
                    "-Users-x-proj/abc/tool-results/t.txt".to_string(),
                    "tool_result",
                    true
                ),
            ]
        );
    }

    #[test]
    fn collect_archived_layout_absent_root_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let found =
            collect_archived_layout(&tmp.path().join("nope"), &tmp.path().join("projects")).unwrap();
        assert!(found.is_empty());
    }
}
