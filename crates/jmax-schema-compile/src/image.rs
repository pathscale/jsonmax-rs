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
    if count(value)? > limit { Err(budget()) } else { Ok(()) }
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

    fn u8(&mut self, value: u8) -> Result<()> { self.bytes(&[value]) }
    fn u16(&mut self, value: u16) -> Result<()> { self.bytes(&value.to_le_bytes()) }
    fn u32(&mut self, value: u32) -> Result<()> { self.bytes(&value.to_le_bytes()) }
    fn u64(&mut self, value: u64) -> Result<()> { self.bytes(&value.to_le_bytes()) }
    fn i16(&mut self, value: i16) -> Result<()> { self.bytes(&value.to_le_bytes()) }
    fn i64(&mut self, value: i64) -> Result<()> { self.bytes(&value.to_le_bytes()) }
    fn i128(&mut self, value: i128) -> Result<()> { self.bytes(&value.to_le_bytes()) }
    fn boolean(&mut self, value: bool) -> Result<()> { self.u8(u8::from(value)) }
    fn length(&mut self, value: usize) -> Result<()> { self.u32(count(value)?) }

    fn blob(&mut self, value: &[u8]) -> Result<()> {
        self.length(value.len())?;
        self.bytes(value)
    }

    fn optional<T>(&mut self, value: Option<&T>, write: impl FnOnce(&mut Self, &T) -> Result<()>) -> Result<()> {
        self.boolean(value.is_some())?;
        if let Some(value) = value { write(self, value)?; }
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
        self.u8(match value.dialect { Dialect::Endpoint => 0, Dialect::JsonSchema202012 => 1 })?;
        self.boolean(value.numeric.allow_Quoted_int)?;
        self.boolean(value.numeric.reject_non_finite_float)?;
        self.boolean(value.numeric.reject_negative_zero)?;
        self.i16(value.numeric.max_exponent_abs)?;
        self.u8(match value.unknown_formats { UnknownFormatPolicy::Reject => 0, UnknownFormatPolicy::Annotate => 1 })?;
        self.boolean(value.allow_recursive)?;
        self.u32(value.max_pattern_bytes)
    }

    fn format_options(&mut self, value: &FormatOptions) -> Result<()> {
        self.u32(value.max_input_bytes)?;
        self.u8(value.nanoid_len)?;
        self.u8(match value.hex.case { HexCase::Any => 0, HexCase::Lower => 1, HexCase::Upper => 2 })?;
        self.boolean(value.hex.prefix)?;
        self.optional(value.hex.exact_bytes.as_ref(), |w, n| w.u32(*n))?;
        self.u8(match value.base64_padding { Padding::Required => 0, Padding::Forbidden => 1, Padding::Optional => 2 })
    }

    fn constraints(&mut self, value: &ConstraintSet) -> Result<()> {
        self.optional(value.min_i64.as_ref(), |w, n| w.i64(*n))?;
        self.optional(value.max_i64.as_ref(), |w, n| w.i64(*n))?;
        self.optional(value.decimal.as_ref(), Self::decimal)?;
        self.optional(value.min_count.as_ref(), |w, n| w.u32(*n))?;
        self.optional(value.max_count.as_ref(), |w, n| w.u32(*n))?;
        self.optional(value.pattern.as_ref(), |w, n| w.u32(*n))?;
        self.optional(value.format.as_ref(), |w, n| w.u16(*n as u16))
    }

    fn state(&mut self, state: &ProgramState) -> Result<()> {
        self.u32(state.id.0)?;
        match &state.kind {
            StateKind::Any => self.u8(0)?,
            StateKind::Scalar(kind) => { self.u8(1)?; self.u8(value_tag(*kind))?; }
            StateKind::Object { key_table, required_count, reject_unknown } => {
                self.u8(2)?;
                self.u32(*key_table)?;
                self.u16(*required_count)?;
                self.boolean(*reject_unknown)?;
            }
            StateKind::Array { items } => { self.u8(3)?; self.u32(items.0)?; }
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
                    self.u8(0)?; self.u32(*scalar)?; self.u32(*next)?;
                }
                PatternOp::Class { ranges, negated, next } => {
                    self.u8(1)?;
                    self.length(ranges.len())?;
                    for &(first, last) in ranges { self.u32(first)?; self.u32(last)?; }
                    self.boolean(*negated)?;
                    self.u32(*next)?;
                }
                PatternOp::Any { next } => { self.u8(2)?; self.u32(*next)?; }
                PatternOp::Split { first, second } => {
                    self.u8(3)?; self.u32(*first)?; self.u32(*second)?;
                }
                PatternOp::Jump { next } => { self.u8(4)?; self.u32(*next)?; }
                PatternOp::AssertStart { next } => { self.u8(5)?; self.u32(*next)?; }
                PatternOp::AssertEnd { next } => { self.u8(6)?; self.u32(*next)?; }
                PatternOp::Accept => self.u8(7)?,
            }
        }
        self.u32(pattern.max_steps)
    }
}

fn value_tag(kind: ValueKind) -> u8 {
    match kind {
        ValueKind::Null => 0, ValueKind::Boolean => 1, ValueKind::Integer => 2,
        ValueKind::Number => 3, ValueKind::Decimal => 4, ValueKind::String => 5,
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
    for state in &program.states { writer.state(state)?; }
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
                for &slot in slots { writer.u32(slot)?; }
            }
        }
    }
    writer.length(program.patterns.len())?;
    for pattern in &program.patterns { writer.pattern(pattern)?; }
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
    let mut writer = Writer { destination: Destination::Count, position: 0 };
    serialize(program, &mut writer).map_err(|mut error| {
        error.schema_id = program.image.header.schema_id;
        error
    })?;
    Ok(writer.position)
}

