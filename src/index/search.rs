//! Multi-index query, snippet generation, and session grouping for `cct search`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, SimpleTokenizer, TextAnalyzer};
use tantivy::{Index, TantivyDocument, Term};

use crate::path_codec::parse_file_path;
use crate::term::{fmt_size, highlight, CYAN, DIM, GRAY, RESET};
use crate::text::{
    classify_line, extract_top_level_timestamp_ms, find_json_values, is_meta_content,
};

use super::build::{read_file_range, stored_string, stored_u64};
use super::schema::{
    delta_dir, fields_from_schema, register_tokenizers, subindex_dir, IndexKind, NGRAM_TOKENIZER,
    WORD_TOKENIZER,
};

#[derive(Clone, Copy, Debug)]
pub enum SearchMode {
    Auto,
    All,
    Only(IndexKind),
}

#[derive(Clone, Debug)]
pub struct DocHit {
    #[allow(dead_code)]
    pub kind: IndexKind,
    pub score: f32,
    pub path: PathBuf,
    #[allow(dead_code)]
    pub file_id: u64,
    #[allow(dead_code)]
    pub chunk_id: u64,
    pub snippet_range: Option<(u64, u64)>,
}

pub struct SearchOptions {
    pub query: String,
    pub mode: SearchMode,
    pub limit: usize,
    pub context_chars: usize,
    pub project_prefix: Option<PathBuf>,
    pub fuzzy: bool,
}

