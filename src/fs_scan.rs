//! Discovery of Claude Code project files: sessions, subagent logs, text-like tool results.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};

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

#[derive(Clone, Debug)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub kind: FileKind,
    pub size: u64,
    pub mtime_ms: u128,
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
    let meta = fs::metadata(&path).with_context(|| format!("stat {}", path.display()))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    out.push(DiscoveredFile {
        path,
        kind,
        size: meta.len(),
        mtime_ms,
    });
    Ok(())
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
