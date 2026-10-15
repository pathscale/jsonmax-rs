//! Lossless adapters for application-owned endpoint-libs wrappers.
//!
//! Names are nonempty, case-sensitive UTF-8 strings. Codes are unique within
//! their service, while nominal types share the supplied service set's namespace.
//! Type roots have depth zero; each owned type edge consumes one depth level.
//! References are checked against declarations without expanding their targets,
//! so finite recursive declarations do not cause infinite traversal.

use alloc::{boxed::Box, string::String, vec::Vec};

use super::{
    CompatOptions, Conversion, EndpointField, EndpointSchemaSpec, EndpointType,
    EnumVariantSpec, Error, ErrorCode, ErrorSource, LegacyEndpoint, LegacyService,
    Result, ServiceSpec, LEGACY_FORMAT_VERSION,
};

/// Import an endpoint, deferring nominal reference resolution to service import.
pub fn import_endpoint(
    legacy: LegacyEndpoint,
    options: CompatOptions,
) -> Result<Conversion<EndpointSchemaSpec>> {
    version(legacy.version, options)?;
    validate_endpoint(&legacy.endpoint, options)?;
    Ok(Conversion::exact(legacy.endpoint))
}

/// Export an endpoint without changing any field, type, spelling, or metadata.
pub fn export_endpoint(
    endpoint: &EndpointSchemaSpec,
    options: CompatOptions,
) -> Result<Conversion<LegacyEndpoint>> {
    version(LEGACY_FORMAT_VERSION, options)?;
    validate_endpoint(endpoint, options)?;
    Ok(Conversion::exact(LegacyEndpoint {
        version: LEGACY_FORMAT_VERSION,
        endpoint: copy_endpoint(endpoint)?,
    }))
}

/// Import a complete service set, resolving references across every service.
///
/// Every wrapper version is checked before model validation. Nothing is returned
/// until the entire set, including stream responses and nominal conflicts, passes.
pub fn import_services(
    legacy: &[LegacyService],
    options: CompatOptions,
) -> Result<Conversion<Vec<ServiceSpec>>> {
    version(LEGACY_FORMAT_VERSION, options)?;
    for service in legacy {
        version(service.version, options)?;
        for endpoint in &service.endpoints {
            version(endpoint.version, options)?;
        }
    }
    validate_service_set(legacy.iter().map(|service| {
        (service.id, service.name.as_str(), service.endpoints.iter().map(|item| &item.endpoint))
    }), options)?;

    let mut services = Vec::new();
    reserve(&mut services, legacy.len())?;
    for service in legacy {
        let mut endpoints = Vec::new();
        reserve(&mut endpoints, service.endpoints.len())?;
        for endpoint in &service.endpoints {
            endpoints.push(copy_endpoint(&endpoint.endpoint)?);
        }
        services.push(ServiceSpec {
            id: service.id,
            name: copy_string(&service.name)?,
            endpoints,
        });
    }
    Ok(Conversion::exact(services))
}

/// Export a complete service set using only the representable version-1 shape.
pub fn export_services(
    services: &[ServiceSpec],
    options: CompatOptions,
) -> Result<Conversion<Vec<LegacyService>>> {
    version(LEGACY_FORMAT_VERSION, options)?;
    validate_service_set(services.iter().map(|service| {
        (service.id, service.name.as_str(), service.endpoints.iter())
    }), options)?;

    let mut legacy = Vec::new();
    reserve(&mut legacy, services.len())?;
    for service in services {
        let mut endpoints = Vec::new();
        reserve(&mut endpoints, service.endpoints.len())?;
        for endpoint in &service.endpoints {
            endpoints.push(LegacyEndpoint {
                version: LEGACY_FORMAT_VERSION,
                endpoint: copy_endpoint(endpoint)?,
            });
        }
        legacy.push(LegacyService {
            version: LEGACY_FORMAT_VERSION,
            id: service.id,
            name: copy_string(&service.name)?,
            endpoints,
        });
    }
    Ok(Conversion::exact(legacy))
}

fn error(code: ErrorCode, detail: &'static str) -> Error {
    Error::new(code, ErrorSource::SchemaCompile, 0, detail)
}

fn version(version: u16, options: CompatOptions) -> Result<()> {
    super::ensure_version(version, options).map_err(|mut error| {
        error.source = ErrorSource::SchemaCompile;
        error
    })
}

fn reserve<T>(values: &mut Vec<T>, additional: usize) -> Result<()> {
    values.try_reserve(additional).map_err(|_| {
        error(ErrorCode::BudgetExhausted, "endpoint compatibility allocation failed")
    })
}

fn push<T>(values: &mut Vec<T>, value: T) -> Result<()> {
    reserve(values, 1)?;
    values.push(value);
    Ok(())
}

fn required_name(name: &str) -> Result<()> {
    if name.is_empty() {
        Err(error(ErrorCode::MissingRequiredField, "endpoint model name is empty"))
    } else {
        Ok(())
    }
}

fn unique<T: Ord>(mut values: Vec<T>, code: ErrorCode, detail: &'static str) -> Result<()> {
    values.sort_unstable();
    if values.windows(2).any(|pair| pair[0] == pair[1]) {
        Err(error(code, detail))
    } else {
        Ok(())
    }
}

fn unique_names<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut values = Vec::new();
    for name in names {
        required_name(name)?;
        push(&mut values, name)?;
    }
    unique(values, ErrorCode::DuplicateObjectKey, "duplicate endpoint model name")
}

