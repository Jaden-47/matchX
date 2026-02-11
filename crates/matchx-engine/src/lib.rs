#![cfg_attr(not(test), no_std)]
extern crate alloc;

pub mod policy;

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use matchx_arena::Arena;
use matchx_book::OrderBook;
use matchx_types::*;
use policy::{PriceTimeFifo, MatchPolicy};

/// Pending stop-limit order waiting for a trade price trigger.
struct StopEntry {
    id: OrderId,
    side: Side,
    limit_price: u64,
    qty: u64,
    time_in_force: TimeInForce,
    visible_qty: Option<u64>,
    stp_group: Option<u32>,
}

pub struct MatchingEngine {
    book: OrderBook,
    arena: Arena,
    policy: PriceTimeFifo,
    config: InstrumentConfig,
    sequence: u64,
    timestamp_ns: u64,
    event_buffer: Vec<MatchEvent>,
    /// Buy stops keyed by stop price; trigger when last_trade_price >= stop_price.
    stop_bids: BTreeMap<u64, VecDeque<StopEntry>>,
    /// Sell stops keyed by stop price; trigger when last_trade_price <= stop_price.
    stop_asks: BTreeMap<u64, VecDeque<StopEntry>>,
    /// Last fill price, used as the stop trigger source.
    last_trade_price: Option<u64>,
}

impl MatchingEngine {
    pub fn new(config: InstrumentConfig, arena_capacity: u32) -> Self {
        Self {
            book: OrderBook::new(config.clone()),
            arena: Arena::new(arena_capacity),
            policy: PriceTimeFifo,
            config,
            sequence: 0,
            timestamp_ns: 0,
            event_buffer: Vec::with_capacity(64),
            stop_bids: BTreeMap::new(),
            stop_asks: BTreeMap::new(),
            last_trade_price: None,
        }
    }

    /// Emit an event with a monotonically increasing logical clock.
    #[inline]
    fn emit(&mut self, event_fn: impl FnOnce(EventMeta) -> MatchEvent) {
        self.sequence += 1;
        self.timestamp_ns += 1;
        let meta = EventMeta {
            sequence: self.sequence,
            timestamp_ns: self.timestamp_ns,
        };
        self.event_buffer.push(event_fn(meta));
    }

