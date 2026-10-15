//! Versioned Spot policies, stable schema identities, and depth sequencing.
//!
//! Decimal text is converted by `jmax_decimal` in the kind-specific decoders.
//! This boundary validates configuration and already converted coefficients;
//! it never changes a decimal's scale or rounds a book level.

use super::*;

/// The frozen version-one Spot configuration.
pub fn spot_profile() -> ExchangeProfile {
    ExchangeProfile::default()
}

/// Reject unsupported versions and malformed policy before inspecting input.
///
/// Other zero syntax budgets remain valid: they prohibit the corresponding
/// resource rather than removing its ceiling. Limits need not accommodate a
/// particular message for the profile itself to be valid.
pub fn validate_profile(profile: &ExchangeProfile) -> Result<()> {
    if profile.version != SPOT_PROFILE_VERSION {
        return Err(exchange_error(ErrorCode::UnsupportedSchemaFeature,
            "unsupported exchange profile version"));
    }
    if profile.max_levels == 0 {
        return Err(exchange_error(ErrorCode::ConstraintViolation,
            "exchange level limit must be nonzero"));
    }
    if profile.max_symbol_bytes == 0 {
        return Err(exchange_error(ErrorCode::ConstraintViolation,
            "exchange symbol byte limit must be nonzero"));
    }
    if profile.limits.max_payload_bytes == 0 {
        return Err(exchange_error(ErrorCode::ConstraintViolation,
            "exchange payload byte limit must be nonzero"));
    }
    if profile.limits.max_depth == 0 {
        return Err(exchange_error(ErrorCode::ConstraintViolation,
            "exchange depth limit must be nonzero"));
    }
    for policy in [&profile.price, &profile.quantity] {
        // The general decimal crate supports 39 digits and scales above 38.
        // Version-one exchange policies deliberately restrict both to 38.
        if policy.max_precision == 0 || policy.max_precision > 38 {
            return Err(exchange_error(ErrorCode::ConstraintViolation,
                "exchange decimal precision must be in 1..=38"));
        }
        if policy.target_scale > 38 {
            return Err(exchange_error(ErrorCode::ConstraintViolation,
                "exchange decimal target scale must not exceed 38"));
        }
        if policy.tick_step < 0 {
            return Err(exchange_error(ErrorCode::ConstraintViolation,
                "exchange decimal tick step must be nonnegative"));
        }
    }
    Ok(())
}

/// FNV-1a-64 over the frozen, explicitly little-endian profile encoding.
///
/// This is a reproducible schema identity, not authentication. Native layout,
/// padding, enum discriminants and platform endianness never enter the hash.
/// Every accepted version fixes its slot layouts and semantic constraints.
pub fn schema_fingerprint(kind: MessageKind, profile: &ExchangeProfile) -> Result<u64> {
    validate_profile(profile)?;
    let mut hash = 14_695_981_039_346_656_037_u64;
    fingerprint_bytes(&mut hash, b"jmax-exchange-v1");
    fingerprint_bytes(&mut hash, &[match kind {
        MessageKind::Depth => 0,
        MessageKind::AggTrade => 1,
        MessageKind::Snapshot => 2,
    }]);
    fingerprint_bytes(&mut hash, &profile.version.to_le_bytes());
    for policy in [&profile.price, &profile.quantity] {
        fingerprint_bytes(&mut hash, &[
            policy.max_precision,
            policy.target_scale,
            u8::from(policy.allow_exponent),
            u8::from(policy.reject_inexact_rescale),
        ]);
        fingerprint_bytes(&mut hash, &policy.tick_step.to_le_bytes());
    }
    fingerprint_bytes(&mut hash, &profile.max_levels.to_le_bytes());
    fingerprint_bytes(&mut hash, &profile.max_symbol_bytes.to_le_bytes());
    fingerprint_bytes(&mut hash, &[match profile.wrapper {
        WrapperPolicy::Bare => 0,
        WrapperPolicy::CombinedStream => 1,
        WrapperPolicy::Either => 2,
    }]);
    let limits = &profile.limits;
    // ABI declaration order is part of the wire identity, including the two
    // u16 fields and both validation flags. Do not hash memory representations.
    fingerprint_bytes(&mut hash, &limits.max_payload_bytes.to_le_bytes());
    fingerprint_bytes(&mut hash, &limits.max_decoded_bytes.to_le_bytes());
    fingerprint_bytes(&mut hash, &limits.max_depth.to_le_bytes());
    fingerprint_bytes(&mut hash, &limits.max_fields_per_object.to_le_bytes());
    fingerprint_bytes(&mut hash, &limits.max_string_bytes.to_le_bytes());
    fingerprint_bytes(&mut hash, &limits.max_number_digits.to_le_bytes());
    fingerprint_bytes(&mut hash, &limits.max_array_items.to_le_bytes());
    fingerprint_bytes(&mut hash, &[
        u8::from(limits.reject_duplicate_keys),
        u8::from(limits.reject_unknown_keys),
    ]);
    Ok(hash)
}

fn fingerprint_bytes(hash: &mut u64, bytes: &[u8]) {
    for &byte in bytes {
        *hash = (*hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211);
    }
}

/// Validate a converted level without rescaling it or dropping zero quantity.
///
/// Prices must be positive and quantities nonnegative. Precision, target scale
/// and tick divisibility are enforced independently for each policy. Without
/// a payload coordinate or kind, errors have offset and schema/field IDs zero;
/// the decoding caller can attach its known schema and field context.
pub fn validate_level(level: BookLevel, profile: &ExchangeProfile) -> Result<()> {
    validate_profile(profile)?;
    jmax_decimal::bounds::validate(
        level.price,
        &profile.price,
        jmax_decimal::DecimalBounds {
            min: Some(Decimal128::new(1, profile.price.target_scale)),
            max: None,
        },
        0,
    )?;
    jmax_decimal::bounds::validate(
        level.quantity,
        &profile.quantity,
        jmax_decimal::DecimalBounds {
            min: Some(Decimal128::new(0, profile.quantity.target_scale)),
            max: None,
        },
        0,
    )
}

/// Establish or replace synchronization from an authoritative snapshot ID.
///
/// The maximum u64 cannot be seeded because no successor can be represented.
/// A rejected seed leaves even a previously synchronized state unchanged.
pub fn seed_snapshot(state: &mut SequenceState, last_update_id: u64) -> Result<()> {
    last_update_id.checked_add(1).ok_or_else(|| exchange_error(
        ErrorCode::NumberOverflow, "snapshot update ID has no u64 successor"))?;
    *state = SequenceState { last_update_id: Some(last_update_id), synchronized: true };
    Ok(())
}