fn encode_raw(program: &CompiledProgram, max_bytes: u32) -> Result<Vec<u8>> {
    let len = encoded_len(program)?;
    limited(len, max_bytes)?;
    let mut writer = Writer { destination: Destination::Bytes(reserved(len)?), position: 0 };
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
        Self { bytes, position: 0, schema_id: 0, field_id: 0 }
    }

    fn error_at(&self, position: usize, detail: &'static str) -> Error {
        Error::new(ErrorCode::SchemaImageInvalid, ErrorSource::SchemaCompile,
            u32::try_from(position).unwrap_or(u32::MAX), detail)
            .with_schema(self.schema_id, self.field_id)
    }

    fn error(&self, detail: &'static str) -> Error { self.error_at(self.position, detail) }

    fn budget_at(&self, position: usize) -> Error {
        let mut error = self.error_at(position, "image resource limit exceeded");
        error.code = ErrorCode::BudgetExhausted;
        error
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.position.checked_add(len).ok_or_else(budget)?;
        let bytes = self.bytes.get(self.position..end)
            .ok_or_else(|| self.error("truncated image record"))?;
        self.position = end;
        Ok(bytes)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut value = [0; N];
        value.copy_from_slice(self.take(N)?);
        Ok(value)
    }

    fn u8(&mut self) -> Result<u8> { Ok(u8::from_le_bytes(self.fixed()?)) }
    fn u16(&mut self) -> Result<u16> { Ok(u16::from_le_bytes(self.fixed()?)) }
    fn u32(&mut self) -> Result<u32> { Ok(u32::from_le_bytes(self.fixed()?)) }
    fn u64(&mut self) -> Result<u64> { Ok(u64::from_le_bytes(self.fixed()?)) }
    fn i16(&mut self) -> Result<i16> { Ok(i16::from_le_bytes(self.fixed()?)) }
    fn i64(&mut self) -> Result<i64> { Ok(i64::from_le_bytes(self.fixed()?)) }
    fn i128(&mut self) -> Result<i128> { Ok(i128::from_le_bytes(self.fixed()?)) }

    fn boolean(&mut self) -> Result<bool> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(false), 1 => Ok(true),
            _ => Err(self.error_at(position, "noncanonical boolean or option tag")),
        }
    }

    fn optional<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if self.boolean()? { Ok(Some(read(self)?)) } else { Ok(None) }
    }

    /// Check the minimum possible wire footprint and caller limit before any
    /// allocation. Each variable-size element is checked again while reading.
    fn length(&mut self, minimum: usize, maximum: u32) -> Result<usize> {
        let position = self.position;
        let len = self.u32()?;
        let size = index(len)?;
        let required = size.checked_mul(minimum).ok_or_else(|| self.budget_at(position))?;
        if required > self.bytes.len().saturating_sub(self.position) {
            return Err(self.error_at(position, "element count exceeds remaining bytes"));
        }
        if len > maximum {
            return Err(self.budget_at(position));
        }
        Ok(size)
    }

    fn vector<T>(&mut self, minimum: usize, maximum: u32,
        mut read: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>>
    {
        let len = self.length(minimum, maximum)?;
        let mut values = reserved(len)?;
        for _ in 0..len { values.push(read(self)?); }
        Ok(values)
    }

    /// A derived structural count is not a caller resource allowance. Reject
    /// mismatches as malformed data before reserving any element storage.
    fn exact_vector<T>(&mut self, minimum: usize, expected: u32,
        mut read: impl FnMut(&mut Self) -> Result<T>) -> Result<Vec<T>>
    {
        let position = self.position;
        let len = self.length(minimum, u32::MAX)?;
        if count(len)? != expected {
            return Err(self.error_at(position, "noncanonical derived vector count"));
        }
        let mut values = reserved(len)?;
        for _ in 0..len { values.push(read(self)?); }
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
            self.error_at(position.saturating_add(error.valid_up_to()), "invalid UTF-8 string")
        })?;
        let mut result = String::new();
        result.try_reserve_exact(len).map_err(|_| budget())?;
        result.push_str(text);
        Ok(result)
    }

    fn finish(&self) -> Result<()> {
        if self.position == self.bytes.len() { Ok(()) }
        else { Err(self.error("trailing bytes after image record")) }
    }

    fn span(&mut self) -> Result<Span32> { Ok(Span32::new(self.u32()?, self.u32()?)) }

    fn value_kind(&mut self) -> Result<ValueKind> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(ValueKind::Null), 1 => Ok(ValueKind::Boolean),
            2 => Ok(ValueKind::Integer), 3 => Ok(ValueKind::Number),
            4 => Ok(ValueKind::Decimal), 5 => Ok(ValueKind::String),
            _ => Err(self.error_at(position, "unknown value-kind tag")),
        }
    }

    fn decimal_value(&mut self) -> Result<Decimal128> {
        Ok(Decimal128 { coefficient: self.i128()?, scale: self.u8()? })
    }

    fn decimal(&mut self) -> Result<DecimalConstraints> {
        Ok(DecimalConstraints {
            policy: DecimalPolicy {
                max_precision: self.u8()?, target_scale: self.u8()?,
                allow_exponent: self.boolean()?, reject_inexact_rescale: self.boolean()?,
                tick_step: self.i128()?,
            },
            minimum: self.optional(Self::decimal_value)?,
            maximum: self.optional(Self::decimal_value)?,
        })
    }

    fn policy(&mut self) -> Result<SchemaPolicy> {
        let position = self.position;
        let dialect = match self.u8()? {
            0 => Dialect::Endpoint, 1 => Dialect::JsonSchema202012,
            _ => return Err(self.error_at(position, "unknown schema dialect")),
        };
        let numeric = NumericPolicy {
            allow_Quoted_int: self.boolean()?, reject_non_finite_float: self.boolean()?,
            reject_negative_zero: self.boolean()?, max_exponent_abs: self.i16()?,
        };
        let position = self.position;
        let unknown_formats = match self.u8()? {
            0 => UnknownFormatPolicy::Reject, 1 => UnknownFormatPolicy::Annotate,
            _ => return Err(self.error_at(position, "unknown format policy")),
        };
        Ok(SchemaPolicy { dialect, numeric, unknown_formats,
            allow_recursive: self.boolean()?, max_pattern_bytes: self.u32()? })
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
            0 => HexCase::Any, 1 => HexCase::Lower, 2 => HexCase::Upper,
            _ => return Err(self.error_at(position, "unknown hex-case tag")),
        };
        let hex = HexPolicy { case, prefix: self.boolean()?, exact_bytes: self.optional(Self::u32)? };
        let position = self.position;
        let base64_padding = match self.u8()? {
            0 => Padding::Required, 1 => Padding::Forbidden, 2 => Padding::Optional,
            _ => return Err(self.error_at(position, "unknown base64-padding tag")),
        };
        Ok(FormatOptions { max_input_bytes, nanoid_len, hex, base64_padding })
    }

    fn constraints(&mut self) -> Result<ConstraintSet> {
        Ok(ConstraintSet {
            min_i64: self.optional(Self::i64)?, max_i64: self.optional(Self::i64)?,
            decimal: self.optional(Self::decimal)?, min_count: self.optional(Self::u32)?,
            max_count: self.optional(Self::u32)?, pattern: self.optional(Self::u32)?,
            format: self.optional(Self::format_id)?,
        })
    }

    fn state(&mut self) -> Result<ProgramState> {
        let id = StateId(self.u32()?);
        let position = self.position;
        let kind = match self.u8()? {
            0 => StateKind::Any,
            1 => StateKind::Scalar(self.value_kind()?),
            2 => StateKind::Object { key_table: self.u32()?, required_count: self.u16()?, reject_unknown: self.boolean()? },
            3 => StateKind::Array { items: StateId(self.u32()?) },
            _ => return Err(self.error_at(position, "unknown state-kind tag")),
        };
        Ok(ProgramState { id, kind, insns: self.span()?, constraints: self.constraints()? })
    }

    fn pattern_op(&mut self, max_bytes: u32) -> Result<PatternOp> {
        let position = self.position;
        match self.u8()? {
            0 => Ok(PatternOp::Char { scalar: self.u32()?, next: self.u32()? }),
            1 => Ok(PatternOp::Class {
                ranges: self.vector(8, max_bytes / 8, |r| Ok((r.u32()?, r.u32()?)))?,
                negated: self.boolean()?, next: self.u32()?,
            }),
            2 => Ok(PatternOp::Any { next: self.u32()? }),
            3 => Ok(PatternOp::Split { first: self.u32()?, second: self.u32()? }),
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
    if magic != IMAGE_MAGIC { return Err(reader.error_at(0, "invalid image magic")); }
    let version = reader.u16()?;
    if version != IMAGE_VERSION { return Err(reader.error_at(4, "unsupported image version")); }
    let flags = reader.u16()?;
    if flags != IMAGE_FLAGS { return Err(reader.error_at(6, "unsupported image flags")); }
    let schema_id = reader.u32()?;
    reader.schema_id = schema_id;
    let header = SchemaImageHeader {
        magic, version, flags, schema_id,
        root_state: reader.u32()?, insn_count: reader.u32()?,
        key_count: reader.u32()?, slot_count: reader.u32()?, fingerprint: reader.u64()?,
    };
    let insns = reader.vector(16, options.limits.max_insns, |r| {
        let position = r.position;
        let opcode = r.u8()?;
        if decode_opcode(opcode).is_none() {
            return Err(r.error_at(position, "unknown instruction opcode"));
        }
        Ok(InsnWord { opcode, flags: r.u8()?, field_id: r.u16()?, arg0: r.u32()?, arg1: r.u64()? })
    })?;
    if count(insns.len())? != header.insn_count {
        return Err(reader.error_at(16, "instruction count disagrees with header"));
    }
    let keys = reader.vector(24, options.limits.max_keys, |r| {
        Ok(KeyEntry { swar_head: r.u64()?, span: r.span()?, field_id: r.u16()?,
            required_bit: r.u16()?, target_state: r.u32()? })
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
                let capacity = key_count.checked_mul(2).and_then(|n| n.checked_next_power_of_two())
                    .ok_or_else(budget)?;
                let slots = r.exact_vector(4, capacity, Reader::u32)?;
                KeyStrategy::PerfectHash { seed, slots }
            }
            _ => return Err(r.error_at(position, "unknown key strategy")),
        };
        Ok(KeyTable { first_key, key_count, strategy })
    })?;
    let mut remaining_ops = options.limits.max_pattern_states;
    let patterns = reader.vector(14, options.limits.max_states.min(remaining_ops), |r| {
        let version = r.u16()?;
        let start = r.u32()?;
        let ops = r.vector(1, remaining_ops, |r| r.pattern_op(options.max_image_bytes))?;
        remaining_ops = remaining_ops.checked_sub(count(ops.len())?).ok_or_else(budget)?;
        Ok(PatternProgram { version, start, ops, max_steps: r.u32()? })
    })?;
    let slots = reader.vector(12, options.limits.max_keys, |r| {
        Ok(SlotSpec { slot: r.u32()?, field_id: FieldId(r.u32()?), state: StateId(r.u32()?) })
    })?;
    if count(slots.len())? != header.slot_count {
        return Err(reader.error_at(24, "slot count disagrees with header"));
    }
    let policy = reader.policy()?;
    let format_options = reader.format_options()?;
    let canonical_schema = reader.blob(options.max_image_bytes)?;
    reader.finish()?;
    Ok(CompiledProgram {
        image: SchemaImage { header, insns, keys, key_bytes }, states, key_tables,
        patterns, slots, policy, format_options, canonical_schema,
    })
}

/// Decode untrusted bytes and issue a witness only after full validation.
pub fn decode(bytes: &[u8], options: &CompilerOptions) -> Result<ValidatedProgram> {
    limited(bytes.len(), options.max_image_bytes)?;
    let mut reader = Reader::new(bytes);
    let program = read_program(&mut reader, options).map_err(|mut error| {
        error.schema_id = reader.schema_id;
        if error.field_id == 0 { error.field_id = reader.field_id; }
        error
    })?;
    validate(program, options)
}

