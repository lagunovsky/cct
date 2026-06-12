//! Multi-index query, snippet generation, and session grouping for `cct search`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

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
    rewrite_slash_command,
};

use super::build::{read_file_range, stored_string, stored_u64};
use super::catalog::{catalog_path, read_catalog, CatalogDisplayStats};
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

#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayStats {
    pub prompts: u32,
    pub tools: u32,
    pub bytes: u64,
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
    pub display_stats: Option<DisplayStats>,
    pub title: Option<String>,
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
    let mut kinds = select_indexes(opts.mode, &opts.query);
    // The ngram(3,3) tokenizer yields no tokens for queries shorter than three
    // chars; drop such subindexes instead of letting build_query fail the
    // whole search (e.g. `c/` enables Path in auto mode).
    kinds.retain(|kind| {
        query_tokens(*kind, &opts.query)
            .map(|tokens| !tokens.is_empty())
            .unwrap_or(true)
    });
    if kinds.is_empty() {
        bail!("query contains no searchable tokens");
    }

    let mut hits: Vec<DocHit> = Vec::new();
    let limit = candidate_limit(opts.limit);
    let project_id = opts
        .project_prefix
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let delta = delta_dir(index_dir);
    let roots = [index_dir.to_path_buf(), delta];
    for root in roots {
        for kind in &kinds {
            let dir = subindex_dir(&root, *kind);
            if !dir.exists() {
                continue;
            }
            let rows = search_one_index(&root, *kind, &opts.query, project_id.as_deref(), limit)
                .with_context(|| format!("search {} index at {}", kind.as_str(), dir.display()))?;
            hits.extend(rows);
        }
    }

    if let Some(prefix) = &opts.project_prefix {
        hits.retain(|h| h.path.starts_with(prefix));
    }

    let groups = group_hits_by_file(hits);
    let catalog = load_catalog_render(index_dir)?;
    let by_session = build_session_results(
        groups,
        &opts.query,
        opts.context_chars,
        opts.fuzzy,
        catalog.as_ref(),
    );

    let mut sessions: Vec<SessionResult> = by_session.into_values().collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_msg_ms));
    Ok(sessions)
}

fn candidate_limit(user_limit: usize) -> usize {
    user_limit.saturating_mul(8).max(user_limit.max(64))
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
    catalog: Option<&CatalogRender>,
) -> BTreeMap<String, SessionResult> {
    const MAX_SNIPPETS_PER_SESSION: usize = 10;

    // Phase 1 — generate snippets per file in parallel. Each `(path, hits)`
    // entry is independent: it does its own disk read + JSON parse + scan.
    // With many matched files this is the dominant cost; the merge in phase 2
    // is cheap and stays single-threaded to preserve dedup/cap semantics.
    let entries: Vec<(PathBuf, Vec<DocHit>)> = by_file.into_iter().collect();
    let snippets = generate_snippets_parallel(&entries, query, context_chars, fuzzy);

    let mut by_session: BTreeMap<String, SessionResult> = BTreeMap::new();
    for ((path, _hits), file_snippets) in entries.into_iter().zip(snippets) {
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
                display_stats: catalog.map(|_| DisplayStats::default()),
                title: None,
            });
        entry.sources.insert(source);
        entry.files.insert(path.clone());
        apply_catalog_display_stats(entry, &path, catalog.map(|c| &c.stats));
        if let Some(catalog) = catalog {
            if entry.title.is_none() {
                if let Some(title) = catalog.titles.get(&path) {
                    entry.title = Some(title.clone());
                }
            }
        }
        if entry.project == "?" && project != "?" {
            entry.project = project;
        }

        for s in file_snippets {
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

/// Per-file display data read once from the catalog: cached prompt/tool/byte
/// stats and the user-assigned session title. Lets the renderer skip re-reading
/// jsonl files.
struct CatalogRender {
    stats: BTreeMap<PathBuf, CatalogDisplayStats>,
    titles: BTreeMap<PathBuf, String>,
}

fn load_catalog_render(index_dir: &Path) -> Result<Option<CatalogRender>> {
    if !catalog_path(index_dir).exists() {
        return Ok(None);
    }
    let mut stats = BTreeMap::new();
    let mut titles = BTreeMap::new();
    for entry in read_catalog(index_dir)? {
        if let Some(s) = entry.display_stats() {
            stats.insert(entry.path.clone(), s);
        }
        if let Some(title) = entry.title {
            titles.insert(entry.path, title);
        }
    }
    Ok(Some(CatalogRender { stats, titles }))
}

fn apply_catalog_display_stats(
    session: &mut SessionResult,
    path: &Path,
    catalog_stats: Option<&BTreeMap<PathBuf, CatalogDisplayStats>>,
) {
    if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
        return;
    }
    let Some(stats_by_path) = catalog_stats else {
        session.display_stats = None;
        return;
    };
    let Some(stats) = stats_by_path.get(path) else {
        session.display_stats = None;
        return;
    };
    let Some(display_stats) = session.display_stats.as_mut() else {
        return;
    };
    display_stats.prompts = display_stats.prompts.max(stats.prompts);
    display_stats.tools = display_stats.tools.max(stats.tools);
    display_stats.bytes = display_stats.bytes.max(stats.bytes);
}

