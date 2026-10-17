//! Versioned, deterministic images with a canonical-schema validation witness.
//!
//! The wire codec never uses native layout. Counting, writing and comparison
//! share one serializer, so every field participates in both identity and
//! validation. Untrusted graphs are reconstructed iteratively and re-lowered;
//! neither a matching fingerprint nor structurally plausible code is a proof.

use super::*;
use jmax_abi::number::{Decimal128, DecimalPolicy, NumericPolicy};
use jmax_format::{HexCase, HexPolicy, Padding};
use jmax_schema_ir::{
    ArrayShape, Dialect, ObjectShape, Property, ResolveLimits, Schema, TypeNode,
    UnknownFormatPolicy,
};

fn invalid(detail: &'static str) -> Error {
    compile_error(ErrorCode::SchemaImageInvalid, detail)
}

fn budget() -> Error {
    compile_error(ErrorCode::BudgetExhausted, "image resource limit exceeded")
}

fn count(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| budget())
}

fn index(value: u32) -> Result<usize> {
    usize::try_from(value).map_err(|_| budget())
}

fn limited(value: usize, limit: u32) -> Result<()> {
    if count(value)? > limit {
        Err(budget())
    } else {
        Ok(())
    }
}

fn reserved<T>(len: usize) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(len).map_err(|_| budget())?;
    Ok(values)
}

/// Schema errors in a purported image are image errors. Resource limits retain
/// their distinct code, including failures in the IR and pattern compilers.
fn image_error(mut error: Error) -> Error {
    if error.code != ErrorCode::BudgetExhausted {
        error.code = ErrorCode::SchemaImageInvalid;
    }
    error.source = ErrorSource::SchemaCompile;
    error.byte_offset = 0;
    error
}

enum Destination<'a> {
    Count,
    Bytes(Vec<u8>),
    Compare(&'a [u8]),
}

struct Writer<'a> {
    destination: Destination<'a>,
    position: usize,
}

impl Writer<'_> {
    fn bytes(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.position.checked_add(bytes.len()).ok_or_else(budget)?;
        count(end)?;
        match &mut self.destination {
            Destination::Count => {}
            Destination::Bytes(output) => {
                output.try_reserve(bytes.len()).map_err(|_| budget())?;
                output.extend_from_slice(bytes);
            }
            Destination::Compare(expected) => {
                if expected.get(self.position..end) != Some(bytes) {
                    return Err(invalid("image differs from canonical lowering"));
                }
            }
        }
        self.position = end;
        Ok(())
    }

    fn u8(&mut self, value: u8) -> Result<()> {
        self.bytes(&[value])
    }
    fn u16(&mut self, value: u16) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn u32(&mut self, value: u32) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn u64(&mut self, value: u64) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn i16(&mut self, value: i16) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn i64(&mut self, value: i64) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn i128(&mut self, value: i128) -> Result<()> {
        self.bytes(&value.to_le_bytes())
    }
    fn boolean(&mut self, value: bool) -> Result<()> {
        self.u8(u8::from(value))
    }
    fn length(&mut self, value: usize) -> Result<()> {
        self.u32(count(value)?)
    }

    fn blob(&mut self, value: &[u8]) -> Result<()> {
        self.length(value.len())?;
        self.bytes(value)
    }

    fn optional<T>(
        &mut self,
        value: Option<&T>,
        write: impl FnOnce(&mut Self, &T) -> Result<()>,
    ) -> Result<()> {
        self.boolean(value.is_some())?;
        if let Some(value) = value {
            write(self, value)?;
        }
        Ok(())
    }

    fn span(&mut self, span: Span32) -> Result<()> {
        self.u32(span.offset)?;
        self.u32(span.len)
    }

    fn decimal_value(&mut self, value: &Decimal128) -> Result<()> {
        self.i128(value.coefficient)?;
        self.u8(value.scale)
    }

    fn decimal(&mut self, value: &DecimalConstraints) -> Result<()> {
        self.u8(value.policy.max_precision)?;
        self.u8(value.policy.target_scale)?;
        self.boolean(value.policy.allow_exponent)?;
        self.boolean(value.policy.reject_inexact_rescale)?;
        self.i128(value.policy.tick_step)?;
        self.optional(value.minimum.as_ref(), Self::decimal_value)?;
        self.optional(value.maximum.as_ref(), Self::decimal_value)
    }

    fn policy(&mut self, value: &SchemaPolicy) -> Result<()> {
        self.u8(match value.dialect {
            Dialect::Endpoint => 0,
            Dialect::JsonSchema202012 => 1,
        })?;
        self.boolean(value.numeric.allow_Quoted_int)?;
        self.boolean(value.numeric.reject_non_finite_float)?;
        self.boolean(value.numeric.reject_negative_zero)?;
        self.i16(value.numeric.max_exponent_abs)?;
        self.u8(match value.unknown_formats {
            UnknownFormatPolicy::Reject => 0,
            UnknownFormatPolicy::Annotate => 1,
        })?;
        self.boolean(value.allow_recursive)?;
        self.u32(value.max_pattern_bytes)
    }

    fn format_options(&mut self, value: &FormatOptions) -> Result<()> {
        self.u32(value.max_input_bytes)?;
        self.u8(value.nanoid_len)?;
        self.u8(match value.hex.case {
            HexCase::Any => 0,
            HexCase::Lower => 1,
            HexCase::Upper => 2,
        })?;
        self.boolean(value.hex.prefix)?;
        self.optional(value.hex.exact_bytes.as_ref(), |w, n| w.u32(*n))?;
        self.u8(match value.base64_padding {
            Padding::Required => 0,
            Padding::Forbidden => 1,
            Padding::Optional => 2,
        })
    }

    fn constraints(&mut self, value: &ConstraintSet) -> Result<()> {
        self.optional(value.min_i64.as_ref(), |w, n| w.i64(*n))?;
        self.optional(value.max_i64.as_ref(), |w, n| w.i64(*n))?;
        self.optional(value.decimal.as_ref(), Self::decimal)?;
        self.optional(value.min_count.as_ref(), |w, n| w.u32(*n))?;
        self.optional(value.max_count.as_ref(), |w, n| w.u32(*n))?;
        self.optional(value.pattern.as_ref(), |w, n| w.u32(*n))?;
        self.optional(value.format.as_ref(), |w, n| w.u16(*n as u16))?;
        self.boolean(value.nullable)
    }

    fn state(&mut self, state: &ProgramState) -> Result<()> {
        self.u32(state.id.0)?;
        match &state.kind {
            StateKind::Any => self.u8(0)?,
            StateKind::Scalar(kind) => {
                self.u8(1)?;
                self.u8(value_tag(*kind))?;
            }
            StateKind::Object {
                key_table,
                required_count,
                reject_unknown,
            } => {
                self.u8(2)?;
                self.u32(*key_table)?;
                self.u16(*required_count)?;
                self.boolean(*reject_unknown)?;
            }
            StateKind::Array { items } => {
                self.u8(3)?;
                self.u32(items.0)?;
            }
        }
        self.span(state.insns)?;
        self.constraints(&state.constraints)
    }

    fn pattern(&mut self, pattern: &PatternProgram) -> Result<()> {
        self.u16(pattern.version)?;
        self.u32(pattern.start)?;
        self.length(pattern.ops.len())?;
        for op in &pattern.ops {
            match op {
                PatternOp::Char { scalar, next } => {
                    self.u8(0)?;
                    self.u32(*scalar)?;
                    self.u32(*next)?;
                }
                PatternOp::Class {
                    ranges,
                    negated,
                    next,
                } => {
                    self.u8(1)?;
                    self.length(ranges.len())?;
                    for &(first, last) in ranges {
                        self.u32(first)?;
                        self.u32(last)?;
                    }
                    self.boolean(*negated)?;
                    self.u32(*next)?;
                }
                PatternOp::Any { next } => {
                    self.u8(2)?;
                    self.u32(*next)?;
                }
                PatternOp::Split { first, second } => {
                    self.u8(3)?;
                    self.u32(*first)?;
                    self.u32(*second)?;
                }
                PatternOp::Jump { next } => {
                    self.u8(4)?;
                    self.u32(*next)?;
                }
                PatternOp::AssertStart { next } => {
                    self.u8(5)?;
                    self.u32(*next)?;
                }
                PatternOp::AssertEnd { next } => {
                    self.u8(6)?;
                    self.u32(*next)?;
                }
                PatternOp::Accept => self.u8(7)?,
            }
        }
        self.u32(pattern.max_steps)
    }
}

