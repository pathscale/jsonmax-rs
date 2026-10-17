//! `count CASE PARSER ITERS`: run ITERS calls after three warmup calls and print
//! `allocations_per_call  bytes_per_call` (allocations and reallocations, bytes
//! requested). Under callgrind, the difference between ITERS=N and ITERS=0 divided
//! by N is the instruction count per call; see icount.sh.
use jmax_native_bench::{with_case, R};
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

static CALLS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Counting;
// SAFETY: forwards every call unchanged to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size() as u64, Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        CALLS.fetch_add(1, Relaxed);
        BYTES.fetch_add(size as u64, Relaxed);
        unsafe { System.realloc(pointer, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;

fn main() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, case, parser, iters] = args.as_slice() else {
        return Err("usage: count CASE PARSER ITERS".into());
    };
    let iters: u64 = iters.parse()?;
    with_case(case, |_, ops| {
        let mut op = ops
            .into_iter()
            .find(|(name, _)| name == parser)
            .ok_or_else(|| format!("unknown parser {parser}"))?
            .1;
        for _ in 0..3 {
            op()?;
        }
        let usage0 = jmax_native_bench::metrics::snapshot();
        let start = std::time::Instant::now();
        let (c0, b0) = (CALLS.load(Relaxed), BYTES.load(Relaxed));
        for _ in 0..iters {
            op()?;
        }
        let ns = start.elapsed().as_nanos() as f64 / iters.max(1) as f64;
        let usage1 = jmax_native_bench::metrics::snapshot();
        let (c1, b1) = (CALLS.load(Relaxed), BYTES.load(Relaxed));
        if iters > 0 {
            if std::env::var_os("HARDWARE").is_some() {
                let (instructions, cycles, peak) = jmax_native_bench::metrics::per_call(usage0, usage1, iters as usize);
                println!("{instructions}\t{}\t{}\t{ns:.1}\t{cycles}\t{peak}", (c1 - c0) as f64 / iters as f64, (b1 - b0) as f64 / iters as f64);
            } else {
                println!("{}\t{}", (c1 - c0) as f64 / iters as f64, (b1 - b0) as f64 / iters as f64);
            }
        }
        Ok(())
    })
}
