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

use crate::{
    aot_error, Decimal128, DecodePlan, Error, ErrorCode, ErrorSource, FieldId,
    Result, SlotKind, SlotReader, SlotSpec, SlotValue, Span32, StrRef,
    ValidatedSlots,
};

/// Check the owner and fingerprint, then the entire plan and every selected value.
///
/// Indices must cover every slot exactly once, and field identifiers must be
/// unique. Plan order need not equal slot order. Structural errors are reported
/// before value errors. Validation uses O(n) temporary storage and O(n log n)
/// comparisons; allocation failure is reported as `BudgetExhausted`.
pub fn validate_slots(values: &ValidatedSlots, plan: &DecodePlan<'_>) -> Result<()> {
    let reader = SlotReader::new(values, plan.schema_fingerprint)?;
    validate_from(&reader, values.slots.len(), plan.slots)
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
pub fn decode_decimal(
    reader: &SlotReader<'_>,
    spec: &SlotSpec,
) -> Result<Option<Decimal128>> {
    decode_from(reader, spec)
}

/// Borrow a string after the reader checks its owner, bounds, UTF-8, and scalar count.
pub fn decode_str<'a>(reader: &SlotReader<'a>, spec: &SlotSpec) -> Result<Option<&'a str>> {
    decode_from(reader, spec)
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
        ).with_schema(0, spec.field_id.0));
    }
    let value = reader.slot(spec.index, spec.field_id)
        .map_err(|error| error.with_schema(0, spec.field_id.0))?;
    if value == SlotValue::Empty {
        return if spec.required {
            Err(decode_error(
                ErrorCode::MissingRequiredField,
                "required slot is empty",
            ).with_schema(0, spec.field_id.0))
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
        u32::try_from(value).map_err(|_| decode_error(
            ErrorCode::NumberOverflow, "unsigned slot does not fit u32",
        ))
    }
}

impl<'a> FromSlot<'a> for i32 {
    const KIND: SlotKind = SlotKind::I64;

