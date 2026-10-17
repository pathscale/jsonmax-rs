//! Median ns per call for every parser on every case, interleaved in random
//! order. TSV: `case  parser  bytes  median_ns`. Env: SAMPLES (15),
//! TARGET_NS (2 ms), CASES=a,b FILTER=substr,substr on `case/parser`.
#[global_allocator]
static GLOBAL: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;

use jmax_native_bench::{with_case, Op, CASES, R};
use std::{io::Write, time::Instant};

fn main() -> R<()> {
    let samples: usize = std::env::var("SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    let target_ns: f64 = std::env::var("TARGET_NS").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000_000.0);
    let cases = std::env::var("CASES").ok();
    let filter = std::env::var("FILTER").ok();
    let mut out = std::io::stdout().lock();
    for case in CASES {
        if cases.as_ref().is_some_and(|c| !c.split(',').any(|x| x == *case)) {
            continue;
        }
        with_case(case, |bytes, ops| {
            let mut jobs: Vec<(&'static str, Op<'_>, usize, Vec<f64>)> = ops
                .into_iter()
                .filter(|(name, _)| {
                    filter.as_ref().map_or(true, |f| f.split(',').any(|p| format!("{case}/{name}").contains(p)))
                })
                .map(|(name, op)| (name, op, 1, vec![]))
                .collect();
            for job in &mut jobs {
                (job.1)()?;
                let start = Instant::now();
                let mut count = 0;
                while count < 3 || start.elapsed().as_millis() < 20 {
                    (job.1)()?;
                    count += 1;
                }
                let ns = start.elapsed().as_nanos() as f64 / count as f64;
                job.2 = (target_ns / ns).ceil().clamp(1., 1_048_576.0) as usize;
            }
            let mut state = 0x6a09e667f3bcc909u64;
            for _ in 0..samples {
                let mut order: Vec<_> = (0..jobs.len()).collect();
                for i in (1..order.len()).rev() {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    order.swap(i, state as usize % (i + 1));
                }
                for i in order {
                    let job = &mut jobs[i];
                    let start = Instant::now();
                    for _ in 0..job.2 {
                        (job.1)()?;
                    }
                    job.3.push(start.elapsed().as_nanos() as f64 / job.2 as f64);
                }
            }
            for job in &mut jobs {
                job.3.sort_by(f64::total_cmp);
                writeln!(out, "{case}\t{}\t{bytes}\t{:.1}", job.0, job.3[(job.3.len() - 1) / 2])?;
            }
            Ok(())
        })?;
    }
    Ok(())
}
