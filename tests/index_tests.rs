//! Integration tests for the Tantivy multi-index backend.
//!
//! Each test builds a self-contained `projects/` tree in a tempdir, runs the
//! relevant index pipeline against a fresh `index_dir`, then asserts what is
//! visible to `run_search`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use cct::fs_scan::{collect_archived_layout, collect_cct_layout, collect_one_project};
use cct::index::build::{run_full_reindex, subindex_segment_count};
use cct::index::catalog::{read_catalog, read_delta_catalog};
use cct::index::compact::{
    optimize_delta, optimize_main, tidy_index, DELTA_OPTIMIZE_MIN_SEGMENTS, MAIN_KEEP_LARGEST,
    MAIN_OPTIMIZE_MIN_SEGMENTS,
};
use cct::index::schema::{delta_dir, subindex_dir};
use cct::index::search::{run_search, SearchMode, SearchOptions, SessionResult, SortBy};
use cct::index::update::{build_plan, build_plan_scoped, flush_delta, needs_update, run_update};
use cct::index::writer::WriterConfig;
use cct::index::IndexKind;

struct Fixture {
    _tmp: tempfile::TempDir,
    source: PathBuf,
    index_dir: PathBuf,
    project_dir: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let tmp = tempfile::tempdir()?;
        let source = tmp.path().join("projects");
        let project_dir = source.join("-tmp-cct-test-project");
        fs::create_dir_all(&project_dir)?;
        let index_dir = tmp.path().join("index");
        Ok(Self {
            _tmp: tmp,
            source,
            index_dir,
            project_dir,
        })
    }

    fn write_session(&self, name: &str, contents: &str) -> PathBuf {
        let path = self.project_dir.join(format!("{name}.jsonl"));
        fs::write(&path, contents).expect("write session");
        path
    }

    fn write_tool_result(&self, session: &str, name: &str, contents: &str) -> PathBuf {
        let dir = self.project_dir.join(session).join("tool-results");
        fs::create_dir_all(&dir).expect("mkdir tool-results");
        let path = dir.join(name);
        fs::write(&path, contents).expect("write tool-result");
        path
    }

    fn full_reindex(&self) -> Result<()> {
        let files = collect_cct_layout(&self.source)?;
        let enabled = IndexKind::all();
        let config = WriterConfig::for_full_reindex();
        run_full_reindex(
            &self.source,
            &self.index_dir,
            &enabled,
            files,
            config,
            false,
        )
    }

    fn full_reindex_only(&self, kinds: BTreeSet<IndexKind>) -> Result<()> {
        let files = collect_cct_layout(&self.source)?;
        let config = WriterConfig::for_full_reindex();
        run_full_reindex(&self.source, &self.index_dir, &kinds, files, config, false)
    }

    fn update(&self) -> Result<()> {
        let files = collect_cct_layout(&self.source)?;
        let plan = build_plan(&self.index_dir, Some(self.source.clone()), files)?;
        let _stats = run_update(
            &self.index_dir,
            plan,
            WriterConfig::for_incremental_update(),
            false,
        )?;
        Ok(())
    }

    fn update_dry(&self) -> Result<bool> {
        let files = collect_cct_layout(&self.source)?;
        let plan = build_plan(&self.index_dir, Some(self.source.clone()), files)?;
        Ok(needs_update(&plan))
    }

    fn search(&self, query: &str, mode: SearchMode) -> Vec<SessionResult> {
        let opts = SearchOptions {
            queries: vec![query.to_string()],
            mode,
            limit: 20,
            context_chars: 80,
            project_prefix: None,
            fuzzy: false,
            sort: SortBy::default(),
        };
        run_search(&self.index_dir, &opts).unwrap_or_default()
    }

    fn search_sorted(&self, query: &str, sort: SortBy) -> Vec<SessionResult> {
        let opts = SearchOptions {
            queries: vec![query.to_string()],
            mode: SearchMode::Auto,
            limit: 20,
            context_chars: 80,
            project_prefix: None,
            fuzzy: false,
            sort,
        };
        run_search(&self.index_dir, &opts).unwrap_or_default()
    }

    fn search_multi(&self, queries: &[&str]) -> Vec<SessionResult> {
        let opts = SearchOptions {
            queries: queries.iter().map(|q| q.to_string()).collect(),
            mode: SearchMode::Auto,
            limit: 20,
            context_chars: 80,
            project_prefix: None,
            fuzzy: false,
            sort: SortBy::default(),
        };
        run_search(&self.index_dir, &opts).unwrap_or_default()
    }

    fn search_fuzzy(&self, query: &str) -> Vec<SessionResult> {
        let opts = SearchOptions {
            queries: vec![query.to_string()],
            mode: SearchMode::Auto,
            limit: 20,
            context_chars: 80,
            project_prefix: None,
            fuzzy: true,
            sort: SortBy::default(),
        };
        run_search(&self.index_dir, &opts).unwrap_or_default()
    }
}

fn jsonl_user(text: &str) -> String {
    format!(
        r#"{{"type":"user","timestamp":"2026-01-15T12:00:00Z","message":{{"content":"{}"}}}}"#,
        text.replace('"', "\\\"")
    )
}

fn jsonl_assistant(text: &str) -> String {
    format!(
        r#"{{"type":"assistant","timestamp":"2026-01-15T12:00:01Z","message":{{"content":[{{"type":"text","text":"{}"}}]}}}}"#,
        text.replace('"', "\\\"")
    )
}

fn jsonl_custom_title(title: &str) -> String {
    format!(
        r#"{{"type":"custom-title","customTitle":"{}","sessionId":"x"}}"#,
        title.replace('"', "\\\"")
    )
}

