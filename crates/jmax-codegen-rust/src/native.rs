//! Wave 5: monomorphic single-pass native decoders over `jmax_native::Reader`.
//!
//! The emitted code decides exactly the documents the embedded reference
//! program (lowered from the same schema graph) accepts. It never builds an
//! error: a failure is `Fail`, and `jmax_native::decode_into` re-runs the VM.

use alloc::{collections::BTreeSet, format, string::String, vec::Vec};
use crate::{
    codegen_error, models, options_with_native_idents, rust_string_literal, validate_native_options,
    validate_options, ArtifactKind, CodegenOptions, EndpointField, EndpointModel, EndpointType,
    ErrorCode, GeneratedArtifact, NativeOptions, NativeSpec, Result, RustTypeMode, GENERATED_NOTICE,
};
use jmax_endpoint_model::{registry, NameCase, Registry, SchemaId};

/// The schema id of the lowered graph of the root at `index`.
const SCHEMA_ID_BASE: u32 = 5000;

fn unsupported(detail: &'static str) -> crate::Error {
    codegen_error(ErrorCode::UnsupportedSchemaFeature, detail)
}

fn constraint(detail: &'static str) -> crate::Error {
    codegen_error(ErrorCode::ConstraintViolation, detail)
}

fn snake(name: &str) -> Result<String> {
    jmax_endpoint_model::wire_names::convert_case(name, NameCase::Snake)
}

struct StructInfo<'a> {
    /// PascalCase generated type name.
    name: String,
    fields: &'a [EndpointField],
    tuple: bool,
}

struct ArrayInfo {
    /// The owned Rust type of the whole Vec, which determines its decoder.
    rust_type: String,
}

struct Emitter<'r, 'a> {
    registry: &'r Registry<'a>,
    spec: &'r NativeSpec,
    options: &'r CodegenOptions,
    structs: Vec<StructInfo<'a>>,
    arrays: Vec<ArrayInfo>,
    decimals: bool,
}

impl<'r, 'a> Emitter<'r, 'a> {
    fn resolve(&self, ty: &'a EndpointType) -> Result<(&'a str, &'a [EndpointField])> {
        match registry::resolve_type(self.registry, ty)? {
            EndpointType::Struct { name, fields } => Ok((name.as_str(), fields.as_slice())),
            _ => Err(constraint("struct reference does not resolve to a struct")),
        }
    }

    fn rust_type(&self, ty: &'a EndpointType) -> Result<String> {
        models::resolved_source(ty, RustTypeMode::Owned, self.options, self.registry, 0)
    }

    fn is_tuple(&self, name: &str) -> bool {
        self.spec.tuples.iter().any(|tuple| tuple == name)
    }

    fn struct_fn(&self, name: &str) -> Result<String> {
        Ok(format!("__decode_{}", snake(&models::pascal_name(name)?)?))
    }

    fn array_index(&self, rust_type: &str) -> Option<usize> {
        self.arrays.iter().position(|array| array.rust_type == rust_type)
    }

