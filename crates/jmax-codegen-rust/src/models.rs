//! Owned model declarations, checked owner views, and concrete ABI adapters.
//!
//! Source generation is allocation-only. References are validated by the model
//! registry; the independent type mapper deliberately performs no resolution.

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use crate::{codegen_error, rust_string_literal, validate_options, ArtifactKind,
    CodegenOptions, EndpointField, EndpointModel, EndpointType, EnumVariantSpec,
    ErrorCode, GeneratedArtifact, Result, RustType, RustTypeMode, GENERATED_NOTICE};
use jmax_endpoint_model::{NameCase, Registry};
use jmax_abi::schema::{SchemaId, SchemaIr};

const MAX_DEPTH: usize = 128;

fn constraint(detail: &'static str) -> crate::Error {
    codegen_error(ErrorCode::ConstraintViolation, detail)
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(codegen_error(ErrorCode::DepthLimitExceeded, "model type nesting exceeds 128"));
    }
    Ok(())
}

fn pascal(name: &str) -> Result<String> {
    let name = jmax_endpoint_model::wire_names::convert_case(name, NameCase::Pascal)?;
    jmax_schema_aot::validate_symbol(&name)?;
    Ok(name)
}

fn field_name(name: &str) -> Result<String> {
    // These identifiers cannot be escaped with Rust's raw identifier syntax.
    if matches!(name, "self" | "Self" | "super" | "crate" | "_") {
        return Err(constraint("field name cannot be a Rust identifier"));
    }
    let name = jmax_endpoint_model::wire_names::convert_case(name, NameCase::Snake)?;
    if matches!(name.as_str(), "self" | "super" | "crate" | "_") {
        return Err(constraint("field name cannot be a Rust identifier"));
    }
    if jmax_schema_aot::RUST_KEYWORDS.contains(&name.as_str()) {
        Ok(format!("r#{name}"))
    } else {
        jmax_schema_aot::validate_symbol(&name)?;
        Ok(name)
    }
}

fn string_like(ty: &EndpointType) -> bool {
    matches!(ty, EndpointType::String | EndpointType::UUID | EndpointType::NanoId { .. }
        | EndpointType::IpAddr | EndpointType::BlockchainAddress
        | EndpointType::BlockchainTransactionHash)
}

/// Map a type syntactically. Nominal references require model validation at the
/// artifact boundary, since this API has no registry or owner to resolve them.
pub fn rust_type(ty: &EndpointType, mode: RustTypeMode, options: &CodegenOptions) -> Result<RustType> {
    validate_options(options)?;
    let source = type_source(ty, mode, options, 0)?;
    let needs_lifetime = source.contains("'a");
    Ok(RustType { source, needs_lifetime })
}

fn type_source(ty: &EndpointType, mode: RustTypeMode, options: &CodegenOptions, depth: usize) -> Result<String> {
    check_depth(depth)?;
    let view = mode == RustTypeMode::View;
    if string_like(ty) {
        if matches!(ty, EndpointType::NanoId { len: 0 }) {
            return Err(constraint("NanoId length must be nonzero"));
        }
        return Ok(String::from(if view { "&'a str" } else { "super::alloc::string::String" }));
    }
    let source = match ty {
        EndpointType::UInt32 => String::from("u32"),
        EndpointType::Int32 => String::from("i32"),
        EndpointType::Int64 | EndpointType::TimeStampMs => String::from("i64"),
        EndpointType::Float64 => String::from("f64"),
        EndpointType::Boolean => String::from("bool"),
        EndpointType::BlockchainDecimal => format!("{}::Decimal128", options.abi_path),
        EndpointType::Unit => String::from("()"),
        EndpointType::Bytea => String::from(if view { "&'a [u8]" } else { "super::alloc::vec::Vec<u8>" }),
        EndpointType::Object => String::from(if view { "&'a super::models::JsonObject" } else { "super::models::JsonObject" }),
        EndpointType::Vec(inner) => {
            let inner = type_source(inner, RustTypeMode::Owned, options, depth + 1)?;
            if view { format!("&'a [{inner}]") } else { format!("super::alloc::vec::Vec<{inner}>") }
        }
        EndpointType::Optional(inner) => {
            let inner = type_source(inner, mode, options, depth + 1)?;
            format!("::core::option::Option<{inner}>")
        }
        EndpointType::Struct { name, .. } | EndpointType::Enum { name, .. }
        | EndpointType::EnumRef { name, .. } => {
            let name = pascal(name)?;
            if view { format!("super::models::{name}View<'a>") } else { format!("super::models::{name}") }
        }
        EndpointType::StructRef(name) | EndpointType::StructTable { struct_ref: name } => {
            let name = pascal(name)?;
            if view { format!("super::models::{name}View<'a>") }
            else { format!("super::alloc::boxed::Box<super::models::{name}>") }
        }
        _ => return Err(constraint("invalid string-like type mapping")),
    };
    Ok(source)
}

// Resolve aliases only at references. An inline nominal owns its declaration;
// it must never be expanded by following fields of recursive records.
fn resolved_source<'a>(ty: &'a EndpointType, mode: RustTypeMode, options: &CodegenOptions,
    registry: &Registry<'a>, depth: usize) -> Result<String> {
    check_depth(depth)?;
    match ty {
        EndpointType::StructRef(_) | EndpointType::StructTable { .. } | EndpointType::EnumRef { .. } => {
            let target = jmax_endpoint_model::registry::resolve_type(registry, ty)?;
            let source = type_source(target, mode, options, depth)?;
            if mode == RustTypeMode::Owned && !matches!(ty, EndpointType::EnumRef { .. }) {
                Ok(format!("super::alloc::boxed::Box<{source}>"))
            } else { Ok(source) }
        }
        EndpointType::Optional(inner) => Ok(format!("::core::option::Option<{}>",
            resolved_source(inner, mode, options, registry, depth + 1)?)),
        EndpointType::Vec(inner) => {
            let inner = resolved_source(inner, RustTypeMode::Owned, options, registry, depth + 1)?;
            if mode == RustTypeMode::View { Ok(format!("&'a [{inner}]")) }
            else { Ok(format!("super::alloc::vec::Vec<{inner}>")) }
        }
        _ => type_source(ty, mode, options, depth),
    }
}

struct Catalog<'a> {
    nominals: Vec<(&'a str, &'a EndpointType)>,
    by_name: BTreeMap<&'a str, &'a EndpointType>,
    object: bool,
    bytea: bool,
}

impl<'a> Catalog<'a> {
    fn new() -> Self {
        Self { nominals: Vec::new(), by_name: BTreeMap::new(), object: false, bytea: false }
    }

    fn visit(&mut self, ty: &'a EndpointType, depth: usize) -> Result<()> {
        check_depth(depth)?;
        match ty {
            EndpointType::Struct { name, fields } => {
                self.intern(name, ty)?;
                for field in fields { self.visit(&field.ty, depth + 1)?; }
            }
            EndpointType::Enum { name, .. } => self.intern(name, ty)?,
            EndpointType::Vec(inner) | EndpointType::Optional(inner) => self.visit(inner, depth + 1)?,
            EndpointType::Object => self.object = true,
            EndpointType::Bytea => self.bytea = true,
            _ => {}
        }
        Ok(())
    }

    fn intern(&mut self, name: &'a str, ty: &'a EndpointType) -> Result<()> {
        if self.by_name.contains_key(name) {
            return Err(constraint("duplicate nominal model name"));
        }
        self.by_name.insert(name, ty);
        self.nominals.push((name, ty));
        Ok(())
    }

    fn collect(model: &'a EndpointModel) -> Result<Self> {
        let mut catalog = Self::new();
        for definition in &model.definitions { catalog.visit(&definition.ty, 0)?; }
        for service in &model.services {
            for endpoint in &service.endpoints {
                for field in endpoint.params.iter().chain(endpoint.response.iter()) {
                    catalog.visit(&field.ty, 0)?;
                }
                if let Some(ty) = &endpoint.stream_response { catalog.visit(ty, 0)?; }
            }
        }
        for header in &model.headers { catalog.visit(&header.ty, 0)?; }
        Ok(catalog)
    }
}

fn reserve_name(names: &mut BTreeMap<String, ()>, name: String) -> Result<()> {
    if names.insert(name, ()).is_some() { return Err(constraint("colliding generated Rust type names")); }
    Ok(())
}

