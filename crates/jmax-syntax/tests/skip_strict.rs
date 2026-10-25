//! `skip_strict` must equal a cursor moved to `start` running
//! `skip_value(StrictValidate)`: end offset, decoded contents, and errors.

use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::{DuplicatePolicyOwner, SkipMode};
use jmax_syntax::{BlockCursor, Limits, Result, SkipScratch, skip_strict};

const SEEDS: &[&str] = &[
    r#"{"a":1,"b":[true,false,null],"c":{"d":"e\né🦀","f":-0.5e10}}"#,
    r#"[{"k":"v","k2":"\"\\\/\b\f\n\r\t"},{"k":"v2"},[],{},[[[[{}]]]]]"#,
    r#"{"a":{"a":{"a":{"a":{"a":[1,2,3,{"x":"y"}]}}}},"b":"héllo 世界 🦀","c":12345678901234567890}"#,
    r#"  {"dup":1, "x": [1, 2.5, -3e-7, 0, -0, 1E+2 ], "s": "long string that crosses a sixty-four byte scanner block boundary"}  "#,
    r#"{"abc":"v","abc":"w","n":[1e308,-1e-308,0.0001]}"#,
    r#"{"e":"aggTrade","E":123456789,"s":"BNBBTC","a":12345,"p":"0.00100000","q":"100.00000000","f":100,"l":105,"T":123456785,"m":true,"M":true}"#,
    r#"{"b":[["0.0024","10"],["0.0025","11"]],"a":[]}"#,
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

type Outcome = std::result::Result<(u32, Vec<u8>), (jmax_syntax::ErrorCode, u32, &'static str)>;

fn summarize(result: Result<u32>, decoded: Vec<u8>) -> Outcome {
    match result {
        Ok(end) => Ok((end, decoded)),
        Err(error) => Err((error.code, error.byte_offset, error.detail)),
    }
}

fn reference(
    input: &[u8],
    start: u32,
    depth: u16,
    limits: Limits,
    numeric: Option<NumericPolicy>,
    owner: DuplicatePolicyOwner,
) -> Outcome {
    let mut decoded = Vec::new();
    let result = BlockCursor::new(input, limits, numeric, owner).and_then(|mut cursor| {
        cursor.advance(start);
        cursor.skip_value(&mut decoded, depth, SkipMode::StrictValidate)?;
        Ok(cursor.offset())
    });
    summarize(result, decoded)
}

#[test]
fn skip_strict_matches_cursor() {
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
    limits.reject_duplicate_keys = false;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.max_number_digits = 4;
    variants.push(limits);
    let mut limits = Limits::strict_default();
    limits.max_decoded_bytes = 6;
    variants.push(limits);
    let policies = [
        Some(NumericPolicy::strict_default()),
        None,
        Some(NumericPolicy { allow_Quoted_int: true, reject_non_finite_float: false, reject_negative_zero: false, max_exponent_abs: 2 }),
    ];
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut scratch = SkipScratch::default();
    let mut compared = 0u32;
    let mut accepted = 0u32;
    for seed in SEEDS {
        for round in 0..600 {
            let mut input = seed.as_bytes().to_vec();
            for _ in 0..(round % 4) {
                if input.is_empty() {
                    break;
                }
                let at = rng.below(input.len());
                match rng.below(4) {
                    0 => input[at] = BYTES[rng.below(BYTES.len())],
                    1 => input.insert(at, BYTES[rng.below(BYTES.len())]),
                    2 => {
                        input.remove(at);
                    }
                    _ => input.truncate(at),
                }
            }
            // Starts: the document start and positions that follow a delimiter.
            let mut starts = vec![0u32];
            for (index, byte) in input.iter().enumerate() {
                if matches!(byte, b':' | b'[' | b',') && index < input.len() {
                    starts.push(index as u32 + 1);
                }
            }
            let picks = [0, starts.len() / 2, starts.len().saturating_sub(1)];
            for pick in picks {
                let start = starts[pick];
                for limits in &variants {
                    for numeric in &policies {
                        let depth = (round % 3) as u16;
                        let owner = DuplicatePolicyOwner::Syntax;
                        let expected = reference(&input, start, depth, *limits, *numeric, owner);
                        let mut decoded = Vec::new();
                        let got = summarize(
                            skip_strict(
                                &input, start, depth, *limits, *numeric, owner, &mut decoded,
                                &mut scratch,
                            ),
                            decoded,
                        );
                        assert_eq!(got, expected, "input {:?} start {start}", String::from_utf8_lossy(&input));
                        compared += 1;
                        accepted += u32::from(got.is_ok());
                    }
                }
            }
        }
    }
    assert!(compared > 10_000 && accepted > 1_000, "{compared} {accepted}");
}

#[test]
fn disabled_duplicate_owner_is_rejected_like_the_cursor() {
    let input = br#"{"a":1}"#;
    let limits = Limits::strict_default();
    let expected = reference(input, 0, 0, limits, None, DuplicatePolicyOwner::Disabled);
    let mut decoded = Vec::new();
    let got = summarize(
        skip_strict(input, 0, 0, limits, None, DuplicatePolicyOwner::Disabled, &mut decoded, &mut SkipScratch::default()),
        decoded,
    );
    assert_eq!(got, expected);
    assert!(got.is_err());
}