    /// Register the structs and arrays reachable from `ty`.
    fn visit(&mut self, ty: &'a EndpointType, depth: usize) -> Result<()> {
        if depth > jmax_endpoint_model::MAX_REFERENCE_DEPTH {
            return Err(codegen_error(ErrorCode::DepthLimitExceeded, "native schema nesting exceeds model limit"));
        }
        match ty {
            EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64
            | EndpointType::TimeStampMs | EndpointType::Float64 | EndpointType::Boolean
            | EndpointType::String => Ok(()),
            EndpointType::BlockchainDecimal => {
                self.decimals = true;
                Ok(())
            }
            EndpointType::UUID | EndpointType::IpAddr | EndpointType::Bytea
            | EndpointType::NanoId { .. } | EndpointType::BlockchainAddress
            | EndpointType::BlockchainTransactionHash => {
                Err(unsupported("format validators are not available in jmax-native"))
            }
            EndpointType::Vec(inner) => {
                let rust_type = self.rust_type(ty)?;
                if self.array_index(&rust_type).is_none() {
                    self.arrays.push(ArrayInfo { rust_type });
                }
                self.visit(inner, depth + 1)
            }
            EndpointType::Struct { .. } | EndpointType::StructRef(_) | EndpointType::StructTable { .. } => {
                let (name, fields) = self.resolve(ty)?;
                let pascal = models::pascal_name(name)?;
                if self.structs.iter().any(|info| info.name == pascal) {
                    return Ok(());
                }
                if fields.is_empty() {
                    return Err(unsupported("empty structs have no native decoder"));
                }
                if fields.len() > 128 {
                    return Err(unsupported("native objects support at most 128 fields"));
                }
                let tuple = self.is_tuple(name);
                self.structs.push(StructInfo { name: pascal, fields, tuple });
                for field in fields {
                    let inner = match &field.ty {
                        EndpointType::Optional(inner) if !tuple => inner.as_ref(),
                        ty => ty,
                    };
                    self.visit(inner, depth + 1)?;
                }
                Ok(())
            }
            EndpointType::Optional(_) => Err(unsupported("nested optional has no native form")),
            EndpointType::Object => Err(unsupported("untyped Object has no native form")),
            EndpointType::Enum { .. } | EndpointType::EnumRef { .. } => {
                Err(unsupported("enums have no native form"))
            }
            EndpointType::Unit => Err(unsupported("unit has no native form")),
        }
    }

    /// An expression reading one scalar, or None for a container or string.
    fn scalar_expr(&self, ty: &EndpointType) -> Option<String> {
        Some(match ty {
            EndpointType::UInt32 => String::from("r.uint(ctx, 4294967295)? as u32"),
            EndpointType::Int32 => String::from("r.int(ctx, -2147483648, 2147483647)? as i32"),
            EndpointType::Int64 | EndpointType::TimeStampMs => String::from("r.int(ctx, i64::MIN, i64::MAX)?"),
            EndpointType::Float64 => String::from("r.float(ctx)?"),
            EndpointType::Boolean => String::from("r.boolean()?"),
            EndpointType::BlockchainDecimal => String::from("r.decimal(ctx, &DECIMAL_POLICY)?"),
            _ => return None,
        })
    }

    /// Statements decoding one value into `place`, an lvalue of the owned Rust
    /// type of `ty`. The value sits at `depth + 1` when it is a container.
    fn value(&self, ty: &'a EndpointType, place: &str, indent: &str) -> Result<String> {
        if let Some(expr) = self.scalar_expr(ty) {
            return Ok(format!("{indent}{place} = {expr};\n"));
        }
        Ok(match ty {
            EndpointType::String => format!(
                "{indent}{{\n{indent}    let text = r.string(ctx)?;\n{indent}    {place}.clear();\n{indent}    {place}.push_str(text);\n{indent}}}\n"
            ),
            EndpointType::Vec(_) => {
                let rust_type = self.rust_type(ty)?;
                let index = self.array_index(&rust_type).ok_or_else(|| constraint("unregistered array"))?;
                format!("{indent}__decode_array_{index}(r, ctx, &mut {place}, depth + 1)?;\n")
            }
            EndpointType::Struct { .. } => {
                let (name, _) = self.resolve(ty)?;
                format!("{indent}{}(r, ctx, &mut {place}, depth + 1)?;\n", self.struct_fn(name)?)
            }
            EndpointType::StructRef(_) | EndpointType::StructTable { .. } => {
                let (name, _) = self.resolve(ty)?;
                format!("{indent}{}(r, ctx, &mut *{place}, depth + 1)?;\n", self.struct_fn(name)?)
            }
            _ => return Err(unsupported("type has no native decoder")),
        })
    }

