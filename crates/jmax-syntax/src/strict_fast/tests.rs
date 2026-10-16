//! The fast acceptor may only ever say "accepted" when the exact strict loop
//! agrees, with identical end offset, discarded total and decoded buffer
//! (contents and capacity). When it declines it must leave `decoded` untouched.

extern crate std;

use super::*;
use crate::block_cursor::BlockCursor;
use alloc::{format, vec};
use jmax_abi::syntax::{DuplicatePolicyOwner, SkipMode};
use std::string::String;

const SEEDS: &[&str] = &[
    r#"{"a":1,"b":[true,false,null],"c":{"d":"e\né🦀","f":-0.5e10}}"#,
    r#"[{"k":"v","k2":"\"\\\/\b\f\n\r\t"},{"k":"v2"},[],{},[[[[{}]]]]]"#,
    r#"{"a":{"a":{"a":{"a":{"a":[1,2,3,{"x":"y"}]}}}},"b":"héllo 世界 🦀","a2":12345678901234567890}"#,
    r#"  {"dup":1, "x": [1, 2.5, -3e-7, 0, -0, 1E+2 ], "s": "tab\tinside"}  "#,
    r#"{"escAkey":"v","escAkey2":"w","n":[1e308,-1e-308,0.0001]}"#,
    r#""just a string""#,
    r#"12345"#,
    r#"[1,[2,[3,[4,[5,[6,[7,[8,[9,[10]]]]]]]]]]"#,
    r#"{"a":1,"a":2}"#,
    r#"{"ab":1,"ab":2}"#,
    r#"{"k\n":1,"k\u000a":2}"#,
    r#"{"ab":1,"cd":{"ef":2,"gh":[{"ij":3},{"ij":4}]},"k":"vé😀"}"#,
    r#"{"x":{"ab":1},"ab":2,"y":{"ab":3,"c":"A"}}"#,
    r#"["\ud800","\u0000x","a\x","\u12","ok"]"#,
    r#"[0,-0,0.0,-0.0,1e5,1E+5,1e-5,-1e-400,1e309,1.5e308,0.5,-0.5,10,-10,0e0,1e0000000000001,01,1.,.5,-]"#,
    r#"[0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001]"#,
    r#"[true,false,null,truee,nul,fals,tru,nulll,true1,[true]]"#,
    "[\"\u{7f}\",\"a\tb\",\"\u{1}\"]",
    r#"{"a":1,}"#,
    r#"[1,]"#,
    r#"{"a" 1}"#,
    r#"{"a":1 "b":2}"#,
    r#"[1 2]"#,
    r#"{}"#,
    r#"[]"#,
    r#"  [ ]  "#,
    "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {\n    \"c\": null\n  }\n}\n",
    r#"{"batch_id":"b","tenant":"t","schema_version":1,"request_count":2,"accepted":true,"records":[{"a":"ÿ"}]}"#,
    r#"{"mode":"m","nodeName":"n","numExecutors":3,"quietingDown":false,"jobs":[{"name":"x","url":"y"}]}"#,
];
const BYTES: &[u8] =
    b"{}[]:,\"\\ \t\n0123456789-+.eEtrufalsn\x00\x1f\x7f\x80\xc3\xa9\xed\xa0\xf0\x9f\xffuA";

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn mutate(rng: &mut Rng, input: &mut Vec<u8>, count: usize) {
    for _ in 0..count {
        if input.is_empty() {
            break;
        }
        let at = rng.below(input.len());
        match rng.below(4) {
            0 => input[at] = BYTES[rng.below(BYTES.len())],
            1 => {
                input.remove(at);
            }
            2 => input.insert(at, BYTES[rng.below(BYTES.len())]),
            _ => {
                let end = (at + 1 + rng.below(6)).min(input.len());
                let chunk = input[at..end].to_vec();
                input.splice(at..at, chunk);
            }
        }
    }
}

fn limit_variants() -> Vec<(Limits, Option<NumericPolicy>)> {
    let strict = NumericPolicy::strict_default();
    let base = Limits::strict_default();
    let mut v = vec![(base, Some(strict))];
    let mut l = base;
    l.max_depth = 4;
    v.push((l, Some(strict)));
    let mut l = base;
    l.max_fields_per_object = 3;
    v.push((l, Some(strict)));
    let mut l = base;
    l.max_array_items = 3;
    v.push((l, Some(strict)));
    let mut l = base;
    l.max_string_bytes = 6;
    v.push((l, Some(strict)));
    let mut l = base;
    l.max_decoded_bytes = 12;
    v.push((l, Some(strict)));
    let mut l = base;
    l.max_number_digits = 3;
    v.push((l, Some(strict)));
    let mut l = base;
    l.reject_duplicate_keys = false;
    v.push((l, Some(strict)));
    let mut policy = strict;
    policy.reject_negative_zero = false;
    policy.max_exponent_abs = 5;
    v.push((base, Some(policy)));
    // No numeric policy at all.
    v.push((base, None));
    let mut l = base;
    l.max_depth = 0;
    v.push((l, None));
    let mut l = base;
    l.max_decoded_bytes = 3;
    l.max_string_bytes = 2;
    v.push((l, None));
    v
}

