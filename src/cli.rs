//! Command-line argument parsing.

use std::collections::BTreeSet;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::index::schema::IndexKind;

#[derive(Parser, Debug)]
#[command(
    name = "cct",
    about = "Claude Code Tools — search & maintain Claude session logs",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

/// Every subcommand name and visible alias the parser recognizes, plus `help`.
/// Kept in sync with `Cmd`; `normalize_args` uses it to decide whether a bare
/// first argument is an implicit search query rather than a (mistyped) command.
pub const KNOWN_SUBCOMMANDS: &[&str] = &[
    "recent",
    "r",
    "search",
    "s",
    "blame",
    "b",
    "reindex",
    "ri",
    "update",
    "u",
    "compact",
    "c",
    "rename",
    "rn",
    "transfer",
    "tr",
    "daemon",
    "d",
    "cleanup-period",
    "cp",
    "archive",
    "unarchive",
    "help",
];

/// Treat a first argument that is not a known subcommand as an implicit
/// `search` invocation: `cct ветка` behaves like `cct search ветка`, and —
/// since top-level `cct` has no flags of its own besides help/version —
/// `cct --json ветка` or `cct -g ветка` become flag-first searches too.
/// `args` is the full argv (program name included); returns it unchanged for
/// known subcommands, help/version, and the no-argument case.
pub fn normalize_args<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args: Vec<String> = args.into_iter().map(Into::into).collect();
    let Some(first) = args.get(1) else {
        return args;
    };
    if KNOWN_SUBCOMMANDS.contains(&first.as_str())
        || matches!(first.as_str(), "-h" | "--help" | "-V" | "--version")
    {
        return args;
    }
    args.insert(1, "search".to_string());
    args
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Recent sessions in current project (default).
    #[command(visible_alias = "r")]
    Recent(RecentCmd),

    /// Full-text search across Claude Code sessions.
    #[command(visible_alias = "s")]
    Search(SearchCmd),

    /// Which sessions discussed a file, and what they said about it.
    #[command(visible_alias = "b")]
    Blame(BlameCmd),

    /// Rebuild the search index from scratch.
    #[command(visible_alias = "ri")]
    Reindex(ReindexCmd),

    /// Apply incremental updates (add new files, append-tail jsonl, drop removed).
    #[command(visible_alias = "u")]
    Update(UpdateCmd),

    /// Compact existing segments to reduce size and segment count.
    #[command(visible_alias = "c")]
    Compact(CompactCmd),

    /// Rename a project: physically move the directory and migrate all its sessions.
    #[command(visible_alias = "rn")]
    Rename(RenameCmd),

    /// Transfer specific sessions between projects.
    #[command(
        visible_alias = "tr",
        override_usage = "cct transfer [OPTIONS] <SESSION>... <TARGET>\n       cct transfer [OPTIONS] <TARGET>"
    )]
    Transfer(TransferCmd),

    /// Manage launchd background indexing (macOS).
    #[command(visible_alias = "d")]
    Daemon(DaemonCmd),

    /// Show or set Claude Code's session retention (cleanupPeriodDays).
    #[command(visible_alias = "cp", name = "cleanup-period")]
    CleanupPeriod(CleanupPeriodCmd),

    /// Move cold sessions into a zstd-compressed archive (search keeps working).
    Archive(ArchiveCmd),

    /// Restore archived sessions back into ~/.claude/projects.
    Unarchive(UnarchiveCmd),
}

#[derive(Args, Debug)]
pub struct RecentCmd {
    /// Max sessions to show.
    #[arg(short = 'n', long = "limit", default_value_t = 20)]
    pub limit: usize,
}

#[derive(Args, Debug)]
pub struct SearchCmd {
    /// Max results to show.
    #[arg(short = 'n', long = "limit", default_value_t = 20)]
    pub limit: usize,

    /// Context characters around each match.
    #[arg(short = 'c', long = "context", default_value_t = 120)]
    pub context: usize,

    /// Verbose: -v shows file paths + agent/hook matches; -vv also shows index update phases.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Search across all projects (default: current directory only).
    #[arg(short, long, default_value_t = false)]
    pub global: bool,

    /// Restrict to a specific project path.
    #[arg(short = 'p', long = "project")]
    pub project: Option<String>,

    /// Skip the auto incremental update before searching.
    #[arg(long = "no-update", default_value_t = false)]
    pub no_update: bool,

    /// Fuzzy: treat tokens independently. Show snippets per token even when the
    /// full phrase isn't present on a single line.
    #[arg(short = 'f', long = "fuzzy", default_value_t = false)]
    pub fuzzy: bool,

    /// Machine-readable output: one JSON object per session on stdout, no
    /// colors or progress text. Same content as the human output: -v adds
    /// agent/hook snippets and matched file paths.
    #[arg(long = "json", default_value_t = false)]
    pub json: bool,

    /// Search backend selection.
    #[arg(long = "mode", value_enum, default_value_t = SearchMode::Auto)]
    pub mode: SearchMode,

