//! Exact raw spans and cooked scalar views borrowed from an immutable document.
//!
//! Numeric conversion is opt-in and never changes the retained spelling or
//! populates a cache. The builder has already validated JSON number grammar;
//! conversion uses safe, allocation-free core parsers without imposing a new
//! exponent policy on the document.

use super::*;

/// Returns the complete original value, including string quotes and container
/// delimiters. Whitespace inside a container remains part of its raw value.
#[inline]
pub fn raw<'a>(node: Node<'a>) -> Result<&'a [u8]> {
    let span = stored_span(node)?;
    span.slice(node.document.raw()).ok_or_else(|| {
        dom_error(
            ErrorCode::InternalError,
            span.offset,
            "DOM raw value span does not resolve in its payload",
        )
    })
}

/// Returns a scalar view: strings are cooked UTF-8, while numbers retain their
/// exact lexical bytes. Arrays and objects return `TypeMismatch`.
#[inline]
pub fn value<'a>(node: Node<'a>) -> Result<Scalar<'a>> {
    match tag(node)? {
        TAG_NULL => Ok(Scalar::Null),
        TAG_FALSE => Ok(Scalar::Bool(false)),
        TAG_TRUE => Ok(Scalar::Bool(true)),
        TAG_NUMBER => raw(node).map(Scalar::Number),
        TAG_STRING => string(node).map(Scalar::String),
        _ => Err(scalar_error(
            node,
            ErrorCode::TypeMismatch,
            "DOM scalar value requires a scalar node",
        )),
    }
}

/// Borrows a string's cooked contents from the payload or decoded arena.
/// This operation performs no allocation or unescaping.
#[inline]
pub fn string<'a>(node: Node<'a>) -> Result<&'a str> {
    require_tag(node, TAG_STRING, "DOM string access requires a string")?;
    let reference = navigate::string_ref(node).ok_or_else(|| {
        scalar_error(
            node,
            ErrorCode::InternalError,
            "DOM string has no cooked reference",
        )
    })?;
    Ok(node.document.resolve_record_str(reference))
}

/// Reads the boolean encoded in a true or false node's tag.
#[inline]
pub fn boolean(node: Node<'_>) -> Result<bool> {
    match tag(node)? {
        TAG_FALSE => Ok(false),
        TAG_TRUE => Ok(true),
        _ => Err(scalar_error(
            node,
            ErrorCode::TypeMismatch,
            "DOM boolean access requires a boolean",
        )),
    }
}

/// Converts an integer spelling to `u64` with checked range handling.
/// Decimal/exponent spellings return `InvalidNumber`; a minus sign, including
/// `-0`, returns `ConstraintViolation`. Strings are never coerced to numbers.
#[inline]
pub fn u64(node: Node<'_>) -> Result<u64> {
    let text = integer_text(node)?;
    unsigned(node, text)
}

/// Converts an integer spelling to `i64`, accepting `-0` as signed zero.
/// Decimal/exponent spellings return `InvalidNumber`, even for integral values.
#[inline]
pub fn i64(node: Node<'_>) -> Result<i64> {
    let text = integer_text(node)?;
    signed(node, text)
}

/// Converts the complete number spelling to correctly rounded binary64 using
/// round-to-nearest, ties-to-even. Signed zero and finite underflow are valid;
/// a nonfinite result returns `NumberOverflow`.
#[inline]
pub fn f64(node: Node<'_>) -> Result<f64> {
    let text = number_text(node)?;
    floating(node, text)
}

/// Chooses `U64` for unsigned integer spellings, `I64` for minus integer
/// spellings, and `F64Bits` for decimal/exponent spellings. Integer overflow
/// never falls back to floating point. No decimal policy is inferred.
#[inline]
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
    Ok(navigate::raw_span(node))
}

fn scalar_error(node: Node<'_>, code: ErrorCode, detail: &'static str) -> Error {
    dom_error(code, navigate::raw_span(node).offset, detail)
}

fn tag(node: Node<'_>) -> Result<u8> {
    Ok(node.document.tape()[node.id.0 as usize].tag())
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
        scalar_error(
            node,
            ErrorCode::InternalError,
            "DOM number spelling is not valid UTF-8",
        )
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
        return Err(scalar_error(
            node,
            ErrorCode::InvalidNumber,
            "DOM integer conversion requires an integer spelling",
        ));
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
        return Err(scalar_error(
            node,
            ErrorCode::ConstraintViolation,
            "DOM unsigned conversion rejects a minus spelling",
        ));
    }
    text.parse::<u64>()
        .map_err(|error| integer_error(node, error))
}

fn signed(node: Node<'_>, text: &str) -> Result<i64> {
    text.parse::<i64>()
        .map_err(|error| integer_error(node, error))
}

fn floating(node: Node<'_>, text: &str) -> Result<f64> {
    // Core's parser handles the entire significand and arbitrary exponent
    // lengths, including correctly rounded subnormal and halfway cases.
    let number = text.parse::<f64>().map_err(|_| {
        scalar_error(
            node,
            ErrorCode::InvalidNumber,
            "DOM float conversion failed",
        )
    })?;
    if !number.is_finite() {
        return Err(scalar_error(
            node,
            ErrorCode::NumberOverflow,
            "DOM float conversion produced a nonfinite value",
        ));
    }
    Ok(number)
}
