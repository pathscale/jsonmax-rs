//! Deterministic, single-process benchmark CLI. Corpus generation is untimed.
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    alloc::{GlobalAlloc, Layout},
    env,
    io::{self, Write},
    process::ExitCode,
    sync::atomic::{AtomicUsize, Ordering},
};

#[cfg(feature = "system-alloc")]
use std::alloc::System;

use jmax_bench::{
    BenchmarkConfig, BenchmarkMode, CorpusConfig, Summary, SystemClock, followup, parse, serialize,
};

const HELP: &str = "jmax_bench [OPTIONS]

Runs Parse, Validate, Extract, and Serialize on all seven deterministic workloads.

Options:
  --warmup N             Untimed corpus passes per mode (default: 3)
  --iterations N         Measured corpus passes per mode (default: 20)
  --documents N          Documents per workload (default: 128)
  --items N              Fields, elements, or levels per document (default: 32)
  --seed N               Deterministic seed, decimal or 0x-prefixed hexadecimal
  --max-corpus-bytes N   Aggregate corpus byte budget (default: 67108864)
  --max-input-bytes N    Input budget per mode including warmup (default: 4294967296)
  -h, --help             Print this help

Times describe complete corpus passes. Payload Bytes is input bytes per pass.
Throughput uses decimal MB (1000000 bytes). Msg/sec counts individual documents.
Both Cold-Alloc and Warm-Scratch (Zero-Alloc Reuse) run for every mode.
Each workload captures its scalar fields or a raw container span;
Extract still validates the complete document and publishes its validation proof.
";

#[path = "support/wave6.rs"]
mod wave6;

struct CountingAllocator;
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);
static DEALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "system-alloc")]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { System.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        new_pointer
    }
}

#[cfg(not(feature = "system-alloc"))]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { rusty_alloc::RustyAlloc.alloc(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        unsafe { rusty_alloc::RustyAlloc.dealloc(pointer, layout) };
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { rusty_alloc::RustyAlloc.alloc_zeroed(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_pointer = unsafe { rusty_alloc::RustyAlloc.realloc(pointer, layout, new_size) };
        if !new_pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
        }
        new_pointer
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[cfg(feature = "system-alloc")]
const fn active_allocator() -> &'static str {
    "std::alloc::System"
}

#[cfg(not(feature = "system-alloc"))]
const fn active_allocator() -> &'static str {
    "ps-rusty_alloc 2.3.3 (RustyAlloc)"
}

