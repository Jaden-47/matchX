use matchx_journal::{JournalReader, JournalWriter, RecoveryManager};
use matchx_types::{Command, OrderId};

#[test]
fn recovery_truncates_to_last_valid_record_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.bin");
    write_n_records(&path, 10);
    corrupt_tail_bytes(&path);

    let report = RecoveryManager::recover_path(&path).unwrap();
    assert_eq!(report.last_valid_sequence, 9);
    assert!(report.truncated_bytes > 0);

    let mut reader = JournalReader::open(&path).unwrap();
    let entries = reader.read_all().unwrap();
    assert_eq!(entries.len(), 9);
}

fn write_n_records(path: &std::path::Path, count: u64) {
    let mut writer = JournalWriter::open(path).unwrap();
    for seq in 1..=count {
        writer
            .append(
                seq,
                &Command::CancelOrder {
                    id: OrderId(1000 + seq),
                },
            )
            .unwrap();
    }
}

fn corrupt_tail_bytes(path: &std::path::Path) {
    let mut bytes = std::fs::read(path).unwrap();
    bytes.truncate(bytes.len() - 3);
    std::fs::write(path, bytes).unwrap();
}
