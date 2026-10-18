//! TSV catalog + plain-text manifest sitting next to the Tantivy sub-indexes.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::schema::{IndexKind, WORD_TOKEN_MAX_CHARS};
use super::writer::WriterConfig;

const CATALOG_HEADER: &str =
    "file_id\tkind\tsize\tmtime_ms\tindexed_offset\tnext_chunk_id\ttail_hash\tprompts\ttools\tdisplay_bytes\ttitle\tpath";

/// How many trailing bytes of the indexed prefix participate in `tail_hash`.
pub const TAIL_HASH_WINDOW: usize = 4096;

/// FNV-1a hash of the last `TAIL_HASH_WINDOW` bytes of an indexed prefix.
/// Lets an incremental update verify that a grown file is a true append —
/// an in-place rewrite that kept the size growing would otherwise leave
/// stale content indexed forever.
pub fn prefix_tail_hash(prefix: &[u8]) -> u64 {
    let from = prefix.len().saturating_sub(TAIL_HASH_WINDOW);
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in &prefix[from..] {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}
const DELTA_CATALOG_HEADER: &str =
    "file_id\tstart_offset\tend_offset\tstart_chunk_id\tend_chunk_id\tpath";

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub file_id: u64,
    pub kind: String,
    pub size: u64,
    pub mtime_ms: u128,
    pub indexed_offset: u64,
    pub next_chunk_id: u64,
    /// `prefix_tail_hash` of the indexed prefix; guards the append fast path.
    pub tail_hash: u64,
    pub prompts: Option<u32>,
    pub tools: Option<u32>,
    pub display_bytes: Option<u64>,
    /// User-assigned session name (`/rename`); `None` when never named.
    pub title: Option<String>,
    pub path: PathBuf,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CatalogDisplayStats {
    pub prompts: u32,
    pub tools: u32,
    pub bytes: u64,
}

impl CatalogEntry {
    pub fn display_stats(&self) -> Option<CatalogDisplayStats> {
        Some(CatalogDisplayStats {
            prompts: self.prompts?,
            tools: self.tools?,
            bytes: self.display_bytes?,
        })
    }
}

#[derive(Clone, Debug)]
pub struct DeltaCatalogEntry {
    pub file_id: u64,
    pub start_offset: u64,
    pub end_offset: u64,
    pub start_chunk_id: u64,
    pub end_chunk_id: u64,
    pub path: PathBuf,
}

pub fn catalog_path(index_dir: &Path) -> PathBuf {
    index_dir.join("catalog.tsv")
}

/// Exclusive inter-process lock serializing index mutations (interactive
/// updates, the daemon's update/flush, reindex, compact). The lock file lives
/// NEXT TO `index_dir`, not inside it — a full reindex wipes the directory,
/// which would silently detach any lock held on a file within. Blocks until
/// the lock is free; released when the returned handle drops.
pub fn lock_index_exclusive(index_dir: &Path) -> Result<File> {
    let name = index_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("index");
    let parent = index_dir.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).ok();
    let path = parent.join(format!("{name}.lock"));
    let file = File::create(&path).with_context(|| format!("create lock {}", path.display()))?;
    file.lock()
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

/// Read the catalog. Returns an empty vec if the file is missing,
/// but bails on a schema mismatch so callers know to rebuild.
pub fn read_catalog(index_dir: &Path) -> Result<Vec<CatalogEntry>> {
    let path = catalog_path(index_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = raw.lines();
    let header = lines.next().unwrap_or_default();
    if header != CATALOG_HEADER {
        bail!(
            "catalog schema is old or incompatible: {}\nRebuild with `cct reindex`.",
            path.display()
        );
    }

    let mut entries = Vec::new();
    for (line_no, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(12, '\t').collect();
        if parts.len() != 12 {
            bail!("bad catalog line {} in {}", line_no + 2, path.display());
        }
        entries.push(CatalogEntry {
            file_id: parts[0]
                .parse()
                .with_context(|| format!("bad file_id at catalog line {}", line_no + 2))?,
            kind: parts[1].to_string(),
            size: parts[2]
                .parse()
                .with_context(|| format!("bad size at catalog line {}", line_no + 2))?,
            mtime_ms: parts[3]
                .parse()
                .with_context(|| format!("bad mtime_ms at catalog line {}", line_no + 2))?,
            indexed_offset: parts[4]
                .parse()
                .with_context(|| format!("bad indexed_offset at catalog line {}", line_no + 2))?,
            next_chunk_id: parts[5]
                .parse()
                .with_context(|| format!("bad next_chunk_id at catalog line {}", line_no + 2))?,
            tail_hash: parts[6]
                .parse()
                .with_context(|| format!("bad tail_hash at catalog line {}", line_no + 2))?,
            prompts: parse_optional_u32(parts[7], "prompts", line_no + 2)?,
            tools: parse_optional_u32(parts[8], "tools", line_no + 2)?,
            display_bytes: parse_optional_u64(parts[9], "display_bytes", line_no + 2)?,
            title: if parts[10].is_empty() {
                None
            } else {
                Some(parts[10].to_string())
            },
            path: PathBuf::from(parts[11]),
        });
    }
    Ok(entries)
}

fn parse_optional_u32(raw: &str, field: &str, line_no: usize) -> Result<Option<u32>> {
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse()
        .map(Some)
        .with_context(|| format!("bad {field} at catalog line {line_no}"))
}

fn parse_optional_u64(raw: &str, field: &str, line_no: usize) -> Result<Option<u64>> {
    if raw.is_empty() {
        return Ok(None);
    }
    raw.parse()
        .map(Some)
        .with_context(|| format!("bad {field} at catalog line {line_no}"))
}

/// Write the catalog atomically via tmp+rename.
pub fn write_catalog(index_dir: &Path, entries: &[CatalogEntry]) -> Result<PathBuf> {
    fs::create_dir_all(index_dir).ok();
    let path = catalog_path(index_dir);
    let tmp_path = index_dir.join("catalog.tsv.tmp");
    let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.path.cmp(&b.path));

    let mut f = BufWriter::new(File::create(&tmp_path)?);
    writeln!(f, "{CATALOG_HEADER}")?;
    for e in sorted {
        let prompts = e.prompts.map(|n| n.to_string()).unwrap_or_default();
        let tools = e.tools.map(|n| n.to_string()).unwrap_or_default();
        let display_bytes = e.display_bytes.map(|n| n.to_string()).unwrap_or_default();
        // Keep the TSV one-row-per-entry: tabs/newlines in a title would corrupt
        // column parsing, so flatten them to spaces.
        let title = e
            .title
            .as_deref()
            .map(|t| t.replace(['\t', '\n', '\r'], " "))
            .unwrap_or_default();
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            e.file_id,
            e.kind,
            e.size,
            e.mtime_ms,
            e.indexed_offset,
            e.next_chunk_id,
            e.tail_hash,
            prompts,
            tools,
            display_bytes,
            title,
            e.path.to_string_lossy()
        )?;
    }
    f.flush()?;
    drop(f);
    fs::rename(&tmp_path, &path).with_context(|| format!("replace catalog {}", path.display()))?;
    Ok(path)
}

