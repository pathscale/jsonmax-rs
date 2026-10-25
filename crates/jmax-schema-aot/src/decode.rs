//! Checked slot adapters for generated decoders.
//!
//! Types are exact: integer narrowing is checked, floats must be finite, and
//! borrowed values retain the lifetime of the reader's backing owner. These
//! adapters neither issue proofs nor validate raw bytes as JSON.
//!
//! A decoder rejects an incompatible specification before accessing the slot,
//! including when the slot is empty. Missing required values, type mismatches,
//! nonfinite floats, and narrowing overflow have distinct ABI error codes.
//! Errors use the Decode source and the supplied field identifier, with schema
//! identifier zero until a caller adds known schema context.

use alloc::vec::Vec;
use core::str::FromStr;

use crate::{
    Decimal128, DecodePlan, Error, ErrorCode, ErrorSource, FieldId, Result, SlotKind, SlotReader,
    SlotSpec, SlotValue, Span32, StrRef, aot_error,
};

/// Check the owner and fingerprint, then the entire plan and every selected value.
///
/// Indices must cover every slot exactly once, and field identifiers must be
/// unique. Plan order need not equal slot order. Structural errors are reported
/// before value errors. Validation uses O(n) temporary storage and O(n log n)
/// comparisons; allocation failure is reported as `BudgetExhausted`.
pub fn validate_slots(
    values: &(impl crate::SlotSource + ?Sized),
    plan: &DecodePlan<'_>,
) -> Result<()> {
    let reader = SlotReader::new(values, plan.schema_fingerprint)?;
    validate_from(&reader, reader.values().slots.len(), plan.slots)
}

/// Read an exact Boolean slot, or `None` for an optional empty slot.
pub fn decode_bool(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<bool>> {
    decode_from(reader, spec)
}

/// Read an exact unsigned integer slot without signed or floating conversion.
pub fn decode_u64(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<u64>> {
    decode_from(reader, spec)
}

/// Read a `U64` slot, rejecting values above `u32::MAX`.
pub fn decode_u32(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<u32>> {
    decode_from(reader, spec)
}

/// Read an exact signed integer slot without unsigned or floating conversion.
pub fn decode_i64(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<i64>> {
    decode_from(reader, spec)
}

/// Read an `I64` slot, rejecting values outside the inclusive `i32` range.
pub fn decode_i32(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<i32>> {
    decode_from(reader, spec)
}

/// Read finite `F64Bits`, preserving subnormals and the sign of zero.
pub fn decode_f64(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<f64>> {
    decode_from(reader, spec)
}

/// Read a decimal with its exact coefficient and scale, without normalization.
pub fn decode_decimal(reader: &SlotReader<'_>, spec: &SlotSpec) -> Result<Option<Decimal128>> {
    decode_from(reader, spec)
}

/// Borrow a string after the reader checks its owner, bounds, UTF-8, and scalar count.
pub fn decode_str<'a>(reader: &SlotReader<'a>, spec: &SlotSpec) -> Result<Option<&'a str>> {
    decode_from(reader, spec)
}

/// Decode a validated string through an explicit foreign-type conversion.
///
/// The converter runs only for a present string. Its error is returned
/// unchanged and receives the same field context as the built-in decoders.
/// This is the preferred hook for `TryFrom<&str>` wrappers with domain-specific
/// error mapping.
pub fn decode_str_with<'a, T>(
    reader: &SlotReader<'a>,
    spec: &SlotSpec,
    convert: impl FnOnce(&'a str) -> Result<T>,
) -> Result<Option<T>> {
    decode_str(reader, spec)?
        .map(convert)
        .transpose()
        .map_err(|error| error.with_schema(0, spec.field_id.0))
}

/// Decode a validated string using `FromStr` for a foreign owned type.
///
/// Parse errors map to `ConstraintViolation`; callers needing richer error
/// metadata can use [`decode_str_with`] instead. No type is accepted unless it
/// actually implements `FromStr`.
pub fn decode_from_str<'a, T: FromStr>(
    reader: &SlotReader<'a>,
    spec: &SlotSpec,
) -> Result<Option<T>> {
    decode_str_with(reader, spec, |text| {
        T::from_str(text).map_err(|_| {
            decode_error(ErrorCode::ConstraintViolation, "string is invalid for foreign type")
        })
    })
}

/// Decode a validated string through a foreign `TryFrom<&str>` implementation.
///
/// Conversion failures map to `ConstraintViolation`. Use [`decode_str_with`]
/// when the hook needs to preserve a more specific error.
pub fn decode_try_from_str<'a, T>(
    reader: &SlotReader<'a>,
    spec: &SlotSpec,
) -> Result<Option<T>>
where
    T: TryFrom<&'a str>,
{
    decode_str_with(reader, spec, |text| {
        T::try_from(text).map_err(|_| {
            decode_error(ErrorCode::ConstraintViolation, "string is invalid for foreign type")
        })
    })
}

/// Decode a string wrapper after checking its exact Unicode scalar length.
///
/// The length convention matches schema string constraints. Byte-oriented
/// fixed-length wrappers should check `text.len()` in their conversion hook
/// instead. A missing optional slot remains `None` and does not invoke either
/// check or conversion.
pub fn decode_str_with_exact_len<'a, T>(
    reader: &SlotReader<'a>,
    spec: &SlotSpec,
    expected_scalar_len: u32,
    convert: impl FnOnce(&'a str) -> Result<T>,
) -> Result<Option<T>> {
    decode_str_with(reader, spec, |text| {
        if u32::try_from(text.chars().count()).ok() != Some(expected_scalar_len) {
            return Err(decode_error(
                ErrorCode::ConstraintViolation,
                "string length differs from fixed wrapper length",
            ));
        }
        convert(text)
    })
}

/// Decode a decimal through an explicit foreign-type conversion.
///
/// The slot's coefficient and scale are preserved exactly before the hook is
/// called. This does not rescale or round a foreign decimal representation.
pub fn decode_decimal_with<T>(
    reader: &SlotReader<'_>,
    spec: &SlotSpec,
    convert: impl FnOnce(Decimal128) -> Result<T>,
) -> Result<Option<T>> {
    decode_decimal(reader, spec)?
        .map(convert)
        .transpose()
        .map_err(|error| error.with_schema(0, spec.field_id.0))
}

/// Decode an exact decimal through a foreign `TryFrom<Decimal128>` hook.
///
/// Conversion failures map to `ConstraintViolation`; no scale conversion is
/// performed by the adapter.
pub fn decode_try_from_decimal<T>(
    reader: &SlotReader<'_>,
    spec: &SlotSpec,
) -> Result<Option<T>>
where
    T: TryFrom<Decimal128>,
{
    decode_decimal_with(reader, spec, |value| {
        T::try_from(value).map_err(|_| {
            decode_error(ErrorCode::ConstraintViolation, "decimal is invalid for foreign type")
        })
    })
}

/// Decode a signed enum representation through an explicit checked hook.
///
/// The schema slot must be `I64`; narrowing to `i32` is checked before the
/// converter runs. This supports the ABI's signed enum representation without
/// assuming Rust enum memory layout or casting discriminants.
pub fn decode_i32_repr<T>(
    reader: &SlotReader<'_>,
    spec: &SlotSpec,
    convert: impl FnOnce(i32) -> Result<T>,
) -> Result<Option<T>> {
    decode_i32(reader, spec)?
        .map(convert)
        .transpose()
        .map_err(|error| error.with_schema(0, spec.field_id.0))
}

/// Borrow a bounded payload span. Success does not certify a JSON fragment.
pub fn decode_raw<'a>(reader: &SlotReader<'a>, spec: &SlotSpec) -> Result<Option<&'a [u8]>> {
    decode_from(reader, spec)
}

