//! Incremental update against an existing index.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Result};

use crate::fs_scan::{is_appendable_jsonl, DiscoveredFile};
use crate::term::fmt_size;

use super::build::{
    add_whole_file, append_file_tail, delete_file_docs, dir_size, open_existing_targets,
    size_ratio, subindex_segment_count,
};
use super::catalog::{read_catalog, read_manifest, write_catalog, write_manifest, CatalogEntry};
use super::schema::{subindex_dir, IndexKind};
use super::writer::{print_writer_config, WriterConfig};

#[derive(Default, Clone, Copy, Debug)]
pub struct UpdateStats {
    pub added: usize,
    pub appended: usize,
    pub reindexed: usize,
    pub deleted: usize,
    pub unchanged: usize,
    pub indexed_chunks: u64,
    pub read_bytes: u64,
    pub indexed_bytes: u64,
}

pub struct UpdatePlan {
    pub source: PathBuf,
    pub enabled: BTreeSet<IndexKind>,
    pub catalog_by_path: BTreeMap<PathBuf, CatalogEntry>,
    pub scanned: BTreeMap<PathBuf, DiscoveredFile>,
}

/// Quick boolean check: would a full update make any changes? Used to skip the
/// writer-open / commit cycle on hot search paths.
pub fn needs_update(plan: &UpdatePlan) -> bool {
    if plan.scanned.len() != plan.catalog_by_path.len() {
        return true;
    }
    for (path, entry) in &plan.catalog_by_path {
        match plan.scanned.get(path) {
            Some(file) => {
                if file.size != entry.size || file.mtime_ms != entry.mtime_ms {
                    return true;
                }
            }
            None => return true,
        }
    }
    false
}

pub fn build_plan(
    index_dir: &Path,
    source_override: Option<PathBuf>,
    files: Vec<DiscoveredFile>,
) -> Result<UpdatePlan> {
    let manifest = read_manifest(index_dir)?;
    let source = source_override
        .or(manifest.source)
        .ok_or_else(|| anyhow::anyhow!("source not set in manifest"))?;
    let enabled = manifest.indexes.unwrap_or_else(IndexKind::all);
    let catalog = read_catalog(index_dir)?;

    let scanned: BTreeMap<PathBuf, DiscoveredFile> =
        files.into_iter().map(|f| (f.path.clone(), f)).collect();
    let catalog_by_path: BTreeMap<PathBuf, CatalogEntry> =
        catalog.into_iter().map(|e| (e.path.clone(), e)).collect();
    Ok(UpdatePlan {
        source,
        enabled,
        catalog_by_path,
        scanned,
    })
}

pub fn run_update(
    index_dir: &Path,
    plan: UpdatePlan,
    config: WriterConfig,
    verbose: bool,
) -> Result<UpdateStats> {
    if plan.enabled.is_empty() {
        bail!("no indexes enabled");
    }
    let UpdatePlan {
        source,
        enabled,
        mut catalog_by_path,
        scanned,
    } = plan;
    let start = Instant::now();

    let enabled_names = enabled
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let active_indexes = affected_indexes(&enabled, &catalog_by_path, &scanned);
    let active_names = if active_indexes.is_empty() {
        "none".to_string()
    } else {
        active_indexes
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(",")
    };
    if verbose {
        eprintln!(
            "Updating {} from {} [{}]",
            index_dir.display(),
            source.display(),
            enabled_names,
        );
        eprintln!("  active writers [{active_names}]");
        if !active_indexes.is_empty() {
            print_writer_config(config, active_indexes.len());
        }
    }

    let mut targets = open_existing_targets(index_dir, &active_indexes, config)?;
    let mut stats = UpdateStats::default();
    let mut next_file_id = catalog_by_path
        .values()
        .map(|e| e.file_id)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    let mut new_catalog: Vec<CatalogEntry> = Vec::new();
    let scanned_paths: BTreeSet<PathBuf> = scanned.keys().cloned().collect();

    for (path, mut entry) in std::mem::take(&mut catalog_by_path) {
        let Some(file) = scanned.get(&path) else {
            delete_file_docs(&mut targets, entry.file_id);
            stats.deleted += 1;
            continue;
        };

        if file.size == entry.size && file.mtime_ms == entry.mtime_ms {
            stats.unchanged += 1;
            new_catalog.push(entry);
            continue;
        }

        if is_append_update(&entry, file) {
            let appended = append_file_tail(&mut targets, &mut entry, file)?;
            stats.appended += 1;
            stats.indexed_chunks += appended.chunks;
            stats.read_bytes += appended.read_bytes;
            stats.indexed_bytes += appended.indexed_bytes;
            new_catalog.push(entry);
            continue;
        }

        delete_file_docs(&mut targets, entry.file_id);
        let (new_entry, chunks) = add_whole_file(&mut targets, file, entry.file_id)?;
        stats.reindexed += 1;
        stats.indexed_chunks += chunks;
        stats.read_bytes += new_entry.size;
        stats.indexed_bytes += new_entry.indexed_offset;
        new_catalog.push(new_entry);
    }

    let in_catalog: BTreeSet<PathBuf> = new_catalog.iter().map(|e| e.path.clone()).collect();
    for path in scanned_paths {
        if in_catalog.contains(&path) {
            continue;
        }
        let Some(file) = scanned.get(&path) else {
            continue;
        };
        let (entry, chunks) = add_whole_file(&mut targets, file, next_file_id)?;
        next_file_id = next_file_id.saturating_add(1);
        stats.added += 1;
        stats.indexed_chunks += chunks;
        stats.read_bytes += entry.size;
        stats.indexed_bytes += entry.indexed_offset;
        new_catalog.push(entry);
    }

    if verbose {
        eprintln!(
            "  added {}, appended {}, reindexed {}, deleted {}, unchanged {}",
            stats.added, stats.appended, stats.reindexed, stats.deleted, stats.unchanged
        );
        eprintln!(
            "  read {}; indexed append/content {}; chunks {}",
            fmt_size(stats.read_bytes),
            fmt_size(stats.indexed_bytes),
            stats.indexed_chunks
        );
    }

    let index_dirty =
        stats.added > 0 || stats.reindexed > 0 || stats.deleted > 0 || stats.indexed_chunks > 0;
    if index_dirty {
        for target in &mut targets {
            let phase = Instant::now();
            if verbose {
                eprintln!("Committing {}...", target.kind.as_str());
            }
            target.writer.commit()?;
            if verbose {
                eprintln!(
                    "Committed {} in {:?}",
                    target.kind.as_str(),
                    phase.elapsed()
                );
            }
        }
        for target in targets {
            let phase = Instant::now();
            if verbose {
                eprintln!("Waiting merges for {}...", target.kind.as_str());
            }
            target.writer.wait_merging_threads()?;
            if verbose {
                eprintln!(
                    "Merges done for {} in {:?}",
                    target.kind.as_str(),
                    phase.elapsed()
                );
            }
        }
    } else if verbose {
        eprintln!("No index documents changed; skipping commit.");
    }

    let _catalog_path = write_catalog(index_dir, &new_catalog)?;
    let total_bytes = new_catalog.iter().map(|e| e.size).sum::<u64>();
    write_manifest(
        index_dir,
        &source,
        new_catalog.len(),
        total_bytes,
        &enabled,
        config,
    )?;

    if verbose {
        let total_index_bytes = dir_size(index_dir)?;
        eprintln!(
            "Done in {:?}; source bytes {}; total index bytes {} ({:.2}x source)",
            start.elapsed(),
            fmt_size(total_bytes),
            fmt_size(total_index_bytes),
            size_ratio(total_index_bytes, total_bytes),
        );
        for kind in &enabled {
            let dir = subindex_dir(index_dir, *kind);
            let size = dir_size(&dir)?;
            let segments = subindex_segment_count(index_dir, *kind)?;
            eprintln!(
                "  {:7} {} ({:.2}x source), segments {}",
                kind.as_str(),
                fmt_size(size),
                size_ratio(size, total_bytes),
                segments
            );
        }
    }
    Ok(stats)
}

