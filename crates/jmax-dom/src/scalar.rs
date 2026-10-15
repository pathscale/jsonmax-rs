//! Exact raw spans and cooked scalar views borrowed from an immutable document.
//!
//! Numeric conversion is opt-in and never changes the retained spelling or
//! populates a cache. The builder has already validated JSON number grammar;
//! conversion uses safe, allocation-free core parsers without imposing a new
//! exponent policy on the document.

use super::*;

/// Returns the complete original value, including string quotes and container
/// delimiters. Whitespace inside a container remains part of its raw value.
pub fn raw<'a>(node: Node<'a>) -> Result<&'a [u8]> {
    let span = stored_span(node)?;
    span.slice(node.document.payload.as_slice()).ok_or_else(|| {
        dom_error(ErrorCode::InternalError, span.offset,
            "DOM raw value span does not resolve in its payload")
    })
}

/// Returns a scalar view: strings are cooked UTF-8, while numbers retain their
/// exact lexical bytes. Arrays and objects return `TypeMismatch`.
pub fn value<'a>(node: Node<'a>) -> Result<Scalar<'a>> {
    match tag(node)? {
        TAG_NULL => Ok(Scalar::Null),
        TAG_FALSE => Ok(Scalar::Bool(false)),
        TAG_TRUE => Ok(Scalar::Bool(true)),
        TAG_NUMBER => raw(node).map(Scalar::Number),
        TAG_STRING => string(node).map(Scalar::String),
        _ => Err(scalar_error(node, ErrorCode::TypeMismatch,
            "DOM scalar value requires a scalar node")),
    }
}

/// Borrows a string's cooked contents from the payload or decoded arena.
/// This operation performs no allocation or unescaping.
pub fn string<'a>(node: Node<'a>) -> Result<&'a str> {
    require_tag(node, TAG_STRING, "DOM string access requires a string")?;
    let reference = node.document.metadata.get(node.id.0 as usize)
        .and_then(|metadata| metadata.string)
        .ok_or_else(|| scalar_error(node, ErrorCode::InternalError,
            "DOM string has no cooked reference"))?;
    node.document.resolve_str(reference).ok_or_else(|| {
        scalar_error(node, ErrorCode::InternalError,
            "DOM cooked string does not resolve in its owner")
    })
}

/// Reads the boolean encoded in a true or false node's tag.
pub fn boolean(node: Node<'_>) -> Result<bool> {
    match tag(node)? {
        TAG_FALSE => Ok(false),
        TAG_TRUE => Ok(true),
        _ => Err(scalar_error(node, ErrorCode::TypeMismatch,
            "DOM boolean access requires a boolean")),
    }
}

/// Converts an integer spelling to `u64` with checked range handling.
/// Decimal/exponent spellings return `InvalidNumber`; a minus sign, including
/// `-0`, returns `ConstraintViolation`. Strings are never coerced to numbers.
pub fn u64(node: Node<'_>) -> Result<u64> {
    let text = integer_text(node)?;
    unsigned(node, text)
}

/// Converts an integer spelling to `i64`, accepting `-0` as signed zero.
/// Decimal/exponent spellings return `InvalidNumber`, even for integral values.
pub fn i64(node: Node<'_>) -> Result<i64> {
    let text = integer_text(node)?;
    signed(node, text)
}

/// Converts the complete number spelling to correctly rounded binary64 using
/// round-to-nearest, ties-to-even. Signed zero and finite underflow are valid;
/// a nonfinite result returns `NumberOverflow`.
pub fn f64(node: Node<'_>) -> Result<f64> {
    let text = number_text(node)?;
    floating(node, text)
}

/// Chooses `U64` for unsigned integer spellings, `I64` for minus integer
/// spellings, and `F64Bits` for decimal/exponent spellings. Integer overflow
/// never falls back to floating point. No decimal policy is inferred.
pub fn converted(node: Node<'_>) -> Result<ConvertedNumber> {
    let text = number_text(node)?;
    if has_fraction_or_exponent(text) {
        floating(node, text).map(|number| ConvertedNumber::F64Bits(number.to_bits()))
    } else if text.as_bytes().first() == Some(&b'-') {
        signed(node, text).map(ConvertedNumber::I64)
    } else {
        unsigned(node, text).map(ConvertedNumber::U64)
    }
}