    /// Result order: best match first, or newest first.
    #[arg(long = "sort", value_enum, default_value_t = SortBy::Relevance)]
    pub sort: SortBy,

    /// Include headless sessions (`claude -p`, Agent SDK), left out by default.
    #[arg(long = "headless", default_value_t = false)]
    pub headless: bool,

    /// Query strings; a session must match all of them (AND), anywhere in the
    /// session. Tokens inside one string are AND-ed in nearby context.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,
}

#[derive(Args, Debug)]
pub struct BlameCmd {
    /// Max sessions to show.
    #[arg(short = 'n', long = "limit", default_value_t = 10)]
    pub limit: usize,

    /// Context characters around each mention.
    #[arg(short = 'c', long = "context", default_value_t = 120)]
    pub context: usize,

    /// Verbose: -v shows file paths + agent/hook mentions; -vv also shows index update phases.
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Look across all projects (default: current directory only).
    #[arg(short, long, default_value_t = false)]
    pub global: bool,

    /// Include headless sessions (`claude -p`, Agent SDK), left out by default.
    #[arg(long = "headless", default_value_t = false)]
    pub headless: bool,

    /// Restrict to a specific project path.
    #[arg(short = 'p', long = "project")]
    pub project: Option<String>,

    /// Skip the auto incremental update before searching.
    #[arg(long = "no-update", default_value_t = false)]
    pub no_update: bool,

    /// Machine-readable output: one JSON object per session on stdout.
    #[arg(long = "json", default_value_t = false)]
    pub json: bool,

    /// File to blame. Existing paths are resolved against the filesystem;
    /// anything else is taken as a path fragment or bare filename.
    #[arg(required = true)]
    pub path: String,
}

#[derive(Args, Debug)]
pub struct ReindexCmd {
    /// Override which physical indexes are built (comma list of word,trigram,path).
    #[arg(long = "only")]
    pub only: Option<String>,

    /// Override the auto memory budget (megabytes).
    #[arg(long = "memory-mb")]
    pub memory_mb: Option<usize>,

    /// Override the number of indexer worker threads.
    #[arg(long = "worker-threads")]
    pub worker_threads: Option<usize>,

    /// Override the number of merge threads (default 4).
    #[arg(long = "merge-threads")]
    pub merge_threads: Option<usize>,

    /// Override the merge policy (none|log).
    #[arg(long = "merge-policy")]
    pub merge_policy: Option<String>,
}

#[derive(Args, Debug)]
pub struct UpdateCmd {
    #[arg(long = "memory-mb")]
    pub memory_mb: Option<usize>,
    #[arg(long = "worker-threads")]
    pub worker_threads: Option<usize>,
    #[arg(long = "merge-threads")]
    pub merge_threads: Option<usize>,
    #[arg(long = "merge-policy")]
    pub merge_policy: Option<String>,
}

#[derive(Args, Debug)]
pub struct CompactCmd {
    /// Target segment count per physical index.
    #[arg(long = "target-segments", default_value_t = 1)]
    pub target_segments: usize,
    #[arg(long = "only")]
    pub only: Option<String>,
    #[arg(long = "memory-mb")]
    pub memory_mb: Option<usize>,
    #[arg(long = "worker-threads")]
    pub worker_threads: Option<usize>,
    #[arg(long = "merge-threads")]
    pub merge_threads: Option<usize>,
}

#[derive(Args, Debug)]
pub struct RenameCmd {
    /// Skip physically renaming the project directory (use when the dir was
    /// already renamed/moved on disk and only the session jsonl-s need cwd
    /// rewiring).
    #[arg(long = "no-move-dir", default_value_t = false)]
    pub no_move_dir: bool,
    /// Show what would change but don't write.
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,

    /// `<new-path>` (uses cwd as old) or `<old-path> <new-path>`.
    #[arg(num_args = 1..=2, required = true)]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
pub struct TransferCmd {
    /// Source project path. Not needed for ids: a session is found wherever it
    /// lives. Restricts lookup to this project, which settles a title shared
    /// by sessions in several projects; with only `<target>`, the project the
    /// picker lists (default: cwd).
    #[arg(long = "from")]
    pub from: Option<String>,
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,
    /// Skip the y/N prompt.
    #[arg(short = 'y', long = "yes", default_value_t = false)]
    pub yes: bool,

