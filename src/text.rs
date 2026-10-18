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
                if is_boilerplate_record(&value) {
                    continue;
                }
                for_each_indexed_string(&value, &mut |s| {
                    buf.push_str(s);
                    buf.push(' ');
                });
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

/// `attachment` records Claude Code writes into every session: the system
/// prompt, the skill and tool listings, CLAUDE.md and auto-memory, output style,
/// environment, model, date. Since September 2026 they are ~2 GB of a 10.8 GB
/// corpus and identical from one session to the next, so any word from the user's
/// instructions or memory matched thousands of sessions at once.
///
/// A denylist rather than an allowlist on purpose: new types keep appearing, and
/// a boilerplate one slipping through is visible noise that gets added here,
/// while a meaningful one silently dropped by an allowlist would never be
/// noticed. Kept indexed are the attachments that carry what happened in this
/// session: `queued_command` (prompts typed while the agent worked),
/// `structured_output` (subagent results), `edited_text_file`, `file`,
/// `diagnostics`, `task_reminder`, `task_status`, hook errors and context,
/// `plan_file_reference`.
const BOILERPLATE_ATTACHMENTS: &[&str] = &[
    "agent_listing_delta",
    "auto_mode",
    "auto_mode_exit",
    "bash_output_audience_note",
    "batching_reminder_sent",
    "budget_usd",
    "command_permissions",
    "context_tip",
    "credential_org",
    "date",
    "date_change",
    "deferred_tools_delta",
    "deferred_tools_record",
    "elapsed_time_reminder",
    "environment",
    "hook_system_message",
    "instructions",
    "invoked_skills",
    "language",
    "mcp_instructions_delta",
    "model",
    "nested_memory",
    "output_style",
    "output_style_instructions",
    "plan_mode",
    "plan_mode_exit",
    "prompt_snapshot",
    "remote_session_change",
    "session_context",
    "silent_turn_reminder",
    "skill_listing",
    "thinking_drop",
    "thinking_stripped",
    "total_tokens_reminder",
];

/// Records that only copy what the transcript already holds. `api-request*`
/// is the request as sent to the API: the system prompt, the tool schemas and
/// the messages again. `file-history-snapshot` lists the files Edit/Write
/// backed up — the same paths, repeated in every checkpoint. A queued prompt is
/// written once when enqueued and again as the prompt when it is delivered;
/// only one that was removed from the queue exists nowhere else.
const DUPLICATE_RECORDS: &[&str] = &[
    "api-request",
    "api-request-blob",
    "api-request-shape",
    "file-history-snapshot",
];

/// True for a record that repeats session-independent context or another
/// record of the same session. See [`BOILERPLATE_ATTACHMENTS`] and
/// [`DUPLICATE_RECORDS`].
pub fn is_boilerplate_record(value: &JsonValue) -> bool {
    let record = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if DUPLICATE_RECORDS.contains(&record) {
        return true;
    }
    if record == "queue-operation" {
        return value.get("operation").and_then(|v| v.as_str()) != Some("remove");
    }
    if record != "attachment" {
        return false;
    }
    let Some(attachment) = value.get("attachment") else {
        return false;
    };
    let kind = attachment.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if BOILERPLATE_ATTACHMENTS.contains(&kind) {
        return true;
    }
    // An edit to CLAUDE.md or memory is broadcast as `edited_text_file` into
    // every session open at the time — 20 sessions for one MEMORY.md change.
    // The session that made the edit still shows it through its Edit/Write call.
    kind == "edited_text_file"
        && attachment
            .get("filename")
            .and_then(|f| f.as_str())
            .is_some_and(is_instruction_file)
}

/// CLAUDE.md files and auto-memory: context Claude Code loads into sessions,
/// not project files a session works on.
fn is_instruction_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "CLAUDE.md" | "CLAUDE.local.md")
        || (path.contains("/.claude/projects/") && path.contains("/memory/"))
}

