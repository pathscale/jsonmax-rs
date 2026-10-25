use jmax_native::{ErrorCode, ErrorSource, NativeContext, NumericPolicy, VmLimits};
use jmax_native_fixtures::{foreign::native_adapters::decode_quote_as, foreign_targets::{Id, Label, Quote}};

#[test]
fn generated_foreign_adapters_compile_and_convert_validated_fields() {
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    let raw = br#"{"id":"AB","label":"\u0041","price":"1.25000000","side":"Bid"}"#;
    assert_eq!(decode_quote_as(&mut ctx, raw).unwrap(), Quote {
        id: Id(*b"AB"), label: Label("A".into()), price: 125_000_000, side: -2,
    });
    let numeric = br#"{"id":"AB","label":"quote","price":"1.25000000","side":7}"#;
    assert_eq!(decode_quote_as(&mut ctx, numeric).unwrap().side, 7);
}

#[test]
fn conversion_failures_are_decode_constraints_and_schema_errors_take_precedence() {
    let mut ctx = NativeContext::new(VmLimits::default(), NumericPolicy::strict_default());
    for raw in [
        br#"{"id":"ab","label":"quote","price":"1.25000000","side":"Bid"}"#.as_slice(),
        br#"{"id":"AB","label":"","price":"1.25000000","side":"Bid"}"#.as_slice(),
    ] {
        let error = decode_quote_as(&mut ctx, raw).unwrap_err();
        assert_eq!((error.code, error.source), (ErrorCode::ConstraintViolation, ErrorSource::Decode));
    }
    // Both the foreign ID conversion and JSON are invalid: the schema decides first.
    let error = decode_quote_as(&mut ctx,
        br#"{"id":"ab","label":"quote","price":"1.25000000","side":"Bid",}"#,
    ).unwrap_err();
    assert_ne!(error.source, ErrorSource::Decode);
    assert_ne!(error.code, ErrorCode::InternalError);
}
