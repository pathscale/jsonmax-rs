//! Schema compilation, strict payload validation, and endpoint catalog facade.

use alloc::{string::String, vec::Vec};

use jmax_abi::schema::{Selection, SlotValue};
use jmax_schema_compile::{CompilerOptions, ValidatedProgram};
use jmax_schema_vm::VmLimits;

use super::{
    facade_error, CompileLimits, CompiledSchema, EndpointCatalog, EndpointDirection,
    EndpointMessage, Error, ErrorCode, ErrorSource, ParseOptions, Payload, Result,
    SchemaId, SchemaImage, SchemaIr, ServiceSpec, ValidatedSlots,
};

/// Normalize, resolve, lower and validate an ABI schema using the native compiler.
pub fn compile_schema(ir: &SchemaIr, limits: CompileLimits) -> Result<CompiledSchema> {
    let normalized = jmax_schema_ir::model::normalize(ir)?;
    let resolved = jmax_schema_ir::resolve::resolve(
        normalized,
        jmax_schema_ir::ResolveLimits::default(),
    )?;
    let options = CompilerOptions { limits, ..CompilerOptions::default() };
    let program = jmax_schema_compile::lower::compile(&resolved, &options)
        .map_err(schema_compile_error)?;
    let image = program.image().clone();
    validate_image(&image)?;
    Ok(CompiledSchema { image })
}

/// Validate an externally supplied image before retaining it behind the facade.
pub fn load_schema(image: SchemaImage) -> Result<CompiledSchema> {
    validate_image(&image)?;
    Ok(CompiledSchema { image })
}