pub fn delta_catalog_path(index_dir: &Path) -> PathBuf {
    super::schema::delta_dir(index_dir).join("catalog.tsv")
}

pub fn read_delta_catalog(index_dir: &Path) -> Result<Vec<DeltaCatalogEntry>> {
    let path = delta_catalog_path(index_dir);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let raw = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut lines = raw.lines();
    let header = lines.next().unwrap_or_default();
    if header != DELTA_CATALOG_HEADER {
        bail!(
            "delta catalog schema is old or incompatible: {}\nRemove delta or run `cct reindex`.",
            path.display()
        );
    }

    let mut entries = Vec::new();
    for (line_no, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.splitn(6, '\t').collect();
        if parts.len() != 6 {
            bail!(
                "bad delta catalog line {} in {}",
                line_no + 2,
                path.display()
            );
        }
        entries.push(DeltaCatalogEntry {
            file_id: parts[0]
                .parse()
                .with_context(|| format!("bad file_id at delta catalog line {}", line_no + 2))?,
            start_offset: parts[1].parse().with_context(|| {
                format!("bad start_offset at delta catalog line {}", line_no + 2)
            })?,
            end_offset: parts[2]
                .parse()
                .with_context(|| format!("bad end_offset at delta catalog line {}", line_no + 2))?,
            start_chunk_id: parts[3].parse().with_context(|| {
                format!("bad start_chunk_id at delta catalog line {}", line_no + 2)
            })?,
            end_chunk_id: parts[4].parse().with_context(|| {
                format!("bad end_chunk_id at delta catalog line {}", line_no + 2)
            })?,
            path: PathBuf::from(parts[5]),
        });
    }
    Ok(entries)
}