fn assert_session(results: &[SessionResult], session_id: &str) {
    assert!(
        results.iter().any(|r| r.session_id == session_id),
        "session {session_id} not in results: {:?}",
        results
            .iter()
            .map(|r| r.session_id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn reindex_finds_normal_word() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-words",
        &format!(
            "{}\n{}\n",
            jsonl_user("hello world"),
            jsonl_assistant("a quick brown fox jumps")
        ),
    );
    fx.full_reindex()?;
    let results = fx.search("brown", SearchMode::Auto);
    assert_session(&results, "session-words");
    let entry = read_catalog(&fx.index_dir)?
        .into_iter()
        .find(|entry| entry.path == session_path)
        .expect("session must be in catalog");
    assert_eq!(entry.prompts, Some(1));
    assert_eq!(entry.tools, Some(0));
    assert_eq!(
        entry.display_bytes,
        Some(fs::metadata(&session_path)?.len())
    );

    let session = results
        .iter()
        .find(|result| result.session_id == "session-words")
        .expect("session result");
    let display = session.display_stats.expect("cached display stats");
    assert_eq!(display.prompts, 1);
    assert_eq!(display.tools, 0);
    assert_eq!(display.bytes, fs::metadata(&session_path)?.len());
    assert_eq!(session.title, None, "unnamed session has no title");
    Ok(())
}

#[test]
fn session_title_is_cached_and_last_rename_wins() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-named",
        &format!(
            "{}\n{}\n{}\n",
            jsonl_user("titleterm here"),
            jsonl_custom_title("first-name"),
            jsonl_custom_title("final-name"),
        ),
    );
    fx.full_reindex()?;

    let entry = read_catalog(&fx.index_dir)?
        .into_iter()
        .find(|entry| entry.path == session_path)
        .expect("session in catalog");
    assert_eq!(entry.title.as_deref(), Some("final-name"));

    let results = fx.search("titleterm", SearchMode::Auto);
    let session = results
        .iter()
        .find(|r| r.session_id == "session-named")
        .expect("session result");
    assert_eq!(session.title.as_deref(), Some("final-name"));

    // A later rename in an appended tail supersedes the cached title.
    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_custom_title("renamed-again"));
    content.push('\n');
    fs::write(&session_path, content)?;
    fx.update()?;

    let entry = read_catalog(&fx.index_dir)?
        .into_iter()
        .find(|entry| entry.path == session_path)
        .expect("session still in catalog");
    assert_eq!(entry.title.as_deref(), Some("renamed-again"));
    Ok(())
}

#[test]
fn trigram_finds_apierror_substring() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-tech",
        &format!(
            "{}\n",
            jsonl_assistant("got APIError: timeout while contacting backend")
        ),
    );
    fx.full_reindex()?;
    let results = fx.search("APIError", SearchMode::Auto);
    assert_session(&results, "session-tech");
    Ok(())
}

#[test]
fn russian_short_token_via_word_index() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-ru",
        &format!("{}\n", jsonl_user("давление 120 80 АД нормальное")),
    );
    fx.full_reindex()?;
    let results = fx.search("АД", SearchMode::Auto);
    assert_session(&results, "session-ru");
    Ok(())
}

#[test]
fn append_update_indexes_only_complete_lines() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-append",
        &format!("{}\n", jsonl_user("first line about astronomers")),
    );
    fx.full_reindex()?;
    let results = fx.search("astronomers", SearchMode::Auto);
    assert_session(&results, "session-append");

    // Append a complete line + a trailing partial line; only the complete one should be searchable.
    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_user("second line about telescopes"));
    content.push('\n');
    content.push_str(r#"{"type":"user","timestamp":"2026-01-15T12:01:00Z","message":{"content":"third line about MISSING"#); // no closing brace, no newline
    fs::write(&session_path, content)?;

    fx.update()?;
    let entry = read_catalog(&fx.index_dir)?
        .into_iter()
        .find(|entry| entry.path == session_path)
        .expect("session must be in catalog");
    assert_eq!(entry.prompts, Some(2));
    assert_eq!(entry.tools, Some(0));
    assert_eq!(
        entry.display_bytes,
        Some(fs::metadata(&session_path)?.len())
    );

    let results = fx.search("telescopes", SearchMode::Auto);
    assert_session(&results, "session-append");

    let results = fx.search("MISSING", SearchMode::Auto);
    assert!(
        results.is_empty(),
        "partial trailing line should not be searchable yet"
    );
    Ok(())
}

#[test]
fn append_update_writes_delta_not_main_segments() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-delta",
        &format!("{}\n", jsonl_user("first line before delta")),
    );
    fx.full_reindex()?;
    let main_word_segments = subindex_segment_count(&fx.index_dir, IndexKind::Word)?;

    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_user("second line DELTATERM"));
    content.push('\n');
    fs::write(&session_path, content)?;
    fx.update()?;

    assert_session(&fx.search("DELTATERM", SearchMode::Auto), "session-delta");
    assert_eq!(
        subindex_segment_count(&fx.index_dir, IndexKind::Word)?,
        main_word_segments,
        "append should not add a main word segment"
    );
    let delta = delta_dir(&fx.index_dir);
    assert!(delta.join("word").is_dir(), "delta word index must exist");
    assert!(
        delta.join("trigram").is_dir(),
        "delta trigram index must exist"
    );
    assert!(
        !delta.join("path").exists(),
        "append should not touch delta path"
    );
    assert_eq!(read_delta_catalog(&fx.index_dir)?.len(), 1);
    Ok(())
}

#[test]
fn rewrite_clears_stale_delta_hits() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-delta-rewrite",
        &format!("{}\n", jsonl_user("first line")),
    );
    fx.full_reindex()?;

    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_user("temporary STALEDELTA"));
    content.push('\n');
    fs::write(&session_path, content)?;
    fx.update()?;
    assert_session(
        &fx.search("STALEDELTA", SearchMode::Auto),
        "session-delta-rewrite",
    );

    fs::write(
        &session_path,
        format!("{}\n", jsonl_user("fresh REWRITTENDELTA")),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(20));
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(&session_path, filetime::FileTime::from_system_time(now)).ok();
    fx.update()?;

    assert!(fx.search("STALEDELTA", SearchMode::Auto).is_empty());
    assert_session(
        &fx.search("REWRITTENDELTA", SearchMode::Auto),
        "session-delta-rewrite",
    );
    assert!(read_delta_catalog(&fx.index_dir)?.is_empty());
    Ok(())
}

