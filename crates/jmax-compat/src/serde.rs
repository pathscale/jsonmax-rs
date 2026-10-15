//! Direct codec adapters with independent native validation.
//!
//! Applications opt into Serde by implementing [`DirectDeserialize`] and
//! [`DirectSerialize`] on their own wrappers. Their codecs must consume or emit
//! exactly one complete JSON value and check end of input. No general-purpose
//! JSON tree, optional dependency, or feature-dependent fallback is used here.

use alloc::vec::Vec;

use super::{
    BoundedSink, ByteSink, CompatOptions, CompatibilityValidator, DirectDeserialize,
    DirectSerialize, Error, ErrorCode, ErrorSource, JsonDecode, JsonEncode,
    LEGACY_FORMAT_VERSION, Limits, Payload, Result, ValidatedSlots,
};

/// Validate the complete input before invoking an application-owned codec.
///
/// The codec receives the original borrowed input, so its returned borrows never
/// depend on the temporary owned validation copy. Native validation is
/// authoritative even when the application's codec has permissive defaults.
pub fn decode_direct<'de, T: DirectDeserialize<'de>>(
    input: &'de [u8],
    validator: &mut dyn CompatibilityValidator,
    options: CompatOptions,
) -> Result<T> {
    let payload = prepare_input(input, options)?;
    validate_payload(&payload, validator, options.limits, ErrorSource::Decode)?;
    T::deserialize_json(input, options.limits)
        .map_err(|error| with_source(error, ErrorSource::Decode))
}

/// Encode directly into bounded storage, then independently validate its bytes.
///
/// Output preserves the codec's exact whitespace, escaping, and number spelling.
/// Nothing is published if encoding, native validation, or ownership checks fail.
pub fn encode_direct<T: DirectSerialize + ?Sized>(
    value: &T,
    validator: &mut dyn CompatibilityValidator,
    options: CompatOptions,
) -> Result<Payload> {
    let payload = encode_payload(options, |sink| value.serialize_json(sink))?;
    validate_payload(&payload, validator, options.limits, ErrorSource::Encode)?;
    Ok(payload)
}

/// Decode the caller's already validated slots without issuing a new proof.
///
/// Implementations of [`JsonDecode`] must resolve string references against the
/// slots' actual owners with checked bounds. This adapter does not rebase spans
/// or substitute a schema for the one used by the caller's validator.
pub fn decode_native<T: JsonDecode>(slots: &ValidatedSlots) -> Result<T> {
    T::decode_slots(slots)
}

/// Apply the same bounded encoding and validation boundary to a native encoder.
pub fn encode_native<T: JsonEncode + ?Sized>(
    value: &T,
    validator: &mut dyn CompatibilityValidator,
    options: CompatOptions,
) -> Result<Payload> {
    let payload = encode_payload(options, |sink| value.encode_json(sink))?;
    validate_payload(&payload, validator, options.limits, ErrorSource::Encode)?;
    Ok(payload)
}

fn with_source(mut error: Error, source: ErrorSource) -> Error {
    error.source = source;
    error
}

fn ensure_options(options: CompatOptions, source: ErrorSource) -> Result<()> {
    super::ensure_version(LEGACY_FORMAT_VERSION, options)
        .map_err(|error| with_source(error, source))
}

fn prepare_input(input: &[u8], options: CompatOptions) -> Result<Payload> {
    ensure_options(options, ErrorSource::Decode)?;
    if input.len() > options.limits.max_payload_bytes as usize {
        return Err(Error::new(
            ErrorCode::PayloadTooLarge,
            ErrorSource::Decode,
            0,
            "compatibility input byte limit exceeded",
        ));
    }
    core::str::from_utf8(input).map_err(|error| {
        Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Decode,
            // The payload ceiling above guarantees a representable offset.
            error.valid_up_to() as u32,
            "compatibility input is not valid UTF-8",
        )
    })?;

    let mut bytes = Vec::new();
    bytes.try_reserve_exact(input.len()).map_err(|_| {
        Error::new(
            ErrorCode::BudgetExhausted,
            ErrorSource::Decode,
            0,
            "compatibility input allocation failed",
        )
    })?;
    bytes.extend_from_slice(input);
    Ok(Payload::from_vec(bytes))
}

