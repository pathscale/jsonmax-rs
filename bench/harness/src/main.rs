#![allow(non_snake_case)]
#[path = "../../alloc_metrics.rs"]
mod alloc_metrics;
#[path = "../../measure.rs"]
mod measure;
#[path = "../../metrics.rs"]
mod metrics;
#[path = "../../trace_report.rs"]
mod trace_report;
#[global_allocator]
static GLOBAL: alloc_metrics::CountingPeakAlloc = alloc_metrics::CountingPeakAlloc;
mod common;
mod compare;
mod diff;
use common::*;
use std::fs;
fn main() -> R<()> {
    if std::env::var_os("DIFF").is_some() {
        diff::run()
    } else {
        compare::run()
    }
}
