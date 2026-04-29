//! Top-level command implementations.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Result};

use crate::cli;
use crate::fs_scan::collect_cct_layout;
use crate::index::build::run_full_reindex;
use crate::index::compact::run_compact;
use crate::index::search::{self, SearchOptions};
use crate::index::update::{build_plan, needs_update, run_update};
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
        Some(cli::Cmd::Move(args)) => run_move(args),
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
        move_dir: args.move_dir,
        dry_run: args.dry_run,
    })
}

pub fn run_move(args: cli::MoveCmd) -> Result<()> {
    crate::move_cmd::run_move(crate::move_cmd::MoveArgs {
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
        return Ok(());
    }
    run_update(
        index_dir,
        plan,
        WriterConfig::for_incremental_update(),
        false,
    )?;
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
    if !args.no_update {
        if !index_dir.exists() {
            eprintln!("{DIM}Building initial index — this may take a while...{RESET}");
            let config = WriterConfig::for_full_reindex();
            let files = collect_cct_layout(&source)?;
            run_full_reindex(
                &source,
                &index_dir,
                &IndexKind::all(),
                files,
                config,
                verbose_index,
            )?;
        } else {
            let sp = Spinner::start("scanning sessions...");
            let files = collect_cct_layout(&source)?;
            let plan = build_plan(&index_dir, Some(source.clone()), files)?;
            if needs_update(&plan) {
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
    };
    let mut sessions = search::run_search(&index_dir, &opts)?;
    sp.stop();

    let total = sessions.len();
    let to_show = sessions.len().min(args.limit);
    if to_show == 0 {
        println!("\n{DIM}No matches found{RESET}");
    } else {
        for s in sessions.iter_mut().take(to_show) {
            let stats = compute_session_stats(s);
            search::print_session_header(s, stats.prompts, stats.tools, stats.bytes);
            search::print_snippets(&s.snippets, &query, args.verbose >= 1);
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

    let elapsed = start.elapsed();
    println!(
        "\n{DIM}{count} sessions in {elapsed:?}{RESET}",
        count = to_show,
    );
    Ok(())
}

fn print_search_header(query: &str, args: &cli::SearchCmd, label: Option<&str>) {
    if args.global || label.is_none() {
        println!("{BOLD}Searching for:{RESET} {query}  {DIM}(global){RESET}");
    } else if let Some(label) = label {
        println!("{BOLD}Searching for:{RESET} {query}  {DIM}in {label}{RESET}");
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
