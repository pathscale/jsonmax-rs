//! Schema compilation, strict payload validation, and endpoint catalog facade.

use alloc::{string::String, sync::Arc, vec::Vec};

use jmax_abi::schema::Selection;
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_vm::VmLimits;

use super::{
    facade_error, CompileLimits, EndpointCatalog, EndpointDirection,
    EndpointMessage, Error, ErrorCode, ErrorSource, ParseOptions, Payload, Result,
    SchemaId, SchemaImage, SchemaIr, ServiceSpec, ValidatedSlots,
};

/// A compiled schema retains the complete validated program, including side tables.
#[derive(Clone, Debug)]
pub struct CompiledSchema {
    image: SchemaImage,
    ir: SchemaIr,
    native_ir: jmax_schema_ir::Schema,
    program: Arc<ValidatedProgram>,
    selection: Selection,
}

impl CompiledSchema {
    pub fn image(&self) -> &SchemaImage { &self.image }
    /// Original flat schema, or identity metadata for a native graph schema.
    /// Use native_ir() for the authoritative graph, including container shapes.
    pub fn ir(&self) -> &SchemaIr { &self.ir }
    pub fn native_ir(&self) -> &jmax_schema_ir::Schema { &self.native_ir }
    pub fn program(&self) -> &ValidatedProgram { &self.program }
    pub fn fingerprint(&self) -> u64 { self.image.header.fingerprint }
}

/// Normalize, resolve, lower and validate an ABI schema once.
pub fn compile_schema(ir: &SchemaIr, limits: CompileLimits) -> Result<CompiledSchema> {
    compile_graph(ir.clone(), jmax_schema_ir::model::normalize(ir)?, limits)
}

/// Compile a native graph, preserving array and nested object constraints.
pub fn compile_native_schema(
    graph: &jmax_schema_ir::Schema,
    limits: CompileLimits,
) -> Result<CompiledSchema> {
    let identity = SchemaIr {
        name: graph.name.clone(), schema_id: graph.schema_id,
        ..SchemaIr::default()
    };
    compile_graph(identity, graph.clone(), limits)
}

fn compile_graph(
    ir: SchemaIr,
    native_ir: jmax_schema_ir::Schema,
    limits: CompileLimits,
) -> Result<CompiledSchema> {
    let resolved = jmax_schema_ir::resolve::resolve(
        native_ir.clone(), jmax_schema_ir::ResolveLimits::default(),
    )?;
    let options = CompilerOptions { limits, ..CompilerOptions::default() };
    let program = jmax_schema_compile::lower::compile(&resolved, &options)
        .map_err(schema_compile_error)?;
    let selection = all_fields(&program)?;
    Ok(CompiledSchema {
        image: program.image().clone(), ir, native_ir,
        program: Arc::new(program), selection,
    })
}

/// An ABI image omits required program tables. Recompile its source and accept
/// it only when every retained image field matches that validated program.
pub fn load_schema(
    image: SchemaImage,
    ir: &SchemaIr,
    limits: CompileLimits,
) -> Result<CompiledSchema> {
    let schema = compile_schema(ir, limits)?;
    if image.header != schema.image.header || image.insns != schema.image.insns
        || image.keys != schema.image.keys || image.key_bytes != schema.image.key_bytes
    {
        return Err(facade_error(ErrorCode::SchemaImageInvalid, ErrorSource::SchemaCompile,
            "schema image does not match its complete source"));
    }
    Ok(schema)
}

/// Validate the complete payload against every schema constraint.
pub fn validate_payload(
    input: Payload,
    schema: &CompiledSchema,
    options: ParseOptions,
) -> Result<ValidatedSlots> {
    run_program(input, &schema.program, &schema.selection, options)
}