fn value_tag(kind: ValueKind) -> u8 {
    match kind {
        ValueKind::Null => 0,
        ValueKind::Boolean => 1,
        ValueKind::Integer => 2,
        ValueKind::Number => 3,
        ValueKind::Decimal => 4,
        ValueKind::String => 5,
    }
}

/// This private serializer deliberately accepts unwitnessed programs. It only
/// encodes fields and performs checked size arithmetic, never issues a proof.
fn serialize(program: &CompiledProgram, writer: &mut Writer<'_>) -> Result<()> {
    let header = &program.image.header;
    writer.u32(header.magic)?;
    writer.u16(header.version)?;
    writer.u16(header.flags)?;
    writer.u32(header.schema_id)?;
    writer.u32(header.root_state)?;
    writer.u32(header.insn_count)?;
    writer.u32(header.key_count)?;
    writer.u32(header.slot_count)?;
    writer.u64(header.fingerprint)?;
    writer.length(program.image.insns.len())?;
    for insn in &program.image.insns {
        writer.u8(insn.opcode)?;
        writer.u8(insn.flags)?;
        writer.u16(insn.field_id)?;
        writer.u32(insn.arg0)?;
        writer.u64(insn.arg1)?;
    }
    writer.length(program.image.keys.len())?;
    for key in &program.image.keys {
        writer.u64(key.swar_head)?;
        writer.span(key.span)?;
        writer.u16(key.field_id)?;
        writer.u16(key.required_bit)?;
        writer.u32(key.target_state)?;
    }
    writer.blob(&program.image.key_bytes)?;
    writer.length(program.states.len())?;
    for state in &program.states {
        writer.state(state)?;
    }
    writer.length(program.key_tables.len())?;
    for table in &program.key_tables {
        writer.u32(table.first_key)?;
        writer.u32(table.key_count)?;
        match &table.strategy {
            KeyStrategy::Swar => writer.u8(0)?,
            KeyStrategy::PerfectHash { seed, slots } => {
                writer.u8(1)?;
                writer.u64(*seed)?;
                writer.length(slots.len())?;
                for &slot in slots {
                    writer.u32(slot)?;
                }
            }
            KeyStrategy::BucketedPerfectHash {
                seed,
                buckets,
                slots,
            } => {
                writer.u8(2)?;
                writer.u64(*seed)?;
                writer.length(buckets.len())?;
                for bucket in buckets {
                    writer.u64(bucket.seed)?;
                    writer.u32(bucket.first_slot)?;
                    writer.u32(bucket.slot_count)?;
                }
                writer.length(slots.len())?;
                for &slot in slots {
                    writer.u32(slot)?;
                }
            }
        }
    }
    writer.length(program.patterns.len())?;
    for pattern in &program.patterns {
        writer.pattern(pattern)?;
    }
    writer.length(program.slots.len())?;
    for slot in &program.slots {
        writer.u32(slot.slot)?;
        writer.u32(slot.field_id.0)?;
        writer.u32(slot.state.0)?;
    }
    writer.policy(&program.policy)?;
    writer.format_options(&program.format_options)?;
    writer.blob(&program.canonical_schema)
}

/// Compute the exact wire length without validation, allocation, or lowering.
pub fn encoded_len(program: &CompiledProgram) -> Result<usize> {
    let mut writer = Writer {
        destination: Destination::Count,
        position: 0,
    };
    serialize(program, &mut writer).map_err(|mut error| {
        error.schema_id = program.image.header.schema_id;
        error
    })?;
    Ok(writer.position)
}

