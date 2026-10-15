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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use crate::{InsnWord, KeyEntry, Opcode, SchemaImageHeader, Span32};
    use jmax_schema_compile::{opcode_byte, IMAGE_FLAGS, IMAGE_MAGIC, IMAGE_VERSION};

    fn insn(opcode: Opcode, field_id: u16, arg0: u32, arg1: u64) -> InsnWord {
        InsnWord { opcode: opcode_byte(opcode), flags: 0, field_id, arg0, arg1 }
    }

    fn any_image() -> SchemaImage {
        SchemaImage {
            header: SchemaImageHeader {
                magic: IMAGE_MAGIC, version: IMAGE_VERSION, flags: IMAGE_FLAGS,
                schema_id: 7, root_state: 0, insn_count: 1, key_count: 0,
                slot_count: 0, fingerprint: 9,
            },
            insns: vec![insn(Opcode::CheckScalarKind, 0, 0, 255)],
            keys: Vec::new(),
            key_bytes: Vec::new(),
        }
    }

    fn object_image() -> SchemaImage {
        let bytes = "\"\\\n\0é🦀abcdefgh".as_bytes().to_vec();
        let mut image = any_image();
        image.header.schema_id = u32::MAX;
        image.header.fingerprint = u64::MAX;
        image.header.insn_count = 7;
        image.header.key_count = 2;
        image.header.slot_count = 2;
        image.insns = vec![
            insn(Opcode::EnterObject, 0, 0, 0),
            insn(Opcode::MatchKeySwar, 0, 0, 0),
            insn(Opcode::EmitSlot, u16::MAX, 0, 1),
            insn(Opcode::EmitSlot, 42, 1, 2),
            insn(Opcode::LeaveObject, 0, 0, 0),
            insn(Opcode::CheckScalarKind, 0, 1, 255),
            insn(Opcode::CheckScalarKind, 0, 2, 255),
        ];
        image.keys = vec![
            KeyEntry {
                swar_head: 0, span: Span32::new(0, 0), field_id: u16::MAX,
                required_bit: 0, target_state: 1,
            },
            KeyEntry {
                swar_head: jmax_schema_compile::swar_head(&bytes),
                span: Span32::new(0, u32::try_from(bytes.len()).unwrap()),
                field_id: 42, required_bit: u16::MAX, target_state: 2,
            },
        ];
        image.key_bytes = bytes;
        image
    }

    fn assert_same_image(left: &SchemaImage, right: &SchemaImage) {
        assert_eq!(left.header, right.header);
        assert_eq!(left.insns, right.insns);
        assert_eq!(left.keys, right.keys);
        assert_eq!(left.key_bytes, right.key_bytes);
    }

    fn assert_rejected(image: &SchemaImage) {
        let borrowed = StaticSchemaImage::from(image);
        let expected = crate::verify::verify_structure(borrowed).unwrap_err();
        assert_eq!(expected.code, ErrorCode::SchemaImageInvalid);
        assert_eq!(expected.source, ErrorSource::SchemaCompile);
        assert_eq!(emit_tables(borrowed, &TableOptions::default()).unwrap_err(), expected);
        assert_eq!(to_owned(borrowed).unwrap_err(), expected);
    }

    #[test]
    fn minimal_projection_has_exact_stable_source() {
        let image = any_image();
        let source = emit_tables((&image).into(), &TableOptions::default()).unwrap();
        let expected = concat!(
            "pub static JMAX_SCHEMA: ::jmax_schema_aot::StaticSchemaImage<'static> = ::jmax_schema_aot::StaticSchemaImage {\n",
            "    header: ::jmax_abi::SchemaImageHeader {\n",
            "        magic: 0x58414d4au32,\n",
            "        version: 1u16,\n",
            "        flags: 0u16,\n",
            "        schema_id: 7u32,\n",
            "        root_state: 0u32,\n",
            "        insn_count: 1u32,\n",
            "        key_count: 0u32,\n",
            "        slot_count: 0u32,\n",
            "        fingerprint: 9u64,\n",
            "    },\n",
            "    insns: &[\n",
            "        ::jmax_abi::InsnWord { opcode: 7u8, flags: 0u8, field_id: 0u16, arg0: 0u32, arg1: 255u64 },\n",
            "    ],\n",
            "    keys: &[\n",
            "    ],\n",
            "    key_bytes: &[\n",
            "    ],\n",
            "};\n",
        );
        assert_eq!(source, expected);
        assert_eq!(source, emit_tables((&image).into(), &TableOptions::default()).unwrap());
        assert_same_image(&image, &to_owned((&image).into()).unwrap());
    }

    #[test]
    fn keys_and_maximum_identifiers_are_preserved() {
        let image = object_image();
        let source = emit_tables((&image).into(), &TableOptions::default()).unwrap();
        assert!(source.contains("schema_id: 4294967295u32"));
        assert!(source.contains("fingerprint: 18446744073709551615u64"));
        assert!(source.contains("field_id: 65535u16, required_bit: 0u16, target_state: 1u32"));
        assert!(source.contains("field_id: 42u16, required_bit: 65535u16, target_state: 2u32"));
        assert!(source.contains("offset: 0u32, len: 0u32"));
        assert!(source.contains("0x22u8, 0x5cu8, 0x0au8, 0x00u8, 0xc3u8, 0xa9u8"));
        let byte_section = source.split_once("    key_bytes: &[\n").unwrap().1;
        assert_eq!(parse_bytes(byte_section.strip_suffix("    ],\n};\n").unwrap()), image.key_bytes);
        assert_same_image(&image, &to_owned((&image).into()).unwrap());
    }

    #[test]
    fn owned_projection_outlives_and_does_not_alias_input() {
        let mut original = object_image();
        let owned = to_owned((&original).into()).unwrap();
        let expected = original.clone();
        assert_ne!(owned.insns.as_ptr(), original.insns.as_ptr());
        assert_ne!(owned.keys.as_ptr(), original.keys.as_ptr());
        assert_ne!(owned.key_bytes.as_ptr(), original.key_bytes.as_ptr());
        original.header.fingerprint = 0;
        original.insns.fill(InsnWord::default());
        original.keys.fill(KeyEntry::default());
        original.key_bytes.fill(0);
        drop(original);
        assert_same_image(&owned, &expected);
    }

    #[test]
    fn unselected_fields_and_repeated_empty_names_across_objects_are_valid() {
        let mut image = object_image();
        image.header.slot_count = 0;
        image.insns = vec![
            insn(Opcode::EnterObject, 0, 0, 0),
            insn(Opcode::MatchKeySwar, 0, 0, 0),
            insn(Opcode::LeaveObject, 0, 0, 0),
            insn(Opcode::EnterObject, 0, 1, 1),
            insn(Opcode::MatchKeySwar, 0, 1, 0),
            insn(Opcode::LeaveObject, 0, 1, 0),
            insn(Opcode::CheckScalarKind, 0, 2, 255),
        ];
        image.key_bytes.clear();
        image.keys[1].span = Span32::new(0, 0);
        image.keys[1].swar_head = 0;
        // Key ownership belongs to complete compiler side tables. Identical
        // names in separate objects and FieldIds above slot_count are legal.
        let source = emit_tables((&image).into(), &TableOptions::default()).unwrap();
        assert_eq!(source.matches("offset: 0u32, len: 0u32").count(), 2);
        assert_same_image(&image, &to_owned((&image).into()).unwrap());
    }

    #[test]
    fn configured_paths_are_used_for_every_type() {
        let image = object_image();
        for (abi_path, aot_path) in [
            ("::renamed_abi", "::renamed_aot"),
            ("crate::abi", "self::aot"),
            ("super::super::abi", "super::aot"),
        ] {
            let options = TableOptions { symbol: "_TABLE_7", abi_path, aot_path };
            let source = emit_tables((&image).into(), &options).unwrap();
            assert!(source.starts_with(format!("pub static _TABLE_7: {aot_path}::StaticSchemaImage<'static>").as_str()));
            for ty in ["SchemaImageHeader", "InsnWord", "KeyEntry", "Span32"] {
                assert!(source.contains(format!("{abi_path}::{ty} {{").as_str()));
            }
            assert!(!source.contains("jmax_abi"));
            assert!(!source.contains("jmax_schema_aot"));
        }
    }

    #[test]
    fn symbols_and_paths_cannot_inject_source() {
        let image = any_image();
        for symbol in ["", "_", "9x", "fn", "gen", "r#type", "é", "X; fn injected() {}", "X\nY"] {
            let options = TableOptions { symbol, ..TableOptions::default() };
            let error = emit_tables((&image).into(), &options).unwrap_err();
            assert_eq!(error, validate_symbol(symbol).unwrap_err());
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
        }
        for path in ["", "::", "x::", "x::::y", "x y", "x<T>", "x/* */", "::crate::x", "x::self", "crate::super", "x; fn injected() {}"] {
            for options in [
                TableOptions { abi_path: path, ..TableOptions::default() },
                TableOptions { aot_path: path, ..TableOptions::default() },
            ] {
                assert_eq!(emit_tables((&image).into(), &options).unwrap_err(), validate_rust_path(path).unwrap_err());
            }
        }
    }

    #[test]
    fn header_and_instruction_corruption_is_rejected() {
        let mutations: &[fn(&mut SchemaImage)] = &[
            |i| i.header.magic ^= 1,
            |i| i.header.version = u16::MAX,
            |i| i.header.flags = 1,
            |i| i.header.root_state = u32::MAX,
            |i| i.header.insn_count = u32::MAX,
            |i| i.header.key_count = 1,
            |i| i.header.slot_count = 1,
            |i| { i.insns.clear(); i.header.insn_count = 0; },
            |i| i.insns[0].opcode = 0,
            |i| i.insns[0].opcode = u8::MAX,
            |i| i.insns[0].flags = u8::MAX,
            |i| i.insns[0].arg0 = u32::MAX,
            |i| i.insns[0].arg1 = 254,
        ];
        for mutate in mutations {
            let mut image = any_image();
            mutate(&mut image);
            assert_rejected(&image);
        }
    }

    #[test]
    fn key_and_state_boundaries_are_checked() {
        let mutations: &[fn(&mut SchemaImage)] = &[
            |i| i.keys[1].span.offset = u32::MAX,
            |i| i.keys[1].span.len = u32::MAX,
            |i| i.keys[1].swar_head ^= 1,
            |i| i.keys[1].field_id = i.keys[0].field_id,
            |i| i.keys[1].target_state = 3,
            |i| i.keys[1].target_state = u32::MAX,
            |i| i.key_bytes.push(0),
            |i| {
                i.key_bytes[0] = 0xff;
                i.keys[1].swar_head = jmax_schema_compile::swar_head(&i.key_bytes);
            },
            |i| i.insns[2].arg0 = 2,
            |i| i.insns[3].arg0 = 0,
            |i| i.insns[2].arg1 = 3,
            |i| i.insns[4].opcode = opcode_byte(Opcode::LeaveArray),
        ];
        for mutate in mutations {
            let mut image = object_image();
            mutate(&mut image);
            assert_rejected(&image);
        }
    }

    #[test]
    fn byte_writer_covers_every_byte_and_line_boundary() {
        let bytes: Vec<u8> = (u8::MIN..=u8::MAX).collect();
        for length in [0, 1, 15, 16, 17, 255, 256] {
            let mut source = String::new();
            write_bytes(&mut source, &bytes[..length], "    ").unwrap();
            assert_eq!(parse_bytes(&source), bytes[..length]);
            assert_eq!(source.lines().count(), length.div_ceil(16));
            for line in source.lines() {
                assert!(line.starts_with("    "));
                assert!(line.ends_with(','));
                assert!(line.split_whitespace().count() <= 16);
            }
        }
    }

    #[test]
    fn renderer_preserves_all_bits_without_normalizing_records() {
        // Exercise the private formatter independently of structural acceptance.
        // Public APIs reject these deliberately non-executable records.
        let image = SchemaImage {
            header: SchemaImageHeader {
                magic: u32::MAX, version: u16::MAX, flags: u16::MAX,
                schema_id: u32::MAX, root_state: u32::MAX, insn_count: u32::MAX,
                key_count: u32::MAX, slot_count: u32::MAX, fingerprint: u64::MAX,
            },
            insns: vec![InsnWord { opcode: u8::MAX, flags: u8::MAX, field_id: u16::MAX, arg0: u32::MAX, arg1: u64::MAX }],
            keys: vec![KeyEntry { swar_head: u64::MAX, span: Span32::new(u32::MAX, u32::MAX), field_id: u16::MAX, required_bit: u16::MAX, target_state: u32::MAX }],
            key_bytes: vec![0, 255],
        };
        let mut source = String::new();
        write_tables(&mut source, (&image).into(), &TableOptions::default()).unwrap();
        assert!(source.contains("magic: 0xffffffffu32"));
        assert!(source.contains("version: 65535u16,\n        flags: 65535u16"));
        assert!(source.contains("opcode: 255u8, flags: 255u8, field_id: 65535u16, arg0: 4294967295u32, arg1: 18446744073709551615u64"));
        assert!(source.contains("swar_head: 18446744073709551615u64, span: ::jmax_abi::Span32 { offset: 4294967295u32, len: 4294967295u32 }, field_id: 65535u16, required_bit: 65535u16, target_state: 4294967295u32"));
        assert!(source.contains("0x00u8, 0xffu8,"));
        assert_rejected(&image);
    }

    fn parse_bytes(source: &str) -> Vec<u8> {
        source.split(',').filter_map(|token| {
            let token = token.trim();
            if token.is_empty() {
                None
            } else {
                let hex = token.strip_prefix("0x").unwrap().strip_suffix("u8").unwrap();
                assert_eq!(hex.len(), 2);
                Some(u8::from_str_radix(hex, 16).unwrap())
            }
        }).collect()
    }

    fn complete_program() -> ValidatedProgram {
        use jmax_schema_ir::{ObjectShape, Property, ScalarConstraints, Schema, TypeId, TypeNode, ValueKind};
        let mut text = ScalarConstraints::new(ValueKind::String);
        text.min_length = Some(1);
        text.max_length = Some(32);
        text.pattern = Some(String::from("^a+$"));
        let mut number = ScalarConstraints::new(ValueKind::Integer);
        number.min_i64 = Some(i64::MIN);
        number.max_i64 = Some(i64::MAX);
        let schema = Schema {
            name: String::from("complete"), schema_id: crate::SchemaId(77), root: TypeId(0),
            nodes: vec![
                TypeNode::Object(ObjectShape {
                    properties: vec![
                        Property { field_id: crate::FieldId(u32::from(u16::MAX)), name: String::from("label"), ty: TypeId(1), required: true, slot: Some(1) },
                        Property { field_id: crate::FieldId(4), name: String::from("count"), ty: TypeId(2), required: false, slot: Some(0) },
                    ],
                    reject_unknown: true, min_properties: Some(1), max_properties: Some(2),
                }),
                TypeNode::Scalar(text),
                TypeNode::Scalar(number),
            ],
            definitions: Vec::new(),
        };
        let resolved = jmax_schema_ir::resolve::resolve(schema, jmax_schema_ir::ResolveLimits::default()).unwrap();
        jmax_schema_compile::lower::compile(&resolved, &crate::CompilerOptions::default()).unwrap()
    }

    #[test]
    fn complete_program_bytes_round_trip_all_side_tables() {
        let program = complete_program();
        let options = TableOptions { symbol: "EXAMPLE_PROGRAM", abi_path: "::abi_alias", aot_path: "::aot_alias" };
        let source = emit_program(&program, &options).unwrap();
        let projection = emit_tables(program.image().into(), &options).unwrap();
        assert!(source.starts_with(projection.as_str()));
        assert_eq!(source, emit_program(&program, &options).unwrap());
        assert_eq!(source.matches("pub static ").count(), 2);
        let marker = "\npub static EXAMPLE_PROGRAM_PROGRAM: &[u8] = &[\n";
        let tail = source.strip_prefix(projection.as_str()).unwrap().strip_prefix(marker).unwrap();
        let bytes = parse_bytes(tail.strip_suffix("];\n").unwrap());
        let expected = jmax_schema_compile::image::encode(&program, jmax_schema_compile::DEFAULT_MAX_IMAGE_BYTES).unwrap();
        assert_eq!(bytes, expected);
        let loaded = crate::load_program(&bytes).unwrap();
        assert_same_image(program.image(), loaded.image());
        let original = program.program();
        let decoded = loaded.program();
        assert!(!original.patterns.is_empty());
        assert!(!original.key_tables.is_empty());
        assert_eq!(original.slots.len(), 2);
        assert!(!original.canonical_schema.is_empty());
        assert_eq!(original.states, decoded.states);
        assert_eq!(original.key_tables, decoded.key_tables);
        assert_eq!(original.patterns, decoded.patterns);
        assert_eq!(original.slots, decoded.slots);
        assert_eq!(original.policy, decoded.policy);
        assert_eq!(original.format_options, decoded.format_options);
        assert_eq!(original.canonical_schema, decoded.canonical_schema);
        assert_eq!(bytes, jmax_schema_compile::image::encode(&loaded, jmax_schema_compile::DEFAULT_MAX_IMAGE_BYTES).unwrap());
        assert!(!source.contains("::jmax_abi"));
        assert!(!source.contains("::jmax_schema_aot"));
    }

    #[test]
    fn program_emission_rejects_invalid_source_configuration() {
        let program = complete_program();
        for options in [
            TableOptions { symbol: "gen", ..TableOptions::default() },
            TableOptions { symbol: "bad-name", ..TableOptions::default() },
            TableOptions { abi_path: "x::<T>", ..TableOptions::default() },
            TableOptions { aot_path: "::self::x", ..TableOptions::default() },
        ] {
            assert_eq!(emit_program(&program, &options).unwrap_err(), validate_options(&options).unwrap_err());
        }
    }

    #[test]
    fn formatting_errors_propagate_instead_of_truncating_successfully() {
        struct RejectWrites;
        impl Write for RejectWrites {
            fn write_str(&mut self, _: &str) -> fmt::Result { Err(fmt::Error) }
        }
        assert!(write_tables(&mut RejectWrites, (&any_image()).into(), &TableOptions::default()).is_err());
        assert!(write_program(&mut RejectWrites, "IMAGE_PROGRAM", &[0]).is_err());
        assert!(write_bytes(&mut RejectWrites, &[0], "").is_err());
        let error = allocation_error(u32::MAX);
        assert_eq!(error.code, ErrorCode::BudgetExhausted);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.schema_id, u32::MAX);
        assert_eq!(error.field_id, 0);
    }
}
