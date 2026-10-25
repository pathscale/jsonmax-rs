//! Count a single selected route; every memory sample has its own baseline.
use jmax_native_bench::alloc_metrics;
#[path = "../../../measure.rs"]
mod measure;
#[path = "../../../metrics.rs"]
mod metrics;
#[global_allocator]
static ALLOCATOR: alloc_metrics::CountingPeakAlloc = alloc_metrics::CountingPeakAlloc;
use jmax_native_bench::{Op, R, general, with_case};
fn count_ops(bytes: usize, ops: Vec<(&'static str, Op<'_>)>, parser: &str, iters: usize) -> R<()> {
    let mut op = ops
        .into_iter()
        .find(|(name, _)| *name == parser)
        .ok_or("unknown parser")?
        .1;
    let baseline = alloc_metrics::live();
    for _ in 0..3 {
        op()?;
    }
    let retained = alloc_metrics::live().saturating_sub(baseline)
        + if (parser.starts_with("simd-json") && !parser.ends_with("serialize")) {
            bytes
        } else if parser == "jmax vm+reparse" {
            jmax_native_bench::VM_RETAINED.load(std::sync::atomic::Ordering::Relaxed)
        } else {
            0
        };
    if iters > 0 && std::env::var_os("HARDWARE").is_some() {
        let getter =
            || jmax_native_bench::QUERY_RETAINED.load(std::sync::atomic::Ordering::Relaxed);
        let mut m = measure::run(
            &mut *op,
            retained,
            if parser.starts_with("ps-pikkr") {
                Some(&getter)
            } else {
                None
            },
        )?;
        m.retained = m.retained.max(retained);
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
            m.instructions,
            m.allocs,
            m.requested,
            m.p50,
            m.cycles,
            m.peak,
            m.retained,
            m.p95,
            bytes as f64 * 1000.0 / m.ns
        );
    } else {
        let mut calls = 0;
        let mut requested = 0;
        for _ in 0..iters {
            alloc_metrics::begin();
            let result = op();
            let sample = alloc_metrics::end();
            result?;
            calls += sample.allocs;
            requested += sample.bytes;
        }
        if iters > 0 {
            println!(
                "{}\t{}",
                calls as f64 / iters as f64,
                requested as f64 / iters as f64
            );
        }
    }
    Ok(())
}
fn main() -> R<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 3 && args[1] == "--list" {
        let list = |_: usize, ops: Vec<(&'static str, Op<'_>)>| -> R<()> {
            for (name, _) in ops {
                println!("{name}");
            }
            Ok(())
        };
        return if let Some(name) = args[2].strip_prefix("general/") {
            general::with_case(name, list)
        } else {
            with_case(&args[2], list)
        };
    }
    let [_, case, parser, iters] = args.as_slice() else {
        return Err("usage: count CASE PARSER ITERS".into());
    };
    let iters = iters.parse()?;
    if let Some(name) = case.strip_prefix("general/") {
        general::with_case(name, |bytes, ops| count_ops(bytes, ops, parser, iters))
    } else {
        with_case(case, |bytes, ops| count_ops(bytes, ops, parser, iters))
    }
}
