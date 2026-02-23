#![cfg_attr(not(test), no_std)]

/// Newtype for order IDs. Monotonically increasing u64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct OrderId(pub u64);

/// Newtype for arena slot indices. u32 to save space in Order struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ArenaIndex(pub u32);

impl ArenaIndex {
    #[inline]
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }
}

/// Bid or Ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Side {
    Bid = 0,
    Ask = 1,
}

impl Side {
    #[inline]
    pub fn opposite(self) -> Self {
        match self {
            Side::Bid => Side::Ask,
            Side::Ask => Side::Bid,
        }
    }
}

/// Order type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OrderType {
    Limit,
    Market,
    PostOnly,
    StopLimit,
    Iceberg,
}

/// Time-in-force.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TimeInForce {
    GTC, // Good-til-Cancel
    IOC, // Immediate-or-Cancel
    FOK, // Fill-or-Kill
}

/// Self-trade prevention mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum StpMode {
    CancelNewest,
    CancelOldest,
    CancelBoth,
    DecrementAndCancel,
}

/// Rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RejectReason {
    InvalidPrice,
    InvalidQuantity,
    InstrumentNotFound,
    OrderNotFound,
    WouldCrossSpread,      // Post-only rejection
    InsufficientLiquidity, // FOK rejection
    SelfTradePreventionTriggered,
    DuplicateOrderId,
}

/// Sentinel value for ArenaIndex meaning "no link" (replaces Option<ArenaIndex>).
pub const ARENA_NULL: ArenaIndex = ArenaIndex(u32::MAX);

/// Sentinel value for stp_group meaning "no group" (replaces Option<u32>).
pub const STP_NONE: u32 = u32::MAX;

/// An order stored in the arena. Uses intrusive doubly-linked list
/// for FIFO queue at each price level.
///
/// Layout is carefully tuned to exactly 64 bytes (one cache line).
/// `stop_price` is NOT stored here — stop-limit orders live in the
/// engine's StopEntry queue until triggered, never in the arena.
#[derive(Debug, Clone)]
#[repr(C, align(64))]
pub struct Order {
    pub id: OrderId,
    pub price: u64,
    pub quantity: u64,
    pub filled: u64,
    pub timestamp: u64,
    pub visible_quantity: u64,
    /// `STP_NONE` (u32::MAX) means no STP group.
    pub stp_group: u32,
    /// `ARENA_NULL` means no previous order in the price-level list.
    pub prev: ArenaIndex,
    /// `ARENA_NULL` means no next order in the price-level list.
    pub next: ArenaIndex,
    pub side: Side,
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub _pad: u8,
}

impl Order {
    /// Remaining unfilled quantity.
    #[inline(always)]
    pub fn remaining(&self) -> u64 {
        self.quantity.saturating_sub(self.filled)
    }

    /// Structural validity check used by pre-trade/replay validation.
    #[inline]
    pub fn is_valid_state(&self) -> bool {
        self.filled <= self.quantity
    }

    /// Whether order is fully filled.
    #[inline(always)]
    pub fn is_filled(&self) -> bool {
        self.filled >= self.quantity
    }

    /// Quantity available for matching from this resting order.
    /// For Iceberg orders this is the current visible slice; for all others it equals `remaining()`.
    /// Uses arithmetic replenishment: `filled % visible_quantity` gives position in current slice.
    #[inline(always)]
    pub fn matchable_qty(&self) -> u64 {
        match self.order_type {
            OrderType::Iceberg if self.visible_quantity > 0 => {
                let peak = self.visible_quantity;
                let filled_this_slice = self.filled % peak;
                (peak - filled_this_slice).min(self.remaining())
            }
            _ => self.remaining(),
        }
    }
}

const _: () = assert!(
    core::mem::size_of::<Order>() == 64,
    "Order must be exactly 64 bytes (one cache line)"
);

/// A price level in the order book.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct PriceLevel {
    pub total_quantity: u64,
    pub order_count: u32,
    pub head: Option<ArenaIndex>,
    pub tail: Option<ArenaIndex>,
}

impl PriceLevel {
    pub const EMPTY: Self = Self {
        total_quantity: 0,
        order_count: 0,
        head: None,
        tail: None,
    };

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.order_count == 0
    }
}

