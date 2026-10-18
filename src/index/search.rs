//! Multi-index query, snippet generation, and session grouping for `cct search`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, Query, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, TantivyDocument, Term};

use crate::path_codec::parse_file_path;
use crate::term::{fmt_size, highlight, CYAN, DIM, GRAY, RESET};
use crate::text::{
    classify_line, extract_top_level_timestamp_ms, find_json_values, is_boilerplate_line,
    is_meta_content, rewrite_slash_command,
};

use super::build::{read_file_range, stored_string, stored_u64};
use super::catalog::{catalog_path, read_catalog, CatalogDisplayStats};
use super::schema::{
    delta_dir, fields_from_schema, is_oversized_word_token, ngram_analyzer, register_tokenizers,
    subindex_dir, word_analyzer, IndexKind, NGRAM_TOKENIZER, WORD_TOKENIZER,
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
    /// Query strings. A session must match every one of them (AND); the
    /// strings may match in different places — even different files — of the
    /// session. Tokens inside one string keep the tighter same-chunk AND.
    pub queries: Vec<String>,
    pub mode: SearchMode,
    pub limit: usize,
    pub context_chars: usize,
    pub project_prefix: Option<PathBuf>,
    pub fuzzy: bool,
    pub sort: SortBy,
}

/// What decides the order of the sessions handed back.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SortBy {
    /// Relevance: how well and how often the session matched, damped by age.
    #[default]
    Relevance,
    /// Newest session first, regardless of match quality.
    Time,
}

/// Per-index weight applied to a normalized hit score. A word hit is the query
/// as typed; a trigram hit may be an accidental substring; a path hit says only
/// that the file is named like the query, not that anything was discussed.
fn index_weight(kind: IndexKind) -> f32 {
    match kind {
        IndexKind::Word => 1.0,
        IndexKind::Trigram => 0.7,
        IndexKind::Path => 0.4,
    }
}

/// A match the user typed themselves outranks one buried in tool output — the
/// same 1.3 deja gives user messages.
const PROMPT_BOOST: f32 = 1.3;

/// Age at which a session's score is halved.
///
/// deja uses `1/(1+age_days)` — a week-old session is already worth an eighth,
/// so over a corpus spanning years the ranking collapses back into "newest
/// first" with extra steps. Six months keeps recency as a tiebreaker between
/// comparable matches while a genuinely better old session can still win, which
/// is the whole point of asking "how did we solve this".
const RECENCY_HALF_LIFE_DAYS: f64 = 180.0;