    fn from_slot(value: SlotValue, reader: &impl SlotAccess<'a>) -> Result<Self> {
        let value = <i64 as FromSlot<'a>>::from_slot(value, reader)?;
        i32::try_from(value).map_err(|_| decode_error(
            ErrorCode::NumberOverflow, "signed slot does not fit i32",
        ))
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
                    Err(decode_error(ErrorCode::InvalidNumber, "floating slot is not finite"))
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
            ErrorCode::SchemaImageInvalid, "decode plan does not cover the slot array",
        ));
    }
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(specs.len()).map_err(|_| decode_error(
        ErrorCode::BudgetExhausted, "cannot allocate decode plan validation scratch",
    ))?;
    ordered.extend(specs.iter());
    ordered.sort_unstable_by_key(|spec| spec.index);
    // Equal lengths and exact dense indices also exclude duplicate indices.
    for (expected, spec) in ordered.iter().enumerate() {
        if usize::try_from(spec.index).ok() != Some(expected) {
            return Err(decode_error(
                ErrorCode::SchemaImageInvalid, "decode plan indices are not unique and dense",
            ).with_schema(0, spec.field_id.0));
        }
    }
    ordered.sort_unstable_by_key(|spec| spec.field_id.0);
    for pair in ordered.windows(2) {
        if pair[0].field_id == pair[1].field_id {
            return Err(decode_error(
                ErrorCode::SchemaImageInvalid, "decode plan has duplicate field identifiers",
            ).with_schema(0, pair[1].field_id.0));
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::{cell::Cell, fmt::Debug};
    use crate::SpanSource;

    const KINDS: [SlotKind; 7] = [
        SlotKind::Bool, SlotKind::U64, SlotKind::I64, SlotKind::F64,
        SlotKind::Decimal, SlotKind::Str, SlotKind::Raw,
    ];

    // No allowed dependency can safely construct a ValidationProof. This
    // fixture exercises the very same private algorithms as the public APIs,
    // but is deliberately not a ValidatedSlots or a source of evidence. The
    // production owner/fingerprint gate remains SlotReader::new in the root.
    struct Fixture<'a> {
        values: &'a [SlotValue],
        payload: &'a [u8],
        decoded: &'a [u8],
        reads: Cell<usize>,
        resolutions: Cell<usize>,
        resolver_error: Option<Error>,
    }

    impl<'a> Fixture<'a> {
        fn new(values: &'a [SlotValue], payload: &'a [u8], decoded: &'a [u8]) -> Self {
            Self {
                values, payload, decoded, reads: Cell::new(0),
                resolutions: Cell::new(0), resolver_error: None,
            }
        }
    }

    impl<'a> SlotAccess<'a> for Fixture<'a> {
        fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue> {
            self.reads.set(self.reads.get() + 1);
            usize::try_from(index).ok().and_then(|index| self.values.get(index)).copied()
                .ok_or_else(|| decode_error(
                    ErrorCode::SchemaImageInvalid, "fixture slot out of bounds",
                ).with_schema(0, field_id.0))
        }

        fn resolve_str(&self, value: StrRef) -> Result<&'a str> {
            self.resolutions.set(self.resolutions.get() + 1);
            if let Some(error) = &self.resolver_error {
                return Err(error.clone());
            }
            let owner = match value.source {
                SpanSource::RawPayload => self.payload,
                SpanSource::DecodedArena => self.decoded,
            };
            let bytes = value.span.slice(owner).ok_or_else(|| decode_error(
                ErrorCode::SchemaImageInvalid, "fixture string out of bounds",
            ))?;
            let text = core::str::from_utf8(bytes).map_err(|_| Error::new(
                ErrorCode::InvalidUtf8, ErrorSource::Decode,
                value.span.offset, "fixture string is not UTF-8",
            ))?;
            if usize::try_from(value.scalar_len).ok() != Some(text.chars().count()) {
                return Err(decode_error(
                    ErrorCode::SchemaImageInvalid, "fixture scalar count mismatch",
                ));
            }
            Ok(text)
        }

        fn resolve_raw(&self, span: Span32) -> Result<&'a [u8]> {
            self.resolutions.set(self.resolutions.get() + 1);
            if let Some(error) = &self.resolver_error {
                return Err(error.clone());
            }
            span.slice(self.payload).ok_or_else(|| decode_error(
                ErrorCode::SchemaImageInvalid, "fixture raw span out of bounds",
            ))
        }
    }

    fn spec(kind: SlotKind) -> SlotSpec {
        SlotSpec { index: 0, field_id: FieldId(u32::MAX), kind, required: true }
    }

    fn assert_error<T: Debug>(result: Result<T>, code: ErrorCode, field_id: FieldId) -> Error {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::Decode);
        assert_eq!(error.schema_id, 0);
        assert_eq!(error.field_id, field_id.0);
        assert!(!error.detail.is_empty());
        error
    }

    fn assert_absence_and_access<T>()
    where
        T: for<'a> FromSlot<'a> + Debug + PartialEq,
    {
        let values = [SlotValue::Empty];
        let reader = Fixture::new(&values, &[], &[]);
        let mut spec = spec(T::KIND);
        assert_error(decode_from::<T>(&reader, &spec), ErrorCode::MissingRequiredField, spec.field_id);
        spec.required = false;
        assert_eq!(decode_from::<T>(&reader, &spec).unwrap(), None);
        for required in [false, true] {
            for kind in KINDS {
                if kind != T::KIND {
                    let wrong = SlotSpec { kind, required, ..spec };
                    let reads = reader.reads.get();
                    assert_error(decode_from::<T>(&reader, &wrong), ErrorCode::SchemaImageInvalid, spec.field_id);
                    assert_eq!(reader.reads.get(), reads);
                }
            }
        }
        for index in [1, u32::MAX] {
            let outside = SlotSpec { index, ..spec };
            assert_error(decode_from::<T>(&reader, &outside), ErrorCode::SchemaImageInvalid, spec.field_id);
        }
        assert_eq!(reader.resolutions.get(), 0);
    }

    #[test]
    fn scalar_absence_spec_kind_and_bounds() {
        assert_absence_and_access::<bool>();
        assert_absence_and_access::<u64>();
        assert_absence_and_access::<u32>();
        assert_absence_and_access::<i64>();
        assert_absence_and_access::<i32>();
        assert_absence_and_access::<f64>();
        assert_absence_and_access::<Decimal128>();
    }

    #[test]
    fn borrowed_absence_spec_kind_and_bounds() {
        let values = [SlotValue::Empty];
        let reader = Fixture::new(&values, &[], &[]);
        for required in [false, true] {
            for kind in KINDS {
                let spec = SlotSpec { required, ..spec(kind) };
                let text = decode_from::<&str>(&reader, &spec);
                let bytes = decode_from::<&[u8]>(&reader, &spec);
                if kind != SlotKind::Str {
                    assert_error(text, ErrorCode::SchemaImageInvalid, spec.field_id);
                } else if required {
                    assert_error(text, ErrorCode::MissingRequiredField, spec.field_id);
                } else {
                    assert_eq!(text.unwrap(), None);
                }
                if kind != SlotKind::Raw {
                    assert_error(bytes, ErrorCode::SchemaImageInvalid, spec.field_id);
                } else if required {
                    assert_error(bytes, ErrorCode::MissingRequiredField, spec.field_id);
                } else {
                    assert_eq!(bytes.unwrap(), None);
                }
            }
        }
        for index in [1, u32::MAX] {
            let text_spec = SlotSpec { index, required: false, ..spec(SlotKind::Str) };
            let raw_spec = SlotSpec { kind: SlotKind::Raw, ..text_spec };
            assert_error(decode_from::<&str>(&reader, &text_spec), ErrorCode::SchemaImageInvalid, text_spec.field_id);
            assert_error(decode_from::<&[u8]>(&reader, &raw_spec), ErrorCode::SchemaImageInvalid, raw_spec.field_id);
        }
        assert_eq!(reader.resolutions.get(), 0);
    }

    #[test]
    fn exact_type_matrix_rejects_all_cross_kind_coercions() {
        let values = [
            SlotValue::Bool(true), SlotValue::U64(1), SlotValue::I64(1),
            SlotValue::F64Bits(1.0f64.to_bits()), SlotValue::Decimal(Decimal128::new(1, 0)),
            SlotValue::Str(StrRef {
                span: Span32::new(0, 1), source: SpanSource::RawPayload, scalar_len: 1,
            }),
            SlotValue::RawSpan(Span32::new(0, 1)),
        ];
        for (value_kind, value) in KINDS.into_iter().zip(values) {
            let values = [value];
            let reader = Fixture::new(&values, b"1", &[]);
            for expected in KINDS {
                let spec = spec(expected);
                let result = validate_from(&reader, 1, &[spec]);
                if value_kind == expected {
                    result.unwrap();
                } else {
                    assert_error(result, ErrorCode::TypeMismatch, spec.field_id);
                }
            }
            for required in [false, true] {
                let unsigned = SlotSpec { required, ..spec(SlotKind::U64) };
                let signed = SlotSpec { required, ..spec(SlotKind::I64) };
                if value_kind == SlotKind::U64 {
                    assert_eq!(decode_from::<u32>(&reader, &unsigned).unwrap(), Some(1));
                } else {
                    assert_error(decode_from::<u32>(&reader, &unsigned), ErrorCode::TypeMismatch, unsigned.field_id);
                }
                if value_kind == SlotKind::I64 {
                    assert_eq!(decode_from::<i32>(&reader, &signed).unwrap(), Some(1));
                } else {
                    assert_error(decode_from::<i32>(&reader, &signed), ErrorCode::TypeMismatch, signed.field_id);
                }
            }
        }
    }

    #[test]
    fn boolean_and_full_width_integer_values_are_preserved() {
        for value in [false, true] {
            let values = [SlotValue::Bool(value)];
            let reader = Fixture::new(&values, &[], &[]);
            assert_eq!(decode_from::<bool>(&reader, &spec(SlotKind::Bool)).unwrap(), Some(value));
        }
        for value in [0, 1, u64::from(u32::MAX), 1u64 << 63, u64::MAX] {
            let values = [SlotValue::U64(value)];
            let reader = Fixture::new(&values, &[], &[]);
            assert_eq!(decode_from::<u64>(&reader, &spec(SlotKind::U64)).unwrap(), Some(value));
        }
        for value in [i64::MIN, i64::from(i32::MIN), -1, 0, 1, i64::from(i32::MAX), i64::MAX] {
            let values = [SlotValue::I64(value)];
            let reader = Fixture::new(&values, &[], &[]);
            assert_eq!(decode_from::<i64>(&reader, &spec(SlotKind::I64)).unwrap(), Some(value));
        }
    }

    #[test]
    fn narrowing_checks_both_inclusive_boundaries() {
        for (value, expected) in [
            (0, Some(0)), (1, Some(1)),
            (u64::from(u32::MAX) - 1, Some(u32::MAX - 1)),
            (u64::from(u32::MAX), Some(u32::MAX)),
            (u64::from(u32::MAX) + 1, None), (u64::MAX, None),
        ] {
            let values = [SlotValue::U64(value)];
            let reader = Fixture::new(&values, &[], &[]);
            let spec = spec(SlotKind::U64);
            match expected {
                Some(expected) => assert_eq!(decode_from::<u32>(&reader, &spec).unwrap(), Some(expected)),
                None => { assert_error(decode_from::<u32>(&reader, &spec), ErrorCode::NumberOverflow, spec.field_id); }
            }
            // A U64 plan itself does not impose a generated consumer's u32 limit.
            validate_from(&reader, 1, &[spec]).unwrap();
        }
        for (value, expected) in [
            (i64::MIN, None), (i64::from(i32::MIN) - 1, None),
            (i64::from(i32::MIN), Some(i32::MIN)),
            (i64::from(i32::MIN) + 1, Some(i32::MIN + 1)),
            (-1, Some(-1)), (0, Some(0)), (1, Some(1)),
            (i64::from(i32::MAX), Some(i32::MAX)),
            (i64::from(i32::MAX) + 1, None), (i64::MAX, None),
        ] {
            let values = [SlotValue::I64(value)];
            let reader = Fixture::new(&values, &[], &[]);
            let spec = spec(SlotKind::I64);
            match expected {
                Some(expected) => assert_eq!(decode_from::<i32>(&reader, &spec).unwrap(), Some(expected)),
                None => { assert_error(decode_from::<i32>(&reader, &spec), ErrorCode::NumberOverflow, spec.field_id); }
            }
            validate_from(&reader, 1, &[spec]).unwrap();
        }
    }

    #[test]
    fn finite_floats_preserve_bits_and_nonfinite_patterns_fail() {
        for bits in [
            0, 1, 0x000f_ffff_ffff_ffff, 0x0010_0000_0000_0000,
            1.5f64.to_bits(), f64::MAX.to_bits(),
            0x8000_0000_0000_0000, 0x8000_0000_0000_0001,
            (-f64::MAX).to_bits(),
        ] {
            let values = [SlotValue::F64Bits(bits)];
            let reader = Fixture::new(&values, &[], &[]);
            let spec = spec(SlotKind::F64);
            assert_eq!(decode_from::<f64>(&reader, &spec).unwrap().unwrap().to_bits(), bits);
            validate_from(&reader, 1, &[spec]).unwrap();
        }
        for bits in [
            0x7ff0_0000_0000_0000, 0xfff0_0000_0000_0000,
            0x7ff0_0000_0000_0001, 0xfff0_0000_0000_0001,
            0x7ff8_0000_0000_0042, 0xfff8_0000_0000_0042, u64::MAX,
        ] {
            let values = [SlotValue::F64Bits(bits)];
            let reader = Fixture::new(&values, &[], &[]);
            for required in [false, true] {
                let spec = SlotSpec { required, ..spec(SlotKind::F64) };
                assert_error(decode_from::<f64>(&reader, &spec), ErrorCode::InvalidNumber, spec.field_id);
                assert_error(validate_from(&reader, 1, &[spec]), ErrorCode::InvalidNumber, spec.field_id);
            }
        }
    }

    #[test]
    fn decimals_preserve_all_scale_and_coefficient_extremes() {
        for coefficient in [i128::MIN, -100, -1, 0, 1, 100, i128::MAX] {
            for scale in [0, 1, 8, 38, u8::MAX] {
                let decimal = Decimal128::new(coefficient, scale);
                let values = [SlotValue::Decimal(decimal)];
                let reader = Fixture::new(&values, &[], &[]);
                let spec = spec(SlotKind::Decimal);
                assert_eq!(decode_from::<Decimal128>(&reader, &spec).unwrap(), Some(decimal));
                validate_from(&reader, 1, &[spec]).unwrap();
            }
        }
    }

    #[test]
    fn strings_borrow_the_selected_owner_with_scalar_counts() {
        let payload = "xé🦀e\u{301}\0z".as_bytes();
        let decoded = "y界🙂a\u{308}\0q".as_bytes();
        for (source, owner, expected) in [
            (SpanSource::RawPayload, payload, "é🦀e\u{301}\0"),
            (SpanSource::DecodedArena, decoded, "界🙂a\u{308}\0"),
        ] {
            let reference = StrRef {
                span: Span32::new(1, u32::try_from(expected.len()).unwrap()),
                source, scalar_len: 5,
            };
            let values = [SlotValue::Str(reference)];
            let text = {
                let reader = Fixture::new(&values, payload, decoded);
                let spec = spec(SlotKind::Str);
                validate_from(&reader, 1, &[spec]).unwrap();
                let text = decode_from::<&str>(&reader, &spec).unwrap().unwrap();
                assert_eq!(reader.resolutions.get(), 2);
                text
            };
            assert_eq!(text, expected);
            assert_eq!(text.as_ptr(), owner[1..].as_ptr());
            // The returned borrow outlives the temporary reader above.
            assert_eq!(text.chars().count(), 5);
        }
    }

    #[test]
    fn empty_present_borrows_are_distinct_from_missing_values() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for offset in [0, 3] {
                let values = [SlotValue::Str(StrRef {
                    span: Span32::new(offset, 0), source, scalar_len: 0,
                })];
                let reader = Fixture::new(&values, b"raw", b"dec");
                assert_eq!(decode_from::<&str>(&reader, &spec(SlotKind::Str)).unwrap(), Some(""));
            }
        }
        for payload in [&b""[..], &b"raw"[..]] {
            let values = [SlotValue::RawSpan(Span32::new(u32::try_from(payload.len()).unwrap(), 0))];
            let reader = Fixture::new(&values, payload, &[]);
            assert_eq!(decode_from::<&[u8]>(&reader, &spec(SlotKind::Raw)).unwrap(), Some(&b""[..]));
        }
    }

    #[test]
    fn invalid_string_ranges_utf8_and_scalar_counts_propagate() {
        for source in [SpanSource::RawPayload, SpanSource::DecodedArena] {
            for (span, scalar_len, code) in [
                (Span32::new(4, 0), 0, ErrorCode::SchemaImageInvalid),
                (Span32::new(2, 2), 1, ErrorCode::SchemaImageInvalid),
                (Span32::new(u32::MAX, 1), 0, ErrorCode::SchemaImageInvalid),
                (Span32::new(1, u32::MAX), 0, ErrorCode::SchemaImageInvalid),
                (Span32::new(u32::MAX, 0), 0, ErrorCode::SchemaImageInvalid),
                (Span32::new(1, 1), 1, ErrorCode::InvalidUtf8),
                (Span32::new(2, 1), 1, ErrorCode::InvalidUtf8),
                (Span32::new(0, 3), 3, ErrorCode::SchemaImageInvalid),
                (Span32::new(0, 3), u32::MAX, ErrorCode::SchemaImageInvalid),
                (Span32::new(3, 0), 1, ErrorCode::SchemaImageInvalid),
            ] {
                let values = [SlotValue::Str(StrRef { span, source, scalar_len })];
                let reader = Fixture::new(&values, "aé".as_bytes(), "aé".as_bytes());
                let spec = spec(SlotKind::Str);
                let error = assert_error(decode_from::<&str>(&reader, &spec), code, spec.field_id);
                if code == ErrorCode::InvalidUtf8 {
                    assert_eq!(error.byte_offset, span.offset);
                }
                assert_error(validate_from(&reader, 1, &[spec]), code, spec.field_id);
            }
        }
        // Equal offsets must still resolve against the designated buffer.
        let values = [SlotValue::Str(StrRef {
            span: Span32::new(0, 2), source: SpanSource::DecodedArena, scalar_len: 2,
        })];
        let reader = Fixture::new(&values, b"ok", b"\xff\xff");
        assert_error(decode_from::<&str>(&reader, &spec(SlotKind::Str)), ErrorCode::InvalidUtf8, FieldId(u32::MAX));
    }

    #[test]
    fn raw_access_only_checks_payload_bounds() {
        let payload = b"x\xffnot JSON\0";
        for span in [Span32::new(0, 0), Span32::new(0, 11), Span32::new(1, 10), Span32::new(11, 0)] {
            let values = [SlotValue::RawSpan(span)];
            let reader = Fixture::new(&values, payload, b"unrelated decoded bytes");
            let spec = spec(SlotKind::Raw);
            let bytes = decode_from::<&[u8]>(&reader, &spec).unwrap().unwrap();
            let expected = span.slice(payload).unwrap();
            assert_eq!(bytes, expected);
            assert_eq!(bytes.as_ptr(), expected.as_ptr());
            validate_from(&reader, 1, &[spec]).unwrap();
        }
        for span in [
            Span32::new(12, 0), Span32::new(11, 1), Span32::new(0, 12),
            Span32::new(u32::MAX, 0), Span32::new(u32::MAX, 1),
            Span32::new(1, u32::MAX),
        ] {
            let values = [SlotValue::RawSpan(span)];
            let reader = Fixture::new(&values, payload, b"a much longer decoded arena");
            let spec = spec(SlotKind::Raw);
            assert_error(decode_from::<&[u8]>(&reader, &spec), ErrorCode::SchemaImageInvalid, spec.field_id);
            assert_error(validate_from(&reader, 1, &[spec]), ErrorCode::SchemaImageInvalid, spec.field_id);
        }
    }

    #[test]
    fn resolver_errors_keep_offset_and_detail_and_gain_field_context() {
        let original = Error::new(
            ErrorCode::InvalidUtf8, ErrorSource::Decode, 97, "resolver diagnostic",
        );
        let values = [
            SlotValue::Str(StrRef {
                span: Span32::new(0, 1), source: SpanSource::DecodedArena, scalar_len: 1,
            }),
            SlotValue::RawSpan(Span32::new(0, 1)),
        ];
        let mut reader = Fixture::new(&values, b"x", b"y");
        reader.resolver_error = Some(original.clone());
        let string = SlotSpec { field_id: FieldId(65_536), ..spec(SlotKind::Str) };
        let raw = SlotSpec { index: 1, field_id: FieldId(u32::MAX), ..spec(SlotKind::Raw) };
        assert_eq!(decode_from::<&str>(&reader, &string).unwrap_err(), original.clone().with_schema(0, string.field_id.0));
        assert_eq!(decode_from::<&[u8]>(&reader, &raw).unwrap_err(), original.with_schema(0, raw.field_id.0));
        assert_eq!(reader.resolutions.get(), 2);
    }

    #[test]
    fn complete_plan_accepts_reordered_slots_and_full_width_field_ids() {
        let values = [
            SlotValue::Bool(false), SlotValue::U64(u64::MAX), SlotValue::I64(i64::MIN),
            SlotValue::F64Bits((-0.0f64).to_bits()),
            SlotValue::Decimal(Decimal128::new(i128::MIN, u8::MAX)),
            SlotValue::Str(StrRef {
                span: Span32::new(0, 2), source: SpanSource::DecodedArena, scalar_len: 1,
            }),
            SlotValue::RawSpan(Span32::new(0, 1)), SlotValue::Empty,
        ];
        let reader = Fixture::new(&values, b"!", "é".as_bytes());
        let mut specs = Vec::new();
        for (index, kind) in KINDS.into_iter().enumerate() {
            let index = u32::try_from(index).unwrap();
            specs.push(SlotSpec { index, field_id: FieldId(u32::MAX - index), kind, required: true });
        }
        specs.push(SlotSpec { index: 7, field_id: FieldId(0), kind: SlotKind::Raw, required: false });
        specs.reverse();
        let original = specs.clone();
        for _ in 0..2 {
            validate_from(&reader, values.len(), &specs).unwrap();
        }
        assert_eq!(specs, original);
        assert_eq!(reader.reads.get(), 16);
        assert_eq!(reader.resolutions.get(), 4);
        validate_from(&Fixture::new(&[], &[], &[]), 0, &[]).unwrap();
    }

    #[test]
    fn plan_rejects_count_gaps_duplicates_and_out_of_range_indices() {
        let base = spec(SlotKind::Bool);
        let second = SlotSpec { index: 1, field_id: FieldId(0), ..base };
        let values = [SlotValue::Empty, SlotValue::Empty];
        let reader = Fixture::new(&values, &[], &[]);
        for specs in [
            vec![], vec![base], vec![base, second, base],
            vec![base, SlotSpec { index: 0, ..second }],
            vec![base, SlotSpec { index: 2, ..second }],
            vec![base, SlotSpec { index: u32::MAX, ..second }],
            vec![SlotSpec { index: 1, ..base }, SlotSpec { index: 2, ..second }],
            vec![base, SlotSpec { field_id: base.field_id, ..second }],
        ] {
            let error = validate_from(&reader, 2, &specs).unwrap_err();
            assert_eq!(error.code, ErrorCode::SchemaImageInvalid);
            assert_eq!(error.source, ErrorSource::Decode);
            assert_eq!(error.schema_id, 0);
            assert_eq!(reader.reads.get(), 0);
        }
        assert_error(validate_shape(0, &[base]), ErrorCode::SchemaImageInvalid, FieldId(0));
        assert_error(validate_shape(usize::MAX, &[]), ErrorCode::SchemaImageInvalid, FieldId(0));
        let duplicate = SlotSpec { field_id: base.field_id, ..second };
        assert_error(validate_shape(2, &[base, duplicate]), ErrorCode::SchemaImageInvalid, base.field_id);
    }

    #[test]
    fn plan_checks_every_required_optional_and_present_value_in_order() {
        for kind in KINDS {
            let values = [SlotValue::Empty];
            let reader = Fixture::new(&values, &[], &[]);
            let required = spec(kind);
            let optional = SlotSpec { required: false, ..required };
            validate_from(&reader, 1, &[optional]).unwrap();
            assert_error(validate_from(&reader, 1, &[required]), ErrorCode::MissingRequiredField, required.field_id);
        }
        let values = [SlotValue::Empty, SlotValue::Bool(true), SlotValue::F64Bits(f64::INFINITY.to_bits())];
        let reader = Fixture::new(&values, &[], &[]);
        let missing = SlotSpec { index: 0, field_id: FieldId(10), ..spec(SlotKind::Bool) };
        let wrong = SlotSpec { index: 1, field_id: FieldId(20), required: false, ..spec(SlotKind::U64) };
        let infinite = SlotSpec { index: 2, field_id: FieldId(30), required: false, ..spec(SlotKind::F64) };
        assert_error(validate_from(&reader, 3, &[missing, wrong, infinite]), ErrorCode::MissingRequiredField, missing.field_id);
        assert_error(validate_from(&reader, 3, &[wrong, infinite, missing]), ErrorCode::TypeMismatch, wrong.field_id);
        assert_error(validate_from(&reader, 3, &[infinite, missing, wrong]), ErrorCode::InvalidNumber, infinite.field_id);
        let all_optional = [SlotSpec { required: false, ..missing }, wrong, infinite];
        assert_error(validate_from(&reader, 3, &all_optional), ErrorCode::TypeMismatch, wrong.field_id);
    }

    #[test]
    fn spec_kind_errors_precede_present_values_and_invalid_indices() {
        let values = [SlotValue::Bool(true)];
        let reader = Fixture::new(&values, &[], &[]);
        for index in [0, u32::MAX] {
            let wrong = SlotSpec { index, ..spec(SlotKind::I64) };
            let error = assert_error(
                decode_from::<bool>(&reader, &wrong),
                ErrorCode::SchemaImageInvalid, wrong.field_id,
            );
            assert_eq!(error.detail, "slot specification has the wrong decoder kind");
        }
        assert_eq!(reader.reads.get(), 0);
    }

    #[test]
    fn dense_plan_validation_handles_many_unsorted_fields() {
        let values = vec![SlotValue::Bool(true); 4096];
        let reader = Fixture::new(&values, &[], &[]);
        let mut specs: Vec<_> = (0u32..4096).rev().map(|index| SlotSpec {
            index, field_id: FieldId(u32::MAX - index), kind: SlotKind::Bool, required: true,
        }).collect();
        validate_from(&reader, values.len(), &specs).unwrap();
        assert_eq!(reader.reads.get(), values.len());
        specs[2048].field_id = specs[2047].field_id;
        assert_error(validate_from(&reader, values.len(), &specs), ErrorCode::SchemaImageInvalid, specs[2048].field_id);
        assert_eq!(reader.reads.get(), values.len());
    }

    #[test]
    fn public_function_signatures_match_the_frozen_contract() {
        let _: fn(&ValidatedSlots, &DecodePlan<'_>) -> Result<()> = validate_slots;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<bool>> = decode_bool;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<u64>> = decode_u64;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<u32>> = decode_u32;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<i64>> = decode_i64;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<i32>> = decode_i32;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<f64>> = decode_f64;
        let _: fn(&SlotReader<'_>, &SlotSpec) -> Result<Option<Decimal128>> = decode_decimal;
        let _: for<'a> fn(&SlotReader<'a>, &SlotSpec) -> Result<Option<&'a str>> = decode_str;
        let _: for<'a> fn(&SlotReader<'a>, &SlotSpec) -> Result<Option<&'a [u8]>> = decode_raw;
    }
}