/// Validate the complete payload and return selected slots in caller order.
pub fn extract_payload(
    input: Payload,
    schema: &CompiledSchema,
    selection: &Selection,
    options: ParseOptions,
) -> Result<ValidatedSlots> {
    if !selection.strict_validate_unselected {
        return Err(Error::new(
            ErrorCode::ConstraintViolation,
            ErrorSource::SchemaVm,
            0,
            "validated slot extraction must validate unselected fields",
        ).with_schema(schema.image.header.schema_id, 0));
    }
    let program = &schema.program;
    let mut slots = run_program(input, program, selection, options)?;
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(selection.field_ids.len()).map_err(|_| facade_error(
        ErrorCode::BudgetExhausted, ErrorSource::SchemaVm,
        "selected slot allocation failed",
    ))?;
    for field in &selection.field_ids {
        let slot = program.slot_for_field(*field)
            .ok_or_else(|| Error::new(
                ErrorCode::SchemaImageInvalid, ErrorSource::SchemaVm, 0,
                "selected field has no slot in the validated image",
            ).with_schema(schema.image.header.schema_id, field.0))?;
        ordered.push(*slots.slots.get(slot.slot as usize).ok_or_else(|| Error::new(
            ErrorCode::SchemaImageInvalid, ErrorSource::SchemaVm, 0,
            "selected slot is outside the VM output",
        ).with_schema(schema.image.header.schema_id, field.0))?);
    }
    slots.slots = ordered;
    Ok(slots)
}

/// Own and validate services without performing network activity.
pub fn catalog(services: Vec<ServiceSpec>) -> Result<EndpointCatalog> {
    let model = jmax_endpoint_model::EndpointModel {
        services,
        ..jmax_endpoint_model::EndpointModel::default()
    };
    jmax_endpoint_model::registry::build_registry(&model).map_err(model_error)?;
    let count = model.services.iter().try_fold(0usize, |sum, service| {
        sum.checked_add(service.endpoints.len()).ok_or_else(|| facade_error(
            ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
            "endpoint count overflow"))
    })?;
    let capacity = count.checked_mul(2).and_then(|n| n.max(1).checked_next_power_of_two())
        .ok_or_else(|| facade_error(ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
            "endpoint lookup capacity overflow"))?;
    let mut entries = Vec::new();
    entries.try_reserve_exact(capacity).map_err(|_| facade_error(
        ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
        "endpoint lookup allocation failed"))?;
    entries.resize_with(capacity, || None);
    let mut catalog = EndpointCatalog { services: model.services, entries };
    for service_index in 0..catalog.services.len() {
        for endpoint_index in 0..catalog.services[service_index].endpoints.len() {
            let endpoint = &catalog.services[service_index].endpoints[endpoint_index];
            let code = endpoint.code;
            let request = project_schema(&catalog, endpoint, EndpointDirection::Request,
                CompileLimits::default_limits())?;
            let response = project_schema(&catalog, endpoint, EndpointDirection::Response,
                CompileLimits::default_limits())?;
            let mut index = catalog_hash(code) & (capacity - 1);
            while catalog.entries[index].is_some() { index = (index + 1) & (capacity - 1); }
            catalog.entries[index] = Some(CatalogEntry {
                code, service_index, endpoint_index, request, response,
            });
        }
    }
    Ok(catalog)
}

/// Project one endpoint direction into and compile its native schema.
pub fn endpoint_schema(
    catalog: &EndpointCatalog,
    code: u32,
    direction: EndpointDirection,
    limits: CompileLimits,
) -> Result<CompiledSchema> {
    let endpoint = catalog.endpoint(code).ok_or_else(|| facade_error(
        ErrorCode::UnknownObjectKey, ErrorSource::SchemaCompile,
        "endpoint code is not present in the catalog",
    ))?;
    project_schema(catalog, endpoint, direction, limits)
}

fn project_schema(
    catalog: &EndpointCatalog,
    endpoint: &super::EndpointSchemaSpec,
    direction: EndpointDirection,
    limits: CompileLimits,
) -> Result<CompiledSchema> {
    let (qualified_name, fields) = match direction {
        EndpointDirection::Request => {
            let mut name = String::from("request::");
            name.push_str(&endpoint.service);
            name.push('.');
            name.push_str(&endpoint.name);
            (name, endpoint.params.as_slice())
        }
        EndpointDirection::Response => {
            let mut name = String::from("response::");
            name.push_str(&endpoint.service);
            name.push('.');
            name.push_str(&endpoint.name);
            (name, endpoint.response.as_slice())
        }
        EndpointDirection::Stream => {
            let Some(_stream) = endpoint.stream_response.as_ref() else {
                return Err(facade_error(
                    ErrorCode::TypeMismatch, ErrorSource::SchemaCompile,
                    "endpoint does not declare a stream response",
                ));
            };
            return Err(facade_error(
                ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile,
                "stream projection requires the native stream schema projector",
            ));
        }
    };
    let schema_id = stable_projection_id(catalog, &qualified_name)?;
    let ir = jmax_endpoint_model::utilities::schema_for_fields(
        &qualified_name,
        schema_id,
        fields,
        true,
    ).map_err(model_error)?;
    compile_schema(&ir, limits)
}