fn encode_raw(program: &CompiledProgram, max_bytes: u32) -> Result<Vec<u8>> {
    let len = encoded_len(program)?;
    limited(len, max_bytes)?;
    let mut writer = Writer {
        destination: Destination::Bytes(reserved(len)?),
        position: 0,
    };
    serialize(program, &mut writer)?;
    match writer.destination {
        Destination::Bytes(bytes) => Ok(bytes),
        _ => Err(invalid("invalid internal serializer destination")),
    }
}

/// Serialize a witnessed program, enforcing the caller's inclusive byte limit.
pub fn encode(program: &ValidatedProgram, max_bytes: u32) -> Result<Vec<u8>> {
    encode_raw(program.program(), max_bytes).map_err(|mut error| {
        error.schema_id = program.image().header.schema_id;
        error
    })
}

/// The complete wire bytes disambiguate fingerprint collisions and options.
pub fn cache_key(program: &ValidatedProgram, max_bytes: u32) -> Result<CacheKey> {
    Ok(CacheKey {
        fingerprint: program.image().header.fingerprint,
        image_version: IMAGE_VERSION,
        bytes: encode(program, max_bytes)?,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
    schema_id: u32,
    field_id: u32,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            position: 0,
            schema_id: 0,
            field_id: 0,
        }
    }

    fn error_at(&self, position: usize, detail: &'static str) -> Error {
        Error::new(
            ErrorCode::SchemaImageInvalid,
            ErrorSource::SchemaCompile,
            u32::try_from(position).unwrap_or(u32::MAX),
            detail,
        )
        .with_schema(self.schema_id, self.field_id)
    }

    fn error(&self, detail: &'static str) -> Error {
        self.error_at(self.position, detail)
    }

    fn budget_at(&self, position: usize) -> Error {
        let mut error = self.error_at(position, "image resource limit exceeded");
        error.code = ErrorCode::BudgetExhausted;
        error
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(len).ok_or_else(budget)?;
        let bytes = self
            .bytes
            .get(self.position..end)
            .ok_or_else(|| self.error("truncated image record"))?;
        self.position = end;
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(u8::from_le_bytes(self.fixed()?))
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.fixed()?))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.fixed()?))
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.fixed()?))
    }
    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.fixed()?))
    }
    fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.fixed()?))
    }
    fn i128(&mut self) -> Result<i128> {
        Ok(i128::from_le_bytes(self.fixed()?))
    }

    fn boolean(&mut self) -> Result<bool> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(self.error_at(position, "noncanonical boolean or option tag")),
        }
    }

    fn optional<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if self.boolean()? {
            Ok(Some(read(self)?))
        } else {
            Ok(None)
        }
    }

    /// Check the minimum possible wire footprint and caller limit before any
    /// allocation. Each variable-size element is checked again while reading.
    fn length(&mut self, minimum: usize, maximum: u32) -> Result<usize> {
        let position = self.position;
        let len = self.u32()?;
        let size = index(len)?;
        let required = size
            .checked_mul(minimum)
            .ok_or_else(|| self.budget_at(position))?;
        if required > self.bytes.len().saturating_sub(self.position) {
            return Err(self.error_at(position, "element count exceeds remaining bytes"));
        }
        if len > maximum {
            return Err(self.budget_at(position));
        }
        Ok(size)
    }

    fn vector<T>(
        &mut self,
        minimum: usize,
        maximum: u32,
        mut read: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let len = self.length(minimum, maximum)?;
        let mut values = reserved(len)?;
        for _ in 0..len {
            values.push(read(self)?);
        }
        Ok(values)
    }

    /// A derived structural count is not a caller resource allowance. Reject
    /// mismatches as malformed data before reserving any element storage.
    fn exact_vector<T>(
        &mut self,
        minimum: usize,
        expected: u32,
        mut read: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let position = self.position;
        let len = self.length(minimum, u32::MAX)?;
        if count(len)? != expected {
            return Err(self.error_at(position, "noncanonical derived vector count"));
        }
        let mut values = reserved(len)?;
        for _ in 0..len {
            values.push(read(self)?);
        }
        Ok(values)
    }

    fn blob(&mut self, maximum: u32) -> Result<Vec<u8>> {
        let len = self.length(1, maximum)?;
        let bytes = self.take(len)?;
        let mut result = reserved(len)?;
        result.extend_from_slice(bytes);
        Ok(result)
    }

    fn string(&mut self, maximum: u32) -> Result<String> {
        let len = self.length(1, maximum)?;
        let position = self.position;
        let bytes = self.take(len)?;
        let text = core::str::from_utf8(bytes).map_err(|error| {
            self.error_at(
                position.saturating_add(error.valid_up_to()),
                "invalid UTF-8 string",
            )
        })?;
        let mut result = String::new();
        result.try_reserve_exact(len).map_err(|_| budget())?;
        result.push_str(text);
        Ok(result)
    }

    fn finish(&self) -> Result<()> {
        if self.position == self.bytes.len() {
            Ok(())
        } else {
            Err(self.error("trailing bytes after image record"))
        }
    }

    fn span(&mut self) -> Result<Span32> {
        Ok(Span32::new(self.u32()?, self.u32()?))
    }

    fn value_kind(&mut self) -> Result<ValueKind> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(ValueKind::Null),
            1 => Ok(ValueKind::Boolean),
            2 => Ok(ValueKind::Integer),
            3 => Ok(ValueKind::Number),
            4 => Ok(ValueKind::Decimal),
            5 => Ok(ValueKind::String),
            _ => Err(self.error_at(position, "unknown value-kind tag")),
        }
    }

    fn decimal_value(&mut self) -> Result<Decimal128> {
        Ok(Decimal128 {
            coefficient: self.i128()?,
            scale: self.u8()?,
        })
    }

    fn decimal(&mut self) -> Result<DecimalConstraints> {
        Ok(DecimalConstraints {
            policy: DecimalPolicy {
                max_precision: self.u8()?,
                target_scale: self.u8()?,
                allow_exponent: self.boolean()?,
                reject_inexact_rescale: self.boolean()?,
                tick_step: self.i128()?,
            },
            minimum: self.optional(Self::decimal_value)?,
            maximum: self.optional(Self::decimal_value)?,
        })
    }

    fn policy(&mut self) -> Result<SchemaPolicy> {
        let position = self.position;
        let dialect = match self.u8()? {
            0 => Dialect::Endpoint,
            1 => Dialect::JsonSchema202012,
            _ => return Err(self.error_at(position, "unknown schema dialect")),
        };
        let numeric = NumericPolicy {
            allow_Quoted_int: self.boolean()?,
            reject_non_finite_float: self.boolean()?,
            reject_negative_zero: self.boolean()?,
            max_exponent_abs: self.i16()?,
        };
        let position = self.position;
        let unknown_formats = match self.u8()? {
            0 => UnknownFormatPolicy::Reject,
            1 => UnknownFormatPolicy::Annotate,
            _ => return Err(self.error_at(position, "unknown format policy")),
        };
        Ok(SchemaPolicy {
            dialect,
            numeric,
            unknown_formats,
            allow_recursive: self.boolean()?,
            max_pattern_bytes: self.u32()?,
        })
    }

    fn format_id(&mut self) -> Result<FormatId> {
        let position = self.position;
        FormatId::from_u16(self.u16()?)
            .ok_or_else(|| self.error_at(position, "unknown format identifier"))
    }

    fn format_options(&mut self) -> Result<FormatOptions> {
        let max_input_bytes = self.u32()?;
        let nanoid_len = self.u8()?;
        let position = self.position;
        let case = match self.u8()? {
            0 => HexCase::Any,
            1 => HexCase::Lower,
            2 => HexCase::Upper,
            _ => return Err(self.error_at(position, "unknown hex-case tag")),
        };
        let hex = HexPolicy {
            case,
            prefix: self.boolean()?,
            exact_bytes: self.optional(Self::u32)?,
        };
        let position = self.position;
        let base64_padding = match self.u8()? {
            0 => Padding::Required,
            1 => Padding::Forbidden,
            2 => Padding::Optional,
            _ => return Err(self.error_at(position, "unknown base64-padding tag")),
        };
        Ok(FormatOptions {
            max_input_bytes,
            nanoid_len,
            hex,
            base64_padding,
        })
    }

    fn constraints(&mut self) -> Result<ConstraintSet> {
        Ok(ConstraintSet {
            min_i64: self.optional(Self::i64)?,
            max_i64: self.optional(Self::i64)?,
            decimal: self.optional(Self::decimal)?,
            min_count: self.optional(Self::u32)?,
            max_count: self.optional(Self::u32)?,
            pattern: self.optional(Self::u32)?,
            format: self.optional(Self::format_id)?,
            nullable: self.boolean()?,
        })
    }

    fn state(&mut self) -> Result<ProgramState> {
        let id = StateId(self.u32()?);
        let position = self.position;
        let kind = match self.u8()? {
            0 => StateKind::Any,
            1 => StateKind::Scalar(self.value_kind()?),
            2 => StateKind::Object {
                key_table: self.u32()?,
                required_count: self.u16()?,
                reject_unknown: self.boolean()?,
            },
            3 => StateKind::Array {
                items: StateId(self.u32()?),
            },
            _ => return Err(self.error_at(position, "unknown state-kind tag")),
        };
        Ok(ProgramState {
            id,
            kind,
            insns: self.span()?,
            constraints: self.constraints()?,
        })
    }

    fn pattern_op(&mut self, max_bytes: u32) -> Result<PatternOp> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(PatternOp::Char {
                scalar: self.u32()?,
                next: self.u32()?,
            }),
            1 => Ok(PatternOp::Class {
                ranges: self.vector(8, max_bytes / 8, |r| Ok((r.u32()?, r.u32()?)))?,
                negated: self.boolean()?,
                next: self.u32()?,
            }),
            2 => Ok(PatternOp::Any { next: self.u32()? }),
            3 => Ok(PatternOp::Split {
                first: self.u32()?,
                second: self.u32()?,
            }),
            4 => Ok(PatternOp::Jump { next: self.u32()? }),
            5 => Ok(PatternOp::AssertStart { next: self.u32()? }),
            6 => Ok(PatternOp::AssertEnd { next: self.u32()? }),
            7 => Ok(PatternOp::Accept),
            _ => Err(self.error_at(position, "unknown pattern-op tag")),
        }
    }
}