fn validate_endpoint(endpoint: &EndpointSchemaSpec, options: CompatOptions) -> Result<()> {
    let mut validator = Validator::new(options);
    validator.endpoint(endpoint)?;
    validator.finish(false)
}

fn validate_service_set<'a, I, E>(services: I, options: CompatOptions) -> Result<()>
where
    I: IntoIterator<Item = (u16, &'a str, E)>,
    E: IntoIterator<Item = &'a EndpointSchemaSpec>,
{
    let mut validator = Validator::new(options);
    let mut ids = Vec::new();
    let mut names = Vec::new();
    for (id, name, endpoints) in services {
        required_name(name)?;
        push(&mut ids, id)?;
        push(&mut names, name)?;
        let mut codes = Vec::new();
        let mut endpoint_names = Vec::new();
        for endpoint in endpoints {
            validator.endpoint(endpoint)?;
            if endpoint.service != name {
                return Err(error(
                    ErrorCode::ConstraintViolation,
                    "endpoint service differs from enclosing service name",
                ));
            }
            push(&mut codes, endpoint.code)?;
            push(&mut endpoint_names, endpoint.name.as_str())?;
        }
        unique(codes, ErrorCode::ConstraintViolation, "duplicate endpoint code within service")?;
        unique(endpoint_names, ErrorCode::DuplicateObjectKey, "duplicate endpoint name within service")?;
    }
    unique(ids, ErrorCode::ConstraintViolation, "duplicate service id")?;
    unique(names, ErrorCode::DuplicateObjectKey, "duplicate service name")?;
    validator.finish(true)
}

#[derive(Clone, Copy)]
enum ReferenceKind { Struct, Enum }

struct Validator<'a> {
    max_depth: u32,
    definitions: Vec<(&'a str, &'a EndpointType)>,
    references: Vec<(&'a str, ReferenceKind)>,
    pending: Vec<(&'a EndpointType, u32)>,
}

impl<'a> Validator<'a> {
    fn new(options: CompatOptions) -> Self {
        Self {
            max_depth: u32::from(options.limits.max_depth),
            definitions: Vec::new(),
            references: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn endpoint(&mut self, endpoint: &'a EndpointSchemaSpec) -> Result<()> {
        required_name(&endpoint.name)?;
        required_name(&endpoint.service)?;
        unique_names(endpoint.roles.iter().map(String::as_str))?;
        self.fields(&endpoint.params)?;
        self.fields(&endpoint.response)?;
        if let Some(ty) = &endpoint.stream_response {
            self.ty(ty)?;
        }
        Ok(())
    }

    fn fields(&mut self, fields: &'a [EndpointField]) -> Result<()> {
        unique_names(fields.iter().map(|field| field.name.as_str()))?;
        for field in fields {
            self.ty(&field.ty)?;
        }
        Ok(())
    }

    fn ty(&mut self, root: &'a EndpointType) -> Result<()> {
        push(&mut self.pending, (root, 0))?;
        while let Some((ty, depth)) = self.pending.pop() {
            if depth > self.max_depth {
                return Err(error(ErrorCode::DepthLimitExceeded, "endpoint type depth limit exceeded"));
            }
            match ty {
                EndpointType::Struct { name, fields } => {
                    required_name(name)?;
                    push(&mut self.definitions, (name.as_str(), ty))?;
                    unique_names(fields.iter().map(|field| field.name.as_str()))?;
                    reserve(&mut self.pending, fields.len())?;
                    for field in fields.iter().rev() {
                        self.pending.push((&field.ty, depth + 1));
                    }
                }
                EndpointType::Enum { name, variants } => {
                    required_name(name)?;
                    push(&mut self.definitions, (name.as_str(), ty))?;
                    unique_names(variants.iter().map(|variant| variant.name.as_str()))?;
                    let mut values = Vec::new();
                    reserve(&mut values, variants.len())?;
                    for variant in variants {
                        values.push(variant.value);
                    }
                    unique(values, ErrorCode::ConstraintViolation, "duplicate enum numeric value")?;
                }
                EndpointType::StructRef(name) | EndpointType::StructTable { struct_ref: name } => {
                    required_name(name)?;
                    push(&mut self.references, (name.as_str(), ReferenceKind::Struct))?;
                }
                EndpointType::EnumRef { name, .. } => {
                    required_name(name)?;
                    push(&mut self.references, (name.as_str(), ReferenceKind::Enum))?;
                }
                EndpointType::Vec(inner) | EndpointType::Optional(inner) => {
                    push(&mut self.pending, (inner.as_ref(), depth + 1))?;
                }
                EndpointType::NanoId { len: 0 } => {
                    return Err(error(ErrorCode::ConstraintViolation, "NanoId length must be nonzero"));
                }
                EndpointType::UInt32 | EndpointType::Int32 | EndpointType::Int64
                | EndpointType::Float64 | EndpointType::Boolean | EndpointType::String
                | EndpointType::Bytea | EndpointType::UUID | EndpointType::NanoId { .. }
                | EndpointType::IpAddr | EndpointType::Object | EndpointType::Unit
                | EndpointType::TimeStampMs | EndpointType::BlockchainDecimal
                | EndpointType::BlockchainAddress | EndpointType::BlockchainTransactionHash => {}
            }
        }
        Ok(())
    }

    fn finish(mut self, resolve_references: bool) -> Result<()> {
        self.definitions.sort_unstable_by(|left, right| left.0.cmp(right.0));
        for pair in self.definitions.windows(2) {
            if pair[0].0 == pair[1].0 && !same_type(pair[0].1, pair[1].1)? {
                return Err(error(ErrorCode::ConstraintViolation, "conflicting nominal type definitions"));
            }
        }
        if resolve_references {
            for (name, kind) in self.references {
                let index = self.definitions.binary_search_by(|definition| definition.0.cmp(name))
                    .map_err(|_| error(
                        ErrorCode::UnsupportedSchemaFeature,
                        "unresolved endpoint type reference",
                    ))?;
                let matches = matches!(
                    (kind, self.definitions[index].1),
                    (ReferenceKind::Struct, EndpointType::Struct { .. })
                        | (ReferenceKind::Enum, EndpointType::Enum { .. })
                );
                if !matches {
                    return Err(error(ErrorCode::ConstraintViolation, "endpoint reference has wrong nominal kind"));
                }
            }
        }
        Ok(())
    }
}

// Derived equality would recurse through owned containers. Use an explicit stack
// so conflict checking also honors large caller-configured traversal ceilings.
fn same_type(left: &EndpointType, right: &EndpointType) -> Result<bool> {
    let mut pending = Vec::new();
    push(&mut pending, (left, right))?;
    while let Some((left, right)) = pending.pop() {
        match (left, right) {
            (EndpointType::Vec(left), EndpointType::Vec(right))
            | (EndpointType::Optional(left), EndpointType::Optional(right)) => {
                push(&mut pending, (left.as_ref(), right.as_ref()))?;
            }
            (
                EndpointType::Struct { name: left_name, fields: left_fields },
                EndpointType::Struct { name: right_name, fields: right_fields },
            ) => {
                if left_name != right_name || left_fields.len() != right_fields.len() {
                    return Ok(false);
                }
                reserve(&mut pending, left_fields.len())?;
                for (left, right) in left_fields.iter().zip(right_fields).rev() {
                    if left.name != right.name || left.description != right.description {
                        return Ok(false);
                    }
                    pending.push((&left.ty, &right.ty));
                }
            }
            _ if left != right => return Ok(false),
            _ => {}
        }
    }
    Ok(true)
}

fn copy_string(source: &str) -> Result<String> {
    let mut result = String::new();
    result.try_reserve_exact(source.len()).map_err(|_| {
        error(ErrorCode::BudgetExhausted, "endpoint compatibility string allocation failed")
    })?;
    result.push_str(source);
    Ok(result)
}

fn copy_endpoint(source: &EndpointSchemaSpec) -> Result<EndpointSchemaSpec> {
    let mut roles = Vec::new();
    reserve(&mut roles, source.roles.len())?;
    for role in &source.roles {
        roles.push(copy_string(role)?);
    }
    Ok(EndpointSchemaSpec {
        code: source.code,
        name: copy_string(&source.name)?,
        service: copy_string(&source.service)?,
        roles,
        params: copy_fields(&source.params)?,
        response: copy_fields(&source.response)?,
        stream_response: source.stream_response.as_ref().map(copy_type).transpose()?,
        description: copy_string(&source.description)?,
    })
}

fn copy_fields(source: &[EndpointField]) -> Result<Vec<EndpointField>> {
    let mut fields = Vec::new();
    reserve(&mut fields, source.len())?;
    for field in source {
        fields.push(EndpointField {
            name: copy_string(&field.name)?,
            ty: copy_type(&field.ty)?,
            description: copy_string(&field.description)?,
        });
    }
    Ok(fields)
}

enum CopyTask<'a> {
    Visit(&'a EndpointType),
    Vec,
    Optional,
    Struct { name: &'a str, fields: &'a [EndpointField] },
}

fn copy_invariant() -> Error {
    error(ErrorCode::InternalError, "endpoint type copy stack is inconsistent")
}

// Copy owned trees in postorder without calling the ABI's recursive Clone.
// Every Vec/String allocation is fallible; Box allocation uses the stable alloc
// API and follows the allocator's normal allocation-error behavior.
fn copy_type(source: &EndpointType) -> Result<EndpointType> {
    let mut tasks = Vec::new();
    let mut results = Vec::new();
    push(&mut tasks, CopyTask::Visit(source))?;
    while let Some(task) = tasks.pop() {
        let value = match task {
            CopyTask::Visit(EndpointType::Vec(inner)) => {
                push(&mut tasks, CopyTask::Vec)?;
                push(&mut tasks, CopyTask::Visit(inner))?;
                continue;
            }
            CopyTask::Visit(EndpointType::Optional(inner)) => {
                push(&mut tasks, CopyTask::Optional)?;
                push(&mut tasks, CopyTask::Visit(inner))?;
                continue;
            }
            CopyTask::Visit(EndpointType::Struct { name, fields }) => {
                push(&mut tasks, CopyTask::Struct { name, fields })?;
                reserve(&mut tasks, fields.len())?;
                for field in fields.iter().rev() {
                    tasks.push(CopyTask::Visit(&field.ty));
                }
                continue;
            }
            CopyTask::Vec => EndpointType::Vec(Box::new(results.pop().ok_or_else(copy_invariant)?)),
            CopyTask::Optional => EndpointType::Optional(Box::new(results.pop().ok_or_else(copy_invariant)?)),
            CopyTask::Struct { name, fields } => {
                let start = results.len().checked_sub(fields.len()).ok_or_else(copy_invariant)?;
                let mut copied = Vec::new();
                reserve(&mut copied, fields.len())?;
                for (field, ty) in fields.iter().zip(results.drain(start..)) {
                    copied.push(EndpointField {
                        name: copy_string(&field.name)?,
                        ty,
                        description: copy_string(&field.description)?,
                    });
                }
                EndpointType::Struct { name: copy_string(name)?, fields: copied }
            }
            CopyTask::Visit(ty) => match ty {
                EndpointType::UInt32 => EndpointType::UInt32,
                EndpointType::Int32 => EndpointType::Int32,
                EndpointType::Int64 => EndpointType::Int64,
                EndpointType::Float64 => EndpointType::Float64,
                EndpointType::Boolean => EndpointType::Boolean,
                EndpointType::String => EndpointType::String,
                EndpointType::Bytea => EndpointType::Bytea,
                EndpointType::UUID => EndpointType::UUID,
                EndpointType::NanoId { len } => EndpointType::NanoId { len: *len },
                EndpointType::IpAddr => EndpointType::IpAddr,
                EndpointType::StructRef(name) => EndpointType::StructRef(copy_string(name)?),
                EndpointType::Object => EndpointType::Object,
                EndpointType::StructTable { struct_ref } => EndpointType::StructTable {
                    struct_ref: copy_string(struct_ref)?,
                },
                EndpointType::Unit => EndpointType::Unit,
                EndpointType::Enum { name, variants } => {
                    let mut copied = Vec::new();
                    reserve(&mut copied, variants.len())?;
                    for variant in variants {
                        copied.push(EnumVariantSpec {
                            name: copy_string(&variant.name)?,
                            value: variant.value,
                            description: copy_string(&variant.description)?,
                        });
                    }
                    EndpointType::Enum { name: copy_string(name)?, variants: copied }
                }
                EndpointType::EnumRef { name, prefixed_name } => EndpointType::EnumRef {
                    name: copy_string(name)?, prefixed_name: *prefixed_name,
                },
                EndpointType::TimeStampMs => EndpointType::TimeStampMs,
                EndpointType::BlockchainDecimal => EndpointType::BlockchainDecimal,
                EndpointType::BlockchainAddress => EndpointType::BlockchainAddress,
                EndpointType::BlockchainTransactionHash => EndpointType::BlockchainTransactionHash,
                EndpointType::Vec(_) | EndpointType::Optional(_) | EndpointType::Struct { .. } => {
                    return Err(copy_invariant());
                }
            },
        };
        push(&mut results, value)?;
    }
    if results.len() != 1 {
        return Err(copy_invariant());
    }
    results.pop().ok_or_else(copy_invariant)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{format, vec};
    use crate::LossPolicy;

    fn field(name: &str, ty: EndpointType) -> EndpointField {
        EndpointField { name: name.into(), ty, description: format!("{name} description\r\n水") }
    }

    fn endpoint() -> EndpointSchemaSpec {
        EndpointSchemaSpec {
            code: u32::MAX,
            name: "WatchHTTP_水".into(),
            service: "Market-v1".into(),
            roles: vec!["Reader".into(), "reader".into()],
            params: vec![field("symbol", EndpointType::String)],
            response: vec![field("accepted", EndpointType::Boolean)],
            stream_response: Some(EndpointType::Optional(Box::new(EndpointType::Unit))),
            description: "Markdown **description**\r\n🦀".into(),
        }
    }

    fn wrapper(endpoint: EndpointSchemaSpec) -> LegacyEndpoint {
        LegacyEndpoint { version: LEGACY_FORMAT_VERSION, endpoint }
    }

    fn service(endpoint: EndpointSchemaSpec) -> ServiceSpec {
        ServiceSpec { id: u16::MAX, name: endpoint.service.clone(), endpoints: vec![endpoint] }
    }

    fn assert_error<T: core::fmt::Debug>(result: Result<T>, code: ErrorCode) {
        let error = result.unwrap_err();
        assert_eq!(error.code, code);
        assert_eq!(error.source, ErrorSource::SchemaCompile);
        assert_eq!((error.byte_offset, error.schema_id, error.field_id), (0, 0, 0));
        assert!(!error.detail.is_empty());
    }

    fn assert_bad_endpoint(endpoint: EndpointSchemaSpec, code: ErrorCode) {
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let options = CompatOptions { loss_policy, ..CompatOptions::default() };
            assert_error(export_endpoint(&endpoint, options), code);
            assert_error(import_endpoint(wrapper(endpoint.clone()), options), code);
        }
    }

    fn assert_bad_services(services: &[ServiceSpec], code: ErrorCode) {
        let legacy: Vec<_> = services.iter().map(|service| LegacyService {
            version: LEGACY_FORMAT_VERSION,
            id: service.id,
            name: service.name.clone(),
            endpoints: service.endpoints.iter().cloned().map(wrapper).collect(),
        }).collect();
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let options = CompatOptions { loss_policy, ..CompatOptions::default() };
            assert_error(export_services(services, options), code);
            assert_error(import_services(&legacy, options), code);
        }
    }

    fn nominal_struct(name: &str) -> EndpointType {
        EndpointType::Struct { name: name.into(), fields: vec![field("id", EndpointType::UInt32)] }
    }

    fn nominal_enum(name: &str) -> EndpointType {
        EndpointType::Enum {
            name: name.into(),
            variants: vec![
                EnumVariantSpec { name: "Min".into(), value: i32::MIN, description: "minimum".into() },
                EnumVariantSpec { name: "Max".into(), value: i32::MAX, description: "maximum".into() },
            ],
        }
    }

    #[test]
    fn all_twenty_three_variants_round_trip_without_metadata_loss() {
        let types = vec![
            EndpointType::UInt32, EndpointType::Int32, EndpointType::Int64,
            EndpointType::Float64, EndpointType::Boolean, EndpointType::String,
            EndpointType::Bytea, EndpointType::UUID, EndpointType::NanoId { len: u8::MAX },
            EndpointType::IpAddr, nominal_struct("Record"),
            EndpointType::StructRef("Record".into()), EndpointType::Object,
            EndpointType::StructTable { struct_ref: "Record".into() },
            EndpointType::Vec(Box::new(EndpointType::Int64)), EndpointType::Unit,
            EndpointType::Optional(Box::new(EndpointType::String)), nominal_enum("State"),
            EndpointType::EnumRef { name: "State".into(), prefixed_name: true },
            EndpointType::TimeStampMs, EndpointType::BlockchainDecimal,
            EndpointType::BlockchainAddress, EndpointType::BlockchainTransactionHash,
        ];
        assert_eq!(types.len(), 23);
        let mut expected = endpoint();
        expected.params = types.into_iter().enumerate()
            .map(|(index, ty)| field(&format!("field_{index}"), ty)).collect();
        expected.response = vec![
            field("symbol", EndpointType::NanoId { len: 1 }),
            field("enum", EndpointType::EnumRef { name: "State".into(), prefixed_name: false }),
        ];
        for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
            let options = CompatOptions { loss_policy, ..CompatOptions::default() };
            let exported = export_endpoint(&expected, options).unwrap();
            assert!(exported.is_lossless());
            assert_eq!(exported.value, wrapper(expected.clone()));
            let imported = import_endpoint(exported.value, options).unwrap();
            assert!(imported.is_lossless());
            assert_eq!(imported.value, expected);
            let services = vec![service(expected.clone())];
            let legacy = export_services(&services, options).unwrap();
            assert!(legacy.is_lossless());
            let native = import_services(&legacy.value, options).unwrap();
            assert!(native.is_lossless());
            assert_eq!(native.value, services);
        }
    }

    #[test]
    fn every_unknown_option_version_is_rejected_even_for_empty_sets() {
        for legacy_version in [0, 2, 255, u16::MAX] {
            for loss_policy in [LossPolicy::Reject, LossPolicy::AllowDocumented] {
                let options = CompatOptions { legacy_version, loss_policy, ..CompatOptions::default() };
                assert_error(import_endpoint(wrapper(endpoint()), options), ErrorCode::UnsupportedSchemaFeature);
                assert_error(export_endpoint(&endpoint(), options), ErrorCode::UnsupportedSchemaFeature);
                assert_error(import_services(&[], options), ErrorCode::UnsupportedSchemaFeature);
                assert_error(export_services(&[], options), ErrorCode::UnsupportedSchemaFeature);
            }
        }
    }

    #[test]
    fn every_wrapper_version_must_match_the_supported_service_version() {
        let options = CompatOptions::default();
        for unsupported in [0, 2, u16::MAX] {
            let mut legacy_endpoint = wrapper(endpoint());
            legacy_endpoint.version = unsupported;
            assert_error(import_endpoint(legacy_endpoint.clone(), options), ErrorCode::UnsupportedSchemaFeature);
            let mut legacy_service = LegacyService {
                version: LEGACY_FORMAT_VERSION, id: 0, name: "Market-v1".into(),
                endpoints: vec![legacy_endpoint],
            };
            assert_error(import_services(&[legacy_service.clone()], options), ErrorCode::UnsupportedSchemaFeature);
            legacy_service.endpoints[0].version = LEGACY_FORMAT_VERSION;
            legacy_service.version = unsupported;
            assert_error(import_services(&[legacy_service.clone()], options), ErrorCode::UnsupportedSchemaFeature);
            legacy_service.endpoints.clear();
            assert_error(import_services(&[legacy_service], options), ErrorCode::UnsupportedSchemaFeature);
        }
    }

    #[test]
    fn empty_sets_and_empty_services_are_lossless_and_keep_numeric_boundaries() {
        let options = CompatOptions::default();
        assert_eq!(export_services(&[], options).unwrap().value, []);
        assert_eq!(import_services(&[], options).unwrap().value, []);
        let services = vec![
            ServiceSpec { id: 0, name: "Empty".into(), endpoints: Vec::new() },
            ServiceSpec { id: u16::MAX, name: "empty".into(), endpoints: Vec::new() },
        ];
        let legacy = export_services(&services, options).unwrap();
        assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
        let mut empty = endpoint();
        empty.code = 0;
        empty.roles.clear();
        empty.params.clear();
        empty.response.clear();
        empty.stream_response = None;
        empty.description.clear();
        assert_eq!(import_endpoint(export_endpoint(&empty, options).unwrap().value, options).unwrap().value, empty);
    }

    #[test]
    fn required_names_are_checked_in_all_owned_positions() {
        let mut cases = Vec::new();
        let mut value = endpoint(); value.name.clear(); cases.push(value);
        let mut value = endpoint(); value.service.clear(); cases.push(value);
        let mut value = endpoint(); value.roles.push(String::new()); cases.push(value);
        let mut value = endpoint(); value.params[0].name.clear(); cases.push(value);
        let mut value = endpoint(); value.response[0].name.clear(); cases.push(value);
        for ty in [
            nominal_struct(""), nominal_enum(""), EndpointType::StructRef(String::new()),
            EndpointType::StructTable { struct_ref: String::new() },
            EndpointType::EnumRef { name: String::new(), prefixed_name: true },
            EndpointType::Struct { name: "Record".into(), fields: vec![field("", EndpointType::String)] },
            EndpointType::Enum { name: "State".into(), variants: vec![
                EnumVariantSpec { name: String::new(), value: 0, description: String::new() },
            ] },
        ] {
            let mut value = endpoint(); value.stream_response = Some(ty); cases.push(value);
        }
        for value in cases { assert_bad_endpoint(value, ErrorCode::MissingRequiredField); }
        assert_bad_services(&[ServiceSpec { id: 0, name: String::new(), endpoints: vec![] }], ErrorCode::MissingRequiredField);
    }

    #[test]
    fn duplicate_fields_roles_and_variant_names_are_rejected() {
        let mut value = endpoint(); value.roles.push(value.roles[0].clone());
        assert_bad_endpoint(value, ErrorCode::DuplicateObjectKey);
        let mut value = endpoint(); value.params.push(field("symbol", EndpointType::Unit));
        assert_bad_endpoint(value, ErrorCode::DuplicateObjectKey);
        let mut value = endpoint(); value.response.push(field("accepted", EndpointType::String));
        assert_bad_endpoint(value, ErrorCode::DuplicateObjectKey);
        for ty in [
            EndpointType::Struct { name: "Record".into(), fields: vec![
                field("id", EndpointType::Int32), field("id", EndpointType::String),
            ] },
            EndpointType::Enum { name: "State".into(), variants: vec![
                EnumVariantSpec { name: "same".into(), value: -1, description: String::new() },
                EnumVariantSpec { name: "same".into(), value: 1, description: String::new() },
            ] },
        ] {
            let mut value = endpoint(); value.stream_response = Some(ty);
            assert_bad_endpoint(value, ErrorCode::DuplicateObjectKey);
        }
    }

    #[test]
    fn duplicate_enum_values_and_zero_nanoid_lengths_are_constraints() {
        let mut value = endpoint();
        value.stream_response = Some(EndpointType::Enum { name: "State".into(), variants: vec![
            EnumVariantSpec { name: "first".into(), value: i32::MIN, description: String::new() },
            EnumVariantSpec { name: "second".into(), value: i32::MIN, description: String::new() },
        ] });
        assert_bad_endpoint(value, ErrorCode::ConstraintViolation);
        let mut value = endpoint();
        value.stream_response = Some(EndpointType::Optional(Box::new(EndpointType::Vec(Box::new(
            EndpointType::NanoId { len: 0 },
        )))));
        assert_bad_endpoint(value, ErrorCode::ConstraintViolation);
    }

    #[test]
    fn service_and_endpoint_uniqueness_have_the_contract_scopes() {
        let first = service(endpoint());
        let mut second = first.clone(); second.name = "Other".into(); second.endpoints.clear();
        assert_bad_services(&[first.clone(), second], ErrorCode::ConstraintViolation);
        let mut second = first.clone(); second.id = 0; second.endpoints.clear();
        assert_bad_services(&[first.clone(), second], ErrorCode::DuplicateObjectKey);
        let mut duplicate_code = first.clone();
        let mut other = endpoint(); other.name = "Other".into();
        duplicate_code.endpoints.push(other);
        assert_bad_services(&[duplicate_code], ErrorCode::ConstraintViolation);
        let mut duplicate_name = first.clone();
        let mut other = endpoint(); other.code = 0;
        duplicate_name.endpoints.push(other);
        assert_bad_services(&[duplicate_name], ErrorCode::DuplicateObjectKey);

        let mut second = first.clone(); second.id = 0; second.name = "Other".into();
        second.endpoints[0].service = "Other".into();
        let services = vec![first, second];
        let options = CompatOptions::default();
        let legacy = export_services(&services, options).unwrap();
        assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
    }

    #[test]
    fn endpoint_service_must_match_exactly() {
        let mut value = service(endpoint()); value.name.make_ascii_lowercase();
        assert_bad_services(&[value], ErrorCode::ConstraintViolation);
    }

    #[test]
    fn endpoint_only_defers_unknown_and_wrong_kind_reference_resolution() {
        for ty in [
            EndpointType::StructRef("External".into()),
            EndpointType::StructTable { struct_ref: "External".into() },
            EndpointType::EnumRef { name: "External".into(), prefixed_name: false },
        ] {
            let mut value = endpoint(); value.stream_response = Some(ty);
            let options = CompatOptions::default();
            let legacy = export_endpoint(&value, options).unwrap();
            assert_eq!(import_endpoint(legacy.value, options).unwrap().value, value);
            assert_bad_services(&[service(value)], ErrorCode::UnsupportedSchemaFeature);
        }
        let mut value = endpoint();
        value.params = vec![field("definition", nominal_enum("State"))];
        value.stream_response = Some(EndpointType::StructRef("State".into()));
        assert!(export_endpoint(&value, CompatOptions::default()).is_ok());
        assert_bad_services(&[service(value)], ErrorCode::ConstraintViolation);
    }

    #[test]
    fn forward_and_cross_service_references_are_resolved_without_reordering() {
        let mut consumer = endpoint(); consumer.code = 0;
        consumer.params = vec![field("rows", EndpointType::StructTable { struct_ref: "Record".into() })];
        consumer.stream_response = Some(EndpointType::EnumRef { name: "State".into(), prefixed_name: true });
        let mut producer = endpoint(); producer.service = "Definitions".into();
        producer.params = vec![field("record", nominal_struct("Record"))];
        producer.response = vec![field("state", nominal_enum("State"))];
        let services = vec![service(consumer), ServiceSpec {
            id: 0, name: "Definitions".into(), endpoints: vec![producer],
        }];
        let options = CompatOptions::default();
        let legacy = export_services(&services, options).unwrap();
        assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
    }

    #[test]
    fn struct_enum_and_table_references_require_the_correct_kind_and_case() {
        for ty in [
            EndpointType::StructRef("State".into()),
            EndpointType::StructTable { struct_ref: "State".into() },
            EndpointType::EnumRef { name: "Record".into(), prefixed_name: false },
        ] {
            let mut value = endpoint();
            value.params = vec![field("r", nominal_struct("Record")), field("s", nominal_enum("State"))];
            value.stream_response = Some(ty);
            assert_bad_services(&[service(value)], ErrorCode::ConstraintViolation);
        }
        let mut value = endpoint();
        value.params = vec![field("r", nominal_struct("Record"))];
        value.stream_response = Some(EndpointType::StructRef("record".into()));
        assert_bad_services(&[service(value)], ErrorCode::UnsupportedSchemaFeature);
    }

    #[test]
    fn repeated_identical_nominal_definitions_are_lossless() {
        let mut value = endpoint();
        value.params = vec![field("first", nominal_struct("Record")), field("state", nominal_enum("State"))];
        value.response = vec![field("second", nominal_struct("Record")), field("state", nominal_enum("State"))];
        let services = vec![service(value)];
        let options = CompatOptions::default();
        let legacy = export_services(&services, options).unwrap();
        assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
    }

    #[test]
    fn nominal_conflicts_include_descriptions_order_and_nested_types() {
        let base = EndpointType::Struct { name: "Record".into(), fields: vec![
            field("first", EndpointType::Optional(Box::new(EndpointType::String))),
            field("second", EndpointType::Int64),
        ] };
        let mut conflicts = Vec::new();
        let mut changed = base.clone();
        if let EndpointType::Struct { fields, .. } = &mut changed { fields[0].description.push('!'); }
        conflicts.push(changed);
        let mut changed = base.clone();
        if let EndpointType::Struct { fields, .. } = &mut changed { fields.reverse(); }
        conflicts.push(changed);
        let mut changed = base.clone();
        if let EndpointType::Struct { fields, .. } = &mut changed {
            fields[0].ty = EndpointType::Vec(Box::new(EndpointType::String));
        }
        conflicts.push(changed);
        conflicts.push(nominal_enum("Record"));
        for conflict in conflicts {
            let mut value = endpoint();
            value.params = vec![field("first", base.clone())];
            value.response = vec![field("second", conflict)];
            assert_bad_endpoint(value.clone(), ErrorCode::ConstraintViolation);
            assert_bad_services(&[service(value)], ErrorCode::ConstraintViolation);
        }
        for change_description in [false, true] {
            let mut changed = nominal_enum("State");
            if let EndpointType::Enum { variants, .. } = &mut changed {
                if change_description { variants[0].description.push('!'); }
                else { variants.reverse(); }
            }
            let mut value = endpoint();
            value.params = vec![field("a", nominal_enum("State")), field("b", changed)];
            assert_bad_endpoint(value, ErrorCode::ConstraintViolation);
        }
    }

    #[test]
    fn recursive_nominal_graphs_are_not_expanded() {
        let first = EndpointType::Struct { name: "First".into(), fields: vec![
            field("self", EndpointType::Optional(Box::new(EndpointType::StructRef("First".into())))),
            field("next", EndpointType::StructRef("Second".into())),
        ] };
        let second = EndpointType::Struct { name: "Second".into(), fields: vec![
            field("previous", EndpointType::StructRef("First".into())),
        ] };
        let mut value = endpoint();
        value.params = vec![field("first", first), field("second", second)];
        let mut options = CompatOptions::default(); options.limits.max_depth = 2;
        let services = vec![service(value)];
        let legacy = export_services(&services, options).unwrap();
        assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
    }

    fn nested(depth: usize) -> EndpointType {
        let mut ty = EndpointType::Unit;
        for index in 0..depth {
            ty = if index % 2 == 0 { EndpointType::Optional(Box::new(ty)) }
                else { EndpointType::Vec(Box::new(ty)) };
        }
        ty
    }

    #[test]
    fn depth_ceiling_is_inclusive_for_owned_type_edges() {
        for maximum in [0, 1, 2, 16, 128] {
            let mut options = CompatOptions::default(); options.limits.max_depth = maximum;
            let mut value = endpoint();
            value.stream_response = Some(nested(usize::from(maximum)));
            let legacy = export_endpoint(&value, options).unwrap();
            assert_eq!(import_endpoint(legacy.value, options).unwrap().value, value);
            let services = vec![service(value.clone())];
            let legacy = export_services(&services, options).unwrap();
            assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
            value.stream_response = Some(nested(usize::from(maximum) + 1));
            assert_error(export_endpoint(&value, options), ErrorCode::DepthLimitExceeded);
            assert_error(import_endpoint(wrapper(value), options), ErrorCode::DepthLimitExceeded);
        }
    }

    #[test]
    fn nested_struct_fields_consume_depth_but_empty_structs_do_not() {
        let mut options = CompatOptions::default(); options.limits.max_depth = 0;
        let mut value = endpoint();
        value.stream_response = Some(EndpointType::Struct { name: "Empty".into(), fields: Vec::new() });
        assert!(export_endpoint(&value, options).is_ok());
        value.stream_response = Some(nominal_struct("Record"));
        assert_error(export_endpoint(&value, options), ErrorCode::DepthLimitExceeded);
        options.limits.max_depth = 1;
        assert!(export_endpoint(&value, options).is_ok());
    }

    #[test]
    fn permissive_key_settings_do_not_disable_model_uniqueness() {
        let mut value = endpoint(); value.roles.push(value.roles[0].clone());
        let options = CompatOptions { limits: super::super::Limits::permissive_default(), ..CompatOptions::default() };
        assert_error(export_endpoint(&value, options), ErrorCode::DuplicateObjectKey);
        assert_error(import_endpoint(wrapper(value), options), ErrorCode::DuplicateObjectKey);
    }

    #[test]
    fn byte_encoding_limits_do_not_truncate_in_memory_models() {
        let mut options = CompatOptions { max_output_bytes: 0, ..CompatOptions::default() };
        options.limits.max_payload_bytes = 0;
        options.limits.max_decoded_bytes = 0;
        options.limits.max_string_bytes = 0;
        let expected = endpoint();
        let legacy = export_endpoint(&expected, options).unwrap();
        assert_eq!(import_endpoint(legacy.value, options).unwrap().value, expected);
    }

    #[test]
    fn service_exports_do_not_mutate_or_alias_the_input() {
        let services = vec![service(endpoint())];
        let expected = services.clone();
        let options = CompatOptions::default();
        let mut exported = export_services(&services, options).unwrap().value;
        exported[0].name.clear();
        exported[0].endpoints[0].endpoint.roles[0].clear();
        exported[0].endpoints[0].endpoint.params[0].description.clear();
        assert_eq!(services, expected);
        let legacy = export_services(&services, options).unwrap().value;
        let mut imported = import_services(&legacy, options).unwrap().value;
        imported[0].endpoints[0].name.clear();
        assert_eq!(legacy[0].endpoints[0].endpoint, expected[0].endpoints[0]);
    }

    #[test]
    fn nominal_conflicts_are_detected_across_services() {
        let mut first = endpoint();
        first.params = vec![field("record", nominal_struct("Shared"))];
        let mut second = endpoint();
        second.service = "Other".into();
        second.params = vec![field("record", EndpointType::Struct {
            name: "Shared".into(), fields: vec![field("id", EndpointType::Int64)],
        })];
        assert_bad_services(&[
            service(first),
            ServiceSpec { id: 0, name: "Other".into(), endpoints: vec![second] },
        ], ErrorCode::ConstraintViolation);
    }

    #[test]
    fn nested_records_and_empty_enums_preserve_every_sibling_in_order() {
        let mut expected = endpoint();
        expected.params = vec![field("tree", EndpointType::Vec(Box::new(EndpointType::Struct {
            name: "Branch".into(),
            fields: vec![
                field("left", nominal_struct("Leaf")),
                field("empty", EndpointType::Struct { name: "Empty".into(), fields: vec![] }),
                field("right", EndpointType::Optional(Box::new(EndpointType::StructRef("Leaf".into())))),
                field("enum", EndpointType::Enum { name: "Never".into(), variants: vec![] }),
                field("unit", EndpointType::Unit),
                field("units", EndpointType::Vec(Box::new(EndpointType::Unit))),
            ],
        })))];
        expected.response = vec![field("tree", EndpointType::StructRef("Branch".into()))];
        let services = vec![service(expected)];
        let options = CompatOptions::default();
        let legacy = export_services(&services, options).unwrap();
        assert_eq!(import_services(&legacy.value, options).unwrap().value, services);
    }

    // The frozen ABI owns recursive Boxes. Dismantle the test's exceptionally
    // deep chain explicitly so its derived destructor does not use the stack.
    fn dismantle_chain(mut ty: EndpointType) {
        loop {
            match ty {
                EndpointType::Vec(inner) | EndpointType::Optional(inner) => ty = *inner,
                EndpointType::Unit => return,
                _ => panic!("expected a container chain ending in Unit"),
            }
        }
    }

    #[test]
    fn full_u16_depth_boundary_uses_iterative_validation_comparison_and_copying() {
        let mut options = CompatOptions::default();
        options.limits.max_depth = u16::MAX;
        let mut original = endpoint();
        original.stream_response = Some(nested(usize::from(u16::MAX)));
        let exported = export_endpoint(&original, options).unwrap();
        assert!(same_type(
            original.stream_response.as_ref().unwrap(),
            exported.value.endpoint.stream_response.as_ref().unwrap(),
        ).unwrap());
        let mut imported = import_endpoint(exported.value, options).unwrap().value;
        dismantle_chain(imported.stream_response.take().unwrap());
        dismantle_chain(original.stream_response.take().unwrap());

        original.stream_response = Some(nested(usize::from(u16::MAX) + 1));
        assert_error(export_endpoint(&original, options), ErrorCode::DepthLimitExceeded);
        dismantle_chain(original.stream_response.take().unwrap());
    }

    #[test]
    fn impossible_reservations_return_canonical_errors_without_changing_values() {
        let mut values = vec![7_u8];
        assert_error(reserve(&mut values, usize::MAX), ErrorCode::BudgetExhausted);
        assert_eq!(values, [7]);
    }
}
