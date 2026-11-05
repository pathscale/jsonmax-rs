use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type Key = (String, String, String);
type Rates = BTreeMap<Key, f64>;
type Bytes = BTreeMap<Key, usize>;

const HEADER: &str = "corpus\tclass\tparser\tbytes\tMB/s\tp50_ns\tp95_ns\tinstructions/call\tcycles/call\tallocs/call\treq_bytes/call\tpeak_live_heap_B\tretained_B\tmax_overhead_B";
const CORPORA: &[&str] = &["twitter", "citm_catalog", "canada", "github_events", "apache_builds", "endpoint_batch"];

fn required<'a>(args: &'a [String], index: usize, name: &str) -> Result<&'a str> {
    args.get(index).map(String::as_str).ok_or_else(|| format!("missing {name}").into())
}

fn positive<T: std::str::FromStr + PartialOrd + Default>(value: &str, name: &str) -> Result<T> {
    let parsed = value.parse::<T>().map_err(|_| format!("invalid {name}"))?;
    if parsed <= T::default() { return Err(format!("{name} must be positive").into()); }
    Ok(parsed)
}

fn run(binary: &str, corpora: &str, data: Option<&str>, samples: Option<&str>, filter: Option<&str>) -> Result<(Rates, Bytes)> {
    let mut command = Command::new(binary);
    command.env("CORPORA", corpora).env_remove("FILTER");
    if let Some(filter) = filter { command.env("FILTER", filter); }
    if let Some(path) = data { command.env("JMAX_DATA", path); }
    if let Some(count) = samples { command.env("SAMPLES", count); }
    let output = command.output()?;
    if !output.status.success() {
        return Err(format!("benchmark process {binary} failed: {}", String::from_utf8_lossy(&output.stderr).trim()).into());
    }
    let text = std::str::from_utf8(&output.stdout)?;
    let mut lines = text.lines();
    if lines.next() != Some(HEADER) { return Err(format!("{binary} emitted an unexpected TSV header").into()); }
    let mut rates = Rates::new();
    let mut bytes = Bytes::new();
    for (line_no, line) in lines.enumerate() {
        if line.is_empty() { continue; }
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 14 { return Err(format!("{binary} row {} has {} columns", line_no + 2, fields.len()).into()); }
        let key = (fields[0].to_owned(), fields[1].to_owned(), fields[2].to_owned());
        let size = fields[3].parse::<usize>().map_err(|_| "invalid byte denominator")?;
        let numbers = [3usize, 4, 5, 6, 9, 10, 11, 12, 13].map(|i| fields[i].parse::<f64>());
        if numbers.iter().any(|number| number.is_err()) { return Err("malformed numeric measurement field".into()); }
        let values: Vec<f64> = numbers.into_iter().map(|n| n.unwrap()).collect();
        if size == 0 || values.iter().any(|n| !n.is_finite()) || values[1..4].iter().any(|n| *n <= 0.0)
            || values[4..].iter().any(|n| *n < 0.0) || values[8] != values[6] + values[7] {
            return Err("invalid numeric measurement field or memory sum".into());
        }
        let rate = values[1];
        if rates.insert(key.clone(), rate).is_some() { return Err("duplicate benchmark row".into()); }
        bytes.insert(key, size);
    }
    if rates.is_empty() { return Err(format!("{binary} produced no matched rows").into()); }
    Ok((rates, bytes))
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[(values.len() - 1) / 2]
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 8 || args.len() > 11 {
        return Err("usage: paired BASELINE_PATH BASELINE_ID CANDIDATE_PATH CANDIDATE_ID CONTRACT CORPORA_CSV BLOCKS MIN_RATIO [DATA_DIR] [SAMPLES] [FILTER]".into());
    }
    let baseline = required(&args, 0, "baseline path")?;
    let baseline_id = required(&args, 1, "baseline identity")?;
    let candidate = required(&args, 2, "candidate path")?;
    let candidate_id = required(&args, 3, "candidate identity")?;
    let contract = required(&args, 4, "contract label")?;
    let corpora_arg = required(&args, 5, "corpora selection")?;
    let blocks = positive::<usize>(required(&args, 6, "block count")?, "block count")?;
    let threshold = positive::<f64>(required(&args, 7, "minimum ratio")?, "minimum ratio")?;
    if !threshold.is_finite() { return Err("minimum ratio must be finite".into()); }
    if [baseline_id, candidate_id, contract].iter().any(|s| s.trim().is_empty() || s.contains('\t')) {
        return Err("identities and contract must be nonempty and contain no tabs".into());
    }
    let selected: Vec<&str> = corpora_arg.split(',').collect();
    let selected_set: BTreeSet<_> = selected.iter().copied().collect();
    if selected.is_empty() || selected.iter().any(|s| !CORPORA.contains(s) || s.is_empty()) || selected_set.len() != selected.len() {
        return Err("corpora must be a nonempty, unique selection supported by the harness".into());
    }
    let data = args.get(8).map(String::as_str);
    let samples = args.get(9).map(String::as_str);
    let filter = args.get(10).map(String::as_str);
    if filter.is_some_and(|value| value.split(',').any(|pattern| pattern.is_empty())) {
        return Err("filter patterns must be nonempty".into());
    }
    if let Some(count) = samples { positive::<usize>(count, "sample count")?; }

    let mut reference_keys: Option<BTreeSet<Key>> = None;
    let mut denominators = Bytes::new();
    let mut ratios: BTreeMap<Key, Vec<f64>> = BTreeMap::new();
    let mut observed = 0usize;
    let mut retry_used = false;
    loop {
        let start = observed;
        let end = if start == 0 { blocks } else { start + 1 };
        while observed < end {
            let (b0, b0_bytes) = run(baseline, corpora_arg, data, samples, filter)?;
            let (c0, c0_bytes) = run(candidate, corpora_arg, data, samples, filter)?;
            let (c1, c1_bytes) = run(candidate, corpora_arg, data, samples, filter)?;
            let (b1, b1_bytes) = run(baseline, corpora_arg, data, samples, filter)?;
            let keys: BTreeSet<_> = b0.keys().cloned().collect();
            let expected = reference_keys.get_or_insert_with(|| keys.clone());
            if &keys != expected || c0.keys().cloned().collect::<BTreeSet<_>>() != *expected
                || c1.keys().cloned().collect::<BTreeSet<_>>() != *expected || b1.keys().cloned().collect::<BTreeSet<_>>() != *expected {
                return Err("missing or unexpected row identity in paired observation".into());
            }
            if expected.iter().any(|key| !selected_set.contains(key.0.as_str()))
                || !selected_set.iter().all(|corpus| expected.iter().any(|key| key.0 == *corpus)) {
                return Err("missing selected corpus rows or unexpected corpus identity".into());
            }
            for key in expected.iter() {
                let sizes = [&b0_bytes, &c0_bytes, &c1_bytes, &b1_bytes].map(|m| m.get(key).copied());
                if sizes.iter().any(Option::is_none) || sizes.iter().any(|s| *s != sizes[0]) { return Err("missing or unequal byte denominator".into()); }
                if let Some(previous) = denominators.insert(key.clone(), sizes[0].unwrap()) {
                    if previous != sizes[0].unwrap() { return Err("byte denominator changed between observations".into()); }
                }
                let base_rate = (b0[key] + b1[key]) / 2.0;
                let candidate_rate = (c0[key] + c1[key]) / 2.0;
                let ratio = candidate_rate / base_rate;
                if !ratio.is_finite() || ratio <= 0.0 { return Err("invalid paired ratio".into()); }
                ratios.entry(key.clone()).or_default().push(ratio);
            }
            observed += 1;
        }
        let failing = ratios.values_mut().any(|values| {
            let mut copy = values.clone();
            median(&mut copy) < threshold
        });
        if start == 0 && failing { retry_used = true; continue; }
        break;
    }

    println!("baseline={baseline_id}\tcandidate={candidate_id}\tcontract={contract}\tcorpora={corpora_arg}\tblocks={blocks}\tretry_block={retry_used}\tthreshold={threshold}");
    let mut passed = true;
    for (key, samples) in ratios {
        let best = samples.iter().copied().max_by(f64::total_cmp).unwrap();
        let med = median(&mut samples.clone());
        let ok = med >= threshold;
        passed &= ok;
        println!("{}\t{}\t{}\tbytes={}\tsamples={}\tbest_ratio={:.6}\tmedian_ratio={:.6}\tperformance={}", key.0, key.1, key.2, denominators[&key], samples.len(), best, med, if ok { "PASS" } else { "FAIL" });
    }
    if !passed { return Err("performance threshold was not met after the single bounded retry".into()); }
    Ok(())
}
