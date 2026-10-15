//! Read-only access to a Beyond All Reason map archive (`.sd7`, a 7-Zip container).
//!
//! Spring/Recoil's VFS is case-insensitive and uses `/` separators, so entries are
//! keyed by a normalised path.
//!
//! Entries are read individually rather than by streaming the whole archive. Map
//! archives are dominated by the tile file (`.smt`), which can exceed 100 MB and is
//! never needed here; decoding only the blocks holding the wanted entries keeps a
//! full sweep of the map library tractable.

use anyhow::{Context, Result};
use sevenz_rust2::{ArchiveReader, Password};
use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};

/// Normalise a VFS path the way the engine's case-insensitive lookup does.
pub fn normalise(path: &str) -> String {
    path.replace('\\', "/").trim_start_matches('/').to_ascii_lowercase()
}

/// An opened map archive.
pub struct MapArchive {
    path: PathBuf,
    reader: ArchiveReader<File>,
    /// normalised path -> name exactly as stored, for case-insensitive lookup
    index: BTreeMap<String, String>,
}

impl MapArchive {
    /// Open an archive and read its index. Reading the index does not decompress
    /// any entry payload.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let reader = ArchiveReader::open(&path, Password::empty())
            .with_context(|| format!("opening archive {}", path.display()))?;

        let index = reader
            .archive()
            .files
            .iter()
            .filter(|file| !file.is_directory())
            .map(|file| (normalise(file.name()), file.name().to_string()))
            .collect();

        Ok(Self { path, reader, index })
    }

    /// Archive file name without extension, e.g. `acidicquarry_5.17`.
    pub fn stem(&self) -> String {
        stem_of(&self.path)
    }

    /// Normalised entry paths, sorted. Stable across runs.
    pub fn entries(&self) -> impl Iterator<Item = &str> {
        self.index.keys().map(String::as_str)
    }

    /// Entry paths accepted by `select`, sorted.
    pub fn entries_matching(&self, mut select: impl FnMut(&str) -> bool) -> Vec<String> {
        self.entries().filter(|path| select(path)).map(str::to_owned).collect()
    }

    /// Decode one entry. `None` when the path is absent from the archive.
    pub fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let Some(stored) = self.index.get(&normalise(path)).cloned() else {
            return Ok(None);
        };
        let data = self
            .reader
            .read_file(&stored)
            .with_context(|| format!("reading {stored} from {}", self.path.display()))?;
        Ok(Some(data))
    }

    /// Decode each of `paths`, skipping any that are absent.
    pub fn read_all(&mut self, paths: &[String]) -> Result<BTreeMap<String, Vec<u8>>> {
        let mut out = BTreeMap::new();
        for path in paths {
            if let Some(data) = self.read(path)? {
                out.insert(normalise(path), data);
            }
        }
        Ok(out)
    }
}

/// File name without extension.
pub fn stem_of(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Every `.sd7` in `dir`, sorted by file name so runs are reproducible.
pub fn discover(dir: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    let dir = dir.as_ref();
    let mut found: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("listing map directory {}", dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|e| e.eq_ignore_ascii_case("sd7")))
        .collect();
    found.sort();
    Ok(found)
}