fn read_program(reader: &mut Reader<'_>, options: &CompilerOptions) -> Result<CompiledProgram> {
    let magic = reader.u32()?;
    if magic != IMAGE_MAGIC {
        return Err(reader.error_at(0, "invalid image magic"));
    }
    let version = reader.u16()?;
    if version != IMAGE_VERSION {
        return Err(reader.error_at(4, "unsupported image version"));
    }
    let flags = reader.u16()?;
    if flags != IMAGE_FLAGS {
        return Err(reader.error_at(6, "unsupported image flags"));
    }
    let schema_id = reader.u32()?;
    reader.schema_id = schema_id;
    let header = SchemaImageHeader {
        magic,
        version,
        flags,
        schema_id,
        root_state: reader.u32()?,
        insn_count: reader.u32()?,
        key_count: reader.u32()?,
        slot_count: reader.u32()?,
        fingerprint: reader.u64()?,
    };
    let insns = reader.vector(16, options.limits.max_insns, |r| {
        let position = r.position;
        let opcode = r.u8()?;
        if decode_opcode(opcode).is_none() {
            return Err(r.error_at(position, "unknown instruction opcode"));
        }
        Ok(InsnWord {
            opcode,
            flags: r.u8()?,
            field_id: r.u16()?,
            arg0: r.u32()?,
            arg1: r.u64()?,
        })
    })?;
    if count(insns.len())? != header.insn_count {
        return Err(reader.error_at(16, "instruction count disagrees with header"));
    }
    let keys = reader.vector(24, options.limits.max_keys, |r| {
        Ok(KeyEntry {
            swar_head: r.u64()?,
            span: r.span()?,
            field_id: r.u16()?,
            required_bit: r.u16()?,
            target_state: r.u32()?,
        })
    })?;
    if count(keys.len())? != header.key_count {
        return Err(reader.error_at(20, "key count disagrees with header"));
    }
    let key_bytes = reader.blob(options.max_image_bytes)?;
    let states = reader.vector(20, options.limits.max_states, Reader::state)?;
    let key_tables = reader.vector(9, options.limits.max_states, |r| {
        let first_key = r.u32()?;
        let key_count = r.u32()?;
        let position = r.position;
        let strategy = match r.u8()? {
            0 => KeyStrategy::Swar,
            1 => {
                let seed = r.u64()?;
                // Deterministic lowering uses next_power_of_two(2 * count).
                // Check this derived bound as well as the remaining bytes.
                if key_count == 0 || key_count > options.limits.max_keys {
                    return Err(r.error_at(position, "invalid perfect-hash key count"));
                }
                let capacity = key_count
                    .checked_mul(2)
                    .and_then(|n| n.checked_next_power_of_two())
                    .ok_or_else(budget)?;
                let slots = r.exact_vector(4, capacity, Reader::u32)?;
                KeyStrategy::PerfectHash { seed, slots }
            }
            2 => {
                let seed = r.u64()?;
                if key_count == 0 || key_count > options.limits.max_keys {
                    return Err(r.error_at(position, "invalid bucketed key count"));
                }
                let capacity = key_count
                    .checked_mul(2)
                    .and_then(u32::checked_next_power_of_two)
                    .ok_or_else(budget)?;
                let buckets = r.exact_vector(16, capacity, |r| {
                    Ok(HashBucket {
                        seed: r.u64()?,
                        first_slot: r.u32()?,
                        slot_count: r.u32()?,
                    })
                })?;
                let slot_limit = key_count.checked_mul(8).ok_or_else(budget)?;
                let slots = r.vector(4, slot_limit, Reader::u32)?;
                KeyStrategy::BucketedPerfectHash {
                    seed,
                    buckets,
                    slots,
                }
            }
            _ => return Err(r.error_at(position, "unknown key strategy")),
        };
        Ok(KeyTable {
            first_key,
            key_count,
            strategy,
        })
    })?;
    let mut remaining_ops = options.limits.max_pattern_states;
    let patterns = reader.vector(14, options.limits.max_states.min(remaining_ops), |r| {
        let version = r.u16()?;
        let start = r.u32()?;
        let ops = r.vector(1, remaining_ops, |r| r.pattern_op(options.max_image_bytes))?;
        remaining_ops = remaining_ops
            .checked_sub(count(ops.len())?)
            .ok_or_else(budget)?;
        Ok(PatternProgram {
            version,
            start,
            ops,
            max_steps: r.u32()?,
        })
    })?;
    let slots = reader.vector(12, options.limits.max_keys, |r| {
        Ok(SlotSpec {
            slot: r.u32()?,
            field_id: FieldId(r.u32()?),
            state: StateId(r.u32()?),
        })
    })?;
    if count(slots.len())? != header.slot_count {
        return Err(reader.error_at(24, "slot count disagrees with header"));
    }
    let policy = reader.policy()?;
    let format_options = reader.format_options()?;
    let canonical_schema = reader.blob(options.max_image_bytes)?;
    reader.finish()?;
    Ok(CompiledProgram {
        image: SchemaImage {
            header,
            insns,
            keys,
            key_bytes,
        },
        states,
        key_tables,
        patterns,
        slots,
        policy,
        format_options,
        canonical_schema,
    })
}

