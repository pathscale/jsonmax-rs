//! Requested live heap accounting for single-threaded benchmark operations.
//! Successful allocations only; realloc records its new requested size and net
//! live change. Failed realloc leaves the original allocation live. No headers,
//! allocator metadata, stack, RSS, or process lifetime high-water marks are used.
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
static LIVE: AtomicUsize = AtomicUsize::new(0);
static BASE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static CALLS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicBool = AtomicBool::new(false);
pub struct CountingPeakAlloc;
pub fn live() -> usize {
    LIVE.load(Relaxed)
}
fn add(size: usize) {
    let now = LIVE.fetch_add(size, Relaxed) + size;
    if ACTIVE.load(Relaxed) {
        PEAK.fetch_max(now, Relaxed);
    }
}
fn request(size: usize) {
    if ACTIVE.load(Relaxed) {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(size, Relaxed);
    }
}
// SAFETY: layouts and pointers are forwarded unchanged to the selected backing
// allocator. Selection is fixed at compile time for every allocation.
unsafe impl GlobalAlloc for CountingPeakAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe {
            if cfg!(feature = "system-alloc") {
                System.alloc(layout)
            } else {
                rusty_alloc::RustyAlloc.alloc(layout)
            }
        };
        if !ptr.is_null() {
            request(layout.size());
            add(layout.size());
        }
        ptr
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe {
            if cfg!(feature = "system-alloc") {
                System.alloc_zeroed(layout)
            } else {
                rusty_alloc::RustyAlloc.alloc_zeroed(layout)
            }
        };
        if !ptr.is_null() {
            request(layout.size());
            add(layout.size());
        }
        ptr
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        unsafe {
            if cfg!(feature = "system-alloc") {
                System.dealloc(ptr, layout)
            } else {
                rusty_alloc::RustyAlloc.dealloc(ptr, layout)
            }
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let next = unsafe {
            if cfg!(feature = "system-alloc") {
                System.realloc(ptr, layout, size)
            } else {
                rusty_alloc::RustyAlloc.realloc(ptr, layout, size)
            }
        };
        if !next.is_null() {
            request(size);
            if size >= layout.size() {
                add(size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - size, Relaxed);
            }
        }
        next
    }
}
#[derive(Clone, Copy, Default)]
pub struct Sample {
    pub peak: usize,
    pub allocs: usize,
    pub bytes: usize,
}
pub fn begin() {
    let baseline = live();
    BASE.store(baseline, Relaxed);
    PEAK.store(baseline, Relaxed);
    CALLS.store(0, Relaxed);
    BYTES.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
}
pub fn end() -> Sample {
    ACTIVE.store(false, Relaxed);
    Sample {
        peak: PEAK.load(Relaxed).saturating_sub(BASE.load(Relaxed)),
        allocs: CALLS.load(Relaxed),
        bytes: BYTES.load(Relaxed),
    }
}
