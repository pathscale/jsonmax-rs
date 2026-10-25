//! The fast acceptor may only ever say "accepted" when the exact strict loop
//! agrees, with identical end offset, discarded total and decoded buffer
//! (contents and capacity). When it declines it must leave `decoded` untouched.

extern crate std;

use super::*;
use crate::block_cursor::BlockCursor;
use alloc::{format, vec};
use jmax_abi::syntax::{DuplicatePolicyOwner, SkipMode};
use std::string::String;

#[test]
fn digit_words_find_the_first_non_digit_for_every_byte_and_lane() {
    for byte in 0..=255u8 {
        for lane in 0..8 {
            let mut word = [b'5'; 8];
            word[lane] = byte;
            let expected = word.iter().position(|b| !b.is_ascii_digit()).unwrap_or(8);
            let mask = non_digit_mask(u64::from_le_bytes(word));
            let actual = if mask == 0 {
                8
            } else {
                (mask.trailing_zeros() / 8) as usize
            };
            assert_eq!(actual, expected, "byte {byte}, lane {lane}");
        }
    }
    for length in 0..40 {
        let mut input = vec![b'7'; length];
        for start in 0..=length {
            assert_eq!(digit_run_end(&input, start), length);
        }
        for end in 0..length {
            input[end] = b',';
            assert_eq!(digit_run_end(&input, 0), end);
            input[end] = b'7';
        }
    }
}

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
    "[\"Deliver to 東京\\n\\\"42\\\"; C:\\\\x\\t✓\",\"é\\u00e9\\u20ac\\ud83d\\ude00z\",\"\\u0041\\/\"]",
    "{\"notes\":\"é\\n✓\",\"k\":[\"\\ud800\",\"\\udc00\",\"\\ud800\\u0041\",\"a\\u12g4\",\"\\u12\"]}",
    "[\"\\n\\xc3\\xa9\",\"\\\u{e9}\",\"a\\\n\"]",
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
                assert!(
                    exact.accepted,
                    "fast accepted what the exact loop rejects: {}",
                    context()
                );
                assert_eq!(end, input.len(), "{}", context());
                assert_eq!(discarded, exact.discarded, "{}", context());
                assert_eq!(decoded, exact.decoded, "{}", context());
                assert_eq!(
                    decoded.capacity(),
                    exact.decoded.capacity(),
                    "{}",
                    context()
                );
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
                body.push_str(&format!(
                    "\"{}\":{{\"{}\":1,\"x\":[{}]}}",
                    key(i),
                    key(i),
                    i
                ));
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
            let (default, numeric) = (
                Limits::strict_default(),
                Some(NumericPolicy::strict_default()),
            );
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
                cursor
                    .skip_value(&mut decoded, depth, SkipMode::StrictValidate)
                    .map(|_| ())
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

