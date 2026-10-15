//! Validated, immutable ownership at the outbound WebSocket frame boundary.

use super::*;

/// Validates one complete JSON document before publishing an owned data frame.
///
/// The validator receives a shared clone of the immutable payload and the exact
/// configured syntax limits. Its implementation must validate the whole document,
/// including unselected data and strict EOF, under its configured schema. The
/// original owner stays alive while the returned buffers and proof are checked.
/// Only that original owner, the opcode, and the unchanged proof are retained;
/// decoded storage and selected slots are dropped before returning.
///
/// This module allocates no payload copies and never issues a validation proof.
/// Validator errors, including schema context and offsets, propagate unchanged.
pub fn prepare_output(
    payload: Payload,
    opcode: Opcode,
    validator: &mut dyn MessageValidator,
    policy: &WssPolicy,
) -> Result<ValidatedOutput> {
    validate_frame(payload.as_slice(), opcode, policy)?;
    let validated = validator.validate(payload.clone(), policy.limits)?;
    ensure_validated_slots(&validated, payload.as_slice())?;
    ensure_payload_len(validated.decoded.len(), policy.limits.max_decoded_bytes)?;
    ensure_utf8(&validated.decoded)?;
    Ok(ValidatedOutput { payload, opcode, proof: validated.proof })
}

/// Rechecks an existing validated frame against the current transport policy.
///
/// This checks its data opcode, all byte ceilings, UTF-8, and proof lengths. The
/// decoded length remains bounded even though decoded storage is not retained in
/// an outbound frame. The existing schema fingerprint is never replaced. These
/// boundary checks do not rerun JSON or schema validation; a caller needing new
/// schema validation must use [`prepare_output`] with its configured validator.
pub fn validate_output(output: &ValidatedOutput, policy: &WssPolicy) -> Result<()> {
    validate_frame(output.payload.as_slice(), output.opcode, policy)?;
    ensure_proof_lengths(
        output.payload.len(),
        output.proof.payload_len(),
        output.proof.decoded_len(),
        policy.limits.max_decoded_bytes,
    )
}

/// Unlike reassembled input, an output is handed to the transport as one atomic
/// frame, so the frame ceiling applies in addition to both message ceilings.
fn validate_frame(bytes: &[u8], opcode: Opcode, policy: &WssPolicy) -> Result<()> {
    policy::validate_policy(policy)?;
    match opcode {
        Opcode::Text => {}
        Opcode::Binary if policy.binary == BinaryPolicy::Json => {}
        Opcode::Binary => {
            return Err(transport_error(ErrorCode::TypeMismatch, "binary JSON is disabled"));
        }
        _ => {
            return Err(transport_error(
                ErrorCode::FramingError,
                "validated JSON output requires a data opcode",
            ));
        }
    }
    ensure_payload_len(bytes.len(), policy.max_frame_bytes)?;
    ensure_payload_len(bytes.len(), policy.max_message_bytes)?;
    ensure_payload_len(bytes.len(), policy.limits.max_payload_bytes)?;
    ensure_utf8(bytes)
}

/// Checks existing evidence only. Metadata cannot establish JSON validity.
fn ensure_proof_lengths(
    payload_len: usize,
    proof_payload_len: u32,
    proof_decoded_len: u32,
    max_decoded_bytes: u32,
) -> Result<()> {
    if usize::try_from(proof_payload_len).ok() != Some(payload_len) {
        return Err(transport_error(
            ErrorCode::InternalError,
            "validated output payload and proof lengths differ",
        ));
    }
    if proof_decoded_len > max_decoded_bytes {
        return Err(transport_error(
            ErrorCode::PayloadTooLarge,
            "validated output decoded byte limit exceeded",
        ));
    }
    Ok(())
}

