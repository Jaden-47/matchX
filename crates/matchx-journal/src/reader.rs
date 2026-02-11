use std::fs::File;
use std::io::Read;
use std::path::Path;
use matchx_types::Command;
use crate::{JournalError, codec};

/// A single decoded journal record.
pub struct JournalEntry {
    pub sequence: u64,
    pub command: Command,
}

/// Sequential reader over a binary journal file.
pub struct JournalReader {
    file: File,
}

impl JournalReader {
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let file = File::open(path)?;
        Ok(Self { file })
    }

    /// Read and validate every record in the file.
    /// Returns `Err(JournalError::CrcMismatch)` if any record is corrupt.
    pub fn read_all(&mut self) -> Result<Vec<JournalEntry>, JournalError> {
        let mut data = Vec::new();
        self.file.read_to_end(&mut data)?;

        let mut pos = 0;
        let mut entries = Vec::new();

        while pos < data.len() {
            // payload_len
            if pos + 4 > data.len() {
                return Err(JournalError::InvalidData);
            }
            let payload_len =
                u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap()) as usize;
            pos += 4;

            // sequence
            if pos + 8 > data.len() {
                return Err(JournalError::InvalidData);
            }
            let sequence =
                u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
            pos += 8;

            // payload
            if pos + payload_len > data.len() {
                return Err(JournalError::InvalidData);
            }
            let payload = &data[pos..pos + payload_len];
            pos += payload_len;

            // stored crc32
            if pos + 4 > data.len() {
                return Err(JournalError::InvalidData);
            }
            let stored_crc =
                u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
            pos += 4;

            // Recompute CRC over sequence_bytes ++ payload_bytes
            let mut crc_input = Vec::with_capacity(8 + payload_len);
            crc_input.extend_from_slice(&sequence.to_le_bytes());
            crc_input.extend_from_slice(payload);
            let computed_crc = crc32fast::hash(&crc_input);

            if computed_crc != stored_crc {
                return Err(JournalError::CrcMismatch);
            }

            let command = codec::decode(payload)?;
            entries.push(JournalEntry { sequence, command });
        }

        Ok(entries)
    }
}
