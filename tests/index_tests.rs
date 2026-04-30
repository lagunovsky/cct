//! Integration tests for the Tantivy multi-index backend.
//!
//! Each test builds a self-contained `projects/` tree in a tempdir, runs the
//! relevant index pipeline against a fresh `index_dir`, then asserts what is
//! visible to `run_search`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use cct::fs_scan::{collect_cct_layout, collect_one_project};
use cct::index::build::{run_full_reindex, subindex_segment_count};
use cct::index::catalog::read_delta_catalog;
use cct::index::compact::optimize_delta;
use cct::index::schema::delta_dir;
use cct::index::search::{run_search, SearchMode, SearchOptions, SessionResult};
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
            query: query.to_string(),
            mode,
            limit: 20,
            context_chars: 80,
            project_prefix: None,
            fuzzy: false,
        };
        run_search(&self.index_dir, &opts).unwrap_or_default()
    }

    fn search_fuzzy(&self, query: &str) -> Vec<SessionResult> {
        let opts = SearchOptions {
            query: query.to_string(),
            mode: SearchMode::Auto,
            limit: 20,
            context_chars: 80,
            project_prefix: None,
            fuzzy: true,
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
    fx.write_session(
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
fn optimize_delta_compacts_append_segments() -> Result<()> {
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
    assert!(optimize_delta(&fx.index_dir, WriterConfig::for_delta_update())?.is_some());
    assert_eq!(subindex_segment_count(&delta, IndexKind::Word)?, 1);
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
    let opts = SearchOptions {
        query: "boundaryterm".to_string(),
        mode: SearchMode::Auto,
        limit: 20,
        context_chars: 80,
        project_prefix: Some(fx.project_dir.clone()),
        fuzzy: false,
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
    assert!(combined.contains("Марат"), "expected Марат in fuzzy snippets");
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
    assert_session(&fx.search("SCOPEDOLD", SearchMode::Auto), "session-scoped-upd");
    assert_session(&fx.search("OTHERTERM", SearchMode::Auto), "session-other");

    // Mutate the scoped project: drop the old session, add a new one.
    fs::remove_file(&scoped_session)?;
    fs::write(
        fx.project_dir.join("session-scoped-new.jsonl"),
        format!("{}\n", jsonl_user("alpha SCOPEDNEW")),
    )?;

    let scope = fx.project_dir.clone();
    let files = collect_one_project(&scope)?;
    let plan =
        build_plan_scoped(&fx.index_dir, Some(fx.source.clone()), files, Some(&scope))?;
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
