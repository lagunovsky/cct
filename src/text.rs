//! JSONL text extraction, line classification, snippet helpers.

use serde_json::Value as JsonValue;

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionStats {
    pub prompts: u32,
    pub answers: u32,
    pub tools: u32,
}

/// Extract the searchable text from a JSONL byte slice (string values only,
/// JSON keys excluded — matches the original Go behavior).
pub fn extract_text(data: &[u8]) -> (String, SessionStats) {
    let mut buf = String::new();
    let mut stats = SessionStats::default();
    for line in data.split(|b| *b == b'\n') {
        let line = trim_ws(line);
        if line.is_empty() {
            continue;
        }
        match serde_json::from_slice::<JsonValue>(line) {
            Ok(value) => {
                collect_strings(&value, &mut buf);
                if let JsonValue::Object(map) = &value {
                    count_stats(map, &mut stats);
                }
            }
            Err(_) => {
                buf.push_str(&String::from_utf8_lossy(line));
            }
        }
        buf.push(' ');
    }
    (buf, stats)
}

fn trim_ws(mut data: &[u8]) -> &[u8] {
    while matches!(data.first(), Some(b' ' | b'\t' | b'\r')) {
        data = &data[1..];
    }
    while matches!(data.last(), Some(b' ' | b'\t' | b'\r')) {
        data = &data[..data.len() - 1];
    }
    data
}

fn collect_strings(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::String(s) if s.len() > 1 => {
            out.push_str(s);
            out.push(' ');
        }
        JsonValue::Array(values) => {
            for v in values {
                collect_strings(v, out);
            }
        }
        JsonValue::Object(map) => {
            for v in map.values() {
                collect_strings(v, out);
            }
        }
        _ => {}
    }
}

fn count_stats(obj: &serde_json::Map<String, JsonValue>, stats: &mut SessionStats) {
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let Some(msg) = obj.get("message").and_then(|v| v.as_object()) else {
        return;
    };
    if let Some(content) = msg.get("content") {
        match content {
            JsonValue::Array(blocks) => {
                for block in blocks {
                    let Some(block_obj) = block.as_object() else {
                        continue;
                    };
                    let ct = block_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match (ct, msg_type) {
                        ("text", "user") => {
                            let text = block_obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
                            if rewrite_slash_command(text).is_some() || !is_meta_content(text) {
                                stats.prompts = stats.prompts.saturating_add(1);
                            }
                        }
                        ("text", "assistant") => {
                            stats.answers = stats.answers.saturating_add(1);
                        }
                        ("tool_use", _) => {
                            stats.tools = stats.tools.saturating_add(1);
                        }
                        _ => {}
                    }
                }
            }
            JsonValue::String(s)
                if msg_type == "user"
                    && (rewrite_slash_command(s).is_some() || !is_meta_content(s)) =>
            {
                stats.prompts = stats.prompts.saturating_add(1);
            }
            _ => {}
        }
    }
}

/// Classify a JSONL line into a short tag like `prompt`, `answer`, `agent`, `hook`,
/// or a tool name. Returns an empty string when nothing recognizable is found.
pub fn classify_line(line: &str) -> String {
    let line = line.trim();
    if !line.starts_with('{') {
        return String::new();
    }
    let Ok(value) = serde_json::from_str::<JsonValue>(line) else {
        return String::new();
    };
    let Some(obj) = value.as_object() else {
        return String::new();
    };
    let msg_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(message) = obj.get("message").and_then(|v| v.as_object()) {
        if let Some(content) = message.get("content").and_then(|v| v.as_array()) {
            for block in content {
                let Some(block_obj) = block.as_object() else {
                    continue;
                };
                let ct = block_obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ct {
                    "tool_use" => {
                        if let Some(name) = block_obj.get("name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                return name.to_lowercase();
                            }
                        }
                        return "tool".into();
                    }
                    "tool_result" => return "tool-result".into(),
                    "thinking" => return "thinking".into(),
                    _ => {}
                }
            }
        }
        match msg_type {
            "user" => return "prompt".into(),
            "assistant" => return "answer".into(),
            _ => {}
        }
    }
    if msg_type == "progress" {
        if let Some(data) = obj.get("data").and_then(|v| v.as_object()) {
            if let Some(dt) = data.get("type").and_then(|v| v.as_str()) {
                return match dt {
                    "agent_progress" => "agent".into(),
                    "hook_progress" => "hook".into(),
                    other => other.to_string(),
                };
            }
        }
    }
    msg_type.to_string()
}

