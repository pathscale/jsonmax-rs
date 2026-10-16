//! Cross-parser comparison. Competitors are dev-dependencies of this crate only.
//!
//! Prints one TSV line per job: `corpus  class  parser  bytes  median_ns`. `bench/compare.sh`
//! runs several interleaved rounds of this binary and aggregates them into MB/s tables.
//!
//! Classes (jmax's own row of the same class is the 1.0 baseline):
//!   validate      syntax check or skip, no value built
//!   dom-owned     fully materialised owned tree
//!   dom-borrowed  tree/tape borrowing the input
//!   typed         the same fields extracted into the structs in src/common.rs
//!   serialize     write a pre-built tree back to bytes
#[global_allocator]
static GLOBAL: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;

#[path = "../src/common.rs"]
mod common;
use common::*;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::{fmt::Debug, fs, hint::black_box, io::Write, time::Instant};

struct Job<'a> {
    corpus: String,
    class: &'static str,
    parser: &'static str,
    bytes: usize,
    run: Box<dyn FnMut() -> R<()> + 'a>,
    reps: usize,
    samples: Vec<f64>,
}
struct Jobs<'a> {
    v: Vec<Job<'a>>,
    corpus: String,
    bytes: usize,
}
impl<'a> Jobs<'a> {
    fn add(&mut self, class: &'static str, parser: &'static str, mut f: impl FnMut() -> R<()> + 'a) {
        self.v.push(Job { corpus: self.corpus.clone(), class, parser, bytes: self.bytes, run: Box::new(move || f()), reps: 1, samples: vec![] });
    }
}
fn err<E: std::fmt::Display>(e: E) -> Box<dyn std::error::Error> {
    e.to_string().into()
}

fn typed<'a, T: PartialEq + Debug + DeserializeOwned + 'a>(
    j: &mut Jobs<'a>, b: &'a [u8], p: &'a Plan, f: fn(&[u8], &Plan) -> R<T>,
) -> R<()> {
    let expected: T = serde_json::from_slice(b)?;
    assert_eq!(expected, f(b, p)?, "jmax typed {}", j.corpus);
    let mut buf = b.to_vec();
    assert_eq!(expected, simd_json::serde::from_slice::<T>(&mut buf).map_err(err)?, "simd-json typed {}", j.corpus);
    assert_eq!(expected, sonic_rs::from_slice::<T>(b).map_err(err)?, "sonic-rs typed {}", j.corpus);
    j.add("typed", "jmax", move || { black_box(f(black_box(b), p)?); Ok(()) });
    j.add("typed", "serde_json", move || { black_box(serde_json::from_slice::<T>(black_box(b))?); Ok(()) });
    let mut buf = b.to_vec();
    j.add("typed", "simd-json", move || {
        buf.copy_from_slice(b);
        black_box(simd_json::serde::from_slice::<T>(black_box(&mut buf)).map_err(err)?);
        Ok(())
    });
    j.add("typed", "sonic-rs", move || { black_box(sonic_rs::from_slice::<T>(black_box(b)).map_err(err)?); Ok(()) });
    Ok(())
}