fn validate_payload(
    payload: &Payload,
    validator: &mut dyn CompatibilityValidator,
    limits: Limits,
    source: ErrorSource,
) -> Result<()> {
    // Retain the exact original owner while the validator consumes its clone.
    // A validator may return a distinct allocation with identical contents.
    let document = validator.validate_json(payload.clone(), limits)?;
    check_validation_result(
        payload.as_slice(),
        document.payload.as_slice(),
        document.proof.payload_len(),
        document.parts.decoded.len(),
        document.proof.decoded_len(),
        limits,
        source,
    )
}

// Check the untrusted return's metadata separately from proof issuance. A
// successful comparison does not create a proof or validate JSON by itself.
fn check_validation_result(
    expected: &[u8],
    owner: &[u8],
    proof_payload_len: u32,
    decoded_len: usize,
    proof_decoded_len: u32,
    limits: Limits,
    source: ErrorSource,
) -> Result<()> {
    if owner != expected
        || proof_payload_len as usize != owner.len()
        || proof_decoded_len as usize != decoded_len
    {
        return Err(Error::new(
            ErrorCode::InternalError,
            source,
            0,
            "compatibility validator returned inconsistent owners or proof lengths",
        ));
    }
    if decoded_len > limits.max_decoded_bytes as usize {
        return Err(Error::new(
            ErrorCode::DecodedArenaOverflow,
            source,
            0,
            "compatibility validator exceeded decoded arena limit",
        ));
    }
    Ok(())
}

fn encode_payload(
    options: CompatOptions,
    encode: impl FnOnce(&mut dyn ByteSink) -> Result<()>,
) -> Result<Payload> {
    ensure_options(options, ErrorSource::Encode)?;
    let maximum = options.max_output_bytes.min(options.limits.max_payload_bytes);
    let mut sink = EncodingSink {
        inner: BoundedSink::new(maximum),
        failure: None,
    };
    let result = encode(&mut sink);
    // Remember sink failures even if a faulty codec ignores or replaces them.
    // A previously emitted valid prefix must not hide an oversized write.
    if let Some(error) = sink.failure {
        return Err(error);
    }
    result.map_err(|error| with_source(error, ErrorSource::Encode))?;
    Ok(Payload::from_vec(sink.inner.into_vec()))
}

/// All output storage remains in the root's fallibly allocating bounded sink.
/// This wrapper records the first failure without allocating additional storage.
struct EncodingSink {
    inner: BoundedSink,
    failure: Option<Error>,
}

impl EncodingSink {
    fn record(&mut self, result: Result<()>) -> Result<()> {
        result.map_err(|error| {
            let error = with_source(error, ErrorSource::Encode);
            self.failure = Some(error.clone());
            error
        })
    }
}

