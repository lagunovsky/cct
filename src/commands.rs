//! Top-level command implementations.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::cli;
use crate::fs_scan::{
    collect_archived_layout, collect_cct_layout, collect_one_project, DiscoveredFile,
};
use crate::index::build::run_full_reindex;
use crate::index::catalog::lock_index_exclusive;
use crate::index::compact::{optimize_delta, run_compact};
use crate::index::search::{self, SearchOptions};
use crate::index::update::{build_plan, build_plan_scoped, flush_delta, needs_update, run_update};
use crate::index::writer::{MergePolicyMode, WriterConfig};
use crate::index::IndexKind;
use crate::path_codec::{
    absolutize_lexical, encode_project_dir, index_root, projects_dir, short_project,
};
use crate::recent::{show_recent_sessions, RecentOptions};
use crate::term::{Spinner, BOLD, DIM, RESET};
use crate::text::extract_text;

pub fn run(cmd: Option<cli::Cmd>) -> Result<()> {
    match cmd {
        None => show_recent_sessions(RecentOptions { limit: 20 }),
        Some(cli::Cmd::Recent(args)) => show_recent_sessions(RecentOptions { limit: args.limit }),
        Some(cli::Cmd::Search(args)) => run_search(args),
        Some(cli::Cmd::Blame(args)) => run_blame(args),
        Some(cli::Cmd::Reindex(args)) => run_reindex(args),
        Some(cli::Cmd::Update(args)) => run_update_cmd(args),
        Some(cli::Cmd::Compact(args)) => run_compact_cmd(args),
        Some(cli::Cmd::Rename(args)) => run_rename(args),
        Some(cli::Cmd::Transfer(args)) => run_transfer(args),
        Some(cli::Cmd::Daemon(args)) => match args.action {
            cli::DaemonAction::Install => crate::daemon::install_daemon(),
            cli::DaemonAction::Uninstall => crate::daemon::uninstall_daemon(),
            cli::DaemonAction::Update => crate::daemon::run_background_update(),
        },
        Some(cli::Cmd::CleanupPeriod(args)) => run_cleanup_period(args),
        Some(cli::Cmd::Archive(args)) => run_archive_cmd(args),
        Some(cli::Cmd::Unarchive(args)) => {
            crate::archive::run_unarchive(&args.ids, args.keep_mtime)
        }
    }
}

fn run_archive_cmd(args: cli::ArchiveCmd) -> Result<()> {
    if let Some(auto) = args.auto.as_deref() {
        if auto == "off" {
            let path = crate::config::set_auto_archive("off")?;
            println!("Background archiving disabled ({}).", path.display());
        } else {
            let days = crate::archive::parse_age_days(auto)?;
            let path = crate::config::set_auto_archive(auto)?;
            println!(
                "Background archiving enabled: the daemon will archive sessions older than {days} days ({}).",
                path.display()
            );
            if !crate::daemon::daemon_installed() {
                println!("{DIM}Note: the daemon is not installed — run `cct daemon install`.{RESET}");
            }
        }
        return Ok(());
    }
    let older_than_days = crate::archive::parse_age_days(&args.older_than)?;
    let project = args
        .project
        .as_deref()
        .map(absolutize_lexical)
        .transpose()?;
    let index_dir = index_root()?;
    // Flush pending delta ranges first (under the same lock the daemon uses):
    // a session with un-flushed appends would lose its tail from the index
    // when the live file disappears mid-flush.
    let _lock = if index_dir.exists() && !args.dry_run {
        let lock = lock_index_exclusive(&index_dir)?;
        flush_delta(&index_dir, WriterConfig::for_incremental_update(), false)?;
        Some(lock)
    } else {
        None
    };
    crate::archive::run_archive(crate::archive::ArchiveOptions {
        older_than_days,
        project,
        dry_run: args.dry_run,
        skip_confirm: args.yes,
        quiet: false,
    })
}