fn stored_span(node: Node<'_>) -> Result<Span32> {
    node.document.parts.spans.get(node.id.0 as usize).copied()
        .ok_or_else(|| dom_error(ErrorCode::InternalError, 0,
            "DOM node has no raw value span"))
}

fn scalar_error(node: Node<'_>, code: ErrorCode, detail: &'static str) -> Error {
    // A valid handle always has a parallel raw span. If that internal table
    // itself is absent, there is no available raw offset to report.
    let offset = node.document.parts.spans.get(node.id.0 as usize)
        .map_or(0, |span| span.offset);
    dom_error(code, offset, detail)
}

fn tag(node: Node<'_>) -> Result<u8> {
    let word = node.document.parts.nodes.get(node.id.0 as usize)
        .ok_or_else(|| scalar_error(node, ErrorCode::InternalError,
            "DOM scalar handle has no node word"))?;
    let tag = word.kind();
    if NodeKind::from_tag(tag).is_none() {
        return Err(scalar_error(node, ErrorCode::InternalError,
            "DOM scalar handle has an unknown node tag"));
    }
    Ok(tag)
}

fn require_tag(node: Node<'_>, expected: u8, detail: &'static str) -> Result<()> {
    if tag(node)? == expected {
        Ok(())
    } else {
        Err(scalar_error(node, ErrorCode::TypeMismatch, detail))
    }
}

fn number_text<'a>(node: Node<'a>) -> Result<&'a str> {
    require_tag(node, TAG_NUMBER, "DOM numeric access requires a number")?;
    core::str::from_utf8(raw(node)?).map_err(|_| {
        scalar_error(node, ErrorCode::InternalError,
            "DOM number spelling is not valid UTF-8")
    })
}

fn has_fraction_or_exponent(text: &str) -> bool {
    text.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E'))
}

fn integer_text<'a>(node: Node<'a>) -> Result<&'a str> {
    let text = number_text(node)?;
    // Spelling classification precedes the unsigned sign restriction, so
    // negative decimal/exponent spellings also report InvalidNumber.
    if has_fraction_or_exponent(text) {
        return Err(scalar_error(node, ErrorCode::InvalidNumber,
            "DOM integer conversion requires an integer spelling"));
    }
    Ok(text)
}

fn integer_error(node: Node<'_>, error: core::num::ParseIntError) -> Error {
    let code = match error.kind() {
        core::num::IntErrorKind::PosOverflow | core::num::IntErrorKind::NegOverflow => {
            ErrorCode::NumberOverflow
        }
        _ => ErrorCode::InvalidNumber,
    };
    scalar_error(node, code, "DOM integer conversion failed")
}

fn unsigned(node: Node<'_>, text: &str) -> Result<u64> {
    if text.as_bytes().first() == Some(&b'-') {
        return Err(scalar_error(node, ErrorCode::ConstraintViolation,
            "DOM unsigned conversion rejects a minus spelling"));
    }
    text.parse::<u64>().map_err(|error| integer_error(node, error))
}

fn signed(node: Node<'_>, text: &str) -> Result<i64> {
    text.parse::<i64>().map_err(|error| integer_error(node, error))
}