fn is_append_update(entry: &CatalogEntry, file: &DiscoveredFile) -> bool {
    is_appendable_jsonl(&file.path)
        && file.size >= entry.size
        && entry.indexed_offset <= file.size
        && file.size > entry.indexed_offset
}

fn affected_indexes(
    enabled: &BTreeSet<IndexKind>,
    catalog_by_path: &BTreeMap<PathBuf, CatalogEntry>,
    scanned: &BTreeMap<PathBuf, DiscoveredFile>,
) -> BTreeSet<IndexKind> {
    let mut content_changed = false;
    let mut path_changed = false;

    for (path, entry) in catalog_by_path {
        let Some(file) = scanned.get(path) else {
            content_changed = true;
            path_changed = true;
            continue;
        };
        if file.size == entry.size && file.mtime_ms == entry.mtime_ms {
            continue;
        }
        if is_append_update(entry, file) {
            content_changed = true;
            continue;
        }
        content_changed = true;
        path_changed = true;
    }

    if scanned
        .keys()
        .any(|path| !catalog_by_path.contains_key(path))
    {
        content_changed = true;
        path_changed = true;
    }

    let mut affected = BTreeSet::new();
    if content_changed {
        for kind in [IndexKind::Word, IndexKind::Trigram] {
            if enabled.contains(&kind) {
                affected.insert(kind);
            }
        }
    }
    if path_changed && enabled.contains(&IndexKind::Path) {
        affected.insert(IndexKind::Path);
    }
    affected
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_scan::FileKind;

    fn catalog_entry(path: &str, size: u64, mtime_ms: u128) -> CatalogEntry {
        CatalogEntry {
            file_id: 7,
            kind: FileKind::SessionJsonl.as_str().to_string(),
            size,
            mtime_ms,
            indexed_offset: size,
            next_chunk_id: 1,
            path: PathBuf::from(path),
        }
    }

    fn discovered_file(path: &str, size: u64, mtime_ms: u128) -> DiscoveredFile {
        DiscoveredFile {
            path: PathBuf::from(path),
            kind: FileKind::SessionJsonl,
            size,
            mtime_ms,
        }
    }

    #[test]
    fn append_update_only_affects_content_indexes() {
        let enabled = IndexKind::all();
        let path = PathBuf::from("/tmp/session.jsonl");
        let mut catalog = BTreeMap::new();
        catalog.insert(path.clone(), catalog_entry("/tmp/session.jsonl", 10, 1));
        let mut scanned = BTreeMap::new();
        scanned.insert(path, discovered_file("/tmp/session.jsonl", 20, 2));

        let affected = affected_indexes(&enabled, &catalog, &scanned);
        assert!(affected.contains(&IndexKind::Word));
        assert!(affected.contains(&IndexKind::Trigram));
        assert!(!affected.contains(&IndexKind::Path));
    }

    #[test]
    fn new_or_rewritten_file_affects_all_enabled_indexes() {
        let enabled = IndexKind::all();
        let path = PathBuf::from("/tmp/session.jsonl");
        let mut catalog = BTreeMap::new();
        catalog.insert(path.clone(), catalog_entry("/tmp/session.jsonl", 20, 1));
        let mut scanned = BTreeMap::new();
        scanned.insert(path, discovered_file("/tmp/session.jsonl", 10, 2));

        assert_eq!(affected_indexes(&enabled, &catalog, &scanned), enabled);
    }
}