/// Validate the complete payload against every schema constraint.
pub fn validate_payload(
    input: Payload,
    schema: &CompiledSchema,
    options: ParseOptions,
) -> Result<ValidatedSlots> {
    let program = validated_program(schema)?;
    run_program(input, &program, &all_fields(&program)?, options)
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
    let program = validated_program(schema)?;
    jmax_schema_vm::check_selection(&program, selection)?;
    let mut slots = run_program(input, &program, selection, options)?;
    let mut ordered = Vec::new();
    ordered.try_reserve_exact(selection.field_ids.len()).map_err(|_| facade_error(
        ErrorCode::BudgetExhausted, ErrorSource::SchemaVm,
        "selected slot allocation failed",
    ))?;
    for field in &selection.field_ids {
        let slot = program.program().slots.iter()
            .find(|slot| slot.field_id == *field)
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
    Ok(EndpointCatalog { services: model.services })
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
    let schema = endpoint_schema(
        catalog,
        code,
        direction,
        CompileLimits::default_limits(),
    )?;
    let slots = validate_payload(input, &schema, options)?;
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

fn validated_program(schema: &CompiledSchema) -> Result<ValidatedProgram> {
    jmax_schema_compile::image::load(
        schema.image.clone(),
        &CompilerOptions::default(),
    ).map_err(schema_compile_error)
}

fn validate_image(image: &SchemaImage) -> Result<()> {
    jmax_schema_compile::image::load(
        image.clone(),
        &CompilerOptions::default(),
    ).map(|_| ()).map_err(schema_compile_error)
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

#[cfg(test)]
mod tests {
    use super::*;
    use jmax_abi::schema::{FieldId, SchemaFieldIr};
    use jmax_abi::syntax::ScalarKind;

    fn schema() -> SchemaIr {
        SchemaIr {
            name: String::from("trade"),
            schema_id: SchemaId(41),
            required_count: 1,
            reject_unknown: true,
            fields: alloc::vec![SchemaFieldIr {
                field_id: FieldId(7), name: String::from("quantity"), required: true,
                scalar_kind: Some(ScalarKind::Number), format: Some(String::from("int64")),
                min_len: None, max_len: None, min_i64: Some(0), max_i64: Some(100),
            }],
        }
    }

    fn assert_error<T>(result: Result<T>, code: ErrorCode) {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
    }

    #[test]
    fn schema_compilation_preserves_identity_and_checks_image_integrity() {
        let compiled = compile_schema(&schema(), CompileLimits::default_limits()).unwrap();
        assert_eq!(compiled.image().header.schema_id, 41);
        assert_ne!(compiled.fingerprint(), 0);
        let mut damaged = compiled.image().clone();
        damaged.header.fingerprint ^= 1;
        assert_error(load_schema(damaged), ErrorCode::SchemaImageInvalid);
    }

    #[test]
    fn schema_ir_boundaries_and_compile_limits_return_native_errors() {
        let mut malformed = schema();
        malformed.required_count = 0;
        assert_error(compile_schema(&malformed, CompileLimits::default_limits()), ErrorCode::ConstraintViolation);
        assert_error(compile_schema(&schema(), CompileLimits {
            max_states: 0, ..CompileLimits::default_limits()
        }), ErrorCode::BudgetExhausted);
    }

    #[test]
    fn validation_checks_required_types_bounds_unknowns_and_complete_syntax() {
        let compiled = compile_schema(&schema(), CompileLimits::default_limits()).unwrap();
        for (bytes, code) in [
            (br#"{"quantity":4}"#.as_slice(), None),
            (br#"{}"#, Some(ErrorCode::MissingRequiredField)),
            (br#"{"quantity":"4"}"#, Some(ErrorCode::TypeMismatch)),
            (br#"{"quantity":101}"#, Some(ErrorCode::ConstraintViolation)),
            (br#"{"quantity":4,"extra":1}"#, Some(ErrorCode::UnknownObjectKey)),
            (br#"{"quantity":4} false"#, Some(ErrorCode::TrailingContent)),
        ] {
            let result = validate_payload(Payload::from_slice(bytes), &compiled, ParseOptions::default());
            match code {
                None => {
                    let slots = result.unwrap();
                    assert_eq!(slots.slots.len(), 1);
                    assert_eq!(slots.proof.schema_fingerprint(), compiled.fingerprint());
                    assert_eq!(slots.proof.payload_len() as usize, bytes.len());
                }
                Some(expected) => assert_error(result, expected),
            }
        }
    }

    #[test]
    fn strict_extraction_rejects_weakening_duplicate_ids_and_bad_ids() {
        let compiled = compile_schema(&schema(), CompileLimits::default_limits()).unwrap();
        let payload = || Payload::from_slice(br#"{"quantity":9}"#);
        assert_error(extract_payload(payload(), &compiled, &Selection::default(), ParseOptions::default()), ErrorCode::ConstraintViolation);
        assert_error(extract_payload(payload(), &compiled, &Selection {
            field_ids: alloc::vec![FieldId(7), FieldId(7)], strict_validate_unselected: true,
        }, ParseOptions::default()), ErrorCode::ConstraintViolation);
        assert_error(extract_payload(payload(), &compiled, &Selection {
            field_ids: alloc::vec![FieldId(u32::MAX)], strict_validate_unselected: true,
        }, ParseOptions::default()), ErrorCode::UnknownObjectKey);
        let selected = extract_payload(payload(), &compiled, &Selection {
            field_ids: alloc::vec![FieldId(7)], strict_validate_unselected: true,
        }, ParseOptions::default()).unwrap();
        assert!(matches!(selected.slots.as_slice(),
            [SlotValue::I64(9)] | [SlotValue::U64(9)]));
    }

    #[test]
    fn catalog_rejects_global_duplicates_authorizes_before_parse_and_checks_directions() {
        let endpoint = jmax_abi::endpoint::EndpointSchemaSpec {
            code: u32::MAX, name: String::from("trade"), service: String::from("orders"),
            roles: alloc::vec![String::from("writer")], params: Vec::new(), response: Vec::new(),
            stream_response: None, description: String::new(),
        };
        let service = ServiceSpec { id: u16::MAX, name: String::from("orders"), endpoints: alloc::vec![endpoint.clone()] };
        let catalog = catalog(alloc::vec![service.clone()]).unwrap();
        assert_eq!(endpoint_schema(&catalog, 0, EndpointDirection::Request,
            CompileLimits::default_limits()).unwrap_err().code, ErrorCode::UnknownObjectKey);
        let error = validate_endpoint(Payload::from_slice(b"not json"), &catalog,
            u32::MAX, EndpointDirection::Request, "reader", ParseOptions::default()).unwrap_err();
        assert_eq!((error.code, error.source), (ErrorCode::AuthFailed, ErrorSource::Auth));
        assert_error(catalog(alloc::vec![service.clone(), service]), ErrorCode::ConstraintViolation);
    }

    #[test]
    fn endpoint_stream_missing_is_an_explicit_type_error() {
        let endpoint = jmax_abi::endpoint::EndpointSchemaSpec {
            code: 4, name: String::from("events"), service: String::from("feed"), roles: Vec::new(),
            params: Vec::new(), response: Vec::new(), stream_response: None,
            description: String::new(),
        };
        let catalog = catalog(alloc::vec![ServiceSpec {
            id: 1, name: String::from("feed"), endpoints: alloc::vec![endpoint],
        }]).unwrap();
        assert_error(endpoint_schema(&catalog, 4, EndpointDirection::Stream,
            CompileLimits::default_limits()), ErrorCode::TypeMismatch);
    }
}
