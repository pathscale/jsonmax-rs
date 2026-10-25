use jmax_abi::buffer::Payload;
use jmax_dom::{Limits, build};
use jmax_encode::{EncodeLimits, ErrorCode, VecSink, typed};

fn reference_owned(raw: &[u8], limits: EncodeLimits) -> Vec<u8> {
    let document = build::parse(Payload::from_slice(raw), Limits::strict_default()).unwrap();
    let mut output = VecSink::new(limits.max_output_bytes);
    typed::write_tape_document(&document, &mut output, limits).unwrap();
    output.into_vec()
}

fn reference_borrowed(raw: &[u8], limits: EncodeLimits) -> Vec<u8> {
    let document = build::parse_borrowed(raw, Limits::strict_default()).unwrap();
    let mut output = VecSink::new(limits.max_output_bytes);
    typed::write_borrowed_document(&document, &mut output, limits).unwrap();
    output.into_vec()
}

#[test]
fn direct_tape_slice_matches_writer_for_owned_and_borrowed_inputs() {
    let cases: &[&[u8]] = &[
        b" null \n",
        r#" { "plain" : "utf8 ✓", "escaped\"key" : "quote: \" slash: \\ line: \n tab: \t", "n": -0.00120E+04 } "#.as_bytes(),
        br#"[[],{},[true,false,null],{"x":[],"y":{"z":"\u0001\u2028\ud83d\ude00"}}]"#,
        br#" { "rows" : [ [ 1.00e+2 , -0 ], [3, -4.5E-6] ] } "#,
        br#"{"wide":1234567890123456789012345678901234567890,"empty":""}"#,
    ];
    let limits = EncodeLimits::default();
    for raw in cases {
        let expected = reference_owned(raw, limits);
        let owned = build::parse(Payload::from_slice(raw), Limits::strict_default()).unwrap();
        let borrowed = build::parse_borrowed(raw, Limits::strict_default()).unwrap();
        assert_eq!(
            typed::encode_tape_document_to_vec(&owned, limits).unwrap(),
            expected
        );
        assert_eq!(
            typed::encode_borrowed_document_to_vec(&borrowed, limits).unwrap(),
            expected
        );
        assert_eq!(reference_borrowed(raw, limits), expected);
    }
}

#[test]
fn direct_tape_two_number_rows_keep_lexemes_and_item_limits() {
    let raw = br#" { "rows" : [ [ 1.00e+2 , -0 ], [3, -4.5E-6] ] } "#;
    let expected = br#"{"rows":[[1.00e+2,-0],[3,-4.5E-6]]}"#;
    let owned = build::parse(Payload::from_slice(raw), Limits::strict_default()).unwrap();
    let borrowed = build::parse_borrowed(raw, Limits::strict_default()).unwrap();
    assert_eq!(
        typed::encode_tape_document_to_vec(&owned, EncodeLimits::default()).unwrap(),
        expected
    );
    assert_eq!(
        typed::encode_borrowed_document_to_vec(&borrowed, EncodeLimits::default()).unwrap(),
        expected
    );
    let row_item_limit = EncodeLimits {
        max_container_items: 1,
        ..EncodeLimits::default()
    };
    assert_eq!(
        typed::encode_tape_document_to_vec(&owned, row_item_limit)
            .unwrap_err()
            .code,
        ErrorCode::BudgetExhausted
    );
    assert_eq!(
        typed::encode_borrowed_document_to_vec(&borrowed, row_item_limit)
            .unwrap_err()
            .code,
        ErrorCode::BudgetExhausted
    );
}

#[test]
fn direct_tape_slice_preserves_encoder_limit_errors() {
    let raw = br#"{"a":["long",12345]}"#;
    let owned = build::parse(Payload::from_slice(raw), Limits::strict_default()).unwrap();
    let borrowed = build::parse_borrowed(raw, Limits::strict_default()).unwrap();
    let limits = [
        EncodeLimits {
            max_output_bytes: 5,
            ..EncodeLimits::default()
        },
        EncodeLimits {
            max_depth: 1,
            ..EncodeLimits::default()
        },
        EncodeLimits {
            max_container_items: 0,
            ..EncodeLimits::default()
        },
        EncodeLimits {
            max_string_bytes: 2,
            ..EncodeLimits::default()
        },
        EncodeLimits {
            max_number_bytes: 2,
            ..EncodeLimits::default()
        },
    ];
    for limits in limits {
        let direct = typed::encode_tape_document_to_vec(&owned, limits).unwrap_err();
        let borrowed_direct =
            typed::encode_borrowed_document_to_vec(&borrowed, limits).unwrap_err();
        let mut sink = VecSink::new(limits.max_output_bytes);
        let streamed = typed::write_tape_document(&owned, &mut sink, limits).unwrap_err();
        let mut borrowed_sink = VecSink::new(limits.max_output_bytes);
        let borrowed_streamed =
            typed::write_borrowed_document(&borrowed, &mut borrowed_sink, limits).unwrap_err();
        assert_eq!(direct.code, streamed.code);
        assert_eq!(borrowed_direct.code, borrowed_streamed.code);
    }
    assert_eq!(
        ErrorCode::BudgetExhausted,
        typed::encode_tape_document_to_vec(
            &owned,
            EncodeLimits {
                max_number_bytes: 2,
                ..EncodeLimits::default()
            },
        )
        .unwrap_err()
        .code
    );
}
