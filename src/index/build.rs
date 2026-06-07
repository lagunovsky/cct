//! Full reindex.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::time::{Instant, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use crate::fs_scan::{is_appendable_jsonl, DiscoveredFile};
use crate::term::fmt_size;
use crate::text::{extract_text, SessionStats};

use super::catalog::{write_catalog, write_manifest, CatalogEntry};
use super::schema::{
    fields_from_schema, register_tokenizers, schema_and_fields, subindex_dir, Fields, IndexKind,
};
use super::writer::{create_writer, print_writer_config, WriterConfig};

/// Per chunk byte target. Larger chunks mean fewer Tantivy docs but coarser snippet ranges.
pub const CHUNK_TARGET_BYTES: usize = 512 * 1024;

pub struct BuildTarget {
    pub kind: IndexKind,
    pub fields: Fields,
    pub writer: IndexWriter,
}

pub struct ByteChunk {
    pub start: usize,
    pub end: usize,
}

#[derive(Default)]
pub struct ContentAddResult {
    pub chunks: u64,
    pub stats: SessionStats,
    pub stats_available: bool,
}

/// Full reindex from scratch. Wipes `index_dir` first when it exists.
pub fn run_full_reindex(
    source: &Path,
    index_dir: &Path,
    enabled: &BTreeSet<IndexKind>,
    files: Vec<DiscoveredFile>,
    config: WriterConfig,
    verbose: bool,
) -> Result<()> {
    if !source.is_dir() {
        bail!("source dir does not exist: {}", source.display());
    }
    if enabled.is_empty() {
        bail!("no indexes enabled");
    }

    if index_dir.exists() {
        fs::remove_dir_all(index_dir)
            .with_context(|| format!("remove existing index {}", index_dir.display()))?;
    }
    fs::create_dir_all(index_dir)
        .with_context(|| format!("create index dir {}", index_dir.display()))?;

    let enabled_names = enabled
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join(",");
    if verbose {
        eprintln!(
            "Indexing {} files from {} into {} [{}]",
            files.len(),
            source.display(),
            index_dir.display(),
            enabled_names,
        );
        print_writer_config(config, enabled.len());
    }

    let mut targets = Vec::new();
    for kind in enabled {
        let dir = subindex_dir(index_dir, *kind);
        fs::create_dir_all(&dir).with_context(|| format!("create subindex {}", dir.display()))?;
        let (schema, fields) = schema_and_fields(*kind);
        let index = Index::create_in_dir(&dir, schema)?;
        register_tokenizers(&index)?;
        let writer = create_writer(&index, &config, enabled.len())?;
        targets.push(BuildTarget {
            kind: *kind,
            fields,
            writer,
        });
    }

    let start = Instant::now();
    let add_start = Instant::now();
    let mut indexed = 0usize;
    let mut total_bytes = 0u64;
    let mut indexed_chunks = 0u64;
    let mut catalog_entries = Vec::new();
    for (file_id, file) in files.iter().enumerate() {
        let (entry, chunks) = match add_whole_file(&mut targets, file, file_id as u64) {
            Ok(result) => result,
            Err(err) => {
                if verbose {
                    eprintln!("  skipped {}: {err:#}", file.path.display());
                }
                continue;
            }
        };
        indexed += 1;
        total_bytes += entry.size;
        indexed_chunks += chunks;
        catalog_entries.push(entry);
        if verbose && indexed.is_multiple_of(500) {
            eprint!("\r  {indexed} indexed");
        }
    }
    if verbose {
        eprintln!("\r  {indexed} indexed");
        eprintln!(
            "Added documents in {:?}; content chunks {}",
            add_start.elapsed(),
            indexed_chunks
        );
    }
    let catalog_path = write_catalog(index_dir, &catalog_entries)?;

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

    write_manifest(index_dir, source, indexed, total_bytes, enabled, config)?;

    if verbose {
        let elapsed = start.elapsed();
        let total_index_bytes = dir_size(index_dir)?;
        eprintln!(
            "Done in {:?}; source bytes {}; total index bytes {} ({:.2}x source)",
            elapsed,
            fmt_size(total_bytes),
            fmt_size(total_index_bytes),
            size_ratio(total_index_bytes, total_bytes),
        );
        for kind in enabled {
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
        eprintln!("  catalog {}", catalog_path.display());
    }
    Ok(())
}

/// Indexed prefix length: for `.jsonl` we stop at the last complete newline,
/// for everything else we index the full file.
pub fn indexable_prefix_len(path: &Path, data: &[u8]) -> usize {
    if is_appendable_jsonl(path) {
        return data
            .iter()
            .rposition(|b| *b == b'\n')
            .map(|p| p + 1)
            .unwrap_or(0);
    }
    data.len()
}

pub fn line_chunks(data: &[u8]) -> Vec<ByteChunk> {
    let mut chunks = Vec::new();
    let mut chunk_start = 0usize;
    let mut pos = 0usize;
    while pos < data.len() {
        let line_end = data[pos..]
            .iter()
            .position(|b| *b == b'\n')
            .map(|rel| pos + rel + 1)
            .unwrap_or(data.len());
        if pos > chunk_start && line_end.saturating_sub(chunk_start) > CHUNK_TARGET_BYTES {
            chunks.push(ByteChunk {
                start: chunk_start,
                end: pos,
            });
            chunk_start = pos;
        }
        pos = line_end;
        if pos.saturating_sub(chunk_start) >= CHUNK_TARGET_BYTES {
            chunks.push(ByteChunk {
                start: chunk_start,
                end: pos,
            });
            chunk_start = pos;
        }
    }
    if chunk_start < data.len() {
        chunks.push(ByteChunk {
            start: chunk_start,
            end: data.len(),
        });
    }
    chunks
}

pub fn add_whole_file(
    targets: &mut [BuildTarget],
    file: &DiscoveredFile,
    file_id: u64,
) -> Result<(CatalogEntry, u64)> {
    let before_read = file_identity(&file.path).ok();
    let data = fs::read(&file.path).with_context(|| format!("read {}", file.path.display()))?;
    let after_read = file_identity(&file.path).ok();
    let (size, mtime_ms) = catalog_identity_for_read(file, data.len(), before_read, after_read);
    let indexed_len = indexable_prefix_len(&file.path, &data);
    let path_string = file.path.to_string_lossy().into_owned();
    let project_id = project_id_for_path(&file.path);
    let mut entry = CatalogEntry {
        file_id,
        kind: file.kind.as_str().to_string(),
        size,
        mtime_ms,
        indexed_offset: indexed_len as u64,
        next_chunk_id: 0,
        prompts: None,
        tools: None,
        display_bytes: None,
        path: file.path.clone(),
    };

    for target in targets.iter_mut().filter(|t| t.kind == IndexKind::Path) {
        add_path_document(target, &entry, &path_string, &project_id)?;
    }

    let content = add_content_bytes(
        targets,
        &mut entry,
        &data[..indexed_len],
        0,
        &path_string,
        &project_id,
    )?;
    if content.stats_available {
        set_entry_display_stats(&mut entry, &file.path, content.stats);
    }
    Ok((entry, content.chunks))
}

fn catalog_identity_for_read(
    file: &DiscoveredFile,
    read_len: usize,
    before_read: Option<(u64, u128)>,
    after_read: Option<(u64, u128)>,
) -> (u64, u128) {
    let read_size = read_len as u64;
    if let (Some(before), Some(after)) = (before_read, after_read) {
        if before == after && after.0 == read_size {
            return after;
        }
        return (read_size, before.1);
    }
    (read_size, file.mtime_ms)
}

fn file_identity(path: &Path) -> Result<(u64, u128)> {
    let meta = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime_ms = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Ok((meta.len(), mtime_ms))
}

pub struct AppendResult {
    pub chunks: u64,
    pub read_bytes: u64,
    pub indexed_bytes: u64,
}

pub fn append_file_tail(
    targets: &mut [BuildTarget],
    entry: &mut CatalogEntry,
    file: &DiscoveredFile,
) -> Result<AppendResult> {
    let start = entry.indexed_offset;
    let data = read_file_range(&file.path, start, file.size)?;
    let indexed_len = indexable_prefix_len(&file.path, &data);
    let indexed_end = start + indexed_len as u64;
    let path_string = file.path.to_string_lossy().into_owned();
    let project_id = project_id_for_path(&file.path);
    let content = add_content_bytes(
        targets,
        entry,
        &data[..indexed_len],
        start,
        &path_string,
        &project_id,
    )?;
    if content.stats_available {
        append_entry_display_stats(entry, &file.path, content.stats, file.size);
    }

    entry.kind = file.kind.as_str().to_string();
    entry.size = file.size;
    entry.mtime_ms = file.mtime_ms;
    entry.indexed_offset = indexed_end;
    entry.path = file.path.clone();

    Ok(AppendResult {
        chunks: content.chunks,
        read_bytes: data.len() as u64,
        indexed_bytes: indexed_len as u64,
    })
}

pub fn add_content_bytes(
    targets: &mut [BuildTarget],
    entry: &mut CatalogEntry,
    data: &[u8],
    base_offset: u64,
    path_string: &str,
    project_id: &str,
) -> Result<ContentAddResult> {
    let mut content_targets: Vec<&mut BuildTarget> = targets
        .iter_mut()
        .filter(|t| matches!(t.kind, IndexKind::Word | IndexKind::Trigram))
        .collect();
    if content_targets.is_empty() {
        return Ok(ContentAddResult::default());
    }
    if data.is_empty() {
        return Ok(ContentAddResult {
            chunks: 0,
            stats: SessionStats::default(),
            stats_available: true,
        });
    }

    let mut chunks_added = 0u64;
    let mut stats = SessionStats::default();
    for chunk in line_chunks(data) {
        let (text, chunk_stats) = extract_text(&data[chunk.start..chunk.end]);
        add_session_stats(&mut stats, chunk_stats);
        if text.trim().is_empty() {
            continue;
        }
        let chunk_id = entry.next_chunk_id;
        entry.next_chunk_id = entry.next_chunk_id.saturating_add(1);
        let start_offset = base_offset + chunk.start as u64;
        let end_offset = base_offset + chunk.end as u64;
        for target in content_targets.iter_mut() {
            let mut document = TantivyDocument::new();
            document.add_u64(target.fields.file_id, entry.file_id);
            document.add_u64(target.fields.chunk_id, chunk_id);
            document.add_text(target.fields.project_id, project_id);
            document.add_text(target.fields.path, path_string);
            document.add_u64(target.fields.start_offset, start_offset);
            document.add_u64(target.fields.end_offset, end_offset);
            document.add_text(target.fields.text, &text);
            target.writer.add_document(document)?;
        }
        chunks_added += 1;
    }
    Ok(ContentAddResult {
        chunks: chunks_added,
        stats,
        stats_available: true,
    })
}

pub fn add_path_document(
    target: &mut BuildTarget,
    entry: &CatalogEntry,
    path_string: &str,
    project_id: &str,
) -> Result<()> {
    let mut document = TantivyDocument::new();
    document.add_u64(target.fields.file_id, entry.file_id);
    document.add_u64(target.fields.chunk_id, 0u64);
    document.add_text(target.fields.project_id, project_id);
    document.add_text(target.fields.path, path_string);
    document.add_u64(target.fields.start_offset, 0u64);
    document.add_u64(target.fields.end_offset, entry.size);
    document.add_text(target.fields.text, path_string);
    target.writer.add_document(document)?;
    Ok(())
}

pub fn project_id_for_path(path: &Path) -> String {
    let project_dir = path
        .parent()
        .and_then(|parent| {
            let parent_name = parent.file_name().and_then(|s| s.to_str());
            if matches!(parent_name, Some("subagents" | "tool-results")) {
                parent.parent().and_then(Path::parent)
            } else {
                Some(parent)
            }
        })
        .unwrap_or(path);
    project_dir.to_string_lossy().into_owned()
}

fn set_entry_display_stats(entry: &mut CatalogEntry, path: &Path, stats: SessionStats) {
    if !is_display_stats_file(path) {
        return;
    }
    entry.prompts = Some(stats.prompts);
    entry.tools = Some(stats.tools);
    entry.display_bytes = Some(entry.size);
}

fn append_entry_display_stats(
    entry: &mut CatalogEntry,
    path: &Path,
    stats: SessionStats,
    file_size: u64,
) {
    if !is_display_stats_file(path) {
        return;
    }
    let (Some(prompts), Some(tools), Some(_bytes)) =
        (entry.prompts, entry.tools, entry.display_bytes)
    else {
        return;
    };
    entry.prompts = Some(prompts.saturating_add(stats.prompts));
    entry.tools = Some(tools.saturating_add(stats.tools));
    entry.display_bytes = Some(file_size);
}

fn is_display_stats_file(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("jsonl")
}

fn add_session_stats(total: &mut SessionStats, stats: SessionStats) {
    total.prompts = total.prompts.saturating_add(stats.prompts);
    total.answers = total.answers.saturating_add(stats.answers);
    total.tools = total.tools.saturating_add(stats.tools);
}

pub fn delete_file_docs(targets: &mut [BuildTarget], file_id: u64) {
    for target in targets {
        let term = Term::from_field_u64(target.fields.file_id, file_id);
        target.writer.delete_term(term);
    }
}

pub fn read_file_range(path: &Path, start: u64, end: u64) -> Result<Vec<u8>> {
    if end < start {
        bail!("bad range {start}..{end} for {}", path.display());
    }
    let len = end - start;
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    use std::io::{Read, Seek, SeekFrom};
    file.seek(SeekFrom::Start(start))?;
    let cap = len.min(usize::MAX as u64) as usize;
    let mut data = Vec::with_capacity(cap);
    file.take(len).read_to_end(&mut data)?;
    Ok(data)
}

pub fn open_existing_targets(
    index_dir: &Path,
    enabled: &BTreeSet<IndexKind>,
    config: WriterConfig,
) -> Result<Vec<BuildTarget>> {
    let mut targets = Vec::new();
    for kind in enabled {
        let dir = subindex_dir(index_dir, *kind);
        if !dir.is_dir() {
            bail!(
                "subindex does not exist: {}\nRebuild with `cct reindex`.",
                dir.display()
            );
        }
        let index =
            Index::open_in_dir(&dir).with_context(|| format!("open index {}", dir.display()))?;
        register_tokenizers(&index)?;
        let schema = index.schema();
        let fields = fields_from_schema(&schema).with_context(|| {
            format!(
                "schema in {} is old or incompatible; rebuild with `cct reindex`",
                dir.display()
            )
        })?;
        let writer = create_writer(&index, &config, enabled.len())?;
        targets.push(BuildTarget {
            kind: *kind,
            fields,
            writer,
        });
    }
    Ok(targets)
}

pub fn create_or_open_targets(
    index_dir: &Path,
    enabled: &BTreeSet<IndexKind>,
    config: WriterConfig,
) -> Result<Vec<BuildTarget>> {
    let mut targets = Vec::new();
    fs::create_dir_all(index_dir)
        .with_context(|| format!("create index root {}", index_dir.display()))?;
    for kind in enabled {
        let dir = subindex_dir(index_dir, *kind);
        let (index, fields) = if dir.exists() {
            let index = Index::open_in_dir(&dir)
                .with_context(|| format!("open index {}", dir.display()))?;
            register_tokenizers(&index)?;
            let schema = index.schema();
            let fields = fields_from_schema(&schema).with_context(|| {
                format!(
                    "schema in {} is old or incompatible; rebuild with `cct reindex`",
                    dir.display()
                )
            })?;
            (index, fields)
        } else {
            fs::create_dir_all(&dir)
                .with_context(|| format!("create subindex {}", dir.display()))?;
            let (schema, fields) = schema_and_fields(*kind);
            let index = Index::create_in_dir(&dir, schema)?;
            register_tokenizers(&index)?;
            (index, fields)
        };
        let writer = create_writer(&index, &config, enabled.len())?;
        targets.push(BuildTarget {
            kind: *kind,
            fields,
            writer,
        });
    }
    Ok(targets)
}

pub fn open_optional_targets(
    index_dir: &Path,
    enabled: &BTreeSet<IndexKind>,
    config: WriterConfig,
) -> Result<Vec<BuildTarget>> {
    let mut existing = BTreeSet::new();
    for kind in enabled {
        if subindex_dir(index_dir, *kind).is_dir() {
            existing.insert(*kind);
        }
    }
    open_existing_targets(index_dir, &existing, config)
}

pub fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0;
    if path.is_dir() {
        dir_size_inner(path, &mut total)?;
    }
    Ok(total)
}

