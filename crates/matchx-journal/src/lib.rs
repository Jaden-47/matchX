mod async_journal;
mod codec;
mod reader;
mod recovery;
mod streaming_reader;
mod writer;

pub use async_journal::{AsyncJournal, AsyncJournalConfig};
pub use reader::{JournalEntry, JournalReader};
pub use recovery::{RecoveryManager, RecoveryReport};
pub use streaming_reader::StreamingReader;
pub use writer::JournalWriter;

/// Errors produced by journal operations.
#[derive(Debug)]
pub enum JournalError {
    Io(std::io::Error),
    CrcMismatch,
    InvalidData,
    QueueFull,
    WriterStopped,
    WriterDegraded,
}

impl From<std::io::Error> for JournalError {
    fn from(e: std::io::Error) -> Self {
        JournalError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn write_and_read_back_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");

        let cmd1 = Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        };
        let cmd2 = Command::CancelOrder { id: OrderId(1) };

        {
            let mut writer = JournalWriter::open(&path).unwrap();
            writer.append(1, &cmd1).unwrap();
            writer.append(2, &cmd2).unwrap();
        }

        let mut reader = JournalReader::open(&path).unwrap();
        let entries: Vec<_> = reader.read_all().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].sequence, 1);
        assert_eq!(entries[1].sequence, 2);
    }

    #[test]
    fn crc_detects_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");

        let cmd = Command::CancelOrder { id: OrderId(42) };

        {
            let mut writer = JournalWriter::open(&path).unwrap();
            writer.append(1, &cmd).unwrap();
        }

        // Corrupt a byte in the sequence field (byte 10 = offset 6 of the u64 sequence)
        let mut data = std::fs::read(&path).unwrap();
        data[10] ^= 0xFF;
        std::fs::write(&path, &data).unwrap();

        let mut reader = JournalReader::open(&path).unwrap();
        assert!(reader.read_all().is_err());
    }

    #[test]
    fn rotates_segments_when_max_bytes_exceeded() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open_segmented(dir.path(), 256).unwrap();
        for seq in 1..=200 {
            writer.append(seq, &cmd()).unwrap();
        }

        let segments = list_segments(dir.path());
        assert!(
            segments.len() > 1,
            "expected >1 segment, got {}",
            segments.len()
        );
    }

    #[test]
    fn reader_reads_across_rotated_segments_in_sequence_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open_segmented(dir.path(), 128).unwrap();
        for seq in 1..=32 {
            writer.append(seq, &cancel_cmd(seq)).unwrap();
        }
        drop(writer);

        let mut reader = JournalReader::open(dir.path()).unwrap();
        let entries = reader.read_all().unwrap();
        assert_eq!(entries.len(), 32);
        for (i, entry) in entries.iter().enumerate() {
            assert_eq!(entry.sequence, i as u64 + 1);
        }
    }

    fn list_segments(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| name.starts_with("journal-") && name.ends_with(".wal"))
                    .unwrap_or(false)
            })
            .collect();
        out.sort();
        out
    }

    fn cmd() -> Command {
        Command::CancelOrder { id: OrderId(42) }
    }

    fn cancel_cmd(id: u64) -> Command {
        Command::CancelOrder { id: OrderId(id) }
    }

    #[test]
    fn streaming_reader_matches_batch_reader() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = JournalWriter::open_segmented(dir.path(), 128).unwrap();
        for seq in 1..=20 {
            writer.append(seq, &cancel_cmd(seq)).unwrap();
        }
        drop(writer);

        // Batch reader
        let mut batch = JournalReader::open(dir.path()).unwrap();
        let batch_entries = batch.read_all().unwrap();

        // Streaming reader
        let mut streaming = StreamingReader::open(dir.path()).unwrap();
        let mut streaming_entries = Vec::new();
        while let Some(entry) = streaming.next_entry().unwrap() {
            streaming_entries.push(entry);
        }

        assert_eq!(batch_entries.len(), streaming_entries.len());
        for (b, s) in batch_entries.iter().zip(streaming_entries.iter()) {
            assert_eq!(b.sequence, s.sequence);
        }
    }
}
