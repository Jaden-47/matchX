use matchx_bench::EndToEndPipeline;
use matchx_engine::MatchingEngine;
use matchx_journal::{AsyncJournalConfig, JournalReader, RecoveryManager};
use matchx_types::*;
use std::time::{Duration, Instant};

#[test]
fn replay_matches_original_output_with_async_wal() {
    let dir = tempfile::tempdir().unwrap();
    let prefix = dir.path().join("async-journal");
    let commands = command_stream();

    let mut pipeline = EndToEndPipeline::new(config(), 1024, journal_cfg(), &prefix).unwrap();
    let mut original_outputs = Vec::with_capacity(commands.len());
    for cmd in &commands {
        original_outputs.push(pipeline.process(cmd.clone()).unwrap());
    }

    wait_until(
        || pipeline.durable_sequence() >= commands.len() as u64,
        Duration::from_millis(500),
    );
    assert_eq!(pipeline.accepted_sequence(), commands.len() as u64);

    drop(pipeline);

    let wal_path = prefix.with_extension("wal");
    let report = RecoveryManager::recover_path(&wal_path).unwrap();
    assert_eq!(report.last_valid_sequence, commands.len() as u64);

    let mut reader = JournalReader::open(&wal_path).unwrap();
    let entries = reader.read_all().unwrap();

    let mut replay_engine = MatchingEngine::new(config(), 1024);
    let replay_outputs: Vec<Vec<MatchEvent>> = entries
        .into_iter()
        .map(|entry| replay_engine.process(entry.command).to_vec())
        .collect();

    assert_eq!(original_outputs, replay_outputs);
}

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

fn journal_cfg() -> AsyncJournalConfig {
    AsyncJournalConfig {
        queue_capacity: 128,
        batch_size: 32,
        flush_interval_ms: 1,
        segment_max_bytes: 1 << 20,
    }
}

fn command_stream() -> Vec<Command> {
    vec![
        Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 50,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        },
        Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 30,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        },
        Command::CancelOrder { id: OrderId(1) },
    ]
}

fn wait_until(mut condition: impl FnMut() -> bool, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if condition() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("condition not met within {:?}", timeout);
}
