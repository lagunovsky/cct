//! Writer configuration and helpers shared between build/update/compact.

#[cfg(target_os = "macos")]
use std::process::Command;

use anyhow::{bail, Context, Result};
use tantivy::indexer::{IndexWriterOptions, NoMergePolicy};
use tantivy::{Index, IndexWriter};

use crate::term::fmt_size;

const FALLBACK_FULL_REINDEX_MEMORY_MB: usize = 5048;
const MAX_FULL_REINDEX_MEMORY_MB: usize = 8192;
pub const DEFAULT_MAINTENANCE_MEMORY_MB: usize = 768;
pub const DEFAULT_DELTA_MEMORY_MB: usize = 128;
const MIN_WRITER_MEMORY_BYTES: usize = 15_000_000;
const MAX_AUTO_WORKER_THREADS: usize = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MergePolicyMode {
    None,
    Log,
}

impl MergePolicyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            MergePolicyMode::None => "none",
            MergePolicyMode::Log => "log",
        }
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "none" => Ok(Self::None),
            "log" => Ok(Self::Log),
            other => bail!("unknown merge policy {other:?}; expected none|log"),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct WriterConfig {
    pub memory_mb: usize,
    pub worker_threads: Option<usize>,
    pub merge_threads: usize,
    pub merge_policy: MergePolicyMode,
}

impl Default for WriterConfig {
    fn default() -> Self {
        Self {
            memory_mb: DEFAULT_MAINTENANCE_MEMORY_MB,
            worker_threads: None,
            merge_threads: 4,
            merge_policy: MergePolicyMode::None,
        }
    }
}

impl WriterConfig {
    pub fn for_full_reindex() -> Self {
        Self {
            memory_mb: default_full_reindex_memory_mb(),
            merge_policy: MergePolicyMode::Log,
            ..Self::default()
        }
    }

    pub fn for_incremental_update() -> Self {
        Self::default()
    }

    pub fn for_delta_update() -> Self {
        Self {
            memory_mb: DEFAULT_DELTA_MEMORY_MB,
            worker_threads: Some(1),
            merge_threads: 1,
            merge_policy: MergePolicyMode::None,
        }
    }
}

pub fn default_full_reindex_memory_mb() -> usize {
    match total_physical_memory_mb() {
        Some(total) => (total / 6).clamp(DEFAULT_MAINTENANCE_MEMORY_MB, MAX_FULL_REINDEX_MEMORY_MB),
        None => FALLBACK_FULL_REINDEX_MEMORY_MB,
    }
}

#[cfg(target_os = "macos")]
fn total_physical_memory_mb() -> Option<usize> {
    let output = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let bytes = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some((bytes / 1024 / 1024) as usize)
}

#[cfg(target_os = "linux")]
fn total_physical_memory_mb() -> Option<usize> {
    let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:") {
            let kb = rest.split_whitespace().next()?.parse::<u64>().ok()?;
            return Some((kb / 1024) as usize);
        }
    }
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn total_physical_memory_mb() -> Option<usize> {
    None
}

pub fn per_index_memory_bytes(memory_mb: usize, enabled_count: usize) -> usize {
    let count = enabled_count.max(1);
    (memory_mb / count).max(64) * 1024 * 1024
}

pub fn auto_worker_threads(memory_bytes: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(1);
    let mut threads = available.min(MAX_AUTO_WORKER_THREADS);
    if threads == 0 {
        threads = 1;
    }
    if memory_bytes / threads < MIN_WRITER_MEMORY_BYTES {
        threads = (memory_bytes / MIN_WRITER_MEMORY_BYTES).max(1);
    }
    threads
}

pub fn create_writer(
    index: &Index,
    config: &WriterConfig,
    enabled_count: usize,
) -> Result<IndexWriter> {
    let per_index_memory = per_index_memory_bytes(config.memory_mb, enabled_count);
    let worker_threads = config
        .worker_threads
        .unwrap_or_else(|| auto_worker_threads(per_index_memory));
    let memory_per_thread = per_index_memory / worker_threads.max(1);
    if memory_per_thread < MIN_WRITER_MEMORY_BYTES {
        bail!(
            "writer memory is too low: {} per thread with {} workers; raise --memory-mb or lower --worker-threads",
            fmt_size(memory_per_thread as u64),
            worker_threads
        );
    }

    let options = IndexWriterOptions::builder()
        .num_worker_threads(worker_threads)
        .num_merge_threads(config.merge_threads)
        .memory_budget_per_thread(memory_per_thread)
        .build();
    let writer: IndexWriter = index
        .writer_with_options(options)
        .with_context(|| "open index writer")?;
    if config.merge_policy == MergePolicyMode::None {
        writer.set_merge_policy(Box::new(NoMergePolicy));
    }
    Ok(writer)
}

pub fn print_writer_config(config: WriterConfig, enabled_count: usize) {
    let per_index_memory = per_index_memory_bytes(config.memory_mb, enabled_count);
    let worker_threads = config
        .worker_threads
        .unwrap_or_else(|| auto_worker_threads(per_index_memory));
    eprintln!(
        "  writer memory {} total, {} per index; workers {}, merge threads {}, merge policy {}",
        fmt_size((config.memory_mb as u64) * 1024 * 1024),
        fmt_size(per_index_memory as u64),
        worker_threads,
        config.merge_threads,
        config.merge_policy.as_str()
    );
}
