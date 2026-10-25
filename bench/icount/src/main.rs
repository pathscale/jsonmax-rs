// jmax-icount CORPUS CLASS PARSER ITERS: 3 warm-up calls, then ITERS calls of one
// benchmark closure, mirroring bench/harness/benches/compare.rs. Measure with 0 and N iterations
// and divide the difference by N; Linux uses callgrind and Darwin uses process counters.
#[global_allocator]
static GLOBAL: rusty_alloc::RustyAlloc = rusty_alloc::RustyAlloc;
#[path = "../../harness/src/common.rs"]
mod common;
#[path = "../../metrics.rs"]
mod metrics;
use common::*;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::hint::black_box;
fn err<E: std::fmt::Display>(e: E) -> Box<dyn std::error::Error> { e.to_string().into() }
type F<'a> = Box<dyn FnMut() -> R<()> + 'a>;
fn typed<'a, T: DeserializeOwned + 'a>(parser: &str, b: &'a [u8], p: &'a Plan, f: fn(&[u8], &Plan, &mut jsonmax::endpoint::ValidationScratch) -> R<T>) -> F<'a> {
    match parser {
        "jmax" => { let mut scratch = jsonmax::endpoint::ValidationScratch::default(); Box::new(move || { black_box(f(black_box(b), p, &mut scratch)?); Ok(()) }) },
        "serde_json" => Box::new(move || { black_box(serde_json::from_slice::<T>(black_box(b))?); Ok(()) }),
        "simd-json" => { let mut buf = b.to_vec(); Box::new(move || { buf.copy_from_slice(b); black_box(simd_json::serde::from_slice::<T>(black_box(&mut buf)).map_err(err)?); Ok(()) }) }
        "sonic-rs" => Box::new(move || { black_box(sonic_rs::from_slice::<T>(black_box(b)).map_err(err)?); Ok(()) }),
        _ => panic!("parser"),
    }
}
fn main() -> R<()> {
    let a: Vec<String> = std::env::args().collect();
    let (name, class, parser, iters) = (a[1].as_str(), a[2].as_str(), a[3].as_str(), a[4].parse::<u64>()?);
    let b = std::fs::read(format!("{}/../data/{name}.json", env!("CARGO_MANIFEST_DIR")))?;
    let b = b.as_slice();
    let p = plan(name)?;
    let p = &p;
    let mut f: F = match (class, parser) {
        ("validate", "jmax") => Box::new(move || { black_box(walk(black_box(b))?); Ok(()) }),
        ("validate", "serde_json") => Box::new(move || { black_box(serde_json::from_slice::<IgnoredAny>(black_box(b))?); Ok(()) }),
        ("validate", "sonic-rs-ignored") => Box::new(move || { black_box(sonic_rs::from_slice::<IgnoredAny>(black_box(b)).map_err(err)?); Ok(()) }),
        ("validate", "jiter") => Box::new(move || { let mut it = jiter::Jiter::new(black_box(b)); it.next_skip().map_err(err)?; it.finish().map_err(err)?; Ok(()) }),
        ("dom-owned", "jmax") => Box::new(move || { black_box(jsonmax::parse::parse_slice(black_box(b), options())?); Ok(()) }),
        ("dom-owned", "serde_json") => Box::new(move || { black_box(serde_json::from_slice::<serde_json::Value>(black_box(b))?); Ok(()) }),
        ("dom-owned", "sonic-rs") => Box::new(move || { black_box(sonic_rs::from_slice::<sonic_rs::Value>(black_box(b)).map_err(err)?); Ok(()) }),
        ("dom-owned", "simd-json") => { let mut buf = b.to_vec(); Box::new(move || { buf.copy_from_slice(b); black_box(simd_json::to_owned_value(black_box(&mut buf)).map_err(err)?); Ok(()) }) }
        ("dom-owned", "jiter") => Box::new(move || { black_box(jiter::JsonValue::parse(black_box(b), false).map_err(err)?.into_static()); Ok(()) }),
        ("dom-owned", "json-rust") => Box::new(move || { black_box(json::parse(std::str::from_utf8(black_box(b)).map_err(err)?).map_err(err)?); Ok(()) }),
        ("dom-borrowed", "jmax") => Box::new(move || { black_box(jsonmax::parse::parse_borrowed(black_box(b), options())?); Ok(()) }),
        ("dom-borrowed", "simd-json-tape") => { let mut buf = b.to_vec(); Box::new(move || { buf.copy_from_slice(b); black_box(simd_json::to_tape(black_box(&mut buf)).map_err(err)?); Ok(()) }) }
        ("dom-borrowed", "jiter") => Box::new(move || { black_box(jiter::JsonValue::parse(black_box(b), false).map_err(err)?); Ok(()) }),
        ("serialize", "jmax") => {
            let doc = jsonmax::parse::parse_slice(b, options())?;
            Box::new(move || {
                let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
                jsonmax::encode::encode_document_streaming(black_box(&doc), &mut sink, Default::default())?;
                black_box(sink.into_vec()); Ok(())
            })
        }
        ("serialize", "serde_json") => { let doc: serde_json::Value = serde_json::from_slice(b)?; Box::new(move || { black_box(serde_json::to_vec(black_box(&doc))?); Ok(()) }) }
        ("serialize", "sonic-rs") => { let doc: sonic_rs::Value = sonic_rs::from_slice(b).map_err(err)?; Box::new(move || { black_box(sonic_rs::to_vec(black_box(&doc)).map_err(err)?); Ok(()) }) }
        ("serialize", "simd-json") => { let mut buf = b.to_vec(); let doc = simd_json::to_owned_value(&mut buf).map_err(err)?; Box::new(move || { black_box(simd_json::to_vec(black_box(&doc)).map_err(err)?); Ok(()) }) }
        ("typed", pr) if pr == "jmax-native" => {
            let mut scratch = common::native::NativeScratch::default();
            Box::new(move || {
                black_box(common::native::project(name, black_box(b), &mut scratch)?);
                Ok(())
            })
        }
        ("typed", pr) => match name {
            "twitter" => typed(pr, b, p, jmax_twitter_with_scratch),
            "citm_catalog" => typed(pr, b, p, jmax_citm_with_scratch),
            "canada" => typed(pr, b, p, jmax_canada_with_scratch),
            "github_events" => typed(pr, b, p, jmax_github_with_scratch),
            "apache_builds" => typed(pr, b, p, jmax_apache_with_scratch),
            _ => typed(pr, b, p, jmax_endpoint_with_scratch),
        },
        _ => panic!("class/parser"),
    };
    for _ in 0..3 { f()?; }
    let before = metrics::snapshot();
    for _ in 0..iters { f()?; }
    let after = metrics::snapshot();
    if cfg!(target_os = "macos") {
        let (a, b) = before.zip(after).ok_or("hardware counters unavailable")?;
        if a.instructions == 0 || b.instructions <= a.instructions { return Err("instruction counter unavailable".into()); }
        println!("{}", b.instructions - a.instructions);
    }
    Ok(())
}
