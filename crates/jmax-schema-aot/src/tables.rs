//! Deterministic Rust source for immutable ABI tables and complete compiler images.
//!
//! An ABI projection is not an executable-program witness. Use [`emit_program`]
//! and [`crate::load_program`] when executable side tables are needed.

use alloc::{string::String, vec::Vec};
use core::fmt::{self, Write};

use crate::{
    aot_error, validate_rust_path, validate_symbol, Error, ErrorCode, ErrorSource,
    Result, SchemaImage, StaticSchemaImage, TableOptions, ValidatedProgram,
};

/// Emit one public static backed by promoted, immutable arrays.
///
/// Checks the ABI projection and all configured source identifiers before
/// interpolation. Accessing the generated static requires no allocation.
/// Projection validation does not certify executable side tables.
pub fn emit_tables(image: StaticSchemaImage<'_>, options: &TableOptions<'_>) -> Result<String> {
    crate::verify::verify_structure(image)?;
    validate_options(options)?;
    let mut source = Source::default();
    write_tables(&mut source, image, options).map_err(|_| allocation_error(image.header.schema_id))?;
    Ok(source.text)
}

/// Emit the ABI static and a `<symbol>_PROGRAM` byte-slice static.
///
/// The byte slice is exactly the complete compiler encoding, bounded by
/// `jmax_schema_compile::DEFAULT_MAX_IMAGE_BYTES`. Loading it through
/// [`crate::load_program`] allocates and validates owned executable side tables.
/// No compiler, formatter, or filesystem operation is performed here.
pub fn emit_program(program: &ValidatedProgram, options: &TableOptions<'_>) -> Result<String> {
    let image = StaticSchemaImage::from(program.image());
    crate::verify::verify_structure(image)?;
    validate_options(options)?;

    let mut symbol = Source::default();
    write!(symbol, "{}_PROGRAM", options.symbol)
        .map_err(|_| allocation_error(image.header.schema_id))?;
    validate_symbol(&symbol.text)?;

    let bytes = jmax_schema_compile::image::encode(
        program,
        jmax_schema_compile::DEFAULT_MAX_IMAGE_BYTES,
    )?;
    let mut source = Source::default();
    write_tables(&mut source, image, options)
        .and_then(|()| write_program(&mut source, &symbol.text, &bytes))
        .map_err(|_| allocation_error(image.header.schema_id))?;
    Ok(source.text)
}

/// Validate and copy a borrowed projection into independently owned arrays.
///
/// This does not construct a `ValidatedProgram` or issue a validation proof.
pub fn to_owned(image: StaticSchemaImage<'_>) -> Result<SchemaImage> {
    crate::verify::verify_structure(image)?;
    let schema_id = image.header.schema_id;
    Ok(SchemaImage {
        header: image.header,
        insns: copy_slice(image.insns, schema_id)?,
        keys: copy_slice(image.keys, schema_id)?,
        key_bytes: copy_slice(image.key_bytes, schema_id)?,
    })
}

fn validate_options(options: &TableOptions<'_>) -> Result<()> {
    validate_symbol(options.symbol)?;
    validate_rust_path(options.abi_path)?;
    validate_rust_path(options.aot_path)
}

fn allocation_error(schema_id: u32) -> Error {
    aot_error(
        ErrorCode::BudgetExhausted,
        ErrorSource::SchemaCompile,
        "cannot allocate static schema tables or source",
    ).with_schema(schema_id, 0)
}

fn copy_slice<T: Copy>(values: &[T], schema_id: u32) -> Result<Vec<T>> {
    let mut owned = Vec::new();
    owned.try_reserve_exact(values.len()).map_err(|_| allocation_error(schema_id))?;
    owned.extend_from_slice(values);
    Ok(owned)
}

/// Formatting only encounters allocation errors: the renderer writes strings
/// and primitive integers, and every append reserves capacity fallibly first.
#[derive(Default)]
struct Source {
    text: String,
}

impl Write for Source {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.text.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.text.push_str(text);
        Ok(())
    }
}

// These private renderers receive validated source identifiers from the public
// entry points. Every record field is spelled out; no native layout is encoded.
fn write_tables(
    out: &mut impl Write,
    image: StaticSchemaImage<'_>,
    options: &TableOptions<'_>,
) -> fmt::Result {
    let abi = options.abi_path;
    let aot = options.aot_path;
    let header = image.header;
    writeln!(out, "pub static {}: {aot}::StaticSchemaImage<'static> = {aot}::StaticSchemaImage {{", options.symbol)?;
    writeln!(out, "    header: {abi}::SchemaImageHeader {{")?;
    writeln!(out, "        magic: {:#010x}u32,", header.magic)?;
    writeln!(out, "        version: {}u16,", header.version)?;
    writeln!(out, "        flags: {}u16,", header.flags)?;
    writeln!(out, "        schema_id: {}u32,", header.schema_id)?;
    writeln!(out, "        root_state: {}u32,", header.root_state)?;
    writeln!(out, "        insn_count: {}u32,", header.insn_count)?;
    writeln!(out, "        key_count: {}u32,", header.key_count)?;
    writeln!(out, "        slot_count: {}u32,", header.slot_count)?;
    writeln!(out, "        fingerprint: {}u64,", header.fingerprint)?;
    out.write_str("    },\n    insns: &[\n")?;
    for insn in image.insns {
        writeln!(out,
            "        {abi}::InsnWord {{ opcode: {}u8, flags: {}u8, field_id: {}u16, arg0: {}u32, arg1: {}u64 }},",
            insn.opcode, insn.flags, insn.field_id, insn.arg0, insn.arg1,
        )?;
    }
    out.write_str("    ],\n    keys: &[\n")?;
    for key in image.keys {
        writeln!(out,
            "        {abi}::KeyEntry {{ swar_head: {}u64, span: {abi}::Span32 {{ offset: {}u32, len: {}u32 }}, field_id: {}u16, required_bit: {}u16, target_state: {}u32 }},",
            key.swar_head, key.span.offset, key.span.len, key.field_id,
            key.required_bit, key.target_state,
        )?;
    }
    out.write_str("    ],\n    key_bytes: &[\n")?;
    write_bytes(out, image.key_bytes, "        ")?;
    out.write_str("    ],\n};\n")
}

fn write_program(out: &mut impl Write, symbol: &str, bytes: &[u8]) -> fmt::Result {
    writeln!(out, "\npub static {symbol}: &[u8] = &[")?;
    write_bytes(out, bytes, "    ")?;
    out.write_str("];\n")
}

fn write_bytes(out: &mut impl Write, bytes: &[u8], indent: &str) -> fmt::Result {
    for chunk in bytes.chunks(16) {
        out.write_str(indent)?;
        for (index, byte) in chunk.iter().enumerate() {
            if index != 0 {
                out.write_char(' ')?;
            }
            write!(out, "0x{byte:02x}u8,")?;
        }
        out.write_char('\n')?;
    }
    Ok(())
}