/// Decode untrusted bytes and issue a witness only after full validation.
pub fn decode(bytes: &[u8], options: &CompilerOptions) -> Result<ValidatedProgram> {
    limited(bytes.len(), options.max_image_bytes)?;
    let mut reader = Reader::new(bytes);
    let program = read_program(&mut reader, options).map_err(|mut error| {
        error.schema_id = reader.schema_id;
        if error.field_id == 0 {
            error.field_id = reader.field_id;
        }
        error
    })?;
    validate(program, options)
}

fn read_scalar(reader: &mut Reader<'_>, options: &CompilerOptions, nullable: bool) -> Result<ScalarConstraints> {
    Ok(ScalarConstraints {
        kind: reader.value_kind()?,
        min_i64: reader.optional(Reader::i64)?,
        max_i64: reader.optional(Reader::i64)?,
        decimal: reader.optional(Reader::decimal)?,
        min_length: reader.optional(Reader::u32)?,
        max_length: reader.optional(Reader::u32)?,
        pattern: reader.optional(|r| r.string(options.policy.max_pattern_bytes))?,
        format: reader.optional(|r| r.string(options.max_image_bytes))?,
        nullable,
    })
}

/// Canonical records have direct numeric edges, including recursive ones.
/// Building a flat node vector avoids recursion even for adversarial graphs.
fn reconstruct(bytes: &[u8], options: &CompilerOptions) -> Result<ResolvedSchema> {
    let mut reader = Reader::new(bytes);
    if reader.take(12)? != b"JMAX-SCHEMA\0" {
        return Err(invalid("invalid canonical schema prefix"));
    }
    if reader.u16()? != jmax_schema_ir::CANONICAL_VERSION {
        return Err(invalid("unsupported canonical schema version"));
    }
    if reader.policy()? != options.policy {
        return Err(invalid("canonical policy differs from compiler options"));
    }
    let name = reader.string(options.max_image_bytes)?;
    let schema_id = SchemaId(reader.u32()?);
    reader.schema_id = schema_id.0;
    let mut remaining_keys = options.limits.max_keys;
    let nodes = reader.vector(1, options.limits.max_states, |r| {
        let position = r.position;
        let tag = r.u8()?;
        match tag {
            0 => Ok(TypeNode::Any),
            1 | 4 => Ok(TypeNode::Scalar(read_scalar(r, options, tag == 4)?)),
            2 | 5 => {
                let nullable = tag == 5;
                let reject_unknown = r.boolean()?;
                let min_properties = r.optional(Reader::u32)?;
                let max_properties = r.optional(Reader::u32)?;
                let properties = r.vector(14, remaining_keys, |r| {
                    let field_id = FieldId(r.u32()?);
                    r.field_id = field_id.0;
                    let property = Property {
                        field_id,
                        name: r.string(options.max_image_bytes)?,
                        ty: TypeId(r.u32()?),
                        required: r.boolean()?,
                        slot: r.optional(Reader::u32)?,
                    };
                    r.field_id = 0;
                    Ok(property)
                })?;
                remaining_keys = remaining_keys
                    .checked_sub(count(properties.len())?)
                    .ok_or_else(budget)?;
                Ok(TypeNode::Object(ObjectShape {
                    nullable,
                    properties,
                    reject_unknown,
                    min_properties,
                    max_properties,
                }))
            }
            3 | 6 => Ok(TypeNode::Array(ArrayShape {
                nullable: tag == 6,
                items: TypeId(r.u32()?),
                min_items: r.optional(Reader::u32)?,
                max_items: r.optional(Reader::u32)?,
            })),
            _ => Err(r.error_at(position, "unknown canonical node tag")),
        }
    })?;
    reader.finish()?;
    let schema = Schema {
        name,
        schema_id,
        root: TypeId(0),
        nodes,
        definitions: Vec::new(),
    };
    // Canonical input has no references or definitions. Names are bounded by
    // the image limit, not the resolver's unrelated default of 4096 bytes.
    let resolved = jmax_schema_ir::resolve::resolve(
        schema,
        ResolveLimits {
            max_nodes: options.limits.max_states,
            max_definitions: 0,
            max_reference_hops: 0,
            max_name_bytes: options.max_image_bytes,
        },
    )
    .map_err(image_error)?;
    let canonical =
        jmax_schema_ir::canonical::canonicalize(&resolved, &options.policy).map_err(image_error)?;
    if canonical.bytes != bytes {
        return Err(invalid(
            "schema bytes are not in canonical reachable BFS order",
        ));
    }
    Ok(resolved)
}

