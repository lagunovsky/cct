//! Explicit segment compaction.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use tantivy::index::{SegmentId, SegmentMeta};
use tantivy::Index;

use crate::term::fmt_size;

use super::build::{dir_size, subindex_segment_count};
use super::catalog::read_manifest;
use super::schema::{delta_dir, subindex_dir, IndexKind};
use super::writer::{create_writer, print_writer_config, MergePolicyMode, WriterConfig};

pub const DELTA_OPTIMIZE_MIN_SEGMENTS: usize = 8;

pub fn run_compact(
    index_dir: &Path,
    enabled_override: Option<BTreeSet<IndexKind>>,
    target_segments: usize,
    mut config: WriterConfig,
) -> Result<()> {
    if !index_dir.is_dir() {
        bail!("index dir does not exist: {}", index_dir.display());
    }
    config.merge_policy = MergePolicyMode::None;

    let manifest = read_manifest(index_dir)?;
    let enabled = enabled_override
        .or(manifest.indexes)
        .unwrap_or_else(IndexKind::all);
    if enabled.is_empty() {
        bail!("no indexes enabled");
    }

    let enabled_names = enabled
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",");
    eprintln!(
        "Compacting {} [{}] to <= {} segments per physical index",
        index_dir.display(),
        enabled_names,
        target_segments
    );
    print_writer_config(config, 1);

    let start = Instant::now();
    for kind in &enabled {
        compact_one_index(index_dir, *kind, config, target_segments.max(1))?;
    }
    let total_index_bytes = dir_size(index_dir)?;
    eprintln!(
        "Done in {:?}; total index bytes {}",
        start.elapsed(),
        fmt_size(total_index_bytes)
    );
    Ok(())
}

pub fn optimize_delta(index_dir: &Path, mut config: WriterConfig) -> Result<Option<Duration>> {
    let root = delta_dir(index_dir);
    if !root.is_dir() {
        return Ok(None);
    }
    config.merge_policy = MergePolicyMode::None;

    let mut needs_compaction = Vec::new();
    for kind in [IndexKind::Word, IndexKind::Trigram, IndexKind::Path] {
        if !subindex_dir(&root, kind).is_dir() {
            continue;
        }
        match quick_segment_count(&root, kind)? {
            Some(count) if count < DELTA_OPTIMIZE_MIN_SEGMENTS => {}
            _ => needs_compaction.push(kind),
        }
    }
    if needs_compaction.is_empty() {
        return Ok(None);
    }

    let start = Instant::now();
    let mut merged = false;
    for kind in needs_compaction {
        merged |= compact_one_index_quiet(&root, kind, config, 1)?;
    }
    Ok(merged.then(|| start.elapsed()))
}

fn compact_one_index(
    root: &Path,
    kind: IndexKind,
    config: WriterConfig,
    target_segments: usize,
) -> Result<()> {
    let dir = subindex_dir(root, kind);
    if !dir.is_dir() {
        bail!("subindex does not exist: {}", dir.display());
    }
    let before_size = dir_size(&dir)?;
    let index =
        Index::open_in_dir(&dir).with_context(|| format!("open index {}", dir.display()))?;
    let metas = index.searchable_segment_metas()?;
    let before_segments = metas.len();
    if before_segments <= target_segments {
        eprintln!(
            "  {:7} skip: segments {}, size {}",
            kind.as_str(),
            before_segments,
            fmt_size(before_size)
        );
        return Ok(());
    }

    let groups = segment_merge_groups(metas, target_segments);
    eprintln!(
        "  {:7} merging {} segments into {} groups, size {}",
        kind.as_str(),
        before_segments,
        groups.len(),
        fmt_size(before_size)
    );

    let mut writer = create_writer(&index, &config, 1)?;
    let merge_start = Instant::now();
    let mut futures = Vec::new();
    for group in &groups {
        if group.len() <= 1 {
            continue;
        }
        futures.push(writer.merge(group));
    }
    for future in futures {
        future.wait()?;
    }
    writer.wait_merging_threads()?;

    let after_size = dir_size(&dir)?;
    let after_segments = subindex_segment_count(root, kind)?;
    eprintln!(
        "  {:7} done in {:?}: segments {} -> {}, size {} -> {}",
        kind.as_str(),
        merge_start.elapsed(),
        before_segments,
        after_segments,
        fmt_size(before_size),
        fmt_size(after_size)
    );
    Ok(())
}

fn compact_one_index_quiet(
    root: &Path,
    kind: IndexKind,
    config: WriterConfig,
    target_segments: usize,
) -> Result<bool> {
    let dir = subindex_dir(root, kind);
    let index =
        Index::open_in_dir(&dir).with_context(|| format!("open index {}", dir.display()))?;
    let metas = index.searchable_segment_metas()?;
    if metas.len() <= target_segments {
        return Ok(false);
    }

    let groups = segment_merge_groups(metas, target_segments);
    let mut writer = create_writer(&index, &config, 1)?;
    let mut futures = Vec::new();
    for group in &groups {
        if group.len() <= 1 {
            continue;
        }
        futures.push(writer.merge(group));
    }
    for future in futures {
        future.wait()?;
    }
    writer.wait_merging_threads()?;
    Ok(true)
}

fn quick_segment_count(root: &Path, kind: IndexKind) -> Result<Option<usize>> {
    let meta_path = subindex_dir(root, kind).join("meta.json");
    let raw = match fs::read_to_string(&meta_path) {
        Ok(raw) => raw,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", meta_path.display())),
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Ok(None);
    };
    Ok(value
        .get("segments")
        .and_then(|segments| segments.as_array())
        .map(|segments| segments.len()))
}

fn segment_merge_groups(
    mut metas: Vec<SegmentMeta>,
    target_segments: usize,
) -> Vec<Vec<SegmentId>> {
    metas.sort_by_key(|m| std::cmp::Reverse(m.num_docs()));
    let group_count = target_segments.max(1).min(metas.len());
    let mut groups: Vec<(u64, Vec<SegmentId>)> =
        (0..group_count).map(|_| (0u64, Vec::new())).collect();
    for meta in metas {
        let target = groups
            .iter()
            .enumerate()
            .min_by_key(|(_, (docs, _))| *docs)
            .map(|(i, _)| i)
            .unwrap_or(0);
        groups[target].0 += meta.num_docs() as u64;
        groups[target].1.push(meta.id());
    }
    groups.into_iter().map(|(_, ids)| ids).collect()
}