#[derive(Default)]
struct Options {
    corpus: CorpusConfig,
    benchmark: BenchmarkConfig,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            let _ = writeln!(io::stderr().lock(), "jmax_bench: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    if env::args().nth(1).as_deref() == Some("--wave6") {
        return wave6::run().map_err(|e| e.to_string());
    }
    let Some(options) = options()? else {
        return Ok(());
    };
    options
        .benchmark
        .validate()
        .map_err(|error| error.to_string())?;
    let suite = followup::make_suite(options.corpus).map_err(|error| error.to_string())?;
    // Retain only summaries until every workload completes. Sample vectors are
    // released between modes and do not inflate later measured working sets.
    let mut reports = Vec::new();
    reports
        .try_reserve_exact(suite.len() * 8)
        .map_err(|_| String::from("cannot allocate benchmark report table"))?;
    let mut raw = match env::var_os("JMAX_SAMPLES_PATH") {
        Some(path) => Some(std::io::BufWriter::new(
            std::fs::File::create(path).map_err(|e| e.to_string())?,
        )),
        None => None,
    };
    let mut clock = SystemClock::new();
    for workload in &suite {
        let first = workload
            .payloads
            .first()
            .ok_or_else(|| String::from("generated workload is empty"))?;
        let backend =
            jmax_dispatch::dispatch(first.len(), jmax_dispatch::DispatchPolicy::default())
                .map_err(|error| error.to_string())?
                .backend();
        let kind = followup::name(workload.kind);
        for mode in [
            BenchmarkMode::Parse,
            BenchmarkMode::Validate,
            BenchmarkMode::Extract,
            BenchmarkMode::Serialize,
        ] {
            for warm in [false, true] {
                let regime = if warm {
                    "Warm-Scratch (Zero-Alloc Reuse)"
                } else {
                    "Cold-Alloc"
                };
                let report = match (mode, warm) {
                    (BenchmarkMode::Parse, false) => {
                        parse::run_parse(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Parse, true) => {
                        parse::run_parse_warm(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Validate, false) => {
                        parse::run_validate(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Validate, true) => {
                        parse::run_validate_warm(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Extract, false) => {
                        parse::run_extract(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Extract, true) => {
                        parse::run_extract_warm(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Serialize, false) => {
                        serialize::run_serialize_cold(workload, options.benchmark, &mut clock)
                    }
                    (BenchmarkMode::Serialize, true) => {
                        serialize::run_serialize(workload, options.benchmark, &mut clock)
                    }
                    _ => unreachable!("CLI modes are fixed"),
                }
                .map_err(|error| format!("{kind}/{}: {error}", mode_name(mode)))?;
                let payload_bytes = report
                    .samples
                    .first()
                    .map_or(0, |sample| sample.input_bytes);
                if let Some(raw) = &mut raw {
                    for (index, sample) in report.samples.iter().enumerate() {
                        writeln!(raw,"{{\"workload\":\"{kind}\",\"mode\":\"{}\",\"regime\":\"{regime}\",\"sample\":{index},\"elapsed_ns\":{},\"documents\":{},\"input_bytes\":{},\"digest\":{}}}",mode_name(mode),sample.elapsed_ns,sample.documents,sample.input_bytes,sample.digest).map_err(|e|e.to_string())?;
                    }
                }
                let summary = report.summarize();
                reports.push((kind, backend, mode, regime, payload_bytes, summary));
            }
        }
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let result = (|| -> io::Result<()> {
        writeln!(output, "jsonmax deterministic benchmark")?;
        writeln!(output, "Allocator: {}", active_allocator())?;
        writeln!(
            output,
            "Allocator calls: {} alloc/realloc/zeroed, {} dealloc",
            ALLOC_CALLS.load(Ordering::Relaxed),
            DEALLOC_CALLS.load(Ordering::Relaxed)
        )?;
        writeln!(
            output,
            "Seed: {} | Documents/workload: {} | Items/document: {} | Warmup: {} | Measured: {}",
            options.corpus.seed,
            options.corpus.documents,
            options.corpus.items_per_document,
            options.benchmark.warmup_iterations,
            options.benchmark.measured_iterations
        )?;
        writeln!(
            output,
            "Times and Payload Bytes describe complete corpus passes; MB/s uses decimal MB."
        )?;
        writeln!(output, "SIMD dispatch for each workload's first payload:")?;
        for (name, backend, _, _, _, _) in reports.iter().step_by(8) {
            writeln!(
                output,
                "  {name}: {}",
                jmax_dispatch::backend_name(*backend)
            )?;
        }
        writeln!(
            output,
            "Extract validates complete schemas and captures fields on every workload. Warm scratch is prewarmed on the entire corpus."
        )?;
        writeln!(output)?;
        writeln!(
            output,
            "| Workload       | Mode      | Allocation regime | Payload Bytes | Min (ns) | Median (ns) | P95 (ns) | Throughput (MB/s) | ns/op | Msg/sec |"
        )?;
        writeln!(
            output,
            "|----------------|-----------|-------------------|--------------:|---------:|------------:|---------:|------------------:|------:|--------:|"
        )?;
        for (name, _, mode, regime, payload_bytes, summary) in &reports {
            write_row(&mut output, name, *mode, regime, *payload_bytes, summary)?;
        }
        output.flush()
    })();
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(format!("cannot write results: {error}")),
    }
}

fn write_row(
    output: &mut dyn Write,
    name: &str,
    mode: BenchmarkMode,
    regime: &str,
    payload_bytes: u64,
    summary: &Summary,
) -> io::Result<()> {
    // A clock with insufficient resolution must not yield infinite rates.
    let (throughput, ns_per_op, messages) = if summary.elapsed_ns == 0 || summary.documents == 0 {
        (
            String::from("n/a"),
            String::from("n/a"),
            String::from("n/a"),
        )
    } else {
        let elapsed = summary.elapsed_ns as f64;
        (
            format!("{:.2}", summary.input_bytes as f64 * 1_000.0 / elapsed),
            format!("{:.2}", elapsed / summary.documents as f64),
            format!(
                "{:.0}",
                summary.documents as f64 * 1_000_000_000.0 / elapsed
            ),
        )
    };
    writeln!(
        output,
        "| {name:<14} | {:<9} | {regime} | {payload_bytes:>13} | {:>8} | {:>11} | {:>8} | {throughput:>17} | {ns_per_op:>5} | {messages:>7} |",
        mode_name(mode),
        summary.min_ns,
        summary.median_ns,
        summary.p95_ns
    )
}

fn options() -> Result<Option<Options>, String> {
    let mut result = Options::default();
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == "--help" || argument == "-h" {
            let stdout = io::stdout();
            match stdout.lock().write_all(HELP.as_bytes()) {
                Ok(()) => return Ok(None),
                Err(error) if error.kind() == io::ErrorKind::BrokenPipe => return Ok(None),
                Err(error) => return Err(format!("cannot write help: {error}")),
            }
        }
        let (flag, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };
        if !matches!(
            flag,
            "--warmup"
                | "--iterations"
                | "--documents"
                | "--items"
                | "--seed"
                | "--max-corpus-bytes"
                | "--max-input-bytes"
        ) {
            return Err(format!("unknown option {flag:?}; use --help"));
        }
        let value = inline
            .or_else(|| arguments.next())
            .ok_or_else(|| format!("{flag} requires an integer"))?;
        let value = parse_integer(flag, &value)?;
        match flag {
            "--warmup" => result.benchmark.warmup_iterations = count(flag, value)?,
            "--iterations" => result.benchmark.measured_iterations = count(flag, value)?,
            "--documents" => result.corpus.documents = count(flag, value)?,
            "--items" => result.corpus.items_per_document = count(flag, value)?,
            "--seed" => result.corpus.seed = value,
            "--max-corpus-bytes" => result.corpus.max_bytes = value,
            "--max-input-bytes" => result.benchmark.max_total_input_bytes = value,
            _ => unreachable!("options were validated above"),
        }
    }
    Ok(Some(result))
}

fn parse_integer(flag: &str, text: &str) -> Result<u64, String> {
    let (digits, radix) = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(digits) => (digits, 16),
        None => (text, 10),
    };
    if digits.is_empty()
        || !digits.bytes().all(|byte| match radix {
            16 => byte.is_ascii_hexdigit(),
            _ => byte.is_ascii_digit(),
        })
    {
        return Err(format!("{flag} requires an unsigned integer, got {text:?}"));
    }
    u64::from_str_radix(digits, radix)
        .map_err(|_| format!("{flag} exceeds the unsigned 64-bit range"))
}

fn count(flag: &str, value: u64) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| format!("{flag} exceeds the unsigned 32-bit range"))
}

fn mode_name(mode: BenchmarkMode) -> &'static str {
    match mode {
        BenchmarkMode::Parse => "Parse",
        BenchmarkMode::Validate => "Validate",
        BenchmarkMode::Extract => "Extract",
        BenchmarkMode::Serialize => "Serialize",
        BenchmarkMode::Escape => "Escape",
        BenchmarkMode::Format => "Format",
        BenchmarkMode::Service => "Service",
    }
}