    /// Process a command and return the emitted events.
    /// The returned slice is reused on each call; copy if needed across calls.
    pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
        self.event_buffer.clear();
        match cmd {
            Command::NewOrder {
                id, side, price, qty, order_type, time_in_force,
                visible_qty, stop_price, stp_group, ..
            } => {
                self.process_new_order(
                    id, side, price, qty, order_type, time_in_force,
                    visible_qty, stop_price, stp_group,
                );
            }
            Command::CancelOrder { id } => {
                self.process_cancel(id);
            }
            Command::ModifyOrder { id, new_price, new_qty } => {
                self.process_modify(id, new_price, new_qty);
            }
        }
        self.drain_stop_triggers();
        &self.event_buffer
    }

    fn process_new_order(
        &mut self,
        id: OrderId,
        side: Side,
        price: u64,
        mut qty: u64,
        order_type: OrderType,
        time_in_force: TimeInForce,
        visible_qty: Option<u64>,
        stop_price: Option<u64>,
        stp_group: Option<u32>,
    ) {
        // --- Post-Only: reject if would cross, otherwise rest directly (no matching) ---
        if order_type == OrderType::PostOnly {
            let would_cross = match side {
                Side::Bid => self.book.best_ask().is_some_and(|ask| price >= ask),
                Side::Ask => self.book.best_bid().is_some_and(|bid| price <= bid),
            };
            if would_cross {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta, id, reason: RejectReason::WouldCrossSpread,
                });
                return;
            }
            self.emit(|meta| MatchEvent::OrderAccepted { meta, id, side, price, qty, order_type });
            self.book.insert_order(id, side, price, qty, order_type, visible_qty, stp_group, &mut self.arena);
            self.emit(|meta| MatchEvent::BookUpdate { meta, side, price, qty });
            return;
        }

        // --- Stop-Limit: accept and queue, defer activation until stop price triggers ---
        if order_type == OrderType::StopLimit {
            if qty == 0 {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta, id, reason: RejectReason::InvalidQuantity,
                });
                return;
            }
            let Some(stop_px) = stop_price else {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta, id, reason: RejectReason::InvalidPrice,
                });
                return;
            };
            self.emit(|meta| MatchEvent::OrderAccepted { meta, id, side, price, qty, order_type });
            let entry = StopEntry { id, side, limit_price: price, qty, time_in_force, visible_qty, stp_group };
            match side {
                Side::Bid => self.stop_bids.entry(stop_px).or_default().push_back(entry),
                Side::Ask => self.stop_asks.entry(stop_px).or_default().push_back(entry),
            }
            return;
        }

        // --- Validation ---
        if qty == 0 {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::InvalidQuantity,
            });
            return;
        }
        if self.book.lookup(id).is_some() {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::DuplicateOrderId,
            });
            return;
        }

        // --- Effective price for market orders (needed before STP and FOK checks) ---
        let effective_price = match order_type {
            OrderType::Market => match side {
                Side::Bid => u64::MAX,
                Side::Ask => 0,
            },
            _ => price,
        };

        // --- FOK pre-check (O(log N) dense + O(log N) sparse) ---
        if time_in_force == TimeInForce::FOK {
            let available = self.check_available_liquidity(side, effective_price);
            if available < qty {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta, id, reason: RejectReason::InsufficientLiquidity,
                });
                return;
            }
        }

        // --- STP CancelNewest: reject before accepting if first maker matches group ---
        if let Some(taker_stp) = stp_group {
            if self.config.stp_mode == StpMode::CancelNewest
                && self.stp_first_maker_matches(side, effective_price, taker_stp)
            {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta, id, reason: RejectReason::SelfTradePreventionTriggered,
                });
                return;
            }
        }

        // --- Accept ---
        self.emit(|meta| MatchEvent::OrderAccepted {
            meta, id, side, price, qty, order_type,
        });

        // --- Match against the opposing side ---
        let cancel_incoming = self.match_against_book(id, side, effective_price, stp_group, &mut qty);

        // --- STP triggered: cancel incoming ---
        if cancel_incoming {
            self.emit(|meta| MatchEvent::OrderCancelled { meta, id, remaining_qty: qty });
            return;
        }

        // --- Handle remainder ---
        match (order_type, time_in_force) {
            // GTC Limit/Iceberg: rest the unfilled portion on the book.
            (OrderType::Limit | OrderType::Iceberg, TimeInForce::GTC) if qty > 0 => {
                self.book.insert_order(id, side, price, qty, order_type, visible_qty, stp_group, &mut self.arena);
                self.emit(|meta| MatchEvent::BookUpdate { meta, side, price, qty });
            }
            // Market, IOC, FOK: cancel any unfilled remainder.
            (OrderType::Market, _) | (_, TimeInForce::IOC) | (_, TimeInForce::FOK)
                if qty > 0 =>
            {
                self.emit(|meta| MatchEvent::OrderCancelled {
                    meta, id, remaining_qty: qty,
                });
            }
            _ => {}
        }
    }

    /// Returns `true` if the first maker at the best opposing crossable price
    /// shares `taker_stp` — used by CancelNewest pre-check before OrderAccepted.
    fn stp_first_maker_matches(&self, taker_side: Side, taker_price: u64, taker_stp: u32) -> bool {
        let best_opposing = match taker_side {
            Side::Bid => self.book.best_ask(),
            Side::Ask => self.book.best_bid(),
        };
        let Some(resting_price) = best_opposing else { return false };
        if !self.policy.is_price_acceptable(taker_side, taker_price, resting_price) {
            return false;
        }
        let head = match taker_side {
            Side::Bid => self.book.get_ask_level(resting_price).head,
            Side::Ask => self.book.get_bid_level(resting_price).head,
        };
        let Some(head_idx) = head else { return false };
        self.arena.get(head_idx).stp_group == Some(taker_stp)
    }

    /// Walk the opposing side of the book, filling the taker one maker at a time.
    /// Fully-filled makers are removed inline so the BBO is always current.
    /// Returns `true` if an STP action requires cancelling the incoming order.
    fn match_against_book(
        &mut self,
        taker_id: OrderId,
        taker_side: Side,
        taker_price: u64,
        taker_stp: Option<u32>,
        remaining: &mut u64,
    ) -> bool {
        while *remaining > 0 {
            // Best opposing price
            let best_price = match taker_side {
                Side::Bid => self.book.best_ask(),
                Side::Ask => self.book.best_bid(),
            };
            let Some(resting_price) = best_price else { break };

            if !self.policy.is_price_acceptable(taker_side, taker_price, resting_price) {
                break;
            }

            // Get the head of the FIFO queue at this level.
            let level_head = match taker_side {
                Side::Bid => self.book.get_ask_level(resting_price).head,
                Side::Ask => self.book.get_bid_level(resting_price).head,
            };
            let Some(maker_idx) = level_head else { break };

            // Compute fill quantity.
            let maker_remaining = self.arena.get(maker_idx).remaining();
            if maker_remaining == 0 {
                // Stale fully-filled maker — remove and continue.
                self.book.remove_order(maker_idx, &mut self.arena);
                continue;
            }
            // For Iceberg makers: limit fill to the current visible slice.
            let maker_matchable = self.arena.get(maker_idx).matchable_qty();
            let maker_id = self.arena.get(maker_idx).id;
            let maker_side = self.arena.get(maker_idx).side;
            let maker_stp = self.arena.get(maker_idx).stp_group;

            // --- Self-Trade Prevention ---
            if let (Some(ts), Some(ms)) = (taker_stp, maker_stp) {
                if ts == ms {
                    match self.config.stp_mode {
                        StpMode::CancelNewest => {
                            // Should have been caught by pre-check; stop matching silently.
                            break;
                        }
                        StpMode::CancelOldest => {
                            // Cancel the resting (oldest) order; incoming continues.
                            self.book.remove_order(maker_idx, &mut self.arena);
                            self.emit(|meta| MatchEvent::OrderCancelled {
                                meta, id: maker_id, remaining_qty: maker_remaining,
                            });
                            let level_qty = self.book.get_level_qty(maker_side, resting_price);
                            self.emit(|meta| MatchEvent::BookUpdate {
                                meta, side: maker_side, price: resting_price, qty: level_qty,
                            });
                            continue;
                        }
                        StpMode::CancelBoth => {
                            // Cancel resting maker, then signal to cancel incoming.
                            self.book.remove_order(maker_idx, &mut self.arena);
                            self.emit(|meta| MatchEvent::OrderCancelled {
                                meta, id: maker_id, remaining_qty: maker_remaining,
                            });
                            let level_qty = self.book.get_level_qty(maker_side, resting_price);
                            self.emit(|meta| MatchEvent::BookUpdate {
                                meta, side: maker_side, price: resting_price, qty: level_qty,
                            });
                            return true;
                        }
                        StpMode::DecrementAndCancel => {
                            // Reduce both by the overlap quantity; cancel incoming.
                            let overlap = (*remaining).min(maker_remaining);
                            if maker_remaining == overlap {
                                self.book.remove_order(maker_idx, &mut self.arena);
                            } else {
                                self.book.reduce_order_qty(maker_idx, overlap, &mut self.arena);
                            }
                            let level_qty = self.book.get_level_qty(maker_side, resting_price);
                            self.emit(|meta| MatchEvent::BookUpdate {
                                meta, side: maker_side, price: resting_price, qty: level_qty,
                            });
                            *remaining -= overlap;
                            return true;
                        }
                    }
                }
            }

            let fill_qty = (*remaining).min(maker_matchable);

            // Apply fill to maker.
            self.arena.get_mut(maker_idx).filled += fill_qty;

            // Update level total_quantity immediately (before removal check).
            match taker_side {
                Side::Bid => {
                    self.book.get_ask_level_mut(resting_price).total_quantity -= fill_qty;
                }
                Side::Ask => {
                    self.book.get_bid_level_mut(resting_price).total_quantity -= fill_qty;
                }
            }

            *remaining -= fill_qty;

            let new_maker_remaining = self.arena.get(maker_idx).remaining();
            let taker_remaining = *remaining;

            // Emit Fill event.
            self.emit(|meta| MatchEvent::Fill {
                meta,
                maker_id,
                taker_id,
                price: resting_price,
                qty: fill_qty,
                maker_remaining: new_maker_remaining,
                taker_remaining,
            });
            self.last_trade_price = Some(resting_price);

            // Emit BookUpdate for the maker's level.
            let level_qty = self.book.get_level_qty(maker_side, resting_price);
            self.emit(|meta| MatchEvent::BookUpdate {
                meta,
                side: maker_side,
                price: resting_price,
                qty: level_qty,
            });

            // Remove maker if fully filled (updates BBO).
            if new_maker_remaining == 0 {
                self.book.remove_order(maker_idx, &mut self.arena);
            }
        }
        false
    }

    /// Collect all stop entries whose stop price has been crossed by `last_trade_price`.
    /// Buy stops trigger when last_trade >= stop_price; sell stops when last_trade <= stop_price.
    fn collect_triggered_stops(&mut self) -> Vec<StopEntry> {
        let Some(last) = self.last_trade_price else { return Vec::new() };
        let mut result = Vec::new();

        // Buy stops: keyed by stop_price; trigger when last_trade >= key (range ..=last)
        let bid_keys: Vec<u64> = self.stop_bids.range(..=last).map(|(&k, _)| k).collect();
        for k in bid_keys {
            if let Some(queue) = self.stop_bids.get_mut(&k) {
                while let Some(entry) = queue.pop_front() {
                    result.push(entry);
                }
            }
            self.stop_bids.remove(&k);
        }

        // Sell stops: keyed by stop_price; trigger when last_trade <= key (range last..)
        let ask_keys: Vec<u64> = self.stop_asks.range(last..).map(|(&k, _)| k).collect();
        for k in ask_keys {
            if let Some(queue) = self.stop_asks.get_mut(&k) {
                while let Some(entry) = queue.pop_front() {
                    result.push(entry);
                }
            }
            self.stop_asks.remove(&k);
        }

        result
    }

    /// Fire all pending stops whose trigger price has been crossed, then process
    /// their triggered limit orders. Loops until no new stops are triggered.
    fn drain_stop_triggers(&mut self) {
        loop {
            let triggered = self.collect_triggered_stops();
            if triggered.is_empty() {
                break;
            }
            for entry in triggered {
                let stop_id = entry.id;
                let new_order_id = entry.id;
                self.emit(|meta| MatchEvent::StopTriggered { meta, stop_id, new_order_id });
                self.process_new_order(
                    entry.id,
                    entry.side,
                    entry.limit_price,
                    entry.qty,
                    OrderType::Limit,
                    entry.time_in_force,
                    entry.visible_qty,
                    None,
                    entry.stp_group,
                );
            }
        }
    }

    fn check_available_liquidity(&self, taker_side: Side, taker_price: u64) -> u64 {
        match taker_side {
            Side::Bid => self.book.ask_available_at_or_below(taker_price),
            Side::Ask => self.book.bid_available_at_or_above(taker_price),
        }
    }

    fn process_cancel(&mut self, id: OrderId) {
        if let Some(idx) = self.book.lookup(id) {
            let order = self.arena.get(idx);
            let remaining = order.remaining();
            let side = order.side;
            let price = order.price;
            // order borrow ends here (NLL)

            self.book.remove_order(idx, &mut self.arena);
            self.emit(|meta| MatchEvent::OrderCancelled {
                meta, id, remaining_qty: remaining,
            });
            let level_qty = self.book.get_level_qty(side, price);
            self.emit(|meta| MatchEvent::BookUpdate {
                meta, side, price, qty: level_qty,
            });
        } else {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::OrderNotFound,
            });
        }
    }

    fn process_modify(&mut self, id: OrderId, new_price: u64, new_qty: u64) {
        if let Some(idx) = self.book.lookup(id) {
            let order = self.arena.get(idx);
            let side = order.side;
            let old_price = order.price;
            // order borrow ends here

            self.book.remove_order(idx, &mut self.arena);
            self.emit(|meta| MatchEvent::OrderModified { meta, id, new_price, new_qty });
            let old_level_qty = self.book.get_level_qty(side, old_price);
            self.emit(|meta| MatchEvent::BookUpdate {
                meta, side, price: old_price, qty: old_level_qty,
            });
            // Route replacement through full new-order path (handles crossing).
            self.process_new_order(
                id, side, new_price, new_qty,
                OrderType::Limit, TimeInForce::GTC,
                None, None, None,
            );
        } else {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta, id, reason: RejectReason::OrderNotFound,
            });
        }
    }

    #[inline]
    pub fn best_bid(&self) -> Option<u64> {
        self.book.best_bid()
    }

    #[inline]
    pub fn best_ask(&self) -> Option<u64> {
        self.book.best_ask()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;

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

    // ---- Task 7: Basic limit order matching ----

    #[test]
    fn limit_buy_rests_on_empty_book() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        let events = engine.process(Command::NewOrder {
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
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(1), .. })));
        assert_eq!(engine.best_bid(), Some(100));
    }

    #[test]
    fn crossing_limit_orders_produce_fill() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::Fill { maker_id: OrderId(1), taker_id: OrderId(2), price: 100, qty: 5, .. }
        )));
    }

    #[test]
    fn partial_fill_remainder_rests() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
        assert_eq!(engine.best_bid(), Some(100)); // remainder rests
        assert_eq!(engine.best_ask(), None);      // ask fully filled
    }

    #[test]
    fn taker_sweeps_multiple_price_levels_in_one_call() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 101, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(3), instrument_id: 1, side: Side::Bid, price: 101, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let filled: u64 = events.iter().filter_map(|e| match e {
            MatchEvent::Fill { qty, .. } => Some(qty),
            _ => None,
        }).sum();
        assert_eq!(filled, 10);
        assert_eq!(engine.best_ask(), None);
    }

    #[test]
    fn cancel_existing_order() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::CancelOrder { id: OrderId(1) });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderCancelled { id: OrderId(1), remaining_qty: 10, .. }
        )));
        assert_eq!(engine.best_bid(), None);
    }

    // ---- Task 8: Market, IOC, FOK ----

    #[test]
    fn market_order_fills_against_book() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 0, qty: 5,
            order_type: OrderType::Market, time_in_force: TimeInForce::IOC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
    }

    #[test]
    fn ioc_cancels_unfilled_remainder() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::IOC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderCancelled { id: OrderId(2), remaining_qty: 5, .. }
        )));
        assert_eq!(engine.best_bid(), None);
    }

    #[test]
    fn fok_rejects_if_insufficient_liquidity() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::FOK,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::InsufficientLiquidity, .. }
        )));
        assert_eq!(engine.best_ask(), Some(100));
    }

    #[test]
    fn fok_fills_when_sufficient_liquidity() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::FOK,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 10, .. })));
    }

    // ---- Task 9: Post-Only Orders ----

    #[test]
    fn post_only_rejected_when_would_cross() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
            order_type: OrderType::PostOnly, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::WouldCrossSpread, .. }
        )));
    }

    #[test]
    fn post_only_rests_when_no_cross() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 110, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
            order_type: OrderType::PostOnly, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(2), .. })));
        assert_eq!(engine.best_bid(), Some(100));
    }

    // ---- Task 12: Stop-Limit Orders ----

    #[test]
    fn stop_limit_buy_triggers_on_last_trade_price() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Stop-limit buy: trigger at or above 105, limit at 110.
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Bid, price: 110, qty: 10,
            order_type: OrderType::StopLimit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: Some(105), stp_group: None,
        });

        // Trade at 104: must NOT trigger the stop.
        engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 104, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(3), instrument_id: 1, side: Side::Bid, price: 104, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });

        // Trade at 105: triggers the stop.
        engine.process(Command::NewOrder {
            id: OrderId(4), instrument_id: 1, side: Side::Ask, price: 105, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(5), instrument_id: 1, side: Side::Bid, price: 105, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::StopTriggered { .. })));
    }

    #[test]
    fn stop_limit_sell_triggers_on_price_down() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Stop-limit sell: trigger at or below 95, limit at 90.
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 90, qty: 10,
            order_type: OrderType::StopLimit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: Some(95), stp_group: None,
        });

        // Trade at 96: must NOT trigger.
        engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Ask, price: 96, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(3), instrument_id: 1, side: Side::Bid, price: 96, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });

        // Trade at 95: triggers the stop.
        engine.process(Command::NewOrder {
            id: OrderId(4), instrument_id: 1, side: Side::Bid, price: 95, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(5), instrument_id: 1, side: Side::Ask, price: 95, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::StopTriggered { .. })));
    }

    // ---- Task 11: Iceberg Orders ----

    #[test]
    fn iceberg_replenishes_visible_after_fill() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Iceberg sell: 5 visible, 20 total
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 20,
            order_type: OrderType::Iceberg, time_in_force: TimeInForce::GTC,
            visible_qty: Some(5), stop_price: None, stp_group: None,
        });
        // Buy 5 — should fill the visible portion
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. })));
        // Iceberg should still be on the book with replenished visible qty
        assert_eq!(engine.best_ask(), Some(100));
    }

    #[test]
    fn iceberg_fully_consumed_when_hidden_exhausted() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Iceberg sell: 5 visible, 10 total
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Iceberg, time_in_force: TimeInForce::GTC,
            visible_qty: Some(5), stop_price: None, stp_group: None,
        });
        // Buy 10 — should fill entire iceberg
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let filled: u64 = events.iter().filter_map(|e| match e {
            MatchEvent::Fill { qty, .. } => Some(qty),
            _ => None,
        }).sum();
        assert_eq!(filled, 10);
        assert_eq!(engine.best_ask(), None);
    }

    // ---- Task 10: Self-Trade Prevention ----

    fn stp_config(mode: StpMode) -> InstrumentConfig {
        InstrumentConfig {
            id: 1, tick_size: 1, lot_size: 1, base_price: 0, max_ticks: 1000,
            stp_mode: mode,
        }
    }

    #[test]
    fn stp_cancel_newest_prevents_self_trade() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::CancelNewest), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderRejected { id: OrderId(2), reason: RejectReason::SelfTradePreventionTriggered, .. }
        )));
        assert_eq!(engine.best_ask(), Some(100)); // resting maker untouched
    }

    #[test]
    fn stp_cancel_oldest_cancels_resting_order() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::CancelOldest), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        // Resting (oldest) cancelled
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderCancelled { id: OrderId(1), .. }
        )));
        // Incoming accepted and rests
        assert_eq!(engine.best_bid(), Some(100));
        assert_eq!(engine.best_ask(), None);
    }

    #[test]
    fn stp_cancel_both_cancels_both_orders() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::CancelBoth), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(1), .. })));
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(2), .. })));
        assert_eq!(engine.best_ask(), None);
        assert_eq!(engine.best_bid(), None);
    }

    #[test]
    fn stp_decrement_and_cancel_reduces_overlap() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::DecrementAndCancel), 1024);
        // Resting ask: 10 lots
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        });
        // Incoming bid: 6 lots — overlap is min(10, 6) = 6, both reduced by 6
        let events: alloc::vec::Vec<MatchEvent> = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 6,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: Some(1),
        }).to_vec();
        // Incoming (6 lots) fully consumed by decrement → cancelled
        assert!(events.iter().any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(2), .. })));
        // Resting reduced from 10 to 4
        assert_eq!(engine.best_ask(), Some(100));
        let ask_qty = events.iter().find_map(|e| match e {
            MatchEvent::BookUpdate { side: Side::Ask, price: 100, qty, .. } => Some(*qty),
            _ => None,
        });
        assert_eq!(ask_qty, Some(4));
    }

    #[test]
    fn fok_reject_does_not_emit_order_accepted() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1), instrument_id: 1, side: Side::Ask, price: 100, qty: 5,
            order_type: OrderType::Limit, time_in_force: TimeInForce::GTC,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2), instrument_id: 1, side: Side::Bid, price: 100, qty: 10,
            order_type: OrderType::Limit, time_in_force: TimeInForce::FOK,
            visible_qty: None, stop_price: None, stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(e,
            MatchEvent::OrderRejected { id: OrderId(2), .. }
        )));
        assert!(!events.iter().any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(2), .. })));
    }
}