fn run_cleanup_period(args: cli::CleanupPeriodCmd) -> Result<()> {
    use crate::config;
    if args.no_warn {
        let path = config::set_cleanup_warning(false)?;
        println!("Low-retention warning disabled ({}).", path.display());
    } else if args.warn {
        let path = config::set_cleanup_warning(true)?;
        println!("Low-retention warning enabled ({}).", path.display());
    }
    if let Some(days) = args.days {
        let path = config::write_cleanup_period_days(days)?;
        println!(
            "cleanupPeriodDays = {days} written to {}.\nClaude Code now retains session transcripts for {days} days.",
            path.display()
        );
        return Ok(());
    }
    if args.no_warn || args.warn {
        return Ok(());
    }

    let configured = config::read_cleanup_period_days()?;
    match configured {
        Some(days) => println!(
            "cleanupPeriodDays = {days} ({})",
            config::claude_settings_path()?.display()
        ),
        None => println!(
            "cleanupPeriodDays is not set — Claude Code uses its default of {} days.",
            config::CLAUDE_DEFAULT_CLEANUP_DAYS
        ),
    }
    println!("Set it with: cct cleanup-period <days>   (e.g. cct cleanup-period 3650)");
    if !config::cleanup_warning_enabled() {
        println!("{DIM}Low-retention warning: disabled (cct cleanup-period --warn to re-enable){RESET}");
    }
    Ok(())
}

/// Warn when Claude Code's retention is short enough that indexed sessions
/// will be deleted from under the index. Shown by reindex paths; silenceable
/// via `cct cleanup-period --no-warn`.
fn maybe_warn_short_cleanup_period() {
    use crate::config;
    if !config::cleanup_warning_enabled() {
        return;
    }
    let configured = config::read_cleanup_period_days().ok().flatten();
    let days = configured.unwrap_or(config::CLAUDE_DEFAULT_CLEANUP_DAYS);
    if days >= config::CLEANUP_WARN_THRESHOLD_DAYS {
        return;
    }
    let source = if configured.is_some() {
        "cleanupPeriodDays"
    } else {
        "cleanupPeriodDays default"
    };
    eprintln!(
        "\n{BOLD}⚠ Claude Code deletes session transcripts after {days} days{RESET} ({source}).\n\
         Old sessions disappear from disk — and from this index — as they expire.\n\
         {DIM}Raise retention:  cct cleanup-period 3650   (writes ~/.claude/settings.json){RESET}\n\
         {DIM}Silence warning:  cct cleanup-period --no-warn{RESET}\n"
    );
}

fn parse_only(raw: Option<&str>) -> Result<Option<BTreeSet<IndexKind>>> {
    let Some(s) = raw else {
        return Ok(None);
    };
    cli::parse_index_list(s)
        .map(Some)
        .ok_or_else(|| anyhow!("bad --only value: {s:?}"))
}

fn writer_config_for_reindex(args: &cli::ReindexCmd) -> Result<WriterConfig> {
    let mut config = WriterConfig::for_full_reindex();
    if let Some(mb) = args.memory_mb {
        config.memory_mb = mb;
    }
    if let Some(workers) = args.worker_threads {
        config.worker_threads = Some(workers);
    }
    if let Some(merge) = args.merge_threads {
        config.merge_threads = merge;
    }
    if let Some(policy) = &args.merge_policy {
        config.merge_policy = MergePolicyMode::parse(policy)?;
    }
    Ok(config)
}

fn writer_config_for_update(args: &cli::UpdateCmd) -> Result<WriterConfig> {
    let mut config = WriterConfig::for_incremental_update();
    if let Some(mb) = args.memory_mb {
        config.memory_mb = mb;
    }
    if let Some(workers) = args.worker_threads {
        config.worker_threads = Some(workers);
    }
    if let Some(merge) = args.merge_threads {
        config.merge_threads = merge;
    }
    if let Some(policy) = &args.merge_policy {
        config.merge_policy = MergePolicyMode::parse(policy)?;
    }
    Ok(config)
}

fn writer_config_for_compact(args: &cli::CompactCmd) -> Result<WriterConfig> {
    let mut config = WriterConfig::default();
    if let Some(mb) = args.memory_mb {
        config.memory_mb = mb;
    }
    if let Some(workers) = args.worker_threads {
        config.worker_threads = Some(workers);
    }
    if let Some(merge) = args.merge_threads {
        config.merge_threads = merge;
    }
    Ok(config)
}

