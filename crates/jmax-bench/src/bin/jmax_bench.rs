//! Deterministic, single-process benchmark CLI. Corpus generation is untimed.
#![forbid(unsafe_code)]

use std::{env, io::{self, Write}, process::ExitCode};

use jmax_bench::{
    corpus, parse, serialize, BenchmarkConfig, BenchmarkMode,
    CorpusConfig, Summary, SystemClock, WorkloadKind,
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
NumberArray and Nested use strict native container schemas with empty selections;
Extract still validates the complete document and publishes its validation proof.
";

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
    let Some(options) = options()? else { return Ok(()); };
    options.benchmark.validate().map_err(|error| error.to_string())?;
    let suite = corpus::make_suite(options.corpus).map_err(|error| error.to_string())?;
    // Retain only summaries until every workload completes. Sample vectors are
    // released between modes and do not inflate later measured working sets.
    let mut reports = Vec::new();
    reports.try_reserve_exact(suite.len() * 4)
        .map_err(|_| String::from("cannot allocate benchmark report table"))?;
    let mut clock = SystemClock::new();
    for workload in &suite {
        let first = workload.payloads.first()
            .ok_or_else(|| String::from("generated workload is empty"))?;
        let backend = jmax_dispatch::dispatch(first.len(), jmax_dispatch::DispatchPolicy::default())
            .map_err(|error| error.to_string())?.backend();
        let kind = workload_name(workload.kind);
        for mode in [BenchmarkMode::Parse, BenchmarkMode::Validate,
            BenchmarkMode::Extract, BenchmarkMode::Serialize]
        {
            let report = match mode {
                BenchmarkMode::Parse => parse::run_parse(workload, options.benchmark, &mut clock),
                BenchmarkMode::Validate => parse::run_validate(workload, options.benchmark, &mut clock),
                BenchmarkMode::Extract => parse::run_extract(workload, options.benchmark, &mut clock),
                BenchmarkMode::Serialize => serialize::run_serialize(workload, options.benchmark, &mut clock),
                _ => unreachable!("CLI modes are fixed"),
            }.map_err(|error| format!("{kind}/{}: {error}", mode_name(mode)))?;
            let payload_bytes = report.samples.first().map_or(0, |sample| sample.input_bytes);
            let summary = report.summarize();
            reports.push((kind, backend, mode, payload_bytes, summary));
        }
    }

    let stdout = io::stdout();
    let mut output = stdout.lock();
    let result = (|| -> io::Result<()> {
        writeln!(output, "jsonmax deterministic benchmark")?;
        writeln!(output, "Seed: {} | Documents/workload: {} | Items/document: {} | Warmup: {} | Measured: {}",
            options.corpus.seed, options.corpus.documents, options.corpus.items_per_document,
            options.benchmark.warmup_iterations, options.benchmark.measured_iterations)?;
        writeln!(output, "Times and Payload Bytes describe complete corpus passes; MB/s uses decimal MB.")?;
        writeln!(output, "SIMD dispatch for each workload's first payload:")?;
        for (name, backend, _, _, _) in reports.iter().step_by(4) {
            writeln!(output, "  {name}: {}", jmax_dispatch::backend_name(*backend))?;
        }
        writeln!(output, "Extract uses empty selections for NumberArray and Nested; full validation still runs.")?;
        writeln!(output)?;
        writeln!(output, "| Workload       | Mode      | Payload Bytes | Min (ns) | Median (ns) | P95 (ns) | Throughput (MB/s) | Msg/sec |")?;
        writeln!(output, "|----------------|-----------|--------------:|---------:|------------:|---------:|------------------:|--------:|")?;
        for (name, _, mode, payload_bytes, summary) in &reports {
            write_row(&mut output, name, *mode, *payload_bytes, summary)?;
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
    payload_bytes: u64,
    summary: &Summary,
) -> io::Result<()> {
    // A clock with insufficient resolution must not yield infinite rates.
    let (throughput, messages) = if summary.elapsed_ns == 0 {
        (String::from("n/a"), String::from("n/a"))
    } else {
        let elapsed = summary.elapsed_ns as f64;
        (format!("{:.2}", summary.input_bytes as f64 * 1_000.0 / elapsed),
            format!("{:.0}", summary.documents as f64 * 1_000_000_000.0 / elapsed))
    };
    writeln!(output, "| {name:<14} | {:<9} | {payload_bytes:>13} | {:>8} | {:>11} | {:>8} | {throughput:>17} | {messages:>7} |",
        mode_name(mode), summary.min_ns, summary.median_ns, summary.p95_ns)
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
        if !matches!(flag, "--warmup" | "--iterations" | "--documents" | "--items"
            | "--seed" | "--max-corpus-bytes" | "--max-input-bytes")
        {
            return Err(format!("unknown option {flag:?}; use --help"));
        }
        let value = inline.or_else(|| arguments.next())
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
    if digits.is_empty() || !digits.bytes().all(|byte| match radix {
        16 => byte.is_ascii_hexdigit(),
        _ => byte.is_ascii_digit(),
    }) {
        return Err(format!("{flag} requires an unsigned integer, got {text:?}"));
    }
    u64::from_str_radix(digits, radix)
        .map_err(|_| format!("{flag} exceeds the unsigned 64-bit range"))
}

fn count(flag: &str, value: u64) -> Result<u32, String> {
    u32::try_from(value).map_err(|_| format!("{flag} exceeds the unsigned 32-bit range"))
}

fn workload_name(kind: WorkloadKind) -> &'static str {
    match kind {
        WorkloadKind::SmallObject => "SmallObject",
        WorkloadKind::EscapedStrings => "EscapedStrings",
        WorkloadKind::Unicode => "Unicode",
        WorkloadKind::NumberArray => "NumberArray",
        WorkloadKind::DepthBook => "DepthBook",
        WorkloadKind::AggTrade => "AggTrade",
        WorkloadKind::Nested => "Nested",
    }
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