#[test]
fn flush_delta_moves_append_ranges_to_main_and_clears_delta() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-delta-flush",
        &format!("{}\n", jsonl_user("first line")),
    );
    fx.full_reindex()?;

    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_user("background FLUSHDELTA"));
    content.push('\n');
    fs::write(&session_path, content)?;
    fx.update()?;
    assert_eq!(read_delta_catalog(&fx.index_dir)?.len(), 1);

    flush_delta(&fx.index_dir, WriterConfig::for_incremental_update(), false)?;
    assert_session(
        &fx.search("FLUSHDELTA", SearchMode::Auto),
        "session-delta-flush",
    );
    assert!(!delta_dir(&fx.index_dir).exists());
    assert!(read_delta_catalog(&fx.index_dir)?.is_empty());
    Ok(())
}

#[test]
fn optimize_delta_waits_for_segment_threshold() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-delta-optimize",
        &format!("{}\n", jsonl_user("first line")),
    );
    fx.full_reindex()?;

    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_user("first OPTDELTA"));
    content.push('\n');
    fs::write(&session_path, &content)?;
    fx.update()?;

    content.push_str(&jsonl_user("second OPTDELTA"));
    content.push('\n');
    fs::write(&session_path, content)?;
    fx.update()?;

    let delta = delta_dir(&fx.index_dir);
    assert!(
        subindex_segment_count(&delta, IndexKind::Word)? > 1,
        "two append commits should create multiple delta word segments"
    );
    assert!(optimize_delta(&fx.index_dir, WriterConfig::for_delta_update())?.is_none());

    for i in 3..=DELTA_OPTIMIZE_MIN_SEGMENTS {
        let mut content = fs::read_to_string(&session_path)?;
        content.push_str(&jsonl_user(&format!("extra OPTDELTA {i}")));
        content.push('\n');
        fs::write(&session_path, content)?;
        fx.update()?;
    }

    assert!(
        subindex_segment_count(&delta, IndexKind::Word)? >= DELTA_OPTIMIZE_MIN_SEGMENTS,
        "delta should reach the auto optimize threshold"
    );
    assert!(optimize_delta(&fx.index_dir, WriterConfig::for_delta_update())?.is_some());
    assert_eq!(subindex_segment_count(&delta, IndexKind::Word)?, 1);
    assert!(optimize_delta(&fx.index_dir, WriterConfig::for_delta_update())?.is_none());
    assert_session(
        &fx.search("OPTDELTA", SearchMode::Auto),
        "session-delta-optimize",
    );
    Ok(())
}

#[test]
fn partial_line_becomes_searchable_after_newline() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session("session-late", &jsonl_user("first uniqueterm\n"));
    fx.full_reindex()?;
    // Append partial without newline.
    let mut data = fs::read_to_string(&session_path)?;
    data.push_str(&jsonl_user("second LATEAPPEND")); // no trailing newline
    fs::write(&session_path, data)?;
    fx.update()?;
    let results = fx.search("LATEAPPEND", SearchMode::Auto);
    assert!(results.is_empty(), "partial line should not be searchable");

    // Now finish the line.
    let mut data = fs::read_to_string(&session_path)?;
    data.push('\n');
    fs::write(&session_path, data)?;
    fx.update()?;
    let results = fx.search("LATEAPPEND", SearchMode::Auto);
    assert_session(&results, "session-late");
    Ok(())
}

#[test]
fn unchanged_update_is_a_noop() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-stable",
        &format!("{}\n", jsonl_user("anchor word")),
    );
    fx.full_reindex()?;
    let needs = fx.update_dry()?;
    assert!(!needs, "no changes should mean no work");
    Ok(())
}

#[test]
fn reindex_catalog_uses_fresh_identity_after_stale_scan() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-stale-scan",
        &format!("{}\n", jsonl_user("initial snapshot term")),
    );
    let files = collect_cct_layout(&fx.source)?;

    let mut content = fs::read_to_string(&session_path)?;
    content.push_str(&jsonl_user("late line after scan"));
    content.push('\n');
    fs::write(&session_path, content)?;

    let enabled = IndexKind::all();
    run_full_reindex(
        &fx.source,
        &fx.index_dir,
        &enabled,
        files,
        WriterConfig::for_full_reindex(),
        false,
    )?;

    assert_session(&fx.search("late", SearchMode::Auto), "session-stale-scan");
    assert!(
        !fx.update_dry()?,
        "freshly reindexed file should not require update when it only changed before read"
    );
    Ok(())
}

#[test]
fn deleted_file_removes_hits() -> Result<()> {
    let fx = Fixture::new()?;
    let path = fx.write_session(
        "session-delete",
        &format!("{}\n", jsonl_user("doomed lucida")),
    );
    fx.full_reindex()?;
    let results = fx.search("lucida", SearchMode::Auto);
    assert_session(&results, "session-delete");

    fs::remove_file(&path)?;
    fx.update()?;
    let results = fx.search("lucida", SearchMode::Auto);
    assert!(
        results.is_empty(),
        "deleted file's hits should be removed: {:?}",
        results
    );
    Ok(())
}

#[test]
fn shrink_or_rewrite_reindexes() -> Result<()> {
    let fx = Fixture::new()?;
    let path = fx.write_session(
        "session-rewrite",
        &format!("{}\n{}\n", jsonl_user("ALPHATERM"), jsonl_user("BETATERM")),
    );
    fx.full_reindex()?;
    assert_session(&fx.search("ALPHATERM", SearchMode::Auto), "session-rewrite");

    // Rewrite the file shorter — drop the alpha line, change beta to gamma.
    fs::write(&path, format!("{}\n", jsonl_user("GAMMATERM")))?;
    // Make sure mtime moves forward.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(now)).ok();

    fx.update()?;
    assert!(fx.search("ALPHATERM", SearchMode::Auto).is_empty());
    assert!(fx.search("BETATERM", SearchMode::Auto).is_empty());
    assert_session(&fx.search("GAMMATERM", SearchMode::Auto), "session-rewrite");
    Ok(())
}

