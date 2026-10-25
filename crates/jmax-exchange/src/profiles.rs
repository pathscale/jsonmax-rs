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
