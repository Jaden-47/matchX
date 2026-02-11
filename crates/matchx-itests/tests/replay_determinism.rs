//! End-to-end test: write commands to journal, replay, verify identical output.

use matchx_engine::MatchingEngine;
use matchx_journal::{JournalWriter, JournalReader};
use matchx_types::*;

fn config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1,
        tick_size: 1,
        lot_size: 1,
        base_price: 0,
        max_ticks: 1000,
        stp_mode: StpMode::CancelNewest,
    }
}

#[test]
fn replay_produces_identical_output() {
    let commands = vec![
        Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 50,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        },
        Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 30,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        },
        Command::CancelOrder { id: OrderId(1) },
    ];

    // Run 1: process commands and write to journal.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.journal");
    let mut engine1 = MatchingEngine::new(config(), 1024);
    let mut writer = JournalWriter::open(&path).unwrap();
    let mut outputs1: Vec<Vec<MatchEvent>> = Vec::new();

    for (i, cmd) in commands.iter().enumerate() {
        writer.append(i as u64 + 1, cmd).unwrap();
        outputs1.push(engine1.process(cmd.clone()).to_vec());
    }
    drop(writer);

    // Run 2: replay from journal.
    let mut engine2 = MatchingEngine::new(config(), 1024);
    let mut reader = JournalReader::open(&path).unwrap();
    let entries = reader.read_all().unwrap();
    let mut outputs2: Vec<Vec<MatchEvent>> = Vec::new();

    for entry in &entries {
        outputs2.push(engine2.process(entry.command.clone()).to_vec());
    }

    assert_eq!(outputs1, outputs2, "Replay output diverged from original");
}
