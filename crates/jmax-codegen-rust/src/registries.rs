//! Deterministic static type catalogs and complete executable schema images.
//!
//! Only ownership edges are traversed when collecting nominal declarations.
//! Reference validation belongs to the endpoint-model registry, so recursive
//! records cannot cause an unbounded walk here. Schema identities follow the
//! same qualified-name ordering as the model and endpoint emitters.

use alloc::{collections::BTreeMap, format, string::String};
use core::fmt::{self, Write};

use crate::{codegen_error, rust_string_literal, validate_options, ArtifactKind,
    CodegenOptions, EndpointField, EndpointModel, EndpointSchemaSpec, EndpointType,
    ErrorCode, GeneratedArtifact, RegistryArtifacts, Result, GENERATED_NOTICE};
use jmax_abi::SchemaId;

const MAX_DEPTH: usize = 128;

fn constraint(detail: &'static str) -> crate::Error {
    codegen_error(ErrorCode::ConstraintViolation, detail)
}

fn check_depth(depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        return Err(codegen_error(ErrorCode::DepthLimitExceeded,
            "registry type nesting exceeds 128"));
    }
    Ok(())
}

/// A zero-based position becomes a nonzero schema id, without truncation.
fn schema_id_at(index: usize) -> Result<u32> {
    index.checked_add(1).and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| codegen_error(ErrorCode::BudgetExhausted,
            "schema count exceeds u32"))
}

struct Catalog<'a> {
    types: BTreeMap<&'a str, &'a EndpointType>,
    nominals: BTreeMap<&'a str, &'a EndpointType>,
}

impl<'a> Catalog<'a> {
    fn collect(model: &'a EndpointModel) -> Result<Self> {
        let mut catalog = Self { types: BTreeMap::new(), nominals: BTreeMap::new() };
        for definition in &model.definitions {
            if catalog.types.insert(&definition.name, &definition.ty).is_some() {
                return Err(constraint("duplicate declared model type name"));
            }
        }
        for definition in &model.definitions { catalog.visit(&definition.ty, 0)?; }
        for service in &model.services {
            for endpoint in &service.endpoints {
                for field in endpoint.params.iter().chain(&endpoint.response) {
                    catalog.visit(&field.ty, 0)?;
                }
                if let Some(ty) = &endpoint.stream_response { catalog.visit(ty, 0)?; }
            }
        }
        for header in &model.headers { catalog.visit(&header.ty, 0)?; }
        Ok(catalog)
    }

