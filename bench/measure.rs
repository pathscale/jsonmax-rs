//! Separate batched throughput/counter and individual-call latency/memory passes.
use crate::{alloc_metrics as heap, metrics};
use std::time::Instant;
pub struct Measurement {
    pub ns: f64,
    pub p50: f64,
    pub p95: f64,
    pub instructions: String,
    pub cycles: String,
    pub allocs: f64,
    pub requested: f64,
    pub peak: usize,
    pub retained: usize,
}
pub fn run<E>(
    f: &mut dyn FnMut() -> Result<(), E>,
    initial_retained: usize,
    retained: Option<&dyn Fn() -> usize>,
) -> Result<Measurement, E> {
    let before = heap::live();
    for _ in 0..8 {
        f()?;
    }
    let warm_retained = initial_retained + heap::live().saturating_sub(before);
    let samples = std::env::var("SAMPLES")
        .ok()
        .and_then(|x| x.parse::<usize>().ok())
        .unwrap_or(101)
        .max(20);
    let target = std::env::var("TARGET_NS")
        .ok()
        .and_then(|x| x.parse::<f64>().ok())
        .unwrap_or(2_000_000.0);
    let start = Instant::now();
    for _ in 0..4 {
        f()?;
    }
    let estimate = start.elapsed().as_nanos() as f64 / 4.0;
    let reps = (target / estimate.max(1.0)).ceil().clamp(1.0, 65536.0) as usize;
    let mut elapsed = 0u128;
    for _ in 0..15 {
        let start = Instant::now();
        for _ in 0..reps {
            f()?;
        }
        elapsed += start.elapsed().as_nanos();
    }
    let counter_reps = reps.max(128);
    let usage0 = metrics::snapshot();
    for _ in 0..counter_reps {
        f()?;
    }
    let usage1 = metrics::snapshot();
    let (instructions, cycles, _) = metrics::per_call(usage0, usage1, counter_reps);
    let mut times = Vec::with_capacity(samples);
    // Memory instrumentation is outside the latency pass, with reset for EACH call.
    for _ in 0..samples {
        let start = Instant::now();
        f()?;
        times.push(start.elapsed().as_nanos() as f64);
    }
    let mut peak = 0;
    let mut allocs = 0;
    let mut requested = 0;
    for _ in 0..samples {
        heap::begin();
        let result = f();
        let sample = heap::end();
        result?;
        peak = peak.max(sample.peak);
        allocs += sample.allocs;
        requested += sample.bytes;
    }
    times.sort_by(f64::total_cmp);
    Ok(Measurement {
        ns: elapsed as f64 / (15 * reps) as f64,
        p50: times[(samples - 1) / 2],
        p95: times[(samples * 95).div_ceil(100) - 1],
        instructions,
        cycles,
        allocs: allocs as f64 / samples as f64,
        requested: requested as f64 / samples as f64,
        peak,
        retained: retained.map_or(warm_retained, |get| get()),
    })
}
pub const HEADER: &str = "corpus\tclass\tparser\tbytes\tMB/s\tp50_ns\tp95_ns\tinstructions/call\tcycles/call\tallocs/call\treq_bytes/call\tpeak_live_heap_B\tretained_B\tmax_overhead_B";
pub fn row(corpus: &str, class: &str, parser: &str, bytes: usize, m: &Measurement) -> String {
    format!(
        "{corpus}\t{class}\t{parser}\t{bytes}\t{:.2}\t{:.1}\t{:.1}\t{}\t{}\t{:.2}\t{:.2}\t{}\t{}\t{}",
        bytes as f64 * 1000.0 / m.ns,
        m.p50,
        m.p95,
        m.instructions,
        m.cycles,
        m.allocs,
        m.requested,
        m.peak,
        m.retained,
        m.peak + m.retained
    )
}