fn decode_error(code: ErrorCode, detail: &'static str) -> Error {
    aot_error(code, ErrorSource::Decode, detail)
}

fn type_mismatch() -> Error {
    decode_error(ErrorCode::TypeMismatch, "slot value has the wrong type")
}

// Keep the algorithms independent of proof construction. Production callers
// always enter through SlotReader; tests can supply unproven data without
// manufacturing ValidationProof or weakening the crate's unsafe-code ban.
trait SlotAccess<'a> {
    fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue>;
    fn resolve_str(&self, value: StrRef) -> Result<&'a str>;
    fn resolve_raw(&self, span: Span32) -> Result<&'a [u8]>;
}

impl<'a> SlotAccess<'a> for SlotReader<'a> {
    fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue> {
        SlotReader::slot(self, index, field_id)
    }

    fn resolve_str(&self, value: StrRef) -> Result<&'a str> {
        SlotReader::resolve_str(self, value)
    }

    fn resolve_raw(&self, span: Span32) -> Result<&'a [u8]> {
        SlotReader::resolve_raw(self, span)
    }
}

// Private and statically dispatched: generated callers cannot substitute an
// unchecked source or add coercions to the public decoder entry points.
trait FromSlot<'a>: Sized {
    const KIND: SlotKind;

    fn from_slot(value: SlotValue, reader: &impl SlotAccess<'a>) -> Result<Self>;
}

fn decode_from<'a, T: FromSlot<'a>>(
    reader: &impl SlotAccess<'a>,
    spec: &SlotSpec,
) -> Result<Option<T>> {
    if spec.kind != T::KIND {
        return Err(decode_error(
            ErrorCode::SchemaImageInvalid,
            "slot specification has the wrong decoder kind",
        )
        .with_schema(0, spec.field_id.0));
    }
    let value = reader
        .slot(spec.index, spec.field_id)
        .map_err(|error| error.with_schema(0, spec.field_id.0))?;
    if value == SlotValue::Empty {
        return if spec.required {
            Err(
                decode_error(ErrorCode::MissingRequiredField, "required slot is empty")
                    .with_schema(0, spec.field_id.0),
            )
        } else {
            Ok(None)
        };
    }
    T::from_slot(value, reader)
        .map(Some)
        .map_err(|error| error.with_schema(0, spec.field_id.0))
}

