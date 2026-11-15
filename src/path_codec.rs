use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{anyhow, Result};

const ENCODED_MAX_LEN: usize = 200;

/// Convert an absolute filesystem path to the form Claude Code uses for
/// `~/.claude/projects/<dir>/`: every UTF-16 code unit outside `[a-zA-Z0-9]`
/// becomes a dash, and results longer than 200 chars are truncated with a
/// base36 hash of the raw path appended.
pub fn encode_project_dir(abs_path: &str) -> String {
    let mut encoded = String::new();
    for unit in abs_path.encode_utf16() {
        let kept = char::from_u32(unit as u32).filter(char::is_ascii_alphanumeric);
        encoded.push(kept.unwrap_or('-'));
    }
    if encoded.len() <= ENCODED_MAX_LEN {
        return encoded;
    }
    let hash = base36(js_string_hash(abs_path).unsigned_abs() as u64);
    format!("{}-{}", &encoded[..ENCODED_MAX_LEN], hash)
}

/// Java-style string hash over UTF-16 code units with i32 wrapping —
/// mirrors Claude Code's `(h << 5) - h + unit | 0`.
fn js_string_hash(s: &str) -> i32 {
    let mut h: i32 = 0;
    for unit in s.encode_utf16() {
        h = h.wrapping_shl(5).wrapping_sub(h).wrapping_add(unit as i32);
    }
    h
}

fn base36(mut n: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if n == 0 {
        return "0".to_string();
    }
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Decode `~/.claude/projects/<encoded>` back to a short, home-aware path.
/// Encoding is lossy (any non-alphanumeric character becomes a dash), so we
/// resolve ambiguity by walking the filesystem and matching directory entries
/// by their encoded form.
pub fn decode_project(encoded: &str) -> String {
    let trimmed = encoded.strip_prefix('-').unwrap_or(encoded);
    if trimmed.is_empty() {
        return "?".to_string();
    }
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(map) = cache.lock() {
        if let Some(hit) = map.get(encoded) {
            return hit.clone();
        }
    }
    let decoded = match resolve_encoded(trimmed, Path::new("/")) {
        Some(resolved) => short_project(&resolved.to_string_lossy()),
        None => short_project(&encoded.replace('-', "/")),
    };
    if let Ok(mut map) = cache.lock() {
        map.insert(encoded.to_string(), decoded.clone());
    }
    decoded
}

/// How many bytes of `rest` this directory entry name accounts for, or None
/// if it doesn't match the start of the encoding. Accepts both the current
/// scheme (non-alphanumeric → '-') and legacy encodings that kept characters
/// like '_' literally.
fn encoded_eats(name: &str, rest: &str) -> Option<usize> {
    let bytes = rest.as_bytes();
    let mut i = 0usize;
    for unit in name.encode_utf16() {
        let b = *bytes.get(i)?;
        let matches = match char::from_u32(unit as u32) {
            Some(c) if c.is_ascii_alphanumeric() => b == c as u8,
            Some(c) if c.is_ascii() => b == b'-' || b == c as u8,
            _ => b == b'-',
        };
        if !matches {
            return None;
        }
        i += 1;
    }
    Some(i)
}

fn resolve_encoded(rest: &str, prefix: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(prefix).ok()?;
    // (bytes of `rest` consumed, entry name); longest-first so the entry
    // covering the most of the encoding wins (`a-b` over `a` + `b`).
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(eaten) = encoded_eats(name, rest) else {
            continue;
        };
        if eaten == rest.len() || rest.as_bytes().get(eaten) == Some(&b'-') {
            candidates.push((eaten, name.to_string()));
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (eaten, name) in candidates {
        let candidate = prefix.join(&name);
        if eaten == rest.len() {
            return Some(candidate);
        }
        if candidate.is_dir() {
            if let Some(resolved) = resolve_encoded(&rest[eaten + 1..], &candidate) {
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
    fn encode_replaces_every_non_alphanumeric_with_dash() {
        assert_eq!(encode_project_dir("/Users/me/.ssh"), "-Users-me--ssh");
        assert_eq!(
            encode_project_dir("/Users/me/Library/Application Support/com.mitchellh.ghostty"),
            "-Users-me-Library-Application-Support-com-mitchellh-ghostty"
        );
        assert_eq!(encode_project_dir("/a/b_c"), "-a-b-c");
        // Non-BMP chars are two UTF-16 units → two dashes, like Claude Code.
        assert_eq!(encode_project_dir("/a/😀"), "-a---");
    }

    #[test]
    fn encode_truncates_long_paths_with_base36_hash() {
        let long = format!("/tmp/{}", "x".repeat(300));
        let encoded = encode_project_dir(&long);
        assert_eq!(encoded.len(), 200 + 1 + base36(js_string_hash(&long).unsigned_abs() as u64).len());
        assert!(encoded.starts_with("-tmp-xxx"));
    }

    #[test]
    fn decode_resolves_dots_and_spaces_via_filesystem() {
        let base = std::env::temp_dir().join(format!("cct-codec-test-{}", std::process::id()));
        let project = base.join("My App.v2").join("sub-dir");
        std::fs::create_dir_all(&project).unwrap();
        let abs = project.to_string_lossy().into_owned();
        let encoded = encode_project_dir(&abs);
        assert_eq!(decode_project(&encoded), short_project(&abs));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn decode_accepts_legacy_underscore_dirs() {
        let base = std::env::temp_dir().join(format!("cct-codec-legacy-{}", std::process::id()));
        let project = base.join("md_export_bot");
        std::fs::create_dir_all(&project).unwrap();
        let abs = project.to_string_lossy().into_owned();
        // Legacy Claude Code kept '_' literally in the encoded name.
        let legacy_encoded: String = abs
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' })
            .collect();
        assert_eq!(decode_project(&legacy_encoded), short_project(&abs));
        std::fs::remove_dir_all(&base).unwrap();
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