#[test]
fn grown_rewrite_with_changed_prefix_reindexes() -> Result<()> {
    let fx = Fixture::new()?;
    let path = fx.write_session(
        "session-grown-rewrite",
        &format!("{}\n", jsonl_user("DELTAONE original prefix")),
    );
    fx.full_reindex()?;
    assert_session(
        &fx.search("DELTAONE", SearchMode::Auto),
        "session-grown-rewrite",
    );

    // Rewrite in place so the file GROWS but the old prefix is gone — the
    // size/mtime heuristic alone would treat this as a pure append and keep
    // stale content indexed forever.
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            jsonl_user("EPSILONTWO replaced prefix"),
            jsonl_user("ZETATHREE appended line")
        ),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(20));
    let now = std::time::SystemTime::now();
    filetime::set_file_mtime(&path, filetime::FileTime::from_system_time(now)).ok();

    fx.update()?;
    assert!(
        fx.search("DELTAONE", SearchMode::Auto).is_empty(),
        "stale prefix content must be gone after in-place rewrite"
    );
    assert_session(
        &fx.search("EPSILONTWO", SearchMode::Auto),
        "session-grown-rewrite",
    );
    assert_session(
        &fx.search("ZETATHREE", SearchMode::Auto),
        "session-grown-rewrite",
    );
    Ok(())
}

#[test]
fn path_search_finds_session_by_path() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-with-uniqueid",
        &format!("{}\n", jsonl_user("ordinary content")),
    );
    fx.full_reindex()?;
    let results = fx.search("session-with-uniqueid", SearchMode::Only(IndexKind::Path));
    assert_session(&results, "session-with-uniqueid");
    Ok(())
}

#[test]
fn path_only_hits_have_visible_snippets() -> Result<()> {
    use cct::index::search::select_visible_snippets;
    let fx = Fixture::new()?;
    fx.write_session(
        "session-with-uniqueid",
        &format!("{}\n", jsonl_user("ordinary content")),
    );
    fx.full_reindex()?;
    let results = fx.search("session-with-uniqueid", SearchMode::Only(IndexKind::Path));
    assert_session(&results, "session-with-uniqueid");
    let session = results
        .iter()
        .find(|r| r.session_id == "session-with-uniqueid")
        .unwrap();
    assert!(
        !select_visible_snippets(&session.snippets, false).is_empty(),
        "path-only hit must produce a visible snippet, got {:?}",
        session.snippets
    );
    Ok(())
}

#[test]
fn short_pathish_query_still_searches_word_index() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-short",
        &format!("{}\n", jsonl_user("see c docs for details")),
    );
    fx.full_reindex()?;
    // "c/" enables the Path subindex in auto mode but is too short for the
    // ngram tokenizer; the word index must still be searched.
    let opts = SearchOptions {
        queries: vec!["c/".to_string()],
        mode: SearchMode::Auto,
        limit: 20,
        context_chars: 80,
        project_prefix: None,
        fuzzy: false,
        sort: SortBy::default(),
    };
    let results = run_search(&fx.index_dir, &opts)?;
    assert_session(&results, "session-short");
    Ok(())
}

#[test]
fn phrase_with_quotes_matches_escaped_jsonl() -> Result<()> {
    use cct::index::search::select_visible_snippets;
    let fx = Fixture::new()?;
    fx.write_session(
        "session-quotes",
        &format!("{}\n", jsonl_user(r#"please say "hello" twice"#)),
    );
    fx.full_reindex()?;
    let results = fx.search(r#"say "hello" twice"#, SearchMode::Auto);
    assert_session(&results, "session-quotes");
    let session = results
        .iter()
        .find(|r| r.session_id == "session-quotes")
        .unwrap();
    assert!(
        !select_visible_snippets(&session.snippets, false).is_empty(),
        "quoted phrase must produce a snippet from the escaped raw line"
    );
    Ok(())
}

#[test]
fn snippets_come_from_chunk_offsets() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-snippets",
        &format!(
            "{}\n{}\n",
            jsonl_user("how do I tweak the planetary alignment threshold?"),
            jsonl_assistant("you can edit the config to change planetary alignment values"),
        ),
    );
    fx.full_reindex()?;
    let results = fx.search("planetary", SearchMode::Auto);
    assert_session(&results, "session-snippets");
    let session = &results[0];
    assert!(
        !session.snippets.is_empty(),
        "expected at least one snippet"
    );
    let combined = session
        .snippets
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        combined.to_lowercase().contains("planetary"),
        "snippet should contain query: {combined}"
    );
    Ok(())
}

#[test]
fn slash_command_invocation_is_searchable_by_command_name() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-slash-command",
        &format!(
            "{}\n",
            jsonl_user(
                "<command-message>/research anomaly budgets</command-message>\
                 <command-args>anomaly budgets</command-args>\
                 <command-name>/research</command-name>"
            ),
        ),
    );
    fx.full_reindex()?;

    let results = fx.search("/research", SearchMode::Auto);
    assert_session(&results, "session-slash-command");
    let combined = results[0]
        .snippets
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        combined.contains("/research anomaly budgets"),
        "expected rewritten slash command snippet: {combined}"
    );
    assert!(
        !combined.contains("<command-name>"),
        "snippet should not show raw command tags: {combined}"
    );
    Ok(())
}

#[test]
fn unicode_lowercase_expansion_does_not_panic_snippets() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-unicode",
        &format!("{}\n", jsonl_user("İstanbul pressure note")),
    );
    fx.full_reindex()?;
    let results = fx.search("İstanbul", SearchMode::Auto);
    assert_session(&results, "session-unicode");
    let combined = results[0]
        .snippets
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(combined.contains("İstanbul"));
    Ok(())
}

#[test]
fn tool_result_text_is_indexed() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-tools",
        &format!("{}\n", jsonl_user("checking logs")),
    );
    fx.write_tool_result(
        "session-tools",
        "trace.log",
        "stack trace: traceback for ALERTKEYWORD inside the daemon\n",
    );
    fx.full_reindex()?;
    let results = fx.search("ALERTKEYWORD", SearchMode::Auto);
    assert_session(&results, "session-tools");
    Ok(())
}