/// The exact loop's key hash only looks at the length and the ends of a key;
/// keys that differ only in the middle must still be distinct, and equal keys
/// must still collide, with and without escapes.
#[test]
fn constant_work_key_hash_keeps_duplicate_semantics() {
    let limits = Limits::strict_default();
    let numeric = Some(NumericPolicy::strict_default());
    let key = |mid: &str| format!("0123456789abcdef{mid}fedcba9876543210");
    let accepts = |text: String| reference(text.as_bytes(), limits, numeric, 0, 0).accepted;
    assert!(accepts(format!(
        r#"{{"{}":1,"{}":2}}"#,
        key("AAAA"),
        key("AAAB")
    )));
    assert!(!accepts(format!(
        r#"{{"{}":1,"{}":2}}"#,
        key("AAAA"),
        key("AAAA")
    )));
    assert!(!accepts(format!(
        r#"{{"{}":1,"{}":2}}"#,
        key("AAAA"),
        key("AA\\u0041A")
    )));
    assert!(accepts(format!(
        r#"{{"{}":1,"{}":2}}"#,
        key("AAAA"),
        key("AA\\u0042A")
    )));
    for n in 0..24usize {
        let a = "k".repeat(n);
        let b = format!("{}x{}", "k".repeat(n / 2), "k".repeat(n - n / 2));
        assert!(accepts(format!(r#"{{"{a}":1,"{b}":2}}"#)));
        assert!(!accepts(format!(r#"{{"{a}":1,"{a}":2}}"#)));
    }
}

fn shaped_array(keys: &[&str], last: &str, repeats: usize) -> String {
    let object = |keys: &[&str]| {
        let mut s = String::from("{");
        for (i, k) in keys.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!("\"{k}\":{i}"));
        }
        s.push('}');
        s
    };
    let mut s = String::from("[");
    for _ in 0..repeats {
        s.push_str(&object(keys));
        s.push(',');
    }
    s.push_str(last);
    s.push(']');
    s
}

#[test]
fn shape_memo_keeps_duplicate_semantics_at_every_key_position() {
    let mut stats = Stats::default();
    let keys = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
    let long_a = "aaaaaaaaaaaaaaaaaaXaaaaaaaaaaaaaaaaaaa";
    let long_b = "aaaaaaaaaaaaaaaaaaYaaaaaaaaaaaaaaaaaaa";
    let long_keys = [long_a, "x", long_b, "y"];
    for (limits, numeric) in limit_variants() {
        for repeats in [1usize, 2, 3] {
            for dup in 0..keys.len() {
                for at in 0..keys.len() {
                    // Object that repeats keys[dup] at position `at` (a duplicate when at != dup).
                    let mut ks = keys.to_vec();
                    ks[at] = keys[dup];
                    let last = format!(
                        "{{{}}}",
                        ks.iter()
                            .enumerate()
                            .map(|(i, k)| format!("\"{k}\":{i}"))
                            .collect::<std::vec::Vec<_>>()
                            .join(",")
                    );
                    check(
                        shaped_array(&keys, &last, repeats).as_bytes(),
                        limits,
                        numeric,
                        ALL_VARIANTS,
                        &mut stats,
                    );
                }
            }
            for last in [
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"delta":5}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"epsilon":5,"zeta":6,"eta":7}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"epsilon":5,"zeta":6,"alpha":7}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"epsilon":5,"zeta":6,"alpha":7}"#,
                r#"{"alpha":1,"beta":2}"#,
                r#"{"alpha":1}"#,
                r#"{"alpha":1,"beta":2,"gamm":3}"#,
                r#"{"alpha":1,"beta":2,"gammaa":3}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"epsilon":5,"zet":6}"#,
                r#"{"alpha":1,"beta":2,"beta":3}"#,
                r#"{"alpha":1,"alpha":2}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"epsilon":5,"zeta":6}"#,
                r#"{"alpha":1,"beta":2,"gamma":3,"delta":4,"epsilon":5,"zeta":6}"#,
                "{\"alpha\":1,\"beta\":2,\"gamma\":3,\"delta\":4,\"epsilon\":5,\"z\u{e9}ta\":6}",
            ] {
                check(
                    shaped_array(&keys, last, repeats).as_bytes(),
                    limits,
                    numeric,
                    ALL_VARIANTS,
                    &mut stats,
                );
            }
            // Distinct keys with equal fingerprints (same length, head and tail words).
            let last = format!("{{\"{long_a}\":1,\"x\":2,\"{long_b}\":3,\"y\":4}}");
            check(
                shaped_array(&long_keys, &last, repeats).as_bytes(),
                limits,
                numeric,
                ALL_VARIANTS,
                &mut stats,
            );
            let last = format!("{{\"{long_a}\":1,\"x\":2,\"{long_a}\":3,\"y\":4}}");
            check(
                shaped_array(&long_keys, &last, repeats).as_bytes(),
                limits,
                numeric,
                ALL_VARIANTS,
                &mut stats,
            );
            // Nested repeated objects at two depths with a duplicate deep inside.
            let inner = r#"{"p":1,"q":2,"r":{"s":1,"t":2}}"#;
            let bad = r#"{"p":1,"q":2,"r":{"s":1,"s":2}}"#;
            let mut nested = String::from("[");
            for _ in 0..repeats + 2 {
                nested.push_str(inner);
                nested.push(',');
            }
            nested.push_str(bad);
            nested.push(']');
            check(nested.as_bytes(), limits, numeric, ALL_VARIANTS, &mut stats);
        }
    }
    assert!(stats.fast > 100);
    assert!(stats.declined > 100);
}