/// The file a JSONL line records an edit of: the `file_path` of an
/// Edit/Write/MultiEdit/NotebookEdit call, or the `filename` of an
/// `edited_text_file` attachment — the only trace a Bash or python edit leaves.
pub fn edited_path(line: &str) -> Option<String> {
    if !line.contains("file_path") && !line.contains("notebook_path") && !line.contains("edited_text_file") {
        return None;
    }
    let value: JsonValue = serde_json::from_str(line.trim()).ok()?;
    if value.get("type").and_then(|v| v.as_str()) == Some("attachment") {
        let a = value.get("attachment")?;
        if a.get("type").and_then(|t| t.as_str()) != Some("edited_text_file") {
            return None;
        }
        return a.get("filename").and_then(|f| f.as_str()).map(str::to_string);
    }
    let blocks = value.get("message")?.get("content")?.as_array()?;
    blocks.iter().find_map(|b| {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_use") {
            return None;
        }
        let name = b.get("name").and_then(|n| n.as_str())?;
        if !matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
            return None;
        }
        let input = b.get("input")?;
        input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .and_then(|f| f.as_str())
            .map(str::to_string)
    })
}

/// Whether a session was started headless — `claude -p` or the Agent SDK — from
/// a line of its log: the `entrypoint` Claude Code stamps on every user and
/// assistant record is `sdk-…` then, and `cli` for an interactive session.
/// `None` for a line without one. Matched on raw bytes: inside a message the
/// key would be JSON-escaped and not match.
pub fn headless_entrypoint(line: &[u8]) -> Option<bool> {
    const KEY: &[u8] = b"\"entrypoint\":\"";
    let at = line.windows(KEY.len()).position(|w| w == KEY)? + KEY.len();
    Some(line[at..].starts_with(b"sdk"))
}

/// [`is_boilerplate_record`] for a raw JSONL line, cheap for every other line:
/// only lines that mention an attachment or a duplicate record are parsed.
pub fn is_boilerplate_line(line: &str) -> bool {
    if !line.contains("\"attachment\"")
        && !line.contains("\"queue-operation\"")
        && !DUPLICATE_RECORDS.iter().any(|t| line.contains(&format!("\"{t}\"")))
    {
        return false;
    }
    serde_json::from_str::<JsonValue>(line.trim())
        .map(|v| is_boilerplate_record(&v))
        .unwrap_or(false)
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

/// Top-level fields that identify one record rather than say anything:
/// a fresh uuid per line, so each is a term no query will ever ask for.
/// `wireToolInputs` repeats the tool_use input; the classifier fields are git
/// state and request ids.
const RECORD_ID_KEYS: &[&str] = &[
    "uuid",
    "parentUuid",
    "logicalParentUuid",
    "requestId",
    "promptId",
    "sourceToolAssistantUUID",
    "toolUseID",
    "parentToolUseID",
    "timestamp",
    "wireToolInputs",
    "serverClassifierContext",
    "serverClassifierRequest",
];

/// toolUseResult strings shorter than this are kept without the duplicate
/// check: they cost nothing, and a short one "contained" in the message may
/// only be part of a longer word there.
const MIN_DEDUP_LEN: usize = 64;

/// Calls `f` with every string of a record worth indexing — the same set the
/// snippet pass looks in, so a session is never found through text it cannot
/// show. Left out:
///
/// - base64: thinking signatures, images, Read of an image (`file.base64`).
///   A third of a sample's text, and no one searches for a blob's middle;
/// - [`RECORD_ID_KEYS`] and the `msg_…`/`toolu_…` ids;
/// - toolUseResult strings already present in the record's message: the
///   tool_result there carries the same stdout or file content (with line
///   numbers for Read). Only what the message lacks — a diff, an original
///   file, output the message truncated — is kept.
pub fn for_each_indexed_string<'a>(record: &'a JsonValue, f: &mut impl FnMut(&'a str)) {
    let JsonValue::Object(map) = record else {
        walk_strings(record, f);
        return;
    };
    let mut message_text: Option<(String, Option<String>)> = None;
    for (key, value) in map {
        if RECORD_ID_KEYS.contains(&key.as_str()) {
            continue;
        }
        if key != "toolUseResult" || !map.contains_key("message") {
            walk_strings(value, f);
            continue;
        }
        walk_strings(value, &mut |s: &'a str| {
            if s.len() < MIN_DEDUP_LEN {
                return f(s);
            }
            let (plain, numbered) = message_text.get_or_insert_with(|| {
                let mut text = String::new();
                walk_strings(&map["message"], &mut |m| {
                    text.push_str(m);
                    text.push('\n');
                });
                (text, None)
            });
            if plain.contains(s) {
                return;
            }
            let stripped = numbered.get_or_insert_with(|| strip_line_numbers(plain));
            if !stripped.contains(s) {
                f(s);
            }
        });
    }
}

