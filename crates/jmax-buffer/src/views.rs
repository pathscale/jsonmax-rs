//! Checked borrows of raw payload and decoded arena storage.
//!
//! A reference supplies coordinates and a source, but no owner identity. The
//! caller selects the owner against which it is resolved. These views check
//! storage bounds and string metadata without authenticating provenance,
//! interpreting JSON escapes, or issuing validation proofs.

use super::{
    checked_range, checked_span, ByteView, Error, ErrorCode, ErrorSource,
    OwnerBytes, ParsedMessage, Result, Span32, SpanSource, StrRef, StrView,
};

impl<'a> OwnerBytes<'a> {
    /// Borrows both buffers after checking their lengths fit the ABI.
    ///
    /// Empty buffers are valid. No configured admission limit or content
    /// validation is applied; each buffer may contain arbitrary bytes.
    pub fn new(raw: &'a [u8], decoded: &'a [u8]) -> Result<Self> {
        validate_owner_lengths(raw.len(), decoded.len())?;
        Ok(Self { raw, decoded })
    }

    /// Borrows the message's actual buffers without consulting its proof.
    ///
    /// Subsequent string lookups still validate UTF-8 and scalar counts, even
    /// when the reference did not originate in the message's validated values.
    pub fn from_message<T>(message: &'a ParsedMessage<T>) -> Result<Self> {
        Self::new(message.raw_bytes(), message.decoded_bytes())
    }

    /// Returns the entire raw payload borrowed from the owner.
    pub fn raw_bytes(&self) -> &'a [u8] {
        self.raw
    }

    /// Returns all logical decoded bytes borrowed from the owner.
    pub fn decoded_bytes(&self) -> &'a [u8] {
        self.decoded
    }

    /// Checks a span against exactly the selected buffer and borrows its bytes.
    ///
    /// Empty spans at the buffer end are valid. Byte views do not require UTF-8
    /// boundaries. The returned borrow can outlive this wrapper, but cannot
    /// outlive the underlying storage.
    pub fn view(&self, source: SpanSource, span: Span32) -> Result<ByteView<'a>> {
        let buffer = match source {
            SpanSource::RawPayload => self.raw,
            SpanSource::DecodedArena => self.decoded,
        };
        let range = checked_range(span, buffer.len())?;
        Ok(ByteView { bytes: &buffer[range], span, source })
    }

    /// Resolves a string after checking bounds, UTF-8, and Unicode scalar count.
    ///
    /// Length metadata counts Unicode scalars, while the span and returned
    /// view's length count UTF-8 bytes. Validation examines only this span, so
    /// unrelated bytes in either buffer need not be valid UTF-8 or JSON.
    pub fn string(&self, reference: StrRef) -> Result<StrView<'a>> {
        let bytes = self.view(reference.source, reference.span)?;
        let text = match core::str::from_utf8(bytes.as_bytes()) {
            Ok(text) => text,
            Err(error) => {
                // The checked view bounds valid_up_to by span.len, and the
                // span's end fits u32. Keep the conversion and addition checked
                // as well, so diagnostic construction never truncates or wraps.
                let invalid_span = || Error::new(
                    ErrorCode::InvalidToken,
                    ErrorSource::Decode,
                    reference.span.offset,
                    "buffer span is out of bounds",
                );
                let valid_up_to = u32::try_from(error.valid_up_to())
                    .map_err(|_| invalid_span())?;
                let byte_offset = reference.span.offset.checked_add(valid_up_to)
                    .ok_or_else(invalid_span)?;
                return Err(Error::new(
                    ErrorCode::InvalidUtf8,
                    ErrorSource::Decode,
                    byte_offset,
                    "buffer string is not valid UTF-8",
                ));
            }
        };
        // A scalar occupies at least one byte, so a checked span also bounds
        // its count by u32::MAX. Compare without an unchecked conversion.
        if u32::try_from(text.chars().count()).ok() != Some(reference.scalar_len) {
            return Err(Error::new(
                ErrorCode::ConstraintViolation,
                ErrorSource::Decode,
                reference.span.offset,
                "buffer string scalar count does not match reference",
            ));
        }
        Ok(StrView { text, reference })
    }
}

