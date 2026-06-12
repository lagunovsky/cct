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

/// Treat a bare first argument that is neither a flag nor a known subcommand as
/// an implicit `search` query, so `cct ветка` behaves like `cct search ветка`.
/// `args` is the full argv (program name included); returns it unchanged in
/// every other case.
pub fn normalize_args<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut args: Vec<String> = args.into_iter().map(Into::into).collect();
    let Some(first) = args.get(1) else {
        return args;
    };
    if first.starts_with('-') || KNOWN_SUBCOMMANDS.contains(&first.as_str()) {
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
    #[command(visible_alias = "tr")]
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

    /// Search backend selection.
    #[arg(long = "mode", value_enum, default_value_t = SearchMode::Auto)]
    pub mode: SearchMode,

    /// Query string. Tokens are AND-ed together.
    #[arg(required = true, num_args = 1..)]
    pub query: Vec<String>,
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
    /// Source project path. Defaults to cwd.
    #[arg(long = "from")]
    pub from: Option<String>,
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,
    /// Skip the y/N prompt.
    #[arg(short = 'y', long = "yes", default_value_t = false)]
    pub yes: bool,

    /// Target project path.
    #[arg(required = true)]
    pub target: String,

    /// Optional session ids (UUIDs or unique prefixes). Without ids: interactive picker.
    #[arg(num_args = 0..)]
    pub session_ids: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ArchiveCmd {
    /// Archive sessions whose last activity is older than this (90d, 3m, 1y, or days).
    #[arg(long = "older-than", default_value = "90d")]
    pub older_than: String,

    /// Restrict to one project path (default: all projects).
    #[arg(short = 'p', long = "project")]
    pub project: Option<String>,

    /// Show what would be archived but don't write.
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,

    /// Skip the y/N prompt.
    #[arg(short = 'y', long = "yes", default_value_t = false)]
    pub yes: bool,
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
        // Flags and the no-argument case are never rewritten.
        assert_eq!(norm(&["cct", "-h"]), vec!["cct", "-h"]);
        assert_eq!(norm(&["cct"]), vec!["cct"]);
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
