use criterion::{Criterion, black_box, criterion_group, criterion_main};
use matchx_bench::{EndToEndPipeline, LatencySummary};
use matchx_engine::MatchingEngine;
use matchx_journal::{AsyncJournalConfig, JournalError};
use matchx_types::*;
use std::time::Instant;

fn config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1,
        tick_size: 1,
        lot_size: 1,
        base_price: 0,
        max_ticks: 10_000,
        stp_mode: StpMode::CancelNewest,
    }
}

fn journal_cfg() -> AsyncJournalConfig {
    AsyncJournalConfig {
        queue_capacity: 8192,
        batch_size: 128,
        flush_interval_ms: 1,
        segment_max_bytes: 8 * 1024 * 1024,
    }
}

fn bench_core_process_only(c: &mut Criterion) {
    c.bench_function("core_process_only", |b| {
        b.iter_custom(|iters| {
            let mut engine = MatchingEngine::new(config(), 65_536);
            let mut order_id = 1_u64;
            let stride = sample_stride(iters);
            let mut samples = Vec::new();

            let started = Instant::now();
            for i in 0..iters {
                let cmd = cancel_cmd(order_id);
                order_id += 1;

                let maybe_start = (i % stride == 0).then(Instant::now);
                let events = engine.process(black_box(cmd));
                black_box(events.len());

                if let Some(t0) = maybe_start {
                    samples.push(t0.elapsed().as_nanos() as u64);
                }
            }
            let elapsed = started.elapsed();
            println!(
                "[bench] core_process_only: {} (samples={})",
                LatencySummary::from_samples(&samples),
                samples.len()
            );
            elapsed
        });
    });
}

fn bench_end_to_end_process_plus_enqueue(c: &mut Criterion) {
    c.bench_function("end_to_end_process_plus_enqueue", |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::tempdir().unwrap();
            let prefix = dir.path().join("e2e");
            let mut pipeline = EndToEndPipeline::new(config(), 65_536, journal_cfg(), &prefix)
                .expect("pipeline init");
            let mut order_id = 1_u64;
            let stride = sample_stride(iters);
            let mut samples = Vec::new();

            let started = Instant::now();
            for i in 0..iters {
                let cmd = cancel_cmd(order_id);
                order_id += 1;

                let maybe_start = (i % stride == 0).then(Instant::now);
                let events = process_with_retry(&mut pipeline, cmd);
                black_box(events.len());

                if let Some(t0) = maybe_start {
                    samples.push(t0.elapsed().as_nanos() as u64);
                }
            }
            let elapsed = started.elapsed();
            println!(
                "[bench] end_to_end_process_plus_enqueue: {} (samples={})",
                LatencySummary::from_samples(&samples),
                samples.len()
            );
            elapsed
        });
    });
}

fn bench_durability_lag_under_load(c: &mut Criterion) {
    c.bench_function("durability_lag_under_load", |b| {
        b.iter_custom(|iters| {
            let dir = tempfile::tempdir().unwrap();
            let prefix = dir.path().join("lag");
            let mut pipeline = EndToEndPipeline::new(config(), 65_536, journal_cfg(), &prefix)
                .expect("pipeline init");
            let mut order_id = 1_u64;
            let stride = sample_stride(iters);
            let mut lag_samples = Vec::new();

            let started = Instant::now();
            for i in 0..iters {
                let _events = process_with_retry(&mut pipeline, cancel_cmd(order_id));
                order_id += 1;

                if i % stride == 0 {
                    lag_samples.push(
                        pipeline
                            .accepted_sequence()
                            .saturating_sub(pipeline.durable_sequence()),
                    );
                }
            }
            let elapsed = started.elapsed();
            println!(
                "[bench] durability_lag_under_load: {} (samples={})",
                LatencySummary::from_samples(&lag_samples),
                lag_samples.len()
            );
            elapsed
        });
    });
}

fn process_with_retry(pipeline: &mut EndToEndPipeline, cmd: Command) -> Vec<MatchEvent> {
    loop {
        match pipeline.process(cmd.clone()) {
            Ok(events) => return events,
            Err(JournalError::QueueFull) => std::thread::yield_now(),
            Err(err) => panic!("pipeline process failed: {err:?}"),
        }
    }
}

fn cancel_cmd(id: u64) -> Command {
    Command::CancelOrder { id: OrderId(id) }
}

fn sample_stride(iters: u64) -> u64 {
    (iters / 10_000).max(1)
}

criterion_group!(
    benches,
    bench_core_process_only,
    bench_end_to_end_process_plus_enqueue,
    bench_durability_lag_under_load
);
criterion_main!(benches);