/// Authorize the caller before parsing, then validate the raw directional body.
pub fn validate_endpoint(
    input: Payload,
    catalog: &EndpointCatalog,
    code: u32,
    direction: EndpointDirection,
    role: &str,
    options: ParseOptions,
) -> Result<EndpointMessage> {
    let endpoint = catalog.endpoint(code).ok_or_else(|| facade_error(
        ErrorCode::UnknownObjectKey, ErrorSource::Auth,
        "endpoint code is not present in the catalog",
    ))?;
    if !endpoint.roles.is_empty() && !endpoint.roles.iter().any(|allowed| allowed == role) {
        return Err(facade_error(
            ErrorCode::AuthFailed, ErrorSource::Auth,
            "caller role is not authorized for this endpoint",
        ));
    }
    let entry = catalog.entry(code).ok_or_else(|| facade_error(
        ErrorCode::InternalError, ErrorSource::SchemaCompile,
        "catalog schema entry is missing"))?;
    let schema = match direction {
        EndpointDirection::Request => &entry.request,
        EndpointDirection::Response => &entry.response,
        EndpointDirection::Stream => return Err(facade_error(
            ErrorCode::UnsupportedSchemaFeature, ErrorSource::SchemaCompile,
            "stream projection requires the native stream schema projector")),
    };
    let slots = validate_payload(input, schema, options)?;
    Ok(EndpointMessage { endpoint_code: code, direction, slots })
}

fn all_fields(program: &ValidatedProgram) -> Result<Selection> {
    let mut fields = Vec::new();
    fields.try_reserve_exact(program.program().slots.len()).map_err(|_| facade_error(
        ErrorCode::BudgetExhausted, ErrorSource::SchemaVm,
        "schema slot selection allocation failed",
    ))?;
    for slot in &program.program().slots {
        fields.push(slot.field_id);
    }
    Ok(Selection { field_ids: fields, strict_validate_unselected: true })
}

/// Assign the catalog's stable one-based projection ordinal by qualified name,
/// matching the endpoint model's canonical request/response catalog ordering.
fn stable_projection_id(catalog: &EndpointCatalog, target: &str) -> Result<SchemaId> {
    let mut preceding = 0u32;
    for endpoint in catalog.services.iter().flat_map(|service| service.endpoints.iter()) {
        for prefix in ["request::", "response::"] {
            let name = qualified_name(prefix, &endpoint.service, &endpoint.name)?;
            if name.as_bytes() < target.as_bytes() {
                preceding = preceding.checked_add(1).ok_or_else(|| facade_error(
                    ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
                    "endpoint schema identity count exceeds u32",
                ))?;
            }
        }
    }
    preceding.checked_add(1).map(SchemaId).ok_or_else(|| facade_error(
        ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
        "endpoint schema identity exceeds u32",
    ))
}

fn qualified_name(prefix: &str, service: &str, endpoint: &str) -> Result<String> {
    let length = prefix.len()
        .checked_add(service.len())
        .and_then(|value| value.checked_add(1))
        .and_then(|value| value.checked_add(endpoint.len()))
        .ok_or_else(|| facade_error(
            ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
            "qualified endpoint schema name length overflow",
        ))?;
    let mut name = String::new();
    name.try_reserve_exact(length).map_err(|_| facade_error(
        ErrorCode::BudgetExhausted, ErrorSource::SchemaCompile,
        "qualified endpoint schema name allocation failed",
    ))?;
    name.push_str(prefix);
    name.push_str(service);
    name.push('.');
    name.push_str(endpoint);
    Ok(name)
}

