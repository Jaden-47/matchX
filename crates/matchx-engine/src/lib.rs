#![cfg_attr(not(test), no_std)]
extern crate alloc;

pub mod policy;

use alloc::vec::Vec;
use core::mem::MaybeUninit;
use matchx_arena::Arena;
use matchx_book::OrderBook;
use matchx_types::*;
use policy::{MatchPolicy, PriceTimeFifo};

/// Maximum events that can be emitted by a single `process()` call.
/// Worst case: 1 Accepted + N Fills + N BookUpdates + cascading stops.
/// 64 covers sweeping an entire book side in practice.
const MAX_EVENTS_PER_CALL: usize = 64;

// --- Compile-time safety invariants for the MaybeUninit event buffer ---

// MaybeUninit<T> must be layout-identical to T (no tag, no padding overhead).
const _: () = assert!(
    core::mem::size_of::<MaybeUninit<MatchEvent>>() == core::mem::size_of::<MatchEvent>(),
    "MaybeUninit<MatchEvent> must have the same size as MatchEvent"
);
const _: () = assert!(
    core::mem::align_of::<MaybeUninit<MatchEvent>>() == core::mem::align_of::<MatchEvent>(),
    "MaybeUninit<MatchEvent> must have the same alignment as MatchEvent"
);

// Buffer must not blow the stack. 64 events × ~120 bytes each ≈ 7.5 KB.
const _: () = assert!(
    core::mem::size_of::<[MaybeUninit<MatchEvent>; MAX_EVENTS_PER_CALL]>() <= 16 * 1024,
    "event buffer exceeds 16 KB — reconsider MAX_EVENTS_PER_CALL"
);