/// Classify an inclusive update range without changing synchronization state.
///
/// Overlaps are accepted when the range covers the next expected update. An
/// applicable range must have a representable successor, even at the u64 edge.
pub fn classify_depth(
    state: &SequenceState,
    first_update_id: u64,
    final_update_id: u64,
) -> Result<SequenceDecision> {
    let last = match (state.synchronized, state.last_update_id) {
        (true, Some(last)) => last,
        _ => return Err(exchange_error(ErrorCode::ConstraintViolation,
            "depth sequence requires a synchronized snapshot")),
    };
    if first_update_id > final_update_id {
        return Err(exchange_error(ErrorCode::ConstraintViolation,
            "depth first update ID exceeds final update ID"));
    }
    if final_update_id <= last {
        return Ok(SequenceDecision::Stale);
    }
    let expected = last.checked_add(1).ok_or_else(|| exchange_error(
        ErrorCode::NumberOverflow, "depth sequence has no u64 successor"))?;
    if first_update_id > expected {
        return Ok(SequenceDecision::Gap { expected, received: first_update_id });
    }
    let next_update_id = final_update_id.checked_add(1).ok_or_else(|| exchange_error(
        ErrorCode::NumberOverflow, "depth final update ID has no u64 successor"))?;
    Ok(SequenceDecision::Apply { next_update_id })
}

