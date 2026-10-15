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
    let reference = node.document.parts.node_metadata.get(node.id.0 as usize)
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