fn ensure_utf8(bytes: &[u8]) -> Result<()> {
    if let Err(error) = core::str::from_utf8(bytes) {
        let offset = u32::try_from(error.valid_up_to()).map_err(|_| {
            transport_error(ErrorCode::PayloadTooLarge, "UTF-8 offset exceeds ABI length")
        })?;
        return Err(Error::new(
            ErrorCode::InvalidUtf8,
            ErrorSource::Utf8,
            offset,
            "WebSocket JSON output is not valid UTF-8",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_error<T: core::fmt::Debug>(
        result: Result<T>,
        code: ErrorCode,
        source: ErrorSource,
        offset: u32,
    ) {
        let error = result.expect_err("the output boundary must reject this value");
        assert_eq!(error.code, code);
        assert_eq!(error.source, source);
        assert_eq!(error.byte_offset, offset);
        assert_eq!((error.schema_id, error.field_id), (0, 0));
        assert!(!error.detail.is_empty());
    }

    // The frozen ABI has no safe proof constructor, and EventInput requires an
    // unsafe implementation. The crate forbids unsafe code, including in tests.
    // This recording validator tests the real error/ownership seam without
    // inventing evidence. Proof metadata guards are exercised directly below.
    struct RejectingValidator {
        error: Error,
        calls: usize,
        received: Option<Payload>,
        limits: Option<Limits>,
    }

    impl RejectingValidator {
        fn new(error: Error) -> Self {
            Self { error, calls: 0, received: None, limits: None }
        }
    }

    impl MessageValidator for RejectingValidator {
        fn validate(&mut self, payload: Payload, limits: Limits) -> Result<ValidatedSlots> {
            self.calls += 1;
            self.received = Some(payload);
            self.limits = Some(limits);
            Err(self.error.clone())
        }
    }

    fn validator() -> RejectingValidator {
        RejectingValidator::new(Error::new(
            ErrorCode::InvalidSyntax,
            ErrorSource::Syntax,
            0,
            "recorded validator failure",
        ))
    }

    fn assert_reaches_validator(bytes: &[u8], opcode: Opcode, policy: &WssPolicy) {
        let mut validator = validator();
        let payload = Payload::from_slice(bytes);
        let pointer = payload.as_slice().as_ptr();
        let error = prepare_output(payload, opcode, &mut validator, policy).unwrap_err();
        assert_eq!(error, validator.error);
        assert_eq!(validator.calls, 1);
        assert_eq!(validator.limits, Some(policy.limits));
        let received = validator.received.unwrap();
        assert_eq!(received.as_slice(), bytes);
        assert_eq!(received.as_slice().as_ptr(), pointer);
    }

    #[test]
    fn non_data_opcodes_never_reach_validation() {
        for binary in [BinaryPolicy::Reject, BinaryPolicy::Json] {
            let policy = WssPolicy { binary, ..WssPolicy::default() };
            for opcode in [Opcode::Continuation, Opcode::Close, Opcode::Ping, Opcode::Pong] {
                let mut validator = validator();
                assert_error(
                    prepare_output(Payload::from_slice(b"null"), opcode, &mut validator, &policy),
                    ErrorCode::FramingError,
                    ErrorSource::Transport,
                    0,
                );
                assert_eq!(validator.calls, 0);
                assert!(validator.received.is_none());
            }
        }
    }

    #[test]
    fn binary_requires_explicit_json_policy_and_still_runs_validation() {
        let mut validator = validator();
        assert_error(
            prepare_output(
                Payload::from_slice(b"null"), Opcode::Binary,
                &mut validator, &WssPolicy::default(),
            ),
            ErrorCode::TypeMismatch,
            ErrorSource::Transport,
            0,
        );
        assert_eq!(validator.calls, 0);
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        assert_reaches_validator(b"null", Opcode::Binary, &policy);
        assert_reaches_validator(b"null", Opcode::Text, &policy);
    }

    #[test]
    fn each_byte_ceiling_is_independent_and_inclusive() {
        for tighter in 0..3 {
            let mut policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
            policy.max_frame_bytes = 8;
            policy.max_message_bytes = 8;
            policy.limits.max_payload_bytes = 8;
            match tighter {
                0 => policy.max_frame_bytes = 4,
                1 => policy.max_message_bytes = 4,
                _ => policy.limits.max_payload_bytes = 4,
            }
            for opcode in [Opcode::Text, Opcode::Binary] {
                for bytes in [b"123".as_slice(), b"null".as_slice()] {
                    assert_reaches_validator(bytes, opcode, &policy);
                }
                let mut validator = validator();
                assert_error(
                    prepare_output(
                        Payload::from_slice(b"false"), opcode, &mut validator, &policy,
                    ),
                    ErrorCode::PayloadTooLarge,
                    ErrorSource::Transport,
                    0,
                );
                assert_eq!(validator.calls, 0);
            }
        }
    }

    #[test]
    fn exact_simultaneous_byte_ceilings_allow_validation() {
        let mut policy = WssPolicy::default();
        policy.max_frame_bytes = 4;
        policy.max_message_bytes = 4;
        policy.limits.max_payload_bytes = 4;
        assert_reaches_validator(b"null", Opcode::Text, &policy);
    }

    #[test]
    fn output_must_fit_one_frame_even_when_the_message_budget_is_larger() {
        let policy = WssPolicy { max_frame_bytes: 1, ..WssPolicy::default() };
        assert_error(
            validate_frame(b"null", Opcode::Text, &policy),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
        assert_reaches_validator(b"0", Opcode::Text, &policy);
    }

    #[test]
    fn frame_limit_may_exceed_message_limit() {
        let policy = WssPolicy {
            max_frame_bytes: 8, max_message_bytes: 4, ..WssPolicy::default()
        };
        assert_reaches_validator(b"null", Opcode::Text, &policy);
        assert_error(
            validate_frame(b"false", Opcode::Text, &policy),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn zero_required_policy_limits_fail_before_validation() {
        for zero in 0..5 {
            let mut policy = WssPolicy::default();
            match zero {
                0 => policy.max_frame_bytes = 0,
                1 => policy.max_message_bytes = 0,
                2 => policy.max_fragments = 0,
                3 => policy.limits.max_payload_bytes = 0,
                _ => policy.limits.max_depth = 0,
            }
            let mut validator = validator();
            assert_error(
                prepare_output(
                    Payload::from_slice(b"null"), Opcode::Text, &mut validator, &policy,
                ),
                ErrorCode::ConstraintViolation,
                ErrorSource::Transport,
                0,
            );
            assert_eq!(validator.calls, 0);
            // The same preflight is used to recheck previously validated output.
            assert_error(
                validate_frame(b"null", Opcode::Text, &policy),
                ErrorCode::ConstraintViolation,
                ErrorSource::Transport,
                0,
            );
        }
    }

    #[test]
    fn every_configured_syntax_limit_is_forwarded_exactly() {
        let mut policy = WssPolicy::default();
        policy.limits = Limits {
            max_payload_bytes: 7,
            max_decoded_bytes: 0,
            max_depth: 1,
            max_fields_per_object: 0,
            max_string_bytes: 0,
            max_number_digits: 0,
            max_array_items: 0,
            reject_duplicate_keys: false,
            reject_unknown_keys: false,
        };
        assert_reaches_validator(b"null", Opcode::Text, &policy);
        policy.limits.reject_duplicate_keys = true;
        policy.limits.reject_unknown_keys = true;
        assert_reaches_validator(b"null", Opcode::Text, &policy);
    }

    #[test]
    fn valid_utf8_does_not_skip_json_or_schema_validation() {
        let vectors: &[&[u8]] = &[
            b"", b" ", b"not json", b"{", b"null true", b"\"\\uD800\"",
            b"\"\\q\"", b"[1,]", b"01", b"\0", b"{\"a\":1,\"a\":2}",
        ];
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for &bytes in vectors {
            for opcode in [Opcode::Text, Opcode::Binary] {
                assert_reaches_validator(bytes, opcode, &policy);
            }
        }
    }

    #[test]
    fn unicode_vectors_reach_the_configured_validator_for_both_data_opcodes() {
        let vectors = ["\"ASCII\"", "\"é\"", "\"水\"", "\"😀\"", "\"\u{10ffff}\"", "\u{feff}null"];
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for text in vectors {
            for opcode in [Opcode::Text, Opcode::Binary] {
                assert_reaches_validator(text.as_bytes(), opcode, &policy);
            }
        }
    }

    #[test]
    fn invalid_utf8_reports_the_first_invalid_sequence_without_calling_validator() {
        let vectors: &[(&[u8], u32)] = &[
            (b"\xff", 0),
            (b"\"\x80\"", 1),
            (b"ok\xc0\xaf", 2),
            (b"ok\xe2\x28\xa1", 2),
            (b"\"\xed\xa0\x80\"", 1),
            (b"\"\xf4\x90\x80\x80\"", 1),
            (b"null \xf0\x9f", 5),
            (b"\xc3\xa9\xff", 2),
        ];
        let policy = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        for &(bytes, offset) in vectors {
            for opcode in [Opcode::Text, Opcode::Binary] {
                let mut validator = validator();
                assert_error(
                    prepare_output(Payload::from_slice(bytes), opcode, &mut validator, &policy),
                    ErrorCode::InvalidUtf8,
                    ErrorSource::Utf8,
                    offset,
                );
                assert_eq!(validator.calls, 0);
            }
            // This guard also checks the returned decoded arena before publication.
            assert_error(ensure_utf8(bytes), ErrorCode::InvalidUtf8, ErrorSource::Utf8, offset);
        }
    }

    #[test]
    fn decoded_utf8_allows_empty_nul_and_unicode_scalars() {
        for text in ["", "\0", "ASCII", "é", "水", "😀", "\u{10ffff}"] {
            ensure_utf8(text.as_bytes()).unwrap();
        }
    }

    #[test]
    fn policy_opcode_and_byte_errors_precede_utf8_work() {
        let mut policy = WssPolicy { max_frame_bytes: 1, ..WssPolicy::default() };
        assert_error(
            validate_frame(b"\xff\xff", Opcode::Text, &policy),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
        assert_error(
            validate_frame(b"\xff", Opcode::Binary, &policy),
            ErrorCode::TypeMismatch,
            ErrorSource::Transport,
            0,
        );
        assert_error(
            validate_frame(b"\xff", Opcode::Ping, &policy),
            ErrorCode::FramingError,
            ErrorSource::Transport,
            0,
        );
        policy.max_fragments = 0;
        assert_error(
            validate_frame(b"\xff", Opcode::Ping, &policy),
            ErrorCode::ConstraintViolation,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn validator_errors_preserve_all_diagnostic_metadata() {
        let vectors = [
            (b"null true".as_slice(), ErrorCode::TrailingContent, ErrorSource::Syntax, 5),
            (b"{".as_slice(), ErrorCode::UnexpectedEof, ErrorSource::Syntax, 1),
            (b"[1,]".as_slice(), ErrorCode::InvalidSyntax, ErrorSource::Syntax, 3),
            (b"\"\\uD800\"".as_slice(), ErrorCode::LoneSurrogate, ErrorSource::Decode, 1),
            (b"null".as_slice(), ErrorCode::DepthLimitExceeded, ErrorSource::Cursor, 0),
            (b"null".as_slice(), ErrorCode::InvalidUtf8, ErrorSource::Utf8, 2),
            (b"null".as_slice(), ErrorCode::MissingRequiredField, ErrorSource::SchemaVm, 4),
            (b"null".as_slice(), ErrorCode::DuplicateObjectKey, ErrorSource::SchemaVm, 3),
            (b"null".as_slice(), ErrorCode::UnknownObjectKey, ErrorSource::SchemaVm, 1),
            (b"null".as_slice(), ErrorCode::TypeMismatch, ErrorSource::SchemaVm, 0),
            (b"null".as_slice(), ErrorCode::ConstraintViolation, ErrorSource::SchemaVm, 0),
            (b"null".as_slice(), ErrorCode::PatternMismatch, ErrorSource::SchemaVm, 0),
            (b"null".as_slice(), ErrorCode::DecodedArenaOverflow, ErrorSource::Decode, 1),
            (b"null".as_slice(), ErrorCode::BudgetExhausted, ErrorSource::SchemaVm, 2),
            (b"null".as_slice(), ErrorCode::Cancelled, ErrorSource::Transport, 0),
            (b"null".as_slice(), ErrorCode::InternalError, ErrorSource::SchemaVm, 3),
        ];
        for (bytes, code, source, offset) in vectors {
            let expected = Error::new(code, source, offset, "configured validation failure")
                .with_schema(91, 7);
            let mut validator = RejectingValidator::new(expected.clone());
            let error = prepare_output(
                Payload::from_slice(bytes), Opcode::Text, &mut validator, &WssPolicy::default(),
            ).unwrap_err();
            assert_eq!(error, expected);
            assert_eq!(validator.calls, 1);
        }
    }

    #[test]
    fn validator_receives_shared_clone_and_exact_limits_once() {
        let policy = WssPolicy::default();
        for payload in [Payload::from_slice(b"null"), Payload::from_vec(b"null".to_vec())] {
            let shared = matches!(&payload, Payload::Shared(_));
            let original = payload.clone();
            let pointer = original.as_slice().as_ptr();
            let mut validator = validator();
            assert_eq!(
                prepare_output(payload, Opcode::Text, &mut validator, &policy).unwrap_err(),
                validator.error,
            );
            assert_eq!(validator.calls, 1);
            assert_eq!(validator.limits, Some(policy.limits));
            drop(original);
            let retained = validator.received.unwrap();
            assert_eq!(retained.as_slice(), b"null");
            assert_eq!(retained.as_slice().as_ptr(), pointer);
            assert_eq!(matches!(&retained, Payload::Shared(_)), shared);
        }
    }

    #[test]
    fn sliced_bytes_keep_their_owner_across_application_buffer_drops() {
        let backing = Bytes::from(b"prefix{\"v\":1}suffix".to_vec());
        let bytes = backing.slice(6..13);
        let pointer = bytes.as_ptr();
        let mut validator = validator();
        let error = prepare_output(
            Payload::Bytes(bytes.clone()), Opcode::Text,
            &mut validator, &WssPolicy::default(),
        ).unwrap_err();
        assert_eq!(error, validator.error);
        drop(bytes);
        drop(backing);
        let retained = validator.received.unwrap();
        assert!(matches!(&retained, Payload::Bytes(_)));
        assert_eq!(retained.as_slice(), b"{\"v\":1}");
        assert_eq!(retained.as_slice().as_ptr(), pointer);
    }

    #[test]
    fn proof_payload_lengths_must_match_exactly_including_empty() {
        for (payload, proof) in [(0, 0), (1, 1), (4, 4), (u32::MAX as usize, u32::MAX)] {
            ensure_proof_lengths(payload, proof, 0, 0).unwrap();
        }
        for (payload, proof) in [(0, 1), (1, 0), (3, 4), (5, 4)] {
            assert_error(
                ensure_proof_lengths(payload, proof, 0, 0),
                ErrorCode::InternalError,
                ErrorSource::Transport,
                0,
            );
        }
    }

    #[test]
    fn decoded_proof_ceiling_is_inclusive_even_without_retained_arena() {
        for (decoded, maximum) in [(0, 0), (0, 3), (2, 3), (3, 3), (u32::MAX, u32::MAX)] {
            ensure_proof_lengths(4, 4, decoded, maximum).unwrap();
        }
        for (decoded, maximum) in [(1, 0), (4, 3), (u32::MAX, u32::MAX - 1)] {
            assert_error(
                ensure_proof_lengths(4, 4, decoded, maximum),
                ErrorCode::PayloadTooLarge,
                ErrorSource::Transport,
                0,
            );
        }
        // Corrupt payload evidence takes precedence over a tighter decoded cap.
        assert_error(
            ensure_proof_lengths(4, 3, 1, 0),
            ErrorCode::InternalError,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn decoded_storage_length_guard_handles_zero_exact_and_excess_lengths() {
        for (length, maximum) in [(0, 0), (2, 3), (3, 3), (u32::MAX as usize, u32::MAX)] {
            assert_eq!(ensure_payload_len(length, maximum).unwrap() as usize, length);
        }
        for (length, maximum) in [(1, 0), (4, 3)] {
            assert_error(
                ensure_payload_len(length, maximum),
                ErrorCode::PayloadTooLarge,
                ErrorSource::Transport,
                0,
            );
        }
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn abi_overflow_is_rejected_without_allocating_large_buffers() {
        let length = u32::MAX as usize + 1;
        assert_error(
            ensure_payload_len(length, u32::MAX),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
        assert_error(
            ensure_proof_lengths(length, u32::MAX, 0, 0),
            ErrorCode::InternalError,
            ErrorSource::Transport,
            0,
        );
    }

    #[test]
    fn tightened_current_policy_rejects_the_same_previously_acceptable_frame() {
        let original = WssPolicy { binary: BinaryPolicy::Json, ..WssPolicy::default() };
        validate_frame(b"null", Opcode::Binary, &original).unwrap();
        for tighter in 0..4 {
            let mut policy = original;
            match tighter {
                0 => policy.max_frame_bytes = 3,
                1 => policy.max_message_bytes = 3,
                2 => policy.limits.max_payload_bytes = 3,
                _ => policy.binary = BinaryPolicy::Reject,
            }
            let code = if tighter == 3 { ErrorCode::TypeMismatch } else { ErrorCode::PayloadTooLarge };
            assert_error(
                validate_frame(b"null", Opcode::Binary, &policy),
                code,
                ErrorSource::Transport,
                0,
            );
        }
        ensure_proof_lengths(4, 4, 3, original.limits.max_decoded_bytes).unwrap();
        let mut policy = original;
        policy.limits.max_decoded_bytes = 2;
        assert_error(
            ensure_proof_lengths(4, 4, 3, policy.limits.max_decoded_bytes),
            ErrorCode::PayloadTooLarge,
            ErrorSource::Transport,
            0,
        );
    }
}