/// Best-effort top-level RFC3339 timestamp from a JSONL line. Returns 0 when missing or unparseable.
pub fn extract_top_level_timestamp_ms(line: &str) -> i64 {
    let line = line.trim();
    if !line.starts_with('{') {
        return 0;
    }
    #[derive(serde::Deserialize)]
    struct Wrap<'a> {
        timestamp: Option<&'a str>,
    }
    let Ok(parsed) = serde_json::from_str::<Wrap>(line) else {
        return 0;
    };
    let Some(ts) = parsed.timestamp else {
        return 0;
    };
    chrono::DateTime::parse_from_rfc3339(ts)
        .map(|dt| dt.timestamp_millis())
        .unwrap_or(0)
}

pub fn is_meta_content(s: &str) -> bool {
    let s = s.trim_start();
    const PREFIXES: &[&str] = &[
        "<local-command",
        "<command-",
        "<system-reminder>",
        "<bash-input>",
        "<bash-stdout>",
        "<bash-stderr>",
        "<user-prompt-submit-hook>",
    ];
    PREFIXES.iter().any(|p| s.starts_with(p))
}

/// Collapse a `<command-message>…</command-message><command-name>/foo</command-name><command-args>…</command-args>`
/// block (any tag order, whitespace-separated) into the human form `/foo …`.
/// Returns `None` when the input is not a slash-command invocation.
pub fn rewrite_slash_command(s: &str) -> Option<String> {
    fn extract<'a>(s: &'a str, tag: &str) -> Option<&'a str> {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        let start = s.find(&open)? + open.len();
        let end = s[start..].find(&close)? + start;
        Some(s[start..end].trim())
    }
    let name = extract(s, "command-name")?;
    let args = extract(s, "command-args").unwrap_or("");
    if args.is_empty() {
        Some(name.to_string())
    } else {
        Some(format!("{name} {args}"))
    }
}

