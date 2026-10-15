//! Owned parsing and typed decoding entry points for the native facade.

use alloc::vec::Vec;

use jmax_abi::syntax::{Event, ScalarKind, ScanStep};
use jmax_cursor::SkipMode;

use super::{
    facade_error, CompileLimits, Decoded, Document, Error, ErrorCode, ErrorSource, JsonDecode,
    NumericPolicy, ParseOptions, Payload, Result, SchemaProvider,
};

/// Copies borrowed input into an owner-bearing payload and parses it with the
/// default syntax and numeric policies.
pub fn parse_slice(input: &[u8]) -> Result<Document> {
    parse_payload(Payload::from_slice(input), ParseOptions::default())
}

/// Adopts an owned byte vector and parses it without another input copy.
pub fn parse_owned(input: Vec<u8>, options: ParseOptions) -> Result<Document> {
    parse_payload(Payload::from_vec(input), options)
}

/// Parses a payload while retaining its original owner in the resulting
/// document.
pub fn parse_payload(input: Payload, options: ParseOptions) -> Result<Document> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;
    let document = jmax_dom::build::parse(input, options.limits)?;

    // The DOM builder validates complete JSON syntax and publishes a flat
    // document. Its number table intentionally remains empty; inspect each
    // number node's original spelling so policy failures retain input offsets.
    let parts = &document.parts;
    for (index, word) in parts.nodes.iter().enumerate() {
        if word.kind() == jmax_dom::TAG_NUMBER {
            let span = *parts.spans.get(index).ok_or_else(|| facade_error(
                ErrorCode::InternalError, ErrorSource::Dom,
                "DOM number node has no corresponding raw span",
            ))?;
            let bytes = span.slice(document.payload.as_slice()).ok_or_else(|| Error::new(
                ErrorCode::InternalError, ErrorSource::Dom, span.offset,
                "DOM number span is outside its payload",
            ))?;
            validate_numeric_lexeme(bytes, span.offset, options.numeric)?;
        }
    }
    Ok(document)
}

/// Validates complete JSON syntax and numeric policy without constructing a
/// DOM. Every cursor event is consumed through `Finished`, so trailing content
/// and malformed unselected subtrees are always checked.
pub fn validate_syntax(input: &[u8], options: ParseOptions) -> Result<()> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;

    let payload = Payload::from_slice(input);
    let mut cursor = jmax_cursor::walk::from_payload(
        payload,
        options.limits,
        SkipMode::Strict,
    )?;
    loop {
        match jmax_cursor::walk::next(&mut cursor)? {
            ScanStep::Emit(Event::Scalar { kind: ScalarKind::Number, span, .. }) => {
                let bytes = span.slice(cursor.raw_bytes()).ok_or_else(|| Error::new(
                    ErrorCode::InternalError, ErrorSource::Cursor, span.offset,
                    "cursor emitted a number span outside its payload",
                ))?;
                validate_numeric_lexeme(bytes, span.offset, options.numeric)?;
            }
            ScanStep::Emit(_) => {}
            ScanStep::Finished => return Ok(()),
            ScanStep::NeedMore => {
                return Err(Error::new(
                    ErrorCode::InternalError, ErrorSource::Cursor,
                    cursor.state().byte_offset,
                    "complete-input cursor requested additional bytes",
                ));
            }
        }
    }
}

/// Parses and strictly decodes a typed value using its native schema.
pub fn from_slice<T: JsonDecode + SchemaProvider>(input: &[u8]) -> Result<Decoded<T>> {
    decode_payload(Payload::from_slice(input), ParseOptions::default())
}

/// Compiles and checks `T`'s schema, validates the complete payload against it,
/// and invokes the decoder only after native schema validation succeeds.
pub fn decode_payload<T: JsonDecode + SchemaProvider>(
    input: Payload,
    options: ParseOptions,
) -> Result<Decoded<T>> {
    ensure_payload_length(input.len())?;
    validate_policy_configuration(options.numeric)?;

    let schema_ir = T::schema_ir();
    let schema = super::endpoint::compile_schema(
        &schema_ir,
        CompileLimits::default_limits(),
    )?;
    if schema.fingerprint() != T::schema_fingerprint() {
        return Err(facade_error(
            ErrorCode::SchemaImageInvalid,
            ErrorSource::SchemaCompile,
            "type schema fingerprint does not match its compiled schema",
        ));
    }

    // Endpoint validation owns the proof publication and performs complete
    // syntax, numeric, and schema checks while retaining the payload and arena.
    let slots = super::endpoint::validate_payload(input, &schema, options)?;
    let value = T::decode_slots(&slots)?;
    Ok(Decoded { slots, value })
}