fn recency_factor(last_msg_ms: i64, now_ms: i64) -> f32 {
    if last_msg_ms <= 0 {
        return 1.0;
    }
    let age_days = ((now_ms - last_msg_ms).max(0) as f64) / 86_400_000.0;
    (1.0 / (1.0 + age_days / RECENCY_HALF_LIFE_DAYS)) as f32
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DisplayStats {
    pub prompts: u32,
    pub tools: u32,
    pub bytes: u64,
}

const MAX_SNIPPETS_PER_SESSION: usize = 10;
/// Display cap: how many of a session's snippets the renderers show.
const MAX_SNIPPETS_SHOWN: usize = 5;

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
    /// The session lives in the zstd archive, not under `~/.claude/projects`.
    pub archived: bool,
    /// Relevance before the age damping, summed over the query strings.
    /// Meaningless across searches — only the order within one is.
    pub score: f32,
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

/// True when the query holds a run of alphanumerics longer than the word index
/// stores in full — a key, a hash, a base64 chunk. It still matches, through
/// its truncated prefix; the snippet pass is what confirms the whole string.
pub fn has_oversized_token(q: &str) -> bool {
    q.split(|c: char| !c.is_alphanumeric())
        .any(is_oversized_word_token)
}

pub fn run_search(index_dir: &Path, opts: &SearchOptions) -> Result<Vec<SessionResult>> {
    let queries: Vec<&str> = opts
        .queries
        .iter()
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
        .collect();
    if queries.is_empty() {
        bail!("empty query");
    }

    let limit = candidate_limit(opts.limit);
    let project_id = opts
        .project_prefix
        .as_ref()
        .map(|path| path.to_string_lossy().into_owned());
    let delta = delta_dir(index_dir);
    let roots = [index_dir.to_path_buf(), delta];

    // Phase 1: index hits per query string, kept separate — the AND below is
    // at session level, not chunk level, so the strings must not be merged
    // into one boolean query.
    let mut per_query_groups: Vec<BTreeMap<PathBuf, Vec<DocHit>>> = Vec::new();
    for query in &queries {
        let mut kinds = select_indexes(opts.mode, query);
        // The ngram(3,3) tokenizer yields no tokens for queries shorter than
        // three chars; drop such subindexes instead of letting build_query
        // fail the whole search (e.g. `c/` enables Path in auto mode).
        kinds.retain(|kind| {
            query_tokens(*kind, query)
                .map(|tokens| !tokens.is_empty())
                .unwrap_or(true)
        });
        if kinds.is_empty() {
            bail!("no searchable tokens in {query:?}");
        }
        let mut hits: Vec<DocHit> = Vec::new();
        for root in &roots {
            for kind in &kinds {
                let dir = subindex_dir(root, *kind);
                if !dir.exists() {
                    continue;
                }
                let rows = search_one_index(root, *kind, query, project_id.as_deref(), limit)
                    .with_context(|| {
                        format!("search {} index at {}", kind.as_str(), dir.display())
                    })?;
                hits.extend(rows);
            }
        }
        if let Some(prefix) = &opts.project_prefix {
            hits.retain(|h| h.path.starts_with(prefix));
        }
        per_query_groups.push(group_hits_by_file(hits));
    }

    // Phase 2: AND across query strings — a session survives only when every
    // string matched somewhere in it. Files of dropped sessions are pruned
    // before the expensive snippet phase.
    if per_query_groups.len() > 1 {
        let session_sets: Vec<BTreeSet<String>> = per_query_groups
            .iter()
            .map(|groups| groups.keys().map(|p| parse_file_path(p).0).collect())
            .collect();
        let mut alive = session_sets[0].clone();
        for set in &session_sets[1..] {
            alive.retain(|id| set.contains(id));
        }
        for groups in &mut per_query_groups {
            groups.retain(|path, _| alive.contains(&parse_file_path(path).0));
        }
    }

    // Phase 3: snippets and session assembly per query string, then merge.
    let catalog = load_catalog_render(index_dir)?;
    let mut merged: BTreeMap<String, SessionResult> = BTreeMap::new();
    // Per-session snippet lists, one slot per query string, so the final list
    // can interleave them — appending would let the first string's snippets
    // crowd the later ones out of the per-session cap entirely.
    let mut snippet_rounds: BTreeMap<String, Vec<Vec<Snippet>>> = BTreeMap::new();
    for (query, groups) in queries.iter().zip(per_query_groups) {
        let mut by_session =
            build_session_results(groups, query, opts.context_chars, opts.fuzzy, catalog.as_ref());
        if has_oversized_token(query) {
            // The word index stores such a token by its first 32 chars, so two
            // different blobs sharing a prefix land on the same term. Only the
            // snippet pass has seen the whole string; without a snippet the
            // session is an unverified prefix collision, not a match.
            by_session.retain(|_, result| !result.snippets.is_empty());
        }
        for (id, mut result) in by_session {
            snippet_rounds
                .entry(id.clone())
                .or_default()
                .push(std::mem::take(&mut result.snippets));
            match merged.entry(id) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(result);
                }
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    merge_session_meta(slot.get_mut(), result);
                }
            }
        }
    }
    for (id, rounds) in snippet_rounds {
        if let Some(session) = merged.get_mut(&id) {
            session.snippets = interleave_snippets(rounds, MAX_SNIPPETS_PER_SESSION);
        }
    }

    let mut sessions: Vec<SessionResult> = merged.into_values().collect();
    match opts.sort {
        SortBy::Time => sessions.sort_by_key(|s| std::cmp::Reverse(s.last_msg_ms)),
        SortBy::Relevance => {
            let now_ms = chrono::Utc::now().timestamp_millis();
            // The stored score stays age-free so `--json` consumers can see the
            // textual relevance; only the ordering is damped.
            let ranked: BTreeMap<String, f32> = sessions
                .iter()
                .map(|s| {
                    let boost = if s.snippets.iter().any(|sn| sn.tag == "prompt") {
                        PROMPT_BOOST
                    } else {
                        1.0
                    };
                    (
                        s.session_id.clone(),
                        s.score * boost * recency_factor(s.last_msg_ms, now_ms),
                    )
                })
                .collect();
            sessions.sort_by(|a, b| {
                let (ra, rb) = (
                    ranked.get(a.session_id.as_str()).copied().unwrap_or(0.0),
                    ranked.get(b.session_id.as_str()).copied().unwrap_or(0.0),
                );
                rb.partial_cmp(&ra)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(b.last_msg_ms.cmp(&a.last_msg_ms))
            });
        }
    }
    Ok(sessions)
}

