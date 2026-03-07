#![cfg_attr(not(test), no_std)]
extern crate alloc;

use alloc::vec::Vec;
use core::mem::MaybeUninit;
use matchx_types::{ArenaIndex, Order};

const FREE_LIST_END: u32 = u32::MAX;

/// Pre-allocated arena for Order objects using parallel arrays.
///
/// Layout:
/// - `data`: flat `Vec<MaybeUninit<Order>>`. Each element is exactly
///   `size_of::<Order>()` = 64 bytes (one cache line). No enum overhead.
/// - `next_free`: parallel `Vec<u32>` used as a singly-linked free list.
///   For free slots: stores index of next free slot (FREE_LIST_END = end).
///   For occupied slots: value is undefined (not read).
///
/// This avoids the old `Slot` enum which would have grown to ~128 bytes
/// once `Order` gained `align(64)`.
pub struct Arena {
    data: Vec<MaybeUninit<Order>>,
    next_free: Vec<u32>,
    free_head: u32,
    len: u32,
    capacity: u32,
    #[cfg(debug_assertions)]
    generation: Vec<u64>,
}

impl Arena {
    /// Create arena with given capacity. All slots start free.
    pub fn new(capacity: u32) -> Self {
        let cap = capacity as usize;
        let mut next_free = Vec::with_capacity(cap);
        for i in 0..capacity {
            next_free.push(if i + 1 < capacity { i + 1 } else { FREE_LIST_END });
        }
        Self {
            data: (0..cap).map(|_| MaybeUninit::uninit()).collect(),
            next_free,
            free_head: if capacity > 0 { 0 } else { FREE_LIST_END },
            len: 0,
            capacity,
            #[cfg(debug_assertions)]
            generation: alloc::vec![0u64; cap],
        }
    }

    /// Allocate a slot for the given order. Returns None if full.
    #[inline]
    pub fn alloc(&mut self, order: Order) -> Option<ArenaIndex> {
        if self.free_head == FREE_LIST_END {
            return None;
        }
        let idx = self.free_head;
        self.free_head = self.next_free[idx as usize];
        // SAFETY: slot `idx` is free (taken from free list); we have exclusive access.
        unsafe { self.data[idx as usize].as_mut_ptr().write(order) };
        self.len += 1;
        #[cfg(debug_assertions)]
        {
            self.generation[idx as usize] += 1;
        }
        Some(ArenaIndex(idx))
    }

    /// Free a slot, returning it to the free list.
    ///
    /// # Safety (caller responsibility)
    /// The caller must ensure `index` refers to an occupied slot.
    #[inline]
    pub fn free(&mut self, index: ArenaIndex) {
        let idx = index.0;
        #[cfg(debug_assertions)]
        {
            assert!(
                self.generation[idx as usize] % 2 == 1,
                "double-free: slot {} has generation {} (already free)",
                idx,
                self.generation[idx as usize]
            );
            self.generation[idx as usize] += 1;
        }
        // SAFETY: caller guarantees slot is occupied.
        unsafe { self.data[idx as usize].assume_init_drop() };
        self.next_free[idx as usize] = self.free_head;
        self.free_head = idx;
        self.len -= 1;
    }

    /// Get immutable reference to order at index.
    ///
    /// # Safety (caller responsibility)
    /// The caller must ensure `index` refers to an occupied slot.
    #[inline]
    pub fn get(&self, index: ArenaIndex) -> &Order {
        #[cfg(debug_assertions)]
        {
            assert!(
                self.generation[index.as_usize()] % 2 == 1,
                "use-after-free: slot {} has generation {} (freed)",
                index.0,
                self.generation[index.as_usize()]
            );
        }
        // SAFETY: caller guarantees slot is occupied.
        unsafe { self.data[index.as_usize()].assume_init_ref() }
    }

    /// Get mutable reference to order at index.
    ///
    /// # Safety (caller responsibility)
    /// The caller must ensure `index` refers to an occupied slot.
    #[inline]
    pub fn get_mut(&mut self, index: ArenaIndex) -> &mut Order {
        #[cfg(debug_assertions)]
        {
            assert!(
                self.generation[index.as_usize()] % 2 == 1,
                "use-after-free: slot {} has generation {} (freed)",
                index.0,
                self.generation[index.as_usize()]
            );
        }
        // SAFETY: caller guarantees slot is occupied.
        unsafe { self.data[index.as_usize()].assume_init_mut() }
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
        self.capacity
    }
}

// SAFETY: Order is Send; Arena holds no thread-local state.
unsafe impl Send for Arena {}

#[cfg(all(feature = "huge_pages", target_os = "linux"))]
mod huge {
    use core::mem::MaybeUninit;
    use matchx_types::{ArenaIndex, Order};
    use alloc::vec::Vec;

    const FREE_LIST_END: u32 = u32::MAX;
    // MAP_HUGE_SHIFT = 26 per Linux kernel (not always exported by libc crate)
    const MAP_HUGE_SHIFT: i32 = 26;

