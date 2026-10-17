//! Public API regressions for strict object-shape memo reuse.

use jmax_abi::number::NumericPolicy;
use jmax_abi::syntax::{DuplicatePolicyOwner, SkipMode};
use jmax_syntax::{BlockCursor, ErrorCode, Limits, Result, SkipScratch, skip_strict};

type Outcome = std::result::Result<(u32, Vec<u8>), (ErrorCode, u32, &'static str)>;

fn outcome(result: Result<u32>, decoded: Vec<u8>) -> Outcome {
    match result {
        Ok(end) => Ok((end, decoded)),
        Err(error) => Err((error.code, error.byte_offset, error.detail)),
    }
}

fn reference(input: &[u8], limits: Limits) -> Outcome {
    let mut decoded = Vec::new();
    let result = BlockCursor::new(
        input,
        limits,
        None::<NumericPolicy>,
        DuplicatePolicyOwner::Syntax,
    )
    .and_then(|mut cursor| {
        cursor.skip_value(&mut decoded, 0, SkipMode::StrictValidate)?;
        Ok(cursor.offset())
    });
    outcome(result, decoded)
}

fn compare(input: &[u8], limits: Limits, scratch: &mut SkipScratch) -> Outcome {
    let mut decoded = Vec::new();
    let result = skip_strict(
        input,
        0,
        0,
        limits,
        None,
        DuplicatePolicyOwner::Syntax,
        &mut decoded,
        scratch,
    );
    let actual = outcome(result, decoded);
    assert_eq!(actual, reference(input, limits), "input {:?}", input);
    actual
}

#[test]
fn strict_shape_memo_matches_cursor_for_short_repeated_object_shapes() {
    let inputs: &[&[u8]] = &[
        br#"[{"a":1,"b":2},{"x":3,"y":4},{"a":5,"b":6},{"m":7,"n":8}]"#,
        br#"{"a":0,"a":1}"#,
        br#"{"a":0,"b":1,"c":2,"d":3,"d":4}"#,
        br#"{"a":0,"b":1,"c":2,"d":3,"e":4,"f":5,"g":6,"g":7}"#,
        br#"{"a":0,"\u0061":1}"#,
        br#"[{"a":0,"b":1},{"x":0,"y":1},{"q":0,"q":1}]"#,
    ];
    let mut scratch = SkipScratch::default();
    let mut successes = 0;
    let mut failures = 0;
    for input in inputs {
        assert!(input.len() < 256, "single input exceeds the short-row bound");
        match compare(input, Limits::strict_default(), &mut scratch) {
            Ok(_) => successes += 1,
            Err(_) => failures += 1,
        }
    }
    assert!(successes >= 1);
    assert!(failures >= 1);
}

#[test]
fn strict_shape_memo_scratch_is_safe_across_equal_length_inputs_and_limits() {
    let left = br#"{"a":1,"b":2}"#;
    let right = br#"{"x":1,"y":2}"#;
    assert_eq!(left.len(), right.len());
    assert!(left.len() < 256);

    let mut scratch = SkipScratch::default();
    assert!(compare(left, Limits::strict_default(), &mut scratch).is_ok());
    assert!(compare(right, Limits::strict_default(), &mut scratch).is_ok());

    let duplicate = br#"{"a":1,"a":2}"#;
    let mut permissive = Limits::strict_default();
    permissive.reject_duplicate_keys = false;
    assert!(compare(duplicate, permissive, &mut scratch).is_ok());
    assert!(compare(duplicate, Limits::strict_default(), &mut scratch).is_err());

    let mut tight = Limits::strict_default();
    tight.max_fields_per_object = 1;
    let wide = br#"{"a":1,"b":2}"#;
    assert!(compare(wide, tight, &mut scratch).is_err());
    assert!(compare(wide, Limits::strict_default(), &mut scratch).is_ok());
}