pub fn write_delta_catalog(index_dir: &Path, entries: &[DeltaCatalogEntry]) -> Result<PathBuf> {
    let delta = super::schema::delta_dir(index_dir);
    fs::create_dir_all(&delta).ok();
    let path = delta_catalog_path(index_dir);
    let tmp_path = delta.join("catalog.tsv.tmp");
    let mut sorted: Vec<&DeltaCatalogEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        a.file_id
            .cmp(&b.file_id)
            .then_with(|| a.start_offset.cmp(&b.start_offset))
            .then_with(|| a.path.cmp(&b.path))
    });

    let mut f = BufWriter::new(File::create(&tmp_path)?);
    writeln!(f, "{DELTA_CATALOG_HEADER}")?;
    for e in sorted {
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}",
            e.file_id,
            e.start_offset,
            e.end_offset,
            e.start_chunk_id,
            e.end_chunk_id,
            e.path.to_string_lossy()
        )?;
    }
    f.flush()?;
    drop(f);
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("replace delta catalog {}", path.display()))?;
    Ok(path)
}

#[derive(Clone, Debug, Default)]
pub struct Manifest {
    pub source: Option<PathBuf>,
    pub indexes: Option<BTreeSet<IndexKind>>,
}

pub fn manifest_path(index_dir: &Path) -> PathBuf {
    index_dir.join("manifest.txt")
}

pub fn read_manifest(index_dir: &Path) -> Result<Manifest> {
    let mut manifest = Manifest::default();
    let path = manifest_path(index_dir);
    let Ok(raw) = fs::read_to_string(&path) else {
        return Ok(manifest);
    };
    for line in raw.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "source" => manifest.source = Some(PathBuf::from(value)),
            "indexes" => {
                let mut set = BTreeSet::new();
                for part in value.split(',') {
                    if let Ok(kind) = part.parse::<IndexKind>() {
                        set.insert(kind);
                    }
                }
                manifest.indexes = Some(set);
            }
            _ => {}
        }
    }
    Ok(manifest)
}

pub fn write_manifest(
    index_dir: &Path,
    source: &Path,
    indexed_files: usize,
    source_bytes: u64,
    enabled: &BTreeSet<IndexKind>,
    config: WriterConfig,
) -> Result<()> {
    fs::create_dir_all(index_dir).ok();
    let path = manifest_path(index_dir);
    let tmp_path = index_dir.join("manifest.txt.tmp");
    let mut f = BufWriter::new(File::create(&tmp_path)?);
    writeln!(f, "source={}", source.display())?;
    writeln!(f, "indexed_files={indexed_files}")?;
    writeln!(f, "source_bytes={source_bytes}")?;
    writeln!(f, "writer_memory_mb={}", config.memory_mb)?;
    writeln!(
        f,
        "writer_threads={}",
        config
            .worker_threads
            .map(|n| n.to_string())
            .unwrap_or_else(|| "auto".to_string())
    )?;
    writeln!(f, "merge_threads={}", config.merge_threads)?;
    writeln!(f, "merge_policy={}", config.merge_policy.as_str())?;
    writeln!(
        f,
        "indexes={}",
        enabled
            .iter()
            .map(|k| k.as_str())
            .collect::<Vec<_>>()
            .join(",")
    )?;
    // Diagnostic: an index built before the cap still works (long tokens just
    // sit there unused), but its `word` dir is several times larger.
    writeln!(f, "word_token_max_chars={WORD_TOKEN_MAX_CHARS}")?;
    // Flush explicitly: a BufWriter drop swallows write errors, and a torn
    // manifest loses the `indexes=` line, breaking `--only` setups.
    f.flush()?;
    drop(f);
    fs::rename(&tmp_path, &path)
        .with_context(|| format!("replace manifest {}", path.display()))?;
    Ok(())
}