#[derive(Clone, Debug)]
pub struct Snippet {
    pub tag: String,
    pub text: String,
    pub time_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SessionResult {
    pub session_id: String,
    pub project: String,
    pub sources: BTreeSet<String>,
    pub files: BTreeSet<PathBuf>,
    pub snippets: Vec<Snippet>,
    pub last_msg_ms: i64,
}

pub fn select_indexes(mode: SearchMode, query: &str) -> Vec<IndexKind> {
    match mode {
        SearchMode::Only(k) => vec![k],
        SearchMode::All => vec![IndexKind::Word, IndexKind::Trigram, IndexKind::Path],
        SearchMode::Auto => {
            let mut out = vec![IndexKind::Word];
            if query.chars().count() >= 3 && looks_technical(query) {
                out.push(IndexKind::Trigram);
            }
            if looks_pathish(query) {
                out.push(IndexKind::Path);
            }
            out
        }
    }
}

pub fn looks_technical(q: &str) -> bool {
    let mut letters = 0usize;
    let mut upper = 0usize;
    let mut has_lower = false;
    let mut has_upper = false;
    let mut has_non_ascii = false;
    let mut has_tech_char = false;
    for ch in q.chars() {
        has_non_ascii |= !ch.is_ascii();
        has_tech_char |= ch.is_ascii_digit() || matches!(ch, '_' | '-' | ':' | '.' | '#' | '@');
        if ch.is_alphabetic() {
            letters += 1;
            if ch.is_lowercase() {
                has_lower = true;
            }
            if ch.is_uppercase() {
                has_upper = true;
                upper += 1;
            }
        }
    }
    let acronym = letters >= 2 && upper == letters;
    (has_lower && has_upper) || acronym || has_non_ascii || has_tech_char
}

pub fn looks_pathish(q: &str) -> bool {
    q.chars().any(|c| matches!(c, '/' | '\\' | '~'))
}

pub fn run_search(index_dir: &Path, opts: &SearchOptions) -> Result<Vec<SessionResult>> {
    if opts.query.trim().is_empty() {
        bail!("empty query");
    }
    let kinds = select_indexes(opts.mode, &opts.query);

    let mut hits: Vec<DocHit> = Vec::new();
    let total_limit = candidate_limit(opts.limit, opts.project_prefix.is_some());
    let delta = delta_dir(index_dir);
    let roots = [index_dir.to_path_buf(), delta];
    for root in roots {
        for kind in &kinds {
            if !subindex_dir(&root, *kind).exists() {
                continue;
            }
            match search_one_index(&root, *kind, &opts.query, total_limit) {
                Ok(rows) => hits.extend(rows),
                Err(err) => eprintln!("  warning: {} search failed: {err:#}", kind.as_str()),
            }
        }
    }

    if let Some(prefix) = &opts.project_prefix {
        hits.retain(|h| h.path.starts_with(prefix));
    }

    let groups = group_hits_by_file(hits);
    let by_session = build_session_results(groups, &opts.query, opts.context_chars, opts.fuzzy);

    let mut sessions: Vec<SessionResult> = by_session.into_values().collect();
    sessions.sort_by(|a, b| b.last_msg_ms.cmp(&a.last_msg_ms));
    Ok(sessions)
}

fn candidate_limit(user_limit: usize, scoped_project: bool) -> usize {
    let base = user_limit.saturating_mul(8).max(user_limit.max(64));
    if scoped_project {
        base.max(10_000)
    } else {
        base
    }
}

fn group_hits_by_file(hits: Vec<DocHit>) -> BTreeMap<PathBuf, Vec<DocHit>> {
    let mut by_file: BTreeMap<PathBuf, Vec<DocHit>> = BTreeMap::new();
    for hit in hits {
        by_file.entry(hit.path.clone()).or_default().push(hit);
    }
    for hits in by_file.values_mut() {
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    by_file
}

fn build_session_results(
    by_file: BTreeMap<PathBuf, Vec<DocHit>>,
    query: &str,
    context_chars: usize,
    fuzzy: bool,
) -> BTreeMap<String, SessionResult> {
    const MAX_SNIPPETS_PER_SESSION: usize = 10;
    let mut by_session: BTreeMap<String, SessionResult> = BTreeMap::new();

    for (path, hits) in by_file {
        let (session_id, project, source) = parse_file_path(&path);
        let entry = by_session
            .entry(session_id.clone())
            .or_insert_with(|| SessionResult {
                session_id: session_id.clone(),
                project: project.clone(),
                sources: BTreeSet::new(),
                files: BTreeSet::new(),
                snippets: Vec::new(),
                last_msg_ms: 0,
            });
        entry.sources.insert(source);
        entry.files.insert(path.clone());
        if entry.project == "?" && project != "?" {
            entry.project = project;
        }

        let snippets = snippets_for_hits(&path, &hits, query, context_chars, fuzzy);
        for s in snippets {
            if entry.snippets.len() >= MAX_SNIPPETS_PER_SESSION {
                break;
            }
            if entry
                .snippets
                .iter()
                .any(|existing| existing.text == s.text)
            {
                continue;
            }
            if s.time_ms > entry.last_msg_ms {
                entry.last_msg_ms = s.time_ms;
            }
            entry.snippets.push(s);
        }

        if entry.last_msg_ms == 0 {
            if let Ok(meta) = fs::metadata(&path) {
                if let Ok(modified) = meta.modified() {
                    if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                        entry.last_msg_ms = d.as_millis() as i64;
                    }
                }
            }
        }
    }
    by_session
}

fn snippets_for_hits(
    path: &Path,
    hits: &[DocHit],
    query: &str,
    context_chars: usize,
    fuzzy: bool,
) -> Vec<Snippet> {
    let mut out = Vec::new();
    let mut seen_lines = BTreeSet::new();
    let needles: Vec<String> = if fuzzy {
        let mut tokens: Vec<String> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.chars().count() >= 2)
            .map(|s| s.to_string())
            .collect();
        if tokens.is_empty() {
            tokens.push(query.to_string());
        }
        tokens
    } else {
        vec![query.to_string()]
    };
    let needles_lower: Vec<String> = needles.iter().map(|n| n.to_lowercase()).collect();
    let mut budget = 6usize;

