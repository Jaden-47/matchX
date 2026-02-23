use crate::{JournalError, codec, reader};
use std::fs::OpenOptions;
use std::path::Path;

#[derive(Debug, Clone, Copy, Default)]
pub struct RecoveryReport {
    pub last_valid_sequence: u64,
    pub truncated_bytes: u64,
}

pub struct RecoveryManager;

impl RecoveryManager {
    pub fn recover_dir(dir: &Path) -> Result<RecoveryReport, JournalError> {
        let mut report = RecoveryReport::default();
        let paths = reader::list_segment_paths(dir)?;
        for path in paths {
            let segment_report = recover_file(&path)?;
            if segment_report.last_valid_sequence != 0 {
                report.last_valid_sequence = segment_report.last_valid_sequence;
            }
            report.truncated_bytes += segment_report.truncated_bytes;
        }
        Ok(report)
    }

    pub fn recover_path(path: &Path) -> Result<RecoveryReport, JournalError> {
        if path.is_dir() {
            return Self::recover_dir(path);
        }
        recover_file(path)
    }
}

fn recover_file(path: &Path) -> Result<RecoveryReport, JournalError> {
    let data = std::fs::read(path)?;
    let mut pos = 0usize;
    let mut last_valid_offset = 0usize;
    let mut last_valid_sequence = 0u64;

    while pos < data.len() {
        match codec::decode_record(&data[pos..]) {
            Ok((sequence, _cmd, used)) => {
                pos += used;
                last_valid_offset = pos;
                last_valid_sequence = sequence;
            }
            Err(JournalError::InvalidData | JournalError::CrcMismatch) => break,
            Err(e) => return Err(e),
        }
    }

    let truncated_bytes = (data.len() - last_valid_offset) as u64;
    if truncated_bytes > 0 {
        let file = OpenOptions::new().write(true).open(path)?;
        file.set_len(last_valid_offset as u64)?;
    }

    Ok(RecoveryReport {
        last_valid_sequence,
        truncated_bytes,
    })
}