fn floating(node: Node<'_>, text: &str) -> Result<f64> {
    // Core's parser handles the entire significand and arbitrary exponent
    // lengths, including correctly rounded subnormal and halfway cases.
    let number = text.parse::<f64>().map_err(|_| {
        scalar_error(node, ErrorCode::InvalidNumber, "DOM float conversion failed")
    })?;
    if !number.is_finite() {
        return Err(scalar_error(node, ErrorCode::NumberOverflow,
            "DOM float conversion produced a nonfinite value"));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigate::{array, node, object, raw_span, root};

    fn document(input: &[u8]) -> Document {
        build::parse(Payload::from_slice(input), Limits::strict_default()).unwrap()
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode, offset: u32) {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Dom);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    #[test]
    fn raw_roots_cover_all_kinds_and_borrow_exact_payload_ranges() {
        let cases: &[(&[u8], &[u8])] = &[
            (b" \tnull\r\n", b"null"),
            (b" false ", b"false"),
            (b"\ntrue\t", b"true"),
            (b" -0 ", b"-0"),
            (b" 1.2300e+0042 ", b"1.2300e+0042"),
            (br#" "a\n\u0062" "#, br#""a\n\u0062""#),
            (b" [ 0, {} ] \n", b"[ 0, {} ]"),
            (br#" { "a" : [ ] } "#, br#"{ "a" : [ ] }"#),
            (b"[]", b"[]"),
            (b"{}", b"{}"),
            (br#""""#, br#""""#),
        ];
        for &(input, expected) in cases {
            let doc = document(input);
            let handle = root(&doc);
            let bytes = raw(handle).unwrap();
            let span = raw_span(handle);
            assert_eq!(bytes, expected);
            assert_eq!(bytes.as_ptr(), doc.payload.as_slice()[span.offset as usize..].as_ptr());
            assert_eq!(span.len as usize, bytes.len());
            assert_eq!(doc.payload.as_slice(), input);
        }
    }

    #[test]
    fn raw_container_spans_use_node_ids_rather_than_container_indices() {
        let doc = document(br#" {"n":0,"a":[ true, { "s" : "x\n" } ],"o":{}} "#);
        let members = object(root(&doc)).unwrap();
        let a = members.get("a").unwrap();
        assert_eq!(a.id(), NodeId(2));
        assert_eq!(doc.parts.nodes[2].payload_index(), 1);
        assert_eq!(raw(a).unwrap(), br#"[ true, { "s" : "x\n" } ]"#);
        let elements = array(a).unwrap();
        assert_eq!(raw(elements.get(0).unwrap()).unwrap(), b"true");
        let inner = elements.get(1).unwrap();
        assert_eq!(raw(inner).unwrap(), br#"{ "s" : "x\n" }"#);
        let text = object(inner).unwrap().get("s").unwrap();
        assert_eq!(raw(text).unwrap(), br#""x\n""#);
        assert_eq!(string(text).unwrap(), "x\n");
        assert_eq!(raw(members.get("o").unwrap()).unwrap(), b"{}");
    }

    #[test]
    fn scalar_values_preserve_variants_and_exact_number_spelling() {
        let doc = document(br#"[null,false,true,-0,1.2300E+0042,"a\n\u0062"]"#);
        let expected = [
            Scalar::Null, Scalar::Bool(false), Scalar::Bool(true),
            Scalar::Number(b"-0"), Scalar::Number(b"1.2300E+0042"),
            Scalar::String("a\nb"),
        ];
        for (handle, expected) in array(root(&doc)).unwrap().iter().zip(expected) {
            assert_eq!(value(handle).unwrap(), expected);
            if let Scalar::Number(bytes) = value(handle).unwrap() {
                assert_eq!(bytes.as_ptr(), raw(handle).unwrap().as_ptr());
            }
            if let Scalar::String(text) = value(handle).unwrap() {
                assert_eq!(text.as_ptr(), string(handle).unwrap().as_ptr());
            }
        }
    }

    #[test]
    fn strings_cover_unicode_controls_surrogates_and_both_owners() {
        let cases: &[(&[u8], &str, SpanSource)] = &[
            (br#""""#, "", SpanSource::RawPayload),
            (br#""plain""#, "plain", SpanSource::RawPayload),
            ("\"é中😀\"".as_bytes(), "é中😀", SpanSource::RawPayload),
            (br#""\"\\\/\b\f\n\r\t\u0000\u001f""#,
                "\"\\/\u{8}\u{c}\n\r\t\0\u{1f}", SpanSource::DecodedArena),
            (br#""\u00e9\u4e2d\ud83d\ude00""#, "é中😀", SpanSource::DecodedArena),
            (br#""\u0000tail""#, "\0tail", SpanSource::DecodedArena),
            (br#""e\u0301""#, "e\u{301}", SpanSource::DecodedArena),
        ];
        for &(input, expected, source) in cases {
            let doc = document(input);
            let handle = root(&doc);
            let text = string(handle).unwrap();
            let reference = doc.metadata[0].string.unwrap();
            assert_eq!(text, expected);
            assert_eq!(reference.source, source);
            assert_eq!(text.chars().count(), reference.scalar_len as usize);
            let owner = match source {
                SpanSource::RawPayload => doc.payload.as_slice(),
                SpanSource::DecodedArena => &doc.parts.decoded,
            };
            assert_eq!(text.as_ptr(), reference.span.slice(owner).unwrap().as_ptr());
            assert_eq!(raw(handle).unwrap(), input);
        }
    }

    #[test]
    fn every_wrong_kind_reports_type_mismatch_at_the_absolute_start() {
        let cases: &[(&[u8], NodeKind)] = &[
            (b"null", NodeKind::Null), (b"false", NodeKind::Bool),
            (b"true", NodeKind::Bool), (b"123", NodeKind::Number),
            (br#""123""#, NodeKind::String), (b"[]", NodeKind::Array),
            (b"{}", NodeKind::Object),
        ];
        for &(input, kind) in cases {
            let mut padded = Vec::from(&b" \n\t"[..]);
            padded.extend_from_slice(input);
            padded.extend_from_slice(b"\r ");
            let doc = document(&padded);
            let handle = root(&doc);
            if kind != NodeKind::String {
                assert_error(string(handle), ErrorCode::TypeMismatch, 3);
            }
            if kind != NodeKind::Bool {
                assert_error(boolean(handle), ErrorCode::TypeMismatch, 3);
            }
            if kind != NodeKind::Number {
                assert_error(u64(handle), ErrorCode::TypeMismatch, 3);
                assert_error(i64(handle), ErrorCode::TypeMismatch, 3);
                assert_error(f64(handle), ErrorCode::TypeMismatch, 3);
                assert_error(converted(handle), ErrorCode::TypeMismatch, 3);
            }
            if matches!(kind, NodeKind::Array | NodeKind::Object) {
                assert_error(value(handle), ErrorCode::TypeMismatch, 3);
            }
        }
        let doc = document(br#"{"x": ["-0"]}"#);
        let quoted = array(object(root(&doc)).unwrap().get("x").unwrap()).unwrap().get(0).unwrap();
        assert_eq!(raw_span(quoted).offset, 7);
        assert_error(u64(quoted), ErrorCode::TypeMismatch, 7);
        assert_error(i64(quoted), ErrorCode::TypeMismatch, 7);
        assert_error(f64(quoted), ErrorCode::TypeMismatch, 7);
        assert_error(converted(quoted), ErrorCode::TypeMismatch, 7);
        assert_eq!(string(quoted).unwrap(), "-0");
    }

    #[test]
    fn booleans_read_only_their_literal_tags() {
        let doc = document(b"[true,false,true,false]");
        for (handle, expected) in array(root(&doc)).unwrap().iter().zip([true, false, true, false]) {
            assert_eq!(boolean(handle), Ok(expected));
            assert_eq!(value(handle), Ok(Scalar::Bool(expected)));
        }
    }

    #[test]
    fn unsigned_integer_vectors_cover_decimal_lengths_and_range_endpoints() {
        let cases: &[(&[u8], u64)] = &[
            (b"0", 0), (b"1", 1), (b"9", 9), (b"10", 10),
            (b"9999999", 9_999_999), (b"12345678", 12_345_678),
            (b"100000000", 100_000_000),
            (b"1234567890123456", 1_234_567_890_123_456),
            (b"9223372036854775807", i64::MAX as u64),
            (b"9223372036854775808", 1_u64 << 63),
            (b"18446744073709551614", u64::MAX - 1),
            (b"18446744073709551615", u64::MAX),
        ];
        for &(input, expected) in cases {
            let doc = document(input);
            let handle = root(&doc);
            assert_eq!(u64(handle), Ok(expected));
            assert_eq!(converted(handle), Ok(ConvertedNumber::U64(expected)));
            match i64::try_from(expected) {
                Ok(expected) => assert_eq!(i64(handle), Ok(expected)),
                Err(_) => assert_error(i64(handle), ErrorCode::NumberOverflow, 0),
            }
        }
        for input in [b"18446744073709551616".as_slice(), b"99999999999999999999",
            b"100000000000000000000", b"1844674407370955161500000000000000"] {
            let doc = document(input);
            let handle = root(&doc);
            assert_error(u64(handle), ErrorCode::NumberOverflow, 0);
            assert_error(i64(handle), ErrorCode::NumberOverflow, 0);
            assert_error(converted(handle), ErrorCode::NumberOverflow, 0);
            assert!(f64(handle).unwrap().is_finite());
            assert_eq!(value(handle), Ok(Scalar::Number(input)));
        }
    }

    #[test]
    fn signed_integer_boundaries_negative_zero_and_unsigned_sign_rejection() {
        let cases: &[(&[u8], i64)] = &[
            (b"-0", 0), (b"-1", -1), (b"-12345678", -12_345_678),
            (b"-9223372036854775807", i64::MIN + 1),
            (b"-9223372036854775808", i64::MIN),
        ];
        for &(input, expected) in cases {
            let doc = document(input);
            let handle = root(&doc);
            assert_eq!(i64(handle), Ok(expected));
            assert_eq!(converted(handle), Ok(ConvertedNumber::I64(expected)));
            assert_error(u64(handle), ErrorCode::ConstraintViolation, 0);
        }
        for input in [b"-9223372036854775809".as_slice(), b"-18446744073709551616",
            b"-100000000000000000000000000000000"] {
            let doc = document(input);
            let handle = root(&doc);
            assert_error(i64(handle), ErrorCode::NumberOverflow, 0);
            assert_error(converted(handle), ErrorCode::NumberOverflow, 0);
            assert_error(u64(handle), ErrorCode::ConstraintViolation, 0);
            assert_eq!(raw(handle).unwrap(), input);
        }
    }

    #[test]
    fn decimal_and_exponent_spellings_are_never_integer_coercions() {
        for input in [b"0.0".as_slice(), b"-0.0", b"1.0", b"1e0", b"1E+00",
            b"-1e0", b"-0e0", b"100e-2", b"1e9999", b"-1e-9999",
            b"18446744073709551616.0"] {
            let doc = document(input);
            let handle = root(&doc);
            assert_error(u64(handle), ErrorCode::InvalidNumber, 0);
            assert_error(i64(handle), ErrorCode::InvalidNumber, 0);
            assert_eq!(raw(handle).unwrap(), input);
        }
    }

    #[test]
    fn floats_cover_rounding_subnormals_and_signed_zero_with_exact_bits() {
        let cases: &[(&[u8], u64)] = &[
            (b"0", 0), (b"-0", 0x8000_0000_0000_0000),
            (b"-0.0", 0x8000_0000_0000_0000),
            (b"-0E+9999", 0x8000_0000_0000_0000),
            (b"1", 0x3ff0_0000_0000_0000),
            (b"0.1", 0x3fb9_9999_9999_999a),
            (b"-1.5", 0xbff8_0000_0000_0000),
            (b"9007199254740993", 0x4340_0000_0000_0000),
            (b"9007199254740995", 0x4340_0000_0000_0002),
            (b"1.00000000000000011102230246251565404236316680908203125",
                0x3ff0_0000_0000_0000),
            (b"1.00000000000000011102230246251565404236316680908203126",
                0x3ff0_0000_0000_0001),
            (b"1.00000000000000033306690738754696212708950042724609375",
                0x3ff0_0000_0000_0002),
            (b"2.2250738585072014e-308", 0x0010_0000_0000_0000),
            (b"2.225073858507201e-308", 0x000f_ffff_ffff_ffff),
            (b"4.9406564584124654e-324", 1),
            (b"2.4703282292062327e-324", 0),
            (b"2.4703282292062328e-324", 1),
            (b"-4.9406564584124654e-324", 0x8000_0000_0000_0001),
            (b"1e-9999", 0), (b"-1e-9999", 0x8000_0000_0000_0000),
            (b"1.7976931348623157e308", 0x7fef_ffff_ffff_ffff),
            (b"1.7976931348623158e308", 0x7fef_ffff_ffff_ffff),
        ];
        for &(input, expected) in cases {
            let doc = document(input);
            let handle = root(&doc);
            assert_eq!(f64(handle).unwrap().to_bits(), expected, "input: {input:?}");
            if input.iter().any(|&byte| matches!(byte, b'.' | b'e' | b'E')) {
                assert_eq!(converted(handle), Ok(ConvertedNumber::F64Bits(expected)));
            }
            assert_eq!(raw(handle).unwrap(), input);
        }
    }

    #[test]
    fn float_overflow_is_explicit_and_never_discards_the_original_number() {
        for input in [b"1.7976931348623159e308".as_slice(), b"-1.7976931348623159e308",
            b"1e309", b"-1e309", b"1e9999", b"1e999999999999999999999999999999"] {
            let doc = document(input);
            let handle = root(&doc);
            assert_error(f64(handle), ErrorCode::NumberOverflow, 0);
            assert_error(converted(handle), ErrorCode::NumberOverflow, 0);
            assert_eq!(raw(handle).unwrap(), input);
            assert_eq!(value(handle), Ok(Scalar::Number(input)));
        }
    }

    #[test]
    fn exponent_lengths_are_not_restricted_by_an_unrelated_numeric_policy() {
        let cases: &[(&[u8], u64)] = &[
            (b"0e999999999999999999999999999999", 0),
            (b"-0e999999999999999999999999999999", 1_u64 << 63),
            (b"1e-999999999999999999999999999999", 0),
            (b"-1e-999999999999999999999999999999", 1_u64 << 63),
            (b"1e0000000000000000000000000000000000", 1.0_f64.to_bits()),
            (b"1e-00000000000000000000000000000324", 0),
        ];
        for &(input, bits) in cases {
            let doc = document(input);
            assert_eq!(f64(root(&doc)).unwrap().to_bits(), bits);
            assert_eq!(converted(root(&doc)), Ok(ConvertedNumber::F64Bits(bits)));
        }
    }

    #[test]
    fn converted_uses_spelling_to_choose_a_variant_without_float_fallback() {
        let cases: &[(&[u8], ConvertedNumber)] = &[
            (b"0", ConvertedNumber::U64(0)),
            (b"-0", ConvertedNumber::I64(0)),
            (b"18446744073709551615", ConvertedNumber::U64(u64::MAX)),
            (b"-9223372036854775808", ConvertedNumber::I64(i64::MIN)),
            (b"1.0", ConvertedNumber::F64Bits(1.0_f64.to_bits())),
            (b"1E+0", ConvertedNumber::F64Bits(1.0_f64.to_bits())),
            (b"18446744073709551616.0", ConvertedNumber::F64Bits(0x43f0_0000_0000_0000)),
            (b"-0.0", ConvertedNumber::F64Bits(1_u64 << 63)),
        ];
        for &(input, expected) in cases {
            let doc = document(input);
            assert_eq!(converted(root(&doc)), Ok(expected));
        }
        for input in [b"18446744073709551616".as_slice(), b"-9223372036854775809"] {
            let doc = document(input);
            assert!(f64(root(&doc)).is_ok());
            assert_error(converted(root(&doc)), ErrorCode::NumberOverflow, 0);
        }
    }

    #[test]
    fn nested_conversion_errors_report_the_value_start_in_the_document() {
        let doc = document(br#" {"n": [18446744073709551616,-0,1.0,1e9999]} "#);
        let elements = array(object(root(&doc)).unwrap().get("n").unwrap()).unwrap();
        for (index, code) in [ErrorCode::NumberOverflow, ErrorCode::ConstraintViolation,
            ErrorCode::InvalidNumber, ErrorCode::InvalidNumber].into_iter().enumerate() {
            let handle = elements.get(index).unwrap();
            let start = raw_span(handle).offset;
            assert!(start > 0);
            assert_error(u64(handle), code, start);
            assert_error(string(handle), ErrorCode::TypeMismatch, start);
            if index == 3 {
                assert_error(f64(handle), ErrorCode::NumberOverflow, start);
                assert_error(converted(handle), ErrorCode::NumberOverflow, start);
            }
        }
    }

    #[test]
    fn long_significands_and_integer_digit_limits_preserve_raw_access() {
        for length in [19, 20, 21, 31, 32, 63, 64, 127, 128, 1024] {
            let spelling = alloc::vec![b'9'; length];
            let doc = document(&spelling);
            let handle = root(&doc);
            assert_error(i64(handle), ErrorCode::NumberOverflow, 0);
            if length >= 20 {
                assert_error(u64(handle), ErrorCode::NumberOverflow, 0);
                assert_error(converted(handle), ErrorCode::NumberOverflow, 0);
            }
            if length >= 309 {
                assert_error(f64(handle), ErrorCode::NumberOverflow, 0);
            } else {
                assert!(f64(handle).unwrap().is_finite());
            }
            assert_eq!(raw(handle).unwrap(), spelling.as_slice());
        }
        // A long tail smaller than half an ULP must not disturb rounding to 1.
        let mut spelling = Vec::from(&b"1."[..]);
        spelling.resize(1001, b'0');
        spelling.push(b'1');
        let doc = document(&spelling);
        assert_eq!(f64(root(&doc)).unwrap().to_bits(), 1.0_f64.to_bits());
        assert_eq!(raw(root(&doc)).unwrap(), spelling.as_slice());
    }

    #[test]
    fn deterministic_full_width_integer_vectors_convert_exactly() {
        let mut state = 0x4d59_5df4_d0f3_3173_u64;
        for _ in 0..256 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let spelling = alloc::format!("{state}");
            let doc = document(spelling.as_bytes());
            assert_eq!(u64(root(&doc)), Ok(state));
            assert_eq!(converted(root(&doc)), Ok(ConvertedNumber::U64(state)));
            let signed = i64::from_le_bytes(state.to_le_bytes());
            let spelling = alloc::format!("{signed}");
            let doc = document(spelling.as_bytes());
            assert_eq!(i64(root(&doc)), Ok(signed));
            if signed < 0 {
                assert_error(u64(root(&doc)), ErrorCode::ConstraintViolation, 0);
                assert_eq!(converted(root(&doc)), Ok(ConvertedNumber::I64(signed)));
            } else {
                assert_eq!(converted(root(&doc)), Ok(ConvertedNumber::U64(signed as u64)));
            }
        }
    }

    #[test]
    fn repeated_successes_and_failures_leave_all_storage_and_refs_unchanged() {
        let doc = document(br#"[0,-0,1.2500,18446744073709551616,1e9999,"raw","a\n"]"#);
        let saved = doc.clone();
        let payload_pointer = doc.payload.as_slice().as_ptr();
        let decoded_pointer = doc.parts.decoded.as_ptr();
        for _ in 0..4 {
            for handle in array(root(&doc)).unwrap().iter() {
                let before = raw(handle).unwrap();
                let _ = value(handle);
                let _ = boolean(handle);
                let _ = string(handle);
                let _ = u64(handle);
                let _ = i64(handle);
                let _ = f64(handle);
                let _ = converted(handle);
                assert_eq!(raw(handle).unwrap(), before);
                assert_eq!(raw(handle).unwrap().as_ptr(), before.as_ptr());
            }
        }
        assert_eq!(doc.payload.as_slice(), saved.payload.as_slice());
        assert_eq!(doc.payload.as_slice().as_ptr(), payload_pointer);
        assert_eq!(doc.parts.decoded.as_ptr(), decoded_pointer);
        assert_eq!(doc.parts.nodes, saved.parts.nodes);
        assert_eq!(doc.parts.spans, saved.parts.spans);
        assert_eq!(doc.parts.containers, saved.parts.containers);
        assert_eq!(doc.parts.decoded, saved.parts.decoded);
        assert_eq!(doc.metadata, saved.metadata);
        assert!(doc.parts.numbers.is_empty());
        assert!(doc.metadata.iter().all(|metadata| metadata.number_index.is_none()));
    }

    #[test]
    fn cloned_document_views_resolve_in_their_own_retained_owners() {
        let original = document(br#"["raw","a\n",-0.0]"#);
        let cloned = original.clone();
        let original_text = array(root(&original)).unwrap().get(1).unwrap();
        let cloned_text = array(root(&cloned)).unwrap().get(1).unwrap();
        assert_eq!(string(original_text), string(cloned_text));
        assert_ne!(string(original_text).unwrap().as_ptr(), string(cloned_text).unwrap().as_ptr());
        drop(original);
        let values = array(root(&cloned)).unwrap();
        assert_eq!(string(values.get(0).unwrap()), Ok("raw"));
        assert_eq!(string(values.get(1).unwrap()), Ok("a\n"));
        assert_eq!(raw(values.get(1).unwrap()).unwrap(), br#""a\n""#);
        assert_eq!(converted(values.get(2).unwrap()), Ok(ConvertedNumber::F64Bits(1_u64 << 63)));
    }

    #[test]
    fn unresolved_raw_spans_return_internal_errors_without_panics() {
        for span in [Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX),
            Span32::new(100, 0), Span32::new(2, 100)] {
            let mut doc = document(b" 123 ");
            doc.parts.spans[0] = span;
            let handle = root(&doc);
            assert_error(raw(handle), ErrorCode::InternalError, span.offset);
            assert_error(value(handle), ErrorCode::InternalError, span.offset);
            assert_error(u64(handle), ErrorCode::InternalError, span.offset);
            assert_error(i64(handle), ErrorCode::InternalError, span.offset);
            assert_error(f64(handle), ErrorCode::InternalError, span.offset);
            assert_error(converted(handle), ErrorCode::InternalError, span.offset);
        }
        let mut doc = document(b"123");
        doc.parts.spans.clear();
        assert_error(raw(root(&doc)), ErrorCode::InternalError, 0);
        assert_error(converted(root(&doc)), ErrorCode::InternalError, 0);
    }

    #[test]
    fn unresolved_cooked_refs_and_invalid_utf8_are_internal_errors() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for span in [Span32::new(u32::MAX, 1), Span32::new(100, 0), Span32::new(2, 100)] {
                let mut doc = document(br#" "a\n" "#);
                doc.metadata[0].string = Some(StrRef { span, source, scalar_len: 2 });
                assert_error(string(root(&doc)), ErrorCode::InternalError, 1);
                assert_error(value(root(&doc)), ErrorCode::InternalError, 1);
                assert_eq!(raw(root(&doc)).unwrap(), br#""a\n""#);
            }
        }
        let mut doc = document(br#" "a\n" "#);
        doc.parts.decoded[0] = 0xff;
        assert_error(string(root(&doc)), ErrorCode::InternalError, 1);
        doc.metadata[0].string = None;
        assert_error(string(root(&doc)), ErrorCode::InternalError, 1);
        doc.metadata.clear();
        assert_error(value(root(&doc)), ErrorCode::InternalError, 1);
        let mut doc = document(br#" "raw" "#);
        doc.payload = Payload::from_slice(b" \"\xffaw\" ");
        assert_error(string(root(&doc)), ErrorCode::InternalError, 1);
    }

    #[test]
    fn damaged_internal_number_text_and_node_words_return_errors() {
        let mut doc = document(b" 1 ");
        doc.payload = Payload::from_slice(b" \xff ");
        assert_error(f64(root(&doc)), ErrorCode::InternalError, 1);
        assert_error(u64(root(&doc)), ErrorCode::InternalError, 1);
        assert_error(converted(root(&doc)), ErrorCode::InternalError, 1);
        doc.payload = Payload::from_slice(b" x ");
        assert_error(f64(root(&doc)), ErrorCode::InvalidNumber, 1);
        assert_error(i64(root(&doc)), ErrorCode::InvalidNumber, 1);
        assert_error(u64(root(&doc)), ErrorCode::InvalidNumber, 1);
        doc.parts.nodes[0] = NodeWord::pack(u8::MAX, 0, 0);
        assert_error(value(root(&doc)), ErrorCode::InternalError, 1);
        assert_error(boolean(root(&doc)), ErrorCode::InternalError, 1);
        doc.parts.nodes.clear();
        assert_error(value(root(&doc)), ErrorCode::InternalError, 1);
        assert_error(string(root(&doc)), ErrorCode::InternalError, 1);
        let valid = document(b"1");
        assert!(node(&valid, NodeId(NO_NODE)).is_none());
    }
}