const _: () = assert!(
    core::mem::size_of::<PriceLevel>() <= 32,
    "PriceLevel should fit within 32 bytes (half a cache line)"
);

/// Metadata shared by all emitted events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventMeta {
    pub sequence: u64,     // monotonic output sequence
    pub timestamp_ns: u64, // monotonic engine clock
}

/// Events emitted by the matching engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchEvent {
    OrderAccepted {
        meta: EventMeta,
        id: OrderId,
        side: Side,
        price: u64,
        qty: u64,
        order_type: OrderType,
    },
    OrderRejected {
        meta: EventMeta,
        id: OrderId,
        reason: RejectReason,
    },
    Fill {
        meta: EventMeta,
        maker_id: OrderId,
        taker_id: OrderId,
        price: u64,
        qty: u64,
        maker_remaining: u64,
        taker_remaining: u64,
    },
    OrderCancelled {
        meta: EventMeta,
        id: OrderId,
        remaining_qty: u64,
    },
    OrderModified {
        meta: EventMeta,
        id: OrderId,
        new_price: u64,
        new_qty: u64,
    },
    BookUpdate {
        meta: EventMeta,
        side: Side,
        price: u64,
        qty: u64, // new total at this level (0 = level removed)
    },
    StopTriggered {
        meta: EventMeta,
        stop_id: OrderId,
        new_order_id: OrderId,
    },
}

/// Commands into the matching engine.
#[derive(Debug, Clone)]
pub enum Command {
    NewOrder {
        id: OrderId,
        instrument_id: u32,
        side: Side,
        price: u64,
        qty: u64,
        order_type: OrderType,
        time_in_force: TimeInForce,
        visible_qty: Option<u64>, // Iceberg
        stop_price: Option<u64>,  // Stop-Limit
        stp_group: Option<u32>,
    },
    CancelOrder {
        id: OrderId,
    },
    ModifyOrder {
        id: OrderId,
        new_price: u64,
        new_qty: u64,
    },
}

/// Instrument configuration.
#[derive(Debug, Clone)]
pub struct InstrumentConfig {
    pub id: u32,
    pub tick_size: u64,
    pub lot_size: u64,
    pub base_price: u64, // lowest representable price in ticks
    pub max_ticks: u32,  // number of tick slots per side
    pub stp_mode: StpMode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn side_opposite() {
        assert_eq!(Side::Bid.opposite(), Side::Ask);
        assert_eq!(Side::Ask.opposite(), Side::Bid);
    }

    #[test]
    fn order_remaining_quantity() {
        let order = Order {
            id: OrderId(1),
            side: Side::Bid,
            price: 1000,
            quantity: 100,
            filled: 30,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 100,
            stp_group: STP_NONE,
            prev: ARENA_NULL,
            next: ARENA_NULL,
            _pad: 0,
        };
        assert_eq!(order.remaining(), 70);
        assert!(!order.is_filled());
    }

    #[test]
    fn order_is_filled() {
        let order = Order {
            id: OrderId(1),
            side: Side::Bid,
            price: 100,
            quantity: 50,
            filled: 50,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 50,
            stp_group: STP_NONE,
            prev: ARENA_NULL,
            next: ARENA_NULL,
            _pad: 0,
        };
        assert_eq!(order.remaining(), 0);
        assert!(order.is_filled());
    }

    #[test]
    fn arena_index_conversion() {
        let idx = ArenaIndex(42);
        assert_eq!(idx.as_usize(), 42);
    }

    #[test]
    fn remaining_saturates_on_invalid_overfill_state() {
        let order = Order {
            id: OrderId(9),
            side: Side::Ask,
            price: 100,
            quantity: 10,
            filled: 15, // invalid state
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 10,
            stp_group: STP_NONE,
            prev: ARENA_NULL,
            next: ARENA_NULL,
            _pad: 0,
        };
        assert_eq!(order.remaining(), 0);
        assert!(!order.is_valid_state());
    }

    #[test]
    fn print_order_size() {
        // Run with: cargo test -p matchx-types print_order_size -- --nocapture
        println!("size_of::<Order>() = {}", core::mem::size_of::<Order>());
        println!("align_of::<Order>() = {}", core::mem::align_of::<Order>());
        assert_eq!(core::mem::size_of::<Order>(), 64);
        assert_eq!(core::mem::align_of::<Order>(), 64);
    }
}