#[test]
fn binary_pdf_files_are_skipped() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-bins",
        &format!("{}\n", jsonl_user("attached pdf below")),
    );
    let pdf_dir = fx.project_dir.join("session-bins").join("tool-results");
    fs::create_dir_all(&pdf_dir)?;
    fs::write(
        pdf_dir.join("doc.pdf"),
        b"%PDF-1.4 binary stuff PDFSECRETKEY",
    )?;
    fx.full_reindex()?;
    let results = fx.search("PDFSECRETKEY", SearchMode::Auto);
    assert!(
        results.is_empty(),
        "pdf must not be indexed: {:?}",
        results
            .iter()
            .map(|r| r.session_id.clone())
            .collect::<Vec<_>>()
    );
    Ok(())
}

#[test]
fn only_word_subset_skips_trigram_subindex() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-only-word",
        &format!("{}\n", jsonl_user("only word index please")),
    );
    let mut kinds = BTreeSet::new();
    kinds.insert(IndexKind::Word);
    fx.full_reindex_only(kinds)?;
    let trigram_dir = fx.index_dir.join("trigram");
    assert!(!trigram_dir.exists(), "trigram dir must not be created");
    let path_dir = fx.index_dir.join("path");
    assert!(!path_dir.exists(), "path dir must not be created");
    Ok(())
}

#[test]
fn auto_search_index_selection_for_acronyms_and_non_ascii() {
    use cct::index::search::select_indexes;
    assert_eq!(
        select_indexes(SearchMode::Auto, "API"),
        vec![IndexKind::Word, IndexKind::Trigram]
    );
    assert_eq!(
        select_indexes(SearchMode::Auto, "давление"),
        vec![IndexKind::Word, IndexKind::Trigram]
    );
    // 2-char Russian token uses word only (probe parity).
    assert_eq!(
        select_indexes(SearchMode::Auto, "АД"),
        vec![IndexKind::Word]
    );
    assert_eq!(
        select_indexes(SearchMode::Auto, "/etc/passwd"),
        vec![IndexKind::Word, IndexKind::Path]
    );
}

#[test]
fn project_scope_uses_path_boundaries_not_string_prefix() -> Result<()> {
    let fx = Fixture::new()?;
    let other_project = fx.source.join("-tmp-cct-test-project-extra");
    fs::create_dir_all(&other_project)?;

    fx.write_session(
        "session-scoped",
        &format!("{}\n", jsonl_user("boundaryterm from scoped project")),
    );
    fs::write(
        other_project.join("session-extra.jsonl"),
        format!("{}\n", jsonl_user("boundaryterm from extra project")),
    )?;

    fx.full_reindex()?;
    let word_index = tantivy::Index::open_in_dir(fx.index_dir.join("word"))?;
    assert!(word_index.schema().get_field("project_id").is_ok());

    let opts = SearchOptions {
        queries: vec!["boundaryterm".to_string()],
        mode: SearchMode::Auto,
        limit: 20,
        context_chars: 80,
        project_prefix: Some(fx.project_dir.clone()),
        fuzzy: false,
        sort: SortBy::default(),
    };
    let results = run_search(&fx.index_dir, &opts)?;
    assert_session(&results, "session-scoped");
    assert!(
        !results.iter().any(|r| r.session_id == "session-extra"),
        "project path prefix must not match sibling project names"
    );
    Ok(())
}

#[test]
fn search_reports_incompatible_catalog() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-catalog-schema",
        &format!("{}\n", jsonl_user("catalogschema term")),
    );
    fx.full_reindex()?;
    fs::write(
        fx.index_dir.join("catalog.tsv"),
        "file_id\tkind\tsize\tmtime_ms\tindexed_offset\tnext_chunk_id\tpath\n",
    )?;

    let opts = SearchOptions {
        queries: vec!["catalogschema".to_string()],
        mode: SearchMode::Auto,
        limit: 20,
        context_chars: 80,
        project_prefix: None,
        fuzzy: false,
        sort: SortBy::default(),
    };
    let err = run_search(&fx.index_dir, &opts).expect_err("catalog schema must be strict");
    let message = format!("{err:#}");
    assert!(message.contains("catalog schema is old or incompatible"));
    assert!(message.contains("cct reindex"));
    Ok(())
}

#[test]
fn fuzzy_finds_session_when_phrase_is_split_across_lines() -> Result<()> {
    let fx = Fixture::new()?;
    fx.write_session(
        "session-fuzzy",
        &format!(
            "{}\n{}\n",
            jsonl_user("Марат сегодня заглянул"),
            jsonl_assistant("позже пришли последние сообщения с упрёком, всё хорошо"),
        ),
    );
    fx.full_reindex()?;

    // Strict search: phrase is not on one line, so no snippets are produced.
    let strict = fx.search("Марат, последние сообщения упрёком", SearchMode::Auto);
    let strict_session = strict.iter().find(|r| r.session_id == "session-fuzzy");
    if let Some(r) = strict_session {
        assert!(
            r.snippets.is_empty(),
            "strict mode must not produce a snippet when the phrase isn't one line"
        );
    }

    // Fuzzy: per-token snippets pick up "Марат" and "упрёком" separately.
    let fuzzy = fx.search_fuzzy("Марат, последние сообщения упрёком");
    let r = fuzzy
        .iter()
        .find(|r| r.session_id == "session-fuzzy")
        .expect("fuzzy must surface the session");
    let combined = r
        .snippets
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        combined.contains("Марат"),
        "expected Марат in fuzzy snippets"
    );
    assert!(
        combined.contains("упрёком"),
        "expected упрёком in fuzzy snippets"
    );
    Ok(())
}