// Separate length-only admission permits boundary tests without huge buffers or
// manufacturing invalid slices. Raw admission intentionally precedes decoded.
fn validate_owner_lengths(raw_len: usize, decoded_len: usize) -> Result<()> {
    checked_span(0, raw_len, SpanSource::RawPayload)?;
    checked_span(0, decoded_len, SpanSource::DecodedArena)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Arc, DecodedArena, Payload, Vec, MAX_BUFFER_BYTES};

    const SOURCES: [SpanSource; 2] = [
        SpanSource::RawPayload,
        SpanSource::DecodedArena,
    ];

    fn reference(source: SpanSource, offset: u32, len: u32, scalar_len: u32) -> StrRef {
        StrRef { span: Span32::new(offset, len), source, scalar_len }
    }

    fn assert_error(error: Error, code: ErrorCode, offset: u32, detail: &'static str) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.byte_offset, offset);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert_eq!(error.detail, detail);
    }

    fn assert_bounds_error(error: Error, offset: u32) {
        assert_error(
            error, ErrorCode::InvalidToken, offset,
            "buffer span is out of bounds",
        );
    }

    #[test]
    fn owner_lengths_accept_zero_and_the_inclusive_abi_boundary() {
        for len in [0, 1, 64 * 1024 * 1024, MAX_BUFFER_BYTES - 1, MAX_BUFFER_BYTES] {
            assert_eq!(validate_owner_lengths(len, 0), Ok(()));
            assert_eq!(validate_owner_lengths(0, len), Ok(()));
            assert_eq!(validate_owner_lengths(len, len), Ok(()));
        }
    }

    #[test]
    fn oversized_owner_lengths_have_source_specific_errors_without_allocating() {
        if let Some(too_large) = MAX_BUFFER_BYTES.checked_add(1) {
            for len in [too_large, usize::MAX] {
                for decoded_len in [0, len] {
                    assert_error(
                        validate_owner_lengths(len, decoded_len).unwrap_err(),
                        ErrorCode::PayloadTooLarge, u32::MAX,
                        "buffer span exceeds ABI range",
                    );
                }
                assert_error(
                    validate_owner_lengths(0, len).unwrap_err(),
                    ErrorCode::DecodedArenaOverflow, u32::MAX,
                    "buffer span exceeds ABI range",
                );
            }
        } else {
            // On 32-bit targets every possible slice length fits the ABI.
            assert_eq!(validate_owner_lengths(usize::MAX, usize::MAX), Ok(()));
        }
    }

    #[test]
    fn construction_borrows_arbitrary_bytes_without_content_validation() {
        let raw = [0xff, 0, b'{', b'\\'];
        let decoded = [0x80, 0xfe];
        let owner = OwnerBytes::new(&raw, &decoded).unwrap();
        assert_eq!(owner.raw_bytes(), raw);
        assert_eq!(owner.decoded_bytes(), decoded);
        assert_eq!(owner.raw_bytes().as_ptr(), raw.as_ptr());
        assert_eq!(owner.decoded_bytes().as_ptr(), decoded.as_ptr());
        let cloned = owner;
        assert_eq!(cloned.raw_bytes().as_ptr(), owner.raw_bytes().as_ptr());
        assert_eq!(cloned.decoded_bytes().as_ptr(), owner.decoded_bytes().as_ptr());
    }

    #[test]
    fn empty_owners_allow_only_empty_zero_offset_spans() {
        let owner = OwnerBytes::new(&[], &[]).unwrap();
        assert!(owner.raw_bytes().is_empty());
        assert!(owner.decoded_bytes().is_empty());
        for source in SOURCES {
            let bytes = owner.view(source, Span32::new(0, 0)).unwrap();
            assert!(bytes.is_empty());
            assert_eq!(bytes.len(), 0);
            assert_eq!(bytes.span(), Span32::new(0, 0));
            assert_eq!(bytes.source(), source);
            let empty = reference(source, 0, 0, 0);
            let text = owner.string(empty).unwrap();
            assert_eq!(text.as_str(), "");
            assert!(text.is_empty());
            assert_eq!(text.len(), 0);
            assert_eq!(text.reference(), empty);
            for span in [Span32::new(0, 1), Span32::new(1, 0), Span32::new(u32::MAX, 0)] {
                assert_bounds_error(owner.view(source, span).unwrap_err(), span.offset);
            }
        }
    }

    #[test]
    fn views_preserve_source_coordinates_and_allocation_identity() {
        let raw = b"[raw:body]";
        let decoded = b"[new:text]";
        let owner = OwnerBytes::new(raw, decoded).unwrap();
        for (source, buffer, expected) in [
            (SpanSource::RawPayload, &raw[..], &b"body"[..]),
            (SpanSource::DecodedArena, &decoded[..], &b"text"[..]),
        ] {
            let span = Span32::new(5, 4);
            let view = owner.view(source, span).unwrap();
            assert_eq!(view.as_bytes(), expected);
            assert_eq!(view.as_bytes().as_ptr(), buffer[5..9].as_ptr());
            assert_eq!(view.span(), span);
            assert_eq!(view.source(), source);
            assert_eq!(view.len(), 4);
            assert!(!view.is_empty());
            let string_ref = reference(source, 5, 4, 4);
            let text = owner.string(string_ref).unwrap();
            assert_eq!(text.as_str().as_bytes(), expected);
            assert_eq!(text.as_str().as_ptr(), view.as_bytes().as_ptr());
            assert_eq!(text.reference(), string_ref);
        }
    }

    #[test]
    fn every_small_subrange_matches_the_selected_owner_slice() {
        let raw = b"\0\xffraw";
        let decoded = b"\x80decoded\0";
        let owner = OwnerBytes::new(raw, decoded).unwrap();
        for (source, buffer) in [
            (SpanSource::RawPayload, &raw[..]),
            (SpanSource::DecodedArena, &decoded[..]),
        ] {
            for start in 0..=buffer.len() {
                for end in start..=buffer.len() {
                    let span = Span32::new(
                        u32::try_from(start).unwrap(),
                        u32::try_from(end - start).unwrap(),
                    );
                    let view = owner.view(source, span).unwrap();
                    assert_eq!(view.as_bytes(), &buffer[start..end]);
                    assert_eq!(view.as_bytes().as_ptr(), buffer[start..end].as_ptr());
                    assert_eq!(view.span(), span);
                    assert_eq!(view.source(), source);
                    assert_eq!(view.is_empty(), start == end);
                }
            }
        }
    }

    #[test]
    fn source_selection_never_falls_back_to_the_other_buffer() {
        for (raw, decoded, longer_source, shorter_source) in [
            (&b"long"[..], &b"x"[..], SpanSource::RawPayload, SpanSource::DecodedArena),
            (&b"x"[..], &b"long"[..], SpanSource::DecodedArena, SpanSource::RawPayload),
        ] {
            let owner = OwnerBytes::new(raw, decoded).unwrap();
            let span = Span32::new(1, 3);
            assert_eq!(owner.view(longer_source, span).unwrap().as_bytes(), b"ong");
            assert_bounds_error(owner.view(shorter_source, span).unwrap_err(), 1);
            assert_bounds_error(owner.string(reference(shorter_source, 1, 3, 3)).unwrap_err(), 1);
        }
    }

    #[test]
    fn invalid_bounds_and_overflow_precede_string_content_checks() {
        let owner = OwnerBytes::new(b"\xffabc", b"\x80def").unwrap();
        let invalid = [
            Span32::new(0, 5), Span32::new(3, 2), Span32::new(5, 0),
            Span32::new(u32::MAX, 0), Span32::new(u32::MAX, 1),
            Span32::new(u32::MAX - 1, 2), Span32::new(1, u32::MAX),
        ];
        for source in SOURCES {
            for span in invalid {
                let error = owner.view(source, span).unwrap_err();
                assert_bounds_error(error.clone(), span.offset);
                for scalar_len in [0, 1, u32::MAX] {
                    assert_eq!(
                        owner.string(StrRef { span, source, scalar_len }).unwrap_err(),
                        error,
                    );
                }
            }
        }
        assert_eq!(owner.raw_bytes(), b"\xffabc");
        assert_eq!(owner.decoded_bytes(), b"\x80def");
    }

    #[test]
    fn empty_strings_are_valid_at_every_in_bounds_byte_offset() {
        let bytes = "\u{1f600}".as_bytes();
        let owner = OwnerBytes::new(bytes, bytes).unwrap();
        for source in SOURCES {
            // Empty slices are UTF-8 even between bytes of a multibyte scalar.
            for offset in 0..=4 {
                let empty = reference(source, offset, 0, 0);
                let view = owner.string(empty).unwrap();
                assert!(view.is_empty());
                assert_eq!(view.reference(), empty);
                for scalar_len in [1, u32::MAX] {
                    assert_error(
                        owner.string(StrRef { scalar_len, ..empty }).unwrap_err(),
                        ErrorCode::ConstraintViolation, offset,
                        "buffer string scalar count does not match reference",
                    );
                }
            }
            assert_bounds_error(owner.string(reference(source, 5, 0, 0)).unwrap_err(), 5);
        }
    }

    #[test]
    fn unicode_vectors_distinguish_bytes_scalars_and_graphemes() {
        let vectors: &[(&str, u32, u32)] = &[
            ("", 0, 0), ("ascii", 5, 5), ("\0", 1, 1),
            ("\u{7f}", 1, 1), ("\u{80}", 2, 1), ("\u{7ff}", 2, 1),
            ("\u{800}", 3, 1), ("\u{d7ff}", 3, 1), ("\u{e000}", 3, 1),
            ("\u{ffff}", 3, 1), ("\u{10000}", 4, 1), ("\u{10ffff}", 4, 1),
            ("e\u{301}", 3, 2), ("\u{1f469}\u{200d}\u{1f4bb}", 11, 3),
            ("a\u{e9}\u{20ac}\u{1f600}", 10, 4),
        ];
        for &(text, byte_len, scalar_len) in vectors {
            let mut buffer = Vec::from(&b"\xff!"[..]);
            buffer.extend_from_slice(text.as_bytes());
            buffer.push(0x80);
            let owner = OwnerBytes::new(&buffer, &buffer).unwrap();
            for source in SOURCES {
                let string_ref = reference(source, 2, byte_len, scalar_len);
                let view = owner.string(string_ref).unwrap();
                assert_eq!(view.as_str(), text);
                assert_eq!(view.reference(), string_ref);
                assert_eq!(view.len(), usize::try_from(byte_len).unwrap());
                assert_eq!(view.is_empty(), text.is_empty());
                assert_eq!(view.as_str().as_ptr(), buffer[2..].as_ptr());
            }
        }
    }

    #[test]
    fn scalar_count_mismatches_report_the_span_start() {
        let owner = OwnerBytes::new(b"__abc", b"__\xc3\xa9").unwrap();
        for (source, len, correct, wrong_counts) in [
            (SpanSource::RawPayload, 3, 3, [0, 1, 2, 4, u32::MAX]),
            (SpanSource::DecodedArena, 2, 1, [0, 2, 3, 4, u32::MAX]),
        ] {
            let valid = reference(source, 2, len, correct);
            assert!(owner.string(valid).is_ok());
            for scalar_len in wrong_counts {
                assert_error(
                    owner.string(StrRef { scalar_len, ..valid }).unwrap_err(),
                    ErrorCode::ConstraintViolation, 2,
                    "buffer string scalar count does not match reference",
                );
            }
        }
    }

    #[test]
    fn malformed_utf8_reports_the_absolute_start_of_the_invalid_sequence() {
        let vectors: &[(&[u8], u32)] = &[
            (b"\x80", 0), (b"\xbf", 0), (b"\xc0\x80", 0), (b"\xc1\xbf", 0),
            (b"\xc2", 0), (b"\xc2A", 0), (b"\xe0\x80\x80", 0),
            (b"\xe2\x82", 0), (b"\xe2(\xa1", 0), (b"\xed\xa0\x80", 0),
            (b"\xed\xbf\xbf", 0), (b"\xf0\x80\x80\x80", 0),
            (b"\xf0\x9f\x98", 0), (b"\xf4\x90\x80\x80", 0),
            (b"\xf5\x80\x80\x80", 0), (b"\xf8", 0), (b"\xfe", 0), (b"\xff", 0),
            (b"abc\xff", 3), (b"A\xc3\xa9\xf0\x9f", 3),
            (b"\xf0\x9f\x98\x80\x80", 4),
        ];
        for &(invalid, valid_prefix_len) in vectors {
            let mut buffer = Vec::from(&b"\xff!?"[..]);
            buffer.extend_from_slice(invalid);
            buffer.push(0x80);
            let owner = OwnerBytes::new(&buffer, &buffer).unwrap();
            let len = u32::try_from(invalid.len()).unwrap();
            for source in SOURCES {
                assert_eq!(owner.view(source, Span32::new(3, len)).unwrap().as_bytes(), invalid);
                // Invalid UTF-8 must win over any scalar metadata mismatch.
                for scalar_len in [0, 1, u32::MAX] {
                    assert_error(
                        owner.string(reference(source, 3, len, scalar_len)).unwrap_err(),
                        ErrorCode::InvalidUtf8, 3 + valid_prefix_len,
                        "buffer string is not valid UTF-8",
                    );
                }
            }
        }
    }

    #[test]
    fn byte_views_allow_splitting_scalars_but_string_views_reject_it() {
        let buffer = "!\u{e9}\u{20ac}\u{1f600}?".as_bytes();
        let owner = OwnerBytes::new(buffer, buffer).unwrap();
        for source in SOURCES {
            for (offset, width) in [(1u32, 2u32), (3, 3), (6, 4)] {
                assert!(owner.string(reference(source, offset, width, 1)).is_ok());
                for split in 1..width {
                    for span in [
                        Span32::new(offset, split),
                        Span32::new(offset + split, width - split),
                    ] {
                        assert!(owner.view(source, span).is_ok());
                        assert_error(
                            owner.string(StrRef { span, source, scalar_len: 1 }).unwrap_err(),
                            ErrorCode::InvalidUtf8, span.offset,
                            "buffer string is not valid UTF-8",
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn strings_preserve_json_escapes_quotes_and_control_bytes_literally() {
        for text in [r"\uD800", r"\u00e9", r"\n", "\"", "\0\n\r\t", "{not json}"] {
            let owner = OwnerBytes::new(text.as_bytes(), text.as_bytes()).unwrap();
            for source in SOURCES {
                let string_ref = reference(
                    source, 0, u32::try_from(text.len()).unwrap(),
                    u32::try_from(text.chars().count()).unwrap(),
                );
                assert_eq!(owner.string(string_ref).unwrap().as_str(), text);
            }
        }
        let owner = OwnerBytes::new(br"\u00e9", &[]).unwrap();
        assert_error(
            owner.string(reference(SpanSource::RawPayload, 0, 6, 1)).unwrap_err(),
            ErrorCode::ConstraintViolation, 0,
            "buffer string scalar count does not match reference",
        );
    }

    #[test]
    fn references_resolve_against_the_owner_chosen_by_the_caller() {
        let first = OwnerBytes::new(b"cat", b"one").unwrap();
        let second = OwnerBytes::new(b"dog", b"two").unwrap();
        for (source, first_text, second_text) in [
            (SpanSource::RawPayload, "cat", "dog"),
            (SpanSource::DecodedArena, "one", "two"),
        ] {
            let string_ref = reference(source, 0, 3, 3);
            assert_eq!(first.string(string_ref).unwrap().as_str(), first_text);
            assert_eq!(second.string(string_ref).unwrap().as_str(), second_text);
        }
    }

    #[test]
    fn returned_views_and_accessors_outlive_a_temporary_wrapper() {
        let raw = b"raw".to_vec();
        let decoded = b"text".to_vec();
        let (raw_borrow, decoded_borrow, byte_view, str_view) = {
            let owner = OwnerBytes::new(&raw, &decoded).unwrap();
            (
                owner.raw_bytes(), owner.decoded_bytes(),
                owner.view(SpanSource::RawPayload, Span32::new(1, 2)).unwrap(),
                owner.string(reference(SpanSource::DecodedArena, 0, 4, 4)).unwrap(),
            )
        };
        assert_eq!(raw_borrow, b"raw");
        assert_eq!(decoded_borrow, b"text");
        assert_eq!(byte_view.as_bytes(), b"aw");
        assert_eq!(str_view.as_str(), "text");
        assert_eq!(byte_view.as_bytes().as_ptr(), raw[1..].as_ptr());
        assert_eq!(str_view.as_str().as_ptr(), decoded.as_ptr());
    }

    #[test]
    fn shared_and_sliced_bytes_payloads_are_borrowed_without_cloning_owners() {
        let shared: Arc<[u8]> = Arc::from(&b"body"[..]);
        let payload = Payload::Shared(Arc::clone(&shared));
        let before = Arc::strong_count(&shared);
        let owner = OwnerBytes::new(payload.as_slice(), &[]).unwrap();
        let view = owner.string(reference(SpanSource::RawPayload, 0, 4, 4)).unwrap();
        assert_eq!(view.as_str(), "body");
        assert_eq!(view.as_str().as_ptr(), shared.as_ptr());
        assert_eq!(Arc::strong_count(&shared), before);

        let sliced = match Payload::from_vec(b"prefix:body:suffix".to_vec()) {
            Payload::Bytes(bytes) => Payload::Bytes(bytes.slice(7..11)),
            Payload::Shared(_) => panic!("ABI vector constructor must produce Bytes"),
        };
        let owner = OwnerBytes::new(sliced.as_slice(), &[]).unwrap();
        assert_eq!(owner.raw_bytes(), b"body");
        let view = owner.view(SpanSource::RawPayload, Span32::new(0, 4)).unwrap();
        assert_eq!(view.as_bytes().as_ptr(), sliced.as_slice().as_ptr());
        assert_bounds_error(owner.view(SpanSource::RawPayload, Span32::new(4, 1)).unwrap_err(), 4);
    }

    #[test]
    fn arena_references_are_checked_again_after_truncation_and_coordinate_reuse() {
        let mut arena = DecodedArena::new(16);
        arena.append(b"\xff").unwrap();
        let string_ref = arena.append_str("\u{e9}").unwrap();
        assert_eq!(
            OwnerBytes::new(&[], arena.as_slice()).unwrap().string(string_ref).unwrap().as_str(),
            "\u{e9}",
        );
        arena.truncate(2).unwrap();
        let owner = OwnerBytes::new(&[], arena.as_slice()).unwrap();
        assert_bounds_error(owner.string(string_ref).unwrap_err(), 1);
        assert_error(
            owner.string(reference(SpanSource::DecodedArena, 1, 1, 1)).unwrap_err(),
            ErrorCode::InvalidUtf8, 1, "buffer string is not valid UTF-8",
        );
        arena.truncate(1).unwrap();
        arena.append(b"ab").unwrap();
        assert_error(
            OwnerBytes::new(&[], arena.as_slice()).unwrap().string(string_ref).unwrap_err(),
            ErrorCode::ConstraintViolation, 1,
            "buffer string scalar count does not match reference",
        );
        arena.clear();
        assert_bounds_error(
            OwnerBytes::new(&[], arena.as_slice()).unwrap().string(string_ref).unwrap_err(), 1,
        );
    }

    #[test]
    fn from_message_signature_requires_no_traits_on_values() {
        enum Values {}
        // There is no safe ABI constructor for ParsedMessage. Verify its
        // generic borrowing signature without issuing or forging a proof.
        let _constructor: for<'a> fn(&'a ParsedMessage<Values>) -> Result<OwnerBytes<'a>> =
            OwnerBytes::from_message::<Values>;
    }
}
