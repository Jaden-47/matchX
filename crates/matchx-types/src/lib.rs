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
    GTC,  // Good-til-Cancel
    IOC,  // Immediate-or-Cancel
    FOK,  // Fill-or-Kill
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
    WouldCrossSpread,             // Post-only rejection
    InsufficientLiquidity,        // FOK rejection
    SelfTradePreventionTriggered,
    DuplicateOrderId,
}

/// An order stored in the arena. Uses intrusive doubly-linked list
/// for FIFO queue at each price level.
#[derive(Debug, Clone)]
#[repr(C)]
pub struct Order {
    pub id: OrderId,
    pub side: Side,
    pub price: u64,            // ticks
    pub quantity: u64,         // lots
    pub filled: u64,           // lots
    pub order_type: OrderType,
    pub time_in_force: TimeInForce,
    pub timestamp: u64,        // nanoseconds, monotonic
    pub visible_quantity: u64, // for Iceberg
    pub stop_price: Option<u64>,
    pub stp_group: Option<u32>,
    // Intrusive linked list pointers (arena indices)
    pub prev: Option<ArenaIndex>,
    pub next: Option<ArenaIndex>,
}

impl Order {
    /// Remaining unfilled quantity.
    #[inline]
    pub fn remaining(&self) -> u64 {
        self.quantity.saturating_sub(self.filled)
    }

    /// Structural validity check used by pre-trade/replay validation.
    #[inline]
    pub fn is_valid_state(&self) -> bool {
        self.filled <= self.quantity
    }

    /// Whether order is fully filled.
    #[inline]
    pub fn is_filled(&self) -> bool {
        self.filled >= self.quantity
    }

    /// Quantity available for matching from this resting order.
    /// For Iceberg orders this is the current visible slice; for all others it equals `remaining()`.
    /// Uses arithmetic replenishment: `filled % visible_quantity` gives position in current slice.
    #[inline]
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
        visible_qty: Option<u64>,  // Iceberg
        stop_price: Option<u64>,   // Stop-Limit
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
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
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
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
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
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        };
        assert_eq!(order.remaining(), 0);
        assert!(!order.is_valid_state());
    }
}