    fn emit_default(&self, out: &mut String, info: &StructInfo<'a>, options: &CodegenOptions) -> Result<()> {
        out.push_str(&format!(
            "impl ::core::default::Default for super::models::{} {{\n    fn default() -> Self {{\n        Self {{\n",
            info.name
        ));
        for field in info.fields {
            out.push_str(&format!(
                "            {}: ::core::default::Default::default(),\n",
                models::member_ident(options, &info.name, &field.name)?
            ));
        }
        out.push_str("        }\n    }\n}\n");
        Ok(())
    }

    fn emit_object(&self, out: &mut String, info: &StructInfo<'a>) -> Result<()> {
        let fname = self.struct_fn(&info.name)?;
        let open = !self.spec.reject_unknown;
        let count = info.fields.len();
        let seen_type = match count {
            0..=8 => "u8",
            9..=16 => "u16",
            17..=32 => "u32",
            33..=64 => "u64",
            _ => "u128",
        };
        let count_literal = count;
        let mut required_mask: u128 = 0;
        for (index, field) in info.fields.iter().enumerate() {
            if !matches!(field.ty, EndpointType::Optional(_)) {
                required_mask |= 1u128 << index;
            }
        }
        out.push_str(&format!(
            "fn {fname}(r: &mut __native::Reader<'_>, ctx: &mut __native::NativeContext, out: &mut super::models::{}, depth: u16) -> __native::NativeResult<()> {{\n",
            info.name
        ));
        out.push_str("    let limits = ctx.limits().syntax;\n    if depth > limits.max_depth {\n        return Err(__native::Fail);\n    }\n    r.expect(b'{')?;\n");
        if open {
            out.push_str("    let mark = ctx.begin_object();\n");
        }
        out.push_str(&format!("    let mut seen: {seen_type} = 0;\n    let mut fields = 0usize;\n"));
        out.push_str("    if !r.eat(b'}') {\n        loop {\n");
        if open {
            out.push_str("            let at = r.clone();\n");
        }
        out.push_str("            let key = r.key(ctx)?;\n            fields += 1;\n            if fields > limits.max_fields_per_object as usize {\n                return Err(__native::Fail);\n            }\n");
        out.push_str(&self.dispatch(info, count_literal)?);
        out.push_str(&format!(
            "            if which < {count_literal} {{\n                let bit: {seen_type} = 1 << which;\n                if seen & bit != 0 && limits.reject_duplicate_keys {{\n                    return Err(__native::Fail);\n                }}\n                seen |= bit;\n            }}\n"
        ));
        out.push_str("            match which {\n");
        for (index, field) in info.fields.iter().enumerate() {
            out.push_str(&format!("                {index} => {{\n"));
            let member = models::member_ident(self.options, &info.name, &field.name)?;
            let place = format!("out.{member}");
            match &field.ty {
                EndpointType::Optional(inner) => {
                    if let Some(expr) = self.scalar_expr(inner) {
                        out.push_str(&format!("                    {place} = Some({expr});\n"));
                    } else {
                        out.push_str(&format!(
                            "                    let slot = {place}.get_or_insert_with(::core::default::Default::default);\n"
                        ));
                        out.push_str(&self.value(inner, "(*slot)", "                    ")?);
                    }
                }
                ty => out.push_str(&self.value(ty, &place, "                    ")?),
            }
            out.push_str("                }\n");
        }
        out.push_str("                _ => {\n");
        if open {
            out.push_str("                    if limits.reject_unknown_keys {\n                        return Err(__native::Fail);\n                    }\n                    let mut again = at;\n                    again.unknown_key(ctx)?;\n                    r.skip_value(ctx, depth)?;\n");
        } else {
            out.push_str("                    return Err(__native::Fail);\n");
        }
        out.push_str("                }\n            }\n");
        out.push_str("            if r.eat(b',') {\n                continue;\n            }\n            r.expect(b'}')?;\n            break;\n        }\n    }\n");
        if required_mask != 0 {
            out.push_str(&format!(
                "    if seen & {required_mask}{seen_type} != {required_mask}{seen_type} {{\n        return Err(__native::Fail);\n    }}\n"
            ));
        }
        for (index, field) in info.fields.iter().enumerate() {
            if matches!(field.ty, EndpointType::Optional(_)) {
                let member = models::member_ident(self.options, &info.name, &field.name)?;
                out.push_str(&format!(
                    "    if seen & {}{seen_type} == 0 {{\n        out.{member} = None;\n    }}\n",
                    1u128 << index
                ));
            }
        }
        if open {
            out.push_str("    ctx.end_object(mark)?;\n");
        }
        out.push_str("    Ok(())\n}\n");
        Ok(())
    }