fn make_decoded(prefix: usize, capacity: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(capacity.max(prefix));
    v.extend((0..prefix).map(|i| 0xa0 + i as u8));
    v
}

struct Reference {
    accepted: bool,
    discarded: u32,
    decoded: Vec<u8>,
}

fn reference(
    input: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    prefix: usize,
    capacity: usize,
) -> Reference {
    let mut decoded = make_decoded(prefix, capacity);
    let Ok(mut cursor) = BlockCursor::new(input, limits, numeric, DuplicatePolicyOwner::Syntax)
    else {
        return Reference {
            accepted: false,
            discarded: 0,
            decoded,
        };
    };
    let result = cursor
        .skip_value(&mut decoded, 0, SkipMode::StrictValidate)
        .and_then(|_| cursor.finish_eof());
    Reference {
        accepted: result.is_ok(),
        discarded: cursor.discarded_decoded(),
        decoded,
    }
}

#[derive(Default)]
struct Stats {
    fast: usize,
    declined_valid: usize,
    declined: usize,
}

fn check(
    input: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    variants: &[(usize, usize)],
    stats: &mut Stats,
) {
    for &(prefix, capacity) in variants {
        let exact = reference(input, limits, numeric, prefix, capacity);
        let mut decoded = make_decoded(prefix, capacity);
        let before = decoded.clone();
        let capacity_before = decoded.capacity();
        let params = Params {
            limits,
            numeric,
            max_depth: usize::from(limits.max_depth),
            masker: jmax_string::EscapeMasker::new(),
            whole_input: true,
        };
        let mut scratch = FastScratch::default();
        let outcome = accept(input, 0, &params, 0, &mut decoded, &mut scratch);
        let context = || {
            format!(
                "input {:?} limits {limits:?} numeric {numeric:?} prefix {prefix} capacity {capacity}",
                String::from_utf8_lossy(input)
            )
        };
        match outcome {
            Some((end, discarded)) => {
                assert!(exact.accepted, "fast accepted what the exact loop rejects: {}", context());
                assert_eq!(end, input.len(), "{}", context());
                assert_eq!(discarded, exact.discarded, "{}", context());
                assert_eq!(decoded, exact.decoded, "{}", context());
                assert_eq!(decoded.capacity(), exact.decoded.capacity(), "{}", context());
                stats.fast += 1;
            }
            None => {
                assert_eq!(decoded, before, "{}", context());
                assert_eq!(decoded.capacity(), capacity_before, "{}", context());
                if exact.accepted {
                    stats.declined_valid += 1;
                } else {
                    stats.declined += 1;
                }
            }
        }
    }
}

const ALL_VARIANTS: &[(usize, usize)] = &[(0, 0), (0, 64), (3, 3), (5, 40)];

#[test]
fn fast_matches_exact_on_seeds_and_mutations() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut stats = Stats::default();
    for (limits, numeric) in limit_variants() {
        for round in 0..1500 {
            let mut input = SEEDS[round % SEEDS.len()].as_bytes().to_vec();
            mutate(&mut rng, &mut input, round % 4);
            check(&input, limits, numeric, ALL_VARIANTS, &mut stats);
        }
    }
    assert!(stats.fast > 1000, "fast path barely used: {}", stats.fast);
    assert!(stats.declined > 1000);
}

#[test]
fn fast_matches_exact_on_large_objects() {
    // Past the inline key limit: table mode, with duplicates at every position,
    // escaped keys that live in the arena, and nested tables at two depths.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut stats = Stats::default();
    for escaped in [false, true] {
        let key = |i: usize| {
            if escaped {
                format!("k\\u0041{i}")
            } else {
                format!("k{i}")
            }
        };
        for keys in [31usize, 32, 33, 34, 100] {
            let mut body = String::from("{");
            for i in 0..keys {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&format!("\"{}\":{{\"{}\":1,\"x\":[{}]}}", key(i), key(i), i));
            }
            body.push('}');
            for (limits, numeric) in limit_variants() {
                check(body.as_bytes(), limits, numeric, ALL_VARIANTS, &mut stats);
                for dup_at in [0, keys / 2, keys - 1] {
                    let dup = format!(",\"{}\":0}}", key(dup_at));
                    let mut input = body.clone();
                    input.pop();
                    input.push_str(&dup);
                    check(input.as_bytes(), limits, numeric, ALL_VARIANTS, &mut stats);
                    // The same key in different spellings.
                    let mut input = body.clone();
                    input.pop();
                    input.push_str(&format!(",\"k\\u0041{dup_at}\":0}}"));
                    check(input.as_bytes(), limits, numeric, ALL_VARIANTS, &mut stats);
                }
                for _ in 0..30 {
                    let mut input = body.as_bytes().to_vec();
                    let count = 1 + rng.below(3);
                    mutate(&mut rng, &mut input, count);
                    check(&input, limits, numeric, ALL_VARIANTS, &mut stats);
                }
            }
        }
    }
    assert!(stats.fast > 100);
    assert!(stats.declined > 100);
}