    for hit in hits {
        if budget == 0 {
            break;
        }
        let Some((start, end)) = hit.snippet_range else {
            continue;
        };
        let Ok(bytes) = read_file_range(path, start, end) else {
            continue;
        };
        let Ok(text) = std::str::from_utf8(&bytes) else {
            continue;
        };
        let lines = split_keep_offsets(text);
        for (line_start, line) in lines {
            let lower = line.to_lowercase();
            if !needles_lower.iter().any(|n| lower.contains(n)) {
                continue;
            }
            let abs = start as usize + line_start;
            if !seen_lines.insert(abs) {
                continue;
            }
            let tag = classify_line(line);
            let ts = extract_top_level_timestamp_ms(line);
            for (needle, needle_lower) in needles.iter().zip(needles_lower.iter()) {
                if !lower.contains(needle_lower) {
                    continue;
                }
                for snippet_text in extract_all_snippets(line, needle, context_chars, budget) {
                    if out
                        .iter()
                        .any(|existing: &Snippet| existing.text == snippet_text)
                    {
                        continue;
                    }
                    out.push(Snippet {
                        tag: tag.clone(),
                        text: snippet_text,
                        time_ms: ts,
                    });
                    budget = budget.saturating_sub(1);
                    if budget == 0 {
                        break;
                    }
                }
                if budget == 0 {
                    break;
                }
            }
            if budget == 0 {
                break;
            }
        }
    }
    out
}

fn split_keep_offsets(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            out.push((start, &text[start..i]));
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push((start, &text[start..]));
    }
    out
}

fn extract_all_snippets(
    line: &str,
    query: &str,
    context_chars: usize,
    max_count: usize,
) -> Vec<String> {
    let q_lower = query.to_lowercase();
    let mut values = find_json_values(line, &q_lower);
    if values.is_empty() {
        let mut clean = line.replace("\\n", " ");
        clean = clean.replace("\\t", " ");
        clean = clean.replace("\\\"", "\"");
        clean = clean.replace("\\\\", "\\");
        values = vec![clean];
    }

    let mut results: Vec<String> = Vec::new();
    'outer: for value in values {
        if is_meta_content(&value) {
            continue;
        }
        let (lower, spans) = lowercase_with_spans(&value);
        let mut search_from = 0usize;
        while results.len() < max_count {
            let Some(rel) = lower[search_from..].find(&q_lower) else {
                break;
            };
            let idx = search_from + rel;
            let match_end = idx + q_lower.len();
            let Some((orig_start, orig_end)) =
                original_span_for_lower_match(&spans, idx, match_end)
            else {
                search_from = match_end;
                continue;
            };
            let snippet = snippet_window(&value, orig_start, orig_end - orig_start, context_chars);
            if !snippet.is_empty() && !results.iter().any(|existing| existing == &snippet) {
                results.push(snippet);
                if results.len() >= max_count {
                    break 'outer;
                }
            }
            search_from = match_end;
        }
    }
    results
}

#[derive(Clone, Copy)]
struct LowerSpan {
    lower_start: usize,
    lower_end: usize,
    original_start: usize,
    original_end: usize,
}

fn lowercase_with_spans(text: &str) -> (String, Vec<LowerSpan>) {
    let mut lower = String::with_capacity(text.len());
    let mut spans = Vec::new();
    for (original_start, ch) in text.char_indices() {
        let original_end = original_start + ch.len_utf8();
        let lower_start = lower.len();
        for folded in ch.to_lowercase() {
            lower.push(folded);
        }
        spans.push(LowerSpan {
            lower_start,
            lower_end: lower.len(),
            original_start,
            original_end,
        });
    }
    (lower, spans)
}

fn original_span_for_lower_match(
    spans: &[LowerSpan],
    lower_start: usize,
    lower_end: usize,
) -> Option<(usize, usize)> {
    let mut original_start = None;
    let mut original_end = None;
    for span in spans {
        if span.lower_end <= lower_start {
            continue;
        }
        if span.lower_start >= lower_end {
            break;
        }
        if original_start.is_none() {
            original_start = Some(span.original_start);
        }
        original_end = Some(span.original_end);
    }
    Some((original_start?, original_end?))
}

