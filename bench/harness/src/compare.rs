//! Shared full-metric comparison jobs for the executable and bench entry point.
use crate::{alloc_metrics, common, measure};
use common::*;
use serde::de::{DeserializeOwned, IgnoredAny};
use std::{fmt::Debug, fs, hint::black_box};

struct Job<'a> {
    corpus: String,
    class: &'static str,
    parser: &'static str,
    bytes: usize,
    run: Box<dyn FnMut() -> R<()> + 'a>,
    retained: usize,
    retained_get: Option<Box<dyn Fn() -> usize + 'a>>,
}
struct Jobs<'a> {
    v: Vec<Job<'a>>,
    corpus: String,
    bytes: usize,
}
impl<'a> Jobs<'a> {
    fn add(
        &mut self,
        class: &'static str,
        parser: &'static str,
        mut f: impl FnMut() -> R<()> + 'a,
    ) {
        self.v.push(Job {
            corpus: self.corpus.clone(),
            class,
            parser,
            bytes: self.bytes,
            run: Box::new(move || f()),
            retained: 0,
            retained_get: None,
        });
    }
}
fn err<E: std::fmt::Display>(e: E) -> Box<dyn std::error::Error> {
    e.to_string().into()
}

fn typed<'a, T: PartialEq + Debug + DeserializeOwned + 'a>(
    j: &mut Jobs<'a>,
    b: &'a [u8],
    p: &'a Plan,
    f: fn(&[u8], &Plan, &mut jsonmax::endpoint::ValidationScratch) -> R<T>,
) -> R<()> {
    let expected: T = serde_json::from_slice(b)?;
    let mut check_scratch = jsonmax::endpoint::ValidationScratch::default();
    assert_eq!(
        expected,
        f(b, p, &mut check_scratch)?,
        "jmax typed {}",
        j.corpus
    );
    let mut buf = b.to_vec();
    assert_eq!(
        expected,
        simd_json::serde::from_slice::<T>(&mut buf).map_err(err)?,
        "simd-json typed {}",
        j.corpus
    );
    assert_eq!(
        expected,
        sonic_rs::from_slice::<T>(b).map_err(err)?,
        "sonic-rs typed {}",
        j.corpus
    );
    let mut scratch = jsonmax::endpoint::ValidationScratch::default();
    j.add("typed", "jmax", move || {
        black_box(f(black_box(b), p, &mut scratch)?);
        Ok(())
    });
    j.add("typed", "serde_json", move || {
        black_box(serde_json::from_slice::<T>(black_box(b))?);
        Ok(())
    });
    let mut buf = b.to_vec();
    j.add("typed", "simd-json", move || {
        buf.copy_from_slice(b);
        black_box(simd_json::serde::from_slice::<T>(black_box(&mut buf)).map_err(err)?);
        Ok(())
    });
    j.add("typed", "sonic-rs", move || {
        black_box(sonic_rs::from_slice::<T>(black_box(b)).map_err(err)?);
        Ok(())
    });
    Ok(())
}

