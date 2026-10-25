//! Reusable decoding state: limits, effective numeric policy, scratch and the
//! lazily built reference program.

use crate::{
    Error, ErrorCode, ErrorSource, NativeResult, NumericPolicy, Result, ValidatedProgram, VmLimits,
};
use alloc::vec::Vec;
use core::any::TypeId;
use jmax_syntax::SkipScratch;

#[derive(Clone, Copy, Debug)]
struct UnknownEntry {
    hash: u64,
    start: u32,
    len: u32,
}

/// Scratch and policy for one decoder at a time. Debug is not derived from the
/// program: only its presence is reported.
pub struct NativeContext {
    pub(crate) limits: VmLimits,
    /// The caller's numeric policy, passed to the VM unchanged.
    pub(crate) requested: NumericPolicy,
    /// The VM's combination of the request and the program's schema policy,
    /// which governs number tokens.
    pub(crate) numeric: NumericPolicy,
    /// The program's own policy, which governs numbers inside strings.
    pub(crate) schema_numeric: NumericPolicy,
    pub(crate) arena: Vec<u8>,
    pub(crate) skip_decoded: Vec<u8>,
    pub(crate) skip: SkipScratch,
    pub(crate) unknown: UnknownKeys,
    pub(crate) program: Option<ValidatedProgram>,
    pub(crate) program_type: Option<TypeId>,
    /// Largest input length judged safe by `budget_safe`, if any.
    pub(crate) safe_max_len: Option<usize>,
    /// Decimal scalars read since the last `core_decode` began.
    pub(crate) decimals: u64,
    /// Unknown object fields encountered since the last `core_decode` began.
    pub(crate) skipped_fields: u64,
    /// Enum membership checks since the last `core_decode` began.
    pub(crate) enum_values: u64,
    /// Decoded bytes and entries of all unknown keys seen in this decode.
    pub(crate) unknown_key_bytes: u64,
    pub(crate) unknown_fields: u64,
    /// Total disjoint bytes consumed through Reader::skip_value.
    pub(crate) skipped_bytes: u64,
    /// The program's format options, as the schema VM applies them.
    pub(crate) format_options: jmax_format::FormatOptions,
}

impl core::fmt::Debug for NativeContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NativeContext")
            .field("limits", &self.limits)
            .field("numeric", &self.numeric)
            .field("has_program", &self.program.is_some())
            .finish()
    }
}

impl NativeContext {
    pub fn new(limits: VmLimits, numeric: NumericPolicy) -> Self {
        Self {
            limits,
            requested: numeric,
            numeric,
            schema_numeric: numeric,
            arena: Vec::new(),
            skip_decoded: Vec::new(),
            skip: SkipScratch::default(),
            unknown: UnknownKeys::default(),
            program: None,
            program_type: None,
            safe_max_len: None,
            decimals: 0,
            skipped_fields: 0,
            enum_values: 0,
            unknown_key_bytes: 0,
            unknown_fields: 0,
            skipped_bytes: 0,
            format_options: jmax_format::FormatOptions::default(),
        }
    }

    /// A context whose policies and safe length are derived from `program`, for
    /// callers that drive a `Reader` directly. `decode_into` reinstalls the
    /// program of the decoder it is given.
    pub fn with_program(
        program: &ValidatedProgram,
        limits: VmLimits,
        numeric: NumericPolicy,
    ) -> Result<Self> {
        let mut context = Self::new(limits, numeric);
        context.install(program.clone(), TypeId::of::<()>())?;
        context.program_type = None;
        Ok(context)
    }

    pub const fn limits(&self) -> VmLimits {
        self.limits
    }

    /// The effective policy for number tokens.
    pub const fn numeric(&self) -> NumericPolicy {
        self.numeric
    }

    /// Install the reference program and derive the policies and the safe
    /// length that depend on it, exactly as the schema VM combines them.
    pub(crate) fn install(&mut self, program: ValidatedProgram, type_id: TypeId) -> Result<()> {
        let schema = program.program().policy.numeric;
        if self.requested.max_exponent_abs < 0 || schema.max_exponent_abs < 0 {
            return Err(Error::new(
                ErrorCode::ConstraintViolation,
                ErrorSource::SchemaVm,
                0,
                "negative numeric exponent limit",
            ));
        }
        self.numeric = NumericPolicy {
            allow_Quoted_int: self.requested.allow_Quoted_int && schema.allow_Quoted_int,
            reject_non_finite_float: self.requested.reject_non_finite_float
                || schema.reject_non_finite_float,
            reject_negative_zero: self.requested.reject_negative_zero
                || schema.reject_negative_zero,
            max_exponent_abs: self.requested.max_exponent_abs.min(schema.max_exponent_abs),
        };
        self.schema_numeric = schema;
        self.format_options = program.program().format_options;
        self.safe_max_len = largest_safe_len(&program, self.limits);
        self.program = Some(program);
        self.program_type = Some(type_id);
        Ok(())
    }

    /// Open an object scope for unknown-key tracking; pass the mark to `end_object`.
    pub fn begin_object(&self) -> usize {
        let _t = jmax_trace::span!(NATIVE_READER_OPEN);
        self.unknown.entries.len()
    }