fn read_scalar(reader: &mut Reader<'_>, options: &CompilerOptions) -> Result<ScalarConstraints> {
    Ok(ScalarConstraints {
        kind: reader.value_kind()?,
        min_i64: reader.optional(Reader::i64)?,
        max_i64: reader.optional(Reader::i64)?,
        decimal: reader.optional(Reader::decimal)?,
        min_length: reader.optional(Reader::u32)?,
        max_length: reader.optional(Reader::u32)?,
        pattern: reader.optional(|r| r.string(options.policy.max_pattern_bytes))?,
        format: reader.optional(|r| r.string(options.max_image_bytes))?,
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
        match r.u8()? {
            0 => Ok(TypeNode::Any),
            1 => Ok(TypeNode::Scalar(read_scalar(r, options)?)),
            2 => {
                let reject_unknown = r.boolean()?;
                let min_properties = r.optional(Reader::u32)?;
                let max_properties = r.optional(Reader::u32)?;
                let properties = r.vector(14, remaining_keys, |r| {
                    let field_id = FieldId(r.u32()?);
                    r.field_id = field_id.0;
                    let property = Property {
                        field_id, name: r.string(options.max_image_bytes)?, ty: TypeId(r.u32()?),
                        required: r.boolean()?, slot: r.optional(Reader::u32)?,
                    };
                    r.field_id = 0;
                    Ok(property)
                })?;
                remaining_keys = remaining_keys.checked_sub(count(properties.len())?).ok_or_else(budget)?;
                Ok(TypeNode::Object(ObjectShape { properties, reject_unknown, min_properties, max_properties }))
            }
            3 => Ok(TypeNode::Array(ArrayShape {
                items: TypeId(r.u32()?), min_items: r.optional(Reader::u32)?, max_items: r.optional(Reader::u32)?,
            })),
            _ => Err(r.error_at(position, "unknown canonical node tag")),
        }
    })?;
    reader.finish()?;
    let schema = Schema { name, schema_id, root: TypeId(0), nodes, definitions: Vec::new() };
    // Canonical input has no references or definitions. Names are bounded by
    // the image limit, not the resolver's unrelated default of 4096 bytes.
    let resolved = jmax_schema_ir::resolve::resolve(schema, ResolveLimits {
        max_nodes: options.limits.max_states, max_definitions: 0,
        max_reference_hops: 0, max_name_bytes: options.max_image_bytes,
    }).map_err(image_error)?;
    let canonical = jmax_schema_ir::canonical::canonicalize(&resolved, &options.policy)
        .map_err(image_error)?;
    if canonical.bytes != bytes {
        return Err(invalid("schema bytes are not in canonical reachable BFS order"));
    }
    Ok(resolved)
}

/// Fast structural rejection precedes canonical recompilation. The final
/// byte comparison below additionally proves exact templates, all operands,
/// constraint compatibility, graph reachability and capture-path uniqueness.
fn inspect(program: &CompiledProgram, options: &CompilerOptions) -> Result<()> {
    let image = &program.image;
    let header = &image.header;
    if header.magic != IMAGE_MAGIC || header.version != IMAGE_VERSION || header.flags != IMAGE_FLAGS {
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
        return Err(invalid("image policy or version-one format options mismatch"));
    }
    if header.root_state != 0 || program.states.is_empty()
        || header.insn_count != count(image.insns.len())?
        || header.key_count != count(image.keys.len())?
        || header.slot_count != count(program.slots.len())?
    {
        return Err(invalid("invalid root or inconsistent image counts"));
    }
    if header.fingerprint != jmax_schema_ir::canonical::fingerprint_bytes(&program.canonical_schema) {
        return Err(invalid("canonical schema fingerprint mismatch"));
    }
    let state_count = count(program.states.len())?;
    let mut instruction_end = 0u32;
    let mut table_index = 0u32;
    let mut key_end = 0u32;
    for (number, state) in program.states.iter().enumerate() {
        if state.id.0 != count(number)? || state.insns.offset != instruction_end || state.insns.len == 0 {
            return Err(invalid("states or instruction ranges are not dense"));
        }
        instruction_end = state.insns.end().ok_or_else(|| invalid("instruction range overflow"))?;
        if instruction_end > header.insn_count {
            return Err(invalid("state instruction range exceeds image"));
        }
        match &state.kind {
            StateKind::Object { key_table, required_count, .. } => {
                if *key_table != table_index { return Err(invalid("key table has no unique state owner")); }
                let table = program.key_tables.get(index(table_index)?)
                    .ok_or_else(|| invalid("object key table is missing"))?;
                if table.first_key != key_end { return Err(invalid("key tables do not partition the keys")); }
                keys::validate(image, table)?;
                key_end = key_end.checked_add(table.key_count).ok_or_else(|| invalid("key range overflow"))?;
                let entries = image.keys.get(index(table.first_key)?..index(key_end)?)
                    .ok_or_else(|| invalid("key range exceeds image"))?;
                if entries.iter().filter(|key| key.required_bit != NO_REQUIRED_BIT).count() != usize::from(*required_count) {
                    return Err(invalid("object required count differs from key table"));
                }
                table_index = table_index.checked_add(1).ok_or_else(budget)?;
            }
            StateKind::Array { items } if items.0 >= state_count => {
                return Err(invalid("array target is outside the state graph"));
            }
            _ => {}
        }
        if state.constraints.pattern.is_some_and(|id| u64::from(id) >= program.patterns.len() as u64) {
            return Err(invalid("pattern index is outside the program"));
        }
    }
    if instruction_end != header.insn_count || key_end != header.key_count
        || table_index != count(program.key_tables.len())?
    {
        return Err(invalid("unowned instructions, keys or key tables"));
    }
    for insn in &image.insns {
        let opcode = decode_opcode(insn.opcode).ok_or_else(|| invalid("unknown instruction opcode"))?;
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
            return Err(invalid("duplicate field, invalid target or unowned key bytes")
                .with_schema(header.schema_id, u32::from(key.field_id)));
        }
        byte_end = key.span.end().ok_or_else(|| invalid("key byte range overflow"))?;
    }
    if byte_end != count(image.key_bytes.len())? { return Err(invalid("trailing unowned key bytes")); }
    for (number, slot) in program.slots.iter().enumerate() {
        if slot.slot != count(number)? || slot.state.0 >= state_count || slot.field_id.0 > u32::from(u16::MAX) {
            return Err(invalid("invalid slot mapping").with_schema(header.schema_id, slot.field_id.0));
        }
    }
    let mut pattern_states = 0u32;
    for pattern in &program.patterns {
        pattern_states = pattern_states.checked_add(count(pattern.ops.len())?).ok_or_else(budget)?;
        if pattern_states > options.limits.max_pattern_states { return Err(budget()); }
        patterns::validate(pattern, options.limits.max_pattern_states)?;
    }
    Ok(())
}