fn run_program(
    input: Payload,
    program: &ValidatedProgram,
    selection: &Selection,
    options: ParseOptions,
) -> Result<ValidatedSlots> {
    if u32::try_from(input.len()).is_err() {
        return Err(facade_error(
            ErrorCode::PayloadTooLarge, ErrorSource::SchemaVm,
            "payload exceeds the 32-bit proof length limit",
        ));
    }
    if options.numeric.max_exponent_abs < 0 {
        return Err(facade_error(
            ErrorCode::ConstraintViolation, ErrorSource::Lexer,
            "numeric exponent limit cannot be negative",
        ));
    }
    // The VM owns proof issuance and revalidates the input and every slot before
    // publishing the original payload and decoded arena together.
    jmax_schema_vm::execute::validate(
        program,
        jmax_schema_vm::input::CursorInput::new(input, options.limits, options.numeric)?,
        selection,
        VmLimits { syntax: options.limits, ..VmLimits::default() },
    )
}

/// Immutable entries use a bounded open-addressed index at a load factor <= 1/2.
#[derive(Clone, Debug)]
pub(crate) struct CatalogEntry {
    pub(crate) code: u32,
    pub(crate) service_index: usize,
    pub(crate) endpoint_index: usize,
    request: CompiledSchema,
    response: CompiledSchema,
}

pub(crate) fn catalog_hash(code: u32) -> usize {
    let mut value = code;
    value ^= value >> 16;
    value = value.wrapping_mul(0x7feb_352d);
    value ^= value >> 15;
    value = value.wrapping_mul(0x846c_a68b);
    (value ^ (value >> 16)) as usize
}

fn schema_compile_error(mut error: Error) -> Error {
    if error.source == ErrorSource::SchemaCompile {
        return error;
    }
    error.source = ErrorSource::SchemaCompile;
    error
}

fn model_error(mut error: Error) -> Error {
    if error.source == ErrorSource::SchemaCompile {
        return error;
    }
    error.source = ErrorSource::SchemaCompile;
    error
}

/// Reusable complete-input schema validation storage.
pub use jmax_schema_vm::ValidationScratch;

pub fn validate_payload_into<'a>(input: &Payload, schema: &CompiledSchema,
    options: ParseOptions, scratch: &'a mut ValidationScratch) -> Result<&'a ValidatedSlots> {
    jmax_schema_vm::execute::validate_into(schema.program(), input.clone(),
        &schema.selection, VmLimits { syntax: options.limits, ..VmLimits::default() },
        options.numeric, scratch)
}

pub fn extract_payload_into<'a>(input: &Payload, schema: &CompiledSchema,
    selection: &Selection, options: ParseOptions,
    scratch: &'a mut ValidationScratch) -> Result<&'a ValidatedSlots> {
    jmax_schema_vm::execute::validate_into(schema.program(), input.clone(), selection,
        VmLimits { syntax: options.limits, ..VmLimits::default() }, options.numeric, scratch)?;
    scratch.order_selection(schema.program(), selection)
}

/// Cache one compilation (including failures) per Rust type in std builds.
/// Compile under the lock so simultaneous first calls cannot compile twice.
#[cfg(feature = "std")]
pub(crate) fn cached_schema<T: super::SchemaProvider + 'static>() -> Result<Arc<CompiledSchema>> {
    use core::any::TypeId;
    use std::{collections::HashMap, sync::{Mutex, OnceLock}};
    static CACHE: OnceLock<Mutex<HashMap<TypeId, Result<Arc<CompiledSchema>>>>> = OnceLock::new();
    let mut cache = CACHE.get_or_init(|| Mutex::new(HashMap::new())).lock()
        .map_err(|_| facade_error(ErrorCode::InternalError, ErrorSource::SchemaCompile,
            "typed schema cache lock is poisoned"))?;
    cache.entry(TypeId::of::<T>()).or_insert_with(|| {
        let schema = compile_schema(&T::schema_ir(), CompileLimits::default_limits())?;
        if schema.fingerprint() != T::schema_fingerprint() {
            return Err(facade_error(ErrorCode::SchemaImageInvalid, ErrorSource::SchemaCompile,
                "type schema fingerprint does not match its compiled schema"));
        }
        Ok(Arc::new(schema))
    }).clone()
}
