//! `cct recent` — list recent sessions in the current project.
//! `cct` with no args dispatches here as well.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde_json::Value as JsonValue;

use crate::path_codec::{encode_project_dir, projects_dir};
use crate::term::{fmt_size, BOLD, CYAN, DIM, GREEN, RESET};
use crate::text::{is_meta_content, last_message_time_ms};

pub struct RecentOptions {
    pub limit: usize,
}

#[derive(Clone)]
struct PromptPair {
    user: String,
    response: String,
}

pub fn show_recent_sessions(opts: RecentOptions) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cwd_str = cwd.to_string_lossy().into_owned();
    let encoded = encode_project_dir(&cwd_str);
    let sessions_dir = projects_dir()?.join(&encoded);

    let entries = match fs::read_dir(&sessions_dir) {
        Ok(it) => it,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("\n{DIM}No Claude Code sessions found for {cwd_str}{RESET}");
            return Ok(());
        }
        Err(err) => return Err(err.into()),
    };

    let mut files: Vec<(PathBuf, i64)> = Vec::new();
    for e in entries {
        let e = e?;
        if !e.file_type()?.is_file() {
            continue;
        }
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let mtime = session_activity_time_ms(&path);
        files.push((path, mtime));
    }
    if files.is_empty() {
        println!(
            "\n{DIM}No sessions found in {}{RESET}",
            sessions_dir.display()
        );
        return Ok(());
    }
    files.sort_by(|a, b| b.1.cmp(&a.1));
    let limit = opts.limit.min(files.len());

    println!("{BOLD}Recent sessions in:{RESET} {cwd_str}");

    for (path, mtime_ms) in files.iter().take(limit) {
        let session_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let (prompts, total_prompts, tool_calls, bytes) = match load_session_summary(path, 2) {
            Ok(v) => v,
            Err(err) => {
                println!("  {DIM}(error reading {session_id}: {err}){RESET}");
                continue;
            }
        };
        print_session_summary(
            &session_id,
            *mtime_ms,
            &prompts,
            total_prompts,
            tool_calls,
            bytes,
        );
    }
    Ok(())
}

/// Activity time for sorting: last top-level timestamp in the file (read via tail), else mtime.
pub fn session_activity_time_ms(path: &Path) -> i64 {
    const TAIL: u64 = 256 * 1024;
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let mtime_fallback = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let Ok(mut f) = fs::File::open(path) else {
        return mtime_fallback;
    };
    let total = meta.len();
    let mut data = Vec::new();
    if total > TAIL {
        if f.seek(SeekFrom::Start(total - TAIL)).is_ok() {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() {
                if let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                    data = buf[nl + 1..].to_vec();
                }
            }
        }
    }
    if data.is_empty() {
        let _ = f.seek(SeekFrom::Start(0));
        let mut buf = Vec::new();
        if f.read_to_end(&mut buf).is_ok() {
            data = buf;
        }
    }
    let ts = last_message_time_ms(&data);
    if ts != 0 {
        ts
    } else {
        mtime_fallback
    }
}

/// Returns `(tail_pairs, total_prompts, tool_calls, bytes)`.
fn load_session_summary(path: &Path, n: usize) -> Result<(Vec<PromptPair>, u32, u32, u64)> {
    let data = fs::read(path)?;
    let bytes = data.len() as u64;
    let mut pairs = Vec::new();
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut tool_calls = 0u32;
    for line in data.split(|b| *b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("");
        if line.is_empty() {
            continue;
        }
        if let Some((role, text, tools)) = parse_recent_entry(line) {
            tool_calls += tools;
            if let (Some(role_s), Some(text_s)) = (role, text) {
                entries.push((role_s, text_s));
            }
        }
    }
    for (i, (role, text)) in entries.iter().enumerate() {
        if role != "user" {
            continue;
        }
        let mut response = String::new();
        for j in i + 1..entries.len() {
            if entries[j].0 == "user" {
                break;
            }
            if entries[j].0 == "assistant" && !entries[j].1.is_empty() {
                response = entries[j].1.clone();
                break;
            }
        }
        pairs.push(PromptPair {
            user: text.clone(),
            response,
        });
    }
    let total = pairs.len() as u32;
    if pairs.len() > n {
        pairs = pairs[pairs.len() - n..].to_vec();
    }
    Ok((pairs, total, tool_calls, bytes))
}