fn walk_strings<'a>(value: &'a JsonValue, f: &mut impl FnMut(&'a str)) {
    match value {
        JsonValue::String(s) if s.len() > 1 => f(s),
        JsonValue::Array(values) => {
            for v in values {
                walk_strings(v, f);
            }
        }
        JsonValue::Object(map) => {
            let kind = map.get("type").and_then(|t| t.as_str()).unwrap_or("");
            for (key, v) in map {
                let skip = match key.as_str() {
                    "base64" | "tool_use_id" => true,
                    "signature" => kind == "thinking",
                    "data" => matches!(kind, "base64" | "redacted_thinking"),
                    "id" => matches!(kind, "message" | "tool_use"),
                    _ => false,
                };
                if !skip {
                    walk_strings(v, f);
                }
            }
        }
        _ => {}
    }
}

/// Read results reach the message as `12\tline` (or `12→line`); toolUseResult
/// holds the same lines bare.
fn strip_line_numbers(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split('\n') {
        let rest = line.trim_start_matches(' ');
        let body = rest.trim_start_matches(|c: char| c.is_ascii_digit());
        let stripped = if body.len() < rest.len() {
            body.strip_prefix('\t').or_else(|| body.strip_prefix('→'))
        } else {
            None
        };
        out.push_str(stripped.unwrap_or(line));
        out.push('\n');
    }
    out
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
    if msg_type == "attachment"
        && obj
            .get("attachment")
            .and_then(|a| a.get("type"))
            .and_then(|t| t.as_str())
            == Some("edited_text_file")
    {
        // The file changed on disk — usually this session's own Bash/python
        // edit, which leaves no Edit tool call behind. "attachment" said nothing.
        return "edited".into();
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

/// The indexed string values of a JSONL line (see [`for_each_indexed_string`])
/// that contain `query_lower`, longest first. `None` when the line is not JSON.
pub fn find_json_values(line: &str, query_lower: &str) -> Option<Vec<String>> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let value = serde_json::from_str::<JsonValue>(line).ok()?;
    let mut out = Vec::new();
    for_each_indexed_string(&value, &mut |s| {
        if s.to_lowercase().contains(query_lower) {
            out.push(s.to_string());
        }
    });
    out.sort_by(|a, b| {
        if a.len() != b.len() {
            b.len().cmp(&a.len())
        } else {
            a.cmp(b)
        }
    });
    Some(out)
}

/// True when one of `needles_lower` occurs in what the index holds of `line`,
/// not only in a skipped field — a uuid, a blob, a duplicated tool output.
/// A line that is not JSON is taken as it is.
pub fn indexed_text_matches(line: &str, needles_lower: &[String]) -> bool {
    let Ok(value) = serde_json::from_str::<JsonValue>(line.trim()) else {
        return true;
    };
    let mut found = false;
    for_each_indexed_string(&value, &mut |s| {
        if !found {
            let lower = s.to_lowercase();
            found = needles_lower.iter().any(|n| lower.contains(n.as_str()));
        }
    });
    found
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
    use super::{
        classify_line, edited_path, extract_session_title, extract_text, find_json_values,
        indexed_text_matches, is_boilerplate_line,
    };

    #[test]
    fn blobs_ids_and_duplicated_tool_output_are_not_indexed() {
        let thinking = r#"{"type":"assistant","uuid":"UUIDWORD","message":{"id":"msg_IDWORD","type":"message","content":[{"type":"thinking","thinking":"THINKWORD","signature":"SIGWORD"},{"type":"tool_use","id":"toolu_TOOLWORD","name":"Read","input":{"file_path":"/p/a.rs"}}]}}"#;
        let image = r#"{"type":"user","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"IMAGEWORD"}}]}}"#;
        let body = "fn alpha() {}\nfn beta() { ORIGWORD }\nfn gamma() { more text to pass the length bar }";
        let numbered = "1\tfn alpha() {}\n2\tfn beta() { ORIGWORD }\n3\tfn gamma() { more text to pass the length bar }";
        let read = serde_json::json!({
            "type": "user",
            "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_X", "content": numbered}]},
            "toolUseResult": {
                "type": "text",
                "file": {"filePath": "/p/a.rs", "content": body},
                "bashEditDiff": "@@ -1 +1 @@ DIFFWORD and enough text to be over the dedup bar",
            },
        })
        .to_string();
        let snapshot = r#"{"type":"file-history-snapshot","snapshot":{"trackedFileBackups":{"/p/SNAPWORD.rs":{}}}}"#;
        let enqueue = r#"{"type":"queue-operation","operation":"enqueue","content":"ENQUEUEWORD"}"#;
        let removed = r#"{"type":"queue-operation","operation":"remove","content":"REMOVEDWORD"}"#;
        let data = [thinking, image, &read, snapshot, enqueue, removed].join("\n");

        let (text, _) = extract_text(data.as_bytes());
        for gone in ["UUIDWORD", "msg_IDWORD", "SIGWORD", "toolu_TOOLWORD", "IMAGEWORD", "SNAPWORD", "ENQUEUEWORD"] {
            assert!(!text.contains(gone), "{gone} should not be indexed");
        }
        for kept in ["THINKWORD", "DIFFWORD", "REMOVEDWORD", "/p/a.rs"] {
            assert!(text.contains(kept), "{kept} should be indexed");
        }
        assert_eq!(text.matches("ORIGWORD").count(), 1, "Read content is indexed once, from the message");

        assert!(!indexed_text_matches(thinking, &["sigword".into()]));
        assert!(indexed_text_matches(thinking, &["thinkword".into()]));
        assert_eq!(find_json_values(thinking, "sigword"), Some(vec![]));
        assert!(is_boilerplate_line(snapshot));
        assert!(is_boilerplate_line(enqueue));
        assert!(!is_boilerplate_line(removed));
    }

    #[test]
    fn edits_are_recognised_whoever_made_them() {
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/p/src/a.rs","old_string":"x","new_string":"y"}}]}}"#;
        let bash_edit = r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"/p/src/b.rs","snippet":"1\tfn b() {}"}}"#;
        let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Read","input":{"file_path":"/p/src/a.rs"}}]}}"#;
        assert_eq!(edited_path(tool).as_deref(), Some("/p/src/a.rs"));
        assert_eq!(edited_path(bash_edit).as_deref(), Some("/p/src/b.rs"));
        assert_eq!(edited_path(read), None, "reading a file is not editing it");
        assert_eq!(classify_line(bash_edit), "edited");
    }

    #[test]
    fn instruction_file_edit_broadcasts_are_boilerplate() {
        let memory = r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"/Users/x/.claude/projects/-p/memory/MEMORY.md","snippet":"- rule"}}"#;
        let claude_md = r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"/Users/x/.claude/CLAUDE.md","snippet":"- rule"}}"#;
        let source = r#"{"type":"attachment","attachment":{"type":"edited_text_file","filename":"/p/src/main.rs","snippet":"fn main"}}"#;
        assert!(is_boilerplate_line(memory));
        assert!(is_boilerplate_line(claude_md));
        assert!(!is_boilerplate_line(source), "a project file edit is this session's work");
    }

    #[test]
    fn boilerplate_attachments_are_not_indexed_but_session_ones_are() {
        let memory = r#"{"type":"attachment","attachment":{"type":"instructions","files":[{"path":"/Users/x/.claude/CLAUDE.md","content":"MEMORYWORD rule"}]}}"#;
        let snapshot = r#"{"type":"attachment","attachment":{"type":"prompt_snapshot","systemPrompt":["PROMPTWORD"]}}"#;
        let queued = r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"QUEUEDWORD please"}}"#;
        let result = r#"{"type":"attachment","attachment":{"type":"structured_output","data":{"verdict":"VERDICTWORD"}}}"#;
        let user = r#"{"type":"user","message":{"content":"USERWORD here"}}"#;
        let data = [memory, snapshot, queued, result, user].join("\n");

        let (text, _) = extract_text(data.as_bytes());
        assert!(!text.contains("MEMORYWORD"), "CLAUDE.md/memory is the same in every session");
        assert!(!text.contains("PROMPTWORD"), "system prompt is the same in every session");
        assert!(text.contains("QUEUEDWORD"), "a prompt typed mid-run is the user's");
        assert!(text.contains("VERDICTWORD"), "subagent results belong to this session");
        assert!(text.contains("USERWORD"));

        assert!(is_boilerplate_line(memory));
        assert!(is_boilerplate_line(snapshot));
        assert!(!is_boilerplate_line(queued));
        assert!(!is_boilerplate_line(user));
    }

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