pub fn run() -> R<()> {
    let dir = std::env::var("JMAX_DATA")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../data").into());
    let filter = std::env::var("FILTER").ok();
    let all = [
        "twitter",
        "citm_catalog",
        "canada",
        "github_events",
        "apache_builds",
        "endpoint_batch",
    ];
    let corpora = std::env::var("CORPORA").ok();
    let names: Vec<&str> = all
        .iter()
        .copied()
        .filter(|n| {
            corpora
                .as_ref()
                .map_or(true, |c| c.split(',').any(|x| x == *n))
        })
        .collect();

    let files: Vec<Vec<u8>> = names
        .iter()
        .map(|n| fs::read(format!("{dir}/{n}.json")))
        .collect::<Result<_, _>>()?;
    let plans: Vec<Plan> = names.iter().map(|n| plan(n)).collect::<R<_>>()?;
    let docs: Vec<_> = files
        .iter()
        .map(|b| jsonmax::parse::parse_slice(b, options()))
        .collect::<Result<_, _>>()?;
    let values: Vec<serde_json::Value> = files
        .iter()
        .map(|b| serde_json::from_slice(b))
        .collect::<Result<_, _>>()?;
    let sonic_vals: Vec<sonic_rs::Value> = files
        .iter()
        .map(|b| sonic_rs::from_slice(b))
        .collect::<Result<_, _>>()
        .map_err(err)?;
    let simd_vals: Vec<simd_json::OwnedValue> = files
        .iter()
        .map(|b| {
            let mut c = b.clone();
            simd_json::to_owned_value(&mut c)
        })
        .collect::<Result<_, _>>()
        .map_err(err)?;
    let json_vals: Vec<json::JsonValue> = files
        .iter()
        .map(|b| json::parse(std::str::from_utf8(b).unwrap()))
        .collect::<Result<_, _>>()
        .map_err(err)?;

    let mut j = Jobs {
        v: vec![],
        corpus: String::new(),
        bytes: 0,
    };
    for (i, name) in names.iter().enumerate() {
        let b = files[i].as_slice();
        j.corpus = name.to_string();
        j.bytes = b.len();
        let (doc, val, p) = (&docs[i], &values[i], &plans[i]);

        // Equivalence of what each serializer writes (parsed back by serde_json).
        let check = |who: &str, out: &[u8]| {
            let ok = serde_json::from_slice::<serde_json::Value>(out)
                .map_or(false, |v| approx_eq(&v, val));
            assert!(ok, "serialize {who} on {name}");
        };
        let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
        jsonmax::encode::encode_document_streaming(doc, &mut sink, Default::default())?;
        check("jmax", &sink.into_vec());
        check("serde_json", &serde_json::to_vec(val)?);
        check("sonic-rs", &sonic_rs::to_vec(&sonic_vals[i]).map_err(err)?);
        check("simd-json", &simd_json::to_vec(&simd_vals[i]).map_err(err)?);
        check("json-rust", json_vals[i].dump().as_bytes());

        j.add("ref", "memcpy", {
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(black_box(b));
                black_box(&buf);
                Ok(())
            }
        });

        // validate / skip
        let mut validation = jsonmax::Scratch::default();
        let mut skip = jmax_syntax::SkipScratch::default();
        j.add("validate", "jmax", move || {
            let opts = options();
            black_box(jmax_syntax::validate_borrowed_reusing(
                black_box(b),
                opts.limits,
                Some(opts.numeric),
                &mut validation,
                &mut skip,
            )?);
            Ok(())
        });
        j.add("validate", "serde_json", move || {
            black_box(serde_json::from_slice::<IgnoredAny>(black_box(b))?);
            Ok(())
        });
        j.add("lazy-skip", "sonic-rs", move || {
            black_box(sonic_rs::get(black_box(b), &[] as &[&str]).map_err(err)?);
            Ok(())
        });
        j.add("validate", "sonic-rs ignored", move || {
            black_box(sonic_rs::from_slice::<IgnoredAny>(black_box(b)).map_err(err)?);
            Ok(())
        });
        j.add("validate", "jiter", move || {
            let mut it = jiter::Jiter::new(black_box(b));
            it.next_skip().map_err(err)?;
            it.finish().map_err(err)?;
            Ok(())
        });

        // Query rows select the same raw JSON value, with setup assertions.
        let path = match *name {
            "twitter" => "$.search_metadata.query",
            "citm_catalog" => "$.venueNames.PLEYEL_PLEYEL",
            "canada" => "$.type",
            "github_events" => "$[0].actor.login",
            "apache_builds" => "$.mode",
            _ => "$.batch_id",
        };
        let pointer = match *name {
            "twitter" => "/search_metadata/query",
            "citm_catalog" => "/venueNames/PLEYEL_PLEYEL",
            "canada" => "/type",
            "github_events" => "/0/actor/login",
            "apache_builds" => "/mode",
            _ => "/batch_id",
        };
        let expected_query = val.pointer(pointer).expect("query field exists");
        for (parser_name, training) in [("ps-pikkr untrained", usize::MAX), ("ps-pikkr trained", 1)]
        {
            let before = alloc_metrics::live();
            let mut engine = pikkr::Pikkr::new(&[path], training).map_err(err)?;
            for _ in 0..3 {
                let result = engine.parse(b).map_err(err)?;
                assert_eq!(
                    &serde_json::from_slice::<serde_json::Value>(
                        result[0].expect("pikkr query result")
                    )?,
                    expected_query
                );
            }
            let measured_retained = alloc_metrics::live().saturating_sub(before);
            let parser = std::rc::Rc::new(std::cell::RefCell::new(engine));
            let getter = parser.clone();
            j.add("query-only", parser_name, move || {
                black_box(parser.borrow_mut().parse(black_box(b)).map_err(err)?);
                Ok(())
            });
            j.v.last_mut().unwrap().retained_get = Some(Box::new(move || {
                getter.borrow().retained_heap_bytes().max(measured_retained)
            }));
        }
        for (class, mode) in [
            ("query-only", jsonmax::QueryMode::Query),
            ("strict-extract", jsonmax::QueryMode::Strict),
        ] {
            let before = alloc_metrics::live();
            let query = jsonmax::query::compile_paths(&[path
                .trim_start_matches("$.")
                .trim_start_matches('$')])?;
            let query_retained = alloc_metrics::live().saturating_sub(before);
            let result = jsonmax::query::execute_borrowed(
                &query,
                b,
                jsonmax::QueryOptions {
                    mode,
                    ..Default::default()
                },
            )?;
            assert_eq!(result.matches().len(), 1);
            assert_eq!(
                &serde_json::from_slice::<serde_json::Value>(
                    result.raw_match(0, 0).expect("jmax query result")
                )?,
                expected_query
            );
            let mut scratch = jsonmax::query::QueryScratch::default();
            j.add(class, "jmax", move || {
                let result = black_box(jsonmax::query::execute_borrowed_with_scratch(
                    &query,
                    black_box(b),
                    jsonmax::QueryOptions {
                        mode,
                        ..Default::default()
                    },
                    &mut scratch,
                )?);
                scratch.recycle_borrowed(result);
                Ok(())
            });
            j.v.last_mut().unwrap().retained = query_retained;
        }

        // dom-owned
        j.add("dom-owned", "jmax", move || {
            black_box(jsonmax::parse::parse_slice(black_box(b), options())?);
            Ok(())
        });
        j.add("dom-owned", "serde_json", move || {
            black_box(serde_json::from_slice::<serde_json::Value>(black_box(b))?);
            Ok(())
        });
        j.add("dom-owned", "sonic-rs", move || {
            black_box(sonic_rs::from_slice::<sonic_rs::Value>(black_box(b)).map_err(err)?);
            Ok(())
        });
        j.add("dom-owned", "simd-json owned", {
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::to_owned_value(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        });
        j.add("dom-owned", "json-rust", move || {
            black_box(json::parse(std::str::from_utf8(black_box(b)).map_err(err)?).map_err(err)?);
            Ok(())
        });
        j.add("dom-owned", "jiter", move || {
            black_box(
                jiter::JsonValue::parse(black_box(b), false)
                    .map_err(err)?
                    .into_static(),
            );
            Ok(())
        });

        // dom-borrowed
        j.add("dom-borrowed", "jmax", move || {
            black_box(jsonmax::parse::parse_borrowed(black_box(b), options())?);
            Ok(())
        });
        j.add("dom-borrowed", "simd-json tape", {
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::to_tape(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        });
        j.add("dom-borrowed", "simd-json borrowed", {
            let mut buf = b.to_vec();
            move || {
                buf.copy_from_slice(b);
                black_box(simd_json::to_borrowed_value(black_box(&mut buf)).map_err(err)?);
                Ok(())
            }
        });
        j.add("dom-borrowed", "jiter", move || {
            black_box(jiter::JsonValue::parse(black_box(b), false).map_err(err)?);
            Ok(())
        });

        // typed
        match *name {
            "twitter" => typed(&mut j, b, p, jmax_twitter_with_scratch)?,
            "citm_catalog" => typed(&mut j, b, p, jmax_citm_with_scratch)?,
            "canada" => typed(&mut j, b, p, jmax_canada_with_scratch)?,
            "github_events" => typed(&mut j, b, p, jmax_github_with_scratch)?,
            "apache_builds" => typed(&mut j, b, p, jmax_apache_with_scratch)?,
            _ => typed(&mut j, b, p, jmax_endpoint_with_scratch)?,
        }
        common::native::check(name, b)?;
        let mut native_scratch = common::native::NativeScratch::default();
        let native_name = *name;
        j.add("typed", "jmax-native", move || {
            black_box(common::native::project(
                native_name,
                black_box(b),
                &mut native_scratch,
            )?);
            Ok(())
        });

        // serialize
        j.add("serialize", "jmax", move || {
            let mut sink = jmax_encode::VecSink::new(jsonmax::DEFAULT_MAX_OUTPUT_BYTES);
            jsonmax::encode::encode_document_streaming(
                black_box(doc),
                &mut sink,
                Default::default(),
            )?;
            black_box(sink.into_vec());
            Ok(())
        });
        j.add("serialize", "serde_json", move || {
            black_box(serde_json::to_vec(black_box(val))?);
            Ok(())
        });
        let (sv, dv, jv) = (&sonic_vals[i], &simd_vals[i], &json_vals[i]);
        j.add("serialize", "sonic-rs", move || {
            black_box(sonic_rs::to_vec(black_box(sv)).map_err(err)?);
            Ok(())
        });
        j.add("serialize", "simd-json", move || {
            black_box(simd_json::to_vec(black_box(dv)).map_err(err)?);
            Ok(())
        });
        j.add("serialize", "json-rust", move || {
            black_box(black_box(jv).dump());
            Ok(())
        });
    }
    let mut jobs = j.v;
    if let Some(f) = &filter {
        let pats: Vec<&str> = f.split(',').collect();
        jobs.retain(|j| {
            pats.iter()
                .any(|p| format!("{}/{}/{}", j.corpus, j.class, j.parser).contains(p))
        });
    }
    println!("{}", measure::HEADER);
    for job in &mut jobs {
        if (job.parser.starts_with("simd-json") && job.class != "serialize")
            || job.parser == "memcpy"
        {
            job.retained += job.bytes;
        }
        let m = measure::run(&mut *job.run, job.retained, job.retained_get.as_deref())?;
        println!(
            "{}",
            measure::row(&job.corpus, job.class, job.parser, job.bytes, &m)
        );
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
        (Object(x), Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, p)| y.get(k).is_some_and(|q| approx_eq(p, q)))
        }
        _ => a == b,
    }
}
