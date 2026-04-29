//! Discovery of Claude Code project files: sessions, subagent logs, text-like tool results.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};

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
pub fn collect_cct_layout(projects_dir: &Path) -> Result<Vec<DiscoveredFile>> {
    let mut out = Vec::new();
    if !projects_dir.is_dir() {
        return Ok(out);
    }

    for project_entry in fs::read_dir(projects_dir)
        .with_context(|| format!("read dir {}", projects_dir.display()))?
    {
        let project_entry = project_entry?;
        if !project_entry.file_type()?.is_dir() {
            continue;
        }
        let project_path = project_entry.path();
        for entry in fs::read_dir(&project_path)
            .with_context(|| format!("read dir {}", project_path.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let ty = entry.file_type()?;
            if ty.is_file() {
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    push_file(&mut out, path, FileKind::SessionJsonl)?;
                }
                continue;
            }
            if !ty.is_dir() {
                continue;
            }

            collect_session_subdirs(&path, &mut out)?;
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn collect_session_subdirs(session_dir: &Path, out: &mut Vec<DiscoveredFile>) -> Result<()> {
    let subagents = session_dir.join("subagents");
    if let Ok(entries) = fs::read_dir(&subagents) {
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                push_file(out, p, FileKind::SubagentJsonl)?;
            }
        }
    }

    let tool_results = session_dir.join("tool-results");
    if let Ok(entries) = fs::read_dir(&tool_results) {
        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
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