fn validate_names(model: &EndpointModel, catalog: &Catalog<'_>, options: &CodegenOptions) -> Result<()> {
    let mut names = BTreeMap::new();
    if catalog.object {
        reserve_name(&mut names, String::from("JsonObject"))?;
        reserve_name(&mut names, String::from("JsonValue"))?;
    }
    for (name, ty) in &catalog.nominals {
        let name = pascal(name)?;
        reserve_name(&mut names, name.clone())?;
        if options.emit_views { reserve_name(&mut names, format!("{name}View"))?; }
        match ty {
            EndpointType::Struct { fields, .. } => {
                let mut fields_seen = BTreeMap::new();
                for (index, field) in fields.iter().enumerate() {
                    let id = u32::try_from(index).map_err(|_| codegen_error(ErrorCode::BudgetExhausted, "field count exceeds u32"))?;
                    let name = field_name(&field.name).map_err(|error| error.with_schema(0, id))?;
                    if options.emit_views && matches!(name.as_str(), "new" | "value") {
                        return Err(constraint("view field getter collides with constructor or owner accessor").with_schema(0, id));
                    }
                    if fields_seen.insert(name, ()).is_some() {
                        return Err(constraint("colliding generated Rust field names").with_schema(0, id));
                    }
                }
            }
            EndpointType::Enum { variants, .. } => validate_variants(variants, options)?,
            _ => {}
        }
    }
    for definition in &model.definitions {
        if !matches!(definition.ty, EndpointType::Struct { .. } | EndpointType::Enum { .. }) {
            reserve_name(&mut names, pascal(&definition.name)?)?;
        }
    }
    Ok(())
}

fn validate_variants(variants: &[EnumVariantSpec], options: &CodegenOptions) -> Result<()> {
    if variants.is_empty() { return Err(constraint("empty enum has no numeric JSON representation")); }
    let mut names = BTreeMap::new();
    let mut values = BTreeMap::new();
    for variant in variants {
        let name = pascal(&variant.name)?;
        if name == "Self" || (options.compatibility.emit_definition_variants && name == "JMAX_DEFINITION_VARIANTS") {
            return Err(constraint("enum variant collides with generated associated item"));
        }
        if names.insert(name, ()).is_some() || values.insert(variant.value, ()).is_some() {
            return Err(constraint("duplicate generated enum variant name or discriminant"));
        }
    }
    Ok(())
}

struct SchemaEntry {
    ir: SchemaIr,
    fingerprint: u64,
}

// This is the same qualified-name ordering specified for registries.rs. Include
// endpoint candidates even though this artifact only emits model adapters.
fn schemas(model: &EndpointModel, catalog: &Catalog<'_>, options: &CodegenOptions) -> Result<BTreeMap<String, SchemaEntry>> {
    let mut candidates = BTreeMap::new();
    if !options.emit_schema_tables { return Ok(BTreeMap::new()); }
    for (name, ty) in &catalog.nominals {
        if let EndpointType::Struct { fields, .. } = ty {
            candidates.insert(format!("model::{name}"), fields.as_slice());
        }
    }
    for service in &model.services {
        for endpoint in &service.endpoints {
            candidates.insert(format!("request::{}.{}", service.name, endpoint.name), endpoint.params.as_slice());
            candidates.insert(format!("response::{}.{}", service.name, endpoint.name), endpoint.response.as_slice());
        }
    }
    let mut output = BTreeMap::new();
    for (index, (name, fields)) in candidates.into_iter().enumerate() {
        let id = index.checked_add(1).and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| codegen_error(ErrorCode::BudgetExhausted, "schema count exceeds u32"))?;
        let ir = jmax_endpoint_model::utilities::schema_for_fields(&name, SchemaId(id), fields, true)?;
        // Do not emit a provider for an IR the authoritative compiler rejects.
        let _program = jmax_schema_aot::compile_schema(&ir)?;
        let fingerprint = jmax_schema_aot::fingerprint_schema(&ir)?;
        output.insert(name, SchemaEntry { ir, fingerprint });
    }
    Ok(output)
}

/// Generate every owned declaration and nominal owner-borrowing view used by a
/// validated model. No caller data or sibling artifact is modified.
pub fn generate_models(model: &EndpointModel, options: &CodegenOptions) -> Result<GeneratedArtifact> {
    validate_options(options)?;
    let registry = jmax_endpoint_model::registry::build_registry(model)?;
    let catalog = Catalog::collect(model)?;
    validate_names(model, &catalog, options)?;
    let schemas = schemas(model, &catalog, options)?;
    let mut out = String::from(GENERATED_NOTICE);
    out.push_str(&format!("use super::alloc;\nuse {} as __jmax_abi;\nuse {} as __jmax_aot;\n", options.abi_path, options.aot_path));
    out.push_str(DEPTH_SUPPORT);
    if catalog.bytea { out.push_str(BASE64_SUPPORT); }
    if catalog.object { out.push_str(OBJECT_SUPPORT); }
    // Aliases never receive trait implementations: their targets may be foreign.
    for definition in &model.definitions {
        if !matches!(definition.ty, EndpointType::Struct { .. } | EndpointType::Enum { .. }) {
            let ty = resolved_source(&definition.ty, RustTypeMode::Owned, options, &registry, 0)?;
            out.push_str(&format!("pub type {} = {ty};\n", pascal(&definition.name)?));
        }
    }
    for (name, ty) in &catalog.nominals {
        match ty {
            EndpointType::Struct { fields, .. } => {
                let schema = schemas.get(&format!("model::{name}"));
                emit_record(&mut out, name, fields, options, &registry, schema)?;
            }
            EndpointType::Enum { variants, .. } => emit_enum(&mut out, name, variants, options)?,
            _ => return Err(constraint("catalog contains a non-nominal type")),
        }
    }
    // Every emitter appends complete lines, including for an empty model.
    Ok(GeneratedArtifact { path: format!("{}/models.rs", options.module_name),
        contents: out, kind: ArtifactKind::Models })
}

fn emit_record<'a>(out: &mut String, original: &str, fields: &'a [EndpointField], options: &CodegenOptions,
    registry: &Registry<'a>, schema: Option<&SchemaEntry>) -> Result<()> {
    let name = pascal(original)?;
    out.push_str(&format!("#[derive(Clone, Debug, PartialEq)]\npub struct {name} {{\n"));
    for field in fields {
        let ty = resolved_source(&field.ty, RustTypeMode::Owned, options, registry, 0)?;
        out.push_str(&format!("    pub {}: {ty},\n", field_name(&field.name)?));
    }
    out.push_str("}\n");
    out.push_str(&format!("impl {name} {{\n    pub(super) fn __jmax_encode(&self, sink: &mut dyn __jmax_abi::ByteSink, depth: usize) -> __jmax_abi::Result<()> {{\n        let depth = __jmax_next_depth(depth)?;\n        sink.write_byte(b'{{')?;\n"));
    if !fields.is_empty() { out.push_str("        let mut __jmax_first = true;\n"); }
    for field in fields {
        let member = field_name(&field.name)?;
        let wire = rust_string_literal(&field.name);
        if let EndpointType::Optional(inner) = &field.ty {
            out.push_str(&format!("        if let ::core::option::Option::Some(__jmax_value) = &self.{member} {{\n"));
            emit_key(out, &wire);
            emit_value(out, inner, "__jmax_value", 0)?;
            out.push_str("        }\n");
        } else {
            out.push_str("        {\n");
            emit_key(out, &wire);
            emit_value(out, &field.ty, &format!("&self.{member}"), 0)?;
            out.push_str("        }\n");
        }
    }
    out.push_str("        sink.write_byte(b'}')\n    }\n}\n");
    emit_encode_impl(out, &name);
    if options.emit_views { emit_view(out, &name, Some(fields), options, registry)?; }
    if let Some(schema) = schema { emit_schema(out, &name, fields, schema)?; }
    Ok(())
}

fn emit_key(out: &mut String, wire: &str) {
    out.push_str(&format!("            if !__jmax_first {{ sink.write_byte(b',')?; }}\n            __jmax_first = false;\n            __jmax_aot::encode::write_json_string({wire}, sink)?;\n            sink.write_byte(b':')?;\n"));
}

