use matchx_engine::MatchingEngine;
use matchx_types::*;
use proptest::prelude::*;

fn test_config() -> InstrumentConfig {
    InstrumentConfig {
        id: 1,
        tick_size: 1,
        lot_size: 1,
        base_price: 0,
        max_ticks: 1000,
        stp_mode: StpMode::CancelNewest,
    }
}

proptest! {
    #[test]
    fn bbo_never_crosses(
        prices in prop::collection::vec(1u64..999, 1..50),
        sides in prop::collection::vec(prop::bool::ANY, 1..50),
        qtys in prop::collection::vec(1u64..100, 1..50),
    ) {
        let mut engine = MatchingEngine::new(test_config(), 4096);
        let len = prices.len().min(sides.len()).min(qtys.len());

        for i in 0..len {
            let side = if sides[i] { Side::Bid } else { Side::Ask };
            engine.process(Command::NewOrder {
                id: OrderId(i as u64 + 1),
                instrument_id: 1,
                side,
                price: prices[i],
                qty: qtys[i],
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            });

            // Invariant: best bid < best ask (if both exist)
            if let (Some(bid), Some(ask)) = (engine.best_bid(), engine.best_ask()) {
                prop_assert!(bid < ask,
                    "BBO crossed: bid={} >= ask={} after order {}", bid, ask, i);
            }
        }
    }

    #[test]
    fn fill_quantity_conserved(
        ask_qty in 1u64..100,
        bid_qty in 1u64..100,
    ) {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: ask_qty,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events: Vec<MatchEvent> = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: bid_qty,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        }).to_vec();

        let total_filled: u64 = events.iter()
            .filter_map(|e| match e {
                MatchEvent::Fill { qty, .. } => Some(*qty),
                _ => None,
            })
            .sum();

        let expected = ask_qty.min(bid_qty);
        prop_assert_eq!(total_filled, expected,
            "Fill quantity mismatch: got {} expected {}", total_filled, expected);
    }

    #[test]
    fn deterministic_replay(
        prices in prop::collection::vec(1u64..999, 1..30),
        sides in prop::collection::vec(prop::bool::ANY, 1..30),
        qtys in prop::collection::vec(1u64..100, 1..30),
    ) {
        let len = prices.len().min(sides.len()).min(qtys.len());
        let commands: Vec<Command> = (0..len).map(|i| {
            Command::NewOrder {
                id: OrderId(i as u64 + 1),
                instrument_id: 1,
                side: if sides[i] { Side::Bid } else { Side::Ask },
                price: prices[i],
                qty: qtys[i],
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            }
        }).collect();

        let run = |cmds: &[Command]| -> Vec<Vec<MatchEvent>> {
            let mut engine = MatchingEngine::new(test_config(), 4096);
            let mut results = Vec::new();
            for c in cmds {
                results.push(engine.process(c.clone()).to_vec());
            }
            results
        };

        let run1 = run(&commands);
        let run2 = run(&commands);
        prop_assert_eq!(run1, run2, "Non-deterministic: different outputs for same input");
    }
}