#[test]
fn fast_matches_exact_on_fixtures_and_corpora() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut stats = Stats::default();
    let mut rng = Rng(0x1234_5678_9abc_def1);
    let mut corpora_seen = 0;
    for dir in ["fixtures", "bench/data"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            let large = bytes.len() > 20_000;
            let variants: &[(usize, usize)] = if large { &[(0, 0)] } else { ALL_VARIANTS };
            let (default, numeric) = (Limits::strict_default(), Some(NumericPolicy::strict_default()));
            let mut probe = Limits::strict_default();
            probe.reject_unknown_keys = false;
            check(&bytes, probe, numeric, variants, &mut stats);
            if dir == "bench/data" {
                // Every benchmark corpus must take the fast path outright.
                let before = stats.fast;
                let mut scratch = FastScratch::default();
                let params = Params {
                    limits: default,
                    numeric,
                    max_depth: usize::from(default.max_depth),
                    masker: jmax_string::EscapeMasker::new(),
                    whole_input: true,
                };
                let mut decoded = Vec::new();
                assert!(
                    accept(&bytes, 0, &params, 0, &mut decoded, &mut scratch).is_some(),
                    "{} left the fast path",
                    path.display()
                );
                assert!(before <= stats.fast);
                corpora_seen += 1;
            }
            let rounds = if large { 12 } else { 400 };
            for (limits, numeric) in limit_variants() {
                for _ in 0..rounds / 4 {
                    let mut input = bytes.clone();
                    let count = 1 + rng.below(2);
                    mutate(&mut rng, &mut input, count);
                    check(&input, limits, numeric, variants, &mut stats);
                }
            }
        }
    }
    let _ = corpora_seen;
    assert!(stats.fast > 20);
}

#[test]
fn declined_inputs_do_not_disturb_the_arena() {
    // Escaped keys grow the arena before a later failure (duplicate, bad escape,
    // trailing bytes): the arena must come back with its original capacity.
    let inputs: &[&str] = &[
        r#"{"ab":1,"cd":2,"ab":3}"#,
        r#"{"ab":1,"cd":2,"e":"\x"}"#,
        r#"{"ab":1,"cd":2} x"#,
        r#"{"ab":1,"cd":"#,
    ];
    let mut stats = Stats::default();
    for input in inputs {
        for (limits, numeric) in limit_variants() {
            check(input.as_bytes(), limits, numeric, ALL_VARIANTS, &mut stats);
        }
    }
    assert!(stats.declined > 0);
}

/// Runs two consecutive container skips through the cursor, with or without
/// the fast path, and reports everything observable afterwards.
fn cursor_run(
    input: &[u8],
    limits: Limits,
    numeric: Option<NumericPolicy>,
    depth: u16,
    prefix: usize,
    capacity: usize,
    fast: bool,
) -> (std::vec::Vec<(bool, u32)>, u32, Vec<u8>, usize) {
    let mut decoded = make_decoded(prefix, capacity);
    let mut steps = vec![];
    let mut discarded = 0;
    if let Ok(mut cursor) = BlockCursor::new(input, limits, numeric, DuplicatePolicyOwner::Syntax) {
        for _ in 0..2 {
            let result = if fast {
                cursor.skip_value(&mut decoded, depth, SkipMode::StrictValidate).map(|_| ())
            } else {
                cursor
                    .skip_whitespace()
                    .and_then(|_| cursor.skip_strict_exact_for_test(&mut decoded, depth))
            };
            steps.push((result.is_ok(), cursor.offset()));
            if result.is_err() {
                break;
            }
        }
        discarded = cursor.discarded_decoded();
    }
    let capacity = decoded.capacity();
    (steps, discarded, decoded, capacity)
}

#[test]
fn cursor_fast_skip_matches_exact_offsets_totals_and_capacity() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut fast_steps = 0usize;
    for (limits, numeric) in limit_variants() {
        for round in 0..900 {
            let a = SEEDS[round % SEEDS.len()].trim();
            let b = SEEDS[(round * 7 + 3) % SEEDS.len()].trim();
            let pad = " ".repeat(round % 131);
            let mut input = format!("{pad}{a}{}{b}", ["", " ", "\n\n"][round % 3]).into_bytes();
            mutate(&mut rng, &mut input, round % 3);
            for depth in [0u16, 1, 3] {
                for &(prefix, capacity) in ALL_VARIANTS {
                    let exact = cursor_run(&input, limits, numeric, depth, prefix, capacity, false);
                    let fast = cursor_run(&input, limits, numeric, depth, prefix, capacity, true);
                    assert_eq!(
                        fast,
                        exact,
                        "input {:?} limits {limits:?} depth {depth} prefix {prefix} capacity {capacity}",
                        String::from_utf8_lossy(&input)
                    );
                    fast_steps += exact.0.iter().filter(|s| s.0).count();
                }
            }
        }
    }
    assert!(fast_steps > 5000, "too few accepted skips: {fast_steps}");
}
