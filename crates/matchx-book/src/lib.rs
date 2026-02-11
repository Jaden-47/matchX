#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::collections::{btree_map::Entry, BTreeMap};
use alloc::vec;
use alloc::vec::Vec;
use core::hash::BuildHasherDefault;
use hashbrown::HashMap;
use matchx_arena::Arena;
use matchx_types::*;
use twox_hash::XxHash64;

/// Fixed-seed deterministic hasher — required for replay determinism.
pub type DeterministicHasher = BuildHasherDefault<XxHash64>;

/// Fenwick (Binary Indexed) Tree for prefix-sum range queries on dense tick depth.
pub struct FenwickTree {
    data: Vec<u64>,
}

impl FenwickTree {
    pub fn new(size: usize) -> Self {
        Self { data: vec![0; size + 1] }
    }

    pub fn add(&mut self, index: usize, delta: u64) {
        let mut i = index + 1;
        while i < self.data.len() {
            self.data[i] += delta;
            i += i & i.wrapping_neg();
        }
    }

    pub fn sub(&mut self, index: usize, delta: u64) {
        let mut i = index + 1;
        while i < self.data.len() {
            self.data[i] = self.data[i]
                .checked_sub(delta)
                .expect("fenwick underflow");
            i += i & i.wrapping_neg();
        }
    }

    /// Prefix sum from index 0 to `index` (inclusive).
    pub fn prefix_sum(&self, index: usize) -> u64 {
        let mut i = index + 1;
        let mut acc = 0;
        while i > 0 {
            acc += self.data[i];
            i -= i & i.wrapping_neg();
        }
        acc
    }

    #[inline]
    pub fn prefix_sum_le(&self, index: usize) -> u64 {
        self.prefix_sum(index)
    }

    /// Sum from `index` to the end (inclusive).
    pub fn suffix_sum_ge(&self, index: usize) -> u64 {
        let total = self.prefix_sum(self.data.len() - 2);
        let before = index.checked_sub(1).map_or(0, |i| self.prefix_sum(i));
        total - before
    }
}

/// Augmented sparse price index: sorted price keys with a parallel Fenwick tree
/// for O(log N) range-volume queries without linear BTreeMap scans.
struct SparseVolumeIndex {
    prices: Vec<u64>,
    qtys: Vec<u64>,
    fenwick: FenwickTree,
}

impl SparseVolumeIndex {
    fn new() -> Self {
        Self { prices: Vec::new(), qtys: Vec::new(), fenwick: FenwickTree::new(0) }
    }

    fn add(&mut self, price: u64, qty: u64) {
        match self.prices.binary_search(&price) {
            Ok(rank) => {
                self.qtys[rank] += qty;
                self.fenwick.add(rank, qty);
            }
            Err(rank) => {
                self.prices.insert(rank, price);
                self.qtys.insert(rank, qty);
                self.rebuild_fenwick();
            }
        }
    }

    fn sub(&mut self, price: u64, qty: u64) {
        let rank = self.prices.binary_search(&price)
            .expect("price not in sparse volume index");
        self.qtys[rank] = self.qtys[rank]
            .checked_sub(qty)
            .expect("sparse volume index underflow");
        self.fenwick.sub(rank, qty);
        if self.qtys[rank] == 0 {
            self.prices.remove(rank);
            self.qtys.remove(rank);
            self.rebuild_fenwick();
        }
    }

    fn rebuild_fenwick(&mut self) {
        let n = self.prices.len();
        self.fenwick = FenwickTree::new(n);
        for (i, &qty) in self.qtys.iter().enumerate() {
            if qty > 0 {
                self.fenwick.add(i, qty);
            }
        }
    }

    /// Sum of quantities at prices ≤ `price`.
    fn sum_at_or_below(&self, price: u64) -> u64 {
        let n = self.prices.partition_point(|&p| p <= price);
        if n == 0 { return 0; }
        self.fenwick.prefix_sum(n - 1)
    }

    /// Sum of quantities at prices ≥ `price`.
    fn sum_at_or_above(&self, price: u64) -> u64 {
        let start = self.prices.partition_point(|&p| p < price);
        let n = self.prices.len();
        if start >= n { return 0; }
        let total = self.fenwick.prefix_sum(n - 1);
        let before = if start > 0 { self.fenwick.prefix_sum(start - 1) } else { 0 };
        total - before
    }
}

