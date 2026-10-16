#![allow(non_snake_case)]
#[global_allocator] static GLOBAL: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;

mod common;
use common::*;
use std::{fs, hint::black_box, io::Write, time::Instant};
struct Job<'a> {
    name: String,
    run: Box<dyn FnMut() -> R<()> + 'a>,
    reps: usize,
    samples: Vec<f64>,
}
fn add<'a, T: 'a>(jobs: &mut Vec<Job<'a>>, name: String, mut f: impl FnMut() -> R<T> + 'a) {
    jobs.push(Job { name, run: Box::new(move || { black_box(f()?); Ok(()) }), reps: 1, samples: vec![] });
}
fn typed<'a, T: PartialEq + std::fmt::Debug + serde::de::DeserializeOwned + 'a>(
    jobs: &mut Vec<Job<'a>>, name: &str, b: &'a [u8], p: &'a Plan, f: fn(&[u8], &Plan) -> R<T>, serde: bool,
) -> R<()> {
    let expected: T = serde_json::from_slice(b)?;
    assert_eq!(expected, f(b, p)?);
    add(jobs, format!("{name}\ttyped"), move || f(black_box(b), p));
    if serde {
        add(jobs, format!("{name}\tserde-typed"), move || Ok(serde_json::from_slice::<T>(black_box(b))?));
    }
    Ok(())
}

