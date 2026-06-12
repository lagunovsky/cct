//! Incremental update against an existing index.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Result};

use crate::fs_scan::{is_appendable_jsonl, DiscoveredFile};
use crate::term::fmt_size;

use super::build::{
    add_content_bytes, add_whole_file, append_file_tail, create_or_open_targets, delete_file_docs,
    dir_size, open_existing_targets, open_optional_targets, project_id_for_path, read_file_range,
    size_ratio, subindex_segment_count,
};
use super::catalog::{
    read_catalog, read_delta_catalog, read_manifest, write_catalog, write_delta_catalog,
    write_manifest, CatalogEntry, DeltaCatalogEntry,
};
use super::schema::{delta_dir, subindex_dir, IndexKind};
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
    /// Catalog entries outside the current scope. Carried through `run_update`
    /// without re-stat'ing or re-indexing them so a scoped update does not
    /// accidentally drop unrelated projects.
    pub preserved: Vec<CatalogEntry>,
    /// Grown files whose indexed prefix no longer matches its recorded
    /// `tail_hash` — they were rewritten in place, not appended, and must take
    /// the full-reindex path instead of the append fast path.
    pub force_reindex: BTreeSet<PathBuf>,
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
    build_plan_scoped(index_dir, source_override, files, None)
}

/// Like `build_plan` but treats catalog entries whose path is not under
/// `scope` as untouched — they ride through `run_update` unchanged.
pub fn build_plan_scoped(
    index_dir: &Path,
    source_override: Option<PathBuf>,
    files: Vec<DiscoveredFile>,
    scope: Option<&Path>,
) -> Result<UpdatePlan> {
    let manifest = read_manifest(index_dir)?;
    let source = source_override
        .or(manifest.source)
        .ok_or_else(|| anyhow::anyhow!("source not set in manifest"))?;
    let enabled = manifest.indexes.unwrap_or_else(IndexKind::all);
    let catalog = read_catalog(index_dir)?;

    let scanned: BTreeMap<PathBuf, DiscoveredFile> =
        files.into_iter().map(|f| (f.path.clone(), f)).collect();

    let mut catalog_by_path: BTreeMap<PathBuf, CatalogEntry> = BTreeMap::new();
    let mut preserved: Vec<CatalogEntry> = Vec::new();
    for entry in catalog {
        if let Some(scope) = scope {
            if !entry.path.starts_with(scope) {
                preserved.push(entry);
                continue;
            }
        }
        catalog_by_path.insert(entry.path.clone(), entry);
    }

    let mut force_reindex = BTreeSet::new();
    for (path, entry) in &catalog_by_path {
        let Some(file) = scanned.get(path) else {
            continue;
        };
        if file.size == entry.size && file.mtime_ms == entry.mtime_ms {
            continue;
        }
        if is_append_candidate(entry, file) && !indexed_prefix_intact(entry) {
            force_reindex.insert(path.clone());
        }
    }

    Ok(UpdatePlan {
        source,
        enabled,
        catalog_by_path,
        scanned,
        preserved,
        force_reindex,
    })
}