/// The session name Claude Code renders in its picker. When a session is named
/// — via `/rename`, explicit or auto — Claude Code writes an
/// `{"type":"agent-name","agentName":"…"}` record. A forked session instead gets
/// only a placeholder `{"type":"custom-title","customTitle":"… (Branch)"}` and no
/// `agent-name`, and Claude Code shows no name for it; so neither do we.
///
/// Preference order, mirroring Claude Code:
///   1. the last non-empty `agent-name`;
///   2. otherwise the last non-empty `custom-title`, unless it is a `(Branch)`
///      placeholder — the fallback covers pre-`agent-name` sessions that only
///      ever wrote `custom-title`.
///
/// A session can be renamed more than once, so the last record of each kind
/// wins. Returns `None` when the session was never named.
pub fn extract_session_title(data: &[u8]) -> Option<String> {
    let mut agent_name = None;
    let mut custom_title = None;
    for line in data.split(|b| *b == b'\n') {
        // Cheap byte prefilter so we only JSON-parse the rare title records.
        let has_agent = slice_contains(line, b"\"agent-name\"");
        let has_custom = !has_agent && slice_contains(line, b"\"custom-title\"");
        if !has_agent && !has_custom {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<JsonValue>(line) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("agent-name") => {
                if let Some(t) = value.get("agentName").and_then(|t| t.as_str()) {
                    if !t.is_empty() {
                        agent_name = Some(t.to_string());
                    }
                }
            }
            Some("custom-title") => {
                if let Some(t) = value.get("customTitle").and_then(|t| t.as_str()) {
                    if !t.is_empty() {
                        custom_title = Some(t.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    agent_name.or_else(|| custom_title.filter(|t| !t.ends_with(" (Branch)")))
}

fn slice_contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Walk an arbitrary JSON value and collect all string values containing `query` (lowercased).
pub fn find_json_values(line: &str, query_lower: &str) -> Vec<String> {
    let line = line.trim();
    if !line.starts_with('{') {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<JsonValue>(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_matching_values(&value, query_lower, &mut out);
    out.sort_by(|a, b| {
        if a.len() != b.len() {
            b.len().cmp(&a.len())
        } else {
            a.cmp(b)
        }
    });
    out
}

fn collect_matching_values(value: &JsonValue, query: &str, out: &mut Vec<String>) {
    match value {
        JsonValue::String(s) if s.len() > 1 && s.to_lowercase().contains(query) => {
            out.push(s.clone());
        }
        JsonValue::Array(values) => {
            for v in values {
                collect_matching_values(v, query, out);
            }
        }
        JsonValue::Object(map) => {
            for v in map.values() {
                collect_matching_values(v, query, out);
            }
        }
        _ => {}
    }
}

/// Find the last top-level `timestamp` field by scanning lines from the end.
/// Returns 0 if none is found.
pub fn last_message_time_ms(data: &[u8]) -> i64 {
    let mut end = data.len();
    while end > 0 && data[end - 1] == b'\n' {
        end -= 1;
    }
    while end > 0 {
        let start = data[..end].iter().rposition(|b| *b == b'\n');
        let (line, next_end) = match start {
            Some(i) => (&data[i + 1..end], i),
            None => (&data[..end], 0),
        };
        if let Ok(line_str) = std::str::from_utf8(line) {
            let ts = extract_top_level_timestamp_ms(line_str);
            if ts != 0 {
                return ts;
            }
        }
        end = next_end;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::extract_session_title;

    #[test]
    fn extract_session_title_picks_last_agent_name() {
        let data = concat!(
            r#"{"type":"user","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"first","sessionId":"x"}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"second","sessionId":"x"}"#,
            "\n",
        );
        assert_eq!(extract_session_title(data.as_bytes()).as_deref(), Some("second"));
    }

    #[test]
    fn extract_session_title_prefers_agent_name_over_custom_title() {
        // `/rename` writes both records; the picker renders `agent-name`.
        let data = concat!(
            r#"{"type":"custom-title","customTitle":"stale","sessionId":"x"}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"real-name","sessionId":"x"}"#,
            "\n",
        );
        assert_eq!(
            extract_session_title(data.as_bytes()).as_deref(),
            Some("real-name")
        );
    }

    #[test]
    fn extract_session_title_ignores_branch_placeholder() {
        // A fork gets only a `(Branch)` custom-title and no agent-name; Claude
        // Code shows no name for it.
        let data =
            r#"{"type":"custom-title","customTitle":"do the thing (Branch)","sessionId":"x"}"#;
        assert_eq!(extract_session_title(data.as_bytes()), None);
    }

    #[test]
    fn extract_session_title_forked_then_renamed_uses_agent_name() {
        // A forked session that was later renamed has both the `(Branch)`
        // placeholder and a real agent-name; the agent-name wins.
        let data = concat!(
            r#"{"type":"custom-title","customTitle":"do the thing (Branch)","sessionId":"x"}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":"my-name","sessionId":"x"}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"my-name","sessionId":"x"}"#,
            "\n",
        );
        assert_eq!(
            extract_session_title(data.as_bytes()).as_deref(),
            Some("my-name")
        );
    }

    #[test]
    fn extract_session_title_falls_back_to_custom_title_when_no_agent_name() {
        // Pre-`agent-name` sessions only ever wrote `custom-title`.
        let data = concat!(
            r#"{"type":"custom-title","customTitle":"first","sessionId":"x"}"#,
            "\n",
            r#"{"type":"custom-title","customTitle":"second","sessionId":"x"}"#,
            "\n",
        );
        assert_eq!(extract_session_title(data.as_bytes()).as_deref(), Some("second"));
    }

    #[test]
    fn extract_session_title_none_when_absent() {
        let data = r#"{"type":"user","message":{"content":"no title here"}}"#;
        assert_eq!(extract_session_title(data.as_bytes()), None);
    }

    #[test]
    fn extract_session_title_ignores_empty() {
        let data = concat!(
            r#"{"type":"custom-title","customTitle":"","sessionId":"x"}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"","sessionId":"x"}"#,
        );
        assert_eq!(extract_session_title(data.as_bytes()), None);
    }
}
