use criterion::{black_box, criterion_group, criterion_main, Criterion};
use matchx_engine::MatchingEngine;
use matchx_types::*;

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

fn bench_insert_limit_order(c: &mut Criterion) {
    c.bench_function("insert_limit_order", |b| {
        let mut engine = MatchingEngine::new(config(), 65536);
        let mut id = 1u64;
        b.iter(|| {
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
        });
    });
}

fn bench_crossing_trade(c: &mut Criterion) {
    c.bench_function("crossing_trade", |b| {
        b.iter_custom(|iters| {
            let mut engine = MatchingEngine::new(config(), 65536);
            // Pre-populate asks across 100 price levels.
            for i in 0u64..1000 {
                engine.process(Command::NewOrder {
                    id: OrderId(i + 1),
                    instrument_id: 1,
                    side: Side::Ask,
                    price: 5000 + (i % 100),
                    qty: 10,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::GTC,
                    visible_qty: None,
                    stop_price: None,
                    stp_group: None,
                });
            }
            let start = std::time::Instant::now();
            for i in 0..iters {
                engine.process(black_box(Command::NewOrder {
                    id: OrderId(10000 + i),
                    instrument_id: 1,
                    side: Side::Bid,
                    price: 5000,
                    qty: 1,
                    order_type: OrderType::Limit,
                    time_in_force: TimeInForce::GTC,
                    visible_qty: None,
                    stop_price: None,
                    stp_group: None,
                }));
            }
            start.elapsed()
        });
    });
}

fn bench_cancel_order(c: &mut Criterion) {
    c.bench_function("cancel_order", |b| {
        let mut engine = MatchingEngine::new(config(), 65536);
        // Pre-populate 10 000 resting bids spread across 1 000 price levels.
        for i in 0u64..10000 {
            engine.process(Command::NewOrder {
                id: OrderId(i + 1),
                instrument_id: 1,
                side: Side::Bid,
                price: 4000 + (i % 1000),
                qty: 10,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            });
        }
        let mut cancel_id = 1u64;
        b.iter(|| {
            engine.process(black_box(Command::CancelOrder {
                id: OrderId(cancel_id),
            }));
            cancel_id += 1;
        });
    });
}

criterion_group!(benches, bench_insert_limit_order, bench_crossing_trade, bench_cancel_order);
criterion_main!(benches);