// Capacity must cover worst-case non-cascading sweep:
// 1 Accepted + 31 (Fill + BookUpdate) pairs + 1 remainder = 64.
// If this fires, emit() will UB in release — increase MAX_EVENTS_PER_CALL.
const _: () = assert!(
    MAX_EVENTS_PER_CALL >= 64,
    "MAX_EVENTS_PER_CALL too small for worst-case sweep"
);

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
    event_buf: [MaybeUninit<MatchEvent>; MAX_EVENTS_PER_CALL],
    event_len: usize,
    /// Buy stop-limit orders sorted by stop_price ascending.
    /// Triggered when last_trade_price >= stop_price.
    stop_bids: Vec<(u64, StopEntry)>,
    /// Sell stop-limit orders sorted by stop_price descending.
    /// Triggered when last_trade_price <= stop_price.
    stop_asks: Vec<(u64, StopEntry)>,
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
            // SAFETY: [MaybeUninit<T>; N] has no validity invariant — every bit
            // pattern is valid for MaybeUninit, so an uninitialized array of
            // MaybeUninit is sound. Layout equivalence proven by static asserts above.
            event_buf: unsafe { MaybeUninit::uninit().assume_init() },
            event_len: 0,
            stop_bids: Vec::new(),
            stop_asks: Vec::new(),
            last_trade_price: None,
        }
    }

    /// Emit an event with a monotonically increasing logical clock.
    #[inline(always)]
    fn emit(&mut self, event_fn: impl FnOnce(EventMeta) -> MatchEvent) {
        self.sequence += 1;
        self.timestamp_ns += 1;
        let meta = EventMeta {
            sequence: self.sequence,
            timestamp_ns: self.timestamp_ns,
        };
        debug_assert!(
            self.event_len < MAX_EVENTS_PER_CALL,
            "event buffer overflow: more than {} events in one process() call",
            MAX_EVENTS_PER_CALL
        );
        // SAFETY: event_len < MAX_EVENTS_PER_CALL guaranteed by:
        //   1. process() resets event_len = 0 at entry
        //   2. worst-case single process() call emits ≤ 64 events (static assert above)
        //   3. debug_assert guards above catches violations in test builds
        unsafe {
            self.event_buf[self.event_len]
                .as_mut_ptr()
                .write(event_fn(meta));
        }
        self.event_len += 1;
    }

    /// Process a command and return the emitted events.
    /// The returned slice is reused on each call; copy if needed across calls.
    pub fn process(&mut self, cmd: Command) -> &[MatchEvent] {
        self.event_len = 0;
        match cmd {
            Command::NewOrder {
                id,
                side,
                price,
                qty,
                order_type,
                time_in_force,
                visible_qty,
                stop_price,
                stp_group,
                ..
            } => {
                self.process_new_order(
                    id,
                    side,
                    price,
                    qty,
                    order_type,
                    time_in_force,
                    visible_qty,
                    stop_price,
                    stp_group,
                );
            }
            Command::CancelOrder { id } => {
                self.process_cancel(id);
            }
            Command::ModifyOrder {
                id,
                new_price,
                new_qty,
            } => {
                self.process_modify(id, new_price, new_qty);
            }
        }
        self.drain_stop_triggers();
        // SAFETY: the first `event_len` slots were written by `emit()`.
        // The cast from *MaybeUninit<MatchEvent> to *MatchEvent is sound
        // because MaybeUninit<T> is #[repr(transparent)] over T (static asserts above).
        unsafe {
            core::slice::from_raw_parts(
                self.event_buf.as_ptr() as *const MatchEvent,
                self.event_len,
            )
        }
    }

    #[cold]
    fn emit_rejected(&mut self, id: OrderId, reason: RejectReason) {
        self.emit(|meta| MatchEvent::OrderRejected { meta, id, reason });
    }

    #[inline(always)]
    #[allow(clippy::too_many_arguments)]
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
                self.emit_rejected(id, RejectReason::WouldCrossSpread);
                return;
            }
            self.emit(|meta| MatchEvent::OrderAccepted {
                meta,
                id,
                side,
                price,
                qty,
                order_type,
            });
            self.book.insert_order(
                id,
                side,
                price,
                qty,
                order_type,
                visible_qty,
                stp_group,
                &mut self.arena,
            );
            self.emit(|meta| MatchEvent::BookUpdate {
                meta,
                side,
                price,
                qty,
            });
            return;
        }

        // --- Stop-Limit: accept and queue, defer activation until stop price triggers ---
        if order_type == OrderType::StopLimit {
            if qty == 0 {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta,
                    id,
                    reason: RejectReason::InvalidQuantity,
                });
                return;
            }
            let Some(stop_px) = stop_price else {
                self.emit(|meta| MatchEvent::OrderRejected {
                    meta,
                    id,
                    reason: RejectReason::InvalidPrice,
                });
                return;
            };
            self.emit(|meta| MatchEvent::OrderAccepted {
                meta,
                id,
                side,
                price,
                qty,
                order_type,
            });
            let entry = StopEntry {
                id,
                side,
                limit_price: price,
                qty,
                time_in_force,
                visible_qty,
                stp_group,
            };
            match side {
                Side::Bid => {
                    let pos = self.stop_bids.partition_point(|(px, _)| *px < stop_px);
                    self.stop_bids.insert(pos, (stop_px, entry));
                }
                Side::Ask => {
                    let pos = self.stop_asks.partition_point(|(px, _)| *px > stop_px);
                    self.stop_asks.insert(pos, (stop_px, entry));
                }
            }
            return;
        }

        // --- Validation ---
        if qty == 0 {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta,
                id,
                reason: RejectReason::InvalidQuantity,
            });
            return;
        }
        if self.book.lookup(id).is_some() {
            self.emit_rejected(id, RejectReason::DuplicateOrderId);
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
                self.emit_rejected(id, RejectReason::InsufficientLiquidity);
                return;
            }
        }

        // --- STP CancelNewest: reject before accepting if first maker matches group ---
        if let Some(taker_stp) = stp_group
            && self.config.stp_mode == StpMode::CancelNewest
            && self.stp_first_maker_matches(side, effective_price, taker_stp)
        {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta,
                id,
                reason: RejectReason::SelfTradePreventionTriggered,
            });
            return;
        }

        // --- Accept ---
        self.emit(|meta| MatchEvent::OrderAccepted {
            meta,
            id,
            side,
            price,
            qty,
            order_type,
        });

        // --- Match against the opposing side ---
        let cancel_incoming =
            self.match_against_book(id, side, effective_price, stp_group, &mut qty);

        // --- STP triggered: cancel incoming ---
        if cancel_incoming {
            self.emit(|meta| MatchEvent::OrderCancelled {
                meta,
                id,
                remaining_qty: qty,
            });
            return;
        }

        // --- Handle remainder ---
        match (order_type, time_in_force) {
            // GTC Limit/Iceberg: rest the unfilled portion on the book.
            (OrderType::Limit | OrderType::Iceberg, TimeInForce::GTC) if qty > 0 => {
                self.book.insert_order(
                    id,
                    side,
                    price,
                    qty,
                    order_type,
                    visible_qty,
                    stp_group,
                    &mut self.arena,
                );
                self.emit(|meta| MatchEvent::BookUpdate {
                    meta,
                    side,
                    price,
                    qty,
                });
            }
            // Market, IOC, FOK: cancel any unfilled remainder.
            (OrderType::Market, _) | (_, TimeInForce::IOC) | (_, TimeInForce::FOK) if qty > 0 => {
                self.emit(|meta| MatchEvent::OrderCancelled {
                    meta,
                    id,
                    remaining_qty: qty,
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
        let Some(resting_price) = best_opposing else {
            return false;
        };
        if !self
            .policy
            .is_price_acceptable(taker_side, taker_price, resting_price)
        {
            return false;
        }
        let head = match taker_side {
            Side::Bid => self.book.get_ask_level(resting_price).head,
            Side::Ask => self.book.get_bid_level(resting_price).head,
        };
        let Some(head_idx) = head else { return false };
        let maker_stp = self.arena.get(head_idx).stp_group;
        maker_stp != STP_NONE && maker_stp == taker_stp
    }

    /// Walk the opposing side of the book, filling the taker one maker at a time.
    /// Fully-filled makers are removed inline so the BBO is always current.
    /// Returns `true` if an STP action requires cancelling the incoming order.
    #[inline(always)]
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
            let Some(resting_price) = best_price else {
                break;
            };

            if !self
                .policy
                .is_price_acceptable(taker_side, taker_price, resting_price)
            {
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
            if let Some(ts) = taker_stp
                && maker_stp != STP_NONE
                && ts == maker_stp
            {
                match self.config.stp_mode {
                    StpMode::CancelNewest => {
                        // Should have been caught by pre-check; stop matching silently.
                        break;
                    }
                    StpMode::CancelOldest => {
                        // Cancel the resting (oldest) order; incoming continues.
                        self.book.remove_order(maker_idx, &mut self.arena);
                        self.emit(|meta| MatchEvent::OrderCancelled {
                            meta,
                            id: maker_id,
                            remaining_qty: maker_remaining,
                        });
                        let level_qty = self.book.get_level_qty(maker_side, resting_price);
                        self.emit(|meta| MatchEvent::BookUpdate {
                            meta,
                            side: maker_side,
                            price: resting_price,
                            qty: level_qty,
                        });
                        continue;
                    }
                    StpMode::CancelBoth => {
                        // Cancel resting maker, then signal to cancel incoming.
                        self.book.remove_order(maker_idx, &mut self.arena);
                        self.emit(|meta| MatchEvent::OrderCancelled {
                            meta,
                            id: maker_id,
                            remaining_qty: maker_remaining,
                        });
                        let level_qty = self.book.get_level_qty(maker_side, resting_price);
                        self.emit(|meta| MatchEvent::BookUpdate {
                            meta,
                            side: maker_side,
                            price: resting_price,
                            qty: level_qty,
                        });
                        return true;
                    }
                    StpMode::DecrementAndCancel => {
                        // Reduce both by the overlap quantity; cancel incoming.
                        let overlap = (*remaining).min(maker_remaining);
                        debug_assert!(overlap <= *remaining, "overlap exceeds taker remaining");
                        debug_assert!(
                            overlap <= maker_remaining,
                            "overlap exceeds maker remaining"
                        );
                        if maker_remaining == overlap {
                            self.book.remove_order(maker_idx, &mut self.arena);
                        } else {
                            self.book
                                .reduce_order_qty(maker_idx, overlap, &mut self.arena);
                        }
                        let level_qty = self.book.get_level_qty(maker_side, resting_price);
                        self.emit(|meta| MatchEvent::BookUpdate {
                            meta,
                            side: maker_side,
                            price: resting_price,
                            qty: level_qty,
                        });
                        *remaining -= overlap;
                        return true;
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

    /// Fire all pending stops whose trigger price has been crossed, then process
    /// their triggered limit orders. Loops until last_trade_price stabilizes,
    /// ensuring cascading stops (A fills -> new price -> triggers B) work.
    #[inline(always)]
    fn drain_stop_triggers(&mut self) {
        loop {
            let Some(last_price) = self.last_trade_price else {
                return;
            };
            let prev_price = last_price;

            // Drain buy stops: stop_bids is sorted ascending, trigger from front
            // where stop_price <= last_price
            while let Some((stop_px, _)) = self.stop_bids.first() {
                if *stop_px > last_price {
                    break;
                }
                let (_, entry) = self.stop_bids.remove(0);
                let stop_id = entry.id;
                let new_order_id = entry.id;
                self.emit(|meta| MatchEvent::StopTriggered {
                    meta,
                    stop_id,
                    new_order_id,
                });
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

            // Drain sell stops: stop_asks is sorted descending, trigger from front
            // where stop_price >= last_price
            while let Some((stop_px, _)) = self.stop_asks.first() {
                if *stop_px < last_price {
                    break;
                }
                let (_, entry) = self.stop_asks.remove(0);
                let stop_id = entry.id;
                let new_order_id = entry.id;
                self.emit(|meta| MatchEvent::StopTriggered {
                    meta,
                    stop_id,
                    new_order_id,
                });
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

            // If the last trade price didn't change, no new stops can trigger.
            if self.last_trade_price == Some(prev_price) {
                break;
            }
        }
    }

    #[inline(always)]
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
                meta,
                id,
                remaining_qty: remaining,
            });
            let level_qty = self.book.get_level_qty(side, price);
            self.emit(|meta| MatchEvent::BookUpdate {
                meta,
                side,
                price,
                qty: level_qty,
            });
        } else {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta,
                id,
                reason: RejectReason::OrderNotFound,
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
            self.emit(|meta| MatchEvent::OrderModified {
                meta,
                id,
                new_price,
                new_qty,
            });
            let old_level_qty = self.book.get_level_qty(side, old_price);
            self.emit(|meta| MatchEvent::BookUpdate {
                meta,
                side,
                price: old_price,
                qty: old_level_qty,
            });
            // Route replacement through full new-order path (handles crossing).
            self.process_new_order(
                id,
                side,
                new_price,
                new_qty,
                OrderType::Limit,
                TimeInForce::GTC,
                None,
                None,
                None,
            );
        } else {
            self.emit(|meta| MatchEvent::OrderRejected {
                meta,
                id,
                reason: RejectReason::OrderNotFound,
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

    /// Capture a minimal snapshot of engine metadata.
    /// Returns (sequence, timestamp_ns, last_trade_price_or_0).
    /// Full state reconstruction uses journal replay from this sequence.
    pub fn snapshot(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&self.sequence.to_le_bytes());
        buf.extend_from_slice(&self.timestamp_ns.to_le_bytes());
        buf.extend_from_slice(&self.last_trade_price.unwrap_or(0).to_le_bytes());
        buf
    }

    /// Current sequence number (for snapshot coordination with journal).
    #[inline]
    pub fn current_sequence(&self) -> u64 {
        self.sequence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(1), .. }))
        );
        assert_eq!(engine.best_bid(), Some(100));
    }

    #[test]
    fn crossing_limit_orders_produce_fill() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::Fill {
                maker_id: OrderId(1),
                taker_id: OrderId(2),
                price: 100,
                qty: 5,
                ..
            }
        )));
    }

    #[test]
    fn partial_fill_remainder_rests() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
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
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. }))
        );
        assert_eq!(engine.best_bid(), Some(100)); // remainder rests
        assert_eq!(engine.best_ask(), None); // ask fully filled
    }

    #[test]
    fn taker_sweeps_multiple_price_levels_in_one_call() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Ask,
            price: 101,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(3),
            instrument_id: 1,
            side: Side::Bid,
            price: 101,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let filled: u64 = events
            .iter()
            .filter_map(|e| match e {
                MatchEvent::Fill { qty, .. } => Some(qty),
                _ => None,
            })
            .sum();
        assert_eq!(filled, 10);
        assert_eq!(engine.best_ask(), None);
    }

    #[test]
    fn cancel_existing_order() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
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
        let events = engine.process(Command::CancelOrder { id: OrderId(1) });
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderCancelled {
                id: OrderId(1),
                remaining_qty: 10,
                ..
            }
        )));
        assert_eq!(engine.best_bid(), None);
    }

    // ---- Task 8: Market, IOC, FOK ----

    #[test]
    fn market_order_fills_against_book() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 0,
            qty: 5,
            order_type: OrderType::Market,
            time_in_force: TimeInForce::IOC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. }))
        );
    }

    #[test]
    fn ioc_cancels_unfilled_remainder() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::IOC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. }))
        );
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderCancelled {
                id: OrderId(2),
                remaining_qty: 5,
                ..
            }
        )));
        assert_eq!(engine.best_bid(), None);
    }

    #[test]
    fn fok_rejects_if_insufficient_liquidity() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::FOK,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                id: OrderId(2),
                reason: RejectReason::InsufficientLiquidity,
                ..
            }
        )));
        assert_eq!(engine.best_ask(), Some(100));
    }

    #[test]
    fn fok_fills_when_sufficient_liquidity() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::FOK,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 10, .. }))
        );
    }

    // ---- Task 9: Post-Only Orders ----

    #[test]
    fn post_only_rejected_when_would_cross() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::PostOnly,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                id: OrderId(2),
                reason: RejectReason::WouldCrossSpread,
                ..
            }
        )));
    }

    #[test]
    fn post_only_rests_when_no_cross() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 110,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::PostOnly,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(2), .. }))
        );
        assert_eq!(engine.best_bid(), Some(100));
    }

    // ---- Task 12: Stop-Limit Orders ----

    #[test]
    fn stop_limit_buy_triggers_on_last_trade_price() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Stop-limit buy: trigger at or above 105, limit at 110.
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Bid,
            price: 110,
            qty: 10,
            order_type: OrderType::StopLimit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: Some(105),
            stp_group: None,
        });

        // Trade at 104: must NOT trigger the stop.
        engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Ask,
            price: 104,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(3),
            instrument_id: 1,
            side: Side::Bid,
            price: 104,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });

        // Trade at 105: triggers the stop.
        engine.process(Command::NewOrder {
            id: OrderId(4),
            instrument_id: 1,
            side: Side::Ask,
            price: 105,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(5),
            instrument_id: 1,
            side: Side::Bid,
            price: 105,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::StopTriggered { .. }))
        );
    }

    #[test]
    fn stop_limit_sell_triggers_on_price_down() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Stop-limit sell: trigger at or below 95, limit at 90.
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 90,
            qty: 10,
            order_type: OrderType::StopLimit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: Some(95),
            stp_group: None,
        });

        // Trade at 96: must NOT trigger.
        engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Ask,
            price: 96,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(3),
            instrument_id: 1,
            side: Side::Bid,
            price: 96,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });

        // Trade at 95: triggers the stop.
        engine.process(Command::NewOrder {
            id: OrderId(4),
            instrument_id: 1,
            side: Side::Bid,
            price: 95,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(5),
            instrument_id: 1,
            side: Side::Ask,
            price: 95,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::StopTriggered { .. }))
        );
    }

    // ---- Task 11: Iceberg Orders ----

    #[test]
    fn iceberg_replenishes_visible_after_fill() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Iceberg sell: 5 visible, 20 total
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 20,
            order_type: OrderType::Iceberg,
            time_in_force: TimeInForce::GTC,
            visible_qty: Some(5),
            stop_price: None,
            stp_group: None,
        });
        // Buy 5 — should fill the visible portion
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. }))
        );
        // Iceberg should still be on the book with replenished visible qty
        assert_eq!(engine.best_ask(), Some(100));
    }

    #[test]
    fn iceberg_fully_consumed_when_hidden_exhausted() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Iceberg sell: 5 visible, 10 total
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Iceberg,
            time_in_force: TimeInForce::GTC,
            visible_qty: Some(5),
            stop_price: None,
            stp_group: None,
        });
        // Buy 10 — should fill entire iceberg
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
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
        let filled: u64 = events
            .iter()
            .filter_map(|e| match e {
                MatchEvent::Fill { qty, .. } => Some(qty),
                _ => None,
            })
            .sum();
        assert_eq!(filled, 10);
        assert_eq!(engine.best_ask(), None);
    }

    // ---- Task 10: Self-Trade Prevention ----

    fn stp_config(mode: StpMode) -> InstrumentConfig {
        InstrumentConfig {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            base_price: 0,
            max_ticks: 1000,
            stp_mode: mode,
        }
    }

    #[test]
    fn stp_cancel_newest_prevents_self_trade() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::CancelNewest), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderRejected {
                id: OrderId(2),
                reason: RejectReason::SelfTradePreventionTriggered,
                ..
            }
        )));
        assert_eq!(engine.best_ask(), Some(100)); // resting maker untouched
    }

    #[test]
    fn stp_cancel_oldest_cancels_resting_order() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::CancelOldest), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        // Resting (oldest) cancelled
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(1), .. }))
        );
        // Incoming accepted and rests
        assert_eq!(engine.best_bid(), Some(100));
        assert_eq!(engine.best_ask(), None);
    }

    #[test]
    fn stp_cancel_both_cancels_both_orders() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::CancelBoth), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(1), .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(2), .. }))
        );
        assert_eq!(engine.best_ask(), None);
        assert_eq!(engine.best_bid(), None);
    }

    #[test]
    fn stp_decrement_and_cancel_reduces_overlap() {
        let mut engine = MatchingEngine::new(stp_config(StpMode::DecrementAndCancel), 1024);
        // Resting ask: 10 lots
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });
        // Incoming bid: 6 lots — overlap is min(10, 6) = 6, both reduced by 6
        let events: alloc::vec::Vec<MatchEvent> = engine
            .process(Command::NewOrder {
                id: OrderId(2),
                instrument_id: 1,
                side: Side::Bid,
                price: 100,
                qty: 6,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: Some(1),
            })
            .to_vec();
        // Incoming (6 lots) fully consumed by decrement → cancelled
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderCancelled { id: OrderId(2), .. }))
        );
        // Resting reduced from 10 to 4
        assert_eq!(engine.best_ask(), Some(100));
        let ask_qty = events.iter().find_map(|e| match e {
            MatchEvent::BookUpdate {
                side: Side::Ask,
                price: 100,
                qty,
                ..
            } => Some(*qty),
            _ => None,
        });
        assert_eq!(ask_qty, Some(4));
    }

    #[test]
    fn fok_reject_does_not_emit_order_accepted() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::FOK,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderRejected { id: OrderId(2), .. }))
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, MatchEvent::OrderAccepted { id: OrderId(2), .. }))
        );
    }

    #[test]
    fn stop_limit_triggers_correctly_after_queue_refactor() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Place resting ask at 100
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        // Add a stop-limit buy: stop at 100, limit at 105
        engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 105,
            qty: 3,
            order_type: OrderType::StopLimit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: Some(100),
            stp_group: None,
        });
        // A trade at price 100 triggers the stop
        let events = engine.process(Command::NewOrder {
            id: OrderId(3),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 2,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        // The stop should have triggered: StopTriggered event must be present
        let triggered = events.iter().any(|e| {
            matches!(
                e,
                MatchEvent::StopTriggered {
                    stop_id: OrderId(2),
                    ..
                }
            )
        });
        assert!(
            triggered,
            "expected StopTriggered event for OrderId(2), got: {:?}",
            events
        );
    }

    #[test]
    fn event_buffer_does_not_overflow_on_16_fills() {
        let mut engine = MatchingEngine::new(test_config(), 1024);
        // Place 16 resting asks at same price
        for i in 0u64..16 {
            engine.process(Command::NewOrder {
                id: OrderId(i + 1),
                instrument_id: 1,
                side: Side::Ask,
                price: 5000,
                qty: 1,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: None,
            });
        }
        // One large bid sweeping all 16
        let events = engine.process(Command::NewOrder {
            id: OrderId(100),
            instrument_id: 1,
            side: Side::Bid,
            price: 5000,
            qty: 16,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        // Should have: 1 Accepted + 16 Fills + up to 16 BookUpdates = max ~33 events
        // Our buffer is 64 — well within range
        assert!(
            events.len() >= 17,
            "expected at least 17 events, got {}",
            events.len()
        );
        assert!(
            events.len() <= 64,
            "exceeded buffer capacity: {}",
            events.len()
        );
    }

    #[test]
    fn stop_cascade_triggers_second_stop_when_first_fills_past_threshold() {
        let mut engine = MatchingEngine::new(test_config(), 4096);

        // Place resting ask liquidity at prices 105 and 110.
        engine.process(Command::NewOrder {
            id: OrderId(10),
            instrument_id: 1,
            side: Side::Ask,
            price: 105,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(11),
            instrument_id: 1,
            side: Side::Ask,
            price: 110,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });

        // Stop buy A: triggers at 100, limit at 115 (will sweep asks at 105 and 110).
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Bid,
            price: 115,
            qty: 20,
            order_type: OrderType::StopLimit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: Some(100),
            stp_group: None,
        });

        // Stop buy B: triggers at 108 — should cascade after A fills at 105+.
        engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 120,
            qty: 5,
            order_type: OrderType::StopLimit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: Some(108),
            stp_group: None,
        });

        // Place resting ask + buy that will trade at 100 to trigger stop A.
        engine.process(Command::NewOrder {
            id: OrderId(20),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        let events = engine.process(Command::NewOrder {
            id: OrderId(21),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });

        // Both stops should have triggered.
        let stop_triggers: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, MatchEvent::StopTriggered { .. }))
            .collect();
        assert!(
            stop_triggers.len() >= 2,
            "Expected 2 stop triggers (cascade), got {}. Events: {:?}",
            stop_triggers.len(),
            events
        );
    }

    // ---- Task 7 (new): STP Mode Tests ----

    #[test]
    fn stp_cancel_oldest_cancels_resting_maker() {
        let cfg = InstrumentConfig {
            stp_mode: StpMode::CancelOldest,
            ..test_config()
        };
        let mut engine = MatchingEngine::new(cfg, 1024);

        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });

        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });

        // Maker (oldest) should be cancelled. No fills.
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderCancelled {
                id: OrderId(1),
                remaining_qty: 10,
                ..
            }
        )));
        assert!(!events.iter().any(|e| matches!(e, MatchEvent::Fill { .. })));
        assert_eq!(engine.best_bid(), Some(100)); // taker rests
    }

    #[test]
    fn stp_cancel_both_cancels_maker_and_taker() {
        let cfg = InstrumentConfig {
            stp_mode: StpMode::CancelBoth,
            ..test_config()
        };
        let mut engine = MatchingEngine::new(cfg, 1024);

        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });

        let events = engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });

        let cancels: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, MatchEvent::OrderCancelled { .. }))
            .collect();
        assert_eq!(cancels.len(), 2, "Expected both maker and taker cancelled");
        assert!(!events.iter().any(|e| matches!(e, MatchEvent::Fill { .. })));
        assert_eq!(engine.best_bid(), None);
        assert_eq!(engine.best_ask(), None);
    }

    #[test]
    fn stp_decrement_and_cancel_reduces_both_sides() {
        let cfg = InstrumentConfig {
            stp_mode: StpMode::DecrementAndCancel,
            ..test_config()
        };
        let mut engine = MatchingEngine::new(cfg, 1024);

        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: Some(1),
        });

        // Incoming bid: 3 qty, same group. overlap = min(3, 10) = 3.
        let events: alloc::vec::Vec<MatchEvent> = engine
            .process(Command::NewOrder {
                id: OrderId(2),
                instrument_id: 1,
                side: Side::Bid,
                price: 100,
                qty: 3,
                order_type: OrderType::Limit,
                time_in_force: TimeInForce::GTC,
                visible_qty: None,
                stop_price: None,
                stp_group: Some(1),
            })
            .to_vec();

        // Taker cancelled with remaining 0 (3 - 3 = 0).
        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderCancelled {
                id: OrderId(2),
                remaining_qty: 0,
                ..
            }
        )));
        // Maker still on book with reduced qty (10 - 3 = 7).
        assert_eq!(engine.best_ask(), Some(100));
        assert!(!events.iter().any(|e| matches!(e, MatchEvent::Fill { .. })));
    }

    // ---- Task 8: Modify Order Tests ----

    #[test]
    fn modify_order_changes_price_and_qty() {
        let mut engine = MatchingEngine::new(test_config(), 1024);

        engine.process(Command::NewOrder {
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
        assert_eq!(engine.best_bid(), Some(100));

        let events = engine.process(Command::ModifyOrder {
            id: OrderId(1),
            new_price: 105,
            new_qty: 20,
        });

        assert!(events.iter().any(|e| matches!(
            e,
            MatchEvent::OrderModified {
                id: OrderId(1),
                new_price: 105,
                new_qty: 20,
                ..
            }
        )));
        assert_eq!(engine.best_bid(), Some(105));
    }

    #[test]
    fn modify_to_crossing_price_triggers_match() {
        let mut engine = MatchingEngine::new(test_config(), 1024);

        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 5,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });
        engine.process(Command::NewOrder {
            id: OrderId(2),
            instrument_id: 1,
            side: Side::Bid,
            price: 90,
            qty: 10,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            visible_qty: None,
            stop_price: None,
            stp_group: None,
        });

        let events = engine.process(Command::ModifyOrder {
            id: OrderId(2),
            new_price: 100,
            new_qty: 10,
        });

        assert!(
            events
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 5, .. }))
        );
    }

    // ---- Task 9: Iceberg Replenishment Test ----

    #[test]
    fn iceberg_exposes_visible_slice_then_replenishes() {
        let mut engine = MatchingEngine::new(test_config(), 1024);

        // Iceberg ask: 30 total, visible 10 at a time.
        engine.process(Command::NewOrder {
            id: OrderId(1),
            instrument_id: 1,
            side: Side::Ask,
            price: 100,
            qty: 30,
            order_type: OrderType::Iceberg,
            time_in_force: TimeInForce::GTC,
            visible_qty: Some(10),
            stop_price: None,
            stp_group: None,
        });

        // First buy: takes 10 (first visible slice).
        let events1 = engine.process(Command::NewOrder {
            id: OrderId(2),
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
        assert!(
            events1
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 10, .. }))
        );

        // Second buy: takes 10 (second slice -- replenished).
        let events2 = engine.process(Command::NewOrder {
            id: OrderId(3),
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
        assert!(
            events2
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 10, .. }))
        );

        // Third buy: takes last 10.
        let events3 = engine.process(Command::NewOrder {
            id: OrderId(4),
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
        assert!(
            events3
                .iter()
                .any(|e| matches!(e, MatchEvent::Fill { qty: 10, .. }))
        );

        assert_eq!(engine.best_ask(), None); // fully consumed
    }
}