#[allow(clippy::type_complexity)]
fn parse_recent_entry(line: &str) -> Option<(Option<String>, Option<String>, u32)> {
    let parsed: JsonValue = serde_json::from_str(line).ok()?;
    let obj = parsed.as_object()?;
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");

    let mut tools = 0u32;
    if let Some(message) = obj.get("message").and_then(|v| v.as_object()) {
        if let Some(JsonValue::Array(blocks)) = message.get("content") {
            for block in blocks {
                if let Some(b) = block.as_object() {
                    if b.get("type").and_then(|v| v.as_str()) == Some("tool_use") {
                        tools += 1;
                    }
                }
            }
        }
    }

    let (role, text) = match msg_type {
        "user" => {
            let text = extract_user_text(obj.get("message"))?;
            if is_meta_content(&text) {
                return Some((None, None, tools));
            }
            (Some("user".to_string()), Some(text))
        }
        "assistant" => {
            let text = extract_assistant_text(obj.get("message"));
            if text.is_empty() {
                return Some((None, None, tools));
            }
            (Some("assistant".to_string()), Some(text))
        }
        _ => return Some((None, None, tools)),
    };
    Some((role, text, tools))
}

fn extract_user_text(message: Option<&JsonValue>) -> Option<String> {
    let msg = message?.as_object()?;
    let content = msg.get("content")?;
    match content {
        JsonValue::String(s) => Some(s.clone()),
        JsonValue::Array(blocks) => {
            let mut buf = String::new();
            for block in blocks {
                let Some(b) = block.as_object() else { continue };
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(text) = b.get("text").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            if !buf.is_empty() {
                                buf.push(' ');
                            }
                            buf.push_str(text);
                        }
                    }
                }
            }
            if buf.is_empty() {
                None
            } else {
                Some(buf)
            }
        }
        _ => None,
    }
}

fn extract_assistant_text(message: Option<&JsonValue>) -> String {
    let Some(msg) = message.and_then(|v| v.as_object()) else {
        return String::new();
    };
    let Some(JsonValue::Array(blocks)) = msg.get("content") else {
        return String::new();
    };
    let mut buf = String::new();
    for block in blocks {
        let Some(b) = block.as_object() else { continue };
        if b.get("type").and_then(|v| v.as_str()) == Some("text") {
            if let Some(text) = b.get("text").and_then(|v| v.as_str()) {
                if !buf.is_empty() {
                    buf.push(' ');
                }
                buf.push_str(text);
            }
        }
    }
    buf
}

fn print_session_summary(
    session_id: &str,
    mtime_ms: i64,
    prompts: &[PromptPair],
    total_prompts: u32,
    tool_calls: u32,
    bytes: u64,
) {
    let when = if mtime_ms > 0 {
        chrono::DateTime::from_timestamp_millis(mtime_ms)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M")
                    .to_string()
            })
            .unwrap_or_else(|| "?".into())
    } else {
        "?".into()
    };
    let stats = if total_prompts > 0 || bytes > 0 {
        format!(
            "  {DIM}{total_prompts} prompts · {tool_calls} tools · {size}{RESET}",
            size = fmt_size(bytes),
        )
    } else {
        String::new()
    };
    println!("\n{CYAN}{BOLD}{session_id}{RESET}  {DIM}{when}{RESET}{stats}");
    if prompts.is_empty() {
        println!("  {DIM}(no user prompts){RESET}");
        return;
    }
    for p in prompts {
        let user_text = trunc_runes(&collapse_spaces(&p.user), 200);
        let resp_text = trunc_runes(&collapse_spaces(&p.response), 200);
        println!("  {GREEN}>{RESET} {user_text}");
        if !resp_text.is_empty() {
            println!("    {DIM}{resp_text}{RESET}");
        }
    }
}

fn trunc_runes(s: &str, n: usize) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    for ch in s.chars() {
        if count >= n {
            out.push_str("...");
            return out;
        }
        out.push(ch);
        count += 1;
    }
    out
}

fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for ch in s.chars() {
        let normalized = if ch == '\n' || ch == '\t' || ch == '\r' {
            ' '
        } else {
            ch
        };
        if normalized == ' ' {
            if prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(normalized);
    }
    out.trim().to_string()
}

/// Read the prompts (without responses) used by the move/picker preview.
pub fn first_user_prompt(path: &Path) -> Option<String> {
    let data = fs::read(path).ok()?;
    for line in data.split(|b| *b == b'\n') {
        let line = std::str::from_utf8(line).unwrap_or("");
        if line.is_empty() {
            continue;
        }
        let parsed: JsonValue = serde_json::from_str(line).ok()?;
        let obj = parsed.as_object()?;
        if obj.get("type").and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        if let Some(text) = extract_user_text(obj.get("message")) {
            if is_meta_content(&text) {
                continue;
            }
            return Some(collapse_spaces(&text));
        }
    }
    None
}
