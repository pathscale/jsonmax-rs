//! The fuzz target `native_vs_vm`: one byte string drives one differential check
//! of a generated decoder against the schema VM and the value oracle.
//!
//! `data[0]` selects the decoder (modulo [`SUBJECTS`]), `data[1]` selects the limit
//! set (modulo the number of limit variants), and the rest is the input. Any
//! disagreement panics through the assertions of `harness::check_one`, which is
//! what a fuzzing engine reports. Decoders, contexts and reused outputs are built
//! once per thread, so each call costs one check.
//!
//! The libFuzzer entry point is `fuzz/fuzz_targets/native_vs_vm.rs`; the smoke test
//! `tests/fuzz_smoke.rs` runs the same function over [`seed_corpus`] and
//! deterministic mutants without any fuzzing engine.

use crate::harness::{
    depth_document, limit_variants, check_one, snapshot_document, Subject, Tally, AGG_TRADE, SHAPE,
};
use crate::harness::{oracle_open, oracle_plain, oracle_strict, shape_open, shape_strict};
use crate::regen::{BINANCE, SHAPES};
use crate::{binance, binance_open, binance_plain, shapes, shapes_open};
use jmax_abi::number::NumericPolicy;
use jmax_native::{NativeContext, NativeDecode, VmLimits};
use std::cell::RefCell;

/// The decoders a fuzz input can select, in selector order.
pub const SUBJECTS: &[&str] = &[
    "strict AggTrade",
    "strict DepthUpdate",
    "strict DepthSnapshot",
    "open AggTrade",
    "open DepthUpdate",
    "open DepthSnapshot",
    "plain AggTrade",
    "plain DepthUpdate",
    "strict Shape",
    "open Shape",
];

struct Runner {
    run: Box<dyn FnMut(usize, &[u8])>,
}

fn runner<T>(subject: Subject<T>) -> Runner
where
    T: NativeDecode + Default + PartialEq + core::fmt::Debug + 'static,
{
    let variants = limit_variants();
    let mut states: Vec<Option<(NativeContext, T)>> = variants.iter().map(|_| None).collect();
    let mut tally = Tally::default();
    Runner {
        run: Box::new(move |limit, input| {
            let index = limit % variants.len();
            let limits: VmLimits = variants[index];
            let (ctx, reused) = states[index].get_or_insert_with(|| {
                (NativeContext::new(limits, NumericPolicy::strict_default()), T::default())
            });
            check_one(&subject, ctx, reused, limits, input, &mut tally);
        }),
    }
}

fn runners() -> Vec<Runner> {
    vec![
        runner(Subject::<binance::models::AggTrade>::new(&BINANCE, "AggTrade", true, oracle_strict::trade)),
        runner(Subject::<binance::models::DepthUpdate>::new(&BINANCE, "DepthUpdate", true, oracle_strict::depth)),
        runner(Subject::<binance::models::DepthSnapshot>::new(&BINANCE, "DepthSnapshot", true, oracle_strict::snapshot)),
        runner(Subject::<binance_open::models::AggTrade>::new(&BINANCE, "AggTrade", false, oracle_open::trade)),
        runner(Subject::<binance_open::models::DepthUpdate>::new(&BINANCE, "DepthUpdate", false, oracle_open::depth)),
        runner(Subject::<binance_open::models::DepthSnapshot>::new(&BINANCE, "DepthSnapshot", false, oracle_open::snapshot)),
        runner(Subject::<binance_plain::models::AggTrade>::new(&BINANCE, "AggTrade", true, oracle_plain::trade)),
        runner(Subject::<binance_plain::models::DepthUpdate>::new(&BINANCE, "DepthUpdate", true, oracle_plain::depth)),
        runner(Subject::<shapes::models::Shape>::new(&SHAPES, "Shape", true, shape_strict::shape)),
        runner(Subject::<shapes_open::models::Shape>::new(&SHAPES, "Shape", false, shape_open::shape)),
    ]
}

thread_local! {
    static RUNNERS: RefCell<Option<Vec<Runner>>> = const { RefCell::new(None) };
}

/// The fuzz target. Inputs shorter than two bytes select nothing and are ignored.
pub fn native_vs_vm(data: &[u8]) {
    let [subject, limit, input @ ..] = data else { return };
    RUNNERS.with(|cell| {
        let mut slot = cell.borrow_mut();
        let runners = slot.get_or_insert_with(runners);
        let chosen = usize::from(*subject) % runners.len();
        (runners[chosen].run)(usize::from(*limit), input);
    });
}

/// The selector prefix for a decoder and a limit variant.
pub fn selector(subject: usize, limit: usize) -> [u8; 2] {
    [(subject % SUBJECTS.len()) as u8, (limit % limit_variants().len()) as u8]
}

/// Valid and near-valid documents for every decoder, each with its selector
/// prefix (limit variant 0, the defaults): the starting corpus for a fuzzing
/// engine and the seeds of the smoke test.
pub fn seed_corpus() -> Vec<Vec<u8>> {
    let trade = AGG_TRADE.as_bytes().to_vec();
    let mut documents: Vec<(usize, Vec<u8>)> = vec![
        (0, trade.clone()),
        (0, AGG_TRADE.replace(",\"M\":true", "").into_bytes()),
        (0, AGG_TRADE.replace('"', "\\u0022").into_bytes()),
        (3, trade.clone()),
        (3, AGG_TRADE.replace("\"m\"", "\"x\":[1,{\"y\":null}],\"m\"").into_bytes()),
        (6, trade),
        (8, SHAPE.as_bytes().to_vec()),
        (9, SHAPE.as_bytes().to_vec()),
        (9, SHAPE.replace("\"ok\"", "\"u\":[1,{\"k\":[null,true]}],\"ok\"").into_bytes()),
    ];
    for levels in [0usize, 1, 2, 20] {
        for subject in [1usize, 4, 7] {
            documents.push((subject, depth_document(levels)));
        }
        for subject in [2usize, 5] {
            documents.push((subject, snapshot_document(levels)));
        }
    }
    documents
        .into_iter()
        .map(|(subject, document)| {
            let mut data = selector(subject, 0).to_vec();
            data.extend(document);
            data
        })
        .collect()
}
