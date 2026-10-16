//! Shared contracts for static schema images and generated ABI adapters.
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::{string::String, vec::Vec};
use core::fmt::Write;
pub use jmax_abi::buffer::ValidationProof;
pub use jmax_abi::error::{Error, ErrorCode, ErrorSource, Result};
pub use jmax_abi::number::Decimal128;
pub use jmax_abi::schema::{
    FieldId, InsnWord, KeyEntry, Opcode, SchemaFieldIr, SchemaId, SchemaImage, SchemaImageHeader,
    SchemaIr, SlotValue, StateId, ValidatedSlots,
};
pub use jmax_abi::span::{Span32, SpanSource, StrRef};
pub use jmax_abi::syntax::ScalarKind;
pub use jmax_abi::traits::{ByteSink, JsonDecode, JsonEncode, SchemaProvider};
pub use jmax_schema_compile::{CompiledProgram, CompilerOptions, ValidatedProgram};

pub mod decode;
pub mod encode;
pub mod tables;
pub mod verify;

pub const CONTRACT_VERSION: u16 = 1;
pub const FINGERPRINT_OFFSET: u64 = 0xcbf29ce484222325;
pub const FINGERPRINT_PRIME: u64 = 0x100000001b3;

/// Borrowed host-side tables, never a transmuted view of serialized bytes.
#[derive(Clone, Copy, Debug)]
pub struct StaticSchemaImage<'a> {
    pub header: SchemaImageHeader,
    pub insns: &'a [InsnWord],
    pub keys: &'a [KeyEntry],
    pub key_bytes: &'a [u8],
}

impl<'a> From<&'a SchemaImage> for StaticSchemaImage<'a> {
    fn from(image: &'a SchemaImage) -> Self {
        Self {
            header: image.header,
            insns: &image.insns,
            keys: &image.keys,
            key_bytes: &image.key_bytes,
        }
    }
}

/// Expected identity is supplied by the producer and checked against its image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageIdentity {
    pub magic: u32,
    pub version: u16,
    pub schema_id: SchemaId,
    pub fingerprint: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotKind {
    Bool,
    U64,
    I64,
    F64,
    Decimal,
    Str,
    Raw,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotSpec {
    pub index: u32,
    pub field_id: FieldId,
    pub kind: SlotKind,
    pub required: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct DecodePlan<'a> {
    pub schema_fingerprint: u64,
    pub slots: &'a [SlotSpec],
}

#[derive(Clone, Copy, Debug)]
pub struct EncodeField<'a> {
    pub wire_name: &'a str,
    pub slot: SlotSpec,
    pub omit_empty: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct TableOptions<'a> {
    pub symbol: &'a str,
    pub abi_path: &'a str,
    pub aot_path: &'a str,
}

impl Default for TableOptions<'static> {
    fn default() -> Self {
        Self {
            symbol: "JMAX_SCHEMA",
            abi_path: "::jmax_abi",
            aot_path: "::jmax_schema_aot",
        }
    }
}

/// Borrowed owner view used by the same decoder for owned and borrowed slots.
#[derive(Clone, Copy, Debug)]
pub struct SlotValues<'a> {
    pub raw: &'a [u8],
    pub decoded: &'a [u8],
    pub slots: &'a [SlotValue],
    pub proof: ValidationProof,
}

mod source_seal {
    pub trait Sealed {}
}

/// Audited slot owners accepted by generated decoder readers. Query-only
/// results deliberately do not implement this sealed trait.
pub trait SlotSource: source_seal::Sealed {
    fn slot_values(&self) -> SlotValues<'_>;
}
impl source_seal::Sealed for ValidatedSlots {}
impl SlotSource for ValidatedSlots {
    fn slot_values(&self) -> SlotValues<'_> {
        SlotValues {
            raw: self.payload.as_slice(),
            decoded: &self.decoded,
            slots: &self.slots,
            proof: self.proof,
        }
    }
}
impl source_seal::Sealed for jmax_schema_vm::BorrowedValidatedSlots<'_> {}
impl SlotSource for jmax_schema_vm::BorrowedValidatedSlots<'_> {
    fn slot_values(&self) -> SlotValues<'_> {
        SlotValues {
            raw: self.raw,
            decoded: &self.decoded,
            slots: &self.slots,
            proof: self.proof,
        }
    }
}
impl source_seal::Sealed for jmax_schema_vm::BorrowedValidatedSlotsRef<'_, '_> {}
impl SlotSource for jmax_schema_vm::BorrowedValidatedSlotsRef<'_, '_> {
    fn slot_values(&self) -> SlotValues<'_> {
        SlotValues {
            raw: self.raw,
            decoded: self.decoded,
            slots: self.slots,
            proof: self.proof,
        }
    }
}