/// Fold `add` (the same session matched via another query string) into `into`.
/// Snippets are handled separately in `interleave_snippets`.
fn merge_session_meta(into: &mut SessionResult, add: SessionResult) {
    into.sources.extend(add.sources);
    into.files.extend(add.files);
    into.last_msg_ms = into.last_msg_ms.max(add.last_msg_ms);
    // AND-ed query strings: a session that matched both well ranks above one
    // that scraped through on the second string.
    into.score += add.score;
    into.archived |= add.archived;
    if into.title.is_none() {
        into.title = add.title;
    }
    if into.project == "?" && add.project != "?" {
        into.project = add.project;
    }
    match (into.display_stats.as_mut(), add.display_stats) {
        (Some(a), Some(b)) => {
            a.prompts = a.prompts.max(b.prompts);
            a.tools = a.tools.max(b.tools);
            a.bytes = a.bytes.max(b.bytes);
        }
        (None, Some(b)) => into.display_stats = Some(b),
        _ => {}
    }
}

/// Round-robin over per-query snippet lists so every query string stays
/// represented within the per-session cap; drops cross-list duplicates (one
/// line can match several strings).
fn interleave_snippets(rounds: Vec<Vec<Snippet>>, cap: usize) -> Vec<Snippet> {
    let mut out: Vec<Snippet> = Vec::new();
    let longest = rounds.iter().map(|r| r.len()).max().unwrap_or(0);
    'fill: for i in 0..longest {
        for round in &rounds {
            let Some(s) = round.get(i) else {
                continue;
            };
            if out.iter().any(|existing| existing.text == s.text) {
                continue;
            }
            out.push(s.clone());
            if out.len() >= cap {
                break 'fill;
            }
        }
    }
    out
}

fn candidate_limit(user_limit: usize) -> usize {
    user_limit.saturating_mul(8).max(user_limit.max(64))
}

/// How much a match is worth by where it sits. Tool output and hook chatter
/// repeat a term far more often than the conversation about it does, so an
/// unweighted count would rank a session by how noisy its tools were.
fn source_weight(source: &str) -> f32 {
    match source {
        "session" => 1.0,
        "subagent" => 0.7,
        _ => 0.5,
    }
}