/// Fast structural rejection precedes canonical recompilation. The final
/// byte comparison below additionally proves exact templates, all operands,
/// constraint compatibility, graph reachability and capture-path uniqueness.
fn inspect(program: &CompiledProgram, options: &CompilerOptions) -> Result<()> {
    let image = &program.image;
    let header = &image.header;
    if header.magic != IMAGE_MAGIC || header.version != IMAGE_VERSION || header.flags != IMAGE_FLAGS
    {
        return Err(invalid("invalid image magic, version or flags"));
    }
    limited(program.states.len(), options.limits.max_states)?;
    limited(image.insns.len(), options.limits.max_insns)?;
    limited(image.keys.len(), options.limits.max_keys)?;
    limited(program.key_tables.len(), options.limits.max_states)?;
    limited(program.slots.len(), options.limits.max_keys)?;
    limited(program.patterns.len(), options.limits.max_states)?;
    limited(encoded_len(program)?, options.max_image_bytes)?;
    if program.policy != options.policy || program.format_options != FormatOptions::default() {
        return Err(invalid("image policy or default format options mismatch"));
    }
    if header.root_state != 0
        || program.states.is_empty()
        || header.insn_count != count(image.insns.len())?
        || header.key_count != count(image.keys.len())?
        || header.slot_count != count(program.slots.len())?
    {
        return Err(invalid("invalid root or inconsistent image counts"));
    }
    if header.fingerprint != jmax_schema_ir::canonical::fingerprint_bytes(&program.canonical_schema)
    {
        return Err(invalid("canonical schema fingerprint mismatch"));
    }
    let state_count = count(program.states.len())?;
    let mut instruction_end = 0u32;
    let mut table_index = 0u32;
    let mut key_end = 0u32;
    for (number, state) in program.states.iter().enumerate() {
        if state.id.0 != count(number)?
            || state.insns.offset != instruction_end
            || state.insns.len == 0
        {
            return Err(invalid("states or instruction ranges are not dense"));
        }
        instruction_end = state
            .insns
            .end()
            .ok_or_else(|| invalid("instruction range overflow"))?;
        if instruction_end > header.insn_count {
            return Err(invalid("state instruction range exceeds image"));
        }
        match &state.kind {
            StateKind::Object {
                key_table,
                required_count,
                ..
            } => {
                if *key_table != table_index {
                    return Err(invalid("key table has no unique state owner"));
                }
                let table = program
                    .key_tables
                    .get(index(table_index)?)
                    .ok_or_else(|| invalid("object key table is missing"))?;
                if table.first_key != key_end {
                    return Err(invalid("key tables do not partition the keys"));
                }
                keys::validate(image, table)?;
                key_end = key_end
                    .checked_add(table.key_count)
                    .ok_or_else(|| invalid("key range overflow"))?;
                let entries = image
                    .keys
                    .get(index(table.first_key)?..index(key_end)?)
                    .ok_or_else(|| invalid("key range exceeds image"))?;
                if entries
                    .iter()
                    .filter(|key| key.required_bit != NO_REQUIRED_BIT)
                    .count()
                    != usize::from(*required_count)
                {
                    return Err(invalid("object required count differs from key table"));
                }
                table_index = table_index.checked_add(1).ok_or_else(budget)?;
            }
            StateKind::Array { items } if items.0 >= state_count => {
                return Err(invalid("array target is outside the state graph"));
            }
            _ => {}
        }
        if state
            .constraints
            .pattern
            .is_some_and(|id| u64::from(id) >= program.patterns.len() as u64)
        {
            return Err(invalid("pattern index is outside the program"));
        }
    }
    if instruction_end != header.insn_count
        || key_end != header.key_count
        || table_index != count(program.key_tables.len())?
    {
        return Err(invalid("unowned instructions, keys or key tables"));
    }
    for insn in &image.insns {
        let opcode =
            decode_opcode(insn.opcode).ok_or_else(|| invalid("unknown instruction opcode"))?;
        if insn.flags != 0 || (opcode != Opcode::EmitSlot && insn.field_id != 0) {
            return Err(invalid("nonzero reserved instruction field")
                .with_schema(header.schema_id, u32::from(insn.field_id)));
        }
    }
    let mut byte_end = 0u32;
    // A fixed bitmap covers the complete ABI field domain without allocation.
    let mut fields = [0u64; 1024];
    for key in &image.keys {
        let field = usize::from(key.field_id);
        let mask = 1u64 << (field % 64);
        let duplicate = fields[field / 64] & mask != 0;
        fields[field / 64] |= mask;
        if duplicate || key.target_state >= state_count || key.span.offset != byte_end {
            return Err(
                invalid("duplicate field, invalid target or unowned key bytes")
                    .with_schema(header.schema_id, u32::from(key.field_id)),
            );
        }
        byte_end = key
            .span
            .end()
            .ok_or_else(|| invalid("key byte range overflow"))?;
    }
    let (names, canonical) = schema_capsule(image, options.max_image_bytes)?;
    if byte_end as usize != names.len() || canonical != program.canonical_schema {
        return Err(invalid(
            "key bytes or schema capsule differ from their owners",
        ));
    }
    for (number, slot) in program.slots.iter().enumerate() {
        if slot.slot != count(number)?
            || slot.state.0 >= state_count
            || slot.field_id.0 > u32::from(u16::MAX)
        {
            return Err(
                invalid("invalid slot mapping").with_schema(header.schema_id, slot.field_id.0)
            );
        }
    }
    let mut pattern_states = 0u32;
    for pattern in &program.patterns {
        pattern_states = pattern_states
            .checked_add(count(pattern.ops.len())?)
            .ok_or_else(budget)?;
        if pattern_states > options.limits.max_pattern_states {
            return Err(budget());
        }
        patterns::validate(pattern, options.limits.max_pattern_states)?;
    }
    Ok(())
}