/// Re-hash the tail of the recorded indexed prefix and compare against the
/// catalog. A mismatch means the file was rewritten in place even though it
/// grew, so appending the new tail would leave stale content indexed.
fn indexed_prefix_intact(entry: &CatalogEntry) -> bool {
    if entry.indexed_offset == 0 {
        return true;
    }
    let from = entry
        .indexed_offset
        .saturating_sub(super::catalog::TAIL_HASH_WINDOW as u64);
    match read_file_range(&entry.path, from, entry.indexed_offset) {
        Ok(window) => super::catalog::prefix_tail_hash(&window) == entry.tail_hash,
        Err(_) => false,
    }
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
        preserved,
        force_reindex,
    } = plan;
    let start = Instant::now();

    let enabled_names = enabled
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let active = affected_indexes(&enabled, &catalog_by_path, &scanned, &force_reindex);
    let main_names = index_names(&active.main);
    let delta_names = index_names(&active.delta);
    let delta_config = WriterConfig::for_delta_update();
    if verbose {
        eprintln!(
            "Updating {} from {} [{}]",
            index_dir.display(),
            source.display(),
            enabled_names,
        );
        eprintln!("  active main writers [{main_names}], delta writers [{delta_names}]");
        if !active.main.is_empty() {
            eprintln!("  main writer:");
            print_writer_config(config, active.main.len());
        }
        if !active.delta.is_empty() {
            eprintln!("  delta writer:");
            print_writer_config(delta_config, active.delta.len());
        }
    }

    let delta_root = delta_dir(index_dir);
    let mut main_targets = open_existing_targets(index_dir, &active.main, config)?;
    let mut delta_targets = if active.delta.is_empty() {
        Vec::new()
    } else if active.delta_needs_create {
        create_or_open_targets(&delta_root, &active.delta, delta_config)?
    } else {
        open_optional_targets(&delta_root, &active.delta, delta_config)?
    };
    let mut delta_catalog = read_delta_catalog(index_dir)?;
    let mut delta_catalog_dirty = false;
    let mut stats = UpdateStats::default();
    let max_in_scope = catalog_by_path.values().map(|e| e.file_id).max();
    let max_preserved = preserved.iter().map(|e| e.file_id).max();
    let mut next_file_id = max_in_scope
        .into_iter()
        .chain(max_preserved)
        .max()
        .unwrap_or(0)
        .saturating_add(1);

    let mut new_catalog: Vec<CatalogEntry> = preserved;
    let scanned_paths: BTreeSet<PathBuf> = scanned.keys().cloned().collect();

    for (path, mut entry) in std::mem::take(&mut catalog_by_path) {
        let Some(file) = scanned.get(&path) else {
            delete_file_docs(&mut main_targets, entry.file_id);
            delete_file_docs(&mut delta_targets, entry.file_id);
            delta_catalog_dirty |= remove_delta_entries_for_file(&mut delta_catalog, entry.file_id);
            stats.deleted += 1;
            continue;
        };

        if file.size == entry.size && file.mtime_ms == entry.mtime_ms {
            stats.unchanged += 1;
            new_catalog.push(entry);
            continue;
        }

        if is_append_candidate(&entry, file) && !force_reindex.contains(&path) {
            let start_offset = entry.indexed_offset;
            let start_chunk_id = entry.next_chunk_id;
            let appended = match append_file_tail(&mut delta_targets, &mut entry, file) {
                Ok(appended) => appended,
                // Vanished between scan and read (session cleanup): treat as
                // deleted instead of aborting the whole update.
                Err(err) if is_not_found(&err) => {
                    delete_file_docs(&mut main_targets, entry.file_id);
                    delete_file_docs(&mut delta_targets, entry.file_id);
                    delta_catalog_dirty |=
                        remove_delta_entries_for_file(&mut delta_catalog, entry.file_id);
                    stats.deleted += 1;
                    continue;
                }
                Err(err) => return Err(err),
            };
            stats.appended += 1;
            stats.indexed_chunks += appended.chunks;
            stats.read_bytes += appended.read_bytes;
            stats.indexed_bytes += appended.indexed_bytes;
            if appended.chunks > 0 && appended.indexed_bytes > 0 && !delta_targets.is_empty() {
                delta_catalog.push(DeltaCatalogEntry {
                    file_id: entry.file_id,
                    start_offset,
                    end_offset: entry.indexed_offset,
                    start_chunk_id,
                    end_chunk_id: entry.next_chunk_id,
                    path: entry.path.clone(),
                });
                delta_catalog_dirty = true;
            }
            new_catalog.push(entry);
            continue;
        }

        delete_file_docs(&mut main_targets, entry.file_id);
        delete_file_docs(&mut delta_targets, entry.file_id);
        delta_catalog_dirty |= remove_delta_entries_for_file(&mut delta_catalog, entry.file_id);
        let (new_entry, chunks) = match add_whole_file(&mut main_targets, file, entry.file_id) {
            Ok(added) => added,
            // Docs are already deleted above, so a vanished file is just a delete.
            Err(err) if is_not_found(&err) => {
                stats.deleted += 1;
                continue;
            }
            Err(err) => return Err(err),
        };
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
        // A crash between commit and write_catalog re-runs this branch with
        // the same deterministic file_id; deleting first keeps it idempotent
        // (a no-op delete on the first run).
        delete_file_docs(&mut main_targets, next_file_id);
        let (entry, chunks) = match add_whole_file(&mut main_targets, file, next_file_id) {
            Ok(added) => added,
            Err(err) if is_not_found(&err) => continue,
            Err(err) => return Err(err),
        };
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
        commit_targets("main", main_targets, verbose)?;
        commit_targets("delta", delta_targets, verbose)?;
    } else if verbose {
        eprintln!("No index documents changed; skipping commit.");
    }

    if delta_catalog_dirty {
        if delta_catalog.is_empty() {
            remove_delta_catalog(index_dir)?;
        } else {
            write_delta_catalog(index_dir, &delta_catalog)?;
        }
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
        let delta_root = delta_dir(index_dir);
        if delta_root.is_dir() {
            let delta_size = dir_size(&delta_root)?;
            eprintln!("  delta   {}", fmt_size(delta_size));
            for kind in &enabled {
                let dir = subindex_dir(&delta_root, *kind);
                if !dir.is_dir() {
                    continue;
                }
                let size = dir_size(&dir)?;
                let segments = subindex_segment_count(&delta_root, *kind)?;
                eprintln!(
                    "    {:5} {} segments {}",
                    kind.as_str(),
                    fmt_size(size),
                    segments
                );
            }
        }
    }
    Ok(stats)
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.root_cause()
        .downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// Size/mtime say this could be a pure append; `force_reindex` (built from