/// A file's contribution to its session's relevance: how many lines matched,
/// weighted by where the file sits and by which index found it. Passed through
/// `ln(1+x)` at the session level, so matching in twenty places beats matching
/// in one without letting a sprawling session win on volume alone.
///
/// Tantivy's own BM25 decides which chunks come back, but cannot rank sessions:
/// a document here is a 512 KB chunk, and BM25's length normalization then
/// cancels out almost exactly the term frequency it is supposed to reward.
fn file_relevance(hits: &[DocHit], source: &str, match_lines: u32) -> f32 {
    let index_weight = hits
        .iter()
        .map(|h| index_weight(h.kind))
        .fold(0.0_f32, f32::max);
    // Path hits carry no byte range and so match no lines; they still say the
    // file is named after the query.
    let lines = match_lines.max(u32::from(hits.iter().all(|h| h.kind == IndexKind::Path)));
    lines as f32 * source_weight(source) * index_weight
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
    // Phase 1 — generate snippets per file in parallel. Each `(path, hits)`
    // entry is independent: it does its own disk read + JSON parse + scan.
    // With many matched files this is the dominant cost; the merge in phase 2
    // is cheap and stays single-threaded to preserve dedup/cap semantics.
    let entries: Vec<(PathBuf, Vec<DocHit>)> = by_file.into_iter().collect();
    let scanned = generate_snippets_parallel(&entries, query, context_chars, fuzzy);

    let mut by_session: BTreeMap<String, SessionResult> = BTreeMap::new();
    for ((path, hits), scan) in entries.into_iter().zip(scanned) {
        let FileScan {
            snippets: file_snippets,
            match_lines,
        } = scan;
        let (session_id, project, source) = parse_file_path(&path);
        let relevance = file_relevance(&hits, &source, match_lines);
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
                archived: false,
                score: 0.0,
            });
        entry.sources.insert(source);
        // Raw sum here; the ln() lands once per session in `finish_scores`.
        entry.score += relevance;
        entry.archived |= !path.exists() && crate::archive::is_archived(&path);
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
            // Archived twins carry over the original mtime, so they serve as
            // the fallback when the live file is gone.
            let meta = fs::metadata(&path).or_else(|_| {
                crate::archive::archive_path_for(&path)
                    .map(fs::metadata)
                    .unwrap_or_else(|| Err(std::io::Error::from(std::io::ErrorKind::NotFound)))
            });
            if let Ok(meta) = meta {
                if let Ok(modified) = meta.modified() {
                    if let Ok(d) = modified.duration_since(std::time::UNIX_EPOCH) {
                        entry.last_msg_ms = d.as_millis() as i64;
                    }
                }
            }
        }
    }
    // Saturate once per session: twenty matches beat one, but not twentyfold.
    for result in by_session.values_mut() {
        result.score = (1.0 + result.score).ln();
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

/// What one file's scan produced: the snippets worth showing, plus how many
/// lines matched in total — the snippet budget stops rendering long before the
/// scan stops counting, and relevance needs the count, not the render.
#[derive(Default)]
pub struct FileScan {
    pub snippets: Vec<Snippet>,
    pub match_lines: u32,
}

/// Ceiling on counted lines per file. A session that mentions the query 300
/// times is not meaningfully more relevant than one that mentions it 200 —
/// and the cap keeps a pathological chunk from scanning forever.
const MATCH_LINE_CAP: u32 = 200;

fn generate_snippets_parallel(
    entries: &[(PathBuf, Vec<DocHit>)],
    query: &str,
    context_chars: usize,
    fuzzy: bool,
) -> Vec<FileScan> {
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

    let mut out: Vec<FileScan> = (0..n).map(|_| FileScan::default()).collect();
    let counter = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let counter = &counter;
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(move || -> Vec<(usize, FileScan)> {
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
) -> FileScan {
    let mut out = Vec::new();
    let mut match_lines = 0u32;
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

    // Archived sessions: the live file is gone — decompress the zstd twin
    // once and serve every hit range from memory.
    let archived_data: Option<Vec<u8>> = if path.exists() {
        None
    } else {
        crate::archive::read_archived(path)
    };

    for hit in hits {
        // The snippet budget only stops rendering; scanning continues until
        // the counted-line cap, because relevance is measured in matches.
        if budget == 0 && match_lines >= MATCH_LINE_CAP {
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
        let range_data: Vec<u8>;
        let raw: &[u8] = if let Some(data) = &archived_data {
            let s = (start as usize).min(data.len());
            let e = (end as usize).min(data.len()).max(s);
            &data[s..e]
        } else {
            let Ok(bytes) = read_file_range(path, start, end) else {
                continue;
            };
            range_data = bytes;
            &range_data
        };
        let Ok(text) = std::str::from_utf8(raw) else {
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
            // Indexes built before boilerplate was excluded still hold it; and
            // a chunk matched on real content must not surface the system
            // prompt or CLAUDE.md as its snippet either.
            if is_boilerplate_line(line) {
                continue;
            }
            match_lines += 1;
            if budget == 0 {
                if match_lines >= MATCH_LINE_CAP {
                    break;
                }
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
        }
    }
    FileScan {
        snippets: out,
        match_lines,
    }
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
    let mut chars = snippet.chars().peekable();
    while let Some(ch) = chars.next() {
        // Session logs quote terminal output, so snippets can contain real
        // ANSI CSI sequences (stored as […). Swallow them whole: mapping
        // just the ESC to a space would leave the tail ("[1m", "[0m") behind
        // as visible garbage. A sequence cut off by the window start still
        // leaks its tail — rare enough to live with.
        if ch == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&c) {
                    break;
                }
            }
            continue;
        }
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
            // Must match how the field was indexed, or the scorer reads no
            // frequencies and falls back to ranking by document length.
            let q: Box<dyn Query> = Box::new(TermQuery::new(term, kind.record_option()));
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
        WORD_TOKENIZER => word_analyzer(),
        NGRAM_TOKENIZER => ngram_analyzer()?,
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
    let archived = if s.archived {
        format!("  {DIM}[archived — cct unarchive {id} to resume]{RESET}", id = s.session_id)
    } else {
        String::new()
    };
    println!(
        "\n{CYAN}{BOLD}{id}{RESET}{title}  {DIM}{project}{RESET}{stats}{archived}",
        id = s.session_id,
        project = s.project,
    );
}

/// One session as a machine-readable JSON value for `--json` output. Mirrors
/// the human renderer's content selection so the flag only changes the format:
/// the same snippet tiers and display cap apply, and `-v` adds agent/hook
/// snippets plus matched file paths. Snippet text is raw (no ANSI
/// highlighting); `last_activity` duplicates `last_msg_ms` as RFC 3339 local
/// time for readability.
pub fn session_json(
    s: &SessionResult,
    prompts: u32,
    tools: u32,
    bytes: u64,
    verbose: bool,
) -> serde_json::Value {
    let last_activity = chrono::DateTime::from_timestamp_millis(s.last_msg_ms)
        .map(|d| d.with_timezone(&chrono::Local).to_rfc3339());
    let snippets = select_visible_snippets(&s.snippets, verbose);
    let mut value = serde_json::json!({
        "session_id": s.session_id,
        "title": s.title,
        "project": s.project,
        "archived": s.archived,
        "last_msg_ms": s.last_msg_ms,
        "last_activity": last_activity,
        "prompts": prompts,
        "tools": tools,
        "bytes": bytes,
        // Relevance before age damping. Comparable within one search only —
        // an agent can tell "clearly the best hit" from "one of twenty".
        "score": (s.score * 1000.0).round() / 1000.0,
        "snippets": snippets.iter().take(MAX_SNIPPETS_SHOWN).map(|sn| serde_json::json!({
            "tag": sn.tag,
            "time_ms": sn.time_ms,
            "text": sn.text,
        })).collect::<Vec<_>>(),
    });
    if verbose {
        value["files"] = serde_json::json!(s
            .files
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>());
    }
    value
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
pub fn print_snippets(snippets: &[Snippet], queries: &[String], verbose: bool, fuzzy: bool) {
    use crate::term::highlight_any;
    let show = select_visible_snippets(snippets, verbose);
    let mut token_strings: Vec<String> = Vec::new();
    for query in queries {
        if fuzzy {
            token_strings.extend(
                query
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .filter(|t| t.chars().count() >= 2)
                    .map(|s| s.to_string()),
            );
        } else {
            // Mirror the needle-expansion in `snippets_for_hits`: when
            // "/research" matches lines via the bare "research" token,
            // highlight that too.
            let tokens: Vec<&str> = query
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .filter(|t| t.chars().count() >= 2)
                .collect();
            if queries.len() > 1 || (tokens.len() == 1 && tokens[0] != query) {
                token_strings.push(query.to_string());
            }
            if tokens.len() == 1 && tokens[0] != query {
                token_strings.push(tokens[0].to_string());
            }
        }
    }
    let single_query = queries.first().map(String::as_str).unwrap_or("");
    let token_refs: Vec<&str> = token_strings.iter().map(|s| s.as_str()).collect();

    for (j, s) in show.iter().enumerate() {
        if j >= MAX_SNIPPETS_SHOWN {
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
            highlight(&s.text, single_query)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_window_swallows_csi_sequences_whole() {
        let text = "\u{1b}[1mSearching for:\u{1b}[0m dnscrypt \u{1b}[2m(global)\u{1b}[0m";
        let idx = text.find("dnscrypt").unwrap();
        let snippet = snippet_window(text, idx, "dnscrypt".len(), 120);
        assert_eq!(snippet, "Searching for: dnscrypt (global)");
    }

    fn sample_session() -> SessionResult {
        SessionResult {
            session_id: "abc-123".to_string(),
            project: "~/Work/x".to_string(),
            sources: BTreeSet::from(["session".to_string()]),
            files: BTreeSet::from([PathBuf::from("/p/-x/abc-123.jsonl")]),
            snippets: vec![
                Snippet {
                    tag: "prompt".to_string(),
                    text: "fix the auth ⋯ timeout".to_string(),
                    time_ms: 1_700_000_000_000,
                },
                Snippet {
                    tag: "agent".to_string(),
                    text: "subagent found it".to_string(),
                    time_ms: 1_700_000_000_000,
                },
            ],
            last_msg_ms: 1_700_000_000_000,
            display_stats: None,
            title: Some("my-session".to_string()),
            archived: true,
            score: 1.25,
        }
    }

    #[test]
    fn recency_damps_but_does_not_decide() {
        let now = 1_700_000_000_000;
        let day = 86_400_000;
        assert_eq!(recency_factor(now, now), 1.0);
        // A month barely costs anything: relevance still decides.
        let month = recency_factor(now - 30 * day, now);
        assert!(month > 0.85, "30d factor was {month}");
        // Half a year is worth half; a year, a third — a strong old match can
        // still outrank a weak fresh one.
        let half_year = recency_factor(now - 180 * day, now);
        assert!((half_year - 0.5).abs() < 0.01, "180d factor was {half_year}");
        let year = recency_factor(now - 365 * day, now);
        assert!(year > 0.3 && year < 0.35, "365d factor was {year}");
        // No timestamp (nothing on disk to date the session) is not punished.
        assert_eq!(recency_factor(0, now), 1.0);
    }

    #[test]
    fn file_relevance_counts_matches_and_discounts_noisy_sources() {
        let hit = |kind| DocHit {
            kind,
            score: 4.0,
            path: PathBuf::from("/p/-x/s.jsonl"),
            file_id: 0,
            chunk_id: 0,
            snippet_range: Some((0, 10)),
        };
        let word = [hit(IndexKind::Word)];

        // Twenty matching lines outweigh one — the count is the signal BM25
        // over 512 KB chunks cannot give us.
        assert!(
            file_relevance(&word, "session", 20) > file_relevance(&word, "session", 1) * 10.0
        );
        // The same twenty matches in tool output are worth half as much.
        assert_eq!(
            file_relevance(&word, "tool-result", 20),
            file_relevance(&word, "session", 20) * 0.5
        );
        // Substring evidence is discounted against a word match.
        assert!(
            file_relevance(&[hit(IndexKind::Trigram)], "session", 5)
                < file_relevance(&word, "session", 5)
        );
        // A path-only hit matches no lines but still counts for something.
        let path_hit = DocHit {
            snippet_range: None,
            ..hit(IndexKind::Path)
        };
        assert!(file_relevance(&[path_hit], "session", 0) > 0.0);
    }

    #[test]
    fn session_json_is_flat_and_ansi_free() {
        let v = session_json(&sample_session(), 12, 34, 5678, false);
        assert_eq!(v["session_id"], "abc-123");
        assert_eq!(v["title"], "my-session");
        assert_eq!(v["archived"], true);
        assert_eq!(v["prompts"], 12);
        assert_eq!(v["tools"], 34);
        assert_eq!(v["bytes"], 5678);
        assert_eq!(v["snippets"][0]["tag"], "prompt");
        assert_eq!(v["snippets"][0]["text"], "fix the auth ⋯ timeout");
        assert!(v["last_activity"].as_str().unwrap().starts_with("2023-11-14"));
        let line = v.to_string();
        assert!(!line.contains('\u{1b}'), "no ANSI escapes in JSON output");
        assert!(!line.contains('\n'), "one line per session");
    }

    #[test]
    fn session_json_respects_verbose_tiers() {
        // Without -v: prompt tier only, no files — same economy as the
        // human renderer.
        let v = session_json(&sample_session(), 1, 2, 3, false);
        assert_eq!(v["snippets"].as_array().unwrap().len(), 1);
        assert!(v.get("files").is_none());

        // With -v: agent/hook snippets and file paths appear.
        let v = session_json(&sample_session(), 1, 2, 3, true);
        let tags: Vec<&str> = v["snippets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["tag"].as_str().unwrap())
            .collect();
        assert_eq!(tags, ["prompt", "agent"]);
        assert_eq!(v["files"][0], "/p/-x/abc-123.jsonl");
    }
}
