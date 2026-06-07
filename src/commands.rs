//! Top-level command implementations.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};

use crate::cli;
use crate::fs_scan::{collect_cct_layout, collect_one_project};
use crate::index::build::run_full_reindex;
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
use crate::text::{extract_text, SessionStats};

pub fn run(cmd: Option<cli::Cmd>) -> Result<()> {
    match cmd {
        None => show_recent_sessions(RecentOptions { limit: 20 }),
        Some(cli::Cmd::Recent(args)) => show_recent_sessions(RecentOptions { limit: args.limit }),
        Some(cli::Cmd::Search(args)) => run_search(args),
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
    }
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

pub fn run_reindex(args: cli::ReindexCmd) -> Result<()> {
    let config = writer_config_for_reindex(&args)?;
    let only = parse_only(args.only.as_deref())?;
    let enabled = only.unwrap_or_else(IndexKind::all);
    let source = projects_dir()?;
    let index_dir = index_root()?;
    let files = collect_cct_layout(&source)?;
    let start = Instant::now();
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
pub fn run_silent_update(index_dir: &Path) -> Result<()> {
    let source = projects_dir()?;
    if !index_dir.exists() {
        return Ok(());
    }
    let files = collect_cct_layout(&source)?;
    let plan = build_plan(index_dir, Some(source.clone()), files)?;
    if !needs_update(&plan) {
        return flush_delta(index_dir, WriterConfig::for_incremental_update(), false);
    }
    run_update(
        index_dir,
        plan,
        WriterConfig::for_incremental_update(),
        false,
    )?;
    flush_delta(index_dir, WriterConfig::for_incremental_update(), false)?;
    Ok(())
}

pub fn run_search(args: cli::SearchCmd) -> Result<()> {
    let query = args.query.join(" ");
    if query.trim().is_empty() {
        bail!("empty query");
    }

    let index_dir = index_root()?;
    let source = projects_dir()?;

    let resolved = resolve_project_prefix(&args)?;
    let project_prefix = resolved.as_ref().map(|r| r.prefix.clone());
    let project_label = resolved.as_ref().map(|r| r.label.clone());

    let start = Instant::now();
    print_search_header(&query, &args, project_label.as_deref());

    let verbose_index = args.verbose >= 2;
    let mut scan_elapsed = Duration::ZERO;
    let mut update_elapsed = Duration::ZERO;
    if !args.no_update {
        if !index_dir.exists() {
            eprintln!("{DIM}Building initial index — this may take a while...{RESET}");
            let config = WriterConfig::for_full_reindex();
            let files = collect_cct_layout(&source)?;
            let upd_start = Instant::now();
            run_full_reindex(
                &source,
                &index_dir,
                &IndexKind::all(),
                files,
                config,
                verbose_index,
            )?;
            update_elapsed = upd_start.elapsed();
        } else {
            let scan_start = Instant::now();
            let sp = Spinner::start("scanning sessions...");
            let (files, scope) = if let Some(prefix) = project_prefix.as_deref() {
                (collect_one_project(prefix)?, Some(prefix))
            } else {
                (collect_cct_layout(&source)?, None)
            };
            let plan = build_plan_scoped(&index_dir, Some(source.clone()), files, scope)?;
            let need = needs_update(&plan);
            scan_elapsed = scan_start.elapsed();
            if need {
                let upd_start = Instant::now();
                if verbose_index {
                    sp.stop();
                    run_update(
                        &index_dir,
                        plan,
                        WriterConfig::for_incremental_update(),
                        true,
                    )?;
                } else {
                    sp.set_message("updating index...");
                    run_update(
                        &index_dir,
                        plan,
                        WriterConfig::for_incremental_update(),
                        false,
                    )?;
                    sp.stop();
                }
                update_elapsed = upd_start.elapsed();
            } else {
                sp.stop();
            }
        }
    }
    let sp = Spinner::start("searching...");

    let mode = match args.mode {
        cli::SearchMode::Auto => search::SearchMode::Auto,
        cli::SearchMode::All => search::SearchMode::All,
        cli::SearchMode::Word => search::SearchMode::Only(IndexKind::Word),
        cli::SearchMode::Trigram => search::SearchMode::Only(IndexKind::Trigram),
        cli::SearchMode::Path => search::SearchMode::Only(IndexKind::Path),
    };
    let opts = SearchOptions {
        query: query.clone(),
        mode,
        limit: args.limit,
        context_chars: args.context,
        project_prefix: project_prefix.clone(),
        fuzzy: args.fuzzy,
    };
    let search_start = Instant::now();
    let mut sessions = search::run_search(&index_dir, &opts)?;
    let search_elapsed = search_start.elapsed();
    sp.stop();

    let render_start = Instant::now();
    sessions.retain(|s| !search::select_visible_snippets(&s.snippets, args.verbose >= 1).is_empty());
    let total = sessions.len();
    let to_show = sessions.len().min(args.limit);
    if to_show == 0 {
        println!("\n{DIM}No matches found{RESET}");
    } else {
        // Precompute stats in parallel — each session's stats require reading
        // and parsing its full JSONL, which is the dominant render cost when
        // many sessions match.
        let visible: Vec<&search::SessionResult> = sessions.iter().take(to_show).collect();
        let stats_per_session = compute_session_stats_parallel(&visible);
        for (s, stats) in sessions.iter_mut().take(to_show).zip(stats_per_session) {
            search::print_session_header(s, stats.prompts, stats.tools, stats.bytes);
            search::print_snippets(&s.snippets, &query, args.verbose >= 1, args.fuzzy);
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

    let elapsed = start.elapsed();
    let breakdown = if args.verbose >= 2 {
        let mut parts: Vec<(&str, Duration)> = Vec::new();
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
    match optimize_delta(&index_dir, WriterConfig::for_delta_update()) {
        Ok(Some(delta_elapsed))
            if args.verbose >= 2 || delta_elapsed >= Duration::from_secs(1) =>
        {
            println!("{DIM}delta optimize {delta_elapsed:?}{RESET}");
        }
        Ok(_) => {}
        Err(err) => eprintln!("{DIM}delta optimize skipped: {err:#}{RESET}"),
    }
    Ok(())
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
    if args.global && args.project.is_some() {
        bail!("--global and --project are mutually exclusive");
    }
    if args.global {
        return Ok(None);
    }
    let abs = match args.project.as_deref() {
        Some(".") | None => std::env::current_dir()?,
        Some(p) => absolutize_lexical(p)?,
    };
    let label = short_project(&abs.to_string_lossy());
    let prefix = projects_dir()?.join(encode_project_dir(&abs.to_string_lossy()));
    if !prefix.exists() && args.project.is_none() {
        eprintln!(
            "No Claude Code sessions in {label}. Use 'cct search -g <query>' to search across all projects."
        );
        return Err(anyhow!("no sessions in cwd"));
    }
    Ok(Some(ResolvedProject { prefix, label }))
}

#[derive(Default, Clone, Copy)]
struct DisplayStats {
    prompts: u32,
    tools: u32,
    bytes: u64,
}

/// Read each session jsonl file (small in practice) to derive prompts/tools/size for display.
fn compute_session_stats(session: &search::SessionResult) -> DisplayStats {
    let mut out = DisplayStats::default();
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
            let (_, stats): (String, SessionStats) = extract_text(&data);
            out.prompts = out.prompts.max(stats.prompts);
            out.tools = out.tools.max(stats.tools);
        }
    }
    out
}

fn compute_session_stats_parallel(sessions: &[&search::SessionResult]) -> Vec<DisplayStats> {
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
    let mut out: Vec<DisplayStats> = vec![DisplayStats::default(); n];
    let counter = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        let counter = &counter;
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(move || -> Vec<(usize, DisplayStats)> {
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