/// Hybrid order book: dense tick window near BBO for O(1) hot-path access,
/// sparse BTreeMap for far-from-BBO prices.
pub struct OrderBook {
    pub instrument_id: u32,
    bids_dense: Vec<PriceLevel>,
    asks_dense: Vec<PriceLevel>,
    dense_base_price: u64,
    dense_max_ticks: u32,
    bids_sparse: BTreeMap<u64, PriceLevel>,
    asks_sparse: BTreeMap<u64, PriceLevel>,
    bid_depth_index: FenwickTree,
    ask_depth_index: FenwickTree,
    best_bid_tick: Option<u64>,
    best_ask_tick: Option<u64>,
    // Occupancy bitset: one bit per dense tick for O(1) next-best lookup
    bids_occupied: Vec<u64>, // ceil(dense_max_ticks / 64) words
    asks_occupied: Vec<u64>,
    /// O(1) order lookup by ID. Fixed-seed hasher for deterministic replay.
    order_index: HashMap<OrderId, ArenaIndex, DeterministicHasher>,
    /// O(log N) range-volume index for sparse bid/ask levels.
    bids_sparse_index: SparseVolumeIndex,
    asks_sparse_index: SparseVolumeIndex,
}

impl OrderBook {
    pub fn new(config: InstrumentConfig) -> Self {
        let dense_n = config.max_ticks as usize;
        let bitset_words = (dense_n + 63) / 64;
        Self {
            instrument_id: config.id,
            bids_dense: vec![PriceLevel::EMPTY; dense_n],
            asks_dense: vec![PriceLevel::EMPTY; dense_n],
            dense_base_price: config.base_price,
            dense_max_ticks: config.max_ticks,
            bids_sparse: BTreeMap::new(),
            asks_sparse: BTreeMap::new(),
            bid_depth_index: FenwickTree::new(dense_n),
            ask_depth_index: FenwickTree::new(dense_n),
            best_bid_tick: None,
            best_ask_tick: None,
            bids_occupied: vec![0u64; bitset_words],
            asks_occupied: vec![0u64; bitset_words],
            order_index: HashMap::with_hasher(DeterministicHasher::default()),
            bids_sparse_index: SparseVolumeIndex::new(),
            asks_sparse_index: SparseVolumeIndex::new(),
        }
    }

    #[inline]
    fn dense_index(&self, price: u64) -> Option<usize> {
        if price < self.dense_base_price {
            return None;
        }
        let idx = price - self.dense_base_price;
        (idx < self.dense_max_ticks as u64).then_some(idx as usize)
    }