#[test]
fn scoped_update_leaves_other_projects_intact() -> Result<()> {
    let fx = Fixture::new()?;
    let other_project = fx.source.join("-tmp-cct-test-other");
    fs::create_dir_all(&other_project)?;

    let scoped_session = fx.write_session(
        "session-scoped-upd",
        &format!("{}\n", jsonl_user("alpha SCOPEDOLD")),
    );
    fs::write(
        other_project.join("session-other.jsonl"),
        format!("{}\n", jsonl_user("bravo OTHERTERM keeps existing")),
    )?;
    fx.full_reindex()?;

    // Sanity: both terms searchable globally before the scoped update.
    assert_session(
        &fx.search("SCOPEDOLD", SearchMode::Auto),
        "session-scoped-upd",
    );
    assert_session(&fx.search("OTHERTERM", SearchMode::Auto), "session-other");

    // Mutate the scoped project: drop the old session, add a new one.
    fs::remove_file(&scoped_session)?;
    fs::write(
        fx.project_dir.join("session-scoped-new.jsonl"),
        format!("{}\n", jsonl_user("alpha SCOPEDNEW")),
    )?;

    let scope = fx.project_dir.clone();
    let files = collect_one_project(&scope)?;
    let plan = build_plan_scoped(&fx.index_dir, Some(fx.source.clone()), files, Some(&scope))?;
    let _ = run_update(
        &fx.index_dir,
        plan,
        WriterConfig::for_incremental_update(),
        false,
    )?;

    // Scoped changes took effect.
    assert!(
        fx.search("SCOPEDOLD", SearchMode::Auto).is_empty(),
        "deleted scoped session must be gone"
    );
    assert_session(
        &fx.search("SCOPEDNEW", SearchMode::Auto),
        "session-scoped-new",
    );
    // The other project is untouched: same term still resolves to its session.
    assert_session(&fx.search("OTHERTERM", SearchMode::Auto), "session-other");
    Ok(())
}

#[test]
fn full_reindex_includes_archived_sessions() -> Result<()> {
    let fx = Fixture::new()?;
    // Old timestamp so the session is "cold" regardless of the wall clock.
    let session = fx.write_session(
        "zebra-session",
        &format!(
            "{}\n{}\n",
            jsonl_custom_title("zebra-archived-title"),
            r#"{"type":"user","timestamp":"2020-01-01T00:00:00Z","message":{"content":"distinctivezebraword appears here"}}"#,
        ),
    );

    // Archive it into a sibling archive dir; the live jsonl is removed.
    let archive = fx.source.parent().unwrap().join("archive");
    cct::archive::run_archive_in(
        cct::archive::ArchiveOptions {
            older_than_days: 0,
            project: None,
            dry_run: false,
            skip_confirm: true,
            quiet: true,
        },
        &fx.source,
        &archive,
    )?;
    assert!(!session.exists(), "live session should be gone after archive");

    // A live-only reindex drops the archived session — the bug being fixed.
    fx.full_reindex()?;
    assert!(
        fx.search("distinctivezebraword", SearchMode::Auto).is_empty(),
        "live-only reindex must not surface the archived session"
    );

    // A reindex that also feeds the archive serves it again.
    let mut files = collect_cct_layout(&fx.source)?;
    files.extend(collect_archived_layout(&archive, &fx.source)?);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    run_full_reindex(
        &fx.source,
        &fx.index_dir,
        &IndexKind::all(),
        files,
        WriterConfig::for_full_reindex(),
        false,
    )?;
    assert_session(
        &fx.search("distinctivezebraword", SearchMode::Auto),
        "zebra-session",
    );
    Ok(())
}

#[test]
fn multi_string_search_ands_at_session_level() -> Result<()> {
    let fx = Fixture::new()?;
    // Both needles live in one session but on far-apart lines (separate
    // prompts), so the AND must hold at session level, not line level.
    fx.write_session(
        "session-both",
        &format!(
            "{}\n{}\n{}\n",
            jsonl_user("we discussed alphaneedle here"),
            jsonl_assistant("unrelated middle content"),
            jsonl_user("and betaneedle much later"),
        ),
    );
    fx.write_session(
        "session-alpha-only",
        &format!("{}\n", jsonl_user("only alphaneedle in this one")),
    );
    fx.write_session(
        "session-beta-only",
        &format!("{}\n", jsonl_user("only betaneedle in this one")),
    );
    fx.full_reindex()?;

    // Single strings still find their sessions.
    assert_session(&fx.search("alphaneedle", SearchMode::Auto), "session-both");
    assert_session(
        &fx.search("alphaneedle", SearchMode::Auto),
        "session-alpha-only",
    );

    // AND keeps only the session containing every string.
    let results = fx.search_multi(&["alphaneedle", "betaneedle"]);
    assert_eq!(
        results.len(),
        1,
        "only session-both matches all strings: {:?}",
        results.iter().map(|r| &r.session_id).collect::<Vec<_>>()
    );
    assert_eq!(results[0].session_id, "session-both");
    // Snippets represent both strings.
    assert!(
        results[0]
            .snippets
            .iter()
            .any(|s| s.text.contains("alphaneedle")),
        "snippets must cover the first string: {:?}",
        results[0].snippets
    );
    assert!(
        results[0]
            .snippets
            .iter()
            .any(|s| s.text.contains("betaneedle")),
        "snippets must cover the second string: {:?}",
        results[0].snippets
    );

    // No session has all three strings.
    assert!(fx
        .search_multi(&["alphaneedle", "betaneedle", "gammaneedle"])
        .is_empty());
    Ok(())
}

#[test]
fn multi_string_search_matches_across_session_files() -> Result<()> {
    let fx = Fixture::new()?;
    // One string in the main jsonl, the other only in a tool-result sidecar:
    // the session-level AND must still see them as one session.
    fx.write_session(
        "session-spread",
        &format!("{}\n", jsonl_user("main log mentions firstprobe")),
    );
    fx.write_tool_result(
        "session-spread",
        "grep-output.txt",
        "tool output line with secondprobe inside\n",
    );
    fx.write_session(
        "session-decoy",
        &format!("{}\n", jsonl_user("decoy also mentions firstprobe")),
    );
    fx.full_reindex()?;

    let results = fx.search_multi(&["firstprobe", "secondprobe"]);
    assert_eq!(
        results.len(),
        1,
        "decoy lacks secondprobe: {:?}",
        results.iter().map(|r| &r.session_id).collect::<Vec<_>>()
    );
    assert_eq!(results[0].session_id, "session-spread");
    Ok(())
}