fn dir_size_inner(path: &Path, total: &mut u64) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            dir_size_inner(&entry.path(), total)?;
        } else if ty.is_file() {
            *total += entry.metadata()?.len();
        }
    }
    Ok(())
}

pub fn size_ratio(size: u64, source_bytes: u64) -> f64 {
    if source_bytes == 0 {
        return 0.0;
    }
    size as f64 / source_bytes as f64
}

pub fn subindex_segment_count(root: &Path, kind: IndexKind) -> Result<usize> {
    let dir = subindex_dir(root, kind);
    if !dir.is_dir() {
        return Ok(0);
    }
    let index =
        Index::open_in_dir(&dir).with_context(|| format!("open index {}", dir.display()))?;
    Ok(index.load_metas()?.segments.len())
}

// Re-exported helper to avoid dragging tantivy::TantivyDocument into the rest of the crate.
pub use tantivy::TantivyDocument as TantivyDoc;

pub fn stored_string(doc: &TantivyDoc, field: tantivy::schema::Field) -> Option<String> {
    use tantivy::schema::Value;
    doc.get_first(field)?.as_str().map(ToOwned::to_owned)
}

pub fn stored_u64(doc: &TantivyDoc, field: tantivy::schema::Field) -> Option<u64> {
    use tantivy::schema::Value;
    doc.get_first(field)?.as_u64()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::project_id_for_path;

    #[test]
    fn project_id_for_indexed_paths_points_at_project_dir() {
        assert_eq!(
            project_id_for_path(Path::new("/projects/-proj/session.jsonl")),
            "/projects/-proj"
        );
        assert_eq!(
            project_id_for_path(Path::new("/projects/-proj/session/subagents/agent.jsonl")),
            "/projects/-proj"
        );
        assert_eq!(
            project_id_for_path(Path::new("/projects/-proj/session/tool-results/out.log")),
            "/projects/-proj"
        );
    }
}
