//! The strict discard loop behind `validate_borrowed` and `SkipMode::StrictValidate`
//! is direct-coded for speed. It must accept and reject exactly what the
//! event state machine (`BlockCursor::next_event`) accepts and rejects, with the
//! same error code and byte offset, under default and tightened limits.

use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::DuplicatePolicyOwner;
use jmax_syntax::{BlockCursor, Limits, Result, ScanStep, Scratch, validate_borrowed};

const SEEDS: &[&str] = &[
    r#"{"a":1,"b":[true,false,null],"c":{"d":"e\né🦀","f":-0.5e10}}"#,
    r#"[{"k":"v","k2":"\"\\\/\b\f\n\r\t"},{"k":"v2"},[],{},[[[[{}]]]]]"#,
    r#"{"a":{"a":{"a":{"a":{"a":[1,2,3,{"x":"y"}]}}}},"b":"héllo 世界 🦀","c":12345678901234567890}"#,
    r#"  {"dup":1, "x": [1, 2.5, -3e-7, 0, -0, 1E+2 ], "s": "long string that crosses a sixty-four byte scanner block boundary"}  "#,
    r#"{"escAkey":"v","escAkey2":"w","n":[1e308,-1e-308,0.0001]}"#,
    r#""just a string""#,
    r#"-12345"#,
    r#"[1,[2,[3,[4,[5,[6,[7,[8,[9,[10]]]]]]]]]]"#,
];
const BYTES: &[u8] = b"{}[]:,\"\\ \t\n0123456789-+.eEtrufalsn\x00\x1f\x80\xc3\xa9\xed\xf0\xffuA";

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

fn reference(input: &[u8], limits: Limits, numeric: NumericPolicy) -> Result<()> {
    let mut cursor = BlockCursor::new(input, limits, Some(numeric), DuplicatePolicyOwner::Syntax)?;
    let mut decoded = Vec::new();
    while let ScanStep::Emit(_) = cursor.next_event(&mut decoded)? {}
    cursor.finish_eof()
}

fn summary(result: Result<()>) -> Option<(jmax_syntax::ErrorCode, u32)> {
    result.err().map(|error| (error.code, error.byte_offset))
}

#[test]
fn discard_loop_matches_event_machine() {
    let mut variants = vec![Limits::strict_default()];
    let mut limits = Limits::strict_default();
    limits.max_depth = 3;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.max_fields_per_object = 2;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.max_array_items = 2;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.max_string_bytes = 5;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.max_number_digits = 3;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.reject_duplicate_keys = false;
    variants.push(limits);
    let numeric = NumericPolicy::strict_default();
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    let mut checked = 0usize;
    for round in 0..4000 {
        let mut input = SEEDS[round % SEEDS.len()].as_bytes().to_vec();
        for _ in 0..(round % 4) {
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
        for &limits in &variants {
            let fast = validate_borrowed(&input, limits, Some(numeric), &mut Scratch::default());
            let slow = reference(&input, limits, numeric);
            assert_eq!(
                summary(fast.map(|_| ())),
                summary(slow),
                "input {:?} limits {limits:?}",
                String::from_utf8_lossy(&input)
            );
            checked += 1;
        }
    }
    assert!(checked > 0);
}
