use matchx_arena::Arena;
use matchx_types::*;

/// A single fill generated during matching.
pub struct Fill {
    pub maker_idx: ArenaIndex,
    pub maker_id: OrderId,
    pub taker_id: OrderId,
    pub price: u64,
    pub qty: u64,
}

/// Allocation-free sink for fills produced by the matching loop.
pub trait FillSink {
    fn on_fill(&mut self, fill: Fill);
}

/// Pluggable matching policy trait.
pub trait MatchPolicy {
    /// Walk one resting level and push fills into sink.
    fn match_order(
        &self,
        taker_id: OrderId,
        remaining: &mut u64,
        resting_price: u64,
        level_head: Option<ArenaIndex>,
        arena: &mut Arena,
        sink: &mut dyn FillSink,
    );

    /// Whether an incoming order's price can trade against a resting price.
    fn is_price_acceptable(
        &self,
        incoming_side: Side,
        incoming_price: u64,
        resting_price: u64,
    ) -> bool;
}

/// Standard price-time FIFO matching.
pub struct PriceTimeFifo;

impl MatchPolicy for PriceTimeFifo {
    fn match_order(
        &self,
        taker_id: OrderId,
        remaining: &mut u64,
        resting_price: u64,
        mut cursor: Option<ArenaIndex>,
        arena: &mut Arena,
        sink: &mut dyn FillSink,
    ) {
        while let Some(maker_idx) = cursor {
            if *remaining == 0 {
                break;
            }
            let maker = arena.get(maker_idx);
            let fill_qty = (*remaining).min(maker.remaining());
            let maker_id = maker.id;
            cursor = maker.next;
            sink.on_fill(Fill {
                maker_idx,
                maker_id,
                taker_id,
                price: resting_price,
                qty: fill_qty,
            });
            *remaining -= fill_qty;
        }
    }

    #[inline]
    fn is_price_acceptable(
        &self,
        incoming_side: Side,
        incoming_price: u64,
        resting_price: u64,
    ) -> bool {
        match incoming_side {
            // Buy: willing to pay up to incoming_price
            Side::Bid => incoming_price >= resting_price,
            // Sell: willing to sell down to incoming_price
            Side::Ask => incoming_price <= resting_price,
        }
    }
}