/// Apply a classification atomically to the sequence tracker.
///
/// Stale input preserves every field. A gap preserves the last accepted ID and
/// invalidates synchronization until a new snapshot is seeded. Any error leaves
/// the whole state unchanged; this function does not update order-book storage.
pub fn apply_depth(
    state: &mut SequenceState,
    first_update_id: u64,
    final_update_id: u64,
) -> Result<SequenceDecision> {
    let decision = classify_depth(state, first_update_id, final_update_id)?;
    match decision {
        SequenceDecision::Apply { .. } => state.last_update_id = Some(final_update_id),
        SequenceDecision::Gap { .. } => state.synchronized = false,
        SequenceDecision::Stale => {}
    }
    Ok(decision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use jmax_abi::{buffer::ValidationProof, syntax::{Event, ScanStep}};
    use jmax_cursor::{walk, SkipMode};

    fn assert_error(error: Error, code: ErrorCode) {
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, 0);
        assert!(!error.detail.is_empty());
    }

    fn level(price: i128, quantity: i128, scale: u8) -> BookLevel {
        BookLevel {
            price: Decimal128::new(price, scale),
            quantity: Decimal128::new(quantity, scale),
        }
    }

    fn policy(scale: u8) -> DecimalPolicy {
        DecimalPolicy { target_scale: scale, tick_step: 0, ..DecimalPolicy::spot_8() }
    }

    #[test]
    fn default_profile_matches_every_frozen_default() {
        let profile = spot_profile();
        assert_eq!(profile, ExchangeProfile::default());
        assert_eq!(profile.version, 1);
        assert_eq!(profile.price, DecimalPolicy::spot_8());
        assert_eq!(profile.quantity, DecimalPolicy::spot_8());
        assert_eq!(profile.max_levels, 5000);
        assert_eq!(profile.max_symbol_bytes, 32);
        assert_eq!(profile.wrapper, WrapperPolicy::Bare);
        assert_eq!(profile.limits, Limits::strict_default());
        validate_profile(&profile).unwrap();
    }

    #[test]
    fn versions_fail_before_any_other_configuration_error() {
        for version in [0, 2, u16::MAX] {
            let profile = ExchangeProfile { version, max_levels: 0, ..spot_profile() };
            assert_error(validate_profile(&profile).unwrap_err(), ErrorCode::UnsupportedSchemaFeature);
            for kind in [MessageKind::Depth, MessageKind::AggTrade, MessageKind::Snapshot] {
                assert_error(schema_fingerprint(kind, &profile).unwrap_err(),
                    ErrorCode::UnsupportedSchemaFeature);
            }
            assert_error(validate_level(level(0, -1, 255), &profile).unwrap_err(),
                ErrorCode::UnsupportedSchemaFeature);
        }
    }

    #[test]
    fn required_nonzero_budgets_reject_zero_and_accept_their_endpoints() {
        let base = spot_profile();
        for profile in [
            ExchangeProfile { max_levels: 0, ..base },
            ExchangeProfile { max_symbol_bytes: 0, ..base },
            ExchangeProfile { limits: Limits { max_payload_bytes: 0, ..base.limits }, ..base },
            ExchangeProfile { limits: Limits { max_depth: 0, ..base.limits }, ..base },
        ] {
            assert_error(validate_profile(&profile).unwrap_err(), ErrorCode::ConstraintViolation);
            assert_error(schema_fingerprint(MessageKind::Depth, &profile).unwrap_err(),
                ErrorCode::ConstraintViolation);
        }
        for (wide, narrow) in [(u32::MAX, u16::MAX), (1, 1)] {
            let profile = ExchangeProfile {
                max_levels: wide, max_symbol_bytes: wide,
                limits: Limits { max_payload_bytes: wide, max_depth: narrow, ..base.limits },
                ..base
            };
            validate_profile(&profile).unwrap();
        }
    }

    #[test]
    fn optional_resource_zeros_remain_real_accepted_ceilings() {
        let profile = ExchangeProfile {
            limits: Limits {
                max_payload_bytes: 1, max_decoded_bytes: 0, max_depth: 1,
                max_fields_per_object: 0, max_string_bytes: 0, max_number_digits: 0,
                max_array_items: 0, reject_duplicate_keys: false, reject_unknown_keys: false,
            },
            ..spot_profile()
        };
        validate_profile(&profile).unwrap();
        schema_fingerprint(MessageKind::Snapshot, &profile).unwrap();
    }

    #[test]
    fn both_decimal_policies_have_exchange_specific_bounds() {
        for quantity in [false, true] {
            for bad in [
                DecimalPolicy { max_precision: 0, ..policy(8) },
                DecimalPolicy { max_precision: 39, ..policy(8) },
                DecimalPolicy { max_precision: u8::MAX, ..policy(8) },
                policy(39), policy(u8::MAX),
                DecimalPolicy { tick_step: -1, ..policy(8) },
                DecimalPolicy { tick_step: i128::MIN, ..policy(8) },
            ] {
                let mut profile = spot_profile();
                if quantity { profile.quantity = bad; } else { profile.price = bad; }
                assert_error(validate_profile(&profile).unwrap_err(), ErrorCode::ConstraintViolation);
                assert_error(validate_level(level(1, 0, 8), &profile).unwrap_err(),
                    ErrorCode::ConstraintViolation);
            }
        }
        for precision in [1, 38] {
            for scale in [0, 38] {
                for tick in [0, 1, i128::MAX] {
                    for allow_exponent in [false, true] {
                        for reject_inexact_rescale in [false, true] {
                            let policy = DecimalPolicy { max_precision: precision, target_scale: scale,
                                tick_step: tick, allow_exponent, reject_inexact_rescale };
                            validate_profile(&ExchangeProfile {
                                price: policy, quantity: policy, ..spot_profile()
                            }).unwrap();
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn fingerprint_golden_vectors_cover_every_kind_and_wrapper() {
        // Independent byte-encoding reference: 94 bytes per version-one profile.
        let vectors = [
            (WrapperPolicy::Bare, [0x98c1_d751_7651_0682, 0x5de7_42f2_cf39_8d9b, 0xe2b7_cbf1_f8ca_ce98]),
            (WrapperPolicy::CombinedStream, [0x4832_f463_32a0_af05, 0xc20c_6744_e173_9738, 0x7c76_e986_af9b_96fb]),
            (WrapperPolicy::Either, [0x6f18_3e80_a43a_b614, 0x7350_1b6a_997d_be09, 0xa2f8_ebc4_a5d1_d156]),
        ];
        for (wrapper, expected) in vectors {
            let profile = ExchangeProfile { wrapper, ..spot_profile() };
            for (kind, fingerprint) in [MessageKind::Depth, MessageKind::AggTrade, MessageKind::Snapshot]
                .into_iter().zip(expected)
            {
                assert_eq!(schema_fingerprint(kind, &profile).unwrap(), fingerprint);
                assert_eq!(schema_fingerprint(kind, &profile).unwrap(), fingerprint);
            }
        }
    }

    #[test]
    fn fingerprint_wide_integer_and_boolean_golden_vectors() {
        let policy = DecimalPolicy { max_precision: 38, target_scale: 38,
            allow_exponent: true, reject_inexact_rescale: false, tick_step: i128::MAX };
        let profile = ExchangeProfile {
            price: policy, quantity: policy, max_levels: u32::MAX,
            max_symbol_bytes: u32::MAX, wrapper: WrapperPolicy::Either,
            limits: Limits::permissive_default(), ..spot_profile()
        };
        for (kind, expected) in [
            (MessageKind::Depth, 0x7d1f_42b2_21cd_8b68),
            (MessageKind::AggTrade, 0x3c85_7123_50dd_a8a1),
            (MessageKind::Snapshot, 0xc03f_358e_d157_c7d6),
        ] {
            assert_eq!(schema_fingerprint(kind, &profile).unwrap(), expected);
        }
    }

    #[test]
    fn fingerprint_asymmetric_vector_fixes_policy_order_and_byte_widths() {
        let profile = ExchangeProfile {
            version: 1,
            price: DecimalPolicy { max_precision: 17, target_scale: 6,
                allow_exponent: true, reject_inexact_rescale: false,
                tick_step: 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10 },
            quantity: DecimalPolicy { max_precision: 29, target_scale: 23,
                allow_exponent: false, reject_inexact_rescale: true,
                tick_step: 0x1020_3040_5060_7080_90a0_b0c0_d0e0_f000 },
            max_levels: 0x1020_3040, max_symbol_bytes: 0x5060_7080,
            wrapper: WrapperPolicy::CombinedStream,
            limits: Limits {
                max_payload_bytes: 0x1122_3344, max_decoded_bytes: 0x5566_7788,
                max_depth: 0x99aa, max_fields_per_object: 0xbbcc_ddee,
                max_string_bytes: 0x0f1e_2d3c, max_number_digits: 0x4b5a,
                max_array_items: 0x6978_8796, reject_duplicate_keys: false,
                reject_unknown_keys: true,
            },
        };
        for (kind, expected) in [
            (MessageKind::Depth, 0x1e6a_f110_5cd9_3e13),
            (MessageKind::AggTrade, 0x74bd_0983_0035_bc6a),
            (MessageKind::Snapshot, 0x01cf_1a32_11da_e509),
        ] {
            assert_eq!(schema_fingerprint(kind, &profile).unwrap(), expected);
        }
    }

    #[test]
    fn fingerprint_includes_every_independent_configuration_field() {
        let base = spot_profile();
        let mut variants = Vec::new();
        for quantity in [false, true] {
            for changed in [
                DecimalPolicy { max_precision: 37, ..base.price },
                DecimalPolicy { target_scale: 7, ..base.price },
                DecimalPolicy { allow_exponent: true, ..base.price },
                DecimalPolicy { reject_inexact_rescale: false, ..base.price },
                DecimalPolicy { tick_step: 0, ..base.price },
                DecimalPolicy { tick_step: 1_i128 << 120, ..base.price },
            ] {
                let mut profile = base;
                if quantity { profile.quantity = changed; } else { profile.price = changed; }
                variants.push(profile);
            }
        }
        variants.extend([
            ExchangeProfile { max_levels: 5001, ..base },
            ExchangeProfile { max_symbol_bytes: 33, ..base },
            ExchangeProfile { wrapper: WrapperPolicy::CombinedStream, ..base },
            ExchangeProfile { wrapper: WrapperPolicy::Either, ..base },
        ]);
        for limits in [
            Limits { max_payload_bytes: base.limits.max_payload_bytes + 1, ..base.limits },
            Limits { max_decoded_bytes: base.limits.max_decoded_bytes + 1, ..base.limits },
            Limits { max_depth: base.limits.max_depth + 1, ..base.limits },
            Limits { max_fields_per_object: base.limits.max_fields_per_object + 1, ..base.limits },
            Limits { max_string_bytes: base.limits.max_string_bytes + 1, ..base.limits },
            Limits { max_number_digits: base.limits.max_number_digits + 1, ..base.limits },
            Limits { max_array_items: base.limits.max_array_items + 1, ..base.limits },
            Limits { reject_duplicate_keys: false, ..base.limits },
            Limits { reject_unknown_keys: false, ..base.limits },
        ] {
            variants.push(ExchangeProfile { limits, ..base });
        }
        for kind in [MessageKind::Depth, MessageKind::AggTrade, MessageKind::Snapshot] {
            let baseline = schema_fingerprint(kind, &base).unwrap();
            let mut identities = Vec::new();
            for profile in &variants {
                let identity = schema_fingerprint(kind, profile).unwrap();
                assert_ne!(identity, baseline);
                assert!(!identities.contains(&identity));
                identities.push(identity);
            }
        }
    }

    #[test]
    fn level_positivity_and_zero_deletions_preserve_values() {
        let profile = spot_profile();
        for valid in [level(1, 0, 8), level(100_000_000, 250_000_000, 8)] {
            let original = valid;
            validate_level(valid, &profile).unwrap();
            assert_eq!(valid, original);
        }
        for invalid in [level(0, 0, 8), level(-1, 1, 8), level(1, -1, 8)] {
            assert_error(validate_level(invalid, &profile).unwrap_err(), ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn converted_values_require_exact_independent_target_scales() {
        let profile = ExchangeProfile { price: policy(2), quantity: policy(4), ..spot_profile() };
        let valid = BookLevel { price: Decimal128::new(123, 2), quantity: Decimal128::new(0, 4) };
        validate_level(valid, &profile).unwrap();
        for invalid in [
            BookLevel { price: Decimal128::new(1230, 3), ..valid },
            BookLevel { quantity: Decimal128::new(0, 2), ..valid },
            BookLevel { price: Decimal128::new(123, 255), ..valid },
            BookLevel { quantity: Decimal128::new(0, 255), ..valid },
        ] {
            assert_error(validate_level(invalid, &profile).unwrap_err(), ErrorCode::DecimalScaleMismatch);
        }
    }

    #[test]
    fn coefficient_precision_handles_38_digits_and_signed_i128_endpoints() {
        let profile = ExchangeProfile { price: policy(0), quantity: policy(0), ..spot_profile() };
        let max_38 = 99_999_999_999_999_999_999_999_999_999_999_999_999_i128;
        validate_level(level(max_38, max_38, 0), &profile).unwrap();
        for coefficient in [max_38 + 1, i128::MAX, i128::MIN] {
            assert_error(validate_level(level(coefficient, 0, 0), &profile).unwrap_err(),
                ErrorCode::DecimalPrecisionOverflow);
            assert_error(validate_level(level(1, coefficient, 0), &profile).unwrap_err(),
                ErrorCode::DecimalPrecisionOverflow);
        }
        let narrow = ExchangeProfile { price: DecimalPolicy { max_precision: 1, ..policy(0) },
            quantity: DecimalPolicy { max_precision: 1, ..policy(0) }, ..profile };
        validate_level(level(9, 0, 0), &narrow).unwrap();
        for invalid in [level(10, 0, 0), level(1, 10, 0)] {
            assert_error(validate_level(invalid, &narrow).unwrap_err(), ErrorCode::DecimalPrecisionOverflow);
        }
    }

    #[test]
    fn ticks_are_coefficient_multiples_and_zero_disables_the_check() {
        let profile = ExchangeProfile {
            price: DecimalPolicy { tick_step: 25, ..policy(2) },
            quantity: DecimalPolicy { tick_step: 5, ..policy(2) }, ..spot_profile()
        };
        for valid in [level(25, 0, 2), level(125, 15, 2)] {
            validate_level(valid, &profile).unwrap();
        }
        for invalid in [level(26, 0, 2), level(25, 1, 2)] {
            assert_error(validate_level(invalid, &profile).unwrap_err(), ErrorCode::TickMultipleViolation);
        }
        let disabled = ExchangeProfile { price: policy(2), quantity: policy(2), ..profile };
        validate_level(level(26, 1, 2), &disabled).unwrap();
        let huge = ExchangeProfile { price: DecimalPolicy { tick_step: i128::MAX, ..policy(2) },
            ..disabled };
        assert_error(validate_level(level(1, 0, 2), &huge).unwrap_err(), ErrorCode::TickMultipleViolation);
    }

    #[test]
    fn decimal_parser_vectors_integrate_with_level_validation() {
        let profile = spot_profile();
        for (price, quantity, expected_price, expected_quantity) in [
            ("4.00000200", "0.00000000", 400_000_200, 0),
            ("0.00000001", "-0.00000000", 1, 0),
            ("12345.67890123", "2.5", 1_234_567_890_123, 250_000_000),
            ("1.0000000000", "0", 100_000_000, 0),
        ] {
            let price = jmax_decimal::parse::parse_quoted(format!("\"{price}\"").as_bytes(),
                41, &profile.price).unwrap();
            let quantity = jmax_decimal::parse::parse_quoted(format!("\"{quantity}\"").as_bytes(),
                71, &profile.quantity).unwrap();
            assert_eq!(price, Decimal128::new(expected_price, 8));
            assert_eq!(quantity, Decimal128::new(expected_quantity, 8));
            validate_level(BookLevel { price, quantity }, &profile).unwrap();
        }
    }

    #[test]
    fn decimal_grammar_and_canonical_error_metadata_are_preserved() {
        for text in ["", "+1", "01", "-01", ".1", "1.", "1e0", "1E+2",
            "NaN", "Infinity", "1_0", "1 0", " 1", "1 ", "１", "--1"]
        {
            let quoted = format!("\"{text}\"");
            let error = jmax_decimal::parse::parse_quoted(quoted.as_bytes(), 17,
                &spot_profile().price).unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidNumber, "{text}");
            assert_eq!(error.source, ErrorSource::Lexer);
            assert!(error.byte_offset >= 18);
        }
        for (text, code) in [
            ("0.000000001", ErrorCode::DecimalScaleMismatch),
            ("1000000000000000000000000000000", ErrorCode::DecimalPrecisionOverflow),
            ("9999999999999999999999999999999999999999", ErrorCode::NumberOverflow),
        ] {
            let error = jmax_decimal::parse::parse_quoted(format!("\"{text}\"").as_bytes(),
                17, &spot_profile().price).unwrap_err();
            assert_eq!(error.code, code, "{text}");
            assert_eq!(error.source, ErrorSource::Decode);
            assert!(error.byte_offset >= 17);
        }
    }

    #[test]
    fn exponent_and_truncation_vectors_use_no_rounding() {
        let exact = DecimalPolicy { allow_exponent: true, ..policy(2) };
        for (text, coefficient) in [("1e2", 10_000), ("1.25E+1", 1250),
            ("125e-2", 125), ("-0e32767", 0), ("0e-32767", 0)]
        {
            assert_eq!(jmax_decimal::parse::parse_quoted(format!("\"{text}\"").as_bytes(),
                0, &exact).unwrap(), Decimal128::new(coefficient, 2));
        }
        let truncating = DecimalPolicy { reject_inexact_rescale: false, ..exact };
        for (text, coefficient) in [("1.239", 123), ("-1.239", -123),
            ("9.999e-1", 99), ("-0.009", 0), ("1e-32767", 0)]
        {
            assert_eq!(jmax_decimal::parse::parse_quoted(format!("\"{text}\"").as_bytes(),
                0, &truncating).unwrap(), Decimal128::new(coefficient, 2));
            assert_error(jmax_decimal::parse::parse_quoted(format!("\"{text}\"").as_bytes(),
                0, &exact).unwrap_err(), ErrorCode::DecimalScaleMismatch);
        }
        assert_error(jmax_decimal::parse::parse_quoted(b"\"1e32767\"", 0, &exact).unwrap_err(),
            ErrorCode::NumberOverflow);
        let tick = DecimalPolicy { tick_step: 5, ..truncating };
        assert_error(jmax_decimal::parse::parse_quoted(b"\"1.239\"", 0, &tick).unwrap_err(),
            ErrorCode::TickMultipleViolation);
    }

    #[test]
    fn target_scale_38_preserves_the_full_fixed_point_boundary() {
        let profile = ExchangeProfile { price: policy(38), quantity: policy(38), ..spot_profile() };
        let value = jmax_decimal::parse::parse_quoted(
            b"\"0.99999999999999999999999999999999999999\"", 0, &profile.price).unwrap();
        assert_eq!(value.scale, 38);
        assert_eq!(value.coefficient, 10_i128.pow(38) - 1);
        validate_level(BookLevel { price: value, quantity: Decimal128::new(0, 38) }, &profile).unwrap();
        assert_error(jmax_decimal::parse::parse_quoted(b"\"1\"", 0, &profile.price).unwrap_err(),
            ErrorCode::DecimalPrecisionOverflow);
    }

    #[test]
    fn snapshot_seeding_replaces_state_and_overflow_is_atomic() {
        let mut state = SequenceState::new();
        assert_eq!(state.last_update_id(), None);
        assert!(!state.is_synchronized());
        for id in [0, 100, u64::MAX - 1, 7] {
            seed_snapshot(&mut state, id).unwrap();
            assert_eq!(state.last_update_id(), Some(id));
            assert!(state.is_synchronized());
        }
        let before = state;
        assert_error(seed_snapshot(&mut state, u64::MAX).unwrap_err(), ErrorCode::NumberOverflow);
        assert_eq!(state, before);
        state.reset();
        assert_eq!(state, SequenceState::default());
        assert_error(seed_snapshot(&mut state, u64::MAX).unwrap_err(), ErrorCode::NumberOverflow);
        assert_eq!(state, SequenceState::new());
    }

    #[test]
    fn unseeded_and_unsynchronized_states_cannot_classify_or_apply() {
        for initial in [
            SequenceState::new(),
            SequenceState { last_update_id: None, synchronized: true },
            SequenceState { last_update_id: Some(100), synchronized: false },
        ] {
            for (first, last) in [(0, 0), (101, 102), (200, 100), (0, u64::MAX)] {
                assert_error(classify_depth(&initial, first, last).unwrap_err(), ErrorCode::ConstraintViolation);
                let mut state = initial;
                assert_error(apply_depth(&mut state, first, last).unwrap_err(), ErrorCode::ConstraintViolation);
                assert_eq!(state, initial);
            }
        }
    }

    #[test]
    fn classification_vectors_cover_stale_overlap_contiguous_and_gap() {
        let mut state = SequenceState::new();
        seed_snapshot(&mut state, 100).unwrap();
        let initial = state;
        for (first, last, expected) in [
            (0, 0, SequenceDecision::Stale),
            (99, 100, SequenceDecision::Stale),
            (100, 100, SequenceDecision::Stale),
            (0, 101, SequenceDecision::Apply { next_update_id: 102 }),
            (100, 103, SequenceDecision::Apply { next_update_id: 104 }),
            (101, 101, SequenceDecision::Apply { next_update_id: 102 }),
            (101, 110, SequenceDecision::Apply { next_update_id: 111 }),
            (102, 102, SequenceDecision::Gap { expected: 101, received: 102 }),
            (110, 120, SequenceDecision::Gap { expected: 101, received: 110 }),
        ] {
            assert_eq!(classify_depth(&state, first, last).unwrap(), expected);
            assert_eq!(state, initial);
        }
        for (first, last) in [(1, 0), (101, 100), (u64::MAX, 0)] {
            assert_error(classify_depth(&state, first, last).unwrap_err(), ErrorCode::ConstraintViolation);
            assert_eq!(state, initial);
        }
    }

    #[test]
    fn apply_commits_only_accepted_ranges_and_gaps_require_reseeding() {
        let mut state = SequenceState::new();
        seed_snapshot(&mut state, 100).unwrap();
        assert_eq!(apply_depth(&mut state, 99, 102).unwrap(),
            SequenceDecision::Apply { next_update_id: 103 });
        assert_eq!(state.last_update_id(), Some(102));
        assert!(state.is_synchronized());
        let before = state;
        assert_eq!(apply_depth(&mut state, 99, 102).unwrap(), SequenceDecision::Stale);
        assert_eq!(state, before);
        assert_error(apply_depth(&mut state, 104, 103).unwrap_err(), ErrorCode::ConstraintViolation);
        assert_eq!(state, before);
        assert_eq!(apply_depth(&mut state, 105, 110).unwrap(),
            SequenceDecision::Gap { expected: 103, received: 105 });
        assert_eq!(state.last_update_id(), Some(102));
        assert!(!state.is_synchronized());
        let gap_state = state;
        assert_error(apply_depth(&mut state, 103, 104).unwrap_err(), ErrorCode::ConstraintViolation);
        assert_eq!(state, gap_state);
        assert_error(seed_snapshot(&mut state, u64::MAX).unwrap_err(), ErrorCode::NumberOverflow);
        assert_eq!(state, gap_state);
        seed_snapshot(&mut state, 109).unwrap();
        assert_eq!(apply_depth(&mut state, 110, 111).unwrap(),
            SequenceDecision::Apply { next_update_id: 112 });
        assert_eq!(state.last_update_id(), Some(111));
        assert!(state.is_synchronized());
    }

    #[test]
    fn sequence_u64_boundaries_never_wrap_or_partially_mutate() {
        let mut state = SequenceState::new();
        seed_snapshot(&mut state, 0).unwrap();
        assert_eq!(apply_depth(&mut state, 0, 0).unwrap(), SequenceDecision::Stale);
        assert_eq!(apply_depth(&mut state, 0, 1).unwrap(),
            SequenceDecision::Apply { next_update_id: 2 });
        seed_snapshot(&mut state, u64::MAX - 2).unwrap();
        let before = state;
        assert_eq!(classify_depth(&state, u64::MAX, u64::MAX).unwrap(),
            SequenceDecision::Gap { expected: u64::MAX - 1, received: u64::MAX });
        assert_eq!(state, before);
        let mut gap_state = state;
        assert_eq!(apply_depth(&mut gap_state, u64::MAX, u64::MAX).unwrap(),
            SequenceDecision::Gap { expected: u64::MAX - 1, received: u64::MAX });
        assert_eq!(gap_state.last_update_id(), Some(u64::MAX - 2));
        assert!(!gap_state.is_synchronized());
        assert_eq!(apply_depth(&mut state, u64::MAX - 1, u64::MAX - 1).unwrap(),
            SequenceDecision::Apply { next_update_id: u64::MAX });
        let before = state;
        for first in [0, u64::MAX - 1, u64::MAX] {
            assert_error(classify_depth(&state, first, u64::MAX).unwrap_err(), ErrorCode::NumberOverflow);
            assert_error(apply_depth(&mut state, first, u64::MAX).unwrap_err(), ErrorCode::NumberOverflow);
            assert_eq!(state, before);
        }
        assert_eq!(apply_depth(&mut state, u64::MAX - 1, u64::MAX - 1).unwrap(), SequenceDecision::Stale);
        assert_eq!(state, before);
        // Even an internally constructed terminal state obeys stale-before-
        // successor classification, so its unneeded successor is never formed.
        let terminal = SequenceState { last_update_id: Some(u64::MAX), synchronized: true };
        assert_eq!(classify_depth(&terminal, u64::MAX, u64::MAX).unwrap(), SequenceDecision::Stale);
    }

    #[test]
    fn exhaustive_small_ranges_match_classification_and_mutation_rules() {
        for last in 0_u64..=8 {
            for first in 0_u64..=10 {
                for final_id in 0_u64..=10 {
                    let mut initial = SequenceState::new();
                    seed_snapshot(&mut initial, last).unwrap();
                    let classified = classify_depth(&initial, first, final_id);
                    let mut state = initial;
                    let applied = apply_depth(&mut state, first, final_id);
                    assert_eq!(classified, applied);
                    if first > final_id {
                        assert_error(applied.unwrap_err(), ErrorCode::ConstraintViolation);
                        assert_eq!(state, initial);
                    } else if final_id <= last {
                        assert_eq!(applied.unwrap(), SequenceDecision::Stale);
                        assert_eq!(state, initial);
                    } else if first > last + 1 {
                        assert_eq!(applied.unwrap(), SequenceDecision::Gap { expected: last + 1, received: first });
                        assert_eq!(state.last_update_id(), Some(last));
                        assert!(!state.is_synchronized());
                    } else {
                        assert_eq!(applied.unwrap(), SequenceDecision::Apply { next_update_id: final_id + 1 });
                        assert_eq!(state.last_update_id(), Some(final_id));
                        assert!(state.is_synchronized());
                    }
                }
            }
        }
    }

    const DEPTH_VECTOR: &[u8] = br#"{"e":"depthUpdate","E":1234,"s":"BTCUSDT","U":99,"u":102,"b":[["4.00000000","2.00000000"]],"a":[["4.00000200","0.00000000"]]}"#;
    const TRADE_VECTOR: &[u8] = br#"{"e":"aggTrade","E":1234,"s":"BTCUSDT","a":77,"p":"4.00000000","q":"2.00000000","f":8,"l":9,"T":1233,"m":true}"#;
    const SNAPSHOT_VECTOR: &[u8] = br#"{"lastUpdateId":100,"bids":[["4.00000000","2.00000000"]],"asks":[["4.00000200","0.00000000"]]}"#;

    fn fixture(kind: MessageKind) -> &'static [u8] {
        match kind {
            MessageKind::Depth => DEPTH_VECTOR,
            MessageKind::AggTrade => TRADE_VECTOR,
            MessageKind::Snapshot => SNAPSHOT_VECTOR,
        }
    }

    fn fixture_payload(kind: MessageKind, wrapped: bool) -> Payload {
        let bytes = fixture(kind);
        if wrapped {
            let inner = core::str::from_utf8(bytes).unwrap();
            Payload::from_vec(format!(r#"{{"stream":"btcusdt@depth","data":{inner}}}"#).into_bytes())
        } else {
            Payload::from_slice(bytes)
        }
    }

    fn fixture_span(raw: &[u8], bytes: &[u8]) -> Span32 {
        let start = raw.windows(bytes.len()).position(|window| window == bytes).unwrap();
        Span32::new(u32::try_from(start).unwrap(), u32::try_from(bytes.len()).unwrap())
    }

    fn fixture_string(raw: &[u8], text: &[u8]) -> SlotValue {
        let span = fixture_span(raw, text);
        let text = core::str::from_utf8(span.slice(raw).unwrap()).unwrap();
        SlotValue::Str(StrRef { span, source: SpanSource::RawPayload,
            scalar_len: u32::try_from(text.chars().count()).unwrap() })
    }

    // This adapter attests only to the three exact, fully known vectors above
    // under the default decimal/resource configuration and a compatible wrapper.
    // It is deliberately not a general-purpose exchange validator.
    struct FixtureValidator { wrapped: bool, calls: usize }

    impl ExchangeValidator for FixtureValidator {
        fn validate(&mut self, kind: MessageKind, payload: Payload,
            profile: &ExchangeProfile) -> Result<ValidatedSlots>
        {
            self.calls += 1;
            validate_profile(profile)?;
            let expected_profile = ExchangeProfile { wrapper: profile.wrapper, ..spot_profile() };
            if *profile != expected_profile {
                return Err(exchange_error(ErrorCode::ConstraintViolation,
                    "fixture validator requires default decimal and resource policies"));
            }
            let expected_payload = fixture_payload(kind, self.wrapped);
            if payload.as_slice() != expected_payload.as_slice() {
                return Err(exchange_error(ErrorCode::InvalidSyntax, "unknown integration fixture"));
            }
            if (self.wrapped && profile.wrapper == WrapperPolicy::Bare)
                || (!self.wrapped && profile.wrapper == WrapperPolicy::CombinedStream)
            {
                return Err(exchange_error(ErrorCode::TypeMismatch, "fixture wrapper policy mismatch"));
            }
            // Fully traverse syntax and all budgets before issuing evidence.
            let mut cursor = walk::from_payload(payload.clone(), profile.limits, SkipMode::Strict)?;
            loop {
                match walk::next(&mut cursor)? {
                    ScanStep::Emit(_) => {}
                    ScanStep::Finished => break,
                    ScanStep::NeedMore => return Err(exchange_error(ErrorCode::UnexpectedEof,
                        "complete fixture requested more input")),
                }
            }
            assert!(cursor.decoded_bytes().is_empty());
            let price = jmax_decimal::parse::parse_quoted(b"\"4.00000000\"", 0, &profile.price)?;
            let quantity = jmax_decimal::parse::parse_quoted(b"\"2.00000000\"", 0, &profile.quantity)?;
            validate_level(BookLevel { price, quantity }, profile)?;
            assert!(quantity.coefficient > 0);
            validate_level(BookLevel {
                price: jmax_decimal::parse::parse_quoted(b"\"4.00000200\"", 0, &profile.price)?,
                quantity: jmax_decimal::parse::parse_quoted(b"\"0.00000000\"", 0, &profile.quantity)?,
            }, profile)?;
            // Exact byte equality establishes required/optional keys, literals,
            // integer values and their ordering, symbol and level constraints.
            // All ranges below are discovered in the complete original owner.
            let raw = payload.as_slice();
            let bid = || SlotValue::RawSpan(fixture_span(raw, br#"[["4.00000000","2.00000000"]]"#));
            let ask = || SlotValue::RawSpan(fixture_span(raw, br#"[["4.00000200","0.00000000"]]"#));
            let slots = match kind {
                MessageKind::Depth => vec![fixture_string(raw, b"depthUpdate"), SlotValue::U64(1234),
                    fixture_string(raw, b"BTCUSDT"), SlotValue::U64(99), SlotValue::U64(102), bid(), ask()],
                MessageKind::AggTrade => vec![fixture_string(raw, b"aggTrade"), SlotValue::U64(1234),
                    fixture_string(raw, b"BTCUSDT"), SlotValue::U64(77), fixture_string(raw, b"4.00000000"),
                    fixture_string(raw, b"2.00000000"), SlotValue::U64(8), SlotValue::U64(9),
                    SlotValue::U64(1233), SlotValue::Bool(true), SlotValue::Empty],
                MessageKind::Snapshot => vec![SlotValue::U64(100), bid(), ask()],
            };
            let decoded = Vec::<u8>::new().into_boxed_slice();
            // SAFETY: Exact known fixture bytes establish the entire kind's
            // schema and semantics, including every book element. Strict cursor
            // traversal validated syntax and resource budgets. Decimal policies
            // and positivity were checked above under the default profile.
            // Every StrRef uses a checked UTF-8 range and exact scalar count;
            // book spans address these same complete bytes, including wrappers.
            // The checked lengths and fingerprint describe precisely the owners
            // and slots returned here, with no subsequent mutation.
            let proof = unsafe { ValidationProof::issue_unchecked(
                schema_fingerprint(kind, profile)?, u32::try_from(payload.len()).unwrap(), 0) };
            Ok(ValidatedSlots { payload, decoded, slots, proof })
        }
    }

    #[test]
    fn all_decoders_share_profiles_fingerprints_and_original_wrapper_coordinates() {
        for (wrapper, wrapped) in [(WrapperPolicy::Bare, false),
            (WrapperPolicy::CombinedStream, true), (WrapperPolicy::Either, false),
            (WrapperPolicy::Either, true)]
        {
            let profile = ExchangeProfile { wrapper, ..spot_profile() };
            let mut validator = FixtureValidator { wrapped, calls: 0 };
            let snapshot_payload = fixture_payload(MessageKind::Snapshot, wrapped);
            let snapshot_pointer = snapshot_payload.as_slice().as_ptr();
            let snapshot = snapshot::decode_snapshot(snapshot_payload, &profile, &mut validator).unwrap();
            let depth_payload = fixture_payload(MessageKind::Depth, wrapped);
            let depth_pointer = depth_payload.as_slice().as_ptr();
            let depth = depth::decode_depth(depth_payload, &profile, &mut validator).unwrap();
            let trade_payload = fixture_payload(MessageKind::AggTrade, wrapped);
            let trade_pointer = trade_payload.as_slice().as_ptr();
            let trade = trade::decode_trade(trade_payload, &profile, &mut validator).unwrap();
            assert_eq!(validator.calls, 3);
            assert_eq!(snapshot.raw_bytes().as_ptr(), snapshot_pointer);
            assert_eq!(depth.raw_bytes().as_ptr(), depth_pointer);
            assert_eq!(trade.raw_bytes().as_ptr(), trade_pointer);
            assert_eq!(snapshot.proof().schema_fingerprint(), schema_fingerprint(MessageKind::Snapshot, &profile).unwrap());
            assert_eq!(depth.proof().schema_fingerprint(), schema_fingerprint(MessageKind::Depth, &profile).unwrap());
            assert_eq!(trade.proof().schema_fingerprint(), schema_fingerprint(MessageKind::AggTrade, &profile).unwrap());
            assert_eq!(snapshot.values().last_update_id, 100);
            assert_eq!(snapshot.values().bids, vec![level(400_000_000, 200_000_000, 8)]);
            assert_eq!(snapshot.values().asks, vec![level(400_000_200, 0, 8)]);
            assert_eq!(depth.values().bids, snapshot.values().bids);
            assert_eq!(depth.values().asks, snapshot.values().asks);
            assert_eq!(depth.resolve_str(depth.values().symbol), Some("BTCUSDT"));
            assert_eq!(trade.resolve_str(trade.values().symbol), Some("BTCUSDT"));
            assert_eq!(trade.values().price, snapshot.values().bids[0].price);
            assert_eq!(trade.values().quantity, snapshot.values().bids[0].quantity);
            assert!(trade.values().buyer_is_maker);
            assert_eq!(trade.values().best_match, None);
            let mut state = SequenceState::new();
            seed_snapshot(&mut state, snapshot.values().last_update_id).unwrap();
            assert_eq!(apply_depth(&mut state, depth.values().first_update_id,
                depth.values().final_update_id).unwrap(), SequenceDecision::Apply { next_update_id: 103 });
            assert_eq!(state.last_update_id(), Some(102));
            assert_eq!(apply_depth(&mut state, depth.values().first_update_id,
                depth.values().final_update_id).unwrap(), SequenceDecision::Stale);
        }
    }

    #[test]
    fn every_decoder_rejects_invalid_profiles_before_calling_the_validator() {
        let base = spot_profile();
        for (profile, code) in [
            (ExchangeProfile { version: 2, ..base }, ErrorCode::UnsupportedSchemaFeature),
            (ExchangeProfile { price: policy(39), ..base }, ErrorCode::ConstraintViolation),
            (ExchangeProfile { quantity: DecimalPolicy { max_precision: 39, ..base.quantity }, ..base },
                ErrorCode::ConstraintViolation),
            (ExchangeProfile { max_levels: 0, ..base }, ErrorCode::ConstraintViolation),
        ] {
            let mut validator = FixtureValidator { wrapped: false, calls: 0 };
            assert_error(depth::decode_depth(Payload::from_slice(b"invalid"), &profile,
                &mut validator).unwrap_err(), code);
            assert_error(trade::decode_trade(Payload::from_slice(b"invalid"), &profile,
                &mut validator).unwrap_err(), code);
            assert_error(snapshot::decode_snapshot(Payload::from_slice(b"invalid"), &profile,
                &mut validator).unwrap_err(), code);
            assert_eq!(validator.calls, 0);
        }
    }

    struct ReturnedFixture(Option<ValidatedSlots>);

    impl ExchangeValidator for ReturnedFixture {
        fn validate(&mut self, _: MessageKind, _: Payload,
            _: &ExchangeProfile) -> Result<ValidatedSlots>
        {
            Ok(self.0.take().expect("exactly one fixture validation"))
        }
    }

    #[test]
    fn proof_fingerprints_cannot_cross_profile_boundaries_in_any_decoder() {
        let base = spot_profile();
        for profile in [
            ExchangeProfile { price: DecimalPolicy { tick_step: 2, ..base.price }, ..base },
            ExchangeProfile { quantity: DecimalPolicy { allow_exponent: true, ..base.quantity }, ..base },
            ExchangeProfile { max_levels: base.max_levels - 1, ..base },
            ExchangeProfile { wrapper: WrapperPolicy::Either, ..base },
            ExchangeProfile { limits: Limits { reject_unknown_keys: false, ..base.limits }, ..base },
        ] {
            for kind in [MessageKind::Depth, MessageKind::AggTrade, MessageKind::Snapshot] {
                let payload = fixture_payload(kind, false);
                let slots = FixtureValidator { wrapped: false, calls: 0 }
                    .validate(kind, payload.clone(), &base).unwrap();
                let mut validator = ReturnedFixture(Some(slots));
                let (error, schema) = match kind {
                    MessageKind::Depth => (depth::decode_depth(payload, &profile,
                        &mut validator).unwrap_err(), DEPTH_SCHEMA_ID),
                    MessageKind::AggTrade => (trade::decode_trade(payload, &profile,
                        &mut validator).unwrap_err(), TRADE_SCHEMA_ID),
                    MessageKind::Snapshot => (snapshot::decode_snapshot(payload, &profile,
                        &mut validator).unwrap_err(), SNAPSHOT_SCHEMA_ID),
                };
                assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
                assert_eq!(error.source, ErrorSource::Decode);
                assert_eq!(error.byte_offset, 0);
                assert_eq!(error.schema_id, schema.0);
                assert_eq!(error.field_id, 0);
                assert!(validator.0.is_none());
            }
        }
    }

    #[test]
    fn book_reader_applies_policies_to_every_level_and_keeps_zero_quantities() {
        let profile = ExchangeProfile { price: DecimalPolicy { tick_step: 25, ..policy(2) },
            quantity: DecimalPolicy { tick_step: 5, ..policy(3) }, ..spot_profile() };
        let bytes = br#"[["1.25","0"],["2.50","0.015"]]"#;
        let mut cursor = walk::from_payload(Payload::from_slice(bytes), profile.limits, SkipMode::Strict).unwrap();
        assert!(matches!(walk::next(&mut cursor).unwrap(), ScanStep::Emit(Event::ArrayStart { .. })));
        let (span, values) = depth::read_book_levels(&mut cursor, &profile, DEPTH_SCHEMA_ID, FieldId(6)).unwrap();
        assert_eq!(span, Span32::new(0, u32::try_from(bytes.len()).unwrap()));
        assert_eq!(values, vec![
            BookLevel { price: Decimal128::new(125, 2), quantity: Decimal128::new(0, 3) },
            BookLevel { price: Decimal128::new(250, 2), quantity: Decimal128::new(15, 3) },
        ]);
        assert_eq!(walk::next(&mut cursor).unwrap(), ScanStep::Finished);
        for (bad, code) in [
            (r#"[["1.25","0"],["2.51","0"]]"#, ErrorCode::TickMultipleViolation),
            (r#"[["1.25","0"],["2.50","-0.005"]]"#, ErrorCode::ConstraintViolation),
            (r#"[["1.25","0"],["2.50","0.0001"]]"#, ErrorCode::DecimalScaleMismatch),
            (r#"[["1.25","0"],["2.50","0.001"]]"#, ErrorCode::TickMultipleViolation),
        ] {
            let mut cursor = walk::from_payload(Payload::from_slice(bad.as_bytes()),
                profile.limits, SkipMode::Strict).unwrap();
            walk::next(&mut cursor).unwrap();
            let error = depth::read_book_levels(&mut cursor, &profile, SNAPSHOT_SCHEMA_ID, FieldId(3)).unwrap_err();
            assert_eq!(error.code, code);
            assert_eq!(error.source, ErrorSource::Decode);
            assert_eq!(error.schema_id, SNAPSHOT_SCHEMA_ID.0);
            assert_eq!(error.field_id, 3);
        }
    }
}