fn main() -> R<()> {
    let dir = std::env::var("JMAX_DATA").unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    let samples: usize = std::env::var("SAMPLES").ok().and_then(|s| s.parse().ok()).unwrap_or(9);
    let target_ns: f64 = std::env::var("TARGET_NS").ok().and_then(|s| s.parse().ok()).unwrap_or(2_000_000.0);
    let filter = std::env::var("FILTER").ok();
    let all = ["twitter", "citm_catalog", "canada", "github_events", "apache_builds", "endpoint_batch"];
    let corpora = std::env::var("CORPORA").ok();
    let names: Vec<&str> = all.iter().copied().filter(|n| corpora.as_ref().map_or(true, |c| c.split(',').any(|x| x == *n))).collect();

    let files: Vec<Vec<u8>> = names.iter().map(|n| fs::read(format!("{dir}/{n}.json"))).collect::<Result<_, _>>()?;
    let plans: Vec<Plan> = names.iter().map(|n| plan(n)).collect::<R<_>>()?;
    let docs: Vec<_> = files.iter().map(|b| jsonmax::parse::parse_slice(b, options())).collect::<Result<_, _>>()?;
    let values: Vec<serde_json::Value> = files.iter().map(|b| serde_json::from_slice(b)).collect::<Result<_, _>>()?;
    let sonic_vals: Vec<sonic_rs::Value> = files.iter().map(|b| sonic_rs::from_slice(b)).collect::<Result<_, _>>().map_err(err)?;
    let simd_vals: Vec<simd_json::OwnedValue> = files
        .iter()
        .map(|b| { let mut c = b.clone(); simd_json::to_owned_value(&mut c) })
        .collect::<Result<_, _>>().map_err(err)?;
    let json_vals: Vec<json::JsonValue> = files.iter().map(|b| json::parse(std::str::from_utf8(b).unwrap())).collect::<Result<_, _>>().map_err(err)?;

    let mut j = Jobs { v: vec![], corpus: String::new(), bytes: 0 };
    for (i, name) in names.iter().enumerate() {
        let b = files[i].as_slice();
        j.corpus = name.to_string();
        j.bytes = b.len();
        let (doc, val, p) = (&docs[i], &values[i], &plans[i]);

        // Equivalence of what each serializer writes (parsed back by serde_json).
        let check = |who: &str, out: &[u8]| {
            let ok = serde_json::from_slice::<serde_json::Value>(out).map_or(false, |v| approx_eq(&v, val));
            if !ok { eprintln!("# serialize check: {who} on {name} does not round-trip to the reference Value"); }
        };
        let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
        jsonmax::encode::encode_document_streaming(doc, &mut sink, Default::default())?;
        check("jmax", &sink.into_vec());
        check("serde_json", &serde_json::to_vec(val)?);
        check("sonic-rs", &sonic_rs::to_vec(&sonic_vals[i]).map_err(err)?);
        check("simd-json", &simd_json::to_vec(&simd_vals[i]).map_err(err)?);
        check("json-rust", json_vals[i].dump().as_bytes());

        j.add("ref", "memcpy", { let mut buf = b.to_vec(); move || { buf.copy_from_slice(black_box(b)); black_box(&buf); Ok(()) } });

        // validate / skip
        j.add("validate", "jmax", move || { black_box(walk(black_box(b))?); Ok(()) });
        j.add("validate", "serde_json", move || { black_box(serde_json::from_slice::<IgnoredAny>(black_box(b))?); Ok(()) });
        j.add("validate", "sonic-rs lazy-skip", move || { black_box(sonic_rs::get(black_box(b), &[] as &[&str]).map_err(err)?); Ok(()) });
        j.add("validate", "sonic-rs ignored", move || { black_box(sonic_rs::from_slice::<IgnoredAny>(black_box(b)).map_err(err)?); Ok(()) });
        j.add("validate", "jiter", move || {
            let mut it = jiter::Jiter::new(black_box(b));
            it.next_skip().map_err(err)?;
            it.finish().map_err(err)?;
            Ok(())
        });

        // dom-owned
        j.add("dom-owned", "jmax", move || { black_box(jsonmax::parse::parse_slice(black_box(b), options())?); Ok(()) });
        j.add("dom-owned", "serde_json", move || { black_box(serde_json::from_slice::<serde_json::Value>(black_box(b))?); Ok(()) });
        j.add("dom-owned", "sonic-rs", move || { black_box(sonic_rs::from_slice::<sonic_rs::Value>(black_box(b)).map_err(err)?); Ok(()) });
        j.add("dom-owned", "simd-json owned", { let mut buf = b.to_vec(); move || {
            buf.copy_from_slice(b);
            black_box(simd_json::to_owned_value(black_box(&mut buf)).map_err(err)?);
            Ok(())
        } });
        j.add("dom-owned", "json-rust", move || {
            black_box(json::parse(std::str::from_utf8(black_box(b)).map_err(err)?).map_err(err)?);
            Ok(())
        });
        j.add("dom-owned", "jiter", move || {
            black_box(jiter::JsonValue::parse(black_box(b), false).map_err(err)?.into_static());
            Ok(())
        });

        // dom-borrowed
        j.add("dom-borrowed", "jmax", move || { black_box(jsonmax::parse::parse_borrowed(black_box(b), options())?); Ok(()) });
        j.add("dom-borrowed", "simd-json tape", { let mut buf = b.to_vec(); move || {
            buf.copy_from_slice(b);
            black_box(simd_json::to_tape(black_box(&mut buf)).map_err(err)?);
            Ok(())
        } });
        j.add("dom-borrowed", "simd-json borrowed", { let mut buf = b.to_vec(); move || {
            buf.copy_from_slice(b);
            black_box(simd_json::to_borrowed_value(black_box(&mut buf)).map_err(err)?);
            Ok(())
        } });
        j.add("dom-borrowed", "jiter", move || { black_box(jiter::JsonValue::parse(black_box(b), false).map_err(err)?); Ok(()) });

        // typed
        match *name {
            "twitter" => typed(&mut j, b, p, jmax_twitter)?,
            "citm_catalog" => typed(&mut j, b, p, jmax_citm)?,
            "canada" => typed(&mut j, b, p, jmax_canada)?,
            "github_events" => typed(&mut j, b, p, jmax_github)?,
            "apache_builds" => typed(&mut j, b, p, jmax_apache)?,
            _ => typed(&mut j, b, p, jmax_endpoint)?,
        }

        // serialize
        j.add("serialize", "jmax", move || {
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_document_streaming(black_box(doc), &mut sink, Default::default())?;
            black_box(sink.into_vec());
            Ok(())
        });
        j.add("serialize", "serde_json", move || { black_box(serde_json::to_vec(black_box(val))?); Ok(()) });
        let (sv, dv, jv) = (&sonic_vals[i], &simd_vals[i], &json_vals[i]);
        j.add("serialize", "sonic-rs", move || { black_box(sonic_rs::to_vec(black_box(sv)).map_err(err)?); Ok(()) });
        j.add("serialize", "simd-json", move || { black_box(simd_json::to_vec(black_box(dv)).map_err(err)?); Ok(()) });
        j.add("serialize", "json-rust", move || { black_box(black_box(jv).dump()); Ok(()) });
    }
    let mut jobs = j.v;
    if let Some(f) = &filter {
        let pats: Vec<&str> = f.split(',').collect();
        jobs.retain(|j| pats.iter().any(|p| format!("{}/{}/{}", j.corpus, j.class, j.parser).contains(p)));
    }
    for job in &mut jobs {
        (job.run)()?;
        let start = Instant::now();
        let mut count = 0;
        while count < 3 || start.elapsed().as_millis() < 20 { (job.run)()?; count += 1; }
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
        writeln!(out, "{}\t{}\t{}\t{}\t{:.1}", job.corpus, job.class, job.parser, job.bytes, med)?;
    }
    Ok(())
}

// serde_json's default float parser is not correctly rounded, so a re-parse of its own output can
// differ in the last ulp; compare numbers with a relative tolerance.
fn approx_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    match (a, b) {
        (Number(x), Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y || (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
            _ => x == y,
        },
        (Array(x), Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| approx_eq(p, q)),
        (Object(x), Object(y)) => x.len() == y.len() && x.iter().all(|(k, p)| y.get(k).is_some_and(|q| approx_eq(p, q))),
        _ => a == b,
    }
}
