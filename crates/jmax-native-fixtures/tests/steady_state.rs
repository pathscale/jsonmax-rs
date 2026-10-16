//! After warmup the generated decoders allocate nothing on the native route.
//!
//! The VM-first route (inputs whose size the step budget cannot vouch for, and
//! decoders that accept unknown keys) builds a fresh VM scratch per call, by
//! contract, and is outside this guarantee.

use jmax_abi::number::NumericPolicy;
use jmax_native::{decode_into, NativeContext, VmLimits};
use jmax_native_fixtures::binance;
use jmax_native_fixtures::harness::{depth_document, snapshot_document, AGG_TRADE};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

struct CountingAllocator;
// SAFETY: forwards every call unchanged to the system allocator.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        if COUNTING.with(Cell::get) {
            ALLOCATIONS.with(|count| count.set(count.get() + 1));
        }
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn ctx(limits: VmLimits) -> NativeContext {
    NativeContext::new(limits, NumericPolicy::strict_default())
}

/// Allocations made by `rounds` calls of `work` after two warmup calls.
fn allocations(rounds: usize, mut work: impl FnMut()) -> u64 {
    work();
    work();
    COUNTING.with(|flag| flag.set(true));
    ALLOCATIONS.with(|count| count.set(0));
    for _ in 0..rounds {
        work();
    }
    COUNTING.with(|flag| flag.set(false));
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn small_documents_allocate_nothing() {
    let escaped = AGG_TRADE.replace("BNBBTC", "BNB\\u0042TC");
    let book = depth_document(20);
    let shot = snapshot_document(20);
    let (mut trade_ctx, mut depth_ctx, mut snapshot_ctx) =
        (ctx(VmLimits::default()), ctx(VmLimits::default()), ctx(VmLimits::default()));
    let mut trade = binance::models::AggTrade::default();
    let mut depth = binance::models::DepthUpdate::default();
    let mut snapshot = binance::models::DepthSnapshot::default();
    let count = allocations(50, || {
        decode_into(&mut trade_ctx, AGG_TRADE.as_bytes(), &mut trade).unwrap();
        decode_into(&mut trade_ctx, escaped.as_bytes(), &mut trade).unwrap();
        decode_into(&mut depth_ctx, &book, &mut depth).unwrap();
        decode_into(&mut snapshot_ctx, &shot, &mut snapshot).unwrap();
    });
    assert_eq!(count, 0);
}

#[test]
fn a_book_of_constant_size_reuses_its_capacity() {
    // Levels are boxed by the generated model, so shrinking drops them and the
    // next growth allocates again; a book of constant size allocates nothing.
    // The default step budget vouches for about 27 KB, so raise it.
    let mut limits = VmLimits::default();
    limits.max_steps = 1 << 32;
    let mut ctx = ctx(limits);
    let mut depth = binance::models::DepthUpdate::default();
    let big = depth_document(1000);
    let count = allocations(20, || decode_into(&mut ctx, &big, &mut depth).unwrap());
    assert_eq!(count, 0);
    assert_eq!(depth.bids.len(), 1000);
}