fn snippet_window(text: &str, match_idx: usize, match_len: usize, context_chars: usize) -> String {
    let suffix_byte_start = match_idx + match_len;

    let before_char_count = text[..match_idx].chars().count();
    let after_char_count = text[suffix_byte_start..].chars().count();
    let take_before = context_chars.min(before_char_count);
    let take_after = context_chars.min(after_char_count);
    let truncated_left = before_char_count > take_before;
    let truncated_right = after_char_count > take_after;

    let mut before_chars: Vec<char> = text[..match_idx].chars().rev().take(take_before).collect();
    before_chars.reverse();
    let after_chars: String = text[suffix_byte_start..].chars().take(take_after).collect();
    let middle = &text[match_idx..suffix_byte_start];
    let snippet: String = before_chars.into_iter().collect::<String>() + middle + &after_chars;

    let mut sanitized = String::with_capacity(snippet.len());
    let mut prev_space = false;
    for ch in snippet.chars() {
        if (ch as u32) < 32 || ch == '{' || ch == '}' {
            if !prev_space {
                sanitized.push(' ');
                prev_space = true;
            }
            continue;
        }
        sanitized.push(ch);
        prev_space = ch == ' ';
    }
    let mut result = collapse_long_spaces(sanitized.trim());

    if truncated_left {
        if let Some(sp_byte) = result.find(' ') {
            let prefix_chars = result[..sp_byte].chars().count();
            let total = result.chars().count();
            if prefix_chars > 0 && prefix_chars < total / 3 {
                result = result[sp_byte + 1..].trim().to_string();
            }
        }
        result = result
            .trim_start_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | ')'))
            .trim()
            .to_string();
        result = format!("⋯ {result}");
    }
    if truncated_right {
        if let Some(sp_byte) = result.rfind(' ') {
            let prefix_chars = result[..sp_byte].chars().count();
            let total = result.chars().count();
            if prefix_chars > total * 2 / 3 {
                result = result[..sp_byte].trim().to_string();
            }
        }
        result = result
            .trim_end_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '!' | '?' | '('))
            .trim()
            .to_string();
        result = format!("{result} ⋯");
    }
    result
}

/// Replace runs of 3+ spaces (terminal padding, table layout, etc.) with a
/// single ellipsis so snippets stay readable when the original text is
/// heavily indented. The ellipsis is colorized at print time, not stored
/// inside the snippet text — that way `highlight()` never sees ANSI escapes.
fn collapse_long_spaces(s: &str) -> String {
    const THRESHOLD: usize = 3;
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != ' ' {
            out.push(ch);
            continue;
        }
        let mut run = 1;
        while chars.peek() == Some(&' ') {
            chars.next();
            run += 1;
        }
        if run >= THRESHOLD {
            out.push_str(" ⋯ ");
        } else {
            for _ in 0..run {
                out.push(' ');
            }
        }
    }
    out
}

/// Wrap every literal `⋯` (inserted by `collapse_long_spaces` /
/// `snippet_window`) in dim ANSI codes. Done after `highlight()` so the
/// highlighter never sees escape sequences and risks matching them.
pub fn dim_ellipses(text: &str) -> String {
    text.replace('⋯', &format!("{GRAY}⋯{RESET}"))
}

fn search_one_index(
    root: &Path,
    kind: IndexKind,
    query_text: &str,
    limit: usize,
) -> Result<Vec<DocHit>> {
    let dir = subindex_dir(root, kind);
    let index =
        Index::open_in_dir(&dir).with_context(|| format!("open index {}", dir.display()))?;
    register_tokenizers(&index)?;
    let schema = index.schema();
    let fields = fields_from_schema(&schema)?;
    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query = build_query(kind, fields.text, query_text)?;
    let top = searcher.search(&query, &TopDocs::with_limit(limit).order_by_score())?;

    let mut hits = Vec::new();
    for (score, address) in top {
        let doc = searcher.doc::<TantivyDocument>(address)?;
        let path_str = stored_string(&doc, fields.path).unwrap_or_default();
        let snippet_range = if kind == IndexKind::Path {
            None
        } else {
            stored_u64(&doc, fields.start_offset).zip(stored_u64(&doc, fields.end_offset))
        };
        let file_id = stored_u64(&doc, fields.file_id).unwrap_or(0);
        let chunk_id = stored_u64(&doc, fields.chunk_id).unwrap_or(0);
        hits.push(DocHit {
            kind,
            score,
            path: PathBuf::from(path_str),
            file_id,
            chunk_id,
            snippet_range,
        });
    }
    Ok(hits)
}

