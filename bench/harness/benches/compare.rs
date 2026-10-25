#[global_allocator]
static GLOBAL: alloc_metrics::CountingPeakAlloc = alloc_metrics::CountingPeakAlloc;
#[path = "../../alloc_metrics.rs"]
mod alloc_metrics;
#[path = "../src/common.rs"]
mod common;
#[path = "../src/compare.rs"]
mod compare;
#[path = "../../measure.rs"]
mod measure;
#[path = "../../metrics.rs"]
mod metrics;
fn main() -> common::R<()> {
    compare::run()
}
