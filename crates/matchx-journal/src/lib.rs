mod codec;
mod writer;
mod reader;

pub use writer::JournalWriter;
pub use reader::{JournalReader, JournalEntry};

/// Errors produced by journal operations.
#[derive(Debug)]
pub enum JournalError {
    Io(std::io::Error),
    CrcMismatch,
    InvalidData,
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

    #[test]
    fn write_and_read_back_commands() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.bin");

        let cmd1 = Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
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
}
