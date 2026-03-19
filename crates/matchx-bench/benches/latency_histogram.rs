//! Latency histogram benchmark for the matching engine hot path.
//!
//! Run with:
//!   cargo bench --bench latency_histogram -- --nocapture
//!
//! Reports p50, p99, p99.9, p99.99, and max latency in nanoseconds.
//! Primary purpose: verify sub-microsecond p99 target on bare metal
//! with CPU isolation active (see scripts/setup-cpu-isolation.sh).

use hdrhistogram::Histogram;
use matchx_engine::MatchingEngine;
use matchx_types::*;
use std::hint::black_box;
use std::time::Instant;

fn config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1,
        tick_size: 1,
        lot_size: 1,
        base_price: 0,
        max_ticks: 10000,
        stp_mode: StpMode::CancelNewest,
    }
}

/// Measures per-call latency of inserting a non-crossing limit order.
fn bench_insert_latency(iters: u64) -> Histogram<u64> {
    let mut hist =
        Histogram::<u64>::new_with_bounds(1, 10_000_000, 3).expect("valid histogram bounds");
    let mut engine = MatchingEngine::new(config(), 65536);
    let mut id = 1u64;

    // Warm up: 10k orders to fill instruction/branch-predictor caches
    for _ in 0..10_000 {
        let side = if id % 2 == 0 { Side::Bid } else { Side::Ask };
        let price = if side == Side::Bid {
            4900 + (id % 100)
        } else {
            5100 + (id % 100)
        };
        engine.process(black_box(Command::NewOrder {
            id: OrderId(id),
            instrument_id: 1,
            side,
            price,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        }));
        id += 1;
    }

    // Reset engine to avoid arena exhaustion
    let mut engine = MatchingEngine::new(config(), 65536);
    id = 1;

    for _ in 0..iters {
        let side = if id % 2 == 0 { Side::Bid } else { Side::Ask };
        let price = if side == Side::Bid {
            4900 + (id % 100)
        } else {
            5100 + (id % 100)
        };
        let t0 = Instant::now();
        let _ = black_box(engine.process(black_box(Command::NewOrder {
            id: OrderId(id),
            instrument_id: 1,
            side,
            price,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        })));
        let elapsed_ns = t0.elapsed().as_nanos() as u64;
        let _ = hist.record(elapsed_ns.max(1));
        id += 1;
    }

    hist
}

fn print_histogram(label: &str, hist: &Histogram<u64>) {
    println!("\n=== {} ===", label);
    println!("  samples : {:>10}", hist.len());
    println!("  p50     : {:>10} ns", hist.value_at_quantile(0.50));
    println!(
        "  p99     : {:>10} ns  ← SLO target: < 1000 ns on bare metal with isolcpus",
        hist.value_at_quantile(0.99)
    );
    println!("  p99.9   : {:>10} ns", hist.value_at_quantile(0.999));
    println!("  p99.99  : {:>10} ns", hist.value_at_quantile(0.9999));
    println!("  max     : {:>10} ns", hist.max());
    println!("  mean    : {:>10.1} ns", hist.mean());
    println!("  stddev  : {:>10.1} ns", hist.stdev());
}

fn main() {
    let iters = 1_000_000u64;
    println!("Running latency histogram ({} iterations)...", iters);
    println!("Note: Run on bare metal with isolcpus for sub-µs p99 measurement.");

    let hist = bench_insert_latency(iters);
    print_histogram("insert_limit_order (non-crossing)", &hist);

    // Simple pass/fail against 10µs target (achievable even on dev machine/WSL2)
    let p99_ns = hist.value_at_quantile(0.99);
    if p99_ns < 10_000 {
        println!(
            "\n✓ p99 < 10µs ({} ns) — dev machine baseline acceptable",
            p99_ns
        );
    } else {
        println!(
            "\n⚠ p99 = {} ns — check for OS jitter or run on bare metal",
            p99_ns
        );
    }
}
