//! UTF-8 evidence tied to the immutable bytes that were actually validated.
//!
//! A proof stores a borrowed string, never an owner identifier or a reconstructed
//! pointer. Copying it preserves the same borrow. Slices use proof-relative byte
//! offsets; payload validation reports errors relative to the payload.

use core::ops::Range;

use super::{checked_len, Error, ErrorCode, ErrorSource, Result, Span32, Utf8Proof, Utf8Summary};
#[cfg(feature = "alloc")]
use super::Payload;

/// Validate the complete input and bind its exact summary to its immutable borrow.
///
/// No bytes are copied, normalized, decoded, or retained independently of the
/// input. Empty input is valid and ASCII-only. Validation uses the same safe
/// backend as [`super::validate::validate_str`].
///
/// # Errors
/// Input exceeding `u32::MAX` bytes returns `PayloadTooLarge` before validation.
/// Malformed or incomplete UTF-8 returns `InvalidUtf8` at the first malformed
/// sequence's leading byte, relative to `bytes`.
///
/// A proof cannot outlive its input:
///
/// ```compile_fail
/// let proof;
/// {
///     let bytes = [b'a', b'b'];
///     proof = jmax_utf8::proof::validate(&bytes).unwrap();
/// }
/// assert_eq!(proof.as_str(), "ab");
/// ```
pub fn validate(bytes: &[u8]) -> Result<Utf8Proof<'_>> {
    let text = super::validate::validate_str(bytes)?;
    let byte_len = checked_len(text.len())?;
    let scalar_len = checked_len(text.chars().count())?;
    // Every scalar takes at least one byte. Equal counts therefore mean that
    // all scalars are ASCII, including the empty string.
    let summary = Utf8Summary {
        byte_len,
        scalar_len,
        ascii_only: byte_len == scalar_len,
    };
    Ok(Utf8Proof { text, summary })
}

/// Validate exactly `span` within `owner` and borrow that payload sub-slice.
///
/// The summary describes only the selected bytes. Bytes outside the span need
/// not be UTF-8. Even an empty span inside an otherwise malformed payload is
/// valid, provided it is within bounds. This operation itself does not allocate.
///
/// # Errors
/// Checks the complete owner length first. Span arithmetic overflow returns
/// `PayloadTooLarge` at `span.offset`; an out-of-bounds span returns
/// `UnexpectedEof` there. UTF-8 errors report the malformed sequence's leading
/// byte relative to the owner, including sequences cut short by the span.
#[cfg(feature = "alloc")]
pub fn validate_span<'a>(owner: &'a Payload, span: Span32) -> Result<Utf8Proof<'a>> {
    let bytes = owner.as_slice();
    let range = checked_range(bytes.len(), span)?;
    let selected = bytes.get(range).ok_or_else(|| out_of_bounds(span.offset))?;
    validate(selected).map_err(|error| rebase_error(error, span.offset))
}

/// Borrow a string view using byte offsets relative to the proof's bytes.
///
/// Both endpoints must be Unicode scalar boundaries. Empty ranges are allowed
/// at any such boundary, including the end. The returned view borrows the
/// original bytes and may outlive the proof value itself.
///
/// # Errors
/// Span arithmetic overflow returns `PayloadTooLarge` at `span.offset`.
/// Out-of-bounds spans return `UnexpectedEof` there, before boundary checks.
/// A non-boundary endpoint returns `InvalidUtf8` at that endpoint, checking the
/// start before the end. All offsets are relative to the proof, even when it
/// was obtained from a nonzero payload offset.
pub fn slice<'a>(proof: &Utf8Proof<'a>, span: Span32) -> Result<&'a str> {
    let text = proof.as_str();
    let range = checked_range(text.len(), span)?;
    if !text.is_char_boundary(range.start) {
        return Err(non_boundary(span.offset));
    }
    // Bounds and the starting boundary are already checked. Safe str slicing
    // can now fail only because the exclusive end splits a scalar.
    text.get(range).ok_or_else(|| non_boundary(span.offset + span.len))
}

fn checked_range(byte_len: usize, span: Span32) -> Result<Range<usize>> {
    let byte_len = checked_len(byte_len)?;
    let end = span.end().ok_or_else(|| span_overflow(span.offset))?;
    if end > byte_len {
        return Err(out_of_bounds(span.offset));
    }
    // Both endpoints are bounded by the original usize length. Checked
    // conversions also keep this code portable to narrower usize targets.
    let start = usize::try_from(span.offset).map_err(|_| out_of_bounds(span.offset))?;
    let end = usize::try_from(end).map_err(|_| out_of_bounds(span.offset))?;
    Ok(start..end)
}