macro_rules! exact_slot {
    ($ty:ty, $kind:ident, $variant:ident) => {
        impl<'a> FromSlot<'a> for $ty {
            const KIND: SlotKind = SlotKind::$kind;

            fn from_slot(value: SlotValue, _: &impl SlotAccess<'a>) -> Result<Self> {
                match value {
                    SlotValue::$variant(value) => Ok(value),
                    _ => Err(type_mismatch()),
                }
            }
        }
    };
}

exact_slot!(bool, Bool, Bool);
exact_slot!(u64, U64, U64);
exact_slot!(i64, I64, I64);
exact_slot!(Decimal128, Decimal, Decimal);

impl<'a> FromSlot<'a> for u32 {
    const KIND: SlotKind = SlotKind::U64;

    fn from_slot(value: SlotValue, reader: &impl SlotAccess<'a>) -> Result<Self> {
        let value = <u64 as FromSlot<'a>>::from_slot(value, reader)?;
        u32::try_from(value)
            .map_err(|_| decode_error(ErrorCode::NumberOverflow, "unsigned slot does not fit u32"))
    }
}

impl<'a> FromSlot<'a> for i32 {
    const KIND: SlotKind = SlotKind::I64;

    fn from_slot(value: SlotValue, reader: &impl SlotAccess<'a>) -> Result<Self> {
        let value = <i64 as FromSlot<'a>>::from_slot(value, reader)?;
        i32::try_from(value)
            .map_err(|_| decode_error(ErrorCode::NumberOverflow, "signed slot does not fit i32"))
    }
}

impl<'a> FromSlot<'a> for f64 {
    const KIND: SlotKind = SlotKind::F64;

    fn from_slot(value: SlotValue, _: &impl SlotAccess<'a>) -> Result<Self> {
        match value {
            SlotValue::F64Bits(bits) => {
                let value = f64::from_bits(bits);
                if value.is_finite() {
                    Ok(value)
                } else {
                    Err(decode_error(
                        ErrorCode::InvalidNumber,
                        "floating slot is not finite",
                    ))
                }
            }
            _ => Err(type_mismatch()),
        }
    }
}

impl<'a> FromSlot<'a> for &'a str {
    const KIND: SlotKind = SlotKind::Str;

    fn from_slot(value: SlotValue, reader: &impl SlotAccess<'a>) -> Result<Self> {
        match value {
            SlotValue::Str(value) => reader.resolve_str(value),
            _ => Err(type_mismatch()),
        }
    }
}

impl<'a> FromSlot<'a> for &'a [u8] {
    const KIND: SlotKind = SlotKind::Raw;

    fn from_slot(value: SlotValue, reader: &impl SlotAccess<'a>) -> Result<Self> {
        match value {
            SlotValue::RawSpan(span) => reader.resolve_raw(span),
            _ => Err(type_mismatch()),
        }
    }
}

fn validate_shape(slot_count: usize, specs: &[SlotSpec]) -> Result<()> {
    if specs.len() != slot_count {
        return Err(decode_error(
            ErrorCode::SchemaImageInvalid,
            "decode plan does not cover the slot array",
        ));
    }
    let mut ordered: Vec<&SlotSpec> = Vec::new();
    ordered.try_reserve_exact(specs.len()).map_err(|_| {
        decode_error(
            ErrorCode::BudgetExhausted,
            "cannot allocate decode plan validation scratch",
        )
    })?;
    ordered.extend(specs.iter());
    ordered.sort_unstable_by_key(|spec| spec.index);
    // Equal lengths and exact dense indices also exclude duplicate indices.
    for (expected, spec) in ordered.iter().enumerate() {
        if usize::try_from(spec.index).ok() != Some(expected) {
            return Err(decode_error(
                ErrorCode::SchemaImageInvalid,
                "decode plan indices are not unique and dense",
            )
            .with_schema(0, spec.field_id.0));
        }
    }
    ordered.sort_unstable_by_key(|spec| spec.field_id.0);
    for pair in ordered.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(decode_error(
                ErrorCode::SchemaImageInvalid,
                "decode plan has duplicate field identifiers",
            )
            .with_schema(0, pair[1].field_id.0));
        }
    }
    Ok(())
}

fn validate_from<'a>(
    reader: &impl SlotAccess<'a>,
    slot_count: usize,
    specs: &[SlotSpec],
) -> Result<()> {
    validate_shape(slot_count, specs)?;
    // Preserve declaration order for deterministic value-error precedence.
    for spec in specs {
        match spec.kind {
            SlotKind::Bool => decode_from::<bool>(reader, spec).map(|_| ()),
            SlotKind::U64 => decode_from::<u64>(reader, spec).map(|_| ()),
            SlotKind::I64 => decode_from::<i64>(reader, spec).map(|_| ()),
            SlotKind::F64 => decode_from::<f64>(reader, spec).map(|_| ()),
            SlotKind::Decimal => decode_from::<Decimal128>(reader, spec).map(|_| ()),
            SlotKind::Str => decode_from::<&str>(reader, spec).map(|_| ()),
            SlotKind::Raw => decode_from::<&[u8]>(reader, spec).map(|_| ()),
        }?;
    }
    Ok(())
}
