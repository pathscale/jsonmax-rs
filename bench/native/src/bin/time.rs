//! Full native and general operation measurements. See bench/MEASUREMENT.md.
use jmax_native_bench::alloc_metrics;
#[path = "../../../measure.rs"]
mod measure;
#[path = "../../../metrics.rs"]
mod metrics;
#[path = "../../../trace_report.rs"]
mod trace_report;
#[global_allocator]
static GLOBAL: alloc_metrics::CountingPeakAlloc = alloc_metrics::CountingPeakAlloc;
use jmax_native_bench::{CASES, Op, R, general, with_case};
fn report(case: &str, bytes: usize, ops: Vec<(&'static str, Op<'_>)>) -> R<()> {
    let filter = std::env::var("FILTER").ok();
    for (name, mut op) in ops {
        if filter
            .as_ref()
            .is_some_and(|f| !f.split(',').any(|p| format!("{case}/{name}").contains(p)))
        {
            continue;
        }
        let retained = if (name.starts_with("simd-json") && !name.ends_with("serialize")) {
            bytes
        } else if name == jmax_native_bench::VM_NATIVE_DIAG_ROW {
            jmax_native_bench::VM_RETAINED.load(std::sync::atomic::Ordering::Relaxed)
        } else {
            0
        };
        let getter =
            || jmax_native_bench::QUERY_RETAINED.load(std::sync::atomic::Ordering::Relaxed);
        let m = measure::run(
            &mut *op,
            retained,
            if name.starts_with("ps-pikkr") {
                Some(&getter)
            } else {
                None
            },
        )?;
        trace_report::report(&format!("{case}/{name}"), &mut *op)?;
        let class = [
            "query-only",
            "strict-extract",
            "validate",
            "dom-owned",
            "dom-borrowed",
            "serialize",
        ]
        .into_iter()
        .find(|cl| name.contains(cl))
        .unwrap_or(if name.ends_with("tape") || name.ends_with("borrowed") {
            "dom-borrowed"
        } else if name.contains("lazy-skip") {
            "lazy-skip"
        } else if name.ends_with("ignored") {
            "validate"
        } else {
            "typed"
        });
        let parser = name
            .strip_suffix(&format!(" {class}"))
            .unwrap_or(name);
        println!("{}", measure::row(case, class, parser, bytes, &m));
    }
    Ok(())
}
fn main() -> R<()> {
    println!("{}", measure::HEADER);
    let requested = std::env::var("CASES").unwrap_or_else(|_| CASES.join(","));
    for case in requested.split(',') {
        if let Some(name) = case.strip_prefix("general/") {
            general::with_case(name, |bytes, ops| report(case, bytes, ops))?;
        } else {
            with_case(case, |bytes, ops| report(case, bytes, ops))?;
        }
    }
    Ok(())
}
