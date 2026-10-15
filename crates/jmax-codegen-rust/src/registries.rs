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