    /// Arena variant using `mmap(MAP_HUGETLB | MAP_HUGE_2MB)`.
    /// Falls back to `mmap(MAP_ANONYMOUS)` if huge pages are unavailable.
    /// Identical public API to `Arena`.
    pub struct HugePageArena {
        data: *mut MaybeUninit<Order>,
        mmap_len: usize,
        next_free: Vec<u32>,
        free_head: u32,
        len: u32,
        capacity: u32,
    }

    impl HugePageArena {
        pub fn new(capacity: u32) -> Self {
            let byte_len = (capacity as usize) * core::mem::size_of::<Order>();
            let page_2mb = 2 * 1024 * 1024usize;
            let mmap_len = (byte_len + page_2mb - 1) & !(page_2mb - 1);

            let ptr = unsafe {
                const PROT_READ: i32 = 0x1;
                const PROT_WRITE: i32 = 0x2;
                const MAP_PRIVATE: i32 = 0x02;
                const MAP_ANONYMOUS: i32 = 0x20;
                const MAP_HUGETLB: i32 = 0x40000;
                const MAP_FAILED: *mut libc::c_void = !0usize as *mut libc::c_void;

                let huge_flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_HUGETLB
                    | (21 << MAP_HUGE_SHIFT);
                let p = libc::mmap(
                    core::ptr::null_mut(),
                    mmap_len,
                    PROT_READ | PROT_WRITE,
                    huge_flags,
                    -1,
                    0,
                );
                if p == MAP_FAILED {
                    // Fallback: regular anonymous mapping
                    let p2 = libc::mmap(
                        core::ptr::null_mut(),
                        mmap_len,
                        PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS,
                        -1,
                        0,
                    );
                    assert_ne!(p2, MAP_FAILED, "mmap failed for arena backing");
                    p2
                } else {
                    p
                }
            };

            let cap = capacity as usize;
            let mut next_free = Vec::with_capacity(cap);
            for i in 0..capacity {
                next_free.push(if i + 1 < capacity { i + 1 } else { FREE_LIST_END });
            }

            Self {
                data: ptr as *mut MaybeUninit<Order>,
                mmap_len,
                next_free,
                free_head: if capacity > 0 { 0 } else { FREE_LIST_END },
                len: 0,
                capacity,
            }
        }

        #[inline]
        pub fn alloc(&mut self, order: Order) -> Option<ArenaIndex> {
            if self.free_head == FREE_LIST_END { return None; }
            let idx = self.free_head;
            self.free_head = self.next_free[idx as usize];
            // SAFETY: slot idx is free; we have exclusive access.
            unsafe { self.data.add(idx as usize).write(MaybeUninit::new(order)) };
            self.len += 1;
            Some(ArenaIndex(idx))
        }

        #[inline]
        pub fn free(&mut self, index: ArenaIndex) {
            let idx = index.0;
            // SAFETY: caller guarantees slot is occupied.
            unsafe { (*self.data.add(idx as usize)).assume_init_drop() };
            self.next_free[idx as usize] = self.free_head;
            self.free_head = idx;
            self.len -= 1;
        }

        #[inline]
        pub fn get(&self, index: ArenaIndex) -> &Order {
            // SAFETY: caller guarantees slot is occupied.
            unsafe { (*self.data.add(index.as_usize())).assume_init_ref() }
        }

        #[inline]
        pub fn get_mut(&mut self, index: ArenaIndex) -> &mut Order {
            // SAFETY: caller guarantees slot is occupied.
            unsafe { (*self.data.add(index.as_usize())).assume_init_mut() }
        }

        pub fn len(&self) -> u32 { self.len }
        pub fn is_empty(&self) -> bool { self.len == 0 }
        pub fn capacity(&self) -> u32 { self.capacity }
    }

    impl Drop for HugePageArena {
        fn drop(&mut self) {
            // SAFETY: data was allocated via mmap with mmap_len bytes.
            unsafe { libc::munmap(self.data as *mut libc::c_void, self.mmap_len) };
        }
    }

    // SAFETY: HugePageArena holds no thread-local state; Order is Send.
    unsafe impl Send for HugePageArena {}
}

#[cfg(all(feature = "huge_pages", target_os = "linux"))]
pub use huge::HugePageArena;

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
            stp_group: STP_NONE,
            prev: ARENA_NULL,
            next: ARENA_NULL,
            _pad: 0,
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
        assert_eq!(c, a);
        assert_eq!(arena.get(c).id, OrderId(3));
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

    #[test]
    fn slot_size_is_exactly_64_bytes() {
        assert_eq!(core::mem::size_of::<Order>(), 64);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "use-after-free")]
    fn debug_detects_use_after_free() {
        let mut arena = Arena::new(4);
        let idx = arena.alloc(make_order(1)).unwrap();
        arena.free(idx);
        let _ = arena.get(idx);
    }

    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "double-free")]
    fn debug_detects_double_free() {
        let mut arena = Arena::new(4);
        let idx = arena.alloc(make_order(1)).unwrap();
        arena.free(idx);
        arena.free(idx);
    }
}
