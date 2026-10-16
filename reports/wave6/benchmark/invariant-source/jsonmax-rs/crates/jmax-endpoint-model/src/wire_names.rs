//! Deterministic wire casing, collision detection, and canonical method names.
//!
//! Conversion uses only ASCII classification and casing. Preservation copies the
//! input bytes exactly, including Unicode, punctuation, and control characters.
//! Collision checks never change the emitted spelling or invent disambiguators.

use super::{model_error, ErrorCode, NameCase, Result, WireName, WireNamePolicy};
use alloc::{collections::BTreeSet, string::String, vec::Vec};

/// Convert a nonempty name according to the shared wire word-boundary rules.
///
/// `Preserve` accepts any nonempty UTF-8 string. Other cases accept ASCII letters
/// and digits, with underscores, hyphens, and ASCII whitespace as separators.
/// Empty separator-delimited words are ignored. An uppercase letter begins a
/// word after a lowercase letter or digit, or between an uppercase letter and a
/// lowercase letter. Digits otherwise stay in their current word.
///
/// # Errors
///
/// Returns `ConstraintViolation` for an empty input, a conversion containing
/// unsupported characters, or a conversion with no remaining words.
pub fn convert_case(name: &str, case: NameCase) -> Result<String> {
    if name.is_empty() {
        return Err(model_error(ErrorCode::ConstraintViolation, "wire name must not be empty"));
    }
    if case == NameCase::Preserve {
        return Ok(String::from(name));
    }
    if !name.is_ascii() {
        return Err(model_error(
            ErrorCode::ConstraintViolation,
            "wire case conversion requires ASCII input",
        ));
    }

    let bytes = name.as_bytes();
    let mut output = String::with_capacity(name.len());
    let mut previous: Option<u8> = None;
    for (index, byte) in name.bytes().enumerate() {
        if byte == b'_' || byte == b'-' || byte.is_ascii_whitespace() {
            previous = None;
            continue;
        }
        if !byte.is_ascii_alphanumeric() {
            return Err(model_error(
                ErrorCode::ConstraintViolation,
                "invalid character in wire case conversion",
            ));
        }

        // Inspect the original spelling, rather than already cased output, so
        // HTTPServer becomes HTTP + Server and HTTP2Server becomes HTTP2 + Server.
        let starts_word = match previous {
            None => true,
            Some(previous) => {
                byte.is_ascii_uppercase()
                    && (previous.is_ascii_lowercase()
                        || previous.is_ascii_digit()
                        || (previous.is_ascii_uppercase()
                            && bytes.get(index + 1).is_some_and(u8::is_ascii_lowercase)))
            }
        };
        let first_word = output.is_empty();
        if starts_word && !first_word {
            match case {
                NameCase::Snake | NameCase::ScreamingSnake => output.push('_'),
                NameCase::Kebab => output.push('-'),
                NameCase::Preserve | NameCase::Camel | NameCase::Pascal => {}
            }
        }
        let rendered = match case {
            NameCase::Preserve => byte,
            NameCase::ScreamingSnake => byte.to_ascii_uppercase(),
            NameCase::Pascal if starts_word => byte.to_ascii_uppercase(),
            NameCase::Camel if starts_word && !first_word => byte.to_ascii_uppercase(),
            NameCase::Snake | NameCase::Kebab | NameCase::Camel | NameCase::Pascal => {
                byte.to_ascii_lowercase()
            }
        };
        output.push(char::from(rendered));
        previous = Some(byte);
    }

    if output.is_empty() {
        return Err(model_error(
            ErrorCode::ConstraintViolation,
            "wire case conversion requires at least one word",
        ));
    }
    Ok(output)
}

/// Resolve names in declaration order, rejecting any collision in their output.
///
/// Comparisons use complete wire strings. With `case_insensitive`, only ASCII
/// letters are folded for comparison; Unicode bytes and emitted names remain
/// unchanged. Sources are copied exactly. An empty input list is valid, and
/// neither successful resolution nor failure mutates the caller's names.
///
/// # Errors
///
/// Propagates `ConstraintViolation` from [`convert_case`]. A collision, including
/// a repeated identical source, returns `DuplicateObjectKey`. Validation stops
/// at the first failing name in declaration order, without returning a partial
/// resolution.
pub fn resolve_wire_names(names: &[String], policy: WireNamePolicy) -> Result<Vec<WireName>> {
    let mut seen = BTreeSet::new();
    let mut resolved = Vec::with_capacity(names.len());
    for source in names {
        let wire = convert_case(source, policy.case)?;
        let key = if policy.case_insensitive {
            wire.to_ascii_lowercase()
        } else {
            wire.clone()
        };
        if !seen.insert(key) {
            return Err(model_error(ErrorCode::DuplicateObjectKey, "duplicate wire name"));
        }
        resolved.push(WireName { source: source.clone(), wire });
    }
    Ok(resolved)
}

/// Form an exact `service.endpoint` key from two canonical model identifiers.
///
/// Components must match `[A-Za-z_][A-Za-z0-9_]*`, excluding the single `_`.
/// No trimming or case conversion is applied.
///
/// # Errors
///
/// Returns `ConstraintViolation` if either component is not an identifier.
pub fn method_name(service: &str, endpoint: &str) -> Result<String> {
    validate_identifier(service)?;
    validate_identifier(endpoint)?;
    let mut method = String::from(service);
    method.push('.');
    method.push_str(endpoint);
    Ok(method)
}

// Keep this grammar identical to registry's identifier validation without
// introducing a dependency on registry construction or its private helpers.
fn validate_identifier(name: &str) -> Result<()> {
    let bytes = name.as_bytes();
    let valid_start = bytes.first().is_some_and(|byte| byte.is_ascii_alphabetic() || *byte == b'_');
    if !valid_start || name == "_"
        || !bytes.iter().all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
    {
        return Err(model_error(ErrorCode::ConstraintViolation, "invalid ASCII identifier"));
    }
    Ok(())
}
