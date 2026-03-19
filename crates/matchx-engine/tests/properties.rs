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

fn arb_side() -> impl Strategy<Value = Side> {
    prop::bool::ANY.prop_map(|b| if b { Side::Bid } else { Side::Ask })
}

fn arb_tif() -> impl Strategy<Value = TimeInForce> {
    prop_oneof![
        Just(TimeInForce::GTC),
        Just(TimeInForce::IOC),
        Just(TimeInForce::FOK),
    ]
}

fn arb_order_type() -> impl Strategy<Value = OrderType> {
    prop_oneof![Just(OrderType::Limit), Just(OrderType::Market),]
}

fn arb_stp_group() -> impl Strategy<Value = Option<u32>> {
    prop_oneof![Just(None), (1u32..4).prop_map(Some),]
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

            if let (Some(bid), Some(ask)) = (engine.best_bid(), engine.best_ask()) {
                prop_assert!(bid < ask,
                    "BBO crossed: bid={} >= ask={} after order {}", bid, ask, i);
            }
        }
    }

    #[test]
    fn bbo_never_crosses_mixed_order_types(
        prices in prop::collection::vec(1u64..999, 1..40),
        sides in prop::collection::vec(arb_side(), 1..40),
        qtys in prop::collection::vec(1u64..100, 1..40),
        tifs in prop::collection::vec(arb_tif(), 1..40),
        order_types in prop::collection::vec(arb_order_type(), 1..40),
    ) {
        let mut engine = MatchingEngine::new(test_config(), 4096);
        let len = prices.len().min(sides.len()).min(qtys.len()).min(tifs.len()).min(order_types.len());

        for i in 0..len {
            engine.process(Command::NewOrder {
                id: OrderId(i as u64 + 1),
                instrument_id: 1,
                side: sides[i],
                price: prices[i],
                qty: qtys[i],
                order_type: order_types[i],
                time_in_force: tifs[i],
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            });

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
    fn stp_never_produces_self_trade_fill(
        prices in prop::collection::vec(1u64..999, 1..30),
        sides in prop::collection::vec(arb_side(), 1..30),
        qtys in prop::collection::vec(1u64..100, 1..30),
        stp_groups in prop::collection::vec(arb_stp_group(), 1..30),
    ) {
        let mut engine = MatchingEngine::new(test_config(), 4096);
        let len = prices.len().min(sides.len()).min(qtys.len()).min(stp_groups.len());

        // Track which order IDs belong to which STP group
        let mut order_stp: std::collections::HashMap<OrderId, u32> = std::collections::HashMap::new();

        for i in 0..len {
            let order_id = OrderId(i as u64 + 1);
            if let Some(group) = stp_groups[i] {
                order_stp.insert(order_id, group);
            }

            let events: Vec<MatchEvent> = engine.process(Command::NewOrder {
                id: order_id,
                instrument_id: 1,
                side: sides[i],
                price: prices[i],
                qty: qtys[i],
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: stp_groups[i],
            }).to_vec();

            // Check that no fill occurs between two orders in the same STP group
            for event in &events {
                if let MatchEvent::Fill { maker_id, taker_id, .. } = event {
                    let maker_group = order_stp.get(maker_id);
                    let taker_group = order_stp.get(taker_id);
                    if let (Some(mg), Some(tg)) = (maker_group, taker_group) {
                        prop_assert!(mg != tg,
                            "Self-trade fill between maker {:?} and taker {:?} in STP group {}",
                            maker_id, taker_id, mg);
                    }
                }
            }
        }
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