#[test]
fn shape_memo_does_not_leak_across_inputs() {
    let a = "a".repeat(130);
    let b = "b".repeat(130);
    let first = format!("[{{\"{a}\":1,\"{b}\":2}},{{\"{a}\":3,\"{b}\":4}}]").into_bytes();
    // Same length and offsets, different second key: a stale memo would accept the duplicate.
    let second = format!("[{{\"{a}\":1,\"{b}\":2}},{{\"{a}\":3,\"{a}\":4}}]").into_bytes();
    let limits = Limits::strict_default();
    let params = Params {
        limits,
        numeric: Some(NumericPolicy::strict_default()),
        max_depth: usize::from(limits.max_depth),
        masker: jmax_string::EscapeMasker::new(),
        whole_input: true,
    };
    let mut scratch = FastScratch::default();
    let mut buffer = first.clone();
    let mut decoded = vec![];
    assert!(accept(&buffer, 0, &params, 0, &mut decoded, &mut scratch).is_some());
    // The same allocation with new content.
    buffer.copy_from_slice(&second);
    scratch.new_input();
    assert!(accept(&buffer, 0, &params, 0, &mut decoded, &mut scratch).is_none());
    buffer.copy_from_slice(&first);
    scratch.new_input();
    assert!(accept(&buffer, 0, &params, 0, &mut decoded, &mut scratch).is_some());
}

#[test]
fn shape_memo_survives_slot_collisions_between_nested_objects() {
    // Many small key vocabularies force direct-mapped slot collisions between a
    // parent still matching its memo and the children closing inside it.
    let mut rng = Rng(0xfeed_beef_1234_5678);
    let mut stats = Stats::default();
    fn object(rng: &mut Rng, depth: usize, out: &mut String) {
        out.push('{');
        let n = 2 + rng.below(4);
        let mut used = std::vec::Vec::new();
        for i in 0..n {
            let mut k = rng.below(6);
            if rng.below(40) != 0 {
                while used.contains(&k) {
                    k = rng.below(6);
                }
            }
            used.push(k);
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!("\"key{k}\":"));
            if depth < 3 && rng.below(2) == 0 {
                object(rng, depth + 1, out);
            } else {
                out.push('1');
            }
        }
        out.push('}');
    }
    let (limits, numeric) = limit_variants().remove(0);
    for _ in 0..3000 {
        let mut s = String::from("[");
        for i in 0..2 + rng.below(8) {
            if i > 0 {
                s.push(',');
            }
            object(&mut rng, 0, &mut s);
        }
        s.push(']');
        check(s.as_bytes(), limits, numeric, &[(0, 0)], &mut stats);
    }
    assert!(stats.fast > 1000, "{}", stats.fast);
}

