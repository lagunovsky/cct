//! `cct show <session> [@where]` — read a session's conversation around one
//! point: the place a search snippet came from (`@offset`), a moment in time,
//! or the end. What an agent otherwise does with a python loop over the JSONL.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value as JsonValue;

use crate::fs_scan::find_session_log;
use crate::index::catalog::read_catalog;
use crate::path_codec::{index_root, projects_dir};
use crate::term::{BOLD, CYAN, DIM, RESET};
use crate::text::{
    extract_top_level_timestamp_ms, is_boilerplate_record, is_meta_content, rewrite_slash_command,
};

pub struct ShowArgs {
    pub session: String,
    /// `@<offset>` or `@<path in session dir>:<offset>`, as search prints it.
    pub at: Option<String>,
    /// A moment instead: the first record at or after it.
    pub time: Option<String>,
    /// Records shown before and after the point.
    pub context: usize,
    /// Print each record whole instead of its first `TRUNCATE_CHARS`.
    pub full: bool,
}

const TRUNCATE_CHARS: usize = 600;

pub fn run_show(args: ShowArgs) -> Result<()> {
    let log = resolve_session(&args.session)?;
    let (file, offset) = match args.at.as_deref() {
        Some(at) => parse_locator(&log, at)?,
        None => (log.clone(), None),
    };
    let data = read_log(&file)?;
    let id = log
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    println!("{BOLD}{id}{RESET}  {DIM}{}{RESET}", file.display());

    if file.extension().and_then(|e| e.to_str()) != Some("jsonl") {
        // A tool result is plain text: show its lines around the offset.
        return show_text_lines(&data, offset.unwrap_or(0), args.context);
    }

    // Only the lines around the point are parsed: a long session's log runs
    // to hundreds of megabytes.
    let spans = line_spans(&data);
    let line_at = |i: usize| &data[spans[i].0..spans[i].1];
    let start_line = if let Some(offset) = offset {
        spans
            .partition_point(|(start, _)| *start as u64 <= offset)
            .saturating_sub(1)
    } else if let Some(time) = args.time.as_deref() {
        let ms = parse_time_bound(time, false)?;
        (0..spans.len())
            .find(|&i| {
                std::str::from_utf8(line_at(i))
                    .is_ok_and(|l| extract_top_level_timestamp_ms(l) >= ms)
            })
            .ok_or_else(|| anyhow!("no record at or after {time}"))?
    } else {
        spans.len().saturating_sub(1)
    };
    let records_of = |i: usize| line_records(line_at(i), spans[i].0 as u64);

    // The point is the first readable record at or after the start line (or
    // the last one in the log); then context on both sides.
    let mut records: Vec<Record> = Vec::new();
    let mut target = None;
    for i in start_line..spans.len() {
        let found = records_of(i);
        if !found.is_empty() {
            records.extend(found);
            target = Some(i);
            break;
        }
    }
    let target_line = match target {
        Some(i) => i,
        None => {
            let i = (0..start_line.min(spans.len()))
                .rev()
                .find(|&i| !records_of(i).is_empty())
                .ok_or_else(|| anyhow!("no conversation records in {}", file.display()))?;
            records = records_of(i);
            i
        }
    };
    let mut before: Vec<Record> = Vec::new();
    for i in (0..target_line).rev() {
        if before.len() >= args.context {
            break;
        }
        let mut found = records_of(i);
        found.append(&mut before);
        before = found;
    }
    let skip = before.len().saturating_sub(args.context);
    let target = before.len() - skip;
    let mut shown: Vec<Record> = before.into_iter().skip(skip).collect();
    shown.extend(records);
    for i in target_line + 1..spans.len() {
        if shown.len() > target + args.context {
            break;
        }
        shown.extend(records_of(i));
    }
    shown.truncate(target + args.context + 1);
    let records = shown;
    for (i, r) in records.iter().enumerate() {
        let mark = if i == target { "▶" } else { " " };
        let ts = chrono::DateTime::from_timestamp_millis(r.time_ms)
            .map(|d| {
                d.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        println!(
            "\n{mark} {DIM}{ts}{RESET} {CYAN}[{}]{RESET} {DIM}@{}{RESET}",
            r.role, r.offset
        );
        let text = if args.full {
            r.text.clone()
        } else {
            truncate(&r.text, TRUNCATE_CHARS)
        };
        for line in text.lines() {
            println!("    {line}");
        }
    }
    Ok(())
}

/// The main log of the session `q` names: a full id or unique prefix. The
/// index catalog knows archived sessions too; without it, live logs are
/// searched by file name.
pub fn resolve_session(q: &str) -> Result<PathBuf> {
    let q = q.trim();
    if q.is_empty() || q.contains(['/', '\\']) {
        bail!("not a session id: {q:?}");
    }
    if let Ok(entries) = index_root().and_then(|dir| read_catalog(&dir)) {
        let mut matches: Vec<PathBuf> = entries
            .into_iter()
            .filter(|e| e.kind == "session_jsonl")
            .map(|e| e.path)
            .filter(|p| {
                p.file_stem()
                    .is_some_and(|s| s.to_string_lossy().starts_with(q))
            })
            .collect();
        if let Some(exact) = matches
            .iter()
            .find(|p| p.file_stem().is_some_and(|s| s == q))
        {
            return Ok(exact.clone());
        }
        matches.sort();
        matches.dedup();
        match matches.len() {
            1 => return Ok(matches.remove(0)),
            0 => {}
            n => bail!("session prefix {q:?} is ambiguous ({n} matches)"),
        }
    }
    find_session_log(&projects_dir()?, q)?.ok_or_else(|| anyhow!("no session matches {q:?}"))
}

/// `@123` → the main log at byte 123; `@subagents/agent-x.jsonl:123` → that
/// file in the session's directory.
fn parse_locator(log: &Path, at: &str) -> Result<(PathBuf, Option<u64>)> {
    let at = at.strip_prefix('@').unwrap_or(at);
    let (file, offset) = match at.rsplit_once(':') {
        Some((rel, offset)) => {
            if rel.split('/').any(|part| part == ".." || part.is_empty()) {
                bail!("bad location {at:?}");
            }
            (log.with_extension("").join(rel), offset)
        }
        None => (log.to_path_buf(), at),
    };
    let offset: u64 = offset
        .parse()
        .with_context(|| format!("bad offset in {at:?}"))?;
    Ok((file, Some(offset)))
}

fn read_log(path: &Path) -> Result<Vec<u8>> {
    match fs::read(path) {
        Ok(data) => Ok(data),
        Err(err) => crate::archive::read_archived(path)
            .ok_or(err)
            .with_context(|| format!("read {}", path.display())),
    }
}

struct Record {
    offset: u64,
    time_ms: i64,
    role: String,
    text: String,
}

fn line_spans(data: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (i, b) in data.iter().enumerate() {
        if *b == b'\n' {
            spans.push((start, i));
            start = i + 1;
        }
    }
    if start < data.len() {
        spans.push((start, data.len()));
    }
    spans
}

/// The readable parts of one log line — prompt, answer, tool call, result,
/// queued prompt, edit — or none for boilerplate and bookkeeping records.
fn line_records(line: &[u8], offset: u64) -> Vec<Record> {
    let Ok(value) = serde_json::from_slice::<JsonValue>(line) else {
        return Vec::new();
    };
    if is_boilerplate_record(&value) {
        return Vec::new();
    }
    let time_ms = value
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|d| d.timestamp_millis())
        .unwrap_or(0);
    render_record(&value)
        .into_iter()
        .map(|(role, text)| Record {
            offset,
            time_ms,
            role,
            text,
        })
        .collect()
}

/// `(role, text)` for each readable part of one record.
fn render_record(value: &JsonValue) -> Vec<(String, String)> {
    let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    let mut out = Vec::new();
    match kind {
        "user" | "assistant" => {
            let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
                return out;
            };
            if let Some(text) = content.as_str() {
                push_text(&mut out, kind, text);
                return out;
            }
            for block in content.as_array().into_iter().flatten() {
                match block.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                    "text" => {
                        let text = block.get("text").and_then(|t| t.as_str()).unwrap_or("");
                        push_text(&mut out, kind, text);
                    }
                    "tool_use" => {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let input = block.get("input").cloned().unwrap_or(JsonValue::Null);
                        out.push((name.to_lowercase(), tool_summary(&input)));
                    }
                    "tool_result" => {
                        out.push(("result".to_string(), tool_result_text(block)));
                    }
                    "image" => out.push(("image".to_string(), String::new())),
                    _ => {}
                }
            }
        }
        "attachment" => {
            let Some(a) = value.get("attachment") else {
                return out;
            };
            match a.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "queued_command" => {
                    let text = a.get("prompt").and_then(|p| p.as_str()).unwrap_or("");
                    out.push(("queued".to_string(), text.to_string()));
                }
                "edited_text_file" => {
                    let file = a.get("filename").and_then(|f| f.as_str()).unwrap_or("");
                    out.push(("edited".to_string(), file.to_string()));
                }
                _ => {}
            }
        }
        _ => {}
    }
    out
}