impl ByteSink for EncodingSink {
    fn reserve(&mut self, additional: usize) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.inner.reserve(additional);
        self.record(result)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        let result = self.inner.write_bytes(bytes);
        self.record(result)
    }

    fn len(&self) -> usize {
        self.inner.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LossPolicy, ValidatedDocument};
    use core::cell::Cell;

    // Proof issuance is deliberately unavailable in this safe crate. Public
    // rejection paths use an observing validator that always denies publication;
    // metadata success and corruption cases exercise the same private checker
    // used for actual validated documents, without manufacturing a proof.
    struct RejectingValidator {
        calls: usize,
        observed: Option<Payload>,
        limits: Option<Limits>,
        error: Error,
    }

    impl RejectingValidator {
        fn new(error: Error) -> Self {
            Self { calls: 0, observed: None, limits: None, error }
        }
    }

    impl CompatibilityValidator for RejectingValidator {
        fn validate_json(&mut self, input: Payload, limits: Limits) -> Result<ValidatedDocument> {
            self.calls += 1;
            self.observed = Some(input);
            self.limits = Some(limits);
            Err(self.error.clone())
        }
    }

    fn validation_error(code: ErrorCode) -> Error {
        Error::new(code, ErrorSource::SchemaVm, 7, "validator rejected document")
            .with_schema(31, 47)
    }

    #[derive(Debug)]
    struct Null;

    impl<'de> DirectDeserialize<'de> for Null {
        fn deserialize_json(input: &'de [u8], _: Limits) -> Result<Self> {
            if input == b"null" {
                Ok(Self)
            } else {
                Err(Error::new(
                    ErrorCode::TypeMismatch, ErrorSource::Decode, 0, "expected null",
                ))
            }
        }
    }

    #[derive(Debug)]
    struct MustNotDecode;

    impl<'de> DirectDeserialize<'de> for MustNotDecode {
        fn deserialize_json(_: &'de [u8], _: Limits) -> Result<Self> {
            panic!("decoding must occur only after successful validation")
        }
    }

    struct Bytes<'a>(&'a [u8]);

    impl DirectSerialize for Bytes<'_> {
        fn serialize_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(self.0)
        }
    }

    impl JsonEncode for Bytes<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            sink.write_bytes(self.0)
        }
    }

    struct CountedEncoder<'a>(&'a Cell<usize>);

    impl DirectSerialize for CountedEncoder<'_> {
        fn serialize_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.0.set(self.0.get() + 1);
            sink.write_bytes(b"null")
        }
    }

    impl JsonEncode for CountedEncoder<'_> {
        fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
            self.serialize_json(sink)
        }
    }

    #[test]
    fn every_unknown_version_is_rejected_before_codecs_and_validation() {
        let calls = Cell::new(0);
        let encoder = CountedEncoder(&calls);
        let mut validator = RejectingValidator::new(validation_error(ErrorCode::Cancelled));
        for version in [0, 2, 255, u16::MAX] {
            for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
                let options = CompatOptions {
                    legacy_version: version, loss_policy, ..CompatOptions::default()
                };
                let error = decode_direct::<MustNotDecode>(b"null", &mut validator, options)
                    .unwrap_err();
                assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
                assert_eq!(error.source, ErrorSource::Decode);
                for error in [
                    encode_direct(&encoder, &mut validator, options).unwrap_err(),
                    encode_native(&encoder, &mut validator, options).unwrap_err(),
                ] {
                    assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
                    assert_eq!(error.source, ErrorSource::Encode);
                }
            }
        }
        assert_eq!(calls.get(), 0);
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn input_ceiling_is_inclusive_and_ignores_the_output_ceiling() {
        let mut options = CompatOptions { max_output_bytes: 0, ..CompatOptions::default() };
        options.limits.max_payload_bytes = 4;
        let expected = validation_error(ErrorCode::TypeMismatch);
        let mut validator = RejectingValidator::new(expected.clone());
        assert_eq!(decode_direct::<Null>(b"null", &mut validator, options).unwrap_err(), expected);
        assert_eq!(validator.calls, 1);
        assert_eq!(validator.observed.as_ref().unwrap().as_slice(), b"null");

        let error = decode_direct::<MustNotDecode>(b"null ", &mut validator, options).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(validator.calls, 1);
    }

    #[test]
    fn zero_input_ceiling_still_passes_empty_input_to_the_validator() {
        let mut options = CompatOptions::default();
        options.limits.max_payload_bytes = 0;
        let expected = validation_error(ErrorCode::UnexpectedEof);
        let mut validator = RejectingValidator::new(expected.clone());
        assert_eq!(decode_direct::<MustNotDecode>(b"", &mut validator, options).unwrap_err(), expected);
        assert_eq!(validator.calls, 1);
        assert!(validator.observed.as_ref().unwrap().is_empty());
        let error = decode_direct::<MustNotDecode>(b"0", &mut validator, options).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(validator.calls, 1);
    }

    #[test]
    fn invalid_utf8_is_rejected_at_its_first_invalid_byte() {
        let mut validator = RejectingValidator::new(validation_error(ErrorCode::Cancelled));
        let vectors: &[(&[u8], u32)] = &[
            (&[0xff], 0),
            (&[b'"', 0xc0, 0xaf, b'"'], 1),
            (&[b'"', 0xed, 0xa0, 0x80, b'"'], 1),
            (&[b'"', b'a', 0xe2, 0x82], 2),
        ];
        for &(input, offset) in vectors {
            let error = decode_direct::<MustNotDecode>(input, &mut validator, CompatOptions::default())
                .unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidUtf8);
            assert_eq!(error.source, ErrorSource::Decode);
            assert_eq!(error.byte_offset, offset);
        }
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn byte_budget_is_enforced_before_utf8_decoding() {
        let mut options = CompatOptions::default();
        options.limits.max_payload_bytes = 0;
        let error = prepare_input(&[0xff], options).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn validation_copy_is_owned_and_preserves_all_input_bytes() {
        let options = CompatOptions::default();
        let mut input = b" \r\n[null, true, -0.25e+2]\t".to_vec();
        let payload = prepare_input(&input, options).unwrap();
        assert_eq!(payload.as_slice(), input.as_slice());
        assert_ne!(payload.as_slice().as_ptr(), input.as_ptr());
        input.fill(b'x');
        assert_eq!(payload.as_slice(), b" \r\n[null, true, -0.25e+2]\t");
        assert_eq!(prepare_input("\"é水🦀\"".as_bytes(), options).unwrap().as_slice(),
            "\"é水🦀\"".as_bytes());
    }

    #[test]
    fn complete_input_and_exact_limits_reach_the_validator_before_the_codec() {
        let mut options = CompatOptions::default();
        options.limits.max_depth = 3;
        options.limits.max_decoded_bytes = 9;
        options.limits.max_fields_per_object = 2;
        options.limits.max_array_items = 5;
        options.limits.max_string_bytes = 7;
        options.limits.max_number_digits = 4;
        for (input, code) in [
            (&b"null true"[..], ErrorCode::TrailingContent),
            (&b"{\"a\":1,\"a\":2}"[..], ErrorCode::DuplicateObjectKey),
            (&b"[1,]"[..], ErrorCode::InvalidSyntax),
            (&b"[[[[]]]]"[..], ErrorCode::DepthLimitExceeded),
            (&b"\"\\uD800\""[..], ErrorCode::LoneSurrogate),
        ] {
            let expected = validation_error(code);
            let mut validator = RejectingValidator::new(expected.clone());
            let error = decode_direct::<MustNotDecode>(input, &mut validator, options).unwrap_err();
            assert_eq!(error, expected);
            assert_eq!(validator.calls, 1);
            assert_eq!(validator.observed.unwrap().as_slice(), input);
            assert_eq!(validator.limits, Some(options.limits));
        }
    }

    #[test]
    fn metadata_accepts_exact_copies_and_inclusive_arena_ceiling() {
        let mut limits = Limits::strict_default();
        limits.max_decoded_bytes = 3;
        let input = b"\"\\u0061\\u00e9\"";
        let copy = input.to_vec();
        for source in [ErrorSource::Decode, ErrorSource::Encode] {
            check_validation_result(input, &copy, input.len() as u32, 3, 3, limits, source).unwrap();
            check_validation_result(b"null", b"null", 4, 0, 0, limits, source).unwrap();
        }
    }

    #[test]
    fn metadata_rejects_substituted_truncated_and_extended_payloads() {
        for (expected, owner, proof_len) in [
            (&b"true"[..], &b"null"[..], 4),
            (&b"null "[..], &b"null"[..], 4),
            (&b"null"[..], &b"null "[..], 5),
            (&b"null"[..], &b"null"[..], 3),
            (&b"null"[..], &b"null"[..], 5),
            (&b"null"[..], &b"null"[..], u32::MAX),
        ] {
            for source in [ErrorSource::Decode, ErrorSource::Encode] {
                let error = check_validation_result(
                    expected, owner, proof_len, 0, 0, Limits::strict_default(), source,
                ).unwrap_err();
                assert_eq!(error.code, ErrorCode::InternalError);
                assert_eq!(error.source, source);
            }
        }
    }

    #[test]
    fn metadata_rejects_inconsistent_decoded_lengths_before_arena_limits() {
        let mut limits = Limits::strict_default();
        limits.max_decoded_bytes = 0;
        for (actual, proven) in [(0, 1), (1, 0), (2, 1), (0, u32::MAX)] {
            let error = check_validation_result(
                b"null", b"null", 4, actual, proven, limits, ErrorSource::Decode,
            ).unwrap_err();
            assert_eq!(error.code, ErrorCode::InternalError);
        }
    }

    #[test]
    fn metadata_enforces_decoded_arena_limit_in_both_directions() {
        for source in [ErrorSource::Decode, ErrorSource::Encode] {
            let mut limits = Limits::strict_default();
            limits.max_decoded_bytes = 2;
            let error = check_validation_result(
                b"\"abc\"", b"\"abc\"", 5, 3, 3, limits, source,
            ).unwrap_err();
            assert_eq!(error.code, ErrorCode::DecodedArenaOverflow);
            assert_eq!(error.source, source);
            limits.max_decoded_bytes = 0;
            check_validation_result(b"null", b"null", 4, 0, 0, limits, source).unwrap();
        }
    }

    #[test]
    fn metadata_does_not_truncate_lengths_above_the_abi_range() {
        if let Some(too_large) = (u32::MAX as usize).checked_add(1) {
            let error = check_validation_result(
                b"null", b"null", 4, too_large, 0,
                Limits::permissive_default(), ErrorSource::Decode,
            ).unwrap_err();
            assert_eq!(error.code, ErrorCode::InternalError);
        }
    }

    #[test]
    fn encoding_preserves_spelling_whitespace_and_unicode_without_a_tree() {
        for bytes in [
            &b" \n{\"n\":-0.00e+02,\"s\":\"\\u0061\"}\t"[..],
            &b"[null,true,false,18446744073709551615,-9223372036854775808]"[..],
            "\"é水🦀\"".as_bytes(),
        ] {
            let payload = encode_payload(CompatOptions::default(), |sink| {
                sink.reserve(bytes.len())?;
                assert_eq!(sink.len(), 0);
                for &byte in bytes {
                    sink.write_byte(byte)?;
                }
                assert_eq!(sink.len(), bytes.len());
                Ok(())
            }).unwrap();
            assert_eq!(payload.as_slice(), bytes);
        }
    }

    #[test]
    fn encoding_ceiling_is_the_minimum_of_output_and_payload_limits() {
        for (output, input) in [(4, 100), (100, 4), (4, 4), (u32::MAX, 4)] {
            let mut options = CompatOptions { max_output_bytes: output, ..CompatOptions::default() };
            options.limits.max_payload_bytes = input;
            assert_eq!(encode_payload(options, |sink| sink.write_bytes(b"null")).unwrap().as_slice(), b"null");
            let error = encode_payload(options, |sink| sink.write_bytes(b"null ")).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            assert_eq!(error.source, ErrorSource::Encode);
        }
    }

    #[test]
    fn output_overflow_never_calls_validation() {
        let mut validator = RejectingValidator::new(validation_error(ErrorCode::Cancelled));
        for (output, input) in [(3, 100), (100, 3), (0, 100), (100, 0)] {
            let mut options = CompatOptions { max_output_bytes: output, ..CompatOptions::default() };
            options.limits.max_payload_bytes = input;
            let value = Bytes(b"null");
            for error in [
                encode_direct(&value, &mut validator, options).unwrap_err(),
                encode_native(&value, &mut validator, options).unwrap_err(),
            ] {
                assert_eq!(error.code, ErrorCode::PayloadTooLarge);
                assert_eq!(error.source, ErrorSource::Encode);
            }
        }
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn all_encoder_bytes_reach_independent_validation_even_when_invalid() {
        for (bytes, code) in [
            (&b" \nnull\t"[..], ErrorCode::ConstraintViolation),
            (&b"null true"[..], ErrorCode::TrailingContent),
            (&b"{"[..], ErrorCode::UnexpectedEof),
            (&b""[..], ErrorCode::UnexpectedEof),
            (&b"NaN"[..], ErrorCode::InvalidToken),
            (&b"\"\xff\""[..], ErrorCode::InvalidUtf8),
        ] {
            let options = CompatOptions::default();
            let expected = validation_error(code);
            let value = Bytes(bytes);
            let direct: &dyn DirectSerialize = &value;
            let native: &dyn JsonEncode = &value;
            let mut validator = RejectingValidator::new(expected.clone());
            assert_eq!(encode_direct(direct, &mut validator, options).unwrap_err(), expected);
            assert_eq!(validator.observed.as_ref().unwrap().as_slice(), bytes);
            assert_eq!(encode_native(native, &mut validator, options).unwrap_err(), expected);
            assert_eq!(validator.observed.as_ref().unwrap().as_slice(), bytes);
            assert_eq!(validator.calls, 2);
            assert_eq!(validator.limits, Some(options.limits));
        }
    }

    #[test]
    fn codec_errors_keep_precise_context_and_use_encode_source() {
        let original = Error::new(
            ErrorCode::NumberOverflow, ErrorSource::Decode, 13, "codec number overflow",
        ).with_schema(17, 23);
        let error = encode_payload(CompatOptions::default(), |sink| {
            sink.write_bytes(b"[")?;
            Err(original.clone())
        }).unwrap_err();
        assert_eq!(error, Error { source: ErrorSource::Encode, ..original });
    }

    #[test]
    fn direct_and_native_codec_failures_never_reach_validation() {
        struct Failing;
        impl DirectSerialize for Failing {
            fn serialize_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
                sink.write_bytes(b"null")?;
                Err(Error::new(
                    ErrorCode::TypeMismatch, ErrorSource::Decode, 2, "codec rejected value",
                ).with_schema(5, 8))
            }
        }
        impl JsonEncode for Failing {
            fn encode_json(&self, sink: &mut dyn ByteSink) -> Result<()> {
                self.serialize_json(sink)
            }
        }
        let mut validator = RejectingValidator::new(validation_error(ErrorCode::Cancelled));
        for error in [
            encode_direct(&Failing, &mut validator, CompatOptions::default()).unwrap_err(),
            encode_native(&Failing, &mut validator, CompatOptions::default()).unwrap_err(),
        ] {
            assert_eq!(error.code, ErrorCode::TypeMismatch);
            assert_eq!(error.source, ErrorSource::Encode);
            assert_eq!((error.byte_offset, error.schema_id, error.field_id), (2, 5, 8));
        }
        assert_eq!(validator.calls, 0);
    }

    #[test]
    fn ignored_sink_failures_cannot_publish_an_existing_valid_prefix() {
        let options = CompatOptions { max_output_bytes: 4, ..CompatOptions::default() };
        let error = encode_payload(options, |sink| {
            sink.write_bytes(b"null")?;
            let first = sink.write_byte(b' ').unwrap_err();
            assert_eq!(sink.len(), 4);
            assert_eq!(sink.reserve(0).unwrap_err(), first);
            assert_eq!(sink.write_bytes(b"").unwrap_err(), first);
            Ok(())
        }).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(error.source, ErrorSource::Encode);
    }

    #[test]
    fn failed_reservation_cannot_be_hidden_or_replaced_by_the_codec() {
        let options = CompatOptions { max_output_bytes: 4, ..CompatOptions::default() };
        let error = encode_payload(options, |sink| {
            sink.write_bytes(b"null")?;
            let error = sink.reserve(usize::MAX).unwrap_err();
            assert_eq!(error.code, ErrorCode::PayloadTooLarge);
            assert_eq!(sink.len(), 4);
            Err(Error::new(ErrorCode::TypeMismatch, ErrorSource::Encode, 0, "replacement"))
        }).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
    }

    #[test]
    fn zero_output_budget_allows_no_bytes_and_still_requires_validation() {
        let options = CompatOptions { max_output_bytes: 0, ..CompatOptions::default() };
        let mut validator = RejectingValidator::new(validation_error(ErrorCode::UnexpectedEof));
        let error = encode_direct(&Bytes(b""), &mut validator, options).unwrap_err();
        assert_eq!(error, validation_error(ErrorCode::UnexpectedEof));
        assert_eq!(validator.calls, 1);
        let error = encode_native(&Bytes(b"0"), &mut validator, options).unwrap_err();
        assert_eq!(error.code, ErrorCode::PayloadTooLarge);
        assert_eq!(validator.calls, 1);
    }

    #[test]
    fn loss_policy_cannot_suppress_native_validation_errors() {
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let options = CompatOptions { loss_policy, ..CompatOptions::default() };
            let expected = validation_error(ErrorCode::DuplicateObjectKey);
            let mut validator = RejectingValidator::new(expected.clone());
            let bytes = b"{\"a\":1,\"a\":2}";
            assert_eq!(decode_direct::<MustNotDecode>(bytes, &mut validator, options).unwrap_err(), expected);
            assert_eq!(encode_direct(&Bytes(bytes), &mut validator, options).unwrap_err(), expected);
            assert_eq!(encode_native(&Bytes(bytes), &mut validator, options).unwrap_err(), expected);
            assert_eq!(validator.calls, 3);
        }
    }
}