// Each expression denotes a borrow of its owned type. Containers propagate the
// current depth rather than restarting a public JsonEncode entry point.
fn emit_value(out: &mut String, ty: &EndpointType, expr: &str, nesting: usize) -> Result<()> {
    check_depth(nesting)?;
    if string_like(ty) {
        out.push_str(&format!("            __jmax_aot::encode::write_json_string(({expr}).as_str(), sink)?;\n"));
        return Ok(());
    }
    match ty {
        EndpointType::UInt32 => out.push_str(&format!("            __jmax_aot::encode::write_u64(u64::from(*({expr})), sink)?;\n")),
        EndpointType::Int32 => out.push_str(&format!("            __jmax_aot::encode::write_i64(i64::from(*({expr})), sink)?;\n")),
        EndpointType::Int64 | EndpointType::TimeStampMs => out.push_str(&format!("            __jmax_aot::encode::write_i64(*({expr}), sink)?;\n")),
        EndpointType::Float64 => out.push_str(&format!("            __jmax_aot::encode::write_f64(*({expr}), sink)?;\n")),
        EndpointType::Boolean => out.push_str(&format!("            __jmax_aot::encode::write_bool(*({expr}), sink)?;\n")),
        EndpointType::BlockchainDecimal => out.push_str(&format!("            __jmax_aot::encode::write_decimal(*({expr}), sink)?;\n")),
        EndpointType::Unit => out.push_str("            sink.write_bytes(b\"null\")?;\n"),
        EndpointType::Bytea => out.push_str(&format!("            __jmax_encode_base64(({expr}).as_slice(), sink)?;\n")),
        EndpointType::Struct { .. } | EndpointType::StructRef(_) | EndpointType::StructTable { .. }
        | EndpointType::Enum { .. } | EndpointType::EnumRef { .. } | EndpointType::Object => {
            out.push_str(&format!("            ({expr}).__jmax_encode(sink, depth)?;\n"));
        }
        EndpointType::Vec(inner) => {
            let item = format!("__jmax_item_{nesting}");
            out.push_str(&format!("            {{\n                let depth = __jmax_next_depth(depth)?;\n                sink.write_byte(b'[')?;\n                for (__jmax_index, {item}) in ({expr}).iter().enumerate() {{\n                    if __jmax_index != 0 {{ sink.write_byte(b',')?; }}\n"));
            emit_value(out, inner, &item, nesting + 1)?;
            out.push_str("                }\n                sink.write_byte(b']')?;\n            }\n");
        }
        EndpointType::Optional(inner) => {
            let item = format!("__jmax_some_{nesting}");
            out.push_str(&format!("            match ({expr}).as_ref() {{\n                ::core::option::Option::Some({item}) => {{\n"));
            emit_value(out, inner, &item, nesting + 1)?;
            out.push_str("                }\n                ::core::option::Option::None => sink.write_bytes(b\"null\")?,\n            }\n");
        }
        _ => return Err(constraint("unhandled model encoder type")),
    }
    Ok(())
}

fn emit_encode_impl(out: &mut String, name: &str) {
    out.push_str(&format!("impl __jmax_abi::JsonEncode for {name} {{\n    fn encode_json(&self, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {{\n        self.__jmax_encode(sink, 0)\n    }}\n}}\n"));
}

fn emit_enum(out: &mut String, original: &str, variants: &[EnumVariantSpec], options: &CodegenOptions) -> Result<()> {
    let name = pascal(original)?;
    out.push_str(&format!("#[derive(Clone, Copy, Debug, PartialEq, Eq)]\n#[repr(i32)]\npub enum {name} {{\n"));
    for variant in variants {
        out.push_str(&format!("    {} = {},\n", pascal(&variant.name)?, variant.value));
    }
    out.push_str(&format!("}}\nimpl {name} {{\n    pub(super) fn __jmax_encode(&self, sink: &mut dyn __jmax_abi::ByteSink, _depth: usize) -> __jmax_abi::Result<()> {{\n        __jmax_aot::encode::write_i64(i64::from(*self as i32), sink)\n    }}\n"));
    if options.compatibility.emit_definition_variants {
        out.push_str("    pub const JMAX_DEFINITION_VARIANTS: &'static [(&'static str, i32, &'static str)] = &[\n");
        for variant in variants {
            out.push_str(&format!("        ({}, {}, {}),\n", rust_string_literal(&variant.name), variant.value, rust_string_literal(&variant.description)));
        }
        out.push_str("    ];\n");
    }
    out.push_str("}\n");
    emit_encode_impl(out, &name);
    if options.emit_views {
        // Enum views need no field resolution or registry.
        emit_view_shell(out, &name);
        out.push_str("}\n");
        emit_view_encode(out, &name);
    }
    Ok(())
}

fn emit_view_shell(out: &mut String, name: &str) {
    out.push_str(&format!("#[derive(Clone, Copy, Debug, PartialEq)]\npub struct {name}View<'a> {{\n    value: &'a {name},\n}}\nimpl<'a> {name}View<'a> {{\n    pub fn new(value: &'a {name}) -> Self {{ Self {{ value }} }}\n    pub fn value(&self) -> &'a {name} {{ self.value }}\n"));
}

fn emit_view_encode(out: &mut String, name: &str) {
    out.push_str(&format!("impl<'a> __jmax_abi::JsonEncode for {name}View<'a> {{\n    fn encode_json(&self, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {{\n        <{name} as __jmax_abi::JsonEncode>::encode_json(self.value, sink)\n    }}\n}}\n"));
}

fn emit_view<'a>(out: &mut String, name: &str, fields: Option<&'a [EndpointField]>, options: &CodegenOptions,
    registry: &Registry<'a>) -> Result<()> {
    emit_view_shell(out, name);
    if let Some(fields) = fields {
        for field in fields {
            let member = field_name(&field.name)?;
            let ty = resolved_source(&field.ty, RustTypeMode::View, options, registry, 0)?;
            let expr = view_expr(&field.ty, &format!("&self.value.{member}"), options, registry, 0)?;
            out.push_str(&format!("    pub fn {member}(&self) -> {ty} {{ {expr} }}\n"));
        }
    }
    out.push_str("}\n");
    emit_view_encode(out, name);
    Ok(())
}

fn view_expr<'a>(ty: &'a EndpointType, expr: &str, options: &CodegenOptions, registry: &Registry<'a>, depth: usize) -> Result<String> {
    check_depth(depth)?;
    if string_like(ty) { return Ok(format!("({expr}).as_str()")); }
    match ty {
        EndpointType::Bytea | EndpointType::Vec(_) => Ok(format!("({expr}).as_slice()")),
        EndpointType::Object => Ok(String::from(expr)),
        EndpointType::Optional(inner) => {
            let value = format!("__jmax_borrow_{depth}");
            let inner = view_expr(inner, &value, options, registry, depth + 1)?;
            Ok(format!("({expr}).as_ref().map(|{value}| {inner})"))
        }
        EndpointType::Struct { .. } | EndpointType::StructRef(_) | EndpointType::StructTable { .. }
        | EndpointType::Enum { .. } | EndpointType::EnumRef { .. } => {
            let ty = resolved_source(ty, RustTypeMode::View, options, registry, depth)?;
            // The lifetime on a constructor path is inferred from the owner.
            let name = ty.strip_suffix("<'a>").ok_or_else(|| constraint("nominal view lacks lifetime"))?;
            Ok(format!("{name}::new({expr})"))
        }
        _ => Ok(format!("*({expr})")),
    }
}

fn option_u32(value: Option<u32>) -> String {
    match value {
        Some(value) => format!("::core::option::Option::Some({value}u32)"),
        None => String::from("::core::option::Option::None"),
    }
}

fn option_i64(value: Option<i64>) -> String {
    match value {
        Some(value) => format!("::core::option::Option::Some({value}i64)"),
        None => String::from("::core::option::Option::None"),
    }
}

fn scalar_source(kind: Option<jmax_abi::syntax::ScalarKind>) -> &'static str {
    use jmax_abi::syntax::ScalarKind;
    match kind {
        Some(ScalarKind::Null) => "::core::option::Option::Some(__jmax_abi::syntax::ScalarKind::Null)",
        Some(ScalarKind::True) => "::core::option::Option::Some(__jmax_abi::syntax::ScalarKind::True)",
        Some(ScalarKind::False) => "::core::option::Option::Some(__jmax_abi::syntax::ScalarKind::False)",
        Some(ScalarKind::Number) => "::core::option::Option::Some(__jmax_abi::syntax::ScalarKind::Number)",
        Some(ScalarKind::String) => "::core::option::Option::Some(__jmax_abi::syntax::ScalarKind::String)",
        None => "::core::option::Option::None",
    }
}