fn ensure_payload_length(length: usize) -> Result<()> {
    if u32::try_from(length).is_err() {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Lexer,
            u32::MAX,
            "payload exceeds the 32-bit ABI length limit",
        ));
    }
    Ok(())
}

fn validate_policy_configuration(policy: NumericPolicy) -> Result<()> {
    if policy.max_exponent_abs < 0 {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::Lexer,
            0,
            "numeric exponent limit cannot be negative",
        ));
    }
    Ok(())
}

/// Applies the caller's lexical numeric policy to a number token already
/// accepted by the strict syntax engine.
fn validate_numeric_lexeme(bytes: &[u8], offset: u32, policy: NumericPolicy) -> Result<()> {
    if bytes.is_empty() {
        return Err(Error::new(
            ErrorCode::InternalError, ErrorSource::Lexer, offset,
            "syntax engine emitted an empty number token",
        ));
    }

    let negative = bytes.first() == Some(&b'-');
    let digits_start = usize::from(negative);
    let mut all_zero = true;
    let mut has_fraction_or_exponent = false;
    let mut index = digits_start;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'0'..=b'9' => {
                all_zero &= byte == b'0';
                index += 1;
            }
            b'.' => {
                has_fraction_or_exponent = true;
                index += 1;
            }
            b'e' | b'E' => {
                has_fraction_or_exponent = true;
                let exponent_marker = index;
                index += 1;
                let exponent_negative = match bytes.get(index) {
                    Some(b'-') => { index += 1; true }
                    Some(b'+') => { index += 1; false }
                    _ => false,
                };
                let first_digit = index;
                let mut magnitude: u32 = 0;
                let limit = policy.max_exponent_abs as u32;
                while let Some(b'0'..=b'9') = bytes.get(index) {
                    let digit = (bytes[index] - b'0') as u32;
                    if magnitude > limit / 10
                        || (magnitude == limit / 10 && digit > limit % 10)
                    {
                        return Err(Error::new(
                            ErrorCode::ConstraintViolation,
                            ErrorSource::Lexer,
                            offset.saturating_add(first_digit as u32),
                            "numeric exponent exceeds the configured limit",
                        ));
                    }
                    magnitude = magnitude * 10 + digit;
                    index += 1;
                }
                if index == first_digit {
                    return Err(Error::new(
                        ErrorCode::InternalError, ErrorSource::Lexer,
                        offset.saturating_add(exponent_marker as u32),
                        "syntax engine emitted an incomplete exponent",
                    ));
                }
                let _signed_exponent = if exponent_negative {
                    -(magnitude as i32)
                } else {
                    magnitude as i32
                };
                break;
            }
            _ => {
                return Err(Error::new(
                    ErrorCode::InternalError, ErrorSource::Lexer,
                    offset.saturating_add(index as u32),
                    "syntax engine emitted an invalid number byte",
                ));
            }
        }
    }

    if negative && policy.reject_negative_zero && all_zero {
        return Err(Error::new(
            ErrorCode::ConstraintViolation, ErrorSource::Lexer, offset,
            "negative zero is rejected by the numeric policy",
        ));
    }

    if has_fraction_or_exponent && policy.reject_non_finite_float {
        let text = core::str::from_utf8(bytes).map_err(|_| Error::new(
            ErrorCode::InternalError, ErrorSource::Lexer, offset,
            "syntax engine emitted non-UTF-8 number bytes",
        ))?;
        if text.parse::<f64>().map_or(true, |value| !value.is_finite()) {
            return Err(Error::new(
                ErrorCode::NumberOverflow, ErrorSource::Lexer, offset,
                "number is outside the finite binary64 range",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options_with_limits(limits: super::super::Limits) -> ParseOptions {
        ParseOptions { limits, numeric: NumericPolicy::strict_default() }
    }

    #[test]
    fn parses_scalar_and_nested_documents_with_owned_payloads() {
        let scalar = parse_slice(b"  null \n").unwrap();
        assert_eq!(scalar.payload.as_slice(), b"  null \n");
        assert_eq!(scalar.parts.nodes.len(), 1);
        assert_eq!(scalar.parts.spans[0], jmax_abi::Span32::new(2, 4));

        let mut bytes = br#"{"a":[true,2],"b":"x\u00e9"}"#.to_vec();
        let document = parse_owned(bytes.clone(), ParseOptions::default()).unwrap();
        bytes[0] = b'!';
        assert_eq!(document.payload.as_slice(), br#"{"a":[true,2],"b":"x\u00e9"}"#);
        assert!(document.parts.nodes.len() >= 6);
    }

    #[test]
    fn syntax_validation_accepts_valid_edges_and_checks_entire_input() {
        for input in [b"0".as_slice(), b"[]", b"{}", b"[1,-2.5e+2]", b"\t\r\n true "] {
            validate_syntax(input, ParseOptions::default()).unwrap();
        }
        let error = validate_syntax(b"null false", ParseOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::TrailingContent);
    }

    #[test]
    fn empty_and_whitespace_only_inputs_are_unexpected_eof() {
        for input in [b"".as_slice(), b" \t\r\n"] {
            let error = validate_syntax(input, ParseOptions::default()).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnexpectedEof);
        }
    }

    #[test]
    fn parser_propagates_syntax_utf8_and_duplicate_key_errors() {
        for (input, expected) in [
            (b"[1,]".as_slice(), ErrorCode::InvalidSyntax),
            (b"\xff".as_slice(), ErrorCode::InvalidUtf8),
            (br#"{"x":1,"x":2}"#.as_slice(), ErrorCode::DuplicateObjectKey),
        ] {
            let error = parse_slice(input).unwrap_err();
            assert_eq!(error.code, expected);
        }
    }

    #[test]
    fn numeric_policy_checks_exponent_negative_zero_and_finite_range() {
        let error = validate_syntax(b"-0.0", ParseOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.byte_offset, 0);

        let error = validate_syntax(b"1e309", ParseOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.byte_offset, 2);

        let mut options = ParseOptions::default();
        options.numeric.max_exponent_abs = 400;
        let error = validate_syntax(b"1e309", options).unwrap_err();
        assert_eq!(error.code, ErrorCode::NumberOverflow);
        assert_eq!(error.byte_offset, 0);

        let permissive = NumericPolicy { reject_negative_zero: false, ..NumericPolicy::strict_default() };
        validate_syntax(b"-0.0", ParseOptions { numeric: permissive, ..ParseOptions::default() }).unwrap();
    }

    #[test]
    fn configured_syntax_limits_are_forwarded_and_enforced() {
        let mut limits = super::super::Limits::strict_default();
        limits.max_depth = 1;
        let error = validate_syntax(b"[[0]]", options_with_limits(limits)).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);

        let mut limits = super::super::Limits::strict_default();
        limits.max_array_items = 1;
        let error = parse_slice(b"[0,1]").and_then(|_| validate_syntax(b"[0,1]", options_with_limits(limits))).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn numeric_validator_handles_zero_exponent_and_boundary_values() {
        let strict = NumericPolicy::strict_default();
        validate_numeric_lexeme(b"0e308", 11, strict).unwrap();
        validate_numeric_lexeme(b"1e-308", 0, strict).unwrap();
        assert_eq!(
            validate_numeric_lexeme(b"1e309", 7, strict).unwrap_err().byte_offset,
            9,
        );
        assert_eq!(
            validate_numeric_lexeme(b"-0", 4, strict).unwrap_err().byte_offset,
            4,
        );
        let allow_negative_zero = NumericPolicy { reject_negative_zero: false, ..strict };
        validate_numeric_lexeme(b"-0.000e0", 0, allow_negative_zero).unwrap();
    }

    #[test]
    fn invalid_numeric_policy_is_rejected_before_input_traversal() {
        let options = ParseOptions {
            numeric: NumericPolicy { max_exponent_abs: -1, ..NumericPolicy::strict_default() },
            ..ParseOptions::default()
        };
        let error = validate_syntax(b"null", options).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.byte_offset, 0);
    }
}