fn validate_inner(program: &CompiledProgram, options: &CompilerOptions) -> Result<()> {
    inspect(program, options)?;
    let resolved = reconstruct(&program.canonical_schema, options).map_err(image_error)?;
    if resolved.schema().schema_id.0 != program.image.header.schema_id {
        return Err(invalid("canonical schema identifier differs from image header"));
    }
    // Never call lower::compile here: that function calls validate. The lower
    // pass only calls encoded_len, which is pure checked size computation.
    let expected = lower::lower(&resolved, options).map_err(image_error)?;
    let bytes = encode_raw(&expected, options.max_image_bytes)?;
    let mut comparison = Writer { destination: Destination::Compare(&bytes), position: 0 };
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
    Ok(ValidatedProgram { program })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use jmax_schema_ir::NamedType;

    const SCHEMA_ID: u32 = 0x0102_0304;
    const ANY_FINGERPRINT: u64 = 0xd42f_04ce_417a_2321;

    // Independent frozen-grammar vectors: empty schema name, one Any node,
    // default policy/options, schema ID 0x01020304, and root state zero.
    const ANY_CANONICAL: &[u8] = &[
        74, 77, 65, 88, 45, 83, 67, 72, 69, 77, 65, 0, 1, 0,
        0, 0, 1, 1, 52, 1, 0, 1, 0, 16, 0, 0,
        0, 0, 0, 0, 4, 3, 2, 1, 1, 0, 0, 0, 0,
    ];
    const ANY_WIRE: &[u8] = &[
        0x4a, 0x4d, 0x41, 0x58, 0x01, 0x00, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x21, 0x23, 0x7a, 0x41,
        0xce, 0x04, 0x2f, 0xd4, 0x01, 0x00, 0x00, 0x00, 0x07, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0xff, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x01, 0x34, 0x01, 0x00, 0x01, 0x00, 0x10, 0x00, 0x00,
        0x00, 0x00, 0x10, 0x00, 0x15, 0x00, 0x00, 0x00, 0x00, 0x27, 0x00, 0x00, 0x00, 0x4a, 0x4d, 0x41,
        0x58, 0x2d, 0x53, 0x43, 0x48, 0x45, 0x4d, 0x41, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x34,
        0x01, 0x00, 0x01, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x03, 0x02, 0x01, 0x01,
        0x00, 0x00, 0x00, 0x00,
    ];

    fn schema(nodes: Vec<TypeNode>) -> Schema {
        Schema { name: String::new(), schema_id: SchemaId(SCHEMA_ID),
            root: TypeId(0), nodes, definitions: Vec::new() }
    }

    fn resolved(input: Schema) -> ResolvedSchema {
        jmax_schema_ir::resolve::resolve(input, ResolveLimits {
            max_name_bytes: DEFAULT_MAX_IMAGE_BYTES, ..ResolveLimits::default()
        }).unwrap()
    }

    fn raw(nodes: Vec<TypeNode>, options: &CompilerOptions) -> CompiledProgram {
        lower::lower(&resolved(schema(nodes)), options).unwrap()
    }

    fn any() -> CompiledProgram { raw(vec![TypeNode::Any], &CompilerOptions::default()) }

    fn property(name: &str, field: u32, ty: u32, required: bool, slot: Option<u32>) -> Property {
        Property { name: String::from(name), field_id: FieldId(field),
            ty: TypeId(ty), required, slot }
    }

    fn object(properties: Vec<Property>) -> TypeNode {
        TypeNode::Object(ObjectShape { properties, reject_unknown: false,
            min_properties: None, max_properties: None })
    }

    fn array(items: u32) -> TypeNode {
        TypeNode::Array(ArrayShape { items: TypeId(items), min_items: None, max_items: None })
    }

    fn patterned(source: &str) -> TypeNode {
        let mut constraints = ScalarConstraints::new(ValueKind::String);
        constraints.pattern = Some(String::from(source));
        TypeNode::Scalar(constraints)
    }

    fn fixture(options: &CompilerOptions) -> CompiledProgram {
        let mut text = ScalarConstraints::new(ValueKind::String);
        text.min_length = Some(0);
        text.max_length = Some(32);
        text.pattern = Some(String::from("^(a|é)*[0-9]?$"));
        text.format = Some(String::from("uuid"));
        let mut integer = ScalarConstraints::new(ValueKind::Integer);
        integer.min_i64 = Some(i64::MIN);
        integer.max_i64 = Some(i64::MAX);
        let mut decimal = ScalarConstraints::new(ValueKind::Decimal);
        decimal.decimal = Some(DecimalConstraints {
            policy: DecimalPolicy::spot_8(), minimum: Some(Decimal128::new(-123, 8)),
            maximum: Some(Decimal128::new(456, 8)),
        });
        raw(vec![
            TypeNode::Object(ObjectShape {
                properties: vec![property("g", 6, 8, false, None),
                    property("f", 5, 7, false, None), property("e", 4, 6, false, Some(2)),
                    property("d", 3, 5, false, None), property("c", 2, 3, false, None),
                    property("b", u32::from(u16::MAX), 2, true, Some(0)),
                    property("a", 0, 1, true, Some(1))],
                reject_unknown: true, min_properties: Some(0), max_properties: Some(u32::MAX),
            }),
            TypeNode::Scalar(text), TypeNode::Scalar(integer),
            TypeNode::Array(ArrayShape { items: TypeId(4), min_items: Some(0), max_items: Some(u32::MAX) }),
            TypeNode::Scalar(decimal), object(Vec::new()), TypeNode::Any,
            patterned("^(a|é)*[0-9]?$"), patterned("^.$"),
        ], options)
    }

    fn error_is(error: Error, code: ErrorCode) -> Error {
        assert_eq!(error.code, code, "{}", error.detail);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert!(!error.detail.is_empty());
        error
    }

    fn same_program(left: &CompiledProgram, right: &CompiledProgram) {
        assert_eq!(left.image.header, right.image.header);
        assert_eq!(left.image.insns, right.image.insns);
        assert_eq!(left.image.keys, right.image.keys);
        assert_eq!(left.image.key_bytes, right.image.key_bytes);
        assert_eq!(left.states, right.states);
        assert_eq!(left.key_tables, right.key_tables);
        assert_eq!(left.patterns, right.patterns);
        assert_eq!(left.slots, right.slots);
        assert_eq!(left.policy, right.policy);
        assert_eq!(left.format_options, right.format_options);
        assert_eq!(left.canonical_schema, right.canonical_schema);
    }

    fn roundtrip(program: CompiledProgram, options: &CompilerOptions) -> ValidatedProgram {
        let len = encoded_len(&program).unwrap();
        let validated = validate(program.clone(), options).unwrap();
        let bytes = encode(&validated, options.max_image_bytes).unwrap();
        assert_eq!(bytes.len(), len);
        let decoded = decode(&bytes, options).unwrap();
        same_program(&program, decoded.program());
        assert_eq!(encode(&decoded, options.max_image_bytes).unwrap(), bytes);
        assert_eq!(cache_key(&validated, options.max_image_bytes).unwrap(),
            cache_key(&decoded, options.max_image_bytes).unwrap());
        validated
    }

    fn reject_with(program: CompiledProgram, options: &CompilerOptions, code: ErrorCode) {
        let bytes = encode_raw(&program, u32::MAX).unwrap();
        let id = program.image.header.schema_id;
        let error = error_is(validate(program, options).unwrap_err(), code);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(error.schema_id, id);
        error_is(decode(&bytes, options).unwrap_err(), code);
    }

    fn reject(program: CompiledProgram) {
        reject_with(program, &CompilerOptions::default(), ErrorCode::SchemaImageInvalid);
    }

    fn refresh_fingerprint(program: &mut CompiledProgram) {
        program.image.header.fingerprint =
            jmax_schema_ir::canonical::fingerprint_bytes(&program.canonical_schema);
    }

    #[test]
    fn exact_any_canonical_fingerprint_and_complete_wire_vector() {
        let options = CompilerOptions::default();
        let program = any();
        assert_eq!(program.canonical_schema, ANY_CANONICAL);
        assert_eq!(program.image.header.fingerprint, ANY_FINGERPRINT);
        assert_eq!(encoded_len(&program).unwrap(), 164);
        let witnessed = roundtrip(program, &options);
        assert_eq!(encode(&witnessed, 164).unwrap(), ANY_WIRE);
        same_program(witnessed.program(), decode(ANY_WIRE, &options).unwrap().program());
        let key = cache_key(&witnessed, 164).unwrap();
        assert_eq!(key.fingerprint, ANY_FINGERPRINT);
        assert_eq!(key.image_version, 1);
        assert_eq!(key.bytes, ANY_WIRE);
    }

    #[test]
    fn mixed_graph_integrates_keys_patterns_slots_bounds_and_formats() {
        let options = CompilerOptions::default();
        let validated = roundtrip(fixture(&options), &options);
        let program = validated.program();
        assert_eq!(program.states.len(), 9);
        assert_eq!(program.key_tables.len(), 2);
        assert_eq!(program.patterns.len(), 2);
        assert_eq!(program.slots.len(), 3);
        assert_eq!(program.image.keys[1].field_id, u16::MAX);
        assert_eq!(program.slots[0].field_id, FieldId(u32::from(u16::MAX)));
        assert_eq!(program.states[1].constraints.pattern, Some(0));
        assert_eq!(program.states[6].constraints.pattern, Some(0));
        assert_eq!(program.states[1].constraints.format, Some(FormatId::Uuid));
        assert_eq!(program.states[2].constraints.min_i64, Some(i64::MIN));
        assert_eq!(program.states[2].constraints.max_i64, Some(i64::MAX));
        for table in &program.key_tables { keys::validate(&program.image, table).unwrap(); }
        let table = &program.key_tables[0];
        assert_eq!(keys::lookup(&program.image, table, b"b").unwrap().unwrap().field_id, u16::MAX);
        assert!(keys::lookup(&program.image, table, b"missing").unwrap().is_none());
        for (text, expected) in [("", true), ("aé7", true), ("éé", true), ("b", false), ("12", false)] {
            let mut steps = 100_000;
            assert_eq!(patterns::is_match(&program.patterns[0], text, &mut steps).unwrap(), expected);
        }
    }

    #[test]
    fn every_scalar_kind_and_extreme_constraint_representation_roundtrips() {
        let options = CompilerOptions::default();
        for kind in [ValueKind::Null, ValueKind::Boolean, ValueKind::Integer,
            ValueKind::Number, ValueKind::Decimal, ValueKind::String]
        {
            let mut scalar = ScalarConstraints::new(kind);
            match kind {
                ValueKind::Integer => { scalar.min_i64 = Some(i64::MIN); scalar.max_i64 = Some(i64::MAX); }
                ValueKind::Decimal => scalar.decimal = Some(DecimalConstraints {
                    policy: DecimalPolicy { max_precision: 38, target_scale: 38,
                        allow_exponent: true, reject_inexact_rescale: false, tick_step: 0 },
                    minimum: Some(Decimal128::new(i128::MIN, 38)),
                    maximum: Some(Decimal128::new(i128::MAX, 38)),
                }),
                ValueKind::String => { scalar.min_length = Some(0); scalar.max_length = Some(u32::MAX); }
                _ => {}
            }
            let program = raw(vec![TypeNode::Scalar(scalar)], &options);
            let validated = roundtrip(program, &options);
            assert_eq!(validated.program().states[0].kind, StateKind::Scalar(kind));
        }
    }

    #[test]
    fn all_format_ids_and_unknown_annotations_preserve_identity() {
        for number in 1u16..=10 {
            let options = CompilerOptions::default();
            let format = FormatId::from_u16(number).unwrap();
            let mut scalar = ScalarConstraints::new(ValueKind::String);
            scalar.format = Some(String::from(format.name()));
            let validated = roundtrip(raw(vec![TypeNode::Scalar(scalar)], &options), &options);
            assert_eq!(validated.program().states[0].constraints.format, Some(format));
        }
        let mut options = CompilerOptions::default();
        options.policy.unknown_formats = UnknownFormatPolicy::Annotate;
        let mut keys = Vec::new();
        for name in ["", "UUID", "custom/é", "a-name-longer-than-the-format-registry-maximum-name-length-of-sixty-four-bytes"] {
            let mut scalar = ScalarConstraints::new(ValueKind::String);
            scalar.format = Some(String::from(name));
            let validated = roundtrip(raw(vec![TypeNode::Scalar(scalar)], &options), &options);
            assert_eq!(validated.program().states[0].constraints.format, None);
            keys.push(cache_key(&validated, options.max_image_bytes).unwrap());
        }
        for pair in keys.windows(2) { assert_ne!(pair[0], pair[1]); }
    }

    #[test]
    fn all_pattern_op_variants_survive_wire_encoding() {
        let options = CompilerOptions::default();
        let mut seen = [false; 8];
        for source in ["", "^(()|a)*[é-ω]?$", "^(a|.)+$", "^[^a-z]*$", "[]", "[^]"] {
            let program = roundtrip(raw(vec![patterned(source)], &options), &options);
            for op in &program.program().patterns[0].ops {
                seen[match op {
                    PatternOp::Char { .. } => 0, PatternOp::Class { .. } => 1,
                    PatternOp::Any { .. } => 2, PatternOp::Split { .. } => 3,
                    PatternOp::Jump { .. } => 4, PatternOp::AssertStart { .. } => 5,
                    PatternOp::AssertEnd { .. } => 6, PatternOp::Accept => 7,
                }] = true;
            }
        }
        assert_eq!(seen, [true; 8]);
    }

    #[test]
    fn swar_phf_fallback_and_rebased_tables_have_exact_vectors() {
        let mut options = CompilerOptions::default();
        options.swar_threshold = 0;
        let nodes = vec![object(vec![property("a", 1, 1, true, None), property("b", 2, 1, false, None)]),
            object(vec![property("aa", 3, 2, false, None), property("d", 4, 2, true, None)]), TypeNode::Any];
        let validated = roundtrip(raw(nodes.clone(), &options), &options);
        let program = validated.program();
        assert_eq!(program.key_tables[0].strategy, KeyStrategy::PerfectHash {
            seed: 0, slots: vec![0, 1, EMPTY_KEY_SLOT, EMPTY_KEY_SLOT],
        });
        assert_eq!(program.key_tables[1].strategy, KeyStrategy::PerfectHash {
            seed: 1, slots: vec![3, EMPTY_KEY_SLOT, 2, EMPTY_KEY_SLOT],
        });
        assert_eq!(program.image.key_bytes, b"abaad");
        assert_eq!(program.image.keys[2].span, Span32::new(2, 2));
        assert_eq!(keys::lookup(&program.image, &program.key_tables[1], b"aa").unwrap().unwrap().field_id, 3);
        options.max_hash_attempts = 1;
        let fallback = roundtrip(raw(nodes.clone(), &options), &options);
        assert_eq!(fallback.program().key_tables[1].strategy, KeyStrategy::Swar);
        options.max_hash_attempts = 0;
        let swar = roundtrip(raw(nodes, &options), &options);
        assert!(swar.program().key_tables.iter().all(|table| table.strategy == KeyStrategy::Swar));
    }

    #[test]
    fn empty_unicode_nul_and_equal_swar_prefix_keys_roundtrip() {
        let options = CompilerOptions::default();
        let names = ["", "\0", "a", "a\0", "abcdefghx", "abcdefghy", "e\u{301}", "é", "😀"];
        let properties = names.iter().enumerate().map(|(field, name)| {
            property(name, u32::try_from(field).unwrap(), 1, field % 2 == 0, None)
        }).collect();
        let program = roundtrip(raw(vec![object(properties), TypeNode::Any], &options), &options);
        for (field, name) in names.iter().enumerate() {
            let entry = keys::lookup(program.image(), &program.program().key_tables[0], name.as_bytes()).unwrap().unwrap();
            assert_eq!(usize::from(entry.field_id), field);
        }
        assert!(keys::lookup(program.image(), &program.program().key_tables[0], b"abcdefghz").unwrap().is_none());
    }

    #[test]
    fn complete_u16_field_domain_and_required_sentinel_roundtrip() {
        let mut options = CompilerOptions::default();
        options.swar_threshold = u16::MAX;
        options.max_hash_attempts = 0;
        let mut properties = Vec::new();
        for field in 0..=u32::from(u16::MAX) {
            properties.push(property(&format!("p{field:05}"), field, 1,
                field < u32::from(u16::MAX), None));
        }
        let validated = roundtrip(raw(vec![object(properties), TypeNode::Any], &options), &options);
        assert_eq!(validated.image().header.key_count, 65_536);
        assert!(matches!(validated.program().states[0].kind,
            StateKind::Object { required_count: u16::MAX, .. }));
        assert_eq!(validated.image().keys[65_534].required_bit, 65_534);
        assert_eq!(validated.image().keys[65_535].required_bit, NO_REQUIRED_BIT);
        assert_eq!(validated.image().keys[65_535].field_id, u16::MAX);
    }

    #[test]
    fn aliases_sharing_recursion_and_property_permutations_are_canonical() {
        let options = CompilerOptions::default();
        let mut input = schema(vec![TypeNode::Ref(String::from("root")), TypeNode::Any,
            object(vec![property("z", 1, 4, false, None), property("a", 2, 3, true, None)]),
            array(2), TypeNode::Ref(String::from("text")),
            TypeNode::Scalar(ScalarConstraints::new(ValueKind::String))]);
        input.definitions = vec![NamedType { name: String::from("root"), ty: TypeId(2) },
            NamedType { name: String::from("text"), ty: TypeId(5) }];
        let first = roundtrip(lower::lower(&resolved(input.clone()), &options).unwrap(), &options);
        assert_eq!(first.program().states.len(), 3);
        assert_eq!(first.program().states[1].kind, StateKind::Array { items: StateId(0) });
        if let TypeNode::Object(shape) = &mut input.nodes[2] { shape.properties.reverse(); }
        input.definitions.reverse();
        let second = roundtrip(lower::lower(&resolved(input), &options).unwrap(), &options);
        assert_eq!(cache_key(&first, options.max_image_bytes).unwrap(), cache_key(&second, options.max_image_bytes).unwrap());
        for nodes in [vec![array(0)], vec![object(vec![property("next", 99, 0, false, None)])]] {
            roundtrip(raw(nodes, &options), &options);
        }
    }

    #[test]
    fn deep_graph_and_long_names_use_bounded_iterative_reconstruction() {
        let options = CompilerOptions::default();
        let mut nodes = Vec::new();
        for target in 1..=2048 { nodes.push(array(target)); }
        nodes.push(TypeNode::Any);
        let program = roundtrip(raw(nodes, &options), &options);
        assert_eq!(program.program().states.len(), 2049);
        let name = "é".repeat(4097);
        let mut input = schema(vec![object(vec![property(&name, 0, 1, true, Some(0))]), TypeNode::Any]);
        input.name = name;
        roundtrip(lower::lower(&resolved(input), &options).unwrap(), &options);
    }

    #[test]
    fn byte_state_instruction_key_and_total_pattern_limits_are_inclusive() {
        let default = CompilerOptions::default();
        let baseline = fixture(&default);
        let bytes = encode_raw(&baseline, default.max_image_bytes).unwrap();
        let mut exact = default;
        exact.max_image_bytes = u32::try_from(bytes.len()).unwrap();
        exact.limits.max_states = u32::try_from(baseline.states.len()).unwrap();
        exact.limits.max_insns = baseline.image.header.insn_count;
        exact.limits.max_keys = baseline.image.header.key_count;
        exact.limits.max_pattern_states = baseline.patterns.iter().map(|p| u32::try_from(p.ops.len()).unwrap()).sum();
        let validated = roundtrip(baseline.clone(), &exact);
        assert_eq!(encode(&validated, exact.max_image_bytes).unwrap(), bytes);
        error_is(encode(&validated, exact.max_image_bytes - 1).unwrap_err(), ErrorCode::BudgetExhausted);
        error_is(cache_key(&validated, exact.max_image_bytes - 1).unwrap_err(), ErrorCode::BudgetExhausted);
        for component in 0..5 {
            let mut short = exact;
            match component {
                0 => short.max_image_bytes -= 1, 1 => short.limits.max_states -= 1,
                2 => short.limits.max_insns -= 1, 3 => short.limits.max_keys -= 1,
                _ => short.limits.max_pattern_states -= 1,
            }
            reject_with(baseline.clone(), &short, ErrorCode::BudgetExhausted);
        }
    }

    #[test]
    fn zero_limits_reject_only_present_resources() {
        let mut options = CompilerOptions::default();
        options.limits.max_states = 1;
        options.limits.max_insns = 1;
        options.limits.max_keys = 0;
        options.limits.max_pattern_states = 0;
        options.max_pattern_steps = 0;
        roundtrip(any(), &options);
        options.limits.max_states = 0;
        reject_with(any(), &options, ErrorCode::BudgetExhausted);
        options.limits.max_states = 1;
        options.limits.max_insns = 0;
        reject_with(any(), &options, ErrorCode::BudgetExhausted);
        options.limits.max_insns = 1;
        options.max_image_bytes = 0;
        reject_with(any(), &options, ErrorCode::BudgetExhausted);
    }

    #[test]
    fn encoded_len_only_counts_even_semantically_invalid_programs() {
        let mut program = any();
        program.image.header.magic = 0;
        program.image.insns[0].opcode = u8::MAX;
        program.canonical_schema.clear();
        assert_eq!(encoded_len(&program).unwrap(), ANY_WIRE.len() - ANY_CANONICAL.len());
        program.image.key_bytes.extend_from_slice(b"abc");
        assert_eq!(encoded_len(&program).unwrap(), ANY_WIRE.len() - ANY_CANONICAL.len() + 3);
        assert_eq!(encode_raw(&program, u32::MAX).unwrap().len(), encoded_len(&program).unwrap());
        reject(program);
    }

    #[test]
    fn every_header_invariant_is_checked() {
        for component in 0..8 {
            let mut program = any();
            match component {
                0 => program.image.header.magic ^= 1, 1 => program.image.header.version += 1,
                2 => program.image.header.flags = 1, 3 => program.image.header.root_state = 1,
                4 => program.image.header.insn_count += 1, 5 => program.image.header.key_count += 1,
                6 => program.image.header.slot_count += 1, _ => program.image.header.fingerprint ^= 1,
            }
            reject(program);
        }
        let mut program = any();
        program.image.header.schema_id ^= 1;
        reject(program);
    }

    #[test]
    fn dense_states_nonempty_ranges_and_complete_coverage_are_required() {
        for component in 0..7 {
            let mut program = fixture(&CompilerOptions::default());
            match component {
                0 => program.states[1].id = StateId(0),
                1 => program.states[0].insns.len = 0,
                2 => program.states[1].insns.offset -= 1,
                3 => program.states[1].insns.offset += 1,
                4 => program.states[0].insns.len = u32::MAX,
                5 => { program.states.pop(); }
                _ => { program.image.insns.push(InsnWord::default()); program.image.header.insn_count += 1; }
            }
            reject(program);
        }
        let mut program = any();
        program.states.clear();
        reject(program);
    }

    #[test]
    fn every_instruction_byte_and_operand_is_bound_to_canonical_lowering() {
        let baseline = fixture(&CompilerOptions::default());
        for index in 0..baseline.image.insns.len() {
            for component in 0..5 {
                let mut program = baseline.clone();
                let word = &mut program.image.insns[index];
                match component {
                    0 => word.opcode = if word.opcode == opcode_byte(Opcode::CheckScalarKind) {
                        opcode_byte(Opcode::EnterObject)
                    } else { opcode_byte(Opcode::CheckScalarKind) },
                    1 => word.flags = 1, 2 => word.field_id ^= 1,
                    3 => word.arg0 ^= 1, _ => word.arg1 ^= 1,
                }
                reject(program);
            }
        }
        let mut program = any();
        program.image.insns[0].opcode = 255;
        reject(program);
    }

    #[test]
    fn instruction_order_cannot_be_changed_even_with_valid_opcodes() {
        let mut program = fixture(&CompilerOptions::default());
        let start = usize::try_from(program.states[1].insns.offset).unwrap();
        program.image.insns.swap(start + 1, start + 2);
        reject(program);
    }

    #[test]
    fn key_table_ownership_required_counts_and_graph_edges_are_checked() {
        for component in 0..8 {
            let mut program = fixture(&CompilerOptions::default());
            match component {
                0 => program.key_tables[0].first_key = 1,
                1 => program.key_tables[0].key_count -= 1,
                2 => program.key_tables[1].first_key = 0,
                3 => program.key_tables.push(KeyTable { first_key: 7, key_count: 0, strategy: KeyStrategy::Swar }),
                4 => { if let StateKind::Object { required_count, .. } = &mut program.states[0].kind { *required_count += 1; } }
                5 => { if let StateKind::Object { key_table, .. } = &mut program.states[0].kind { *key_table = 1; } }
                6 => program.image.keys[0].target_state = u32::MAX,
                _ => program.states[3].kind = StateKind::Array { items: StateId(u32::MAX) },
            }
            reject(program);
        }
    }

    #[test]
    fn key_metadata_and_entire_byte_storage_have_unique_owners() {
        let baseline = fixture(&CompilerOptions::default());
        for component in 0..9 {
            let mut program = baseline.clone();
            match component {
                0 => program.image.keys[0].swar_head ^= 1,
                1 => program.image.keys[0].span.offset = 1,
                2 => program.image.keys[0].span.len = u32::MAX,
                3 => program.image.keys[0].required_bit = 1,
                4 => program.image.keys[2].field_id = program.image.keys[1].field_id,
                5 => program.image.key_bytes.push(0),
                6 => { program.image.key_bytes[0] = 0xff; program.image.keys[0].swar_head = 0xff; }
                7 => { program.image.keys.swap(0, 1); }
                _ => program.image.keys[0].target_state = program.image.keys[1].target_state,
            }
            reject(program);
        }
        // Duplicate FieldIds in different tables are invalid even when each
        // individual table is otherwise locally well formed.
        let options = CompilerOptions::default();
        let mut program = raw(vec![object(vec![property("outer", 1, 1, false, None)]),
            object(vec![property("inner", 2, 2, false, None)]), TypeNode::Any], &options);
        program.image.keys[1].field_id = 1;
        keys::validate(&program.image, &program.key_tables[1]).unwrap();
        reject(program);
    }

    #[test]
    fn structurally_valid_alternate_phf_seed_or_strategy_is_not_a_witness() {
        let mut options = CompilerOptions::default();
        options.swar_threshold = 0;
        let baseline = raw(vec![object(vec![property("a", 1, 1, false, None)]), TypeNode::Any], &options);
        let mut alternate = baseline.clone();
        let mut slots = vec![EMPTY_KEY_SLOT; 2];
        slots[usize::try_from(key_hash(b"a", 1) & 1).unwrap()] = 0;
        alternate.key_tables[0].strategy = KeyStrategy::PerfectHash { seed: 1, slots };
        keys::validate(&alternate.image, &alternate.key_tables[0]).unwrap();
        reject_with(alternate, &options, ErrorCode::SchemaImageInvalid);
        let mut swar = baseline;
        swar.key_tables[0].strategy = KeyStrategy::Swar;
        swar.image.insns[1].opcode = opcode_byte(Opcode::MatchKeySwar);
        keys::validate(&swar.image, &swar.key_tables[0]).unwrap();
        reject_with(swar, &options, ErrorCode::SchemaImageInvalid);
    }

    #[test]
    fn malformed_phf_capacities_slots_and_seed_placement_are_rejected() {
        let mut options = CompilerOptions::default();
        options.swar_threshold = 0;
        let baseline = raw(vec![object(vec![property("a", 1, 1, false, None)]), TypeNode::Any], &options);
        for component in 0..5 {
            let mut program = baseline.clone();
            if let KeyStrategy::PerfectHash { slots, .. } = &mut program.key_tables[0].strategy {
                match component {
                    0 => slots.clear(), 1 => { slots.pop(); }
                    2 => { slots.extend_from_slice(&[EMPTY_KEY_SLOT; 2]); }
                    3 => slots.fill(0), _ => slots.fill(u32::MAX - 1),
                }
            }
            reject_with(program, &options, ErrorCode::SchemaImageInvalid);
        }
    }

    #[test]
    fn slots_must_match_field_occurrences_and_dense_user_assignments() {
        let baseline = fixture(&CompilerOptions::default());
        for component in 0..6 {
            let mut program = baseline.clone();
            match component {
                0 => program.slots[0].slot = 1,
                1 => program.slots[0].field_id = FieldId(u32::MAX),
                2 => program.slots[0].field_id = program.slots[1].field_id,
                3 => program.slots[0].state = StateId(u32::MAX),
                4 => program.slots[0].state = program.slots[1].state,
                _ => { program.slots.swap(0, 1); }
            }
            reject(program);
        }
        let mut program = baseline;
        program.slots.clear();
        program.image.header.slot_count = 0;
        reject(program);
    }

    #[test]
    fn constraints_cannot_be_incompatible_contradictory_or_silently_changed() {
        for component in 0..10 {
            let mut program = fixture(&CompilerOptions::default());
            match component {
                0 => program.states[0].constraints.min_i64 = Some(0),
                1 => program.states[1].constraints.min_count = Some(33),
                2 => program.states[1].constraints.pattern = Some(u32::MAX),
                3 => program.states[1].constraints.pattern = Some(1),
                4 => program.states[1].constraints.format = Some(FormatId::Hex),
                5 => program.states[2].constraints.min_i64 = Some(1),
                6 => program.states[2].constraints.max_i64 = Some(i64::MIN),
                7 => program.states[3].constraints.max_count = Some(0),
                8 => program.states[8].constraints.decimal = None,
                _ => { if let StateKind::Object { reject_unknown, .. } = &mut program.states[0].kind { *reject_unknown = false; } }
            }
            reject(program);
        }
    }

    #[test]
    fn policy_and_format_options_are_frozen_in_version_one() {
        for component in 0..6 {
            let mut program = any();
            match component {
                0 => program.policy.allow_recursive = false,
                1 => program.format_options.max_input_bytes += 1,
                2 => program.format_options.nanoid_len = 0,
                3 => program.format_options.hex.case = HexCase::Upper,
                4 => program.format_options.hex.exact_bytes = Some(0),
                _ => program.format_options.base64_padding = Padding::Optional,
            }
            reject(program);
        }
        let mut options = CompilerOptions::default();
        options.policy.dialect = Dialect::JsonSchema202012;
        options.policy.numeric.allow_Quoted_int = true;
        options.policy.numeric.reject_negative_zero = false;
        options.policy.numeric.max_exponent_abs = 0;
        options.policy.allow_recursive = false;
        roundtrip(raw(vec![TypeNode::Any], &options), &options);
    }

    #[test]
    fn malformed_patterns_and_valid_but_wrong_pattern_languages_are_rejected() {
        let options = CompilerOptions::default();
        let baseline = raw(vec![patterned("a")], &options);
        for component in 0..7 {
            let mut program = baseline.clone();
            match component {
                0 => program.patterns[0].version += 1,
                1 => program.patterns[0].start = u32::MAX,
                2 => program.patterns[0].max_steps = 0,
                3 => program.patterns[0].max_steps -= 1,
                4 => program.patterns[0].ops[0] = PatternOp::Char { scalar: 0xd800, next: 1 },
                5 => program.patterns[0].ops[0] = PatternOp::Jump { next: u32::MAX },
                _ => program.patterns[0] = patterns::compile("b", options.limits.max_pattern_states, options.max_pattern_steps).unwrap(),
            }
            reject(program);
        }
        let mut extra = baseline;
        extra.patterns.push(patterns::compile("", 2, options.max_pattern_steps).unwrap());
        reject(extra);
    }

    #[test]
    fn fingerprint_equality_does_not_authenticate_executable_or_canonical_bytes() {
        let mut changed_code = any();
        changed_code.image.insns[0].arg1 = 0;
        assert_eq!(changed_code.image.header.fingerprint, ANY_FINGERPRINT);
        reject(changed_code);
        let mut changed_canonical = any();
        changed_canonical.canonical_schema[38] = 1;
        refresh_fingerprint(&mut changed_canonical);
        reject(changed_canonical);
        let options = CompilerOptions::default();
        let mut new_source = raw(vec![patterned("a")], &options);
        let end = new_source.canonical_schema.len();
        // Scalar pattern string is followed only by its absent format option.
        assert_eq!(new_source.canonical_schema[end - 2], b'a');
        new_source.canonical_schema[end - 2] = b'b';
        refresh_fingerprint(&mut new_source);
        patterns::validate(&new_source.patterns[0], options.limits.max_pattern_states).unwrap();
        reject(new_source);
    }

    #[test]
    fn canonical_compiler_failures_become_image_errors_with_field_context() {
        let options = CompilerOptions::default();
        for nodes in [
            vec![array(1), object(vec![property("value", 99, 2, false, Some(0))]), TypeNode::Any],
            vec![object(vec![property("value", 99, 1, false, Some(1))]), TypeNode::Any],
            vec![object(vec![property("value", 65_536, 1, false, None)]), TypeNode::Any],
        ] {
            // These are valid IR graphs, but their capture paths, sparse slots,
            // or field domain cannot be represented by the compiler ABI.
            let canonical = jmax_schema_ir::canonical::canonicalize(&resolved(schema(nodes)), &options.policy).unwrap();
            let mut program = any();
            program.canonical_schema = canonical.bytes;
            program.image.header.fingerprint = canonical.fingerprint;
            let bytes = encode_raw(&program, options.max_image_bytes).unwrap();
            let error = error_is(validate(program, &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
            assert!(error.field_id == 99 || error.field_id == 65_536);
            assert_eq!(error.schema_id, SCHEMA_ID);
            assert_eq!(error.byte_offset, 0);
            error_is(decode(&bytes, &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
        }
        for (original, replacement) in [("a", "("), ("aa", "\\d")] {
            let mut program = raw(vec![patterned(original)], &options);
            let end = program.canonical_schema.len() - 1;
            program.canonical_schema[end - original.len()..end].copy_from_slice(replacement.as_bytes());
            refresh_fingerprint(&mut program);
            reject(program);
        }
    }

    #[test]
    fn canonical_prefix_version_tags_counts_and_utf8_are_strict() {
        for component in 0..8 {
            let mut program = any();
            match component {
                0 => program.canonical_schema[0] ^= 1,
                1 => program.canonical_schema[12] = 2,
                2 => program.canonical_schema[14] = 2,
                3 => program.canonical_schema[15] = 2,
                4 => program.canonical_schema[38] = 4,
                5 => program.canonical_schema[34..38].copy_from_slice(&0u32.to_le_bytes()),
                6 => program.canonical_schema[26..30].copy_from_slice(&u32::MAX.to_le_bytes()),
                _ => program.canonical_schema.push(0),
            }
            refresh_fingerprint(&mut program);
            reject(program);
        }
        let mut input = schema(vec![TypeNode::Any]);
        input.name = String::from("x");
        let mut program = lower::lower(&resolved(input), &CompilerOptions::default()).unwrap();
        program.canonical_schema[30] = 0xff;
        refresh_fingerprint(&mut program);
        reject(program);
    }

    #[test]
    fn unreachable_or_non_bfs_canonical_graphs_are_not_accepted() {
        let mut unreachable = any();
        unreachable.canonical_schema[34..38].copy_from_slice(&2u32.to_le_bytes());
        unreachable.canonical_schema.push(0);
        refresh_fingerprint(&mut unreachable);
        reject(unreachable);
        let options = CompilerOptions::default();
        let mut reordered = raw(vec![array(1), TypeNode::Any], &options);
        // Canonical node 0 is array(tag, target, absent min, absent max).
        // Exchange the two records and retarget the array to Any at index 0.
        let node_start = 38;
        assert_eq!(reordered.canonical_schema[node_start], 3);
        reordered.canonical_schema.splice(node_start.., [0, 3, 0, 0, 0, 0, 0, 0]);
        refresh_fingerprint(&mut reordered);
        reject(reordered);
    }

    #[test]
    fn changed_canonical_slots_and_repeated_capture_paths_are_rejected() {
        let options = CompilerOptions::default();
        let mut selected = raw(vec![object(vec![property("next", 7, 1, false, Some(0))]), TypeNode::Any], &options);
        // Object: tag, reject bool, two absent counts, property count;
        // property: FieldId, string length, four name bytes, then target.
        let target_offset = 38 + 8 + 4 + 4 + 4;
        selected.canonical_schema[target_offset..target_offset + 4].copy_from_slice(&0u32.to_le_bytes());
        // Drop the now unreachable Any record and make the canonical graph a
        // one-node self-recursive object. Its selected occurrence can repeat.
        selected.canonical_schema.pop();
        selected.canonical_schema[34..38].copy_from_slice(&1u32.to_le_bytes());
        refresh_fingerprint(&mut selected);
        reject(selected);
    }

    #[test]
    fn every_truncated_wire_prefix_and_trailing_byte_is_rejected() {
        let options = CompilerOptions::default();
        for bytes in [ANY_WIRE.to_vec(), encode_raw(&fixture(&options), options.max_image_bytes).unwrap()] {
            for end in 0..bytes.len() {
                error_is(decode(&bytes[..end], &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
            }
            let mut trailing = bytes.clone();
            trailing.push(0);
            let error = error_is(decode(&trailing, &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
            assert_eq!(usize::try_from(error.byte_offset).unwrap(), bytes.len());
        }
    }

    #[test]
    fn wire_unknown_tags_booleans_options_and_counts_fail_at_their_positions() {
        let options = CompilerOptions::default();
        // Positions in the independent Any wire vector, never native offsets.
        for (position, value) in [(0, 0), (4, 2), (6, 1), (40, 255), (72, 4),
            (81, 2), (100, 2), (101, 2), (106, 2), (107, 2), (117, 3),
            (118, 2), (119, 2), (120, 3)]
        {
            let mut bytes = ANY_WIRE.to_vec();
            bytes[position] = value;
            let error = error_is(decode(&bytes, &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
            assert_eq!(usize::try_from(error.byte_offset).unwrap(), position);
        }
        for position in [36, 56, 60, 64, 88, 92, 96, 121] {
            let mut bytes = ANY_WIRE.to_vec();
            bytes[position..position + 4].copy_from_slice(&u32::MAX.to_le_bytes());
            let error = error_is(decode(&bytes, &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
            assert_eq!(usize::try_from(error.byte_offset).unwrap(), position);
        }
        let mut short = options;
        short.limits.max_insns = 0;
        let error = error_is(decode(ANY_WIRE, &short).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_eq!(error.byte_offset, 36);
        assert_eq!(error.schema_id, SCHEMA_ID);
    }

    #[test]
    fn key_entry_wire_vector_has_no_native_padding_and_unknown_strategy_fails() {
        let options = CompilerOptions::default();
        let program = raw(vec![object(vec![property("a", 0x1234, 1, true, None)]), TypeNode::Any], &options);
        let bytes = encode_raw(&program, options.max_image_bytes).unwrap();
        // Header(36), instruction count(4), four 16-byte instructions,
        // key count(4), then a 24-byte ABI entry in declaration order.
        assert_eq!(program.image.insns.len(), 4);
        assert_eq!(&bytes[108..132], &[
            97, 0, 0, 0, 0, 0, 0, 0, // SWAR head
            0, 0, 0, 0, 1, 0, 0, 0, // byte span
            0x34, 0x12, 0, 0, 1, 0, 0, 0, // field, required bit, target
        ]);
        // Key bytes(5), states count(4), Object state(27), Any state(20),
        // key table count(4), first_key/key_count(8), strategy tag.
        let strategy_position = 132 + 5 + 4 + 27 + 20 + 4 + 8;
        assert_eq!(bytes[strategy_position], 0);
        let mut malformed = bytes;
        malformed[strategy_position] = 2;
        let error = error_is(decode(&malformed, &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
        assert_eq!(usize::try_from(error.byte_offset).unwrap(), strategy_position);
    }

    #[test]
    fn schema_and_field_context_survive_truncated_wire_records() {
        let options = CompilerOptions::default();
        let bytes = encode_raw(&fixture(&options), options.max_image_bytes).unwrap();
        // Every post-header truncation has a known schema identifier.
        for end in [12, 35, 40, bytes.len() - 1] {
            let error = error_is(decode(&bytes[..end], &options).unwrap_err(), ErrorCode::SchemaImageInvalid);
            assert_eq!(error.schema_id, SCHEMA_ID);
            assert!(usize::try_from(error.byte_offset).unwrap() <= end);
        }
        let mut reader = Reader::new(&[0, 0, 0, 0]);
        reader.schema_id = SCHEMA_ID;
        reader.field_id = 77;
        reader.u32().unwrap();
        let error = error_is(reader.u8().unwrap_err(), ErrorCode::SchemaImageInvalid);
        assert_eq!(error.byte_offset, 4);
        assert_eq!(error.schema_id, SCHEMA_ID);
        assert_eq!(error.field_id, 77);
    }

    #[test]
    fn low_level_wire_enums_are_exhaustive_and_unknown_tags_fail() {
        for value in [6, 255] {
            error_is(Reader::new(&[value]).value_kind().unwrap_err(), ErrorCode::SchemaImageInvalid);
        }
        for value in [0u16, 11, u16::MAX] {
            error_is(Reader::new(&value.to_le_bytes()).format_id().unwrap_err(), ErrorCode::SchemaImageInvalid);
        }
        for value in [8, 255] {
            error_is(Reader::new(&[value]).pattern_op(1024).unwrap_err(), ErrorCode::SchemaImageInvalid);
        }
        for value in [2, 255] {
            error_is(Reader::new(&[value]).boolean().unwrap_err(), ErrorCode::SchemaImageInvalid);
            error_is(Reader::new(&[value]).optional(Reader::u32).unwrap_err(), ErrorCode::SchemaImageInvalid);
        }
        let state = [0, 0, 0, 0, 1, 6];
        let error = error_is(Reader::new(&state).state().unwrap_err(), ErrorCode::SchemaImageInvalid);
        assert_eq!(error.byte_offset, 5);
    }

    #[test]
    fn reader_rejects_noncanonical_derived_counts_before_allocation() {
        let bytes = [3, 0, 0, 0, 10, 0, 0, 0, 20, 0, 0, 0, 30, 0, 0, 0];
        let error = error_is(Reader::new(&bytes).exact_vector(4, 2, Reader::u32).unwrap_err(), ErrorCode::SchemaImageInvalid);
        assert_eq!(error.byte_offset, 0);
        assert_eq!(Reader::new(&bytes).exact_vector(4, 3, Reader::u32).unwrap(), [10, 20, 30]);
        let mut reader = Reader::new(&bytes);
        reader.schema_id = SCHEMA_ID;
        reader.field_id = 99;
        let error = error_is(reader.vector(4, 2, Reader::u32).unwrap_err(), ErrorCode::BudgetExhausted);
        assert_eq!(error.schema_id, SCHEMA_ID);
        assert_eq!(error.field_id, 99);
        let impossible = [255; 4];
        error_is(Reader::new(&impossible).vector(16, 0, Reader::u8).unwrap_err(), ErrorCode::SchemaImageInvalid);
        let malformed_utf8 = [2, 0, 0, 0, b'a', 0xff];
        let error = error_is(Reader::new(&malformed_utf8).string(2).unwrap_err(), ErrorCode::SchemaImageInvalid);
        assert_eq!(error.byte_offset, 5);
    }

    #[test]
    fn primitive_signed_decimal_policy_and_option_vectors_are_portable() {
        let mut writer = Writer { destination: Destination::Bytes(Vec::new()), position: 0 };
        writer.i16(i16::MIN).unwrap();
        writer.i64(i64::MIN).unwrap();
        writer.i128(i128::MIN).unwrap();
        writer.optional(Some(&u32::MAX), |w, value| w.u32(*value)).unwrap();
        writer.optional::<u32>(None, |w, value| w.u32(*value)).unwrap();
        let bytes = match writer.destination { Destination::Bytes(bytes) => bytes, _ => panic!("byte destination") };
        let mut expected = vec![0, 128];
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 128]);
        expected.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128]);
        expected.extend_from_slice(&[1, 255, 255, 255, 255, 0]);
        assert_eq!(bytes, expected);
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.i16().unwrap(), i16::MIN);
        assert_eq!(reader.i64().unwrap(), i64::MIN);
        assert_eq!(reader.i128().unwrap(), i128::MIN);
        assert_eq!(reader.optional(Reader::u32).unwrap(), Some(u32::MAX));
        assert_eq!(reader.optional(Reader::u32).unwrap(), None);
        reader.finish().unwrap();
        let decimal = DecimalConstraints {
            policy: DecimalPolicy { max_precision: 38, target_scale: 0, allow_exponent: true,
                reject_inexact_rescale: false, tick_step: i128::MAX },
            minimum: Some(Decimal128::new(i128::MIN, 0)), maximum: Some(Decimal128::new(i128::MAX, 38)),
        };
        let mut writer = Writer { destination: Destination::Bytes(Vec::new()), position: 0 };
        writer.decimal(&decimal).unwrap();
        let bytes = match writer.destination { Destination::Bytes(bytes) => bytes, _ => panic!("byte destination") };
        assert_eq!(bytes.len(), 56);
        assert_eq!(&bytes[..4], &[38, 0, 1, 0]);
        assert_eq!(Reader::new(&bytes).decimal().unwrap(), decimal);
    }

    #[test]
    fn all_hex_and_padding_wire_tags_follow_the_frozen_order() {
        for (case, case_tag) in [(HexCase::Any, 0), (HexCase::Lower, 1), (HexCase::Upper, 2)] {
            for (padding, padding_tag) in [(Padding::Required, 0), (Padding::Forbidden, 1), (Padding::Optional, 2)] {
                let options = FormatOptions { max_input_bytes: u32::MAX, nanoid_len: 255,
                    hex: HexPolicy { case, prefix: true, exact_bytes: Some(0) }, base64_padding: padding };
                let mut writer = Writer { destination: Destination::Bytes(Vec::new()), position: 0 };
                writer.format_options(&options).unwrap();
                let bytes = match writer.destination { Destination::Bytes(bytes) => bytes, _ => panic!("byte destination") };
                assert_eq!(bytes, [255, 255, 255, 255, 255, case_tag, 1, 1, 0, 0, 0, 0, padding_tag]);
                assert_eq!(Reader::new(&bytes).format_options().unwrap(), options);
            }
        }
    }

    #[test]
    fn cache_keys_compare_full_encoding_and_compiler_options() {
        let mut swar_options = CompilerOptions::default();
        swar_options.swar_threshold = 1;
        let mut phf_options = swar_options;
        phf_options.swar_threshold = 0;
        let nodes = vec![object(vec![property("a", 1, 1, false, None)]), TypeNode::Any];
        let swar = roundtrip(raw(nodes.clone(), &swar_options), &swar_options);
        let phf = roundtrip(raw(nodes, &phf_options), &phf_options);
        let first = cache_key(&swar, swar_options.max_image_bytes).unwrap();
        let second = cache_key(&phf, phf_options.max_image_bytes).unwrap();
        assert_eq!(first.fingerprint, second.fingerprint);
        assert_eq!(first.image_version, second.image_version);
        assert_ne!(first.bytes, second.bytes);
        assert_ne!(first, second);
        let mut collision = first.clone();
        collision.bytes[40] ^= 1;
        assert_eq!(collision.fingerprint, first.fingerprint);
        assert_ne!(collision, first);
        reject_with(phf.into_program(), &swar_options, ErrorCode::SchemaImageInvalid);
    }

    #[test]
    fn deterministic_schema_corpus_roundtrips_and_repeats_exactly() {
        let options = CompilerOptions::default();
        for seed in 0u32..32 {
            let mut properties = Vec::new();
            for field in (0..seed % 9).rev() {
                properties.push(property(&format!("key-{field:02}-é"), field, 1 + field % 3, field % 2 == 0, None));
            }
            let mut integer = ScalarConstraints::new(ValueKind::Integer);
            integer.min_i64 = (seed % 2 == 0).then_some(-i64::from(seed));
            integer.max_i64 = (seed % 3 == 0).then_some(i64::from(seed));
            let mut nodes = vec![object(properties), TypeNode::Scalar(integer),
                patterned(if seed % 2 == 0 { "^(a|é)*$" } else { "[0-9]+" }), array(1)];
            let first = roundtrip(raw(nodes.clone(), &options), &options);
            if let TypeNode::Object(shape) = &mut nodes[0] { shape.properties.reverse(); }
            let repeated = roundtrip(raw(nodes, &options), &options);
            assert_eq!(cache_key(&first, options.max_image_bytes).unwrap(), cache_key(&repeated, options.max_image_bytes).unwrap());
        }
    }

    #[test]
    fn checked_arithmetic_allocation_and_comparison_fail_without_panics() {
        error_is(reserved::<u64>(usize::MAX).unwrap_err(), ErrorCode::BudgetExhausted);
        let mut writer = Writer { destination: Destination::Count, position: usize::MAX };
        error_is(writer.u8(0).unwrap_err(), ErrorCode::BudgetExhausted);
        #[cfg(target_pointer_width = "64")]
        {
            error_is(count(u32::MAX as usize + 1).unwrap_err(), ErrorCode::BudgetExhausted);
            let mut writer = Writer { destination: Destination::Count, position: u32::MAX as usize };
            error_is(writer.u8(0).unwrap_err(), ErrorCode::BudgetExhausted);
        }
        let mut writer = Writer { destination: Destination::Compare(&[1, 2]), position: 0 };
        writer.u8(1).unwrap();
        error_is(writer.u8(3).unwrap_err(), ErrorCode::SchemaImageInvalid);
        assert_eq!(writer.position, 1);
        writer.u8(2).unwrap();
        error_is(writer.u8(0).unwrap_err(), ErrorCode::SchemaImageInvalid);
        let mut reader = Reader::new(&[]);
        reader.position = usize::MAX;
        error_is(reader.take(1).unwrap_err(), ErrorCode::BudgetExhausted);
    }
}
