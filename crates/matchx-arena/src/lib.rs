#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::vec::Vec;
use matchx_types::{ArenaIndex, Order};

/// Pre-allocated arena for Order objects.
/// Uses a free list for O(1) alloc/free with zero heap allocation
/// after construction.
pub struct Arena {
    slots: Vec<Slot>,
    free_head: Option<u32>,
    len: u32,
}

enum Slot {
    Occupied(Order),
    Free { next_free: Option<u32> },
}

impl Arena {
    /// Create arena with given capacity. All slots start free.
    pub fn new(capacity: u32) -> Self {
        let mut slots = Vec::with_capacity(capacity as usize);
        for i in 0..capacity {
            let next = if i + 1 < capacity { Some(i + 1) } else { None };
            slots.push(Slot::Free { next_free: next });
        }
        Self {
            slots,
            free_head: if capacity > 0 { Some(0) } else { None },
            len: 0,
        }
    }

    /// Allocate a slot for the given order. Returns None if full.
    #[inline]
    pub fn alloc(&mut self, order: Order) -> Option<ArenaIndex> {
        let idx = self.free_head?;
        match &self.slots[idx as usize] {
            Slot::Free { next_free } => {
                self.free_head = *next_free;
            }
            Slot::Occupied(_) => unreachable!(),
        }
        self.slots[idx as usize] = Slot::Occupied(order);
        self.len += 1;
        Some(ArenaIndex(idx))
    }

    /// Free a slot, returning it to the free list.
    #[inline]
    pub fn free(&mut self, index: ArenaIndex) {
        let idx = index.0;
        self.slots[idx as usize] = Slot::Free {
            next_free: self.free_head,
        };
        self.free_head = Some(idx);
        self.len -= 1;
    }

    /// Get immutable reference to order at index.
    #[inline]
    pub fn get(&self, index: ArenaIndex) -> &Order {
        match &self.slots[index.as_usize()] {
            Slot::Occupied(order) => order,
            Slot::Free { .. } => panic!("access to freed arena slot"),
        }
    }

    /// Get mutable reference to order at index.
    #[inline]
    pub fn get_mut(&mut self, index: ArenaIndex) -> &mut Order {
        match &mut self.slots[index.as_usize()] {
            Slot::Occupied(order) => order,
            Slot::Free { .. } => panic!("access to freed arena slot"),
        }
    }

    /// Number of live (occupied) slots.
    #[inline]
    pub fn len(&self) -> u32 {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Total capacity.
    #[inline]
    pub fn capacity(&self) -> u32 {
        self.slots.len() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use matchx_types::*;

    fn make_order(id: u64) -> Order {
        Order {
            id: OrderId(id),
            side: Side::Bid,
            price: 100,
            quantity: 10,
            filled: 0,
            order_type: OrderType::Limit,
            time_in_force: TimeInForce::GTC,
            timestamp: 0,
            visible_quantity: 10,
            stop_price: None,
            stp_group: None,
            prev: None,
            next: None,
        }
    }

    #[test]
    fn alloc_and_get() {
        let mut arena = Arena::new(16);
        let idx = arena.alloc(make_order(1)).unwrap();
        assert_eq!(arena.get(idx).id, OrderId(1));
    }

    #[test]
    fn alloc_reuses_freed_slot() {
        let mut arena = Arena::new(2);
        let a = arena.alloc(make_order(1)).unwrap();
        let b = arena.alloc(make_order(2)).unwrap();
        arena.free(a);
        let c = arena.alloc(make_order(3)).unwrap();
        // c should reuse a's slot
        assert_eq!(c, a);
        assert_eq!(arena.get(c).id, OrderId(3));
        // b still intact
        assert_eq!(arena.get(b).id, OrderId(2));
    }

    #[test]
    fn alloc_fails_when_full() {
        let mut arena = Arena::new(1);
        arena.alloc(make_order(1)).unwrap();
        assert!(arena.alloc(make_order(2)).is_none());
    }

    #[test]
    fn len_tracks_live_count() {
        let mut arena = Arena::new(4);
        assert_eq!(arena.len(), 0);
        let a = arena.alloc(make_order(1)).unwrap();
        let b = arena.alloc(make_order(2)).unwrap();
        assert_eq!(arena.len(), 2);
        arena.free(a);
        assert_eq!(arena.len(), 1);
        arena.free(b);
        assert_eq!(arena.len(), 0);
    }

    #[test]
    fn get_mut_modifies_in_place() {
        let mut arena = Arena::new(4);
        let idx = arena.alloc(make_order(1)).unwrap();
        arena.get_mut(idx).filled = 5;
        assert_eq!(arena.get(idx).filled, 5);
    }
}