    /// `let which: u32 = ...;` where an unknown key yields the field count.
    fn dispatch(&self, info: &StructInfo<'a>, count: usize) -> Result<String> {
        let mut lengths: Vec<usize> = info.fields.iter().map(|field| field.name.len()).collect();
        lengths.sort_unstable();
        lengths.dedup();
        let mut text = String::from("            let which: u32 = match key.len() {\n");
        for length in lengths {
            text.push_str(&format!("                {length} => "));
            let members: Vec<(usize, &EndpointField)> = info
                .fields
                .iter()
                .enumerate()
                .filter(|(_, field)| field.name.len() == length)
                .collect();
            if length == 1 {
                text.push_str("match key.as_bytes() {\n");
                for (index, field) in &members {
                    text.push_str(&format!(
                        "                    [{}] => {index},\n",
                        byte_literal(field.name.as_bytes()[0])
                    ));
                }
                text.push_str(&format!("                    _ => {count},\n                }},\n"));
            } else {
                text.push_str("{\n                    let bytes = key.as_bytes();\n                    ");
                for (position, (index, field)) in members.iter().enumerate() {
                    if position > 0 {
                        text.push_str(" else ");
                    }
                    text.push_str(&format!(
                        "if bytes == {} {{\n                        {index}\n                    }}",
                        byte_string(field.name.as_bytes())
                    ));
                }
                text.push_str(&format!(" else {{\n                        {count}\n                    }}\n                }}\n"));
            }
        }
        text.push_str(&format!("                _ => {count},\n            }};\n"));
        Ok(text)
    }

    fn emit_tuple(&self, out: &mut String, info: &StructInfo<'a>) -> Result<()> {
        let fname = self.struct_fn(&info.name)?;
        let count = info.fields.len();
        out.push_str(&format!(
            "fn {fname}(r: &mut __native::Reader<'_>, ctx: &mut __native::NativeContext, out: &mut super::models::{}, depth: u16) -> __native::NativeResult<()> {{\n",
            info.name
        ));
        out.push_str(&format!(
            "    let limits = ctx.limits().syntax;\n    if depth > limits.max_depth || {count}u32 > limits.max_array_items {{\n        return Err(__native::Fail);\n    }}\n    r.expect(b'[')?;\n"
        ));
        for (index, field) in info.fields.iter().enumerate() {
            if index > 0 {
                out.push_str("    r.expect(b',')?;\n");
            }
            let member = models::member_ident(self.options, &info.name, &field.name)?;
            out.push_str(&self.value(&field.ty, &format!("out.{member}"), "    ")?);
        }
        out.push_str("    r.expect(b']')\n}\n");
        Ok(())
    }

    fn emit_array(&self, out: &mut String, index: usize, info: &ArrayInfo, element: &'a EndpointType) -> Result<()> {
        out.push_str(&format!(
            "fn __decode_array_{index}(r: &mut __native::Reader<'_>, ctx: &mut __native::NativeContext, out: &mut {}, depth: u16) -> __native::NativeResult<()> {{\n",
            info.rust_type
        ));
        out.push_str("    let limits = ctx.limits().syntax;\n    if depth > limits.max_depth {\n        return Err(__native::Fail);\n    }\n    r.expect(b'[')?;\n    let mut count = 0usize;\n    if !r.eat(b']') {\n        loop {\n");
        out.push_str("            if count >= limits.max_array_items as usize {\n                return Err(__native::Fail);\n            }\n            if count == out.len() {\n                out.push(::core::default::Default::default());\n            }\n            let Some(item) = out.get_mut(count) else {\n                return Err(__native::Fail);\n            };\n");
        out.push_str(&self.value(element, "(*item)", "            ")?);
        out.push_str("            count += 1;\n            if r.eat(b',') {\n                continue;\n            }\n            r.expect(b']')?;\n            break;\n        }\n    }\n    out.truncate(count);\n    Ok(())\n}\n");
        Ok(())
    }
}