/// Files for a full reindex: live sessions plus archived ones mapped back to
/// their logical live paths, so a from-scratch rebuild keeps serving the
/// archive. Incremental updates preserve archived docs already in the catalog,
/// but a full reindex starts from an empty index and would otherwise drop them.
fn collect_for_full_reindex(source: &Path) -> Result<Vec<DiscoveredFile>> {
    let mut files = collect_cct_layout(source)?;
    let archive = crate::archive::archive_root()?;
    let mut seen: HashSet<PathBuf> = files.iter().map(|f| f.path.clone()).collect();
    for f in collect_archived_layout(&archive, source)? {
        // A live file and its archive twin can momentarily coexist mid-archive;
        // prefer the live copy and never index the same session twice.
        if seen.insert(f.path.clone()) {
            files.push(f);
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(files)
}

pub fn run_reindex(args: cli::ReindexCmd) -> Result<()> {
    maybe_warn_short_cleanup_period();
    let config = writer_config_for_reindex(&args)?;
    let only = parse_only(args.only.as_deref())?;
    let enabled = only.unwrap_or_else(IndexKind::all);
    let source = projects_dir()?;
    let index_dir = index_root()?;
    let files = collect_for_full_reindex(&source)?;
    let start = Instant::now();
    let _lock = lock_index_exclusive(&index_dir)?;
    run_full_reindex(&source, &index_dir, &enabled, files, config, true)?;
    eprintln!("Done in {:?}", start.elapsed());
    Ok(())
}

pub fn run_update_cmd(args: cli::UpdateCmd) -> Result<()> {
    let config = writer_config_for_update(&args)?;
    let source = projects_dir()?;
    let index_dir = index_root()?;
    if !index_dir.exists() {
        bail!(
            "no index at {} — run `cct reindex` first",
            index_dir.display()
        );
    }
    // The lock must cover plan building too: a plan built against a catalog
    // another process is rewriting would roll its entries back on write.
    let _lock = lock_index_exclusive(&index_dir)?;
    let files = collect_cct_layout(&source)?;
    let plan = build_plan(&index_dir, Some(source.clone()), files)?;
    if !needs_update(&plan) {
        println!("Nothing to update.");
        return Ok(());
    }
    run_update(&index_dir, plan, config, true)?;
    Ok(())
}

pub fn run_compact_cmd(args: cli::CompactCmd) -> Result<()> {
    let config = writer_config_for_compact(&args)?;
    let only = parse_only(args.only.as_deref())?;
    let index_dir = index_root()?;
    if !index_dir.exists() {
        bail!("no index at {}", index_dir.display());
    }
    let _lock = lock_index_exclusive(&index_dir)?;
    run_compact(&index_dir, only, args.target_segments, config)
}

pub fn run_rename(args: cli::RenameCmd) -> Result<()> {
    let (old_arg, new_arg) = match args.paths.len() {
        1 => (".".to_string(), args.paths[0].clone()),
        2 => (args.paths[0].clone(), args.paths[1].clone()),
        _ => bail!("rename takes 1 or 2 path arguments"),
    };
    crate::rename::rename_project(crate::rename::RenameArgs {
        old_arg,
        new_arg,
        move_dir: !args.no_move_dir,
        dry_run: args.dry_run,
    })
}

pub fn run_transfer(args: cli::TransferCmd) -> Result<()> {
    crate::transfer::run_transfer(crate::transfer::TransferArgs {
        target: args.target,
        from: args.from,
        session_args: args.session_ids,
        dry_run: args.dry_run,
        skip_confirm: args.yes,
    })
}

/// Used by daemon update — silent except for errors.
pub fn run_silent_update(index_dir: &Path, config: WriterConfig) -> Result<()> {
    let source = projects_dir()?;
    if !index_dir.exists() {
        return Ok(());
    }
    let _lock = lock_index_exclusive(index_dir)?;
    let files = collect_cct_layout(&source)?;
    let plan = build_plan(index_dir, Some(source.clone()), files)?;
    if !needs_update(&plan) {
        return flush_delta(index_dir, config, false);
    }
    run_update(index_dir, plan, config, false)?;
    flush_delta(index_dir, config, false)?;
    Ok(())
}

pub fn run_blame(args: cli::BlameCmd) -> Result<()> {
    let query = crate::blame::PathQuery::new(&args.path)
        .ok_or_else(|| anyhow!("{:?} has no filename to blame", args.path))?;

    let index_dir = index_root()?;
    let source = projects_dir()?;
    let resolved = resolve_project_scope(args.global, args.project.as_deref())?;
    let project_prefix = resolved.as_ref().map(|r| r.prefix.clone());

    let start = Instant::now();
    if !args.json {
        let scope = resolved
            .as_ref()
            .map(|r| r.label.clone())
            .unwrap_or_else(|| "all projects".to_string());
        println!(
            "{BOLD}Sessions that touched:{RESET} {}  {DIM}({scope}){RESET}",
            query.basename
        );
    }

    refresh_index_for_search(
        &index_dir,
        &source,
        project_prefix.as_deref(),
        args.no_update,
        args.verbose >= 2,
    )?;

    // Over-fetch: the basename is what the index can answer, and the verify
    // pass below drops namesakes and prose, so the display limit has to be
    // applied after ranking, not before it.
    let sp = Spinner::start("searching...");
    let opts = SearchOptions {
        queries: vec![query.basename.clone()],
        mode: search::SearchMode::Auto,
        limit: args.limit.saturating_mul(4).max(40),
        context_chars: args.context,
        project_prefix,
        fuzzy: false,
        // Relevance already damps by age; blame re-sorts by how specifically
        // each session named the file, keeping this order within each tier.
        sort: search::SortBy::Relevance,
    };
    let mut sessions = search::run_search(&index_dir, &opts)?;
    sp.stop();

    // Verify, then tier: sessions naming a qualified path first, bare
    // mentions after, anything that only looked like a match dropped.
    let mut tiered: Vec<(crate::blame::Mention, search::SessionResult)> = Vec::new();
    for session in sessions.drain(..) {
        let best = session
            .snippets
            .iter()
            .map(|s| query.classify(&s.text))
            .max()
            .unwrap_or(crate::blame::Mention::None);
        if best == crate::blame::Mention::None {
            continue;
        }
        tiered.push((best, session));
    }
    // Stable: within a tier the relevance order from the search survives.
    tiered.sort_by_key(|(mention, _)| std::cmp::Reverse(*mention));

    let total = tiered.len();
    let to_show = total.min(args.limit);
    if args.json {
        let visible: Vec<&search::SessionResult> =
            tiered.iter().take(to_show).map(|(_, s)| s).collect();
        let stats_per_session = compute_session_stats_parallel(&visible);
        for ((mention, s), stats) in tiered.iter().take(to_show).zip(stats_per_session) {
            let mut value = search::session_json(
                s,
                stats.prompts,
                stats.tools,
                stats.bytes,
                args.verbose >= 1,
            );
            value["mention"] = serde_json::json!(match mention {
                crate::blame::Mention::Qualified => "qualified",
                _ => "bare",
            });
            println!("{value}");
        }
        return Ok(());
    }

    if total == 0 {
        println!("\n{DIM}No session mentions {}{RESET}", query.basename);
        return Ok(());
    }
    let visible: Vec<&search::SessionResult> =
        tiered.iter().take(to_show).map(|(_, s)| s).collect();
    let stats_per_session = compute_session_stats_parallel(&visible);
    let queries = vec![query.basename.clone()];
    for ((_, s), stats) in tiered.iter().take(to_show).zip(stats_per_session) {
        search::print_session_header(s, stats.prompts, stats.tools, stats.bytes);
        search::print_snippets(&s.snippets, &queries, args.verbose >= 1, false);
        if args.verbose >= 1 {
            for f in &s.files {
                println!("  {DIM}{}{RESET}", f.display());
            }
        }
    }
    if total > to_show {
        println!(
            "\n{DIM}...and {} more sessions. Use -n to show more.{RESET}",
            total - to_show
        );
    }
    println!(
        "\n{DIM}{to_show} sessions in {elapsed:?}{RESET}",
        elapsed = start.elapsed()
    );
    Ok(())
}

/// How long the pre-search index work took, for the `-vv` timing breakdown.
struct IndexRefresh {
    scan_elapsed: Duration,
    update_elapsed: Duration,
}

/// Bring the index up to date before a read-only query: build it from scratch
/// when missing, otherwise scan for changes and apply the incremental update.
/// Shared by `search` and `blame`; the writer lock is released on return, since
/// readers do not take it.
fn refresh_index_for_search(
    index_dir: &Path,
    source: &Path,
    project_prefix: Option<&Path>,
    no_update: bool,
    verbose_index: bool,
) -> Result<IndexRefresh> {
    let mut out = IndexRefresh {
        scan_elapsed: Duration::ZERO,
        update_elapsed: Duration::ZERO,
    };
    if no_update {
        return Ok(out);
    }
    let _lock = lock_index_exclusive(index_dir)?;
    if !index_dir.exists() {
        maybe_warn_short_cleanup_period();
        eprintln!("{DIM}Building initial index — this may take a while...{RESET}");
        let config = WriterConfig::for_full_reindex();
        let files = collect_for_full_reindex(source)?;
        let upd_start = Instant::now();
        run_full_reindex(
            source,
            index_dir,
            &IndexKind::all(),
            files,
            config,
            verbose_index,
        )?;
        out.update_elapsed = upd_start.elapsed();
        return Ok(out);
    }

    let scan_start = Instant::now();
    let sp = Spinner::start("scanning sessions...");
    let (files, scope) = if let Some(prefix) = project_prefix {
        (collect_one_project(prefix)?, Some(prefix))
    } else {
        (collect_cct_layout(source)?, None)
    };
    let plan = build_plan_scoped(index_dir, Some(source.to_path_buf()), files, scope)?;
    let need = needs_update(&plan);
    out.scan_elapsed = scan_start.elapsed();
    if !need {
        sp.stop();
        return Ok(out);
    }
    let upd_start = Instant::now();
    if verbose_index {
        sp.stop();
        run_update(index_dir, plan, WriterConfig::for_incremental_update(), true)?;
    } else {
        sp.set_message("updating index...");
        run_update(index_dir, plan, WriterConfig::for_incremental_update(), false)?;
        sp.stop();
    }
    out.update_elapsed = upd_start.elapsed();
    Ok(out)
}

pub fn run_search(args: cli::SearchCmd) -> Result<()> {
    // Each argument is one query string; a session must match all of them.
    let queries: Vec<String> = args
        .query
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if queries.is_empty() {
        bail!("empty query");
    }
    let query_label = if queries.len() == 1 {
        queries[0].clone()
    } else {
        queries
            .iter()
            .map(|q| format!("\"{q}\""))
            .collect::<Vec<_>>()
            .join(" AND ")
    };

    let index_dir = index_root()?;
    let source = projects_dir()?;

    let resolved = resolve_project_prefix(&args)?;
    let project_prefix = resolved.as_ref().map(|r| r.prefix.clone());
    let project_label = resolved.as_ref().map(|r| r.label.clone());

    let start = Instant::now();
    if !args.json {
        print_search_header(&query_label, &args, project_label.as_deref());
    }

    let verbose_index = args.verbose >= 2;
    let IndexRefresh {
        scan_elapsed,
        update_elapsed,
    } = refresh_index_for_search(
        &index_dir,
        &source,
        project_prefix.as_deref(),
        args.no_update,
        verbose_index,
    )?;
    let sp = Spinner::start("searching...");

    let mode = match args.mode {
        cli::SearchMode::Auto => search::SearchMode::Auto,
        cli::SearchMode::All => search::SearchMode::All,
        cli::SearchMode::Word => search::SearchMode::Only(IndexKind::Word),
        cli::SearchMode::Trigram => search::SearchMode::Only(IndexKind::Trigram),
        cli::SearchMode::Path => search::SearchMode::Only(IndexKind::Path),
    };
    let sort = match args.sort {
        cli::SortBy::Relevance => search::SortBy::Relevance,
        cli::SortBy::Time => search::SortBy::Time,
    };
    let opts = SearchOptions {
        queries: queries.clone(),
        mode,
        limit: args.limit,
        context_chars: args.context,
        project_prefix: project_prefix.clone(),
        fuzzy: args.fuzzy,
        sort,
    };
    let search_start = Instant::now();
    let mut sessions = search::run_search(&index_dir, &opts)?;
    let search_elapsed = search_start.elapsed();
    sp.stop();

    let render_start = Instant::now();
    if args.json {
        // Machine mode: one JSON object per session, nothing else on stdout.
        // Content selection matches the human renderer — -v widens snippets
        // to agent/hook matches and adds file paths — so agents opt into
        // depth instead of paying tokens for it on every search.
        sessions.retain(|s| {
            !search::select_visible_snippets(&s.snippets, args.verbose >= 1).is_empty()
        });
        let to_show = sessions.len().min(args.limit);
        let visible: Vec<&search::SessionResult> = sessions.iter().take(to_show).collect();
        let stats_per_session = compute_session_stats_parallel(&visible);
        for (s, stats) in visible.iter().zip(stats_per_session) {
            let value = search::session_json(
                s,
                stats.prompts,
                stats.tools,
                stats.bytes,
                args.verbose >= 1,
            );
            println!("{value}");
        }
        // Same post-search index hygiene as the human path, quiet on success.
        if let Err(err) =
            lock_index_exclusive(&index_dir).and_then(|_lock| optimize_delta(&index_dir, WriterConfig::for_delta_update()))
        {
            eprintln!("delta optimize skipped: {err:#}");
        }
        return Ok(());
    }
    sessions
        .retain(|s| !search::select_visible_snippets(&s.snippets, args.verbose >= 1).is_empty());
    let total = sessions.len();
    let to_show = sessions.len().min(args.limit);
    if total == 0 {
        println!("\n{DIM}No matches found{RESET}");
    } else if to_show == 0 {
        // -n 0: matches exist but none are displayed.
        println!("\n{DIM}{total} match(es). Use -n to show them.{RESET}");
    } else {
        // Precompute stats in parallel — each session's stats require reading
        // and parsing its full JSONL, which is the dominant render cost when
        // many sessions match.
        let visible: Vec<&search::SessionResult> = sessions.iter().take(to_show).collect();
        let stats_per_session = compute_session_stats_parallel(&visible);
        for (s, stats) in sessions.iter_mut().take(to_show).zip(stats_per_session) {
            search::print_session_header(s, stats.prompts, stats.tools, stats.bytes);
            search::print_snippets(&s.snippets, &queries, args.verbose >= 1, args.fuzzy);
            if args.verbose >= 1 {
                for f in &s.files {
                    println!("  {DIM}{}{RESET}", f.display());
                }
            }
        }
        if total > to_show {
            println!(
                "\n{DIM}...and {} more results. Use -n to show more.{RESET}",
                total - to_show
            );
        }
    }
    let render_elapsed = render_start.elapsed();

    let mut delta_optimize_error: Option<String> = None;
    let delta_config = WriterConfig::for_delta_update();
    let delta_optimize_elapsed = match lock_index_exclusive(&index_dir)
        .and_then(|_lock| optimize_delta(&index_dir, delta_config))
    {
        Ok(elapsed) => elapsed,
        Err(err) => {
            delta_optimize_error = Some(format!("{err:#}"));
            None
        }
    };

    let elapsed = start.elapsed();
    let breakdown = if args.verbose >= 2 {
        let parts = search_timing_parts(
            scan_elapsed,
            update_elapsed,
            search_elapsed,
            render_elapsed,
            delta_optimize_elapsed,
        );
        if parts.len() > 1 {
            format!("{DIM} ({}){RESET}", format_timing_parts(&parts))
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    println!(
        "\n{DIM}{count} sessions in {elapsed:?}{RESET}{breakdown}",
        count = to_show,
    );
    if args.verbose < 2 {
        if let Some(delta_elapsed) = delta_optimize_elapsed {
            if delta_elapsed >= Duration::from_secs(1) {
                println!("{DIM}delta optimize {delta_elapsed:?}{RESET}");
            }
        }
    }
    if let Some(err) = delta_optimize_error {
        eprintln!("{DIM}delta optimize skipped: {err}{RESET}");
    }
    Ok(())
}

fn search_timing_parts(
    scan_elapsed: Duration,
    update_elapsed: Duration,
    search_elapsed: Duration,
    render_elapsed: Duration,
    delta_optimize_elapsed: Option<Duration>,
) -> Vec<(&'static str, Duration)> {
    let mut parts: Vec<(&'static str, Duration)> = Vec::new();
    if scan_elapsed.as_millis() >= 5 {
        parts.push(("scan", scan_elapsed));
    }
    if update_elapsed.as_millis() >= 5 {
        parts.push(("update", update_elapsed));
    }
    parts.push(("search", search_elapsed));
    if render_elapsed.as_millis() >= 5 {
        parts.push(("render", render_elapsed));
    }
    if let Some(delta_elapsed) = delta_optimize_elapsed {
        parts.push(("delta optimize", delta_elapsed));
    }
    parts
}

fn format_timing_parts(parts: &[(&str, Duration)]) -> String {
    parts
        .iter()
        .map(|(label, duration)| format!("{label} {}", format_duration(*duration)))
        .collect::<Vec<_>>()
        .join(" + ")
}

fn format_duration(duration: Duration) -> String {
    let ms = duration.as_secs_f64() * 1000.0;
    if ms >= 100.0 {
        format!("{ms:.0}ms")
    } else if ms >= 10.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{ms:.2}ms")
    }
}

fn print_search_header(query: &str, args: &cli::SearchCmd, label: Option<&str>) {
    if args.global || label.is_none() {
        println!("{BOLD}Searching for:{RESET} {query}  {DIM}(global){RESET}");
    } else if args.project.is_some() {
        if let Some(label) = label {
            println!("{BOLD}Searching for:{RESET} {query}  {DIM}in {label}{RESET}");
        }
    }
}

struct ResolvedProject {
    prefix: PathBuf,
    label: String,
}

fn resolve_project_prefix(args: &cli::SearchCmd) -> Result<Option<ResolvedProject>> {
    resolve_project_scope(args.global, args.project.as_deref())
}

fn resolve_project_scope(global: bool, project: Option<&str>) -> Result<Option<ResolvedProject>> {
    if global && project.is_some() {
        bail!("--global and --project are mutually exclusive");
    }
    if global {
        return Ok(None);
    }
    let abs = match project {
        Some(".") | None => std::env::current_dir()?,
        Some(p) => absolutize_lexical(p)?,
    };
    let label = short_project(&abs.to_string_lossy());
    let prefix = projects_dir()?.join(encode_project_dir(&abs.to_string_lossy()));
    if !prefix.exists() {
        // For an explicit -p this makes a typo'd path distinguishable from
        // "no matches found".
        eprintln!(
            "No Claude Code sessions in {label}. Use 'cct search -g <query>' to search across all projects."
        );
        return Err(anyhow!("no sessions in {label}"));
    }
    Ok(Some(ResolvedProject { prefix, label }))
}

/// Read each session jsonl file (small in practice) to derive prompts/tools/size for display.
fn compute_session_stats(session: &search::SessionResult) -> search::DisplayStats {
    if let Some(stats) = session.display_stats {
        return stats;
    }
    let mut out = search::DisplayStats::default();
    for path in &session.files {
        let Ok(meta) = fs::metadata(path) else {
            continue;
        };
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if meta.len() > out.bytes {
            out.bytes = meta.len();
        }
        if let Ok(data) = fs::read(path) {
            let (_, stats) = extract_text(&data);
            out.prompts = out.prompts.max(stats.prompts);
            out.tools = out.tools.max(stats.tools);
        }
    }
    out
}

fn compute_session_stats_parallel(
    sessions: &[&search::SessionResult],
) -> Vec<search::DisplayStats> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let n = sessions.len();
    if n <= 1 {
        return sessions.iter().map(|s| compute_session_stats(s)).collect();
    }
    let workers = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1)
        .min(8)
        .min(n)
        .max(1);
    if workers == 1 {
        return sessions.iter().map(|s| compute_session_stats(s)).collect();
    }
    let mut out: Vec<search::DisplayStats> = vec![search::DisplayStats::default(); n];
    let counter = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let counter = &counter;
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(move || -> Vec<(usize, search::DisplayStats)> {
                let mut local = Vec::new();
                loop {
                    let i = counter.fetch_add(1, Ordering::Relaxed);
                    if i >= sessions.len() {
                        break;
                    }
                    local.push((i, compute_session_stats(sessions[i])));
                }
                local
            }));
        }
        for h in handles {
            // Propagate worker panics — an empty DisplayStats would silently
            // show 0 prompts/0 tools/0 bytes for an affected session.
            let local = h.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
            for (i, s) in local {
                out[i] = s;
            }
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_timing_parts_include_delta_optimize_when_it_ran() {
        let parts = search_timing_parts(
            Duration::from_millis(1),
            Duration::ZERO,
            Duration::from_millis(2),
            Duration::from_millis(1),
            Some(Duration::from_millis(3)),
        );
        let labels: Vec<&str> = parts.iter().map(|(label, _)| *label).collect();

        assert_eq!(labels, vec!["search", "delta optimize"]);
        assert!(format_timing_parts(&parts).contains("delta optimize 3.00ms"));
    }
}