fn push_text(out: &mut Vec<(String, String)>, kind: &str, text: &str) {
    let text = match rewrite_slash_command(text) {
        Some(command) => command,
        None if is_meta_content(text) => return,
        None => text.trim().to_string(),
    };
    if text.is_empty() {
        return;
    }
    let role = if kind == "user" { "prompt" } else { "answer" };
    out.push((role.to_string(), text));
}

/// The one line that says what a tool call did: the command, the file, the
/// pattern — whichever the tool takes.
fn tool_summary(input: &JsonValue) -> String {
    for key in ["command", "file_path", "notebook_path", "pattern", "url", "query", "description", "prompt"] {
        if let Some(v) = input.get(key).and_then(|v| v.as_str()) {
            return v.to_string();
        }
    }
    let raw = input.to_string();
    if raw == "null" {
        String::new()
    } else {
        raw
    }
}

fn tool_result_text(block: &JsonValue) -> String {
    match block.get("content") {
        Some(JsonValue::String(s)) => s.clone(),
        Some(JsonValue::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((idx, _)) => format!("{}{DIM} ⋯ (+{} chars, --full){RESET}", &text[..idx], text[idx..].chars().count()),
        None => text.to_string(),
    }
}

fn show_text_lines(data: &[u8], offset: u64, context: usize) -> Result<()> {
    let text = String::from_utf8_lossy(data);
    let mut start = 0u64;
    let lines: Vec<(u64, &str)> = text
        .split('\n')
        .map(|l| {
            let at = start;
            start += l.len() as u64 + 1;
            (at, l)
        })
        .collect();
    let target = lines.iter().rposition(|(at, _)| *at <= offset).unwrap_or(0);
    let from = target.saturating_sub(context);
    let to = (target + context + 1).min(lines.len());
    for (i, (_, line)) in lines[from..to].iter().enumerate() {
        let mark = if from + i == target { "▶" } else { " " };
        println!("{mark} {line}");
    }
    Ok(())
}

/// A time bound from the command line, in ms: `2026-10-25`, `2026-10-25 18:00`
/// (local time; `T` works as the separator too) or an age — `30m`, `2h`, `3d`
/// ago. A bare date opens its day, or closes it when `end_of_day` is set.
pub fn parse_time_bound(raw: &str, end_of_day: bool) -> Result<i64> {
    use chrono::{Local, NaiveDate, NaiveDateTime, TimeZone};
    let raw = raw.trim();
    let now = Local::now();
    if let Some(unit) = raw.chars().last().filter(|c| matches!(c, 'm' | 'h' | 'd')) {
        if let Ok(n) = raw[..raw.len() - 1].parse::<i64>() {
            let secs = match unit {
                'm' => 60,
                'h' => 3600,
                _ => 86_400,
            };
            return Ok(now.timestamp_millis() - n * secs * 1000);
        }
    }
    let normalized = raw.replace('T', " ");
    for format in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(&normalized, format) {
            return local_ms(Local.from_local_datetime(&dt).earliest(), raw);
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(&normalized, "%Y-%m-%d") {
        let dt = if end_of_day {
            date.and_hms_milli_opt(23, 59, 59, 999)
        } else {
            date.and_hms_opt(0, 0, 0)
        };
        return local_ms(dt.and_then(|d| Local.from_local_datetime(&d).earliest()), raw);
    }
    bail!("cannot read time {raw:?}: use 2026-10-25, \"2026-10-25 18:00\" or 2h/3d")
}

fn local_ms(dt: Option<chrono::DateTime<chrono::Local>>, raw: &str) -> Result<i64> {
    dt.map(|d| d.timestamp_millis())
        .ok_or_else(|| anyhow!("no such local time: {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locators_point_into_the_session_dir() {
        let log = Path::new("/p/-proj/abc.jsonl");
        assert_eq!(parse_locator(log, "@42").unwrap(), (log.to_path_buf(), Some(42)));
        assert_eq!(
            parse_locator(log, "@subagents/agent-x.jsonl:7").unwrap(),
            (PathBuf::from("/p/-proj/abc/subagents/agent-x.jsonl"), Some(7))
        );
        assert!(parse_locator(log, "@../../etc/passwd:1").is_err());
        assert!(parse_locator(log, "@nope").is_err());
    }

    #[test]
    fn records_render_prompts_answers_and_tool_calls() {
        let data = [
            r#"{"type":"user","timestamp":"2026-10-25T18:00:00Z","message":{"content":"lock the doors"}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-25T18:00:05Z","message":{"content":[{"type":"text","text":"Locking."},{"type":"tool_use","name":"Bash","input":{"command":"adb shell lock"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-25T18:00:06Z","message":{"content":[{"type":"tool_result","content":"locked"}]}}"#,
            r#"{"type":"file-history-snapshot","snapshot":{}}"#,
        ]
        .join("\n");
        let bytes = data.as_bytes();
        let records: Vec<Record> = line_spans(bytes)
            .into_iter()
            .flat_map(|(s, e)| line_records(&bytes[s..e], s as u64))
            .collect();
        let roles: Vec<&str> = records.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(roles, ["prompt", "answer", "bash", "result"]);
        assert_eq!(records[2].text, "adb shell lock");
        assert_eq!(records[1].offset, records[2].offset, "one line, two parts");
        assert!(records[3].offset > records[2].offset);
    }

    #[test]
    fn time_bounds_read_dates_times_and_ages() {
        let day = parse_time_bound("2026-10-25", false).unwrap();
        let day_end = parse_time_bound("2026-10-25", true).unwrap();
        let evening = parse_time_bound("2026-10-25 18:00", false).unwrap();
        assert!(day < evening && evening < day_end);
        assert_eq!(evening, parse_time_bound("2026-10-25T18:00", false).unwrap());
        let two_hours = parse_time_bound("2h", false).unwrap();
        let now = chrono::Local::now().timestamp_millis();
        assert!((now - two_hours - 7_200_000).abs() < 5_000);
        assert!(parse_time_bound("yesterday", false).is_err());
    }
}