/// A length-checked, fingerprint-checked reader for either ownership model.
/// Public ABI/result fields remain checked on every individual access.
#[derive(Clone, Copy, Debug)]
pub struct SlotReader<'a> {
    values: SlotValues<'a>,
}

/// Typed decoding from a checked reader without requiring an owned payload.
/// `JsonDecode` derives implement both this trait and the owned ABI trait.
/// Custom decoders can use the same implementation for either ownership model.
/// Returned values must own any data copied from the temporary decoded arena.
pub trait JsonDecodeBorrowed: Sized {
    fn decode_borrowed(reader: BorrowedSlotReader<'_>) -> Result<Self>;
}

/// Both readers share exactly the same checked access and decoder functions.
pub type BorrowedSlotReader<'a> = SlotReader<'a>;

impl<'a> SlotReader<'a> {
    pub fn new(values: &'a (impl SlotSource + ?Sized), expected_fingerprint: u64) -> Result<Self> {
        let values = values.slot_values();
        if values.proof.schema_fingerprint() != expected_fingerprint {
            return Err(aot_error(
                ErrorCode::SchemaImageInvalid,
                ErrorSource::Decode,
                "slot schema fingerprint mismatch",
            ));
        }
        if usize::try_from(values.proof.payload_len()).ok() != Some(values.raw.len())
            || usize::try_from(values.proof.decoded_len()).ok() != Some(values.decoded.len())
        {
            return Err(aot_error(
                ErrorCode::SchemaImageInvalid,
                ErrorSource::Decode,
                "slot owner lengths differ from proof",
            ));
        }
        Ok(Self { values })
    }

    pub fn values(&self) -> SlotValues<'a> {
        self.values
    }

    pub fn slot(&self, index: u32, field_id: FieldId) -> Result<SlotValue> {
        usize::try_from(index)
            .ok()
            .and_then(|index| self.values.slots.get(index))
            .copied()
            .ok_or_else(|| {
                aot_error(
                    ErrorCode::SchemaImageInvalid,
                    ErrorSource::Decode,
                    "slot index out of bounds",
                )
                .with_schema(0, field_id.0)
            })
    }

    pub fn resolve_str(&self, value: StrRef) -> Result<&'a str> {
        let bytes = match value.source {
            SpanSource::RawPayload => self.values.raw,
            SpanSource::DecodedArena => self.values.decoded,
        };
        let bytes = value.span.slice(bytes).ok_or_else(|| {
            aot_error(
                ErrorCode::SchemaImageInvalid,
                ErrorSource::Decode,
                "string span out of bounds",
            )
        })?;
        let text = core::str::from_utf8(bytes).map_err(|_| {
            Error::new(
                ErrorCode::InvalidUtf8,
                ErrorSource::Decode,
                value.span.offset,
                "slot string is not UTF-8",
            )
        })?;
        if value.scalar_len != jmax_utf8::count_scalars_trusted_utf8(text.as_bytes()) {
            return Err(aot_error(
                ErrorCode::SchemaImageInvalid,
                ErrorSource::Decode,
                "slot string scalar length mismatch",
            ));
        }
        Ok(text)
    }

    pub fn resolve_raw(&self, span: Span32) -> Result<&'a [u8]> {
        span.slice(self.values.raw).ok_or_else(|| {
            aot_error(
                ErrorCode::SchemaImageInvalid,
                ErrorSource::Decode,
                "raw slot span out of bounds",
            )
        })
    }
}

pub(crate) fn aot_error(code: ErrorCode, source: ErrorSource, detail: &'static str) -> Error {
    Error::new(code, source, 0, detail)
}

