use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use matchx_types::Command;
use crate::{JournalError, codec};

/// Append-only binary journal writer.
///
/// Record layout (all little-endian):
///   [u32 payload_len][u64 sequence][payload_bytes...][u32 crc32]
///
/// CRC32 is computed over `sequence_bytes ++ payload_bytes`.
pub struct JournalWriter {
    file: File,
}

impl JournalWriter {
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self { file })
    }

    pub fn append(&mut self, sequence: u64, cmd: &Command) -> Result<(), JournalError> {
        let payload = codec::encode(cmd);
        let payload_len = payload.len() as u32;

        // CRC covers sequence bytes followed by the payload bytes.
        let mut crc_input = Vec::with_capacity(8 + payload.len());
        crc_input.extend_from_slice(&sequence.to_le_bytes());
        crc_input.extend_from_slice(&payload);
        let crc = crc32fast::hash(&crc_input);

        self.file.write_all(&payload_len.to_le_bytes())?;
        self.file.write_all(&sequence.to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.file.write_all(&crc.to_le_bytes())?;
        self.file.flush()?;
        Ok(())
    }
}
