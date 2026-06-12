use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, Result};

/// Convert an absolute filesystem path to the form Claude Code uses for
/// `~/.claude/projects/<dir>/` — slashes become dashes.
pub fn encode_project_dir(abs_path: &str) -> String {
    abs_path.replace('/', "-")
}

/// Decode `~/.claude/projects/<encoded>` back to a short, home-aware path.
/// Encoding replaces `/` with `-`, but directory names may also contain `-`,
/// so we resolve ambiguity by checking which paths actually exist.
pub fn decode_project(encoded: &str) -> String {
    let parts: Vec<&str> = {
        let mut parts: Vec<&str> = encoded.split('-').collect();
        if matches!(parts.first(), Some(&"")) {
            parts.remove(0);
        }
        parts
    };
    if parts.is_empty() {
        return "?".to_string();
    }
    if let Some(resolved) = resolve_path_parts(&parts, Path::new("/")) {
        return short_project(&resolved.to_string_lossy());
    }
    short_project(&encoded.replace('-', "/"))
}

fn resolve_path_parts(parts: &[&str], prefix: &Path) -> Option<PathBuf> {
    if parts.is_empty() {
        return prefix.metadata().ok().map(|_| prefix.to_path_buf());
    }
    for end in (1..=parts.len()).rev() {
        let segment = parts[..end].join("-");
        let candidate = prefix.join(&segment);
        let Ok(meta) = candidate.metadata() else {
            continue;
        };
        if parts[end..].is_empty() {
            return Some(candidate);
        }
        if meta.is_dir() {
            if let Some(resolved) = resolve_path_parts(&parts[end..], &candidate) {
                return Some(resolved);
            }
        }
    }
    None
}

pub fn short_project(project: &str) -> String {
    if project.is_empty() {
        return "?".to_string();
    }
    if let Some(home) = dirs::home_dir() {
        let home_str = home.to_string_lossy();
        if project == home_str {
            return "~".to_string();
        }
        let prefix = format!("{home_str}/");
        if let Some(rest) = project.strip_prefix(&prefix) {
            return format!("~/{rest}");
        }
    }
    project.to_string()
}

pub fn projects_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".claude").join("projects"))
}

/// Convert a user-supplied path into an absolute, lexically normalized path.
///
/// This intentionally does not call `canonicalize`: Claude Code session dirs are
/// encoded from the cwd string Claude recorded, and resolving symlinks on macOS
/// can change `/var/...` into `/private/var/...`.
pub fn absolutize_lexical(path: &str) -> Result<PathBuf> {
    let expanded = expand_tilde(path);
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };
    Ok(clean_path_lexical(&abs))
}

/// Expand a leading `~` or `~/` to the home directory (e.g. a quoted `"~/foo"`
/// the shell never expanded). Other forms — including `~user` — are returned
/// unchanged.
fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    } else if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

pub fn clean_path_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !path.is_absolute() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

pub fn index_root() -> Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no home directory"))?;
    Ok(home.join(".claude").join("search-index"))
}

/// Parse a Claude Code project file path and return `(session, project, source)`.
/// `source` is one of `"session"`, `"subagent"`, `"tool-result"`, or `"file"`.
///
/// Recognises both layouts:
///   `<projects>/<encoded>/<session>.jsonl`
///   `<projects>/<encoded>/<session_id>/{subagents|tool-results}/<file>`
pub fn parse_file_path(path: &Path) -> (String, String, String) {
    let parent = path.parent();
    let parent_name = parent
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("");

    if parent_name == "subagents" || parent_name == "tool-results" {
        let source = if parent_name == "subagents" {
            "subagent"
        } else {
            "tool-result"
        };
        let grandparent = parent.and_then(|p| p.parent());
        let session = grandparent
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let encoded = grandparent
            .and_then(|p| p.parent())
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .unwrap_or("");
        return (session, decode_project(encoded), source.to_string());
    }

    if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
        let session = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let encoded = parent_name;
        return (session, decode_project(encoded), "session".to_string());
    }

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    (stem, "?".to_string(), "file".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_lexical_removes_dot_and_parent_without_symlink_resolution() {
        assert_eq!(
            clean_path_lexical(Path::new("/tmp/cct/./old/../new")),
            PathBuf::from("/tmp/cct/new")
        );
        assert_eq!(
            clean_path_lexical(Path::new("/var/tmp/..")),
            PathBuf::from("/var")
        );
    }

    #[test]
    fn absolutize_lexical_expands_leading_tilde() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(absolutize_lexical("~").unwrap(), clean_path_lexical(&home));
        assert_eq!(
            absolutize_lexical("~/Temp/x").unwrap(),
            clean_path_lexical(&home.join("Temp/x"))
        );
        // `~` only expands at the start; mid-path tilde stays literal.
        assert_eq!(
            absolutize_lexical("/abs/~/x").unwrap(),
            PathBuf::from("/abs/~/x")
        );
    }
}
