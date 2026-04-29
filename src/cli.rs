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

    /// Migrate a project's sessions when its directory is renamed/moved.
    #[command(visible_alias = "rn")]
    Rename(RenameCmd),

    /// Move specific sessions between projects.
    #[command(visible_alias = "mv")]
    Move(MoveCmd),

    /// Manage launchd background indexing (macOS).
    #[command(visible_alias = "d")]
    Daemon(DaemonCmd),
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
    /// Also rename the project directory itself.
    #[arg(long = "move-dir", default_value_t = false)]
    pub move_dir: bool,
    /// Show what would change but don't write.
    #[arg(long = "dry-run", default_value_t = false)]
    pub dry_run: bool,

    /// `<new-path>` (uses cwd as old) or `<old-path> <new-path>`.
    #[arg(num_args = 1..=2, required = true)]
    pub paths: Vec<String>,
}

#[derive(Args, Debug)]
pub struct MoveCmd {
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
        match IndexKind::from_str(part.trim()) {
            Some(k) => {
                set.insert(k);
            }
            None if part.trim().is_empty() => {}
            None => return None,
        }
    }
    Some(set)
}
