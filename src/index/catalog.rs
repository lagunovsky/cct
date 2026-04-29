//! TSV catalog + plain-text manifest sitting next to the Tantivy sub-indexes.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::schema::IndexKind;
use super::writer::WriterConfig;

const CATALOG_HEADER: &str = "file_id\tkind\tsize\tmtime_ms\tindexed_offset\tnext_chunk_id\tpath";

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub file_id: u64,
    pub kind: String,
    pub size: u64,
    pub mtime_ms: u128,
    pub indexed_offset: u64,
    pub next_chunk_id: u64,
    pub path: PathBuf,
}

pub fn catalog_path(index_dir: &Path) -> PathBuf {
    index_dir.join("catalog.tsv")
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
        let parts: Vec<&str> = line.splitn(7, '\t').collect();
        if parts.len() != 7 {
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
            path: PathBuf::from(parts[6]),
        });
    }
    Ok(entries)
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
        writeln!(
            f,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            e.file_id,
            e.kind,
            e.size,
            e.mtime_ms,
            e.indexed_offset,
            e.next_chunk_id,
            e.path.to_string_lossy()
        )?;
    }
    f.flush()?;
    drop(f);
    fs::rename(&tmp_path, &path).with_context(|| format!("replace catalog {}", path.display()))?;
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
                    if let Some(kind) = IndexKind::from_str(part) {
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
    let mut f = BufWriter::new(File::create(&path)?);
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
    Ok(())
}