    #[inline]
    fn level_mut(&mut self, side: Side, price: u64) -> &mut PriceLevel {
        match (side, self.dense_index(price)) {
            (Side::Bid, Some(i)) => &mut self.bids_dense[i],
            (Side::Ask, Some(i)) => &mut self.asks_dense[i],
            (Side::Bid, None) => match self.bids_sparse.entry(price) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(PriceLevel::EMPTY),
            },
            (Side::Ask, None) => match self.asks_sparse.entry(price) {
                Entry::Occupied(e) => e.into_mut(),
                Entry::Vacant(e) => e.insert(PriceLevel::EMPTY),
            },
        }
    }

    #[inline]
    fn set_occupied(&mut self, side: Side, dense_idx: usize) {
        let word = dense_idx / 64;
        let bit = dense_idx % 64;
        match side {
            Side::Bid => self.bids_occupied[word] |= 1u64 << bit,
            Side::Ask => self.asks_occupied[word] |= 1u64 << bit,
        }
    }

    #[inline]
    fn clear_occupied(&mut self, side: Side, dense_idx: usize) {
        let word = dense_idx / 64;
        let bit = dense_idx % 64;
        match side {
            Side::Bid => self.bids_occupied[word] &= !(1u64 << bit),
            Side::Ask => self.asks_occupied[word] &= !(1u64 << bit),
        }
    }

    /// Insert a new order into the book. Returns the arena index, or None if arena is full.
    pub fn insert_order(
        &mut self,
        id: OrderId,
        side: Side,
        price: u64,
        qty: u64,
        order_type: OrderType,
        visible_qty: Option<u64>,
        stp_group: Option<u32>,
        arena: &mut Arena,
    ) -> Option<ArenaIndex> {
        // For Iceberg: visible_quantity is the peak slice size.
        // For all others: visible_quantity equals total qty.
        let visible_quantity = visible_qty.unwrap_or(qty);
        let order = Order {
            id,
            side,
            price,
            quantity: qty,
            filled: 0,
            order_type,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity,
            stop_price: None,
            stp_group,
            prev: None,
            next: None,
        };

        // Reject duplicate order IDs — determinism contract requires unique IDs.
        if self.order_index.contains_key(&id) {
            return None;
        }

        let arena_idx = arena.alloc(order)?;
        self.order_index.insert(id, arena_idx);

        let level = self.level_mut(side, price);

        let order_mut = arena.get_mut(arena_idx);
        order_mut.prev = level.tail;
        order_mut.next = None;

        if let Some(tail) = level.tail {
            arena.get_mut(tail).next = Some(arena_idx);
        } else {
            level.head = Some(arena_idx);
        }
        level.tail = Some(arena_idx);
        level.total_quantity += qty;
        level.order_count += 1;

        self.depth_add(side, price, qty);
        if let Some(di) = self.dense_index(price) {
            self.set_occupied(side, di);
        }

        match side {
            Side::Bid => {
                if self.best_bid_tick.is_none_or(|b| price > b) {
                    self.best_bid_tick = Some(price);
                }
            }
            Side::Ask => {
                if self.best_ask_tick.is_none_or(|a| price < a) {
                    self.best_ask_tick = Some(price);
                }
            }
        }
        Some(arena_idx)
    }

    #[inline]
    pub fn best_bid(&self) -> Option<u64> {
        self.best_bid_tick
    }

    #[inline]
    pub fn best_ask(&self) -> Option<u64> {
        self.best_ask_tick
    }

    pub fn get_bid_level(&self, price: u64) -> &PriceLevel {
        if let Some(i) = self.dense_index(price) {
            &self.bids_dense[i]
        } else {
            self.bids_sparse.get(&price).expect("missing bid level")
        }
    }

    pub fn get_ask_level(&self, price: u64) -> &PriceLevel {
        if let Some(i) = self.dense_index(price) {
            &self.asks_dense[i]
        } else {
            self.asks_sparse.get(&price).expect("missing ask level")
        }
    }

    pub fn get_bid_level_mut(&mut self, price: u64) -> &mut PriceLevel {
        if let Some(i) = self.dense_index(price) {
            &mut self.bids_dense[i]
        } else {
            self.bids_sparse.entry(price).or_insert(PriceLevel::EMPTY)
        }
    }

    pub fn get_ask_level_mut(&mut self, price: u64) -> &mut PriceLevel {
        if let Some(i) = self.dense_index(price) {
            &mut self.asks_dense[i]
        } else {
            self.asks_sparse.entry(price).or_insert(PriceLevel::EMPTY)
        }
    }

    /// Return the total quantity at a level, or 0 if the level doesn't exist.
    pub fn get_level_qty(&self, side: Side, price: u64) -> u64 {
        match (side, self.dense_index(price)) {
            (Side::Bid, Some(i)) => self.bids_dense[i].total_quantity,
            (Side::Ask, Some(i)) => self.asks_dense[i].total_quantity,
            (Side::Bid, None) => self.bids_sparse.get(&price).map_or(0, |l| l.total_quantity),
            (Side::Ask, None) => self.asks_sparse.get(&price).map_or(0, |l| l.total_quantity),
        }
    }

    /// O(1) lookup of an order's arena index by its OrderId.
    #[inline]
    pub fn lookup(&self, id: OrderId) -> Option<ArenaIndex> {
        self.order_index.get(&id).copied()
    }

    fn depth_add(&mut self, side: Side, price: u64, qty: u64) {
        if let Some(i) = self.dense_index(price) {
            match side {
                Side::Bid => self.bid_depth_index.add(i, qty),
                Side::Ask => self.ask_depth_index.add(i, qty),
            }
        } else {
            match side {
                Side::Bid => self.bids_sparse_index.add(price, qty),
                Side::Ask => self.asks_sparse_index.add(price, qty),
            }
        }
    }

    fn depth_remove(&mut self, side: Side, price: u64, qty: u64) {
        if let Some(i) = self.dense_index(price) {
            match side {
                Side::Bid => self.bid_depth_index.sub(i, qty),
                Side::Ask => self.ask_depth_index.sub(i, qty),
            }
        } else {
            match side {
                Side::Bid => self.bids_sparse_index.sub(price, qty),
                Side::Ask => self.asks_sparse_index.sub(price, qty),
            }
        }
    }

    /// Reduce an order's remaining quantity by `delta` without removing it.
    /// Updates level total_quantity and the dense/sparse depth index.
    pub fn reduce_order_qty(&mut self, idx: ArenaIndex, delta: u64, arena: &mut Arena) {
        {
            let order = arena.get_mut(idx);
            order.filled += delta;
        }
        let side = arena.get(idx).side;
        let price = arena.get(idx).price;
        let level = self.level_mut(side, price);
        level.total_quantity = level.total_quantity
            .checked_sub(delta)
            .expect("level qty underflow in reduce_order_qty");
        self.depth_remove(side, price, delta);
    }

    // ---- Task 5: Cancel / Remove ----

    /// Remove an order from its price level. Frees the arena slot.
    /// Returns the side and price of the removed order.
    pub fn remove_order(&mut self, idx: ArenaIndex, arena: &mut Arena) -> (Side, u64) {
        let order = arena.get(idx);
        let id = order.id;
        let side = order.side;
        let price = order.price;
        let qty = order.remaining();
        let prev = order.prev;
        let next = order.next;

        {
            let level = self.level_mut(side, price);

            match prev {
                Some(p) => arena.get_mut(p).next = next,
                None => level.head = next,
            }
            match next {
                Some(n) => arena.get_mut(n).prev = prev,
                None => level.tail = prev,
            }

            level.total_quantity -= qty;
            level.order_count -= 1;
        }

        self.depth_remove(side, price, qty);
        self.order_index.remove(&id);
        arena.free(idx);

        if self.level_is_empty(side, price) {
            self.prune_if_sparse_empty(side, price);
            self.refresh_best_after_level_empty(side, price);
        }

        (side, price)
    }

    fn level_is_empty(&self, side: Side, price: u64) -> bool {
        match (side, self.dense_index(price)) {
            (Side::Bid, Some(i)) => self.bids_dense[i].is_empty(),
            (Side::Ask, Some(i)) => self.asks_dense[i].is_empty(),
            (Side::Bid, None) => self.bids_sparse.get(&price).is_none_or(|l| l.is_empty()),
            (Side::Ask, None) => self.asks_sparse.get(&price).is_none_or(|l| l.is_empty()),
        }
    }

    fn prune_if_sparse_empty(&mut self, side: Side, price: u64) {
        if self.dense_index(price).is_some() {
            return;
        }
        match side {
            Side::Bid => {
                if self.bids_sparse.get(&price).is_some_and(|l| l.is_empty()) {
                    self.bids_sparse.remove(&price);
                }
            }
            Side::Ask => {
                if self.asks_sparse.get(&price).is_some_and(|l| l.is_empty()) {
                    self.asks_sparse.remove(&price);
                }
            }
        }
    }

    fn refresh_best_after_level_empty(&mut self, side: Side, removed_price: u64) {
        if let Some(di) = self.dense_index(removed_price) {
            self.clear_occupied(side, di);
        }

        match side {
            Side::Bid if self.best_bid_tick == Some(removed_price) => {
                self.best_bid_tick = self.find_highest_occupied_bid();
                if self.best_bid_tick.is_none() {
                    self.best_bid_tick = self.bids_sparse.keys().next_back().copied();
                }
            }
            Side::Ask if self.best_ask_tick == Some(removed_price) => {
                self.best_ask_tick = self.find_lowest_occupied_ask();
                if self.best_ask_tick.is_none() {
                    self.best_ask_tick = self.asks_sparse.keys().next().copied();
                }
            }
            _ => {}
        }
    }

    /// Scan bids_occupied bitset for highest set bit → best bid in dense window.
    fn find_highest_occupied_bid(&self) -> Option<u64> {
        for (word_idx, &word) in self.bids_occupied.iter().enumerate().rev() {
            if word != 0 {
                let bit = 63 - word.leading_zeros() as usize;
                let tick_idx = word_idx * 64 + bit;
                return Some(self.dense_base_price + tick_idx as u64);
            }
        }
        None
    }

    /// Scan asks_occupied bitset for lowest set bit → best ask in dense window.
    fn find_lowest_occupied_ask(&self) -> Option<u64> {
        for (word_idx, &word) in self.asks_occupied.iter().enumerate() {
            if word != 0 {
                let bit = word.trailing_zeros() as usize;
                let tick_idx = word_idx * 64 + bit;
                return Some(self.dense_base_price + tick_idx as u64);
            }
        }
        None
    }

    // ---- Task 5A: Dense Window Recentering ----

    /// Check if BBO has drifted past 70% of the window and recenter if needed.
    pub fn maybe_recenter(&mut self, arena: &mut Arena) {
        let window_size = self.dense_max_ticks as u64;
        let threshold = window_size * 7 / 10;

        let needs_recenter = match (self.best_bid_tick, self.best_ask_tick) {
            (Some(bid), _) if bid >= self.dense_base_price + threshold => true,
            (_, Some(ask)) if ask < self.dense_base_price + (window_size - threshold) => true,
            _ => false,
        };

        if needs_recenter {
            let center = self.best_bid_tick.or(self.best_ask_tick).unwrap_or(0);
            let new_base = center.saturating_sub(window_size / 2);
            self.force_recenter(new_base, arena);
        }
    }

    /// Recenter the dense window to a new base price.
    /// Evicts all dense levels to sparse, updates the base, then re-absorbs
    /// sparse levels that fall within the new window.
    pub fn force_recenter(&mut self, new_base: u64, _arena: &mut Arena) {
        if new_base == self.dense_base_price {
            return;
        }
        let old_base = self.dense_base_price;
        let dense_n = self.dense_max_ticks as usize;
        let new_end = new_base + dense_n as u64;

        // 1. Evict ALL non-empty dense levels to sparse, clear the dense array.
        for i in 0..dense_n {
            let price = old_base + i as u64;
            if !self.bids_dense[i].is_empty() {
                let moved = core::mem::replace(&mut self.bids_dense[i], PriceLevel::EMPTY);
                self.bids_sparse.insert(price, moved);
            } else {
                self.bids_dense[i] = PriceLevel::EMPTY;
            }
            if !self.asks_dense[i].is_empty() {
                let moved = core::mem::replace(&mut self.asks_dense[i], PriceLevel::EMPTY);
                self.asks_sparse.insert(price, moved);
            } else {
                self.asks_dense[i] = PriceLevel::EMPTY;
            }
        }

        self.dense_base_price = new_base;

        // 2. Absorb sparse levels that now fall inside the new window → dense.
        let bid_keys: Vec<u64> = self
            .bids_sparse
            .range(new_base..new_end)
            .map(|(&k, _)| k)
            .collect();
        for price in bid_keys {
            if let Some(level) = self.bids_sparse.remove(&price) {
                let di = (price - new_base) as usize;
                self.bids_dense[di] = level;
            }
        }
        let ask_keys: Vec<u64> = self
            .asks_sparse
            .range(new_base..new_end)
            .map(|(&k, _)| k)
            .collect();
        for price in ask_keys {
            if let Some(level) = self.asks_sparse.remove(&price) {
                let di = (price - new_base) as usize;
                self.asks_dense[di] = level;
            }
        }

        // 3. Rebuild bitsets and Fenwick trees from scratch.
        self.rebuild_indices();

        // 4. Refresh BBO if it's now in the new dense window (or update to sparse).
        self.refresh_bbo_after_recenter();
    }

    fn rebuild_indices(&mut self) {
        let dense_n = self.dense_max_ticks as usize;
        let bitset_words = (dense_n + 63) / 64;

        self.bids_occupied = vec![0u64; bitset_words];
        self.asks_occupied = vec![0u64; bitset_words];
        self.bid_depth_index = FenwickTree::new(dense_n);
        self.ask_depth_index = FenwickTree::new(dense_n);

        for i in 0..dense_n {
            if !self.bids_dense[i].is_empty() {
                self.set_occupied(Side::Bid, i);
                self.bid_depth_index.add(i, self.bids_dense[i].total_quantity);
            }
            if !self.asks_dense[i].is_empty() {
                self.set_occupied(Side::Ask, i);
                self.ask_depth_index.add(i, self.asks_dense[i].total_quantity);
            }
        }

        self.rebuild_sparse_indices();
    }

    fn rebuild_sparse_indices(&mut self) {
        let bid_pairs = self.bids_sparse.iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(&p, l)| (p, l.total_quantity));
        let ask_pairs = self.asks_sparse.iter()
            .filter(|(_, l)| !l.is_empty())
            .map(|(&p, l)| (p, l.total_quantity));

        // BTreeMap iterates in ascending order — prices are already sorted.
        let mut bids_index = SparseVolumeIndex::new();
        for (price, qty) in bid_pairs {
            // Use add() for correctness; prices come sorted so no Vec shifts occur.
            bids_index.add(price, qty);
        }
        let mut asks_index = SparseVolumeIndex::new();
        for (price, qty) in ask_pairs {
            asks_index.add(price, qty);
        }
        self.bids_sparse_index = bids_index;
        self.asks_sparse_index = asks_index;
    }

    fn refresh_bbo_after_recenter(&mut self) {
        // Refresh best bid
        if let Some(bid) = self.best_bid_tick {
            if self.level_is_empty(Side::Bid, bid) {
                self.best_bid_tick = self
                    .find_highest_occupied_bid()
                    .or_else(|| self.bids_sparse.keys().next_back().copied());
            }
        }
        // Refresh best ask
        if let Some(ask) = self.best_ask_tick {
            if self.level_is_empty(Side::Ask, ask) {
                self.best_ask_tick = self
                    .find_lowest_occupied_ask()
                    .or_else(|| self.asks_sparse.keys().next().copied());
            }
        }
    }

    /// Whether the given price is in the sparse map for the given side.
    pub fn is_in_sparse(&self, side: Side, price: u64) -> bool {
        match side {
            Side::Bid => self.bids_sparse.contains_key(&price),
            Side::Ask => self.asks_sparse.contains_key(&price),
        }
    }

    /// Current dense window base price.
    pub fn dense_base_price(&self) -> u64 {
        self.dense_base_price
    }

    /// Total ask quantity available at or below `price` (dense Fenwick + sparse index).
    pub fn ask_available_at_or_below(&self, price: u64) -> u64 {
        let window_end = self.dense_base_price.saturating_add(self.dense_max_ticks as u64);
        if let Some(i) = self.dense_index(price) {
            // Dense prefix sum + all sparse asks below the dense window start.
            let dense = self.ask_depth_index.prefix_sum_le(i);
            let sparse = self.asks_sparse_index
                .sum_at_or_below(self.dense_base_price.saturating_sub(1));
            dense + sparse
        } else if price >= window_end {
            // Price above window: all dense asks + sparse asks ≤ price.
            let dense_total = if self.dense_max_ticks > 0 {
                self.ask_depth_index.prefix_sum(self.dense_max_ticks as usize - 1)
            } else {
                0
            };
            dense_total + self.asks_sparse_index.sum_at_or_below(price)
        } else {
            // Price below window start: only sparse asks ≤ price.
            self.asks_sparse_index.sum_at_or_below(price)
        }
    }

    /// Total bid quantity available at or above `price` (dense Fenwick + sparse index).
    pub fn bid_available_at_or_above(&self, price: u64) -> u64 {
        let window_end = self.dense_base_price.saturating_add(self.dense_max_ticks as u64);
        if let Some(i) = self.dense_index(price) {
            // Dense suffix sum + all sparse bids above the dense window end.
            let dense = self.bid_depth_index.suffix_sum_ge(i);
            let sparse = self.bids_sparse_index.sum_at_or_above(window_end);
            dense + sparse
        } else if price < self.dense_base_price {
            // Price below window: all dense bids + sparse bids ≥ price.
            let dense_total = if self.dense_max_ticks > 0 {
                self.bid_depth_index.prefix_sum(self.dense_max_ticks as usize - 1)
            } else {
                0
            };
            dense_total + self.bids_sparse_index.sum_at_or_above(price)
        } else {
            // Price above window end: only sparse bids ≥ price.
            self.bids_sparse_index.sum_at_or_above(price)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> InstrumentConfig {
        InstrumentConfig {
            id: 1,
            tick_size: 1,
            lot_size: 1,
            base_price: 0,
            max_ticks: 1000,
            stp_mode: StpMode::CancelNewest,
        }
    }

    // ---- Task 4: Insert + Best Price ----

    #[test]
    fn insert_bid_updates_best_bid() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena);
        assert_eq!(book.best_bid(), Some(500));
        assert_eq!(book.best_ask(), None);
    }

    #[test]
    fn insert_ask_updates_best_ask() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Ask, 600, 10, OrderType::Limit, None, None, &mut arena);
        assert_eq!(book.best_ask(), Some(600));
        assert_eq!(book.best_bid(), None);
    }

    #[test]
    fn multiple_bids_best_is_highest() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena);
        book.insert_order(OrderId(2), Side::Bid, 510, 5, OrderType::Limit, None, None, &mut arena);
        book.insert_order(OrderId(3), Side::Bid, 490, 20, OrderType::Limit, None, None, &mut arena);
        assert_eq!(book.best_bid(), Some(510));
    }

    #[test]
    fn multiple_asks_best_is_lowest() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Ask, 600, 10, OrderType::Limit, None, None, &mut arena);
        book.insert_order(OrderId(2), Side::Ask, 590, 5, OrderType::Limit, None, None, &mut arena);
        book.insert_order(OrderId(3), Side::Ask, 610, 20, OrderType::Limit, None, None, &mut arena);
        assert_eq!(book.best_ask(), Some(590));
    }

    #[test]
    fn level_quantity_accumulates() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena);
        book.insert_order(OrderId(2), Side::Bid, 500, 20, OrderType::Limit, None, None, &mut arena);
        let level = book.get_bid_level(500);
        assert_eq!(level.total_quantity, 30);
        assert_eq!(level.order_count, 2);
    }

    #[test]
    #[should_panic(expected = "fenwick underflow")]
    fn fenwick_sub_underflow_panics() {
        let mut fw = FenwickTree::new(8);
        fw.sub(0, 1);
    }

    // ---- Task 5: Cancel and Remove ----

    #[test]
    fn cancel_only_order_clears_best() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        let idx = book.insert_order(OrderId(1), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena).unwrap();
        book.remove_order(idx, &mut arena);
        assert_eq!(book.best_bid(), None);
        assert!(book.get_bid_level(500).is_empty());
    }

    #[test]
    fn cancel_best_bid_finds_next_best() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(1), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena);
        let top = book.insert_order(OrderId(2), Side::Bid, 510, 5, OrderType::Limit, None, None, &mut arena).unwrap();
        book.insert_order(OrderId(3), Side::Bid, 490, 20, OrderType::Limit, None, None, &mut arena);

        book.remove_order(top, &mut arena);
        assert_eq!(book.best_bid(), Some(500));
    }

    #[test]
    fn cancel_middle_of_queue_preserves_links() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        let a = book.insert_order(OrderId(1), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena).unwrap();
        let b = book.insert_order(OrderId(2), Side::Bid, 500, 20, OrderType::Limit, None, None, &mut arena).unwrap();
        let c = book.insert_order(OrderId(3), Side::Bid, 500, 30, OrderType::Limit, None, None, &mut arena).unwrap();

        book.remove_order(b, &mut arena);

        let level = book.get_bid_level(500);
        assert_eq!(level.total_quantity, 40); // 10 + 30
        assert_eq!(level.order_count, 2);
        // a -> c
        assert_eq!(arena.get(a).next, Some(c));
        assert_eq!(arena.get(c).prev, Some(a));
    }

    // ---- Task 5A: Dense Window Recentering ----

    #[test]
    fn recenter_when_bbo_drifts_past_threshold() {
        let mut arena = matchx_arena::Arena::new(128);
        let mut config = config();
        config.max_ticks = 100;
        let mut book = OrderBook::new(config);

        // Insert asks far above dense window to force sparse
        book.insert_order(OrderId(1), Side::Ask, 200, 10, OrderType::Limit, None, None, &mut arena);
        assert!(book.is_in_sparse(Side::Ask, 200));

        // Move BBO up by inserting bids near top of dense window
        for i in 0u64..80 {
            book.insert_order(OrderId(100 + i), Side::Bid, 70 + (i % 10), 1, OrderType::Limit, None, None, &mut arena);
        }

        // Trigger recenter — BBO has drifted past 70% of window
        book.maybe_recenter(&mut arena);

        // After recenter, the new dense window should be centered near BBO
        let new_base = book.dense_base_price();
        assert!(new_base > 0, "dense window should have shifted up");
    }

    #[test]
    fn recenter_preserves_order_linkage() {
        let mut arena = matchx_arena::Arena::new(128);
        let mut config = config();
        config.max_ticks = 100;
        let mut book = OrderBook::new(config);

        let a = book.insert_order(OrderId(1), Side::Bid, 50, 10, OrderType::Limit, None, None, &mut arena).unwrap();
        let b = book.insert_order(OrderId(2), Side::Bid, 50, 20, OrderType::Limit, None, None, &mut arena).unwrap();

        book.force_recenter(25, &mut arena);

        // Linkage still intact
        assert_eq!(arena.get(a).next, Some(b));
        assert_eq!(arena.get(b).prev, Some(a));
        let level = book.get_bid_level(50);
        assert_eq!(level.total_quantity, 30);
        assert_eq!(level.order_count, 2);
    }

    #[test]
    fn recenter_updates_bitset_and_fenwick() {
        let mut arena = matchx_arena::Arena::new(128);
        let mut config = config();
        config.max_ticks = 100;
        let mut book = OrderBook::new(config);

        book.insert_order(OrderId(1), Side::Ask, 60, 10, OrderType::Limit, None, None, &mut arena);
        book.force_recenter(20, &mut arena);

        // Ask at 60 should still be findable as best ask
        assert_eq!(book.best_ask(), Some(60));
        // Fenwick should reflect the quantity
        let avail = book.ask_available_at_or_below(60);
        assert_eq!(avail, 10);
    }

    // ---- Task 6: Order Index ----

    #[test]
    fn lookup_order_by_id() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        book.insert_order(OrderId(42), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena);
        let idx = book.lookup(OrderId(42)).unwrap();
        assert_eq!(arena.get(idx).id, OrderId(42));
    }

    #[test]
    fn lookup_returns_none_after_cancel() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        let idx = book.insert_order(OrderId(42), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena).unwrap();
        book.remove_order(idx, &mut arena);
        assert!(book.lookup(OrderId(42)).is_none());
    }

    #[test]
    fn duplicate_order_id_is_rejected_and_original_mapping_preserved() {
        let mut arena = matchx_arena::Arena::new(64);
        let mut book = OrderBook::new(config());

        let first = book.insert_order(OrderId(42), Side::Bid, 500, 10, OrderType::Limit, None, None, &mut arena).unwrap();
        let duplicate = book.insert_order(OrderId(42), Side::Ask, 600, 5, OrderType::Limit, None, None, &mut arena);
        assert!(duplicate.is_none());
        assert_eq!(book.lookup(OrderId(42)), Some(first));
    }

    #[test]
    fn deterministic_hasher_produces_stable_output() {
        use core::hash::{BuildHasher, Hash, Hasher};
        let build = DeterministicHasher::default();
        let mut h = build.build_hasher();
        OrderId(12345).hash(&mut h);
        let result = h.finish();
        assert_eq!(
            result,
            {
                let mut h2 = build.build_hasher();
                OrderId(12345).hash(&mut h2);
                h2.finish()
            },
            "Hasher must produce identical output for identical input"
        );
    }
}
