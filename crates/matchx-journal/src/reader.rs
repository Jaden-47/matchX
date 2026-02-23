use crate::{JournalError, codec};
use matchx_types::Command;
use std::path::{Path, PathBuf};

/// A single decoded journal record.
pub struct JournalEntry {
    pub sequence: u64,
    pub command: Command,
}

/// Sequential reader over a binary journal file.
pub struct JournalReader {
    paths: Vec<PathBuf>,
}

impl JournalReader {
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        if path.is_dir() {
            return Ok(Self {
                paths: list_segment_paths(path)?,
            });
        }

        // Preserve existing semantics: opening a missing file fails early.
        let _ = std::fs::File::open(path)?;

        Ok(Self {
            paths: vec![path.to_path_buf()],
        })
    }

    /// Read and validate every record in the file.
    /// Returns `Err(JournalError::CrcMismatch)` if any record is corrupt.
    pub fn read_all(&mut self) -> Result<Vec<JournalEntry>, JournalError> {
        let mut entries = Vec::new();

        for path in &self.paths {
            let data = std::fs::read(path)?;
            let mut pos = 0;
            while pos < data.len() {
                let (sequence, command, used) = codec::decode_record(&data[pos..])?;
                pos += used;
                entries.push(JournalEntry { sequence, command });
            }
        }

        Ok(entries)
    }
}

pub(crate) fn list_segment_paths(dir: &Path) -> Result<Vec<PathBuf>, JournalError> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if is_segment_file(&path) {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn is_segment_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with("journal-") && name.ends_with(".wal"))
        .unwrap_or(false)
}