/// the catalog `tail_hash` check) has the final word.
fn is_append_candidate(entry: &CatalogEntry, file: &DiscoveredFile) -> bool {
    is_appendable_jsonl(&file.path)
        && file.size >= entry.size
        && entry.indexed_offset <= file.size
        && file.size > entry.indexed_offset
}

struct AffectedIndexes {
    main: BTreeSet<IndexKind>,
    delta: BTreeSet<IndexKind>,
    delta_needs_create: bool,
}

fn affected_indexes(
    enabled: &BTreeSet<IndexKind>,
    catalog_by_path: &BTreeMap<PathBuf, CatalogEntry>,
    scanned: &BTreeMap<PathBuf, DiscoveredFile>,
    force_reindex: &BTreeSet<PathBuf>,
) -> AffectedIndexes {
    let mut main_content_changed = false;
    let mut main_path_changed = false;
    let mut delta_content_changed = false;
    let mut delta_needs_create = false;

    for (path, entry) in catalog_by_path {
        let Some(file) = scanned.get(path) else {
            main_content_changed = true;
            main_path_changed = true;
            delta_content_changed = true;
            continue;
        };
        if file.size == entry.size && file.mtime_ms == entry.mtime_ms {
            continue;
        }
        if is_append_candidate(entry, file) && !force_reindex.contains(path) {
            delta_content_changed = true;
            delta_needs_create = true;
            continue;
        }
        main_content_changed = true;
        main_path_changed = true;
        delta_content_changed = true;
    }

    if scanned
        .keys()
        .any(|path| !catalog_by_path.contains_key(path))
    {
        main_content_changed = true;
        main_path_changed = true;
    }

    let mut main = BTreeSet::new();
    if main_content_changed {
        for kind in [IndexKind::Word, IndexKind::Trigram] {
            if enabled.contains(&kind) {
                main.insert(kind);
            }
        }
    }
    if main_path_changed && enabled.contains(&IndexKind::Path) {
        main.insert(IndexKind::Path);
    }

    let mut delta = BTreeSet::new();
    if delta_content_changed {
        for kind in [IndexKind::Word, IndexKind::Trigram] {
            if enabled.contains(&kind) {
                delta.insert(kind);
            }
        }
    }

    AffectedIndexes {
        main,
        delta,
        delta_needs_create,
    }
}

fn index_names(indexes: &BTreeSet<IndexKind>) -> String {
    if indexes.is_empty() {
        return "none".to_string();
    }
    indexes
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",")
}

fn remove_delta_entries_for_file(entries: &mut Vec<DeltaCatalogEntry>, file_id: u64) -> bool {
    let before = entries.len();
    entries.retain(|e| e.file_id != file_id);
    entries.len() != before
}

fn remove_delta_catalog(index_dir: &Path) -> Result<()> {
    let root = delta_dir(index_dir);
    match fs::remove_dir_all(&root) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(Into::into(err)),
    }
}

fn commit_targets(
    label: &str,
    mut targets: Vec<super::build::BuildTarget>,
    verbose: bool,
) -> Result<()> {
    for target in &mut targets {
        let phase = Instant::now();
        if verbose {
            eprintln!("Committing {label}/{}...", target.kind.as_str());
        }
        target.writer.commit()?;
        if verbose {
            eprintln!(
                "Committed {label}/{} in {:?}",
                target.kind.as_str(),
                phase.elapsed()
            );
        }
    }
    for target in targets {
        let phase = Instant::now();
        if verbose {
            eprintln!("Waiting merges for {label}/{}...", target.kind.as_str());
        }
        target.writer.wait_merging_threads()?;
        if verbose {
            eprintln!(
                "Merges done for {label}/{} in {:?}",
                target.kind.as_str(),
                phase.elapsed()
            );
        }
    }
    Ok(())
}

