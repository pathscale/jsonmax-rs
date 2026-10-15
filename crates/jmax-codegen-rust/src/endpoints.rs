//! Request and response payloads, owner views, and model-global wire errors.
//!
//! Generation is deterministic and allocation-only. Schema identities use the
//! same complete qualified candidate ordering as the model and registry emitters.

use alloc::{collections::BTreeMap, format, string::String, vec::Vec};
use crate::{codegen_error, rust_string_literal, validate_options, ArtifactKind,
    CodegenOptions, EndpointField, EndpointModel, EndpointType, EnumVariantSpec,
    ErrorCode, GeneratedArtifact, Result, RustTypeMode, GENERATED_NOTICE};
use jmax_abi::schema::{SchemaId, SchemaIr};
use jmax_endpoint_model::{NameCase, Registry};

const MAX_DEPTH: usize = 128;

fn constraint(detail: &'static str) -> crate::Error {
    codegen_error(ErrorCode::ConstraintViolation, detail)
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(codegen_error(ErrorCode::DepthLimitExceeded, "endpoint type nesting exceeds 128"));
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

// Resolve aliases only at references. An inline nominal owns its declaration;
// it must never be expanded by following fields of recursive records.
fn resolved_source<'a>(ty: &'a EndpointType, mode: RustTypeMode, options: &CodegenOptions,
    registry: &Registry<'a>, depth: usize) -> Result<String> {
    check_depth(depth)?;
    match ty {
        EndpointType::StructRef(_) | EndpointType::StructTable { .. } | EndpointType::EnumRef { .. } => {
            let target = jmax_endpoint_model::registry::resolve_type(registry, ty)?;
            let source = crate::models::rust_type(target, mode, options)?.source;
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
        _ => Ok(crate::models::rust_type(ty, mode, options)?.source),
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

#[derive(Debug)]
struct SchemaEntry {
    ir: SchemaIr,
    fingerprint: u64,
}

// This is the same qualified-name ordering specified for registries.rs. Include
// endpoint candidates, regardless of which artifact owns their adapters.
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

/// Generate endpoint payload source without changing the model or writing files.
///
/// Every public entry point validates options and the complete model. Enabling
/// schema tables requires every schema candidate, including model records, to
/// have an exact supported flat projection; failure returns no partial artifact.
pub fn generate_endpoints(model: &EndpointModel, options: &CodegenOptions) -> Result<GeneratedArtifact> {
    validate_options(options)?;
    let registry = jmax_endpoint_model::registry::build_registry(model)?;
    let catalog = Catalog::collect(model)?;
    // The payloads refer to these sibling nominal declarations. Reject an
    // ambiguous sibling namespace before returning mutually unresolved source.
    validate_names(model, &catalog, options)?;
    let schemas = schemas(model, &catalog, options)?;
    validate_endpoint_names(model, options, &schemas)?;
    let mut out = String::from(GENERATED_NOTICE);
    out.push_str(&format!("use super::alloc;\nuse {} as __jmax_abi;\nuse {} as __jmax_aot;\n", options.abi_path, options.aot_path));
    out.push_str(DEPTH_SUPPORT);
    if catalog.bytea { out.push_str(BASE64_SUPPORT); }
    out.push_str(ERROR_SUPPORT);
    for service in &model.services {
        for endpoint in &service.endpoints {
            let base = endpoint_base(&service.name, &endpoint.name)?;
            let request = format!("{base}Request");
            let response = format!("{base}Response");
            let request_key = format!("request::{}.{}", service.name, endpoint.name);
            let response_key = format!("response::{}.{}", service.name, endpoint.name);
            emit_record(&mut out, &request, &endpoint.params, options, &registry, schemas.get(&request_key))?;
            out.push_str(&format!("impl {request} {{\n    pub const CODE: u32 = {}u32;\n    pub const METHOD: &'static str = {};\n    pub const ROLES: &'static [&'static str] = &[", endpoint.code,
                rust_string_literal(&jmax_endpoint_model::wire_names::method_name(&service.name, &endpoint.name)?)));
            for role in &endpoint.roles {
                out.push_str(&format!("{},", rust_string_literal(role)));
            }
            out.push_str("];\n}\n");
            emit_record(&mut out, &response, &endpoint.response, options, &registry, schemas.get(&response_key))?;
            if let Some(ty) = &endpoint.stream_response {
                let ty = resolved_source(ty, RustTypeMode::Owned, options, &registry, 0)?;
                out.push_str(&format!("pub type {base}Stream = {ty};\n"));
            }
        }
    }
    emit_errors(&mut out, model)?;
    Ok(GeneratedArtifact { path: format!("{}/endpoints.rs", options.module_name),
        contents: out, kind: ArtifactKind::Endpoints })
}

fn endpoint_base(service: &str, endpoint: &str) -> Result<String> {
    // Convert components separately. Converting their concatenation again would
    // merge acronym boundaries, e.g. A + B must remain AB in ABRequest.
    Ok(format!("{}{}", pascal(service)?, pascal(endpoint)?))
}

fn validate_endpoint_names(model: &EndpointModel, options: &CodegenOptions,
    schemas: &BTreeMap<String, SchemaEntry>) -> Result<()> {
    let mut names = BTreeMap::new();
    reserve_name(&mut names, String::from("EndpointError"))?;
    for service in &model.services {
        for endpoint in &service.endpoints {
            let base = endpoint_base(&service.name, &endpoint.name)?;
            for (suffix, qualifier, fields) in [
                ("Request", "request", endpoint.params.as_slice()),
                ("Response", "response", endpoint.response.as_slice()),
            ] {
                let name = format!("{base}{suffix}");
                reserve_name(&mut names, name.clone())?;
                if options.emit_views { reserve_name(&mut names, format!("{name}View"))?; }
                let key = format!("{qualifier}::{}.{}", service.name, endpoint.name);
                let schema_id = schemas.get(&key).map_or(0, |entry| entry.ir.schema_id.0);
                let mut members = BTreeMap::new();
                for (index, field) in fields.iter().enumerate() {
                    let id = u32::try_from(index).map_err(|_| codegen_error(
                        ErrorCode::BudgetExhausted, "endpoint field count exceeds u32").with_schema(schema_id, 0))?;
                    let member = field_name(&field.name).map_err(|error| error.with_schema(schema_id, id))?;
                    if options.emit_views && matches!(member.as_str(), "new" | "value") {
                        return Err(constraint("view field getter collides with constructor or owner accessor").with_schema(schema_id, id));
                    }
                    if members.insert(member, ()).is_some() {
                        return Err(constraint("colliding generated Rust endpoint field names").with_schema(schema_id, id));
                    }
                }
            }
            if endpoint.stream_response.is_some() {
                reserve_name(&mut names, format!("{base}Stream"))?;
            }
        }
    }
    let mut variants = BTreeMap::new();
    reserve_name(&mut variants, String::from("Unknown"))?;
    for error in &model.errors { reserve_name(&mut variants, pascal(&error.name)?)?; }
    Ok(())
}

fn emit_errors(out: &mut String, model: &EndpointModel) -> Result<()> {
    out.push_str("#[derive(Clone, Debug, PartialEq, Eq)]\npub enum EndpointError {\n");
    for error in &model.errors { out.push_str(&format!("    {},\n", pascal(&error.name)?)); }
    out.push_str("    Unknown { code: u32, message: alloc::string::String },\n}\nimpl EndpointError {\n    pub fn code(&self) -> u32 {\n        match self {\n");
    for error in &model.errors {
        out.push_str(&format!("            Self::{} => {}u32,\n", pascal(&error.name)?, error.code));
    }
    out.push_str("            Self::Unknown { code, .. } => *code,\n        }\n    }\n    pub fn message(&self) -> &str {\n        match self {\n");
    for error in &model.errors {
        out.push_str(&format!("            Self::{} => {},\n", pascal(&error.name)?, rust_string_literal(&error.message)));
    }
    out.push_str("            Self::Unknown { message, .. } => message.as_str(),\n        }\n    }\n}\nimpl __jmax_abi::JsonEncode for EndpointError {\n    fn encode_json(&self, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {\n        __jmax_encode_error(self.code(), self.message(), sink)\n    }\n}\n");
    Ok(())
}

fn emit_record<'a>(out: &mut String, original: &str, fields: &'a [EndpointField], options: &CodegenOptions,
    registry: &Registry<'a>, schema: Option<&SchemaEntry>) -> Result<()> {
    let name = original;
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
    emit_encode_impl(out, name);
    if options.emit_views { emit_view(out, name, Some(fields), options, registry)?; }
    if let Some(schema) = schema { emit_schema(out, name, fields, schema)?; }
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
        _ => return Err(constraint("unhandled endpoint encoder type")),
    }
    Ok(())
}

fn emit_encode_impl(out: &mut String, name: &str) {
    out.push_str(&format!("impl __jmax_abi::JsonEncode for {name} {{\n    fn encode_json(&self, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {{\n        self.__jmax_encode(sink, 0)\n    }}\n}}\n"));
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
        _ => Err(codegen_error(ErrorCode::UnsupportedSchemaFeature, "type has no flat endpoint decoder")),
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
            format!("({converted}).ok_or_else(|| __jmax_abi::Error::new(__jmax_abi::ErrorCode::MissingRequiredField, __jmax_abi::ErrorSource::Decode, 0, \"missing required endpoint field\").with_schema({id}u32, {index}u32))?")
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
            0, "generated endpoint encoding depth exceeds 128"));
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