fn main() -> R<()> {
    if std::env::var_os("DIFF").is_some() { return diff::run(); }
    let dir = std::env::var("JMAX_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    let samples: usize = std::env::var("SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    let filter = std::env::var("FILTER").ok();
    let serde = std::env::var_os("SERDE").is_some();
    let target_ns: f64 = std::env::var("TARGET_NS").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000_000.0);
    let all = ["twitter", "citm_catalog", "canada", "github_events", "apache_builds", "endpoint_batch"];
    let corpora = std::env::var("CORPORA").ok();
    let names: Vec<&str> = all.iter().copied().filter(|n| corpora.as_ref().map_or(true, |c| c.split(',').any(|x| x == *n))).collect();
    let mut files = vec![];
    for name in &names { files.push(fs::read(format!("{dir}/{name}.json"))?); }
    let mut docs = vec![];
    let mut values = vec![];
    let mut plans = vec![];
    for (i, name) in names.iter().enumerate() {
        let b = files[i].as_slice();
        let doc = jsonmax::parse::parse_slice(b, options())?;
        let reference: serde_json::Value = serde_json::from_slice(b)?;
        let encoded = jsonmax::encode::document_to_vec(&doc, Default::default())?;
        assert_eq!(reference, serde_json::from_slice::<serde_json::Value>(&encoded)?, "round trip {name}");
        let mut bad = b.to_vec();
        bad.extend_from_slice(b" false");
        assert!(walk(&bad).is_err());
        assert!(jsonmax::parse::parse_borrowed(&bad, options()).is_err());
        docs.push(doc);
        values.push(reference);
        plans.push(plan(name)?);
    }
    let small = small_store(&dir)?;
    let mut anys = vec![];
    let mut queries = vec![];
    for name in &names {
        anys.push(jsonmax::endpoint::compile_native_schema(&Schema { name: "any".into(), schema_id: jsonmax::SchemaId(2), root: TypeId(0), nodes: vec![TypeNode::Any], definitions: vec![] }, CompileLimits::default_limits())?);
        let path = match *name { "twitter" => "search_metadata.query", "citm_catalog" => "venueNames.PLEYEL_PLEYEL", "canada" => "type", "github_events" => "[0].id", "apache_builds" => "mode", _ => "batch_id" };
        queries.push(jsonmax::query::compile_paths(&[path])?);
    }
    let mut jobs: Vec<Job> = vec![];
    for (i, name) in names.iter().enumerate() {
        let b = files[i].as_slice();
        let doc = &docs[i];
        let val = &values[i];
        let p = &plans[i];
        add(&mut jobs, format!("{name}\tscan"), move || walk(black_box(b)));
        let any = &anys[i];
        add(&mut jobs, format!("{name}\tvm-any"), move || Ok(jsonmax::endpoint::validate_payload(Payload::from_slice(black_box(b)), any, options())?));
        add(&mut jobs, format!("{name}\tquery"), {
            let q = &queries[i];
            move || Ok(jsonmax::query::execute_borrowed(q, black_box(b), jsonmax::QueryOptions { mode: jsonmax::QueryMode::Query, ..Default::default() })?)
        });
        add(&mut jobs, format!("{name}\tdom-borrowed"), move || Ok(jsonmax::parse::parse_borrowed(black_box(b), options())?));
        add(&mut jobs, format!("{name}\tdom-owned"), move || Ok(jsonmax::parse::parse_slice(black_box(b), options())?));
        match *name {
            "twitter" => typed(&mut jobs, name, b, p, jmax_twitter, serde)?,
            "citm_catalog" => typed(&mut jobs, name, b, p, jmax_citm, serde)?,
            "canada" => typed(&mut jobs, name, b, p, jmax_canada, serde)?,
            "github_events" => typed(&mut jobs, name, b, p, jmax_github, serde)?,
            "apache_builds" => typed(&mut jobs, name, b, p, jmax_apache, serde)?,
            _ => typed(&mut jobs, name, b, p, jmax_endpoint, serde)?,
        }
        add(&mut jobs, format!("{name}\tserialize"), move || {
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_document_streaming(black_box(doc), &mut sink, Default::default())?;
            Ok(sink.into_vec())
        });
        if serde {
            add(&mut jobs, format!("{name}\tserde-scan"), move || Ok(serde_json::from_slice::<serde::de::IgnoredAny>(black_box(b))?));
            add(&mut jobs, format!("{name}\tserde-dom"), move || Ok(serde_json::from_slice::<serde_json::Value>(black_box(b))?));
            add(&mut jobs, format!("{name}\tserde-serialize"), move || Ok(serde_json::to_vec(black_box(val))?));
        }
    }
    if corpora.as_ref().map_or(true, |c| c.contains("small")) { small_jobs(&mut jobs, &dir, &small)?; }
    if let Some(f) = &filter {
        let pats: Vec<&str> = f.split(',').collect();
        jobs.retain(|j| pats.iter().any(|p| j.name.contains(p)));
    }
    for job in &mut jobs {
        (job.run)()?;
        let start = Instant::now();
        let mut count = 0;
        let fixed = std::env::var_os("FIXED").is_some();
        while count < 3 || (!fixed && start.elapsed().as_millis() < 20) { (job.run)()?; count += 1; }
        let ns = start.elapsed().as_nanos() as f64 / count as f64;
        job.reps = (target_ns / ns).ceil().clamp(1., 65536.) as usize;
    }
    let mut state = 0x6a09e667f3bcc909u64;
    for _ in 0..samples {
        let mut order: Vec<_> = (0..jobs.len()).collect();
        for i in (1..order.len()).rev() {
            state ^= state << 13; state ^= state >> 7; state ^= state << 17;
            order.swap(i, state as usize % (i + 1));
        }
        for i in order {
            let job = &mut jobs[i];
            let start = Instant::now();
            for _ in 0..job.reps { (job.run)()?; }
            job.samples.push(start.elapsed().as_nanos() as f64 / job.reps as f64);
        }
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    for job in &mut jobs {
        job.samples.sort_by(f64::total_cmp);
        let med = job.samples[(job.samples.len() - 1) / 2];
        writeln!(out, "{}\t{:.1}", job.name, med)?;
    }
    Ok(())
}
mod diff;

fn small_jobs<'a>(jobs: &mut Vec<Job<'a>>, dir: &str, store: &'a [(String, Vec<u8>, Plan)]) -> R<()> {
    let _ = dir;
    for (name, b, p) in store {
        let b = b.as_slice();
        add(jobs, format!("{name}\tscan"), move || walk(black_box(b)));
        add(jobs, format!("{name}\tdom-borrowed"), move || Ok(jsonmax::parse::parse_borrowed(black_box(b), options())?.node_count()));
        add(jobs, format!("{name}\tserialize"), move || {
            let doc = jsonmax::parse::parse_borrowed(black_box(b), options())?;
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_borrowed_document_streaming(&doc, &mut sink, Default::default())?;
            Ok(sink.into_vec())
        });
        add(jobs, format!("{name}\ttyped"), move || {
            let s = jsonmax::endpoint::extract_borrowed(&p.schema, &p.selection, black_box(b), options())?;
            Ok(s.slots.len())
        });
    }
    Ok(())
}
fn small_store(dir: &str) -> R<Vec<(String, Vec<u8>, Plan)>> {
    use ValueKind::*;
    Ok(vec![
        ("rpc_small".into(), fs::read(format!("{dir}/rpc_small.json"))?, make_plan("rpc", &[(&["method"], String), (&["seq"], Integer), (&["params", "account_id"], String)])?),
        ("binance_small".into(), fs::read(format!("{dir}/binance_small.json"))?, make_plan("bn", &[(&["e"], String), (&["E"], Integer), (&["p"], String), (&["m"], Boolean)])?),
    ])
}
