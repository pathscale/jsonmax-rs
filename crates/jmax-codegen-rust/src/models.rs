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

pub(crate) fn enum_variant_ident(name: &str) -> Result<String> {
    pascal(name)
}

/// The generated type name of a model name.
pub(crate) fn pascal_name(name: &str) -> Result<String> {
    pascal(name)
}

/// The Rust identifier of one wire field of the generated struct `type_name`:
/// a configured override, else the derived snake_case name.
pub(crate) fn member_ident(options: &CodegenOptions, type_name: &str, wire: &str) -> Result<String> {
    match options.ident_overrides.iter().find(|item| item.type_name == type_name && item.wire == wire) {
        Some(item) => Ok(item.ident.clone()),
        None => field_name(wire),
    }
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
pub(crate) fn resolved_source<'a>(ty: &'a EndpointType, mode: RustTypeMode, options: &CodegenOptions,
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
                    let member = member_ident(options, &name, &field.name).map_err(|error| error.with_schema(0, id))?;
                    if options.emit_views && matches!(member.as_str(), "new" | "value") {
                        return Err(constraint("view field getter collides with constructor or owner accessor").with_schema(0, id));
                    }
                    if fields_seen.insert(member, ()).is_some() {
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
    for item in &options.ident_overrides {
        let mut found = false;
        for (original, ty) in &catalog.nominals {
            if let EndpointType::Struct { fields, .. } = ty {
                if pascal(original)? == item.type_name {
                    found = fields.iter().any(|field| field.name == item.wire);
                    break;
                }
            }
        }
        if !found {
            return Err(constraint("identifier override names no struct field"));
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
        out.push_str(&format!("    pub {}: {ty},\n", member_ident(options, &name, &field.name)?));
    }
    out.push_str("}\n");
    out.push_str(&format!("impl {name} {{\n    pub(super) fn __jmax_encode(&self, sink: &mut dyn __jmax_abi::ByteSink, depth: usize) -> __jmax_abi::Result<()> {{\n        let depth = __jmax_next_depth(depth)?;\n        sink.write_byte(b'{{')?;\n"));
    if !fields.is_empty() { out.push_str("        let mut __jmax_first = true;\n"); }
    for field in fields {
        let member = member_ident(options, &name, &field.name)?;
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
    if let Some(schema) = schema { emit_schema(out, &name, fields, schema, options)?; }
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
        out.push_str(&format!("    {} = {},\n", enum_variant_ident(&variant.name)?, variant.value));
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
            let member = member_ident(options, name, &field.name)?;
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

fn emit_schema(out: &mut String, name: &str, fields: &[EndpointField], schema: &SchemaEntry, options: &CodegenOptions) -> Result<()> {
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
        out.push_str(&format!("            {}: {value},\n", member_ident(options, name, &field.name)?));
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
