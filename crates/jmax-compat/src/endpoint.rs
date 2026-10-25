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