    fn visit(&mut self, ty: &'a EndpointType, depth: usize) -> Result<()> {
        check_depth(depth)?;
        match ty {
            EndpointType::Struct { name, fields } => {
                self.intern(name, ty)?;
                for field in fields { self.visit(&field.ty, depth + 1)?; }
            }
            EndpointType::Enum { name, variants } => {
                if variants.is_empty() {
                    return Err(constraint("empty enum has no numeric JSON representation"));
                }
                self.intern(name, ty)?;
            }
            EndpointType::Vec(inner) | EndpointType::Optional(inner) => {
                self.visit(inner, depth + 1)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn intern(&mut self, name: &'a str, ty: &'a EndpointType) -> Result<()> {
        if self.nominals.insert(name, ty).is_some() {
            return Err(constraint("duplicate nominal model name"));
        }
        if let Some(existing) = self.types.get(name) {
            // The top-level declaration and its own nominal are one entry.
            if !core::ptr::eq(*existing, ty) {
                return Err(constraint("nominal model name collides with a definition"));
            }
        } else {
            self.types.insert(name, ty);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SchemaIdentity {
    id: u32,
    fingerprint: u64,
}

/// Reserve before every source append so allocation failure cannot be returned
/// as a successfully truncated artifact. Dependency failures remain unchanged.
#[derive(Default)]
struct Source {
    text: String,
    schema_id: u32,
}

impl Write for Source {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.text.try_reserve(text.len()).map_err(|_| fmt::Error)?;
        self.text.push_str(text);
        Ok(())
    }
}

impl Source {
    fn append(&mut self, arguments: fmt::Arguments<'_>) -> Result<()> {
        self.write_fmt(arguments).map_err(|_| codegen_error(ErrorCode::BudgetExhausted,
            "cannot allocate registry source").with_schema(self.schema_id, 0))
    }
}

fn insert_candidate<'a>(candidates: &mut BTreeMap<String, &'a [EndpointField]>,
    name: String, fields: &'a [EndpointField]) -> Result<()> {
    if candidates.insert(name, fields).is_some() {
        return Err(constraint("duplicate qualified schema name"));
    }
    Ok(())
}

fn schema_candidates<'a>(model: &'a EndpointModel, catalog: &Catalog<'a>)
    -> Result<BTreeMap<String, &'a [EndpointField]>> {
    let mut candidates = BTreeMap::new();
    for (name, ty) in &catalog.nominals {
        if let EndpointType::Struct { fields, .. } = ty {
            insert_candidate(&mut candidates, format!("model::{name}"), fields)?;
        }
    }
    for service in &model.services {
        for endpoint in &service.endpoints {
            insert_candidate(&mut candidates,
                format!("request::{}.{}", service.name, endpoint.name), &endpoint.params)?;
            insert_candidate(&mut candidates,
                format!("response::{}.{}", service.name, endpoint.name), &endpoint.response)?;
        }
    }
    Ok(candidates)
}

fn schema_tables<'a>(model: &'a EndpointModel, catalog: &Catalog<'a>, options: &CodegenOptions)
    -> Result<(BTreeMap<String, SchemaIdentity>, String)> {
    let mut source = Source::default();
    source.append(format_args!("{GENERATED_NOTICE}"))?;
    let mut identities = BTreeMap::new();
    if !options.emit_schema_tables {
        source.append(format_args!("pub const SCHEMA_COUNT: usize = 0;\n"))?;
        return Ok((identities, source.text));
    }

    // BTreeMap string ordering is lexicographic UTF-8 byte ordering. Include
    // every candidate before assigning ids, even if a later candidate fails.
    let candidates = schema_candidates(model, catalog)?;
    let count = u32::try_from(candidates.len()).map_err(|_| codegen_error(
        ErrorCode::BudgetExhausted, "schema count exceeds u32"))?;
    source.append(format_args!("pub const SCHEMA_COUNT: usize = {count};\n"))?;
    for (index, (name, fields)) in candidates.into_iter().enumerate() {
        let id = schema_id_at(index)?;
        let ir = jmax_endpoint_model::utilities::schema_for_fields(&name, SchemaId(id), fields, true)?;
        let program = jmax_schema_aot::compile_schema(&ir)?;
        let fingerprint = jmax_schema_aot::fingerprint_schema(&ir)?;
        if program.image().header.schema_id != id || program.image().header.fingerprint != fingerprint {
            return Err(codegen_error(ErrorCode::SchemaImageInvalid,
                "compiled schema identity differs from canonical IR").with_schema(id, 0));
        }
        let symbol = format!("SCHEMA_{id}");
        let tables = jmax_schema_aot::tables::emit_program(&program, &jmax_schema_aot::TableOptions {
            symbol: &symbol, abi_path: &options.abi_path, aot_path: &options.aot_path,
        })?;
        source.schema_id = id;
        source.append(format_args!("{tables}"))?;
        identities.insert(name, SchemaIdentity { id, fingerprint });
    }
    Ok((identities, source.text))
}

fn optional_u32(value: Option<u32>) -> String {
    match value {
        Some(value) => format!("::core::option::Option::Some({value}u32)"),
        None => String::from("::core::option::Option::None"),
    }
}

fn optional_u64(value: Option<u64>) -> String {
    match value {
        Some(value) => format!("::core::option::Option::Some({value}u64)"),
        None => String::from("::core::option::Option::None"),
    }
}

// Compile these same tokens in the lookup tests. The emitted functions therefore
// cannot drift from the exercised binary-search and static-borrow behavior.
macro_rules! source_tokens {
    ($($source:tt)*) => { concat!(stringify!($($source)*), "\n") };
}

macro_rules! registry_support {
    ($emit:ident) => { $emit! {
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TypeEntry {
    pub name: &'static str,
    pub schema_id: Option<u32>,
    pub fingerprint: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointEntry {
    pub code: u32,
    pub method: &'static str,
    pub roles: &'static [&'static str],
    pub request_schema: Option<u32>,
    pub response_schema: Option<u32>,
}

pub fn type_by_name(name: &str) -> Option<&'static TypeEntry> {
    TYPES.binary_search_by(|entry| entry.name.as_bytes().cmp(name.as_bytes()))
        .ok().and_then(|index| TYPES.get(index))
}

pub fn endpoint_by_code(code: u32) -> Option<&'static EndpointEntry> {
    ENDPOINTS.binary_search_by_key(&code, |entry| entry.code)
        .ok().and_then(|index| ENDPOINTS.get(index))
}
    } };
}

const REGISTRY_SUPPORT: &str = registry_support!(source_tokens);

fn registry_source(model: &EndpointModel, catalog: &Catalog<'_>,
    identities: &BTreeMap<String, SchemaIdentity>) -> Result<String> {
    let mut source = Source::default();
    source.append(format_args!("{GENERATED_NOTICE}{REGISTRY_SUPPORT}pub static TYPES: &[TypeEntry] = &[\n"))?;
    for (name, ty) in &catalog.types {
        // Only actual nominal records own schema providers. A scalar, enum,
        // container alias, or reference alias does not invent another schema.
        let identity = if matches!(ty, EndpointType::Struct { .. }) {
            identities.get(&format!("model::{name}"))
        } else { None };
        source.append(format_args!("    TypeEntry {{ name: {}, schema_id: {}, fingerprint: {} }},\n",
            rust_string_literal(name), optional_u32(identity.map(|entry| entry.id)),
            optional_u64(identity.map(|entry| entry.fingerprint))))?;
    }
    source.append(format_args!("];\npub static ENDPOINTS: &[EndpointEntry] = &[\n"))?;
    let mut endpoints: BTreeMap<u32, &EndpointSchemaSpec> = BTreeMap::new();
    for service in &model.services {
        for endpoint in &service.endpoints {
            if endpoints.insert(endpoint.code, endpoint).is_some() {
                return Err(constraint("duplicate endpoint code"));
            }
        }
    }
    for endpoint in endpoints.values() {
        let method = jmax_endpoint_model::wire_names::method_name(&endpoint.service, &endpoint.name)?;
        let request = identities.get(&format!("request::{method}"));
        let response = identities.get(&format!("response::{method}"));
        source.append(format_args!("    EndpointEntry {{ code: {}u32, method: {}, roles: &[",
            endpoint.code, rust_string_literal(&method)))?;
        for role in &endpoint.roles {
            source.append(format_args!("{},", rust_string_literal(role)))?;
        }
        source.append(format_args!("], request_schema: {}, response_schema: {} }},\n",
            optional_u32(request.map(|entry| entry.id)), optional_u32(response.map(|entry| entry.id))))?;
    }
    source.append(format_args!("];\n"))?;
    Ok(source.text)
}

/// Generate both registry artifacts, including explicit empty catalogs.
///
/// Each call validates its options and complete model independently. Enabling
/// schema tables requires every candidate to compile with its exact semantics;
/// an error discards all local source. No model, file, or sibling emitter is
/// modified or invoked, and no validation proof is constructed by this module.
pub fn generate_registries(model: &EndpointModel, options: &CodegenOptions) -> Result<RegistryArtifacts> {
    validate_options(options)?;
    let _registry = jmax_endpoint_model::registry::build_registry(model)?;
    let catalog = Catalog::collect(model)?;
    let (identities, tables) = schema_tables(model, &catalog, options)?;
    let registries = registry_source(model, &catalog, &identities)?;
    Ok(RegistryArtifacts {
        registries: GeneratedArtifact { path: format!("{}/registries.rs", options.module_name),
            contents: registries, kind: ArtifactKind::Registries },
        schema_tables: GeneratedArtifact { path: format!("{}/schema_tables.rs", options.module_name),
            contents: tables, kind: ArtifactKind::SchemaTables },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{boxed::Box, vec, vec::Vec};
    use crate::{EnumVariantSpec, ErrorSource, HeaderSpec, RoleSpec, ServiceSpec, TypeDefinition};

    fn options_without_tables() -> CodegenOptions {
        CodegenOptions { emit_schema_tables: false, ..CodegenOptions::default() }
    }

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: String::from(name), ty, description: String::new() }
    }

    fn record(name: &str, fields: Vec<EndpointField>) -> EndpointType {
        EndpointType::Struct { name: String::from(name), fields }
    }

    fn enumeration(name: &str) -> EndpointType {
        EndpointType::Enum { name: String::from(name), variants: vec![EnumVariantSpec {
            name: String::from("Ready"), value: i32::MIN, description: String::from("ready é"),
        }] }
    }

    fn definition(name: &str, ty: EndpointType) -> TypeDefinition {
        TypeDefinition { name: String::from(name), ty }
    }

    fn endpoint(service: &str, name: &str, code: u32) -> EndpointSchemaSpec {
        EndpointSchemaSpec { code, name: String::from(name), service: String::from(service),
            roles: vec![], params: vec![], response: vec![], stream_response: None,
            description: String::new() }
    }

    fn service(name: &str, id: u16, endpoints: Vec<EndpointSchemaSpec>) -> ServiceSpec {
        ServiceSpec { name: String::from(name), id, endpoints }
    }

    fn simple_model() -> EndpointModel {
        EndpointModel { services: vec![service("api", 1, vec![endpoint("api", "get", 7)])],
            ..EndpointModel::default() }
    }

    fn parse_program(source: &str, id: u32) -> Vec<u8> {
        let marker = format!("pub static SCHEMA_{id}_PROGRAM: &[u8] = &[\n");
        let tail = source.split_once(marker.as_str()).unwrap().1;
        let bytes = tail.split_once("];\n").unwrap().0;
        bytes.split_whitespace().map(|token| {
            let hex = token.strip_prefix("0x").unwrap().strip_suffix("u8,").unwrap();
            u8::from_str_radix(hex, 16).unwrap()
        }).collect()
    }

    fn type_line<'a>(source: &'a str, name: &str) -> &'a str {
        let prefix = format!("    TypeEntry {{ name: {},", rust_string_literal(name));
        source.lines().find(|line| line.starts_with(prefix.as_str())).unwrap()
    }

    #[test]
    fn empty_models_emit_both_complete_artifacts_in_both_modes() {
        for emit_schema_tables in [false, true] {
            let options = CodegenOptions { emit_schema_tables, ..CodegenOptions::default() };
            let model = EndpointModel::default();
            let output = generate_registries(&model, &options).unwrap();
            assert_eq!(output, generate_registries(&model, &options).unwrap());
            assert_eq!(output.registries.path, "generated/registries.rs");
            assert_eq!(output.registries.kind, ArtifactKind::Registries);
            assert_eq!(output.schema_tables.path, "generated/schema_tables.rs");
            assert_eq!(output.schema_tables.kind, ArtifactKind::SchemaTables);
            assert_eq!(output.schema_tables.contents,
                format!("{GENERATED_NOTICE}pub const SCHEMA_COUNT: usize = 0;\n"));
            assert!(output.registries.contents.contains("pub static TYPES: &[TypeEntry] = &[\n];\n"));
            assert!(output.registries.contents.contains("pub static ENDPOINTS: &[EndpointEntry] = &[\n];\n"));
            for artifact in [&output.registries, &output.schema_tables] {
                assert!(artifact.contents.starts_with(GENERATED_NOTICE));
                assert!(artifact.contents.ends_with('\n'));
                assert!(!artifact.contents.ends_with("\n\n"));
                assert!(!artifact.contents.contains('\r'));
                crate::validate_artifact_path(&artifact.path).unwrap();
            }
        }
    }

    #[test]
    fn catalogs_include_aliases_and_all_inline_declarations_once_in_byte_order() {
        let mut model = simple_model();
        model.definitions = vec![
            definition("Zulu", record("Zulu", vec![field("child", record("Child", vec![]))])),
            definition("Number", EndpointType::UInt32),
            definition("State", enumeration("State")),
            definition("Alias", EndpointType::StructRef(String::from("Zulu"))),
            definition("Collection", EndpointType::Vec(Box::new(record("Element", vec![])))),
        ];
        model.services[0].endpoints[0].params = vec![field("param", record("Param", vec![]))];
        model.services[0].endpoints[0].response = vec![field("reply", enumeration("Reply"))];
        model.services[0].endpoints[0].stream_response = Some(record("StreamItem", vec![]));
        model.headers = vec![HeaderSpec { name: String::from("X-Record"), ty: record("Header", vec![]),
            required: false, description: String::new() }];
        let before = model.clone();
        let catalog = Catalog::collect(&model).unwrap();
        let expected = ["Alias", "Child", "Collection", "Element", "Header", "Number", "Param",
            "Reply", "State", "StreamItem", "Zulu"];
        assert_eq!(catalog.types.keys().copied().collect::<Vec<_>>(), expected);
        let options = options_without_tables();
        let output = generate_registries(&model, &options).unwrap();
        let source = &output.registries.contents;
        let mut previous = 0;
        for name in expected {
            let line = type_line(source, name);
            assert!(line.contains("schema_id: ::core::option::Option::None"));
            assert!(line.contains("fingerprint: ::core::option::Option::None"));
            let position = source.find(line).unwrap();
            assert!(position > previous);
            previous = position;
        }
        assert_eq!(source.matches("    TypeEntry { name:").count(), expected.len());
        assert_eq!(output, generate_registries(&model, &options).unwrap());
        assert_eq!(model, before);
    }

    #[test]
    fn schemas_share_the_exact_sorted_identity_of_all_prior_emitters() {
        let mut model = simple_model();
        model.definitions = vec![definition("Zulu", record("Zulu", vec![])),
            definition("Alpha", record("Alpha", vec![field("count", EndpointType::UInt32)])),
            definition("Number", EndpointType::Int64), definition("State", enumeration("State")),
            definition("Alias", EndpointType::StructRef(String::from("Alpha")))];
        model.headers = vec![HeaderSpec { name: String::from("X-Record"), ty: record("Header", vec![]),
            required: false, description: String::new() }];
        model.services[0].endpoints[0].stream_response = Some(record("StreamItem", vec![]));
        model.services[0].endpoints[0].params = vec![field("z", EndpointType::UInt32),
            field("a", EndpointType::Optional(Box::new(EndpointType::String)))];
        model.services[0].endpoints[0].response = vec![field("ok", EndpointType::Boolean)];
        let catalog = Catalog::collect(&model).unwrap();
        let candidates = schema_candidates(&model, &catalog).unwrap();
        let options = CodegenOptions::default();
        let (identities, source) = schema_tables(&model, &catalog, &options).unwrap();
        let expected = ["model::Alpha", "model::Header", "model::StreamItem", "model::Zulu",
            "request::api.get", "response::api.get"];
        assert_eq!(identities.keys().map(String::as_str).collect::<Vec<_>>(), expected);
        assert!(source.contains("pub const SCHEMA_COUNT: usize = 6;\n"));
        for (index, name) in expected.iter().enumerate() {
            let identity = identities[*name];
            assert_eq!(identity.id, u32::try_from(index + 1).unwrap());
            let ir = jmax_endpoint_model::utilities::schema_for_fields(name, SchemaId(identity.id),
                candidates[*name], true).unwrap();
            assert!(ir.reject_unknown);
            assert_eq!(identity.fingerprint, jmax_schema_aot::fingerprint_schema(&ir).unwrap());
            let expected_program = jmax_schema_aot::compile_schema(&ir).unwrap();
            let loaded = jmax_schema_aot::load_program(&parse_program(&source, identity.id)).unwrap();
            assert_eq!(loaded.image().header, expected_program.image().header);
            assert_eq!(loaded.image().insns, expected_program.image().insns);
            assert_eq!(loaded.image().keys, expected_program.image().keys);
            assert_eq!(loaded.image().key_bytes, expected_program.image().key_bytes);
            assert_eq!(loaded.program().states, expected_program.program().states);
            assert_eq!(loaded.program().key_tables, expected_program.program().key_tables);
            assert_eq!(loaded.program().patterns, expected_program.program().patterns);
            assert_eq!(loaded.program().slots, expected_program.program().slots);
            assert_eq!(loaded.program().policy, expected_program.program().policy);
            assert_eq!(loaded.program().format_options, expected_program.program().format_options);
            assert_eq!(loaded.program().canonical_schema, expected_program.program().canonical_schema);
            let symbol = format!("SCHEMA_{}", identity.id);
            let exact = jmax_schema_aot::tables::emit_program(&expected_program,
                &jmax_schema_aot::TableOptions { symbol: &symbol, abi_path: &options.abi_path,
                    aot_path: &options.aot_path }).unwrap();
            assert!(source.contains(exact.as_str()));
        }
        let output = generate_registries(&model, &options).unwrap();
        for name in ["Alpha", "Header", "StreamItem", "Zulu"] {
            let identity = identities[&format!("model::{name}")];
            let line = type_line(&output.registries.contents, name);
            assert!(line.contains(optional_u32(Some(identity.id)).as_str()));
            assert!(line.contains(optional_u64(Some(identity.fingerprint)).as_str()));
        }
        for name in ["Alias", "Number", "State"] {
            let line = type_line(&output.registries.contents, name);
            assert!(line.contains("schema_id: ::core::option::Option::None"));
            assert!(line.contains("fingerprint: ::core::option::Option::None"));
        }
        assert!(output.registries.contents.contains("request_schema: ::core::option::Option::Some(5u32), response_schema: ::core::option::Option::Some(6u32)"));
        assert_eq!(output.schema_tables.contents, source);
    }

    #[test]
    fn endpoint_codes_sort_numerically_while_methods_roles_and_schema_ids_stay_canonical() {
        let mut last = endpoint("zeta", "last", u32::MAX);
        last.roles = vec![String::from("writer"), String::from("reader")];
        let model = EndpointModel {
            services: vec![service("zeta", u16::MAX, vec![last]),
                service("alpha", 0, vec![endpoint("alpha", "zero", 0), endpoint("alpha", "middle", 7)])],
            roles: vec![RoleSpec { name: String::from("reader"), includes: vec![], description: String::new() },
                RoleSpec { name: String::from("writer"), includes: vec![String::from("reader")], description: String::new() }],
            ..EndpointModel::default()
        };
        let source = generate_registries(&model, &CodegenOptions::default()).unwrap().registries.contents;
        let zero = source.find("    EndpointEntry { code: 0u32,").unwrap();
        let middle = source.find("    EndpointEntry { code: 7u32,").unwrap();
        let maximum = source.find("    EndpointEntry { code: 4294967295u32,").unwrap();
        assert!(zero < middle && middle < maximum);
        assert!(source.contains("method: \"zeta.last\", roles: &[\"writer\",\"reader\",]"));
        assert!(source.contains("method: \"alpha.zero\", roles: &[], request_schema: ::core::option::Option::Some(2u32), response_schema: ::core::option::Option::Some(5u32)"));
        assert!(source.contains("method: \"alpha.middle\", roles: &[], request_schema: ::core::option::Option::Some(1u32), response_schema: ::core::option::Option::Some(4u32)"));
        assert!(source.contains("request_schema: ::core::option::Option::Some(3u32), response_schema: ::core::option::Option::Some(6u32)"));
    }

    #[test]
    fn dense_fields_preserve_order_requiredness_bounds_and_format_side_tables() {
        let mut model = simple_model();
        model.services[0].endpoints[0].params = vec![
            field("z", EndpointType::UInt32), field("a", EndpointType::Int32),
            field("long", EndpointType::Int64), field("time", EndpointType::TimeStampMs),
            field("f", EndpointType::Float64), field("flag", EndpointType::Boolean),
            field("text", EndpointType::String), field("uuid", EndpointType::UUID),
            field("ip", EndpointType::IpAddr), field("bytes", EndpointType::Bytea),
            field("nano", EndpointType::NanoId { len: 21 }), field("address", EndpointType::BlockchainAddress),
            field("hash", EndpointType::BlockchainTransactionHash),
            field("maybe", EndpointType::Optional(Box::new(EndpointType::String))),
        ];
        let fields = &model.services[0].endpoints[0].params;
        let ir = jmax_endpoint_model::utilities::schema_for_fields("request::api.get", SchemaId(1), fields, true).unwrap();
        assert_eq!(ir.required_count, 13);
        for (index, (actual, original)) in ir.fields.iter().zip(fields).enumerate() {
            assert_eq!(actual.field_id.0, u32::try_from(index).unwrap());
            assert_eq!(actual.name, original.name);
            assert_eq!(actual.required, index != 13);
        }
        assert_eq!((ir.fields[0].min_i64, ir.fields[0].max_i64), (Some(0), Some(i64::from(u32::MAX))));
        assert_eq!((ir.fields[1].min_i64, ir.fields[1].max_i64), (Some(i64::from(i32::MIN)), Some(i64::from(i32::MAX))));
        for index in [2, 3] {
            assert_eq!((ir.fields[index].min_i64, ir.fields[index].max_i64), (Some(i64::MIN), Some(i64::MAX)));
        }
        for (index, format) in [(7, "uuid"), (8, "ip"), (9, "base64"), (10, "nanoid"),
            (11, "blockchain-address"), (12, "blockchain-transaction-hash")] {
            assert_eq!(ir.fields[index].format.as_deref(), Some(format));
        }
        let output = generate_registries(&model, &CodegenOptions::default()).unwrap();
        let loaded = jmax_schema_aot::load_program(&parse_program(&output.schema_tables.contents, 1)).unwrap();
        assert_eq!(loaded.image().header.slot_count, 14);
        assert_eq!(loaded.image().header.fingerprint, jmax_schema_aot::fingerprint_schema(&ir).unwrap());
        assert_eq!(loaded.program().slots.len(), fields.len());
        let empty = jmax_schema_aot::load_program(&parse_program(&output.schema_tables.contents, 2)).unwrap();
        assert_eq!(empty.image().header.slot_count, 0);
        assert!(empty.program().slots.is_empty());
    }

    #[test]
    fn disabled_tables_skip_every_unsupported_projection_and_clear_all_identities() {
        let mut model = simple_model();
        model.definitions = vec![definition("Compound", record("Compound", vec![
            field("items", EndpointType::Vec(Box::new(EndpointType::Object)))]))];
        model.services[0].endpoints[0].params = vec![field("decimal", EndpointType::BlockchainDecimal)];
        let output = generate_registries(&model, &options_without_tables()).unwrap();
        assert_eq!(output.schema_tables.contents,
            format!("{GENERATED_NOTICE}pub const SCHEMA_COUNT: usize = 0;\n"));
        assert!(!output.registries.contents.contains("::core::option::Option::Some("));
        assert!(!output.schema_tables.contents.contains("_PROGRAM"));
        let error = generate_registries(&model, &CodegenOptions::default()).unwrap_err();
        assert_eq!(error.code, ErrorCode::UnsupportedSchemaFeature);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
    }

    #[test]
    fn every_unsupported_candidate_aborts_generation_without_weakening_the_schema() {
        let unsupported = [EndpointType::Object, EndpointType::BlockchainDecimal, EndpointType::Unit,
            EndpointType::NanoId { len: 20 }, EndpointType::Vec(Box::new(EndpointType::UInt32)),
            EndpointType::Optional(Box::new(EndpointType::Optional(Box::new(EndpointType::String)))),
            record("Nested", vec![]), enumeration("State")];
        for ty in unsupported {
            for location in 0..3 {
                let mut model = simple_model();
                let fields = vec![field("supported", EndpointType::Boolean), field("unsupported", ty.clone())];
                match location {
                    0 => model.definitions.push(definition("Record", record("Record", fields))),
                    1 => model.services[0].endpoints[0].params = fields,
                    _ => model.services[0].endpoints[0].response = fields,
                }
                let catalog = Catalog::collect(&model).unwrap();
                let candidates = schema_candidates(&model, &catalog).unwrap();
                let mut expected = None;
                for (index, (name, fields)) in candidates.iter().enumerate() {
                    let ir = jmax_endpoint_model::utilities::schema_for_fields(name,
                        SchemaId(schema_id_at(index).unwrap()), fields, true);
                    let attempt = ir.and_then(|ir| jmax_schema_aot::compile_schema(&ir));
                    if let Err(error) = attempt { expected = Some(error); break; }
                }
                let expected = expected.unwrap();
                assert_eq!(expected.code, ErrorCode::UnsupportedSchemaFeature);
                let before = model.clone();
                assert_eq!(generate_registries(&model, &CodegenOptions::default()).unwrap_err(), expected);
                assert_eq!(model, before);
                assert!(generate_registries(&model, &options_without_tables()).is_ok());
            }
        }
    }

    #[test]
    fn malformed_models_propagate_registry_errors_even_with_tables_disabled() {
        let mut models = Vec::new();
        let mut duplicate_code = simple_model();
        duplicate_code.services[0].endpoints.push(endpoint("api", "other", 7));
        models.push(duplicate_code);
        let mut mismatch = simple_model();
        mismatch.services[0].endpoints[0].service = String::from("wrong");
        models.push(mismatch);
        let mut role = simple_model();
        role.services[0].endpoints[0].roles.push(String::from("missing"));
        models.push(role);
        models.push(EndpointModel { definitions: vec![definition("Bad",
            EndpointType::StructRef(String::from("Missing")))], ..EndpointModel::default() });
        models.push(EndpointModel { definitions: vec![definition("A", EndpointType::StructRef(String::from("B"))),
            definition("B", EndpointType::StructRef(String::from("A")))], ..EndpointModel::default() });
        models.push(EndpointModel { definitions: vec![definition("Mismatch", record("Other", vec![]))],
            ..EndpointModel::default() });
        let mut duplicate_field = simple_model();
        duplicate_field.services[0].endpoints[0].params = vec![field("x", EndpointType::UInt32), field("x", EndpointType::String)];
        models.push(duplicate_field);
        let mut injected = simple_model();
        injected.services[0].name = String::from("api\";\ninjected");
        models.push(injected);
        for model in models {
            let expected = jmax_endpoint_model::registry::build_registry(&model).unwrap_err();
            for options in [CodegenOptions::default(), options_without_tables()] {
                assert_eq!(generate_registries(&model, &options).unwrap_err(), expected);
            }
        }
    }

    #[test]
    fn recursive_references_terminate_and_aliases_do_not_create_schema_candidates() {
        let model = EndpointModel { definitions: vec![definition("Node", record("Node", vec![
            field("next", EndpointType::Optional(Box::new(EndpointType::StructRef(String::from("Node")))))])),
            definition("Alias", EndpointType::StructTable { struct_ref: String::from("Node") })],
            ..EndpointModel::default() };
        let catalog = Catalog::collect(&model).unwrap();
        assert_eq!(catalog.nominals.len(), 1);
        assert_eq!(catalog.types.len(), 2);
        assert_eq!(schema_candidates(&model, &catalog).unwrap().len(), 1);
        let output = generate_registries(&model, &options_without_tables()).unwrap();
        assert!(type_line(&output.registries.contents, "Alias").contains("Option::None"));
        assert_eq!(generate_registries(&model, &CodegenOptions::default()).unwrap_err().code,
            ErrorCode::UnsupportedSchemaFeature);
    }

    #[test]
    fn invalid_options_fail_before_any_model_processing_or_path_interpolation() {
        let mut model = simple_model();
        model.services[0].name.clear();
        let mut invalid = Vec::new();
        for name in ["", "..", "../escape", "CON", "bad-name", "mod", "é"] {
            invalid.push(CodegenOptions { module_name: String::from(name), ..CodegenOptions::default() });
        }
        for path in ["", "::abi; injected()", "::self::abi", "crate::abi::<T>", "::abi\n"] {
            invalid.push(CodegenOptions { abi_path: String::from(path), ..CodegenOptions::default() });
            invalid.push(CodegenOptions { aot_path: String::from(path), ..CodegenOptions::default() });
            invalid.push(CodegenOptions { derive_path: String::from(path), ..CodegenOptions::default() });
        }
        for options in invalid {
            let expected = validate_options(&options).unwrap_err();
            assert_eq!(generate_registries(&model, &options).unwrap_err(), expected);
        }
    }

    #[test]
    fn configured_paths_are_used_verbatim_and_arbitrary_field_bytes_stay_data() {
        let name = "quote\" slash\\ newline\n carriage\r tab\t nul\0 é🦀";
        let mut model = simple_model();
        model.services[0].endpoints[0].params = vec![field(name, EndpointType::String)];
        let options = CodegenOptions { module_name: String::from("client"),
            abi_path: String::from("crate::abi"), aot_path: String::from("crate::aot"),
            derive_path: String::from("crate::derive"), ..CodegenOptions::default() };
        let output = generate_registries(&model, &options).unwrap();
        assert_eq!(output.registries.path, "client/registries.rs");
        assert_eq!(output.schema_tables.path, "client/schema_tables.rs");
        assert!(output.schema_tables.contents.contains("pub static SCHEMA_1: crate::aot::StaticSchemaImage<'static>"));
        assert!(output.schema_tables.contents.contains("header: crate::abi::SchemaImageHeader"));
        assert!(!output.schema_tables.contents.contains("::jmax_abi"));
        assert!(!output.schema_tables.contents.contains("::jmax_schema_aot"));
        assert!(!output.schema_tables.contents.contains(name));
        assert!(!output.schema_tables.contents.contains('\0'));
        assert!(!output.schema_tables.contents.contains('\r'));
        let loaded = jmax_schema_aot::load_program(&parse_program(&output.schema_tables.contents, 1)).unwrap();
        assert_eq!(loaded.image().key_bytes, name.as_bytes());
    }

    #[test]
    fn index_literal_and_traversal_boundaries_are_checked() {
        assert_eq!(schema_id_at(0).unwrap(), 1);
        assert_eq!(schema_id_at(1).unwrap(), 2);
        assert_eq!(schema_id_at(usize::MAX).unwrap_err().code, ErrorCode::BudgetExhausted);
        if let Ok(maximum) = usize::try_from(u32::MAX) {
            assert_eq!(schema_id_at(maximum - 1).unwrap(), u32::MAX);
            assert_eq!(schema_id_at(maximum).unwrap_err().code, ErrorCode::BudgetExhausted);
        }
        assert_eq!(optional_u32(Some(0)), "::core::option::Option::Some(0u32)");
        assert_eq!(optional_u32(Some(u32::MAX)), "::core::option::Option::Some(4294967295u32)");
        assert_eq!(optional_u64(Some(0)), "::core::option::Option::Some(0u64)");
        assert_eq!(optional_u64(Some(u64::MAX)), "::core::option::Option::Some(18446744073709551615u64)");
        assert_eq!(optional_u32(None), "::core::option::Option::None");
        assert_eq!(optional_u64(None), "::core::option::Option::None");
        assert!(check_depth(0).is_ok());
        assert!(check_depth(127).is_ok());
        for depth in [128, 129, usize::MAX] {
            let error = check_depth(depth).unwrap_err();
            assert_eq!(error.code, ErrorCode::DepthLimitExceeded);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
            assert_eq!(error.byte_offset, 0);
        }
        let mut ty = EndpointType::UInt32;
        for _ in 0..128 { ty = EndpointType::Vec(Box::new(ty)); }
        let model = EndpointModel { definitions: vec![definition("Deep", ty)], ..EndpointModel::default() };
        assert_eq!(generate_registries(&model, &options_without_tables()).unwrap_err().code,
            ErrorCode::DepthLimitExceeded);
    }

    #[test]
    fn duplicate_internal_names_and_empty_enums_return_structured_errors() {
        let mut catalog = Catalog { types: BTreeMap::new(), nominals: BTreeMap::new() };
        let ty = record("Record", vec![]);
        catalog.visit(&ty, 0).unwrap();
        assert_eq!(catalog.visit(&ty, 0).unwrap_err().code, ErrorCode::ConstraintViolation);
        let other = record("Record", vec![]);
        let mut catalog = Catalog { types: BTreeMap::new(), nominals: BTreeMap::new() };
        catalog.types.insert("Record", &ty);
        assert_eq!(catalog.visit(&other, 0).unwrap_err().code, ErrorCode::ConstraintViolation);
        let fields = vec![];
        let mut candidates = BTreeMap::new();
        insert_candidate(&mut candidates, String::from("model::Record"), &fields).unwrap();
        assert_eq!(insert_candidate(&mut candidates, String::from("model::Record"), &fields).unwrap_err().code,
            ErrorCode::ConstraintViolation);
        let model = EndpointModel { definitions: vec![definition("Empty", EndpointType::Enum {
            name: String::from("Empty"), variants: vec![] })], ..EndpointModel::default() };
        for options in [CodegenOptions::default(), options_without_tables()] {
            let error = generate_registries(&model, &options).unwrap_err();
            assert_eq!(error.code, ErrorCode::ConstraintViolation);
            assert_eq!(error.source, ErrorSource::SchemaCompile);
        }
    }

    #[test]
    fn schema_images_are_deterministic_and_corruption_is_rejected_by_the_full_loader() {
        let model = simple_model();
        let first = generate_registries(&model, &CodegenOptions::default()).unwrap();
        assert_eq!(first, generate_registries(&model, &CodegenOptions::default()).unwrap());
        let bytes = parse_program(&first.schema_tables.contents, 1);
        assert!(jmax_schema_aot::load_program(&bytes).is_ok());
        for length in [0, 1, bytes.len() / 2, bytes.len() - 1] {
            assert!(jmax_schema_aot::load_program(&bytes[..length]).is_err());
        }
        let mut corrupt = bytes.clone();
        corrupt[0] ^= 0xff;
        assert_eq!(jmax_schema_aot::load_program(&corrupt).unwrap_err().code, ErrorCode::SchemaImageInvalid);
    }

    mod lookup {
        macro_rules! items { ($($source:tt)*) => { $($source)* }; }
        registry_support!(items);

        pub static TYPES: &[TypeEntry] = &[
            TypeEntry { name: "Alpha", schema_id: Some(1), fingerprint: Some(0) },
            TypeEntry { name: "Zulu", schema_id: None, fingerprint: None },
            TypeEntry { name: "_private", schema_id: Some(u32::MAX), fingerprint: Some(u64::MAX) },
            TypeEntry { name: "alpha", schema_id: None, fingerprint: None },
        ];
        pub static ENDPOINTS: &[EndpointEntry] = &[
            EndpointEntry { code: 0, method: "api.zero", roles: &[], request_schema: Some(1), response_schema: Some(2) },
            EndpointEntry { code: 7, method: "api.middle", roles: &["writer", "reader"], request_schema: None, response_schema: None },
            EndpointEntry { code: u32::MAX, method: "api.maximum", roles: &[], request_schema: None, response_schema: None },
        ];

        #[test]
        fn exact_case_sensitive_search_returns_static_entries_including_extremes() {
            for (index, entry) in TYPES.iter().enumerate() {
                let found: &'static TypeEntry = type_by_name(entry.name).unwrap();
                assert!(core::ptr::eq(found, &TYPES[index]));
            }
            for name in ["", "ALPHA", "Alph", "Alpha\0", "Beta", "zulu", "é", "🦀"] {
                assert_eq!(type_by_name(name), None);
            }
            for (index, entry) in ENDPOINTS.iter().enumerate() {
                let found: &'static EndpointEntry = endpoint_by_code(entry.code).unwrap();
                assert!(core::ptr::eq(found, &ENDPOINTS[index]));
            }
            for code in [1, 6, 8, u32::MAX - 1] { assert_eq!(endpoint_by_code(code), None); }
            assert_eq!(type_by_name("Alpha").unwrap().fingerprint, Some(0));
            assert_eq!(endpoint_by_code(7).unwrap().roles, ["writer", "reader"]);
        }
    }

    mod empty_lookup {
        macro_rules! items { ($($source:tt)*) => { $($source)* }; }
        registry_support!(items);
        pub static TYPES: &[TypeEntry] = &[];
        pub static ENDPOINTS: &[EndpointEntry] = &[];

        #[test]
        fn empty_searches_are_total() {
            for name in ["", "Alpha", "é"] { assert_eq!(type_by_name(name), None); }
            for code in [0, 1, u32::MAX] { assert_eq!(endpoint_by_code(code), None); }
        }
    }
}