fn byte_literal(byte: u8) -> String {
    match byte {
        b'\'' => String::from("b'\\''"),
        b'\\' => String::from("b'\\\\'"),
        0x20..=0x7e => format!("b'{}'", byte as char),
        _ => format!("0x{byte:02x}u8"),
    }
}

fn byte_string(bytes: &[u8]) -> String {
    let mut text = String::from("b\"");
    for &byte in bytes {
        match byte {
            b'"' => text.push_str("\\\""),
            b'\\' => text.push_str("\\\\"),
            0x20..=0x7e => text.push(byte as char),
            _ => text.push_str(&format!("\\x{byte:02x}")),
        }
    }
    text.push('"');
    text
}

fn array_elements<'a>(ty: &'a EndpointType, found: &mut Vec<(&'a EndpointType, &'a EndpointType)>) {
    if let EndpointType::Vec(inner) = ty {
        found.push((ty, inner.as_ref()));
        array_elements(inner, found);
    }
}

/// Generate `M/native.rs` for every root of `spec`.
pub fn generate_native(
    model: &EndpointModel,
    spec: &NativeSpec,
    options: &CodegenOptions,
    native: &NativeOptions,
) -> Result<GeneratedArtifact> {
    validate_options(options)?;
    validate_native_options(native)?;
    let registry = registry::build_registry(model)?;
    jmax_endpoint_model::native::validate_native_spec(&registry, spec)?;
    if spec.roots.is_empty() {
        return Err(constraint("native specification has no roots"));
    }
    let merged = options_with_native_idents(options, spec)?;
    let mut emitter = Emitter {
        registry: &registry,
        spec,
        options: &merged,
        structs: Vec::new(),
        arrays: Vec::new(),
        decimals: false,
    };

    // Lower, compile and emit each root's reference program first: the lowering
    // rejects whatever has no native form, with its own diagnostics.
    let mut tables = String::new();
    let mut root_names = BTreeSet::new();
    let mut roots: Vec<(String, String)> = Vec::new();
    for (index, root) in spec.roots.iter().enumerate() {
        let pascal = models::pascal_name(root)?;
        let symbol = format!("NATIVE_{}", snake(&pascal)?.to_ascii_uppercase());
        if !root_names.insert(symbol.clone()) {
            return Err(constraint("native root program symbols collide"));
        }
        let id = SCHEMA_ID_BASE
            .checked_add(u32::try_from(index).map_err(|_| codegen_error(ErrorCode::BudgetExhausted, "root count exceeds u32"))?)
            .ok_or_else(|| codegen_error(ErrorCode::BudgetExhausted, "schema id overflow"))?;
        let graph = jmax_endpoint_model::native::schema_graph_for(&registry, spec, root, SchemaId(id))?;
        let program = jmax_schema_aot::compile_graph(&graph)?;
        tables.push_str(&format!("pub static {symbol}_PROGRAM: &[u8] = &[\n"));
        for chunk in jmax_schema_aot::dump_program(&program)?.chunks(16) {
            tables.push_str("   ");
            for byte in chunk {
                tables.push_str(&format!(" 0x{byte:02x}u8,"));
            }
            tables.push('\n');
        }
        tables.push_str("];\n");
        let definition = registry::definition_by_name(&registry, root)
            .ok_or_else(|| constraint("native root is not a definition"))?;
        emitter.visit(&definition.ty, 0)?;
        roots.push((pascal, symbol));
    }

    // Unique helper names.
    let mut names = BTreeSet::new();
    for info in &emitter.structs {
        if !names.insert(emitter.struct_fn(&info.name)?) {
            return Err(constraint("native decoder function names collide"));
        }
    }

    let mut out = String::from(GENERATED_NOTICE);
    out.push_str(&format!(
        "#![allow(clippy::all)]\nuse {} as __native;\nuse {} as __jmax_abi;\nuse {} as __jmax_aot;\n",
        native.runtime_path, merged.abi_path, merged.aot_path
    ));
    out.push_str(&tables);
    if emitter.decimals {
        let policy = &spec.decimal_policy;
        out.push_str(&format!(
            "const DECIMAL_POLICY: __jmax_abi::number::DecimalPolicy = __jmax_abi::number::DecimalPolicy {{\n    max_precision: {}u8,\n    target_scale: {}u8,\n    allow_exponent: {},\n    reject_inexact_rescale: {},\n    tick_step: {}i128,\n}};\n",
            policy.max_precision, policy.target_scale, policy.allow_exponent, policy.reject_inexact_rescale, policy.tick_step
        ));
    }
    out.push_str(&format!(
        "pub const NATIVE_ROOTS: &[&str] = &[{}];\n",
        spec.roots.iter().map(|root| rust_string_literal(root)).collect::<Vec<_>>().join(", ")
    ));
    for info in &emitter.structs {
        emitter.emit_default(&mut out, info, &merged)?;
    }
    let any_open = !spec.reject_unknown;
    for (pascal, symbol) in &roots {
        let fname = emitter.struct_fn(pascal)?;
        out.push_str(&format!(
            "impl __native::NativeDecode for super::models::{pascal} {{\n    fn build_program() -> __native::Result<__native::ValidatedProgram> {{\n        __jmax_aot::load_program({symbol}_PROGRAM)\n    }}\n\n    fn decode_core(r: &mut __native::Reader<'_>, ctx: &mut __native::NativeContext, out: &mut Self) -> __native::NativeResult<()> {{\n"
        ));
        if any_open {
            out.push_str(&format!(
                "        let mark = ctx.begin_object();\n        let result = {fname}(r, ctx, out, 1);\n        if result.is_err() {{\n            let _ = ctx.end_object(mark);\n        }}\n        result\n"
            ));
        } else {
            out.push_str(&format!("        {fname}(r, ctx, out, 1)\n"));
        }
        out.push_str("    }\n}\n");
    }
    // Struct decoders in discovery order, then array decoders.
    let mut array_types: Vec<(&EndpointType, &EndpointType)> = Vec::new();
    for info in &emitter.structs {
        for field in info.fields {
            let ty = match &field.ty {
                EndpointType::Optional(inner) => inner.as_ref(),
                ty => ty,
            };
            array_elements(ty, &mut array_types);
        }
    }
    for definition in spec.roots.iter().filter_map(|root| registry::definition_by_name(&registry, root)) {
        array_elements(&definition.ty, &mut array_types);
    }
    for info in &emitter.structs {
        if info.tuple {
            emitter.emit_tuple(&mut out, info)?;
        } else {
            emitter.emit_object(&mut out, info)?;
        }
    }
    let mut emitted = BTreeSet::new();
    for (whole, element) in array_types {
        let rust_type = emitter.rust_type(whole)?;
        if let Some(index) = emitter.array_index(&rust_type) {
            if emitted.insert(index) {
                emitter.emit_array(&mut out, index, &emitter.arrays[index], element)?;
            }
        }
    }
    if emitted.len() != emitter.arrays.len() {
        return Err(constraint("a native array decoder was registered but not emitted"));
    }
    Ok(GeneratedArtifact {
        path: format!("{}/native.rs", options.module_name),
        contents: out,
        kind: ArtifactKind::Native,
    })
}