macro_rules! error_support {
    ($emit:ident) => { $emit! {
fn __jmax_encode_error(code: u32, message: &str, sink: &mut dyn __jmax_abi::ByteSink) -> __jmax_abi::Result<()> {
    sink.write_bytes(b"{\"code\":")?;
    __jmax_aot::encode::write_u64(u64::from(code), sink)?;
    sink.write_bytes(b",\"message\":")?;
    __jmax_aot::encode::write_json_string(message, sink)?;
    sink.write_byte(b'}')
}
    } };
}

const ERROR_SUPPORT: &str = error_support!(source_tokens);

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec};
    use crate::{EndpointSchemaSpec, ErrorSource, ErrorSpec, HeaderSpec, RoleSpec,
        ServiceSpec, TypeDefinition};

    fn options() -> CodegenOptions {
        CodegenOptions { emit_schema_tables: false, ..CodegenOptions::default() }
    }

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::new() }
    }

    fn record(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn variant(name: &str, value: i32) -> EnumVariantSpec {
        EnumVariantSpec { name: String::from(name), value, description: String::new() }
    }

    fn enumeration(name: &str) -> EndpointType {
        EndpointType::Enum { name: String::from(name), variants: vec![variant("Ready", 7)] }
    }

    fn endpoint(service: &str, name: &str, code: u32) -> EndpointSchemaSpec {
        EndpointSchemaSpec { service: String::from(service), name: String::from(name), code,
            roles: vec![], params: vec![], response: vec![], stream_response: None,
            description: String::new() }
    }

    fn model(params: Vec<EndpointField>, response: Vec<EndpointField>) -> EndpointModel {
        let mut spec = endpoint("api", "get", 7);
        spec.params = params;
        spec.response = response;
        EndpointModel { services: vec![ServiceSpec { id: 1, name: String::from("api"),
            endpoints: vec![spec] }], ..EndpointModel::default() }
    }

    fn error(name: &str, code: u32, message: &str) -> ErrorSpec {
        ErrorSpec { name: String::from(name), code, message: String::from(message) }
    }

    fn generated(model: &EndpointModel) -> String {
        generate_endpoints(model, &options()).unwrap().contents
    }

    fn constraint_error(model: &EndpointModel) -> crate::Error {
        let error = generate_endpoints(model, &options()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!(error.byte_offset, 0);
        error
    }

    #[test]
    fn empty_catalog_is_a_complete_deterministic_artifact() {
        let model = EndpointModel::default();
        let artifact = generate_endpoints(&model, &CodegenOptions::default()).unwrap();
        assert_eq!(artifact.path, "generated/endpoints.rs");
        assert_eq!(artifact.kind, ArtifactKind::Endpoints);
        assert!(artifact.contents.starts_with(GENERATED_NOTICE));
        assert!(artifact.contents.ends_with('\n'));
        assert!(!artifact.contents.ends_with("\n\n"));
        assert!(!artifact.contents.contains('\r'));
        assert!(artifact.contents.contains("pub enum EndpointError {\n    Unknown { code: u32, message: alloc::string::String },"));
        assert!(artifact.contents.contains("impl __jmax_abi::JsonEncode for EndpointError"));
        assert!(!artifact.contents.contains("impl __jmax_abi::JsonDecode"));
        assert_eq!(artifact, generate_endpoints(&model, &CodegenOptions::default()).unwrap());
    }

    #[test]
    fn empty_payloads_are_objects_with_owner_borrowing_views() {
        let source = generated(&model(vec![], vec![]));
        for name in ["ApiGetRequest", "ApiGetResponse"] {
            assert!(source.contains(&format!("pub struct {name} {{\n}}")));
            assert!(source.contains(&format!("impl __jmax_abi::JsonEncode for {name}")));
            assert!(source.contains(&format!("pub struct {name}View<'a> {{\n    value: &'a {name},")));
            assert!(source.contains(&format!("pub fn new(value: &'a {name}) -> Self")));
            assert!(source.contains(&format!("pub fn value(&self) -> &'a {name}")));
            assert!(source.contains(&format!("<{name} as __jmax_abi::JsonEncode>::encode_json(self.value, sink)")));
        }
        assert_eq!(source.matches("sink.write_byte(b'{')?").count(), 2);
        assert!(source.contains("sink.write_byte(b'}')\n"));
        assert!(!source.contains("let mut __jmax_first"));
    }

    #[test]
    fn component_case_boundaries_survive_concatenation() {
        let model = EndpointModel { services: vec![ServiceSpec { id: 0,
            name: String::from("a"), endpoints: vec![endpoint("a", "b", 0)] }],
            ..EndpointModel::default() };
        let source = generated(&model);
        assert!(source.contains("pub struct ABRequest {"));
        assert!(source.contains("pub struct ABResponse {"));
        assert!(!source.contains("pub struct AbRequest"));
        assert_eq!(endpoint_base("HTTP_api", "getURL").unwrap(), "HttpApiGetUrl");
    }

    #[test]
    fn request_metadata_preserves_codes_methods_and_role_order() {
        let mut model = model(vec![], vec![]);
        model.roles = vec![RoleSpec { name: String::from("writer"), includes: vec![],
            description: String::new() }, RoleSpec { name: String::from("reader"),
            includes: vec![], description: String::new() }];
        model.services[0].endpoints[0].roles = vec![String::from("reader"), String::from("writer")];
        for code in [0, 1, u32::MAX] {
            model.services[0].endpoints[0].code = code;
            let source = generated(&model);
            assert!(source.contains(&format!("pub const CODE: u32 = {code}u32;")));
            assert!(source.contains("pub const METHOD: &'static str = \"api.get\";"));
            assert!(source.contains("pub const ROLES: &'static [&'static str] = &[\"reader\",\"writer\",];"));
            assert_eq!(source.matches("pub const CODE:").count(), 1);
            assert!(!source.contains("trait Endpoint"));
        }
        model.services[0].endpoints[0].roles.clear();
        assert!(generated(&model).contains("pub const ROLES: &'static [&'static str] = &[];"));
    }

    #[test]
    fn payload_fields_preserve_wire_names_order_and_scalar_mappings() {
        let fields = vec![field("userID", EndpointType::UInt32), field("type", EndpointType::Int32),
            field("long", EndpointType::Int64), field("time", EndpointType::TimeStampMs),
            field("ratio", EndpointType::Float64), field("active", EndpointType::Boolean),
            field("label", EndpointType::String), field("uuid", EndpointType::UUID),
            field("nano", EndpointType::NanoId { len: 21 }), field("ip", EndpointType::IpAddr),
            field("address", EndpointType::BlockchainAddress), field("hash", EndpointType::BlockchainTransactionHash),
            field("bytes", EndpointType::Bytea), field("price", EndpointType::BlockchainDecimal),
            field("unit", EndpointType::Unit)];
        let source = generated(&model(fields, vec![]));
        for text in ["pub user_id: u32,", "pub r#type: i32,", "pub long: i64,", "pub time: i64,",
            "pub ratio: f64,", "pub active: bool,", "pub label: super::alloc::string::String,",
            "pub bytes: super::alloc::vec::Vec<u8>,", "pub price: ::jmax_abi::Decimal128,", "pub unit: (),",
            "write_json_string(\"userID\", sink)?", "write_u64(u64::from(*(&self.user_id)), sink)?",
            "write_i64(i64::from(*(&self.r#type)), sink)?", "write_f64(*(&self.ratio), sink)?",
            "write_bool(*(&self.active), sink)?", "write_decimal(*(&self.price), sink)?",
            "__jmax_encode_base64((&self.bytes).as_slice(), sink)?", "sink.write_bytes(b\"null\")?"] {
            assert!(source.contains(text), "missing {text}");
        }
        assert!(source.find("pub user_id:").unwrap() < source.find("pub r#type:").unwrap());
        assert!(source.find("pub bytes:").unwrap() < source.find("pub price:").unwrap());
        assert!(source.contains("#[derive(Clone, Debug, PartialEq)]\npub struct ApiGetRequest"));
    }

    #[test]
    fn optional_fields_are_omitted_and_optional_array_values_are_null() {
        let items = EndpointType::Vec(Box::new(EndpointType::Optional(Box::new(EndpointType::UInt32))));
        let source = generated(&model(vec![
            field("maybe", EndpointType::Optional(Box::new(EndpointType::String))),
            field("items", items),
        ], vec![]));
        assert!(source.contains("if let ::core::option::Option::Some(__jmax_value) = &self.maybe"));
        assert!(source.contains("::core::option::Option::None => sink.write_bytes(b\"null\")?"));
        assert!(source.contains("pub fn maybe(&self) -> ::core::option::Option<&'a str>"));
        assert!(source.contains("pub fn items(&self) -> &'a [::core::option::Option<u32>]"));
        assert!(source.contains("if __jmax_index != 0 { sink.write_byte(b',')?; }"));
    }

    #[test]
    fn nominal_and_object_payloads_use_sibling_owners_and_depth_entry_points() {
        let source = generated(&model(vec![field("node", record("Node", vec![
            field("next", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node"))))),
        ])), field("state", enumeration("State")), field("object", EndpointType::Object)], vec![]));
        assert!(source.contains("pub node: super::models::Node,"));
        assert!(source.contains("pub state: super::models::State,"));
        assert!(source.contains("pub object: super::models::JsonObject,"));
        assert!(source.contains("(&self.node).__jmax_encode(sink, depth)?;"));
        assert!(source.contains("(&self.object).__jmax_encode(sink, depth)?;"));
        assert!(source.contains("pub fn node(&self) -> super::models::NodeView<'a>"));
        assert!(source.contains("super::models::NodeView::new(&self.value.node)"));
        assert!(source.contains("pub fn object(&self) -> &'a super::models::JsonObject"));
        assert!(!source.contains("pub struct Node {"));
        assert!(!source.contains("pub struct JsonObject"));
    }

    #[test]
    fn reference_aliases_resolve_once_without_extra_boxes() {
        let mut model = model(vec![field("root", EndpointType::StructRef(String::from("Alias"))),
            field("table", EndpointType::StructTable { struct_ref: String::from("Alias") }),
            field("state", EndpointType::EnumRef { name: String::from("StatusAlias"), prefixed_name: true })], vec![]);
        model.definitions = vec![TypeDefinition { name: String::from("Node"), ty: record("Node", vec![]) },
            TypeDefinition { name: String::from("Alias"), ty: EndpointType::StructRef(String::from("Node")) },
            TypeDefinition { name: String::from("State"), ty: enumeration("State") },
            TypeDefinition { name: String::from("StatusAlias"), ty: EndpointType::EnumRef { name: String::from("State"), prefixed_name: false } }];
        let source = generated(&model);
        assert!(source.contains("pub root: super::alloc::boxed::Box<super::models::Node>,"));
        assert!(source.contains("pub table: super::alloc::boxed::Box<super::models::Node>,"));
        assert!(source.contains("pub state: super::models::State,"));
        assert!(source.contains("pub fn root(&self) -> super::models::NodeView<'a>"));
        assert!(!source.contains("Box<super::alloc::boxed::Box"));
    }

    #[test]
    fn streams_are_conditional_owned_aliases_without_transport_claims() {
        let mut model = model(vec![], vec![]);
        assert!(!generated(&model).contains("pub type ApiGetStream"));
        model.services[0].endpoints[0].stream_response = Some(EndpointType::Vec(Box::new(record("Update", vec![]))));
        let source = generated(&model);
        assert!(source.contains("pub type ApiGetStream = super::alloc::vec::Vec<super::models::Update>;"));
        assert!(!source.contains("ApiGetStreamView"));
        assert!(!source.contains("impl __jmax_abi::JsonEncode for ApiGetStream"));
        // Streams are not flat object schema candidates.
        assert!(generate_endpoints(&model, &CodegenOptions::default()).is_ok());
    }

    #[test]
    fn disabling_views_allows_fields_named_new_and_value() {
        let model = model(vec![field("new", EndpointType::UInt32), field("value", EndpointType::String)], vec![]);
        constraint_error(&model);
        let mut options = options();
        options.emit_views = false;
        let source = generate_endpoints(&model, &options).unwrap().contents;
        assert!(source.contains("pub new: u32,"));
        assert!(source.contains("pub value: super::alloc::string::String,"));
        assert!(!source.contains("RequestView"));
        assert!(!source.contains("ResponseView"));
    }

    #[test]
    fn transformed_field_collisions_and_invalid_identifiers_have_field_context() {
        for fields in [vec![field("userID", EndpointType::UInt32), field("user_id", EndpointType::Int32)],
            vec![field("ok", EndpointType::Boolean), field("Self", EndpointType::Boolean)],
            vec![field("ok", EndpointType::Boolean), field("123", EndpointType::Boolean)],
            vec![field("ok", EndpointType::Boolean), field("bad;name", EndpointType::Boolean)],
            vec![field("ok", EndpointType::Boolean), field("é", EndpointType::Boolean)]] {
            let error = constraint_error(&model(fields, vec![]));
            assert_eq!(error.field_id, 1);
        }
        for name in ["self", "Self", "super", "crate", "_", "", "__", "0name"] {
            assert!(field_name(name).is_err(), "{name}");
        }
        for keyword in ["type", "match", "async", "await", "gen"] {
            assert_eq!(field_name(keyword).unwrap(), format!("r#{keyword}"));
        }
    }

    #[test]
    fn transformed_endpoint_and_concatenation_collisions_are_errors() {
        for services in [
            vec![ServiceSpec { id: 1, name: String::from("api"), endpoints: vec![
                endpoint("api", "getURL", 1), endpoint("api", "get_url", 2)] }],
            vec![ServiceSpec { id: 1, name: String::from("api"), endpoints: vec![endpoint("api", "get", 1)] },
                ServiceSpec { id: 2, name: String::from("API"), endpoints: vec![endpoint("API", "get", 2)] }],
            vec![ServiceSpec { id: 1, name: String::from("a_b"), endpoints: vec![endpoint("a_b", "c", 1)] },
                ServiceSpec { id: 2, name: String::from("a"), endpoints: vec![endpoint("a", "b_c", 2)] }],
        ] {
            constraint_error(&EndpointModel { services, ..EndpointModel::default() });
        }
        // AbC and ABC are distinct Rust identifiers and remain distinguishable.
        let model = EndpointModel { services: vec![
            ServiceSpec { id: 1, name: String::from("ab"), endpoints: vec![endpoint("ab", "c", 1)] },
            ServiceSpec { id: 2, name: String::from("a"), endpoints: vec![endpoint("a", "b_c", 2)] },
        ], ..EndpointModel::default() };
        let source = generated(&model);
        assert!(source.contains("pub struct AbCRequest"));
        assert!(source.contains("pub struct ABCRequest"));
    }

    #[test]
    fn model_namespace_and_inline_enum_collisions_are_rejected_directly() {
        let mut model = model(vec![field("one", record("HTTP", vec![])),
            field("two", record("Http", vec![]))], vec![]);
        constraint_error(&model);
        model.services[0].endpoints[0].params = vec![field("state", EndpointType::Enum {
            name: String::from("State"), variants: vec![variant("HTTP", 1), variant("Http", 2)] })];
        constraint_error(&model);
        model.services[0].endpoints[0].params = vec![field("state", EndpointType::Enum {
            name: String::from("State"), variants: vec![] })];
        constraint_error(&model);
        model.services[0].endpoints[0].params = vec![field("data", EndpointType::Object),
            field("named", record("JsonObject", vec![]))];
        constraint_error(&model);
    }

    #[test]
    fn errors_preserve_exact_codes_messages_and_unknown_storage() {
        let model = EndpointModel { errors: vec![error("not_found", 0, "Missing"),
            error("HTTP_failure", u32::MAX, "é\n\"\\\u{0}")], ..EndpointModel::default() };
        let source = generated(&model);
        assert!(source.contains("pub enum EndpointError {\n    NotFound,\n    HttpFailure,"));
        assert!(source.contains("Self::NotFound => 0u32,"));
        assert!(source.contains("Self::HttpFailure => 4294967295u32,"));
        assert!(source.contains("Self::NotFound => \"Missing\","));
        assert!(source.contains(&format!("Self::HttpFailure => {},", rust_string_literal("é\n\"\\\u{0}"))));
        assert!(source.contains("Self::Unknown { code, .. } => *code,"));
        assert!(source.contains("Self::Unknown { message, .. } => message.as_str(),"));
        assert!(source.contains("__jmax_encode_error(self.code(), self.message(), sink)"));
        assert!(!source.contains('\0'));
        assert!(!source.contains("allowed_errors"));
    }

    #[test]
    fn unknown_and_transformed_error_variant_collisions_are_reserved() {
        for errors in [vec![error("Unknown", 1, "")], vec![error("UNKNOWN", 1, "")],
            vec![error("notFound", 1, "a"), error("not_found", 2, "b")],
            vec![error("Self", 1, "")]] {
            constraint_error(&EndpointModel { errors, ..EndpointModel::default() });
        }
    }

    #[test]
    fn duplicate_codes_and_invalid_model_metadata_are_not_hidden() {
        let mut model = model(vec![], vec![]);
        model.services[0].endpoints.push(endpoint("api", "other", 7));
        constraint_error(&model);
        model.services[0].endpoints.pop();
        model.services[0].endpoints[0].service = String::from("different");
        constraint_error(&model);
        model.services[0].endpoints[0].service = String::from("api");
        model.services[0].endpoints[0].roles.push(String::from("missing"));
        constraint_error(&model);
        constraint_error(&EndpointModel { errors: vec![error("A", 1, ""), error("B", 1, "")],
            ..EndpointModel::default() });
        let mut model = model_with_missing_reference();
        constraint_error(&model);
        model.services[0].endpoints[0].params.clear();
        model.services[0].endpoints[0].stream_response = Some(EndpointType::StructRef(String::from("Missing")));
        constraint_error(&model);
    }

    fn model_with_missing_reference() -> EndpointModel {
        model(vec![field("missing", EndpointType::StructRef(String::from("Missing")))], vec![])
    }

    #[test]
    fn configured_paths_are_validated_and_used_verbatim() {
        let mut options = options();
        options.module_name = String::from("client");
        options.abi_path = String::from("crate::abi");
        options.aot_path = String::from("crate::aot");
        let artifact = generate_endpoints(&model(vec![field("decimal", EndpointType::BlockchainDecimal)], vec![]), &options).unwrap();
        assert_eq!(artifact.path, "client/endpoints.rs");
        assert!(artifact.contents.contains("use crate::abi as __jmax_abi;"));
        assert!(artifact.contents.contains("use crate::aot as __jmax_aot;"));
        assert!(artifact.contents.contains("pub decimal: crate::abi::Decimal128,"));
        for bad in ["", "../escape", "CON", "bad-name"] {
            options.module_name = String::from(bad);
            assert_eq!(generate_endpoints(&EndpointModel::default(), &options).unwrap_err().code,
                ErrorCode::ConstraintViolation);
        }
        options.module_name = String::from("client");
        for bad in ["", "::abi; injected()", "::crate::abi", "abi::<T>"] {
            options.abi_path = String::from(bad);
            assert_eq!(generate_endpoints(&EndpointModel::default(), &options).unwrap_err().code,
                ErrorCode::ConstraintViolation);
        }
        options.abi_path = String::from("crate::abi");
        options.derive_path = String::from("derive /* injection */");
        assert!(generate_endpoints(&EndpointModel::default(), &options).is_err());
    }

    #[test]
    fn schema_ids_include_model_header_and_stream_nominals_in_sorted_order() {
        let mut model = model(vec![field("z", EndpointType::UInt32), field("a", EndpointType::Optional(Box::new(EndpointType::String)))],
            vec![field("ok", EndpointType::Boolean)]);
        model.definitions = vec![TypeDefinition { name: String::from("Zulu"), ty: record("Zulu", vec![]) },
            TypeDefinition { name: String::from("Alpha"), ty: record("Alpha", vec![]) }];
        model.headers = vec![HeaderSpec { name: String::from("X-Record"), ty: record("Header", vec![]),
            required: false, description: String::new() }];
        model.services[0].endpoints[0].stream_response = Some(record("StreamItem", vec![]));
        let catalog = Catalog::collect(&model).unwrap();
        let schemas = schemas(&model, &catalog, &CodegenOptions::default()).unwrap();
        let expected = ["model::Alpha", "model::Header", "model::StreamItem", "model::Zulu",
            "request::api.get", "response::api.get"];
        assert_eq!(schemas.len(), expected.len());
        for (index, key) in expected.iter().enumerate() {
            let entry = &schemas[*key];
            assert_eq!(entry.ir.schema_id, SchemaId((index + 1) as u32));
            assert_eq!(entry.ir.name, *key);
            assert_eq!(entry.fingerprint, jmax_schema_aot::fingerprint_schema(&entry.ir).unwrap());
            assert!(entry.ir.reject_unknown);
        }
        let request = &schemas["request::api.get"].ir;
        assert_eq!(request.required_count, 1);
        assert_eq!(request.fields[0].name, "z");
        assert_eq!(request.fields[0].field_id.0, 0);
        assert_eq!(request.fields[1].name, "a");
        assert_eq!(request.fields[1].field_id.0, 1);
        assert!(!request.fields[1].required);
        assert_eq!(request.fields[1].scalar_kind, Some(jmax_abi::syntax::ScalarKind::String));
        let source = generate_endpoints(&model, &CodegenOptions::default()).unwrap().contents;
        assert!(source.contains("schema_id: __jmax_abi::SchemaId(5u32)"));
        assert!(source.contains("schema_id: __jmax_abi::SchemaId(6u32)"));
        assert!(!source.contains("impl __jmax_abi::SchemaProvider for StreamItem"));
    }

    #[test]
    fn flat_schema_emits_checked_plan_narrowing_and_owned_decoders() {
        let model = model(vec![field("u", EndpointType::UInt32), field("i", EndpointType::Int32),
            field("long", EndpointType::Int64), field("time", EndpointType::TimeStampMs),
            field("f", EndpointType::Float64), field("b", EndpointType::Boolean),
            field("s", EndpointType::String), field("raw", EndpointType::Bytea),
            field("maybe", EndpointType::Optional(Box::new(EndpointType::Bytea)))], vec![]);
        let source = generate_endpoints(&model, &CodegenOptions::default()).unwrap().contents;
        for decoder in ["decode_u32", "decode_i32", "decode_i64", "decode_f64", "decode_bool", "decode_str"] {
            assert!(source.contains(&format!("__jmax_aot::decode::{decoder}(&reader, &specs[")));
        }
        assert!(source.contains("impl __jmax_abi::SchemaProvider for ApiGetRequest"));
        assert!(source.contains("impl __jmax_abi::JsonDecode for ApiGetRequest"));
        assert!(source.contains("__jmax_aot::decode::validate_slots(slots, &plan)"));
        assert!(source.contains("__jmax_aot::SlotReader::new(slots,"));
        assert!(source.contains("index: 8u32, field_id: __jmax_abi::FieldId(8u32), kind: __jmax_aot::SlotKind::Str, required: false"));
        assert!(source.contains("map(alloc::string::String::from)"));
        assert!(source.contains("map(__jmax_decode_base64).transpose()"));
        assert!(source.contains("error.with_schema(1u32, 7u32)"));
        assert!(source.contains("error.with_schema(1u32, 8u32)"));
        assert!(source.contains("MissingRequiredField, __jmax_abi::ErrorSource::Decode"));
        assert!(source.contains("min_i64: ::core::option::Option::Some(-2147483648i64)"));
        assert!(source.contains("max_i64: ::core::option::Option::Some(4294967295i64)"));
    }

    #[test]
    fn empty_records_get_exact_zero_slot_decode_plans() {
        let source = generate_endpoints(&model(vec![], vec![]), &CodegenOptions::default()).unwrap().contents;
        assert_eq!(source.matches("let specs: &[__jmax_aot::SlotSpec] = &[\n        ];").count(), 2);
        assert_eq!(source.matches("required_count: 0u16").count(), 2);
        assert_eq!(source.matches("decode::validate_slots(slots, &plan)").count(), 2);
        assert_eq!(source.matches("::core::result::Result::Ok(Self {\n        })").count(), 2);
    }

    #[test]
    fn schemas_disabled_emit_no_claim_of_decoding() {
        let model = model(vec![field("objects", EndpointType::Vec(Box::new(EndpointType::Object)))], vec![]);
        let source = generated(&model);
        assert!(!source.contains("impl __jmax_abi::SchemaProvider"));
        assert!(!source.contains("impl __jmax_abi::JsonDecode"));
        assert!(!source.contains("__jmax_aot::DecodePlan"));
        let catalog = Catalog::collect(&model).unwrap();
        assert!(schemas(&model, &catalog, &options()).unwrap().is_empty());
    }

    #[test]
    fn unsupported_payload_fields_fail_the_entire_schema_generation() {
        for ty in [EndpointType::BlockchainDecimal, EndpointType::Object, EndpointType::Unit,
            EndpointType::Vec(Box::new(EndpointType::UInt32)), record("Nested", vec![]),
            enumeration("State"), EndpointType::NanoId { len: 20 },
            EndpointType::Optional(Box::new(EndpointType::Optional(Box::new(EndpointType::String))))] {
            for response in [false, true] {
                let model = if response { model(vec![], vec![field("bad", ty.clone())]) }
                    else { model(vec![field("bad", ty.clone())], vec![]) };
                let error = generate_endpoints(&model, &CodegenOptions::default()).unwrap_err();
                assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature, "{ty:?}");
                assert_eq!(error.source, ErrorSource::SchemaCompile);
                assert!(generate_endpoints(&model, &options()).is_ok());
            }
        }
    }

    #[test]
    fn unsupported_sibling_candidates_cannot_silently_shift_schema_identity() {
        let mut model = model(vec![], vec![]);
        model.definitions.push(TypeDefinition { name: String::from("Bad"),
            ty: record("Bad", vec![field("compound", EndpointType::Vec(Box::new(EndpointType::String)))]) });
        let error = generate_endpoints(&model, &CodegenOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
        assert!(generate_endpoints(&model, &options()).is_ok());
    }

    #[test]
    fn supported_string_formats_preserve_canonical_ir_and_dense_slots() {
        let model = model(vec![field("uuid", EndpointType::UUID), field("ip", EndpointType::IpAddr),
            field("bytes", EndpointType::Bytea), field("nano", EndpointType::NanoId { len: 21 }),
            field("address", EndpointType::BlockchainAddress), field("hash", EndpointType::BlockchainTransactionHash)], vec![]);
        let catalog = Catalog::collect(&model).unwrap();
        let schemas = schemas(&model, &catalog, &CodegenOptions::default()).unwrap();
        let fields = &schemas["request::api.get"].ir.fields;
        for (index, format) in ["uuid", "ip", "base64", "nanoid", "blockchain-address", "blockchain-transaction-hash"].iter().enumerate() {
            assert_eq!(fields[index].format.as_deref(), Some(*format));
            assert_eq!(fields[index].field_id.0, index as u32);
            assert!(fields[index].required);
        }
    }

    #[test]
    fn known_schema_and_field_ids_are_attached_to_name_errors() {
        let model = model(vec![field("ok", EndpointType::Boolean), field("value", EndpointType::Boolean)], vec![]);
        let error = generate_endpoints(&model, &CodegenOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::ConstraintViolation);
        assert_eq!((error.schema_id, error.field_id), (1, 1));
    }

    #[test]
    fn decoder_dispatch_and_integer_literal_boundaries_are_explicit() {
        for (ty, expected) in [(EndpointType::UInt32, ("U64", "decode_u32")),
            (EndpointType::Int32, ("I64", "decode_i32")), (EndpointType::Int64, ("I64", "decode_i64")),
            (EndpointType::TimeStampMs, ("I64", "decode_i64")), (EndpointType::Float64, ("F64", "decode_f64")),
            (EndpointType::Boolean, ("Bool", "decode_bool")), (EndpointType::String, ("Str", "decode_str")),
            (EndpointType::Bytea, ("Str", "decode_str"))] {
            assert_eq!(slot_decoder(&ty).unwrap(), expected);
        }
        for ty in [EndpointType::Unit, EndpointType::BlockchainDecimal, EndpointType::Object,
            enumeration("State"), EndpointType::Vec(Box::new(EndpointType::UInt32))] {
            let error = slot_decoder(&ty).unwrap_err();
            assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
        }
        assert_eq!(option_i64(Some(i64::MIN)), "::core::option::Option::Some(-9223372036854775808i64)");
        assert_eq!(option_i64(Some(i64::MAX)), "::core::option::Option::Some(9223372036854775807i64)");
        assert_eq!(option_u32(Some(u32::MAX)), "::core::option::Option::Some(4294967295u32)");
        assert_eq!(option_i64(None), "::core::option::Option::None");
        assert_eq!(option_u32(None), "::core::option::Option::None");
    }

    #[test]
    fn field_and_error_literals_cannot_inject_source_lines() {
        let text = "quote\" slash\\ newline\n carriage\r tab\t nul\0 é";
        let mut source = String::new();
        emit_key(&mut source, &rust_string_literal(text));
        assert!(source.contains(&rust_string_literal(text)));
        assert!(!source.contains('\0'));
        let model = EndpointModel { errors: vec![error("Failure", 3, text)], ..EndpointModel::default() };
        let source = generated(&model);
        assert!(source.contains(&rust_string_literal(text)));
        assert!(!source.contains('\r'));
        assert!(!source.contains('\t'));
    }

    #[test]
    fn bounded_type_traversal_returns_a_structured_error() {
        assert!(check_depth(0).is_ok());
        assert!(check_depth(127).is_ok());
        for depth in [128, 129, usize::MAX] {
            let error = check_depth(depth).unwrap_err();
            assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
        }
        let mut ty = EndpointType::UInt32;
        for _ in 0..129 { ty = EndpointType::Vec(Box::new(ty)); }
        let error = generate_endpoints(&model(vec![field("deep", ty)], vec![]), &options()).unwrap_err();
        assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn repeated_generation_preserves_service_endpoint_field_and_error_order() {
        let mut model = model(vec![field("z", EndpointType::String), field("a", EndpointType::UInt32)], vec![]);
        model.services[0].endpoints[0].code = 99;
        model.services[0].endpoints.push(endpoint("api", "last", 1));
        model.errors = vec![error("Zulu", 99, "z"), error("Alpha", 1, "a")];
        let before = model.clone();
        let artifact = generate_endpoints(&model, &CodegenOptions::default()).unwrap();
        assert_eq!(model, before);
        assert_eq!(artifact, generate_endpoints(&model, &CodegenOptions::default()).unwrap());
        let source = artifact.contents;
        assert!(source.find("pub struct ApiGetRequest").unwrap() < source.find("pub struct ApiLastRequest").unwrap());
        assert!(source.find("pub z:").unwrap() < source.find("pub a:").unwrap());
        assert!(source.contains("pub enum EndpointError {\n    Zulu,\n    Alpha,"));
    }

    // Instantiate the exact helper tokens emitted above. These tests exercise
    // codec arithmetic and sink propagation without evaluating generated source
    // or manufacturing a validation proof.
    mod runtime {
        use alloc::{format, vec::Vec};
        use jmax_abi as __jmax_abi;
        use jmax_schema_aot as __jmax_aot;
        use __jmax_abi::{ByteSink, Error, ErrorCode, ErrorSource, Result};

        macro_rules! items {
            ($($source:tt)*) => { $($source)* };
        }
        depth_support!(items);
        base64_support!(items);
        error_support!(items);

        fn sentinel(offset: usize) -> Error {
            Error::new(ErrorCode::Cancelled, ErrorSource::Encode,
                u32::try_from(offset).unwrap_or(u32::MAX), "deterministic endpoint sink failure").with_schema(41, 9)
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
        fn padded_base64_rfc4648_and_binary_vectors_round_trip() {
            let vectors: &[(&[u8], &str)] = &[(b"", ""), (b"f", "Zg=="), (b"fo", "Zm8="),
                (b"foo", "Zm9v"), (b"foob", "Zm9vYg=="), (b"fooba", "Zm9vYmE="),
                (b"foobar", "Zm9vYmFy"), (&[0], "AA=="), (&[0, 0], "AAA="),
                (&[0xff, 0xff, 0xff], "////"), (&[0xfb, 0xff], "+/8=")];
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
        fn noncanonical_base64_is_rejected_with_decode_errors() {
            for value in ["A", "AA", "AAA", "Zg", "Zg=", "Zg===", "Zh==", "Zm9=", "Zg==AAAA",
                "=AAA", "A===", "AA=A", "_AAA", "-AAA", " AA==", "/w==\n", "éAA", "AAAA===="] {
                let error = __jmax_decode_base64(value).unwrap_err();
                assert_eq!(error.code, ErrorCode::FormatMismatch, "{value:?}");
                assert_eq!(error.source, ErrorSource::Decode);
            }
        }

        #[test]
        fn all_padding_bit_violations_are_rejected() {
            let alphabet = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
            for (index, byte) in alphabet.iter().copied().enumerate() {
                let two = [b'A', byte, b'=', b'='];
                let wire = core::str::from_utf8(&two).unwrap();
                assert_eq!(__jmax_decode_base64(wire).is_ok(), index & 15 == 0);
                let three = [b'A', b'A', byte, b'='];
                let wire = core::str::from_utf8(&three).unwrap();
                assert_eq!(__jmax_decode_base64(wire).is_ok(), index & 3 == 0);
            }
        }

        #[test]
        fn base64_sink_failures_preserve_every_error_field() {
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
        fn error_wire_objects_have_exact_keys_order_codes_and_escaping() {
            for (code, message, expected) in [
                (0, "", "{\"code\":0,\"message\":\"\"}"),
                (1, "Missing", "{\"code\":1,\"message\":\"Missing\"}"),
                (u32::MAX, "é\n\"\\\0", "{\"code\":4294967295,\"message\":\"é\\n\\\"\\\\\\u0000\"}"),
            ] {
                let mut encoded = Vec::new();
                __jmax_encode_error(code, message, &mut encoded).unwrap();
                assert_eq!(encoded, expected.as_bytes());
            }
        }

        #[test]
        fn error_encoder_propagates_sink_failure_at_every_output_boundary() {
            let mut expected = Vec::new();
            __jmax_encode_error(u32::MAX, "é\n\"\\", &mut expected).unwrap();
            for limit in 0..expected.len() {
                let mut sink = LimitedSink { bytes: Vec::new(), limit };
                let error = __jmax_encode_error(u32::MAX, "é\n\"\\", &mut sink).unwrap_err();
                assert_eq!(error, sentinel(sink.bytes.len()));
                assert!(expected.starts_with(&sink.bytes));
            }
            let mut sink = LimitedSink { bytes: Vec::new(), limit: expected.len() };
            __jmax_encode_error(u32::MAX, "é\n\"\\", &mut sink).unwrap();
            assert_eq!(sink.bytes, expected);
        }

        #[test]
        fn runtime_depth_boundary_is_checked_before_incrementing() {
            assert_eq!(__jmax_next_depth(0).unwrap(), 1);
            assert_eq!(__jmax_next_depth(127).unwrap(), 128);
            for depth in [128, 129, usize::MAX] {
                let error = __jmax_next_depth(depth).unwrap_err();
                assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
                assert_eq!(error.source, ErrorSource::Encode);
                assert_eq!(error.byte_offset, 0);
            }
        }
    }
}