fn slot_decoder(ty: &EndpointType) -> Result<(&'static str, &'static str)> {
    if string_like(ty) || matches!(ty, EndpointType::Bytea) { return Ok(("Str", "decode_str")); }
    match ty {
        EndpointType::UInt32 => Ok(("U64", "decode_u32")),
        EndpointType::Int32 => Ok(("I64", "decode_i32")),
        EndpointType::Int64 | EndpointType::TimeStampMs => Ok(("I64", "decode_i64")),
        EndpointType::Float64 => Ok(("F64", "decode_f64")),
        EndpointType::Boolean => Ok(("Bool", "decode_bool")),
        _ => Err(codegen_error(ErrorCode::UnsupportedSchemaFeature, "type has no flat model decoder")),
    }
}

fn emit_schema(out: &mut String, name: &str, fields: &[EndpointField], schema: &SchemaEntry) -> Result<()> {
    let ir = &schema.ir;
    let id = ir.schema_id.0;
    out.push_str(&format!("impl __jmax_abi::SchemaProvider for {name} {{\n    fn schema_ir() -> __jmax_abi::SchemaIr {{\n        __jmax_abi::SchemaIr {{\n            name: alloc::string::String::from({}),\n            schema_id: __jmax_abi::SchemaId({id}u32),\n            required_count: {}u16,\n            reject_unknown: true,\n            fields: alloc::vec![\n", rust_string_literal(&ir.name), ir.required_count));
    for field in &ir.fields {
        let format = match &field.format {
            Some(value) => format!("::core::option::Option::Some(alloc::string::String::from({}))", rust_string_literal(value)),
            None => String::from("::core::option::Option::None"),
        };
        out.push_str(&format!("                __jmax_abi::SchemaFieldIr {{ field_id: __jmax_abi::FieldId({}u32), name: alloc::string::String::from({}), required: {}, scalar_kind: {}, format: {format}, min_len: {}, max_len: {}, min_i64: {}, max_i64: {} }},\n",
            field.field_id.0, rust_string_literal(&field.name), field.required, scalar_source(field.scalar_kind),
            option_u32(field.min_len), option_u32(field.max_len), option_i64(field.min_i64), option_i64(field.max_i64)));
    }
    out.push_str(&format!("            ],\n        }}\n    }}\n    fn schema_fingerprint() -> u64 {{ {}u64 }}\n}}\n", schema.fingerprint));
    out.push_str(&format!("impl __jmax_abi::JsonDecode for {name} {{\n    fn decode_slots(slots: &__jmax_abi::ValidatedSlots) -> __jmax_abi::Result<Self> {{\n        let specs: &[__jmax_aot::SlotSpec] = &[\n"));
    for (index, field) in fields.iter().enumerate() {
        let (inner, required) = match &field.ty {
            EndpointType::Optional(inner) => (inner.as_ref(), false),
            ty => (ty, true),
        };
        let (kind, _) = slot_decoder(inner)?;
        out.push_str(&format!("            __jmax_aot::SlotSpec {{ index: {index}u32, field_id: __jmax_abi::FieldId({index}u32), kind: __jmax_aot::SlotKind::{kind}, required: {required} }},\n"));
    }
    out.push_str(&format!("        ];\n        let plan = __jmax_aot::DecodePlan {{ schema_fingerprint: {}u64, slots: specs }};\n        __jmax_aot::decode::validate_slots(slots, &plan).map_err(|error| {{ let field_id = error.field_id; error.with_schema({id}u32, field_id) }})?;\n        let reader = __jmax_aot::SlotReader::new(slots, {}u64).map_err(|error| {{ let field_id = error.field_id; error.with_schema({id}u32, field_id) }})?;\n        ::core::result::Result::Ok(Self {{\n", schema.fingerprint, schema.fingerprint));
    for (index, field) in fields.iter().enumerate() {
        let (inner, required) = match &field.ty {
            EndpointType::Optional(inner) => (inner.as_ref(), false),
            ty => (ty, true),
        };
        let (_, decoder) = slot_decoder(inner)?;
        let read = format!("__jmax_aot::decode::{decoder}(&reader, &specs[{index}]).map_err(|error| error.with_schema({id}u32, {index}u32))?");
        let converted = if matches!(inner, EndpointType::Bytea) {
            format!("({read}).map(__jmax_decode_base64).transpose().map_err(|error| error.with_schema({id}u32, {index}u32))?")
        } else if string_like(inner) {
            format!("({read}).map(alloc::string::String::from)")
        } else { read };
        let value = if required {
            format!("({converted}).ok_or_else(|| __jmax_abi::Error::new(__jmax_abi::ErrorCode::MissingRequiredField, __jmax_abi::ErrorSource::Decode, 0, \"missing required model field\").with_schema({id}u32, {index}u32))?")
        } else { converted };
        out.push_str(&format!("            {}: {value},\n", field_name(&field.name)?));
    }
    out.push_str("        })\n    }\n}\n");
    Ok(())
}

// Keep executable helper tokens and emitted helper source identical. The tests
// instantiate these same token bodies, rather than reimplementing the codecs.
macro_rules! source_tokens {
    ($($source:tt)*) => { concat!(stringify!($($source)*), "\n") };
}

macro_rules! depth_support {
    ($emit:ident) => { $emit! {
#[allow(dead_code)]
fn __jmax_next_depth(depth: usize) -> __jmax_abi::Result<usize> {
    if depth >= 128 {
        return ::core::result::Result::Err(__jmax_abi::Error::new(
            __jmax_abi::ErrorCode::DepthLimitExceeded, __jmax_abi::ErrorSource::Encode,
            0, "generated model encoding depth exceeds 128"));
    }
    ::core::result::Result::Ok(depth + 1)
}
    } };
}

const DEPTH_SUPPORT: &str = depth_support!(source_tokens);

macro_rules! base64_support {
    ($emit:ident) => { $emit! {
#[allow(dead_code)]
fn __jmax_encode_base64(value: &[u8], sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    sink.write_byte(b'"')?;
    for chunk in value.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        let encoded = [ALPHABET[usize::from(a >> 2)], ALPHABET[usize::from(((a & 3) << 4) | (b >> 4))],
            if chunk.len() > 1 { ALPHABET[usize::from(((b & 15) << 2) | (c >> 6))] } else { b'=' },
            if chunk.len() > 2 { ALPHABET[usize::from(c & 63)] } else { b'=' }];
        sink.write_bytes(&encoded)?;
    }
    sink.write_byte(b'"')
}
#[allow(dead_code)]
fn __jmax_decode_base64(value: &str) -> __jmax_abi::Result<alloc::vec::Vec<u8>> {
    fn invalid() -> __jmax_abi::Error {
        __jmax_abi::Error::new(__jmax_abi::ErrorCode::FormatMismatch,
            __jmax_abi::ErrorSource::Decode, 0, "noncanonical padded standard base64")
    }
    fn sextet(byte: u8) -> __jmax_abi::Result<u8> {
        match byte {
            b'A'..=b'Z' => ::core::result::Result::Ok(byte - b'A'),
            b'a'..=b'z' => ::core::result::Result::Ok(byte - b'a' + 26),
            b'0'..=b'9' => ::core::result::Result::Ok(byte - b'0' + 52),
            b'+' => ::core::result::Result::Ok(62),
            b'/' => ::core::result::Result::Ok(63),
            _ => ::core::result::Result::Err(invalid()),
        }
    }
    let bytes = value.as_bytes();
    if bytes.len() % 4 != 0 { return ::core::result::Result::Err(invalid()); }
    let mut out = alloc::vec::Vec::new();
    out.try_reserve((bytes.len() / 4) * 3).map_err(|_| __jmax_abi::Error::new(
        __jmax_abi::ErrorCode::BudgetExhausted, __jmax_abi::ErrorSource::Decode,
        0, "base64 decoded capacity reservation failed"))?;
    for (index, chunk) in bytes.chunks_exact(4).enumerate() {
        let a = sextet(chunk[0])?;
        let b = sextet(chunk[1])?;
        let last = index == bytes.len() / 4 - 1;
        if chunk[2] == b'=' {
            if !last || chunk[3] != b'=' || b & 15 != 0 { return ::core::result::Result::Err(invalid()); }
            out.push((a << 2) | (b >> 4));
        } else {
            let c = sextet(chunk[2])?;
            if chunk[3] == b'=' {
                if !last || c & 3 != 0 { return ::core::result::Result::Err(invalid()); }
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
            } else {
                let d = sextet(chunk[3])?;
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
                out.push((c << 6) | d);
            }
        }
    }
    ::core::result::Result::Ok(out)
}
    } };
}

