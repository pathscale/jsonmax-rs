//! General JSON corpus routes. This module intentionally reports these rows
//! under separate corpus names from the Binance native decoder cases.
use crate::common;

use crate::{Op, R, err};
use serde::de::{DeserializeOwned, IgnoredAny};
use std::{fmt::Debug, fs, hint::black_box};

pub const CASES: &[&str] = &[
    "twitter",
    "citm_catalog",
    "canada",
    "github_events",
    "apache_builds",
    "endpoint_batch",
];

fn default_dir() -> String {
    std::env::var("JMAX_DATA")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into())
}

fn check_roundtrip(who: &str, out: &[u8], reference: &serde_json::Value) -> R<()> {
    let got: serde_json::Value = serde_json::from_slice(out)?;
    if !approx_eq(&got, reference) {
        return Err(err(format!(
            "{who} serialization differs from general corpus reference"
        )));
    }
    Ok(())
}

pub fn with_case<T>(
    name: &str,
    f: impl FnOnce(usize, Vec<(&'static str, Op<'_>)>) -> R<T>,
) -> R<T> {
    if !CASES.contains(&name) {
        return Err(err(format!("unknown general case {name}")));
    }
    let path = format!("{}/{name}.json", default_dir());
    let bytes = fs::read(path)?;
    let b = bytes.as_slice();
    let plan = common::plan(name)?;
    let doc = jsonmax::parse::parse_slice(b, common::options())?;
    let reference: serde_json::Value = serde_json::from_slice(b)?;
    let sonic_value: sonic_rs::Value = sonic_rs::from_slice(b).map_err(err)?;
    let mut simd_bytes = b.to_vec();
    let simd_value = simd_json::to_owned_value(&mut simd_bytes).map_err(err)?;
    let json_value = json::parse(std::str::from_utf8(b).map_err(err)?).map_err(err)?;

    // Check each output-producing serializer against the same parsed reference.
    let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
    jsonmax::encode::encode_document_streaming(&doc, &mut sink, Default::default())?;
    check_roundtrip("jmax", &sink.into_vec(), &reference)?;
    check_roundtrip("serde_json", &serde_json::to_vec(&reference)?, &reference)?;
    check_roundtrip(
        "sonic-rs",
        &sonic_rs::to_vec(&sonic_value).map_err(err)?,
        &reference,
    )?;
    check_roundtrip(
        "simd-json",
        &simd_json::to_vec(&simd_value).map_err(err)?,
        &reference,
    )?;
    check_roundtrip("json-rust", json_value.dump().as_bytes(), &reference)?;

    // The parsed jmax DOM and each competitor DOM must preserve the input's
    // complete JSON value. These checks run once before any timed rows.
    let serde_dom: serde_json::Value = serde_json::from_slice(b)?;
    let sonic_dom: sonic_rs::Value = sonic_rs::from_slice(b).map_err(err)?;
    let mut owned_buf = b.to_vec();
    let simd_dom = simd_json::to_owned_value(&mut owned_buf).map_err(err)?;
    let json_dom = json::parse(std::str::from_utf8(b).map_err(err)?).map_err(err)?;
    let mut tape_buf = b.to_vec();
    let tape_dom: serde_json::Value = simd_json::to_tape(&mut tape_buf)
        .map_err(err)?
        .deserialize()
        .map_err(err)?;
    let mut borrowed_buf = b.to_vec();
    let borrowed_dom = simd_json::to_borrowed_value(&mut borrowed_buf).map_err(err)?;
    let jiter_owned = jiter_to_reference(jiter::JsonValue::parse(b, false).map_err(err)?)
        .ok_or_else(|| err("jiter owned DOM cannot be represented by the reference value"))?;
    let jiter_borrowed = jiter_to_reference(jiter::JsonValue::parse(b, false).map_err(err)?)
        .ok_or_else(|| err("jiter borrowed DOM cannot be represented by the reference value"))?;
    if serde_dom != reference
        || sonic_rs::to_vec(&sonic_dom).map_or(true, |v| !roundtrip_eq(&v, &reference))
        || simd_json::to_vec(&simd_dom).map_or(true, |v| !roundtrip_eq(&v, &reference))
        || !roundtrip_eq(json_dom.dump().as_bytes(), &reference)
        || !approx_eq(&tape_dom, &reference)
        || simd_json::to_vec(&borrowed_dom).map_or(true, |v| !roundtrip_eq(&v, &reference))
        || !approx_eq(&jiter_owned, &reference)
        || !approx_eq(&jiter_borrowed, &reference)
    {
        return Err(err(
            "a general DOM parser disagrees with the reference value",
        ));
    }

    let mut jobs: Vec<(&'static str, Op<'_>)> = Vec::new();
    jobs.push((
        "jmax validate",
        Box::new(move || {
            black_box(common::walk(black_box(b))?);
            Ok(())
        }),
    ));
    jobs.push((
        "serde_json validate",
        Box::new(move || {
            black_box(serde_json::from_slice::<IgnoredAny>(black_box(b))?);
            Ok(())
        }),
    ));
    jobs.push((
        "sonic-rs lazy-skip",
        Box::new(move || {
            black_box(sonic_rs::get(black_box(b), &[] as &[&str]).map_err(err)?);
            Ok(())
        }),
    ));
    jobs.push((
        "sonic-rs ignored",
        Box::new(move || {
            black_box(sonic_rs::from_slice::<IgnoredAny>(black_box(b)).map_err(err)?);
            Ok(())
        }),
    ));
    jobs.push((
        "jiter validate",
        Box::new(move || {
            let mut it = jiter::Jiter::new(black_box(b));
            it.next_skip().map_err(err)?;
            it.finish().map_err(err)?;
            Ok(())
        }),
    ));

    jobs.push((
        "jmax dom-owned",
        Box::new(move || {
            black_box(jsonmax::parse::parse_slice(
                black_box(b),
                common::options(),
            )?);
            Ok(())
        }),
    ));
    jobs.push((
        "serde_json dom-owned",
        Box::new(move || {
            black_box(serde_json::from_slice::<serde_json::Value>(black_box(b))?);
            Ok(())
        }),
    ));
    jobs.push((
        "sonic-rs dom-owned",
        Box::new(move || {
            black_box(sonic_rs::from_slice::<sonic_rs::Value>(black_box(b)).map_err(err)?);
            Ok(())
        }),
    ));
    jobs.push((
        "simd-json dom-owned",
        Box::new({
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::to_owned_value(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        }),
    ));
    jobs.push((
        "json-rust dom-owned",
        Box::new(move || {
            black_box(json::parse(std::str::from_utf8(black_box(b)).map_err(err)?).map_err(err)?);
            Ok(())
        }),
    ));
    jobs.push((
        "jiter dom-owned",
        Box::new(move || {
            black_box(
                jiter::JsonValue::parse(black_box(b), false)
                    .map_err(err)?
                    .into_static(),
            );
            Ok(())
        }),
    ));

    jobs.push((
        "jmax dom-borrowed",
        Box::new(move || {
            black_box(jsonmax::parse::parse_borrowed(
                black_box(b),
                common::options(),
            )?);
            Ok(())
        }),
    ));
    jobs.push((
        "simd-json tape",
        Box::new({
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::to_tape(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        }),
    ));
    jobs.push((
        "simd-json borrowed",
        Box::new({
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::to_borrowed_value(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        }),
    ));
    jobs.push((
        "jiter dom-borrowed",
        Box::new(move || {
            black_box(jiter::JsonValue::parse(black_box(b), false).map_err(err)?);
            Ok(())
        }),
    ));

    match name {
        "twitter" => typed_rows(&mut jobs, b, &plan, common::jmax_twitter_with_scratch)?,
        "citm_catalog" => typed_rows(&mut jobs, b, &plan, common::jmax_citm_with_scratch)?,
        "canada" => typed_rows(&mut jobs, b, &plan, common::jmax_canada_with_scratch)?,
        "github_events" => typed_rows(&mut jobs, b, &plan, common::jmax_github_with_scratch)?,
        "apache_builds" => typed_rows(&mut jobs, b, &plan, common::jmax_apache_with_scratch)?,
        _ => typed_rows(&mut jobs, b, &plan, common::jmax_endpoint_with_scratch)?,
    }

    common::native::check(name, b)?;
    let native_name = name.to_owned();
    jobs.push((
        "jmax native typed",
        Box::new({
            let mut scratch = common::native::NativeScratch::default();
            move || {
                black_box(common::native::project(
                    &native_name,
                    black_box(b),
                    &mut scratch,
                )?);
                Ok(())
            }
        }),
    ));
    let (path, pointer) = match name {
        "twitter" => ("$.search_metadata.query", "/search_metadata/query"),
        "citm_catalog" => ("$.venueNames.PLEYEL_PLEYEL", "/venueNames/PLEYEL_PLEYEL"),
        "canada" => ("$.type", "/type"),
        "github_events" => ("$[0].actor.login", "/0/actor/login"),
        "apache_builds" => ("$.mode", "/mode"),
        _ => ("$.batch_id", "/batch_id"),
    };
    jobs.extend(crate::query_rows(b, path, pointer)?);
    jobs.push((
        "jmax serialize",
        Box::new(move || {
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_document_streaming(
                black_box(&doc),
                &mut sink,
                Default::default(),
            )?;
            black_box(sink.into_vec());
            Ok(())
        }),
    ));
    jobs.push((
        "serde_json serialize",
        Box::new(move || {
            black_box(serde_json::to_vec(black_box(&reference))?);
            Ok(())
        }),
    ));
    jobs.push((
        "sonic-rs serialize",
        Box::new(move || {
            black_box(sonic_rs::to_vec(black_box(&sonic_value)).map_err(err)?);
            Ok(())
        }),
    ));
    jobs.push((
        "simd-json serialize",
        Box::new(move || {
            black_box(simd_json::to_vec(black_box(&simd_value)).map_err(err)?);
            Ok(())
        }),
    ));
    jobs.push((
        "json-rust serialize",
        Box::new(move || {
            black_box(black_box(&json_value).dump());
            Ok(())
        }),
    ));
    f(b.len(), jobs)
}

fn typed_rows<'a, T: DeserializeOwned + PartialEq + Debug + 'a>(
    jobs: &mut Vec<(&'static str, Op<'a>)>,
    b: &'a [u8],
    p: &'a common::Plan,
    decode: fn(&[u8], &common::Plan, &mut jsonmax::endpoint::ValidationScratch) -> R<T>,
) -> R<()> {
    let expected: T = serde_json::from_slice(b)?;
    let mut check = jsonmax::endpoint::ValidationScratch::default();
    assert_eq!(expected, decode(b, p, &mut check)?, "jmax typed reference");
    let mut simd_check = b.to_vec();
    assert_eq!(
        expected,
        simd_json::serde::from_slice::<T>(&mut simd_check).map_err(err)?,
        "simd-json typed reference"
    );
    assert_eq!(
        expected,
        sonic_rs::from_slice::<T>(b).map_err(err)?,
        "sonic-rs typed reference"
    );
    jobs.push((
        "jmax vm typed",
        Box::new({
            let mut scratch = jsonmax::endpoint::ValidationScratch::default();
            move || {
                black_box(decode(black_box(b), p, &mut scratch)?);
                Ok(())
            }
        }),
    ));
    jobs.push((
        "serde_json typed",
        Box::new(move || {
            black_box(serde_json::from_slice::<T>(black_box(b))?);
            Ok(())
        }),
    ));
    jobs.push((
        "simd-json typed",
        Box::new({
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::serde::from_slice::<T>(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        }),
    ));
    jobs.push((
        "sonic-rs typed",
        Box::new(move || {
            black_box(sonic_rs::from_slice::<T>(black_box(b)).map_err(err)?);
            Ok(())
        }),
    ));
    Ok(())
}

fn roundtrip_eq(bytes: &[u8], reference: &serde_json::Value) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes).is_ok_and(|got| approx_eq(&got, reference))
}

fn jiter_to_reference(value: jiter::JsonValue<'_>) -> Option<serde_json::Value> {
    use jiter::JsonValue as J;
    use serde_json::Value as S;
    Some(match value {
        J::Null => S::Null,
        J::Bool(v) => S::Bool(v),
        J::Int(v) => S::Number(v.into()),
        J::BigInt(v) => serde_json::from_str(&v.to_string()).ok()?,
        J::Float(v) => S::Number(serde_json::Number::from_f64(v)?),
        J::Str(v) => S::String(v.into_owned()),
        J::Array(values) => S::Array(
            values
                .iter()
                .cloned()
                .map(jiter_to_reference)
                .collect::<Option<_>>()?,
        ),
        J::Object(values) => {
            let mut out = serde_json::Map::new();
            for (key, value) in values.iter() {
                out.insert(key.to_string(), jiter_to_reference(value.clone())?);
            }
            S::Object(out)
        }
    })
}

fn approx_eq(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    use serde_json::Value::*;
    match (a, b) {
        (Number(x), Number(y)) => match (x.as_f64(), y.as_f64()) {
            (Some(x), Some(y)) => x == y || (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
            _ => x == y,
        },
        (Array(x), Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| approx_eq(p, q)),
        (Object(x), Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, p)| y.get(k).is_some_and(|q| approx_eq(p, q)))
        }
        _ => a == b,
    }
}