#[test]
fn oversized_tokens_are_indexed_by_prefix_and_verified_in_full() -> Result<()> {
    let fx = Fixture::new()?;
    // 44 chars: what an API key body or a base64 chunk looks like in a log.
    // The two differ only after char 32, so the index cannot tell them apart.
    let blob = "R8sTn4kQm2vX7wLp0zYc5bHj9dFgA1eKuI3oP6rT7wZq";
    let twin = "R8sTn4kQm2vX7wLp0zYc5bHj9dFgA1eKDIFFERENTTAIL";
    assert_eq!(blob[..32], twin[..32]);
    fx.write_session(
        "session-blob",
        &format!("{}\n", jsonl_assistant(&format!("token={blob} rejected"))),
    );
    fx.write_session(
        "session-twin",
        &format!("{}\n", jsonl_assistant(&format!("token={twin} accepted"))),
    );
    fx.full_reindex()?;

    // Both sessions share the indexed prefix, but only the one holding the
    // full string survives: the snippet pass verifies it against the file.
    let hits = fx.search(blob, SearchMode::Only(IndexKind::Word));
    assert_session(&hits, "session-blob");
    assert!(
        !hits.iter().any(|s| s.session_id == "session-twin"),
        "prefix twin must not pass as a match: {:?}",
        hits.iter().map(|s| &s.session_id).collect::<Vec<_>>()
    );
    assert_session(&fx.search(blob, SearchMode::Auto), "session-blob");
    Ok(())
}

#[test]
fn long_identifier_under_the_cap_stays_in_the_word_index() -> Result<()> {
    let fx = Fixture::new()?;
    // 31 chars, one below the cap: a real identifier must not be swept up.
    let ident = "shouldcancelpendingattachonclos";
    fx.write_session(
        "session-ident",
        &format!("{}\n", jsonl_user(&format!("added {ident} to the suite"))),
    );
    fx.full_reindex()?;
    assert_session(
        &fx.search(ident, SearchMode::Only(IndexKind::Word)),
        "session-ident",
    );
    Ok(())
}

#[test]
fn cyrillic_words_are_capped_by_chars_not_bytes() -> Result<()> {
    let fx = Fixture::new()?;
    // 25 Cyrillic chars = 50 bytes: over a byte-based cap, under the char one.
    let word = "переиспользованиеконтекст";
    assert_eq!(word.chars().count(), 25);
    fx.write_session(
        "session-cyr",
        &format!("{}\n", jsonl_user(&format!("обсудили {word} подробно"))),
    );
    fx.full_reindex()?;
    assert_session(
        &fx.search(word, SearchMode::Only(IndexKind::Word)),
        "session-cyr",
    );
    Ok(())
}

/// One jsonl line dated `days_ago`, so ranking tests can age a session.
fn jsonl_user_aged(text: &str, days_ago: i64) -> String {
    let ts = chrono::Utc::now() - chrono::Duration::days(days_ago);
    format!(
        r#"{{"type":"user","timestamp":"{}","message":{{"content":"{}"}}}}"#,
        ts.to_rfc3339(),
        text.replace('"', "\\\"")
    )
}

#[test]
fn relevance_beats_recency_but_time_sort_still_works() -> Result<()> {
    let fx = Fixture::new()?;
    // Nine months old, the query all over it: the session that actually
    // answers "how did we deal with this".
    let deep: String = (0..12)
        .map(|i| format!("{}\n", jsonl_user_aged(&format!("kubelet eviction threshold pass {i}"), 270)))
        .collect();
    fx.write_session("session-deep", &deep);
    // Yesterday, one passing mention.
    fx.write_session(
        "session-fresh",
        &format!("{}\n", jsonl_user_aged("renamed a file, kubelet untouched", 1)),
    );
    fx.full_reindex()?;

    let by_score = fx.search_sorted("kubelet", SortBy::Relevance);
    assert_eq!(
        by_score.first().map(|s| s.session_id.as_str()),
        Some("session-deep"),
        "relevance order: {:?}",
        by_score.iter().map(|s| (&s.session_id, s.score)).collect::<Vec<_>>()
    );

    let by_time = fx.search_sorted("kubelet", SortBy::Time);
    assert_eq!(
        by_time.first().map(|s| s.session_id.as_str()),
        Some("session-fresh"),
        "time order: {:?}",
        by_time.iter().map(|s| &s.session_id).collect::<Vec<_>>()
    );
    Ok(())
}

#[test]
fn a_fresh_equal_match_wins_on_recency() -> Result<()> {
    let fx = Fixture::new()?;
    // Same text, same shape, different age: recency is the tiebreaker.
    for (name, age) in [("session-older", 400), ("session-newer", 2)] {
        let body: String = (0..4)
            .map(|i| format!("{}\n", jsonl_user_aged(&format!("keychain unlock prompt {i}"), age)))
            .collect();
        fx.write_session(name, &body);
    }
    fx.full_reindex()?;

    let ranked = fx.search_sorted("keychain", SortBy::Relevance);
    assert_eq!(
        ranked.first().map(|s| s.session_id.as_str()),
        Some("session-newer"),
        "ranked: {:?}",
        ranked.iter().map(|s| (&s.session_id, s.score)).collect::<Vec<_>>()
    );
    Ok(())
}


/// Segments and deleted documents in one main sub-index.
fn main_segment_stats(fx: &Fixture, kind: IndexKind) -> Result<(usize, u32)> {
    let index = tantivy::Index::open_in_dir(subindex_dir(&fx.index_dir, kind))?;
    let metas = index.searchable_segment_metas()?;
    let dead = metas.iter().map(|m| m.num_deleted_docs()).sum();
    Ok((metas.len(), dead))
}