const BASE64_SUPPORT: &str = base64_support!(source_tokens);

macro_rules! object_support {
    ($emit:ident) => { $emit! {
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JsonObject(pub alloc::vec::Vec<(alloc::string::String, JsonValue)>);
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JsonValue {
    Null,
    Bool(bool),
    U64(u64),
    I64(i64),
    F64Bits(u64),
    String(alloc::string::String),
    Array(alloc::vec::Vec<JsonValue>),
    Object(JsonObject),
}
impl JsonObject {
    pub(super) fn __jmax_encode(&self, sink: &mut dyn __jmax_abi::ByteSink, depth: usize) -> __jmax_abi::Result<()> {
        let depth = __jmax_next_depth(depth)?;
        let mut keys = alloc::collections::BTreeSet::new();
        for (key, _) in &self.0 {
            if !keys.insert(key.as_str()) {
                return ::core::result::Result::Err(__jmax_abi::Error::new(
                    __jmax_abi::ErrorCode::DuplicateObjectKey, __jmax_abi::ErrorSource::Encode,
                    0, "duplicate generated JSON object key"));
            }
        }
        sink.write_byte(b'{')?;
        for (index, (key, value)) in self.0.iter().enumerate() {
            if index != 0 { sink.write_byte(b',')?; }
            __jmax_aot::encode::write_json_string(key.as_str(), sink)?;
            sink.write_byte(b':')?;
            value.__jmax_encode(sink, depth)?;
        }
        sink.write_byte(b'}')
    }
}
impl JsonValue {
    pub(super) fn __jmax_encode(&self, sink: &mut dyn __jmax_abi::ByteSink, depth: usize) -> __jmax_abi::Result<()> {
        match self {
            Self::Null => sink.write_bytes(b"null"),
            Self::Bool(value) => __jmax_aot::encode::write_bool(*value, sink),
            Self::U64(value) => __jmax_aot::encode::write_u64(*value, sink),
            Self::I64(value) => __jmax_aot::encode::write_i64(*value, sink),
            Self::F64Bits(value) => __jmax_aot::encode::write_f64(f64::from_bits(*value), sink),
            Self::String(value) => __jmax_aot::encode::write_json_string(value.as_str(), sink),
            Self::Object(value) => value.__jmax_encode(sink, depth),
            Self::Array(values) => {
                let depth = __jmax_next_depth(depth)?;
                sink.write_byte(b'[')?;
                for (index, value) in values.iter().enumerate() {
                    if index != 0 { sink.write_byte(b',')?; }
                    value.__jmax_encode(sink, depth)?;
                }
                sink.write_byte(b']')
            }
        }
    }
}
impl __jmax_abi::JsonEncode for JsonObject {
    fn encode_json(&self, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {
        self.__jmax_encode(sink, 0)
    }
}
impl __jmax_abi::JsonEncode for JsonValue {
    fn encode_json(&self, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {
        self.__jmax_encode(sink, 0)
    }
}
    } };
}

const OBJECT_SUPPORT: &str = object_support!(source_tokens);

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec};
    use crate::{EndpointSchemaSpec, ServiceSpec, TypeDefinition};

    fn options() -> CodegenOptions {
        CodegenOptions { emit_schema_tables: false, ..CodegenOptions::default() }
    }

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::new() }
    }

    fn record(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn model(ty: EndpointType) -> EndpointModel {
        let name = match &ty {
            EndpointType::Struct { name, .. } | EndpointType::Enum { name, .. } => name.clone(),
            _ => String::from("Alias"),
        };
        EndpointModel { definitions: vec![TypeDefinition { name, ty }], ..EndpointModel::default() }
    }

    fn variant(name: &str, value: i32) -> EnumVariantSpec {
        EnumVariantSpec { name: String::from(name), value, description: String::new() }
    }

    fn enumeration(name: &str, variants: Vec<EnumVariantSpec>) -> EndpointType {
        EndpointType::Enum { name: String::from(name), variants }
    }

    fn generated(ty: EndpointType) -> String {
        generate_models(&model(ty), &options()).unwrap().contents
    }

    #[test]
    fn scalar_owned_and_view_vectors() {
        let options = options();
        let vectors = [
            (EndpointType::UInt32, "u32"), (EndpointType::Int32, "i32"),
            (EndpointType::Int64, "i64"), (EndpointType::TimeStampMs, "i64"),
            (EndpointType::Float64, "f64"), (EndpointType::Boolean, "bool"),
            (EndpointType::Unit, "()"), (EndpointType::BlockchainDecimal, "::jmax_abi::Decimal128"),
        ];
        for (ty, expected) in vectors {
            for mode in [RustTypeMode::Owned, RustTypeMode::View] {
                assert_eq!(rust_type(&ty, mode, &options).unwrap(), RustType {
                    source: String::from(expected), needs_lifetime: false,
                });
            }
        }
    }

    #[test]
    fn borrowed_strings_preserve_wrapper_contents() {
        for ty in [EndpointType::String, EndpointType::UUID, EndpointType::NanoId { len: 1 },
            EndpointType::NanoId { len: u8::MAX }, EndpointType::IpAddr,
            EndpointType::BlockchainAddress, EndpointType::BlockchainTransactionHash] {
            assert_eq!(rust_type(&ty, RustTypeMode::Owned, &options()).unwrap().source, "super::alloc::string::String");
            assert_eq!(rust_type(&ty, RustTypeMode::View, &options()).unwrap(), RustType {
                source: String::from("&'a str"), needs_lifetime: true,
            });
        }
        assert_eq!(rust_type(&EndpointType::NanoId { len: 0 }, RustTypeMode::Owned, &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn containers_and_boxed_references_have_exact_types() {
        let ty = EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::String))));
        assert_eq!(rust_type(&ty, RustTypeMode::View, &options()).unwrap().source,
            "&'a [::core::option::Option<super::alloc::string::String>]");
        let ty = EndpointType::Optional(Box::new(EndpointType::UInt32));
        assert!(!rust_type(&ty, RustTypeMode::View, &options()).unwrap().needs_lifetime);
        for ty in [EndpointType::StructRef(String::from("HTTPServer")),
            EndpointType::StructTable { struct_ref: String::from("HTTPServer") }] {
            assert_eq!(rust_type(&ty, RustTypeMode::Owned, &options()).unwrap().source,
                "super::alloc::boxed::Box<super::models::HttpServer>");
            assert_eq!(rust_type(&ty, RustTypeMode::View, &options()).unwrap().source,
                "super::models::HttpServerView<'a>");
        }
        assert_eq!(rust_type(&EndpointType::Bytea, RustTypeMode::View, &options()).unwrap().source, "&'a [u8]");
        assert_eq!(rust_type(&EndpointType::Object, RustTypeMode::View, &options()).unwrap().source,
            "&'a super::models::JsonObject");
    }

    #[test]
    fn enum_prefix_flag_never_changes_canonical_mapping() {
        let a = EndpointType::EnumRef { name: String::from("State"), prefixed_name: false };
        let b = EndpointType::EnumRef { name: String::from("State"), prefixed_name: true };
        for mode in [RustTypeMode::Owned, RustTypeMode::View] {
            assert_eq!(rust_type(&a, mode, &options()).unwrap(), rust_type(&b, mode, &options()).unwrap());
        }
    }

    #[test]
    fn type_depth_boundary_is_checked_before_recursion() {
        let mut ty = EndpointType::Boolean;
        for _ in 0..127 { ty = EndpointType::Optional(Box::new(ty)); }
        assert!(rust_type(&ty, RustTypeMode::Owned, &options()).is_ok());
        ty = EndpointType::Optional(Box::new(ty));
        assert_eq!(rust_type(&ty, RustTypeMode::Owned, &options()).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        assert!(check_depth(127).is_ok());
        assert_eq!(check_depth(128).unwrap_err().code, ErrorCode::DepthLimitExceeded);
        assert_eq!(check_depth(usize::MAX).unwrap_err().code, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn empty_artifact_is_complete_and_deterministic() {
        let model = EndpointModel::default();
        let a = generate_models(&model, &CodegenOptions::default()).unwrap();
        assert_eq!(a, generate_models(&model, &CodegenOptions::default()).unwrap());
        assert_eq!(a.path, "generated/models.rs");
        assert_eq!(a.kind, ArtifactKind::Models);
        assert!(a.contents.starts_with(GENERATED_NOTICE));
        assert!(a.contents.ends_with('\n'));
        assert!(!a.contents.ends_with("\n\n"));
        assert!(!a.contents.contains('\r'));
        assert!(!a.contents.contains("pub struct JsonObject"));
    }

    #[test]
    fn transformed_nominal_and_alias_names_cannot_collide() {
        let model = EndpointModel { definitions: vec![
            TypeDefinition { name: String::from("HTTPServer"), ty: record("HTTPServer", vec![]) },
            TypeDefinition { name: String::from("http_server"), ty: EndpointType::UInt32 },
        ], ..EndpointModel::default() };
        assert_eq!(generate_models(&model, &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn support_and_view_type_names_are_reserved() {
        let ty = record("JsonObject", vec![field("object", EndpointType::Object)]);
        assert_eq!(generate_models(&model(ty), &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
        let model = EndpointModel { definitions: vec![
            TypeDefinition { name: String::from("User"), ty: record("User", vec![]) },
            TypeDefinition { name: String::from("UserView"), ty: EndpointType::UInt32 },
        ], ..EndpointModel::default() };
        assert_eq!(generate_models(&model, &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
        let mut options = options();
        options.emit_views = false;
        assert!(generate_models(&model, &options).is_ok());
    }

    #[test]
    fn legal_keywords_use_raw_fields_and_wire_names_remain_original() {
        let source = generated(record("Message", vec![field("type", EndpointType::String), field("HTTPServerID", EndpointType::Int64)]));
        assert!(source.contains("pub r#type: super::alloc::string::String"));
        assert!(source.contains("pub fn r#type(&self) -> &'a str"));
        assert!(source.contains("write_json_string(\"HTTPServerID\", sink)"));
        assert!(source.contains("pub http_server_id: i64"));
        for forbidden in ["self", "Self", "super", "crate", "_"] {
            assert_eq!(field_name(forbidden).unwrap_err().code, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn getter_and_field_collisions_fail_with_field_context() {
        for fields in [vec![field("value", EndpointType::Boolean)], vec![field("new", EndpointType::Boolean)],
            vec![field("userID", EndpointType::Int32), field("user_id", EndpointType::Int32)]] {
            let error = generate_models(&model(record("Message", fields)), &options()).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
            assert_eq!(error.source, crate::ErrorSource::SchemaCompile);
        }
    }

    #[test]
    fn enum_discriminants_and_bridge_metadata_are_exact() {
        let ty = enumeration("State", vec![variant("Minimum", i32::MIN),
            EnumVariantSpec { name: String::from("Maximum"), value: i32::MAX, description: String::from("é\n\"\\") }]);
        let mut options = options();
        options.compatibility.emit_definition_variants = true;
        let source = generate_models(&model(ty), &options).unwrap().contents;
        assert!(source.contains("#[repr(i32)]"));
        assert!(source.contains("Minimum = -2147483648"));
        assert!(source.contains("Maximum = 2147483647"));
        assert!(source.contains("write_i64(i64::from(*self as i32), sink)"));
        assert!(source.contains("JMAX_DEFINITION_VARIANTS"));
        assert!(source.contains("\"é\\n\\\"\\\\\""));
        assert!(source.contains("pub struct StateView<'a>"));
    }

    #[test]
    fn malformed_enums_are_explicit_constraint_errors() {
        for variants in [vec![], vec![variant("A", 1), variant("B", 1)],
            vec![variant("HTTP", 1), variant("Http", 2)]] {
            let error = generate_models(&model(enumeration("State", variants)), &options()).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn aliases_receive_no_foreign_trait_implementations() {
        let source = generated(EndpointType::Vec(Box::new(EndpointType::Float64)));
        assert!(source.contains("pub type Alias = super::alloc::vec::Vec<f64>;"));
        assert!(!source.contains("impl __jmax_abi::JsonEncode for Alias"));
        assert!(!source.contains("impl __jmax_abi::SchemaProvider for Alias"));
    }

    #[test]
    fn recursive_nominals_are_boxed_and_encoding_preserves_depth() {
        let source = generated(record("Node", vec![field("next", EndpointType::Optional(Box::new(
            EndpointType::StructRef(String::from("Node")))))]));
        assert!(source.contains("Option<super::alloc::boxed::Box<super::models::Node>>"));
        assert!(source.contains("(__jmax_value).__jmax_encode(sink, depth)?"));
        assert!(source.contains("super::models::NodeView::new(__jmax_borrow_0)"));
        assert!(source.contains("let depth = __jmax_next_depth(depth)?"));
    }

    #[test]
    fn absent_record_fields_and_optional_array_elements_differ() {
        let source = generated(record("Message", vec![
            field("maybe", EndpointType::Optional(Box::new(EndpointType::Int32))),
            field("items", EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::Int32))))),
        ]));
        assert!(source.contains("if let ::core::option::Option::Some(__jmax_value) = &self.maybe"));
        assert!(source.contains("::core::option::Option::None => sink.write_bytes(b\"null\")?"));
        assert!(source.contains("pub fn items(&self) -> &'a [::core::option::Option<i32>]"));
    }

    #[test]
    fn float_records_never_derive_eq_and_decimal_uses_exact_writer() {
        let source = generated(record("Numbers", vec![field("floating", EndpointType::Float64),
            field("decimal", EndpointType::BlockchainDecimal), field("unit", EndpointType::Unit)]));
        assert!(source.contains("#[derive(Clone, Debug, PartialEq)]\npub struct Numbers"));
        assert!(source.contains("write_f64(*(&self.floating), sink)?"));
        assert!(source.contains("write_decimal(*(&self.decimal), sink)?"));
        assert!(source.contains("sink.write_bytes(b\"null\")?"));
    }

    #[test]
    fn empty_record_has_object_encoding_and_owner_view() {
        let source = generated(record("Empty", vec![]));
        assert!(source.contains("sink.write_byte(b'{')?"));
        assert!(source.contains("sink.write_byte(b'}')"));
        assert!(source.contains("pub fn new(value: &'a Empty) -> Self"));
        assert!(source.contains("pub fn value(&self) -> &'a Empty"));
    }

    #[test]
    fn object_support_is_ordered_checked_and_conditional() {
        let source = generated(record("Payload", vec![field("data", EndpointType::Object)]));
        assert_eq!(source.matches("pub struct JsonObject").count(), 1);
        assert!(source.contains("pub enum JsonValue"));
        assert!(source.contains("DuplicateObjectKey"));
        assert!(source.contains("write_f64"));
        assert!(source.contains("F64Bits"));
        assert!(!source.contains("RawFragment"));
    }

    #[test]
    fn base64_source_has_standard_alphabet_and_padding_checks() {
        let source = generated(record("Bytes", vec![field("data", EndpointType::Bytea)]));
        assert!(source.contains("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"));
        assert!(source.contains("__jmax_encode_base64"));
        assert!(source.contains("__jmax_decode_base64"));
        assert!(source.contains("try_reserve"));
    }

    #[test]
    fn all_input_locations_contribute_inline_nominals() {
        let model = EndpointModel {
            definitions: vec![TypeDefinition { name: String::from("Container"), ty: record("Container", vec![
                field("child", record("Nested", vec![])),
            ]) }],
            services: vec![ServiceSpec { id: 1, name: String::from("api"), endpoints: vec![EndpointSchemaSpec {
                code: 1, name: String::from("get"), service: String::from("api"), roles: vec![],
                params: vec![field("query", record("Query", vec![]))],
                response: vec![field("result", enumeration("Status", vec![variant("Ready", 0)]))],
                stream_response: Some(EndpointType::Vec(Box::new(record("StreamItem", vec![])))), description: String::new(),
            }] }],
            headers: vec![crate::HeaderSpec { name: String::from("X-Metadata"), ty: record("Metadata", vec![]), required: false, description: String::new() }],
            ..EndpointModel::default()
        };
        let source = generate_models(&model, &options()).unwrap().contents;
        for name in ["Container", "Nested", "Query", "StreamItem", "Metadata"] {
            assert_eq!(source.matches(&format!("pub struct {name} {{")).count(), 1);
        }
        assert_eq!(source.matches("pub enum Status {").count(), 1);
    }

    #[test]
    fn custom_paths_are_validated_before_emission() {
        let mut options = options();
        options.module_name = String::from("client");
        options.abi_path = String::from("crate::abi");
        options.aot_path = String::from("crate::aot");
        let artifact = generate_models(&model(record("Row", vec![field("amount", EndpointType::BlockchainDecimal)])), &options).unwrap();
        assert_eq!(artifact.path, "client/models.rs");
        assert!(artifact.contents.contains("use crate::abi as __jmax_abi;"));
        assert!(artifact.contents.contains("pub amount: crate::abi::Decimal128"));
        options.abi_path = String::from("::abi; injected()");
        assert_eq!(generate_models(&EndpointModel::default(), &options).unwrap_err().code, ErrorCode::ConstraintViolation);
        assert_eq!(rust_type(&EndpointType::UInt32, RustTypeMode::Owned, &options).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn unresolved_and_wrong_kind_targets_fail() {
        let bad = model(record("Row", vec![field("child", EndpointType::StructRef(String::from("Missing")))]));
        assert_eq!(generate_models(&bad, &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
        let bad = EndpointModel { definitions: vec![
            TypeDefinition { name: String::from("Scalar"), ty: EndpointType::UInt32 },
            TypeDefinition { name: String::from("Row"), ty: record("Row", vec![field("child", EndpointType::StructRef(String::from("Scalar")))]) },
        ], ..EndpointModel::default() };
        assert_eq!(generate_models(&bad, &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn alias_only_cycles_are_rejected() {
        let model = EndpointModel { definitions: vec![
            TypeDefinition { name: String::from("A"), ty: EndpointType::StructRef(String::from("B")) },
            TypeDefinition { name: String::from("B"), ty: EndpointType::StructRef(String::from("A")) },
        ], ..EndpointModel::default() };
        assert!(generate_models(&model, &options()).is_err());
    }

    #[test]
    fn schema_identity_is_qualified_sorted_and_canonical() {
        let model = EndpointModel { definitions: vec![
            TypeDefinition { name: String::from("Zulu"), ty: record("Zulu", vec![field("count", EndpointType::UInt32)]) },
            TypeDefinition { name: String::from("Alpha"), ty: record("Alpha", vec![field("maybe", EndpointType::Optional(Box::new(EndpointType::String)))]) },
        ], ..EndpointModel::default() };
        let catalog = Catalog::collect(&model).unwrap();
        let schemas = schemas(&model, &catalog, &CodegenOptions::default()).unwrap();
        assert_eq!(schemas["model::Alpha"].ir.schema_id, SchemaId(1));
        assert_eq!(schemas["model::Zulu"].ir.schema_id, SchemaId(2));
        for schema in schemas.values() {
            assert_eq!(schema.fingerprint, jmax_schema_aot::fingerprint_schema(&schema.ir).unwrap());
            assert!(schema.ir.reject_unknown);
        }
        let source = generate_models(&model, &CodegenOptions::default()).unwrap().contents;
        assert!(source.contains("impl __jmax_abi::SchemaProvider for Alpha"));
        assert!(source.contains("impl __jmax_abi::JsonDecode for Zulu"));
        assert!(source.contains("decode_u32(&reader"));
        assert!(source.contains("decode::validate_slots(slots, &plan)"));
        assert!(source.contains("SlotReader::new(slots"));
        assert!(source.contains("required: false"));
    }

    #[test]
    fn unsupported_schemas_fail_instead_of_claiming_decoding() {
        for ty in [EndpointType::BlockchainDecimal, EndpointType::Object, EndpointType::Unit,
            EndpointType::Vec(Box::new(EndpointType::UInt32)),
            EndpointType::Optional(Box::new(EndpointType::Optional(Box::new(EndpointType::String))))] {
            let model = model(record("Row", vec![field("data", ty)]));
            assert_eq!(generate_models(&model, &CodegenOptions::default()).unwrap_err().code, ErrorCode::UnsupportedSchemaFeature);
            let source = generate_models(&model, &options()).unwrap().contents;
            assert!(!source.contains("impl __jmax_abi::SchemaProvider"));
            assert!(!source.contains("impl __jmax_abi::JsonDecode"));
        }
    }

    #[test]
    fn flat_bytea_decoder_is_canonical_and_attaches_context() {
        let source = generate_models(&model(record("Bytes", vec![field("data", EndpointType::Bytea)])),
            &CodegenOptions::default()).unwrap().contents;
        assert!(source.contains("map(__jmax_decode_base64).transpose()"));
        assert!(source.contains("error.with_schema(1u32, 0u32)"));
        assert!(source.contains("FormatMismatch"));
    }

    #[test]
    fn direct_mapper_rejects_invalid_nominal_symbols() {
        for name in ["", "_", "bad;name", "é", "123"] {
            assert_eq!(rust_type(&EndpointType::StructRef(String::from(name)), RustTypeMode::Owned,
                &options()).unwrap_err().code, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn model_and_field_order_are_not_modified() {
        let model = model(record("Row", vec![field("z", EndpointType::Int32), field("a", EndpointType::Int32)]));
        let before = model.clone();
        let a = generate_models(&model, &options()).unwrap();
        assert_eq!(model, before);
        assert!(a.contents.find("pub z:").unwrap() < a.contents.find("pub a:").unwrap());
        assert_eq!(a, generate_models(&model, &options()).unwrap());
    }

    #[test]
    fn source_literals_escape_untrusted_field_metadata() {
        let wire = "wire\"\\\n\u{0}";
        let mut out = String::new();
        emit_key(&mut out, &rust_string_literal(wire));
        assert!(out.contains("wire\\\"\\\\\\n\\u{0}"));
        assert!(!out.contains('\0'));
    }

    #[test]
    fn canonical_reference_aliases_do_not_introduce_extra_boxes() {
        let model = EndpointModel { definitions: vec![
            TypeDefinition { name: String::from("Node"), ty: record("Node", vec![]) },
            TypeDefinition { name: String::from("Link"), ty: EndpointType::StructRef(String::from("Node")) },
            TypeDefinition { name: String::from("Tree"), ty: record("Tree", vec![
                field("root", EndpointType::StructRef(String::from("Link"))),
            ]) },
        ], ..EndpointModel::default() };
        let source = generate_models(&model, &options()).unwrap().contents;
        assert!(source.contains("pub type Link = super::alloc::boxed::Box<super::models::Node>;"));
        assert!(source.contains("pub root: super::alloc::boxed::Box<super::models::Node>"));
        assert!(source.contains("pub fn root(&self) -> super::models::NodeView<'a>"));
    }

    #[test]
    fn empty_records_and_endpoint_candidates_share_dense_schema_ids() {
        let model = EndpointModel {
            definitions: vec![TypeDefinition { name: String::from("Empty"), ty: record("Empty", vec![]) }],
            services: vec![ServiceSpec { id: 1, name: String::from("api"), endpoints: vec![EndpointSchemaSpec {
                code: 7, name: String::from("get"), service: String::from("api"), roles: vec![],
                params: vec![], response: vec![field("ok", EndpointType::Boolean)],
                stream_response: None, description: String::new(),
            }] }],
            ..EndpointModel::default()
        };
        let catalog = Catalog::collect(&model).unwrap();
        let schemas = schemas(&model, &catalog, &CodegenOptions::default()).unwrap();
        assert_eq!(schemas["model::Empty"].ir.schema_id, SchemaId(1));
        assert_eq!(schemas["request::api.get"].ir.schema_id, SchemaId(2));
        assert_eq!(schemas["response::api.get"].ir.schema_id, SchemaId(3));
        assert!(schemas["model::Empty"].ir.fields.is_empty());
        assert!(generate_models(&model, &CodegenOptions::default()).is_ok());
    }

    #[test]
    fn decoder_dispatch_and_literal_boundaries_are_exhaustive() {
        for (ty, expected) in [(EndpointType::UInt32, ("U64", "decode_u32")),
            (EndpointType::Int32, ("I64", "decode_i32")),
            (EndpointType::Int64, ("I64", "decode_i64")),
            (EndpointType::TimeStampMs, ("I64", "decode_i64")),
            (EndpointType::Float64, ("F64", "decode_f64")),
            (EndpointType::Boolean, ("Bool", "decode_bool")),
            (EndpointType::Bytea, ("Str", "decode_str"))] {
            assert_eq!(slot_decoder(&ty).unwrap(), expected);
        }
        assert_eq!(slot_decoder(&EndpointType::Unit).unwrap_err().code, ErrorCode::UnsupportedSchemaFeature);
        assert_eq!(option_i64(Some(i64::MIN)), "::core::option::Option::Some(-9223372036854775808i64)");
        assert_eq!(option_i64(Some(i64::MAX)), "::core::option::Option::Some(9223372036854775807i64)");
        assert_eq!(option_u32(Some(u32::MAX)), "::core::option::Option::Some(4294967295u32)");
        assert_eq!(option_u32(None), "::core::option::Option::None");
        assert_eq!(option_i64(None), "::core::option::Option::None");
    }

    // These are the exact helper token bodies that become generated source.
    // No compiler subprocess or generated-source evaluation is needed to test
    // their arithmetic, recursive limits, and propagation of ABI sink errors.
    mod runtime {
        use alloc::{format, string::String, vec, vec::Vec};
        use jmax_abi as __jmax_abi;
        use jmax_schema_aot as __jmax_aot;
        use __jmax_abi::{ByteSink, Error, ErrorCode, ErrorSource, JsonEncode, Result};

        macro_rules! items {
            ($($source:tt)*) => { $($source)* };
        }

        depth_support!(items);
        base64_support!(items);
        object_support!(items);

        fn sentinel(offset: usize) -> Error {
            Error::new(ErrorCode::Cancelled, ErrorSource::Encode,
                u32::try_from(offset).unwrap_or(u32::MAX), "deterministic sink failure").with_schema(99, 17)
        }

        struct LimitedSink {
            bytes: Vec<u8>,
            limit: usize,
        }

        impl ByteSink for LimitedSink {
            fn reserve(&mut self, additional: usize) -> Result<()> {
                if additional > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(sentinel(self.bytes.len()));
                }
                ByteSink::reserve(&mut self.bytes, additional)
            }

            fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
                self.reserve(bytes.len())?;
                self.bytes.extend_from_slice(bytes);
                Ok(())
            }

            fn len(&self) -> usize { self.bytes.len() }
        }

        #[test]
        fn base64_rfc4648_and_binary_vectors_roundtrip() {
            let vectors: &[(&[u8], &str)] = &[
                (b"", ""), (b"f", "Zg=="), (b"fo", "Zm8="), (b"foo", "Zm9v"),
                (b"foob", "Zm9vYg=="), (b"fooba", "Zm9vYmE="), (b"foobar", "Zm9vYmFy"),
                (&[0], "AA=="), (&[0, 0], "AAA="), (&[0xff, 0xff, 0xff], "////"),
                (&[0xfb, 0xff], "+/8="),
            ];
            for &(bytes, wire) in vectors {
                let mut encoded = Vec::new();
                __jmax_encode_base64(bytes, &mut encoded).unwrap();
                assert_eq!(encoded, format!("\"{wire}\"").as_bytes());
                assert_eq!(__jmax_decode_base64(wire).unwrap(), bytes);
            }
            let bytes: Vec<u8> = (0..=u8::MAX).collect();
            let mut encoded = Vec::new();
            __jmax_encode_base64(&bytes, &mut encoded).unwrap();
            let wire = core::str::from_utf8(&encoded[1..encoded.len() - 1]).unwrap();
            assert_eq!(__jmax_decode_base64(wire).unwrap(), bytes);
        }

        #[test]
        fn base64_rejects_bad_padding_alphabets_and_nonzero_pad_bits() {
            for wire in ["Zg", "Zg=", "Zg===", "Zh==", "Zm9=", "Zg==AAAA", "=AAA",
                "A===", "AA=A", "_AAA", "-AAA", " AA==", "/w==\n", "éAA", "AAAA===="] {
                let error = __jmax_decode_base64(wire).unwrap_err();
                assert_eq!(error.code, ErrorCode::FormatMismatch, "{wire:?}");
                assert_eq!(error.source, ErrorSource::Decode);
            }
        }

        #[test]
        fn base64_propagates_each_sink_failure_without_replacement() {
            for limit in 0..10 {
                let mut sink = LimitedSink { bytes: Vec::new(), limit };
                let error = __jmax_encode_base64(b"foobar", &mut sink).unwrap_err();
                assert_eq!(error, sentinel(sink.bytes.len()));
                assert!(sink.bytes.len() <= limit);
            }
            let mut sink = LimitedSink { bytes: Vec::new(), limit: 10 };
            __jmax_encode_base64(b"foobar", &mut sink).unwrap();
            assert_eq!(sink.bytes, b"\"Zm9vYmFy\"");
        }

        #[test]
        fn ordered_objects_encode_every_json_value_kind() {
            let value = JsonObject(vec![
                (String::from("z"), JsonValue::Null),
                (String::from("b"), JsonValue::Bool(true)),
                (String::from("u"), JsonValue::U64(u64::MAX)),
                (String::from("i"), JsonValue::I64(i64::MIN)),
                (String::from("s"), JsonValue::String(String::from("é\n\"\\"))),
                (String::from("a"), JsonValue::Array(vec![JsonValue::Bool(false),
                    JsonValue::Object(JsonObject::default()), JsonValue::F64Bits(1.5f64.to_bits())])),
            ]);
            let mut bytes = Vec::new();
            value.encode_json(&mut bytes).unwrap();
            assert_eq!(bytes, "{\"z\":null,\"b\":true,\"u\":18446744073709551615,\"i\":-9223372036854775808,\"s\":\"é\\n\\\"\\\\\",\"a\":[false,{},1.5]}".as_bytes());
        }

        #[test]
        fn duplicate_keys_fail_before_object_output() {
            for key in ["", "a", "é", "\"\\\n"] {
                let value = JsonObject(vec![(String::from(key), JsonValue::Null),
                    (String::from(key), JsonValue::Bool(false))]);
                let mut bytes = b"prefix".to_vec();
                let error = value.encode_json(&mut bytes).unwrap_err();
                assert_eq!(error.code, ErrorCode::DuplicateObjectKey);
                assert_eq!(error.source, ErrorSource::Encode);
                assert_eq!(bytes, b"prefix");
            }
            let value = JsonObject(vec![(String::from("é"), JsonValue::Null),
                (String::from("e\u{301}"), JsonValue::Null)]);
            value.encode_json(&mut Vec::new()).unwrap();
        }

        #[test]
        fn object_numbers_reject_nonfinite_values_and_preserve_finite_bits() {
            for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
                let mut bytes = Vec::new();
                let error = JsonValue::F64Bits(value.to_bits()).encode_json(&mut bytes).unwrap_err();
                assert_eq!(error.code, ErrorCode::InvalidNumber);
                assert_eq!(error.source, ErrorSource::Encode);
                assert!(bytes.is_empty());
            }
            for value in [0.0f64, -0.0, f64::MIN, f64::MAX, f64::MIN_POSITIVE, f64::from_bits(1)] {
                let mut bytes = Vec::new();
                JsonValue::F64Bits(value.to_bits()).encode_json(&mut bytes).unwrap();
                let decoded: f64 = core::str::from_utf8(&bytes).unwrap().parse().unwrap();
                assert_eq!(decoded.to_bits(), value.to_bits());
            }
        }

        #[test]
        fn encoding_depth_limit_is_inclusive_and_cannot_wrap() {
            assert_eq!(__jmax_next_depth(0).unwrap(), 1);
            assert_eq!(__jmax_next_depth(127).unwrap(), 128);
            for depth in [128, usize::MAX] {
                assert_eq!(__jmax_next_depth(depth).unwrap_err().code, ErrorCode::DepthLimitExceeded);
            }
            let mut value = JsonValue::Null;
            for _ in 0..128 { value = JsonValue::Array(vec![value]); }
            let mut bytes = Vec::new();
            value.encode_json(&mut bytes).unwrap();
            assert_eq!(bytes.len(), 260);
            value = JsonValue::Array(vec![value]);
            let error = value.encode_json(&mut Vec::new()).unwrap_err();
            assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
            assert_eq!(error.source, ErrorSource::Encode);
        }

        #[test]
        fn objects_share_array_depth_and_propagate_sink_context() {
            let object = JsonObject(vec![(String::from("nested"), JsonValue::Array(vec![
                JsonValue::Object(JsonObject::default()),
            ]))]);
            assert_eq!(object.__jmax_encode(&mut Vec::new(), 126).unwrap_err().code, ErrorCode::DepthLimitExceeded);
            object.__jmax_encode(&mut Vec::new(), 125).unwrap();
            let mut expected = Vec::new();
            object.encode_json(&mut expected).unwrap();
            for limit in 0..expected.len() {
                let mut sink = LimitedSink { bytes: Vec::new(), limit };
                let error = object.encode_json(&mut sink).unwrap_err();
                assert_eq!(error, sentinel(sink.bytes.len()));
            }
        }
    }
}