fn generate_snippets_parallel(
    entries: &[(PathBuf, Vec<DocHit>)],
    query: &str,
    context_chars: usize,
    fuzzy: bool,
) -> Vec<Vec<Snippet>> {
    let n = entries.len();
    if n <= 1 {
        return entries
            .iter()
            .map(|(p, h)| snippets_for_hits(p, h, query, context_chars, fuzzy))
            .collect();
    }
    let workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .min(8)
        .min(n)
        .max(1);
    if workers == 1 {
        return entries
            .iter()
            .map(|(p, h)| snippets_for_hits(p, h, query, context_chars, fuzzy))
            .collect();
    }

    let mut out: Vec<Vec<Snippet>> = (0..n).map(|_| Vec::new()).collect();
    let counter = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let counter = &counter;
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(move || -> Vec<(usize, Vec<Snippet>)> {
                let mut local = Vec::new();
                loop {
                    let i = counter.fetch_add(1, Ordering::Relaxed);
                    if i >= entries.len() {
                        break;
                    }
                    let (path, hits) = &entries[i];
                    local.push((
                        i,
                        snippets_for_hits(path, hits, query, context_chars, fuzzy),
                    ));
                }
                local
            }));
        }
        for h in handles {
            // Propagate worker panics rather than silently dropping the slot —
            // empty results would otherwise look like a session with no matches.
            let local = h.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
            for (i, s) in local {
                out[i] = s;
            }
        }
    });
    out
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
        let mut n = vec![query.to_string()];
        // Word index tokenizes "/research" as ["research"], so a doc may match
        // without the literal slash being on any single line (e.g. a Claude
        // slash command mentioned as plain "research"). For single-token
        // queries wrapped in non-word punctuation, accept the bare token too.
        let tokens: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.chars().count() >= 2)
            .collect();
        if tokens.len() == 1 && tokens[0] != query {
            n.push(tokens[0].to_string());
        }
        n
    };
    // The line gate scans the RAW jsonl line, where the needle may appear
    // JSON-escaped (`say "hi"` is stored as `say \"hi\"`), so each needle
    // also matches via its escaped form. Snippet extraction below still uses
    // the original needle against decoded JSON values.
    let needle_variants: Vec<Vec<String>> = needles
        .iter()
        .map(|n| {
            let lower = n.to_lowercase();
            let escaped = json_escape(&lower);
            let mut variants = vec![lower];
            if escaped != variants[0] {
                variants.push(escaped);
            }
            variants
        })
        .collect();
    let mut budget = 6usize;

    for hit in hits {
        if budget == 0 {
            break;
        }
        let Some((start, end)) = hit.snippet_range else {
            // Path-index hits carry no byte range; surface the matched file
            // path itself so the session isn't dropped as snippet-less.
            let text = path.display().to_string();
            if !out.iter().any(|existing: &Snippet| existing.text == text) {
                out.push(Snippet {
                    tag: "path".to_string(),
                    text,
                    time_ms: 0,
                });
                budget = budget.saturating_sub(1);
            }
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
            if !needle_variants
                .iter()
                .flatten()
                .any(|n| lower.contains(n.as_str()))
            {
                continue;
            }
            let abs = start as usize + line_start;
            if !seen_lines.insert(abs) {
                continue;
            }
            let tag = classify_line(line);
            let ts = extract_top_level_timestamp_ms(line);
            for (needle, variants) in needles.iter().zip(needle_variants.iter()) {
                if !variants.iter().any(|v| lower.contains(v.as_str())) {
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

/// How `text` looks inside a JSON string literal, without the surrounding quotes.
fn json_escape(text: &str) -> String {
    let quoted = serde_json::to_string(text).unwrap_or_default();
    quoted
        .strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .unwrap_or(text)
        .to_string()
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
        let value = match rewrite_slash_command(&value) {
            Some(rewritten) => rewritten,
            None if is_meta_content(&value) => continue,
            None => value,
        };
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
            .trim_start_matches(['.', ',', ';', ':', '!', '?', ')'])
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
            .trim_end_matches(['.', ',', ';', ':', '!', '?', '('])
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
    project_id: Option<&str>,
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
    let query = build_query(kind, &fields, query_text, project_id)?;
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
    fields: &super::schema::Fields,
    query: &str,
    project_id: Option<&str>,
) -> Result<Box<dyn Query>> {
    let tokens = query_tokens(kind, query)?;
    if tokens.is_empty() {
        bail!(
            "{} mode could not extract any searchable tokens from query",
            kind.as_str()
        );
    }
    let mut clauses: Vec<(Occur, Box<dyn Query>)> = tokens
        .into_iter()
        .map(|t| {
            let term = Term::from_field_text(fields.text, &t);
            let q: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
            (Occur::Must, q)
        })
        .collect();
    if let Some(project_id) = project_id {
        let term = Term::from_field_text(fields.project_id, project_id);
        let q: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
        clauses.push((Occur::Must, q));
    }
    Ok(Box::new(BooleanQuery::new(clauses)))
}

fn query_tokens(kind: IndexKind, query: &str) -> Result<Vec<String>> {
    match kind {
        IndexKind::Word => token_text(WORD_TOKENIZER, query),
        IndexKind::Trigram | IndexKind::Path => token_text(NGRAM_TOKENIZER, query),
    }
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
    use crate::term::{BOLD, GREEN};
    let mut stats = String::new();
    if prompts > 0 || bytes > 0 {
        stats = format!(
            "  {DIM}{prompts} prompts · {tools} tools · {size}{RESET}",
            size = fmt_size(bytes)
        );
    }
    let title = match &s.title {
        Some(t) if !t.is_empty() => format!("  {GREEN}{BOLD}{t}{RESET}"),
        _ => String::new(),
    };
    println!(
        "\n{CYAN}{BOLD}{id}{RESET}{title}  {DIM}{project}{RESET}{stats}",
        id = s.session_id,
        project = s.project,
    );
}

/// Three-tier fallback used by the Go tool: prompts/answers first, then any
/// "other" tagged snippets, finally agent/hook (only when verbose).
pub fn select_visible_snippets(snippets: &[Snippet], verbose: bool) -> Vec<&Snippet> {
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
        // Mirror the needle-expansion in `snippets_for_hits`: when "/research"
        // matches lines via the bare "research" token, highlight that too.
        let tokens: Vec<&str> = query
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|t| t.chars().count() >= 2)
            .collect();
        if tokens.len() == 1 && tokens[0] != query {
            vec![query.to_string(), tokens[0].to_string()]
        } else {
            Vec::new()
        }
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
        let highlighted = if !token_refs.is_empty() {
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