#[test]
fn shape_memo_covers_table_sized_objects() {
    let mut stats = Stats::default();
    let names: std::vec::Vec<String> = (0..40).map(|i| format!("field{i}")).collect();
    let keys: std::vec::Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let body = |ks: &[&str]| {
        format!(
            "{{{}}}",
            ks.iter()
                .enumerate()
                .map(|(i, k)| format!("\"{k}\":{i}"))
                .collect::<std::vec::Vec<_>>()
                .join(",")
        )
    };
    let (limits, numeric) = limit_variants().remove(0);
    for repeats in [1usize, 2, 3] {
        check(
            shaped_array(&keys, &body(&keys), repeats).as_bytes(),
            limits,
            numeric,
            ALL_VARIANTS,
            &mut stats,
        );
        for dup in [0usize, 1, 31, 32, 33, 39] {
            for at in [1usize, 2, 30, 31, 32, 33, 34, 39] {
                let mut ks = keys.clone();
                ks[at] = keys[dup];
                check(
                    shaped_array(&keys, &body(&ks), repeats).as_bytes(),
                    limits,
                    numeric,
                    ALL_VARIANTS,
                    &mut stats,
                );
            }
        }
        // Extra key after the memo, escaped twin of the last key, shorter object.
        let mut ks = keys.clone();
        ks.push("extra");
        check(
            shaped_array(&keys, &body(&ks), repeats).as_bytes(),
            limits,
            numeric,
            ALL_VARIANTS,
            &mut stats,
        );
        ks.push("field39");
        check(
            shaped_array(&keys, &body(&ks), repeats).as_bytes(),
            limits,
            numeric,
            ALL_VARIANTS,
            &mut stats,
        );
        ks.pop();
        ks.pop();
        ks[39] = "field3\\u0039";
        check(
            shaped_array(&keys, &body(&ks), repeats).as_bytes(),
            limits,
            numeric,
            ALL_VARIANTS,
            &mut stats,
        );
        check(
            shaped_array(&keys, &body(&keys[..35]), repeats).as_bytes(),
            limits,
            numeric,
            ALL_VARIANTS,
            &mut stats,
        );
    }
    assert!(stats.fast > 10);
    assert!(stats.declined > 100);
}

#[test]
fn shape_memo_skips_short_slices_and_single_objects() {
    let limits = Limits::strict_default();
    let params = Params {
        limits,
        numeric: Some(NumericPolicy::strict_default()),
        max_depth: usize::from(limits.max_depth),
        masker: jmax_string::EscapeMasker::new(),
        whole_input: true,
    };
    let mut decoded = Vec::new();
    let mut scratch = FastScratch::default();

    // Even repeated objects do not pay for shape setup on a short remaining slice.
    let short = br#"[{"alpha":1,"beta":2},{"alpha":3,"beta":4}]"#;
    assert!(accept(short, 0, &params, 0, &mut decoded, &mut scratch).is_some());
    assert!(scratch.shapes.is_empty());

    // A lone large object only records a first-seen mark; it does not allocate
    // or retain a shape that cannot be looked up by another object.
    scratch.new_input();
    let singleton = format!("{{\"{}\":1,\"beta\":2}}", "a".repeat(300));
    assert!(
        accept(
            singleton.as_bytes(),
            0,
            &params,
            0,
            &mut decoded,
            &mut scratch
        )
        .is_some()
    );
    assert!(scratch.shapes.is_empty());
    assert!(scratch.seen_objects != 0);
}

#[test]
fn shape_memo_still_records_repeated_long_objects() {
    let limits = Limits::strict_default();
    let params = Params {
        limits,
        numeric: Some(NumericPolicy::strict_default()),
        max_depth: usize::from(limits.max_depth),
        masker: jmax_string::EscapeMasker::new(),
        whole_input: true,
    };
    let obj = format!("{{\"{}\":1,\"beta\":2}}", "a".repeat(120));
    let input = format!("[{obj},{obj},{obj}]");
    let mut decoded = Vec::new();
    let mut scratch = FastScratch::default();
    assert!(accept(input.as_bytes(), 0, &params, 0, &mut decoded, &mut scratch).is_some());
    assert!(
        scratch
            .shapes
            .iter()
            .any(|shape| shape.generation == scratch.generation && shape.keys.len() == 2)
    );
}