    /// `<session>... <target>`: sessions (ids, unique id prefixes or `/rename`
    /// titles) and the target project path, last. `<target>` alone: pick the
    /// sessions interactively.
    #[arg(num_args = 1.., required = true, value_name = "ARGS")]
    pub args: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ArchiveCmd {
    /// Archive sessions whose last activity is older than this (90d, 3m, 1y,
    /// or days). Default 90d, unless only --headless-older-than is given.
    #[arg(long = "older-than")]
    pub older_than: Option<String>,

    /// Archive headless sessions (`claude -p`) older than this instead.
    #[arg(long = "headless-older-than", value_name = "AGE")]
    pub headless_older_than: Option<String>,

    /// Restrict to one project path (default: all projects).
    #[arg(short = 'p', long = "project")]
    pub project: Option<String>,

    /// Show what would be archived but don't write.
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,

    /// Skip the y/N prompt.
    #[arg(short = 'y', long = "yes", default_value_t = false)]
    pub yes: bool,

    /// Configure background archiving by the daemon: an age (90d, 6m, 1y) to
    /// enable, or `off` to disable. Stored in ~/.config/cct/config; nothing
    /// is archived right now.
    #[arg(long = "auto", value_name = "AGE|off")]
    pub auto: Option<String>,

    /// Configure the daemon's age for headless sessions (`claude -p`): an age
    /// (10d) to archive them sooner than the rest, or `off` to follow --auto.
    #[arg(long = "auto-headless", value_name = "AGE|off")]
    pub auto_headless: Option<String>,
}

#[derive(Args, Debug)]
pub struct UnarchiveCmd {
    /// Keep the original mtime instead of stamping the restore time. Note:
    /// Claude Code's age-based cleanup may then reap the session right away.
    #[arg(long = "keep-mtime", default_value_t = false)]
    pub keep_mtime: bool,

    /// Session ids (or unique prefixes) to restore.
    #[arg(required = true, num_args = 1..)]
    pub ids: Vec<String>,
}

#[derive(Args, Debug)]
pub struct CleanupPeriodCmd {
    /// Days to retain transcripts; writes cleanupPeriodDays to
    /// ~/.claude/settings.json. Omit to show the current value.
    pub days: Option<u64>,

    /// Permanently silence the low-retention warning (stored in ~/.config/cct/config).
    #[arg(long = "no-warn", default_value_t = false)]
    pub no_warn: bool,

    /// Re-enable the low-retention warning.
    #[arg(long = "warn", default_value_t = false, conflicts_with = "no_warn")]
    pub warn: bool,
}

#[derive(Args, Debug)]
pub struct DaemonCmd {
    #[command(subcommand)]
    pub action: DaemonAction,
}

#[derive(Subcommand, Debug)]
pub enum DaemonAction {
    Install,
    Uninstall,
    /// Periodic action used by launchd; safe to run interactively.
    Update,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum SearchMode {
    Auto,
    All,
    Word,
    Trigram,
    Path,
}

#[derive(Copy, Clone, Debug, Default, ValueEnum)]
pub enum SortBy {
    /// Best match first: match quality and count, damped by session age.
    #[default]
    Relevance,
    /// Newest session first.
    Time,
}

pub fn parse_index_list(raw: &str) -> Option<BTreeSet<IndexKind>> {
    let mut set = BTreeSet::new();
    for part in raw.split(',') {
        match part.trim().parse::<IndexKind>() {
            Ok(k) => {
                set.insert(k);
            }
            Err(()) if part.trim().is_empty() => {}
            Err(()) => return None,
        }
    }
    Some(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn norm(args: &[&str]) -> Vec<String> {
        normalize_args(args.iter().copied())
    }

    #[test]
    fn bare_query_becomes_implicit_search() {
        assert_eq!(norm(&["cct", "ветка"]), vec!["cct", "search", "ветка"]);
        assert_eq!(
            norm(&["cct", "ветка", "-g"]),
            vec!["cct", "search", "ветка", "-g"]
        );
    }

    #[test]
    fn known_commands_aliases_and_flags_pass_through() {
        for cmd in ["search", "s", "reindex", "ri", "transfer", "tr", "daemon", "help"] {
            assert_eq!(norm(&["cct", cmd]), vec!["cct", cmd]);
        }
        // Help/version and the no-argument case are never rewritten.
        assert_eq!(norm(&["cct", "-h"]), vec!["cct", "-h"]);
        assert_eq!(norm(&["cct", "--help"]), vec!["cct", "--help"]);
        assert_eq!(norm(&["cct", "-V"]), vec!["cct", "-V"]);
        assert_eq!(norm(&["cct", "--version"]), vec!["cct", "--version"]);
        assert_eq!(norm(&["cct"]), vec!["cct"]);
    }

    #[test]
    fn flag_first_invocation_becomes_implicit_search() {
        assert_eq!(
            norm(&["cct", "--json", "ветка"]),
            vec!["cct", "search", "--json", "ветка"]
        );
        assert_eq!(
            norm(&["cct", "-g", "hook", "PreToolUse"]),
            vec!["cct", "search", "-g", "hook", "PreToolUse"]
        );
    }

    #[test]
    fn known_subcommands_list_stays_in_sync_with_cmd() {
        for sub in Cli::command().get_subcommands() {
            assert!(
                KNOWN_SUBCOMMANDS.contains(&sub.get_name()),
                "KNOWN_SUBCOMMANDS missing command name {:?}",
                sub.get_name()
            );
            for alias in sub.get_visible_aliases() {
                assert!(
                    KNOWN_SUBCOMMANDS.contains(&alias),
                    "KNOWN_SUBCOMMANDS missing alias {alias:?}"
                );
            }
        }
    }
}
