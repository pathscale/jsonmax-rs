#[path = "../../../alloc_metrics.rs"]
mod alloc_metrics;
#[global_allocator]
static GLOBAL: alloc_metrics::CountingPeakAlloc = alloc_metrics::CountingPeakAlloc;
fn main() {
    use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
    use std::hint::black_box;
    // Fixed layouts exercise alloc, zeroed alloc, growth, shrink and deallocation.
    unsafe {
        let old = black_box(alloc(Layout::from_size_align(32, 8).unwrap()));
        alloc_metrics::begin();
        let mut p = black_box(alloc_zeroed(Layout::from_size_align(8, 8).unwrap()));
        p = black_box(realloc(p, Layout::from_size_align(8, 8).unwrap(), 24));
        p = black_box(realloc(p, Layout::from_size_align(24, 8).unwrap(), 16));
        dealloc(p, Layout::from_size_align(16, 8).unwrap());
        let sample = alloc_metrics::end();
        assert_eq!((sample.peak, sample.allocs, sample.bytes), (24, 3, 48));
        alloc_metrics::begin();
        dealloc(old, Layout::from_size_align(32, 8).unwrap());
        let next = black_box(alloc(Layout::from_size_align(16, 8).unwrap()));
        let sample = alloc_metrics::end();
        assert_eq!((sample.peak, sample.allocs, sample.bytes), (0, 1, 16));
        dealloc(next, Layout::from_size_align(16, 8).unwrap());
    }
    println!("allocation accounting passed");
}