pub fn flush_delta(index_dir: &Path, config: WriterConfig, verbose: bool) -> Result<()> {
    let delta_catalog = read_delta_catalog(index_dir)?;
    if delta_catalog.is_empty() {
        return Ok(());
    }

    let manifest = read_manifest(index_dir)?;
    let enabled = manifest.indexes.unwrap_or_else(IndexKind::all);
    let mut content_indexes = BTreeSet::new();
    for kind in [IndexKind::Word, IndexKind::Trigram] {
        if enabled.contains(&kind) {
            content_indexes.insert(kind);
        }
    }
    if content_indexes.is_empty() {
        return Ok(());
    }

    if verbose {
        eprintln!(
            "Flushing delta {} ranges into main [{}]",
            delta_catalog.len(),
            index_names(&content_indexes)
        );
        print_writer_config(config, content_indexes.len());
    }

    let catalog_by_id: BTreeMap<u64, CatalogEntry> = read_catalog(index_dir)?
        .into_iter()
        .map(|e| (e.file_id, e))
        .collect();

    let mut targets = open_existing_targets(index_dir, &content_indexes, config)?;
    let start = Instant::now();
    // Per file: delete its main content docs and re-add the whole indexed
    // prefix. Replaying delta ranges on top of main would duplicate documents
    // whenever a crash lands between commit and delta-dir removal; the
    // delete-before-add makes a replay idempotent.
    let mut flushed_files = BTreeSet::new();
    for delta in &delta_catalog {
        if !flushed_files.insert(delta.file_id) {
            continue;
        }
        let Some(entry) = catalog_by_id.get(&delta.file_id) else {
            continue;
        };
        // Read before deleting: if the file vanished, keep its existing main
        // docs — the next update will notice the deletion and clean them up.
        let Ok(data) = read_file_range(&entry.path, 0, entry.indexed_offset) else {
            if verbose {
                eprintln!("  skipped vanished {}", entry.path.display());
            }
            continue;
        };
        let path_string = entry.path.to_string_lossy().into_owned();
        let project_id = project_id_for_path(&entry.path);
        delete_file_docs(&mut targets, delta.file_id);
        let mut fresh = CatalogEntry {
            file_id: entry.file_id,
            kind: String::new(),
            size: entry.indexed_offset,
            mtime_ms: 0,
            indexed_offset: entry.indexed_offset,
            next_chunk_id: 0,
            tail_hash: 0,
            prompts: None,
            tools: None,
            display_bytes: None,
            title: None,
            path: entry.path.clone(),
        };
        let _content = add_content_bytes(
            &mut targets,
            &mut fresh,
            &data,
            0,
            &path_string,
            &project_id,
        )?;
    }
    commit_targets("main", targets, verbose)?;

    let delta_root = delta_dir(index_dir);
    if delta_root.exists() {
        fs::remove_dir_all(&delta_root)?;
    }
    if verbose {
        eprintln!("Flushed delta in {:?}", start.elapsed());
    }
    Ok(())
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
            tail_hash: 0,
            prompts: None,
            tools: None,
            display_bytes: None,
            title: None,
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

        let affected = affected_indexes(&enabled, &catalog, &scanned, &BTreeSet::new());
        assert!(affected.main.is_empty());
        assert!(affected.delta.contains(&IndexKind::Word));
        assert!(affected.delta.contains(&IndexKind::Trigram));
        assert!(!affected.delta.contains(&IndexKind::Path));
        assert!(affected.delta_needs_create);
    }

    #[test]
    fn new_or_rewritten_file_affects_all_enabled_indexes() {
        let enabled = IndexKind::all();
        let path = PathBuf::from("/tmp/session.jsonl");
        let mut catalog = BTreeMap::new();
        catalog.insert(path.clone(), catalog_entry("/tmp/session.jsonl", 20, 1));
        let mut scanned = BTreeMap::new();
        scanned.insert(path, discovered_file("/tmp/session.jsonl", 10, 2));

        let affected = affected_indexes(&enabled, &catalog, &scanned, &BTreeSet::new());
        assert_eq!(affected.main, enabled);
        assert!(affected.delta.contains(&IndexKind::Word));
        assert!(affected.delta.contains(&IndexKind::Trigram));
        assert!(!affected.delta_needs_create);
    }
}