fn validate_inner(program: &CompiledProgram, options: &CompilerOptions) -> Result<()> {
    inspect(program, options)?;
    let resolved = reconstruct(&program.canonical_schema, options).map_err(image_error)?;
    if resolved.schema().schema_id.0 != program.image.header.schema_id {
        return Err(invalid(
            "canonical schema identifier differs from image header",
        ));
    }
    // Never call lower::compile here: that function calls validate. The lower
    // pass only calls encoded_len, which is pure checked size computation.
    let expected = lower::lower(&resolved, options).map_err(image_error)?;
    let bytes = encode_raw(&expected, options.max_image_bytes)?;
    let mut comparison = Writer {
        destination: Destination::Compare(&bytes),
        position: 0,
    };
    serialize(program, &mut comparison)?;
    if comparison.position != bytes.len() {
        return Err(invalid("image encoding is shorter than canonical lowering"));
    }
    Ok(())
}

/// Validate all owned data and its exact relationship to the canonical schema.
/// This is the sole witness constructor; mutation requires validation again.
pub fn validate(program: CompiledProgram, options: &CompilerOptions) -> Result<ValidatedProgram> {
    validate_inner(&program, options).map_err(|mut error| {
        error.source = ErrorSource::SchemaCompile;
        error.byte_offset = 0;
        error.schema_id = program.image.header.schema_id;
        error
    })?;
    let indexes = build_indexes(&program)
        .map_err(|error| error.with_schema(program.image.header.schema_id, 0))?;
    Ok(ValidatedProgram {
        program: Arc::new(program),
        indexes: Arc::new(indexes),
    })
}

// A standalone ABI image has no side-table fields. Version two retains the
// canonical graph after the key-name prefix, followed by length and magic.
// Key spans continue to address only names. Full wire images encode this
// capsule too, and canonical validation checks its exact bytes.
const CAPSULE_MAGIC: &[u8; 8] = b"JMXSCM02";
const CAPSULE_FOOTER: usize = 12;

pub(crate) fn attach_schema_capsule(program: &mut CompiledProgram, max_bytes: u32) -> Result<()> {
    let additional = program
        .canonical_schema
        .len()
        .checked_add(CAPSULE_FOOTER)
        .ok_or_else(budget)?;
    let total = program
        .image
        .key_bytes
        .len()
        .checked_add(additional)
        .ok_or_else(budget)?;
    limited(total, max_bytes)?;
    let canonical_len = count(program.canonical_schema.len())?;
    program
        .image
        .key_bytes
        .try_reserve_exact(additional)
        .map_err(|_| budget())?;
    program
        .image
        .key_bytes
        .extend_from_slice(&program.canonical_schema);
    program
        .image
        .key_bytes
        .extend_from_slice(&canonical_len.to_le_bytes());
    program.image.key_bytes.extend_from_slice(CAPSULE_MAGIC);
    Ok(())
}

fn schema_capsule(image: &SchemaImage, max_bytes: u32) -> Result<(&[u8], &[u8])> {
    limited(image.key_bytes.len(), max_bytes)?;
    let bytes = &image.key_bytes;
    let footer = bytes
        .len()
        .checked_sub(CAPSULE_FOOTER)
        .ok_or_else(|| invalid("schema capsule is missing"))?;
    if bytes.get(footer + 4..) != Some(CAPSULE_MAGIC.as_slice()) {
        return Err(invalid("schema capsule magic is invalid"));
    }
    let mut encoded = [0u8; 4];
    encoded.copy_from_slice(&bytes[footer..footer + 4]);
    let len = index(u32::from_le_bytes(encoded))?;
    let start = footer
        .checked_sub(len)
        .ok_or_else(|| invalid("schema capsule length is invalid"))?;
    Ok((&bytes[..start], &bytes[start..footer]))
}