/// Normalize through the schema IR's authoritative canonical grammar.
pub fn fingerprint_schema(schema: &SchemaIr) -> Result<u64> {
    let schema = jmax_schema_ir::model::normalize(schema)?;
    let resolved =
        jmax_schema_ir::resolve::resolve(schema, jmax_schema_ir::ResolveLimits::default())?;
    jmax_schema_ir::canonical::fingerprint(&resolved, &jmax_schema_ir::SchemaPolicy::default())
}

/// Compile flat ABI IR with the same policy and identity used by schema providers.
pub fn compile_schema(schema: &SchemaIr) -> Result<ValidatedProgram> {
    let schema = jmax_schema_ir::model::normalize(schema)?;
    let resolved =
        jmax_schema_ir::resolve::resolve(schema, jmax_schema_ir::ResolveLimits::default())?;
    jmax_schema_compile::lower::compile(&resolved, &CompilerOptions::default())
}

/// Compile an already nested schema graph with the same path and options as
/// `compile_schema`. The input is cloned for resolution and never modified.
pub fn compile_graph(schema: &jmax_schema_ir::Schema) -> Result<ValidatedProgram> {
    let resolved =
        jmax_schema_ir::resolve::resolve(schema.clone(), jmax_schema_ir::ResolveLimits::default())?;
    jmax_schema_compile::lower::compile(&resolved, &CompilerOptions::default())
}

/// Decode the complete compiler image, including all executable side tables.
pub fn load_program(bytes: &[u8]) -> Result<ValidatedProgram> {
    jmax_schema_compile::image::decode(bytes, &CompilerOptions::default())
}

/// Rust source literal with no raw-string delimiter hazards.
pub fn rust_string_literal(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                // Formatting into an owned String cannot fail.
                let _ = write!(&mut out, "\\u{{{:x}}}", u32::from(ch));
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// ASCII Rust identifiers used for generated statics. Keywords are rejected.
pub fn validate_symbol(symbol: &str) -> Result<()> {
    let mut bytes = symbol.bytes();
    let valid_start = bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_');
    if !valid_start
        || symbol == "_"
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || RUST_KEYWORDS.contains(&symbol)
    {
        return Err(aot_error(
            ErrorCode::ConstraintViolation,
            ErrorSource::SchemaCompile,
            "invalid Rust symbol",
        ));
    }
    Ok(())
}

pub const RUST_KEYWORDS: &[&str] = &[
    "as", "async", "await", "become", "box", "break", "const", "continue", "crate", "do", "dyn",
    "else", "enum", "extern", "false", "final", "fn", "for", "gen", "if", "impl", "in", "let",
    "loop", "macro", "match", "mod", "move", "mut", "override", "priv", "pub", "ref", "return",
    "self", "Self", "static", "struct", "super", "trait", "true", "try", "type", "typeof", "union",
    "unsafe", "unsized", "use", "virtual", "where", "while", "yield", "abstract",
];

/// Validate a source path without accepting arbitrary Rust expressions.
pub fn validate_rust_path(path: &str) -> Result<()> {
    let absolute = path.starts_with("::");
    let path = path.strip_prefix("::").unwrap_or(path);
    if path.is_empty() {
        return Err(aot_error(
            ErrorCode::ConstraintViolation,
            ErrorSource::SchemaCompile,
            "empty Rust path",
        ));
    }
    let mut leading_super = true;
    for (index, part) in path.split("::").enumerate() {
        match part {
            "crate" | "self" => {
                if absolute || index != 0 {
                    return Err(aot_error(
                        ErrorCode::ConstraintViolation,
                        ErrorSource::SchemaCompile,
                        "misplaced Rust path keyword",
                    ));
                }
                leading_super = false;
            }
            "super" => {
                if absolute || !leading_super {
                    return Err(aot_error(
                        ErrorCode::ConstraintViolation,
                        ErrorSource::SchemaCompile,
                        "misplaced super path component",
                    ));
                }
            }
            _ => {
                validate_symbol(part)?;
                leading_super = false;
            }
        }
    }
    Ok(())
}

/// Deterministic field order used by generated plans, without changing the caller's IR.
pub fn ordered_field_ids(schema: &SchemaIr) -> Vec<FieldId> {
    schema.fields.iter().map(|field| field.field_id).collect()
}