fn span_overflow(offset: u32) -> Error {
    Error::new(
        ErrorCode::PayloadTooLarge,
        ErrorSource::Utf8,
        offset,
        "UTF-8 span exceeds the 32-bit ABI offset limit",
    )
}

fn out_of_bounds(offset: u32) -> Error {
    Error::new(
        ErrorCode::UnexpectedEof,
        ErrorSource::Utf8,
        offset,
        "UTF-8 span is outside the borrowed bytes",
    )
}

fn non_boundary(offset: u32) -> Error {
    Error::new(
        ErrorCode::InvalidUtf8,
        ErrorSource::Utf8,
        offset,
        "UTF-8 slice endpoint is not a Unicode scalar boundary",
    )
}

#[cfg(feature = "alloc")]
fn rebase_error(mut error: Error, offset: u32) -> Error {
    if error.code == ErrorCode::InvalidUtf8 {
        // Validated owner bounds imply this addition fits. Keep it checked so
        // even error translation never relies on wrapping offset arithmetic.
        match offset.checked_add(error.byte_offset) {
            Some(absolute) => error.byte_offset = absolute,
            None => return span_overflow(offset),
        }
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error(error: &Error, code: ErrorCode, offset: u32) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Utf8);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_valid(bytes: &[u8], scalars: u32, ascii_only: bool) {
        let proof = validate(bytes).unwrap();
        assert_eq!(proof.as_bytes(), bytes);
        assert_eq!(proof.as_bytes().as_ptr(), bytes.as_ptr());
        assert_eq!(proof.summary(), Utf8Summary {
            byte_len: u32::try_from(bytes.len()).unwrap(),
            scalar_len: scalars,
            ascii_only,
        });
        assert_eq!(proof.summary(), crate::validate::validate(bytes).unwrap());
        let copied = proof;
        assert_eq!(copied.as_str().as_ptr(), proof.as_str().as_ptr());
        assert_eq!(copied.summary(), proof.summary());
    }

    #[test]
    fn empty_and_every_ascii_byte_have_exact_borrowed_summaries() {
        assert_valid(b"", 0, true);
        assert_eq!(validate(b"").unwrap().summary(), Utf8Summary::default());
        let mut bytes = [0u8; 128];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::try_from(index).unwrap();
        }
        assert_valid(&bytes, 128, true);
        assert_valid(br"\uD800\uDC00", 12, true);
    }

    #[test]
    fn scalar_limits_and_unmodified_unicode_are_counted_exactly() {
        for scalar in [
            '\u{80}', '\u{7ff}', '\u{800}', '\u{d7ff}', '\u{e000}',
            '\u{fdd0}', '\u{fffe}', '\u{ffff}', '\u{10000}', '\u{10ffff}',
        ] {
            let mut bytes = [0u8; 4];
            assert_valid(scalar.encode_utf8(&mut bytes).as_bytes(), 1, false);
        }
        assert_valid("A\u{e9}\u{20ac}\u{1f600}".as_bytes(), 4, false);
        assert_valid("\u{feff}e\u{301}\u{fffd}".as_bytes(), 4, false);
        assert_valid("\u{1f469}\u{200d}\u{1f4bb}".as_bytes(), 3, false);
    }

    #[test]
    fn malformed_and_truncated_inputs_never_issue_a_proof() {
        let cases: &[&[u8]] = &[
            b"\x80", b"\xbf", b"\xc0\x80", b"\xc1\xbf",
            b"\xe0\x80\x80", b"\xe0\x9f", b"\xed\xa0\x80", b"\xed\xbf\xbf",
            b"\xf0\x8f\xbf\xbf", b"\xf4\x90\x80\x80", b"\xf5\x80\x80\x80",
            b"\xf8\x88\x80\x80\x80", b"\xff", b"\xc2", b"\xe2", b"\xe2\x82",
            b"\xf0", b"\xf0\x9f", b"\xf0\x9f\x98", b"\xc2!", b"\xe2\x82!",
            b"\xf0\x9f!\x80", b"\xf0\x9f\x98!",
        ];
        for &invalid in cases {
            let mut storage = [0u8; 16];
            let prefix = "a\u{e9}".as_bytes();
            storage[..prefix.len()].copy_from_slice(prefix);
            storage[prefix.len()..prefix.len() + invalid.len()].copy_from_slice(invalid);
            for bytes in [invalid, &storage[..prefix.len() + invalid.len()]] {
                let expected = crate::validate::validate_str(bytes).unwrap_err();
                let actual = validate(bytes).unwrap_err();
                assert_eq!(actual, expected);
                assert_error(&actual, ErrorCode::InvalidUtf8,
                    if bytes.len() == invalid.len() { 0 } else { 3 });
            }
        }
        assert_error(&validate(b"a\xe2\x82!\xff").unwrap_err(), ErrorCode::InvalidUtf8, 1);
    }

    #[test]
    fn every_one_and_two_byte_input_matches_core() {
        fn compare(bytes: &[u8]) {
            match core::str::from_utf8(bytes) {
                Ok(text) => assert_valid(bytes,
                    u32::try_from(text.chars().count()).unwrap(), bytes.is_ascii()),
                Err(error) => assert_error(&validate(bytes).unwrap_err(),
                    ErrorCode::InvalidUtf8, u32::try_from(error.valid_up_to()).unwrap()),
            }
        }
        for first in 0u8..=255 {
            compare(&[first]);
            for second in 0u8..=255 {
                compare(&[first, second]);
            }
        }
    }

    #[test]
    fn all_slice_endpoints_follow_scalar_boundaries_and_bounds_precedence() {
        let text = "A\u{a2}\u{20ac}\u{1f600}Z";
        let proof = validate(text.as_bytes()).unwrap();
        for start in 0..=text.len() + 1 {
            for end in start..=text.len() + 2 {
                let span = Span32::new(u32::try_from(start).unwrap(),
                    u32::try_from(end - start).unwrap());
                let result = slice(&proof, span);
                if end > text.len() {
                    assert_error(&result.unwrap_err(), ErrorCode::UnexpectedEof, span.offset);
                } else if !text.is_char_boundary(start) {
                    assert_error(&result.unwrap_err(), ErrorCode::InvalidUtf8, span.offset);
                } else if !text.is_char_boundary(end) {
                    assert_error(&result.unwrap_err(), ErrorCode::InvalidUtf8,
                        u32::try_from(end).unwrap());
                } else {
                    let actual = result.unwrap();
                    assert_eq!(actual, &text[start..end]);
                    assert_eq!(actual.as_ptr(), text.as_bytes()[start..].as_ptr());
                }
            }
        }
        assert_eq!(slice(&validate(b"").unwrap(), Span32::new(0, 0)).unwrap(), "");
    }

    #[test]
    fn overflow_is_distinct_from_out_of_bounds() {
        let proof = validate(b"abc").unwrap();
        for span in [Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX),
            Span32::new(u32::MAX - 1, 2)] {
            assert_error(&slice(&proof, span).unwrap_err(), ErrorCode::PayloadTooLarge, span.offset);
        }
        for span in [Span32::new(u32::MAX, 0), Span32::new(0, u32::MAX),
            Span32::new(4, 0), Span32::new(3, 1)] {
            assert_error(&slice(&proof, span).unwrap_err(), ErrorCode::UnexpectedEof, span.offset);
        }
        if let Ok(maximum) = usize::try_from(u32::MAX) {
            // Test arithmetic at the ABI limit without fabricating large slices.
            assert_eq!(checked_range(maximum, Span32::new(u32::MAX, 0)).unwrap(),
                maximum..maximum);
            assert_eq!(checked_range(maximum, Span32::new(u32::MAX - 1, 1)).unwrap(),
                maximum - 1..maximum);
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn oversized_owner_guard_precedes_span_arithmetic() {
        let maximum = usize::try_from(u32::MAX).unwrap();
        for length in [maximum + 1, usize::MAX] {
            for span in [Span32::new(0, 0), Span32::new(7, u32::MAX)] {
                assert_error(&checked_range(length, span).unwrap_err(),
                    ErrorCode::PayloadTooLarge, u32::MAX);
            }
        }
    }

    #[test]
    fn returned_views_outlive_proof_values_and_remain_bound_to_input() {
        fn view(bytes: &[u8]) -> Result<&str> {
            let proof = validate(bytes)?;
            slice(&proof, Span32::new(1, 2))
        }
        fn copied(bytes: &[u8]) -> Result<Utf8Proof<'_>> {
            let proof = validate(bytes)?;
            let copy = proof;
            Ok(copy)
        }
        let first = [b'a', 0xc2, 0xa2, b'z'];
        let second = [b'b', 0xc2, 0xa3, b'y'];
        let invalid = [b'a', 0xff, 0xff, b'z'];
        let first_proof = copied(&first).unwrap();
        let second_proof = copied(&second).unwrap();
        assert_eq!(view(&first).unwrap(), "\u{a2}");
        assert_eq!(view(&first).unwrap().as_ptr(), first[1..].as_ptr());
        assert_eq!(slice(&second_proof, Span32::new(1, 2)).unwrap(), "\u{a3}");
        assert_eq!(first_proof.as_bytes().as_ptr(), first.as_ptr());
        assert_eq!(second_proof.as_bytes().as_ptr(), second.as_ptr());
        assert_error(&validate(&invalid).unwrap_err(), ErrorCode::InvalidUtf8, 1);
        assert_eq!(slice(&first_proof, Span32::new(1, 2)).unwrap(), "\u{a2}");
    }

    #[test]
    fn unaligned_subslices_validate_only_selected_bytes() {
        let mut storage = [0xff; 144];
        for start in [1, 2, 3, 61, 62, 63, 64, 65, 127, 128, 129] {
            storage.fill(0xff);
            let text = "a\u{a2}\u{20ac}\u{1f600}";
            storage[start..start + text.len()].copy_from_slice(text.as_bytes());
            assert_valid(&storage[start..start + text.len()], 4, false);
            assert_error(&validate(&storage[start..start + text.len() + 1]).unwrap_err(),
                ErrorCode::InvalidUtf8, u32::try_from(text.len()).unwrap());
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn payload_subspan_has_relative_summary_and_exact_owner_borrow() {
        fn view(owner: &Payload) -> Result<&str> {
            let proof = validate_span(owner, Span32::new(2, 10))?;
            slice(&proof, Span32::new(3, 7))
        }
        let bytes = b"\xff!a\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80\xff";
        // Exercise both canonical Payload storage variants without importing
        // their implementation dependencies into this crate.
        for owner in [Payload::from_slice(bytes), Payload::from_vec(bytes.to_vec())] {
            let proof = validate_span(&owner, Span32::new(2, 10)).unwrap();
            assert_eq!(proof.as_str(), "a\u{a2}\u{20ac}\u{1f600}");
            assert_eq!(proof.as_bytes().as_ptr(), owner.as_slice()[2..].as_ptr());
            assert_eq!(proof.summary(), Utf8Summary {
                byte_len: 10, scalar_len: 4, ascii_only: false,
            });
            assert_eq!(view(&owner).unwrap(), "\u{20ac}\u{1f600}");
            assert_eq!(view(&owner).unwrap().as_ptr(), owner.as_slice()[5..].as_ptr());
            assert_error(&slice(&proof, Span32::new(2, 0)).unwrap_err(),
                ErrorCode::InvalidUtf8, 2);
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn every_payload_subspan_matches_independent_validation() {
        let owner = Payload::from_slice(b"\xffa\xc2\xa2\xe2\x82\xac\xf0\x9f\x98\x80!\xfe");
        let bytes = owner.as_slice();
        for start in 0..=bytes.len() {
            for end in start..=bytes.len() {
                let span = Span32::new(u32::try_from(start).unwrap(),
                    u32::try_from(end - start).unwrap());
                match validate(&bytes[start..end]) {
                    Ok(expected) => {
                        let actual = validate_span(&owner, span).unwrap();
                        assert_eq!(actual.as_str(), expected.as_str());
                        assert_eq!(actual.summary(), expected.summary());
                        assert_eq!(actual.as_bytes().as_ptr(), bytes[start..].as_ptr());
                    }
                    Err(mut expected) => {
                        expected.byte_offset += span.offset;
                        assert_eq!(validate_span(&owner, span).unwrap_err(), expected);
                    }
                }
            }
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn payload_bounds_overflow_and_empty_ranges() {
        for owner in [Payload::from_slice(b""), Payload::from_slice(b"\xff\x80!")] {
            let len = u32::try_from(owner.len()).unwrap();
            for offset in 0..=len {
                let proof = validate_span(&owner, Span32::new(offset, 0)).unwrap();
                assert_eq!(proof.summary(), Utf8Summary::default());
                assert!(proof.as_str().is_empty());
            }
            for span in [Span32::new(len + 1, 0), Span32::new(len, 1),
                Span32::new(0, u32::MAX), Span32::new(u32::MAX, 0)] {
                assert_error(&validate_span(&owner, span).unwrap_err(),
                    ErrorCode::UnexpectedEof, span.offset);
            }
            for span in [Span32::new(u32::MAX, 1), Span32::new(1, u32::MAX)] {
                assert_error(&validate_span(&owner, span).unwrap_err(),
                    ErrorCode::PayloadTooLarge, span.offset);
            }
        }
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn error_rebasing_is_checked_and_preserves_diagnostics() {
        let original = Error::new(ErrorCode::InvalidUtf8, ErrorSource::Utf8,
            3, "incomplete UTF-8 sequence");
        let rebased = rebase_error(original.clone(), u32::MAX - 3);
        assert_error(&rebased, ErrorCode::InvalidUtf8, u32::MAX);
        assert_eq!(rebased.detail, original.detail);
        assert_error(&rebase_error(original, u32::MAX - 2),
            ErrorCode::PayloadTooLarge, u32::MAX - 2);
        let other = span_overflow(7);
        assert_eq!(rebase_error(other.clone(), 17), other);
    }
}