#[test]
fn fast_escaped_strings_with_raw_non_ascii_match_exact() {
    let valid = [
        "[\"Deliver to 東京 \\n \\\"42\\\"; C:\\\\x\\t✓\"]",
        "[\"é\\u00e9\\u20ac\\ud83d\\ude00z\",\"\\u0041\\/\\u0000\"]",
        "{\"é\\n\":\"\\u00e9\",\"x\":[\"a\\nb\"]}",
    ];
    let invalid: [&[u8]; 8] = [
        b"[\"\xc3\\n\"]",
        b"[\"\\n\xe6\x9d\"]",
        b"[\"\\n\xff\"]",
        b"[\"\\ud800\"]",
        b"[\"\\udc00x\"]",
        b"[\"\\ud83d\\u0041\"]",
        b"[\"\\u12g4\"]",
        b"[\"\xed\xa0\x80\\n\"]",
    ];
    for (limits, numeric) in limit_variants() {
        let mut stats = Stats::default();
        for text in valid {
            check(text.as_bytes(), limits, numeric, ALL_VARIANTS, &mut stats);
        }
        for bytes in invalid {
            check(bytes, limits, numeric, ALL_VARIANTS, &mut stats);
        }
        if limits.max_string_bytes >= 64
            && limits.max_decoded_bytes >= 4096
            && limits.max_depth >= 4
            && limits.max_fields_per_object >= 3
            && limits.max_array_items >= 3
        {
            assert!(
                stats.fast >= 3,
                "valid escaped strings were not accepted fast: {limits:?}"
            );
        }
    }
}

fn record_array(records: &[&str]) -> Vec<u8> {
    let mut text = String::from("[");
    for (n, record) in records.iter().enumerate() {
        if n > 0 {
            text.push_str(",\n  ");
        }
        text.push_str(record);
    }
    text.push(']');
    text.into_bytes()
}

#[test]
fn repeated_shape_recognizer_matches_exact() {
    let base = r#"{"id":1,"name":"é\né","tags":["a","b",3],"meta":{"x":1.5,"y":null,"z":[true,{"k":"v"}]},"empty":[],"o":{}}"#;
    let variants = [
        base,
        r#"{"id":2,"name":"plain","tags":[],"meta":{"x":-0.0,"y":null,"z":[]},"empty":[],"o":{}}"#,
        // extra key, missing key, reordered keys, duplicate key, escaped key
        r#"{"id":3,"name":"n","tags":[],"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{},"more":1}"#,
        r#"{"id":4,"name":"n","tags":[],"meta":{"x":1,"y":2,"z":[]},"empty":[]}"#,
        r#"{"name":"n","id":5,"tags":[],"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{}}"#,
        r#"{"id":6,"name":"n","tags":[],"meta":{"x":1,"x":2,"z":[]},"empty":[],"o":{}}"#,
        r#"{"id":7,"name":"n","tags":[],"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{}}"#,
        r#"{"id":8,"name":"n","tags":{"a":1},"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{}}"#,
        r#"{"id":9,"name":"\ud800","tags":[],"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{}}"#,
        r#"{"id":01,"name":"n","tags":[],"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{}}"#,
        r#"{"id":10,"name":"n","tags":[1,],"meta":{"x":1,"y":2,"z":[]},"empty":[],"o":{}}"#,
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut stats = Stats::default();
    for (limits, numeric) in limit_variants() {
        // Three identical records first, so the shapes are memoized before a variant.
        for variant in variants {
            let input = record_array(&[base, base, base, variant, base]);
            check(&input, limits, numeric, ALL_VARIANTS, &mut stats);
            for round in 0..40 {
                let mut mutated = input.clone();
                mutate(&mut rng, &mut mutated, 1 + round % 3);
                check(&mutated, limits, numeric, ALL_VARIANTS, &mut stats);
            }
        }
    }
    assert!(stats.fast > 500, "fast path barely used: {}", stats.fast);
    assert!(stats.declined > 500);
}