/// One daemon cycle as it happens in real use: the active session grows (its
/// append lands in delta, the flush re-adds it whole into main and marks the
/// previous copy deleted) and a new session appears. The new session is what
/// keeps each segment alive — with the active one alone, tantivy drops a
/// segment the moment all of its documents are deleted, and nothing piles up.
fn daemon_cycle(fx: &Fixture, session_path: &std::path::Path, line: &str) -> Result<()> {
    let mut content = fs::read_to_string(session_path)?;
    content.push_str(&jsonl_user(line));
    content.push('\n');
    fs::write(session_path, content)?;
    let n = fs::read_dir(&fx.project_dir)?.count();
    fx.write_session(&format!("session-new-{n}"), &format!("{}\n", jsonl_user("unrelated chatter")));
    fx.update()?;
    flush_delta(&fx.index_dir, WriterConfig::for_incremental_update(), false)?;
    Ok(())
}

#[test]
fn optimize_main_folds_flush_segments_and_reclaims_dead_copies() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-growing",
        &format!("{}\n", jsonl_user("opening MAINFOLD line")),
    );
    // A session that never changes shares the reindex segment, so that segment
    // outlives the growing session's first flush and keeps its dead copy —
    // exactly how the big post-reindex segments rot in real use. One worker
    // thread, or each file would get a segment of its own and die whole.
    fx.write_session("session-stable", &format!("{}\n", jsonl_user("stable neighbour")));
    let config = WriterConfig {
        worker_threads: Some(1),
        ..WriterConfig::for_full_reindex()
    };
    run_full_reindex(
        &fx.source,
        &fx.index_dir,
        &IndexKind::all(),
        collect_cct_layout(&fx.source)?,
        config,
        false,
    )?;

    // What the daemon did for ten weeks: flush after flush, never a merge.
    for i in 0..MAIN_OPTIMIZE_MIN_SEGMENTS {
        daemon_cycle(&fx, &session_path, &format!("growth MAINFOLD {i}"))?;
    }
    let (before, dead_before) = main_segment_stats(&fx, IndexKind::Word)?;
    assert!(
        before >= MAIN_OPTIMIZE_MIN_SEGMENTS,
        "each flush should add a main segment, got {before}"
    );
    assert!(dead_before > 0, "re-added sessions must leave dead copies behind");

    assert!(optimize_main(&fx.index_dir, WriterConfig::for_delta_update())?.is_some());
    let (after, dead_after) = main_segment_stats(&fx, IndexKind::Word)?;
    assert!(
        after <= MAIN_KEEP_LARGEST + 1,
        "small segments should fold into one: {before} -> {after}"
    );
    assert_eq!(dead_after, 0, "merging must drop the dead copies");

    // Same answers as before the merge: the first and the latest line, once.
    let hits = fx.search("MAINFOLD", SearchMode::Auto);
    assert_eq!(hits.len(), 1, "one session, not one per stale copy");
    assert_session(&fx.search("opening", SearchMode::Auto), "session-growing");
    let last = format!("growth MAINFOLD {}", MAIN_OPTIMIZE_MIN_SEGMENTS - 1);
    assert_session(&fx.search(&last, SearchMode::Auto), "session-growing");

    // A tidy main is left alone.
    assert!(optimize_main(&fx.index_dir, WriterConfig::for_delta_update())?.is_none());
    Ok(())
}

#[test]
fn tidy_keeps_main_bounded_across_many_daemon_cycles() -> Result<()> {
    let fx = Fixture::new()?;
    let session_path = fx.write_session(
        "session-daemon",
        &format!("{}\n", jsonl_user("start BOUNDED")),
    );
    fx.full_reindex()?;

    // Twice the threshold: without tidy this ends with as many segments as
    // cycles (see the test above); with it, main never reaches the threshold.
    let cycles = MAIN_OPTIMIZE_MIN_SEGMENTS * 2;
    let mut peak = 0;
    for i in 0..cycles {
        daemon_cycle(&fx, &session_path, &format!("cycle BOUNDED {i}"))?;
        tidy_index(&fx.index_dir)?;
        peak = peak.max(main_segment_stats(&fx, IndexKind::Word)?.0);
    }
    assert!(
        peak < MAIN_OPTIMIZE_MIN_SEGMENTS,
        "main should never fragment past the threshold, peaked at {peak} over {cycles} cycles"
    );
    assert_session(
        &fx.search(&format!("cycle BOUNDED {}", cycles - 1), SearchMode::Auto),
        "session-daemon",
    );
    Ok(())
}

/// The CLAUDE.md/memory attachment Claude Code writes into every session.
fn jsonl_memory_attachment(text: &str) -> String {
    format!(
        r#"{{"type":"attachment","timestamp":"2026-10-01T12:00:00Z","attachment":{{"type":"instructions","files":[{{"path":"/Users/x/.claude/CLAUDE.md","type":"User","content":"{}"}}]}}}}"#,
        text.replace('"', "\\\"")
    )
}

#[test]
fn memory_attachments_do_not_make_every_session_match() -> Result<()> {
    let fx = Fixture::new()?;
    let memory = jsonl_memory_attachment("always use MEMOTERM for commits");
    for i in 0..5 {
        fx.write_session(
            &format!("session-boiler-{i}"),
            &format!("{memory}\n{}\n", jsonl_user("unrelated work")),
        );
    }
    fx.write_session(
        "session-real",
        &format!("{memory}\n{}\n", jsonl_user("why does MEMOTERM break the hook")),
    );
    fx.full_reindex()?;

    let hits = fx.search("MEMOTERM", SearchMode::Auto);
    assert_eq!(
        hits.iter().map(|s| s.session_id.as_str()).collect::<Vec<_>>(),
        vec!["session-real"],
        "only the session that discussed it, not every session carrying the memory"
    );
    assert!(
        hits[0].snippets.iter().all(|s| !s.text.contains("for commits")),
        "the memory line itself must not be shown as a snippet: {:?}",
        hits[0].snippets
    );
    Ok(())
}