fn build_query(
    kind: IndexKind,
    field: tantivy::schema::Field,
    query: &str,
) -> Result<Box<dyn Query>> {
    let tokens = match kind {
        IndexKind::Word => token_text(WORD_TOKENIZER, query)?,
        IndexKind::Trigram | IndexKind::Path => token_text(NGRAM_TOKENIZER, query)?,
    };
    if tokens.is_empty() {
        bail!(
            "{} mode could not extract any searchable tokens from query",
            kind.as_str()
        );
    }
    let clauses = tokens
        .into_iter()
        .map(|t| {
            let term = Term::from_field_text(field, &t);
            let q: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            (Occur::Must, q)
        })
        .collect();
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn token_text(name: &str, text: &str) -> Result<Vec<String>> {
    let mut analyzer = match name {
        WORD_TOKENIZER => TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(LowerCaser)
            .build(),
        NGRAM_TOKENIZER => TextAnalyzer::builder(NgramTokenizer::new(3, 3, false)?)
            .filter(LowerCaser)
            .build(),
        _ => bail!("unknown tokenizer {name}"),
    };
    let mut stream = analyzer.token_stream(text);
    let mut out = BTreeSet::new();
    while let Some(token) = stream.next() {
        out.insert(token.text.clone());
    }
    Ok(out.into_iter().collect())
}

/// Header line printed for each result before its snippets.
pub fn print_session_header(s: &SessionResult, prompts: u32, tools: u32, bytes: u64) {
    use crate::term::BOLD;
    let mut stats = String::new();
    if prompts > 0 || bytes > 0 {
        stats = format!(
            "  {DIM}{prompts} prompts · {tools} tools · {size}{RESET}",
            size = fmt_size(bytes)
        );
    }
    println!(
        "\n{CYAN}{BOLD}{id}{RESET}  {DIM}{project}{RESET}{stats}",
        id = s.session_id,
        project = s.project,
    );
}

/// Three-tier fallback used by the Go tool: prompts/answers first, then any
/// "other" tagged snippets, finally agent/hook (only when verbose).
pub fn select_visible_snippets<'a>(snippets: &'a [Snippet], verbose: bool) -> Vec<&'a Snippet> {
    let mut primary = Vec::new();
    let mut secondary = Vec::new();
    let mut tertiary = Vec::new();
    for s in snippets {
        match s.tag.as_str() {
            "prompt" | "answer" => primary.push(s),
            "agent" | "hook" => tertiary.push(s),
            _ => secondary.push(s),
        }
    }
    if verbose {
        primary
            .into_iter()
            .chain(secondary)
            .chain(tertiary)
            .collect()
    } else if !primary.is_empty() {
        primary
    } else if !secondary.is_empty() {
        secondary
    } else {
        tertiary
    }
}

/// Print snippets respecting the three-tier fallback used by the Go tool.
pub fn print_snippets(snippets: &[Snippet], query: &str, verbose: bool, fuzzy: bool) {
    use crate::term::highlight_any;
    let show = select_visible_snippets(snippets, verbose);
    let token_strings: Vec<String> = if fuzzy {
        query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.chars().count() >= 2)
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };
    let token_refs: Vec<&str> = token_strings.iter().map(|s| s.as_str()).collect();

    for (j, s) in show.iter().enumerate() {
        if j >= 5 {
            break;
        }
        let ts = if s.time_ms > 0 {
            chrono::DateTime::from_timestamp_millis(s.time_ms)
                .map(|d| {
                    d.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M ")
                        .to_string()
                })
                .unwrap_or_default()
        } else {
            String::new()
        };
        let highlighted = if fuzzy && !token_refs.is_empty() {
            highlight_any(&s.text, &token_refs)
        } else {
            highlight(&s.text, query)
        };
        let highlighted = dim_ellipses(&highlighted);
        if !s.tag.is_empty() {
            println!(
                "  {DIM}{ts}[{tag}]{RESET} {text}",
                tag = s.tag,
                text = highlighted,
            );
        } else {
            println!("  {DIM}{ts}{RESET}{text}", text = highlighted);
        }
    }
}