/// Restore a complete witnessed program from a standalone version-two image.
/// Both borrowed images and owned images are accepted for facade compatibility.
/// Call once when loading a schema, then retain or cheaply clone the witness.
pub fn load(
    image: impl core::borrow::Borrow<SchemaImage>,
    options: &CompilerOptions,
) -> Result<ValidatedProgram> {
    let image = image.borrow();
    let result = (|| {
        if image.header.magic != IMAGE_MAGIC
            || image.header.version != IMAGE_VERSION
            || image.header.flags != IMAGE_FLAGS
        {
            return Err(invalid("invalid image magic, version or flags"));
        }
        limited(image.insns.len(), options.limits.max_insns)?;
        limited(image.keys.len(), options.limits.max_keys)?;
        let (_, canonical) = schema_capsule(image, options.max_image_bytes)?;
        let resolved = reconstruct(canonical, options).map_err(image_error)?;
        let expected = lower::lower(&resolved, options).map_err(image_error)?;
        if expected.image.header != image.header
            || expected.image.insns != image.insns
            || expected.image.keys != image.keys
            || expected.image.key_bytes != image.key_bytes
        {
            return Err(invalid("standalone image differs from canonical lowering"));
        }
        validate(expected, options)
    })();
    result.map_err(|mut error| {
        error.schema_id = image.header.schema_id;
        error
    })
}

fn build_indexes(program: &CompiledProgram) -> Result<ProgramIndexes> {
    let field_count = program
        .slots
        .iter()
        .map(|slot| slot.field_id.0 as usize + 1)
        .max()
        .unwrap_or(0);
    let mut field_slots = reserved(field_count)?;
    field_slots.resize(field_count, EMPTY_KEY_SLOT);
    for slot in &program.slots {
        let entry = field_slots
            .get_mut(slot.field_id.0 as usize)
            .ok_or_else(|| invalid("invalid indexed field"))?;
        if *entry != EMPTY_KEY_SLOT {
            return Err(invalid("duplicate indexed field"));
        }
        *entry = slot.slot;
    }
    let mut table_states = reserved(program.key_tables.len())?;
    for state in &program.states {
        if let StateKind::Object { key_table, .. } = state.kind {
            if key_table as usize != table_states.len() {
                return Err(invalid("unordered object table owner"));
            }
            table_states.push(state.id);
        }
    }
    let mut dispatch = reserved(program.states.len())?;
    for state in &program.states {
        use jmax_abi::syntax::ScalarKind as S;
        let bit = scalar_kind_bit;
        let scalar_mask = match state.kind {
            StateKind::Any => 31,
            StateKind::Scalar(ValueKind::Null) => bit(S::Null),
            StateKind::Scalar(ValueKind::Boolean) => bit(S::True) | bit(S::False),
            StateKind::Scalar(ValueKind::Integer) => {
                bit(S::Number)
                    | if program.policy.numeric.allow_Quoted_int {
                        bit(S::String)
                    } else {
                        0
                    }
            }
            StateKind::Scalar(ValueKind::Number) => bit(S::Number),
            StateKind::Scalar(ValueKind::Decimal | ValueKind::String) => bit(S::String),
            StateKind::Object { .. } | StateKind::Array { .. } => 0,
        } | if state.constraints.nullable { bit(S::Null) } else { 0 };
        let has_constraints =
            state.kind != StateKind::Any || state.constraints != ConstraintSet::default();
        dispatch.push(CompiledStateDispatch {
            scalar_mask,
            slot_index: None,
            has_slots: false,
            has_constraints,
            subtree_has_slots: false,
            subtree_has_constraints: has_constraints,
            unknown_fields_unconstrained: matches!(
                state.kind,
                StateKind::Any
                    | StateKind::Object {
                        reject_unknown: false,
                        ..
                    }
            ),
        });
    }
    for slot in &program.slots {
        let entry = &mut dispatch[slot.state.0 as usize];
        // A shared state can be captured by several different properties.
        entry.slot_index = if entry.has_slots {
            None
        } else {
            Some(slot.slot)
        };
        entry.has_slots = true;
        entry.subtree_has_slots = true;
    }
    // Reverse edges and a monotone work queue handle recursive schemas in
    // O(states + edges), without recursion or repeated whole-graph scans.
    let mut parents: Vec<Vec<StateId>> = reserved(program.states.len())?;
    parents.resize_with(program.states.len(), Vec::new);
    for (owner, state) in program.states.iter().enumerate() {
        match state.kind {
            StateKind::Array { items } => {
                parents[items.0 as usize]
                    .try_reserve(1)
                    .map_err(|_| budget())?;
                parents[items.0 as usize].push(StateId(owner as u32));
            }
            StateKind::Object { key_table, .. } => {
                let table = &program.key_tables[key_table as usize];
                for key in &program.image.keys
                    [table.first_key as usize..(table.first_key + table.key_count) as usize]
                {
                    let list = &mut parents[key.target_state as usize];
                    list.try_reserve(1).map_err(|_| budget())?;
                    list.push(StateId(owner as u32));
                }
            }
            _ => {}
        }
    }
    let mut queue = reserved(program.states.len().checked_mul(2).ok_or_else(budget)?)?;
    for (id, entry) in dispatch.iter().enumerate() {
        if entry.subtree_has_slots || entry.subtree_has_constraints {
            queue.push(id);
        }
    }
    let mut cursor = 0;
    while let Some(&child) = queue.get(cursor) {
        let flags = dispatch[child];
        for &parent_id in &parents[child] {
            let parent = parent_id.0 as usize;
            let entry = &mut dispatch[parent];
            let changed = (flags.subtree_has_slots && !entry.subtree_has_slots)
                || (flags.subtree_has_constraints && !entry.subtree_has_constraints);
            entry.subtree_has_slots |= flags.subtree_has_slots;
            entry.subtree_has_constraints |= flags.subtree_has_constraints;
            if changed {
                queue.try_reserve(1).map_err(|_| budget())?;
                queue.push(parent);
            }
        }
        cursor += 1;
    }
    let mut required_masks = reserved(program.key_tables.len())?;
    for table in &program.key_tables {
        let mut mask = reserved((table.key_count as usize + 63) / 64)?;
        mask.resize((table.key_count as usize + 63) / 64, 0u64);
        for (local, key) in program.image.keys
            [table.first_key as usize..(table.first_key + table.key_count) as usize]
            .iter()
            .enumerate()
        {
            if key.required_bit != NO_REQUIRED_BIT {
                mask[local / 64] |= 1u64 << (local % 64);
            }
        }
        required_masks.push(mask);
    }
    Ok(ProgramIndexes {
        field_slots,
        table_states,
        dispatch,
        parents,
        required_masks,
    })
}
