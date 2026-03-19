use crate::{JournalEntry, JournalError, codec, reader::list_segment_paths};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

const BUF_SIZE: usize = 64 * 1024; // 64KB

/// Streaming journal reader that processes records one at a time
/// without loading entire files into memory.
pub struct StreamingReader {
    paths: Vec<PathBuf>,
    current_segment: usize,
    reader: Option<BufReader<std::fs::File>>,
    header_buf: [u8; 4],
}

impl StreamingReader {
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let paths = if path.is_dir() {
            list_segment_paths(path)?
        } else {
            vec![path.to_path_buf()]
        };
        let mut s = Self {
            paths,
            current_segment: 0,
            reader: None,
            header_buf: [0u8; 4],
        };
        s.open_next_segment()?;
        Ok(s)
    }

    fn open_next_segment(&mut self) -> Result<bool, JournalError> {
        if self.current_segment >= self.paths.len() {
            self.reader = None;
            return Ok(false);
        }
        let file = std::fs::File::open(&self.paths[self.current_segment])?;
        self.reader = Some(BufReader::with_capacity(BUF_SIZE, file));
        self.current_segment += 1;
        Ok(true)
    }

    /// Read the next journal entry. Returns None when all segments are exhausted.
    pub fn next_entry(&mut self) -> Result<Option<JournalEntry>, JournalError> {
        loop {
            let Some(reader) = &mut self.reader else {
                return Ok(None);
            };

            // Try reading 4-byte length header.
            match reader.read_exact(&mut self.header_buf) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // End of this segment — try next.
                    if !self.open_next_segment()? {
                        return Ok(None);
                    }
                    continue;
                }
                Err(e) => return Err(JournalError::Io(e)),
            }

            let payload_len = u32::from_le_bytes(self.header_buf) as usize;
            // Read sequence (8) + payload + CRC (4).
            let body_len = 8 + payload_len + 4;
            let mut body = vec![0u8; body_len];
            reader
                .read_exact(&mut body)
                .map_err(|_| JournalError::InvalidData)?;

            // Reconstruct the full record for decode_record.
            let mut full_record = Vec::with_capacity(4 + body_len);
            full_record.extend_from_slice(&self.header_buf);
            full_record.extend_from_slice(&body);

            let (sequence, command, _) = codec::decode_record(&full_record)?;
            return Ok(Some(JournalEntry { sequence, command }));
        }
    }
}
