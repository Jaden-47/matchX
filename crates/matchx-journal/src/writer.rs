use crate::{JournalError, codec};
use matchx_types::Command;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Append-only binary journal writer.
///
/// Record layout (all little-endian):
///   [u32 payload_len][u64 sequence][payload_bytes...][u32 crc32]
///
/// CRC32 is computed over `sequence_bytes ++ payload_bytes`.
pub struct JournalWriter {
    mode: WriterMode,
}

enum WriterMode {
    Single { file: File },
    Segmented(SegmentedWriter),
}

struct SegmentedWriter {
    dir: PathBuf,
    max_segment_bytes: u64,
    current_segment_index: u64,
    current_segment_size: u64,
    file: File,
}

impl JournalWriter {
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            mode: WriterMode::Single { file },
        })
    }

    pub fn open_segmented(dir: &Path, max_segment_bytes: u64) -> Result<Self, JournalError> {
        std::fs::create_dir_all(dir)?;
        let max_segment_bytes = max_segment_bytes.max(1);

        let segments = list_segment_paths(dir)?;
        let (current_segment_index, file_path) = if let Some(last) = segments.last() {
            let idx = parse_segment_index(last).unwrap_or(1);
            (idx, last.clone())
        } else {
            let idx = 1;
            (idx, segment_path(dir, idx))
        };

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)?;
        let current_segment_size = file.metadata()?.len();

        Ok(Self {
            mode: WriterMode::Segmented(SegmentedWriter {
                dir: dir.to_path_buf(),
                max_segment_bytes,
                current_segment_index,
                current_segment_size,
                file,
            }),
        })
    }

    pub fn append(&mut self, sequence: u64, cmd: &Command) -> Result<(), JournalError> {
        let record = codec::encode_record(sequence, cmd);
        self.append_raw(&record)?;
        self.flush()?;
        Ok(())
    }

    pub(crate) fn append_raw_batch(&mut self, batch: &[Vec<u8>]) -> Result<(), JournalError> {
        for record in batch {
            self.append_raw(record)?;
        }
        self.flush()?;
        Ok(())
    }

    fn append_raw(&mut self, record: &[u8]) -> Result<(), JournalError> {
        match &mut self.mode {
            WriterMode::Single { file } => {
                file.write_all(record)?;
            }
            WriterMode::Segmented(segmented) => {
                let next_size = segmented.current_segment_size + record.len() as u64;
                if segmented.current_segment_size > 0 && next_size > segmented.max_segment_bytes {
                    rotate_segment(segmented)?;
                }
                segmented.file.write_all(record)?;
                segmented.current_segment_size += record.len() as u64;
            }
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), JournalError> {
        match &mut self.mode {
            WriterMode::Single { file } => file.flush()?,
            WriterMode::Segmented(segmented) => segmented.file.flush()?,
        }
        Ok(())
    }
}

fn rotate_segment(segmented: &mut SegmentedWriter) -> Result<(), JournalError> {
    segmented.current_segment_index += 1;
    let next_path = segment_path(&segmented.dir, segmented.current_segment_index);
    segmented.file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(next_path)?;
    segmented.current_segment_size = 0;
    Ok(())
}

fn segment_path(dir: &Path, index: u64) -> PathBuf {
    dir.join(format!("journal-{index:08}.wal"))
}

fn list_segment_paths(dir: &Path) -> Result<Vec<PathBuf>, JournalError> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if parse_segment_index(&path).is_some() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn parse_segment_index(path: &Path) -> Option<u64> {
    let name = path.file_name()?.to_str()?;
    if !(name.starts_with("journal-") && name.ends_with(".wal")) {
        return None;
    }
    let index = &name["journal-".len()..name.len() - ".wal".len()];
    index.parse::<u64>().ok()
}