    /// Record a decoded unknown key of the object opened at `mark`.
    pub fn unknown_key(&mut self, key: &str) -> NativeResult<()> {
        let _t = jmax_trace::span!(NATIVE_KEY_DISPATCH);
        self.unknown.push_key(key)?;
        self.unknown_key_bytes = self
            .unknown_key_bytes
            .checked_add(key.len() as u64)
            .ok_or(crate::Fail)?;
        self.unknown_fields = self.unknown_fields.checked_add(1).ok_or(crate::Fail)?;
        Ok(())
    }

    /// Record an enum membership check before reading its string or integer.
    /// Decoders opting into `NativeDecode::COUNTS_ENUM_VALUES` must call this
    /// for every enum value, including enums inside arrays and nested objects.
    #[inline]
    pub fn record_enum_value(&mut self) -> NativeResult<()> {
        self.enum_values = self.enum_values.checked_add(1).ok_or(crate::Fail)?;
        Ok(())
    }

    /// Record one successfully decoded unknown field. Nested skipped-value
    /// work is bounded by the input bytes; this count bounds per-field table
    /// and duplicate-tracking work.
    pub(crate) fn record_skipped_field(&mut self) -> NativeResult<()> {
        self.skipped_fields = self.skipped_fields.checked_add(1).ok_or(crate::Fail)?;
        Ok(())
    }

    /// Close the object opened at `mark`. With duplicate rejection on, two equal
    /// decoded unknown keys in it are a Fail. Always discards the object's keys.
    pub fn end_object(&mut self, mark: usize) -> NativeResult<()> {
        let _t = jmax_trace::span!(NATIVE_READER_OPEN);
        self.unknown
            .finish(mark, self.limits.syntax.reject_duplicate_keys)
    }
}

/// The unknown keys of every open object, innermost last.
#[derive(Default)]
pub(crate) struct UnknownKeys {
    entries: Vec<UnknownEntry>,
    bytes: Vec<u8>,
}

impl UnknownKeys {
    /// Forget every key, e.g. after a decode that failed inside an object.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes.clear();
    }

    pub(crate) fn push_key(&mut self, key: &str) -> NativeResult<()> {
        self.push_key_bytes(key.as_bytes())
    }

    pub(crate) fn push_key_bytes(&mut self, key: &[u8]) -> NativeResult<()> {
        let start = u32::try_from(self.bytes.len()).map_err(|_| crate::Fail)?;
        let len = u32::try_from(key.len()).map_err(|_| crate::Fail)?;
        self.bytes.extend_from_slice(key);
        self.entries.push(UnknownEntry {
            hash: key_hash(key),
            start,
            len,
        });
        Ok(())
    }

    fn finish(&mut self, mark: usize, reject_duplicates: bool) -> NativeResult<()> {
        let base = self.entries.get(mark).map(|entry| entry.start as usize);
        let result = if reject_duplicates {
            let bytes = &self.bytes;
            let slice = self.entries.get_mut(mark..).ok_or(crate::Fail)?;
            slice.sort_unstable_by_key(|entry| entry.hash);
            let key = |entry: &UnknownEntry| {
                bytes.get(entry.start as usize..(entry.start + entry.len) as usize)
            };
            let mut duplicate = false;
            for (index, entry) in slice.iter().enumerate() {
                // Equal hashes are adjacent; compare each against its whole run.
                for other in slice[index + 1..]
                    .iter()
                    .take_while(|other| other.hash == entry.hash)
                {
                    duplicate |= key(entry) == key(other);
                }
            }
            if duplicate { Err(crate::Fail) } else { Ok(()) }
        } else {
            Ok(())
        };
        if let Some(base) = base {
            self.bytes.truncate(base);
        }
        self.entries.truncate(mark);
        result
    }
}

fn key_hash(bytes: &[u8]) -> u64 {
    // Every byte takes part, a word at a time; equal keys hash equal and the
    // duplicate check still compares bytes, so only the spread matters.
    const K: u64 = 0x9e37_79b9_7f4a_7c15;
    let n = bytes.len();
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default());
    let mut hash = (n as u64).wrapping_mul(K);
    if n >= 8 {
        let mut at = 0;
        while at + 8 < n {
            hash = (hash ^ word(at)).wrapping_mul(K);
            hash ^= hash >> 32;
            at += 8;
        }
        hash = (hash ^ word(n - 8)).wrapping_mul(K);
    } else {
        let small = if n >= 4 {
            let low = u32::from_le_bytes(bytes[..4].try_into().unwrap_or_default());
            let high = u32::from_le_bytes(bytes[n - 4..].try_into().unwrap_or_default());
            u64::from(low) | u64::from(high) << 32
        } else if n > 0 {
            u64::from(bytes[0]) | u64::from(bytes[n / 2]) << 8 | u64::from(bytes[n - 1]) << 16
        } else {
            0
        };
        hash = (hash ^ small).wrapping_mul(K);
    }
    hash ^ (hash >> 29)
}

/// The bound grows with the length, so the safe lengths form a prefix.
fn largest_safe_len(program: &ValidatedProgram, limits: VmLimits) -> Option<usize> {
    let ceiling = limits
        .syntax
        .max_payload_bytes
        .min(limits.syntax.max_decoded_bytes) as usize;
    if !jmax_schema_vm::budget_safe(program, 0, limits) {
        return None;
    }
    let (mut low, mut high) = (0usize, ceiling);
    while low < high {
        let mid = low + (high - low).div_ceil(2);
        if jmax_schema_vm::budget_safe(program, mid, limits) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(low)
}
